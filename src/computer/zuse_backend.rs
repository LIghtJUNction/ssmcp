//! Fixed-environment, connection-owned execution of the zuse Rust library.
//!
//! The library's OS handles remain inside a short-lived worker. Environment is
//! selected at spawn, never changed in the multithreaded client. Closing stdin
//! cancels between native calls. X11 text is one whole-string library call:
//! cancellation after dispatch cannot undo input, and cleanup is acknowledged
//! only after that call returns or the owned worker's failure is recorded.
use super::*;
use std::{
    io::{BufRead, Write},
    sync::{Condvar, OnceLock, mpsc},
};
use tokio::io::{AsyncBufReadExt, BufReader};

const WORKER_FLAG: &str = "--internal-zuse-worker";
const WORKER_READY: &[u8] = b"ssmcp-zuse-worker-v1\n";
const WORKER_START_TIMEOUT: Duration = Duration::from_secs(3);
static WORKER_ENTRY_REGISTERED: AtomicBool = AtomicBool::new(false);
// JSON escaping can expand each of the 16,384 allowed text bytes sixfold.
const REQUEST_LIMIT: usize = 128 * 1024;
const RESPONSE_LIMIT: usize = 768 * 1024;
const CANCELLED: &str = "zuse operation cancelled";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: String,
    arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capture_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    host_ticket: Option<host_control::WorkerTicket>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    value: Option<Value>,
    error: Option<String>,
    cleanup_ok: bool,
}

/// Local executable startup calls this before entering its main runtime.
/// Embedding binaries that construct `Computer` must forward this entry too.
/// It is not an MCP tool and accepts no caller-selected executable or script.
pub fn worker_entry() -> Option<Result<(), String>> {
    // A library embedder must opt in before any Computer can respawn its
    // executable. Missing integration must not duplicate the host application.
    WORKER_ENTRY_REGISTERED.store(true, Ordering::Release);
    let mut args = std::env::args_os();
    args.next();
    if args.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_FLAG)) {
        return None;
    }
    Some(if args.next().is_some() {
        Err("the internal zuse worker takes no arguments".into())
    } else {
        worker_main()
    })
}

struct Worker {
    process: Managed,
    input: Option<tokio::process::ChildStdin>,
    output: Option<BufReader<tokio::process::ChildStdout>>,
    error: Option<tokio::process::ChildStderr>,
    input_pending: bool,
    // Owned by the parent, so forced worker termination cannot leak a capture.
    _capture_directory: Option<CaptureDirectory>,
}

impl Worker {
    async fn finish(mut self) -> Result<(), String> {
        // EOF is the cooperative cancellation signal. Keep stdout drained so
        // a completed screenshot cannot block cleanup on a full pipe.
        self.input.take();
        let output = self.output.take();
        let error = self.error.take();
        let completion = tokio::time::timeout(RELEASE_HELPER_TIMEOUT, async {
            let drain = async {
                if let Some(output) = output {
                    tokio::io::copy(
                        &mut output.take((RESPONSE_LIMIT + 1) as u64),
                        &mut tokio::io::sink(),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                }
                Ok::<(), String>(())
            };
            let errors = async {
                let mut bytes = Vec::new();
                if let Some(error) = error {
                    error
                        .take(8193)
                        .read_to_end(&mut bytes)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok::<_, String>(bytes)
            };
            let (drained, errors, status) = tokio::join!(drain, errors, self.process.wait());
            drained?;
            let errors = errors?;
            let status = status.map_err(|error| error.to_string())?;
            if !status.success() && self.input_pending {
                return Err(format!(
                    "zuse worker exited before confirming cleanup ({status}): {}",
                    bounded(String::from_utf8_lossy(&errors).trim(), 1024)
                ));
            }
            Ok(())
        })
        .await;
        match completion {
            Ok(result) => result,
            Err(_) => {
                self.process.shutdown().await;
                if self.input_pending {
                    Err("zuse worker cleanup timed out; input release is unconfirmed".into())
                } else {
                    Ok(())
                }
            }
        }
    }
}

struct WorkerGuard {
    worker: Option<Worker>,
    releases: Arc<ReleaseTasks>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.input.take();
            self.releases.start_async(worker.finish());
        }
    }
}

/// Keep the ingress generation even when an isolated desktop has its own
/// Computer. A worker must never acquire a fresh permit after Off/On.
fn worker_ticket_for_dispatch(
    computer: &Computer,
    operation: &str,
    host_ticket: Option<host_control::Ticket>,
) -> Result<Option<host_control::WorkerTicket>, String> {
    if !host_control::input_tool(&format!("ssmcp.computer.{operation}")) {
        return Ok(None);
    }
    let ticket = if let Some(ticket) = host_ticket {
        computer.host_control.worker_ticket(ticket)?
    } else {
        host_control::Permit::current()
            .ok_or("missing computer-use permit")?
            .worker_ticket()?
    };
    Ok(Some(ticket))
}

async fn call(
    computer: &Computer,
    operation: &str,
    arguments: Value,
    host_ticket: Option<host_control::Ticket>,
) -> Result<Value, String> {
    #[cfg(not(test))]
    if !WORKER_ENTRY_REGISTERED.load(Ordering::Acquire) {
        return Err("the embedding executable must call computer::worker_entry() before starting its runtime; no worker was started".into());
    }
    let capture_directory = if operation == "capture" {
        Some(CaptureDirectory::new()?)
    } else {
        None
    };
    let request = serde_json::to_vec(&Request {
        operation: operation.into(),
        arguments,
        capture_path: capture_directory
            .as_ref()
            .map(|directory| directory.0.join("capture.png")),
        host_ticket: worker_ticket_for_dispatch(computer, operation, host_ticket)?,
    })
    .map_err(|error| error.to_string())?;
    if request.len() > REQUEST_LIMIT {
        return Err("zuse request exceeds 128 KiB".into());
    }
    let executable = worker_executable()?;
    let mut command = tokio::process::Command::new(executable);
    #[cfg(not(test))]
    command.arg(WORKER_FLAG);
    #[cfg(test)]
    command
        .args([
            "--exact",
            "computer::zuse_backend::tests::subprocess_worker",
            "--ignored",
            "--nocapture",
            "--quiet",
        ])
        .env("SSMCP_ZUSE_WORKER_FIXTURE", "1");
    command
        .current_dir(&computer.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("ZUSE_BACKEND");
    if let Some(environment) = computer.environment.as_ref() {
        environment.apply(command.as_std_mut());
        command.env("ZUSE_BACKEND", "x11");
    } else if cfg!(target_os = "linux") {
        command.env(
            "ZUSE_BACKEND",
            if computer.session == Session::X11 {
                "x11"
            } else {
                "wayland"
            },
        );
    }
    super::host_control::Permit::check_current()?;
    let mut process = Managed::spawn(&mut command)
        .map_err(|error| format!("could not start zuse library worker: {error}"))?;
    let worker = Worker {
        input: process.child.stdin.take(),
        output: process.child.stdout.take().map(BufReader::new),
        error: process.child.stderr.take(),
        process,
        input_pending: false,
        _capture_directory: capture_directory,
    };
    let mut guard = WorkerGuard {
        worker: Some(worker),
        releases: computer.releases.clone(),
    };
    let worker = guard.worker.as_mut().ok_or("zuse worker unavailable")?;
    let output = worker
        .output
        .as_mut()
        .ok_or("zuse worker stdout unavailable")?;
    let ready = tokio::time::timeout(WORKER_START_TIMEOUT, read_line(output, 256))
        .await
        .map_err(|_| "zuse worker did not complete its startup handshake within three seconds; no input was sent; restart this ssmcp session".to_owned())?
        .map_err(|error| format!("zuse worker startup failed before input; restart this ssmcp session or ensure the embedding executable calls computer::worker_entry(): {error}"))?;
    #[cfg(test)]
    let ready = {
        let mut ready = ready;
        // The test executable adds a fixed preamble before its worker fixture.
        // Production accepts only the exact protocol greeting below.
        for expected in [b"\n".as_slice(), b"running 1 test\n".as_slice()] {
            if ready == expected {
                ready = read_line(output, 256).await?;
            }
        }
        ready
    };
    if ready != WORKER_READY {
        return Err("current executable does not implement the zuse worker entry; call computer::worker_entry() before starting the application runtime".into());
    }
    worker.input_pending = matches!(
        operation,
        "click" | "drag" | "key" | "type" | "scroll" | "num_lock" | "watch_until_click"
    );
    let input = worker
        .input
        .as_mut()
        .ok_or("zuse worker stdin unavailable")?;
    input
        .write_all(&request)
        .await
        .map_err(|error| error.to_string())?;
    input
        .write_all(b"\n")
        .await
        .map_err(|error| error.to_string())?;
    input.flush().await.map_err(|error| error.to_string())?;
    let response = read_line(output, RESPONSE_LIMIT).await?;
    let reply: Reply = serde_json::from_slice(&response)
        .map_err(|error| format!("invalid zuse worker response: {error}"))?;
    if !reply.cleanup_ok {
        computer.releases.fail(format!(
            "zuse reported an input-release failure: {}",
            reply
                .error
                .as_deref()
                .unwrap_or("worker returned no error detail")
        ));
    } else {
        worker.input_pending = false;
    }
    let worker = guard.worker.take().ok_or("zuse worker unavailable")?;
    // Register cleanup before the caller can be cancelled again. Waiting on the
    // shared tracker preserves the host input lock until the exact child exits.
    computer.releases.start_async(worker.finish());
    computer.releases.wait(RELEASE_WAIT_TIMEOUT).await?;
    match (reply.value, reply.error) {
        (Some(value), None) => Ok(value),
        (None, Some(error)) => Err(error),
        _ => Err("zuse worker returned an inconsistent response".into()),
    }
}

fn worker_executable() -> Result<PathBuf, String> {
    #[cfg(target_os = "linux")]
    {
        // Resolve the running image, including an inode unlinked by an upgrade.
        // Never select an arbitrary same-named executable from PATH, and never
        // retry an action through a different binary after input has begun.
        let running = PathBuf::from("/proc/self/exe");
        if running.is_file() {
            return Ok(running);
        }
    }
    let executable = std::env::current_exe().map_err(|error| {
        format!("cannot locate the current zuse worker before input: {error}; restart this ssmcp session")
    })?;
    if !executable.is_file() {
        return Err("the running executable was replaced and its worker image is unavailable; no input was sent; restart this ssmcp session".into());
    }
    Ok(executable)
}

async fn read_line(
    output: &mut BufReader<tokio::process::ChildStdout>,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut response = Vec::new();
    output
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut response)
        .await
        .map_err(|error| error.to_string())?;
    if response.len() > limit || response.last() != Some(&b'\n') {
        return Err("zuse worker returned an incomplete or oversized response".into());
    }
    Ok(response)
}

fn coordinate_space() -> &'static str {
    if cfg!(windows) {
        "desktop_physical"
    } else {
        "desktop_logical"
    }
}

pub(super) async fn status(computer: &Computer) -> Value {
    let session = match computer.session {
        Session::Hyprland => "hyprland",
        Session::Wayland => "wayland",
        Session::X11 => "x11",
        Session::Unavailable => "unavailable",
    };
    if cfg!(target_os = "linux") && computer.session == Session::Unavailable {
        return json!({"platform":std::env::consts::OS,"session":session,"available":false,
            "engine":"zuse","engine_version":zuse::VERSION,"reason":"No graphical desktop session is available"});
    }
    let native = call(computer, "capabilities", json!({}), None).await;
    let geometry = monitors(computer).await;
    match native {
        Ok(native) => {
            let caps = &native["result"];
            let backend = native["backend"].as_str().unwrap_or("zuse");
            let reason = geometry.as_ref().err().map(String::as_str);
            let pointer = caps["pointer"] == true;
            let keyboard = caps["keyboard"] == true;
            json!({"platform":std::env::consts::OS,"session":session,"available":true,
            "engine":"zuse","engine_version":zuse::VERSION,"backend":backend,
            "coordinate_space":coordinate_space(),"native_capabilities":caps,
            "capabilities":{
                "monitors":capability(geometry.is_ok(),"native monitor adapter",reason),
                "windows":capability(caps["windows"]==true,backend,Some("Native window enumeration is unavailable")),
                "screenshot":capability(caps["capture"]==true&&geometry.is_ok(),backend,reason.or(Some("Native capture is unavailable"))),
                "move":capability(pointer&&geometry.is_ok(),backend,reason.or(Some("Native pointer input is unavailable"))),
                "move_path":capability(cfg!(target_os="linux")&&pointer&&geometry.is_ok(),backend,reason.or(Some("Continuous pointer paths require a Linux pointer backend"))),
                "click":capability(pointer,backend,Some("Native pointer input is unavailable")),
                "drag":capability(pointer&&geometry.is_ok(),backend,reason.or(Some("Native pointer input is unavailable"))),
                "key":capability(keyboard,backend,Some("Native keyboard input is unavailable")),
                "keyboard_state":capability(caps["keyboard_state"]==true,backend,Some("Keyboard-state inspection requires a supported Hyprland session")),
                "num_lock":capability(caps["num_lock"]==true,backend,Some("Explicit Num Lock control requires a supported Hyprland session")),
                "type":capability(keyboard,backend,Some("Native keyboard input is unavailable")),
                "scroll":capability(pointer,backend,Some("Native pointer input is unavailable"))
            }})
        }
        Err(error) => json!({"platform":std::env::consts::OS,"session":session,"available":false,
            "engine":"zuse","engine_version":zuse::VERSION,"reason":error}),
    }
}

async fn monitors(computer: &Computer) -> Result<Vec<Monitor>, String> {
    #[cfg(windows)]
    let native = windows::perform(&computer.root, "ssmcp.computer.monitors", json!({})).await?;
    #[cfg(target_os = "macos")]
    let native = macos::perform(&computer.root, "ssmcp.computer.monitors", json!({})).await?;
    #[cfg(any(windows, target_os = "macos"))]
    return serde_json::from_value(native["structuredContent"]["monitors"].clone())
        .map_err(|error| format!("invalid native monitor metadata: {error}"));
    #[cfg(not(any(windows, target_os = "macos")))]
    computer.monitors().await
}

fn validate_point(point: Point, monitors: &[Monitor]) -> Result<(), String> {
    point.validate()?;
    if monitors.iter().any(|monitor| {
        Region {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        }
        .contains(point)
    }) {
        Ok(())
    } else {
        Err("point is outside the active monitors; refresh computer.monitors".into())
    }
}

pub(super) async fn perform(
    computer: &Computer,
    name: &str,
    args: Value,
    host_ticket: Option<host_control::Ticket>,
) -> Result<Value, String> {
    // Do not connect a native library for invalid/unknown operations.
    let (operation, arguments, metadata) = match name {
        "ssmcp.computer.windows" => {
            parse::<Empty>(args)?;
            ("windows", json!({}), None)
        }
        "ssmcp.computer.watch_until_click" => {
            if host_ticket.is_none() || computer.environment.is_some() {
                return Err(
                    "watch_until_click requires the original host-input authorization".into(),
                );
            }
            let value: WatchUntilClick = parse(args)?;
            value.validate()?;
            ("watch_until_click", json!(value), None)
        }
        "ssmcp.computer.focus" => {
            let value: Focus = parse(args)?;
            if !valid_window_id(&value.window_id) {
                return Err("window_id must be a numeric ID returned by computer.windows".into());
            }
            (
                "focus",
                json!({"window_id":value.window_id}),
                Some(json!({"focused":value.window_id})),
            )
        }
        "ssmcp.computer.move" => {
            let point: Point = parse(args)?;
            validate_point(point, &monitors(computer).await?)?;
            (
                "move",
                json!(point),
                Some(json!({"x":point.x,"y":point.y,"coordinate_space":coordinate_space()})),
            )
        }
        "ssmcp.computer.move_path" if cfg!(target_os = "linux") => {
            let value: MovePath = parse(args)?;
            let frames = timed_path(&value)?;
            let geometry = monitors(computer).await?;
            for frame in &frames {
                validate_point(frame.point, &geometry)?;
            }
            ("move_path", json!(frames), None)
        }
        "ssmcp.computer.click" => {
            let value: Click = parse(args)?;
            if !(1..=3).contains(&value.count) {
                return Err("count must be between 1 and 3".into());
            }
            let point = optional_point(value.x, value.y)?;
            #[cfg(target_os = "linux")]
            let binding = click_window_binding(&value, point)?;
            #[cfg(not(target_os = "linux"))]
            let binding: Option<WindowBinding> = None;
            if let Some(point) = point
                && !binding.as_ref().is_some_and(|binding| binding.relative)
            {
                validate_point(point, &monitors(computer).await?)?;
            }
            (
                "click",
                json!({"point":point,"button":button_name(value.button),"count":value.count,"window":binding}),
                None,
            )
        }
        "ssmcp.computer.drag" => {
            let value: Drag = parse(args)?;
            if !(2..=64).contains(&value.path.len()) || value.duration_ms > 5000 {
                return Err("drag requires 2–64 points and duration_ms at most 5000".into());
            }
            let geometry = monitors(computer).await?;
            for &point in &value.path {
                validate_point(point, &geometry)?;
            }
            (
                "drag",
                json!({"path":value.path,"button":button_name(value.button),"duration_ms":value.duration_ms}),
                Some(json!({"dragged":true,"button_released":true})),
            )
        }
        "ssmcp.computer.key" => {
            let value: Key = parse(args)?;
            validate_key(&value)?;
            (
                "key",
                json!({"key":value.key,"modifiers":value.modifiers}),
                Some(json!({"pressed":true,"released":true})),
            )
        }
        #[cfg(target_os = "linux")]
        "ssmcp.computer.keyboard_state" => {
            parse::<Empty>(args)?;
            ("keyboard_state", json!({}), None)
        }
        #[cfg(target_os = "linux")]
        "ssmcp.computer.num_lock" => {
            let value: NumLock = parse(args)?;
            validate_num_lock(&value.state)?;
            ("num_lock", json!({"state":value.state}), None)
        }
        "ssmcp.computer.type" => {
            let value: TypeText = parse(args)?;
            if value.text.len() > 16384 || value.text.contains('\0') {
                return Err("text must be at most 16384 UTF-8 bytes without NUL".into());
            }
            let characters = value.text.chars().count();
            (
                "type",
                json!({"text":value.text}),
                Some(json!({"typed":true,"characters":characters})),
            )
        }
        "ssmcp.computer.scroll" => {
            let value: Scroll = parse(args)?;
            if !(1..=100).contains(&value.steps) {
                return Err("steps must be between 1 and 100".into());
            }
            let (dx, dy) = match value.direction.as_str() {
                "up" => (0, -1),
                "down" => (0, 1),
                "left" => (-1, 0),
                "right" => (1, 0),
                _ => return Err("direction must be up, down, left or right".into()),
            };
            let point = optional_point(value.x, value.y)?;
            if let Some(point) = point {
                validate_point(point, &monitors(computer).await?)?;
            }
            (
                "scroll",
                json!({"point":point,"dx":dx,"dy":dy,"steps":value.steps}),
                Some(json!({"scrolled":true})),
            )
        }
        "ssmcp.computer.screenshot" => {
            let value: Screenshot = parse(args)?;
            let desktop = bounds(&monitors(computer).await?)?;
            let region = validate_capture(&value, desktop)?;
            (
                "capture",
                json!({"desktop":desktop,"region":region,"max_width":value.max_width,"quality":value.quality,"cursor_requested":value.cursor}),
                None,
            )
        }
        _ => return Err(format!("unknown computer tool: {name}")),
    };
    let native = call(computer, operation, arguments, host_ticket).await?;
    if operation == "focus" && computer.session == Session::Hyprland {
        let id = native["result"]["focused"]
            .as_str()
            .ok_or("zuse focus returned no window ID")?;
        computer.verify_hypr_focus(id).await?;
    }
    if operation == "capture" {
        let data = native["data"]
            .as_str()
            .ok_or("zuse returned no capture data")?;
        let metadata = &native["metadata"];
        return Ok(
            json!({"content":[{"type":"text","text":metadata.to_string()},
            {"type":"image","mimeType":"image/jpeg","data":data}],
            "structuredContent":metadata,"isError":false}),
        );
    }
    let mut metadata = metadata.unwrap_or_else(|| native["result"].clone());
    if operation == "windows" {
        normalize_window_identities(native["backend"].as_str().unwrap_or(""), &mut metadata)?;
    }
    if let Some(object) = metadata.as_object_mut() {
        object.insert("engine".into(), json!("zuse"));
        object.insert("engine_version".into(), json!(zuse::VERSION));
        object.insert("backend".into(), native["backend"].clone());
    }
    let failed_watch = operation == "watch_until_click" && metadata["status"] != "completed";
    let mut response = result(metadata);
    if failed_watch {
        response["isError"] = json!(true);
    }
    Ok(response)
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WatchUntilClick {
    endpoint: String,
    tab_id: String,
    scope: String,
    selector: String,
    expected_origin: String,
    max_clicks: u32,
    timeout_ms: u64,
}

impl WatchUntilClick {
    fn options(&self) -> zuse::observation::ClickWatchOptions {
        zuse::observation::ClickWatchOptions {
            scope: self.scope.clone(),
            selector: self.selector.clone(),
            expected_origin: Some(self.expected_origin.clone()),
            max_clicks: self.max_clicks,
            timeout_ms: self.timeout_ms,
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.options().validate()?;
        validate_browser_endpoint(&self.endpoint)?;
        if self.tab_id.trim().is_empty()
            || self.tab_id.len() > 1024
            || self.tab_id.chars().any(char::is_control)
        {
            return Err(
                "tab_id must identify an existing browser page using 1..1024 non-control bytes"
                    .into(),
            );
        }
        let origin = self
            .expected_origin
            .strip_prefix("https://")
            .or_else(|| self.expected_origin.strip_prefix("http://"))
            .ok_or("expected_origin must be an exact observed http(s) origin")?;
        if origin.is_empty()
            || origin.contains(['/', '?', '#', '@'])
            || origin.chars().any(|c| c.is_control() || c.is_whitespace())
        {
            return Err(
                "expected_origin must contain only the observed scheme, host and optional port"
                    .into(),
            );
        }
        Ok(())
    }
}

fn validate_browser_endpoint(endpoint: &str) -> Result<(), String> {
    if endpoint.len() > 8192
        || endpoint
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
        || endpoint.contains(['@', '#', '?'])
    {
        return Err("invalid explicit browser debugging endpoint".into());
    }
    let authority = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("ws://"))
        .ok_or("browser endpoint must use http:// or ws:// on loopback")?
        .split('/')
        .next()
        .unwrap_or("");
    let (host, port) = if let Some(authority) = authority.strip_prefix('[') {
        authority
            .split_once("]:")
            .ok_or("browser endpoint requires an explicit port")?
    } else {
        authority
            .rsplit_once(':')
            .ok_or("browser endpoint requires an explicit port")?
    };
    if !port.parse::<u16>().is_ok_and(|port| port > 0) {
        return Err("browser endpoint requires a nonzero valid port".into());
    }
    if !host.eq_ignore_ascii_case("localhost")
        && !host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    {
        return Err("browser endpoint must use localhost or a literal loopback IP".into());
    }
    if endpoint.contains("/devtools/page/") {
        return Err("use the browser debugging endpoint and an explicit existing tab_id".into());
    }
    Ok(())
}

pub(super) fn watch_operation_timeout(args: &Value) -> Result<Duration, String> {
    let value: WatchUntilClick = parse(args.clone())?;
    value.validate()?;
    // The requested duration bounds the watch itself. Connection and bounded
    // observer cleanup have separate limits and must fit before this deadline.
    Ok(Duration::from_millis(value.timeout_ms + 20_000))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct PointerFrame {
    point: Point,
    at_ms: u32,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WindowBinding {
    window_id: String,
    relative: bool,
    expected: Option<WindowGeometry>,
}

#[derive(Debug, PartialEq)]
struct WindowGuard {
    window_id: String,
    pid: u64,
    geometry: WindowGeometry,
}

#[cfg(target_os = "linux")]
fn click_window_binding(
    value: &Click,
    point: Option<Point>,
) -> Result<Option<WindowBinding>, String> {
    let space = value.coordinate_space.as_deref().unwrap_or("desktop");
    if !matches!(space, "desktop" | "window") {
        return Err("click coordinate_space must be desktop or window".into());
    }
    let Some(id) = &value.window_id else {
        if space == "window" || value.expected_window.is_some() {
            return Err("window-relative coordinates and expected_window require window_id".into());
        }
        return Ok(None);
    };
    if !valid_window_id(id) || point.is_none() {
        return Err("a window-bound click requires a valid window_id and both x and y".into());
    }
    if let Some(expected) = &value.expected_window {
        expected.region()?;
    }
    Ok(Some(WindowBinding {
        window_id: id.clone(),
        relative: space == "window",
        expected: value.expected_window.clone(),
    }))
}

fn read_window_guard(desktop: &zuse::platform::Desktop, id: &str) -> Result<WindowGuard, String> {
    window_guard(&windows_value(desktop)?, id)
}

fn window_guard(windows: &Value, id: &str) -> Result<WindowGuard, String> {
    let windows = windows["windows"]
        .as_array()
        .ok_or("window metadata is unavailable")?;
    let mut matches = windows.iter().filter(|window| window["window_id"] == id);
    let window = matches
        .next()
        .ok_or("target window is unavailable; refresh computer.windows")?;
    if matches.next().is_some() {
        return Err("target window identity is ambiguous".into());
    }
    if window["focused"] != true || window["mapped"] != true {
        return Err(
            "target window must be mapped and currently focused; no button input was sent".into(),
        );
    }
    let pid = window["pid"]
        .as_u64()
        .filter(|pid| *pid != 0)
        .ok_or("window-bound clicks require an observed native process identity")?;
    let geometry: WindowGeometry = serde_json::from_value(json!({
        "x":window["x"],"y":window["y"],"width":window["width"],"height":window["height"],"scale":window["scale"]
    }))
    .map_err(|_| "window-bound clicks require native geometry and scale metadata".to_owned())?;
    geometry.region()?;
    Ok(WindowGuard {
        window_id: id.into(),
        pid,
        geometry,
    })
}

fn resolve_window_point(
    binding: &WindowBinding,
    point: Point,
    window: &WindowGuard,
) -> Result<Point, String> {
    if binding
        .expected
        .as_ref()
        .is_some_and(|expected| expected != &window.geometry)
    {
        return Err(
            "target window geometry or scale changed since observation; locate the target again"
                .into(),
        );
    }
    let point = if binding.relative {
        // Desktop and window coordinates are logical. The observed scale is
        // retained as a guard, never multiplied into logical coordinates again.
        if point.x < 0 || point.y < 0 {
            return Err("window-local coordinates must be inside the target window".into());
        }
        Point {
            x: window
                .geometry
                .x
                .checked_add(point.x)
                .ok_or("window x overflow")?,
            y: window
                .geometry
                .y
                .checked_add(point.y)
                .ok_or("window y overflow")?,
        }
        .validate()?
    } else {
        point.validate()?
    };
    if !window.geometry.region()?.contains(point) {
        return Err("click point is outside the target window".into());
    }
    Ok(point)
}

fn verify_window_guard(
    desktop: &zuse::platform::Desktop,
    expected: &WindowGuard,
) -> Result<(), String> {
    if read_window_guard(desktop, &expected.window_id)? != *expected {
        return Err(
            "target window identity, geometry or scale changed; no further button input was sent"
                .into(),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn validate_num_lock(state: &str) -> Result<(), String> {
    if matches!(state, "on" | "off" | "toggle") {
        Ok(())
    } else {
        Err("Num Lock state must be on, off or toggle".into())
    }
}

fn timed_path(value: &MovePath) -> Result<Vec<PointerFrame>, String> {
    use zuse::motion::{Curve, FunctionalMotion, PathOptions};
    if !(2..=64).contains(&value.path.len()) || !(20..=5000).contains(&value.duration_ms) {
        return Err("move_path requires 2–64 points and duration_ms 20..5000".into());
    }
    if !(0.0..=5.0).contains(&value.jitter) {
        return Err("move_path jitter must be 0..5 logical pixels".into());
    }
    for &point in &value.path {
        point.validate()?;
    }
    let points = &value.path;
    let end = points.len() - 1;
    let closed = points.len() > 3 && points[0] == points[end];
    let motion = FunctionalMotion::new(
        |t| {
            let phase = t * end as f64;
            let segment = (phase as usize).min(end - 1);
            let u = phase - segment as f64;
            let a = if segment == 0 && closed {
                points[end - 1]
            } else {
                points[segment.saturating_sub(1)]
            };
            let b = points[segment];
            let c = points[segment + 1];
            let d = if segment + 2 > end && closed {
                points[1]
            } else {
                points[(segment + 2).min(end)]
            };
            let spline = |a, b, c, d| {
                let (a, b, c, d) = (f64::from(a), f64::from(b), f64::from(c), f64::from(d));
                b + 0.5
                    * u
                    * ((c - a)
                        + u * ((2.0 * a - 5.0 * b + 4.0 * c - d)
                            + u * (-a + 3.0 * b - 3.0 * c + d)))
            };
            zuse::motion::Point {
                x: spline(a.x, b.x, c.x, d.x),
                y: spline(a.y, b.y, c.y, d.y),
            }
        },
        PathOptions {
            duration_ms: value.duration_ms,
            steps: (value.duration_ms / 8).clamp(2, 600),
            easing: if closed { Curve::Linear } else { Curve::Smooth },
            jitter: value.jitter,
            speed_variation: value.speed_variation,
            seed: value.seed,
        },
    )
    .map_err(|e| e.to_string())?;
    motion
        .map(|s| {
            let s = s.map_err(|e| e.to_string())?;
            let point = Point {
                x: s.point.x.round() as i32,
                y: s.point.y.round() as i32,
            }
            .validate()?;
            Ok(PointerFrame {
                point,
                at_ms: s.at_ms,
            })
        })
        .collect()
}

fn validate_timed_frames(frames: &[PointerFrame]) -> Result<(), String> {
    if !(3..=601).contains(&frames.len())
        || frames.first().map(|f| f.at_ms) != Some(0)
        || !frames
            .last()
            .is_some_and(|f| (20..=5000).contains(&f.at_ms))
        || frames
            .windows(2)
            .any(|pair| pair[1].at_ms.saturating_sub(pair[0].at_ms) < 8)
    {
        return Err("invalid timed pointer frames".into());
    }
    for f in frames {
        f.point.validate()?;
    }
    Ok(())
}

fn play_timed_path(
    frames: &[PointerFrame],
    cancellation: &Cancellation,
    mut move_to: impl FnMut(Point) -> Result<(), String>,
) -> Result<usize, String> {
    validate_timed_frames(frames)?;
    let start = std::time::Instant::now();
    let mut delivered = 0;
    for (index, frame) in frames.iter().enumerate() {
        let due = Duration::from_millis(u64::from(frame.at_ms));
        let elapsed = start.elapsed();
        // If a backend stalls, skip expired interior frames instead of
        // replaying them in a burst. Always keep the endpoints.
        if index > 0
            && frames
                .get(index + 1)
                .is_some_and(|next| Duration::from_millis(u64::from(next.at_ms)) <= elapsed)
        {
            continue;
        }
        if let Some(delay) = due.checked_sub(elapsed) {
            cancellation.delay(delay)?;
        }
        cancellation.submit(|| move_to(frame.point))?;
        delivered += 1;
    }
    Ok(delivered)
}

fn validate_key(value: &Key) -> Result<(), String> {
    if value.key.is_empty()
        || value.key.len() > 64
        || !value
            .key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err("key must be an XKB key name using letters, digits or underscores".into());
    }
    if value.modifiers.len() > 4 {
        return Err("at most four modifiers are supported".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for modifier in &value.modifiers {
        if !matches!(modifier.as_str(), "ctrl" | "alt" | "shift" | "super")
            || !seen.insert(modifier)
        {
            return Err("modifiers must be unique ctrl, alt, shift or super values".into());
        }
    }
    Ok(())
}

fn validate_capture(value: &Screenshot, desktop: Region) -> Result<Region, String> {
    if !(320..=2560).contains(&value.max_width) || !(30..=90).contains(&value.quality) {
        return Err("max_width must be 320–2560; quality must be 30–90".into());
    }
    let region = value.region.unwrap_or(desktop).validate()?;
    if i64::from(region.x) < i64::from(desktop.x)
        || i64::from(region.y) < i64::from(desktop.y)
        || i64::from(region.x) + i64::from(region.width)
            > i64::from(desktop.x) + i64::from(desktop.width)
        || i64::from(region.y) + i64::from(region.height)
            > i64::from(desktop.y) + i64::from(desktop.height)
    {
        return Err("capture region is outside desktop bounds".into());
    }
    Ok(region)
}

fn button_name(button: Button) -> &'static str {
    match button {
        Button::Left => "left",
        Button::Middle => "middle",
        Button::Right => "right",
    }
}

#[derive(Default)]
struct Cancellation {
    cancelled: StdMutex<bool>,
    changed: Condvar,
    host_ticket: OnceLock<host_control::WorkerTicket>,
    observation: OnceLock<zuse::observation::Cancellation>,
}

impl Cancellation {
    fn cancel(&self) {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.changed.notify_all();
        if let Some(observation) = self.observation.get() {
            observation.cancel();
        }
    }
    fn check(&self) -> Result<(), String> {
        if *self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner())
        {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    }
    fn submit<T>(&self, operation: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        let cancelled = self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if *cancelled {
            return Err(CANCELLED.into());
        }
        // Cancellation and native submission share this lock. Once cancel()
        // returns, no later frame can pass this boundary, including an endpoint.
        if let Some(ticket) = self.host_ticket.get() {
            ticket.submit(operation)
        } else {
            operation()
        }
    }
    fn delay(&self, duration: Duration) -> Result<(), String> {
        let lock = self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (lock, _) = self
            .changed
            .wait_timeout_while(lock, duration, |cancelled| !*cancelled)
            .unwrap_or_else(|error| error.into_inner());
        if *lock { Err(CANCELLED.into()) } else { Ok(()) }
    }

    fn watch_token(&self) -> Result<zuse::observation::Cancellation, String> {
        let ticket = self
            .host_ticket
            .get()
            .cloned()
            .ok_or("watch_until_click worker requires the original host-input ticket")?;
        let token = zuse::observation::Cancellation::with_dispatch_guard(move || {
            Ok(Box::new(ticket.acquire_input_dispatch()?) as Box<dyn Send>)
        });
        self.observation
            .set(token.clone())
            .map_err(|_| "worker observation cancellation was already bound".to_owned())?;
        // Covers EOF arriving before the token was registered. A later EOF
        // calls cancel() on this exact token and drains its dispatch gate.
        if let Err(error) = self.check() {
            token.cancel();
            return Err(error);
        }
        Ok(token)
    }
}

fn read_request(reader: &mut impl BufRead) -> Result<Request, String> {
    let mut bytes = Vec::new();
    std::io::Read::take(reader, (REQUEST_LIMIT + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > REQUEST_LIMIT || bytes.last() != Some(&b'\n') {
        return Err("zuse worker request must be one bounded JSON line".into());
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid zuse worker request: {error}"))
}

fn worker_main() -> Result<(), String> {
    {
        let mut output = std::io::stdout().lock();
        output
            .write_all(WORKER_READY)
            .and_then(|_| output.flush())
            .map_err(|error| error.to_string())?;
    }
    let cancellation = Arc::new(Cancellation::default());
    let (sender, receiver) = mpsc::sync_channel(1);
    let reading = cancellation.clone();
    // The reader also observes EOF while a native operation is in progress.
    let input = std::thread::Builder::new()
        .name("ssmcp-zuse-input".into())
        .spawn(move || {
            let mut reader = std::io::stdin().lock();
            let request = read_request(&mut reader);
            if sender.send(request).is_err() {
                return;
            }
            // Only one operation is accepted per owned worker; any further byte
            // cancels it rather than injecting a second operation after revocation.
            let mut byte = [0u8; 1];
            let _ = std::io::Read::read(&mut reader, &mut byte);
            reading.cancel();
        })
        .map_err(|error| error.to_string())?;
    let request = receiver.recv().map_err(|error| error.to_string())??;
    let mut cleanup_ok = true;
    let outcome = execute(request, &cancellation, &mut cleanup_ok);
    let reply = match outcome {
        Ok(value) => Reply {
            value: Some(value),
            error: None,
            cleanup_ok,
        },
        Err(error) => Reply {
            value: None,
            error: Some(bounded(&error, 2048)),
            cleanup_ok,
        },
    };
    let bytes = serde_json::to_vec(&reply).map_err(|error| error.to_string())?;
    if bytes.len() > RESPONSE_LIMIT {
        return Err("zuse worker result exceeds its output limit".into());
    }
    let mut output = std::io::stdout().lock();
    output
        .write_all(&bytes)
        .and_then(|_| output.write_all(b"\n"))
        .and_then(|_| output.flush())
        .map_err(|error| error.to_string())?;
    // Normal completion and cancelled calls both wait for parent EOF. A zero
    // exit status therefore acknowledges that native input cleanup is done.
    input
        .join()
        .map_err(|_| "zuse input monitor panicked".to_owned())?;
    if cleanup_ok {
        Ok(())
    } else {
        Err("zuse native input release is unconfirmed".into())
    }
}

fn execute(
    mut request: Request,
    cancellation: &Cancellation,
    cleanup_ok: &mut bool,
) -> Result<Value, String> {
    if let Some(ticket) = request.host_ticket.take() {
        cancellation
            .host_ticket
            .set(ticket)
            .map_err(|_| "worker input ticket was already bound".to_owned())?;
    }
    cancellation.check()?;
    if request.operation == "watch_until_click" {
        let value: WatchUntilClick = parse(request.arguments)?;
        value.validate()?;
        let token = cancellation.watch_token()?;
        let mut browser = zuse::browser::Browser::connect_existing_cancellable(
            &value.endpoint,
            &value.tab_id,
            &token,
        )?;
        let outcome = browser.watch_until_click(&value.options(), &token);
        // The attached browser owns no user tab. Its short, bounded close only
        // unregisters observers and disconnects, including after cancellation.
        let close_error = browser.close().err();
        let mut receipt = click_watch_receipt(outcome, cleanup_ok)?;
        if !*cleanup_ok {
            return Err(format!(
                "browser click-watch input release is unconfirmed; no action was replayed: {receipt}"
            ));
        }
        if let Some(error) = close_error {
            receipt["connection_cleanup_error"] = json!(error);
            if receipt["status"] == "completed" {
                receipt["status"] = json!("cleanup_failed");
            }
        }
        return Ok(json!({"result":receipt,"backend":"cdp"}));
    }
    #[cfg(windows)]
    windows::interactive_desktop()?;
    let mut desktop = zuse::platform::Desktop::connect()?;
    cancellation.check()?;
    let backend = desktop.backend();
    let args = request.arguments;
    let result = match request.operation.as_str() {
        "capabilities" => {
            parse::<Empty>(args)?;
            serde_json::from_str(&desktop.capabilities())
                .map_err(|error| format!("invalid zuse capabilities: {error}"))?
        }
        "windows" => windows_value(&desktop)?,
        "focus" => {
            let value: Focus = parse(args)?;
            if !valid_window_id(&value.window_id) {
                return Err("invalid native window ID".into());
            }
            let windows = windows_value(&desktop)?;
            if !windows["windows"].as_array().is_some_and(|windows| {
                windows
                    .iter()
                    .any(|window| window["window_id"] == value.window_id)
            }) {
                return Err(
                    "window is not in the current window list; refresh computer.windows".into(),
                );
            }
            cancellation.submit(|| desktop.focus(&value.window_id))?;
            json!({"focused":value.window_id})
        }
        "move" => {
            let value: Point = parse(args)?;
            value.validate()?;
            cancellation.submit(|| desktop.move_to(value.x, value.y))?;
            json!({})
        }
        "move_path" if cfg!(target_os = "linux") => {
            let frames: Vec<PointerFrame> = parse(args)?;
            let delivered = play_timed_path(&frames, cancellation, |point| {
                desktop.move_to(point.x, point.y)
            })?;
            let endpoint = frames.last().ok_or("trajectory has no endpoint")?;
            json!({"samples":frames.len(),"delivered":delivered,"x":endpoint.point.x,"y":endpoint.point.y,"duration_ms":endpoint.at_ms,"coordinate_space":coordinate_space()})
        }
        "click" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ClickRequest {
                point: Option<Point>,
                button: Button,
                count: u8,
                window: Option<WindowBinding>,
            }
            let value: ClickRequest = parse(args)?;
            if !(1..=3).contains(&value.count) {
                return Err("invalid click count".into());
            }
            let guard = value
                .window
                .as_ref()
                .map(|binding| {
                    if !valid_window_id(&binding.window_id) {
                        return Err("invalid native window ID".into());
                    }
                    read_window_guard(&desktop, &binding.window_id)
                })
                .transpose()?;
            let point = if let Some(binding) = &value.window {
                let point = value
                    .point
                    .ok_or("a window-bound click requires coordinates")?;
                Some(resolve_window_point(
                    binding,
                    point,
                    guard.as_ref().ok_or("window guard unavailable")?,
                )?)
            } else {
                value.point
            };
            if let Some(point) = point {
                point.validate()?;
                cancellation.submit(|| desktop.move_to(point.x, point.y))?;
            }
            for index in 0..value.count {
                with_button(
                    &mut desktop,
                    button_name(value.button),
                    cleanup_ok,
                    cancellation,
                    |desktop| {
                        if let Some(guard) = &guard {
                            verify_window_guard(desktop, guard).map_err(|error| {
                                format!("{error}; {index} click(s) completed; do not retry automatically")
                            })?;
                        }
                        Ok(())
                    },
                    |_| Ok(()),
                )?;
                if index + 1 < value.count {
                    cancellation.delay(Duration::from_millis(40))?;
                }
            }
            json!({"clicked":true,"count":value.count,"point":point,"window_id":guard.as_ref().map(|guard| &guard.window_id),"coordinate_space":coordinate_space()})
        }
        "drag" => {
            let value: Drag = parse(args)?;
            if !(2..=64).contains(&value.path.len()) || value.duration_ms > 5000 {
                return Err("invalid drag".into());
            }
            for point in &value.path {
                point.validate()?;
            }
            cancellation.submit(|| desktop.move_to(value.path[0].x, value.path[0].y))?;
            with_button(
                &mut desktop,
                button_name(value.button),
                cleanup_ok,
                cancellation,
                |_| Ok(()),
                |desktop| {
                    let start = std::time::Instant::now();
                    for (index, point) in value.path[1..].iter().enumerate() {
                        let due = Duration::from_millis(
                            value.duration_ms * (index + 1) as u64 / (value.path.len() - 1) as u64,
                        );
                        if let Some(delay) = due.checked_sub(start.elapsed()) {
                            cancellation.delay(delay)?;
                        }
                        cancellation.submit(|| desktop.move_to(point.x, point.y))?;
                    }
                    Ok(())
                },
            )?;
            json!({})
        }
        "key" => {
            let value: Key = parse(args)?;
            validate_key(&value)?;
            let mut parts = value.modifiers;
            parts.push(value.key);
            if let Err(error) = cancellation.submit(|| desktop.key(&parts.join("+"))) {
                *cleanup_ok = clean_cancellation(&error)
                    || input_error_precedes_effect(backend, &error, false);
                return Err(error);
            }
            json!({})
        }
        #[cfg(target_os = "linux")]
        "keyboard_state" => {
            parse::<Empty>(args)?;
            serde_json::from_str(&desktop.keyboard_status()?)
                .map_err(|error| format!("invalid native keyboard state: {error}"))?
        }
        #[cfg(target_os = "linux")]
        "num_lock" => execute_num_lock(args, cancellation, cleanup_ok, |state| {
            desktop.num_lock(state)
        })?,
        "type" => {
            let value: TypeText = parse(args)?;
            validate_literal_text(backend, &value.text)?;
            if backend == "x11" {
                // The pinned zuse adapter chooses keys or confirmed AT-SPI
                // delivery for the entire string before any input. Splitting
                // here could type a prefix before a later scalar is refused.
                if let Err(error) = cancellation.submit(|| desktop.type_text(&value.text)) {
                    *cleanup_ok = clean_cancellation(&error)
                        || input_error_precedes_effect(backend, &error, true);
                    return Err(error);
                }
                if cancellation.check().is_err() {
                    return Err("zuse operation cancelled after X11 text dispatch; text may have changed; do not retry".into());
                }
            } else {
                let mut buffer = [0u8; 4];
                for character in value.text.chars() {
                    if let Err(error) = cancellation
                        .submit(|| desktop.type_text(character.encode_utf8(&mut buffer)))
                    {
                        *cleanup_ok = clean_cancellation(&error)
                            || input_error_precedes_effect(backend, &error, true);
                        return Err(error);
                    }
                }
            }
            json!({})
        }
        "scroll" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ScrollRequest {
                point: Option<Point>,
                dx: i32,
                dy: i32,
                steps: u8,
            }
            let value: ScrollRequest = parse(args)?;
            if !(1..=100).contains(&value.steps) || value.dx.abs_diff(0) + value.dy.abs_diff(0) != 1
            {
                return Err("invalid scroll".into());
            }
            if let Some(point) = value.point {
                point.validate()?;
                cancellation.submit(|| desktop.move_to(point.x, point.y))?;
            }
            for _ in 0..value.steps {
                cancellation.submit(|| desktop.scroll(value.dx, value.dy))?;
            }
            json!({})
        }
        "capture" => {
            return capture(
                &desktop,
                args,
                request
                    .capture_path
                    .ok_or("capture requires a parent-owned output path")?,
                cancellation,
            );
        }
        _ => return Err("unknown zuse worker operation".into()),
    };
    Ok(json!({"result":result,"backend":backend}))
}

#[cfg(target_os = "linux")]
fn execute_num_lock(
    args: Value,
    cancellation: &Cancellation,
    cleanup_ok: &mut bool,
    operation: impl FnOnce(&str) -> Result<String, String>,
) -> Result<Value, String> {
    let value: NumLock = parse(args)?;
    validate_num_lock(&value.state)?;
    let status = cancellation.submit(|| {
        // Native errors do not confirm release. A cancelled or rejected
        // submission never invokes this closure and leaves cleanup unchanged.
        operation(&value.state).inspect_err(|_| *cleanup_ok = false)
    })?;
    serde_json::from_str(&status)
        .map_err(|error| format!("invalid native Num Lock readback: {error}"))
}

fn click_watch_receipt(
    outcome: Result<String, String>,
    cleanup_ok: &mut bool,
) -> Result<Value, String> {
    let text = match outcome {
        Ok(receipt) => receipt,
        Err(error) => {
            let Some((status, receipt)) = error.split_once(": ") else {
                return Err(error);
            };
            if !matches!(
                status,
                "cancelled" | "timeout" | "stopped" | "cleanup_failed"
            ) || !receipt.starts_with('{')
            {
                // These are preflight failures before the watch can dispatch.
                return Err(error);
            }
            receipt.to_owned()
        }
    };
    *cleanup_ok = false;
    if text.len() > RESPONSE_LIMIT / 2 {
        return Err("browser click-watch receipt exceeds its bounded transport limit; input state is unconfirmed".into());
    }
    let receipt: Value = serde_json::from_str(&text)
        .map_err(|error| format!("invalid browser click-watch receipt: {error}"))?;
    if receipt["action"] != "watch_until_click"
        || !matches!(
            receipt["status"].as_str(),
            Some("completed" | "cancelled" | "timeout" | "stopped" | "cleanup_failed")
        )
    {
        return Err("browser click-watch receipt has an unknown action or status".into());
    }
    let clicks = receipt["clicks"]
        .as_array()
        .ok_or("browser click-watch receipt has no dispatch list")?;
    if clicks.len() > 64 || receipt["dispatch_attempts"].as_u64() != Some(clicks.len() as u64) {
        return Err("browser click-watch receipt has inconsistent dispatch counts".into());
    }
    // An attempted frame can have reached the socket only partially even when
    // no complete write was reported. Only an acknowledged matching release
    // proves that a dispatch is finished; an empty dispatch list is also clean.
    *cleanup_ok = clicks.iter().all(|click| {
        click["dispatch"]["sent_mouse_events"].as_u64() == Some(2)
            && click["dispatch"]["release_confirmed"] == true
    });
    Ok(receipt)
}

fn validate_literal_text(backend: &str, text: &str) -> Result<(), String> {
    if text.len() > 16384 || text.contains('\0') {
        return Err("invalid literal text".into());
    }
    if backend == "wayland" {
        // zuse validates the whole string before creating a virtual keyboard.
        // Keep that guarantee when splitting native calls for cancellation.
        for (offset, character) in text.char_indices() {
            if character.is_control() && !matches!(character, '\t' | '\n' | '\r') {
                return Err(format!(
                    "unsupported control U+{:04X} at byte {offset}; text permits only Tab, LF and CR",
                    character as u32
                ));
            }
        }
    }
    Ok(())
}

fn input_error_precedes_effect(backend: &str, error: &str, typing: bool) -> bool {
    // zuse exposes native failures as String errors. These branches are verified
    // against the locked library source and precede native input mutation.
    // Unknown native/runtime errors remain unconfirmed and keep input locked.
    if backend == "x11" {
        // x11_input::unsupported wraps only failures before its first mutation.
        // Match its complete outer envelope, before worker_main bounds errors.
        // The post-mutation uncertain envelope must never be treated as clean.
        let text_refused = typing
            && (error
                .strip_prefix("Unsupported X11 Unicode text: ")
                .and_then(|reason| reason.strip_suffix("; no input was sent"))
                .is_some_and(|reason| !reason.is_empty())
                || matches!(
                    error,
                    "Unsupported X11 Unicode text on a remote display; no input was sent"
                        | "X11 focus, target identity or geometry changed; no input was sent"
                ));
        return text_refused
            || error == "XTest extension library unavailable"
            || (!typing
                && (error.starts_with("unknown X11 keysym ")
                    || (error.starts_with("key ")
                        && error.ends_with(" is absent from the X11 keyboard map"))))
            || (typing
                && matches!(
                    error,
                    "invalid X11 keyboard-map range"
                        | "could not read X11 keyboard mapping"
                        | "invalid X11 keyboard mapping width"
                        | "Shift is absent from the X11 keyboard map"
                ));
    }
    if backend == "wayland" {
        // These exact state checks precede shortcut dispatch and virtual-keyboard
        // creation in the pinned zuse 0.3.0. A generic prefix could also
        // accept a new error or appended restoration failure, so keep it exact.
        return matches!(
            error,
            "unsupported_state: keyboard state inspection requires Hyprland 0.55 or newer"
                | "unsupported_state: a keyboard key is held; release it before input"
                | "unsupported_state: compositor cannot report held keyboard keys"
                | "unsupported_state: Wayland keys require Hyprland keyboard-state inspection"
                | "unsupported_state: cannot parse compositor keyboard state"
                | "unsupported_state: compositor keyboard state is unavailable"
                | "unsupported_state: too many keyboards to inspect"
                | "unsupported_state: Caps Lock is enabled; disable it before input"
                | "unsupported_state: Num Lock is enabled; disable it before input"
                | "unsupported_state: Caps Lock state is unavailable"
                | "unsupported_state: Num Lock state is unavailable"
                | "unsupported_state: Num Lock differs between the active and other keyboards"
                | "unsupported_state: cannot restore the same active keyboard after a Num Lock chord"
                | "unsupported_state: cannot restore the same active keyboard after virtual input"
                | "unsupported_state: keyboard layout group must be 0 before input"
                | "unsupported_state: keyboard layout group is unavailable"
                | "unsupported_state: a unique main keyboard is required"
        );
    }
    if backend == "win32" && !typing {
        return error.starts_with("unknown key ") || error == "function key must be F1..F24";
    }
    if backend == "quartz" {
        return error.starts_with("macOS Accessibility permission is required")
            || (!typing && error.starts_with("unknown macOS virtual key "));
    }
    false
}

fn clean_cancellation(error: &str) -> bool {
    matches!(error, CANCELLED | host_control::ENDED)
}

fn with_button<T>(
    desktop: &mut zuse::platform::Desktop,
    button: &str,
    cleanup_ok: &mut bool,
    cancellation: &Cancellation,
    before_down: impl FnOnce(&zuse::platform::Desktop) -> Result<(), String>,
    operation: impl FnOnce(&mut zuse::platform::Desktop) -> Result<T, String>,
) -> Result<T, String> {
    struct Release<'a> {
        desktop: &'a mut zuse::platform::Desktop,
        button: &'a str,
        armed: bool,
    }
    impl Drop for Release<'_> {
        fn drop(&mut self) {
            if self.armed {
                let _ = self.desktop.button(self.button, "up");
            }
        }
    }
    let mut release = Release {
        desktop,
        button,
        armed: false,
    };
    let down = cancellation.submit(|| {
        before_down(release.desktop)?;
        release.armed = true;
        release.desktop.button(button, "down")
    });
    if !release.armed {
        return Err(down.unwrap_err());
    }
    if let Err(error) = &down
        && input_error_precedes_effect(release.desktop.backend(), error, false)
    {
        release.armed = false;
        return Err(error.clone());
    }
    let outcome = down.and_then(|_| operation(release.desktop));
    let released = release.desktop.button(button, "up");
    if released.is_ok() {
        release.armed = false;
    } else {
        *cleanup_ok = false;
    }
    released?;
    outcome
}

// Normalize only the public adapter boundary. Internal worker focus continues
// to use zuse's native handle, while the Windows host adapter verifies HWND+PID.
fn normalize_window_identities(backend: &str, metadata: &mut Value) -> Result<(), String> {
    if backend != "win32" {
        return Ok(());
    }
    let windows = metadata["windows"]
        .as_array_mut()
        .ok_or("zuse returned no Windows window list")?;
    for window in windows {
        let id = super::windows_identity::from_zuse(&window["window_id"], &window["pid"])?;
        window["window_id"] = json!(id);
    }
    Ok(())
}

fn windows_value(desktop: &zuse::platform::Desktop) -> Result<Value, String> {
    let raw = desktop.windows()?;
    if raw.len() > 4 * 1024 * 1024 {
        return Err("native window metadata exceeds 4 MiB".into());
    }
    let windows: Vec<Value> = serde_json::from_str(&raw)
        .map_err(|error| format!("invalid zuse window metadata: {error}"))?;
    let mapped: Vec<Value> = windows.iter().take(100).map(|window| json!({
        "window_id":window["id"],"title":bounded(window["title"].as_str().unwrap_or(""),512),
        "app":bounded(window["class"].as_str().unwrap_or(""),128),"pid":window["pid"],
        "x":window["x"],"y":window["y"],"width":window["width"],"height":window["height"],
        "workspace":window["workspace"],"wine":window["wine"],
        "scale":window["scale"],"focused":window["focused"],"mapped":window["mapped"]
    })).collect();
    Ok(
        json!({"windows":mapped,"truncated":windows.len()>100,"coordinate_space":coordinate_space()}),
    )
}

struct CaptureDirectory(PathBuf);
impl CaptureDirectory {
    fn new() -> Result<Self, String> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(|error| error.to_string())?;
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir().join(format!("ssmcp-zuse-{}-{suffix}", std::process::id()));
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder.create(&path).map_err(|error| error.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn capture(
    desktop: &zuse::platform::Desktop,
    args: Value,
    path: PathBuf,
    cancellation: &Cancellation,
) -> Result<Value, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CaptureRequest {
        desktop: Region,
        region: Region,
        max_width: u32,
        quality: u8,
        cursor_requested: bool,
    }
    let request: CaptureRequest = parse(args)?;
    request.desktop.validate()?;
    validate_capture(
        &Screenshot {
            region: Some(request.region),
            max_width: request.max_width,
            quality: request.quality,
            cursor: request.cursor_requested,
        },
        request.desktop,
    )?;
    cancellation.check()?;
    if !path.is_absolute() || path.file_name() != Some(std::ffi::OsStr::new("capture.png")) {
        return Err("capture output must be the parent-owned absolute capture.png path".into());
    }
    desktop.screenshot(&path, None)?;
    cancellation.check()?;
    let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > 256 * 1024 * 1024 {
        return Err("native capture exceeds 256 MiB".into());
    }
    let mut reader =
        image::ImageReader::with_format(std::io::BufReader::new(file), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|error| format!("invalid zuse capture: {error}"))?;
    encode_capture(
        image,
        request.desktop,
        request.region,
        request.max_width,
        request.quality,
        request.cursor_requested,
        desktop.backend(),
    )
}

fn encode_capture(
    image: image::DynamicImage,
    desktop: Region,
    region: Region,
    max_width: u32,
    quality: u8,
    cursor_requested: bool,
    backend: &str,
) -> Result<Value, String> {
    let sx = f64::from(image.width()) / f64::from(desktop.width);
    let sy = f64::from(image.height()) / f64::from(desktop.height);
    let x = (f64::from(region.x - desktop.x) * sx).round() as u32;
    let y = (f64::from(region.y - desktop.y) * sy).round() as u32;
    let right = ((i64::from(region.x) - i64::from(desktop.x) + i64::from(region.width)) as f64 * sx)
        .round() as u32;
    let bottom = ((i64::from(region.y) - i64::from(desktop.y) + i64::from(region.height)) as f64
        * sy)
        .round() as u32;
    if x >= right || y >= bottom || right > image.width() || bottom > image.height() {
        return Err("native capture dimensions do not contain the requested region".into());
    }
    let source = image.crop_imm(x, y, right - x, bottom - y).into_rgb8();
    let mut width = source.width().min(max_width);
    for _ in 0..6 {
        let height = ((f64::from(source.height()) * f64::from(width) / f64::from(source.width()))
            .round() as u32)
            .max(1);
        let scaled = image::imageops::resize(
            &source,
            width,
            height,
            image::imageops::FilterType::Triangle,
        );
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(&scaled)
            .map_err(|error| error.to_string())?;
        if bytes.len() <= IMAGE_LIMIT {
            let metadata = json!({"mime_type":"image/jpeg","width":width,"height":height,"bytes":bytes.len(),
                "coordinate_space":coordinate_space(),"region":region,"cursor":false,"cursor_requested":cursor_requested,
                "engine":"zuse","engine_version":zuse::VERSION,"backend":backend,
                "image_to_desktop":{"origin_x":region.x,"origin_y":region.y,"scale_x":f64::from(region.width)/f64::from(width),"scale_y":f64::from(region.height)/f64::from(height)}});
            return Ok(json!({"data":STANDARD.encode(bytes),"metadata":metadata}));
        }
        width = (width * 7 / 10).max(1);
    }
    Err("screenshot exceeds 400 KiB after bounded downscaling; select a smaller region".into())
}

#[cfg(all(test, target_os = "linux"))]
mod browser_tests;
#[cfg(all(test, target_os = "linux"))]
mod wayland_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn isolated_worker_inherits_ingress_generation_without_rearming() {
        let control = Arc::new(host_control::HostControl::default());
        control.set_local(true).unwrap();
        let permit = control.permit().unwrap();
        let private = Computer::new(std::env::temp_dir()).unwrap();
        // This is intentionally a different store. The private Computer must
        // not select its own setting when the ingress permit is forwarded.
        private.host_control.set_local(false).unwrap();
        let inherited = permit
            .scope(async { worker_ticket_for_dispatch(&private, "move_path", None) })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inherited.submit(|| Ok(7)), Ok(7));

        private.host_control.set_local(true).unwrap();
        control.set_local(false).unwrap();
        control.set_local(true).unwrap();
        assert!(private.host_control.ticket().is_ok());
        assert_eq!(inherited.submit(|| Ok(())), Err(host_control::ENDED.into()));
        let error = permit
            .scope(async { worker_ticket_for_dispatch(&private, "move_path", None) })
            .await
            .err()
            .unwrap();
        assert_eq!(error, host_control::ENDED);
        assert!(worker_ticket_for_dispatch(&private, "key", None).is_err());
        assert!(
            worker_ticket_for_dispatch(&private, "keyboard_state", None)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn num_lock_uncertain_native_write_marks_worker_cleanup_unconfirmed() {
        for detail in [
            "native Num Lock input was incomplete (24/96 bytes); input was not replayed",
            "write Num Lock to the current keyboard: device disconnected; input was not replayed",
        ] {
            let mut cleanup_ok = true;
            let error = execute_num_lock(
                json!({"state":"toggle"}),
                &Cancellation::default(),
                &mut cleanup_ok,
                |state| {
                    assert_eq!(state, "toggle");
                    Err(detail.into())
                },
            )
            .unwrap_err();
            assert_eq!(error, detail);
            assert!(
                !cleanup_ok,
                "the worker must not acknowledge release: {error}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn num_lock_pre_dispatch_cancellation_and_success_preserve_cleanup() {
        let cancellation = Cancellation::default();
        let mut cleanup_ok = true;
        let status = execute_num_lock(
            json!({"state":"on"}),
            &cancellation,
            &mut cleanup_ok,
            |state| {
                assert_eq!(state, "on");
                Ok(json!({"num_lock":true}).to_string())
            },
        )
        .unwrap();
        assert_eq!(status, json!({"num_lock":true}));
        assert!(cleanup_ok);

        cancellation.cancel();
        let error = execute_num_lock(
            json!({"state":"toggle"}),
            &cancellation,
            &mut cleanup_ok,
            |_| panic!("a cancelled Num Lock request reached native input"),
        )
        .unwrap_err();
        assert_eq!(error, CANCELLED);
        assert!(cleanup_ok);
    }

    fn click_watch_arguments(endpoint: &str) -> Value {
        json!({"endpoint":endpoint,"tab_id":"owned-existing-page","scope":"#owned-scope",
            "selector":"button.ready","expected_origin":"https://example.test",
            "max_clicks":1,"timeout_ms":10000})
    }

    #[test]
    fn click_watch_rejects_remote_endpoints_and_requires_explicit_bounds() {
        for endpoint in [
            "http://localhost:9222",
            "ws://127.0.0.1:9222/devtools/browser/owned",
            "http://[::1]:9222",
        ] {
            let args: WatchUntilClick = parse(click_watch_arguments(endpoint)).unwrap();
            args.validate().unwrap();
        }
        for endpoint in [
            "http://example.test:9222",
            "ws://192.0.2.1:9222",
            "http://127.0.0.1:0",
            "http://localhost",
            "ws://127.0.0.1:9222/devtools/page/other",
            "http://user@127.0.0.1:9222",
            "ws://127.0.0.1:9222/\r\ninjected",
        ] {
            let args: WatchUntilClick = parse(click_watch_arguments(endpoint)).unwrap();
            assert!(args.validate().is_err(), "{endpoint}");
        }
        for field in [
            "tab_id",
            "scope",
            "selector",
            "expected_origin",
            "max_clicks",
            "timeout_ms",
        ] {
            let mut args = click_watch_arguments("http://127.0.0.1:9222");
            args.as_object_mut().unwrap().remove(field);
            assert!(parse::<WatchUntilClick>(args).is_err(), "missing {field}");
        }
        let tools = Computer::tools();
        let schema = tools
            .iter()
            .find(|tool| tool["name"] == "ssmcp.computer.watch_until_click")
            .unwrap();
        assert_eq!(
            schema["inputSchema"]["properties"]["desktop"]["enum"],
            json!(["host"])
        );
        assert!(
            schema["inputSchema"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("desktop"))
        );
        assert!(!routing::platform_input(
            "windows",
            "ssmcp.computer.watch_until_click"
        ));
        assert!(!routing::platform_input(
            "macos",
            "ssmcp.computer.watch_until_click"
        ));
    }

    #[test]
    fn click_watch_cleanup_requires_the_original_release_acknowledgement() {
        for (sent, confirmed, expected) in [
            (0, false, false),
            (1, false, false),
            (2, false, false),
            (2, true, true),
        ] {
            let receipt = json!({"action":"watch_until_click","status":"cancelled","dispatch_attempts":1,
                "clicks":[{"dispatch":{"sent_mouse_events":sent,"release_confirmed":confirmed},"received":null}]});
            let mut clean = true;
            let returned =
                click_watch_receipt(Err(format!("cancelled: {receipt}")), &mut clean).unwrap();
            assert_eq!(returned, receipt);
            assert_eq!(clean, expected);
        }
        let receipt = json!({"action":"watch_until_click","status":"cancelled","dispatch_attempts":0,"clicks":[]});
        let mut clean = false;
        click_watch_receipt(Err(format!("cancelled: {receipt}")), &mut clean).unwrap();
        assert!(
            clean,
            "cancellation before any dispatch leaves no input to release"
        );
        let mut clean = true;
        assert!(
            click_watch_receipt(
                Err("cancelled: {\"action\":\"watch_until_click\"}".into()),
                &mut clean
            )
            .is_err()
        );
        assert!(
            !clean,
            "malformed receipts cannot acknowledge input cleanup"
        );
    }

    #[test]
    fn click_watch_cancellation_is_bound_even_when_eof_arrives_before_registration() {
        let control = host_control::HostControl::default();
        let ticket = control.worker_ticket(control.ticket().unwrap()).unwrap();
        let before = Cancellation::default();
        assert!(before.host_ticket.set(ticket.clone()).is_ok());
        before.cancel();
        assert!(before.watch_token().is_err());
        let after = Cancellation::default();
        assert!(after.host_ticket.set(ticket).is_ok());
        let token = after.watch_token().unwrap();
        after.cancel();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn click_watch_host_off_cancels_the_owned_worker_during_attachment() {
        let directory = CaptureDirectory::new().unwrap();
        let computer = Computer::new(directory.0.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "ws://{}/devtools/browser/owned",
            listener.local_addr().unwrap()
        );
        let mut args = click_watch_arguments(&endpoint);
        let isolated = computer
            .perform("ssmcp.computer.watch_until_click", args.clone())
            .await
            .unwrap_err();
        assert!(isolated.contains("explicit desktop: host"));
        args["desktop"] = json!("host");
        computer.set_host_input_local(false).unwrap();
        assert_eq!(
            computer
                .perform("ssmcp.computer.watch_until_click", args.clone())
                .await
                .unwrap_err(),
            host_control::LOCKED
        );
        computer.set_host_input_local(true).unwrap();
        let mut operation = Box::pin(computer.perform("ssmcp.computer.watch_until_click", args));
        let (mut peer, _) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                accepted = listener.accept() => accepted.unwrap(),
                result = &mut operation => panic!("watch ended before owned attachment: {result:?}"),
            }
        }).await.unwrap();
        let mut handshake = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handshake.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                peer.read_exact(&mut byte).await.unwrap();
                handshake.push(byte[0]);
                assert!(handshake.len() < 16384);
            }
        })
        .await
        .unwrap();
        computer.set_host_input_local(false).unwrap();
        let error = tokio::time::timeout(Duration::from_secs(4), operation)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error, host_control::ENDED);
        assert!(!HOST_INPUT_CLEANUP_FAILED.load(Ordering::Acquire));
        let mut remainder = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), peer.read_to_end(&mut remainder))
            .await
            .unwrap()
            .unwrap();
        assert!(
            remainder.is_empty(),
            "revocation must not continue CDP attachment or dispatch input"
        );
        computer.shutdown().await.unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_starts_after_the_running_executable_is_unlinked() {
        let executable = std::env::current_exe().unwrap();
        let link =
            executable.with_file_name(format!("ssmcp-unlinked-worker-test-{}", std::process::id()));
        std::fs::hard_link(&executable, &link).unwrap();
        let output = std::process::Command::new(&link)
            .args([
                "--exact",
                "computer::zuse_backend::tests::unlinked_worker_fixture",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env("SSMCP_UNLINKED_WORKER_FIXTURE", &link)
            .stdin(Stdio::null())
            .output();
        let _ = std::fs::remove_file(&link);
        let output = output.unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "selected by the unlinked-executable regression"]
    fn unlinked_worker_fixture() {
        let Some(path) = std::env::var_os("SSMCP_UNLINKED_WORKER_FIXTURE") else {
            return;
        };
        let path = PathBuf::from(path);
        assert_eq!(std::env::current_exe().unwrap(), path);
        std::fs::remove_file(&path).unwrap();
        assert!(!std::env::current_exe().unwrap().is_file());
        let mut worker = std::process::Command::new(worker_executable().unwrap())
            .args([
                "--exact",
                "computer::zuse_backend::tests::subprocess_worker",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env("SSMCP_ZUSE_WORKER_FIXTURE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = std::io::BufReader::new(worker.stdout.take().unwrap());
        let mut ready = String::new();
        for _ in 0..4 {
            ready.clear();
            std::io::BufRead::read_line(&mut output, &mut ready).unwrap();
            if ready.as_bytes() == WORKER_READY {
                break;
            }
        }
        worker.stdin.take();
        let status = worker.wait().unwrap();
        assert_eq!(
            ready.as_bytes(),
            WORKER_READY,
            "worker handshake after upgrade: {status}"
        );
    }

    fn window_snapshot() -> Value {
        json!({"windows":[{"window_id":"0x123","pid":42,"x":100,"y":200,"width":400,"height":300,"scale":1.25,"focused":true,"mapped":true}]})
    }

    #[test]
    fn window_click_maps_logical_coordinates_once_and_rejects_changed_geometry() {
        let snapshot = window_snapshot();
        let window = window_guard(&snapshot, "0x123").unwrap();
        let binding = WindowBinding {
            window_id: "0x123".into(),
            relative: true,
            expected: Some(window.geometry.clone()),
        };
        assert_eq!(
            resolve_window_point(&binding, Point { x: 10, y: 20 }, &window).unwrap(),
            Point { x: 110, y: 220 }
        );
        assert!(resolve_window_point(&binding, Point { x: 400, y: 20 }, &window).is_err());
        for (field, value) in [("x", json!(101)), ("scale", json!(1.5))] {
            let mut changed = snapshot.clone();
            changed["windows"][0][field] = value;
            let current = window_guard(&changed, "0x123").unwrap();
            assert!(resolve_window_point(&binding, Point { x: 10, y: 20 }, &current).is_err());
        }
    }

    #[test]
    fn window_click_rejects_lost_focus_unmapped_or_ambiguous_targets() {
        let snapshot = window_snapshot();
        for field in ["focused", "mapped"] {
            let mut changed = snapshot.clone();
            changed["windows"][0][field] = json!(false);
            assert!(window_guard(&changed, "0x123").is_err());
        }
        let mut missing_scale = snapshot.clone();
        missing_scale["windows"][0]
            .as_object_mut()
            .unwrap()
            .remove("scale");
        assert!(window_guard(&missing_scale, "0x123").is_err());
        let mut ambiguous = snapshot.clone();
        ambiguous["windows"]
            .as_array_mut()
            .unwrap()
            .push(snapshot["windows"][0].clone());
        assert!(window_guard(&ambiguous, "0x123").is_err());
        assert!(window_guard(&snapshot, "0x999").is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn window_coordinates_require_explicit_identity_and_coordinate_space() {
        let click: Click = parse(json!({"x":10,"y":20,"coordinate_space":"window"})).unwrap();
        assert!(click_window_binding(&click, Some(Point { x: 10, y: 20 })).is_err());
        let click: Click = parse(json!({"x":10,"y":20,"window_id":"0x123"})).unwrap();
        assert!(
            !click_window_binding(&click, Some(Point { x: 10, y: 20 }))
                .unwrap()
                .unwrap()
                .relative
        );
        assert!(validate_num_lock("on").is_ok());
        assert!(validate_num_lock("toggle").is_ok());
        assert!(validate_num_lock("auto").is_err());
    }

    #[test]
    fn timed_path_preserves_endpoints_and_bounds() {
        let mut value = MovePath {
            path: vec![Point { x: 30, y: 40 }, Point { x: 300, y: 340 }],
            duration_ms: 240,
            jitter: 0.5,
            speed_variation: 0.3,
            seed: 42,
        };
        let frames = timed_path(&value).unwrap();
        assert_eq!(frames.len(), 31);
        assert_eq!(frames.first().unwrap().point, value.path[0]);
        assert_eq!(frames.last().unwrap().point, value.path[1]);
        assert_eq!(frames.last().unwrap().at_ms, 240);
        validate_timed_frames(&frames).unwrap();
        value.jitter = f64::NAN;
        assert!(timed_path(&value).is_err());
    }

    #[test]
    fn timed_worker_does_not_move_after_cancellation() {
        let point = Point { x: 30, y: 40 };
        let frames = [
            PointerFrame { point, at_ms: 0 },
            PointerFrame { point, at_ms: 250 },
            PointerFrame { point, at_ms: 500 },
        ];
        let cancellation = Arc::new(Cancellation::default());
        let (first, observed) = mpsc::sync_channel(1);
        let cancelling = cancellation.clone();
        let cancelled = std::thread::spawn(move || {
            observed.recv().unwrap();
            cancelling.cancel();
        });
        let mut sent = 0;
        let error = play_timed_path(&frames, &cancellation, |_| {
            sent += 1;
            if sent == 1 {
                first.send(()).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        cancelled.join().unwrap();
        assert_eq!(error, CANCELLED);
        assert_eq!(sent, 1);
    }

    #[test]
    fn timed_worker_keeps_the_exact_endpoint_after_a_slow_native_frame() {
        let frames = [
            PointerFrame {
                point: Point { x: 1, y: 2 },
                at_ms: 0,
            },
            PointerFrame {
                point: Point { x: 5, y: 6 },
                at_ms: 10,
            },
            PointerFrame {
                point: Point { x: 10, y: 12 },
                at_ms: 20,
            },
        ];
        let mut sent = Vec::new();
        play_timed_path(&frames, &Cancellation::default(), |point| {
            sent.push(point);
            if sent.len() == 1 {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(sent, vec![frames[0].point, frames[2].point]);
    }

    #[test]
    fn windows_enumeration_issues_the_identity_required_by_native_focus() {
        let native = json!({"windows":[{"window_id":"0x1a2b","pid":42,"title":"Editor"}]});
        let mut value = native.clone();
        normalize_window_identities("win32", &mut value).unwrap();
        let id = value["windows"][0]["window_id"].as_str().unwrap();
        assert_eq!(
            super::super::windows_identity::parse(id).unwrap(),
            (0x1a2b, 42)
        );
        assert_eq!(value["windows"][0]["pid"], 42);
        assert!(
            normalize_window_identities("win32", &mut json!({"windows":[{"window_id":"0x1a2b"}]}))
                .is_err()
        );
        let mut x11 = native.clone();
        normalize_window_identities("x11", &mut x11).unwrap();
        assert_eq!(x11, native, "other backends retain their native window IDs");
    }

    #[test]
    #[ignore = "selected explicitly by the native worker transport in unit-test builds"]
    fn subprocess_worker() {
        if std::env::var("SSMCP_ZUSE_WORKER_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        match worker_main() {
            Ok(()) => std::process::exit(0),
            Err(error) => {
                eprintln!("ssmcp zuse worker: {error}");
                std::process::exit(1);
            }
        }
    }

    #[test]
    fn worker_rejects_unbounded_or_injected_requests() {
        assert!(read_request(&mut std::io::Cursor::new(vec![b'x'; REQUEST_LIMIT + 1])).is_err());
        assert!(
            read_request(&mut std::io::Cursor::new(
                b"{\"operation\":\"type\",\"arguments\":{},\"command\":\"sh\"}\n"
            ))
            .is_err()
        );
        assert!(read_request(&mut std::io::Cursor::new(b"{}\n")).is_err());
    }

    #[test]
    fn bounded_transport_accepts_the_full_declared_literal_text_limit() {
        let mut request = serde_json::to_vec(&Request {
            operation: "type".into(),
            arguments: json!({"text":"\u{1}".repeat(16384)}),
            capture_path: None,
            host_ticket: None,
        })
        .unwrap();
        request.push(b'\n');
        assert!(read_request(&mut std::io::Cursor::new(request)).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_readonly_worker_exit_removes_parent_owned_capture() {
        let directory = CaptureDirectory::new().unwrap();
        let path = directory.0.clone();
        let mut command =
            tokio::process::Command::new(crate::program::executable("python3").unwrap());
        command.args(["-c", "import pathlib,sys,time; (pathlib.Path(sys.argv[1])/'capture.png').write_bytes(b'partial capture'); print('ready',flush=True); time.sleep(60)"])
            .arg(&path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut process = Managed::spawn(&mut command).unwrap();
        let mut worker = Worker {
            input: None,
            output: process.child.stdout.take().map(BufReader::new),
            error: process.child.stderr.take(),
            process,
            input_pending: false,
            _capture_directory: Some(directory),
        };
        assert_eq!(
            read_line(worker.output.as_mut().unwrap(), 256)
                .await
                .unwrap(),
            b"ready\n"
        );
        assert!(path.join("capture.png").is_file());
        worker.finish().await.unwrap();
        assert!(
            !path.exists(),
            "reaped workers must not leave their capture directory behind"
        );
    }

    #[test]
    fn cancellation_interrupts_drag_delay_without_polling() {
        let cancellation = Arc::new(Cancellation::default());
        let worker = cancellation.clone();
        let waiting = std::thread::spawn(move || worker.delay(Duration::from_secs(5)));
        cancellation.cancel();
        assert_eq!(waiting.join().unwrap().unwrap_err(), CANCELLED);
    }

    #[test]
    fn capture_preserves_negative_origin_and_hidpi_mapping() {
        let image = image::DynamicImage::new_rgb8(1280, 800);
        let desktop = Region {
            x: -320,
            y: -100,
            width: 640,
            height: 400,
        };
        let region = Region {
            x: -160,
            y: 0,
            width: 320,
            height: 200,
        };
        let value = encode_capture(image, desktop, region, 320, 70, true, "test").unwrap();
        let metadata = &value["metadata"];
        assert_eq!(metadata["width"], 320);
        assert_eq!(metadata["height"], 200);
        assert_eq!(metadata["image_to_desktop"]["origin_x"], -160);
        assert_eq!(metadata["image_to_desktop"]["scale_x"], 1.0);
        assert_eq!(metadata["cursor"], false);
        let bytes = STANDARD.decode(value["data"].as_str().unwrap()).unwrap();
        assert!(bytes.len() <= IMAGE_LIMIT);
        assert_eq!(jpeg_dimensions(&bytes).unwrap(), (320, 200));
    }

    #[test]
    fn modifiers_and_capture_bounds_are_validated_before_library_input() {
        assert!(
            validate_key(&Key {
                key: "Return".into(),
                modifiers: vec!["ctrl".into(), "ctrl".into()]
            })
            .is_err()
        );
        assert!(
            validate_key(&Key {
                key: "a; mouse.click()".into(),
                modifiers: vec![]
            })
            .is_err()
        );
        let desktop = Region {
            x: -50,
            y: 0,
            width: 100,
            height: 100,
        };
        assert!(
            validate_capture(
                &Screenshot {
                    region: Some(Region {
                        x: 49,
                        y: 0,
                        width: 2,
                        height: 1
                    }),
                    max_width: 320,
                    quality: 70,
                    cursor: false
                },
                desktop
            )
            .is_err()
        );
    }

    #[test]
    fn x11_text_refusals_are_clean_only_before_the_first_mutation() {
        for error in [
            "Unsupported X11 Unicode text: no focused editable control; no input was sent",
            "Unsupported X11 Unicode text: could not load libatspi.so.0: unavailable; no input was sent",
            "Unsupported X11 Unicode text on a remote display; no input was sent",
            "X11 focus, target identity or geometry changed; no input was sent",
            "invalid X11 keyboard mapping width",
            "Shift is absent from the X11 keyboard map",
        ] {
            assert!(input_error_precedes_effect("x11", error, true), "{error}");
            assert!(!input_error_precedes_effect("x11", error, false), "{error}");
        }
    }

    #[test]
    fn x11_text_error_classifier_rejects_uncertain_or_extended_messages() {
        for error in [
            "Unsupported X11 Unicode text: missing completion suffix",
            "Unsupported X11 Unicode text: ; no input was sent",
            "Unsupported X11 Unicode text: no focused editable control; no input was sent; cleanup failed",
            "X11 Unicode insertion was not fully confirmed: Unsupported X11 Unicode text: changed target; no input was sent",
            "X11 focus, target identity or geometry changed",
            "native input may have changed; no input was sent",
            "zuse operation cancelled after X11 text dispatch; text may have changed; do not retry",
        ] {
            assert!(!input_error_precedes_effect("x11", error, true), "{error}");
        }
    }

    #[test]
    fn unsupported_keys_do_not_poison_input_cleanup() {
        assert!(input_error_precedes_effect(
            "x11",
            "unknown X11 keysym \"DefinitelyNotAKey\"",
            false
        ));
        assert!(input_error_precedes_effect(
            "x11",
            "key \"XF86Foo\" is absent from the X11 keyboard map",
            false
        ));
        assert!(input_error_precedes_effect(
            "win32",
            "unknown key Whatever",
            false
        ));
        assert!(!input_error_precedes_effect(
            "win32",
            "native keyboard/mouse injection failed",
            false
        ));
        assert!(!input_error_precedes_effect(
            "x11",
            "unknown native failure",
            false
        ));
    }
}
