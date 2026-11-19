//! Wait for a nonblocking terminal writer without spending a whole frame per
//! kernel PTY buffer. The caller checks shutdown between bounded waits.

#[cfg(unix)]
pub(super) fn writable(
    fd: std::os::fd::RawFd,
    timeout: std::time::Duration,
) -> std::io::Result<bool> {
    use std::{io, time::Instant};
    if fd < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid terminal descriptor",
        ));
    }
    let deadline = Instant::now() + timeout;
    let mut descriptor = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let milliseconds = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        descriptor.revents = 0;
        // SAFETY: one initialized descriptor is borrowed only for this call;
        // the owning PhysicalOutput stays alive throughout the drawing loop.
        let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                if Instant::now() < deadline {
                    continue;
                }
                return Ok(false);
            }
            return Err(error);
        }
        if descriptor.revents & (libc::POLLNVAL | libc::POLLERR | libc::POLLHUP) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal output closed",
            ));
        }
        return Ok(result > 0 && descriptor.revents & libc::POLLOUT != 0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::{self, Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        time::Duration,
    };

    fn full_writer() -> (UnixStream, UnixStream) {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let mut bytes = 0;
        loop {
            match writer.write(&[0; 16 * 1024]) {
                Ok(n) => {
                    bytes += n;
                    assert!(n > 0 && bytes <= 4 * 1024 * 1024);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("fill writer: {error}"),
            }
        }
        (writer, reader)
    }

    #[test]
    fn ready_output_does_not_need_a_frame_timer() {
        let (writer, _reader) = UnixStream::pair().unwrap();
        assert!(writable(writer.as_raw_fd(), Duration::ZERO).unwrap());
    }

    #[test]
    fn a_full_output_buffer_times_out_without_consuming_or_dropping_data() {
        let (writer, mut reader) = full_writer();
        assert!(!writable(writer.as_raw_fd(), Duration::from_millis(5)).unwrap());
        let mut bytes = Vec::new();
        let error = reader.read_to_end(&mut bytes).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert!(!bytes.is_empty() && bytes.iter().all(|byte| *byte == 0));
        assert!(writable(writer.as_raw_fd(), Duration::ZERO).unwrap());
    }

    #[test]
    fn a_reader_releases_backpressure_before_the_wait_deadline() {
        let (writer, mut reader) = full_writer();
        let thread = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let error = reader.read_to_end(&mut bytes).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
            // Keep the reader alive until the writer has observed readiness.
            (reader, bytes)
        });
        assert!(writable(writer.as_raw_fd(), Duration::from_secs(2)).unwrap());
        let (_reader, bytes) = thread.join().unwrap();
        assert!(!bytes.is_empty());
    }

    #[test]
    fn closed_or_invalid_output_is_an_error_instead_of_a_spin_loop() {
        let (writer, reader) = UnixStream::pair().unwrap();
        drop(reader);
        assert!(writable(writer.as_raw_fd(), Duration::ZERO).is_err());
        assert!(writable(-1, Duration::ZERO).is_err());
        assert!(writable(i32::MAX, Duration::ZERO).is_err());
    }
}
