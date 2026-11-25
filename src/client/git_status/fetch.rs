use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum FetchState {
    #[default]
    Disabled,
    Waiting,
    Running,
    Fresh,
    Failed,
    NoUpstream,
}

impl FetchState {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Fresh => "fresh",
            Self::Failed => "failed",
            Self::NoUpstream => "no-upstream",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FetchSnapshot {
    pub state: FetchState,
    pub interval_seconds: u64,
    pub remote: Option<String>,
    pub last_attempt_at: Option<Instant>,
    pub last_success_at: Option<Instant>,
    pub last_error: Option<String>,
}

impl FetchSnapshot {
    pub(super) fn new(interval_seconds: u64) -> Self {
        Self {
            state: if interval_seconds == 0 {
                FetchState::Disabled
            } else {
                FetchState::Waiting
            },
            interval_seconds,
            ..Self::default()
        }
    }
}

#[derive(Debug)]
enum Error {
    Cancelled,
    Failed(&'static str),
}

struct Output {
    code: Option<i32>,
    stdout: Vec<u8>,
}

async fn run(
    mut command: Command,
    timeout: Duration,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Output, Error> {
    if *stopped.borrow() {
        return Err(Error::Cancelled);
    }
    let mut process =
        Managed::spawn(&mut command).map_err(|_| Error::Failed("Git fetch could not start"))?;
    let stdout = process.child.stdout.take().expect("piped Git stdout");
    let stderr = process.child.stderr.take().expect("piped Git stderr");
    let result = {
        let complete = async {
            let (stdout, _stderr, exit) = tokio::try_join!(
                limited(stdout, 64 * 1024),
                limited(stderr, ERROR_LIMIT),
                async {
                    process
                        .wait()
                        .await
                        .map_err(|_| Failure::Error("Git fetch could not be reaped"))
                },
            )
            .map_err(|_| Error::Failed("Git fetch output could not be read within its limit"))?;
            Ok(Output {
                code: exit.code(),
                stdout,
            })
        };
        tokio::select! {
            biased;
            _ = stopped.changed() => Err(Error::Cancelled),
            _ = tokio::time::sleep(timeout) => Err(Error::Failed("Git fetch timed out")),
            result = complete => result,
        }
    };
    process.shutdown().await;
    result
}

async fn read(
    git: &Path,
    root: &Path,
    arguments: &[&str],
    stopped: &mut watch::Receiver<bool>,
) -> Result<Output, Error> {
    let mut command =
        base_command(git, root).map_err(|_| Error::Failed("Git workspace is unavailable"))?;
    command.args(arguments);
    run(command, TIMEOUT, stopped).await
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    remote: String,
    source: String,
    destination: String,
}

async fn target(
    git: &Path,
    root: &Path,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Option<(Target, PathBuf)>, Error> {
    let branch = read(git, root, &["symbolic-ref", "--quiet", "HEAD"], stopped).await?;
    if branch.code == Some(1) {
        return Ok(None);
    }
    if branch.code != Some(0) {
        return Err(Error::Failed("Git fetch requires a repository"));
    }
    let branch = std::str::from_utf8(&branch.stdout)
        .ok()
        .and_then(|s| s.strip_suffix('\n'))
        .filter(|s| {
            s.starts_with("refs/heads/") && s.len() <= 2048 && !s.chars().any(char::is_control)
        })
        .ok_or(Error::Failed("Git branch could not be identified"))?;
    let upstream = read(
        git,
        root,
        &[
            "for-each-ref",
            "--format=%(upstream:remotename)%00%(upstream:remoteref)%00%(upstream)",
            "--",
            branch,
        ],
        stopped,
    )
    .await?;
    if upstream.code != Some(0) {
        return Err(Error::Failed("Git upstream could not be read"));
    }
    let text = std::str::from_utf8(&upstream.stdout)
        .map_err(|_| Error::Failed("Git upstream is not UTF-8"))?;
    let fields: Vec<_> = text.trim_end_matches('\n').split('\0').collect();
    if fields.len() != 3 || fields[0].is_empty() || fields[0] == "." {
        return Ok(None);
    }
    if fields
        .iter()
        .any(|s| s.len() > 2048 || s.chars().any(char::is_control))
        || fields[0].starts_with('-')
        || !fields[1].starts_with("refs/heads/")
        || !fields[2].starts_with("refs/remotes/")
    {
        return Err(Error::Failed(
            "Git fetch requires a remote-tracking upstream",
        ));
    }
    // Never follow a remote-tracking symbolic ref into a local branch.
    let symbolic = read(git, root, &["symbolic-ref", "--quiet", fields[2]], stopped).await?;
    if symbolic.code != Some(1) {
        return Err(Error::Failed("Git upstream must not be a symbolic ref"));
    }
    let common = read(
        git,
        root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        stopped,
    )
    .await?;
    let common = std::str::from_utf8(&common.stdout)
        .ok()
        .and_then(|s| s.strip_suffix('\n'))
        .filter(|s| !s.chars().any(char::is_control))
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or(Error::Failed(
            "Git fetch coordination directory is unavailable",
        ))?;
    Ok(Some((
        Target {
            remote: fields[0].into(),
            source: fields[1].into(),
            destination: fields[2].into(),
        },
        common,
    )))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attempt {
    target: Target,
    unix_seconds: u64,
    retry_seconds: u64,
    failures: u32,
}

struct Lease {
    file: File,
    attempt: Attempt,
}

impl Drop for Lease {
    fn drop(&mut self) {
        // A concurrent fork can retain the same open file description until
        // exec. Release our lock explicitly before closing this descriptor.
        while let Err(error) = self.file.unlock() {
            if error.kind() != std::io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

impl Lease {
    fn save(&mut self) -> Result<(), Error> {
        let bytes = serde_json::to_vec(&self.attempt)
            .map_err(|_| Error::Failed("Git fetch schedule could not be saved"))?;
        self.file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.set_len(0))
            .and_then(|_| self.file.write_all(&bytes))
            .and_then(|_| self.file.sync_data())
            .map_err(|_| Error::Failed("Git fetch schedule could not be saved"))
    }
    fn finish(&mut self, success: bool, interval: u64) -> Result<(), Error> {
        self.attempt.failures = if success {
            0
        } else {
            self.attempt.failures.saturating_add(1)
        };
        self.attempt.retry_seconds = interval
            .saturating_mul(1 << self.attempt.failures.min(5))
            .min(interval.max(1800));
        self.save()
    }
}

// The guard explicitly unlocks every exit after acquisition, including cache
// read failures and not-yet-due returns. The common file coordinates linked
// worktrees and other ssmcp clients so their background fetches do not overlap.
fn acquire(
    common: &Path,
    target: &Target,
    interval: u64,
    now: u64,
) -> Result<Option<Lease>, Error> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    }
    let file = options
        .open(common.join("ssmcp-fetch.json"))
        .map_err(|_| Error::Failed("Git fetch coordination is unavailable"))?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= 4096)
    {
        return Err(Error::Failed("Git fetch schedule is invalid"));
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(_) => return Err(Error::Failed("Git fetch coordination is unavailable")),
    }
    let mut lease = Lease {
        file,
        attempt: Attempt {
            target: target.clone(),
            unix_seconds: now,
            retry_seconds: interval,
            failures: 0,
        },
    };
    let mut bytes = Vec::new();
    (&mut lease.file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::Failed("Git fetch schedule is unreadable"))?;
    // The schedule is a disposable cache. Recover a torn timestamp write under
    // the lease instead of permanently disabling fetch after an interrupted exit.
    let old = serde_json::from_slice::<Attempt>(&bytes).ok();
    if let Some(old) = &old
        && old.target == *target
        && now >= old.unix_seconds
        && now - old.unix_seconds < interval.max(old.retry_seconds.min(86_400))
    {
        return Ok(None);
    }
    lease.attempt.failures = old
        .as_ref()
        .filter(|v| v.target == *target)
        .map_or(0, |v| v.failures);
    lease.save()?;
    Ok(Some(lease))
}

fn fetch_command(git: &Path, root: &Path, target: &Target) -> Result<Command, Error> {
    let mut command = pull_requests::remote_command(git, root)
        .map_err(|_| Error::Failed("Git workspace is unavailable"))?;
    command
        .args([
            "fetch",
            "--quiet",
            "--atomic",
            "--no-tags",
            "--no-prune",
            "--no-prune-tags",
            "--no-write-fetch-head",
            "--no-auto-maintenance",
            "--no-write-commit-graph",
            "--recurse-submodules=no",
            "--refmap=",
            "--",
        ])
        .arg(&target.remote)
        .arg(format!("+{}:{}", target.source, target.destination));
    Ok(command)
}

fn update(
    sender: &watch::Sender<Arc<GitSnapshot>>,
    state: FetchState,
    remote: Option<&str>,
    error: Option<&str>,
) {
    sender.send_modify(|value| {
        let fetch = &mut Arc::make_mut(value).fetch;
        fetch.state = state;
        fetch.remote = remote.map(str::to_owned);
        fetch.last_error = error.map(str::to_owned);
        if state == FetchState::Running {
            fetch.last_attempt_at = Some(Instant::now());
        }
        if state == FetchState::Fresh {
            fetch.last_success_at = Some(Instant::now());
        }
    });
}

async fn attempt(
    git: &Path,
    root: &Path,
    interval: u64,
    sender: &watch::Sender<Arc<GitSnapshot>>,
    refresh: &Notify,
    stopped: &mut watch::Receiver<bool>,
) -> Result<Option<String>, Error> {
    let Some((target, common)) = target(git, root, stopped).await? else {
        update(sender, FetchState::NoUpstream, None, None);
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Failed("System time is unavailable"))?
        .as_secs();
    let lock_target = target.clone();
    let lease = tokio::task::spawn_blocking(move || acquire(&common, &lock_target, interval, now))
        .await
        .map_err(|_| Error::Failed("Git fetch coordination failed"))??;
    let Some(mut lease) = lease else {
        update(sender, FetchState::Waiting, Some(&target.remote), None);
        return Ok(Some(target.remote));
    };
    update(sender, FetchState::Running, Some(&target.remote), None);
    let result = run(fetch_command(git, root, &target)?, FETCH_TIMEOUT, stopped).await;
    if matches!(result, Err(Error::Cancelled)) {
        return Err(Error::Cancelled);
    }
    let success = result.as_ref().is_ok_and(|result| result.code == Some(0));
    lease.finish(success, interval)?;
    match result {
        Ok(result) if result.code == Some(0) => {
            update(sender, FetchState::Fresh, Some(&target.remote), None);
            refresh.notify_one();
            Ok(Some(target.remote))
        }
        Ok(_) => Err(Error::Failed(
            "Git fetch failed; check remote credentials or network",
        )),
        Err(error) => Err(error),
    }
}

pub(super) async fn observe(
    cwd: PathBuf,
    interval: u64,
    sender: watch::Sender<Arc<GitSnapshot>>,
    refresh: Arc<Notify>,
    events: Option<mpsc::Sender<Event>>,
    mut stopped: watch::Receiver<bool>,
) {
    let initialize = async {
        if !cwd.is_absolute() {
            return None;
        }
        let root = tokio::fs::canonicalize(cwd).await.ok()?;
        let git = tokio::task::spawn_blocking(|| crate::program::executable("git"))
            .await
            .ok()?
            .ok()?;
        Some((root, git))
    };
    let initialized = tokio::select! { biased; _ = stopped.changed() => return, value = tokio::time::timeout(TIMEOUT, initialize) => value.ok().flatten() };
    let Some((root, git)) = initialized else {
        update(
            &sender,
            FetchState::Failed,
            None,
            Some("Git is unavailable"),
        );
        return;
    };
    let mut pulls = pull_requests::PullRequests::default();
    loop {
        if *stopped.borrow() {
            return;
        }
        match attempt(&git, &root, interval, &sender, &refresh, &mut stopped).await {
            Ok(Some(remote)) => {
                if let Some(tx) = &events {
                    match pulls.observe(&git, &root, &remote, &mut stopped).await {
                        Ok(numbers) => {
                            for number in numbers {
                                let event = Event::PullRequestOpened {
                                    remote: remote.clone(),
                                    number,
                                };
                                if event.validate().is_ok() {
                                    tokio::select! {
                                        biased;
                                        _ = stopped.changed() => return,
                                        result = tx.send(event) => if result.is_err() { return; },
                                    }
                                }
                            }
                        }
                        Err(Failure::Cancelled) => return,
                        Err(_) => {} // Retry at the next remote refresh with the same baseline.
                    }
                }
            }
            Ok(None) => {
                pulls = pull_requests::PullRequests::default();
            }
            Err(Error::Cancelled) => return,
            Err(Error::Failed(error)) => {
                let remote = sender.borrow().fetch.remote.clone();
                update(&sender, FetchState::Failed, remote.as_deref(), Some(error));
            }
        }
        tokio::select! { biased; _ = stopped.changed() => return, _ = tokio::time::sleep(Duration::from_secs(interval)) => {} }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-fetch-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
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

    fn git(executable: &Path, root: &Path, args: &[&str]) -> Vec<u8> {
        let output = std::process::Command::new(executable)
            .current_dir(root)
            .args([
                "-c",
                "user.name=ssmcp fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("no-global-config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture Git error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[tokio::test]
    async fn periodic_fetch_updates_upstream_without_touching_head_index_worktree_tags_or_fetch_head()
     {
        let executable = crate::program::executable("git").unwrap();
        let directory = Directory::new();
        let bare = directory.0.join("remote.git");
        let source = directory.0.join("source");
        let root = directory.0.join("worktree");
        git(
            &executable,
            &directory.0,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                bare.to_str().unwrap(),
            ],
        );
        git(
            &executable,
            &directory.0,
            &["init", "--initial-branch=main", source.to_str().unwrap()],
        );
        std::fs::write(source.join("tracked"), "initial\n").unwrap();
        git(&executable, &source, &["add", "tracked"]);
        git(&executable, &source, &["commit", "-qm", "initial"]);
        git(
            &executable,
            &source,
            &["remote", "add", "origin", bare.to_str().unwrap()],
        );
        git(&executable, &source, &["push", "-q", "origin", "main"]);
        git(
            &executable,
            &directory.0,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&executable, &root, &["tag", "local-tag"]);
        let head = git(&executable, &root, &["rev-parse", "HEAD"]);
        let tag = git(&executable, &root, &["rev-parse", "refs/tags/local-tag"]);
        std::fs::write(root.join("tracked"), "dirty local edit\n").unwrap();
        std::fs::write(root.join("staged"), "staged local data\n").unwrap();
        git(&executable, &root, &["add", "staged"]);
        std::fs::write(root.join("untracked"), "untracked local data\n").unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        std::fs::write(root.join(".git/FETCH_HEAD"), "preserve-manual-fetch\n").unwrap();
        std::fs::write(source.join("tracked"), "new remote change\n").unwrap();
        git(&executable, &source, &["commit", "-qam", "remote update"]);
        git(&executable, &source, &["tag", "local-tag"]);
        git(
            &executable,
            &source,
            &["push", "-q", "origin", "main", "--tags"],
        );
        let (_stop, mut stopped) = watch::channel(false);
        let before = inspect(&executable, &root, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(before.behind, Some(0));
        let (target, common) = target(&executable, &root, &mut stopped)
            .await
            .unwrap()
            .unwrap();
        // A late config change must not redirect this background fetch into local branches.
        git(
            &executable,
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
            ],
        );
        let mut lease = acquire(&common, &target, 300, 1000).unwrap().unwrap();
        let output = run(
            fetch_command(&executable, &root, &target).unwrap(),
            FETCH_TIMEOUT,
            &mut stopped,
        )
        .await
        .unwrap();
        assert_eq!(output.code, Some(0));
        lease.finish(true, 300).unwrap();
        drop(lease);
        git(
            &executable,
            &root,
            &[
                "config",
                "remote.origin.fetch",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        let after = inspect(&executable, &root, TIMEOUT, &mut stopped)
            .await
            .unwrap();
        assert_eq!(after.behind, Some(1));
        assert_eq!(git(&executable, &root, &["rev-parse", "HEAD"]), head);
        assert_eq!(
            git(&executable, &root, &["rev-parse", "refs/tags/local-tag"]),
            tag
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        for (name, expected) in [
            ("tracked", "dirty local edit\n"),
            ("staged", "staged local data\n"),
            ("untracked", "untracked local data\n"),
            (".git/FETCH_HEAD", "preserve-manual-fetch\n"),
        ] {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), expected);
        }
        git(
            &executable,
            &source,
            &["commit", "--allow-empty", "-qm", "second remote update"],
        );
        git(&executable, &source, &["push", "-q", "origin", "main"]);
        let (events, mut observed) = mpsc::channel(8);
        let observer = GitStatus::start(
            root.clone(),
            crate::client::config::GitSettings {
                fetch_interval_seconds: 60,
            },
            Some(events),
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = observer.snapshot();
                if snapshot.fetch.state == FetchState::Fresh && snapshot.behind == Some(2) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = observed.recv().await {
                if let Event::UpstreamChanged {
                    upstream,
                    ahead: 0,
                    behind: 2,
                    ..
                } = event
                {
                    assert_eq!(upstream, "origin/main");
                    return;
                }
            }
            panic!("observer closed before emitting the remote comparison");
        })
        .await
        .unwrap();
        observer.shutdown().await;
        assert_eq!(git(&executable, &root, &["rev-parse", "HEAD"]), head);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
    }

    #[test]
    fn periodic_fetch_shares_a_nonblocking_lock_schedule_and_failure_backoff() {
        let directory = Directory::new();
        let target = Target {
            remote: "origin".into(),
            source: "refs/heads/main".into(),
            destination: "refs/remotes/origin/main".into(),
        };
        let mut first = acquire(&directory.0, &target, 300, 1000).unwrap().unwrap();
        assert!(acquire(&directory.0, &target, 300, 2000).unwrap().is_none());
        first.finish(false, 300).unwrap();
        drop(first);
        assert!(acquire(&directory.0, &target, 300, 1599).unwrap().is_none());
        let mut retry = acquire(&directory.0, &target, 300, 1600).unwrap().unwrap();
        retry.finish(true, 300).unwrap();
        drop(retry);
        assert!(acquire(&directory.0, &target, 300, 1899).unwrap().is_none());
        assert!(acquire(&directory.0, &target, 300, 1900).unwrap().is_some());
        std::fs::write(directory.0.join("ssmcp-fetch.json"), "{torn").unwrap();
        assert!(acquire(&directory.0, &target, 300, 2000).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn periodic_fetch_unlocks_even_when_another_descriptor_survives() {
        fn return_with_error(lease: Lease) -> Result<(), Error> {
            let _held = lease;
            Err(Error::Failed("fixture early return"))
        }

        for early_return in [false, true] {
            let directory = Directory::new();
            let target = Target {
                remote: "origin".into(),
                source: "refs/heads/main".into(),
                destination: "refs/remotes/origin/main".into(),
            };
            let mut lease = acquire(&directory.0, &target, 300, 1000).unwrap().unwrap();
            // try_clone retains the same Unix open file description, just as
            // an inherited descriptor does before the child reaches exec.
            let retained = lease.file.try_clone().unwrap();
            if early_return {
                assert!(return_with_error(lease).is_err());
            } else {
                lease.finish(true, 300).unwrap();
                drop(lease);
            }
            assert!(acquire(&directory.0, &target, 300, 2000).unwrap().is_some());
            drop(retained);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn periodic_fetch_timeout_and_cancellation_reap_owned_children() {
        use std::os::unix::fs::PermissionsExt;
        let directory = Directory::new();
        let executable = directory.0.join("slow-git");
        std::fs::write(
            &executable,
            "#!/bin/sh\nsleep 30 &\necho $! > child.pid\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = Target {
            remote: "origin".into(),
            source: "refs/heads/main".into(),
            destination: "refs/remotes/origin/main".into(),
        };
        let (_stop, mut stopped) = watch::channel(false);
        assert!(matches!(
            run(
                fetch_command(&executable, &directory.0, &target).unwrap(),
                Duration::from_millis(150),
                &mut stopped
            )
            .await,
            Err(Error::Failed("Git fetch timed out"))
        ));
        let pid = std::fs::read_to_string(directory.0.join("child.pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
        std::fs::remove_file(directory.0.join("child.pid")).unwrap();
        let (stop, mut stopped) = watch::channel(false);
        let command = fetch_command(&executable, &directory.0, &target).unwrap();
        let job =
            tokio::spawn(async move { run(command, Duration::from_secs(20), &mut stopped).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !directory.0.join("child.pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.send(true).unwrap();
        assert!(matches!(job.await.unwrap(), Err(Error::Cancelled)));
        let pid = std::fs::read_to_string(directory.0.join("child.pid")).unwrap();
        assert!(!Path::new("/proc").join(pid.trim()).exists());
    }
}
