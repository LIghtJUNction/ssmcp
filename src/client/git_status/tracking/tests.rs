use super::super::{TIMEOUT, inspect};
use super::Tracking;
use crate::wire::GitTrackingChange;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::sync::watch;

const UPSTREAM: &str = "refs/remotes/origin/main";
static NEXT_REPO: AtomicU64 = AtomicU64::new(0);

struct Repo {
    git: PathBuf,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let git = crate::program::executable("git").expect("git is required for this test");
        let root = loop {
            let id = NEXT_REPO.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ssmcp-tracking-{}-{id}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => break path,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("create test repository at {path:?}: {error}"),
            }
        };
        let repo = Self { git, root };
        repo.run(&["init", "--quiet", "--template="]);
        repo.run(&["symbolic-ref", "HEAD", "refs/heads/main"]);
        repo.run(&["commit", "--allow-empty", "-qm", "base"]);
        repo.run(&["remote", "add", "origin", "."]);
        repo.run(&["update-ref", UPSTREAM, "HEAD"]);
        repo.run(&["branch", "--set-upstream-to=origin/main", "main"]);
        repo
    }

    fn run(&self, args: &[&str]) -> String {
        let mut git = Command::new(&self.git);
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                git.env_remove(key);
            }
        }
        let output = git
            .current_dir(&self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.root.join(".git").join("unused-global-config"),
            )
            .args([
                "-c",
                "user.name=Tracking Test",
                "-c",
                "user.email=tracking@example.invalid",
            ])
            .args(args)
            .output()
            .expect("start git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    // commit-tree creates an upstream commit without moving the checked-out branch.
    fn child(&self, parent: &str, message: &str) -> String {
        let tree = self.run(&["rev-parse", &format!("{parent}^{{tree}}")]);
        self.run(&["commit-tree", &tree, "-p", parent, "-m", message])
    }

    async fn observe(
        &self,
        tracking: &mut Tracking,
        stopped: &mut watch::Receiver<bool>,
    ) -> Option<GitTrackingChange> {
        let snapshot = inspect(&self.git, &self.root, TIMEOUT, stopped)
            .await
            .unwrap();
        tracking
            .observe(&self.git, &self.root, &snapshot, stopped)
            .await
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn reports_upstream_oid_changes_even_when_divergence_is_unchanged() {
    let repo = Repo::new();
    let (_stop_tx, mut stopped) = watch::channel(false);
    let mut tracking = Tracking::default();
    let base = repo.run(&["rev-parse", "HEAD"]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    fs::write(repo.root.join("untracked.txt"), b"working tree change").unwrap();
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    let first = repo.child(&base, "upstream change");
    repo.run(&["update-ref", UPSTREAM, &first]);
    let event = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(event.branch, "main");
    assert_eq!(event.upstream, UPSTREAM);
    assert_eq!(event.head, base);
    assert_eq!(event.previous_upstream_oid, base);
    assert_eq!(event.upstream_oid, first);
    assert_eq!((event.ahead, event.behind), (0, 1));
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    let replacement = repo.child(&base, "replacement upstream change");
    assert_ne!(replacement, first);
    repo.run(&["update-ref", UPSTREAM, &replacement]);
    let event = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(event.previous_upstream_oid, first);
    assert_eq!(event.upstream_oid, replacement);
    assert_eq!((event.ahead, event.behind), (0, 1));
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    repo.run(&["commit", "--allow-empty", "-qm", "local change"]);
    let local = repo.run(&["rev-parse", "HEAD"]);
    let local_change = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(local_change.head, local);
    assert_eq!(local_change.previous_upstream_oid, replacement);
    assert_eq!(local_change.upstream_oid, replacement);
    assert_eq!((local_change.ahead, local_change.behind), (1, 1));
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());
    let next = repo.child(&replacement, "second upstream change");
    repo.run(&["update-ref", UPSTREAM, &next]);
    let event = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(event.head, local);
    assert_eq!(event.previous_upstream_oid, replacement);
    assert_eq!(event.upstream_oid, next);
    assert_eq!((event.ahead, event.behind), (1, 2));
}

#[tokio::test]
async fn detaching_and_reattaching_starts_a_new_baseline() {
    let repo = Repo::new();
    let (_stop_tx, mut stopped) = watch::channel(false);
    let mut tracking = Tracking::default();
    let base = repo.run(&["rev-parse", "HEAD"]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    repo.run(&["checkout", "--quiet", "--detach", "HEAD"]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());
    let detached_tip = repo.child(&base, "upstream moved while detached");
    repo.run(&["update-ref", UPSTREAM, &detached_tip]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    repo.run(&["checkout", "--quiet", "main"]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());
    let next = repo.child(&detached_tip, "upstream moved after reattach");
    repo.run(&["update-ref", UPSTREAM, &next]);
    let event = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(event.previous_upstream_oid, detached_tip);
    assert_eq!(event.upstream_oid, next);
    assert_eq!((event.ahead, event.behind), (0, 2));
}

#[tokio::test]
async fn incomplete_observations_preserve_the_last_verified_upstream_oid() {
    let repo = Repo::new();
    let (_stop_tx, mut stopped) = watch::channel(false);
    let mut tracking = Tracking::default();
    let base = repo.run(&["rev-parse", "HEAD"]);
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());

    let moved = repo.child(&base, "upstream moved");
    repo.run(&["update-ref", UPSTREAM, &moved]);
    let mut snapshot = inspect(&repo.git, &repo.root, TIMEOUT, &mut stopped)
        .await
        .unwrap();
    let absent_git = repo.root.join("absent-git");
    assert!(
        tracking
            .observe(&absent_git, &repo.root, &snapshot, &mut stopped)
            .await
            .is_none()
    );
    snapshot.behind = None;
    assert!(
        tracking
            .observe(&repo.git, &repo.root, &snapshot, &mut stopped)
            .await
            .is_none()
    );

    let change = repo.observe(&mut tracking, &mut stopped).await.unwrap();
    assert_eq!(change.previous_upstream_oid, base);
    assert_eq!(change.upstream_oid, moved);
    assert_eq!((change.ahead, change.behind), (0, 1));
    assert!(repo.observe(&mut tracking, &mut stopped).await.is_none());
}
