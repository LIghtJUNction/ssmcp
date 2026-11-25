use super::*;
use crate::managed_files::{Category, CleanupConfig, CleanupRule};
use std::{os::unix::fs::symlink, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let mut nonce = [0; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let root = std::env::temp_dir().join(format!(
            "ssmcp-managed-git-test-{}",
            crate::hex::encode(nonce)
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        let this = Self(root);
        this.git(&this.0, &["init", "--quiet"]);
        fs::write(this.0.join("tracked"), "seed\n").unwrap();
        fs::write(this.0.join(".gitignore"), "/.ssmcp/\n*.ignored\n").unwrap();
        this.git(&this.0, &["add", "--", ".gitignore", "tracked"]);
        this.git(&this.0, &["commit", "-qm", "seed"]);
        this
    }
    fn git(&self, dir: &Path, args: &[&str]) -> String {
        let o = Command::new(crate::program::executable("git").unwrap())
            .current_dir(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim_end().to_owned()
    }
    fn create(&self) -> (String, PathBuf) {
        let j: Value =
            serde_json::from_str(&command(&self.0, &["create".into()]).unwrap()).unwrap();
        (
            j["id"].as_str().unwrap().to_string(),
            self.0.join(j["path"].as_str().unwrap()),
        )
    }
    fn cleanup(&self, dry_run: bool) -> Value {
        cleanup(
            &self.0,
            dry_run,
            &CleanupConfig {
                max_age_seconds: 0,
                ..Default::default()
            },
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn real_git_respects_active_release_rules_preview_and_unmanaged_directories() {
    let f = Fixture::new();
    assert_eq!(f.cleanup(false)["removed"], 0);
    assert!(!f.0.join(".ssmcp").exists());
    let (id, path) = f.create();
    let other = path.with_file_name("old-user-tree");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("private"), b"mine").unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    command(&f.0, &["release".into(), id.clone().into()]).unwrap();
    release(&f.0, &id).unwrap();
    assert_eq!(
        cleanup(&f.0, false, &CleanupConfig::default()).unwrap()["removed"],
        0
    );
    let mut cfg = CleanupConfig {
        enabled: false,
        max_age_seconds: 0,
        ..Default::default()
    };
    assert_eq!(cleanup(&f.0, false, &cfg).unwrap()["removed"], 0);
    cfg.enabled = true;
    cfg.rules.push(CleanupRule {
        relative_prefix: ".ssmcp/tmp".into(),
        category: Category::Temporary,
        max_age_seconds: Some(0),
    });
    assert_eq!(cleanup(&f.0, false, &cfg).unwrap()["removed"], 0);
    let preview = f.cleanup(true);
    assert_eq!(preview["eligible"], 1, "{preview}");
    assert!(path.exists());
    assert_eq!(f.cleanup(false)["removed"], 1);
    assert!(!path.exists());
    assert!(
        !f.git(&f.0, &["worktree", "list", "--porcelain"])
            .contains(&id)
    );
    assert_eq!(f.cleanup(false)["removed"], 0);
    assert_eq!(fs::read(other.join("private")).unwrap(), b"mine");
}

#[test]
fn changes_ignored_files_and_both_git_and_receipt_locks_are_kept() {
    let f = Fixture::new();
    let (id, path) = f.create();
    release(&f.0, &id).unwrap();
    for file in ["scratch.txt", "build.ignored", "tracked"] {
        fs::write(path.join(file), "work in progress\n").unwrap();
        assert_eq!(f.cleanup(false)["eligible"], 0, "{file}");
        assert!(path.join(file).exists());
        if file == "tracked" {
            f.git(&path, &["restore", "--", file]);
        } else {
            fs::remove_file(path.join(file)).unwrap();
        }
    }
    f.git(&path, &["update-index", "--assume-unchanged", "tracked"]);
    fs::write(path.join("tracked"), "hidden work\n").unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    f.git(&path, &["update-index", "--no-assume-unchanged", "tracked"]);
    f.git(&path, &["restore", "--", "tracked"]);
    f.git(&f.0, &["worktree", "lock", path.to_str().unwrap()]);
    assert_eq!(f.cleanup(false)["eligible"], 0);
    f.git(&f.0, &["worktree", "unlock", path.to_str().unwrap()]);
    let admin = f.0.join(".git/worktrees").join(&id);
    fs::write(admin.join("index.lock"), "").unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    fs::remove_file(admin.join("index.lock")).unwrap();
    let guard = secure_file(&admin.join(RECEIPT), false).unwrap();
    guard.try_lock().unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    drop(guard);
    assert_eq!(f.cleanup(false)["removed"], 1);
}

#[test]
fn detached_commits_survive_reset_until_merged_into_the_project() {
    let f = Fixture::new();
    let (id, path) = f.create();
    release(&f.0, &id).unwrap();
    f.git(&path, &["commit", "--allow-empty", "-qm", "detached"]);
    let tip = f.git(&path, &["rev-parse", "HEAD"]);
    assert_eq!(f.cleanup(false)["eligible"], 0);
    f.git(&path, &["reset", "--hard", "HEAD^"]);
    assert_eq!(f.cleanup(false)["eligible"], 0);
    let branch = f.git(&f.0, &["symbolic-ref", "--short", "HEAD"]);
    f.git(&f.0, &["checkout", "--detach", &tip]);
    assert_eq!(
        f.cleanup(false)["eligible"],
        0,
        "a detached project HEAD is not a durable ref"
    );
    f.git(&f.0, &["checkout", &branch]);
    f.git(&f.0, &["merge", "--ff-only", &tip]);
    assert_eq!(f.cleanup(false)["removed"], 1);
}

#[test]
fn registered_but_uninitialized_gitlinks_must_be_empty_before_removal() {
    let f = Fixture::new();
    let sub = Fixture::new();
    f.git(
        &f.0,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            sub.0.to_str().unwrap(),
            "vendor/dep",
        ],
    );
    f.git(&f.0, &["commit", "-qam", "gitlink"]);
    let (id, path) = f.create();
    release(&f.0, &id).unwrap();
    let module = path.join("vendor/dep");
    fs::write(module.join("locally-created"), "valuable").unwrap();
    assert_eq!(f.cleanup(false)["eligible"], 0);
    assert_eq!(
        fs::read_to_string(module.join("locally-created")).unwrap(),
        "valuable"
    );
    fs::remove_file(module.join("locally-created")).unwrap();
    assert_eq!(f.cleanup(false)["removed"], 1);
    let (id, path) = f.create();
    release(&f.0, &id).unwrap();
    f.git(
        &path,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
        ],
    );
    assert_eq!(f.cleanup(false)["eligible"], 0);
    assert!(path.join("vendor/dep/.git").is_file());
}

#[test]
fn uncertain_release_replaced_tree_and_unowned_registration_are_preserved() {
    let f = Fixture::new();
    let (id, path) = f.create();
    let admin = f.0.join(".git/worktrees").join(&id);
    symlink(f.0.join("tracked"), admin.join(RELEASE)).unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    fs::remove_file(admin.join(RELEASE)).unwrap();
    release(&f.0, &id).unwrap();
    let original = path.with_file_name("original");
    fs::rename(&path, &original).unwrap();
    fs::create_dir(&path).unwrap();
    fs::copy(original.join(".git"), path.join(".git")).unwrap();
    fs::write(path.join("mine"), "data").unwrap();
    assert_eq!(f.cleanup(false)["removed"], 0);
    assert_eq!(fs::read(path.join("mine")).unwrap(), b"data");
    fs::remove_file(path.join(".git")).unwrap();
    assert!(
        registered_paths(&f.0, Instant::now() + CLEANUP_BUDGET)
            .unwrap()
            .contains(&path)
    );
    let unowned = path.with_file_name("0123456789abcdef0123456789abcdef");
    f.git(
        &f.0,
        &["worktree", "add", "--detach", unowned.to_str().unwrap()],
    );
    assert!(release(&f.0, unowned.file_name().unwrap().to_str().unwrap()).is_err());
    assert_eq!(f.cleanup(false)["removed"], 0);
    assert!(unowned.join("tracked").is_file());
}
