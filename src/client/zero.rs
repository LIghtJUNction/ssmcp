//! Minimal account recovery link. No Runtime, PTY, tool discovery or child at idle.
mod install;

use super::{account, trust};
use crate::{
    Result,
    wire::{ClientMetadata, Frame, Mode, Operation, VERSION},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinSet,
    time::{Instant, timeout},
};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
    tungstenite::{
        Message, client::IntoClientRequest, http::HeaderValue, protocol::WebSocketConfig,
    },
};

const FRAME_LIMIT: usize = 64 * 1024;
const OUTPUT_LIMIT: usize = 32 * 1024;
const MAX_SESSIONS: usize = 16;
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);
type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

const HELP: &str = "ssmcp 007 — minimal Linux recovery and session control
Usage:
  ssmcp 007 install [--server URL] [--name NAME] [--cwd DIR] [--client PATH] [--dry-run]
  ssmcp 007 upgrade [--dry-run] [--json]
  ssmcp 007 start|stop|status|uninstall [--json]
  ssmcp 007 run
  ssmcp 007 sessions [list|status|start|close|restart|remove] [NAME]

007 keeps one authenticated connection without loading tools or a terminal.
Remote tools: ssmcp.007.status/list/recover/wake/close/remove.
wake is idempotent for an existing name. recovery=true starts a new session from
the preserved recovery executable. Use a new name to start an upgraded client.
The recovery service and its sessions have independent systemd lifetimes.
A valid saved account is required. User services need linger for boot/logout.
";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    pub format: u32,
    pub server: String,
    pub name: String,
    pub cwd: PathBuf,
    pub client: PathBuf,
}

impl Config {
    pub(super) fn validate(&self) -> Result<()> {
        if self.format != 1 || account::normalize_server(&self.server)? != self.server {
            return Err("unsupported 007 configuration or invalid server origin".into());
        }
        for path in [&self.cwd, &self.client] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
                || path
                    .to_str()
                    .is_none_or(|text| text.chars().any(char::is_control))
            {
                return Err(
                    "007 paths must be absolute UTF-8 paths without traversal or controls".into(),
                );
            }
        }
        // The installed client may temporarily be absent during an upgrade.
        // Neither it nor the workspace needs to be opened to keep recovery online.
        self.metadata()?.validate().map_err(Into::into)
    }

    fn metadata(&self) -> Result<ClientMetadata> {
        Ok(ClientMetadata {
            name: self.name.clone(),
            title: Some("007 recovery · wake and manage sessions".into()),
            cwd: self.cwd.to_str().ok_or("invalid workspace path")?.into(),
            hostname: crate::platform::hostname()?,
            allow: Vec::new(),
        })
    }
}

pub(super) fn state_root(create: bool) -> Result<PathBuf> {
    let root = if let Some(path) = std::env::var_os("SSMCP_007_HOME") {
        PathBuf::from(path)
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?)
            .join(".local/share/ssmcp/007")
    };
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        || root
            .to_str()
            .is_none_or(|text| text.chars().any(char::is_control))
    {
        return Err(
            "SSMCP_007_HOME must be an absolute UTF-8 path without traversal or controls".into(),
        );
    }
    if create {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)?;
    }
    match fs::symlink_metadata(&root) {
        Ok(metadata) => {
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(
                    "007 state must be an owned private directory (0700), not a symlink".into(),
                );
            }
            Ok(root.canonicalize()?)
        }
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => Ok(root),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn load_config(root: &Path) -> Result<Config> {
    let config: Config = crate::session::read_json(&root.join("config.json"))?;
    config.validate()?;
    Ok(config)
}

pub(super) async fn dispatch(args: Vec<OsString>) -> Result<()> {
    let operation = args
        .first()
        .and_then(|arg| arg.to_str())
        .unwrap_or("--help");
    match operation {
        "--help" | "-h" | "help" => {
            println!("{HELP}");
            Ok(())
        }
        "run" if args.len() == 1 => run().await,
        "__sessions" => session_helper(&args[1..]).await,
        "sessions" => {
            let root = state_root(false)?;
            let config = load_config(&root)?;
            let words = args[1..]
                .iter()
                .map(|arg| arg.to_str().ok_or("arguments must be UTF-8"))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let words = words
                .into_iter()
                .filter(|word| *word != "--json")
                .collect::<Vec<_>>();
            let (op, name) =
                match words.as_slice() {
                    [] | ["list"] => ("list", None),
                    [
                        op @ ("status" | "start" | "close" | "restart" | "remove"),
                        name,
                    ] => {
                        crate::session::validate_name(name)?;
                        (*op, Some(*name))
                    }
                    _ => return Err(
                        "usage: ssmcp 007 sessions [list|status|start|close|restart|remove] [NAME]"
                            .into(),
                    ),
                };
            let value = Control {
                config,
                root,
                started: Instant::now(),
            }
            .sessions(op, name)
            .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        _ => install::manage(args).await,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wake {
    name: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    recovery: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recover {
    name: String,
    expected_device_id: String,
    expected_definition_id: String,
}

impl Recover {
    fn validate(&self) -> Result<()> {
        crate::session::validate_name(&self.name)?;
        let device = self
            .expected_device_id
            .strip_prefix("dev_")
            .ok_or("expected_device_id must be an exact dev_ ID")?;
        crate::wire::device_address::decode::<32>(device)?;
        crate::wire::device_address::decode::<32>(&self.expected_definition_id)?;
        Ok(())
    }
}

struct Control {
    config: Config,
    root: PathBuf,
    started: Instant,
}

impl Control {
    fn status(&self) -> Value {
        json!({
            "mode":"007", "version":env!("CARGO_PKG_VERSION"),
            "wire_version":VERSION,
            "pid":std::process::id(), "name":self.config.name, "server":self.config.server,
            "workspace":self.config.cwd, "uptime_secs":self.started.elapsed().as_secs(),
            "session_directory":self.root.join("sessions"),
            "installed_client":self.config.client, "recovery_client":self.root.join("ssmcp"),
            "max_sessions":MAX_SESSIONS, "max_inflight":1,
            "max_frame_bytes":FRAME_LIMIT, "idle_timeout_secs":IDLE_TIMEOUT.as_secs()
        })
    }

    async fn sessions(&self, operation: &str, name: Option<&str>) -> Result<Value> {
        let mut args = vec!["007".into(), "__sessions".into(), operation.into()];
        if let Some(name) = name {
            crate::session::validate_name(name)?;
            args.push(name.into());
        }
        // Management always uses the preserved client, even if an upgrade broke
        // the normal entry point. Only the private saved-session root is exposed.
        self.command(&self.root.join("ssmcp"), &args, None).await
    }

    async fn command(
        &self,
        executable: &Path,
        args: &[OsString],
        cwd: Option<&Path>,
    ) -> Result<Value> {
        let mut command = Command::new(executable);
        command
            .args(args)
            .env("SSMCP_SESSION_HOME", self.root.join("sessions"))
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().ok_or("missing session output")?;
        let stderr = child.stderr.take().ok_or("missing session error stream")?;
        let result = timeout(Duration::from_secs(40), async {
            tokio::try_join!(child.wait(), read_bounded(stdout), read_bounded(stderr))
        })
        .await;
        let (status, out, err) = match result {
            Ok(Ok(value)) => value,
            failure => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(match failure {
                    Err(_) => {
                        "session operation timed out; inspect 007 sessions before retrying".into()
                    }
                    Ok(Err(error)) => error.into(),
                    Ok(Ok(_)) => unreachable!(),
                });
            }
        };
        if !status.success() {
            let message = String::from_utf8_lossy(&err);
            let safe = crate::audit::event(json!({"message": message.trim()}))["message"]
                .as_str()
                .unwrap_or("session operation failed")
                .chars()
                .take(1024)
                .collect::<String>();
            return Err(format!(
                "session command failed ({}): {safe}",
                status.code().unwrap_or(-1)
            )
            .into());
        }
        Ok(serde_json::from_slice(&out)?)
    }

    async fn wake(&self, input: Wake) -> Result<Value> {
        crate::session::validate_name(&input.name)?;
        let cwd = crate::session::control::resolve_cwd(&self.config.cwd, input.cwd.as_deref())?;
        let entries = self.sessions("list", None).await?;
        let entries = entries
            .as_array()
            .ok_or("invalid saved-session inventory")?;
        if entries.iter().any(|value| value["name"] == input.name) {
            let existing = self.sessions("status", Some(&input.name)).await?;
            if existing["cwd"].as_str() != cwd.to_str() {
                return Err(
                    "session name already has a different workspace; choose a new name".into(),
                );
            }
            let mut value = if existing["manager_active"] == true {
                existing.clone()
            } else {
                self.sessions("start", Some(&input.name)).await?
            };
            value["reused"] = true.into();
            value["executable_preserved"] = true.into();
            return Ok(value);
        }
        if entries.len() >= MAX_SESSIONS {
            return Err("007 session limit reached; remove a stopped session first".into());
        }
        let executable = if input.recovery {
            self.root.join("ssmcp")
        } else {
            self.config.client.clone()
        };
        self.command(
            &executable,
            &[
                "--daemon".into(),
                "--no-setup".into(),
                "--name".into(),
                input.name.into(),
                "--server".into(),
                self.config.server.clone().into(),
                "--watchdog".into(),
                "--log-format".into(),
                "jsonl".into(),
            ],
            Some(&cwd),
        )
        .await
    }

    async fn recover(&self, input: Recover) -> Result<Value> {
        input.validate()?;
        let actual = account::device_identity::verified_device_id(&self.config.server).await?;
        if actual != input.expected_device_id {
            return Err("recovery device/account mismatch; no session was started".into());
        }
        // The helper checks the definition and stop marker again under the same
        // management lock used by close/remove/start. No list-then-start race.
        let mut value = self
            .command(
                &self.root.join("ssmcp"),
                &[
                    "007".into(),
                    "__sessions".into(),
                    "recover".into(),
                    input.name.into(),
                    input.expected_definition_id.into(),
                ],
                None,
            )
            .await?;
        value["reused"] = true.into();
        value["executable_preserved"] = true.into();
        value["task_replay"] = json!("never");
        Ok(value)
    }

    async fn perform(&self, operation: Operation) -> std::result::Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::ListTools { cursor: None } => Ok(json!({"tools":catalog()})),
            Operation::Status {} => Ok(self.status()),
            Operation::CallTool { name, arguments } => {
                let value = match name.as_str() {
                    "ssmcp.007.status" => {
                        serde_json::from_value::<Empty>(arguments).map_err(|e| e.to_string())?;
                        Ok(self.status())
                    }
                    "ssmcp.007.list" => {
                        serde_json::from_value::<Empty>(arguments).map_err(|e| e.to_string())?;
                        self.sessions("list", None)
                            .await
                            .map(|sessions| json!({"sessions":sessions}))
                    }
                    "ssmcp.007.wake" => {
                        let input = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                        self.wake(input).await
                    }
                    "ssmcp.007.recover" => {
                        let input = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                        self.recover(input).await
                    }
                    "ssmcp.007.close" | "ssmcp.007.remove" => {
                        let input: Named =
                            serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                        self.sessions(name.rsplit('.').next().unwrap(), Some(&input.name))
                            .await
                    }
                    _ => return Err(
                        "007 only supports its recovery/session tools; wake a full session first"
                            .into(),
                    ),
                }
                .map_err(|e| e.to_string())?;
                Ok(crate::backend::result(value))
            }
            _ => Err("007 has no terminal or execution backend; wake a full session first".into()),
        }
    }
}

// The short-lived helper reuses saved-session lifecycle code while keeping
// verbose manifests and diagnostics out of the resident recovery process.
async fn session_helper(args: &[OsString]) -> Result<()> {
    let root = state_root(false)?;
    if std::env::var_os("SSMCP_SESSION_HOME").as_deref() != Some(root.join("sessions").as_os_str())
    {
        return Err("007 session helper requires its private session directory".into());
    }
    let args = args
        .iter()
        .map(|arg| arg.to_str().ok_or("session arguments must be UTF-8"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let value = match args.as_slice() {
        ["recover", name, expected] => {
            compact_status(crate::session::recover_saved(name, expected).await?, true)
        }
        ["list"] => Value::Array(
            crate::session::list()
                .await?
                .into_iter()
                .map(|value| compact_status(value, false))
                .collect(),
        ),
        [operation, name] => {
            crate::session::validate_name(name)?;
            let value = match *operation {
                "status" => crate::session::status(name).await?,
                "start" => crate::session::start_saved(name).await?,
                "close" => crate::session::stop(name).await?,
                "restart" => crate::session::restart(name).await?,
                "remove" => crate::session::remove(name).await?,
                _ => return Err("invalid 007 session operation".into()),
            };
            compact_status(value, true)
        }
        _ => return Err("invalid 007 session helper arguments".into()),
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

fn compact_status(value: Value, include_cwd: bool) -> Value {
    let mut out = serde_json::Map::new();
    for field in [
        "name",
        "manager",
        "supervised",
        "manager_id",
        "manager_state",
        "manager_active",
        "manager_pid",
        "generation",
        "definition_id",
        "recovery_state",
        "connection_state",
        "phase",
        "version",
        "stop_requested",
        "removed",
    ] {
        if let Some(value) = value.get(field) {
            out.insert(field.into(), value.clone());
        }
    }
    if include_cwd && let Some(cwd) = value.get("cwd") {
        out.insert("cwd".into(), cwd.clone());
    }
    if let Some(id) = value["connection"]["client_id"].as_str() {
        out.insert("connection".into(), json!({"client_id":id}));
    }
    Value::Object(out)
}

async fn read_bounded(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut reader)
        .take(OUTPUT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(std::io::Error::other(
            "session command output exceeded its limit",
        ));
    }
    Ok(bytes)
}

fn catalog() -> Vec<Value> {
    let name = json!({"type":"string","minLength":1,"maxLength":128});
    let named = json!({"type":"object","additionalProperties":false,"properties":{"name":name},"required":["name"]});
    [
        ("status", "Inspect this minimal recovery connection.", json!({"type":"object","additionalProperties":false,"properties":{}}), true),
        ("list", "List only this 007 service's saved sessions, with actual connection state.", json!({"type":"object","additionalProperties":false,"properties":{}}), true),
        ("recover", "Recover only a previously verified saved definition on the exact device/account. Rechecks current authorization and refuses replaced definitions or explicit stop requests. An active manager is observed, never restarted. Wait for actual connected state, reconcile original task IDs, and never replay unknown outcomes. A timeout requires inspection before any further mutation.",
            json!({"type":"object","additionalProperties":false,"properties":{"name":name,"expected_device_id":{"type":"string","pattern":"^dev_[0-9a-f]{64}$"},"expected_definition_id":{"type":"string","pattern":"^[0-9a-f]{64}$"}},"required":["name","expected_device_id","expected_definition_id"]}), false),
        ("wake", "Start a full session; repeated names reuse the existing session. cwd stays inside the configured workspace. recovery=true requires a new name and uses the preserved executable. Wait for connection_state=connected before using it.",
            json!({"type":"object","additionalProperties":false,"properties":{"name":name,"cwd":{"type":"string","maxLength":4096},"recovery":{"type":"boolean","default":false}},"required":["name"]}), false),
        ("close", "Stop one saved 007 session and preserve its definition. Leaves the recovery link and other sessions running.", named.clone(), false),
        ("remove", "Stop and remove one saved 007 session. Leaves the recovery link and other sessions running.", named, false),
    ].into_iter().map(|(op, description, schema, read_only)| json!({
        "name":format!("ssmcp.007.{op}"),"description":description,"inputSchema":schema,
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"idempotentHint":op != "remove","openWorldHint":false}
    })).collect()
}

async fn send(socket: &mut Socket, frame: Frame) -> Result<()> {
    let mut bytes = serde_json::to_vec(&frame)?;
    if bytes.len() >= FRAME_LIMIT {
        if let Frame::Reply { id, .. } = frame {
            bytes = serde_json::to_vec(&Frame::Reply {
                id,
                result: Err("007 response exceeds its size limit".into()),
            })?;
        } else {
            return Err("007 frame exceeds its size limit".into());
        }
    }
    bytes.push(b'\n');
    timeout(
        Duration::from_secs(10),
        socket.send(Message::Binary(bytes.into())),
    )
    .await
    .map_err(|_| "007 relay send timed out")??;
    Ok(())
}

async fn connect(control: Arc<Control>) -> Result<()> {
    let credential = account::credentials(Some(&control.config.server))
        .await?
        .ok_or_else(|| super::relay::failure(super::relay::AUTH_EXPIRED.into()))?;
    let mut url = reqwest::Url::parse(&credential.server)?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme).map_err(|_| "invalid relay scheme")?;
    url.set_path("/api/client/connect");
    let mut request = url.as_str().into_client_request()?;
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", credential.token))
        .map_err(|_| "invalid saved credential")?;
    authorization.set_sensitive(true);
    request.headers_mut().insert("authorization", authorization);
    let session_key = crate::session::definition_id(&control.root)?;
    let proof =
        account::device_identity::proof_header(&credential.server, &credential.token, &session_key)
            .await?;
    if let Some(proof) = &proof {
        request.headers_mut().insert(
            crate::wire::device_address::PROOF_HEADER,
            HeaderValue::from_str(proof).map_err(|_| "invalid recovery device proof")?,
        );
    }
    let configuration = WebSocketConfig::default()
        .read_buffer_size(4096)
        .write_buffer_size(1024)
        .max_write_buffer_size(FRAME_LIMIT * 2)
        .max_message_size(Some(FRAME_LIMIT))
        .max_frame_size(Some(FRAME_LIMIT));
    let (mut socket, _) = timeout(
        Duration::from_secs(30),
        connect_async_tls_with_config(
            request,
            Some(configuration),
            true,
            trust::tls_config(&credential.server)?.map(Connector::Rustls),
        ),
    )
    .await
    .map_err(|_| "007 connection timed out")?
    .map_err(|error| -> crate::Error {
        if matches!(&error, tokio_tungstenite::tungstenite::Error::Http(response)
            if matches!(response.status().as_u16(),401|403))
        {
            super::relay::failure(super::relay::AUTH_EXPIRED.into())
        } else {
            "007 encrypted connection failed; check network and server trust".into()
        }
    })?;
    drop(credential);
    send(
        &mut socket,
        Frame::Hello {
            // Recovery shares the current transport while keeping its small tool set.
            version: VERSION,
            mode: Mode::Shell,
            metadata: control.config.metadata()?,
        },
    )
    .await?;
    let mut ready = false;
    let mut requests: JoinSet<(u64, std::result::Result<Value, String>)> = JoinSet::new();
    let mut deadline = Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Err("007 relay stopped responding".into()),
            complete = requests.join_next(), if !requests.is_empty() => {
                let (id, result) = complete.ok_or("007 request worker disappeared")??;
                send(&mut socket, Frame::Reply { id, result }).await?;
            }
            message = socket.next() => {
                let message = message.ok_or("007 relay disconnected")??;
                let frame: Option<Frame> = match message {
                    Message::Ping(bytes) => {
                        timeout(Duration::from_secs(10), socket.send(Message::Pong(bytes)))
                            .await.map_err(|_| "007 pong timed out")??;
                        None
                    }
                    Message::Pong(_) => None,
                    Message::Text(text) => Some(serde_json::from_slice(text.as_bytes())?),
                    Message::Binary(bytes) => Some(serde_json::from_slice(&bytes)?),
                    Message::Close(reason) => return Err(if reason.is_some_and(|reason|u16::from(reason.code)==4001) {
                        super::relay::failure(super::relay::AUTH_EXPIRED.into())
                    } else { "007 relay closed".into() }),
                    _ => return Err("unexpected 007 transport frame".into()),
                };
                // Pings during registration cannot postpone its deadline forever.
                if ready { deadline = Instant::now() + IDLE_TIMEOUT; }
                match frame {
                    Some(Frame::Ready { client_id, capabilities, .. }) if !ready => {
                        if proof.is_some() && !capabilities.iter().any(|c|c==crate::wire::device_address::CAPABILITY) {
                            return Err("server did not acknowledge recovery device identity".into());
                        }
                        if client_id.len() != 64 || !client_id.bytes().all(|c| c.is_ascii_hexdigit()) {
                            return Err("invalid 007 registration".into());
                        }
                        ready = true;
                        deadline = Instant::now() + IDLE_TIMEOUT;
                        record(&control.root, json!({"state":"connected","client_id":client_id,"pid":std::process::id()}));
                        eprintln!("ssmcp 007: recovery link connected");
                    }
                    Some(Frame::Call { id, operation, .. }) if ready => {
                        if requests.is_empty() {
                            let control = control.clone();
                            requests.spawn(async move { (id, control.perform(operation).await) });
                        } else {
                            send(&mut socket, Frame::Reply { id, result: Err("007 is busy; inspect the session before retrying".into()) }).await?;
                        }
                    }
                    Some(Frame::CodeModeFinished { completion }) if ready => {
                        // Source-audit observation must not start a runtime or interrupt recovery.
                        completion.validate()?;
                    }
                    Some(Frame::Reject { message }) => return Err(if message==super::relay::AUTH_EXPIRED {
                        super::relay::failure(message)
                    } else { format!("007 registration rejected: {}",safe_error(&message)).into() }),
                    Some(_) => return Err("unexpected 007 protocol frame".into()),
                    None => {}
                }
            }
        }
    }
}

// Observations must never become a dependency of the emergency transport.
fn record(root: &Path, mut value: Value) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    value["version"] = json!(env!("CARGO_PKG_VERSION"));
    value["wire_version"] = json!(VERSION);
    value["observed_at"] = json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    );
    if crate::session::write_json(&root.join("state.json"), &value).is_err()
        && !WARNED.swap(true, Ordering::Relaxed)
    {
        eprintln!("ssmcp 007: state cache unavailable; recovery connection continues");
    }
}

fn safe_error(message: &str) -> String {
    let redacted = crate::audit::event(json!({"message":message}));
    let mut text = redacted["message"]
        .as_str()
        .unwrap_or("007 connection failed")
        .chars()
        .filter(|character| !character.is_control())
        .take(1024)
        .collect::<String>();
    let mut end = text.len().min(1024);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

fn backoff(attempt: u32, entropy: u8) -> Duration {
    super::recovery::delay(1, attempt, entropy)
}

async fn run() -> Result<()> {
    let root = state_root(false)?;
    let config = load_config(&root)?;
    let _lock = crate::session::lock(&root.join("run.lock"), true)?;
    let control = Arc::new(Control {
        config,
        root,
        started: Instant::now(),
    });
    let mut interrupted = crate::platform::interrupt_signal()?;
    let mut terminated = crate::platform::terminate_signal()?;
    let mut attempt = 0u32;
    let mut previous_error = String::new();
    let mut authorization_block: Option<Option<(String, String)>> = None;
    loop {
        let credential_stamp = account::recovery_credential_stamp(&control.config.server)
            .ok()
            .flatten();
        if let Some(previous) = &authorization_block {
            if !super::recovery::renewed_same_account(previous.as_ref(), credential_stamp.as_ref())
            {
                record(
                    &control.root,
                    json!({"state":"authorization_required","pid":std::process::id(),
                    "resume_condition":"original_account_explicitly_reauthorized","task_replay":"never"}),
                );
                tokio::select! {
                    _ = interrupted.recv() => break,
                    _ = terminated.recv() => break,
                    _ = tokio::time::sleep(Duration::from_secs(15)) => continue,
                }
            }
            authorization_block = None;
            attempt = 0;
        }
        record(
            &control.root,
            json!({"state":"connecting","pid":std::process::id(),"last_error":previous_error}),
        );
        let started = Instant::now();
        let result = tokio::select! {
            _ = interrupted.recv() => break,
            _ = terminated.recv() => break,
            result = connect(control.clone()) => result,
        };
        if started.elapsed() >= IDLE_TIMEOUT * 2 {
            attempt = 0;
        }
        if result.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<super::relay::PermanentFailure>()
                .is_some_and(|failure| failure.0 == super::relay::AUTH_EXPIRED)
        }) {
            authorization_block = Some(credential_stamp);
            eprintln!(
                "ssmcp 007: authorization required; recovery paused until the original account is explicitly reauthorized"
            );
            continue;
        }
        let message = result
            .err()
            .map(|error| safe_error(&error.to_string()))
            .unwrap_or_default();
        if previous_error != message {
            eprintln!("ssmcp 007: {message}; reconnecting automatically");
            previous_error = message.clone();
        }
        let mut entropy = [0];
        crate::platform::random_bytes(&mut entropy)?;
        let delay = backoff(attempt, entropy[0]);
        attempt = attempt.saturating_add(1);
        record(
            &control.root,
            json!({"state":"reconnecting","retry_after_ms":delay.as_millis(),"pid":std::process::id(),"last_error":message}),
        );
        tokio::select! {
            _ = interrupted.recv() => break,
            _ = terminated.recv() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    record(
        &control.root,
        json!({"state":"stopped","pid":std::process::id()}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_requires_both_previously_observed_exact_identities() {
        let valid = json!({"name":"work","expected_device_id":format!("dev_{}","a".repeat(64)),"expected_definition_id":"b".repeat(64)});
        serde_json::from_value::<Recover>(valid.clone())
            .unwrap()
            .validate()
            .unwrap();
        for missing in ["name", "expected_device_id", "expected_definition_id"] {
            let mut input = valid.clone();
            input.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<Recover>(input).is_err());
        }
        for key in ["command", "cwd", "force", "account", "client_id", "replay"] {
            let mut input = valid.clone();
            input[key] = json!("unexpected");
            assert!(serde_json::from_value::<Recover>(input).is_err());
        }
        for (field, value) in [
            ("name", "../work"),
            ("expected_device_id", "work@host.local"),
            ("expected_definition_id", "unknown"),
        ] {
            let mut input = valid.clone();
            input[field] = json!(value);
            assert!(
                serde_json::from_value::<Recover>(input)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }

    #[test]
    fn recovery_errors_are_redacted_bounded_and_cannot_control_a_terminal() {
        let error = format!(
            "Authorization: Bearer raw-access-value\n{}\u{1b}[2J",
            "界".repeat(2000)
        );
        let safe = safe_error(&error);
        assert!(!safe.contains("raw-access-value"));
        assert!(safe.len() <= 1024);
        assert!(!safe.chars().any(char::is_control));
    }

    #[test]
    fn retry_delay_is_bounded_and_grows() {
        assert_eq!(backoff(0, 0), Duration::from_secs(1));
        assert!(backoff(3, 255) > backoff(2, 255));
        assert_eq!(backoff(u32::MAX, 255), Duration::from_secs(60));
    }

    #[test]
    fn recovery_has_no_generic_execution_or_identity_arguments() {
        let tools = catalog();
        assert_eq!(tools.len(), 6);
        for value in tools {
            assert!(value["name"].as_str().unwrap().starts_with("ssmcp.007."));
            assert_eq!(value["inputSchema"]["additionalProperties"], false);
        }
        for key in [
            "command",
            "args",
            "server",
            "client",
            "allow",
            "session_control",
        ] {
            let mut value = json!({"name":"test"});
            value[key] = json!("untrusted");
            assert!(serde_json::from_value::<Wake>(value).is_err());
        }
    }

    #[test]
    fn inventory_does_not_expand_with_paths_or_diagnostics() {
        let entry = json!({
            "name":"n".repeat(128), "cwd":"x".repeat(4096),
            "state_directory":"s".repeat(4096), "logs_path":"l".repeat(4096),
            "last_error":"e".repeat(2048), "manager_active":true,
            "connection":{"client_id":"a".repeat(64),"cwd":"x".repeat(4096)}
        });
        let inventory = vec![compact_status(entry.clone(), false); MAX_SESSIONS];
        assert!(serde_json::to_vec(&inventory).unwrap().len() < OUTPUT_LIMIT / 2);
        assert_eq!(
            compact_status(entry, true)["cwd"].as_str().unwrap().len(),
            4096
        );
    }

    #[tokio::test]
    async fn command_output_is_bounded_before_allocation() {
        assert!(
            read_bounded(&vec![b'x'; OUTPUT_LIMIT + 1][..])
                .await
                .is_err()
        );
        assert_eq!(read_bounded(&b"ok"[..]).await.unwrap(), b"ok");
    }
}
