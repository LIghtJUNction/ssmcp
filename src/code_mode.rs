//! JavaScript orchestration through system Node.js inside mandatory bubblewrap isolation.
pub(crate) mod reminders;
pub(crate) mod user_wait;
use crate::backend::process::Managed;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::Semaphore,
    task::JoinSet,
};

pub const MAX_CODE_BYTES: usize = crate::wire::MAX_CODE_BYTES;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_BYTES: usize = 64 * 1024;
pub const MAX_IMAGES: usize = 4;
pub const MAX_IMAGE_ENCODED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_CALLS: usize = 128;
pub const MAX_NATIVE_INFLIGHT: usize = 16;
pub(crate) const MAX_CONTEXT_ENTRY_BYTES: usize = crate::wire::MAX_CONTEXT_ENTRY_BYTES;
pub(crate) const MAX_CONTEXT_BYTES: usize = 64 * 1024;
pub const MAX_CONCURRENT: usize = 4;
const MAX_ERROR_BYTES: usize = 4096;
static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT);
static PREPARATIONS: Semaphore = Semaphore::const_new(MAX_CONCURRENT);

/// Scoped around each host callback, including built-in workspace bridges. A
/// client receives source only when an authorized native call is dispatched to
/// that client; discovery inventory alone never broadcasts the program.
#[derive(Clone)]
pub(crate) struct AuditRun {
    pub context: Arc<crate::wire::CodeContext>,
    pub controller: Option<crate::wire::Controller>,
    targets: Arc<Mutex<HashSet<String>>>,
    steering_targets: Arc<Mutex<HashSet<String>>>,
    finished: Arc<Mutex<Option<crate::wire::CodeCompletion>>>,
    bindings: Arc<Mutex<HashMap<String, RunBinding>>>,
    memories: Arc<Mutex<ContextMemories>>,
    accepted_replies: Arc<Mutex<reminders::Receipts>>,
    waits: Arc<Mutex<user_wait::State>>,
    started: Instant,
}

/// Per-run capture only; no workspace history is cached between callers.
#[derive(Default)]
struct ContextMemories {
    clients: HashMap<String, ContextMemory>,
    bytes: usize,
    overflowed: bool,
}

struct ContextMemory {
    workspace: String,
    revision: u64,
    entry: Option<Value>,
    bytes: usize,
}

/// Private server-side proof for one room, never serialized to a native client.
#[derive(Clone)]
pub(crate) struct RunBinding {
    pub owner: String,
    pub agent_id: String,
    pub token: String,
}

tokio::task_local! {
    pub(crate) static AUDIT_RUN: AuditRun;
}

impl AuditRun {
    pub(crate) fn new(
        code: &str,
        controller: Option<crate::wire::Controller>,
    ) -> Result<Self, String> {
        let mut bytes = [0u8; 16];
        crate::platform::random_bytes(&mut bytes)
            .map_err(|_| "cannot allocate code audit run ID")?;
        let context = crate::wire::CodeContext {
            run_id: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            code: code.into(),
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        };
        context.validate()?;
        Ok(Self {
            context: Arc::new(context),
            controller,
            targets: Arc::new(Mutex::new(HashSet::new())),
            steering_targets: Arc::new(Mutex::new(HashSet::new())),
            finished: Arc::new(Mutex::new(None)),
            bindings: Arc::new(Mutex::new(HashMap::new())),
            memories: Arc::new(Mutex::new(ContextMemories::default())),
            accepted_replies: Arc::new(Mutex::new(reminders::Receipts::default())),
            waits: Arc::new(Mutex::new(user_wait::State::default())),
            started: Instant::now(),
        })
    }

    pub(crate) fn dispatched(&self, client_id: &str) {
        self.targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(client_id.into());
    }

    pub(crate) fn targets(&self) -> Vec<String> {
        let mut targets = self
            .targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        targets.sort();
        targets
    }

    /// UI and client metadata queries still carry source audit, but cannot
    /// consume a human requirement merely by refreshing display or discovery.
    /// A visible agent message is an interaction and may receive Next caller
    /// requirements. Exact-agent delivery still requires an explicit binding.
    pub(crate) fn native_dispatched(&self, client_id: &str, tool: Option<&str>) {
        self.dispatched(client_id);
        if tool.is_some_and(|name| {
            (matches!(name, "ssmcp.ui.message" | "ssmcp.ui.present")
                || !name.starts_with("ssmcp.ui."))
                && !matches!(
                    name,
                    "ssmcp.client.capabilities"
                        | "ssmcp.resources.list"
                        | "ssmcp.resources.templates"
                        | "ssmcp.resources.read"
                        | "ssmcp.context.status"
                        | "ssmcp.context.read"
                        | "ssmcp.context.list"
                )
        }) {
            self.interacted(client_id);
        }
    }

    pub(crate) fn interacted(&self, client_id: &str) {
        self.dispatched(client_id);
        self.steering_targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(client_id.into());
    }

    pub(crate) fn steering_targets(&self) -> Vec<String> {
        let mut targets = self
            .steering_targets
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        targets.sort();
        targets
    }

    /// The broker calls this with a real native result and the binding captured
    /// before dispatch. Returned JavaScript JSON cannot acknowledge a dialogue.
    pub(crate) fn native_reply(
        &self,
        client_id: &str,
        agent_id: Option<&str>,
        tool: Option<&str>,
        result: &Value,
        received_at: Instant,
    ) {
        self.accepted_replies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observe(client_id, agent_id, tool, result, received_at);
    }

    pub(crate) fn accepted_replies(&self, client_id: &str) -> Vec<reminders::AcceptedReply> {
        self.accepted_replies
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .for_client(client_id)
    }

    /// Called only after the host has reauthorized a completed native call.
    /// Older concurrent replies cannot overwrite a newer workspace revision.
    pub(crate) fn capture_context(&self, client_id: &str, result: &Value) {
        if client_id.is_empty()
            || client_id.len() > 128
            || !self
                .targets
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(client_id)
        {
            return;
        }
        let Some(context) = result
            .get("_meta")
            .and_then(|meta| meta.get("ssmcp.context"))
            .filter(|context| context.is_object())
        else {
            return;
        };
        let Some(workspace) = context["workspace"]["id"]
            .as_str()
            .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        else {
            return;
        };
        let Some(revision) = context["revision"].as_u64() else {
            return;
        };
        let mut memories = self
            .memories
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(old) = memories.clients.get(client_id) {
            if old.workspace == workspace && old.revision >= revision {
                return;
            }
        } else if memories.clients.len() >= MAX_CALLS {
            memories.overflowed = true;
            return;
        }
        let old_bytes = memories.clients.get(client_id).map_or(0, |old| old.bytes);
        let retained = memories.bytes.saturating_sub(old_bytes);
        let entry = json!({"client_id":client_id,"context":context});
        let bytes = entry.to_string().len();
        // Include array separators in the capture bound. HTTP separately bounds
        // the complete structured + textual presentation before claiming requests.
        let fits = bytes <= MAX_CONTEXT_ENTRY_BYTES
            && retained
                .saturating_add(bytes)
                .saturating_add(memories.clients.len() + 3)
                <= MAX_CONTEXT_BYTES;
        let bytes = if fits { bytes } else { 0 };
        memories.bytes = retained + bytes;
        memories.clients.insert(
            client_id.into(),
            ContextMemory {
                workspace: workspace.into(),
                revision,
                entry: fits.then_some(entry),
                bytes,
            },
        );
    }

    pub(crate) fn context_memories(&self) -> (Vec<Value>, bool) {
        let memories = self
            .memories
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut clients = memories.clients.iter().collect::<Vec<_>>();
        clients.sort_by(|left, right| left.0.cmp(right.0));
        let omitted = memories.overflowed || clients.iter().any(|(_, value)| value.entry.is_none());
        let entries = clients
            .into_iter()
            .filter_map(|(_, value)| value.entry.clone())
            .collect();
        (entries, omitted)
    }

    pub(crate) fn completion(&self, result: &Result<Value, String>) -> crate::wire::CodeCompletion {
        let completion = crate::wire::CodeCompletion {
            run_id: self.context.run_id.clone(),
            success: result.is_ok(),
            status: if result.is_ok() {
                "completed"
            } else if result
                .as_ref()
                .is_err_and(|error| error.starts_with("code_mode timed out"))
            {
                "timeout"
            } else {
                "failed"
            }
            .into(),
            duration_ms: self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            error: result
                .as_ref()
                .err()
                .map(|error| bounded(error.clone(), 4000)),
        };
        *self
            .finished
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(completion.clone());
        completion
    }

    pub(crate) fn completed(&self) -> Option<crate::wire::CodeCompletion> {
        self.finished
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn bind(
        &self,
        client_id: &str,
        owner: &str,
        agent_id: &str,
        token: &str,
    ) -> Result<(), String> {
        let mut bindings = self
            .bindings
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = bindings.get(client_id) {
            if existing.owner != owner || existing.agent_id != agent_id || existing.token != token {
                return Err(
                    "a code_mode run cannot switch its bound agent within the same room".into(),
                );
            }
        } else {
            if bindings.len() >= MAX_CALLS {
                return Err("too many agent bindings in one code_mode run".into());
            }
            bindings.insert(
                client_id.into(),
                RunBinding {
                    owner: owner.into(),
                    agent_id: agent_id.into(),
                    token: token.into(),
                },
            );
        }
        drop(bindings);
        // Explicitly binding a room is an authorized interaction with it, even
        // if this program only checks messages and makes no native tool call.
        self.interacted(client_id);
        Ok(())
    }

    pub(crate) fn binding(&self, client_id: &str) -> Option<RunBinding> {
        self.bindings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(client_id)
            .cloned()
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequest {
    Builtin {
        name: String,
        arguments: Value,
    },
    Call {
        client_id: String,
        name: String,
        arguments: Value,
    },
    Tools {
        client_id: String,
        #[serde(default)]
        cursor: Option<String>,
    },
    WaitUser {
        client_id: String,
        arguments: user_wait::Arguments,
    },
}
pub type Host = Arc<
    dyn Fn(HostRequest) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerFrame {
    Call { id: u64, request: HostRequest },
    Log { line: String },
    Image { content: Value },
    Result { value: Value },
    Error { message: String },
}

struct Worker {
    process: Managed,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Signal the owned process tree before closing a pipe can trigger an early exit.
        self.process.terminate();
    }
}

fn bounded(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [truncated]");
    }
    text
}

pub(crate) fn executable(name: &str) -> Result<PathBuf, String> {
    if name == "node" {
        crate::program::node()
    } else {
        crate::program::executable(name)
    }
}

fn runner_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SSMCP_CODE_MODE_RUNNER") {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err("SSMCP_CODE_MODE_RUNNER must name an existing absolute runner file".into());
        }
        return path.canonicalize().map_err(|e| e.to_string());
    }
    let installed = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent()?.parent().map(Path::to_path_buf))
        .map(|prefix| prefix.join("libexec/ssmcp/code-mode.js"));
    if let Some(path) = installed.as_ref().filter(|path| path.is_file()) {
        return path.canonicalize().map_err(|e| e.to_string());
    }
    #[cfg(debug_assertions)]
    {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/code_mode.js");
        if path.is_file() {
            return Ok(path);
        }
    }
    let location = installed
        .map(|path| format!(" at {}", path.display()))
        .unwrap_or_else(|| " (cannot determine the executable's installation prefix)".into());
    Err(format!(
        "code_mode runner is missing{location}; run make install-server or set SSMCP_CODE_MODE_RUNNER to its installed absolute path"
    ))
}

fn node_arguments(command: &mut Command) {
    command.args(crate::program::NODE_FLAGS);
}

fn sandbox_command() -> Result<Command, String> {
    let node = executable("node")?;
    let bwrap = executable("bwrap")?;
    let prlimit = executable("prlimit")?;
    let runner = runner_path()?;
    let mut command = Command::new(bwrap);
    command.env_clear().args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--uid",
        "65534",
        "--gid",
        "65534",
        "--cap-drop",
        "ALL",
        "--new-session",
        "--die-with-parent",
        "--clearenv",
        "--setenv",
        "HOME",
        "/tmp",
        "--setenv",
        "LANG",
        "C.UTF-8",
        "--setenv",
        "PATH",
        "/runtime",
    ]);
    // Only runtime libraries are visible. The worker needs no procfs; omitting it
    // also preserves systemd's protected /proc mounts without relaxing the service.
    for path in ["/usr/lib", "/lib", "/lib64", "/usr/share/nodejs"] {
        command.args(["--ro-bind-try", path, path]);
    }
    command
        .arg("--ro-bind")
        .arg(node)
        .arg("/runtime/node")
        .arg("--ro-bind")
        .arg(prlimit)
        .arg("/runtime/prlimit")
        .arg("--ro-bind")
        .arg(runner)
        .arg("/runner.js")
        .args([
            "--dev",
            "/dev",
            "--remount-ro",
            "/dev",
            "--size",
            "16777216",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/work",
            "--chdir",
            "/work",
            "--remount-ro",
            "/",
            "--",
            "/runtime/prlimit",
            "--nproc=128:128",
            "--",
            "/runtime/node",
        ]);
    node_arguments(&mut command);
    command.arg("/runner.js");
    Ok(command)
}

fn limit_process(command: &mut Command) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // NPROC is set by prlimit after bubblewrap enters its user namespace. Applying
    // a low per-UID process limit here would count all threads owned by the host
    // user and prevent namespace creation on an otherwise healthy busy machine.
    // SAFETY: after fork this closure only calls async-signal-safe setrlimit.
    unsafe {
        command.pre_exec(|| {
            for (resource, value) in [
                (libc::RLIMIT_CPU, 2),
                (libc::RLIMIT_DATA, 256 * 1024 * 1024),
                (libc::RLIMIT_AS, 2 * 1024 * 1024 * 1024),
                (libc::RLIMIT_NOFILE, 64),
                (libc::RLIMIT_CORE, 0),
            ] {
                let limits = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limits) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Run an async JavaScript body with the installed system runtime.
/// Bubblewrap is mandatory: an unavailable sandbox fails closed.
pub async fn execute(
    code: String,
    timeout_ms: u64,
    clients: Value,
    host: Host,
) -> Result<Value, String> {
    let command = prepare_command(sandbox_command).await?;
    execute_process(command, code, timeout_ms, clients, host).await
}

const RUNTIME_CHECK_CODE: &str =
    "return await Promise.resolve({ssmcp_runtime_check: 1, clients: clients.length});";

fn runtime_check_host() -> (Host, Arc<AtomicBool>) {
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let host: Host = Arc::new(move |_| {
        observed.store(true, Ordering::Relaxed);
        Box::pin(async { Err("native calls are disabled during the runtime check".into()) })
    });
    (host, called)
}

fn validate_runtime_check(output: Value, host_called: bool) -> Result<(), String> {
    if host_called {
        return Err("code_mode runtime check attempted a disabled native call".into());
    }
    if output != json!({"result":{"ssmcp_runtime_check":1,"clients":0},"logs":[]}) {
        return Err("code_mode runtime check returned an unexpected result".into());
    }
    Ok(())
}

/// Verify the installed runner through the mandatory sandbox without connecting
/// clients, reading gateway configuration, or permitting native host operations.
pub async fn check_runtime() -> Result<(), String> {
    let (host, called) = runtime_check_host();
    let output = execute(RUNTIME_CHECK_CODE.into(), 10_000, json!([]), host).await?;
    validate_runtime_check(output, called.load(Ordering::Relaxed))
}

async fn prepare_command(
    prepare: impl FnOnce() -> Result<Command, String> + Send + 'static,
) -> Result<Command, String> {
    // Runtime discovery may run its bounded native compatibility probe. An
    // aborted request drops only this await: the blocking worker still owns the
    // probe, reaches its existing deadline, and terminates/reaps it if needed.
    let permit = PREPARATIONS
        .try_acquire()
        .map_err(|_| "code_mode runtime preparation is busy; retry later".to_owned())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        prepare()
    })
    .await
    .map_err(|_| "code_mode runtime preparation failed".to_owned())?
}

async fn execute_process(
    mut command: Command,
    code: String,
    timeout_ms: u64,
    clients: Value,
    host: Host,
) -> Result<Value, String> {
    if code.trim().is_empty() || code.len() > MAX_CODE_BYTES {
        return Err(format!(
            "code must contain 1..={MAX_CODE_BYTES} UTF-8 bytes"
        ));
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!("timeout_ms must be 1..={MAX_TIMEOUT_MS}"));
    }
    if !clients.is_array() || clients.to_string().len() > MAX_RESULT_BYTES {
        return Err("clients must be an array within the code_mode metadata limit".into());
    }
    let _permit = SLOTS
        .try_acquire()
        .map_err(|_| "code_mode is busy; retry later")?;
    limit_process(&mut command);
    let mut process = Managed::spawn(&mut command)
        .map_err(|e| format!("cannot start isolated code_mode runtime: {e}"))?;
    let stdin = process
        .child
        .stdin
        .take()
        .ok_or("runtime stdin unavailable")?;
    let stdout = process
        .child
        .stdout
        .take()
        .ok_or("runtime stdout unavailable")?;
    let stderr = process
        .child
        .stderr
        .take()
        .ok_or("runtime stderr unavailable")?;
    let diagnostics = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr
            .take(MAX_ERROR_BYTES as u64)
            .read_to_end(&mut bytes)
            .await;
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let mut worker = Worker {
        process,
        stdin,
        stdout: BufReader::new(stdout),
    };
    let mut logs = Vec::new();
    let mut images = Vec::new();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        send_frame(&mut worker.stdin, &json!({"type":"run", "code":code, "clients":clients})).await?;
        bridge(&mut worker.stdout, &mut worker.stdin, host, &mut logs, &mut images).await
    }).await.unwrap_or_else(|_| Err("code_mode timed out; an already submitted native operation may continue and its outcome may be unknown. Inspect the client before retrying a mutating call.".into()));
    // Managed also terminates the owned process tree when this future is cancelled.
    worker.process.shutdown().await;
    let diagnostics = diagnostics.await.unwrap_or_default();
    match result {
        Ok(value) => {
            let mut output = json!({"result":value, "logs":logs});
            if !images.is_empty() {
                output["images"] = images.into();
            }
            Ok(output)
        }
        Err(mut error) => {
            if !diagnostics.trim().is_empty() {
                error.push_str("\nRuntime: ");
                error.push_str(diagnostics.trim());
            }
            if !logs.is_empty() {
                error.push_str("\nCaptured logs:\n");
                error.push_str(&logs.join("\n"));
            }
            Err(error)
        }
    }
}

/// Build the public MCP response without duplicating image base64 into text or
/// structuredContent. Only host-validated image frames live at this envelope level.
pub fn tool_result(result: Result<Value, String>) -> Value {
    let success = (|| {
        let mut value = result?;
        let images = value
            .as_object_mut()
            .and_then(|object| object.remove("images"));
        let mut response = crate::backend::result(value);
        if let Some(images) = images {
            let images = images
                .as_array()
                .ok_or("invalid code_mode image envelope")?;
            let mut bytes = 0;
            if images.len() > MAX_IMAGES {
                return Err("code_mode images exceed the image count limit".into());
            }
            for image in images {
                crate::output::validate_image_content(image)?;
                bytes += image["data"].as_str().unwrap().len();
                if bytes > MAX_IMAGE_ENCODED_BYTES {
                    return Err("code_mode images exceed 2 MiB of encoded data".into());
                }
                response["content"].as_array_mut().unwrap().push(json!({
                    "type":"image", "mimeType":image["mimeType"], "data":image["data"]
                }));
            }
        }
        if response.to_string().len() > crate::wire::MAX_FRAME - 8192 {
            return Err(
                "code_mode response exceeds 4 MiB; return a smaller summary or fewer images".into(),
            );
        }
        Ok(response)
    })();
    success.unwrap_or_else(
        |message: String| json!({"content":[{"type":"text","text":message}],"isError":true}),
    )
}

fn validate_request(request: &HostRequest) -> Result<(), String> {
    let client_id = match request {
        HostRequest::Builtin { name, arguments } => {
            if name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
                || !arguments.is_object()
                || arguments.to_string().len() > crate::wire::MAX_FRAME / 2
            {
                return Err("invalid builtin name or arguments".into());
            }
            return Ok(());
        }
        HostRequest::Call {
            client_id,
            name,
            arguments,
        } => {
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                || !arguments.is_object()
            {
                return Err("invalid native tool name or arguments".into());
            }
            if arguments.to_string().len() > crate::wire::MAX_FRAME / 2 {
                return Err(
                    "native tool arguments exceed 2 MiB; send a smaller argument payload".into(),
                );
            }
            client_id
        }
        HostRequest::Tools { client_id, cursor } => {
            if cursor.as_ref().is_some_and(|c| c.len() > 128) {
                return Err("tool cursor is too long".into());
            }
            client_id
        }
        HostRequest::WaitUser {
            client_id,
            arguments,
        } => {
            arguments.validate()?;
            client_id
        }
    };
    if client_id.is_empty() || client_id.len() > 128 {
        return Err("invalid client ID".into());
    }
    Ok(())
}

async fn bridge<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    writer: &mut W,
    host: Host,
    logs: &mut Vec<String>,
    images: &mut Vec<Value>,
) -> Result<Value, String> {
    let mut partial = Vec::new();
    let mut pending = HashSet::new();
    let mut calls = JoinSet::new();
    let mut count = 0u64;
    let mut log_bytes = 0;
    let mut image_bytes = 0;
    let result = async {
        loop {
            tokio::select! {
                frame = read_frame(&mut reader, &mut partial) => match frame? {
                    Some(WorkerFrame::Call {id, request}) => {
                        count += 1;
                        if count > MAX_CALLS as u64 { return Err(format!("code_mode exceeds {MAX_CALLS} native calls")); }
                        if id != count { return Err("runtime sent an invalid or reused native call ID".into()); }
                        let validation = if pending.len() >= MAX_NATIVE_INFLIGHT {
                            Err(format!("code_mode allows at most {MAX_NATIVE_INFLIGHT} simultaneous native calls; await a smaller batch"))
                        } else { validate_request(&request) };
                        if let Err(error) = validation { send_reply(writer, id, Err(error)).await?; continue; }
                        pending.insert(id);
                        let host = host.clone();
                        calls.spawn(async move { (id, host(request).await) });
                    },
                    Some(WorkerFrame::Log {line}) => {
                        if log_bytes + line.len() + 1 > MAX_LOG_BYTES { return Err(format!("code_mode log output exceeds {MAX_LOG_BYTES} bytes")); }
                        log_bytes += line.len() + 1;
                        logs.push(line);
                    },
                    Some(WorkerFrame::Image {content}) => {
                        crate::output::validate_image_content(&content)?;
                        let bytes = content["data"].as_str().unwrap().len();
                        if images.len() >= MAX_IMAGES || image_bytes + bytes > MAX_IMAGE_ENCODED_BYTES {
                            return Err("code_mode images exceed 4 images or 2 MiB of encoded data".into());
                        }
                        image_bytes += bytes;
                        images.push(json!({"type":"image","mimeType":content["mimeType"],"data":content["data"]}));
                    },
                    Some(WorkerFrame::Result {value}) => {
                        if !pending.is_empty() { return Err("code_mode returned with pending native calls; await every call".into()); }
                        if value.to_string().len() > MAX_RESULT_BYTES { return Err(format!("code_mode result exceeds {MAX_RESULT_BYTES} bytes; return a smaller summary")); }
                        return Ok(value);
                    },
                    Some(WorkerFrame::Error {message}) => return Err(bounded(message, MAX_ERROR_BYTES)),
                    None => return Err("isolated code_mode runtime exited before returning a result; check system Node.js, bubblewrap namespace support and process resource limits".into()),
                },
                completed = calls.join_next(), if !calls.is_empty() => {
                    let (id, result) = completed.ok_or("native call task disappeared")?.map_err(|_| "native call task failed")?;
                    pending.remove(&id);
                    send_reply(writer, id, result).await?;
                },
            }
        }
    }.await;
    calls.abort_all();
    while calls.join_next().await.is_some() {}
    result.map_err(|error: String| {
        if !pending.is_empty() { format!("{error}\nSubmitted native operations may continue and their outcome may be unknown; inspect client state before retrying.") } else { error }
    })
}

async fn send_reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: u64,
    result: Result<Value, String>,
) -> Result<(), String> {
    let mut reply = match result {
        Ok(value) => json!({"type":"reply","id":id,"ok":true,"value":value}),
        Err(error) => {
            json!({"type":"reply","id":id,"ok":false,"error":bounded(error, MAX_ERROR_BYTES)})
        }
    };
    if reply.to_string().len() + 1 > crate::wire::MAX_FRAME {
        reply = json!({"type":"reply","id":id,"ok":false,"error":"native operation completed but its response exceeds the code_mode limit; inspect state before retrying a mutating call"});
    }
    send_frame(writer, &reply).await
}

async fn send_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > crate::wire::MAX_FRAME {
        return Err("code_mode protocol frame exceeds its byte limit".into());
    }
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("runtime input failed: {e}"))?;
    writer.flush().await.map_err(|e| e.to_string())
}

async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    partial: &mut Vec<u8>,
) -> Result<Option<WorkerFrame>, String> {
    // Keep partial bytes outside this future: host completions can cancel a read mid-frame.
    loop {
        let bytes = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return if partial.is_empty() {
                Ok(None)
            } else {
                Err("truncated runtime frame".into())
            };
        }
        let length = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if partial.len() + length > crate::wire::MAX_FRAME {
            return Err("runtime frame exceeds its byte limit".into());
        }
        let complete = bytes[length - 1] == b'\n';
        partial.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if complete {
            let frame =
                serde_json::from_slice(partial).map_err(|e| format!("invalid runtime frame: {e}"));
            partial.clear();
            return frame.map(Some);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Instant;
    use tokio::sync::Notify;

    static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn memory_result(workspace: char, revision: u64, text: &str) -> Value {
        json!({"content":[],"_meta":{"ssmcp.context":{
            "workspace":{"id":workspace.to_string().repeat(64)},
            "revision":revision,"summary":text,"mode":"provided_summary"
        }}})
    }

    #[test]
    fn native_argument_size_has_an_explicit_error_at_the_unchanged_boundary() {
        let limit = crate::wire::MAX_FRAME / 2;
        let overhead = json!({"data":""}).to_string().len();
        let request = |size| HostRequest::Call {
            client_id: "client".into(),
            name: "ssmcp.fs.write".into(),
            arguments: json!({"data":"x".repeat(size - overhead)}),
        };
        assert!(validate_request(&request(limit)).is_ok());
        assert_eq!(
            validate_request(&request(limit + 1)).unwrap_err(),
            "native tool arguments exceed 2 MiB; send a smaller argument payload",
        );
        assert_eq!(
            validate_request(&HostRequest::Call {
                client_id: "client".into(),
                name: "invalid name".into(),
                arguments: json!({}),
            })
            .unwrap_err(),
            "invalid native tool name or arguments",
        );
    }

    #[test]
    fn context_capture_is_per_run_per_client_and_keeps_the_latest_revision() {
        let run = AuditRun::new("return native.stdout;", None).unwrap();
        run.capture_context("one", &memory_result('a', 4, "not dispatched"));
        assert!(run.context_memories().0.is_empty());
        run.native_dispatched("one", Some("ssmcp.exec.run"));
        run.capture_context("one", &memory_result('a', 4, "newest"));
        run.capture_context("one", &memory_result('a', 3, "late old reply"));
        run.capture_context("one", &memory_result('a', 4, "duplicate"));
        let (memories, omitted) = run.context_memories();
        assert!(!omitted);
        assert_eq!(memories.len(), 1);
        assert_eq!(memories[0]["client_id"], "one");
        assert_eq!(memories[0]["context"]["summary"], "newest");
        run.capture_context("one", &memory_result('b', 1, "different workspace"));
        assert_eq!(
            run.context_memories().0[0]["context"]["summary"],
            "different workspace"
        );
        let other_run = AuditRun::new("return 1;", None).unwrap();
        assert!(other_run.context_memories().0.is_empty());
    }

    #[test]
    fn context_capture_enforces_budgets_without_reintroducing_an_older_snapshot() {
        let run = AuditRun::new("return 1;", None).unwrap();
        run.native_dispatched("one", Some("ssmcp.exec.run"));
        run.capture_context("one", &memory_result('a', 1, "old"));
        run.capture_context(
            "one",
            &memory_result('a', 2, &"x".repeat(MAX_CONTEXT_ENTRY_BYTES)),
        );
        run.capture_context("one", &memory_result('a', 1, "late old"));
        assert!(run.context_memories().0.is_empty());
        assert!(run.context_memories().1);
        run.capture_context("one", &memory_result('a', 3, "new compact snapshot"));
        assert!(!run.context_memories().1);
        for index in 0..20 {
            let client = format!("client-{index}");
            run.native_dispatched(&client, Some("ssmcp.exec.run"));
            run.capture_context(&client, &memory_result('a', 1, &"x".repeat(11 * 1024)));
        }
        let (memories, omitted) = run.context_memories();
        assert!(omitted);
        assert!(serde_json::to_vec(&memories).unwrap().len() <= MAX_CONTEXT_BYTES);
        assert!(
            memories
                .iter()
                .all(|entry| entry.to_string().len() <= MAX_CONTEXT_ENTRY_BYTES)
        );
    }

    #[test]
    fn context_reads_are_observation_and_context_mutations_are_interaction() {
        let run = AuditRun::new("return 1;", None).unwrap();
        for tool in [
            "ssmcp.context.status",
            "ssmcp.context.read",
            "ssmcp.context.list",
        ] {
            run.native_dispatched("reader", Some(tool));
        }
        assert!(run.steering_targets().is_empty());
        assert_eq!(run.targets(), ["reader"]);
        for tool in [
            "ssmcp.context.compact",
            "ssmcp.context.fork",
            "ssmcp.context.switch",
        ] {
            let run = AuditRun::new("return 1;", None).unwrap();
            run.native_dispatched("writer", Some(tool));
            assert_eq!(run.steering_targets(), ["writer"]);
        }
    }

    #[test]
    fn ui_observation_retains_source_audit_without_claiming_user_requirements() {
        let run = AuditRun::new("return await ui.get('display');", None).unwrap();
        for tool in [
            "ssmcp.ui.get",
            "ssmcp.ui.queue",
            "ssmcp.ui.set",
            "ssmcp.ui.action",
            "ssmcp.ui.filter",
            "ssmcp.ui.questions",
            "ssmcp.ui.history",
            "ssmcp.ui.harness",
            "ssmcp.ui.listener",
            "ssmcp.ui.wait",
            "ssmcp.ui.messages",
            "ssmcp.client.capabilities",
            "ssmcp.resources.list",
            "ssmcp.resources.templates",
            "ssmcp.resources.read",
        ] {
            run.native_dispatched("display", Some(tool));
            assert!(
                run.steering_targets().is_empty(),
                "unbound observation {tool} must not claim a user requirement"
            );
        }
        run.native_dispatched("catalog", None);
        assert_eq!(run.targets(), ["catalog", "display"]);
        assert!(run.steering_targets().is_empty());

        run.native_dispatched("worker", Some("terminal.status"));
        run.native_dispatched("files", Some("ssmcp.fs.read_text"));
        assert_eq!(run.steering_targets(), ["files", "worker"]);

        run.bind("display", "owner", "agent_one", "private-token")
            .unwrap();
        assert_eq!(run.steering_targets(), ["display", "files", "worker"]);
        assert_eq!(run.context.code, "return await ui.get('display');");
    }

    #[test]
    fn agent_messages_receive_next_caller_without_fabricating_an_exact_agent_binding() {
        for tool in ["ssmcp.ui.message", "ssmcp.ui.present"] {
            let run = AuditRun::new("return await say('work', 'Ready');", None).unwrap();
            run.native_dispatched("observed", Some("ssmcp.ui.history"));
            run.native_dispatched("work", Some(tool));
            assert_eq!(run.targets(), ["observed", "work"]);
            assert_eq!(run.steering_targets(), ["work"]);
            assert!(run.binding("work").is_none());

            run.bind("work", "owner", "agent_one", "private-token")
                .unwrap();
            run.native_dispatched("work", Some(tool));
            let bound = run.binding("work").unwrap();
            assert_eq!(bound.owner, "owner");
            assert_eq!(bound.agent_id, "agent_one");
            assert_eq!(bound.token, "private-token");
            assert!(
                run.bind("work", "owner", "agent_two", "other-token")
                    .is_err()
            );
            assert_eq!(run.binding("work").unwrap().agent_id, "agent_one");
            assert_eq!(run.steering_targets(), ["work"]);
        }
    }

    // Fixed test programs verify the runner/pipe protocol; execute() always requires bubblewrap.
    async fn trusted_execute(
        code: String,
        timeout_ms: u64,
        clients: Value,
        host: Host,
    ) -> Result<Value, String> {
        let command = prepare_command(|| {
            let mut command = Command::new(executable("node")?);
            command.env_clear();
            node_arguments(&mut command);
            command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/code_mode.js"));
            Ok(command)
        })
        .await?;
        execute_process(command, code, timeout_ms, clients, host).await
    }

    #[test]
    fn runtime_check_rejects_missing_wrong_and_error_results() {
        let expected = json!({"result":{"ssmcp_runtime_check":1,"clients":0},"logs":[]});
        validate_runtime_check(expected.clone(), false).unwrap();
        for output in [
            Value::Null,
            json!({"result":true,"logs":[]}),
            json!({"result":{"ssmcp_runtime_check":1,"clients":1},"logs":[]}),
            json!({"result":{"ssmcp_runtime_check":1,"clients":0},"logs":["error"]}),
            json!({"result":{"ssmcp_runtime_check":1,"clients":0},"logs":[],"isError":true}),
        ] {
            assert_eq!(
                validate_runtime_check(output, false).unwrap_err(),
                "code_mode runtime check returned an unexpected result"
            );
        }
        assert_eq!(
            validate_runtime_check(expected, true).unwrap_err(),
            "code_mode runtime check attempted a disabled native call"
        );
    }

    #[tokio::test]
    async fn runtime_check_denies_every_host_operation_and_records_the_attempt() {
        for request in [
            HostRequest::Builtin {
                name: "resources.read".into(),
                arguments: json!({}),
            },
            HostRequest::Call {
                client_id: "unavailable".into(),
                name: "ssmcp.exec.run".into(),
                arguments: json!({}),
            },
            HostRequest::Tools {
                client_id: "unavailable".into(),
                cursor: None,
            },
            HostRequest::WaitUser {
                client_id: "unavailable".into(),
                arguments: serde_json::from_value(json!({})).unwrap(),
            },
        ] {
            let (host, called) = runtime_check_host();
            assert!(!called.load(Ordering::Relaxed));
            assert_eq!(
                host(request).await.unwrap_err(),
                "native calls are disabled during the runtime check"
            );
            assert!(called.load(Ordering::Relaxed));
        }
    }

    #[tokio::test]
    async fn runtime_check_program_uses_the_runner_protocol_without_clients_or_host_calls() {
        let _guard = TESTS.lock().await;
        let (host, called) = runtime_check_host();
        let output = trusted_execute(RUNTIME_CHECK_CODE.into(), 2_000, json!([]), host)
            .await
            .unwrap();
        validate_runtime_check(output, called.load(Ordering::Relaxed)).unwrap();

        // A runner must not turn a swallowed host rejection into a successful check.
        let (host, called) = runtime_check_host();
        let output = trusted_execute(
            format!(
                "try {{ await call('unavailable','echo',{{}}); }} catch {{}} {RUNTIME_CHECK_CODE}"
            ),
            2_000,
            json!([]),
            host,
        )
        .await
        .unwrap();
        assert_eq!(
            validate_runtime_check(output, called.load(Ordering::Relaxed)).unwrap_err(),
            "code_mode runtime check attempted a disabled native call"
        );
    }

    #[tokio::test]
    #[ignore = "requires system Node.js and bubblewrap namespace support; CI runs --include-ignored"]
    async fn runtime_check_uses_the_mandatory_system_sandbox() {
        let _guard = TESTS.lock().await;
        check_runtime().await.unwrap();
    }

    #[tokio::test]
    async fn runtime_preparation_leaves_the_async_executor_responsive() {
        let _guard = TESTS.lock().await;
        let (entered, notice) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let task = tokio::spawn(prepare_command(move || {
            entered.send(()).map_err(|_| "fixture listener closed")?;
            gate.recv_timeout(Duration::from_secs(2))
                .map_err(|_| "async executor did not release runtime preparation")?;
            Ok(Command::new("unused-command-fixture"))
        }));
        // This test uses Tokio's single-thread runtime. A synchronous builder
        // cannot receive this release until its bounded fixture wait fails.
        notice.await.unwrap();
        release.send(()).unwrap();
        assert!(task.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cancelled_runtime_preparation_still_finishes_owned_cleanup() {
        let _guard = TESTS.lock().await;
        let (entered, notice) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (cleaned, cleanup) = tokio::sync::oneshot::channel();
        struct Cleanup(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let task = tokio::spawn(prepare_command(move || {
            let _cleanup = Cleanup(Some(cleaned));
            entered.send(()).map_err(|_| "fixture listener closed")?;
            gate.recv_timeout(Duration::from_secs(2))
                .map_err(|_| "fixture release missing")?;
            Ok(Command::new("unused-command-fixture"))
        }));
        notice.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(PREPARATIONS.available_permits(), MAX_CONCURRENT - 1);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            cleanup.await.unwrap();
            while PREPARATIONS.available_permits() != MAX_CONCURRENT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn host() -> Host {
        Arc::new(|request| {
            Box::pin(async move {
                match request {
                    HostRequest::Builtin { name, arguments } => {
                        Ok(json!({"name":name,"arguments":arguments}))
                    }
                    HostRequest::Call {
                        client_id,
                        name,
                        arguments,
                    } => {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        if name == "fail" {
                            return Err("native failure".into());
                        }
                        Ok(
                            json!({"content":[],"isError":name == "native_error","structuredContent":{"id":client_id,"name":name,"arguments":arguments},"_meta":{"native":true}}),
                        )
                    }
                    HostRequest::Tools { client_id, cursor } => Ok(
                        json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}],"id":client_id,"cursor":cursor}),
                    ),
                    HostRequest::WaitUser {
                        client_id,
                        arguments,
                    } => Ok(json!({
                        "client_id":client_id,"status":"timed_out","timeout_ms":arguments.timeout_ms
                    })),
                }
            })
        })
    }

    async fn run_code(code: &str) -> Result<Value, String> {
        trusted_execute(
            code.into(),
            2_000,
            json!([{"id":"one","name":"workstation","cwd":"/work","type":"shell"}]),
            host(),
        )
        .await
    }

    #[tokio::test]
    async fn user_wait_helper_uses_a_controlled_host_request() {
        let _guard = TESTS.lock().await;
        let result =
            run_code("return await ui.wait('one',{timeout_ms:1000,intent:'finish',notify:false});")
                .await
                .unwrap();
        assert_eq!(result["result"]["client_id"], "one");
        assert_eq!(result["result"]["status"], "timed_out");
        assert_eq!(result["result"]["timeout_ms"], 1000);
    }

    #[tokio::test]
    async fn real_javascript_awaits_parallel_calls_and_preserves_native_results() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const results = await Promise.all([1,2,3].map(n => call(clients[0].id, "echo", {n})));
            let sum = 0;
            for (const item of results) { if (!item.isError) sum += item.structuredContent.arguments.n; }
            console.log("sum", sum);
            const catalog = await tools(clients[0].id);
            return {sum, native:results[0], tool:catalog.tools[0].name};
        "#).await.unwrap();
        assert_eq!(output["result"]["sum"], 6);
        assert_eq!(output["result"]["native"]["_meta"]["native"], true);
        assert_eq!(output["result"]["tool"], "echo");
        assert_eq!(output["logs"], json!(["sum 6"]));
        assert_eq!(
            run_code("print('done');").await.unwrap()["result"],
            Value::Null
        );
    }

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";

    #[tokio::test]
    async fn native_screenshots_reach_mcp_as_images_without_base64_text_duplication() {
        let _guard = TESTS.lock().await;
        let output = crate::output::OutputStore::new_connection().unwrap();
        let host: Host = Arc::new(move |request| {
            let output = output.clone();
            Box::pin(async move {
                let HostRequest::Call {
                    name, arguments, ..
                } = request
                else {
                    return Err("unexpected screenshot fixture request".into());
                };
                assert_eq!(name, "ssmcp.browser.screenshot");
                assert_eq!(arguments["target_id"], "tab");
                Ok(output.guard_result(json!({
                    "isError":false,
                    "content":[{"type":"image","mimeType":"image/png","data":PNG}],
                    "structuredContent":{"width":1,"height":1,"target_id":"tab"}
                })))
            })
        });
        let output = trusted_execute(
            "const shot = await browser.screenshot('one',{target_id:'tab'}); image(shot); print('captured'); return shot.structuredContent;".into(),
            2_000, json!([{"id":"one"}]), host,
        ).await.unwrap();
        assert_eq!(output["images"].as_array().unwrap().len(), 1);
        let response = tool_result(Ok(output));
        assert_eq!(response["isError"], false);
        assert_eq!(response["content"].as_array().unwrap().len(), 2);
        assert_eq!(
            response["content"][1],
            json!({"type":"image","mimeType":"image/png","data":PNG})
        );
        assert_eq!(response["structuredContent"]["result"]["target_id"], "tab");
        assert_eq!(response["structuredContent"]["logs"], json!(["captured"]));
        assert!(response["structuredContent"].get("images").is_none());
        assert!(
            !response["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(PNG)
        );
    }

    #[tokio::test]
    async fn image_api_rejects_invalid_content_and_enforces_per_call_budgets() {
        let _guard = TESTS.lock().await;
        let valid = json!({"type":"image","mimeType":"image/png","data":PNG}).to_string();
        for (code, expected) in [
            ("image({});".to_owned(), "expects an image"),
            (
                format!("image({{isError:true,content:[{valid}]}});"),
                "failed native",
            ),
            (
                "image({type:'image',mimeType:'image/svg+xml',data:'PHN2Zy8+'});".into(),
                "PNG, JPEG, GIF or WebP",
            ),
            (
                "image({type:'image',mimeType:'image/png',data:'not-base64'});".into(),
                "base64",
            ),
            (
                "image({type:'image',mimeType:'image/png',data:'YWJj'});".into(),
                "MIME type",
            ),
            (
                "image({type:'image',mimeType:'image/png',data:'A'.repeat(550000)});".into(),
                "400 KiB",
            ),
            (format!("for(let i=0;i<5;i++) image({valid});"), "4 images"),
        ] {
            let error = run_code(&code).await.unwrap_err();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        let output = run_code(&format!("image({valid}); return 9;"))
            .await
            .unwrap();
        assert_eq!(output["result"], 9);
        assert_eq!(output["images"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn host_validates_image_frames_independently_of_the_javascript_worker() {
        let mut input = serde_json::to_vec(&json!({
            "type":"image","content":{"type":"image","mimeType":"image/png","data":"%%%"}
        }))
        .unwrap();
        input.push(b'\n');
        let mut logs = Vec::new();
        let mut images = Vec::new();
        let error = bridge(
            input.as_slice(),
            &mut Vec::new(),
            host(),
            &mut logs,
            &mut images,
        )
        .await
        .unwrap_err();
        assert!(error.contains("base64"), "{error}");
        assert!(images.is_empty());
    }

    #[test]
    fn public_image_response_enforces_aggregate_budgets() {
        use base64::{Engine, engine::general_purpose::STANDARD};
        let mut bytes = vec![0u8; crate::output::MAX_IMAGE_BYTES];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let image = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(&bytes)});
        let response = tool_result(Ok(json!({"result":null,"logs":[],"images":vec![image;4]})));
        assert_eq!(response["isError"], true);
        assert!(
            response["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("2 MiB")
        );
        let image = json!({"type":"image","mimeType":"image/png","data":PNG});
        let response = tool_result(Ok(json!({"result":null,"logs":[],"images":vec![image;5]})));
        assert_eq!(response["isError"], true);
        assert!(
            response["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("count")
        );
    }

    #[tokio::test]
    async fn computer_use_helpers_use_native_routes_without_unwrapping_results() {
        let _guard = TESTS.lock().await;
        let host: Host = Arc::new(|request| {
            Box::pin(async move {
                let HostRequest::Call {
                    name, arguments, ..
                } = request
                else {
                    return Err("unexpected fixture request".into());
                };
                Ok(crate::backend::result(
                    json!({"name":name,"arguments":arguments}),
                ))
            })
        });
        let output = trusted_execute(
            r#"return {
                computer:await computer.type('one',{text:'hello'}),
                browser:await browser.navigate('one',{target_id:'t',url:'about:blank'}),
                omarchy:await omarchy.audio_set('one',{target:'output',percent:25}),
                frozen:Object.isFrozen(computer)&&Object.isFrozen(browser)&&Object.isFrozen(omarchy),
                hidden:typeof __ssmcp_image
            };"#
            .into(),
            2_000,
            json!([]),
            host,
        )
        .await
        .unwrap();
        assert_eq!(
            output["result"]["computer"]["structuredContent"]["name"],
            "ssmcp.computer.type"
        );
        assert_eq!(
            output["result"]["computer"]["structuredContent"]["arguments"]["text"],
            "hello"
        );
        assert_eq!(
            output["result"]["browser"]["structuredContent"]["name"],
            "ssmcp.browser.navigate"
        );
        assert_eq!(output["result"]["frozen"], true);
        assert_eq!(
            output["result"]["omarchy"]["structuredContent"]["name"],
            "ssmcp.omarchy.audio_set"
        );
        assert_eq!(
            output["result"]["omarchy"]["structuredContent"]["arguments"]["percent"],
            25
        );
        assert_eq!(output["result"]["hidden"], "undefined");
    }

    #[tokio::test]
    async fn workspace_helpers_dispatch_through_the_authenticated_host_bridge() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const transfer = await rsync({source:{client_id:'one',path:'src'},destination:{client_id:'two',path:'dst'}});
            const resource = await resources.read('ssmcp://guide');
            const file = await fs.read('one',{path:'data.bin',offset:8});
            const mapping = await ports.list();
            const job = await jobs.wait('job',{wait_ms:100});
            const restarted = await reload('one');
            const artifact = await outputs.read('one',{id:'abc',offset:42,max_bytes:128});
            const metadata = await outputs.stat('one',{id:'abc'});
            const cleaned = await outputs.cleanup('one');
            return {transfer,resource,file,mapping,job,restarted,artifact,metadata,cleaned,alias:sync === rsync,
                frozen:[fs,jobs,ports,resources,outputs].every(Object.isFrozen)};
        "#).await.unwrap();
        assert_eq!(output["result"]["transfer"]["name"], "sync");
        assert_eq!(
            output["result"]["resource"]["arguments"]["uri"],
            "ssmcp://guide"
        );
        assert_eq!(output["result"]["file"]["arguments"]["offset"], 8);
        assert_eq!(output["result"]["mapping"]["name"], "ports.list");
        assert_eq!(output["result"]["job"]["arguments"]["wait_ms"], 100);
        assert_eq!(output["result"]["restarted"]["id"], "one");
        assert_eq!(output["result"]["restarted"]["arguments"], json!({}));
        assert_eq!(output["result"]["artifact"]["arguments"]["offset"], 42);
        assert_eq!(output["result"]["metadata"]["arguments"]["id"], "abc");
        assert_eq!(output["result"]["cleaned"]["arguments"], json!({}));
        assert_eq!(output["result"]["alias"], true);
        assert_eq!(output["result"]["frozen"], true);
        assert!(run_code("return await rsync([]);").await.is_err());
    }

    #[tokio::test]
    async fn collaboration_helpers_bind_room_and_keep_long_waits_outside_code_mode() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const joined = await collab.join('room',{name:'writer',client_id:'cannot-override'});
            const bound = await collab.bind('room',{agent_id:'a',token:'private',client_id:'cannot-override'});
            const messages = await collab.receive('room',{agent_id:'a',token:'private',after:7,wait_ms:1000});
            const sent = await collab.send('room',{agent_id:'a',token:'private',to:'b',payload:{file:'src/lib.rs'},idempotency_key:'handoff-1'});
            const acked = await collab.ack('room',{agent_id:'a',token:'private',through:8});
            let rejected = '';
            try { await collab.receive('room',{agent_id:'a',token:'private',wait_ms:30000}); }
            catch (error) { rejected = error.message; }
            return {joined,bound,messages,sent,acked,rejected,frozen:Object.isFrozen(collab)};
        "#).await.unwrap();
        let result = &output["result"];
        assert_eq!(result["joined"]["name"], "collab.join");
        assert_eq!(result["joined"]["arguments"]["client_id"], "room");
        assert_eq!(result["bound"]["name"], "collab.bind");
        assert_eq!(result["bound"]["arguments"]["client_id"], "room");
        assert_eq!(result["messages"]["name"], "collab.receive");
        assert_eq!(result["messages"]["arguments"]["after"], 7);
        assert_eq!(result["sent"]["arguments"]["payload"]["file"], "src/lib.rs");
        assert_eq!(result["sent"]["arguments"]["idempotency_key"], "handoff-1");
        assert_eq!(result["acked"]["arguments"]["through"], 8);
        assert!(
            result["rejected"]
                .as_str()
                .unwrap()
                .contains("direct collab tool")
        );
        assert_eq!(result["frozen"], true);
    }

    #[tokio::test]
    async fn session_helpers_use_selected_client_and_preserve_generation_guard() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const listed = await sessions.list('manager');
            const created = await sessions.create('manager',{name:'worker',cwd:'project'});
            const restarted = await sessions.restart('manager',{name:'worker',expected_generation:'old-generation'});
            const closed = await sessions.close('manager',{name:'worker'});
            return {listed,created,restarted,closed,frozen:Object.isFrozen(sessions)};
        "#).await.unwrap();
        assert_eq!(output["result"]["listed"]["name"], "ssmcp.session.list");
        assert_eq!(output["result"]["listed"]["id"], "manager");
        assert_eq!(output["result"]["created"]["arguments"]["cwd"], "project");
        assert_eq!(
            output["result"]["restarted"]["arguments"]["expected_generation"],
            "old-generation"
        );
        assert_eq!(output["result"]["closed"]["name"], "ssmcp.session.close");
        assert_eq!(output["result"]["frozen"], true);
    }

    #[tokio::test]
    async fn history_helpers_route_metadata_search_and_reject_action_overrides() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const list = await history.list('work',{limit:8});
            const metadata = await history.metadata('work',{id:'conversation_fixture'});
            const read = await history.read('work',{id:'conversation_fixture',offset:4,limit:1});
            const search = await history.search('work',{query:'登录',role:'user',case_sensitive:true,offset:2,limit:3});
            const rejected = [];
            for (const args of [null, [], 'bad', {action:'list'}]) {
                try { await history.search('work',args); }
                catch (error) { rejected.push(error.message); }
            }
            return {list,metadata,read,search,rejected,frozen:Object.isFrozen(history)};
        "#).await.unwrap();
        let result = &output["result"];
        for action in ["list", "metadata", "read", "search"] {
            assert_eq!(result[action]["name"], "ssmcp.ui.history");
            assert_eq!(result[action]["id"], "work");
            assert_eq!(result[action]["arguments"]["action"], action);
        }
        assert_eq!(
            result["metadata"]["arguments"]["id"],
            "conversation_fixture"
        );
        assert_eq!(result["read"]["arguments"]["offset"], 4);
        assert_eq!(
            result["search"]["arguments"],
            json!({
                "action":"search","query":"登录","role":"user","case_sensitive":true,"offset":2,"limit":3
            })
        );
        assert_eq!(result["rejected"].as_array().unwrap().len(), 4);
        assert_eq!(result["frozen"], true);
    }

    #[tokio::test]
    async fn ui_helpers_dispatch_atomic_client_operations_without_rewriting_arguments() {
        let _guard = TESTS.lock().await;
        let output = run_code(
            r#"
            return {
                state: await ui.get('one'),
                theme: await ui.set('one',{theme:'nord'}),
                action: await ui.action('one',{action:'audit.open'}),
                filter: await ui.filter('one',{text:'cargo',level:'commands'}),
                frozen: Object.isFrozen(ui)
            };
        "#,
        )
        .await
        .unwrap();
        assert_eq!(output["result"]["state"]["name"], "ssmcp.ui.get");
        assert_eq!(output["result"]["state"]["arguments"], json!({}));
        assert_eq!(output["result"]["theme"]["arguments"]["theme"], "nord");
        assert_eq!(
            output["result"]["action"]["arguments"]["action"],
            "audit.open"
        );
        assert_eq!(output["result"]["filter"]["name"], "ssmcp.ui.filter");
        assert_eq!(
            output["result"]["filter"]["arguments"],
            json!({"text":"cargo","level":"commands"})
        );
        assert_eq!(output["result"]["frozen"], true);
    }

    #[tokio::test]
    async fn execution_helpers_bound_parallelism_and_keep_every_command_result() {
        let _guard = TESTS.lock().await;
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let jobs = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            bool,
        >::new()));
        let host: Host = {
            let active = active.clone();
            let maximum = maximum.clone();
            let jobs = jobs.clone();
            Arc::new(move |request| {
                let active = active.clone();
                let maximum = maximum.clone();
                let jobs = jobs.clone();
                Box::pin(async move {
                    let HostRequest::Call {
                        name, arguments, ..
                    } = request
                    else {
                        return Err("unexpected fixture request".into());
                    };
                    let id = arguments["job_id"].as_str().unwrap_or("").to_owned();
                    let snapshot = || {
                        let running = *jobs.lock().unwrap().get(&id).expect("observed job");
                        json!({
                            "job_id":id,"running":running,
                            "exit_code":if running { None } else { Some(if id == "exit7" { 7 } else { 0 }) },
                            "state":if running {"running"} else {"completed"},
                            "stdout":{"next_cursor":id.len()},"stderr":{"next_cursor":0}
                        })
                    };
                    let result = match name.as_str() {
                        "ssmcp.exec.start" => {
                            let label = arguments["command"][0].as_str().unwrap();
                            if label == "missing" {
                                return Err("fixture command not found".into());
                            }
                            let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                            maximum.fetch_max(count, Ordering::SeqCst);
                            assert!(count <= 2, "poll deadlines cannot release live job slots");
                            assert!(jobs.lock().unwrap().insert(label.into(), true).is_none());
                            json!({"job_id":label,"running":true})
                        }
                        "ssmcp.exec.wait" => {
                            if arguments["wait_ms"] == 60000 {
                                tokio::time::sleep(Duration::from_millis(if id == "first" {
                                    40
                                } else {
                                    1
                                }))
                                .await;
                                assert_eq!(
                                    jobs.lock().unwrap().insert(id.clone(), false),
                                    Some(true)
                                );
                                active.fetch_sub(1, Ordering::SeqCst);
                            } else {
                                assert_eq!(arguments["wait_ms"], 1);
                            }
                            let mut waited = snapshot();
                            waited["wait_timed_out"] = waited["running"].clone();
                            waited
                        }
                        "ssmcp.exec.read" => {
                            assert_eq!(arguments["max_bytes"], 2048);
                            // Reading a live job must not pretend that it has exited.
                            json!({"job_id":id,"stdout":id,"stderr":"","snapshot":snapshot()})
                        }
                        _ => return Err(format!("unexpected fixture tool {name}")),
                    };
                    Ok(crate::backend::result(result))
                })
            })
        };
        let output = trusted_execute(
            r#"
                const results = await exec.parallel('one', [
                    {command:['first'],wait_ms:0}, {command:['live'],wait_ms:1},
                    {command:['exit7'],wait_ms:0}, {command:['missing'],wait_ms:0}
                ], {concurrency:2});
                return {results, frozen:Object.isFrozen(exec)};
            "#
            .into(),
            2_000,
            json!([{"id":"one"}]),
            host,
        )
        .await
        .unwrap();
        let results = &output["result"]["results"];
        assert_eq!(results[0]["value"]["stdout"], "first");
        assert_eq!(results[1]["value"]["running"], false);
        assert_eq!(results[1]["value"]["wait_timed_out"], false);
        assert_eq!(results[1]["value"]["job_id"], "live");
        assert_eq!(results[1]["value"]["snapshot"]["stdout"]["next_cursor"], 4);
        assert_eq!(results[2]["status"], "fulfilled");
        assert_eq!(results[2]["value"]["exit_code"], 7);
        assert_eq!(results[3]["status"], "rejected");
        assert!(
            results[3]["reason"]
                .as_str()
                .unwrap()
                .contains("command not found")
        );
        assert_eq!(output["result"]["frozen"], true);
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn execution_run_validates_before_start_and_preserves_job_id_after_start_errors() {
        let _guard = TESTS.lock().await;
        let starts = Arc::new(AtomicUsize::new(0));
        let host: Host = {
            let starts = starts.clone();
            Arc::new(move |request| {
                let starts = starts.clone();
                Box::pin(async move {
                    match request {
                        HostRequest::Call { name, .. } if name == "ssmcp.exec.start" => {
                            starts.fetch_add(1, Ordering::SeqCst);
                            Ok(crate::backend::result(
                                json!({"job_id":"started-once","running":true}),
                            ))
                        }
                        _ => Err("fixture observation failed".into()),
                    }
                })
            })
        };
        let output = trusted_execute(
            r#"
                const invalid = [];
                for (const options of [{wait_ms:60001}, {max_bytes:0}]) {
                    try { await exec.run('one',{command:['unused'],...options}); }
                    catch (error) { invalid.push(error.message); }
                }
                let after;
                try { await exec.run('one',{command:['run-once']}); }
                catch (error) { after = error.message; }
                return {invalid, after};
            "#
            .into(),
            2_000,
            json!([{"id":"one"}]),
            host,
        )
        .await
        .unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(output["result"]["invalid"].as_array().unwrap().len(), 2);
        let after = output["result"]["after"].as_str().unwrap();
        assert!(after.contains("started-once"), "{after}");
        assert!(after.contains("fixture observation failed"), "{after}");
        assert!(after.contains("exec.status/exec.read"), "{after}");
    }

    #[tokio::test]
    async fn execution_job_helpers_preserve_both_signatures_through_the_runner() {
        let _guard = TESTS.lock().await;
        let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let host: Host = {
            let requests = requests.clone();
            Arc::new(move |request| {
                let requests = requests.clone();
                Box::pin(async move {
                    let HostRequest::Call {
                        client_id,
                        name,
                        arguments,
                    } = request
                    else {
                        return Err("unexpected fixture request".into());
                    };
                    assert_eq!(client_id, "one");
                    let receipt = json!({"name":name,"arguments":arguments});
                    requests.lock().unwrap().push(receipt.clone());
                    Ok(crate::backend::result(receipt))
                })
            })
        };
        let output = trusted_execute(
            r#"
                const replies = [];
                const cases = [
                    ['status', {}],
                    ['wait', {wait_ms:13}],
                    ['read', {stdout_cursor:7,stderr_cursor:3,max_bytes:64,wait_ms:0}],
                    ['write', {data:'中文\n',close_stdin:true}],
                    ['cancel', {}],
                    ['forget', {}],
                ];
                for (const [name, options] of cases) {
                    replies.push(await exec[name]('one',
                        Object.freeze({job_id:'job',...options})));
                    if (['wait','read','write'].includes(name)) {
                        replies.push(await exec[name]('one', 'job',
                            Object.freeze({...options,job_id:'cannot-override'})));
                    } else {
                        replies.push(await exec[name]('one', 'job'));
                    }
                }
                return replies;
            "#
            .into(),
            2_000,
            json!([{"id":"one"}]),
            host,
        )
        .await
        .unwrap();
        let mut expected = Vec::new();
        for (name, mut arguments) in [
            ("status", json!({})),
            ("wait", json!({"wait_ms":13})),
            (
                "read",
                json!({"stdout_cursor":7,"stderr_cursor":3,"max_bytes":64,"wait_ms":0}),
            ),
            ("write", json!({"data":"中文\n","close_stdin":true})),
            ("cancel", json!({})),
            ("forget", json!({})),
        ] {
            arguments["job_id"] = json!("job");
            let receipt = json!({"name":format!("ssmcp.exec.{name}"),"arguments":arguments});
            expected.push(receipt.clone());
            expected.push(receipt);
        }
        assert_eq!(*requests.lock().unwrap(), expected);
        assert_eq!(output["result"], json!(expected));
    }

    #[tokio::test]
    async fn execution_job_helper_failures_cross_the_rust_host_once() {
        let _guard = TESTS.lock().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let host: Host = {
            let requests = requests.clone();
            Arc::new(move |request| {
                let requests = requests.clone();
                Box::pin(async move {
                    let HostRequest::Call {
                        client_id,
                        name,
                        arguments,
                    } = request
                    else {
                        return Err("unexpected fixture request".into());
                    };
                    assert_eq!(client_id, "one");
                    assert_eq!(arguments, json!({"job_id":"job"}));
                    requests.fetch_add(1, Ordering::SeqCst);
                    match name.as_str() {
                        "ssmcp.exec.read" => Err("fixture read transport interrupted".into()),
                        "ssmcp.exec.cancel" => Ok(json!({
                            "isError":true,
                            "content":[{"type":"text","text":"fixture cancellation denied"}]
                        })),
                        _ => Err(format!("unexpected fixture tool {name}")),
                    }
                })
            })
        };
        let output = trusted_execute(
            r#"
                let invalid;
                try {
                    await exec.cancel('one', {job_id:'job'}, {});
                } catch (error) {
                    invalid = error.message;
                }
                const native = [];
                for (const name of ['read','cancel']) {
                    try {
                        await exec[name]('one', {job_id:'job'});
                        native.push(null);
                    } catch (error) {
                        native.push(error.message);
                    }
                }
                return {invalid,native};
            "#
            .into(),
            2_000,
            json!([{"id":"one"}]),
            host,
        )
        .await
        .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert!(
            output["result"]["invalid"]
                .as_str()
                .is_some_and(|message| !message.is_empty())
        );
        assert_eq!(
            output["result"]["native"],
            json!([
                "fixture read transport interrupted",
                "fixture cancellation denied"
            ])
        );
    }

    #[tokio::test]
    async fn sandbox_has_no_os_capabilities_or_mutable_client_metadata() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const absent = [typeof process, typeof require, typeof fetch, typeof std, typeof os, typeof __ssmcp_host];
            let frozen = false;
            try { clients[0].cwd = "/changed"; } catch (e) { frozen = true; }
            return {absent,frozen,cwd:clients[0].cwd};
        "#).await.unwrap();
        assert_eq!(output["result"]["absent"], json!(vec!["undefined"; 6]));
        assert_eq!(output["result"]["frozen"], true);
        assert_eq!(output["result"]["cwd"], "/work");
        assert!(run_code("return await import('fs');").await.is_err());
    }

    #[tokio::test]
    async fn errors_keep_logs_and_native_errors_remain_observable() {
        let _guard = TESTS.lock().await;
        let error = run_code("print('before'); throw new Error('broken');")
            .await
            .unwrap_err();
        assert!(error.contains("broken"));
        assert!(error.contains("before"));
        assert!(
            run_code("return await call('one','fail',{});")
                .await
                .unwrap_err()
                .contains("native failure")
        );
        assert_eq!(
            run_code("return await call('one','native_error',{});")
                .await
                .unwrap()["result"]["isError"],
            true
        );
        assert!(
            run_code("Promise.reject('unobserved'); return 1;")
                .await
                .unwrap_err()
                .contains("unobserved")
        );
        assert!(
            run_code("call('one','echo',{}); return 1;")
                .await
                .unwrap_err()
                .contains("pending native calls")
        );
        assert!(run_code("return {;").await.is_err());
    }

    #[tokio::test]
    async fn runaway_cpu_and_waiting_promises_stop_at_the_deadline() {
        let _guard = TESTS.lock().await;
        for code in [
            "while (true) {}",
            "while (true) { await Promise.resolve(); }",
            "await new Promise(() => {});",
        ] {
            let start = Instant::now();
            let error = trusted_execute(code.into(), 40, json!([]), host())
                .await
                .unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        let pending: Host = Arc::new(|_| Box::pin(std::future::pending()));
        let error = trusted_execute(
            "await call('one','wait',{});".into(),
            40,
            json!([]),
            pending,
        )
        .await
        .unwrap_err();
        assert!(error.contains("outcome may be unknown"));
        assert_eq!(run_code("return 7;").await.unwrap()["result"], 7);
    }

    #[tokio::test]
    async fn memory_output_and_call_budgets_are_enforced() {
        let _guard = TESTS.lock().await;
        assert!(
            run_code("return 'x'.repeat(1048577);")
                .await
                .unwrap_err()
                .contains("result exceeds")
        );
        assert!(
            run_code("print('x'.repeat(65536));")
                .await
                .unwrap_err()
                .contains("log output exceeds")
        );
        assert!(
            run_code("const a=[]; while(true) a.push(new Uint8Array(1024*1024));")
                .await
                .is_err()
        );
        let error = run_code("for (let i=0;i<129;i++) await call('one','echo',{});")
            .await
            .unwrap_err();
        assert!(error.contains("128 native calls"), "{error}");
        assert_eq!(run_code("return 9;").await.unwrap()["result"], 9);
    }

    #[tokio::test]
    async fn cancelling_the_request_releases_worker_and_host_future() {
        let _guard = TESTS.lock().await;
        let entered = Arc::new(Notify::new());
        let released = Arc::new(AtomicBool::new(false));
        struct Released(Arc<AtomicBool>);
        impl Drop for Released {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let notice = entered.clone();
        let flag = released.clone();
        let host: Host = Arc::new(move |_| {
            let notice = notice.clone();
            let flag = flag.clone();
            Box::pin(async move {
                let _released = Released(flag);
                notice.notify_one();
                std::future::pending().await
            })
        });
        let entered_notice = entered.notified();
        tokio::pin!(entered_notice);
        entered_notice.as_mut().enable();
        let mut task = tokio::spawn(trusted_execute(
            "await call('one','wait',{});".into(),
            MAX_TIMEOUT_MS,
            json!([]),
            host,
        ));
        tokio::select! {
            biased;
            _ = &mut entered_notice => {},
            result = &mut task => {
                // This branch already joined the worker; polling it again would panic.
                task.abort();
                panic!("code_mode worker exited before entering the pending host call: {result:?}");
            },
            _ = tokio::time::sleep(Duration::from_secs(15)) => {
                task.abort();
                let result = task.await;
                panic!("code_mode worker did not enter the pending host call within 15 seconds; aborted worker: {result:?}");
            },
        }
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) || SLOTS.available_permits() != MAX_CONCURRENT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    #[ignore = "requires system Node.js and bubblewrap namespace support; CI runs --include-ignored"]
    async fn real_system_runtime_uses_mandatory_os_isolation() {
        let _guard = TESTS.lock().await;
        let result = execute(
            "return await call('one','echo',{value:42});".into(),
            10_000,
            json!([]),
            host(),
        )
        .await
        .unwrap();
        assert_eq!(
            result["result"]["structuredContent"]["arguments"]["value"],
            42
        );
        // Probe the OS profile directly, independently of the VM's API restrictions.
        let base = sandbox_command().unwrap();
        let mut arguments: Vec<_> = base.as_std().get_args().map(|a| a.to_owned()).collect();
        assert_eq!(arguments.pop().unwrap(), "/runner.js");
        let mut probe = Command::new(base.as_std().get_program());
        probe.env_clear().args(arguments).args(["--eval", r#"
            const fs = require('fs');
            const denied = ['/etc/passwd','/proc/1/root/etc/passwd','/home','/run'].map(p => { try { fs.readFileSync(p); return false; } catch (_) { return true; } });
            let readonly = false;
            try { fs.writeFileSync('/work/probe','x'); } catch (_) { readonly = true; }
            fs.writeFileSync('/tmp/probe','x');
            const interfaces = Object.values(require('os').networkInterfaces()).flat();
            const value = {denied,readonly,env:process.env,uid:process.getuid(),pid:process.pid,proc:fs.existsSync('/proc'),loopbackOnly:interfaces.every(address => address.internal)};
            process.stdout.write(JSON.stringify({type:'result',value})+'\n');
        "#]);
        let result = execute_process(probe, "return null;".into(), 10_000, json!([]), host())
            .await
            .unwrap();
        assert_eq!(result["result"]["denied"], json!([true, true, true, true]));
        assert_eq!(result["result"]["readonly"], true);
        assert_eq!(result["result"]["uid"], 65534);
        assert_eq!(result["result"]["proc"], false);
        assert_eq!(result["result"]["loopbackOnly"], true);
        assert!((1..16).contains(&result["result"]["pid"].as_u64().unwrap()));
        assert!(
            result["result"]["env"]
                .as_object()
                .unwrap()
                .keys()
                .all(|k| matches!(k.as_str(), "HOME" | "LANG" | "PATH" | "PWD"))
        );
    }

    #[test]
    fn sandbox_profile_has_no_host_accounts_or_network_access() {
        let command = sandbox_command().unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|v| v == "--unshare-all"));
        assert!(args.iter().any(|v| v == "--unshare-user"));
        assert!(args.iter().any(|v| v == "--disable-userns"));
        assert!(args.windows(5).any(|window| window
            == [
                "--",
                "/runtime/prlimit",
                "--nproc=128:128",
                "--",
                "/runtime/node"
            ]));
        assert!(!args.iter().any(|v| matches!(
            v.as_str(),
            "--share-net" | "--proc" | "/proc" | "/etc" | "/home" | "/run" | "/usr"
        )));
        assert_eq!(command.as_std().get_envs().count(), 0);
    }
}
