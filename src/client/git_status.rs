//! A bounded, read-only view of the repository rooted at the authorized startup cwd.
//! Native porcelain-v2 format: https://git-scm.com/docs/git-status#_porcelain_format_version_2
use crate::backend::process::Managed;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Notify, watch},
    task::JoinHandle,
};

const POLL: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(3);
const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const ERROR_LIMIT: usize = 16 * 1024;
const SAMPLE_LIMIT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitState {
    Checking,
    Ready,
    NotRepository,
    Unavailable,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitChange {
    /// Native XY, "??" for an untracked entry; display strings escape controls.
    pub status: String,
    pub path: String,
}

#[derive(Clone, Debug)]
pub(crate) struct GitSnapshot {
    pub status: GitState,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub upstream: Option<String>,
    /// None means Git did not report a comparison, not zero divergence.
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub detached: bool,
    pub unborn: bool,
    /// Counts native entries. A tracked entry can be both staged and unstaged.
    pub staged: usize,
    pub unstaged: usize,
    /// Normal mode counts an untracked directory as one entry, not its files.
    pub untracked: usize,
    /// Unmerged entries are counted here, not as ready-to-commit staged changes.
    pub conflicts: usize,
    pub changed_files: Vec<GitChange>,
    pub checked_at: Option<Instant>,
    /// Last successful observation. Errors retain the old data and mark it stale.
    pub observed_at: Option<Instant>,
    pub stale: bool,
    pub last_error: Option<String>,
}

impl Default for GitSnapshot {
    fn default() -> Self {
        Self {
            status: GitState::Checking,
            branch: None,
            head: None,
            upstream: None,
            ahead: None,
            behind: None,
            detached: false,
            unborn: false,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            conflicts: 0,
            changed_files: Vec::new(),
            checked_at: None,
            observed_at: None,
            stale: true,
            last_error: None,
        }
    }
}

pub(crate) struct GitStatus {
    current: watch::Receiver<Arc<GitSnapshot>>,
    refresh: Arc<Notify>,
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl GitStatus {
    /// Returns immediately. Neither executable resolution nor Git blocks the UI.
    pub(crate) fn start(cwd: PathBuf) -> Self {
        let (sender, current) = watch::channel(Arc::new(GitSnapshot::default()));
        let (stop, stopped) = watch::channel(false);
        let refresh = Arc::new(Notify::new());
        let task = tokio::spawn(observe(cwd, sender, refresh.clone(), stopped));
        Self {
            current,
            refresh,
            stop,
            task: Some(task),
        }
    }

    pub(crate) fn snapshot(&self) -> Arc<GitSnapshot> {
        self.current.borrow().clone()
    }

    /// Coalesces file/command events. No caller can change the authorized cwd.
    pub(crate) fn refresh(&self) {
        self.refresh.notify_one();
    }

    pub(crate) async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(Duration::from_secs(7), &mut task)
                .await
                .is_err()
        {
            // Managed's drop guard still owns and terminates any child tree.
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for GitStatus {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn observe(
    cwd: PathBuf,
    sender: watch::Sender<Arc<GitSnapshot>>,
    refresh: Arc<Notify>,
    mut stopped: watch::Receiver<bool>,
) {
    let initialize = async {
        if !cwd.is_absolute() {
            return Err("Git workspace must be an absolute startup directory");
        }
        let root = tokio::fs::canonicalize(cwd)
            .await
            .map_err(|_| "Git workspace is unavailable")?;
        let git = tokio::task::spawn_blocking(|| crate::program::executable("git"))
            .await
            .map_err(|_| "Git executable discovery failed")?
            .map_err(|_| "Git is not available as an installed executable")?;
        Ok((root, git))
    };
    let (root, git) = tokio::select! {
        biased;
        _ = stopped.changed() => return,
        ready = tokio::time::timeout(TIMEOUT, initialize) => match ready {
            Ok(Ok(ready)) => ready,
            ready => {
                let error = match ready {
                    Ok(Err(error)) => error,
                    _ => "Git executable discovery timed out",
                };
                publish(&sender, Err(Failure::Unavailable(error)), Instant::now());
                return;
            }
        }
    };
    loop {
        if *stopped.borrow() {
            return;
        }
        let result = inspect(&git, &root, TIMEOUT, &mut stopped).await;
        if matches!(result, Err(Failure::Cancelled)) {
            return;
        }
        publish(&sender, result, Instant::now());
        tokio::select! {
            biased;
            _ = stopped.changed() => return,
            _ = tokio::time::sleep(POLL) => {},
            _ = refresh.notified() => {
                // A burst of file events schedules one short-delay refresh.
                tokio::select! {
                    biased;
                    _ = stopped.changed() => return,
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {},
                }
            },
        }
    }
}

#[derive(Debug)]
enum Failure {
    Cancelled,
    Unavailable(&'static str),
    Error(&'static str),
    NotRepository,
}

fn publish(
    sender: &watch::Sender<Arc<GitSnapshot>>,
    result: Result<GitSnapshot, Failure>,
    now: Instant,
) {
    let next = match result {
        Ok(mut current) => {
            current.checked_at = Some(now);
            current.observed_at = Some(now);
            current
        }
        Err(Failure::Cancelled) => return,
        Err(Failure::NotRepository) => GitSnapshot {
            status: GitState::NotRepository,
            checked_at: Some(now),
            observed_at: Some(now),
            stale: false,
            ..GitSnapshot::default()
        },
        Err(failure) => {
            let mut previous = sender.borrow().as_ref().clone();
            let (status, error) = match failure {
                Failure::Unavailable(error) => (GitState::Unavailable, error),
                Failure::Error(error) => (GitState::Error, error),
                _ => unreachable!(),
            };
            previous.status = status;
            previous.checked_at = Some(now);
            previous.stale = true;
            previous.last_error = Some(error.into());
            previous
        }
    };
    // This observer never prints. Repeated errors only replace the status view.
    sender.send_replace(Arc::new(next));
}

fn command(git: &Path, root: &Path) -> Result<Command, Failure> {
    let mut command = Command::new(git);
    command.args([
        "--no-pager",
        "--no-optional-locks",
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
        "-c",
        "status.submoduleSummary=false",
    ]);
    // Ignore inherited repository-routing, tracing and command config variables.
    // This copies no environment values into the status view or diagnostics.
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    let mut worktree = OsString::from("core.worktree=");
    worktree.push(root);
    command
        .arg("-c")
        .arg(worktree)
        .args([
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
            "--ahead-behind",
            "--",
            ".",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        .env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(parent) = root.parent() {
        // Git must not discover a different repository above the authorized root.
        let ceiling = std::env::join_paths([parent])
            .map_err(|_| Failure::Error("Git workspace boundary cannot be represented"))?;
        command.env("GIT_CEILING_DIRECTORIES", ceiling);
    }
    Ok(command)
}

async fn limited(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Failure::Error("Git status output could not be read"))?;
    if bytes.len() > limit {
        return Err(Failure::Error("Git status exceeded its output limit"));
    }
    Ok(bytes)
}

async fn inspect(
    git: &Path,
    root: &Path,
    timeout: Duration,
    stopped: &mut watch::Receiver<bool>,
) -> Result<GitSnapshot, Failure> {
    if *stopped.borrow() {
        return Err(Failure::Cancelled);
    }
    let mut command = command(git, root)?;
    let mut process = Managed::spawn(&mut command)
        .map_err(|_| Failure::Unavailable("Git status could not start"))?;
    let stdout = process.child.stdout.take().expect("piped Git stdout");
    let stderr = process.child.stderr.take().expect("piped Git stderr");
    let result = {
        let complete = async {
            let (stdout, stderr, exit) = tokio::try_join!(
                limited(stdout, OUTPUT_LIMIT),
                limited(stderr, ERROR_LIMIT),
                async {
                    process
                        .wait()
                        .await
                        .map_err(|_| Failure::Error("Git status could not be reaped"))
                }
            )?;
            if !exit.success() {
                if stderr.starts_with(b"fatal: not a git repository") {
                    return Err(Failure::NotRepository);
                }
                // Native stderr may contain private paths or remote URLs.
                return Err(Failure::Error(
                    "Git status failed; repository status is unknown",
                ));
            }
            parse(&stdout).map_err(Failure::Error)
        };
        tokio::select! {
            biased;
            _ = stopped.changed() => Err(Failure::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(Failure::Error("Git status timed out")),
            result = complete => result,
        }
    };
    // Also drains/reaps after timeout, oversized output, cancellation or parse error.
    process.shutdown().await;
    result
}

fn display(bytes: &[u8]) -> String {
    let mut text = String::new();
    for (index, c) in String::from_utf8_lossy(bytes).chars().enumerate() {
        if index == 512 {
            text.push('…');
            break;
        }
        if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
            text.extend(c.escape_default());
        } else {
            text.push(c);
        }
    }
    text
}

fn parse(bytes: &[u8]) -> Result<GitSnapshot, &'static str> {
    const INVALID: &str = "Git returned incomplete or invalid porcelain status";
    if !bytes.ends_with(&[0]) {
        return Err(INVALID);
    }
    let mut snapshot = GitSnapshot {
        status: GitState::Ready,
        stale: false,
        ..GitSnapshot::default()
    };
    let (mut oid, mut head) = (false, false);
    let mut records = bytes[..bytes.len() - 1].split(|c| *c == 0);
    while let Some(record) = records.next() {
        if let Some(value) = record.strip_prefix(b"# branch.oid ") {
            oid = true;
            snapshot.unborn = value == b"(initial)";
            if !snapshot.unborn {
                if value.is_empty() || !value.iter().all(u8::is_ascii_hexdigit) {
                    return Err(INVALID);
                }
                snapshot.head = Some(display(value));
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.head ") {
            if value.is_empty() {
                return Err(INVALID);
            }
            head = true;
            snapshot.detached = value == b"(detached)";
            if !snapshot.detached {
                snapshot.branch = Some(display(value));
            }
        } else if let Some(value) = record.strip_prefix(b"# branch.upstream ") {
            snapshot.upstream = Some(display(value));
        } else if let Some(value) = record.strip_prefix(b"# branch.ab ") {
            let value = std::str::from_utf8(value).map_err(|_| INVALID)?;
            let (ahead, behind) = value.split_once(' ').ok_or(INVALID)?;
            snapshot.ahead = Some(
                ahead
                    .strip_prefix('+')
                    .ok_or(INVALID)?
                    .parse()
                    .map_err(|_| INVALID)?,
            );
            snapshot.behind = Some(
                behind
                    .strip_prefix('-')
                    .ok_or(INVALID)?
                    .parse()
                    .map_err(|_| INVALID)?,
            );
        } else if record.starts_with(b"# ") || record.starts_with(b"! ") {
            // Native v2 explicitly permits unknown headers.
        } else if let Some(path) = record.strip_prefix(b"? ") {
            if path.is_empty() {
                return Err(INVALID);
            }
            snapshot.untracked += 1;
            sample(&mut snapshot, b"??", path);
        } else {
            let count = match record.first() {
                Some(b'1') => 9,
                Some(b'2') => 10,
                Some(b'u') => 11,
                _ => return Err(INVALID),
            };
            let fields = record.splitn(count, |c| *c == b' ').collect::<Vec<_>>();
            if fields.len() != count
                || fields.iter().any(|field| field.is_empty())
                || fields[1].len() != 2
            {
                return Err(INVALID);
            }
            let xy = fields[1];
            if !xy.iter().all(|c| *c == b'.' || c.is_ascii_uppercase()) {
                return Err(INVALID);
            }
            if record[0] == b'2' && records.next().is_none_or(|path| path.is_empty()) {
                return Err(INVALID);
            }
            if record[0] == b'u' {
                snapshot.conflicts += 1;
            } else {
                snapshot.staged += usize::from(xy[0] != b'.');
                snapshot.unstaged += usize::from(xy[1] != b'.');
            }
            sample(&mut snapshot, xy, fields[count - 1]);
        }
    }
    if !oid || !head {
        return Err(INVALID);
    }
    Ok(snapshot)
}

fn sample(snapshot: &mut GitSnapshot, status: &[u8], path: &[u8]) {
    if snapshot.changed_files.len() < SAMPLE_LIMIT {
        snapshot.changed_files.push(GitChange {
            status: display(status),
            path: display(path),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_nul_records_without_splitting_rename_paths_or_guessing_clean() {
        let bytes = b"# branch.oid abcdef123456\0# branch.head feature/panel\0# branch.upstream origin/main\0# branch.ab +3 -2\0# future accepted\0\
            1 MM N... 100644 100644 100644 aa bb two words\0\
            2 R. N... 100644 100644 100644 aa bb R100 new\nname\0? old path\0\
            u UU N... 100644 100644 100644 100644 aa bb cc conflict\0\
            ? directory/\0? escape\x1b[31m\xff\0";
        let status = parse(bytes).unwrap();
        assert_eq!(status.status, GitState::Ready);
        assert_eq!(status.branch.as_deref(), Some("feature/panel"));
        assert_eq!((status.ahead, status.behind), (Some(3), Some(2)));
        assert_eq!(
            (
                status.staged,
                status.unstaged,
                status.untracked,
                status.conflicts
            ),
            (2, 1, 2, 1)
        );
        assert_eq!(status.changed_files[1].path, "new\\nname");
        assert!(!status.changed_files[4].path.contains('\x1b'));
        assert!(
            parse(b"# branch.oid (initial)\0# branch.head main\0")
                .unwrap()
                .unborn
        );
        let detached = parse(b"# branch.oid abcdef\0# branch.head (detached)\0").unwrap();
        assert!(detached.detached);
        assert_eq!(detached.branch, None);
        assert_eq!(detached.ahead, None);
        for invalid in [
            b"".as_slice(),
            b"# branch.head main\0",
            b"# branch.oid (initial)\0# branch.head main",
            b"# branch.oid (initial)\0# branch.head main\0bad\0",
        ] {
            assert!(parse(invalid).is_err());
        }
    }

    #[test]
    fn failures_preserve_last_known_data_and_explicitly_mark_it_stale() {
        let now = Instant::now();
        let (sender, receiver) = watch::channel(Arc::new(GitSnapshot::default()));
        publish(
            &sender,
            Ok(parse(b"# branch.oid abcdef\0# branch.head main\0? pending\0").unwrap()),
            now,
        );
        publish(
            &sender,
            Err(Failure::Error("Git status timed out")),
            now + POLL,
        );
        let status = receiver.borrow();
        assert_eq!(status.status, GitState::Error);
        assert!(status.stale);
        assert_eq!(status.untracked, 1);
        assert_eq!(status.observed_at, Some(now));
        assert_eq!(status.checked_at, Some(now + POLL));
        assert_eq!(status.last_error.as_deref(), Some("Git status timed out"));
    }

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-git-status-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(git: &Path, root: &Path, args: &[&str]) {
        let mut command = std::process::Command::new(git);
        command
            .current_dir(root)
            .args([
                "-c",
                "user.name=ssmcp test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .arg("-c")
            .arg(format!(
                "core.hooksPath={}",
                root.join(".empty-hooks").display()
            ))
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join(".no-global-config"));
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "Git fixture failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[tokio::test]
    async fn observes_real_repo_without_index_writes_or_external_fsmonitor() {
        let executable =
            crate::program::executable("git").expect("installed Git for repository test");
        let dir = Directory::new();
        let root = &dir.0;
        git(&executable, root, &["init", "-q", "--initial-branch=main"]);
        std::fs::write(root.join("tracked"), "original\n").unwrap();
        git(&executable, root, &["add", "tracked"]);
        git(&executable, root, &["commit", "-qm", "initial"]);
        git(&executable, root, &["branch", "upstream"]);
        git(
            &executable,
            root,
            &["branch", "--set-upstream-to=upstream", "main"],
        );
        git(
            &executable,
            root,
            &["commit", "--allow-empty", "-qm", "ahead"],
        );
        std::fs::write(root.join("tracked"), "changed\n").unwrap();
        std::fs::write(root.join("staged"), "new\n").unwrap();
        git(&executable, root, &["add", "staged"]);
        std::fs::create_dir(root.join("directory")).unwrap();
        std::fs::write(root.join("directory/a"), "a").unwrap();
        std::fs::write(root.join("directory/b"), "b").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let hook = root.join(".git/fsmonitor-fixture");
            std::fs::write(&hook, "#!/bin/sh\ntouch .git/fsmonitor-ran\nexit 1\n").unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            git(
                &executable,
                root,
                &["config", "core.fsmonitor", hook.to_str().unwrap()],
            );
        }
        let index = root.join(".git/index");
        let before = std::fs::read(&index).unwrap();
        let modified = std::fs::metadata(&index).unwrap().modified().unwrap();
        let (_stop, mut stopped) = watch::channel(false);
        let status = inspect(&executable, root, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!((status.ahead, status.behind), (Some(1), Some(0)));
        assert_eq!(
            (
                status.staged,
                status.unstaged,
                status.untracked,
                status.conflicts
            ),
            (1, 1, 1, 0)
        );
        assert_eq!(std::fs::read(&index).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&index).unwrap().modified().unwrap(),
            modified
        );
        assert!(!root.join(".git/fsmonitor-ran").exists());
        // An authorized subdirectory must not silently inspect its parent repo.
        assert!(matches!(
            inspect(&executable, &root.join("directory"), TIMEOUT, &mut stopped).await,
            Err(Failure::NotRepository)
        ));
        let plain = Directory::new();
        assert!(matches!(
            inspect(&executable, &plain.0, TIMEOUT, &mut stopped).await,
            Err(Failure::NotRepository)
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timeout_and_cancel_reap_owned_descendants() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Directory::new();
        let fixture = dir.0.join("slow-git");
        std::fs::write(
            &fixture,
            "#!/bin/sh\nsleep 30 &\necho $! > child.pid\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (stop, mut stopped) = watch::channel(false);
        let result = inspect(&fixture, &dir.0, Duration::from_millis(150), &mut stopped).await;
        assert!(matches!(
            result,
            Err(Failure::Error("Git status timed out"))
        ));
        let pid = std::fs::read_to_string(dir.0.join("child.pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
        let query = inspect(&fixture, &dir.0, TIMEOUT, &mut stopped);
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            stop.send(true).unwrap();
        };
        let (result, _) = tokio::join!(query, cancel);
        assert!(matches!(result, Err(Failure::Cancelled)));
        let pid = std::fs::read_to_string(dir.0.join("child.pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
    }
}
