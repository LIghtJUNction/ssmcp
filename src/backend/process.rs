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

/// Register only the real CLI, before Tokio starts. Library consumers retain
/// the fork-only guardian; their executable need not implement our private mode.
pub fn initialize() -> Option<std::process::ExitCode> {
    #[cfg(target_os = "linux")]
    {
        if let Some(code) = guardian::entry() {
            return Some(std::process::ExitCode::from(code));
        }
        guardian::register_executable();
    }
    None
}

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
        let bootstrap = guardian::bootstrap();
        let launch = bootstrap.as_ref().map(guardian::Bootstrap::launch);
        // SAFETY: guardian::prepare contains only stack data and libc operations.
        unsafe {
            command.pre_exec(move || guardian::prepare(parent, max_fd, launch));
        }
        let child = command.spawn()?;
        drop(bootstrap);
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
    use std::{
        ffi::OsStr,
        io,
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        sync::OnceLock,
    };

    const ENTRY: &str = "__process_guardian";
    static EXECUTABLE: OnceLock<OwnedFd> = OnceLock::new();

    pub(super) struct Bootstrap {
        executable: RawFd,
        read: OwnedFd,
        write: OwnedFd,
    }

    #[derive(Clone, Copy)]
    pub(super) struct Launch {
        executable: RawFd,
        read: RawFd,
        write: RawFd,
    }

    impl Bootstrap {
        pub(super) fn launch(&self) -> Launch {
            Launch {
                executable: self.executable,
                read: self.read.as_raw_fd(),
                write: self.write.as_raw_fd(),
            }
        }
    }

    fn above_reserved(fd: OwnedFd) -> io::Result<OwnedFd> {
        // Reserve 3 and 4 only inside the guardian. Normalizing in the parent
        // avoids descriptor aliases even if its standard streams are closed.
        let next = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
        if next < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(next) })
    }

    pub(super) fn register_executable() {
        // Pin the running inode, including across atomic CLI upgrades. O_PATH
        // also works for an executable that has no read permission.
        let fd = unsafe { libc::open(c"/proc/self/exe".as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd >= 0
            && let Ok(fd) = above_reserved(unsafe { OwnedFd::from_raw_fd(fd) })
        {
            let _ = EXECUTABLE.set(fd);
        }
    }

    pub(super) fn bootstrap() -> Option<Bootstrap> {
        let executable = EXECUTABLE.get()?.as_raw_fd();
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return None;
        }
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        Some(Bootstrap {
            executable,
            read: above_reserved(read).ok()?,
            write: above_reserved(write).ok()?,
        })
    }

    pub(super) fn entry() -> Option<u8> {
        let mut args = std::env::args_os().skip(1);
        if args.next().as_deref() != Some(OsStr::new(ENTRY)) {
            return None;
        }
        let pid = |arg: Option<std::ffi::OsString>| {
            arg?.to_str()?
                .parse::<libc::pid_t>()
                .ok()
                .filter(|pid| *pid > 0)
        };
        let (Some(command), Some(parent)) = (pid(args.next()), pid(args.next())) else {
            return Some(125);
        };
        if args.next().is_some() {
            return Some(125);
        }
        // This is a fresh exec, so ordinary Rust startup allocations are safe.
        // The private entry cannot supervise an arbitrary process: it must
        // already be the session-leading subreaper and the command's parent.
        unsafe {
            let mut subreaper: libc::c_int = 0;
            let mut info: libc::siginfo_t = std::mem::zeroed();
            if libc::getsid(0) != libc::getpid()
                || libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut subreaper, 0, 0, 0) != 0
                || subreaper != 1
                || libc::waitid(
                    libc::P_PID,
                    command as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
                ) != 0
            {
                return Some(125);
            }
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut blocked);
            if libc::sigprocmask(libc::SIG_SETMASK, &blocked, std::ptr::null_mut()) != 0 {
                return Some(125);
            }
            release_command(4, command);
            // A parent may have died during exec; the original loop must then
            // reap the complete owned tree, rather than rejecting the entry.
            supervise(command, parent, &blocked)
        }
    }

    unsafe fn release_command(fd: RawFd, command: libc::pid_t) {
        // A loader/entry failure must never allow the real command to fork
        // descendants before this guardian is ready to supervise them.
        unsafe {
            let mut ready = false;
            loop {
                let written = libc::write(fd, b"R".as_ptr().cast(), 1);
                if written == 1 {
                    ready = true;
                    break;
                }
                if written < 0 && *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                break;
            }
            libc::close(fd);
            if !ready {
                // The command has not been reaped, so this PID cannot be reused.
                libc::kill(command, libc::SIGKILL);
            }
        }
    }

    unsafe fn await_ready(fd: RawFd) -> io::Result<()> {
        unsafe {
            let mut byte = 0_u8;
            let result = loop {
                let read = libc::read(fd, (&mut byte as *mut u8).cast(), 1);
                if read < 0 && *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                break read;
            };
            libc::close(fd);
            if result == 1 && byte == b'R' {
                Ok(())
            } else {
                Err(io::Error::from_raw_os_error(libc::ECANCELED))
            }
        }
    }

    fn decimal_argument(pid: libc::pid_t, bytes: &mut [u8; 24]) -> *const libc::c_char {
        let mut number = pid as u32;
        let mut index = bytes.len() - 1;
        bytes[index] = 0;
        loop {
            index -= 1;
            bytes[index] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                return bytes[index..].as_ptr().cast();
            }
        }
    }

    unsafe fn exec_guardian(fd: RawFd, command: libc::pid_t, parent: libc::pid_t) {
        let mut command_bytes = [0_u8; 24];
        let mut parent_bytes = [0_u8; 24];
        let argv = [
            c"ssmcp".as_ptr(),
            c"__process_guardian".as_ptr(),
            decimal_argument(command, &mut command_bytes),
            decimal_argument(parent, &mut parent_bytes),
            std::ptr::null(),
        ];
        unsafe extern "C" {
            static mut environ: *mut *mut libc::c_char;
        }
        // Keep the same PID, subreaper, signal mask, parent-death signal and
        // standard streams. Only the inherited client address space is replaced.
        // Preserve the original loader environment; no Rust allocation or lock
        // is permitted between fork and this syscall. Errors use the old loop.
        unsafe {
            libc::syscall(
                libc::SYS_execveat,
                fd,
                c"".as_ptr(),
                argv.as_ptr(),
                environ,
                libc::AT_EMPTY_PATH,
            );
        }
    }

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

    pub fn prepare(
        parent: libc::pid_t,
        max_fd: libc::rlim_t,
        launch: Option<Launch>,
    ) -> io::Result<()> {
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
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != owner {
                    libc::_exit(125);
                }
                if let Some(launch) = launch {
                    libc::close(launch.write);
                    await_ready(launch.read)?;
                }
                if libc::sigaction(libc::SIGCHLD, &original_child, std::ptr::null_mut()) != 0
                    || libc::sigprocmask(libc::SIG_SETMASK, &original_mask, std::ptr::null_mut())
                        != 0
                {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            // Closing the guardian's copy of the exec-error pipe is essential:
            // otherwise spawn waits for the entire job instead of only exec.
            // Also release every unrelated SSH, output, and reactor descriptor.
            // The native command already has its own descriptor table. Reserve
            // fd 3 for exec and fd 4 for the ready acknowledgment. All source
            // descriptors were normalized above 4 before forking.
            let helper = launch.is_some_and(|launch| {
                libc::close(launch.read);
                let copied = libc::dup3(launch.executable, 3, libc::O_CLOEXEC) == 3
                    && libc::dup3(launch.write, 4, 0) == 4;
                if !copied {
                    release_command(launch.write, child as libc::pid_t);
                }
                copied
            });
            let first = if helper { 5_u32 } else { 3_u32 };
            if libc::syscall(libc::SYS_close_range, first, u32::MAX, 0_u32) != 0 {
                let mut fd = first as libc::c_int;
                while (fd as libc::rlim_t) < max_fd {
                    libc::close(fd);
                    fd += 1;
                }
            }
            libc::close(libc::STDIN_FILENO);
            if helper {
                exec_guardian(3, child as libc::pid_t, parent);
                libc::close(3);
                release_command(4, child as libc::pid_t);
            }
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

    pub(super) fn kill_owned_child(pid: libc::pid_t) -> bool {
        if pid <= 1 {
            return false;
        }
        // Procfs may be mounted from an ancestor PID namespace. A numeric entry
        // is not authority to signal a process in this guardian's namespace.
        // waitid proves direct parenthood there without reaping it. This single
        // guardian thread cannot reuse the child's PID between this check and kill.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            loop {
                let result = libc::syscall(
                    libc::SYS_waitid,
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info as *mut libc::siginfo_t,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT | libc::__WALL,
                    std::ptr::null_mut::<libc::rusage>(),
                );
                if result == 0 {
                    return libc::kill(pid, libc::SIGKILL) == 0;
                }
                if *libc::__errno_location() != libc::EINTR {
                    return false;
                }
            }
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
                return kill_children_proc();
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
                    return kill_children_proc();
                }
                if n == 0 {
                    if pid > 1 {
                        kill_owned_child(pid);
                    }
                    libc::close(fd);
                    return true;
                }
                for byte in &bytes[..n as usize] {
                    if byte.is_ascii_digit() {
                        pid = pid.saturating_mul(10).saturating_add((byte - b'0') as i32);
                    } else {
                        if pid > 1 {
                            kill_owned_child(pid);
                        }
                        pid = 0;
                    }
                }
            }
        }
    }

    fn kill_children_proc() -> bool {
        // Some Linux kernels do not expose /proc/<tid>/children. Enumerate proc
        // with getdents64 rather than opendir/readdir, which may allocate or lock
        // after fork. PPid narrows candidates; waitid independently verifies
        // ownership in our PID namespace immediately before signalling.
        unsafe {
            let directory = libc::open(
                c"/proc".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            );
            if directory < 0 {
                return false;
            }
            let owner = libc::getpid();
            let mut entries = [0_u8; 4096];
            loop {
                let n = libc::syscall(
                    libc::SYS_getdents64,
                    directory,
                    entries.as_mut_ptr(),
                    entries.len(),
                );
                if n < 0 {
                    if *libc::__errno_location() == libc::EINTR {
                        continue;
                    }
                    libc::close(directory);
                    return false;
                }
                if n == 0 {
                    libc::close(directory);
                    return true;
                }
                if n as usize > entries.len() {
                    libc::close(directory);
                    return false;
                }
                let mut offset = 0;
                while offset < n as usize {
                    // linux_dirent64: ino(8), off(8), reclen(2), type(1), name.
                    if n as usize - offset < 20 {
                        libc::close(directory);
                        return false;
                    }
                    let length =
                        u16::from_ne_bytes([entries[offset + 16], entries[offset + 17]]) as usize;
                    if length < 20 || length > n as usize - offset {
                        libc::close(directory);
                        return false;
                    }
                    let names = &entries[offset + 19..offset + length];
                    let size = names
                        .iter()
                        .position(|byte| *byte == 0)
                        .unwrap_or(names.len());
                    let name = &names[..size];
                    if let Some(pid) = decimal_pid(name)
                        && pid > 1
                        && pid != owner
                        && proc_parent(name) == Some(owner)
                    {
                        kill_owned_child(pid);
                    }
                    offset += length;
                }
            }
        }
    }

    fn decimal_pid(bytes: &[u8]) -> Option<libc::pid_t> {
        if bytes.is_empty() || bytes.len() > 10 {
            return None;
        }
        let mut value: libc::pid_t = 0;
        for byte in bytes {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value.checked_mul(10)?.checked_add((byte - b'0') as i32)?;
        }
        Some(value)
    }

    fn proc_parent(name: &[u8]) -> Option<libc::pid_t> {
        // The path and stat buffer are fixed stack storage. Slice iterators
        // below borrow that storage; they do not create strings or allocate.
        unsafe {
            if name.is_empty() || name.len() > 10 {
                return None;
            }
            let mut path = [0_u8; 32];
            path[..6].copy_from_slice(b"/proc/");
            path[6..6 + name.len()].copy_from_slice(name);
            path[6 + name.len()..12 + name.len()].copy_from_slice(b"/stat\0");
            let fd = libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return None;
            }
            let mut bytes = [0_u8; 1024];
            let n = loop {
                let n = libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len());
                if n < 0 && *libc::__errno_location() == libc::EINTR {
                    continue;
                }
                break n;
            };
            libc::close(fd);
            if n <= 0 {
                return None;
            }
            let bytes = &bytes[..n as usize];
            let end = bytes.iter().rposition(|byte| *byte == b')')?;
            let mut fields = bytes[end + 1..]
                .split(|byte| byte.is_ascii_whitespace())
                .filter(|field| !field.is_empty());
            fields.next()?; // state
            decimal_pid(fields.next()?)
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
                    let reaped = libc::waitpid(-1, &mut status, libc::WNOHANG | libc::__WALL);
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

    #[test]
    #[ignore = "subprocess fixture for clone ownership and adoption regressions"]
    fn clone_without_sigchld_fixture() {
        let Ok(mode) = std::env::var("SSMCP_CLONE_FIXTURE") else {
            return;
        };
        assert!(matches!(mode.as_str(), "adopt" | "direct"));
        if let Ok(pid) = std::env::var("SSMCP_UNRELATED_FIXTURE_PID") {
            assert!(
                !guardian::kill_owned_child(pid.parse().unwrap()),
                "a PID that belongs to another parent must never be signalled"
            );
        }
        let mut pipes = [-1; 2];
        // The raw clone child only executes libc calls, never inherited Rust locks.
        let child = unsafe {
            libc::alarm(15);
            assert_eq!(libc::pipe2(pipes.as_mut_ptr(), libc::O_CLOEXEC), 0);
            let child = libc::syscall(
                libc::SYS_clone,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            );
            if child == 0 {
                libc::close(pipes[0]);
                libc::alarm(15);
                if libc::setsid() < 0 || libc::write(pipes[1], b"R".as_ptr().cast(), 1) != 1 {
                    libc::_exit(125);
                }
                libc::close(pipes[1]);
                loop {
                    libc::pause();
                }
            }
            assert!(child > 0, "clone without SIGCHLD failed");
            libc::close(pipes[1]);
            let mut ready = [0_u8; 1];
            assert_eq!(libc::read(pipes[0], ready.as_mut_ptr().cast(), 1), 1);
            libc::close(pipes[0]);
            child as libc::pid_t
        };
        println!("SSMCP_CLONE_PID {child}");
        std::io::Write::flush(&mut std::io::stdout()).unwrap();
        if mode == "direct" {
            assert!(
                guardian::kill_owned_child(child),
                "waitid must recognize an owned clone child without SIGCHLD"
            );
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(child, &mut status, libc::__WALL) },
                child
            );
            assert!(libc::WIFSIGNALED(status));
            assert_eq!(libc::WTERMSIG(status), libc::SIGKILL);
        }
        // In adopt mode the still-live clone child is reparented to the guardian.
        unsafe {
            libc::_exit(17);
        }
    }

    async fn cloned_fixture(
        mode: &str,
        unrelated: Option<u32>,
    ) -> (Managed, i32, BufReader<tokio::process::ChildStdout>) {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "backend::process::tests::clone_without_sigchld_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SSMCP_CLONE_FIXTURE", mode)
            .env_remove("SSMCP_UNRELATED_FIXTURE_PID")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(pid) = unrelated {
            command.env("SSMCP_UNRELATED_FIXTURE_PID", pid.to_string());
        }
        let mut managed = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(managed.child.stdout.take().unwrap());
        let pid = timeout(Duration::from_secs(5), async {
            loop {
                let mut line = String::new();
                assert_ne!(
                    stdout.read_line(&mut line).await.unwrap(),
                    0,
                    "fixture exited before reporting its child"
                );
                if let Some((_, tail)) = line.split_once("SSMCP_CLONE_PID ") {
                    break tail.trim().parse::<i32>().unwrap();
                }
            }
        })
        .await
        .expect("clone fixture must start within the deadline");
        (managed, pid, stdout)
    }

    #[tokio::test]
    async fn clone_without_sigchld_is_reaped_on_native_exit_and_closes_stdout() {
        use tokio::io::AsyncReadExt;
        let (mut managed, child, mut stdout) = cloned_fixture("adopt", None).await;
        let status = timeout(Duration::from_secs(5), managed.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(17));
        gone(child).await;
        let mut tail = Vec::new();
        timeout(Duration::from_secs(1), stdout.read_to_end(&mut tail))
            .await
            .unwrap()
            .unwrap();
        assert!(
            tail.is_empty(),
            "the adopted clone must release inherited output pipes"
        );
    }

    #[tokio::test]
    async fn child_identity_check_accepts_clone_children_and_rejects_unrelated_pids() {
        let mut unrelated = Command::new("python3")
            .args(["-c", "import time; time.sleep(15)"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (mut managed, child, _stdout) = cloned_fixture("direct", unrelated.id()).await;
        let status = timeout(Duration::from_secs(5), managed.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(17));
        gone(child).await;
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "the ownership check must leave the unrelated controlled fixture alive"
        );
        unrelated.kill().await.unwrap();
        timeout(Duration::from_secs(2), unrelated.wait())
            .await
            .unwrap()
            .unwrap();
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
            .find_map(|line| line.split_once("SSMCP_OWNED_PIDS ").map(|(_, tail)| tail))
            .expect("fixture must report owned identities")
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 3);
        gone(pids[1]).await;
        gone(pids[2]).await;
        // The guardian itself is adopted by the machine's init after the test
        // parent dies. Reaping its last child precedes its own exit; init then
        // controls reaping the guardian, so either absent or zombie is complete.
        let mut last_stat = None;
        let exited = timeout(Duration::from_secs(5), async {
            loop {
                match std::fs::read_to_string(format!("/proc/{}/stat", pids[0])) {
                    Ok(stat) => {
                        let zombie =
                            stat.rsplit_once(')').unwrap().1.split_whitespace().next() == Some("Z");
                        last_stat = Some(stat);
                        if zombie {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) => panic!("cannot observe guardian {}: {error}", pids[0]),
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            exited.is_ok(),
            "guardian {} did not exit after reaping its descendants; last stat: {last_stat:?}",
            pids[0]
        );
    }
}
