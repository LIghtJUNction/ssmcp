//! A separate worker monitor, tied to the lifetime of its foreground client.
use std::{
    ffi::{OsStr, OsString},
    io::{self, BufReader, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const ADDRESS_ENV: &str = "SSMCP_WORKER_WINDOW_ADDRESS";
const TOKEN_ENV: &str = "SSMCP_WORKER_WINDOW_TOKEN";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_TIMEOUT: Duration = Duration::from_secs(5);
// Keep the task overview bounded separately from complete manager conditions.
const MAX_STATUS_BYTES: usize = 4096;
const MAX_STATUS_FRAME_BYTES: usize = 64 * 1024;
const MAX_MANAGER_METADATA_BYTES: usize = 48 * 1024;
const MAX_ACTIVE_METADATA_BYTES: usize = 32 * 1024;
const MAX_HISTORY_METADATA_BYTES: usize = 16 * 1024;
const MAX_ACTIVE_OPERATIONS: usize = 16;
const MAX_HISTORY_OPERATIONS: usize = 64;
const MAX_OPERATION_TARGETS: usize = 128;
static CHILD_SESSION: OnceLock<Weak<ChildState>> = OnceLock::new();
static LINKED: OnceLock<Mutex<Weak<ParentState>>> = OnceLock::new();

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedManager {
    client_id: String,
    thread_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedManagerTarget {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    slot: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
}

impl From<&crate::worker::ManagerTargetView> for LinkedManagerTarget {
    fn from(target: &crate::worker::ManagerTargetView) -> Self {
        Self {
            kind: target.kind.into(),
            task_id: target.task_id.clone(),
            slot: target.slot,
            epoch: target.epoch,
            revision: target.revision,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedManagerOperation {
    operation_id: u64,
    kind: String,
    state: String,
    mode: Option<String>,
    targets: Vec<LinkedManagerTarget>,
    task_id: Option<String>,
    slot: Option<usize>,
    epoch: Option<u64>,
    summary: Option<String>,
    started_at_ms: u64,
    elapsed_ms: u64,
    wait_ms: Option<u64>,
}

impl From<&crate::worker::ManagerOperationView> for LinkedManagerOperation {
    fn from(operation: &crate::worker::ManagerOperationView) -> Self {
        Self {
            operation_id: operation.operation_id,
            kind: operation.kind.into(),
            state: operation.state.into(),
            mode: operation.mode.map(str::to_owned),
            targets: operation.targets.iter().map(Into::into).collect(),
            task_id: operation.task_id.clone(),
            slot: operation.slot,
            epoch: operation.epoch,
            summary: operation.summary.clone(),
            started_at_ms: operation.started_at_ms,
            elapsed_ms: operation.elapsed_ms,
            wait_ms: operation.wait_ms,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedSnapshot {
    client_id: String,
    process_id: u32,
    generation: String,
    #[serde(default)]
    manager: Option<LinkedManager>,
    total: usize,
    desired_total: usize,
    retiring: usize,
    idle: usize,
    working: usize,
    offline: usize,
    queued: usize,
    registered_workers: usize,
    connected: bool,
    closed: bool,
    stopping: bool,
    manager_operations: Vec<LinkedManagerOperation>,
    #[serde(default)]
    last_resize: Option<ResizeReceipt>,
    #[serde(default)]
    activity: crate::worker::Activity,
}

struct ParentState {
    closed: AtomicBool,
    reload_requested: AtomicBool,
    snapshot: Mutex<Option<(LinkedSnapshot, Instant)>>,
}

enum ChildUpdate {
    Snapshot(Box<LinkedSnapshot>),
    Reload,
}

impl ParentState {
    fn receive(&self, update: ChildUpdate) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        match update {
            ChildUpdate::Snapshot(snapshot) => {
                *self
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some((*snapshot, Instant::now()));
                true
            }
            ChildUpdate::Reload => {
                self.reload_requested.store(true, Ordering::Release);
                false
            }
        }
    }

    fn take_reload_request(&self) -> bool {
        self.reload_requested.swap(false, Ordering::AcqRel)
    }

    fn project_snapshot(&self, include_operations: bool) -> Option<serde_json::Value> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (snapshot, received) = snapshot.as_ref()?;
        if !snapshot.connected || received.elapsed() > Duration::from_secs(3) {
            return None;
        }
        let value = if include_operations {
            serde_json::to_value(snapshot).ok()?
        } else {
            // Existing /worker_pool probes have an 8 KiB projection limit.
            // UI details use the same fresh state without this lightweight cut.
            summary_value(snapshot)?
        };
        // Serialization must not turn a just-closed or expired observation live.
        if self.closed.load(Ordering::Acquire) || received.elapsed() > Duration::from_secs(3) {
            None
        } else {
            Some(value)
        }
    }

    fn watch(&self, reader: &mut impl Read, pid: u32) {
        while let Ok(update) = read_child_update(reader, pid) {
            if !self.receive(update) {
                break;
            }
        }
        self.closed.store(true, Ordering::Release);
        *self
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
    }
}

pub(super) fn linked_snapshot() -> Option<serde_json::Value> {
    linked_state()?.project_snapshot(false)
}

pub(super) fn linked_monitor_snapshot() -> Option<serde_json::Value> {
    linked_state()?.project_snapshot(true)
}

fn linked_state() -> Option<Arc<ParentState>> {
    LINKED
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .upgrade()
}

/// Consumers opt into task activity without expanding frequent count-only
/// reminders or the existing worker_pool summary projection.
pub(super) fn linked_activity() -> Option<serde_json::Value> {
    serde_json::to_value(current_snapshot()?.activity).ok()
}

fn summary_value(snapshot: &LinkedSnapshot) -> Option<serde_json::Value> {
    let mut value = serde_json::to_value(snapshot).ok()?;
    value.as_object_mut()?.remove("activity");
    value.as_object_mut()?.remove("manager_operations");
    Some(value)
}

pub(super) fn linked_overview() -> Option<String> {
    Some(format_overview(&current_snapshot()?))
}

fn current_snapshot() -> Option<LinkedSnapshot> {
    fresh_snapshot(linked_state()?.as_ref())
}

fn fresh_snapshot(state: &ParentState) -> Option<LinkedSnapshot> {
    if state.closed.load(Ordering::Acquire) {
        return None;
    }
    let snapshot = state
        .snapshot
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let (snapshot, received) = snapshot.as_ref()?;
    if !snapshot.connected || received.elapsed() > Duration::from_secs(3) {
        return None;
    }
    let value = snapshot.clone();
    if state.closed.load(Ordering::Acquire) || received.elapsed() > Duration::from_secs(3) {
        None
    } else {
        Some(value)
    }
}

fn format_overview(snapshot: &LinkedSnapshot) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "SSMCP workers · Esc closes\n\nSession: {}\nPID: {}\n\nStations: {} · target: {}\nRegistered: {}\nWorking: {} · standby: {} · offline: {} · closing: {}\nQueued: {}\n",
        snapshot.client_id,
        snapshot.process_id,
        snapshot.total,
        snapshot.desired_total,
        snapshot.registered_workers,
        snapshot.working,
        snapshot.idle,
        snapshot.offline,
        snapshot.retiring,
        snapshot.queued
    );
    if let Some(manager) = &snapshot.manager {
        let _ = writeln!(text, "Manager: {}", manager.thread_id);
    }
    for task in &snapshot.activity.tasks {
        let _ = write!(
            text,
            "\n{} · {}\n{}\nOwner: {}\n",
            task.task_id,
            task.status_label(),
            task.summary,
            task.owner_thread_id.as_deref().unwrap_or("unavailable")
        );
        if let Some(slot) = task.slot {
            let _ = writeln!(
                text,
                "Claimed by: {} · station {} · attempt {}",
                task.worker_thread_id
                    .as_deref()
                    .unwrap_or("participant unavailable"),
                slot + 1,
                task.attempt
            );
        } else {
            text.push_str("Claimed: no\n");
        }
        if let Some(progress) = &task.progress {
            let _ = writeln!(text, "Progress: {progress}");
        }
        if let Some(command) = &task.last_command {
            let _ = writeln!(text, "Last command: {command}");
        }
    }
    if snapshot.activity.omitted > 0 {
        let _ = writeln!(
            text,
            "\n{} more retained tasks omitted",
            snapshot.activity.omitted
        );
    }
    text.push_str("\n/worker +N adds stations\n/worker -N retires stations after acknowledgement");
    text
}

/// The terminal launcher can exit before the monitor. Ownership is established
/// by the monitor's authenticated control connection and an actual process handle.
pub(super) struct WindowChild {
    launcher: Child,
    control: Option<TcpStream>,
    command_lock: Mutex<u64>,
    process: Option<Process>,
    reported_closed: AtomicBool,
    state: Arc<ParentState>,
    watcher: Option<thread::JoinHandle<()>>,
}

pub(super) fn launch(
    executable: &Path,
    args: &[OsString],
    cwd: &Path,
) -> Result<WindowChild, String> {
    if !executable.is_absolute() || !cwd.is_absolute() {
        return Err(
            "worker window requires absolute executable and working-directory paths".into(),
        );
    }
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("could not create worker window control channel: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random)
        .map_err(|_| "could not create worker window identity".to_owned())?;
    let token: String = crate::hex::encode(random);
    let mut command = launcher(executable, args)?;
    command
        .current_dir(cwd)
        .env(ADDRESS_ENV, address.to_string())
        .env(TOKEN_ENV, &token);
    let child = command.spawn().map_err(|error| {
        format!("could not open the Workers window: {error}; set SSMCP_WORKER_TERMINAL to a terminal executable")
    })?;
    let mut window = WindowChild {
        launcher: child,
        control: None,
        command_lock: Mutex::new(0),
        process: None,
        reported_closed: AtomicBool::new(false),
        state: Arc::new(ParentState {
            closed: AtomicBool::new(false),
            reload_requested: AtomicBool::new(false),
            snapshot: Mutex::new(None),
        }),
        watcher: None,
    };
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((mut stream, peer)) if peer.ip().is_loopback() => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .map_err(|error| error.to_string())?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .map_err(|error| error.to_string())?;
                let Ok(pid) = read_identity(&mut stream, &token) else {
                    // Unauthenticated local connections cannot select a process.
                    if Instant::now() >= deadline {
                        return Err(
                            "Workers window did not identify itself before the startup deadline"
                                .into(),
                        );
                    }
                    continue;
                };
                window.process = Some(Process::open(pid, executable).map_err(|error| {
                    format!("could not establish ownership of the Workers window: {error}")
                })?);
                window.control = Some(stream);
                let stream = window
                    .control
                    .as_mut()
                    .expect("accepted control connection");
                stream.write_all(b"A").map_err(|error| error.to_string())?;
                stream
                    .set_read_timeout(Some(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .max(Duration::from_millis(1)),
                    ))
                    .map_err(|error| error.to_string())?;
                let mut ready = [0];
                if stream.read_exact(&mut ready).is_err() || ready != *b"R" {
                    return Err("Workers window exited or failed to become ready; check its terminal for the connection error".into());
                }
                let mut reader =
                    BufReader::new(stream.try_clone().map_err(|error| error.to_string())?);
                let snapshot = read_snapshot(&mut reader, pid).map_err(|error| format!("Workers window readiness did not include its real session identity: {error}"))?;
                *window
                    .state
                    .snapshot
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some((snapshot, Instant::now()));
                stream
                    .set_read_timeout(None)
                    .map_err(|error| error.to_string())?;
                let state = Arc::clone(&window.state);
                window.watcher = Some(
                    thread::Builder::new()
                        .name("ssmcp-worker-status".into())
                        .spawn(move || state.watch(&mut reader, pid))
                        .map_err(|error| format!("could not watch the Workers window: {error}"))?,
                );
                *LINKED
                    .get_or_init(|| Mutex::new(Weak::new()))
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Arc::downgrade(&window.state);
                return Ok(window);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => {
                return Err(format!(
                    "could not receive worker window readiness: {error}"
                ));
            }
        }
        if let Some(status) = window
            .launcher
            .try_wait()
            .map_err(|error| error.to_string())?
            && !status.success()
        {
            return Err(format!(
                "terminal could not open the Workers window ({status}); set SSMCP_WORKER_TERMINAL to a working terminal executable"
            ));
        }
        if Instant::now() >= deadline {
            return Err("Workers window did not become ready within 30 seconds; check the desktop display and SSMCP_WORKER_TERMINAL".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

impl WindowChild {
    /// Only this owned child's authenticated control connection can request it.
    pub(super) fn take_reload_request(&self) -> bool {
        self.state.take_reload_request()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
            || self
                .process
                .as_ref()
                .is_some_and(|process| process.exited(Duration::ZERO))
    }

    /// Send an absolute desired size; a later snapshot reports actual core state.
    pub(super) fn request_resize(&self, total: usize) -> Result<u64, String> {
        if total > 128 {
            return Err("worker total must be between 0 and 128".into());
        }
        let mut sequence = self
            .command_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.is_closed() {
            return Err("Workers window is no longer connected".into());
        }
        let request_id = (*sequence)
            .checked_add(1)
            .ok_or("worker resize request counter is exhausted")?;
        let command = resize_command(request_id, total)?;
        *sequence = request_id;
        let mut stream = self
            .control
            .as_ref()
            .ok_or("Workers window has no control channel")?;
        if let Err(error) = stream.write_all(&command) {
            self.state.closed.store(true, Ordering::Release);
            let _ = stream.shutdown(Shutdown::Both);
            return Err(format!("could not resize the Workers window: {error}"));
        }
        Ok(request_id)
    }

    /// Report a closed monitor once, so the caller can keep its chat available.
    pub(super) fn poll_closed(&self) -> Result<bool, String> {
        if self.reported_closed.load(Ordering::Acquire) {
            return Ok(false);
        }
        // A process can exit immediately after writing its reload request. Let
        // the authenticated reader drain that final message before classifying
        // the exit; PID liveness alone would lose the request.
        if self.state.closed.load(Ordering::Acquire)
            || self
                .watcher
                .as_ref()
                .is_some_and(thread::JoinHandle::is_finished)
        {
            self.state.closed.store(true, Ordering::Release);
            return Ok(!self.reported_closed.swap(true, Ordering::AcqRel));
        }
        Ok(false)
    }
}

fn resize_command(request_id: u64, total: usize) -> Result<[u8; 11], String> {
    if total > 128 || request_id == 0 {
        return Err("worker total must be between 0 and 128 with a nonzero request ID".into());
    }
    let mut command = [0; 11];
    command[0] = b'N';
    command[1..3].copy_from_slice(&(total as u16).to_be_bytes());
    command[3..].copy_from_slice(&request_id.to_be_bytes());
    Ok(command)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ResizeRequest {
    request_id: u64,
    target: usize,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResizeReceipt {
    request_id: u64,
    target: usize,
    accepted: bool,
    error: Option<String>,
}

#[derive(Default)]
struct ResizeState {
    pending: Option<ResizeRequest>,
    active: Option<ResizeRequest>,
    receipt: Option<ResizeReceipt>,
}

#[derive(Debug, PartialEq, Eq)]
enum ControlCommand {
    Stop,
    Resize(ResizeRequest),
}

/// A fixed-size decoder preserves fragmented commands across read timeouts.
#[derive(Default)]
struct ControlDecoder {
    payload: [u8; 10],
    received: usize,
    reading: bool,
}

impl ControlDecoder {
    fn push(&mut self, byte: u8) -> io::Result<Option<ControlCommand>> {
        if self.reading {
            self.payload[self.received] = byte;
            self.received += 1;
            if self.received == self.payload.len() {
                self.reading = false;
                let target = usize::from(u16::from_be_bytes(self.payload[..2].try_into().unwrap()));
                let request_id = u64::from_be_bytes(self.payload[2..].try_into().unwrap());
                if target > 128 || request_id == 0 {
                    return Err(io::Error::other(
                        "invalid worker resize target or request ID",
                    ));
                }
                return Ok(Some(ControlCommand::Resize(ResizeRequest {
                    request_id,
                    target,
                })));
            }
            return Ok(None);
        }
        match byte {
            b'S' => Ok(Some(ControlCommand::Stop)),
            b'N' => {
                self.reading = true;
                self.received = 0;
                Ok(None)
            }
            _ => Err(io::Error::other("invalid worker window command")),
        }
    }
}

impl Drop for WindowChild {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        *self
            .state
            .snapshot
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        if let Some(stream) = &mut self.control {
            let _ = stream.set_nonblocking(false);
            let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
            let _ = stream.write_all(b"S");
            let _ = stream.shutdown(Shutdown::Write);
        }
        if let Some(process) = &self.process
            && !process.exited(STOP_TIMEOUT)
        {
            process.stop();
            if !process.exited(Duration::from_secs(2)) {
                eprintln!(
                    "ssmcp: Workers window process {} did not finish stopping",
                    process.pid
                );
            }
        }
        if let Some(stream) = &self.control {
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        if self.launcher.try_wait().ok().flatten().is_none() {
            let _ = self.launcher.kill();
        }
        let _ = self.launcher.wait();
    }
}

struct ChildState {
    stop: AtomicBool,
    ready: AtomicBool,
    reload_sent: AtomicBool,
    stream: Mutex<TcpStream>,
    client_id: Mutex<Option<String>>,
    resize: Mutex<ResizeState>,
}

pub(super) struct ChildSession {
    state: Arc<ChildState>,
    watcher: Option<thread::JoinHandle<()>>,
}

pub(super) fn child_session() -> Result<ChildSession, String> {
    #[cfg(windows)]
    restore_native_console()
        .map_err(|error| format!("could not attach the Workers window console: {error}"))?;
    let address: SocketAddr = std::env::var(ADDRESS_ENV)
        .map_err(|_| "internal Workers window has no parent control channel")?
        .parse()
        .map_err(|_| "internal Workers window control address is invalid")?;
    if !address.ip().is_loopback() {
        return Err("internal Workers window control address must be loopback".into());
    }
    let token =
        std::env::var(TOKEN_ENV).map_err(|_| "internal Workers window has no parent identity")?;
    if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("internal Workers window parent identity is invalid".into());
    }
    let session = connect_child(address, &token)
        .map_err(|error| format!("could not connect this Workers window to its parent: {error}"))?;
    CHILD_SESSION
        .set(Arc::downgrade(&session.state))
        .map_err(|_| "internal Workers window control channel was already initialized")?;
    Ok(session)
}

fn connect_child(address: SocketAddr, token: &str) -> io::Result<ChildSession> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    writeln!(stream, "{token} {}", std::process::id())?;
    let mut accepted = [0];
    stream.read_exact(&mut accepted)?;
    if accepted != *b"A" {
        return Err(io::Error::other("parent rejected the worker identity"));
    }
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    let mut reader = stream.try_clone()?;
    let state = Arc::new(ChildState {
        stop: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        reload_sent: AtomicBool::new(false),
        stream: Mutex::new(stream),
        client_id: Mutex::new(None),
        resize: Mutex::new(ResizeState::default()),
    });
    let watched = Arc::clone(&state);
    let watcher = thread::Builder::new()
        .name("ssmcp-worker-parent".into())
        .spawn(move || {
            let mut last_sent = Instant::now();
            let mut decoder = ControlDecoder::default();
            loop {
                let mut byte = [0];
                match reader.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => match decoder.push(byte[0]) {
                        Ok(Some(ControlCommand::Stop)) | Err(_) => break,
                        Ok(Some(ControlCommand::Resize(request))) => {
                            watched
                                .resize
                                .lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .pending = Some(request);
                        }
                        Ok(None) => {}
                    },
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => break,
                }
                if watched.ready.load(Ordering::Acquire)
                    && last_sent.elapsed() >= Duration::from_secs(1)
                {
                    let client_id = watched
                        .client_id
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone();
                    if let Some(client_id) = client_id
                        && watched.send_current_snapshot(&client_id, false).is_err()
                    {
                        break;
                    }
                    last_sent = Instant::now();
                }
            }
            watched.stop.store(true, Ordering::Release);
        })?;
    Ok(ChildSession {
        state,
        watcher: Some(watcher),
    })
}

pub(super) fn child_ready(client_id: &str) -> Result<(), String> {
    if let Some(state) = CHILD_SESSION.get().and_then(Weak::upgrade) {
        state.announce_ready(client_id).map_err(|error| {
            format!("Workers window parent disconnected before readiness: {error}")
        })?;
    }
    Ok(())
}

/// Pause snapshots before the accepted reload tears down the old runtime.
pub(super) fn child_prepare_reload() {
    if let Some(state) = CHILD_SESSION.get().and_then(Weak::upgrade) {
        state.prepare_reload();
    }
}

/// Called after the old connection, jobs, and terminal have been cleaned up.
pub(super) fn child_request_reload() -> Result<(), String> {
    CHILD_SESSION
        .get()
        .and_then(Weak::upgrade)
        .ok_or("Workers window has no live parent control channel")?
        .request_reload()
        .map_err(|error| format!("could not request worker window reload: {error}"))
}

impl ChildState {
    fn prepare_reload(&self) {
        let _stream = self
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.ready.store(false, Ordering::Release);
    }

    fn request_reload(&self) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::other("parent has stopped"));
        }
        if self.ready.load(Ordering::Acquire) {
            return Err(io::Error::other("worker reload has not been prepared"));
        }
        if !self.reload_sent.load(Ordering::Acquire) {
            stream.write_all(b"L\n")?;
            self.reload_sent.store(true, Ordering::Release);
        }
        Ok(())
    }

    fn take_resize(&self) -> Option<usize> {
        if self.stop.load(Ordering::Acquire) {
            return None;
        }
        let mut resize = self
            .resize
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if resize.active.is_some() {
            return None;
        }
        let request = resize.pending.take()?;
        resize.active = Some(request);
        Some(request.target)
    }

    fn report_resize(
        &self,
        total: usize,
        result: Result<serde_json::Value, String>,
    ) -> io::Result<()> {
        {
            let mut resize = self
                .resize
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let request = resize
                .active
                .filter(|request| request.target == total)
                .ok_or_else(|| {
                    io::Error::other("resize result does not match the active request")
                })?;
            let error = result.err().map(|message| bounded_resize_error(&message));
            resize.receipt = Some(ResizeReceipt {
                request_id: request.request_id,
                target: total,
                accepted: error.is_none(),
                error,
            });
            resize.active = None;
        }
        if self.ready.load(Ordering::Acquire) {
            let client_id = self
                .client_id
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(client_id) = client_id {
                self.send_current_snapshot(&client_id, false)?;
            }
        }
        Ok(())
    }

    fn announce_ready(&self, client_id: &str) -> io::Result<()> {
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::other("parent has stopped"));
        }
        *self
            .client_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(client_id.to_owned());
        self.send_current_snapshot(client_id, !self.ready.load(Ordering::Acquire))
    }

    fn send_current_snapshot(&self, client_id: &str, first: bool) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !first && !self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        // Copy the receipt before sampling core state. A new accepted receipt
        // therefore never accompanies a snapshot taken before its core update.
        let receipt = self
            .resize
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .receipt
            .clone();
        let mut snapshot = local_snapshot(client_id)
            .ok_or_else(|| io::Error::other("worker pool is unavailable"))?;
        snapshot.last_resize = receipt;
        self.write_snapshot(&mut stream, &snapshot, first)
    }

    #[cfg(test)]
    fn send_snapshot(&self, snapshot: &LinkedSnapshot, first: bool) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut snapshot = snapshot.clone();
        snapshot.last_resize = self
            .resize
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .receipt
            .clone();
        self.write_snapshot(&mut stream, &snapshot, first)
    }

    fn write_snapshot(
        &self,
        stream: &mut TcpStream,
        snapshot: &LinkedSnapshot,
        first: bool,
    ) -> io::Result<()> {
        let first = first && !self.ready.load(Ordering::Acquire);
        let frame = status_frame(snapshot, first)?;
        stream.write_all(&frame)?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }
}

fn bounded_resize_error(message: &str) -> String {
    let mut text = String::new();
    for ch in message.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if text.len() + ch.len_utf8() > 512 {
            break;
        }
        text.push(ch);
    }
    text
}

pub(super) fn report_resize_result(
    total: usize,
    result: Result<serde_json::Value, String>,
) -> Result<(), String> {
    let state = CHILD_SESSION
        .get()
        .and_then(Weak::upgrade)
        .ok_or("Workers window has no parent control channel")?;
    state
        .report_resize(total, result)
        .map_err(|error| format!("could not report Workers resize result: {error}"))
}

pub(super) fn child_stop_requested() -> bool {
    CHILD_SESSION
        .get()
        .and_then(Weak::upgrade)
        .is_some_and(|state| state.stop.load(Ordering::Acquire))
}

pub(super) fn take_resize_request() -> Option<usize> {
    CHILD_SESSION
        .get()
        .and_then(Weak::upgrade)
        .and_then(|state| state.take_resize())
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        {
            let stream = self
                .state
                .stream
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let _ = stream.shutdown(Shutdown::Both);
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
    }
}

fn local_snapshot(client_id: &str) -> Option<LinkedSnapshot> {
    let (snapshot, activity) = crate::worker::display_with_activity()?;
    let steering = super::steering::snapshot();
    Some(LinkedSnapshot {
        client_id: client_id.to_owned(),
        process_id: std::process::id(),
        generation: snapshot.generation.clone(),
        manager: snapshot.manager.as_ref().map(|manager| LinkedManager {
            client_id: manager.client_id.clone(),
            thread_id: manager.thread_id.clone(),
        }),
        total: snapshot.total,
        desired_total: snapshot.desired_total,
        retiring: snapshot.retiring,
        idle: snapshot.idle,
        working: snapshot.working,
        offline: snapshot.offline,
        queued: snapshot.queued,
        registered_workers: snapshot.registered_workers,
        connected: steering["connected"].as_bool().unwrap_or(false),
        closed: snapshot.closed,
        stopping: snapshot.stopping,
        manager_operations: snapshot.manager_operations.iter().map(Into::into).collect(),
        last_resize: None,
        activity,
    })
}

fn encode_snapshot(snapshot: &LinkedSnapshot) -> io::Result<Vec<u8>> {
    let mut snapshot = snapshot.clone();
    // Preserve the existing task-overview budget and explicit omitted count.
    // Manager conditions are restored whole after bounding only the overview.
    let operations = std::mem::take(&mut snapshot.manager_operations);
    loop {
        let bytes = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
        if bytes.len() < MAX_STATUS_BYTES {
            break;
        }
        if snapshot.activity.tasks.pop().is_none() {
            return Err(io::Error::other(
                "worker window status exceeded its size limit",
            ));
        }
        snapshot.activity.omitted += 1;
        snapshot.activity.truncated = true;
    }
    snapshot.manager_operations = operations;
    let mut bytes = serde_json::to_vec(&snapshot).map_err(io::Error::other)?;
    if bytes.len() >= MAX_STATUS_FRAME_BYTES {
        return Err(io::Error::other(
            "worker window status exceeded its size limit",
        ));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn bounded_display_text(text: &str, maximum: usize) -> bool {
    text.len() <= maximum && !text.chars().any(char::is_control)
}

fn bounded_identity(text: &str, maximum: usize) -> bool {
    !text.is_empty() && bounded_display_text(text, maximum)
}

fn valid_manager_target(target: &LinkedManagerTarget) -> bool {
    match target.kind.as_str() {
        "task_finished" | "task_result" => {
            target
                .task_id
                .as_deref()
                .is_some_and(|id| bounded_identity(id, 256))
                && target.slot.is_none()
                && target.epoch.is_none()
                && target.revision.is_none()
        }
        "worker_idle" => {
            target.task_id.is_none()
                && target.slot.is_some_and(|slot| slot < 128)
                && target.epoch.is_some_and(|epoch| epoch > 0)
                && target.revision.is_none()
        }
        "availability_change" => {
            target.task_id.is_none() && target.slot.is_none() && target.epoch.is_none()
        }
        _ => false,
    }
}

fn valid_manager_operation(operation: &LinkedManagerOperation) -> bool {
    if operation.operation_id == 0
        || operation.targets.len() > MAX_OPERATION_TARGETS
        || !operation.targets.iter().all(valid_manager_target)
    {
        return false;
    }
    if matches!(operation.kind.as_str(), "submit" | "claim") {
        let task = operation
            .task_id
            .as_deref()
            .is_some_and(|id| bounded_identity(id, 256));
        let summary = operation
            .summary
            .as_deref()
            .is_some_and(|text| bounded_display_text(text, 160));
        return task
            && summary
            && operation.mode.is_none()
            && operation.targets.is_empty()
            && operation.wait_ms.is_none()
            && operation.elapsed_ms == 0
            && if operation.kind == "submit" {
                operation.state == "submitted"
                    && operation.slot.is_none()
                    && operation.epoch.is_none()
            } else {
                operation.state == "assigned"
                    && operation.slot.is_some_and(|slot| slot < 128)
                    && operation.epoch.is_some_and(|epoch| epoch > 0)
            };
    }
    if operation.task_id.is_some()
        || operation.slot.is_some()
        || operation.epoch.is_some()
        || operation.summary.is_some()
        || !operation.wait_ms.is_some_and(|wait| wait <= 5000)
    {
        return false;
    }
    match operation.kind.as_str() {
        "wait_for" => {
            matches!(operation.mode.as_deref(), Some("any" | "all"))
                && !operation.targets.is_empty()
                && operation
                    .targets
                    .iter()
                    .all(|target| matches!(target.kind.as_str(), "task_finished" | "worker_idle"))
                && matches!(
                    operation.state.as_str(),
                    "waiting"
                        | "satisfied"
                        | "timed_out"
                        | "indeterminate"
                        | "unavailable"
                        | "cancelled"
                        | "error"
                )
        }
        "result" => {
            operation.mode.as_deref() == Some("single")
                && operation.targets.len() == 1
                && operation.targets[0].kind == "task_result"
                && matches!(
                    operation.state.as_str(),
                    "waiting" | "satisfied" | "timed_out" | "indeterminate" | "cancelled" | "error"
                )
        }
        "wait_available" => {
            operation.mode.as_deref() == Some("change")
                && operation.targets.len() == 1
                && operation.targets[0].kind == "availability_change"
                && matches!(
                    operation.state.as_str(),
                    "waiting"
                        | "current"
                        | "changed"
                        | "unchanged"
                        | "not_accepting"
                        | "cancelled"
                        | "error"
                )
        }
        _ => false,
    }
}

fn validate_manager_operations(operations: &[LinkedManagerOperation]) -> io::Result<()> {
    let mut previous = 0;
    let mut active_targets = 0usize;
    let mut active = Vec::new();
    let mut history = Vec::new();
    if operations.len() > MAX_ACTIVE_OPERATIONS + MAX_HISTORY_OPERATIONS {
        return Err(io::Error::other("too many worker manager operations"));
    }
    for operation in operations {
        if !valid_manager_operation(operation) || operation.operation_id <= previous {
            return Err(io::Error::other("invalid worker manager operation"));
        }
        previous = operation.operation_id;
        if operation.state == "waiting" {
            active_targets += operation.targets.len();
            active.push(operation);
        } else {
            history.push(operation);
        }
    }
    if active.len() > MAX_ACTIVE_OPERATIONS
        || history.len() > MAX_HISTORY_OPERATIONS
        || active_targets > MAX_OPERATION_TARGETS
    {
        return Err(io::Error::other("worker manager operation limits exceeded"));
    }
    let active_bytes = serde_json::to_vec(&active).map_err(io::Error::other)?;
    let history_bytes = serde_json::to_vec(&history).map_err(io::Error::other)?;
    let metadata = serde_json::to_vec(operations).map_err(io::Error::other)?;
    if active_bytes.len() > MAX_ACTIVE_METADATA_BYTES
        || history_bytes.len() > MAX_HISTORY_METADATA_BYTES
        || metadata.len() > MAX_MANAGER_METADATA_BYTES
    {
        return Err(io::Error::other(
            "worker manager operation metadata exceeded its size limit",
        ));
    }
    Ok(())
}

fn validate_snapshot(snapshot: &LinkedSnapshot, pid: u32) -> io::Result<()> {
    let counts = snapshot
        .idle
        .checked_add(snapshot.working)
        .and_then(|n| n.checked_add(snapshot.offline))
        .and_then(|n| n.checked_add(snapshot.retiring));
    if snapshot.process_id != pid
        || !bounded_identity(&snapshot.client_id, 128)
        || !bounded_identity(&snapshot.generation, 128)
        || snapshot.manager.as_ref().is_some_and(|manager| {
            manager.client_id != snapshot.client_id || !bounded_identity(&manager.thread_id, 128)
        })
        || (snapshot.manager.is_none() && !snapshot.manager_operations.is_empty())
        || snapshot.total > 128
        || snapshot.desired_total > 128
        || snapshot.registered_workers > snapshot.total
        || snapshot.queued > 128
        || counts != Some(snapshot.total)
        || !snapshot.activity.valid()
        || snapshot.last_resize.as_ref().is_some_and(|receipt| {
            receipt.request_id == 0
                || receipt.target > 128
                || receipt.accepted == receipt.error.is_some()
                || receipt
                    .error
                    .as_ref()
                    .is_some_and(|error| !bounded_display_text(error, 512))
        })
    {
        return Err(io::Error::other("invalid worker window status"));
    }
    validate_manager_operations(&snapshot.manager_operations)
}

fn status_frame(snapshot: &LinkedSnapshot, first: bool) -> io::Result<Vec<u8>> {
    validate_snapshot(snapshot, std::process::id())?;
    let body = encode_snapshot(snapshot)?;
    let overhead = usize::from(first);
    if body.len() > MAX_STATUS_FRAME_BYTES - overhead {
        return Err(io::Error::other(
            "worker window status exceeded its size limit",
        ));
    }
    let mut frame = Vec::with_capacity(body.len() + overhead);
    if first {
        frame.push(b'R');
    }
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn read_snapshot(reader: &mut impl Read, pid: u32) -> io::Result<LinkedSnapshot> {
    // The readiness byte was consumed by the caller and counts toward the frame.
    match read_child_update_bounded(reader, pid, MAX_STATUS_FRAME_BYTES - 1)? {
        ChildUpdate::Snapshot(snapshot) => Ok(*snapshot),
        ChildUpdate::Reload => Err(io::Error::other("worker requested reload before readiness")),
    }
}

fn read_child_update(reader: &mut impl Read, pid: u32) -> io::Result<ChildUpdate> {
    read_child_update_bounded(reader, pid, MAX_STATUS_FRAME_BYTES)
}

fn read_child_update_bounded(
    reader: &mut impl Read,
    pid: u32,
    maximum: usize,
) -> io::Result<ChildUpdate> {
    let mut line = Vec::with_capacity(512);
    for _ in 0..maximum {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        if byte == *b"\n" {
            if line == b"L" {
                return Ok(ChildUpdate::Reload);
            }
            let snapshot: LinkedSnapshot =
                serde_json::from_slice(&line).map_err(io::Error::other)?;
            validate_snapshot(&snapshot, pid)?;
            return Ok(ChildUpdate::Snapshot(Box::new(snapshot)));
        }
        line.push(byte[0]);
    }
    Err(io::Error::other(
        "worker window status exceeded its size limit",
    ))
}

fn read_identity(stream: &mut TcpStream, token: &str) -> io::Result<u32> {
    let mut message = Vec::with_capacity(80);
    for _ in 0..80 {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        if byte == *b"\n" {
            let message = std::str::from_utf8(&message)
                .map_err(|_| io::Error::other("invalid worker identity"))?;
            let (actual, pid) = message
                .split_once(' ')
                .ok_or_else(|| io::Error::other("invalid worker identity"))?;
            if actual != token {
                return Err(io::Error::other("worker identity mismatch"));
            }
            let pid: u32 = pid
                .parse()
                .map_err(|_| io::Error::other("invalid worker PID"))?;
            if pid <= 1 || pid == std::process::id() || pid > i32::MAX as u32 {
                return Err(io::Error::other("invalid worker PID"));
            }
            return Ok(pid);
        }
        message.push(byte[0]);
    }
    Err(io::Error::other("worker identity exceeded its size limit"))
}

fn launcher(executable: &Path, args: &[OsString]) -> Result<Command, String> {
    #[cfg(windows)]
    if std::env::var_os("SSMCP_WORKER_TERMINAL").is_none() {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;
        let mut command = Command::new(executable);
        command
            .args(args)
            .creation_flags(CREATE_NEW_CONSOLE)
            .env("SSMCP_WORKER_WINDOW_NATIVE_CONSOLE", "1");
        return Ok(command);
    }
    let terminal = choose_terminal()?;
    let mut command = Command::new(&terminal);
    if terminal.file_stem() == Some(OsStr::new("ztty")) {
        // A fresh broker preserves the control environment even when an older
        // ztty is already running under the user's default broker socket.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        command.env(
            "ZTTY_SOCKET",
            format!("@ssmcp-worker-{}-{stamp}", std::process::id()),
        );
    }
    command
        .args(terminal_args(&terminal, executable, args))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[cfg(windows)]
fn restore_native_console() -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, IntoRawHandle};
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };
    if std::env::var_os("SSMCP_WORKER_WINDOW_NATIVE_CONSOLE").as_deref() != Some(OsStr::new("1")) {
        return Ok(());
    }
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")?;
    let output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")?;
    let error = output.try_clone()?;
    for (kind, file) in [
        (STD_INPUT_HANDLE, input),
        (STD_OUTPUT_HANDLE, output),
        (STD_ERROR_HANDLE, error),
    ] {
        // SAFETY: the file owns a newly opened handle for this child's console.
        if unsafe { SetStdHandle(kind, file.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // Standard handles remain open for the rest of this child process.
        let _ = file.into_raw_handle();
    }
    Ok(())
}

fn choose_terminal() -> Result<PathBuf, String> {
    if let Some(terminal) = std::env::var_os("SSMCP_WORKER_TERMINAL") {
        if terminal.is_empty() {
            return Err("SSMCP_WORKER_TERMINAL must name one terminal executable".into());
        }
        return find_executable(&terminal).ok_or_else(|| {
            "SSMCP_WORKER_TERMINAL does not name an executable; provide one executable path without shell arguments".into()
        });
    }
    if let Some(terminal) = std::env::var_os("TERMINAL")
        && let Some(path) = find_executable(&terminal)
    {
        return Ok(path);
    }
    for terminal in [
        "kitty",
        "foot",
        "alacritty",
        "wezterm",
        "ghostty",
        "konsole",
        "gnome-terminal",
        "kgx",
        "xterm",
        "ztty",
    ] {
        if let Some(path) = find_executable(OsStr::new(terminal)) {
            return Ok(path);
        }
    }
    Err("could not find a terminal for the Workers window; set SSMCP_WORKER_TERMINAL to an installed terminal executable, or use --daemon for a background pool".into())
}

fn find_executable(name: &OsStr) -> Option<PathBuf> {
    let name = Path::new(name);
    let executable = |path: &Path| {
        let Ok(metadata) = path.metadata() else {
            return false;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            metadata.is_file()
        }
    };
    if name.components().count() > 1 || name.is_absolute() {
        return executable(name).then(|| name.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .find(|path| executable(path))
    })
}

fn terminal_args(terminal: &Path, executable: &Path, args: &[OsString]) -> Vec<OsString> {
    let name = terminal
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    let mut result: Vec<OsString> = match name.as_str() {
        "kitty" => vec!["--title".into(), "SSMCP · Workers".into(), "--".into()],
        "foot" | "footclient" | "kgx" => vec!["--".into()],
        "gnome-terminal" => vec!["--wait".into(), "--window".into(), "--".into()],
        "wezterm" => vec!["start".into(), "--always-new-process".into(), "--".into()],
        "konsole" => vec!["--separate".into(), "-e".into()],
        "wt" => vec!["-w".into(), "new".into(), "new-tab".into(), "--".into()],
        _ => vec!["-e".into()],
    };
    result.push(executable.as_os_str().to_owned());
    result.extend_from_slice(args);
    result
}

struct Process {
    pid: u32,
    #[cfg(target_os = "linux")]
    handle: std::os::fd::OwnedFd,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
}

impl Process {
    fn open(pid: u32, executable: &Path) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            let handle = crate::pty::open_pidfd(pid as libc::pid_t)?;
            let actual = std::fs::metadata(format!("/proc/{pid}/exe"))?;
            let expected = std::fs::metadata(executable)?;
            if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
                return Err(io::Error::other(
                    "worker executable does not match its parent",
                ));
            }
            Ok(Self { pid, handle })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::{FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::Threading::{
                OpenProcess, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            };
            let _ = executable;
            // SAFETY: OpenProcess returns a new handle for this authenticated PID.
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: this fresh handle is owned exactly once.
            Ok(Self {
                pid,
                handle: unsafe { OwnedHandle::from_raw_handle(raw) },
            })
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = executable;
            Ok(Self { pid })
        }
    }

    fn exited(&self, wait: Duration) -> bool {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let start = Instant::now();
            loop {
                let mut poll = libc::pollfd {
                    fd: self.handle.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                let timeout = wait
                    .saturating_sub(start.elapsed())
                    .as_millis()
                    .min(i32::MAX as u128) as i32;
                // SAFETY: one initialized pollfd borrows a live owned process descriptor.
                let result = unsafe { libc::poll(&mut poll, 1, timeout) };
                if result > 0 {
                    return poll.revents & (libc::POLLIN | libc::POLLHUP) != 0;
                }
                if result == 0
                    || start.elapsed() >= wait
                    || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                {
                    return false;
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{
                Foundation::WAIT_OBJECT_0, System::Threading::WaitForSingleObject,
            };
            // SAFETY: the owned process handle remains valid throughout this wait.
            unsafe {
                WaitForSingleObject(
                    self.handle.as_raw_handle(),
                    wait.as_millis().min(u32::MAX as u128) as u32,
                ) == WAIT_OBJECT_0
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let deadline = Instant::now() + wait;
            loop {
                // SAFETY: signal zero only observes the authenticated child's PID.
                if unsafe { libc::kill(self.pid as libc::pid_t, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return true;
                }
                if Instant::now() >= deadline {
                    return false;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = wait;
            false
        }
    }

    fn stop(&self) {
        #[cfg(target_os = "linux")]
        crate::pty::stop_session(self.pid as libc::pid_t, &self.handle);
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: the process handle names only the child authenticated above.
            unsafe {
                windows_sys::Win32::System::Threading::TerminateProcess(
                    self.handle.as_raw_handle(),
                    1,
                );
            }
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        // SAFETY: this PID was obtained over this window's private live control channel.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_commands_keep_each_argument_literal_and_open_new_windows() {
        let executable = Path::new("/tmp/path with spaces/ssmcp");
        let args = [
            "--worker",
            "3",
            "--name",
            "中文 $HOME $(touch nope); 'quotes'",
            "--ssmcp-worker-window-child",
        ]
        .map(OsString::from);
        for (name, prefix) in [
            ("kitty", vec!["--title", "SSMCP · Workers", "--"]),
            ("foot", vec!["--"]),
            ("gnome-terminal", vec!["--wait", "--window", "--"]),
            ("wezterm", vec!["start", "--always-new-process", "--"]),
            ("fake-terminal", vec!["-e"]),
        ] {
            let command = terminal_args(Path::new(name), executable, &args);
            assert_eq!(
                &command[..prefix.len()],
                &prefix.iter().map(OsString::from).collect::<Vec<_>>()
            );
            assert_eq!(command[prefix.len()], executable.as_os_str());
            assert_eq!(&command[prefix.len() + 1..], &args);
            assert!(!command.iter().any(|arg| arg == "-c"));
        }
    }

    #[test]
    fn readiness_requires_identity_and_parent_disconnect_requests_shutdown() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let token = "a".repeat(64);
        let child_token = token.clone();
        let child = thread::spawn(move || connect_child(address, &child_token).unwrap());
        let (mut parent, _) = listener.accept().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut identity = Vec::new();
        while !identity.ends_with(b"\n") {
            let mut byte = [0];
            parent.read_exact(&mut byte).unwrap();
            identity.push(byte[0]);
        }
        assert_eq!(
            String::from_utf8(identity).unwrap(),
            format!("{token} {}\n", std::process::id())
        );
        parent.write_all(b"A").unwrap();
        let session = child.join().unwrap();
        let mut invalid = fixture_snapshot();
        invalid.generation.clear();
        assert!(session.state.send_snapshot(&invalid, true).is_err());
        assert!(!session.state.ready.load(Ordering::Acquire));
        parent.set_nonblocking(true).unwrap();
        assert_eq!(
            parent.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        parent.set_nonblocking(false).unwrap();
        session
            .state
            .send_snapshot(&fixture_snapshot(), true)
            .unwrap();
        let mut ready = [0];
        parent.read_exact(&mut ready).unwrap();
        assert_eq!(ready, *b"R");
        read_snapshot(&mut parent, std::process::id()).unwrap();
        assert!(session.state.request_reload().is_err());
        session.state.prepare_reload();
        // No live worker display is installed in this fixture. A prepared
        // reload must stop sampling it while the old runtime is being removed.
        session
            .state
            .send_current_snapshot("fixture-client", false)
            .unwrap();
        session.state.request_reload().unwrap();
        session.state.request_reload().unwrap();
        assert!(matches!(
            read_child_update(&mut parent, std::process::id()).unwrap(),
            ChildUpdate::Reload
        ));
        parent.set_nonblocking(true).unwrap();
        let mut extra = [0];
        assert_eq!(
            parent.read(&mut extra).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(parent);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.state.stop.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(session.state.stop.load(Ordering::Acquire));
        assert!(session.state.announce_ready("fixture-client").is_err());
        assert!(session.state.request_reload().is_err());
        drop(session);
    }

    #[test]
    fn reload_updates_are_once_per_owned_window_and_stop_or_eof_never_relaunches() {
        let state = || ParentState {
            closed: AtomicBool::new(false),
            reload_requested: AtomicBool::new(false),
            snapshot: Mutex::new(None),
        };
        let old = state();
        old.watch(&mut &b"L\nL\n"[..], std::process::id());
        assert!(old.closed.load(Ordering::Acquire));
        assert!(old.take_reload_request());
        assert!(!old.take_reload_request());
        assert!(!old.receive(ChildUpdate::Reload));
        assert!(!old.take_reload_request());

        let replacement = state();
        assert!(!replacement.take_reload_request());
        replacement.watch(&mut &b""[..], std::process::id());
        assert!(replacement.closed.load(Ordering::Acquire));
        assert!(!replacement.take_reload_request());

        let stopped = state();
        stopped.closed.store(true, Ordering::Release);
        stopped.watch(&mut &b"L\n"[..], std::process::id());
        assert!(!stopped.take_reload_request());
        assert!(read_snapshot(&mut &b"L\n"[..], std::process::id()).is_err());
    }

    #[test]
    fn untrusted_identity_cannot_select_the_parent_or_another_process() {
        for identity in [
            format!("wrong {}\n", 1234),
            format!("secret {}\n", std::process::id()),
            "secret 0\n".into(),
            "x".repeat(81),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let mut sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut receiver, _) = listener.accept().unwrap();
            sender.write_all(identity.as_bytes()).unwrap();
            sender.shutdown(Shutdown::Write).unwrap();
            assert!(read_identity(&mut receiver, "secret").is_err());
        }
    }

    fn fixture_snapshot() -> LinkedSnapshot {
        LinkedSnapshot {
            client_id: "fixture-client".into(),
            process_id: std::process::id(),
            generation: "fixture-pool-generation".into(),
            manager: None,
            total: 3,
            desired_total: 3,
            retiring: 0,
            idle: 2,
            working: 1,
            offline: 0,
            queued: 0,
            registered_workers: 2,
            connected: true,
            closed: false,
            stopping: false,
            manager_operations: Vec::new(),
            last_resize: None,
            activity: crate::worker::Activity::default(),
        }
    }

    #[test]
    fn linked_status_is_bounded_and_binds_the_real_process() {
        let mut bytes = serde_json::to_vec(&fixture_snapshot()).unwrap();
        bytes.push(b'\n');
        let snapshot = read_snapshot(&mut &bytes[..], std::process::id()).unwrap();
        assert_eq!(snapshot.client_id, "fixture-client");
        assert_eq!(snapshot.registered_workers, 2);
        assert!(read_snapshot(&mut &bytes[..], std::process::id() + 1).is_err());
        let mut oversized = io::Cursor::new(vec![b'x'; MAX_STATUS_FRAME_BYTES + 1]);
        assert!(read_child_update(&mut oversized, std::process::id()).is_err());
        assert_eq!(oversized.position(), MAX_STATUS_FRAME_BYTES as u64);
        let mut invalid = fixture_snapshot();
        invalid.working = 9;
        let mut invalid = serde_json::to_vec(&invalid).unwrap();
        invalid.push(b'\n');
        assert!(read_snapshot(&mut &invalid[..], std::process::id()).is_err());
    }

    #[test]
    fn linked_activity_fits_the_existing_frame_even_with_json_escaping_and_resize_errors() {
        let mut status = fixture_snapshot();
        status.total = 128;
        status.desired_total = 128;
        status.working = 128;
        status.idle = 0;
        status.registered_workers = 128;
        status.manager = Some(LinkedManager {
            client_id: status.client_id.clone(),
            thread_id: "m".repeat(128),
        });
        status.activity = serde_json::from_value(serde_json::json!({
            "generation": "g".repeat(100),
            "tasks": (0..8).map(|index| serde_json::json!({
                "task_id": format!("worker_task_{}_{}", "g".repeat(100), index + 1),
                "status": "working", "slot": index, "epoch": 4, "attempt": 1,
                "owner_thread_id": "o".repeat(128),
                "worker_thread_id": "w".repeat(128),
                "summary": "\\\"".repeat(80),
                "progress": "\\\"".repeat(80),
                "last_command": "\\\"".repeat(80),
                "replay_safe": false, "last_log_sequence": 7
            })).collect::<Vec<_>>(),
            "omitted": 3, "truncated": true
        }))
        .unwrap();
        status.last_resize = Some(ResizeReceipt {
            request_id: 7,
            target: 3,
            accepted: false,
            error: Some("\\\"".repeat(256)),
        });
        let bytes = encode_snapshot(&status).unwrap();
        assert!(bytes.len() <= MAX_STATUS_BYTES);
        let decoded = read_snapshot(&mut &bytes[..], std::process::id()).unwrap();
        assert!(!decoded.activity.tasks.is_empty());
        assert!(decoded.activity.tasks.len() < status.activity.tasks.len());
        assert_eq!(decoded.activity.tasks.len() + decoded.activity.omitted, 11);
        assert_eq!(decoded.last_resize.as_ref().unwrap().request_id, 7);
        assert!(decoded.activity.truncated);
        assert_eq!(summary_value(&decoded), summary_value(&status));
        assert!(summary_value(&decoded).unwrap().get("activity").is_none());
        assert!(
            serde_json::to_vec(&summary_value(&decoded).unwrap())
                .unwrap()
                .len()
                < 2048
        );
        assert_eq!(
            decoded.activity.tasks[0].worker_thread_id.as_deref(),
            Some("w".repeat(128).as_str())
        );
        for task in &mut status.activity.tasks {
            task.summary = "中".repeat(53);
            task.progress = Some("中".repeat(53));
            task.last_command = Some("中".repeat(53));
        }
        let unicode = encode_snapshot(&status).unwrap();
        assert!(unicode.len() <= MAX_STATUS_BYTES);
        let decoded = read_snapshot(&mut &unicode[..], std::process::id()).unwrap();
        assert_eq!(summary_value(&decoded), summary_value(&status));
        assert!(decoded.activity.valid());
    }

    #[test]
    fn linked_activity_rejects_forged_control_text_and_unknown_replay_safety() {
        let mut status = fixture_snapshot();
        let task = serde_json::json!({
            "task_id": "worker_task_fixture_1", "status": "unknown",
            "slot": 0, "epoch": 2, "attempt": 1,
            "owner_thread_id": "owner", "worker_thread_id": "real-worker",
            "summary": "task", "progress": null, "last_command": null,
            "replay_safe": false, "last_log_sequence": 3
        });
        status.activity = serde_json::from_value(serde_json::json!({
            "generation": "fixture", "tasks": [task.clone()], "omitted": 2, "truncated": true
        }))
        .unwrap();
        let overview = format_overview(&status);
        assert!(overview.contains("worker_task_fixture_1 · unknown; effects unconfirmed"));
        assert!(overview.contains("Owner: owner"));
        assert!(overview.contains("Claimed by: real-worker · station 1 · attempt 1"));
        assert!(overview.contains("2 more retained tasks omitted"));
        for (field, value) in [
            ("worker_thread_id", serde_json::json!("forged\nthread")),
            ("summary", serde_json::json!("injected\u{1b}[2J")),
            ("replay_safe", serde_json::json!(true)),
            ("slot", serde_json::json!(null)),
            ("epoch", serde_json::json!(null)),
        ] {
            let mut invalid = task.clone();
            invalid[field] = value;
            status.activity = serde_json::from_value(serde_json::json!({
                "generation": "fixture", "tasks": [invalid], "omitted": 0, "truncated": false
            }))
            .unwrap();
            assert!(status_frame(&status, true).is_err());
            let bytes = encode_snapshot(&status).unwrap();
            assert!(read_snapshot(&mut &bytes[..], std::process::id()).is_err());
        }
    }

    #[test]
    fn linked_activity_and_summary_share_the_disconnect_and_three_second_stale_gate() {
        let state = ParentState {
            closed: AtomicBool::new(false),
            reload_requested: AtomicBool::new(false),
            snapshot: Mutex::new(Some((fixture_snapshot(), Instant::now()))),
        };
        assert!(fresh_snapshot(&state).is_some());
        state.closed.store(true, Ordering::Release);
        assert!(fresh_snapshot(&state).is_none());
        state.closed.store(false, Ordering::Release);
        let mut disconnected = fixture_snapshot();
        disconnected.connected = false;
        *state.snapshot.lock().unwrap() = Some((disconnected, Instant::now()));
        assert!(fresh_snapshot(&state).is_none());
        *state.snapshot.lock().unwrap() =
            Some((fixture_snapshot(), Instant::now() - Duration::from_secs(4)));
        assert!(fresh_snapshot(&state).is_none());
    }

    #[test]
    fn linked_status_accepts_zero_and_includes_retiring_slots() {
        let mut status = fixture_snapshot();
        status.total = 0;
        status.desired_total = 0;
        status.idle = 0;
        status.working = 0;
        status.registered_workers = 0;
        let mut bytes = serde_json::to_vec(&status).unwrap();
        bytes.push(b'\n');
        assert_eq!(
            read_snapshot(&mut &bytes[..], std::process::id())
                .unwrap()
                .total,
            0
        );
        status.total = 3;
        status.desired_total = 1;
        status.working = 1;
        status.retiring = 2;
        let mut bytes = serde_json::to_vec(&status).unwrap();
        bytes.push(b'\n');
        let decoded = read_snapshot(&mut &bytes[..], std::process::id()).unwrap();
        assert_eq!(decoded.desired_total, 1);
        assert_eq!(decoded.retiring, 2);
    }

    #[test]
    fn linked_status_exposes_only_the_verified_pool_manager_identity() {
        let mut status = fixture_snapshot();
        status.manager = Some(LinkedManager {
            client_id: status.client_id.clone(),
            thread_id: "agent_verified_manager".into(),
        });
        let mut bytes = serde_json::to_vec(&status).unwrap();
        bytes.push(b'\n');
        let decoded = read_snapshot(&mut &bytes[..], std::process::id()).unwrap();
        let value = serde_json::to_value(decoded).unwrap();
        assert_eq!(value["manager"]["client_id"], "fixture-client");
        assert_eq!(value["manager"]["thread_id"], "agent_verified_manager");
        assert_eq!(value["manager"].as_object().unwrap().len(), 2);
        for (client, thread) in [
            (
                "another-pool".to_owned(),
                "agent_verified_manager".to_owned(),
            ),
            ("fixture-client".to_owned(), String::new()),
            ("fixture-client".to_owned(), "x".repeat(129)),
            ("fixture-client".to_owned(), "agent\nforged".to_owned()),
        ] {
            status.manager = Some(LinkedManager {
                client_id: client,
                thread_id: thread,
            });
            let mut bytes = serde_json::to_vec(&status).unwrap();
            bytes.push(b'\n');
            assert!(read_snapshot(&mut &bytes[..], std::process::id()).is_err());
        }
    }

    fn fixture_manager_snapshot() -> LinkedSnapshot {
        let mut snapshot = fixture_snapshot();
        snapshot.manager = Some(LinkedManager {
            client_id: snapshot.client_id.clone(),
            thread_id: "agent_verified_manager".into(),
        });
        snapshot
    }

    fn task_target(kind: &str, task_id: String) -> LinkedManagerTarget {
        LinkedManagerTarget {
            kind: kind.into(),
            task_id: Some(task_id),
            slot: None,
            epoch: None,
            revision: None,
        }
    }

    fn fixture_wait(operation_id: u64) -> LinkedManagerOperation {
        LinkedManagerOperation {
            operation_id,
            kind: "wait_for".into(),
            state: "waiting".into(),
            mode: Some("all".into()),
            targets: vec![LinkedManagerTarget {
                kind: "worker_idle".into(),
                task_id: None,
                slot: Some(0),
                epoch: Some(1),
                revision: None,
            }],
            task_id: None,
            slot: None,
            epoch: None,
            summary: None,
            started_at_ms: 1000,
            elapsed_ms: 1200,
            wait_ms: Some(5000),
        }
    }

    fn metadata_packet(snapshot: &LinkedSnapshot) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(snapshot).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn manager_status_keeps_every_target_and_light_projection_uses_the_same_freshness() {
        let mut snapshot = fixture_manager_snapshot();
        let mut waiting = fixture_wait(3);
        waiting.targets = (0..128)
            .map(|index| {
                task_target(
                    "task_finished",
                    format!("worker_task_{}_{index}", "x".repeat(144)),
                )
            })
            .collect();
        let submitted = LinkedManagerOperation {
            operation_id: 1,
            kind: "submit".into(),
            state: "submitted".into(),
            mode: None,
            targets: Vec::new(),
            task_id: Some("worker_task_actual".into()),
            slot: None,
            epoch: None,
            summary: Some("Review the actual source".into()),
            started_at_ms: 900,
            elapsed_ms: 0,
            wait_ms: None,
        };
        let assigned = LinkedManagerOperation {
            operation_id: 2,
            kind: "claim".into(),
            state: "assigned".into(),
            slot: Some(1),
            epoch: Some(4),
            ..submitted.clone()
        };
        snapshot.manager_operations = vec![submitted, assigned, waiting];
        snapshot.activity = serde_json::from_value(serde_json::json!({
            "generation": snapshot.generation,
            "tasks": (0..8).map(|index| serde_json::json!({
                "task_id": format!("worker_task_{}_{}", snapshot.generation, index + 1),
                "status": "completed", "slot": 1, "epoch": 4, "attempt": 1,
                "owner_thread_id": "agent_verified_manager", "worker_thread_id": "actual-worker",
                "summary": "\\\"".repeat(80), "progress": "\\\"".repeat(80),
                "last_command": "\\\"".repeat(80), "last_log_sequence": 7,
                "replay_safe": false
            })).collect::<Vec<_>>(),
            "omitted": 0, "truncated": false
        }))
        .unwrap();
        let frame = status_frame(&snapshot, true).unwrap();
        assert!(frame.len() > 4096);
        assert!(frame.len() <= MAX_STATUS_FRAME_BYTES);
        assert_eq!(frame.first(), Some(&b'R'));
        let decoded = read_snapshot(&mut &frame[1..], std::process::id()).unwrap();
        assert_eq!(decoded.generation, snapshot.generation);
        assert_eq!(decoded.manager_operations, snapshot.manager_operations);
        assert!(!decoded.activity.tasks.is_empty());
        assert!(decoded.activity.tasks.len() < snapshot.activity.tasks.len());
        assert_eq!(decoded.activity.tasks.len() + decoded.activity.omitted, 8);
        assert!(decoded.activity.truncated);
        assert!(format_overview(&decoded).contains("actual-worker"));
        let parent = ParentState {
            closed: AtomicBool::new(false),
            reload_requested: AtomicBool::new(false),
            snapshot: Mutex::new(Some((decoded.clone(), Instant::now()))),
        };
        let light = parent.project_snapshot(false).unwrap();
        assert!(light.get("manager_operations").is_none());
        assert!(light.get("activity").is_none());
        assert_eq!(light["generation"], snapshot.generation);
        let projected = serde_json::json!({
            "schema_version":1,"path":"/worker_pool","value":light,
        });
        assert!(serde_json::to_vec(&projected).unwrap().len() < 8 * 1024);
        let complete = parent.project_snapshot(true).unwrap();
        assert_eq!(
            complete["manager_operations"][2]["targets"]
                .as_array()
                .unwrap()
                .len(),
            128
        );
        assert_eq!(complete["manager_operations"][2]["elapsed_ms"], 1200);
        assert_eq!(
            complete["activity"],
            serde_json::to_value(&decoded.activity).unwrap()
        );
        *parent.snapshot.lock().unwrap() =
            Some((decoded.clone(), Instant::now() - Duration::from_secs(4)));
        assert!(parent.project_snapshot(false).is_none());
        assert!(parent.project_snapshot(true).is_none());
        *parent.snapshot.lock().unwrap() = Some((decoded, Instant::now()));
        parent.closed.store(true, Ordering::Release);
        assert!(parent.project_snapshot(false).is_none());
        assert!(parent.project_snapshot(true).is_none());
    }

    #[test]
    fn manager_status_preserves_each_wait_kind_and_its_terminal_outcomes() {
        for (kind, mode, target, states) in [
            (
                "wait_for",
                "any",
                fixture_wait(1).targets.remove(0),
                &[
                    "waiting",
                    "satisfied",
                    "timed_out",
                    "indeterminate",
                    "unavailable",
                    "cancelled",
                    "error",
                ][..],
            ),
            (
                "result",
                "single",
                task_target("task_result", "worker_task_actual".into()),
                &[
                    "waiting",
                    "satisfied",
                    "timed_out",
                    "indeterminate",
                    "cancelled",
                    "error",
                ][..],
            ),
            (
                "wait_available",
                "change",
                LinkedManagerTarget {
                    kind: "availability_change".into(),
                    task_id: None,
                    slot: None,
                    epoch: None,
                    revision: Some(0),
                },
                &[
                    "waiting",
                    "current",
                    "changed",
                    "unchanged",
                    "not_accepting",
                    "cancelled",
                    "error",
                ][..],
            ),
        ] {
            for state in states {
                let mut snapshot = fixture_manager_snapshot();
                let mut operation = fixture_wait(1);
                operation.kind = kind.into();
                operation.mode = Some(mode.into());
                operation.state = (*state).into();
                operation.targets = vec![target.clone()];
                // Scheduling can exceed the requested wait; preserve the observation.
                operation.elapsed_ms = 7000;
                snapshot.manager_operations = vec![operation];
                let frame = status_frame(&snapshot, false).unwrap();
                let ChildUpdate::Snapshot(decoded) =
                    read_child_update(&mut &frame[..], std::process::id()).unwrap()
                else {
                    panic!("a status frame must not become a reload request");
                };
                assert_eq!(decoded.manager_operations, snapshot.manager_operations);
            }
        }
    }

    #[test]
    fn manager_status_rejects_forged_fields_conditions_and_operation_identities() {
        let mut snapshot = fixture_manager_snapshot();
        snapshot.manager_operations = vec![fixture_wait(1)];
        let original = serde_json::to_value(snapshot).unwrap();
        for (pointer, replacement) in [
            ("/generation", serde_json::json!("")),
            ("/generation", serde_json::json!("pool\nforged")),
            ("/generation", serde_json::json!("x".repeat(129))),
            ("/manager", serde_json::Value::Null),
            ("/queued", serde_json::json!(129)),
            ("/registered_workers", serde_json::json!(4)),
            ("/manager_operations/0/operation_id", serde_json::json!(0)),
            ("/manager_operations/0/kind", serde_json::json!("unknown")),
            (
                "/manager_operations/0/state",
                serde_json::json!("completed"),
            ),
            ("/manager_operations/0/mode", serde_json::json!("first")),
            ("/manager_operations/0/wait_ms", serde_json::json!(5001)),
            ("/manager_operations/0/elapsed_ms", serde_json::json!(-1)),
            ("/manager_operations/0/targets", serde_json::json!([])),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"worker_idle","slot":128,"epoch":1}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"worker_idle","slot":0,"epoch":0}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"worker_idle","slot":0,"epoch":1,"revision":2}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"task_finished","task_id":"task\nforged"}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"task_finished","task_id":"x".repeat(257)}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"task_result","task_id":"task_actual"}),
            ),
            (
                "/manager_operations/0/targets/0",
                serde_json::json!({"kind":"worker_idle","slot":0,"epoch":1,"unexpected":true}),
            ),
            (
                "/manager_operations/0/task_id",
                serde_json::json!("task_unrelated"),
            ),
            (
                "/manager_operations/0/summary",
                serde_json::json!("unrelated prompt"),
            ),
        ] {
            let mut invalid = original.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            let mut bytes = serde_json::to_vec(&invalid).unwrap();
            bytes.push(b'\n');
            assert!(
                read_child_update(&mut &bytes[..], std::process::id()).is_err(),
                "accepted invalid field {pointer}"
            );
        }
        let mut snapshot = fixture_manager_snapshot();
        for ids in [[1, 1], [2, 1]] {
            snapshot.manager_operations = ids.into_iter().map(fixture_wait).collect();
            assert!(status_frame(&snapshot, false).is_err());
        }
        snapshot.manager_operations = (1..=17).map(fixture_wait).collect();
        assert!(status_frame(&snapshot, false).is_err());
        snapshot.manager_operations = (1..=65)
            .map(|id| LinkedManagerOperation {
                state: "cancelled".into(),
                ..fixture_wait(id)
            })
            .collect();
        assert!(status_frame(&snapshot, false).is_err());
        snapshot.manager_operations = vec![fixture_wait(1), fixture_wait(2)];
        snapshot.manager_operations[0].targets = vec![fixture_wait(1).targets.remove(0); 128];
        assert!(status_frame(&snapshot, false).is_err());
        let bytes = metadata_packet(&snapshot);
        assert!(read_child_update(&mut &bytes[..], std::process::id()).is_err());
    }

    #[test]
    fn manager_metadata_budget_counts_serialized_escaping_without_truncating_targets() {
        let mut snapshot = fixture_manager_snapshot();
        let mut operation = fixture_wait(1);
        operation.targets = (0..128)
            .map(|index| task_target("task_finished", format!("{}_{index}", "x".repeat(200))))
            .collect();
        snapshot.manager_operations = vec![operation];
        assert!(status_frame(&snapshot, false).is_ok());
        for target in &mut snapshot.manager_operations[0].targets {
            target.task_id = target.task_id.as_ref().map(|id| id.replace('x', "\""));
        }
        assert!(
            serde_json::to_vec(&snapshot.manager_operations)
                .unwrap()
                .len()
                > MAX_MANAGER_METADATA_BYTES
        );
        assert!(status_frame(&snapshot, true).is_err());
        let bytes = metadata_packet(&snapshot);
        assert!(read_child_update(&mut &bytes[..], std::process::id()).is_err());
        assert_eq!(snapshot.manager_operations[0].targets.len(), 128);
    }

    #[test]
    fn resize_commands_are_fixed_size_and_validate_zero_and_maximum() {
        for target in [0, 1, 3, 128] {
            let mut decoder = ControlDecoder::default();
            let encoded = resize_command(42, target).unwrap();
            for &byte in &encoded[..10] {
                assert_eq!(decoder.push(byte).unwrap(), None);
            }
            assert_eq!(
                decoder.push(encoded[10]).unwrap(),
                Some(ControlCommand::Resize(ResizeRequest {
                    request_id: 42,
                    target
                }))
            );
            assert_eq!(decoder.push(b'S').unwrap(), Some(ControlCommand::Stop));
        }
        assert!(resize_command(1, 129).is_err());
        assert!(resize_command(1, usize::MAX).is_err());
        assert!(resize_command(0, 0).is_err());
        let mut decoder = ControlDecoder::default();
        assert!(decoder.push(b'?').is_err());
        let mut invalid = resize_command(1, 0).unwrap();
        invalid[2] = 129;
        for &byte in &invalid[..10] {
            assert_eq!(decoder.push(byte).unwrap(), None);
        }
        assert!(decoder.push(invalid[10]).is_err());
    }

    #[test]
    fn resize_channel_correlates_rejections_and_success_for_the_same_target() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let connecting = thread::spawn(move || connect_child(address, "resize-test"));
        let (mut parent, _) = listener.accept().unwrap();
        parent
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0];
        while byte != *b"\n" {
            parent.read_exact(&mut byte).unwrap();
        }
        parent.write_all(b"A").unwrap();
        let session = connecting.join().unwrap().unwrap();
        let first = resize_command(1, 3).unwrap();
        parent.write_all(&first[..1]).unwrap();
        parent.write_all(&first[1..]).unwrap();
        parent.write_all(&resize_command(2, 0).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if session.state.resize.lock().unwrap().pending
                == Some(ResizeRequest {
                    request_id: 2,
                    target: 0,
                })
            {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!session.state.stop.load(Ordering::Acquire));
        assert_eq!(session.state.take_resize(), Some(0));
        assert_eq!(session.state.take_resize(), None);
        assert!(
            session
                .state
                .report_resize(1, Ok(serde_json::Value::Null))
                .is_err()
        );
        session
            .state
            .report_resize(0, Err("busy\n退役中".repeat(200)))
            .unwrap();
        session
            .state
            .send_snapshot(&fixture_snapshot(), false)
            .unwrap();
        let receipt = read_snapshot(&mut parent, std::process::id())
            .unwrap()
            .last_resize
            .unwrap();
        assert_eq!(receipt.request_id, 2);
        assert_eq!(receipt.target, 0);
        assert!(!receipt.accepted);
        assert!(receipt.error.as_ref().unwrap().len() <= 512);
        assert!(!receipt.error.unwrap().chars().any(char::is_control));
        parent.write_all(&resize_command(3, 0).unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(total) = session.state.take_resize() {
                assert_eq!(total, 0);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        session
            .state
            .report_resize(0, Ok(serde_json::Value::Null))
            .unwrap();
        session
            .state
            .send_snapshot(&fixture_snapshot(), false)
            .unwrap();
        let receipt = read_snapshot(&mut parent, std::process::id())
            .unwrap()
            .last_resize
            .unwrap();
        assert_eq!(receipt.request_id, 3);
        assert!(receipt.accepted);
        assert!(receipt.error.is_none());
        parent.write_all(b"S").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session.state.stop.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        drop(session);
    }
}
