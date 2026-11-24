//! The shared Unix command reactor, independent of the transport runtime.

use std::{
    fs::File,
    future::{Future, pending, poll_fn},
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    pin::pin,
    sync::OnceLock,
    task::Poll,
    time::{Duration, Instant},
};

pub(crate) use zrun::JoinHandle;
use zrun::{Pool, Runtime, Timer, io::Handle};

pub(crate) struct Driver {
    pub io: Handle,
    pub timer: Timer,
    workers: Pool,
}

impl Driver {
    pub(crate) fn get() -> io::Result<&'static Self> {
        static DRIVER: OnceLock<Result<Driver, io::Error>> = OnceLock::new();
        DRIVER
            .get_or_init(Self::start)
            .as_ref()
            .map_err(|error| io::Error::new(error.kind(), error.to_string()))
    }

    fn start() -> io::Result<Self> {
        let mut runtime = Runtime::new();
        let mut poller = zrun::io::Poller::new()?;
        let driver = Self {
            io: poller.handle().clone(),
            timer: runtime.timer(),
            workers: Pool::new(2)?,
        };
        std::thread::Builder::new()
            .name("ssmcp-zrun".into())
            .spawn(move || {
                if let Err(error) = runtime.block_on_with_io(&mut poller, pending::<()>()) {
                    eprintln!("ssmcp: command reactor stopped: {error}");
                }
            })?;
        Ok(driver)
    }

    pub(crate) fn spawn<F>(&self, future: F) -> io::Result<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.workers.spawn(future).map_err(io::Error::other)
    }
}

#[derive(Debug)]
pub(crate) enum DeadlineError {
    Elapsed,
    Reactor(io::Error),
}

pub(crate) async fn timeout_at<F: Future>(
    deadline: Instant,
    future: F,
) -> Result<F::Output, DeadlineError> {
    let driver = Driver::get().map_err(DeadlineError::Reactor)?;
    let mut sleep = pin!(
        driver
            .timer
            .sleep(deadline.saturating_duration_since(Instant::now()))
    );
    let mut future = pin!(future);
    poll_fn(|cx| {
        if let Poll::Ready(value) = future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        if Instant::now() >= deadline {
            return Poll::Ready(Err(DeadlineError::Elapsed));
        }
        match sleep.as_mut().poll(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Err(DeadlineError::Elapsed)),
            Poll::Ready(Err(error)) => {
                Poll::Ready(Err(DeadlineError::Reactor(io::Error::other(error))))
            }
            Poll::Pending => Poll::Pending,
        }
    })
    .await
}

pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, DeadlineError> {
    timeout_at(Instant::now() + duration, future).await
}

pub(crate) fn open_pty(rows: u16, cols: u16) -> io::Result<(File, OwnedFd)> {
    let (mut master, mut slave) = (-1, -1);
    let mut size = window_size(rows, cols);
    // SAFETY: openpty writes two new descriptors to live stack storage.
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openpty returned two exclusively owned descriptors.
    let (master, slave) = unsafe { (File::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        // SAFETY: both descriptors are still live and owned.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

pub(crate) fn resize_pty(master: &File, rows: u16, cols: u16) -> io::Result<()> {
    let size = window_size(rows, cols);
    // SAFETY: the borrowed descriptor and the size record are live during ioctl.
    if unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn window_size(rows: u16, cols: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}
