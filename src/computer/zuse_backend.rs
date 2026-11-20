//! Fixed-environment, connection-owned execution of the zuse Rust library.
//!
//! The library's OS handles remain inside a short-lived worker. Environment is
//! selected at spawn, never changed in the multithreaded client. Closing stdin
//! cancels between native calls; the worker finishes a key/chord or restores a
//! temporary X11 key map before acknowledging cleanup and exiting.
use super::*;
use std::{
    io::{BufRead, Write},
    sync::{Condvar, mpsc},
};
use tokio::io::{AsyncBufReadExt, BufReader};

const WORKER_FLAG: &str = "--internal-zuse-worker";
const WORKER_READY: &[u8] = b"ssmcp-zuse-worker-v1\n";
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

async fn call(computer: &Computer, operation: &str, arguments: Value) -> Result<Value, String> {
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
    })
    .map_err(|error| error.to_string())?;
    if request.len() > REQUEST_LIMIT {
        return Err("zuse request exceeds 128 KiB".into());
    }
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
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
    let ready = read_line(output, 256).await.map_err(|error| format!("zuse worker startup failed; the embedding executable must call computer::worker_entry(): {error}"))?;
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
    worker.input_pending = matches!(operation, "click" | "drag" | "key" | "type" | "scroll");
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
        computer
            .releases
            .fail("zuse reported an input-release failure".into());
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
    let native = call(computer, "capabilities", json!({})).await;
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
                "click":capability(pointer,backend,Some("Native pointer input is unavailable")),
                "drag":capability(pointer&&geometry.is_ok(),backend,reason.or(Some("Native pointer input is unavailable"))),
                "key":capability(keyboard,backend,Some("Native keyboard input is unavailable")),
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

pub(super) async fn perform(computer: &Computer, name: &str, args: Value) -> Result<Value, String> {
    // Do not connect a native library for invalid/unknown operations.
    let (operation, arguments, metadata) = match name {
        "ssmcp.computer.windows" => {
            parse::<Empty>(args)?;
            ("windows", json!({}), None)
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
        "ssmcp.computer.click" => {
            let value: Click = parse(args)?;
            if !(1..=3).contains(&value.count) {
                return Err("count must be between 1 and 3".into());
            }
            let point = optional_point(value.x, value.y)?;
            if let Some(point) = point {
                validate_point(point, &monitors(computer).await?)?;
            }
            (
                "click",
                json!({"point":point,"button":button_name(value.button),"count":value.count}),
                Some(json!({"clicked":true,"count":value.count})),
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
    let native = call(computer, operation, arguments).await?;
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
    if let Some(object) = metadata.as_object_mut() {
        object.insert("engine".into(), json!("zuse"));
        object.insert("engine_version".into(), json!(zuse::VERSION));
        object.insert("backend".into(), native["backend"].clone());
    }
    Ok(result(metadata))
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
}

impl Cancellation {
    fn cancel(&self) {
        *self
            .cancelled
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = true;
        self.changed.notify_all();
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
    request: Request,
    cancellation: &Cancellation,
    cleanup_ok: &mut bool,
) -> Result<Value, String> {
    cancellation.check()?;
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
            cancellation.check()?;
            desktop.focus(&value.window_id)?;
            json!({"focused":value.window_id})
        }
        "move" => {
            let value: Point = parse(args)?;
            value.validate()?;
            cancellation.check()?;
            desktop.move_to(value.x, value.y)?;
            json!({})
        }
        "click" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct ClickRequest {
                point: Option<Point>,
                button: Button,
                count: u8,
            }
            let value: ClickRequest = parse(args)?;
            if !(1..=3).contains(&value.count) {
                return Err("invalid click count".into());
            }
            if let Some(point) = value.point {
                point.validate()?;
                cancellation.check()?;
                desktop.move_to(point.x, point.y)?;
            }
            for index in 0..value.count {
                cancellation.check()?;
                with_button(&mut desktop, button_name(value.button), cleanup_ok, |_| {
                    Ok(())
                })?;
                if index + 1 < value.count {
                    cancellation.delay(Duration::from_millis(40))?;
                }
            }
            json!({})
        }
        "drag" => {
            let value: Drag = parse(args)?;
            if !(2..=64).contains(&value.path.len()) || value.duration_ms > 5000 {
                return Err("invalid drag".into());
            }
            for point in &value.path {
                point.validate()?;
            }
            cancellation.check()?;
            desktop.move_to(value.path[0].x, value.path[0].y)?;
            cancellation.check()?;
            with_button(
                &mut desktop,
                button_name(value.button),
                cleanup_ok,
                |desktop| {
                    for point in &value.path[1..] {
                        cancellation.delay(Duration::from_millis(
                            value.duration_ms / (value.path.len() - 1) as u64,
                        ))?;
                        desktop.move_to(point.x, point.y)?;
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
            cancellation.check()?;
            if let Err(error) = desktop.key(&parts.join("+")) {
                *cleanup_ok = input_error_precedes_effect(backend, &error, false);
                return Err(error);
            }
            json!({})
        }
        "type" => {
            let value: TypeText = parse(args)?;
            validate_literal_text(backend, &value.text)?;
            let mut buffer = [0u8; 4];
            for character in value.text.chars() {
                cancellation.check()?;
                if let Err(error) = desktop.type_text(character.encode_utf8(&mut buffer)) {
                    *cleanup_ok = input_error_precedes_effect(backend, &error, true);
                    return Err(error);
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
                cancellation.check()?;
                desktop.move_to(point.x, point.y)?;
            }
            for _ in 0..value.steps {
                cancellation.check()?;
                desktop.scroll(value.dx, value.dy)?;
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

fn validate_literal_text(backend: &str, text: &str) -> Result<(), String> {
    if text.len() > 16384 || text.contains('\0') {
        return Err("invalid literal text".into());
    }
    if backend == "wayland" {
        // zuse 0.2.0 validates the whole string before creating a virtual keyboard.
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
    // zuse 0.2.0 has String errors. These branches are verified against the
    // pinned library: they return before key injection or keyboard-map writes.
    // Unknown native/runtime errors remain unconfirmed and keep input locked.
    if backend == "x11" {
        return error == "XTest extension library unavailable"
            || (!typing
                && (error.starts_with("unknown X11 keysym ")
                    || (error.starts_with("key ")
                        && error.ends_with(" is absent from the X11 keyboard map"))))
            || (typing
                && matches!(
                    error,
                    "invalid X11 keyboard-map range" | "could not read X11 keyboard mapping"
                ));
    }
    if backend == "wayland" {
        // In zuse 0.2.0 (557ea46), these exact state checks precede shortcut
        // dispatch and virtual-keyboard creation. A generic prefix could also
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

fn with_button<T>(
    desktop: &mut zuse::platform::Desktop,
    button: &str,
    cleanup_ok: &mut bool,
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
        armed: true,
    };
    let down = release.desktop.button(button, "down");
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
        "workspace":window["workspace"],"wine":window["wine"]
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
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
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
mod wayland_tests;

#[cfg(test)]
mod tests {
    use super::*;

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
