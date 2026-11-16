//! Owned commands. Linux uses a per-command subreaper; other platforms keep native ownership.
#[cfg(target_os = "linux")]
use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
    process::ExitStatus,
    time::{Duration, Instant},
};
#[cfg(target_os = "linux")]
use tokio::process::{Child, Command};

#[cfg(not(target_os = "linux"))]
#[path = "process_portable.rs"]
mod portable;
#[cfg(not(target_os = "linux"))]
pub use portable::Managed;

#[cfg(target_os = "linux")]
pub struct Managed {
    pub child: Child,
    pid: libc::pid_t,
    leader: OwnedFd,
    stopped: bool,
}

#[cfg(target_os = "linux")]
impl Managed {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        // Only the guardian is a direct child. Killing it first would orphan
        // detached grandchildren, so all exits go through its bounded cleanup.
        command.kill_on_drop(false);
        let parent = std::process::id() as libc::pid_t;
        let max_fd = guardian::descriptor_limit()?;
        // SAFETY: guardian::prepare contains only stack data and libc operations.
        unsafe {
            command.pre_exec(move || guardian::prepare(parent, max_fd));
        }
        let child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no PID"))? as libc::pid_t;
        let leader = match crate::pty::open_pidfd(pid) {
            Ok(fd) => fd,
            Err(error) => {
                // The unreaped direct child still owns this PID. Ask it to
                // clean up rather than killing its subreaper before descendants.
                unsafe {
                    libc::kill(pid, libc::SIGTERM);
                }
                return Err(error);
            }
        };
        Ok(Self {
            child,
            pid,
            leader,
            stopped: false,
        })
    }

    /// The guardian exits only after reaping its complete owned process tree.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if self.stopped {
            return self.child.wait().await;
        }
        // SAFETY: AsyncFd owns the duplicated pidfd for its whole registration.
        let ready = unsafe { tokio::io::unix::AsyncFd::register(self.leader.try_clone()?) }?;
        let _ = ready.readable().await?;
        self.stopped = true;
        self.child.wait().await
    }

    /// Request cleanup while the subreaper remains alive to adopt and reap
    /// detached descendants. The watchdog also works after a Tokio runtime stops.
    pub fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        if exited(&self.leader, Duration::ZERO) {
            return;
        }
        signal(&self.leader, libc::SIGCONT);
        signal(&self.leader, libc::SIGTERM);
        let pid = self.pid;
        let watchdog = self.leader.try_clone().and_then(|leader| {
            std::thread::Builder::new()
                .name("ssmcp-process-cleanup".into())
                .spawn(move || {
                    if !exited(&leader, Duration::from_secs(5)) {
                        eprintln!(
                            "ssmcp: supervisor {pid} did not exit; forcing owned process cleanup"
                        );
                        crate::pty::stop_session(pid, &leader);
                    }
                })
        });
        if watchdog.is_err() {
            eprintln!(
                "ssmcp: could not start process cleanup watchdog; forcing owned process cleanup"
            );
            crate::pty::stop_session(self.pid, &self.leader);
        }
    }

    pub async fn shutdown(&mut self) {
        self.terminate();
        // Always wait, including after synchronous termination or a cancelled wait.
        let _ = self.child.wait().await;
    }
}

#[cfg(target_os = "linux")]
impl Drop for Managed {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(target_os = "linux")]
fn signal(leader: &OwnedFd, signal: libc::c_int) {
    // SAFETY: the pidfd binds delivery to this exact process, never a reused PID.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            leader.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        );
    }
}

#[cfg(target_os = "linux")]
fn exited(leader: &OwnedFd, wait: Duration) -> bool {
    let started = Instant::now();
    loop {
        let mut poll = libc::pollfd {
            fd: leader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining = wait
            .saturating_sub(started.elapsed())
            .as_millis()
            .min(i32::MAX as u128);
        // SAFETY: poll points at one initialized descriptor and borrows its owner.
        let result = unsafe { libc::poll(&mut poll, 1, remaining as i32) };
        if result > 0 {
            return poll.revents & (libc::POLLIN | libc::POLLHUP) != 0;
        }
        if result == 0 || started.elapsed() >= wait {
            return false;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// A per-command subreaper keeps double-forked descendants inside one owned tree.
/// This runs after std::process forks: no allocator, Rust locks, or atfork hooks.
#[cfg(target_os = "linux")]
mod guardian {
    use std::io;

    pub fn descriptor_limit() -> io::Result<libc::rlim_t> {
        // SAFETY: getrlimit initializes this local structure.
        unsafe {
            let mut limit: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(limit.rlim_cur.min(libc::c_int::MAX as libc::rlim_t))
        }
    }

    pub fn prepare(parent: libc::pid_t, max_fd: libc::rlim_t) -> io::Result<()> {
        // SAFETY: this is the single thread created by Command's fork. Every
        // operation until the command returns to exec is a libc syscall wrapper
        // or stack-only operation. In particular SYS_clone avoids libc atfork
        // handlers inherited from the multithreaded parent.
        unsafe {
            if libc::setsid() < 0 || libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            let mut original_mask: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut blocked);
            if libc::sigprocmask(libc::SIG_SETMASK, &blocked, &mut original_mask) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut original_child: libc::sigaction = std::mem::zeroed();
            let mut default_child: libc::sigaction = std::mem::zeroed();
            default_child.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut default_child.sa_mask);
            if libc::sigaction(libc::SIGCHLD, &default_child, &mut original_child) != 0
                // SIGCONT also resumes a stopped guardian when its parent dies.
                // A parent thread exiting can send it early, so the loop checks
                // getppid rather than treating SIGCONT alone as cancellation.
                || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGCONT, 0, 0, 0) < 0
            {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ECANCELED));
            }
            let owner = libc::getpid();
            let child = libc::syscall(
                libc::SYS_clone,
                libc::SIGCHLD as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
            if child < 0 {
                return Err(io::Error::last_os_error());
            }
            if child == 0 {
                // The actual command retains Command's CLOEXEC error pipe.
                // Its exec failure therefore still fails Command::spawn.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0
                    || libc::sigaction(libc::SIGCHLD, &original_child, std::ptr::null_mut()) != 0
                    || libc::sigprocmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut())
                        != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != owner {
                    libc::_exit(125);
                }
                return Ok(());
            }
            // Closing the guardian's copy of the exec-error pipe is essential:
            // otherwise spawn waits for the entire job instead of only exec.
            // Also release every unrelated SSH, output, and reactor descriptor.
            if libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) != 0 {
                let mut fd = 3;
                while (fd as libc::rlim_t) < max_fd {
                    libc::close(fd);
                    fd += 1;
                }
            }
            libc::close(libc::STDIN_FILENO);
            supervise(child as libc::pid_t, parent, &blocked)
        }
    }

    fn milliseconds() -> i64 {
        // SAFETY: clock_gettime initializes the local timespec and cannot allocate.
        unsafe {
            let mut now: libc::timespec = std::mem::zeroed();
            if libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) != 0 {
                return -1;
            }
            (now.tv_sec as i64).saturating_mul(1000) + now.tv_nsec as i64 / 1_000_000
        }
    }

    fn diagnostic(message: &[u8]) {
        // SAFETY: the static message is live for the duration of write. Errors
        // (including a closed stderr pipe) must not prevent process cleanup.
        unsafe {
            libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
        }
    }

    fn kill_children() -> bool {
        // SAFETY: only direct children are signalled. This guardian is their sole
        // reaper, and does not waitpid while parsing, so their PIDs cannot be reused.
        // Repeat after reaping: each killed parent reparents its children here.
        unsafe {
            let fd = libc::open(
                c"/proc/thread-self/children".as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC,
            );
            if fd < 0 {
                return false;
            }
            let mut bytes = [0_u8; 4096];
            let mut pid: libc::pid_t = 0;
            loop {
                let n = libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len());
                if n < 0 {
                    if *libc::__errno_location() == libc::EINTR {
                        continue;
                    }
                    libc::close(fd);
                    return false;
                }
                if n == 0 {
                    if pid > 1 {
                        libc::kill(pid, libc::SIGKILL);
                    }
                    libc::close(fd);
                    return true;
                }
                for byte in &bytes[..n as usize] {
                    if byte.is_ascii_digit() {
                        pid = pid.saturating_mul(10).saturating_add((byte - b'0') as i32);
                    } else {
                        if pid > 1 {
                            libc::kill(pid, libc::SIGKILL);
                        }
                        pid = 0;
                    }
                }
            }
        }
    }

    fn supervise(command: libc::pid_t, parent: libc::pid_t, blocked: &libc::sigset_t) -> ! {
        // SAFETY: all signals are blocked, so inherited runtime handlers cannot
        // execute. The loop uses only fixed-size stack data and libc operations.
        unsafe {
            let mut command_status = None;
            let mut stopping = false;
            let mut deadline = 0;
            loop {
                loop {
                    let mut status = 0;
                    let reaped = libc::waitpid(-1, &mut status, libc::WNOHANG);
                    if reaped == command {
                        command_status = Some(status);
                        stopping = true;
                    } else if reaped == 0 {
                        break;
                    } else if reaped < 0 {
                        match *libc::__errno_location() {
                            libc::EINTR => continue,
                            libc::ECHILD => {
                                if let Some(status) = command_status {
                                    exit_like(status);
                                }
                                diagnostic(b"ssmcp: supervisor lost the native exit status\n");
                                libc::_exit(125);
                            }
                            _ => {
                                diagnostic(b"ssmcp: supervisor could not reap owned children\n");
                                libc::_exit(125);
                            }
                        }
                    }
                }
                if libc::getppid() != parent {
                    stopping = true;
                }
                if stopping {
                    let now = milliseconds();
                    if deadline == 0 {
                        deadline = now.saturating_add(4000);
                    }
                    if now < 0 || now >= deadline || !kill_children() {
                        diagnostic(b"ssmcp: supervisor could not finish descendant cleanup\n");
                        libc::_exit(125);
                    }
                }
                let pause = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 20_000_000,
                };
                let signal = libc::sigtimedwait(blocked, std::ptr::null_mut(), &pause);
                if matches!(
                    signal,
                    libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGQUIT
                ) {
                    stopping = true;
                } else if signal > 0 && signal != libc::SIGCHLD && signal != libc::SIGCONT {
                    // Preserve deliberate non-cancellation signals to the native
                    // process; its resulting exit status is still returned intact.
                    if command_status.is_none() {
                        libc::kill(command, signal);
                    }
                }
            }
        }
    }

    fn exit_like(status: libc::c_int) -> ! {
        // SAFETY: no children remain. Reproduce the native exit code or signal,
        // while preventing a signal from dumping inherited parent memory.
        unsafe {
            if libc::WIFEXITED(status) {
                libc::_exit(libc::WEXITSTATUS(status));
            }
            if libc::WIFSIGNALED(status) {
                let signal = libc::WTERMSIG(status);
                let limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                libc::setrlimit(libc::RLIMIT_CORE, &limit);
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, signal);
                libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut());
                libc::kill(libc::getpid(), signal);
                libc::_exit(128 + signal);
            }
            libc::_exit(125);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        time::timeout,
    };

    #[tokio::test]
    async fn shutdown_stops_owned_children_in_a_separate_session() {
        let mut command = Command::new("python3");
        command.args(["-u","-c",r#"
import os, subprocess, sys, time
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'], start_new_session=True)
print(child.pid, flush=True)
time.sleep(300)
"#]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut managed = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(managed.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        assert_eq!(
            stat.rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(3)
                .unwrap(),
            pid.to_string()
        );
        managed.shutdown().await;
        timeout(Duration::from_secs(5), async {
            loop {
                let running = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|s| {
                        s.rsplit_once(')').unwrap().1.split_whitespace().next() != Some("Z")
                    });
                if !running {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned child must stop even when it called setsid");
    }
    #[tokio::test]
    async fn natural_exit_cleans_background_children_before_reaping_the_leader() {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", "import subprocess,sys; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(child.pid,flush=True)"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut process = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(process.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        let status = timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        timeout(Duration::from_secs(5), async {
            loop {
                let running = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|s| {
                        s.rsplit_once(')').unwrap().1.split_whitespace().next() != Some("Z")
                    });
                if !running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background child survived the completed command");
    }
    const DOUBLE_FORK: &str = r#"
import os, sys, time
parent = os.getpid()
read, write = os.pipe()
first = os.fork()
if first == 0:
    os.close(read)
    os.setsid()
    if os.fork() != 0:
        os._exit(0)
    print(parent, os.getpid(), flush=True)
    os.write(write, b"R")
    os.close(write)
    time.sleep(60)
    os._exit(0)
os.close(write)
os.read(read, 1)
os.close(read)
os.waitpid(first, 0)
if os.environ.get("SSMCP_FIXTURE_EXIT") == "1":
    sys.exit(17)
time.sleep(60)
"#;

    async fn detached_command(
        completes: bool,
    ) -> (Managed, i32, i32, BufReader<tokio::process::ChildStdout>) {
        let mut command = Command::new("python3");
        command
            .args(["-u", "-c", DOUBLE_FORK])
            .env("SSMCP_FIXTURE_EXIT", if completes { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut managed = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(managed.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pids: Vec<i32> = line
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2);
        (managed, pids[0], pids[1], stdout)
    }

    async fn gone(pid: i32) {
        timeout(Duration::from_secs(5), async {
            while std::path::Path::new(&format!("/proc/{pid}")).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("owned descendant {pid} was not reaped"));
    }

    #[tokio::test]
    async fn double_forked_descendant_is_reaped_after_native_exit() {
        use tokio::io::AsyncReadExt;
        let (mut managed, command, daemon, mut stdout) = detached_command(true).await;
        let status = timeout(Duration::from_secs(5), managed.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(17), "preserve the native exit code");
        gone(command).await;
        gone(daemon).await;
        let mut tail = Vec::new();
        timeout(Duration::from_secs(1), stdout.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tail.is_empty(),
            "all inherited stdout writers must be closed"
        );
    }

    #[tokio::test]
    async fn cancelling_a_stopped_supervisor_reaps_double_forked_descendants() {
        let (mut managed, command, daemon, _stdout) = detached_command(false).await;
        signal(&managed.leader, libc::SIGSTOP);
        timeout(Duration::from_secs(5), managed.shutdown())
            .await
            .unwrap();
        gone(command).await;
        gone(daemon).await;
    }

    #[tokio::test]
    async fn dropping_a_manager_reaps_double_forked_descendants() {
        let (managed, command, daemon, _stdout) = detached_command(false).await;
        drop(managed);
        gone(command).await;
        gone(daemon).await;
    }

    #[tokio::test]
    async fn native_signal_exit_and_exec_failure_are_preserved() {
        use std::os::unix::process::ExitStatusExt;
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import os,signal; os.kill(os.getpid(),signal.SIGTERM)",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut managed = Managed::spawn(&mut command).unwrap();
        let status = timeout(Duration::from_secs(5), managed.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        let mut missing = Command::new("/ssmcp/nonexistent-command-for-exec-error-test");
        let error = Managed::spawn(&mut missing)
            .err()
            .expect("exec failure must fail spawn");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[tokio::test]
    #[ignore = "subprocess fixture for abrupt-parent-death regression"]
    async fn supervisor_parent_death_fixture() {
        if std::env::var_os("SSMCP_PARENT_DEATH_FIXTURE").is_none() {
            return;
        }
        let (managed, command, daemon, _stdout) = detached_command(false).await;
        println!(
            "SSMCP_OWNED_PIDS {} {command} {daemon}",
            managed.child.id().unwrap()
        );
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        // Deliberately skip both Managed::drop and runtime shutdown.
        unsafe {
            libc::_exit(0);
        }
    }

    #[tokio::test]
    async fn abrupt_owner_exit_cleans_and_reaps_detached_commands() {
        let mut fixture = Command::new(std::env::current_exe().unwrap());
        fixture
            .args([
                "--exact",
                "backend::process::tests::supervisor_parent_death_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SSMCP_PARENT_DEATH_FIXTURE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let output = timeout(Duration::from_secs(10), fixture.output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        let pids: Vec<i32> = text
            .lines()
            .find_map(|line| line.strip_prefix("SSMCP_OWNED_PIDS "))
            .expect("fixture must report owned identities")
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 3);
        gone(pids[1]).await;
        gone(pids[2]).await;
        // The guardian itself is adopted by the machine's init after the test
        // parent dies. Descendants were reaped by it; init controls its own reap.
        let guardian = std::fs::read_to_string(format!("/proc/{}/stat", pids[0])).ok();
        assert!(guardian.is_none_or(|s| {
            s.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z")
        }));
    }
}
