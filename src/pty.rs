//! A single Linux pseudo-terminal with bounded, cursor-addressed output.
use crate::{
    output::{Capture, INLINE_BYTES, INLINE_LINES, OutputStore},
    wire::Operation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::unix::AsyncFd,
    process::{Child, Command},
    sync::{Mutex as AsyncMutex, Notify},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
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

pub struct Terminal {
    master: Mutex<Option<Arc<AsyncFd<OwnedFd>>>>,
    child: AsyncMutex<Option<Child>>,
    leader: OwnedFd,
    pid: libc::pid_t,
    output: Mutex<Output>,
    changed: Notify,
    writer: AsyncMutex<()>,
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
        let mut master = -1;
        let mut slave = -1;
        let (rows, cols) = if live {
            crate::client::terminal::pane_size(24, 80)
        } else {
            (24, 80)
        };
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: all pointers refer to initialized, live storage; openpty fills both fds.
        if unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            )
        } < 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: successful openpty returned two newly owned descriptors.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: fd is owned and valid; FD_CLOEXEC prevents unintended inheritance.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
        // SAFETY: master is a valid descriptor and no other flags need preserving on a new PTY.
        if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: OwnedFd keeps this descriptor valid and stable for AsyncFd's whole lifetime.
        let master = Arc::new(unsafe { AsyncFd::register(master) }?);
        let mut command = Command::new(shell);
        command
            .arg("-i")
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
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
        let child = command.spawn()?;
        let pid = child.id().ok_or("shell has no process id")? as libc::pid_t;
        let leader = open_pidfd(pid).map_err(|error| {
            format!("terminal cleanup requires Linux 5.3+ with pidfd support: {error}")
        })?;
        let terminal = Arc::new(Self {
            master: Mutex::new(Some(master.clone())),
            child: AsyncMutex::new(Some(child)),
            leader,
            pid,
            output: Mutex::new(Output {
                capture: Some(output_store.capture("terminal_session")),
                ..Output::default()
            }),
            changed: Notify::new(),
            writer: AsyncMutex::new(()),
            reader: Mutex::new(None),
            closed: AtomicBool::new(false),
            live,
            display_id: display_id.into(),
            observer: output_store.observer().map(|pool| Arc::downgrade(&pool)),
            observer_cwd: cwd
                .map(Path::to_path_buf)
                .unwrap_or(std::env::current_dir()?),
        });
        let weak = Arc::downgrade(&terminal);
        let reader = tokio::spawn(async move {
            let mut data = [0_u8; 8192];
            loop {
                let result = async {
                    loop {
                        let mut ready = master.readable().await?;
                        match ready.try_io(|fd| read_fd(fd.get_ref().as_raw_fd(), &mut data)) {
                            Ok(result) => return result,
                            Err(_) => continue,
                        }
                    }
                }
                .await;
                let Some(terminal) = weak.upgrade() else {
                    break;
                };
                match result {
                    Ok(n) if n > 0 => {
                        terminal.output.lock().unwrap().push(&data[..n]);
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
            }
        });
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
                if !self.running().await {
                    return Err("terminal has exited".into());
                }
                if let Some(pool) = self.observer.as_ref().and_then(std::sync::Weak::upgrade) {
                    pool.terminal_begin(&self.display_id, &self.observer_cwd)
                        .await?;
                }
                timeout(Duration::from_secs(5), self.write(data.as_bytes()))
                    .await
                    .map_err(|_| {
                        "terminal write timed out; input may be partially written".to_string()
                    })?
                    .map_err(|error| error.to_string())?;
                Ok(json!({"written": data.len()}))
            }
            Operation::Resize { rows, cols } => {
                if !self.running().await {
                    return Err("terminal has exited".into());
                }
                let master = self.master()?;
                let size = libc::winsize {
                    ws_row: rows,
                    ws_col: cols,
                    ws_xpixel: 0,
                    ws_ypixel: 0,
                };
                // SAFETY: master and winsize point to live objects for the ioctl duration.
                if unsafe { libc::ioctl(master.get_ref().as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0
                {
                    return Err(io::Error::last_os_error().to_string());
                }
                Ok(json!({"rows":rows,"cols":cols}))
            }
            Operation::PeerPort { .. }
            | Operation::ListTools { .. }
            | Operation::CallTool { .. } => Err("not a terminal operation".into()),
            Operation::Status {} => {
                let running = self.running().await;
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

    fn master(&self) -> Result<Arc<AsyncFd<OwnedFd>>, String> {
        self.master
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "terminal is closed".into())
    }

    async fn running(&self) -> bool {
        let child = self.child.lock().await;
        if self.closed.load(Ordering::Acquire) || child.is_none() {
            return false;
        }
        // Keep the session leader unreaped until shutdown, preventing its PID/session ID reuse.
        // SAFETY: info is writable, zeroed storage; WNOHANG makes waitid nonblocking.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        // SAFETY: waitid initialized the siginfo union on success.
        result == 0 && unsafe { info.si_pid() } == 0
    }

    async fn read(&self, cursor: u64, max_bytes: usize, wait_ms: u64) -> Result<Value, String> {
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
                    if long {
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
                snapshot["running"] = json!(self.running().await);
                return Ok(snapshot);
            }
            let _ = timeout_at(deadline, changed).await;
        }
    }

    async fn write(&self, data: &[u8]) -> io::Result<()> {
        let _guard = self.writer.lock().await;
        let master = self.master().map_err(io::Error::other)?;
        let mut offset = 0;
        while offset < data.len() {
            let mut ready = master.writable().await?;
            match ready.try_io(|fd| write_fd(fd.get_ref().as_raw_fd(), &data[offset..])) {
                Ok(Ok(0)) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "terminal closed"));
                }
                Ok(Ok(n)) => offset += n,
                Ok(Err(error)) => return Err(error),
                Err(_) => continue,
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        if let Some(pool) = self.observer.as_ref().and_then(std::sync::Weak::upgrade) {
            pool.terminal_closed(&self.display_id);
        }
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let mut child = self.child.lock().await;
        if child.is_some() {
            self.stop_session();
            if let Some(mut child) = child.take() {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
        let reader = self.reader.lock().unwrap().take();
        if let Some(reader) = reader {
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
        if self.child.get_mut().is_some() {
            self.stop_session();
        }
        if let Some(reader) = self.reader.get_mut().unwrap().take() {
            reader.abort();
        }
        // Child's kill_on_drop also handles early error paths; normal shutdown explicitly reaps it.
    }
}

fn read_fd(fd: RawFd, data: &mut [u8]) -> io::Result<usize> {
    // SAFETY: fd is valid and the mutable slice owns its writable buffer.
    let n = unsafe { libc::read(fd, data.as_mut_ptr().cast(), data.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_fd(fd: RawFd, data: &[u8]) -> io::Result<usize> {
    // SAFETY: fd is valid and data remains readable for the syscall duration.
    let n = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    if n < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(n as usize)
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
        assert!(!terminal.running().await);
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
                let foreground = unsafe { libc::tcgetpgrp(master.get_ref().as_raw_fd()) };
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

        let mut decoy_command = Command::new("python3");
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

        let mut leader = Command::new("python3")
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
