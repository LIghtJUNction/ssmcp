//! Explicit, coordinated display selection and proof that the new server owns it.
use super::{Managed, OwnedX11File};
use std::{
    collections::HashSet,
    ffi::{CStr, CString},
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

const FIRST: u16 = 10_000;
const END: u16 = 30_000;
const ATTEMPTS: u16 = 64;
const SOCKET_DIRECTORY: &str = "/tmp/.X11-unix";

pub(super) struct Reservation {
    number: u16,
    // Keep the same-UID lock through spawn, readiness and endpoint validation.
    lock: AllocationLock,
}

impl Reservation {
    pub(super) async fn acquire(entropy: u64) -> Result<Self, String> {
        Self::acquire_from(FIRST + (entropy % u64::from(END - FIRST)) as u16).await
    }

    pub(super) async fn acquire_from(start: u16) -> Result<Self, String> {
        if !(FIRST..END).contains(&start) {
            return Err("private display selection is outside the reserved high range".into());
        }
        let lock = AllocationLock::acquire_in(Path::new("/tmp")).await?;
        check_socket_directory()?;
        let endpoints = kernel_endpoints()?;
        let host = std::env::var_os("DISPLAY")
            .as_deref()
            .and_then(|value| display_number(&value.to_string_lossy()));
        for offset in 0..ATTEMPTS {
            let number = candidate(start, offset);
            if host != Some(number) && unreserved(number, &endpoints)? {
                return Ok(Self { number, lock });
            }
        }
        Err(format!(
            "no unreserved private display in {ATTEMPTS} high-range candidates; host display was not used"
        ))
    }

    pub(super) fn number(&self) -> u16 {
        self.number
    }

    pub(super) fn verify(
        &self,
        process: &mut Managed,
        owned: &mut Vec<OwnedX11File>,
    ) -> Result<(), String> {
        self.lock.ensure_current()?;
        check_socket_directory()?;
        let guardian = live_guardian(process)?;
        let path = socket_path(self.number);
        // Pin before connecting, then compare the name again after checking the
        // peer. Never discover cleanup ownership by a later fresh path lookup.
        let pin = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(|error| format!("could not pin private X11 socket: {error}"))?;
        let metadata = pin.metadata().map_err(|error| error.to_string())?;
        if !metadata.file_type().is_socket() || metadata.uid() != effective_uid() {
            return Err("private X11 socket has an unexpected type or owner".into());
        }
        let peer = connect_peer(&path, false)?;
        if peer.credentials.uid != effective_uid() || peer.credentials.pid <= 0 {
            return Err(
                "private X11 socket belongs to another user or has no process identity".into(),
            );
        }
        let server = peer.credentials.pid;
        let server_handle = crate::pty::open_pidfd(server)
            .map_err(|error| format!("could not pin Xvfb process identity: {error}"))?;
        let stat = read_bounded(&format!("/proc/{server}/stat"), 16 * 1024)?;
        if process_parent(&String::from_utf8_lossy(&stat)) != Some(guardian) {
            return Err("private X11 socket is not owned by the newly spawned Xvfb child".into());
        }
        ensure_named_inode(&path, &metadata)?;
        ensure_alive(&server_handle)?;
        if live_guardian(process)? != guardian {
            return Err("private Xvfb guardian changed during readiness".into());
        }
        // From this point the parent keeps the proven inode through process
        // reaping, including if the remaining checks reject a competing alias.
        owned.push(OwnedX11File {
            path: path.clone(),
            device: metadata.dev(),
            inode: metadata.ino(),
            socket: true,
            _pin: pin,
        });

        let endpoints = kernel_endpoints()?;
        // -displayfd suppresses Xserver's PID lock. Any lock or underscore alias
        // is a competing reservation, never a file we may adopt or remove.
        if competing_reservation(self.number, &endpoints)? {
            return Err("private display gained a competing lock or alias during startup".into());
        }
        let abstract_name = format!("@{}", path.display());
        let abstract_peer = if endpoints.contains(&abstract_name) {
            let other = connect_peer(&path, true)?;
            if other.credentials.pid != server || other.credentials.uid != peer.credentials.uid {
                return Err(
                    "private X11 filesystem and abstract sockets have different owners".into(),
                );
            }
            Some(other)
        } else {
            None
        };
        ensure_named_inode(&path, &metadata)?;
        ensure_alive(&server_handle)?;
        if live_guardian(process)? != guardian {
            return Err("private Xvfb guardian changed during readiness".into());
        }
        drop(abstract_peer);
        drop(peer);
        Ok(())
    }
}

fn candidate(start: u16, offset: u16) -> u16 {
    FIRST + (start - FIRST + offset) % (END - FIRST)
}

fn display_number(display: &str) -> Option<u16> {
    let value = display.rsplit_once(':')?.1;
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    value[..digits].parse().ok()
}

fn socket_path(number: u16) -> PathBuf {
    PathBuf::from(format!("{SOCKET_DIRECTORY}/X{number}"))
}

fn competing_reservation(number: u16, endpoints: &HashSet<String>) -> Result<bool, String> {
    for path in [
        PathBuf::from(format!("{SOCKET_DIRECTORY}/X{number}_")),
        PathBuf::from(format!("/tmp/.X{number}-lock")),
        PathBuf::from(format!("/tmp/.X{number}_-lock")),
    ] {
        if path_reserved(&path)? {
            return Ok(true);
        }
    }
    let alias = format!("{SOCKET_DIRECTORY}/X{number}_");
    Ok(endpoints.contains(&alias) || endpoints.contains(&format!("@{alias}")))
}

fn unreserved(number: u16, endpoints: &HashSet<String>) -> Result<bool, String> {
    let path = socket_path(number);
    let name = path.to_string_lossy();
    Ok(!path_reserved(&path)?
        && !endpoints.contains(name.as_ref())
        && !endpoints.contains(&format!("@{name}"))
        && !competing_reservation(number, endpoints)?)
}

fn path_reserved(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true), // Includes dangling symlinks and unexpected file types.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("could not inspect a display reservation: {error}")),
    }
}

fn check_socket_directory() -> Result<(), String> {
    match std::fs::symlink_metadata(SOCKET_DIRECTORY) {
        Ok(metadata) => trusted_parent(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect the X11 socket directory: {error}"
        )),
    }
}

fn read_bounded(path: &str, limit: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(limit as u64 + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("could not inspect private display ownership: {error}"))?;
    if bytes.len() > limit {
        return Err("private display ownership inspection exceeded its size limit".into());
    }
    Ok(bytes)
}

fn kernel_endpoints() -> Result<HashSet<String>, String> {
    let bytes = read_bounded("/proc/net/unix", 4 * 1024 * 1024)?;
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().nth(7))
        .map(str::to_owned)
        .collect())
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory effects.
    unsafe { libc::geteuid() }
}

fn trusted_parent(metadata: &std::fs::Metadata) -> Result<(), String> {
    if !metadata.file_type().is_dir()
        || ![0, effective_uid()].contains(&metadata.uid())
        || (metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0)
    {
        return Err(
            "private display parent must be an owned directory with protected entries".into(),
        );
    }
    Ok(())
}

/// All cooperating clients use one stable inode, independent of TMPDIR/XDG.
/// It is deliberately retained: unlinking a flock file can create two lock domains.
struct AllocationLock {
    directory: File,
    file: File,
    path: PathBuf,
}

impl AllocationLock {
    async fn acquire_in(root: &Path) -> Result<Self, String> {
        let lock = Self::open(root)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match lock.file.try_lock() {
                Ok(()) => {
                    lock.ensure_current()?;
                    return Ok(lock);
                }
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(error) => {
                    return Err(format!(
                        "could not lock private display allocation: {error}"
                    ));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("private display allocation remained busy for five seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn open(root: &Path) -> Result<Self, String> {
        let parent = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .map_err(|error| format!("could not open private display lock parent: {error}"))?;
        trusted_parent(&parent.metadata().map_err(|error| error.to_string())?)?;
        let name = CString::new(format!("ssmcp-x11-{}", effective_uid())).unwrap();
        // SAFETY: the parent descriptor and NUL-terminated name remain valid.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(format!(
                    "could not create private display lock directory: {error}"
                ));
            }
        }
        let directory = open_at(&parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(|error| format!("could not open private display lock directory: {error}"))?;
        let metadata = directory.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_dir()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(
                "private display lock directory must be owned by this UID with mode 0700".into(),
            );
        }
        let file = open_at(
            &directory,
            c"allocate.lock",
            libc::O_CREAT | libc::O_RDWR | libc::O_NONBLOCK,
            0o600,
        )
        .map_err(|error| format!("could not open private display allocation lock: {error}"))?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(
                "private display lock must be an owned, single-link file with mode 0600".into(),
            );
        }
        let lock = Self {
            directory,
            file,
            path: root.join(name.to_str().unwrap()),
        };
        lock.ensure_current()?;
        Ok(lock)
    }

    fn ensure_current(&self) -> Result<(), String> {
        ensure_named_inode(
            &self.path,
            &self
                .directory
                .metadata()
                .map_err(|error| error.to_string())?,
        )?;
        ensure_named_inode(
            &self.path.join("allocate.lock"),
            &self.file.metadata().map_err(|error| error.to_string())?,
        )
    }
}

fn open_at(directory: &File, name: &CStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: openat borrows the directory and name, returning a new owned fd.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: this successful openat descriptor has no other Rust owner.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn ensure_named_inode(path: &Path, expected: &std::fs::Metadata) -> Result<(), String> {
    let found = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if found.dev() != expected.dev()
        || found.ino() != expected.ino()
        || found.file_type() != expected.file_type()
    {
        return Err("private display ownership changed during verification".into());
    }
    Ok(())
}

struct Peer {
    credentials: libc::ucred,
    _socket: OwnedFd,
}

fn connect_peer(path: &Path, abstract_socket: bool) -> Result<Peer, String> {
    // SAFETY: zero initializes the sockaddr including its pathname storage.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() + 1 > address.sun_path.len() || bytes.contains(&0) {
        return Err("private X11 socket address is invalid".into());
    }
    let offset = usize::from(abstract_socket);
    for (target, byte) in address.sun_path[offset..].iter_mut().zip(bytes) {
        *target = *byte as libc::c_char;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    // Nonblocking connect cannot hang on another server's full accept queue.
    // SAFETY: socket returns a new descriptor without borrowing memory.
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(format!(
            "could not open X11 ownership probe: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: this successful socket descriptor has no other Rust owner.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: address is initialized and length fits its sockaddr_un allocation.
    if unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    } != 0
    {
        return Err(format!(
            "could not connect X11 ownership probe: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: getsockopt writes only within the initialized ucred allocation.
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&credentials) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut size,
        )
    } != 0
        || size as usize != std::mem::size_of_val(&credentials)
    {
        return Err("could not establish private X11 peer credentials".into());
    }
    Ok(Peer {
        credentials,
        _socket: socket,
    })
}

fn live_guardian(process: &mut Managed) -> Result<u32, String> {
    match process.child.try_wait() {
        Ok(None) => process
            .child
            .id()
            .ok_or_else(|| "Xvfb guardian has no process identity".into()),
        Ok(Some(_)) => Err("Xvfb exited before private display readiness completed".into()),
        Err(error) => Err(format!("could not inspect Xvfb guardian: {error}")),
    }
}

fn process_parent(stat: &str) -> Option<u32> {
    let (_, fields) = stat.rsplit_once(") ")?;
    let mut fields = fields.split_whitespace();
    if matches!(fields.next()?, "Z" | "X" | "x") {
        return None;
    }
    fields.next()?.parse().ok()
}

fn ensure_alive(process: &OwnedFd) -> Result<(), String> {
    let mut poll = libc::pollfd {
        fd: process.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll borrows one initialized descriptor and never waits.
    match unsafe { libc::poll(&mut poll, 1, 0) } {
        0 => Ok(()),
        _ => Err("Xvfb process exited or could not be verified during readiness".into()),
    }
}

#[cfg(test)]
mod tests;
