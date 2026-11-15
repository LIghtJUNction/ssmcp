//! Owned process sessions. Keep the leader unreaped until all ordinary jobs are stopped.
use std::{io, os::fd::OwnedFd};
use tokio::process::{Child, Command};

pub struct Managed {
    pub child: Child,
    pid: libc::pid_t,
    leader: OwnedFd,
    stopped: bool,
}

impl Managed {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        // SAFETY: after fork, only the async-signal-safe setsid syscall is used.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no PID"))? as libc::pid_t;
        let leader = match crate::pty::open_pidfd(pid) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            pid,
            leader,
            stopped: false,
        })
    }

    pub async fn shutdown(&mut self) {
        if self.stopped {
            return;
        }
        crate::pty::stop_session(self.pid, &self.leader);
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stopped = true;
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        if !self.stopped {
            crate::pty::stop_session(self.pid, &self.leader);
        }
    }
}
