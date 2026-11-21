//! Machine-local, OS-user-private foreground input state and process locks.
use super::State;
#[cfg(test)]
use std::path::Path;
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::Arc,
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
const INPUT_LOCK: &str = "input.lock";
const MAX_STATE_BYTES: u64 = 1024;
// Observation and automatic release must fail closed promptly. An explicit
// local switch can wait longer for another session's durable state update.
const STATE_LOCK_TIMEOUT: Duration = Duration::from_millis(100);
const SWITCH_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum Open {
    Existing,
    Create,
    CreateNew,
}

pub(super) struct Store {
    root: PathBuf,
    #[cfg(test)]
    remove_on_drop: bool,
}

/// Keep both the OS lock and its test directory alive through release cleanup.
pub(in crate::computer) struct InputLease {
    _file: File,
    store: Arc<Store>,
}

impl InputLease {
    pub(in crate::computer) fn revoke(&self) {
        let _ = self
            .store
            .update_with_timeout(Some(false), STATE_LOCK_TIMEOUT);
    }
}

impl Store {
    #[cfg(not(test))]
    pub(super) fn shared() -> Result<Self, String> {
        #[cfg(unix)]
        let root = {
            // Unlike TMPDIR, a fixed machine-local root is shared by every
            // terminal and workspace for this effective OS user.
            let uid = unsafe { libc::geteuid() };
            PathBuf::from(format!("/var/tmp/ssmcp-host-input-{uid}"))
        };
        #[cfg(windows)]
        let root = {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or("foreground input state requires an absolute LOCALAPPDATA")?;
            base.join("ssmcp").join("host-input")
        };
        let store = Self {
            root,
            #[cfg(test)]
            remove_on_drop: false,
        };
        store.initialize()?;
        Ok(store)
    }

    #[cfg(test)]
    pub(super) fn temporary() -> Result<Self, String> {
        let generation = super::generation()?;
        let store = Self {
            root: std::env::temp_dir().join(format!(
                "ssmcp-host-input-test-{}-{}",
                std::process::id(),
                super::generation_text(generation)
            )),
            remove_on_drop: true,
        };
        store.initialize()?;
        Ok(store)
    }

    #[cfg(test)]
    pub(super) fn at(root: PathBuf) -> Self {
        Self {
            root,
            remove_on_drop: false,
        }
    }

    #[cfg(test)]
    pub(super) fn path(&self) -> &Path {
        &self.root
    }

    /// Only the process that creates a genuinely new private directory may
    /// install the default-on setting. Existing missing/invalid state is off.
    pub(super) fn initialize(&self) -> Result<(), String> {
        #[cfg(windows)]
        if let Some(parent) = self.root.parent() {
            std::fs::create_dir_all(parent).map_err(context)?;
        }
        let created = match crate::platform::private_directory(&self.root) {
            Ok(()) => true,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(context(error)),
        };
        let directory = self.directory(false).map_err(context)?;
        if !created {
            return Ok(());
        }
        self.initialize_created(&directory)
    }

    fn initialize_created(&self, directory: &Directory) -> Result<(), String> {
        let mut file = match open(directory, STATE_FILE, Open::CreateNew) {
            Ok(file) => file,
            // Another local session may have explicitly switched off between
            // creating the directory and reaching the state file.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
            Err(error) => return Err(context(error)),
        };
        lock_state(&file, STATE_LOCK_TIMEOUT).map_err(context)?;
        if read_state(&mut file)?.is_some() {
            return Ok(());
        }
        write_state(
            &mut file,
            State {
                schema_version: 1,
                enabled: true,
                generation: super::generation()?,
            },
        )?;
        Ok(())
    }

    pub(super) fn read(&self) -> Result<State, String> {
        let directory = match self.directory(false) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) => return Err(context(error)),
        };
        let mut file = match open(&directory, STATE_FILE, Open::Existing) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(State::default()),
            Err(error) => return Err(context(error)),
        };
        lock_state(&file, STATE_LOCK_TIMEOUT).map_err(context)?;
        read_state(&mut file).map(|state| state.unwrap_or_default())
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
        lock_state(&file, lock_timeout).map_err(context)?;
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

    pub(super) fn try_input(self: &Arc<Self>) -> Result<Option<InputLease>, String> {
        let directory = self.directory(false).map_err(context)?;
        let file = open(&directory, INPUT_LOCK, Open::Create).map_err(context)?;
        match crate::platform::lock_file(&file, true) {
            Ok(()) => Ok(Some(InputLease {
                _file: file,
                store: self.clone(),
            })),
            Err(error) if lock_busy(&error) => Ok(None),
            Err(error) => Err(context(error)),
        }
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
                    "foreground input directory must belong to this OS user with private permissions",
                ));
            }
            Ok(directory)
        }
        #[cfg(windows)]
        {
            if create {
                if let Some(parent) = self.root.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            } else {
                // The platform helper validates ownership, protected ACLs and
                // reparse points, but can also create an absent directory.
                std::fs::symlink_metadata(&self.root)?;
            }
            crate::platform::windows::private_directory(&self.root)
        }
    }
}

#[cfg(test)]
impl Drop for Store {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

fn context(error: io::Error) -> String {
    format!("foreground input state unavailable: {error}")
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

fn lock_state(file: &File, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match crate::platform::lock_file(file, true) {
            Ok(()) => return Ok(()),
            Err(error) if lock_busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if lock_busy(&error) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "timed out waiting for foreground input state lock after {} ms: {error}",
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
        return Err("foreground input state exceeds its size limit".into());
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
        return Err("foreground input state exceeds its size limit".into());
    }
    let state: State = serde_json::from_slice(&data)
        .map_err(|_| "invalid foreground input state; use /computer off to reset it".to_owned())?;
    if state.schema_version != 1 || (state.enabled && state.generation == [0; 16]) {
        return Err("invalid foreground input state; use /computer off to reset it".into());
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
            Open::CreateNew => libc::O_CREAT | libc::O_EXCL,
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
                "foreground input state must be a private, singly linked regular file",
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
            .create_new(matches!(access, Open::CreateNew))
            .follow(FollowSymlinks::No);
        let file = directory.open_with(name, &options)?.into_std();
        crate::platform::windows::verify_file(&file)?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_switch_waits_while_observation_and_revocation_fail_closed() {
        use std::sync::mpsc::{self, TryRecvError};

        for enabled in [Some(false), None] {
            let control = super::super::HostControl::default();
            let store = control.store().unwrap();
            let before = store.read().unwrap();
            let ticket = control.ticket().unwrap();
            let lease = store.try_input().unwrap().unwrap();
            let directory = store.directory(false).unwrap();
            let mut held = open(&directory, STATE_FILE, Open::Existing).unwrap();
            lock_state(&held, STATE_LOCK_TIMEOUT).unwrap();

            let operator = super::super::HostControl::at(store.path().to_owned());
            let (ready_tx, ready_rx) = mpsc::sync_channel(0);
            let (done_tx, done_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let result = match enabled {
                    Some(value) => operator.set_local(value),
                    None => operator.toggle_local(),
                };
                done_tx.send(result).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();

            // These real lock attempts each exhaust the short observation or
            // cleanup budget while the explicit operator command remains queued.
            let blocked_status = control.status();
            let old_ticket_valid = control.valid(ticket);
            lease.revoke();
            let retained = read_state(&mut held).unwrap().unwrap();
            let early = done_rx.try_recv();
            let completed_while_locked = early.is_ok();
            drop(held);
            let result = match early {
                Ok(result) => result,
                Err(TryRecvError::Empty) => done_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
                Err(TryRecvError::Disconnected) => panic!("operator worker disconnected"),
            };
            worker.join().unwrap();

            assert!(
                !completed_while_locked,
                "{enabled:?}: switch stopped waiting"
            );
            assert_eq!(blocked_status["enabled"], false);
            assert!(
                blocked_status["error"]
                    .as_str()
                    .unwrap()
                    .contains("timed out waiting for foreground input state lock")
            );
            assert!(
                !old_ticket_valid,
                "a held state lock cannot authorize input"
            );
            assert_eq!(
                retained, before,
                "timed-out observation/revoke must not write"
            );
            let status = result.unwrap();
            let after = store.read().unwrap();
            assert!(!after.enabled);
            assert_ne!(after.generation, before.generation);
            assert_eq!(status["enabled"], false);
            assert_eq!(
                status["generation"],
                super::super::generation_text(after.generation)
            );
            assert!(control.ticket().is_err());
            assert!(
                store.try_input().unwrap().is_none(),
                "publishing off does not require releasing the existing input lease"
            );
            drop(lease);
            assert!(store.try_input().unwrap().is_some());
        }
    }

    #[test]
    fn state_lock_timeout_preserves_the_last_committed_state() {
        let store = Store::temporary().unwrap();
        let before = store.read().unwrap();
        let directory = store.directory(false).unwrap();
        let held = open(&directory, STATE_FILE, Open::Existing).unwrap();
        lock_state(&held, STATE_LOCK_TIMEOUT).unwrap();
        let competing = open(&directory, STATE_FILE, Open::Existing).unwrap();
        let error = lock_state(&competing, Duration::ZERO).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        let operator = Store::at(store.path().to_owned());
        let failure = operator
            .update_with_timeout(None, Duration::from_millis(20))
            .unwrap_err();
        drop(competing);
        drop(held);
        assert!(failure.contains("timed out waiting for foreground input state lock"));
        assert_eq!(
            store.read().unwrap(),
            before,
            "a timed-out switch is not replayed"
        );
        let off = operator.update(Some(false)).unwrap();
        assert!(!off.enabled);
        assert_ne!(off.generation, before.generation);
        assert_eq!(store.read().unwrap(), off);
    }

    #[test]
    fn another_session_is_off_until_the_directory_creator_publishes_state() {
        let creator = Store::temporary().unwrap();
        // Pause the creation path after its private directory exists.
        std::fs::remove_file(creator.path().join(STATE_FILE)).unwrap();
        let directory = creator.directory(false).unwrap();
        let observer = Store::at(creator.path().to_owned());
        observer.initialize().unwrap();
        assert!(!observer.read().unwrap().enabled);
        assert!(!creator.path().join(STATE_FILE).exists());
        creator.initialize_created(&directory).unwrap();
        assert!(observer.read().unwrap().enabled);
    }

    #[test]
    fn explicit_off_wins_over_a_delayed_first_initialization() {
        let creator = Store::temporary().unwrap();
        std::fs::remove_file(creator.path().join(STATE_FILE)).unwrap();
        let directory = creator.directory(false).unwrap();
        let operator = Store::at(creator.path().to_owned());
        let off = operator.update(Some(false)).unwrap();
        creator.initialize_created(&directory).unwrap();
        assert_eq!(creator.read().unwrap(), off);
    }
}
