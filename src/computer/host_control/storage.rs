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
const STATE_LOCK_TIMEOUT: Duration = Duration::from_millis(100);

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
        let _ = self.store.update(Some(false));
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
        lock_state(&file).map_err(context)?;
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
        lock_state(&file).map_err(context)?;
        read_state(&mut file).map(|state| state.unwrap_or_default())
    }

    /// A fixed inode is locked while reading and updating. A torn write after a
    /// crash is invalid and therefore off; another process never reads a partial
    /// update. The separate input lock cannot delay publishing an off transition.
    pub(super) fn update(&self, enabled: Option<bool>) -> Result<State, String> {
        let directory = self.directory(true).map_err(context)?;
        let mut file = open(&directory, STATE_FILE, Open::Create).map_err(context)?;
        lock_state(&file).map_err(context)?;
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

fn lock_state(file: &File) -> io::Result<()> {
    let deadline = Instant::now() + STATE_LOCK_TIMEOUT;
    loop {
        match crate::platform::lock_file(file, true) {
            Ok(()) => return Ok(()),
            Err(error) if lock_busy(&error) && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1));
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
