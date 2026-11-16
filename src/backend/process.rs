//! Owned process sessions. Keep the leader unreaped until all ordinary jobs are stopped.
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::{io, process::ExitStatus};
use tokio::process::{Child, Command};

pub struct Managed {
    pub child: Child,
    #[cfg(unix)]
    pid: libc::pid_t,
    #[cfg(target_os = "linux")]
    leader: OwnedFd,
    #[cfg(windows)]
    job: crate::platform::WindowsJob,
    stopped: bool,
}

impl Managed {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        // SAFETY: after fork, only the async-signal-safe setsid syscall is used.
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        #[cfg(windows)]
        let job = crate::platform::WindowsJob::new()?;
        #[cfg(windows)]
        command.creation_flags(
            windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
                | windows_sys::Win32::System::Threading::CREATE_SUSPENDED,
        );
        let child = command.spawn()?;
        #[cfg(any(target_os = "linux", windows))]
        let mut child = child;
        #[cfg(windows)]
        if let Some(handle) = child.raw_handle()
            && let Err(error) = job.assign(handle)
        {
            let _ = child.start_kill();
            return Err(error);
        }
        #[cfg(windows)]
        if let Err(error) = crate::platform::resume_process(
            child
                .id()
                .ok_or_else(|| io::Error::other("child has no PID"))?,
        ) {
            job.terminate();
            let _ = child.start_kill();
            return Err(error);
        }
        #[cfg(unix)]
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no PID"))? as libc::pid_t;
        #[cfg(target_os = "linux")]
        let leader = match crate::pty::open_pidfd(pid) {
            Ok(fd) => fd,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            #[cfg(unix)]
            pid,
            #[cfg(target_os = "linux")]
            leader,
            #[cfg(windows)]
            job,
            stopped: false,
        })
    }

    /// Observe exit without releasing the Unix session leader before cleanup.
    /// Keeping its identity also prevents signalling a reused process-group ID.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if self.stopped {
            return self.child.wait().await;
        }
        #[cfg(target_os = "linux")]
        {
            // SAFETY: the duplicated pidfd stays owned until readiness completes.
            let ready = unsafe { tokio::io::unix::AsyncFd::register(self.leader.try_clone()?) }?;
            let _ready = ready.readable().await?;
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        loop {
            // SAFETY: waitid writes initialized child status without reaping it.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if info.si_pid != 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        #[cfg(unix)]
        self.terminate();
        let result = self.child.wait().await;
        #[cfg(windows)]
        self.terminate();
        result
    }

    /// Stop owned descendants while their parent is still alive and identifiable.
    /// Transport owners must call this before closing stdin, which can make a
    /// cooperative native process exit and reparent its detached child sessions.
    pub fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        #[cfg(target_os = "linux")]
        crate::pty::stop_session(self.pid, &self.leader);
        #[cfg(all(unix, not(target_os = "linux")))]
        if self.child.id().is_some() {
            crate::platform::stop_unix_descendants(self.pid);
        }
        #[cfg(windows)]
        self.job.terminate();
        let _ = self.child.start_kill();
        self.stopped = true;
    }

    pub async fn shutdown(&mut self) {
        self.terminate();
        // Always wait, including after synchronous termination or a cancelled wait.
        let _ = self.child.wait().await;
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::{
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::Stdio,
        time::Duration,
    };
    use tokio::{
        io::{AsyncBufReadExt, BufReader},
        time::timeout,
    };
    use windows_sys::Win32::{Foundation::WAIT_OBJECT_0, System::Threading::*};

    #[tokio::test]
    async fn cancellation_terminates_the_owned_process_tree() {
        let mut command = Command::new("python");
        command.args(["-u", "-c", "import subprocess,sys,time; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); print(child.pid,flush=True); time.sleep(60)"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut process = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(process.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(10), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid = line.trim().parse().unwrap();
        let raw = unsafe { OpenProcess(PROCESS_ALL_ACCESS, 0, pid) };
        assert!(!raw.is_null());
        let child = unsafe { OwnedHandle::from_raw_handle(raw) };
        process.shutdown().await;
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
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
    #[tokio::test]
    async fn natural_exit_cleans_background_children_before_reaping_the_leader() {
        let mut command = Command::new("python3");
        command.args(["-u", "-c", "import subprocess,sys; child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); print(child.pid,flush=True)"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut process = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(process.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        let status = timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
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
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background child survived the completed command");
    }
}
