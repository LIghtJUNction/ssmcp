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
static CHILD_SESSION: OnceLock<Weak<ChildState>> = OnceLock::new();
static LINKED: OnceLock<Mutex<Weak<ParentState>>> = OnceLock::new();

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedManager {
    client_id: String,
    thread_id: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LinkedSnapshot {
    client_id: String,
    process_id: u32,
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
    #[serde(default)]
    last_resize: Option<ResizeReceipt>,
}

struct ParentState {
    closed: AtomicBool,
    reload_requested: AtomicBool,
    snapshot: Mutex<Option<(LinkedSnapshot, Instant)>>,
}

enum ChildUpdate {
    Snapshot(LinkedSnapshot),
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
                    .unwrap_or_else(|error| error.into_inner()) = Some((snapshot, Instant::now()));
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
    let state = LINKED
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .upgrade()?;
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
    serde_json::to_value(snapshot).ok()
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
    let token: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
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
        if first && !self.ready.load(Ordering::Acquire) {
            stream.write_all(b"R")?;
        }
        serde_json::to_writer(&mut *stream, snapshot).map_err(io::Error::other)?;
        stream.write_all(b"\n")?;
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
    let snapshot = crate::worker::display()?;
    let steering = super::steering::snapshot();
    Some(LinkedSnapshot {
        client_id: client_id.to_owned(),
        process_id: std::process::id(),
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
        last_resize: None,
    })
}

fn read_snapshot(reader: &mut impl Read, pid: u32) -> io::Result<LinkedSnapshot> {
    match read_child_update(reader, pid)? {
        ChildUpdate::Snapshot(snapshot) => Ok(snapshot),
        ChildUpdate::Reload => Err(io::Error::other("worker requested reload before readiness")),
    }
}

fn read_child_update(reader: &mut impl Read, pid: u32) -> io::Result<ChildUpdate> {
    let mut line = Vec::with_capacity(512);
    for _ in 0..4096 {
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        if byte == *b"\n" {
            if line == b"L" {
                return Ok(ChildUpdate::Reload);
            }
            let snapshot: LinkedSnapshot =
                serde_json::from_slice(&line).map_err(io::Error::other)?;
            let counts = snapshot
                .idle
                .checked_add(snapshot.working)
                .and_then(|n| n.checked_add(snapshot.offline))
                .and_then(|n| n.checked_add(snapshot.retiring));
            if snapshot.process_id != pid
                || snapshot.client_id.is_empty()
                || snapshot.client_id.len() > 128
                || snapshot.client_id.chars().any(char::is_control)
                || snapshot.manager.as_ref().is_some_and(|manager| {
                    manager.client_id != snapshot.client_id
                        || manager.thread_id.is_empty()
                        || manager.thread_id.len() > 128
                        || manager.thread_id.chars().any(char::is_control)
                })
                || snapshot.total > 128
                || snapshot.desired_total > 128
                || counts != Some(snapshot.total)
                || snapshot.last_resize.as_ref().is_some_and(|receipt| {
                    receipt.request_id == 0
                        || receipt.target > 128
                        || receipt.accepted == receipt.error.is_some()
                        || receipt.error.as_ref().is_some_and(|error| {
                            error.len() > 512 || error.chars().any(char::is_control)
                        })
                })
            {
                return Err(io::Error::other("invalid worker window status"));
            }
            return Ok(ChildUpdate::Snapshot(snapshot));
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
        if !metadata.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
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
            last_resize: None,
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
        assert!(read_snapshot(&mut &vec![b'x'; 4097][..], std::process::id()).is_err());
        let mut invalid = fixture_snapshot();
        invalid.working = 9;
        let mut invalid = serde_json::to_vec(&invalid).unwrap();
        invalid.push(b'\n');
        assert!(read_snapshot(&mut &invalid[..], std::process::id()).is_err());
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
