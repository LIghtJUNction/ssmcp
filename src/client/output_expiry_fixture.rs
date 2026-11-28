//! Test-only control of one real loopback client; never a production tool/API.
use super::*;
use crate::output::{OutputStore, expiry_fixture};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{Metadata, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::{
        UnixStream,
        unix::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::oneshot,
    time::{Instant, sleep_until},
};

const FD_ENV: &str = "SSMCP_OUTPUT_EXPIRY_FIXTURE_FD";
const CONFIG_ENV: &str = "SSMCP_OUTPUT_EXPIRY_FIXTURE_CONFIG";
const MAX_FRAME_BYTES: usize = 65_536;
const SCENARIO_SECS: u64 = 75;
const CLEANUP_SECS: u64 = 15;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema_version: u32,
    executable: PathBuf,
    executable_sha256: String,
    argv: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    process_id: u32,
    client_id: String,
    connection_attempt: u64,
    store_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Age {
        schema_version: u32,
        binding: Binding,
        target: expiry_fixture::Artifact,
        fresh: expiry_fixture::Artifact,
        active: expiry_fixture::Artifact,
    },
    Stop {
        schema_version: u32,
        binding: Binding,
    },
}

struct Ready {
    client_id: String,
    attempt: u64,
    store: Arc<OutputStore>,
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    bytes: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    mode: u32,
}

impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            bytes: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
            mode: metadata.mode(),
        }
    }
}

struct PinnedExecutable {
    path: PathBuf,
    identity: Identity,
}

impl PinnedExecutable {
    fn open(config: &Config) -> Result<Self, String> {
        if !config.executable.is_absolute()
            || config.executable_sha256.len() != 64
            || !config
                .executable_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("fixture requires an absolute executable and lowercase SHA256".into());
        }
        let path_metadata = std::fs::symlink_metadata(&config.executable)
            .map_err(|_| "pinned fixture executable is unavailable")?;
        if !path_metadata.is_file() || path_metadata.mode() & 0o111 == 0 {
            return Err(
                "pinned fixture executable must be a regular executable, not a symlink".into(),
            );
        }
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&config.executable)
            .map_err(|_| "cannot open pinned fixture executable")?;
        let identity = Identity::of(
            &file
                .metadata()
                .map_err(|_| "cannot stat pinned executable")?,
        );
        if identity != Identity::of(&path_metadata) {
            return Err("pinned executable changed while opening".into());
        }
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .map_err(|_| "cannot hash pinned executable")?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        if crate::hex::encode(digest.finalize()) != config.executable_sha256
            || identity
                != Identity::of(
                    &file
                        .metadata()
                        .map_err(|_| "cannot restat pinned executable")?,
                )
        {
            return Err("pinned executable bytes or identity differ from the binding".into());
        }
        let pinned = Self {
            path: config.executable.clone(),
            identity,
        };
        pinned.check_current()?;
        Ok(pinned)
    }

    fn check_current(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|_| "pinned fixture executable disappeared")?;
        if !metadata.is_file() || Identity::of(&metadata) != self.identity {
            return Err("pinned fixture executable changed before transport launch".into());
        }
        Ok(())
    }
}

struct Hooks {
    executable: PinnedExecutable,
    ready: mpsc::Sender<Ready>,
    stop: watch::Receiver<bool>,
}

tokio::task_local! {
    static HOOKS: Hooks;
}

/// Outside this one scoped fixture, the normal current_exe path is unchanged.
pub(super) fn transport_executable(current: PathBuf) -> crate::Result<PathBuf> {
    match HOOKS.try_with(|hooks| {
        hooks.executable.check_current()?;
        Ok::<_, String>(hooks.executable.path.clone())
    }) {
        Ok(result) => result.map_err(Into::into),
        Err(_) => Ok(current),
    }
}

/// The private owner stop joins the existing terminate branch and its cleanup.
pub(super) struct StopSignal {
    signal: crate::platform::Signal,
    stop: Option<watch::Receiver<bool>>,
}

impl StopSignal {
    pub(super) fn new(signal: crate::platform::Signal) -> Self {
        Self {
            signal,
            stop: HOOKS.try_with(|hooks| hooks.stop.clone()).ok(),
        }
    }

    pub(super) async fn recv(&mut self) {
        if let Some(stop) = &mut self.stop {
            tokio::select! {
                biased;
                _ = wait_for_stop(stop) => {},
                _ = self.signal.recv() => {},
            }
        } else {
            self.signal.recv().await;
        }
    }
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

pub(super) fn runtime_ready(
    runtime: &Arc<Runtime>,
    client_id: &str,
    attempt: u64,
) -> Result<(), String> {
    HOOKS
        .try_with(|hooks| {
            if attempt != 1 {
                return Err("expiry fixture forbids reconnection".into());
            }
            // Idempotent bind verifies this exact Runtime carries the observed ID.
            runtime.bind_client_id(client_id)?;
            hooks
                .ready
                .try_send(Ready {
                    client_id: client_id.to_owned(),
                    attempt,
                    store: runtime.output(),
                })
                .map_err(|_| {
                    "expiry fixture runtime-ready observer is no longer available".to_owned()
                })
        })
        .unwrap_or(Ok(()))
}

fn inherited_socket(fd: i32) -> Result<UnixStream, String> {
    if fd < 3 {
        return Err("fixture control FD must not alias standard streams".into());
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err("fixture control FD is not open".into());
    }
    // The parent transfers ownership of exactly this open descriptor via pass_fds.
    let descriptor = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut socket_type: libc::c_int = 0;
    let mut type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut address_len = std::mem::size_of_val(&address) as libc::socklen_t;
    // SAFETY: all buffers are live, correctly sized writable local storage.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut type_len,
        )
    } != 0
        || socket_type != libc::SOCK_STREAM
        || unsafe {
            libc::getsockname(
                fd,
                (&mut address as *mut libc::sockaddr_storage).cast(),
                &mut address_len,
            )
        } != 0
        || address.ss_family as libc::c_int != libc::AF_UNIX
    {
        return Err("fixture control FD is not a Unix stream socket".into());
    }
    // Do this before spawning the real transport; only this fixture owns control.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err("cannot make fixture control FD close-on-exec".into());
    }
    let socket = std::os::unix::net::UnixStream::from(descriptor);
    socket
        .set_nonblocking(true)
        .map_err(|_| "cannot configure fixture control socket")?;
    UnixStream::from_std(socket).map_err(|_| "cannot attach fixture control socket".to_owned())
}

struct Reader {
    reader: BufReader<OwnedReadHalf>,
    partial: Vec<u8>,
}

impl Reader {
    async fn next(&mut self) -> Result<Option<Request>, String> {
        loop {
            let available = self
                .reader
                .fill_buf()
                .await
                .map_err(|_| "fixture control read failed")?;
            if available.is_empty() {
                return if self.partial.is_empty() {
                    Ok(None)
                } else {
                    Err("fixture control closed during a JSON frame".into())
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |offset| offset + 1);
            if self.partial.len().saturating_add(count) > MAX_FRAME_BYTES {
                return Err("fixture control frame exceeds 64 KiB".into());
            }
            self.partial.extend_from_slice(&available[..count]);
            self.reader.consume(count);
            if newline.is_some() {
                let parsed = serde_json::from_slice(&self.partial[..self.partial.len() - 1])
                    .map_err(|_| "fixture control frame is not a valid protocol request");
                self.partial.clear();
                return parsed.map(Some).map_err(str::to_owned);
            }
        }
    }
}

async fn send(writer: &mut OwnedWriteHalf, value: serde_json::Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(&value).map_err(|_| "cannot encode fixture response")?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("fixture response exceeds 64 KiB".into());
    }
    timeout(Duration::from_secs(2), writer.write_all(&bytes))
        .await
        .map_err(|_| "fixture control write timed out")?
        .map_err(|_| "fixture control write failed".to_owned())
}

fn check_binding(schema_version: u32, actual: &Binding, expected: &Binding) -> Result<(), String> {
    if schema_version != 1 || actual != expected {
        return Err("fixture schema or exact connection/store binding does not match".into());
    }
    Ok(())
}

async fn observe(
    reader: &mut Reader,
    writer: &mut OwnedWriteHalf,
    ready: &mut mpsc::Receiver<Ready>,
    binding: &mut Option<Binding>,
) -> Result<(), String> {
    let ready = tokio::select! {
        ready = ready.recv() => ready.ok_or("client ended before runtime readiness")?,
        request = reader.next() => {
            request?;
            return Err("fixture control closed or sent a request before runtime readiness".into());
        },
    };
    let mut random = [0u8; 16];
    crate::platform::random_bytes(&mut random)
        .map_err(|_| "fixture store binding entropy unavailable")?;
    let exact = Binding {
        process_id: std::process::id(),
        client_id: ready.client_id,
        connection_attempt: ready.attempt,
        store_id: crate::hex::encode(random),
    };
    *binding = Some(exact.clone());
    send(
        writer,
        json!({"schema_version":1,"event":"ready","binding":exact,
        "runtime_bound":true,"retention_secs":crate::output::RETENTION_SECS,"sweep_secs":60}),
    )
    .await?;
    let mut witness: Option<oneshot::Receiver<Result<expiry_fixture::Witness, String>>> = None;
    let mut armed = false;
    let mut observed = false;
    loop {
        tokio::select! {
            request = reader.next() => match request? {
                None => return Err("fixture controller disconnected".into()),
                Some(Request::Age {schema_version, binding, target, fresh, active}) => {
                    check_binding(schema_version, &binding, &exact)?;
                    if armed { return Err("fixture age is one-shot".into()); }
                    // This request freezes all three identities and original expiries.
                    // The real timer performs deferred aging and reports its actual time.
                    let (receipt, receiver) = expiry_fixture::arm(&ready.store,
                        expiry_fixture::Artifacts {target, fresh, active})?;
                    armed = true;
                    witness = Some(receiver);
                    let mut value = serde_json::to_value(receipt).map_err(|_| "cannot encode armed receipt")?;
                    value["schema_version"] = json!(1);
                    value["event"] = json!("armed");
                    value["binding"] = json!(exact);
                    send(writer, value).await?;
                },
                Some(Request::Stop {schema_version, binding}) => {
                    check_binding(schema_version, &binding, &exact)?;
                    if !observed { return Err("fixture stopped before an exact timer removal witness".into()); }
                    return Ok(());
                },
            },
            result = async {
                match &mut witness {
                    Some(receiver) => receiver.await.map_err(|_| "timer observer ended without a witness".to_owned())?,
                    None => std::future::pending().await,
                }
            }, if witness.is_some() => {
                witness = None;
                let receipt = result?;
                let mut value = serde_json::to_value(receipt).map_err(|_| "cannot encode timer receipt")?;
                value["schema_version"] = json!(1);
                value["event"] = json!("timer_removed");
                value["binding"] = json!(exact);
                send(writer, value).await?;
                observed = true;
            },
        }
    }
}

fn loopback_server(server: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(server).map_err(|_| "fixture relay URL is invalid")?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|host| host.is_loopback());
    if url.scheme() != "https"
        || !loopback
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("fixture requires the private HTTPS loopback gateway origin".into());
    }
    Ok(())
}

async fn connect_once(
    config: Config,
    mut stop: watch::Receiver<bool>,
) -> crate::Result<ConnectionExit> {
    let mut options = Options::parse(config.argv.into_iter().map(OsString::from))?
        .ok_or("fixture arguments must create a foreground connection")?;
    if options.is_local()
        || options.destination.is_some()
        || options.list_tools
        || options.daemon
        || options.worker.is_some()
        || options.worker_window_child
        || options.harness.is_some()
        || options.tty.is_some()
        || options.chat.agent != chat_io::Agent::NextCaller
        || !matches!(
            options.chat.input,
            chat_io::Input::None | chat_io::Input::Terminal
        )
    {
        return Err(
            "fixture accepts a foreground native loopback client without models or workers".into(),
        );
    }
    loopback_server(
        options
            .server
            .as_deref()
            .ok_or("fixture requires an explicit gateway origin")?,
    )?;
    tokio::select! {
        biased;
        _ = wait_for_stop(&mut stop) => return Ok(ConnectionExit::Stopped),
        result = apply_account(&mut options) => result?,
    }
    if *stop.borrow() {
        return Ok(ConnectionExit::Stopped);
    }
    let ui = ui::Ui {
        dashboard: false,
        color: false,
        log_format: render::LogFormat::Jsonl,
        ..ui::Ui::default()
    };
    interface::initialize_path(options.ui_config.as_deref());
    render::configure(ui.clone());
    let mut registered = false;
    connect(&options, &ui, None, None, 1, &mut registered).await
}

async fn run_fixture() -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(SCENARIO_SECS);
    let fd: i32 = std::env::var(FD_ENV)
        .map_err(|_| "missing fixture control FD")?
        .parse()
        .map_err(|_| "invalid fixture control FD")?;
    let socket = inherited_socket(fd)?;
    let config_text = std::env::var(CONFIG_ENV).map_err(|_| "missing fixture client config")?;
    if config_text.len() > MAX_FRAME_BYTES {
        return Err("fixture config exceeds 64 KiB".into());
    }
    let config: Config =
        serde_json::from_str(&config_text).map_err(|_| "invalid fixture client config")?;
    if config.schema_version != 1
        || config.argv.is_empty()
        || config.argv.len() > 128
        || config.argv.iter().any(|arg| arg.contains('\0'))
    {
        return Err("invalid fixture schema or client arguments".into());
    }
    let executable = PinnedExecutable::open(&config)?;
    let (read, mut writer) = socket.into_split();
    let mut reader = Reader {
        reader: BufReader::new(read),
        partial: Vec::new(),
    };
    let (ready_sender, mut ready) = mpsc::channel(1);
    let (stop, stopping) = watch::channel(false);
    let hooks = Hooks {
        executable,
        ready: ready_sender,
        stop: stopping.clone(),
    };
    let mut client = Box::pin(HOOKS.scope(hooks, connect_once(config, stopping)));
    let mut client_ended = None;
    let mut binding = None;
    let observation = tokio::select! {
        result = observe(&mut reader, &mut writer, &mut ready, &mut binding) => result,
        result = &mut client => {
            if let Err(error) = &result { eprintln!("expiry fixture client failed: {error}"); }
            client_ended = Some(result);
            Err("real client ended before the expiry protocol completed".into())
        },
        _ = sleep_until(deadline) => Err("expiry fixture exceeded its 75-second scenario deadline".into()),
    };
    // Finish the original client cleanup rather than dropping its live future.
    stop.send_replace(true);
    let cleanup = match client_ended {
        Some(result) => result,
        None => match timeout(Duration::from_secs(CLEANUP_SECS), &mut client).await {
            Ok(result) => result,
            Err(_) => {
                let _ = send(&mut writer, json!({"schema_version":1,"event":"error","binding":binding,
                    "error":"original client cleanup exceeded 15 seconds","cleanup_completed":false})).await;
                return Err("original client cleanup exceeded 15 seconds; owner must reap its fixture process group".into());
            }
        },
    };
    let cleanup_error = match cleanup {
        Ok(ConnectionExit::Stopped) => None,
        Ok(ConnectionExit::Disconnected) => {
            Some("client disconnected instead of completing the requested stop".to_owned())
        }
        Ok(ConnectionExit::Reload { .. }) => {
            Some("fixture does not execute reloads or reconnect".to_owned())
        }
        Err(error) => {
            eprintln!("expiry fixture cleanup failed: {error}");
            Some("original client ended with an error".to_owned())
        }
    };
    if let Some(error) = cleanup_error {
        let _ = send(
            &mut writer,
            json!({"schema_version":1,"event":"error","binding":binding,
            "error":observation.err(),"cleanup_error":error,"cleanup_completed":false}),
        )
        .await;
        return Err(error);
    }
    match observation {
        Ok(()) => {
            send(
                &mut writer,
                json!({"schema_version":1,"event":"stopped","binding":binding,
            "cleanup_completed":true}),
            )
            .await
        }
        Err(error) => {
            let _ = send(
                &mut writer,
                json!({"schema_version":1,"event":"error","binding":binding,
                "error":error,"cleanup_completed":true}),
            )
            .await;
            Err(error)
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the private Python public-output expiry harness and pinned real ssmcp transport"]
async fn public_output_expiry_client_fixture() {
    run_fixture().await.expect("public output expiry fixture");
}

#[test]
fn expiry_control_rejects_other_bindings_and_non_loopback_origins() {
    let binding = Binding {
        process_id: 42,
        client_id: "a".repeat(64),
        connection_attempt: 1,
        store_id: "b".repeat(32),
    };
    assert!(check_binding(1, &binding, &binding).is_ok());
    let mut wrong = binding.clone();
    wrong.store_id = "c".repeat(32);
    assert!(check_binding(1, &wrong, &binding).is_err());
    wrong = binding.clone();
    wrong.client_id = "d".repeat(64);
    assert!(check_binding(1, &wrong, &binding).is_err());
    assert!(check_binding(2, &binding, &binding).is_err());
    assert!(loopback_server("https://127.0.0.1:4321").is_ok());
    for origin in [
        "http://127.0.0.1:4321",
        "https://example.com",
        "https://user@127.0.0.1",
        "https://127.0.0.1/path",
    ] {
        assert!(loopback_server(origin).is_err());
    }
}

#[tokio::test]
async fn expiry_control_bounds_frames_preserves_partial_input_and_closes_inherited_fd_on_exec() {
    use std::{future::Future, os::fd::IntoRawFd};
    let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd = socket.into_raw_fd();
    let stream = inherited_socket(fd).unwrap();
    assert_ne!(
        unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    peer.set_nonblocking(true).unwrap();
    let mut peer = UnixStream::from_std(peer).unwrap();
    let (reader, _writer) = stream.into_split();
    let mut reader = Reader {
        reader: BufReader::new(reader),
        partial: Vec::new(),
    };
    let binding = Binding {
        process_id: 42,
        client_id: "a".repeat(64),
        connection_attempt: 1,
        store_id: "b".repeat(32),
    };
    let frame =
        serde_json::to_vec(&json!({"op":"stop","schema_version":1,"binding":binding})).unwrap();
    peer.write_all(&frame[..7]).await.unwrap();
    reader.reader.fill_buf().await.unwrap();
    let mut partial_read = Box::pin(reader.next());
    let pending =
        std::future::poll_fn(|context| std::task::Poll::Ready(partial_read.as_mut().poll(context)))
            .await;
    assert!(pending.is_pending());
    drop(partial_read);
    assert_eq!(reader.partial, frame[..7]);
    peer.write_all(&frame[7..]).await.unwrap();
    peer.write_all(b"\n").await.unwrap();
    assert!(matches!(
        reader.next().await.unwrap(),
        Some(Request::Stop { .. })
    ));
    let write = async {
        peer.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1])
            .await
            .unwrap();
    };
    let (_, result) = tokio::join!(write, reader.next());
    assert!(result.unwrap_err().contains("exceeds 64 KiB"));
}

#[tokio::test]
async fn expiry_private_stop_uses_the_existing_termination_wait() {
    let (sender, receiver) = watch::channel(false);
    let mut signal = StopSignal {
        signal: terminate_signal().unwrap(),
        stop: Some(receiver),
    };
    sender.send_replace(true);
    timeout(Duration::from_secs(1), signal.recv())
        .await
        .unwrap();
}
