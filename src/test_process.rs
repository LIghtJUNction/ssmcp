//! Fresh-process tests for contracts that require the final kernel descriptor.
//! A concurrent harness fork can temporarily inherit another test's CLOEXEC FD.
use std::time::Duration;

pub(crate) async fn isolated(case: &str) -> bool {
    const FLAG: &str = "SSMCP_TEST_FD_ISOLATED_CASE";
    if std::env::var(FLAG).as_deref() == Ok(case) {
        return false;
    }
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", case, "--nocapture", "--test-threads=1"])
        .env(FLAG, case)
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("isolated resource test exceeded its watchdog")
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "isolated filter did not execute exactly one case: {case}\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.status.success(),
        "{case}\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

pub(crate) async fn read_line(
    pipe: &mut zrun::io::AsyncFd<std::process::ChildStdout>,
    line: &mut String,
) -> std::io::Result<usize> {
    let mut bytes = Vec::new();
    let mut next = [0];
    while pipe.read(&mut next).await? != 0 {
        bytes.push(next[0]);
        if next[0] == b'\n' {
            break;
        }
    }
    line.push_str(std::str::from_utf8(&bytes).map_err(std::io::Error::other)?);
    Ok(bytes.len())
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn forked_cloexec_descriptor_holds_lock_until_exec() {
    const CASE: &str = "test_process::forked_cloexec_descriptor_holds_lock_until_exec";
    if isolated(CASE).await {
        return;
    }
    use std::{
        fs::OpenOptions,
        os::{
            fd::{AsRawFd, FromRawFd, OwnedFd},
            unix::{fs::OpenOptionsExt, process::CommandExt},
        },
        process::Command,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn pipe() -> [OwnedFd; 2] {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        unsafe { [OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])] }
    }
    struct Release {
        fd: i32,
        sent: bool,
    }
    impl Release {
        fn send(&mut self) {
            if !self.sent {
                let byte = [1u8];
                unsafe {
                    libc::write(self.fd, byte.as_ptr().cast(), 1);
                }
                self.sent = true;
            }
        }
    }
    impl Drop for Release {
        fn drop(&mut self) {
            self.send();
        }
    }
    let cleanup = Cleanup(std::env::temp_dir().join(format!("ssmcp-fork-lock-{}-{}",
        std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos())));
    let owner = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&cleanup.0)
        .unwrap();
    assert_ne!(
        unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(
        unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let ready = pipe();
    let release = pipe();
    let ready_read = ready[0].as_raw_fd();
    let ready_write = ready[1].as_raw_fd();
    let release_read = release[0].as_raw_fd();
    let release_write = release[1].as_raw_fd();
    let executable = std::env::current_exe().unwrap();
    let child = std::thread::spawn(move || {
        let mut command = Command::new(executable);
        // The new image only lists this case; it does not recursively execute it.
        command.args(["--list", "--exact", CASE]);
        unsafe {
            command.pre_exec(move || {
                // Only stack values and async-signal-safe libc operations here.
                libc::close(ready_read);
                libc::close(release_write);
                let mut byte = [1u8];
                if libc::write(ready_write, byte.as_ptr().cast(), 1) != 1
                    || libc::read(release_read, byte.as_mut_ptr().cast(), 1) != 1
                {
                    return Err(std::io::Error::from_raw_os_error(libc::EIO));
                }
                libc::close(ready_write);
                libc::close(release_read);
                Ok(())
            });
        }
        command.output()
    });
    let mut release_guard = Release {
        fd: release_write,
        sent: false,
    };
    let mut descriptor = libc::pollfd {
        fd: ready_read,
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(
        unsafe { libc::poll(&mut descriptor, 1, 5000) },
        1,
        "child fork barrier"
    );
    let mut byte = [0u8];
    assert_eq!(
        unsafe { libc::read(ready_read, byte.as_mut_ptr().cast(), 1) },
        1
    );
    assert_eq!(byte, [1]);
    drop(owner);
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&cleanup.0)
        .unwrap();
    let held = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    let held_error = std::io::Error::last_os_error();
    release_guard.send();
    let output = child.join().unwrap().unwrap();
    assert!(
        output.status.success(),
        "controlled exec: {:?}",
        output.status
    );
    assert_eq!(
        held, -1,
        "forked copy must retain the original open-file description"
    );
    assert_eq!(
        held_error.kind(),
        std::io::ErrorKind::WouldBlock,
        "{held_error}"
    );
    assert_eq!(
        unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0,
        "successful exec must release the last inherited CLOEXEC reference"
    );
}
