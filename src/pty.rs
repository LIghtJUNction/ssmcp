//! A single Linux pseudo-terminal with bounded, cursor-addressed output.
use crate::wire::Operation;
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
}

impl Output {
    fn push(&mut self, data: &[u8]) {
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
}

impl Terminal {
    pub fn spawn(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, false)
    }

    pub fn spawn_live(shell: &Path) -> crate::Result<Arc<Self>> {
        Self::spawn_with_logging(shell, true)
    }

    fn spawn_with_logging(shell: &Path, live: bool) -> crate::Result<Arc<Self>> {
        if !shell.is_absolute() {
            return Err("shell must be an absolute path".into());
        }
        let mut master = -1;
        let mut slave = -1;
        let size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
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
            output: Mutex::new(Output::default()),
            changed: Notify::new(),
            writer: AsyncMutex::new(()),
            reader: Mutex::new(None),
            closed: AtomicBool::new(false),
            live,
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
                        if terminal.live {
                            use std::io::Write;
                            let _ = std::io::stderr().lock().write_all(&data[..n]);
                        }
                    }
                    _ => {
                        // Linux PTYs report EIO when the last slave closes.
                        terminal.output.lock().unwrap().eof = true;
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
                if self.live {
                    eprintln!(
                        "\n[ssmcp shell input] {}",
                        serde_json::to_string(&data).unwrap()
                    );
                }
                if !self.running().await {
                    return Err("terminal has exited".into());
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
            Operation::ListTools { .. } | Operation::CallTool { .. } => {
                Err("not a terminal operation".into())
            }
            Operation::Status {} => {
                let running = self.running().await;
                let output = self.output.lock().unwrap();
                Ok(
                    json!({"running":running,"pid":self.pid,"start_cursor":output.start,"next_cursor":output.end,"output_eof":output.eof}),
                )
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
                    let bytes: Vec<u8> = output
                        .bytes
                        .iter()
                        .skip((from - output.start) as usize)
                        .take(max_bytes)
                        .copied()
                        .collect();
                    Some((from, bytes, cursor < output.start, output.eof))
                } else {
                    None
                }
            };
            if let Some((from, bytes, truncated, eof)) = snapshot {
                return Ok(json!({
                    "text":String::from_utf8_lossy(&bytes), "data_base64":STANDARD.encode(&bytes),
                    "next_cursor":from + bytes.len() as u64, "truncated":truncated,
                    "running":self.running().await, "output_eof":eof,
                }));
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
        self.output.lock().unwrap().eof = true;
        self.changed.notify_waiters();
    }

    fn stop_session(&self) {
        stop_session(self.pid, &self.leader);
    }
}

pub(crate) fn stop_session(session: libc::pid_t, leader: &OwnedFd) {
    // Job-control shells put background jobs in separate process groups. Take one
    // session snapshot, then signal pidfds so PID reuse cannot target another process.
    // This is cleanup for ordinary jobs, not containment for deliberate setsid/fork escape.
    let mut members = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            if pid == session || process_session(pid) != Some(session) {
                continue;
            }
            if let Ok(fd) = open_pidfd(pid) {
                // Validate after opening: the descriptor is bound to this exact process.
                if process_session(pid) == Some(session) {
                    members.push(fd);
                }
            }
        }
    }
    for member in members {
        let _ = signal_pidfd(&member, libc::SIGKILL);
    }
    let _ = signal_pidfd(leader, libc::SIGKILL);
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
}
