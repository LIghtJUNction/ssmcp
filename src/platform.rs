//! Small operating-system differences used by the foreground client.
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub struct Signal(tokio::signal::unix::Signal);
#[cfg(windows)]
pub enum Signal {
    Interrupt(tokio::signal::windows::CtrlC),
    Terminate(tokio::signal::windows::CtrlBreak),
}

impl Signal {
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.0.recv().await;
        }
        #[cfg(windows)]
        match self {
            Self::Interrupt(signal) => {
                signal.recv().await;
            }
            Self::Terminate(signal) => {
                signal.recv().await;
            }
        }
    }
}

pub fn interrupt_signal() -> io::Result<Signal> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map(Signal)
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_c().map(Signal::Interrupt)
    }
}

pub fn terminate_signal() -> io::Result<Signal> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map(Signal)
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_break().map(Signal::Terminate)
    }
}

pub fn default_shell() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32/cmd.exe")
            })
    }
}

pub fn hostname() -> io::Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 256];
        // SAFETY: the buffer is writable for the supplied length, with a spare NUL.
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len() - 1) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..end].to_vec()).map_err(io::Error::other)
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").map_err(io::Error::other)
    }
}

pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
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
    #[cfg(windows)]
    {
        path.extension().is_some_and(|ext| {
            ["exe", "com", "cmd", "bat"]
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
    }
}

pub fn executable(name: &str) -> Result<PathBuf, String> {
    crate::program::executable(name)
}

pub fn random_bytes(bytes: &mut [u8]) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(bytes)
    }
    #[cfg(not(target_os = "linux"))]
    {
        getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
    }
}

pub fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        std::fs::create_dir(path)
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn stop_unix_descendants(owner: libc::pid_t) {
    // The owner is still unreaped. Include shell jobs in separate process groups.
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .output();
    if let Ok(output) = output {
        let rows: Vec<(i32, i32)> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut fields = line.split_whitespace();
                Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
            })
            .collect();
        let mut owned = vec![owner];
        for _ in 0..256 {
            let old = owned.len();
            for (pid, parent) in &rows {
                if *pid > 1 && owned.contains(parent) && !owned.contains(pid) {
                    owned.push(*pid);
                }
            }
            if old == owned.len() {
                break;
            }
        }
        for pid in owned.into_iter().rev() {
            // SAFETY: signal the observed child tree rooted at the still-owned leader.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    // SAFETY: negative owner addresses only the process group created for this child.
    unsafe {
        libc::kill(-owner, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub(crate) struct WindowsJob(std::os::windows::io::OwnedHandle);

#[cfg(windows)]
pub(crate) fn resume_process(pid: u32) -> io::Result<()> {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::{Diagnostics::ToolHelp::*, Threading::*},
    };
    // The child starts suspended so even a fast batch wrapper cannot launch
    // descendants before it belongs to the kill-on-close job.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
    while found {
        if entry.th32OwnerProcessID == pid {
            let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            return Ok(());
        }
        found = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Err(io::Error::other(
        "suspended child primary thread was not found",
    ))
}

#[cfg(windows)]
impl WindowsJob {
    pub fn new() -> io::Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        use windows_sys::Win32::System::JobObjects::*;
        // SAFETY: null parameters create an unnamed job with the caller's security descriptor.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the job and correctly sized limit structure remain live for this call.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }
    pub fn assign(&self, process: std::os::windows::io::RawHandle) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are borrowed live kernel objects.
        if unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0.as_raw_handle(),
                process,
            )
        } == 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn assign_pid(&self, pid: u32) -> io::Result<()> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::*;
        // SAFETY: acquire only the access required to add our child to this job.
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        self.assign(process.as_raw_handle())
    }
    pub fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: this unnamed job only owns children started by this client.
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0.as_raw_handle(), 1);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct DirectoryLock {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(windows)]
    handle: std::os::windows::io::OwnedHandle,
    // Windows mutex ownership belongs to a thread, so guards must stay on it.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn lock_directory(path: &Path) -> Result<DirectoryLock, String> {
    #[cfg(unix)]
    {
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            match lock_file(&file, true) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        Ok(DirectoryLock {
            _file: file,
            _thread: std::marker::PhantomData,
        })
    }
    #[cfg(windows)]
    {
        use sha2::{Digest, Sha256};
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::{
            Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0},
            System::Threading::*,
        };
        let digest = Sha256::digest(path.to_string_lossy().to_lowercase().as_bytes());
        let name: Vec<u16> = format!("Local\\ssmcp-workspace-{digest:x}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        // SAFETY: name is NUL-terminated; default security limits this to the caller's session.
        let raw = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error().to_string());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let result = unsafe { WaitForSingleObject(handle.as_raw_handle(), 500) };
        if result != WAIT_OBJECT_0 && result != WAIT_ABANDONED {
            return Err("workspace directory is busy; retry the file operation".into());
        }
        Ok(DirectoryLock {
            handle,
            _thread: std::marker::PhantomData,
        })
    }
}

#[cfg(windows)]
impl Drop for DirectoryLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: this guard acquired the mutex on the current thread.
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.handle.as_raw_handle());
        }
    }
}

pub(crate) fn lock_file(file: &std::fs::File, nonblocking: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: the borrowed file descriptor stays live throughout flock.
            if unsafe {
                libc::flock(
                    file.as_raw_fd(),
                    libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 },
                )
            } == 0
            {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{
            Storage::FileSystem::*,
            System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0},
        };
        // Reserve a byte beyond any artifact's range. Windows byte-range locks
        // are mandatory; locking data bytes would block readers of live output.
        let mut overlapped = OVERLAPPED {
            Anonymous: OVERLAPPED_0 {
                Anonymous: OVERLAPPED_0_0 {
                    Offset: u32::MAX - 1,
                    OffsetHigh: u32::MAX,
                },
            },
            ..OVERLAPPED::default()
        };
        let flags = LOCKFILE_EXCLUSIVE_LOCK
            | if nonblocking {
                LOCKFILE_FAIL_IMMEDIATELY
            } else {
                0
            };
        // SAFETY: this synchronous file handle and writable OVERLAPPED are live for the call.
        if unsafe { LockFileEx(file.as_raw_handle(), flags, 0, 1, 0, &mut overlapped) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_defaults_and_entropy_are_available() {
        assert!(default_shell().is_absolute());
        assert!(!hostname().unwrap().is_empty());
        let mut one = [0; 32];
        let mut two = [0; 32];
        random_bytes(&mut one).unwrap();
        random_bytes(&mut two).unwrap();
        assert_ne!(one, two);
    }
}
