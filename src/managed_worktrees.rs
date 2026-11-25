//! Detached project worktrees. Only a private creation receipt and a separate
//! durable release allow cleanup; an old directory or a dead client cannot.
mod git;
mod mounts;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const AREA: [&str; 3] = [".ssmcp", "workspace", "worktrees"];
const RECEIPT: &str = "ssmcp-ownership-v1.json";
const RELEASE: &str = "ssmcp-released-v1";
const SCAN_LIMIT: usize = 256;
const CLEANUP_BUDGET: Duration = Duration::from_secs(5);
const HELP: &str = "Usage: ssmcp worktree create [REV]\n       ssmcp worktree release ID\n       ssmcp worktree cleanup [--dry-run]\nRun from the project root. Release ends the persistent lease. Cleanup follows the project cleanup rules and release age.";

fn ensure(ok: bool, message: &'static str) -> io::Result<()> {
    if ok {
        Ok(())
    } else {
        Err(io::Error::other(message))
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
struct Identity(u64, u64);
fn identity(m: &Metadata) -> Identity {
    Identity(m.dev(), m.ino())
}

fn dir_identity(dir: &Path) -> io::Result<Identity> {
    let m = fs::symlink_metadata(dir)?;
    ensure(
        m.is_dir() && m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o022 == 0,
        "directory ownership or permissions changed",
    )?;
    Ok(identity(&m))
}

fn secure_file(path: &Path, create: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(create)
        .create_new(create)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = file.metadata()?;
    ensure(
        m.is_file()
            && m.nlink() == 1
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "ownership record is not private",
    )?;
    Ok(file)
}

fn bounded(file: &File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure(bytes.len() as u64 <= limit, "inspection limit exceeded")?;
    Ok(bytes)
}

fn bounded_path(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    ensure(
        file.metadata()?.is_file(),
        "Git metadata is not a regular file",
    )?;
    bounded(&file, limit)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn oid(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

fn area(root: &Path, create: bool) -> io::Result<PathBuf> {
    let mut current = root.to_owned();
    for component in AREA {
        current.push(component);
        if create {
            match DirBuilder::new().mode(0o700).create(&current) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                result => result?,
            }
        }
        dir_identity(&current)?;
    }
    Ok(current)
}

struct Project {
    root: PathBuf,
    common: PathBuf,
    trees: PathBuf,
}

impl Project {
    fn open(root: &Path, create: bool, deadline: Instant) -> io::Result<Self> {
        let root = root.canonicalize()?;
        if !create {
            area(&root, false)?;
        }
        ensure(
            git::path(&root, "--show-toplevel", deadline)? == root,
            "run from the project Git root",
        )?;
        let common = git::path(&root, "--git-common-dir", deadline)?;
        dir_identity(&common)?;
        let trees = area(&root, create)?;
        Ok(Self {
            root,
            common,
            trees,
        })
    }

    fn admin(&self, id: &str) -> PathBuf {
        self.common.join("worktrees").join(id)
    }

    fn observe(&self, id: &str, deadline: Instant) -> io::Result<Objects> {
        let path = self.trees.join(id);
        let admin = self.admin(id);
        let base = area(&self.root, false)?;
        dir_identity(&self.common.join("worktrees"))?;
        let objects = Objects {
            project: identity(&fs::metadata(&self.root)?),
            parent: dir_identity(&base)?,
            tree: dir_identity(&path)?,
            admin: dir_identity(&admin)?,
            gitfile: identity(&fs::symlink_metadata(path.join(".git"))?),
        };
        ensure(
            fs::symlink_metadata(path.join(".git"))?.is_file()
                && git::path(&path, "--absolute-git-dir", deadline)? == admin
                && git::path(&path, "--show-toplevel", deadline)? == path
                && git::path(&path, "--git-common-dir", deadline)? == self.common,
            "worktree or repository registration changed",
        )?;
        // Git stores a backlink as a filesystem path, followed by one newline.
        let bytes = bounded_path(&admin.join("gitdir"), 16384)?;
        let backlink = bytes
            .strip_suffix(b"\n")
            .ok_or_else(|| io::Error::other("missing Git backlink"))?;
        use std::os::unix::ffi::OsStrExt;
        let backlink = Path::new(OsStr::from_bytes(backlink));
        let backlink = if backlink.is_absolute() {
            backlink.to_owned()
        } else {
            admin.join(backlink)
        };
        ensure(
            backlink.canonicalize()? == path.join(".git"),
            "Git backlink changed",
        )?;
        Ok(objects)
    }
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Objects {
    project: Identity,
    parent: Identity,
    tree: Identity,
    admin: Identity,
    gitfile: Identity,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    id: String,
    base: String,
    objects: Objects,
}

fn create(root: &Path, rev: &OsStr) -> io::Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let project = Project::open(root, true, deadline)?;
    let mut spec = rev.to_owned();
    spec.push("^{commit}");
    let base = git::run(
        &project.root,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new("--end-of-options"),
            &spec,
        ],
        deadline,
    )?;
    let base = std::str::from_utf8(&base)
        .map_err(io::Error::other)?
        .trim_end();
    ensure(oid(base), "invalid commit")?;
    let mut nonce = [0; 16];
    crate::platform::random_bytes(&mut nonce)?;
    let id = crate::hex::encode(nonce);
    let path = project.trees.join(&id);
    // Reserve a private empty directory. Do not import an existing directory.
    DirBuilder::new().mode(0o700).create(&path)?;
    git::run(
        &project.root,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            OsStr::new("--"),
            path.as_os_str(),
            OsStr::new(base),
        ],
        deadline,
    )?;
    ensure(
        registered_paths(&project.root, deadline)?.contains(&path),
        "Git did not register the created checkout",
    )?;
    let receipt = Receipt {
        id: id.clone(),
        base: base.to_owned(),
        objects: project.observe(&id, deadline)?,
    };
    let mut file = secure_file(&project.admin(&id).join(RECEIPT), true)?;
    file.write_all(&serde_json::to_vec(&receipt)?)?;
    file.sync_all()?;
    Ok(
        json!({"id":id,"path":format!(".ssmcp/workspace/worktrees/{id}"),"base":base,"lease":"active"}),
    )
}

fn read_owner(project: &Project, id: &str, deadline: Instant) -> io::Result<(File, Receipt)> {
    ensure(valid_id(id), "invalid worktree ID")?;
    let admin = project.admin(id);
    dir_identity(&project.common.join("worktrees"))?;
    dir_identity(&admin)?;
    let file = secure_file(&admin.join(RECEIPT), false)?;
    file.try_lock()
        .map_err(|_| io::Error::other("ownership record is busy"))?;
    let receipt: Receipt = serde_json::from_slice(&bounded(&file, 4096)?)?;
    ensure(
        receipt.id == id
            && oid(&receipt.base)
            && receipt.objects == project.observe(id, deadline)?,
        "worktree identity no longer matches its creation receipt",
    )?;
    Ok((file, receipt))
}

fn release(root: &Path, id: &str) -> io::Result<Value> {
    ensure(valid_id(id), "invalid worktree ID")?;
    let deadline = Instant::now() + CLEANUP_BUDGET;
    let project = Project::open(root, false, deadline)?;
    let (_lock, _) = read_owner(&project, id, deadline)?;
    let path = project.admin(id).join(RELEASE);
    match secure_file(&path, true) {
        Ok(file) => file.sync_all()?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            secure_file(&path, false)?;
        }
        Err(e) => return Err(e),
    }
    Ok(json!({"id":id,"lease":"released"}))
}

/// Return the paths recorded by Git, even when a checkout or its .git marker
/// has gone missing. The caller decides how to handle unavailable registration.
pub(crate) fn registered_paths(root: &Path, deadline: Instant) -> io::Result<Vec<PathBuf>> {
    use std::os::unix::ffi::OsStrExt;
    let data = git::run(root, &["worktree", "list", "--porcelain", "-z"], deadline)?;
    let mut paths = Vec::new();
    for field in data.split(|&b| b == 0) {
        if let Some(bytes) = field.strip_prefix(b"worktree ") {
            let path = Path::new(OsStr::from_bytes(bytes));
            ensure(path.is_absolute(), "invalid path in Git worktree registry")?;
            paths.push(path.to_path_buf());
        }
    }
    ensure(!paths.is_empty(), "Git worktree registry is empty")?;
    Ok(paths)
}

fn history_safe(project: &Project, id: &str, base: &str, deadline: Instant) -> io::Result<()> {
    // A detached commit followed by "reset --hard" is still recoverable through
    // this worktree's private HEAD reflog. Removing a worktree drops that reflog.
    let admin = project.admin(id);
    let history = bounded_path(&admin.join("logs/HEAD"), 512 * 1024)?;
    let mut commits = BTreeSet::new();
    let mut previous = None;
    for line in history.split(|&b| b == b'\n').filter(|s| !s.is_empty()) {
        let mut fields = line.splitn(3, |&b| b == b' ');
        let mut field = || {
            fields
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .filter(|s| oid(s))
                .ok_or_else(|| io::Error::other("unknown worktree reflog format"))
        };
        let old = field()?;
        let new = field()?;
        ensure(
            match previous {
                None => old.bytes().all(|b| b == b'0') && new == base,
                Some(last) => old == last,
            },
            "incomplete worktree reflog",
        )?;
        previous = Some(new);
        commits.insert(new);
        ensure(
            commits.len() <= 512,
            "worktree history exceeds inspection limit",
        )?;
    }
    let head = bounded_path(&admin.join("HEAD"), 256)?;
    let head = std::str::from_utf8(&head)
        .map_err(io::Error::other)?
        .trim_end();
    ensure(
        oid(head) && Some(head) == previous,
        "attached branch or unverified detached HEAD",
    )?;
    let mut args = vec!["rev-list", "--max-count=1"];
    args.extend(commits);
    args.extend(["--not", "HEAD", "--"]);
    ensure(
        git::run(&project.root, &args, deadline)?.is_empty(),
        "worktree contains commits outside project HEAD",
    )
}

struct GitLock {
    path: PathBuf,
    id: Identity,
}
impl GitLock {
    fn take(path: PathBuf) -> io::Result<Self> {
        let file = secure_file(&path, true)?;
        Ok(Self {
            path,
            id: identity(&file.metadata()?),
        })
    }
    fn matches(&self, path: &Path) -> bool {
        self.path == path && fs::symlink_metadata(path).is_ok_and(|m| identity(&m) == self.id)
    }
}
impl Drop for GitLock {
    fn drop(&mut self) {
        if self.matches(&self.path) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn clean(
    project: &Project,
    id: &str,
    base: &str,
    held: &[GitLock],
    deadline: Instant,
) -> io::Result<()> {
    let admin = project.admin(id);
    for (index, entry) in fs::read_dir(&admin)?.enumerate() {
        ensure(index < 128, "Git administrative entry limit exceeded")?;
        let entry = entry?;
        if held.iter().any(|guard| guard.matches(&entry.path())) {
            continue;
        }
        let name = entry.file_name();
        let name = name.as_encoded_bytes();
        ensure(
            !name.ends_with(b".lock")
                && !matches!(
                    name,
                    b"locked"
                        | b"MERGE_HEAD"
                        | b"CHERRY_PICK_HEAD"
                        | b"REVERT_HEAD"
                        | b"BISECT_START"
                        | b"rebase-apply"
                        | b"rebase-merge"
                        | b"sequencer"
                ),
            "worktree is locked or has a Git operation in progress",
        )?;
    }
    if admin.join("refs").try_exists()? {
        ensure(
            fs::read_dir(admin.join("refs"))?.next().is_none(),
            "worktree has private Git refs",
        )?;
    }
    let tree = project.trees.join(id);
    mounts::check(&tree, deadline)?;
    let index = git::run(&tree, &["ls-files", "-v", "--stage", "-z"], deadline)?;
    for entry in index.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        ensure(
            entry.starts_with(b"H "),
            "worktree has sparse, hidden, or conflicted entries",
        )?;
        if entry.starts_with(b"H 160000 ") {
            use std::os::unix::ffi::OsStrExt;
            let (_, name) = entry.split_at(
                entry
                    .iter()
                    .position(|&b| b == b'\t')
                    .ok_or_else(|| io::Error::other("invalid submodule index entry"))?
                    + 1,
            );
            let rel = Path::new(OsStr::from_bytes(name));
            ensure(
                !rel.as_os_str().is_empty()
                    && rel
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                "invalid submodule path",
            )?;
            // Git can ignore arbitrary files under an uninitialized gitlink.
            // Only a physically empty, uninitialized directory may be removed.
            let mut sub = tree.clone();
            for component in rel.components() {
                sub.push(component);
                ensure(
                    fs::symlink_metadata(&sub)?.is_dir(),
                    "submodule path changed",
                )?;
            }
            ensure(
                fs::read_dir(sub)?.next().is_none(),
                "initialized or nonempty submodule",
            )?;
        }
    }
    ensure(
        git::run(
            &tree,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=normal",
                "--ignored=matching",
                "--ignore-submodules=none",
            ],
            deadline,
        )?
        .is_empty(),
        "worktree contains local changes, untracked or ignored files",
    )?;
    history_safe(project, id, base, deadline)
}

fn inspect(
    project: &Project,
    id: &str,
    age: u64,
    dry_run: bool,
    deadline: Instant,
) -> io::Result<&'static str> {
    let admin = project.admin(id);
    dir_identity(&project.common.join("worktrees"))?;
    dir_identity(&admin)?;
    let released = match secure_file(&admin.join(RELEASE), false) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok("active_or_unmanaged"),
        result => result?,
    };
    ensure(released.metadata()?.len() == 0, "unknown release marker")?;
    let elapsed = SystemTime::now()
        .duration_since(released.metadata()?.modified()?)
        .map_err(|_| io::Error::other("release time lies in the future"))?;
    if elapsed.as_secs() < age {
        return Ok("retained");
    }
    let (lock, receipt) = read_owner(project, id, deadline)?;
    // Git's non-forced removal can ignore HEAD.lock and index.lock. Reserve both
    // across inspection/removal so a real commit or index update cannot race it.
    let mut held = Vec::new();
    if !dry_run {
        for name in ["HEAD.lock", "index.lock"] {
            held.push(GitLock::take(admin.join(name))?);
        }
    }
    clean(project, id, &receipt.base, &held, deadline)?;
    ensure(
        identity(&secure_file(&admin.join(RECEIPT), false)?.metadata()?)
            == identity(&lock.metadata()?)
            && identity(&secure_file(&admin.join(RELEASE), false)?.metadata()?)
                == identity(&released.metadata()?)
            && held.iter().all(|guard| guard.matches(&guard.path)),
        "release, ownership or lock changed",
    )?;
    ensure(
        receipt.objects == project.observe(id, deadline)?,
        "worktree changed during inspection",
    )?;
    if dry_run {
        return Ok("eligible");
    }
    let path = project.trees.join(id);
    // Git performs its own final registration, clean-worktree, and lock checks.
    // Never use --force, git clean, remove_dir_all, or git worktree prune here.
    git::run(
        &project.root,
        &[
            OsStr::new("worktree"),
            OsStr::new("remove"),
            OsStr::new("--"),
            path.as_os_str(),
        ],
        deadline,
    )?;
    Ok("removed")
}

/// Inspect only this project's managed worktree area. The default/disabled/no-area
/// path creates no files. Counts/reasons are bounded independently of directory size.
pub(crate) fn cleanup(
    root: &Path,
    dry_run: bool,
    config: &crate::managed_files::CleanupConfig,
) -> io::Result<Value> {
    let mut decisions = Vec::new();
    let mut removed = 0;
    let mut eligible = 0;
    let mut truncated = false;
    let deadline = Instant::now() + CLEANUP_BUDGET;
    if config.enabled {
        match area(root, false) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
            Ok(_) => {
                let project = Project::open(root, false, deadline)?;
                for (index, entry) in fs::read_dir(&project.trees)?.enumerate() {
                    if index == SCAN_LIMIT || Instant::now() >= deadline {
                        truncated = true;
                        break;
                    }
                    let name = entry?.file_name();
                    let Some(id) = name.to_str().filter(|id| valid_id(id)) else {
                        continue;
                    };
                    let path = format!(".ssmcp/workspace/worktrees/{id}");
                    let Some(age) =
                        config.age_for(&path, crate::managed_files::Category::Temporary)
                    else {
                        continue;
                    };
                    match inspect(&project, id, age, dry_run, deadline) {
                        Ok(reason) => {
                            if reason == "removed" {
                                removed += 1;
                            }
                            if matches!(reason, "eligible" | "removed") {
                                eligible += 1;
                            }
                            decisions.push(json!({"id":id,"result":reason}));
                        }
                        Err(error) => decisions
                            .push(json!({"id":id,"result":"kept","reason":error.to_string()})),
                    }
                }
            }
        }
    }
    Ok(
        json!({"enabled":config.enabled,"dry_run":dry_run,"eligible":eligible,"removed":removed,
        "scan_truncated":truncated,"decisions":decisions}),
    )
}

pub(crate) fn command(root: &Path, args: &[OsString]) -> io::Result<String> {
    let output = match args {
        [cmd] if cmd == "--help" || cmd == "-h" => return Ok(HELP.to_owned()),
        [cmd] if cmd == "create" => create(root, OsStr::new("HEAD"))?,
        [cmd, rev] if cmd == "create" => create(root, rev)?,
        [cmd, id] if cmd == "release" => release(root, id.to_str().unwrap_or(""))?,
        [cmd] if cmd == "cleanup" => cleanup(root, false, &crate::managed_files::config(root)?)?,
        [cmd, flag] if cmd == "cleanup" && flag == "--dry-run" => {
            cleanup(root, true, &crate::managed_files::config(root)?)?
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, HELP)),
    };
    Ok(serde_json::to_string_pretty(&output)?)
}
