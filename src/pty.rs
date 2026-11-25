//! A single Linux pseudo-terminal with bounded, cursor-addressed output.
use crate::{
    command_runtime::{
        DeadlineError, Driver, JoinHandle, open_pty, resize_pty, timeout, timeout_at,
    },
    output::{Capture, INLINE_BYTES, INLINE_LINES, OutputStore},
    wire::Operation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::Path,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use zrun::{
    io::{AsyncFd, FdInterest, Nonblocking},
    process::Child,
};

const OUTPUT_CAPACITY: usize = 1024 * 1024;

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    eof: bool,
    capture: Option<Capture>,
}

impl Output {
    fn push(&mut self, data: &[u8]) {
        if let Some(capture) = &mut self.capture {
            // A failed spool remains explicit in every subsequent read/status;
            // the bounded terminal tail continues to work for interactive use.
            let _ = capture.push(data);
        }
        let retained = &data[data.len().saturating_sub(OUTPUT_CAPACITY)..];
        let excess = (self.bytes.len() + retained.len()).saturating_sub(OUTPUT_CAPACITY);
        self.bytes.drain(..excess);
        self.bytes.extend(retained);
        self.end += data.len() as u64;
        self.start = self.end - self.bytes.len() as u64;
    }
}

pub(crate) type OutputEvent = Arc<dyn Fn(u64, &[u8]) + Send + Sync>;

pub(crate) struct JobConfig<'a> {
    pub command: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a HashMap<String, String>,
    pub id: &'a str,
    pub size: Option<(u16, u16)>,
    pub live: bool,
    pub output_event: Option<OutputEvent>,
    pub memory: Option<&'a crate::backend::command_memory::CommandMemory>,
}

pub struct Terminal {
    master: Mutex<Option<Arc<File>>>,
    child: Mutex<Option<Child>>,
    leader: OwnedFd,
    pid: libc::pid_t,
    output: Mutex<Output>,
    changed: Arc<Notify>,
    input_bytes: AtomicU64,
    output_event: Option<OutputEvent>,
    writer: AsyncMutex<Option<AsyncFd<File>>>,
    reader: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
    live: bool,
    display_id: String,
    observer: Option<std::sync::Weak<crate::observer::Pool>>,
    observer_cwd: std::path::PathBuf,
}

impl Terminal {
    pub fn spawn(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, false, OutputStore::global()?)
    }

    pub fn spawn_live(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true, OutputStore::global()?)
    }

    pub fn spawn_live_with_store(
        shell: &Path,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true, output)
    }

    fn spawn_with_logging(
        shell: &Path,
        live: bool,
        output_store: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_configured(shell, None, live, output_store, "main")
    }

    pub fn spawn_named_with_store(
        shell: &Path,
        cwd: &Path,
        output: Arc<OutputStore>,
        terminal_id: &str,
    ) -> crate::Result<Arc<Self>> {
        Self::spawn_configured(shell, Some(cwd), true, output, terminal_id)
    }

    fn spawn_configured(
        shell: &Path,
        cwd: Option<&Path>,
        live: bool,
        output_store: Arc<OutputStore>,
        display_id: &str,
    ) -> crate::Result<Arc<Self>> {
        if !shell.is_absolute() {
            return Err("shell must be an absolute path".into());
        }
        let cwd = cwd
            .map(Path::to_path_buf)
            .unwrap_or(std::env::current_dir()?);
        let command = vec![shell.to_string_lossy().into_owned(), "-i".into()];
        Self::spawn_job(
            JobConfig {
                command: &command,
                cwd: &cwd,
                env: &HashMap::new(),
                id: display_id,
                size: None,
                live,
                output_event: None,
                memory: None,
            },
            output_store,
        )
    }

    pub(crate) fn spawn_job(
        config: JobConfig<'_>,
        output_store: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        let JobConfig {
            command: argv,
            cwd,
            env,
            id: display_id,
            size,
            live,
            output_event,
            memory,
        } = config;
        if argv.is_empty() || argv[0].is_empty() {
            return Err("PTY command must not be empty".into());
        }
        let (rows, cols) = size.unwrap_or_else(|| {
            if live {
                crate::client::terminal::pane_size(24, 80)
            } else {
                (24, 80)
            }
        });
        let driver = Driver::get()?;
        let (master, slave) = open_pty(rows, cols)?;
        let mut reader = AsyncFd::new(&driver.io, master.try_clone()?, Nonblocking::Set)?;
        let writer = AsyncFd::with_interest(
            &driver.io,
            master.try_clone()?,
            Nonblocking::Set,
            FdInterest::Writable,
        )?;
        let mut command = Command::new(&argv[0]);
        command
            .args(&argv[1..])
            .env("TERM", "xterm-256color")
            .envs(env)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        command.current_dir(cwd);
        // SAFETY: this closure runs after fork and only calls async-signal-safe syscalls.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        if let Some(memory) = memory {
            memory.attach(&mut command)?;
        }
        let mut child = zrun::process::spawn(&mut command, &driver.io)?;
        let pid = child.id() as libc::pid_t;
        let leader = match open_pidfd(pid) {
            Ok(leader) => leader,
            Err(error) => {
                // The still-owned child can be killed safely before reaping.
                let _ = child.start_kill();
                return Err(format!(
                    "terminal cleanup requires Linux 5.3+ with pidfd support: {error}"
                )
                .into());
            }
        };
        let terminal = Arc::new(Self {
            master: Mutex::new(Some(Arc::new(master))),
            child: Mutex::new(Some(child)),
            leader,
            pid,
            output: Mutex::new(Output {
                capture: Some(output_store.capture("terminal_session")),
                ..Output::default()
            }),
            changed: Arc::new(Notify::new()),
            input_bytes: AtomicU64::new(0),
            output_event,
            writer: AsyncMutex::new(Some(writer)),
            reader: Mutex::new(None),
            closed: AtomicBool::new(false),
            live,
            display_id: display_id.into(),
            observer: output_store.observer().map(|pool| Arc::downgrade(&pool)),
            observer_cwd: cwd.to_path_buf(),
        });
        let weak = Arc::downgrade(&terminal);
        let reader = driver.spawn(async move {
            let mut data = [0_u8; 8192];
            loop {
                let result = reader.read(&mut data).await;
                let Some(terminal) = weak.upgrade() else {
                    break;
                };
                match result {
                    Ok(n) if n > 0 => {
                        let offset = {
                            let mut output = terminal.output.lock().unwrap();
                            let offset = output.end;
                            output.push(&data[..n]);
                            offset
                        };
                        if let Some(event) = &terminal.output_event {
                            event(offset, &data[..n]);
                        }
                        if let Some(pool) = terminal
                            .observer
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                        {
                            pool.terminal_output(&terminal.display_id, &data[..n]);
                        }
                        if terminal.live {
                            crate::client::render::terminal_output_for(
                                &terminal.display_id,
                                &data[..n],
                            );
                        }
                    }
                    result => {
                        if let Some(pool) = terminal
                            .observer
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade)
                        {
                            pool.terminal_closed(&terminal.display_id);
                        }
                        // Linux PTYs report EIO when the last slave closes.
                        let mut output = terminal.output.lock().unwrap();
                        output.eof = true;
                        if let Some(capture) = &mut output.capture {
                            if let Err(error) = result
                                && error.raw_os_error() != Some(libc::EIO)
                            {
                                capture.fail(format!("terminal read failed: {error}"));
                            }
                            let _ = capture.finish();
                        }
                        drop(output);
                        terminal.changed.notify_waiters();
                        break;
                    }
                }
                terminal.changed.notify_waiters();
                zrun::yield_now().await;
            }
        })?;
        *terminal.reader.lock().unwrap() = Some(reader);
        Ok(terminal)
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::Read {
                cursor,
                max_bytes,
                wait_ms,
            } => self.read(cursor, max_bytes, wait_ms).await,
            Operation::Write { data } => {
                if !self.running() {
                    return Err("terminal has exited".into());
                }
                let (written, timed_out, error) = self.write_counted(data.as_bytes(), 5000).await;
                Ok(json!({"written":written,"requested_bytes":data.len(),
                    "remaining_bytes":data.len()-written,"timed_out":timed_out,"error":error}))
            }
            Operation::Resize { rows, cols } => {
                if !self.running() {
                    return Err("terminal has exited".into());
                }
                let master = self.master()?;
                resize_pty(&master, rows, cols).map_err(|error| error.to_string())?;
                Ok(json!({"rows":rows,"cols":cols}))
            }
            Operation::PeerPort { .. }
            | Operation::ListTools { .. }
            | Operation::CallTool { .. } => Err("not a terminal operation".into()),
            Operation::Status {} => {
                let running = self.running();
                let output = self.output.lock().unwrap();
                let mut result = json!({"running":running,"pid":self.pid,"start_cursor":output.start,"next_cursor":output.end,"output_eof":output.eof});
                if let Some(capture) = &output.capture {
                    if let Some(artifact) = capture.metadata() {
                        result["output_artifact"] = json!(artifact);
                    }
                    if let Some(error) = capture.error() {
                        result["output_error"] = json!(error);
                    }
                }
                Ok(result)
            }
        }
    }

    fn master(&self) -> Result<Arc<File>, String> {
        self.master
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "terminal is closed".into())
    }

    fn running(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && self
                .child
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|child| matches!(child.try_exit(), Ok(None)))
    }

    async fn read(&self, cursor: u64, max_bytes: usize, wait_ms: u64) -> Result<Value, String> {
        self.read_impl(cursor, max_bytes, wait_ms, true).await
    }

    pub(crate) async fn read_page(
        &self,
        cursor: u64,
        max_bytes: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        self.read_impl(cursor, max_bytes, wait_ms, false).await
    }

    async fn read_impl(
        &self,
        cursor: u64,
        max_bytes: usize,
        wait_ms: u64,
        spill: bool,
    ) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = {
                let output = self.output.lock().unwrap();
                if cursor > output.end {
                    return Err("cursor is ahead of terminal output".into());
                }
                if cursor < output.end
                    || output.eof
                    || Instant::now() >= deadline
                    || self.closed.load(Ordering::Acquire)
                {
                    let from = cursor.max(output.start);
                    let pending_lines = output
                        .bytes
                        .iter()
                        .skip((from - output.start) as usize)
                        .filter(|byte| **byte == b'\n')
                        .take(INLINE_LINES as usize + 1)
                        .count() as u64
                        + u64::from(
                            output.end > from
                                && output.bytes.back().is_some_and(|byte| *byte != b'\n'),
                        );
                    let long =
                        output.end - cursor > INLINE_BYTES as u64 || pending_lines > INLINE_LINES;
                    if spill && long {
                        let capture = output
                            .capture
                            .as_ref()
                            .ok_or("terminal output capture unavailable")?;
                        let artifact = capture.metadata().ok_or_else(|| {
                            capture
                                .error()
                                .unwrap_or("terminal output could not be spilled")
                                .to_owned()
                        })?;
                        let mut result = json!({"text":"","data_base64":"","next_cursor":output.end,
                            "truncated":capture.error().is_some(),"output_eof":output.eof,"spilled":true,
                            "range":{"start":cursor,"end":output.end,"stored_end":artifact.bytes.min(output.end)},
                            "artifact":artifact});
                        if let Some(error) = capture.error() {
                            result["output_error"] = json!(error);
                        }
                        Some(result)
                    } else {
                        let bytes: Vec<u8> = output
                            .bytes
                            .iter()
                            .skip((from - output.start) as usize)
                            .take(max_bytes)
                            .copied()
                            .collect();
                        let mut result = json!({"text":String::from_utf8_lossy(&bytes),"data_base64":STANDARD.encode(&bytes),
                            "next_cursor":from + bytes.len() as u64,"truncated":cursor < output.start,"output_eof":output.eof});
                        if let Some(error) = output.capture.as_ref().and_then(Capture::error) {
                            result["output_error"] = json!(error);
                        }
                        Some(result)
                    }
                } else {
                    None
                }
            };
            if let Some(mut snapshot) = snapshot {
                snapshot["running"] = json!(self.running());
                return Ok(snapshot);
            }
            let _ = timeout_at(deadline, changed).await;
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid as u32
    }

    pub(crate) fn changed(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    pub(crate) fn input_bytes(&self) -> u64 {
        self.input_bytes.load(Ordering::Acquire)
    }

    pub(crate) fn output_snapshot(&self) -> Value {
        let output = self.output.lock().unwrap();
        json!({"start_cursor":output.start,"next_cursor":output.end,
            "truncated":output.start>0,"eof":output.eof,
            "artifact":output.capture.as_ref().and_then(Capture::metadata),
            "error":output.capture.as_ref().and_then(Capture::error)})
    }

    /// Observe the leader without reaping it: its PID/session identity must remain
    /// pinned until shutdown has signalled all owned members.
    pub(crate) async fn wait_exit(&self) -> io::Result<(Option<i32>, Option<i32>)> {
        let driver = Driver::get()?;
        let mut leader = AsyncFd::new(
            &driver.io,
            self.leader.try_clone()?,
            Nonblocking::ReadinessOnly,
        )?;
        let status = leader
            .readable(|_| {
                let child = self.child.lock().unwrap();
                child
                    .as_ref()
                    .ok_or_else(|| io::Error::other("terminal is closed"))?
                    .try_exit()?
                    .ok_or_else(|| io::ErrorKind::WouldBlock.into())
            })
            .await?;
        Ok((status.code(), status.signal()))
    }

    /// Every successful nonblocking syscall is counted before another await.
    /// Dropping or timing out this future never leaves a queued background write.
    pub(crate) async fn write_counted(
        &self,
        data: &[u8],
        wait_ms: u64,
    ) -> (usize, bool, Option<String>) {
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        if !self.running() {
            return (0, false, Some("terminal has exited".into()));
        }
        if let Some(pool) = self.observer.as_ref().and_then(std::sync::Weak::upgrade) {
            match timeout_at(
                deadline,
                pool.terminal_begin(&self.display_id, &self.observer_cwd),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return (0, false, Some(error)),
                Err(DeadlineError::Elapsed) => return (0, true, None),
                Err(DeadlineError::Reactor(error)) => return (0, false, Some(error.to_string())),
            }
        }
        let mut writer = match timeout_at(deadline, self.writer.lock()).await {
            Ok(guard) => guard,
            Err(DeadlineError::Elapsed) => return (0, true, None),
            Err(DeadlineError::Reactor(error)) => return (0, false, Some(error.to_string())),
        };
        let Some(master) = writer.as_mut() else {
            return (0, false, Some("terminal is closed".into()));
        };
        let mut written = 0;
        while written < data.len() {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.closed.load(Ordering::Acquire) {
                return (written, false, Some("terminal is closed".into()));
            }
            if Instant::now() >= deadline {
                return (written, true, None);
            }
            let result = tokio::select! {
                biased;
                _ = &mut changed => continue,
                result = timeout_at(deadline, master.write(&data[written..])) => result,
            };
            match result {
                Ok(Ok(0)) => return (written, false, Some("terminal accepted zero bytes".into())),
                Ok(Ok(count)) => {
                    written += count;
                    self.input_bytes.fetch_add(count as u64, Ordering::AcqRel);
                }
                Ok(Err(error)) | Err(DeadlineError::Reactor(error)) => {
                    return (written, false, Some(error.to_string()));
                }
                Err(DeadlineError::Elapsed) => return (written, true, None),
            }
        }
        (written, false, None)
    }

    pub async fn shutdown(&self) {
        if let Some(pool) = self.observer.as_ref().and_then(std::sync::Weak::upgrade) {
            pool.terminal_closed(&self.display_id);
        }
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.changed.notify_waiters();
        let child = self.child.lock().unwrap().take();
        if let Some(mut child) = child {
            self.stop_session();
            let _ = child.start_kill();
            if let Ok(driver) = Driver::get() {
                let _ = child.wait(&driver.timer).await;
            }
        }
        self.writer.lock().await.take();
        let reader = self.reader.lock().unwrap().take();
        if let Some(mut reader) = reader
            && timeout(Duration::from_secs(2), &mut reader).await.is_err()
        {
            reader.abort();
            let _ = reader.await;
        }
        self.master.lock().unwrap().take();
        let mut output = self.output.lock().unwrap();
        if !output.eof
            && let Some(capture) = &mut output.capture
        {
            capture.fail("terminal was stopped before output EOF; capture may be incomplete");
            let _ = capture.finish();
        }
        output.eof = true;
        drop(output);
        self.changed.notify_waiters();
    }

    fn stop_session(&self) {
        stop_session(self.pid, &self.leader);
    }
}

pub(crate) fn stop_session(session: libc::pid_t, leader: &OwnedFd) {
    // Procfs names belong to its mount's PID namespace; pidfd_open uses ours.
    // Bind the recorded leader first, otherwise proc ancestry could describe a
    // different process with the same number in an ancestor namespace.
    if !pidfd_matches_proc(leader, session) {
        eprintln!(
            "ssmcp: cannot bind process {session} to mounted procfs; cleanup is limited to its recorded pidfd"
        );
        let _ = signal_pidfd(leader, libc::SIGKILL);
        return;
    }
    // Shell jobs can have their own process groups, and native coding agents can
    // create child sessions. Include both this session and its still-owned process
    // descendants, then signal pidfds rather than reusable numeric process IDs.
    // Already reparented, detached daemons require native task cleanup; this is not
    // a security sandbox or a replacement for a cgroup supervisor.
    let mut members = Vec::new();
    let mut unbound = 0;
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            if pid == session || !owned_process(pid, session) {
                continue;
            }
            if let Ok(fd) = open_pidfd(pid) {
                // Revalidate ownership and bind this pidfd to the same procfs
                // entry before delivering any signal.
                if owned_process(pid, session) {
                    if pidfd_matches_proc(&fd, pid) {
                        members.push(fd);
                    } else {
                        unbound += 1;
                    }
                }
            }
        }
    }
    if unbound != 0 {
        eprintln!(
            "ssmcp: skipped {unbound} process entries with unverified procfs identities during cleanup of {session}"
        );
    }
    for member in members {
        let _ = signal_pidfd(&member, libc::SIGKILL);
    }
    let _ = signal_pidfd(leader, libc::SIGKILL);
}

fn pidfd_matches_proc(fd: &OwnedFd, pid: libc::pid_t) -> bool {
    if pid <= 0 {
        return false;
    }
    // Linux reports Pid here in the PID namespace of this procfs instance.
    // A dead/invisible process is -1/0 and cannot authorize a proc-derived kill.
    let Ok(info) = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())) else {
        return false;
    };
    info.lines().find_map(|line| {
        line.strip_prefix("Pid:")?
            .trim()
            .parse::<libc::pid_t>()
            .ok()
    }) == Some(pid)
}

fn owned_process(pid: libc::pid_t, leader: libc::pid_t) -> bool {
    if process_session(pid) == Some(leader) {
        return true;
    }
    // Validate every observed edge again after reaching the unreaped owner. This
    // rejects a process that was reparented or a PID that was reused during lookup.
    let mut ancestry = Vec::new();
    let mut current = pid;
    for _ in 0..256 {
        if current <= 1 || ancestry.iter().any(|(p, _)| *p == current) {
            return false;
        }
        let Some(identity) = process_parent_identity(current) else {
            return false;
        };
        ancestry.push((current, identity));
        if identity.0 == leader {
            return ancestry
                .iter()
                .all(|(p, expected)| process_parent_identity(*p) == Some(*expected));
        }
        current = identity.0;
    }
    false
}

fn process_parent_identity(pid: libc::pid_t) -> Option<(libc::pid_t, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(19)?.parse().ok()?))
}

fn process_session(pid: libc::pid_t) -> Option<libc::pid_t> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(3)?
        .parse()
        .ok()
}

pub(crate) fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open takes only integer arguments and returns a fresh owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a successful pidfd_open returned this newly owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn signal_pidfd(fd: &OwnedFd, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: fd is an owned pidfd; null siginfo requests normal signal delivery.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.get_mut().unwrap().take() {
            self.stop_session();
            let _ = child.start_kill();
        }
        if let Some(reader) = self.reader.get_mut().unwrap().take() {
            reader.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn collect_until(terminal: &Terminal, cursor: &mut u64, needle: &str) -> String {
        let mut text = String::new();
        timeout(Duration::from_secs(5), async {
            loop {
                let result = terminal
                    .perform(Operation::Read {
                        cursor: *cursor,
                        max_bytes: 32768,
                        wait_ms: 500,
                    })
                    .await
                    .unwrap();
                *cursor = result["next_cursor"].as_u64().unwrap();
                text.push_str(result["text"].as_str().unwrap());
                if text.contains(needle) {
                    return;
                }
            }
        })
        .await
        .expect("expected PTY output");
        text
    }

    #[tokio::test]
    async fn terminal_is_interactive_resizes_and_interrupts_jobs() {
        let terminal = Terminal::spawn(Path::new("/bin/sh")).unwrap();
        let mut cursor = 0;
        terminal
            .perform(Operation::Write {
                data: "stty -echo; test -t 0 && printf 'PTY_%s\\n' OK\n".into(),
            })
            .await
            .unwrap();
        collect_until(&terminal, &mut cursor, "PTY_OK").await;
        terminal
            .perform(Operation::Resize {
                rows: 41,
                cols: 111,
            })
            .await
            .unwrap();
        terminal
            .perform(Operation::Write {
                data: "stty size\n".into(),
            })
            .await
            .unwrap();
        collect_until(&terminal, &mut cursor, "41 111").await;
        terminal
            .perform(Operation::Write {
                data: "sleep 30\n".into(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        terminal
            .perform(Operation::Write {
                data: "\u{3}".into(),
            })
            .await
            .unwrap();
        terminal
            .perform(Operation::Write {
                data: "printf 'AFTER_%s\\n' INT\n".into(),
            })
            .await
            .unwrap();
        collect_until(&terminal, &mut cursor, "AFTER_INT").await;
        let pid = terminal.pid;
        terminal.shutdown().await;
        assert!(!terminal.running());
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(
            terminal
                .perform(Operation::Write {
                    data: "echo no\n".into()
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reads_do_not_block_writes_and_shutdown_stops_foreground_job() {
        let terminal = Terminal::spawn(Path::new("/bin/sh")).unwrap();
        let first = terminal
            .perform(Operation::Read {
                cursor: 0,
                max_bytes: 32768,
                wait_ms: 50,
            })
            .await
            .unwrap();
        let cursor = first["next_cursor"].as_u64().unwrap();
        let reader_terminal = terminal.clone();
        let reader = tokio::spawn(async move {
            reader_terminal
                .perform(Operation::Read {
                    cursor,
                    max_bytes: 32768,
                    wait_ms: 5000,
                })
                .await
        });
        terminal
            .perform(Operation::Write {
                data: "sleep 30\n".into(),
            })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), reader)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let master = terminal.master().unwrap();
        let foreground = timeout(Duration::from_secs(2), async {
            loop {
                // SAFETY: master is a live PTY.
                let foreground = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
                if foreground > 0 && foreground != terminal.pid {
                    break foreground;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        terminal.shutdown().await;
        assert_stopped(foreground).await;
    }

    async fn assert_stopped(pid: libc::pid_t) {
        // Grandchildren may briefly be zombies awaiting their system reaper.
        timeout(Duration::from_secs(2), async {
            loop {
                let state =
                    std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                if state.is_empty()
                    || state
                        .rsplit_once(')')
                        .is_some_and(|(_, tail)| tail.trim_start().starts_with('Z'))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("terminal job remained alive after shutdown");
    }

    #[tokio::test]
    async fn shutdown_stops_background_jobs_in_separate_process_groups() {
        for shell in ["/bin/sh", "/bin/bash"] {
            if !Path::new(shell).exists() {
                continue;
            }
            let terminal = Terminal::spawn(Path::new(shell)).unwrap();
            let mut cursor = 0;
            terminal
                .perform(Operation::Write {
                    data: "stty -echo; printf 'READY_%s\\n' OK\n".into(),
                })
                .await
                .unwrap();
            collect_until(&terminal, &mut cursor, "READY_OK").await;
            terminal
                .perform(Operation::Write {
                    data: "sleep 30 & printf 'BG_%s_END\\n' \"$!\"\n".into(),
                })
                .await
                .unwrap();
            let text = collect_until(&terminal, &mut cursor, "_END").await;
            let pid = text
                .split("BG_")
                .find_map(|part| {
                    part.split_once("_END")
                        .and_then(|(id, _)| id.parse::<libc::pid_t>().ok())
                })
                .expect("background PID");
            assert_eq!(process_session(pid), Some(terminal.pid));
            terminal.shutdown().await;
            assert_stopped(pid).await;
        }
    }

    #[test]
    fn pidfd_binding_uses_the_mounted_procfs_namespace() {
        let fd = open_pidfd(std::process::id() as libc::pid_t).unwrap();
        let proc_pid: libc::pid_t = std::fs::read_link("/proc/self")
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(pidfd_matches_proc(&fd, proc_pid));
        assert!(!pidfd_matches_proc(&fd, if proc_pid == 1 { 2 } else { 1 }));
        assert!(!pidfd_matches_proc(&fd, 0));
        assert!(!pidfd_matches_proc(&fd, -1));
    }

    #[tokio::test]
    async fn mismatched_proc_identity_cannot_kill_another_owned_session() {
        use tokio::io::{AsyncBufReadExt, BufReader};

        struct KillFixture(OwnedFd);
        impl Drop for KillFixture {
            fn drop(&mut self) {
                let _ = signal_pidfd(&self.0, libc::SIGKILL);
            }
        }

        let mut decoy_command = tokio::process::Command::new("python3");
        decoy_command
            .args([
                "-u",
                "-c",
                r#"
import subprocess, sys
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(15)'])
print(child.pid, flush=True)
sys.stdin.buffer.read(1)
child.terminate()
child.wait(timeout=2)
"#,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        // SAFETY: the fork child calls setsid before exec without allocating.
        unsafe {
            decoy_command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut decoy = decoy_command.spawn().unwrap();
        let decoy_pid = decoy.id().unwrap() as libc::pid_t;
        let mut line = String::new();
        timeout(
            Duration::from_secs(5),
            BufReader::new(decoy.stdout.take().unwrap()).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let decoy_child: libc::pid_t = line.trim().parse().unwrap();
        let _child_cleanup = KillFixture(open_pidfd(decoy_child).unwrap());
        assert!(owned_process(decoy_child, decoy_pid));

        let mut leader = tokio::process::Command::new("python3")
            .args(["-c", "import time; time.sleep(15)"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let leader_fd = open_pidfd(leader.id().unwrap() as libc::pid_t).unwrap();
        assert!(!pidfd_matches_proc(&leader_fd, decoy_pid));
        // Deliberately supply a procfs identity belonging to a different, fully
        // controlled session. Only the trusted descriptor may be signalled.
        stop_session(decoy_pid, &leader_fd);
        let leader_status = timeout(Duration::from_secs(2), leader.wait())
            .await
            .unwrap()
            .unwrap();
        let decoy_alive = decoy.try_wait().unwrap().is_none();
        let child_alive = std::fs::read_to_string(format!("/proc/{decoy_child}/stat"))
            .ok()
            .is_some_and(|stat| {
                stat.rsplit_once(')').unwrap().1.split_whitespace().next() != Some("Z")
            });
        // Let the controlled parent reap its own child before reporting failures.
        drop(decoy.stdin.take());
        let decoy_status = timeout(Duration::from_secs(3), decoy.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!leader_status.success());
        assert!(
            decoy_alive && child_alive,
            "unbound proc identities must be skipped"
        );
        assert!(decoy_status.success());
    }

    #[test]
    fn output_ring_retains_exact_bounded_tail() {
        let mut output = Output::default();
        output.push(&vec![b'a'; OUTPUT_CAPACITY]);
        output.push(b"tail");
        assert_eq!(output.bytes.len(), OUTPUT_CAPACITY);
        assert_eq!(output.start, 4);
        assert_eq!(output.end, OUTPUT_CAPACITY as u64 + 4);
        assert_eq!(
            output
                .bytes
                .iter()
                .rev()
                .take(4)
                .copied()
                .collect::<Vec<_>>(),
            b"liat"
        );
    }

    #[tokio::test]
    async fn long_terminal_output_keeps_full_prefix_beyond_ring_capacity() {
        use sha2::{Digest, Sha256};
        let output = OutputStore::new_connection().unwrap();
        let terminal =
            Terminal::spawn_with_logging(Path::new("/bin/sh"), false, output.clone()).unwrap();
        terminal.perform(Operation::Write {
            data: "stty -echo; head -c 1100000 /dev/zero | tr '\\0' x; printf '\\nLONG_DONE\\n'; exit\n".into(),
        }).await.unwrap();
        timeout(Duration::from_secs(10), async {
            loop {
                if terminal.output.lock().unwrap().eof {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("long terminal command did not finish");
        let result = terminal
            .perform(Operation::Read {
                cursor: 0,
                max_bytes: 16384,
                wait_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(result["spilled"], true);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["text"], "");
        assert_eq!(result["artifact"]["scope"], "terminal_session");
        assert_eq!(result["artifact"]["complete"], true);
        assert_eq!(result["range"]["start"], 0);
        assert_eq!(result["artifact"]["storage"], "memory");
        assert!(result["artifact"]["path"].is_null());
        assert_eq!(result["output_eof"], true);
        assert!(terminal.output.lock().unwrap().start > 0);
        let id = result["artifact"]["id"].as_str().unwrap();
        let stat = output
            .perform("ssmcp.output.stat", json!({"id":id}))
            .await
            .unwrap();
        assert_eq!(stat, result["artifact"]);
        let size = stat["bytes"].as_u64().unwrap();
        assert_eq!(stat["observed_bytes"], size);
        let other_output = OutputStore::new_connection().unwrap();
        for name in ["ssmcp.output.read", "ssmcp.output.stat"] {
            assert!(other_output.perform(name, json!({"id":id})).await.is_err());
        }
        let mut bytes = Vec::new();
        let mut offset = 0;
        while offset < size {
            let page = output
                .perform(
                    "ssmcp.output.read",
                    json!({"id":id,"offset":offset,"max_bytes":crate::output::READ_BYTES}),
                )
                .await
                .unwrap();
            let data = STANDARD
                .decode(page["data_base64"].as_str().unwrap())
                .unwrap();
            let next = page["next_offset"].as_u64().unwrap();
            assert_eq!(page["id"], id);
            assert_eq!(page["storage"], "memory");
            assert!(page["path"].is_null());
            assert_eq!(page["offset"], offset);
            assert_eq!(page["bytes"], data.len());
            assert_eq!(page["size"], size);
            assert_eq!(page["observed_bytes"], size);
            assert_eq!(page["complete"], true);
            assert_eq!(page["truncated"], false);
            assert!(data.len() <= crate::output::READ_BYTES);
            assert_eq!(next, offset + data.len() as u64);
            assert!(next > offset && next <= size);
            assert_eq!(page["eof"], next == size);
            bytes.extend_from_slice(&data);
            offset = next;
        }
        assert!(
            bytes
                .windows(1100000)
                .any(|part| part.iter().all(|byte| *byte == b'x'))
        );
        assert!(bytes.windows(9).any(|part| part == b"LONG_DONE"));
        assert_eq!(result["artifact"]["bytes"], bytes.len());
        assert_eq!(
            result["artifact"]["sha256"],
            crate::hex::encode(Sha256::digest(&bytes))
        );
        assert_eq!(result["next_cursor"], bytes.len());
        assert_eq!(result["range"]["end"], bytes.len());
        assert_eq!(result["range"]["stored_end"], bytes.len());
        let tail_start = bytes.len().saturating_sub(32);
        let tail = terminal
            .perform(Operation::Read {
                cursor: tail_start as u64,
                max_bytes: 16384,
                wait_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            STANDARD
                .decode(tail["data_base64"].as_str().unwrap())
                .unwrap(),
            bytes[tail_start..]
        );
        assert_eq!(tail["next_cursor"], bytes.len());
        assert_eq!(tail["truncated"], false);
        assert_eq!(tail["output_eof"], true);
        let empty = terminal
            .perform(Operation::Read {
                cursor: bytes.len() as u64,
                max_bytes: 16384,
                wait_ms: 0,
            })
            .await
            .unwrap();
        assert_eq!(empty["text"], "");
        assert_eq!(empty["next_cursor"], bytes.len());
        assert_eq!(empty["output_eof"], true);
        terminal.shutdown().await;
    }
}
