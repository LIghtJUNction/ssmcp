//! Bounded, noninteractive local Git commands for registered project worktrees.
use std::{
    ffi::OsStr,
    io::{self, Read},
    os::unix::{ffi::OsStringExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: u64 = 4 * 1024 * 1024;

pub(super) fn run(
    dir: &Path,
    args: &[impl AsRef<OsStr>],
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    if Instant::now() >= deadline {
        return Err(io::ErrorKind::TimedOut.into());
    }
    let mut cmd = Command::new(crate::program::executable("git").map_err(io::Error::other)?);
    // Use the requested repository even when the client itself was invoked by Git.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.current_dir(dir)
        .args([
            "--no-optional-locks",
            "--no-replace-objects",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.logAllRefUpdates=true",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .args(args)
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn()?;
    let pipe = child.stdout.take().expect("piped stdout");
    let (send, recv) = mpsc::sync_channel(1);
    let reader = thread::Builder::new()
        .name("ssmcp-git-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = pipe
                .take(OUTPUT_LIMIT + 1)
                .read_to_end(&mut bytes)
                .and_then(|_| {
                    if bytes.len() as u64 > OUTPUT_LIMIT {
                        Err(io::Error::other("Git output exceeds the inspection limit"))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = send.send(result);
        });
    let stop = |child: &mut std::process::Child| {
        // Each child owns a freshly created process group.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.wait();
    };
    if let Err(err) = reader {
        stop(&mut child);
        return Err(err);
    }
    let bytes = match recv.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(err)) => {
            stop(&mut child);
            return Err(err);
        }
        Err(_) => {
            stop(&mut child);
            return Err(io::ErrorKind::TimedOut.into());
        }
    };
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => return Ok(bytes),
            Ok(Some(_)) => return Err(io::Error::other("Git declined the operation")),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            other => {
                stop(&mut child);
                return Err(other
                    .err()
                    .unwrap_or_else(|| io::ErrorKind::TimedOut.into()));
            }
        }
    }
}

pub(super) fn path(dir: &Path, flag: &str, deadline: Instant) -> io::Result<PathBuf> {
    let mut data = run(
        dir,
        &["rev-parse", "--path-format=absolute", flag],
        deadline,
    )?;
    if data.pop() != Some(b'\n') {
        return Err(io::Error::other("invalid Git path"));
    }
    std::fs::canonicalize(std::ffi::OsString::from_vec(data))
}
