use super::{ENV, ID_ENV, INTERVAL, STALE_AFTER, Sample, Sampler, Status};
use std::{
    io,
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::net::UnixDatagram,
    },
    time::Instant,
};
use tokio::{process::Command, sync::watch, task::JoinHandle};

pub(in crate::client) struct Prepared {
    socket: UnixDatagram,
}

fn above_stdio(socket: UnixDatagram) -> io::Result<UnixDatagram> {
    if socket.as_raw_fd() >= 3 {
        return Ok(socket);
    }
    // SAFETY: duplicate our live socket into a new, unoccupied FD; never replace
    // any of the caller's descriptors, including a closed standard stream.
    let fd = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { UnixDatagram::from_raw_fd(fd) })
}

pub(super) fn prepare(command: &mut Command) -> Option<Prepared> {
    command.env_remove(ENV).env_remove(ID_ENV);
    let (parent, child) = UnixDatagram::pair().ok()?;
    let parent = above_stdio(parent).ok()?;
    let child = above_stdio(child).ok()?;
    parent.set_nonblocking(true).ok()?;
    child.set_nonblocking(true).ok()?;
    command
        .env(ENV, child.as_raw_fd().to_string())
        .env(ID_ENV, socket_identity(child.as_raw_fd()).ok()?);
    // SAFETY: the closure only uses fcntl on its already owned socket. Because
    // that FD is occupied before Command allocates its exec-error pipe, it cannot
    // collide with that pipe or another startup FD. No dup2 or allocation occurs.
    unsafe {
        command.pre_exec(move || {
            // Optional metrics must not prevent the relay from starting.
            let _ = inherit_descriptor(child.as_raw_fd());
            Ok(())
        });
    }
    Some(Prepared { socket: parent })
}

unsafe fn inherit_descriptor(source: RawFd) -> io::Result<()> {
    // SAFETY: source is the live socket captured by the private relay command.
    let flags = unsafe { libc::fcntl(source, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(source, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct Reporter(UnixDatagram);

pub(super) fn reporter() -> Option<Reporter> {
    let fd: RawFd = std::env::var(ENV).ok()?.parse().ok()?;
    if fd < 3 || std::env::var(ID_ENV).ok()? != socket_identity(fd).ok()? {
        return None;
    }
    claim_reporter(fd).ok()
}

fn socket_identity(fd: RawFd) -> io::Result<String> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat validates fd and initializes this correctly sized output.
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private metrics socket unavailable",
        ));
    }
    Ok(format!("{}:{}", stat.st_dev, stat.st_ino))
}

fn claim_reporter(fd: RawFd) -> io::Result<Reporter> {
    // Restore CLOEXEC before doing anything that could start a descendant.
    // SAFETY: the private environment identifies the same owned socket inode.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut kind: libc::c_int = 0;
    let mut length = std::mem::size_of_val(&kind) as libc::socklen_t;
    // SAFETY: both outputs point to initialized values of the advertised size.
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut length,
        )
    } < 0
        || kind != libc::SOCK_DGRAM
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private metrics socket unavailable",
        ));
    }
    // SAFETY: only this private entry point takes ownership, once per process.
    let socket = unsafe { UnixDatagram::from_raw_fd(fd) };
    socket.set_nonblocking(true)?;
    Ok(Reporter(socket))
}

impl Reporter {
    pub(super) fn send(&self, bytes: &[u8; 32]) -> io::Result<()> {
        self.0.send(bytes).map(|_| ()).map_err(|error| {
            // Darwin can report a full datagram queue as ENOBUFS. Keep the
            // reporter alive and drop this sample, as with EWOULDBLOCK.
            if error.raw_os_error() == Some(libc::ENOBUFS) {
                io::Error::new(io::ErrorKind::WouldBlock, error)
            } else {
                error
            }
        })
    }
}

pub(super) fn observe(
    prepared: Option<Prepared>,
    closed: watch::Receiver<Option<String>>,
    mut report: impl FnMut(Status) + Send + 'static,
) -> JoinHandle<()> {
    let socket =
        prepared.and_then(|prepared| tokio::net::UnixDatagram::from_std(prepared.socket).ok());
    tokio::spawn(async move {
        let disconnected = closed.borrow().is_some();
        if disconnected {
            report(Status::Disconnected);
        } else if let Some(socket) = socket {
            receive(socket, closed, report).await;
        } else {
            report(Status::Unavailable);
            super::wait_closed(closed).await;
            report(Status::Disconnected);
        }
    })
}

async fn receive(
    socket: tokio::net::UnixDatagram,
    mut closed: watch::Receiver<Option<String>>,
    mut report: impl FnMut(Status),
) {
    let mut sampler = Sampler::default();
    let mut last = Instant::now();
    let mut stale = false;
    let mut buffer = [0; 33];
    let mut timer = tokio::time::interval(INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            result = closed.changed() => {
                if result.is_err() || closed.borrow().is_some() {
                    report(Status::Disconnected);
                    return;
                }
            }
            received = socket.recv(&mut buffer) => {
                let Ok(count) = received else {
                    report(Status::Unavailable);
                    super::wait_closed(closed).await;
                    report(Status::Disconnected);
                    return;
                };
                let mut latest = Sample::decode(&buffer[..count]);
                // Registration can precede the UI. Drain its bounded backlog so
                // old startup samples do not replay as current traffic.
                for _ in 0..255 {
                    match socket.try_recv(&mut buffer) {
                        Ok(count) => if let Some(sample) = Sample::decode(&buffer[..count]) {
                            latest = Some(sample);
                        },
                        Err(_) => break,
                    }
                }
                if let Some(sample) = latest {
                    last = Instant::now();
                    stale = false;
                    report(sampler.observe(sample));
                }
            }
            _ = timer.tick() => {
                if !stale && last.elapsed() >= STALE_AFTER {
                    stale = true;
                    sampler = Sampler::default();
                    report(Status::Unavailable);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "unix/tests.rs"]
mod tests;
