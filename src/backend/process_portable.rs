//! Process ownership on Windows and macOS. Linux keeps its pidfd implementation.
#[cfg(windows)]
use crate::platform::{WindowsJob, resume_process};
use std::{io, process::ExitStatus};
use tokio::process::{Child, Command};

pub struct Managed {
    pub child: Child,
    #[cfg(unix)]
    pid: libc::pid_t,
    #[cfg(windows)]
    job: WindowsJob,
    stopped: bool,
}

impl Managed {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        command.kill_on_drop(true);
        #[cfg(unix)]
        // SAFETY: setsid is async-signal-safe and this runs in the forked child.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        #[cfg(windows)]
        command.creation_flags(
            windows_sys::Win32::System::Threading::CREATE_SUSPENDED
                | windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP,
        );
        #[allow(unused_mut)]
        let mut child = command.spawn()?;
        #[cfg(unix)]
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("child has no PID"))? as libc::pid_t;
        #[cfg(windows)]
        let job = match WindowsJob::attach(
            child
                .raw_handle()
                .ok_or_else(|| io::Error::other("child has no process handle"))?,
        )
        .and_then(|job| {
            resume_process(
                child
                    .id()
                    .ok_or_else(|| io::Error::other("child has no PID"))?,
            )?;
            Ok(job)
        }) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        Ok(Self {
            child,
            #[cfg(unix)]
            pid,
            #[cfg(windows)]
            job,
            stopped: false,
        })
    }

    /// Wait without releasing the Unix leader identity until its group is stopped.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        if self.stopped {
            return self.child.wait().await;
        }
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            // Register before checking, so exit between the check and await
            // cannot be lost. siginfo_t stays entirely inside the sync helper.
            let mut exited = signal(SignalKind::child())?;
            loop {
                if crate::platform::wait_unix_child_unreaped(self.pid, true)? {
                    break;
                }
                if exited.recv().await.is_none() {
                    return Err(io::Error::other("child exit notification closed"));
                }
            }
        }
        #[cfg(unix)]
        self.terminate();
        let result = self.child.wait().await;
        #[cfg(windows)]
        self.terminate();
        result
    }

    pub fn terminate(&mut self) {
        if self.stopped {
            return;
        }
        #[cfg(unix)]
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
        let _ = self.child.wait().await;
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};

    #[tokio::test]
    async fn managed_process_starts_and_preserves_exit_status() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(std::env::var_os("COMSPEC").expect("COMSPEC"));
            command.args(["/d", "/c", "exit /b 17"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "exit 17"]);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = Managed::spawn(&mut command).unwrap();
        let status = tokio::time::timeout(Duration::from_secs(10), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.code(), Some(17));
        process.shutdown().await;
    }

    #[tokio::test]
    async fn managed_shutdown_terminates_a_running_command() {
        #[cfg(windows)]
        let mut command = {
            let mut command = Command::new(std::env::var_os("COMSPEC").expect("COMSPEC"));
            command.args(["/d", "/c", "ping -n 60 127.0.0.1 >nul"]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", "sleep 60"]);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = Managed::spawn(&mut command).unwrap();
        tokio::time::timeout(Duration::from_secs(10), process.shutdown())
            .await
            .unwrap();
        assert!(process.child.try_wait().unwrap().is_some());
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
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        assert!(!raw.is_null());
        let child = unsafe { OwnedHandle::from_raw_handle(raw) };
        process.shutdown().await;
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;
    use std::{process::Stdio, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, BufReader},
        time::timeout,
    };

    #[tokio::test]
    async fn natural_exit_closes_background_descendant_pipes() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 60 & printf '%s\\n' \"$!\""])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut process = Managed::spawn(&mut command).unwrap();
        let mut stdout = BufReader::new(process.child.stdout.take().unwrap());
        let mut line = String::new();
        timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let pid: i32 = line.trim().parse().unwrap();
        assert!(pid > 1);
        let status = timeout(Duration::from_secs(5), process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success());
        let mut tail = Vec::new();
        let pipe = timeout(Duration::from_secs(5), stdout.read_to_end(&mut tail)).await;
        if pipe.is_err() {
            // Keep a failed regression from leaving its 60-second fixture alive.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        pipe.expect("background descendant must release stdout after its leader exits")
            .unwrap();
        assert!(tail.is_empty());
        process.shutdown().await;
    }
}
