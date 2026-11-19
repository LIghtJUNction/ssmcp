//! Shared Linux private-socket ownership, peer and inode checks.
use std::{
    ffi::{CString, OsStr},
    fs::{self, File, Metadata},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};
use tokio::net::{UnixListener, UnixStream};

fn denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

pub(crate) fn syntax(path: &Path) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("local socket needs a filename"))?
        .as_bytes();
    if !path.is_absolute()
        || name.is_empty()
        || name.len() > 64
        || !name
            .iter()
            .copied()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(
            "local socket must be an absolute, normalized path with a short filename",
        ));
    }
    Ok(())
}
fn open_at(directory: &File, name: &OsStr, flags: i32) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| invalid("invalid local socket path"))?;
    // SAFETY: name and directory live for this call. The returned FD is owned.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) struct SocketPath {
    directory: File,
    name: PathBuf,
}
impl SocketPath {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        syntax(path)?;
        let mut directory = File::open("/")?;
        for component in path.parent().unwrap().components() {
            let Component::Normal(name) = component else {
                continue;
            };
            directory = open_at(&directory, name, libc::O_RDONLY | libc::O_DIRECTORY)?;
            let meta = directory.metadata()?;
            let mode = meta.mode();
            // Root-owned sticky temporary roots are allowed; the final parent
            // must still be private and owned by this process's effective UID.
            if ![0, uid()].contains(&meta.uid())
                || (mode & 0o022 != 0 && !(meta.uid() == 0 && mode & 0o1000 != 0))
            {
                return Err(denied("local socket ancestor is not trusted"));
            }
        }
        let meta = directory.metadata()?;
        if meta.uid() != uid() || meta.mode() & 0o777 != 0o700 {
            return Err(denied(
                "local socket parent must be daemon-owned and mode 0700",
            ));
        }
        Ok(Self {
            directory,
            name: PathBuf::from(path.file_name().unwrap()),
        })
    }
    pub(crate) fn anchored(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(&self.name)
    }
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        fs::symlink_metadata(self.anchored())
    }
    pub(crate) fn verify(&self) -> io::Result<Metadata> {
        let meta = self.metadata()?;
        if !meta.file_type().is_socket() || meta.uid() != uid() || meta.mode() & 0o777 != 0o600 {
            return Err(denied("local endpoint is not a private owned socket"));
        }
        Ok(meta)
    }
    fn lock(&self) -> io::Result<File> {
        let name = format!("{}.lock", self.name.to_string_lossy());
        let file = open_at(
            &self.directory,
            OsStr::new(&name),
            libc::O_RDWR | libc::O_CREAT | libc::O_NONBLOCK,
        )?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != uid()
            || meta.nlink() != 1
            || meta.mode() & 0o777 != 0o600
        {
            return Err(denied("local writer lock is not private"));
        }
        // SAFETY: file is a live descriptor. flock is held until its last close.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(file)
    }
    fn remove_if_same(&self, expected: &Metadata) -> io::Result<()> {
        let actual = self.verify()?;
        if actual.dev() != expected.dev() || actual.ino() != expected.ino() {
            return Err(denied("local endpoint changed during cleanup"));
        }
        fs::remove_file(self.anchored())
    }
}

pub(crate) fn peer(stream: &UnixStream) -> io::Result<()> {
    if stream.peer_cred()?.uid() != uid() {
        return Err(denied("local peer UID differs"));
    }
    Ok(())
}

pub(crate) struct Bound {
    pub(crate) listener: UnixListener,
    path: SocketPath,
    metadata: Metadata,
    _lock: File,
}
impl Bound {
    pub(crate) async fn bind(path: &Path) -> io::Result<Self> {
        let path = SocketPath::open(path)?;
        let lock = path.lock()?;
        match path.verify() {
            Ok(meta) => {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    UnixStream::connect(path.anchored()),
                )
                .await
                {
                    Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                        path.remove_if_same(&meta)?
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::AddrInUse,
                            "local socket is already active",
                        ));
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(path.anchored())?;
        if let Err(error) = fs::set_permissions(path.anchored(), fs::Permissions::from_mode(0o600))
        {
            let _ = fs::remove_file(path.anchored());
            return Err(error);
        }
        let metadata = path.verify()?;
        Ok(Self {
            listener,
            path,
            metadata,
            _lock: lock,
        })
    }
}
impl Drop for Bound {
    fn drop(&mut self) {
        let _ = self.path.remove_if_same(&self.metadata);
    }
}
