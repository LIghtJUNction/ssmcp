//! An installed APK keeps the writing end of each native child's stdin pipe.
//! The kernel closes that end even if Android kills the app without a callback.

pub(super) fn observe_app_pipe() -> std::io::Result<()> {
    use std::{
        fs::File,
        os::{
            fd::{AsFd, AsRawFd},
            unix::fs::FileTypeExt,
        },
    };
    let pipe = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    if !pipe.metadata()?.file_type().is_fifo() {
        return Err(std::io::Error::other(
            "Android app launch requires a stdin pipe",
        ));
    }
    std::thread::Builder::new()
        .name("apk-lifetime".into())
        .spawn(move || {
            // Request no read events so the normal client retains its complete
            // stdin stream. poll still reports a hangup when every writer closes.
            let mut fd = libc::pollfd {
                fd: pipe.as_raw_fd(),
                events: 0,
                revents: 0,
            };
            loop {
                let status = unsafe { libc::poll(&mut fd, 1, -1) };
                if status < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                }
                unsafe {
                    libc::raise(libc::SIGTERM);
                }
                break;
            }
        })?;
    Ok(())
}
