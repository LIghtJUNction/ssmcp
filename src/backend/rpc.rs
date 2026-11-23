//! A small bidirectional JSON-lines peer for native MCP, Codex App Server and Pi.
use super::{
    events::{AuditMode, EventLog},
    process::Managed,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    process::Stdio,
    sync::{
        Arc, Mutex as SyncMutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{
        AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
        BufReader,
    },
    process::Command,
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::timeout,
};

const MAX_NATIVE_FRAME: usize = 16 * 1024 * 1024;
const MAX_CALLS: usize = 8; // Leave bridge slots free for approvals, interrupt and event reads.
const MAX_DISCOVERY_CALLS: usize = 1; // Refresh must not kill a provider with eight active tool calls.
const MAX_REQUESTS: usize = 64;
type Response = Result<Value, String>;
type NativeWriter = Box<dyn AsyncWrite + Unpin + Send>;

tokio::task_local! {
    static HARNESS_CALL: HarnessCall;
}

struct CallObservation {
    id: u64,
    command_started: AtomicBool,
    command_completed: AtomicBool,
}

/// Correlation belongs to this dispatch future, never to the native protocol.
/// A shared observation survives the separate task that receives the RPC reply.
#[derive(Clone)]
pub(crate) struct HarnessCall(Arc<CallObservation>);

impl HarnessCall {
    /// Carry the same observation across a local plugin executor boundary.
    pub(crate) fn current() -> Option<Self> {
        HARNESS_CALL.try_with(Clone::clone).ok()
    }

    pub(crate) fn current_id() -> Option<u64> {
        HARNESS_CALL.try_with(|call| call.0.id).ok()
    }

    pub(crate) fn new(id: u64) -> Self {
        Self(Arc::new(CallObservation {
            id,
            command_started: AtomicBool::new(false),
            command_completed: AtomicBool::new(false),
        }))
    }

    pub(crate) async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        HARNESS_CALL.scope(self.clone(), future).await
    }

    pub(crate) fn command_started(&self) -> bool {
        self.0.command_started.load(Ordering::Acquire)
    }

    pub(crate) fn command_completed(&self) -> bool {
        self.0.command_completed.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    JsonRpc,
    Pi,
}

struct Waiting {
    method: String,
    internal_discovery: bool,
    harness_call: Option<HarnessCall>,
    reply: oneshot::Sender<Response>,
}

pub struct Peer {
    dialect: Dialect,
    // Fields drop in declaration order. Cancellation can skip async shutdown, so
    // Managed must stop descendants before closing stdin lets their parent exit.
    process: Mutex<Option<Managed>>,
    writer: Mutex<Option<NativeWriter>>,
    reader: SyncMutex<Option<JoinHandle<()>>>,
    stderr_reader: SyncMutex<Option<JoinHandle<()>>>,
    calls: SyncMutex<HashMap<String, Waiting>>,
    requests: SyncMutex<HashMap<String, Value>>,
    request_prefix: String,
    next: AtomicU64,
    alive: AtomicBool,
    events: Arc<EventLog>,
    // Cancellation Drop cannot await a contended process lock. A weak self
    // reference schedules cleanup without creating a permanent ownership cycle.
    weak: Weak<Self>,
}

impl Peer {
    pub async fn spawn(
        command: Command,
        label: &str,
        dialect: Dialect,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_with_output(
            command,
            label,
            dialect,
            crate::output::OutputStore::global()?,
        )
        .await
    }

    pub async fn spawn_with_output(
        command: Command,
        label: &str,
        dialect: Dialect,
        output: Arc<crate::output::OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_with_output_and_audit(command, label, dialect, output, AuditMode::default())
            .await
    }

    pub async fn spawn_with_output_and_audit(
        mut command: Command,
        label: &str,
        dialect: Dialect,
        output: Arc<crate::output::OutputStore>,
        audit_mode: AuditMode,
    ) -> crate::Result<Arc<Self>> {
        crate::program::configure(&mut command);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Managed::spawn(&mut command)?;
        let writer = process
            .child
            .stdin
            .take()
            .ok_or("backend stdin unavailable")?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or("backend stdout unavailable")?;
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or("backend stderr unavailable")?;
        let peer = Self::from_transport(
            stdout,
            Box::new(writer),
            Some(process),
            label,
            dialect,
            output,
            audit_mode,
        )?;
        let events = Arc::downgrade(&peer.events);
        let stderr_task = tokio::spawn(async move {
            let mut bytes = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut bytes).await {
                if n == 0 {
                    break;
                }
                let Some(events) = events.upgrade() else {
                    break;
                };
                events.push(json!({"type":"stderr","text":String::from_utf8_lossy(&bytes[..n])}));
            }
        });
        *peer.stderr_reader.lock().unwrap() = Some(stderr_task);
        Ok(peer)
    }

    /// Connect only to an explicitly selected, already-running native service.
    /// This peer never owns or terminates the process behind the socket.
    #[cfg(unix)]
    pub async fn connect_unix_with_output(
        path: &std::path::Path,
        label: &str,
        dialect: Dialect,
        output: Arc<crate::output::OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        if !path.is_absolute() {
            return Err("native RPC socket requires an explicit absolute local path".into());
        }
        let parent = path
            .parent()
            .ok_or("native RPC socket has no parent directory")?;
        let directory = std::fs::symlink_metadata(parent)?;
        let metadata = std::fs::symlink_metadata(path)?;
        // A private nonsymlink parent protects the checked path. Also verify
        // the actual peer, rather than trusting only a filesystem pathname.
        let uid = unsafe { libc::geteuid() };
        if !directory.is_dir()
            || directory.uid() != uid
            || directory.mode() & 0o077 != 0
            || !metadata.file_type().is_socket()
            || metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
        {
            return Err(
                "native RPC requires a socket and parent directory privately owned by this user"
                    .into(),
            );
        }
        let stream = timeout(
            Duration::from_secs(5),
            tokio::net::UnixStream::connect(path),
        )
        .await
        .map_err(|_| "native RPC socket connection timed out")??;
        if stream.peer_cred()?.uid() != uid {
            return Err("native RPC socket peer belongs to another user".into());
        }
        let (reader, writer) = stream.into_split();
        Self::from_transport(
            reader,
            Box::new(writer),
            None,
            label,
            dialect,
            output,
            AuditMode::default(),
        )
    }

    fn from_transport(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: NativeWriter,
        process: Option<Managed>,
        label: &str,
        dialect: Dialect,
        output: Arc<crate::output::OutputStore>,
        audit_mode: AuditMode,
    ) -> crate::Result<Arc<Self>> {
        // An attached service can outlive this connection and retain old replies.
        // Never reuse an earlier connection's request ID namespace.
        let mut nonce = [0u8; 16];
        crate::platform::random_bytes(&mut nonce)?;
        let request_prefix: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let peer = Arc::new_cyclic(|weak| Self {
            dialect,
            process: Mutex::new(process),
            writer: Mutex::new(Some(writer)),
            reader: SyncMutex::new(None),
            stderr_reader: SyncMutex::new(None),
            calls: SyncMutex::new(HashMap::new()),
            requests: SyncMutex::new(HashMap::new()),
            request_prefix,
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            events: EventLog::with_output_and_audit(label, output, audit_mode),
            weak: weak.clone(),
        });
        let mut stdout = BufReader::new(reader);
        let weak = Arc::downgrade(&peer);
        let task = tokio::spawn(async move {
            loop {
                let message = read_json(&mut stdout).await;
                let Some(peer) = weak.upgrade() else {
                    break;
                };
                match message {
                    Ok(Some(message)) => {
                        if let Err(error) = peer.receive(message) {
                            peer.fail(&error);
                            break;
                        }
                    }
                    Ok(None) => {
                        peer.fail("native peer closed its output stream");
                        break;
                    }
                    Err(error) => {
                        peer.fail(&format!("native JSON stream failed: {error}"));
                        break;
                    }
                }
            }
        });
        *peer.reader.lock().unwrap() = Some(task);
        Ok(peer)
    }

    pub fn events(&self) -> Arc<EventLog> {
        self.events.clone()
    }

    pub async fn send(&self, value: Value) -> Result<(), String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("native process disconnected".into());
        }
        let mut encoded = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        if encoded.len() > MAX_NATIVE_FRAME {
            return Err("native request exceeds 16 MiB".into());
        }
        let outcome = timeout(Duration::from_secs(10), async {
            let mut guard = self.writer.lock().await;
            // A queued sender can wake after another write poisoned this pipe.
            if !self.alive.load(Ordering::Acquire) {
                return Err(io::Error::other("native process disconnected"));
            }
            let writer = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("native stdin closed"))?;
            struct IncompleteFrame<'a> {
                peer: &'a Peer,
                armed: bool,
            }
            impl Drop for IncompleteFrame<'_> {
                fn drop(&mut self) {
                    if self.armed {
                        self.peer.interrupted_write();
                    }
                }
            }
            // Construct after acquiring the writer: cancelling while merely
            // queued cannot have written bytes and must not disconnect a peer.
            let mut frame = IncompleteFrame {
                peer: self,
                armed: true,
            };
            writer.write_all(&encoded).await?;
            writer.flush().await?;
            frame.armed = false;
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(|_| "native stdin write timed out".to_string())
        .and_then(|result| result.map_err(|e| e.to_string()));
        if let Err(error) = &outcome {
            // A cancelled/failed write may leave half a JSON frame in the pipe.
            // Never reuse it for a subsequent request or retry a mutating call.
            self.fail(error);
            if let Some(process) = self.process.lock().await.as_mut() {
                process.shutdown().await;
            }
            self.writer.lock().await.take();
        }
        outcome
    }

    fn interrupted_write(&self) {
        // This runs even when an outer timeout/abort drops send before its
        // normal error branch. No later frame may follow a partial JSON line.
        self.fail("native stdin write interrupted; transport cannot be reused");
        if let Ok(mut process) = self.process.try_lock()
            && let Some(process) = process.as_mut()
        {
            process.terminate();
        }
        if let (Ok(runtime), Some(peer)) =
            (tokio::runtime::Handle::try_current(), self.weak.upgrade())
        {
            // Keep stdin open until shutdown has captured and stopped owned
            // descendants. Also reap if termination happened synchronously.
            runtime.spawn(async move {
                peer.shutdown().await;
            });
        }
    }

    pub async fn request(&self, method: &str, params: Value) -> Response {
        self.request_with_origin(method, params, false).await
    }

    /// Mark only the audit copies of this exact discovery request and response.
    /// Native JSON, unrelated notifications and stderr keep their own provenance.
    #[cfg(test)]
    pub(crate) async fn request_discovery(&self, method: &str, params: Value) -> Response {
        self.request_with_origin(method, params, true).await
    }

    async fn request_with_origin(
        &self,
        method: &str,
        params: Value,
        internal_discovery: bool,
    ) -> Response {
        let harness_call = HARNESS_CALL.try_with(Clone::clone).ok();
        let id = format!(
            "ssmcp-{}-{}",
            self.request_prefix,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let key = json!(id).to_string();
        let (reply, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            let used = calls
                .values()
                .filter(|call| call.internal_discovery == internal_discovery)
                .count();
            if internal_discovery && used >= MAX_DISCOVERY_CALLS {
                return Err("native discovery busy; at most 1 outstanding discovery call".into());
            }
            if !internal_discovery && used >= MAX_CALLS {
                return Err("native backend busy; at most 8 outstanding native calls".into());
            }
            calls.insert(
                key.clone(),
                Waiting {
                    method: method.into(),
                    internal_discovery,
                    harness_call: harness_call.clone(),
                    reply,
                },
            );
        }
        struct Remove<'a> {
            calls: &'a SyncMutex<HashMap<String, Waiting>>,
            key: String,
        }
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                self.calls.lock().unwrap().remove(&self.key);
            }
        }
        let _remove = Remove {
            calls: &self.calls,
            key,
        };
        let message = match self.dialect {
            Dialect::JsonRpc => json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            Dialect::Pi => {
                let mut arguments = params
                    .as_object()
                    .cloned()
                    .ok_or("Pi arguments must be an object")?;
                if arguments.contains_key("id") || arguments.contains_key("type") {
                    return Err("Pi arguments may not override id or type".into());
                }
                arguments.insert("id".into(), json!(id));
                arguments.insert("type".into(), json!(method));
                Value::Object(arguments)
            }
        };
        if !matches!(method, "initialize" | "tools/list") {
            let mut audit = json!({"type":"ssmcp.request","request":message});
            if internal_discovery {
                audit["internal_discovery"] = true.into();
            }
            if let Some(call) = &harness_call {
                audit["harness_call_id"] = call.0.id.into();
            }
            self.events.push(audit);
            if method == "command/exec"
                && let Some(call) = &harness_call
            {
                call.0.command_started.store(true, Ordering::Release);
            }
        }
        self.send(message).await?;
        timeout(Duration::from_secs(900), receiver)
            .await
            .map_err(|_| {
                "native request timed out after 15 minutes; its outcome may be unknown".to_string()
            })?
            .map_err(|_| "native response channel closed".to_string())?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        if self.dialect == Dialect::Pi {
            return Err(
                "use Pi request or respond; arbitrary notifications are not defined".into(),
            );
        }
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    pub async fn pending(&self) -> Value {
        let mut requests: Vec<_> = self.requests.lock().unwrap().values().cloned().collect();
        requests.sort_by_key(|request| request["id"].to_string());
        json!({"requests":requests,"connected":self.alive.load(Ordering::Acquire)})
    }

    pub async fn respond(&self, args: Value) -> Response {
        let object = args.as_object().ok_or("response must be an object")?;
        let id = object
            .get("id")
            .filter(|id| id.is_string() || id.is_i64() || id.is_u64())
            .ok_or("id must identify a pending native request")?;
        let key = id.to_string();
        let request = self
            .requests
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or("no pending native request with this id")?;
        let message = match self.dialect {
            Dialect::JsonRpc => {
                if object
                    .keys()
                    .any(|k| !matches!(k.as_str(), "id" | "result" | "error"))
                    || object.contains_key("result") == object.contains_key("error")
                {
                    return Err("provide id and exactly one of result or error".into());
                }
                if let Some(error) = object.get("error")
                    && (error.get("code").and_then(Value::as_i64).is_none()
                        || error.get("message").and_then(Value::as_str).is_none())
                {
                    return Err("error requires an integer code and string message".into());
                }
                let mut message = object.clone();
                message.insert("jsonrpc".into(), json!("2.0"));
                Value::Object(message)
            }
            Dialect::Pi => {
                let field_count = ["value", "confirmed", "cancelled"]
                    .iter()
                    .filter(|field| object.contains_key(**field))
                    .count();
                if field_count != 1
                    || object
                        .keys()
                        .any(|k| !matches!(k.as_str(), "id" | "value" | "confirmed" | "cancelled"))
                {
                    return Err(
                        "provide id and exactly one of value, confirmed, or cancelled".into(),
                    );
                }
                if object.get("value").is_some_and(|v| !v.is_string())
                    || object.get("confirmed").is_some_and(|v| !v.is_boolean())
                    || object.get("cancelled").is_some_and(|v| v != &json!(true))
                {
                    return Err(
                        "value must be a string, confirmed a boolean, cancelled must be true"
                            .into(),
                    );
                }
                if !object.contains_key("cancelled")
                    && ((request["method"] == "confirm") != object.contains_key("confirmed"))
                {
                    return Err(
                        "confirm requests require confirmed; other dialogs require value".into(),
                    );
                }
                let mut message = object.clone();
                message.insert("type".into(), json!("extension_ui_response"));
                Value::Object(message)
            }
        };
        // Remove before awaiting: simultaneous callers cannot answer the same request twice.
        if self.requests.lock().unwrap().remove(&key).is_none() {
            return Err("native request already answered".into());
        }
        if let Err(error) = self.send(message.clone()).await {
            // Do not resend automatically: a partial write can have an uncertain outcome.
            return Err(format!(
                "native response write failed; outcome may be unknown: {error}"
            ));
        }
        self.events
            .push(json!({"type":"ssmcp.response","response":message}));
        Ok(json!({"sent":true,"id":id}))
    }

    fn receive(&self, mut message: Value) -> Result<(), String> {
        if !message.is_object() {
            return Err("native message must be an object".into());
        }
        let response = match self.dialect {
            Dialect::JsonRpc => {
                message.get("id").is_some()
                    && (message.get("result").is_some() || message.get("error").is_some())
            }
            Dialect::Pi => message["type"] == "response",
        };
        if response {
            let key = message
                .get("id")
                .ok_or("native response has no id")?
                .to_string();
            let waiting = self.calls.lock().unwrap().remove(&key);
            if let Some(waiting) = waiting {
                if !matches!(waiting.method.as_str(), "initialize" | "tools/list") {
                    let mut audit = message.clone();
                    // A peer cannot claim the bridge's internal provenance.
                    audit.as_object_mut().unwrap().remove("internal_discovery");
                    audit.as_object_mut().unwrap().remove("harness_call_id");
                    if waiting.internal_discovery {
                        audit["internal_discovery"] = true.into();
                    }
                    if let Some(call) = &waiting.harness_call {
                        audit["harness_call_id"] = call.0.id.into();
                    }
                    self.events.push(audit);
                    if waiting.method == "command/exec"
                        && let Some(call) = &waiting.harness_call
                    {
                        call.0.command_completed.store(true, Ordering::Release);
                    }
                }
                let result = if self.dialect == Dialect::Pi {
                    Ok(message)
                } else if let Some(error) = message.get("error") {
                    Err(format!("native RPC error: {error}"))
                } else {
                    Ok(message["result"].take())
                };
                let _ = waiting.reply.send(result);
            } else {
                let mut audit = message;
                audit.as_object_mut().unwrap().remove("internal_discovery");
                audit.as_object_mut().unwrap().remove("harness_call_id");
                self.events
                    .push(json!({"type":"ssmcp.late_response","response":audit}));
            }
            return Ok(());
        }
        let is_request = match self.dialect {
            Dialect::JsonRpc => message.get("id").is_some() && message["method"].is_string(),
            Dialect::Pi => {
                message["type"] == "extension_ui_request"
                    && matches!(
                        message["method"].as_str(),
                        Some("select" | "confirm" | "input" | "editor")
                    )
            }
        };
        if is_request {
            let id = message.get("id").ok_or("native request has no id")?;
            let mut requests = self.requests.lock().unwrap();
            if requests.len() >= MAX_REQUESTS {
                return Err(
                    "too many pending native requests; no request was automatically approved"
                        .into(),
                );
            }
            if requests.insert(id.to_string(), message.clone()).is_some() {
                return Err("duplicate pending native request id".into());
            }
        }
        // Codex explicitly resolves approval/input requests on turn cancellation as well.
        if matches!(
            message["method"].as_str(),
            Some("serverRequest/resolved" | "notifications/cancelled")
        ) && let Some(id) = message["params"].get("requestId")
        {
            self.requests.lock().unwrap().remove(&id.to_string());
        }
        let mut audit = message;
        audit.as_object_mut().unwrap().remove("internal_discovery");
        audit.as_object_mut().unwrap().remove("harness_call_id");
        self.events.push(audit);
        Ok(())
    }

    fn fail(&self, error: &str) {
        if self.alive.swap(false, Ordering::AcqRel) {
            self.events
                .push(json!({"type":"ssmcp.disconnected","message":error}));
            for (_, waiting) in self.calls.lock().unwrap().drain() {
                let _ = waiting.reply.send(Err(error.into()));
            }
            self.requests.lock().unwrap().clear();
            self.events.close();
        }
    }

    pub async fn shutdown(&self) {
        self.fail("ssmcp disconnected");
        // Capture and stop owned descendants while the parent still exists.
        // Closing stdin first lets fast native peers exit and orphan setsid jobs.
        if let Some(process) = self.process.lock().await.as_mut() {
            process.shutdown().await;
        }
        self.writer.lock().await.take();
        let stderr_task = self.stderr_reader.lock().unwrap().take();
        if let Some(task) = stderr_task {
            task.abort();
            let _ = task.await;
        }
        let task = self.reader.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(process) = self.process.get_mut().as_mut() {
            process.terminate();
        }
        if let Some(task) = self.stderr_reader.get_mut().unwrap().take() {
            task.abort();
        }
        if let Some(task) = self.reader.get_mut().unwrap().take() {
            task.abort();
        }
    }
}

async fn read_json<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete native JSON line",
                ))
            };
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if line.len() + n > MAX_NATIVE_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native frame exceeds 16 MiB",
            ));
        }
        let complete = bytes[n - 1] == b'\n';
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if complete {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn request_seen(peer: &Peer, id: &str) {
        timeout(Duration::from_secs(5), async {
            loop {
                if peer
                    .requests
                    .lock()
                    .unwrap()
                    .contains_key(&json!(id).to_string())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fixture did not observe native input");
    }

    async fn responsive_peer() -> Arc<Peer> {
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args(["-u", "-c", r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if request.get('method') == 'fixture/hang':
        print(json.dumps({'jsonrpc':'2.0','id':'awaiting','method':'fixture/observed','params':{}}), flush=True)
    elif 'id' in request:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':{'accepted':True}}), flush=True)
"#]);
        Peer::spawn(command, "framing-fixture", Dialect::JsonRpc)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn metadata_audit_keeps_native_arguments_results_and_pending_requests_intact() {
        timeout(Duration::from_secs(10), async {
            const MARKER: &str = "vH8nP4cM9qR2yT6s";
            let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
            command.args(["-u", "-c", r#"
import json,sys
m='vH8nP4cM9qR2yT6s'
print(m,file=sys.stderr,flush=True)
def send(v): print(json.dumps(v),flush=True)
for line in sys.stdin:
 r=json.loads(line)
 if r.get('method')=='tools/call':
  call=r
  send({'method':'notifications/progress','params':{'progressToken':r['id'],'progress':2,'total':3,'message':m}})
  send({'method':'notifications/message','params':{'level':'info','data':{'value':m}}})
  send({'jsonrpc':'2.0','id':'review-1','method':'elicitation/create','params':{'message':m,'requestedSchema':{'type':'object','properties':{'value':{'type':'string','default':m}}}}})
 elif r.get('id')=='review-1':
  send({'jsonrpc':'2.0','id':call['id'],'result':{'content':[{'type':'text','text':m}],'structuredContent':{'arguments':call['params']['arguments'],'approval':r['result']},'isError':False,'_native':[m,None]}})
"#]);
            let peer = Peer::spawn_with_output_and_audit(
                command, "native-metadata-fixture", Dialect::JsonRpc,
                crate::output::OutputStore::new_connection().unwrap(), AuditMode::MetadataOnly,
            ).await.unwrap();
            let arguments = json!({"text":MARKER,"fields":[{"value":MARKER}]});
            let request = peer.request("tools/call", json!({"name":"browser_fill_form","arguments":arguments}));
            tokio::pin!(request);
            tokio::select! {
                _ = request_seen(&peer, "review-1") => {},
                _ = &mut request => panic!("native request completed before caller input"),
            }
            assert_eq!(peer.pending().await["requests"], json!([{
                "jsonrpc":"2.0", "id":"review-1", "method":"elicitation/create",
                "params":{"message":MARKER,"requestedSchema":{"type":"object","properties":{"value":{"type":"string","default":MARKER}}}},
            }]));
            let decision = json!({"action":"decline","content":{"value":MARKER}});
            peer.respond(json!({"id":"review-1","result":decision})).await.unwrap();
            let result = request.await.unwrap();
            assert_eq!(result, json!({
                "content":[{"type":"text","text":MARKER}],
                "structuredContent":{"arguments":arguments,"approval":decision},
                "isError":false,"_native":[MARKER,null],
            }));
            assert!(peer.pending().await["requests"].as_array().unwrap().is_empty());
            // Stderr may arrive on a different task. Wait on the event
            // cursor so this test does not continuously poll the ready page.
            let mut cursor = 0;
            loop {
                let page = peer.events.read(json!({
                    "cursor":cursor,"max_events":64,"wait_ms":1000
                })).await.unwrap();
                assert!(!page.to_string().contains(MARKER));
                if page["events"].as_array().unwrap().iter().any(|entry| entry["event"]["type"] == "stderr") {
                    break;
                }
                assert_eq!(page["closed"], false, "fixture closed before recording stderr");
                cursor = page["next_cursor"].as_u64().unwrap();
            }
            peer.shutdown().await;
            let page = peer.events.read(json!({"max_events":64})).await.unwrap();
            assert!(!page.to_string().contains(MARKER));
            let entries = page["events"].as_array().unwrap();
            assert!(entries.iter().any(|entry| entry["event"]["method"] == "notifications/message"));
            assert!(entries.iter().any(|entry| entry["event"]["method"] == "elicitation/create"));
            let progress = entries.iter().map(|entry| &entry["event"]).find(|event| event["method"] == "notifications/progress").unwrap();
            assert_eq!(progress["params"]["progress"], 2);
            assert_eq!(progress["params"]["total"], 3);
        }).await.expect("native metadata audit fixture stalled");
    }

    #[tokio::test]
    async fn discovery_has_one_reserved_slot_without_raising_the_native_call_limit() {
        timeout(Duration::from_secs(5), async {
            let (client, server) = tokio::io::duplex(64 * 1024);
            let (client_reader, client_writer) = tokio::io::split(client);
            let peer = Peer::from_transport(
                client_reader,
                Box::new(client_writer),
                None,
                "discovery-capacity-fixture",
                Dialect::JsonRpc,
                crate::output::OutputStore::new_connection().unwrap(),
                AuditMode::default(),
            ).unwrap();
            let (native_seen_tx, native_seen_rx) = oneshot::channel();
            let (discovery_seen_tx, discovery_seen_rx) = oneshot::channel();
            let (release_tx, release_rx) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (read, mut write) = tokio::io::split(server);
                let mut read = BufReader::new(read);
                let mut pending = Vec::new();
                for _ in 0..MAX_CALLS {
                    let request = read_json(&mut read).await.unwrap().unwrap();
                    assert_eq!(request["method"], "tools/call");
                    pending.push(request);
                }
                native_seen_tx.send(()).unwrap();
                let discovery = read_json(&mut read).await.unwrap().unwrap();
                assert_eq!(discovery["method"], "tools/list");
                assert!(discovery.get("internal_discovery").is_none());
                discovery_seen_tx.send(()).unwrap();
                release_rx.await.unwrap();
                write.write_all(format!("{}\n", json!({
                    "jsonrpc":"2.0","id":discovery["id"],"result":{"tools":[]}
                })).as_bytes()).await.unwrap();
                for request in pending {
                    write.write_all(format!("{}\n", json!({
                        "jsonrpc":"2.0","id":request["id"],"result":{"index":request["params"]["index"]}
                    })).as_bytes()).await.unwrap();
                }
            });
            let mut native = Vec::new();
            for index in 0..MAX_CALLS {
                let peer = peer.clone();
                native.push(tokio::spawn(async move {
                    peer.request("tools/call", json!({"index":index})).await
                }));
            }
            native_seen_rx.await.unwrap();
            assert!(peer.request("tools/call", json!({"index":8})).await
                .unwrap_err().contains("at most 8"));
            // The public method name alone cannot claim the trusted reserve.
            assert!(peer.request("tools/list", json!({})).await
                .unwrap_err().contains("at most 8"));
            let discovery = tokio::spawn({
                let peer = peer.clone();
                async move { peer.request_discovery("tools/list", json!({})).await }
            });
            discovery_seen_rx.await.unwrap();
            assert_eq!(peer.calls.lock().unwrap().len(), MAX_CALLS + 1);
            assert!(peer.request_discovery("tools/list", json!({})).await
                .unwrap_err().contains("at most 1"));
            assert!(peer.alive.load(Ordering::Acquire));
            release_tx.send(()).unwrap();
            assert_eq!(discovery.await.unwrap().unwrap(), json!({"tools":[]}));
            for (index, task) in native.into_iter().enumerate() {
                assert_eq!(task.await.unwrap().unwrap(), json!({"index":index}));
            }
            server.await.unwrap();
            assert!(peer.calls.lock().unwrap().is_empty());
            peer.shutdown().await;
        }).await.expect("discovery was starved by active native calls or fixture stalled");
    }

    #[tokio::test]
    async fn discovery_provenance_is_bound_to_the_exact_request_and_never_changes_native_json() {
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args(["-u", "-c", r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'internal_discovery':True,
                      'result':{'echo':request,'internal_discovery':True}}), flush=True)
    print(json.dumps({'method':'fixture/notification','internal_discovery':True,'params':{}}), flush=True)
"#]);
        let peer = Peer::spawn(command, "discovery-provenance", Dialect::JsonRpc)
            .await
            .unwrap();
        let internal = peer
            .request_discovery("mcpServerStatus/list", json!({"limit":1}))
            .await
            .unwrap();
        let explicit = peer
            .request("mcpServerStatus/list", json!({"limit":1}))
            .await
            .unwrap();
        for response in [&internal, &explicit] {
            assert_eq!(response["internal_discovery"], true);
            assert!(response["echo"].get("internal_discovery").is_none());
            assert_eq!(response["echo"]["params"], json!({"limit":1}));
        }
        let events = peer.events.read(json!({"max_events":64})).await.unwrap();
        let entries = events["events"].as_array().unwrap();
        for (response, expected) in [(&internal, true), (&explicit, false)] {
            let id = &response["echo"]["id"];
            let matching: Vec<_> = entries
                .iter()
                .map(|entry| &entry["event"])
                .filter(|event| event["request"]["id"] == *id || event["id"] == *id)
                .collect();
            assert_eq!(matching.len(), 2);
            assert!(
                matching
                    .iter()
                    .all(|event| (event["internal_discovery"] == true) == expected)
            );
        }
        assert!(
            entries
                .iter()
                .any(|entry| entry["event"]["method"] == "fixture/notification")
        );
        assert!(
            entries
                .iter()
                .filter(|entry| entry["event"]["method"] == "fixture/notification")
                .all(|entry| entry["event"].get("internal_discovery").is_none())
        );
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn harness_correlation_follows_parallel_requests_without_changing_native_json() {
        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args([
            "-u",
            "-c",
            r#"
import json, sys
pending = [json.loads(sys.stdin.readline()), json.loads(sys.stdin.readline())]
print(json.dumps({'method':'fixture/notification','harness_call_id':999,'params':{}}), flush=True)
for request in reversed(pending):
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'harness_call_id':999,
                      'result':{'echo':request,'harness_call_id':999}}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'harness_call_id':999,
                      'result':{'echo':request}}), flush=True)
"#,
        ]);
        let peer = Peer::spawn(command, "harness-correlation", Dialect::JsonRpc)
            .await
            .unwrap();
        let first = HarnessCall::new(110);
        let second = HarnessCall::new(111);
        let (a, b) = tokio::join!(
            first.scope(peer.request("command/exec", json!({"command":["fixture-a"]}))),
            second.scope(peer.request("command/exec", json!({"command":["fixture-b"]})))
        );
        let a = a.unwrap();
        let b = b.unwrap();
        for (response, expected) in [(&a, "fixture-a"), (&b, "fixture-b")] {
            assert_eq!(response["echo"]["params"]["command"], json!([expected]));
            assert_eq!(response["echo"].as_object().unwrap().len(), 4);
            assert!(response["echo"].get("harness_call_id").is_none());
            assert_eq!(response["harness_call_id"], 999);
        }
        assert!(first.command_started() && first.command_completed());
        assert!(second.command_started() && second.command_completed());
        let lookup = HarnessCall::new(114);
        let lookup_result = lookup
            .scope(peer.request("fixture/ping", json!({})))
            .await
            .unwrap();
        assert!(!lookup.command_started() && !lookup.command_completed());
        // Leaving the scoped future must not associate the next call with it.
        let unscoped = peer.request("fixture/ping", json!({})).await.unwrap();
        let events = peer.events.read(json!({"max_events":64})).await.unwrap();
        let entries = events["events"].as_array().unwrap();
        for (response, expected) in [
            (&a, Some(110)),
            (&b, Some(111)),
            (&lookup_result, Some(114)),
            (&unscoped, None),
        ] {
            let id = &response["echo"]["id"];
            let matching: Vec<_> = entries
                .iter()
                .map(|entry| &entry["event"])
                .filter(|event| event["request"]["id"] == *id || event["id"] == *id)
                .collect();
            assert_eq!(matching.len(), 2);
            assert!(
                matching
                    .iter()
                    .all(|event| event["harness_call_id"].as_u64() == expected)
            );
        }
        let notification = entries
            .iter()
            .map(|entry| &entry["event"])
            .find(|event| event["method"] == "fixture/notification")
            .unwrap();
        assert!(notification.get("harness_call_id").is_none());
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn harness_correlation_keeps_failures_without_a_native_response_visible() {
        let (transport, _server) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(transport);
        let peer = Peer::from_transport(
            reader,
            Box::new(writer),
            None,
            "harness-early-failure",
            Dialect::Pi,
            crate::output::OutputStore::global().unwrap(),
            AuditMode::default(),
        )
        .unwrap();
        let rejected = HarnessCall::new(112);
        let result = rejected
            .scope(peer.request("command/exec", json!({"id":"forbidden"})))
            .await;
        assert!(result.unwrap_err().contains("may not override"));
        assert!(!rejected.command_started() && !rejected.command_completed());

        peer.fail("fixture disconnected before writing");
        let failed_send = HarnessCall::new(113);
        assert!(
            failed_send
                .scope(peer.request("command/exec", json!({})))
                .await
                .is_err()
        );
        assert!(failed_send.command_started());
        assert!(!failed_send.command_completed());
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn cancelled_partial_frame_revokes_transport_and_reaps_native_process() {
        for process_lock_contended in [false, true] {
            let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
            command.args(["-u", "-c", r#"
import json, os, time
assert len(os.read(0, 1)) == 1
print(json.dumps({'jsonrpc':'2.0','id':'partial','method':'fixture/first-byte','params':{}}), flush=True)
time.sleep(60)
"#]);
            let peer = Peer::spawn(command, "partial-frame-fixture", Dialect::JsonRpc)
                .await
                .unwrap();
            let sending = peer.clone();
            let task = tokio::spawn(async move {
                sending
                    .send(
                        json!({"method":"fixture/large","params":{"data":"x".repeat(8*1024*1024)}}),
                    )
                    .await
            });
            request_seen(&peer, "partial").await;
            assert!(
                !task.is_finished(),
                "fixture must stop consuming before the complete frame fits"
            );
            let held = if process_lock_contended {
                Some(peer.process.lock().await)
            } else {
                None
            };
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!peer.alive.load(Ordering::Acquire));
            assert!(peer.send(json!({"method":"fixture/next"})).await.is_err());
            if held.is_some() {
                assert!(
                    peer.writer.lock().await.is_some(),
                    "stdin must stay open until descendant cleanup obtains the process lock"
                );
            }
            drop(held);
            timeout(Duration::from_secs(5), async {
                loop {
                    if peer.writer.lock().await.is_none() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("cancelled write did not finish native cleanup");
            assert!(
                peer.process
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .child
                    .id()
                    .is_none(),
                "native child was not reaped"
            );
            peer.shutdown().await;
        }
    }

    #[tokio::test]
    async fn cancellation_while_queued_for_writer_preserves_the_healthy_peer() {
        let peer = responsive_peer().await;
        let writer = peer.writer.lock().await;
        let sending = peer.clone();
        let task =
            tokio::spawn(async move { sending.send(json!({"method":"fixture/queued"})).await });
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(peer.alive.load(Ordering::Acquire));
        drop(writer);
        assert_eq!(
            peer.request("fixture/ping", json!({})).await.unwrap(),
            json!({"accepted":true})
        );
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn queued_writer_rechecks_liveness_after_acquiring_the_pipe() {
        let peer = responsive_peer().await;
        let writer = peer.writer.lock().await;
        let sending = peer.clone();
        let task =
            tokio::spawn(async move { sending.send(json!({"method":"fixture/queued"})).await });
        tokio::task::yield_now().await;
        peer.fail("fixture disconnected while sender was queued");
        drop(writer);
        assert!(
            timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn cancellation_after_a_complete_frame_keeps_approval_and_control_transport() {
        let peer = responsive_peer().await;
        let waiting = peer.clone();
        let task = tokio::spawn(async move { waiting.request("fixture/hang", json!({})).await });
        request_seen(&peer, "awaiting").await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(peer.alive.load(Ordering::Acquire));
        assert!(peer.calls.lock().unwrap().is_empty());
        assert_eq!(
            peer.request("fixture/ping", json!({})).await.unwrap(),
            json!({"accepted":true})
        );
        assert!(
            peer.requests
                .lock()
                .unwrap()
                .contains_key(&json!("awaiting").to_string()),
            "cancellation must not automatically answer native requests"
        );
        peer.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_retains_native_stdin_until_descendant_cleanup_can_start() {
        // Both commands block on stdin without emitting protocol bytes. Use
        // native shell builtins so release tests need no Python installation.
        #[cfg(windows)]
        let command = {
            let mut command = Command::new(crate::platform::default_shell());
            command.args(["/d", "/q", "/c", "set /p SSMCP_TEST_STDIN="]);
            command
        };
        #[cfg(unix)]
        let command = {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "read ssmcp_test_stdin"])
                .env("ENV", "/dev/null")
                .env("BASH_ENV", "/dev/null");
            command
        };
        let peer = Peer::spawn(command, "cleanup-fixture", Dialect::JsonRpc)
            .await
            .unwrap();
        // Force shutdown to wait before it can inspect native descendants. Closing
        // stdin in this interval lets an EOF-driven parent exit and orphan jobs.
        let process = peer.process.lock().await;
        let stopping = peer.clone();
        let task = tokio::spawn(async move { stopping.shutdown().await });
        timeout(Duration::from_secs(2), async {
            while peer.alive.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let transport_remained_open = peer.writer.lock().await.is_some();
        drop(process);
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            transport_remained_open,
            "native stdin closed before owned descendant cleanup could run"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_peer_stops_separate_session_before_stdin_eof_orphans_it() {
        use std::os::fd::{AsRawFd, OwnedFd};

        struct TestChild(OwnedFd);
        impl TestChild {
            fn running(&self) -> bool {
                let mut poll = libc::pollfd {
                    fd: self.0.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll points to one initialized pollfd holding our pidfd.
                let ready = unsafe { libc::poll(&mut poll, 1, 0) };
                assert!(ready >= 0);
                ready == 0
            }
        }
        impl Drop for TestChild {
            fn drop(&mut self) {
                // SAFETY: this descriptor was bound to the test's owned child.
                // Clean up even if the regression assertion fails.
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.0.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }

        let mut command = Command::new(if cfg!(windows) { "python" } else { "python3" });
        command.args([
            "-u",
            "-c",
            r#"
import json, os, subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'], start_new_session=True)
for line in sys.stdin:
    request = json.loads(line)
    print(json.dumps({'jsonrpc':'2.0', 'id':request['id'], 'result':{'parent':os.getpid(), 'child':child.pid}}), flush=True)
os._exit(0)
"#,
        ]);
        let peer = Peer::spawn(command, "drop-regression", Dialect::JsonRpc)
            .await
            .unwrap();
        let result = peer.request("fixture", json!({})).await.unwrap();
        let parent = result["parent"].as_i64().unwrap() as i32;
        let child = result["child"].as_i64().unwrap() as i32;
        let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        assert_eq!(fields[1].parse::<i32>().unwrap(), parent);
        assert_eq!(fields[3].parse::<i32>().unwrap(), child);
        let child = TestChild(crate::pty::open_pidfd(child).unwrap());
        assert!(child.running());
        timeout(Duration::from_secs(2), async {
            while Arc::strong_count(&peer) > 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        // Deliberately omit shutdown: a cancelled startup future has this path.
        drop(peer);
        timeout(Duration::from_secs(2), async {
            while child.running() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropping a native peer orphaned its separate-session child");
    }

    #[tokio::test]
    async fn json_lines_preserve_fragmentation_and_reject_truncation() {
        let (mut tx, rx) = tokio::io::duplex(64);
        tokio::spawn(async move {
            tx.write_all(b"{\"id\":1,").await.unwrap();
            tokio::task::yield_now().await;
            tx.write_all(b"\"result\":{}}\n").await.unwrap();
        });
        assert_eq!(
            read_json(&mut BufReader::new(rx)).await.unwrap().unwrap()["id"],
            1
        );
        assert!(read_json(&mut &b"{}"[..]).await.is_err());
    }
}
