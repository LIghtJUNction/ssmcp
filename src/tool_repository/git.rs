use super::{ORIGIN, Repository, hex};
use std::{ffi::OsString, path::Path, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, sync::watch};

tokio::task_local! {
    static CANCELLATION: watch::Receiver<bool>;
}

/// The task, rather than its awaiting caller, owns all repository leases and
/// staging directories until every child has finished its actual cleanup.
pub(super) async fn owned<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    struct Cancel(watch::Sender<bool>);
    impl Drop for Cancel {
        fn drop(&mut self) { let _ = self.0.send(true); }
    }
    let (sender, receiver) = watch::channel(false);
    let _cancel = Cancel(sender);
    let owner = tokio::spawn(CANCELLATION.scope(receiver, operation));
    owner.await.map_err(|e| format!("tool repository owner failed: {e}"))?
}

pub(super) fn active() -> Result<(), String> {
    if CANCELLATION.try_with(|rx| *rx.borrow()).unwrap_or(false) {
        return Err("tool repository operation was cancelled".into());
    }
    Ok(())
}

pub(super) async fn cancelled() {
    let Ok(mut receiver) = CANCELLATION.try_with(Clone::clone) else {
        std::future::pending::<()>().await;
        return;
    };
    if *receiver.borrow() { return; }
    let _ = receiver.changed().await;
}

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Output {
    fn checked(self) -> Result<Vec<u8>, String> {
        if self.code == Some(0) {
            Ok(self.stdout)
        } else {
            Err(format!("tool repository Git failed ({:?}): {}", self.code, self.stderr.trim()))
        }
    }
}

fn null_file() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

async fn capture(directory: &Path, args: &[OsString], network: bool) -> Result<Output, String> {
    active()?;
    let executable = crate::program::executable("git")
        .map_err(|_| "Git is required for SSMCP tool repositories, including offline initialization; install Git and retry")?;
    let mut command = tokio::process::Command::new(executable);
    command.env_clear();
    // Git HTTPS needs normal platform executable/library locations, not arbitrary
    // inherited GIT_* configuration, helpers, credentials, or a caller's repo.
    for key in ["PATH", "SystemRoot", "WINDIR", "TEMP", "TMP", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) { command.env(key, value); }
    }
    command.env("HOME", directory)
        .env("LC_ALL", "C")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_file())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args([
            "-c", &format!("core.hooksPath={}", null_file()),
            "-c", "core.fsmonitor=false",
            "-c", "core.autocrlf=false",
            "-c", "core.safecrlf=false",
            "-c", "protocol.allow=never",
            "-c", if network { "protocol.file.allow=never" } else { "protocol.file.allow=always" },
            "-c", "protocol.https.allow=always",
            "-c", "credential.helper=",
            "-c", "fetch.recurseSubmodules=false",
            "-c", "submodule.recurse=false",
            "-c", "maintenance.auto=false",
            "-c", "gc.auto=0",
        ])
        .args(args).current_dir(directory)
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut process = crate::backend::process::Managed::spawn(&mut command)
        .map_err(|e| format!("could not start tool repository Git: {e}"))?;
    let mut stdout = process.child.stdout.take().ok_or("Git stdout unavailable")?.take(512 * 1024 + 1);
    let mut stderr = process.child.stderr.take().ok_or("Git stderr unavailable")?.take(16 * 1024 + 1);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let deadline = Duration::from_secs(if network { 45 } else { 10 });
    let operation = tokio::time::timeout(deadline, async {
        let (a, b, status) = tokio::join!(
            stdout.read_to_end(&mut out), stderr.read_to_end(&mut err), process.wait()
        );
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        let status = status.map_err(|e| e.to_string())?;
        if out.len() > 512 * 1024 || err.len() > 16 * 1024 {
            return Err("tool repository Git exceeded its output limit".into());
        }
        Ok(Output {
            code:status.code(), stdout:out,
            stderr:String::from_utf8_lossy(&err).chars().take(2048).collect(),
        })
    });
    let result = tokio::select! {
        result = operation => result
            .map_err(|_| format!("tool repository Git timed out after {} seconds", deadline.as_secs()))
            .and_then(|result| result),
        _ = cancelled() => Err("tool repository operation was cancelled".into()),
    };
    // The Managed process owns its descendants; always finish cleanup before
    // releasing the repository lease or replacing an initialized directory.
    process.shutdown().await;
    result
}

pub(super) async fn run(directory: &Path, args: &[OsString], network: bool) -> Result<Vec<u8>, String> {
    if network {
        let effective = capture(directory, &[
            "ls-remote".into(), "--get-url".into(), ORIGIN.into(),
        ], false).await?.checked()?;
        if String::from_utf8(effective).map_err(|e| e.to_string())?.trim() != ORIGIN {
            return Err("official tool repository URL is rewritten by local Git configuration; no network request was sent".into());
        }
    }
    capture(directory, args, network).await?.checked()
}

pub(super) async fn text(directory: &Path, args: &[&str]) -> Result<String, String> {
    let args = args.iter().map(|arg| OsString::from(*arg)).collect::<Vec<_>>();
    String::from_utf8(run(directory, &args, false).await?)
        .map(|s| s.trim().to_owned()).map_err(|_| "tool repository Git returned invalid UTF-8".into())
}

pub(super) async fn optional_text(directory: &Path, args: &[&str]) -> Result<Option<String>, String> {
    let args = args.iter().map(|arg| OsString::from(*arg)).collect::<Vec<_>>();
    let result = capture(directory, &args, false).await?;
    if result.code == Some(1) { return Ok(None); }
    String::from_utf8(result.checked()?)
        .map(|s| Some(s.trim().to_owned())).map_err(|_| "tool repository Git returned invalid UTF-8".into())
}

pub(super) async fn identity(directory: &Path) -> Result<Option<Repository>, String> {
    let root = capture(directory, &["rev-parse".into(), "--show-toplevel".into()], false).await?;
    if root.code != Some(0) && root.stderr.contains("not a git repository") {
        return Ok(None);
    }
    let root = String::from_utf8(root.checked()?).map_err(|e| e.to_string())?;
    let root = Path::new(root.trim()).canonicalize().map_err(|e| e.to_string())?;
    let head = optional_text(directory, &["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
    if head.as_ref().is_some_and(|head| !(hex(head, 40) || hex(head, 64))) {
        return Err("tool repository HEAD is invalid".into());
    }
    reject_filters(directory).await?;
    let status = text(directory, &["status", "--porcelain=v1", "--untracked-files=normal"]).await?;
    Ok(Some(Repository { root:root.display().to_string(), head, dirty:!status.is_empty() }))
}

pub(super) async fn remote_head(directory: &Path) -> Result<(String, String), String> {
    let bytes = run(directory, &[
        "ls-remote".into(), "--symref".into(), "--exit-code".into(), ORIGIN.into(), "HEAD".into(),
    ], true).await?;
    let value = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    let mut reference = None;
    let mut head = None;
    for line in value.lines() {
        let Some((left, "HEAD")) = line.split_once('\t') else { continue };
        if let Some(name) = left.strip_prefix("ref: ") {
            if reference.replace(name.to_owned()).is_some() {
                return Err("official tool repository returned duplicate default refs".into());
            }
        } else if head.replace(left.to_owned()).is_some() {
            return Err("official tool repository returned duplicate HEADs".into());
        }
    }
    let reference = reference.ok_or("official tool repository did not advertise its default branch")?;
    let head = head.ok_or("official tool repository did not advertise HEAD")?;
    if !reference.starts_with("refs/heads/") || reference.len() > 512
        || !reference.bytes().all(|c| c.is_ascii_alphanumeric() || b"._/-".contains(&c))
        || reference.contains("..") || reference.ends_with('/') || reference.ends_with(".lock")
        || !(hex(&head, 40) || hex(&head, 64))
    {
        return Err("official tool repository returned an invalid default ref".into());
    }
    Ok((reference, head))
}

pub(super) async fn ancestor(directory: &Path, older: &str, newer: &str) -> Result<bool, String> {
    let output = capture(directory, &[
        "merge-base".into(), "--is-ancestor".into(), older.into(), newer.into(),
    ], false).await?;
    match output.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => output.checked().map(|_| false),
    }
}

pub(super) async fn reject_filters(directory: &Path) -> Result<(), String> {
    // Updating helpers must not execute local smudge/clean filter commands.
    let output = capture(directory, &[
        "config".into(), "--includes".into(), "--null".into(), "--name-only".into(),
        "--get-regexp".into(), "^filter\\.".into(),
    ], false).await?;
    if output.code == Some(1) { return Ok(()); }
    if !output.checked()?.is_empty() {
        return Err("tool repositories with configured Git filters require explicit local inspection; no filter was executed".into());
    }
    Ok(())
}
