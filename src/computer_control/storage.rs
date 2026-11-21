//! Legacy state.json compatibility, using its original stable-inode locks.
//!
//! Old action consumers alone own input.lock. Publishing Off takes the state
//! lock, never input.lock: a busy input lease must not prevent revocation.
//! This module never replaces, removes or acquires the legacy input lock.
use super::State;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
};

#[cfg(unix)]
type Directory = File;
#[cfg(windows)]
type Directory = cap_std::fs::Dir;

const STATE_FILE: &str = "state.json";
const MAX_STATE_BYTES: u64 = 1024;
const STATE_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const SWITCH_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum Open {
    Existing,
    Create,
}

pub(super) struct Store {
    root: PathBuf,
}

impl Store {
    // Tests are deliberately unable to select the operator's shared directory.
    #[cfg(test)]
    pub(super) fn shared() -> Result<Self, String> {
        Err("real legacy computer-use state is unavailable in tests".into())
    }

    #[cfg(not(test))]
    pub(super) fn shared() -> Result<Self, String> {
        #[cfg(unix)]
        let root = {
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/var/tmp/ssmcp-host-input-{uid}"))
        };
        #[cfg(windows)]
        let root = {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or("computer-use state requires an absolute LOCALAPPDATA")?;
            base.join("ssmcp").join("host-input")
        };
        // No initialize(): merely reading status must not create default-On
        // state or alter a still-running older SSMCP session.
        Ok(Self { root })
    }

    /// A separate private sibling prevents a legacy input grant from authorizing observations.
    pub(super) fn observations(&self) -> Self {
        let mut directory = self.root.as_os_str().to_owned();
        directory.push("-zuse-observation");
        Self {
            root: directory.into(),
        }
    }

    #[cfg(test)]
    pub(super) fn at(root: PathBuf) -> Self {
        Self { root }
    }

    pub(super) fn read(&self) -> Result<Option<State>, String> {
        let directory = match self.directory(false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(context(error)),
        };
        let mut file = match open(&directory, STATE_FILE, Open::Existing) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(context(error)),
        };
        lock_state(&file, STATE_LOCK_TIMEOUT, true).map_err(context)?;
        read_state(&mut file)
    }

    /// A fixed inode is locked while reading and updating. A torn write after a
    /// crash is invalid and therefore off; another process never reads a partial
    /// update. The separate input lock cannot delay publishing an off transition.
    pub(super) fn update(&self, enabled: Option<bool>) -> Result<State, String> {
        self.update_with_timeout(enabled, SWITCH_LOCK_TIMEOUT)
    }

    fn update_with_timeout(
        &self,
        enabled: Option<bool>,
        lock_timeout: Duration,
    ) -> Result<State, String> {
        let directory = self.directory(true).map_err(context)?;
        let mut file = open(&directory, STATE_FILE, Open::Create).map_err(context)?;
        lock_state(&file, lock_timeout, false).map_err(context)?;
        let previous = match read_state(&mut file) {
            Ok(previous) => previous,
            // Explicit off is also the local repair path for an interrupted
            // state write. On and toggle never silently accept corrupt state.
            Err(_) if enabled == Some(false) => None,
            Err(error) => return Err(error),
        };
        let next = enabled.unwrap_or_else(|| !previous.unwrap_or_default().enabled);
        if let Some(previous) = previous
            && previous.enabled == next
        {
            return Ok(previous);
        }
        let generation = match super::generation() {
            Ok(generation) => generation,
            // Stopping input must not depend on a working entropy source.
            // A future on transition still requires a fresh random generation.
            Err(_) if !next => previous.unwrap_or_default().generation,
            Err(error) => return Err(error),
        };
        let state = State {
            schema_version: 1,
            enabled: next,
            generation,
        };
        write_state(&mut file, state)?;
        Ok(state)
    }

    fn directory(&self, create: bool) -> io::Result<Directory> {
        #[cfg(unix)]
        {
            if create {
                match std::fs::DirBuilder::new().mode(0o700).create(&self.root) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            let directory = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&self.root)?;
            let metadata = directory.metadata()?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(io::Error::other(
                    "computer-use directory must belong to this OS user with private permissions",
                ));
            }
            Ok(directory)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            };
            if create {
                if let Some(parent) = self.root.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                match crate::platform::private_directory(&self.root) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
            }
            // Open and verify an existing handle even for status. Unlike the
            // old creating helper, this cannot recreate an absent directory.
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&self.root)?;
            crate::platform::windows::verify_directory(&file)?;
            Ok(cap_std::fs::Dir::from_std_file(file))
        }
    }
}

fn context(error: io::Error) -> String {
    format!("computer-use state unavailable: {error}")
}

fn lock_busy(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // LockFileEx uses ERROR_LOCK_VIOLATION for a live competing owner.
        error.raw_os_error() == Some(33)
    }
    #[cfg(unix)]
    {
        false
    }
}

fn lock_state(file: &File, timeout: Duration, shared: bool) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = if shared {
            file.try_lock_shared().map_err(io::Error::from)
        } else {
            crate::platform::lock_file(file, true)
        };
        match attempt {
            Ok(()) => return Ok(()),
            Err(error) if lock_busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if lock_busy(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for computer-use state lock after {} ms: {error}",
                        timeout.as_millis()
                    ),
                ));
            }
            Err(error) => return Err(error),
        }
    }
}

fn read_state(file: &mut File) -> Result<Option<State>, String> {
    if file.metadata().map_err(context)?.len() > MAX_STATE_BYTES {
        return Err("computer-use state exceeds its size limit".into());
    }
    file.seek(SeekFrom::Start(0)).map_err(context)?;
    let mut data = Vec::new();
    file.take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(context)?;
    if data.is_empty() {
        return Ok(None);
    }
    if data.len() as u64 > MAX_STATE_BYTES {
        return Err("computer-use state exceeds its size limit".into());
    }
    let state: State = serde_json::from_slice(&data)
        .map_err(|_| "invalid computer-use state; use /computer off to reset it".to_owned())?;
    if state.schema_version != 1 || (state.enabled && state.generation == [0; 16]) {
        return Err("invalid computer-use state; use /computer off to reset it".into());
    }
    Ok(Some(state))
}

fn write_state(file: &mut File, state: State) -> Result<(), String> {
    let data = serde_json::to_vec(&state).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(0)).map_err(context)?;
    file.write_all(&data).map_err(context)?;
    file.set_len(data.len() as u64).map_err(context)?;
    file.sync_all().map_err(context)
}

fn open(directory: &Directory, name: &str, access: Open) -> io::Result<File> {
    // Lock files must keep a stable inode. Never truncate or replace one before
    // taking its OS lock; only remove these files when no clients are running.
    #[cfg(unix)]
    {
        let name = CString::new(name).map_err(io::Error::other)?;
        let create_flags = match access {
            Open::Existing => 0,
            Open::Create => libc::O_CREAT,
        };
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | create_flags,
                0o600,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(io::Error::other(
                "computer-use state must be a private, singly linked regular file",
            ));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
        let mut options = cap_std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(matches!(access, Open::Create))
            .follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options)?.into_std();
        crate::platform::windows::verify_file(&file)?;
        Ok(file)
    }
}
