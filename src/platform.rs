//! Small operating-system differences used by the foreground client.
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
pub struct Signal(tokio::signal::unix::Signal);
#[cfg(windows)]
pub enum Signal {
    Interrupt(tokio::signal::windows::CtrlC),
    Terminate(tokio::signal::windows::CtrlBreak),
}

impl Signal {
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.0.recv().await;
        }
        #[cfg(windows)]
        match self {
            Self::Interrupt(signal) => {
                signal.recv().await;
            }
            Self::Terminate(signal) => {
                signal.recv().await;
            }
        }
    }
}

pub fn interrupt_signal() -> io::Result<Signal> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).map(Signal)
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_c().map(Signal::Interrupt)
    }
}

pub fn terminate_signal() -> io::Result<Signal> {
    #[cfg(unix)]
    {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).map(Signal)
    }
    #[cfg(windows)]
    {
        tokio::signal::windows::ctrl_break().map(Signal::Terminate)
    }
}

pub fn default_shell() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| {
                PathBuf::from(
                    std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
                )
                .join("System32/cmd.exe")
            })
    }
}

pub fn hostname() -> io::Result<String> {
    #[cfg(unix)]
    {
        let mut bytes = [0u8; 256];
        // SAFETY: the buffer is writable for the supplied length, with a spare NUL.
        if unsafe { libc::gethostname(bytes.as_mut_ptr().cast(), bytes.len() - 1) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..end].to_vec()).map_err(io::Error::other)
    }
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").map_err(io::Error::other)
    }
}

pub fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        path.extension().is_some_and(|ext| {
            ["exe", "com", "cmd", "bat"]
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
    }
}

/// Resolve PATH without canonicalizing symlinks: tools such as mise dispatch by argv[0].
pub fn executable(name: &str) -> Result<PathBuf, String> {
    let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
    executable_in(name, &std::env::var_os("PATH").unwrap_or_default(), &cwd)
}

fn executable_in(name: &str, search_path: &std::ffi::OsStr, cwd: &Path) -> Result<PathBuf, String> {
    for directory in std::env::split_paths(search_path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let candidate = directory.join(name);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
        #[cfg(windows)]
        if Path::new(name).extension().is_none() {
            for extension in std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
                .split(';')
            {
                if extension.starts_with('.') && !extension.contains(['/', '\\']) {
                    let candidate = directory.join(format!("{name}{extension}"));
                    if is_executable(&candidate) {
                        return Ok(candidate);
                    }
                }
            }
        }
    }
    Err(format!("required executable {name} is not installed"))
}

pub fn random_bytes(bytes: &mut [u8]) -> io::Result<()> {
    getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
}

pub fn private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(windows)]
    {
        windows::create_private_directory(path)
    }
}

#[cfg(windows)]
#[path = "platform_windows.rs"]
pub(crate) mod windows;
#[cfg(windows)]
pub(crate) use windows::{WindowsJob, resume_process};

pub(crate) fn lock_file(file: &std::fs::File, nonblocking: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        loop {
            // SAFETY: the borrowed file descriptor stays live throughout flock.
            if unsafe {
                libc::flock(
                    file.as_raw_fd(),
                    libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 },
                )
            } == 0
            {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    #[cfg(windows)]
    {
        windows::lock(file, nonblocking)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn executable_keeps_shim_name_and_resolves_relative_path_entries() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let mut random = [0u8; 16];
        random_bytes(&mut random).unwrap();
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("ssmcp-shim-test-{suffix}"));
        let bin = directory.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let dispatcher = bin.join("dispatcher");
        std::fs::write(&dispatcher, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&dispatcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shim = bin.join("agent-shim");
        symlink("dispatcher", &shim).unwrap();

        let found = executable_in("agent-shim", std::ffi::OsStr::new("bin"), &directory).unwrap();
        assert_eq!(found, shim);
        assert_ne!(found, found.canonicalize().unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn machine_defaults_and_entropy_are_available() {
        assert!(default_shell().is_absolute());
        assert!(!hostname().unwrap().is_empty());
        let mut one = [0; 32];
        let mut two = [0; 32];
        random_bytes(&mut one).unwrap();
        random_bytes(&mut two).unwrap();
        assert_ne!(one, two);
    }
}
