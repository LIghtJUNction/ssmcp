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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        time::timeout,
    };

    #[tokio::test]
    async fn shutdown_stops_owned_children_in_a_separate_session() {
        let mut command = Command::new("python3");
        command.args(["-u","-c",r#"
import os, subprocess, sys, time
child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(300)'], start_new_session=True)
print(child.pid, flush=True)
time.sleep(300)
"#]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut managed = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(managed.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        assert_eq!(
            stat.rsplit_once(')')
                .unwrap()
                .1
                .split_whitespace()
                .nth(3)
                .unwrap(),
            pid.to_string()
        );
        managed.shutdown().await;
        timeout(Duration::from_secs(5), async {
            loop {
                let running = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .is_some_and(|s| {
                        s.rsplit_once(')').unwrap().1.split_whitespace().next() != Some("Z")
                    });
                if !running {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owned child must stop even when it called setsid");
    }
}
