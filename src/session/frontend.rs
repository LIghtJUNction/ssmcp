//! Private discovery and lifetime of frontend-owned session cores.
use std::{
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Child, ExitStatus, Stdio},
    time::Duration,
};

/// Authoritative refusal of a frontend-owned core's account relay registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayAdmissionDenial {
    TrafficQuotaExceeded,
    OnlineSessionLimit,
}

impl RelayAdmissionDenial {
    #[doc(hidden)]
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::TrafficQuotaExceeded => 81,
            Self::OnlineSessionLimit => 82,
        }
    }

    /// Used by the private relay dispatcher and the owned core's executable entry point.
    #[doc(hidden)]
    pub fn exit_code_for(error: &crate::Error) -> u8 {
        error
            .downcast_ref::<Self>()
            .map_or(1, |reason| reason.exit_code())
    }

    /// Only interpret the exit status of the private core this frontend launched.
    pub fn from_exit(status: &ExitStatus) -> Option<Self> {
        [Self::TrafficQuotaExceeded, Self::OnlineSessionLimit]
            .into_iter()
            .find(|reason| status.code() == Some(i32::from(reason.exit_code())))
    }
}

impl std::fmt::Display for RelayAdmissionDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TrafficQuotaExceeded => "本月中继流量已用完。",
            Self::OnlineSessionLimit => "免费账户已有一个在线会话。",
        })
    }
}

impl std::error::Error for RelayAdmissionDenial {}

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
    root: PathBuf,
    private_root: Option<PrivateRoot>,
}

/// A newly created, lifetime-scoped registry for a single frontend.
struct PrivateRoot(PathBuf);

impl PrivateRoot {
    fn create() -> crate::Result<Self> {
        let parent = super::session_root(true)?.join("frontends");
        super::create_private_tree(&parent)?;
        super::verify_directory(&parent)?;
        let path = parent.join(super::random_suffix()?);
        crate::platform::private_directory(&path)?;
        Ok(Self(path))
    }
}

impl Drop for PrivateRoot {
    fn drop(&mut self) {
        // Only launch_private creates this tree. Caller-supplied roots remain owned by callers.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl OwnedSession {
    pub async fn launch(executable: &Path, destination: &str, cwd: &Path) -> crate::Result<Self> {
        let root = super::session_root(true)?;
        Self::launch_in(executable, destination, cwd, &root).await
    }

    /// Create a registry shared exclusively by this window and the tabs it starts.
    pub async fn launch_private(
        executable: &Path,
        destination: &str,
        cwd: &Path,
    ) -> crate::Result<Self> {
        let root = PrivateRoot::create()?;
        let mut owner = Self::launch_in(executable, destination, cwd, &root.0).await?;
        owner.private_root = Some(root);
        Ok(owner)
    }

    /// Start a core inside the caller's registry without changing the parent's environment.
    pub async fn launch_in(
        executable: &Path,
        destination: &str,
        cwd: &Path,
        session_root: &Path,
    ) -> crate::Result<Self> {
        Self::launch_core(executable, destination, cwd, session_root, false).await
    }

    pub(crate) async fn launch_chat_in(
        executable: &Path,
        destination: &str,
        cwd: &Path,
        session_root: &Path,
    ) -> crate::Result<Self> {
        Self::launch_core(executable, destination, cwd, session_root, true).await
    }

    async fn launch_core(
        executable: &Path,
        destination: &str,
        cwd: &Path,
        session_root: &Path,
        chat: bool,
    ) -> crate::Result<Self> {
        crate::server_endpoint::parse_target(destination)?;
        if !executable.is_absolute() {
            return Err("the frontend requires an absolute ssmcp executable path".into());
        }
        if !session_root.is_absolute() {
            return Err("frontend session root must be absolute".into());
        }
        super::create_private_tree(session_root)?;
        super::verify_directory(session_root)?;
        let root = session_root.canonicalize()?;
        let instance = super::random_suffix()?;
        let target = format!("live:{instance}");
        let mut command = std::process::Command::new(executable);
        let private_root = if chat {
            let home = root.join(format!("chat-{instance}"));
            crate::platform::private_directory(&home)?;
            command
                .env_clear()
                .env("HOME", &home)
                .env("SSMCP_AUTH_HOME", home.join("auth"))
                .env("TMPDIR", &home)
                .current_dir(&home)
                .args(["__magisk-chat-frontend", destination, &instance]);
            Some(PrivateRoot(home))
        } else {
            command
                .current_dir(cwd)
                .args(["__frontend-run", destination, &instance]);
            None
        };
        command
            .env("SSMCP_SESSION_HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(if io::stderr().is_terminal() {
                Stdio::null()
            } else {
                Stdio::inherit()
            });
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut session = Self {
            child: command.spawn()?,
            target: String::new(),
            directory: PathBuf::new(),
            root,
            private_root,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(if chat { 45 } else { 7 });
        loop {
            if let Some(status) = session.try_wait()? {
                if let Some(reason) = RelayAdmissionDenial::from_exit(&status) {
                    return Err(Box::new(reason));
                }
                return Err(format!(
                    "ssmcp exited before opening its local frontend endpoint ({status})"
                )
                .into());
            }
            if let Ok((directory, _, pid)) =
                super::overview::foreground_target_in(&session.root, &target)
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
    /// Open the private endpoint of this owned process, pinned to its PID.
    pub async fn connect(&self, read_only: bool) -> crate::Result<super::attach::RemoteFrontend> {
        super::attach::RemoteFrontend::connect_for_pid(
            self.directory.clone(),
            read_only,
            Some(self.child.id()),
        )
        .await
    }

    /// Open the owner or a tab in this same private registry, pinned to its registered PID.
    pub async fn connect_in(
        &self,
        target: &str,
        read_only: bool,
    ) -> crate::Result<super::attach::RemoteFrontend> {
        if target == self.target {
            return self.connect(read_only).await;
        }
        let (directory, _, pid) = super::overview::foreground_target_in(&self.root, target)?;
        super::attach::RemoteFrontend::connect_for_pid(directory, read_only, Some(pid)).await
    }

    pub fn session_root(&self) -> &Path {
        &self.root
    }

    /// Wait for this exact child to exit before reporting a successful close.
    pub async fn shutdown(&mut self) -> crate::Result<()> {
        async fn wait(child: &mut Child) -> io::Result<()> {
            while child.try_wait()?.is_none() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(())
        }
        drop(self.child.stdin.take());
        let grace = Duration::from_secs(2);
        match tokio::time::timeout(grace, wait(&mut self.child)).await {
            Ok(result) => result?,
            Err(_) => {
                if let Err(error) = self.child.kill()
                    && self.child.try_wait()?.is_none()
                {
                    return Err(error.into());
                }
                tokio::time::timeout(grace, wait(&mut self.child))
                    .await
                    .map_err(|_| "could not confirm that the frontend core exited")??;
            }
        }
        drop(self.private_root.take());
        Ok(())
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_shutdown_reaps_only_its_child_and_preserves_caller_state() {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-owner-{}",
            super::super::random_suffix().unwrap()
        ));
        crate::platform::private_directory(&root).unwrap();
        let child = || OwnedSession {
            child: std::process::Command::new("/bin/sh")
                .args(["-c", "read -r line || exit 0"])
                .stdin(Stdio::piped())
                .spawn()
                .unwrap(),
            target: String::new(),
            directory: PathBuf::new(),
            root: root.clone(),
            private_root: None,
        };
        let mut first = child();
        let mut second = child();
        first.shutdown().await.unwrap();
        assert!(first.try_wait().unwrap().is_some());
        assert!(second.try_wait().unwrap().is_none());
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
        assert!(root.is_dir());
        std::fs::remove_dir(root).unwrap();
    }
}
