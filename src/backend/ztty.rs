//! ztty owns the PTY and desktop view. All terminal I/O uses its native MCP tools.
use super::{
    mcp_stdio::{Limits, McpClient},
    process::Managed,
};
use crate::{
    output::{Capture, OutputStore},
    registry::{Provider, Registry},
    wire::Operation,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinHandle,
    time::{Instant, timeout},
};

const TOOLS: &[&str] = &[
    "session_create",
    "session_list",
    "session_commands",
    "session_command_read",
    "session_read",
    "session_write",
    "session_resize",
    "session_control",
    "session_close",
];

pub(crate) fn selected(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == "ztty" || name == "ztty.exe")
}

pub(crate) struct Host {
    client: Arc<McpClient>,
    program: PathBuf,
    socket: String,
    shell: PathBuf,
    output: Arc<OutputStore>,
    slots: Arc<Semaphore>,
    closed: AtomicBool,
    registry: Mutex<Option<std::sync::Weak<Registry>>>,
}

impl Host {
    #[cfg(target_os = "linux")]
    pub(crate) async fn start(
        program: &Path,
        root: &Path,
        output: Arc<OutputStore>,
    ) -> Result<Arc<Self>, String> {
        if std::env::var_os("DISPLAY").is_none() {
            return Err(
                "--shell ztty requires an X11 desktop (DISPLAY); no terminal session was created"
                    .into(),
            );
        }
        let program = if program.components().count() == 1 {
            crate::program::executable(
                program
                    .to_str()
                    .ok_or("ztty executable path must be UTF-8")?,
            )?
        } else {
            program
                .canonicalize()
                .map_err(|e| format!("Invalid ztty executable: {e}"))?
        };
        let shell = crate::platform::default_shell();
        if selected(&shell) {
            return Err(
                "SHELL must name an actual command shell; ztty is the terminal host".into(),
            );
        }
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        // A private broker avoids attaching to, closing or taking over user sessions.
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        // SAFETY: geteuid has no memory or ownership preconditions.
        let uid = unsafe { libc::geteuid() };
        let socket = format!("@ztty-{uid}-ssmcp-{nonce}");
        let mut command = Command::new(&program);
        command
            .args(["mcp", "--transport", "stdio"])
            .env("ZTTY_SOCKET", &socket)
            .current_dir(root);
        let client = McpClient::start(&mut command, Limits::default()).await?;
        let catalog = client.tools();
        if TOOLS
            .iter()
            .any(|name| !catalog.iter().any(|tool| tool["name"] == *name))
        {
            client.shutdown().await;
            return Err("ztty did not advertise the required native terminal tools".into());
        }
        Ok(Arc::new(Self {
            client,
            program,
            socket,
            shell,
            output,
            slots: Arc::new(Semaphore::new(crate::terminals::MAX_TERMINALS)),
            closed: AtomicBool::new(false),
            registry: Mutex::new(None),
        }))
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) async fn start(
        _program: &Path,
        _root: &Path,
        _output: Arc<OutputStore>,
    ) -> Result<Arc<Self>, String> {
        Err(
            "--shell ztty currently requires the Linux ztty session broker and an X11 desktop"
                .into(),
        )
    }

    pub(crate) fn tools(&self) -> Vec<Value> {
        self.client.tools()
    }

    pub(crate) fn bind_registry(&self, registry: &Arc<Registry>) {
        *self.registry.lock().unwrap() = Some(Arc::downgrade(registry));
        self.invalidate_if_closed();
    }

    fn invalidate_if_closed(&self) {
        if self.client.is_closed()
            && let Some(registry) = self
                .registry
                .lock()
                .unwrap()
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
        {
            registry.invalidate(Provider::Ztty);
            registry.invalidate(Provider::Terminal);
        }
    }

    pub(crate) async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("ztty connection is closed".into());
        }
        // Keep content, structuredContent, isError and server metadata unchanged.
        let result = self.client.call(name, arguments).await;
        self.invalidate_if_closed();
        result
    }

    pub(crate) async fn open(
        self: &Arc<Self>,
        display_id: String,
        cwd: PathBuf,
    ) -> Result<Arc<Terminal>, String> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "ztty terminal limit reached")?;
        let host = self.clone();
        let (reply, result) = oneshot::channel();
        // Finish cleanup even if the caller cancels while session_create is in flight.
        tokio::spawn(async move {
            let opened = host.create(display_id, cwd, permit).await;
            if let Err(Ok(terminal)) = reply.send(opened) {
                terminal.shutdown().await;
            }
        });
        result
            .await
            .map_err(|_| "ztty session creation ended unexpectedly".to_owned())?
    }

    async fn create(
        self: &Arc<Self>,
        display_id: String,
        cwd: PathBuf,
        permit: OwnedSemaphorePermit,
    ) -> Result<Arc<Terminal>, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("ztty connection is closed".into());
        }
        let value = data(self.client.call("session_create", json!({"name":display_id,"cwd":cwd,"command":[self.shell,"-i"],"cols":80,"rows":24})).await?)?;
        let identity = identity(&value)?;
        let uri = identity["uri"].as_str().unwrap().to_owned();
        let terminal = Arc::new(Terminal {
            host: self.clone(),
            identity,
            display_id,
            pid: value["pid"]
                .as_u64()
                .ok_or("ztty returned no PTY process ID")?,
            view: Mutex::new(None),
            reader: Mutex::new(None),
            closing: AtomicBool::new(false),
            output: Mutex::new(Output {
                capture: self.output.capture("terminal_session"),
                next_cursor: 0,
                first_cursor: 0,
                eof: false,
            }),
            _permit: permit,
        });
        let setup = timeout(Duration::from_secs(8), async {
            let mut command = Command::new(&self.program);
            command
                .args(["attach", &uri])
                .env("ZTTY_SOCKET", &self.socket)
                .current_dir(&cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let view = Managed::spawn(&mut command)
                .map_err(|e| format!("Cannot open the ztty window: {e}"))?;
            *terminal.view.lock().unwrap() = Some(view);
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if self.closed.load(Ordering::Acquire) {
                    return Err("ztty connection was closed during window startup".into());
                }
                let result = terminal.read_native(0, 1, 0).await?;
                // A text-only MCP response cannot honestly implement data_base64.
                raw_output(&result)?;
                if result["visible"] == true
                    && result["attached"] == true
                    && result["window_id"].as_u64().is_some_and(|id| id > 0)
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("ztty desktop window did not become visible".into());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok::<(), String>(())
        })
        .await
        .unwrap_or_else(|_| Err("ztty desktop window startup timed out".into()));
        if let Err(error) = setup {
            terminal.shutdown().await;
            return Err(error);
        }
        terminal.start_reader();
        Ok(terminal)
    }

    pub(crate) async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.client.shutdown().await;
    }
}

struct Output {
    capture: Capture,
    next_cursor: u64,
    first_cursor: u64,
    eof: bool,
}

pub(crate) struct Terminal {
    host: Arc<Host>,
    identity: Value,
    display_id: String,
    pid: u64,
    view: Mutex<Option<Managed>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    closing: AtomicBool,
    output: Mutex<Output>,
    _permit: OwnedSemaphorePermit,
}

impl Terminal {
    fn arguments(&self) -> Value {
        json!({"session":self.identity["uri"],"expected_session":self.identity})
    }

    async fn read_native(
        &self,
        cursor: u64,
        max_bytes: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        let mut arguments = self.arguments();
        arguments["cursor"] = json!(cursor);
        arguments["max_bytes"] = json!(max_bytes);
        arguments["wait_ms"] = json!(wait_ms);
        data(self.host.call("session_read", arguments).await?)
    }

    fn start_reader(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let reader = tokio::spawn(async move {
            let mut cursor = 0u64;
            loop {
                let Some(terminal) = weak.upgrade() else {
                    break;
                };
                if terminal.closing.load(Ordering::Acquire) {
                    break;
                }
                let result = terminal.read_native(cursor, 16384, 1000).await;
                match result
                    .and_then(|result| output_page(&result, cursor).map(|page| (result, page)))
                {
                    Ok((result, page)) => {
                        {
                            let mut output = terminal.output.lock().unwrap();
                            if page.start > cursor {
                                output
                                    .capture
                                    .fail("ztty output expired before it could be archived");
                            }
                            let _ = output.capture.push(&page.bytes);
                            output.next_cursor = page.next;
                            output.first_cursor = page.first;
                            output.eof = result["output_eof"] == true && result["has_more"] != true;
                            if output.eof {
                                let _ = output.capture.finish();
                            }
                        }
                        if !page.bytes.is_empty() {
                            crate::client::render::terminal_output_for(
                                &terminal.display_id,
                                &page.bytes,
                            );
                        }
                        cursor = page.next;
                        if result["output_eof"] == true && result["has_more"] != true {
                            break;
                        }
                    }
                    Err(error) => {
                        if !terminal.closing.load(Ordering::Acquire) {
                            terminal.fail(error);
                        }
                        break;
                    }
                }
            }
        });
        *self.reader.lock().unwrap() = Some(reader);
    }

    fn fail(&self, error: String) {
        let mut output = self.output.lock().unwrap();
        output.capture.fail(error);
        output.eof = true;
        let _ = output.capture.finish();
    }

    fn archive(&self, result: &mut Value) {
        let output = self.output.lock().unwrap();
        if let Some(metadata) = output.capture.metadata() {
            result["output_artifact"] = json!(metadata);
        }
        if let Some(error) = output.capture.error() {
            result["output_error"] = json!(error);
        }
        result["backend"] = json!("ztty");
        result["session"] = self.identity["uri"].clone();
        result["session_identity"] = self.identity.clone();
    }

    pub(crate) async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        if self.closing.load(Ordering::Acquire) {
            return Err("ztty terminal is closed".into());
        }
        let mut result = match operation {
            Operation::Read {
                cursor,
                max_bytes,
                wait_ms,
            } => {
                let native = self.read_native(cursor, max_bytes, wait_ms).await?;
                let page = output_page(&native, cursor)?;
                let mut bytes = page.bytes;
                let start = page.start;
                bytes.truncate(max_bytes);
                json!({"data":String::from_utf8_lossy(&bytes),"data_base64":STANDARD.encode(&bytes),
                    "start_cursor":native["first_cursor"],"next_cursor":start+bytes.len() as u64,
                    "truncated":native["truncated"],"running":native["running"],
                    "eof":native["output_eof"] == true && native["has_more"] != true && native["next_cursor"].as_u64() == Some(start+bytes.len() as u64),
                    "timed_out":native.get("timed_out").cloned().unwrap_or(Value::Bool(false))})
            }
            Operation::Write { data: text } => {
                let mut arguments = self.arguments();
                arguments["data"] = json!(text);
                data(self.host.call("session_write", arguments).await?)?
            }
            Operation::Resize { rows, cols } => {
                let mut arguments = self.arguments();
                arguments["rows"] = json!(rows);
                arguments["cols"] = json!(cols);
                data(self.host.call("session_resize", arguments).await?)?
            }
            Operation::Status {} => {
                let list = data(self.host.call("session_list", json!({})).await?)?;
                let current = list["sessions"].as_array().and_then(|entries| {
                    entries
                        .iter()
                        .find(|v| identity(v).ok().as_ref() == Some(&self.identity))
                });
                let output = self.output.lock().unwrap();
                json!({"running":current.is_some_and(|v| v["running"] == true),"pid":self.pid,
                    "start_cursor":output.first_cursor,"next_cursor":output.next_cursor,"output_eof":output.eof,
                    "visible":current.is_some_and(|v|v["visible"] == true),"window_id":current.map(|v|v["window_id"].clone())})
            }
            _ => return Err("operation is not a terminal primitive".into()),
        };
        self.archive(&mut result);
        Ok(result)
    }

    pub(crate) async fn shutdown(&self) {
        if self.closing.swap(true, Ordering::AcqRel) {
            return;
        }
        let reader = self.reader.lock().unwrap().take();
        if let Some(reader) = reader {
            reader.abort();
            let _ = reader.await;
        }
        let _ = timeout(
            Duration::from_secs(2),
            self.host.client.call("session_close", self.arguments()),
        )
        .await;
        let view = self.view.lock().unwrap().take();
        if let Some(mut view) = view {
            view.shutdown().await;
        }
        let mut output = self.output.lock().unwrap();
        output.eof = true;
        let _ = output.capture.finish();
    }
}

fn identity(value: &Value) -> Result<Value, String> {
    let uri = value["uri"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 4096)
        .ok_or("ztty returned no session URI")?;
    for key in ["id", "created_at", "broker_pid"] {
        if value[key].as_u64().is_none() {
            return Err(format!("ztty returned no {key}"));
        }
    }
    Ok(
        json!({"uri":uri,"id":value["id"],"created_at":value["created_at"],"broker_pid":value["broker_pid"]}),
    )
}

fn raw_output(value: &Value) -> Result<Vec<u8>, String> {
    let encoded = value["output_base64"]
        .as_str()
        .ok_or("ztty lacks lossless MCP output_base64; update ztty before using --shell ztty")?;
    if encoded.len() > 90_000 {
        return Err("ztty returned oversized lossless output".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "ztty returned invalid lossless output".to_owned())?;
    if bytes.len() > 65539 {
        return Err("ztty returned oversized lossless output".into());
    }
    Ok(bytes)
}

struct OutputPage {
    bytes: Vec<u8>,
    first: u64,
    start: u64,
    next: u64,
}

fn output_page(value: &Value, requested: u64) -> Result<OutputPage, String> {
    let bytes = raw_output(value)?;
    let first = value["first_cursor"]
        .as_u64()
        .ok_or("ztty returned no first output cursor")?;
    let next = value["next_cursor"]
        .as_u64()
        .ok_or("ztty returned no next output cursor")?;
    let truncated = value["truncated"]
        .as_bool()
        .ok_or("ztty returned no output truncation state")?;
    // The retained buffer can start inside a UTF-8 character. Native ztty skips
    // that orphaned prefix for truncated reads, so first_cursor is not always
    // the start of the bytes it delivered. Derive that start from the byte count.
    let start = next
        .checked_sub(bytes.len() as u64)
        .ok_or("ztty returned inconsistent output byte cursors")?;
    if truncated != (requested < first)
        || start < first
        || (truncated && start < requested.max(first))
        || (!truncated && start != requested)
    {
        return Err("ztty returned inconsistent output byte cursors".into());
    }
    Ok(OutputPage {
        bytes,
        first,
        start,
        next,
    })
}

fn data(value: Value) -> Result<Value, String> {
    if value["isError"] == true {
        let message = value["content"]
            .as_array()
            .and_then(|v| v.iter().find_map(|v| v["text"].as_str()))
            .unwrap_or("ztty tool failed");
        return Err(message.chars().take(2048).collect());
    }
    value
        .get("structuredContent")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| "ztty returned no native structured result".into())
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
