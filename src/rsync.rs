//! Bounded, directory-scoped streaming endpoints backed by the system rsync.
//!
//! The receiver is a real rsync client. Its remote-shell helper connects to an
//! owned Unix socket, so the protocol can be relayed over the existing MCP wire.
use crate::backend::process::Managed;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, net::UnixStream as StdUnixStream},
    },
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::Command,
    sync::Mutex,
    task::JoinHandle,
    time::timeout,
};

const MAX_HANDLES: usize = 8;
const MAX_BYTES: usize = 32 * 1024;
const MAX_LOG: usize = 64 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const SOCKET_ENV: &str = "SSMCP_RSYNC_SOCKET";
const TOKEN_ENV: &str = "SSMCP_RSYNC_TOKEN";
const HELPER: &str = "__rsync-transport";
type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Default)]
struct Log {
    bytes: Vec<u8>,
    truncated: bool,
}

impl Log {
    fn append(&mut self, bytes: &[u8]) {
        let length = bytes.len().min(MAX_LOG.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..length]);
        self.truncated |= length < bytes.len();
    }
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Self, String> {
        let path =
            std::env::temp_dir().join(format!("ssmcp-rsync-{}-{}", std::process::id(), secret()?));
        std::fs::create_dir(&path)
            .map_err(|error| format!("create rsync transport directory: {error}"))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("io"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

struct Transfer {
    role: Role,
    process: Mutex<Managed>,
    input: Mutex<Option<Writer>>,
    output: Mutex<Reader>,
    eof: AtomicBool,
    closed: AtomicBool,
    log: Arc<Mutex<Log>>,
    drains: Vec<JoinHandle<()>>,
    // Pin the chosen directory inode for the lifetime of the transfer.
    _directory: File,
    _scratch: Option<Scratch>,
}

impl Transfer {
    async fn stop(&self) {
        self.closed.store(true, Ordering::Release);
        self.process.lock().await.shutdown().await;
        self.input.lock().await.take();
        for task in &self.drains {
            task.abort();
        }
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        self.process.get_mut().terminate();
        for task in &self.drains {
            task.abort();
        }
    }
}

/// One client connection's rsync endpoints. Dropping it stops all children.
pub struct Rsync {
    root: File,
    helper: PathBuf,
    transfers: Mutex<HashMap<String, Arc<Transfer>>>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Role {
    Sender,
    Receiver,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    role: Role,
    path: String,
    source: Option<String>,
    server_args: Option<Vec<String>>,
    dry_run: Option<bool>,
    delete: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handle {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    id: String,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
    #[serde(default)]
    wait_ms: u64,
}
fn default_bytes() -> usize {
    MAX_BYTES
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    id: String,
    #[serde(default)]
    data_base64: String,
    #[serde(default)]
    close_stdin: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Handshake {
    token: String,
    arguments: Vec<String>,
}

impl Rsync {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        Self::with_helper(
            root,
            std::env::current_exe().map_err(|error| error.to_string())?,
        )
    }

    /// Supply the ssmcp executable explicitly when embedding or testing this module.
    pub fn with_helper(root: PathBuf, helper: PathBuf) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|error| format!("workspace root: {error}"))?;
        let root = open_directory(None, root.as_os_str().as_encoded_bytes())?;
        let helper = helper
            .canonicalize()
            .map_err(|error| format!("ssmcp helper: {error}"))?;
        Ok(Self {
            root,
            helper,
            transfers: Mutex::new(HashMap::new()),
        })
    }

    pub fn tools() -> Vec<Value> {
        let handle = json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false});
        vec![
            json!({"name":"ssmcp.rsync.start","description":"Open a bounded system-rsync protocol endpoint. Sync directory contents; both selected directories must exist under the shared root. Receiver defaults to dry_run=true and delete=false. Use returned server_args for the matching sender. Links, devices, ownership and extended attributes are not copied.","inputSchema":{"type":"object","properties":{"role":{"enum":["sender","receiver"]},"path":{"type":"string"},"source":{"type":"string"},"server_args":{"type":"array","items":{"type":"string"},"maxItems":5},"dry_run":{"type":"boolean","default":true},"delete":{"type":"boolean","default":false}},"required":["role","path"],"additionalProperties":false}}),
            json!({"name":"ssmcp.rsync.read","description":"Read at most 32768 protocol bytes from an rsync endpoint. Empty data with timed_out=true is not EOF.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_BYTES,"default":MAX_BYTES},"wait_ms":{"type":"integer","minimum":0,"maximum":1000,"default":0}},"required":["id"],"additionalProperties":false}}),
            json!({"name":"ssmcp.rsync.write","description":"Write bounded base64 protocol bytes; close_stdin half-closes after writing. Do not retry a failed write whose result is unknown.","inputSchema":{"type":"object","properties":{"id":{"type":"string"},"data_base64":{"type":"string","maxLength":43692},"close_stdin":{"type":"boolean","default":false}},"required":["id"],"additionalProperties":false}}),
            json!({"name":"ssmcp.rsync.status","description":"Inspect process exit status and bounded diagnostic/itemized logs. Process exit does not imply all pending protocol bytes were read.","inputSchema":handle.clone(),"annotations":{"readOnlyHint":true}}),
            json!({"name":"ssmcp.rsync.close","description":"Stop and release an owned rsync endpoint, its helper and pipes. Safe to repeat for an already closed handle.","inputSchema":handle}),
        ]
    }

    pub async fn perform(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "ssmcp.rsync.start" => self.start(parse(arguments)?).await,
            "ssmcp.rsync.read" => {
                let args: ReadArgs = parse(arguments)?;
                if !(1..=MAX_BYTES).contains(&args.max_bytes) || args.wait_ms > 1000 {
                    return Err("max_bytes must be 1..=32768 and wait_ms must be 0..=1000".into());
                }
                let transfer = self.get(&args.id).await?;
                if transfer.eof.load(Ordering::Acquire) {
                    return Ok(json!({"data_base64":"","eof":true,"timed_out":false}));
                }
                let mut output = transfer.output.lock().await;
                let mut bytes = vec![0u8; args.max_bytes];
                let result = timeout(
                    Duration::from_millis(args.wait_ms.max(1)),
                    output.read(&mut bytes),
                )
                .await;
                match result {
                    Err(_) => Ok(json!({"data_base64":"","eof":false,"timed_out":true})),
                    Ok(Err(error)) => Err(format!("rsync protocol read: {error}")),
                    Ok(Ok(length)) => {
                        transfer.eof.store(length == 0, Ordering::Release);
                        Ok(
                            json!({"data_base64":STANDARD.encode(&bytes[..length]),"eof":length == 0,"timed_out":false}),
                        )
                    }
                }
            }
            "ssmcp.rsync.write" => {
                let args: WriteArgs = parse(arguments)?;
                if args.data_base64.len() > MAX_BYTES.div_ceil(3) * 4 {
                    return Err("rsync write exceeds 32768 bytes".into());
                }
                let bytes = STANDARD
                    .decode(&args.data_base64)
                    .map_err(|_| "invalid base64 data")?;
                if bytes.len() > MAX_BYTES {
                    return Err("rsync write exceeds 32768 bytes".into());
                }
                let transfer = self.get(&args.id).await?;
                let mut input = transfer.input.lock().await;
                let Some(writer) = input.as_mut() else {
                    return if bytes.is_empty() && args.close_stdin {
                        Ok(json!({"written":0,"stdin_closed":true}))
                    } else {
                        Err("rsync stdin is closed".into())
                    };
                };
                let result = timeout(WRITE_TIMEOUT, async {
                    writer.write_all(&bytes).await?;
                    writer.flush().await?;
                    if args.close_stdin {
                        writer.shutdown().await?;
                    }
                    Ok::<_, io::Error>(())
                })
                .await;
                if args.close_stdin || !matches!(result, Ok(Ok(()))) {
                    input.take();
                }
                drop(input);
                if bytes.is_empty()
                    && args.close_stdin
                    && matches!(&result, Ok(Err(error)) if matches!(error.kind(), io::ErrorKind::BrokenPipe | io::ErrorKind::NotConnected))
                {
                    return Ok(json!({"written":0,"stdin_closed":true}));
                }
                match result {
                    Ok(Ok(())) => {
                        Ok(json!({"written":bytes.len(),"stdin_closed":args.close_stdin}))
                    }
                    other => {
                        transfer.stop().await;
                        Err(match other {
                            Err(_) => "rsync write timed out; endpoint closed, partial write outcome unknown".into(),
                            Ok(Err(error)) => format!("rsync write failed; endpoint closed, partial write outcome unknown: {error}"),
                            _ => unreachable!(),
                        })
                    }
                }
            }
            "ssmcp.rsync.status" => {
                let args: Handle = parse(arguments)?;
                let transfer = self.get(&args.id).await?;
                let status = transfer
                    .process
                    .lock()
                    .await
                    .child
                    .try_wait()
                    .map_err(|error| error.to_string())?;
                let log = transfer.log.lock().await;
                Ok(
                    json!({"id":args.id,"role":transfer.role.name(),"running":status.is_none(),
                    "exit_code":status.map(|status| status.code().unwrap_or(-1)),
                    "eof":transfer.eof.load(Ordering::Acquire),"closed":transfer.closed.load(Ordering::Acquire),
                    "log":String::from_utf8_lossy(&log.bytes),"stderr":String::from_utf8_lossy(&log.bytes),"truncated":log.truncated}),
                )
            }
            "ssmcp.rsync.close" => {
                let args: Handle = parse(arguments)?;
                let transfer = self.transfers.lock().await.remove(&args.id);
                let existed = transfer.is_some();
                if let Some(transfer) = transfer {
                    transfer.stop().await;
                }
                Ok(json!({"closed":true,"existed":existed}))
            }
            _ => Err("unknown rsync operation".into()),
        }
    }

    async fn get(&self, id: &str) -> Result<Arc<Transfer>, String> {
        if id.len() != 32 {
            return Err("unknown rsync handle".into());
        }
        self.transfers
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown rsync handle".into())
    }

    async fn start(&self, args: Start) -> Result<Value, String> {
        // Hold the reservation through startup: concurrent opens cannot overrun the cap.
        let mut transfers = self.transfers.lock().await;
        if transfers.len() >= MAX_HANDLES {
            return Err("at most 8 rsync handles; close completed endpoints".into());
        }
        let path = relative(&args.path)?;
        let directory = self.directory(&path)?;
        let id = secret()?;
        let log = Arc::new(Mutex::new(Log::default()));
        let mut drains = Vec::new();
        let mut command = Command::new("rsync");
        command.current_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in [
            "RSYNC_RSH",
            "RSYNC_PROXY",
            "RSYNC_CONNECT_PROG",
            "RSYNC_PARTIAL_DIR",
            "RSYNC_PROTECT_ARGS",
            "RSYNC_OLD_ARGS",
            "RSYNC_PASSWORD",
            "RSYNC_ICONV",
        ] {
            command.env_remove(key);
        }
        let dry_run = args.dry_run.unwrap_or(true);
        let (mut process, input, output, scratch, server_args): (
            Managed,
            Writer,
            Reader,
            Option<Scratch>,
            Option<Vec<String>>,
        ) = match args.role {
            Role::Sender => {
                if args.source.is_some() || args.delete.is_some() || args.dry_run.is_some() {
                    return Err(
                        "sender takes path and server_args; receiver selects dry_run/delete".into(),
                    );
                }
                let remote = args
                    .server_args
                    .ok_or("sender requires receiver-generated server_args")?;
                validate_server_args(&remote, &path)?;
                command.args(&remote[..4]).arg("./");
                let mut process = Managed::spawn(&mut command).map_err(|error| {
                    format!("start system rsync (install rsync with your package manager): {error}")
                })?;
                let input = Box::new(
                    process
                        .child
                        .stdin
                        .take()
                        .ok_or("rsync stdin unavailable")?,
                );
                let output = Box::new(
                    process
                        .child
                        .stdout
                        .take()
                        .ok_or("rsync stdout unavailable")?,
                );
                (process, input, output, None, None)
            }
            Role::Receiver => {
                if args.server_args.is_some() {
                    return Err("receiver does not take server_args".into());
                }
                let source = relative(
                    args.source
                        .as_deref()
                        .ok_or("receiver requires source directory")?,
                )?;
                let scratch = Scratch::new()?;
                let socket = scratch.0.join("io");
                let listener = UnixListener::bind(&socket)
                    .map_err(|error| format!("bind rsync helper: {error}"))?;
                let token = secret()?;
                let helper = self
                    .helper
                    .to_str()
                    .ok_or("ssmcp helper path must be UTF-8")?;
                command.args(["-rt", "--old-args", "--itemize-changes"]);
                if dry_run {
                    command.arg("--dry-run");
                }
                if args.delete.unwrap_or(false) {
                    command.arg("--delete");
                }
                command
                    .arg("--rsh")
                    .arg(format!("{} {HELPER}", rsh_quote(helper)))
                    .arg("--")
                    .arg(format!("peer:{source}/"))
                    .arg("./")
                    .env(SOCKET_ENV, &socket)
                    .env(TOKEN_ENV, &token);
                let mut process = Managed::spawn(&mut command).map_err(|error| {
                    format!("start system rsync (install rsync with your package manager): {error}")
                })?;
                // The ordinary client has no user stdin. Protocol bytes flow through its helper socket.
                process.child.stdin.take();
                let setup = timeout(START_TIMEOUT, async {
                    let (stream, _) = listener.accept().await.map_err(|error| error.to_string())?;
                    if stream.peer_cred().map_err(|error| error.to_string())?.uid()
                        != unsafe { libc::geteuid() }
                    {
                        return Err("rsync helper uid mismatch".to_string());
                    }
                    let mut stream = BufReader::new(stream);
                    let mut line = Vec::new();
                    (&mut stream)
                        .take(8193)
                        .read_until(b'\n', &mut line)
                        .await
                        .map_err(|error| error.to_string())?;
                    if line.len() > 8192 || line.last() != Some(&b'\n') {
                        return Err("invalid rsync helper handshake".into());
                    }
                    let hello: Handshake =
                        serde_json::from_slice(&line).map_err(|error| error.to_string())?;
                    if hello.token != token {
                        return Err("rsync helper token mismatch".into());
                    }
                    validate_server_args(&hello.arguments, &source)?;
                    let (read, write) = tokio::io::split(stream);
                    Ok::<_, String>((
                        hello.arguments,
                        Box::new(write) as Writer,
                        Box::new(read) as Reader,
                    ))
                })
                .await;
                let (remote, input, output) = match setup {
                    Ok(Ok(result)) => result,
                    result => {
                        process.shutdown().await;
                        return Err(match result { Err(_) => "rsync helper startup timed out; use an ssmcp executable with __rsync-transport support".into(), Ok(Err(error)) => error, _=>unreachable!() });
                    }
                };
                drains.push(drain(
                    Box::new(
                        process
                            .child
                            .stdout
                            .take()
                            .ok_or("rsync stdout unavailable")?,
                    ),
                    log.clone(),
                ));
                (process, input, output, Some(scratch), Some(remote))
            }
        };
        drains.push(drain(
            Box::new(
                process
                    .child
                    .stderr
                    .take()
                    .ok_or("rsync stderr unavailable")?,
            ),
            log.clone(),
        ));
        let transfer = Arc::new(Transfer {
            role: args.role,
            process: Mutex::new(process),
            input: Mutex::new(Some(input)),
            output: Mutex::new(output),
            eof: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            log,
            drains,
            _directory: directory,
            _scratch: scratch,
        });
        transfers.insert(id.clone(), transfer);
        let mut result = json!({"id":id,"role":args.role.name(),"max_bytes":MAX_BYTES});
        if let Some(server_args) = server_args {
            result["server_args"] = json!(server_args);
            result["dry_run"] = json!(dry_run);
            result["delete"] = json!(args.delete.unwrap_or(false));
        }
        Ok(result)
    }

    fn directory(&self, path: &str) -> Result<File, String> {
        let mut directory = self.root.try_clone().map_err(|error| error.to_string())?;
        for component in Path::new(path).components() {
            if let Component::Normal(name) = component {
                directory = open_directory(Some(&directory), name.as_encoded_bytes())?;
            }
        }
        Ok(directory)
    }

    pub async fn shutdown(&self) {
        let transfers = std::mem::take(&mut *self.transfers.lock().await);
        for transfer in transfers.into_values() {
            transfer.stop().await;
        }
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("invalid rsync arguments: {error}"))
}

fn relative(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 4096
        || value.chars().any(char::is_control)
        || Path::new(value).is_absolute()
    {
        return Err("rsync paths must be bounded relative directory paths".into());
    }
    let mut parts = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(name) => parts.push(name.to_str().ok_or("rsync path must be UTF-8")?),
            Component::CurDir => {}
            _ => return Err("rsync path cannot escape the shared root".into()),
        }
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

fn validate_server_args(arguments: &[String], source: &str) -> Result<(), String> {
    if arguments.len() != 5
        || arguments[0] != "--server"
        || arguments[1] != "--sender"
        || arguments[3] != "."
        || relative(&arguments[4])? != source
    {
        return Err(
            "rsync sender requires the matching receiver-generated server arguments".into(),
        );
    }
    let option = &arguments[2];
    // rsync -e's private capability suffix is emitted by the client. Everything
    // before it is restricted to recursion, timestamps, itemization and dry run.
    let (flags, capabilities) = option
        .split_once("e.")
        .ok_or("unsupported system rsync option format")?;
    if !flags.starts_with('-')
        || flags.len() > 12
        || !flags[1..].bytes().all(|c| b"nrti".contains(&c))
        || !flags.contains('r')
        || !flags.contains('t')
        || capabilities.len() > 24
        || !capabilities.bytes().all(|c| b"iLsfxCIvu".contains(&c))
    {
        return Err("unsupported or unsafe rsync server options".into());
    }
    Ok(())
}

fn open_directory(parent: Option<&File>, name: &[u8]) -> Result<File, String> {
    let name = CString::new(name).map_err(|_| "invalid directory path")?;
    // SAFETY: name is a live NUL-terminated path, parent remains live, and the
    // resulting descriptor has sole ownership transferred into File.
    let fd = unsafe {
        libc::openat(
            parent.map_or(libc::AT_FDCWD, AsRawFd::as_raw_fd),
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(format!(
            "rsync directory must exist under the shared root without symlinks: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn secret() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .map_err(|error| error.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn rsh_quote(value: &str) -> String {
    // rsync splits its remote-shell command itself; doubled single quotes are
    // its documented way to keep one literal quote within quoted arguments.
    format!("'{}'", value.replace('\'', "''"))
}

fn drain(mut reader: Reader, log: Arc<Mutex<Log>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut bytes = [0u8; 4096];
        loop {
            match reader.read(&mut bytes).await {
                Ok(0) | Err(_) => break,
                Ok(length) => log.lock().await.append(&bytes[..length]),
            }
        }
    })
}

/// Private remote-shell transport. Call before ordinary CLI argument parsing.
/// This does not grant filesystem/network capabilities: it only connects to a
/// same-user, random-token Unix socket created for the owned rsync invocation.
pub async fn transport_main() -> crate::Result<()> {
    let socket = std::env::var_os(SOCKET_ENV).ok_or("rsync helper requires an owned socket")?;
    let token = std::env::var(TOKEN_ENV).map_err(|_| "rsync helper requires a transport token")?;
    let mut arguments = std::env::args().skip(2);
    if arguments.next().as_deref() != Some("peer") || arguments.next().as_deref() != Some("rsync") {
        return Err("invalid rsync helper invocation".into());
    }
    let arguments: Vec<_> = arguments.collect();
    if arguments.len() != 5 {
        return Err("unsupported rsync helper arguments".into());
    }
    let source = relative(&arguments[4])?;
    validate_server_args(&arguments, &source)?;
    if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid rsync transport token".into());
    }
    let mut connection = StdUnixStream::connect(socket)?;
    serde_json::to_writer(
        &mut connection,
        &json!({"token":token,"arguments":arguments}),
    )?;
    connection.write_all(b"\n")?;
    connection.flush()?;
    let mut writer = connection.try_clone()?;
    std::thread::spawn(move || {
        // SAFETY: this dedicated helper owns its standard input descriptor.
        let mut input = unsafe { File::from_raw_fd(libc::STDIN_FILENO) };
        let result = io::copy(&mut input, &mut writer);
        let _ = writer.shutdown(std::net::Shutdown::Write);
        result
    });
    // Bypass Rust stdout line buffering: rsync begins with binary integers.
    // SAFETY: this dedicated helper owns its standard output descriptor.
    let mut output = unsafe { File::from_raw_fd(libc::STDOUT_FILENO) };
    let result = io::copy(&mut connection, &mut output);
    let _ = output.flush();
    // Remote EOF means the remote rsync process ended. Exit like an SSH remote
    // shell: waiting for local stdin here deadlocks rsync waiting for its helper.
    drop(output);
    let _ = connection.shutdown(std::net::Shutdown::Read);
    let _ = connection.shutdown(std::net::Shutdown::Both);
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_server_options_are_not_shell_commands() {
        for path in ["/tmp", "../x", "x/../../x", "x\0y", ""] {
            assert!(relative(path).is_err());
        }
        assert_eq!(
            relative("./space name/./child/").unwrap(),
            "space name/child"
        );
        let mut args: Vec<String> = [
            "--server",
            "--sender",
            "-ntre.iLsfxCIvu",
            ".",
            "space name/",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        assert!(validate_server_args(&args, "space name").is_ok());
        args[2] = "--rsync-path=evil".into();
        assert!(validate_server_args(&args, "space name").is_err());
        args[2] = "-rLte.iLsfxCIvu".into();
        assert!(validate_server_args(&args, "space name").is_err());
        args[2] = "-ntre.iLsfxCIvu".into();
        args[4] = "../escape".into();
        assert!(validate_server_args(&args, "space name").is_err());
    }
}
