//! A bounded repository view plus separately scheduled remote-tracking updates.
//! Native porcelain-v2 format: https://git-scm.com/docs/git-status#_porcelain_format_version_2
//! Native history fields and NUL terminators: https://git-scm.com/docs/pretty-formats
use crate::backend::process::Managed;
mod fetch;
pub(crate) use fetch::{FetchSnapshot, FetchState};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
const HISTORY_LIMIT: usize = 5;
const HISTORY_OUTPUT_LIMIT: usize = 64 * 1024;
const HISTORY_RETRY: Duration = Duration::from_secs(30);

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitCommit {
    pub full_hash: String,
    pub short_hash: String,
    /// A native subject with terminal controls escaped for display.
    pub subject: String,
    pub committed_at_unix: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct GitHistory {
    /// The exact commit this history belongs to, including pending/failed reads.
    pub head: Option<String>,
    pub commits: Vec<GitCommit>,
    pub checked_at: Option<Instant>,
    /// Wall time paired with checked_at for relative commit ages in the panel.
    pub observed_at_unix: Option<i64>,
    pub error: Option<String>,
    pub stale: bool,
}

impl Default for GitHistory {
    fn default() -> Self {
        Self {
            head: None,
            commits: Vec::new(),
            checked_at: None,
            observed_at_unix: None,
            error: None,
            stale: true,
        }
    }
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
    pub history: GitHistory,
    pub fetch: FetchSnapshot,
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
            history: GitHistory::default(),
            fetch: FetchSnapshot::default(),
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
    fetch_task: Option<JoinHandle<()>>,
}

impl GitStatus {
    /// Returns immediately. Neither executable resolution nor Git blocks the UI.
    pub(crate) fn start(cwd: PathBuf, settings: super::config::GitSettings) -> Self {
        let initial = GitSnapshot {
            fetch: FetchSnapshot::new(settings.fetch_interval_seconds),
            ..GitSnapshot::default()
        };
        let (sender, current) = watch::channel(Arc::new(initial));
        let (stop, stopped) = watch::channel(false);
        let refresh = Arc::new(Notify::new());
        let fetch_task = (settings.fetch_interval_seconds != 0).then(|| {
            tokio::spawn(fetch::observe(
                cwd.clone(),
                settings.fetch_interval_seconds,
                sender.clone(),
                refresh.clone(),
                stopped.clone(),
            ))
        });
        let task = tokio::spawn(observe(cwd, sender, refresh.clone(), stopped));
        Self {
            current,
            refresh,
            stop,
            task: Some(task),
            fetch_task,
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
        for mut task in [self.task.take(), self.fetch_task.take()]
            .into_iter()
            .flatten()
        {
            if tokio::time::timeout(Duration::from_secs(7), &mut task)
                .await
                .is_err()
            {
                // Managed's drop guard still owns and terminates any child tree.
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for GitStatus {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(task) = self.fetch_task.take() {
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
        let mut result = inspect(&git, &root, TIMEOUT, &mut stopped).await;
        if matches!(result, Err(Failure::Cancelled)) {
            return;
        }
        if let Ok(current) = &mut result {
            let previous = sender.borrow().history.clone();
            if let Some(history) =
                cached_history(&previous, current.head.as_deref(), Instant::now())
            {
                current.history = history;
            } else if let Some(head) = current.head.clone() {
                // Publish the new HEAD without the old branch's commits while log runs.
                current.history = GitHistory {
                    head: Some(head.clone()),
                    ..GitHistory::default()
                };
                publish(&sender, Ok(current.clone()), Instant::now());
                let history = inspect_history(&git, &root, &head, TIMEOUT, &mut stopped).await;
                if matches!(history, Err(Failure::Cancelled)) {
                    return;
                }
                let checked_at = Instant::now();
                let mut view = GitHistory {
                    head: Some(head.clone()),
                    checked_at: Some(checked_at),
                    ..GitHistory::default()
                };
                match history {
                    Ok(commits) => {
                        view.commits = commits;
                        view.stale = false;
                        view.observed_at_unix = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .ok()
                            .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok());
                        // A checkout may have completed while log was reading. Recheck
                        // status before attaching the result to a displayed branch.
                        result = inspect(&git, &root, TIMEOUT, &mut stopped).await;
                        if let Ok(verified) = &mut result {
                            if verified.head.as_deref() == Some(head.as_str()) {
                                verified.history = view;
                            } else {
                                verified.history.head = verified.head.clone();
                                refresh.notify_one();
                            }
                        }
                    }
                    Err(Failure::Unavailable(error) | Failure::Error(error)) => {
                        view.error = Some(error.into());
                        current.history = view;
                    }
                    Err(Failure::NotRepository) => result = Err(Failure::NotRepository),
                    Err(Failure::Cancelled) => return,
                }
            }
        }
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
    let mut next = match result {
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
            history: GitHistory {
                checked_at: Some(now),
                stale: false,
                ..GitHistory::default()
            },
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
            previous.history.stale = true;
            previous.last_error = Some(error.into());
            previous
        }
    };
    // This observer never prints. Repeated errors only replace the status view.
    sender.send_modify(|current| {
        next.fetch = current.fetch.clone();
        *current = Arc::new(next);
    });
}

fn base_command(git: &Path, root: &Path) -> Result<Command, Failure> {
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

fn command(git: &Path, root: &Path) -> Result<Command, Failure> {
    let mut command = base_command(git, root)?;
    command.args([
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--untracked-files=normal",
        "--ahead-behind",
        "--",
        ".",
    ]);
    Ok(command)
}

fn cached_history(previous: &GitHistory, head: Option<&str>, now: Instant) -> Option<GitHistory> {
    let Some(head) = head else {
        return Some(GitHistory {
            checked_at: Some(now),
            stale: false,
            ..GitHistory::default()
        });
    };
    if previous.head.as_deref() != Some(head) {
        return None;
    }
    if previous.error.is_none()
        && previous
            .commits
            .first()
            .is_some_and(|commit| commit.full_hash == head)
    {
        let mut history = previous.clone();
        // A fresh status with this same HEAD validates a previously stale view.
        history.stale = false;
        return Some(history);
    }
    if previous.error.is_some()
        && previous
            .checked_at
            .is_some_and(|checked| now.saturating_duration_since(checked) < HISTORY_RETRY)
    {
        let mut history = previous.clone();
        history.commits.clear();
        history.stale = true;
        return Some(history);
    }
    None
}

fn valid_oid(value: &[u8]) -> bool {
    matches!(value.len(), 40 | 64) && value.iter().all(u8::is_ascii_hexdigit)
}

fn parse_history(bytes: &[u8], head: &str) -> Result<Vec<GitCommit>, &'static str> {
    const INVALID: &str = "Git returned incomplete or invalid commit history";
    if !bytes.ends_with(&[0]) || !valid_oid(head.as_bytes()) {
        return Err(INVALID);
    }
    let fields = bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .collect::<Vec<_>>();
    if fields.is_empty() || !fields.len().is_multiple_of(4) || fields.len() / 4 > HISTORY_LIMIT {
        return Err(INVALID);
    }
    let mut commits = Vec::with_capacity(fields.len() / 4);
    for fields in fields.as_chunks::<4>().0 {
        let full_hash = std::str::from_utf8(fields[0]).map_err(|_| INVALID)?;
        let short_hash = std::str::from_utf8(fields[1]).map_err(|_| INVALID)?;
        if !valid_oid(fields[0])
            || short_hash.len() < 4
            || !full_hash.starts_with(short_hash)
            || commits
                .iter()
                .any(|commit: &GitCommit| commit.full_hash == full_hash)
        {
            return Err(INVALID);
        }
        let committed_at_unix = std::str::from_utf8(fields[2])
            .map_err(|_| INVALID)?
            .parse::<i64>()
            .map_err(|_| INVALID)?;
        commits.push(GitCommit {
            full_hash: full_hash.into(),
            short_hash: short_hash.into(),
            subject: display(fields[3]),
            committed_at_unix,
        });
    }
    if commits
        .first()
        .is_none_or(|commit| commit.full_hash != head)
    {
        return Err(INVALID);
    }
    Ok(commits)
}

async fn inspect_history(
    git: &Path,
    root: &Path,
    head: &str,
    timeout: Duration,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Vec<GitCommit>, Failure> {
    if *stopped.borrow() {
        return Err(Failure::Cancelled);
    }
    if !valid_oid(head.as_bytes()) {
        return Err(Failure::Error(
            "Git history requires a complete HEAD object id",
        ));
    }
    let mut command = base_command(git, root)?;
    // NUL-terminated tformat gives exactly four fields per commit. A literal
    // object id pins the query; a concurrent checkout cannot retarget the log.
    // No signature verification, patch/textconv, notes, pager or hooks run.
    command
        .args([
            "--no-replace-objects",
            "-c",
            "log.showSignature=false",
            "-c",
            if cfg!(windows) {
                "core.hooksPath=NUL"
            } else {
                "core.hooksPath=/dev/null"
            },
            "log",
            "--max-count=5",
            "--no-patch",
            "--no-ext-diff",
            "--no-textconv",
            "--no-show-signature",
            "--no-decorate",
            "--no-notes",
            "--no-color",
            "--encoding=UTF-8",
            "--abbrev=8",
            "-z",
            "--format=tformat:%H%x00%h%x00%ct%x00%s",
        ])
        .arg(head)
        .arg("--");
    let mut process = Managed::spawn(&mut command)
        .map_err(|_| Failure::Unavailable("Git history could not start"))?;
    let stdout = process.child.stdout.take().expect("piped Git stdout");
    let stderr = process.child.stderr.take().expect("piped Git stderr");
    let result = {
        let complete = async {
            let (stdout, stderr, exit) = tokio::try_join!(
                limited(stdout, HISTORY_OUTPUT_LIMIT),
                limited(stderr, ERROR_LIMIT),
                async {
                    process
                        .wait()
                        .await
                        .map_err(|_| Failure::Error("Git history could not be reaped"))
                }
            )?;
            if !exit.success() {
                if stderr.starts_with(b"fatal: not a git repository") {
                    return Err(Failure::NotRepository);
                }
                return Err(Failure::Error(
                    "Git history failed; commits are unavailable",
                ));
            }
            parse_history(&stdout, head).map_err(Failure::Error)
        };
        tokio::select! {
            biased;
            _ = stopped.changed() => Err(Failure::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(Failure::Error("Git history timed out")),
            result = complete => result,
        }
    };
    process.shutdown().await;
    result
}

async fn limited(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| Failure::Error("Git output could not be read"))?;
    if bytes.len() > limit {
        return Err(Failure::Error("Git output exceeded its limit"));
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
    fn parses_native_history_without_losing_hashes_times_or_terminal_safety() {
        let head = "a".repeat(40);
        let parent = "b".repeat(64);
        let bytes = format!(
            "{head}\0aaaaaaaa\0-123\0subject\x1b[31m\nnext\0{parent}\0bbbbbbbb\0{time}\0parent\0",
            time = 1_700_000_000
        )
        .into_bytes();
        let commits = parse_history(&bytes, &head).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].full_hash, head);
        assert_eq!(commits[1].full_hash, parent);
        assert_eq!(commits[0].committed_at_unix, -123);
        assert_eq!(commits[1].committed_at_unix, 1_700_000_000);
        assert_eq!(commits[0].subject, "subject\\u{1b}[31m\\nnext");
        assert!(parse_history(&bytes[..bytes.len() - 1], &head).is_err());
        assert!(parse_history(&bytes, &"c".repeat(40)).is_err());
        assert!(
            parse_history(
                format!("{head}\0aaaaaaaa\0invalid\0subject\0").as_bytes(),
                &head
            )
            .is_err()
        );
        assert!(
            parse_history(
                format!("{head}\0wrong\0{time}\0subject\0", time = 0).as_bytes(),
                &head
            )
            .is_err()
        );
    }

    #[test]
    fn cached_history_tracks_the_exact_head_and_failed_reads_hide_old_commits() {
        let now = Instant::now();
        let head = "a".repeat(40);
        let newer = "b".repeat(40);
        let previous = GitHistory {
            head: Some(head.clone()),
            commits: vec![GitCommit {
                full_hash: head.clone(),
                short_hash: "aaaaaaaa".into(),
                subject: "known commit".into(),
                committed_at_unix: 123,
            }],
            checked_at: Some(now),
            observed_at_unix: Some(456),
            stale: true,
            error: None,
        };
        let cached = cached_history(&previous, Some(&head), now + POLL).unwrap();
        assert!(!cached.stale);
        assert_eq!(cached.checked_at, Some(now));
        assert_eq!(cached.commits, previous.commits);
        assert!(cached_history(&previous, Some(&newer), now).is_none());
        let failed = GitHistory {
            head: Some(newer.clone()),
            error: Some("Git history timed out".into()),
            ..previous
        };
        let cached = cached_history(&failed, Some(&newer), now + POLL).unwrap();
        assert!(cached.stale);
        assert!(cached.commits.is_empty());
        assert!(cached_history(&failed, Some(&newer), now + HISTORY_RETRY).is_none());
        let unborn = cached_history(&failed, None, now).unwrap();
        assert!(!unborn.stale);
        assert!(unborn.head.is_none());
        assert!(unborn.commits.is_empty());
        assert!(unborn.error.is_none());
    }

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
        let history_root = dir.0.join("history");
        let cancel_root = dir.0.join("cancel");
        std::fs::create_dir(&history_root).unwrap();
        std::fs::create_dir(&cancel_root).unwrap();
        let fixture = dir.0.join("slow-git");
        std::fs::write(
            &fixture,
            "#!/bin/sh\nsleep 30 &\nprintf '%s\\n' \"$!\" > child.pid.tmp && mv child.pid.tmp child.pid || exit 1\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();
        let parse_pid = |stage: &str, raw: &str| {
            raw.trim()
                .parse::<i32>()
                .ok()
                .filter(|&pid| pid > 0)
                .unwrap_or_else(|| panic!("{stage}: invalid fixture child PID {raw:?}"))
        };
        let assert_reaped = |stage: &str, pid: i32, before: Option<&str>| {
            let proc = Path::new("/proc").join(pid.to_string());
            assert!(
                !proc.try_exists().unwrap(),
                "{stage}: PID {pid} still exists after shutdown, before={before:?}, after={:?}",
                std::fs::read_to_string(proc.join("stat"))
            );
        };
        let (stop, mut stopped) = watch::channel(false);
        let result = inspect(&fixture, &dir.0, Duration::from_millis(150), &mut stopped).await;
        assert!(matches!(
            result,
            Err(Failure::Error("Git status timed out"))
        ));
        let pid =
            std::fs::read_to_string(dir.0.join("child.pid")).expect("status fixture child PID");
        assert_reaped("status", parse_pid("status", &pid), None);
        let (_history_stop, mut history_stopped) = watch::channel(false);
        let history = inspect_history(
            &fixture,
            &history_root,
            &"a".repeat(40),
            Duration::from_millis(150),
            &mut history_stopped,
        )
        .await;
        assert!(matches!(
            history,
            Err(Failure::Error("Git history timed out"))
        ));
        let pid = std::fs::read_to_string(history_root.join("child.pid"))
            .expect("history fixture child PID");
        assert_reaped("history", parse_pid("history", &pid), None);

        let query = inspect(&fixture, &cancel_root, TIMEOUT, &mut stopped);
        tokio::pin!(query);
        let marker = cancel_root.join("child.pid");
        let ready = async {
            loop {
                match std::fs::read_to_string(&marker) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    result => break result,
                }
            }
        };
        let pid = tokio::select! {
            result = &mut query => panic!("cancel fixture exited before child ready: {result:?}"),
            raw = ready => parse_pid("cancel", &raw.expect("cancel fixture child PID")),
        };
        let before = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .expect("cancel fixture child must exist before cancellation");
        let state = before
            .rsplit_once(')')
            .and_then(|(_, fields)| fields.split_whitespace().next());
        assert!(
            matches!(state, Some(state) if state != "Z" && state != "X"),
            "{before}"
        );
        stop.send(true).unwrap();
        let result = query.await;
        assert!(matches!(result, Err(Failure::Cancelled)));
        assert_reaped("cancel", pid, Some(&before));
    }

    #[tokio::test]
    async fn native_history_is_bounded_read_only_and_pinned_across_checkouts() {
        let executable = crate::program::executable("git").expect("installed Git for history test");
        let dir = Directory::new();
        let root = &dir.0;
        git(&executable, root, &["init", "-q", "--initial-branch=main"]);
        for index in 0..6 {
            git(
                &executable,
                root,
                &[
                    "commit",
                    "--allow-empty",
                    "-qm",
                    &format!("history {index}"),
                ],
            );
        }
        let index = root.join(".git/index");
        let before = std::fs::read(&index).ok();
        let modified = std::fs::metadata(&index)
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        let (_stop, mut stopped) = watch::channel(false);
        let status = inspect(&executable, root, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        let head = status.head.unwrap();
        let history = inspect_history(&executable, root, &head, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history[0].full_hash, head);
        assert_eq!(history[0].subject, "history 5");
        assert_eq!(history[4].subject, "history 1");
        assert!(
            history
                .iter()
                .all(|commit| commit.full_hash.starts_with(&commit.short_hash)
                    && commit.short_hash.len() >= 8
                    && commit.committed_at_unix > 0)
        );
        assert_eq!(std::fs::read(&index).ok(), before);
        assert_eq!(
            std::fs::metadata(&index)
                .ok()
                .and_then(|metadata| metadata.modified().ok()),
            modified
        );
        git(&executable, root, &["checkout", "-qb", "another-branch"]);
        git(
            &executable,
            root,
            &["commit", "--allow-empty", "-qm", "after switch"],
        );
        let pinned = inspect_history(&executable, root, &head, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(
            pinned, history,
            "an in-flight query must not follow a changed HEAD"
        );
        let current = inspect(&executable, root, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(current.branch.as_deref(), Some("another-branch"));
        let current_head = current.head.unwrap();
        assert_ne!(current_head, head);
        let changed = inspect_history(&executable, root, &current_head, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(changed[0].subject, "after switch");
        assert_eq!(changed[0].full_hash, current_head);
    }
}
