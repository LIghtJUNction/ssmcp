//! Lifetime and discovery of the ordinary CLI process owned by a graphical frontend.
use std::{
    io,
    path::{Path, PathBuf},
    process::{Child, ExitStatus, Stdio},
    time::Duration,
};

/// The private attach endpoint published by this exact, still-owned process.
pub fn target_for_pid(root: &Path, pid: u32) -> crate::Result<Option<(String, PathBuf)>> {
    for row in super::overview::foreground_rows_in(root)? {
        if row["pid"].as_u64() == Some(u64::from(pid)) {
            let target = row["target"].as_str().ok_or("missing frontend target")?;
            let (directory, _, registered_pid) =
                super::overview::foreground_target_in(root, target)?;
            if registered_pid == pid {
                return Ok(Some((target.to_owned(), directory)));
            }
        }
    }
    Ok(None)
}

/// Owns a core process. Its stdin pipe is the lifetime lease, including after a frontend crash.
pub struct OwnedSession {
    child: Child,
    target: String,
    directory: PathBuf,
}

impl OwnedSession {
    pub async fn launch(executable: &Path, destination: &str, cwd: &Path) -> crate::Result<Self> {
        crate::server_endpoint::parse_target(destination)?;
        if !executable.is_absolute() {
            return Err("the frontend requires an absolute ssmcp executable path".into());
        }
        let root = super::session_root(true)?;
        let instance = super::random_suffix()?;
        let target = format!("live:{instance}");
        let mut command = std::process::Command::new(executable);
        command
            .args(["__frontend-run", destination, &instance])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut session = Self {
            child: command.spawn()?,
            target: String::new(),
            directory: PathBuf::new(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(7);
        loop {
            if let Some(status) = session.try_wait()? {
                return Err(format!(
                    "ssmcp exited before opening its local frontend endpoint ({status})"
                )
                .into());
            }
            if let Ok((directory, _, pid)) = super::overview::foreground_target_in(&root, &target)
                && pid == session.child.id()
            {
                session.target = target;
                session.directory = directory;
                return Ok(session);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("timed out waiting for the new core's local frontend endpoint".into());
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
    }
    pub fn target(&self) -> &str {
        &self.target
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        // Give the child the opportunity to close its bridge and registration.
        for _ in 0..20 {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
