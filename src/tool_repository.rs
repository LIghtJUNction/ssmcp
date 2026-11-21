//! Editable code-mode helpers selected on the authorized client, not the relay.
mod git;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub(crate) const URI: &str = "ssmcp-client://tool-sources";
pub(crate) const ORIGIN: &str = "https://github.com/LIghtJUNction/ssmcp-tools.git";
const SCHEMA: &str = "ssmcp-tool-sources-v1";
const SEED_COMMIT: &str = "d80054cf16f38b93a94fb2867fbfbfde78832066";
#[cfg(any(target_os = "linux", test))]
const SEED_SOURCE_SHA256: &str = "885e958633713b500983eac37848487ed34d079dad8ae7db4bfa1262688ca7b8";
const SEED: &[u8] = include_bytes!("tool_repository/official.bundle");
const MAX_RC: usize = 64 * 1024;
const MAX_SOURCE: usize = 128 * 1024;
const MAX_TOTAL: usize = 192 * 1024;
const MAX_BUNDLE: usize = 512 * 1024;
const MAX_OVERRIDES: usize = 16;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Repository {
    pub root: String,
    pub head: Option<String>,
    pub dirty: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Source {
    pub name: String,
    pub path: String,
    pub sha256: String,
    pub code: String,
    pub repository: Option<Repository>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RcIdentity {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bundle {
    pub schema: String,
    pub rc: Option<RcIdentity>,
    pub sources: Vec<Source>,
}

impl Bundle {
    pub(crate) fn validate(self) -> Result<Self, String> {
        if self.schema != SCHEMA
            || self.sources.is_empty()
            || self.sources.len() > MAX_OVERRIDES + 1
        {
            return Err("invalid tool-source bundle schema or source count".into());
        }
        if self
            .rc
            .as_ref()
            .is_some_and(|rc| !path_text(&rc.path) || !hex(&rc.sha256, 64))
        {
            return Err("invalid tool-source RC identity".into());
        }
        let mut names = BTreeSet::new();
        let mut total = 0usize;
        for source in &self.sources {
            total = total.saturating_add(source.code.len());
            if source.name.is_empty()
                || source.name.len() > 64
                || !source
                    .name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._:-".contains(&c))
                || !names.insert(&source.name)
                || !path_text(&source.path)
                || source.code.trim().is_empty()
                || source.code.contains('\0')
                || source.code.len() > MAX_SOURCE
                || total > MAX_TOTAL
                || source.sha256 != digest(source.code.as_bytes())
            {
                return Err("invalid, duplicate, oversized, or changed tool source".into());
            }
            if source.repository.as_ref().is_some_and(|repo| {
                !path_text(&repo.root)
                    || repo
                        .head
                        .as_ref()
                        .is_some_and(|head| !(hex(head, 40) || hex(head, 64)))
            }) {
                return Err("invalid tool-source Git identity".into());
            }
        }
        if self.provenance().to_string().len() > 16 * 1024 {
            return Err("tool-source identity metadata exceeds 16 KiB".into());
        }
        if serde_json::to_vec(&self).map_err(|e| e.to_string())?.len() > MAX_BUNDLE {
            return Err("encoded tool-source bundle exceeds 512 KiB".into());
        }
        Ok(self)
    }

    /// Host-produced provenance is kept outside the editable program's result.
    pub(crate) fn provenance(&self) -> Value {
        json!({
            "schema":self.schema, "rc":self.rc,
            "sources":self.sources.iter().map(|s| json!({
                "name":s.name, "path":s.path, "sha256":s.sha256,
                "bytes":s.code.len(), "repository":s.repository
            })).collect::<Vec<_>>()
        })
    }
}

fn path_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn digest(bytes: &[u8]) -> String {
    crate::hex::encode(Sha256::digest(bytes))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Tools {
    #[serde(default)]
    overrides: Vec<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rc {
    version: u32,
    #[serde(default)]
    tools: Tools,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    entry: PathBuf,
}

struct Paths {
    workspace: PathBuf,
    rc: PathBuf,
    directory: PathBuf,
}

impl Paths {
    fn current(workspace: &Path) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .ok_or("tool repositories require an absolute HOME or USERPROFILE")?;
        let base = |name: &str, fallback: PathBuf| -> Result<PathBuf, String> {
            match std::env::var_os(name) {
                Some(value) if !value.is_empty() => {
                    let path = PathBuf::from(value);
                    if !path.is_absolute() {
                        return Err(format!("{name} must be absolute"));
                    }
                    Ok(path)
                }
                _ => Ok(fallback),
            }
        };
        Ok(Self {
            workspace: workspace.canonicalize().map_err(|e| e.to_string())?,
            rc: base("XDG_CONFIG_HOME", home.join(".config"))?.join("ssmcp/ssmcprc.toml"),
            directory: base("XDG_DATA_HOME", home.join(".local/share"))?.join("ssmcp/tools"),
        })
    }

    fn config(&self) -> Result<(Option<RcIdentity>, Vec<PathBuf>), String> {
        match std::fs::symlink_metadata(&self.rc) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((None, vec![]));
            }
            Err(error) => return Err(error.to_string()),
            Ok(_) => {}
        }
        let bytes = read_file(&self.rc, MAX_RC)
            .map_err(|error| format!("{}: {error}", self.rc.display()))?;
        let text = std::str::from_utf8(&bytes).map_err(|_| "ssmcprc.toml must be UTF-8")?;
        let rc: Rc = toml::from_str(text).map_err(|e| format!("invalid ssmcprc.toml: {e}"))?;
        if rc.version != 1 || rc.tools.overrides.len() > MAX_OVERRIDES {
            return Err("ssmcprc.toml requires version=1 and at most 16 tool overrides".into());
        }
        let mut paths = Vec::new();
        for path in rc.tools.overrides {
            if path.as_os_str().is_empty() {
                return Err("tool override paths cannot be empty".into());
            }
            let path = if path.is_absolute() {
                path
            } else {
                self.workspace.join(path)
            };
            let path = path
                .canonicalize()
                .map_err(|e| format!("tool override {}: {e}", path.display()))?;
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Ok((
            Some(RcIdentity {
                path: self.rc.display().to_string(),
                sha256: digest(&bytes),
            }),
            paths,
        ))
    }
}

fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err("tool inputs must not be reparse points".into());
        }
    }
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("tool inputs must be regular files, not symlinks".into());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|e| e.to_string())?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if opened.file_attributes() & 0x400 != 0 {
            return Err("tool inputs must not be reparse points".into());
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.dev() != opened.dev() || metadata.ino() != opened.ino() {
            return Err("tool input changed while opening it".into());
        }
    }
    if !opened.is_file() {
        return Err("tool input changed while opening it".into());
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > limit {
        return Err(format!("tool input exceeds {limit} bytes"));
    }
    Ok(bytes)
}

fn entry(path: &Path) -> Result<(PathBuf, Option<(PathBuf, Vec<u8>)>), String> {
    if path.is_file() {
        return Ok((path.to_owned(), None));
    }
    let manifest_path = path.join("manifest.json");
    let bytes = read_file(&manifest_path, MAX_RC)?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| format!("invalid tool manifest: {e}"))?;
    if manifest.schema != "ssmcp-tools-v1"
        || manifest.entry.as_os_str().is_empty()
        || manifest
            .entry
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(
            "tool manifest requires schema ssmcp-tools-v1 and a relative entry without traversal"
                .into(),
        );
    }
    let entry = path.join(&manifest.entry);
    let canonical = entry.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(path) {
        return Err("tool entry escapes its registered repository".into());
    }
    // Reading through a symlink is unnecessary: registered files have an exact identity.
    if canonical != entry {
        return Err("tool manifest entry must not contain symlinks".into());
    }
    Ok((entry, Some((manifest_path, bytes))))
}

async fn source(name: String, path: &Path) -> Result<Source, String> {
    let (entry, manifest) = entry(path)?;
    let parent = entry.parent().ok_or("tool source has no parent")?;
    let before = git::identity(parent).await?;
    let bytes = read_file(&entry, MAX_SOURCE)?;
    let code = String::from_utf8(bytes.clone()).map_err(|_| "tool source must be UTF-8")?;
    let after = git::identity(parent).await?;
    if serde_json::to_vec(&before).map_err(|e| e.to_string())?
        != serde_json::to_vec(&after).map_err(|e| e.to_string())?
        || bytes != read_file(&entry, MAX_SOURCE)?
        || manifest
            .as_ref()
            .is_some_and(|(path, bytes)| read_file(path, MAX_RC).as_ref() != Ok(bytes))
    {
        return Err("tool sources changed during the read; start a new code-mode call".into());
    }
    Ok(Source {
        name,
        path: entry.display().to_string(),
        sha256: digest(&bytes),
        code,
        repository: after,
    })
}

/// This fixed resource loads only RC-registered inputs. It accepts no caller path.
pub(crate) async fn bundle(workspace: &Path) -> Result<Value, String> {
    let workspace = workspace.to_owned();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        git::owned(async move { bundle_owned(&workspace).await }),
    )
    .await
    .map_err(|_| {
        "tool-source read timed out; owned Git cleanup continues before its lease is released"
            .to_owned()
    })?
}

async fn bundle_owned(workspace: &Path) -> Result<Value, String> {
    let paths = Paths::current(workspace)?;
    let (rc, overrides) = paths.config()?;
    let official = ensure_official(&paths.directory).await?;
    let mut sources = vec![source("official".into(), &official).await?];
    for (index, path) in overrides.iter().enumerate() {
        sources.push(source(format!("override:{index}"), path).await?);
    }
    let (after, _) = paths.config()?;
    if serde_json::to_vec(&rc).map_err(|e| e.to_string())?
        != serde_json::to_vec(&after).map_err(|e| e.to_string())?
    {
        return Err("tool RC changed during the read; start a new code-mode call".into());
    }
    serde_json::to_value(
        Bundle {
            schema: SCHEMA.into(),
            rc,
            sources,
        }
        .validate()?,
    )
    .map_err(|e| e.to_string())
}

/// Explicit read-only official default: fixed Git objects, never the relay's RC.
#[cfg(any(target_os = "linux", test))]
pub(crate) async fn official_seed() -> Result<Bundle, String> {
    static SOURCE: tokio::sync::OnceCell<Source> = tokio::sync::OnceCell::const_new();
    let source = SOURCE
        .get_or_try_init(|| {
            git::owned(async {
                let temporary = Temporary::new(&std::env::temp_dir())?;
                let bundle = temporary.0.join("official.bundle");
                write_new(&bundle, SEED)?;
                let repo = temporary.0.join("objects");
                git::run(
                    &temporary.0,
                    &[
                        "init".into(),
                        "--bare".into(),
                        repo.clone().into_os_string(),
                    ],
                    false,
                )
                .await?;
                git::run(
                    &repo,
                    &[
                        "fetch".into(),
                        "--no-tags".into(),
                        "--no-recurse-submodules".into(),
                        "--no-write-fetch-head".into(),
                        bundle.into_os_string(),
                        "refs/heads/main".into(),
                    ],
                    false,
                )
                .await?;
                let code = git::run(
                    &repo,
                    &["show".into(), format!("{SEED_COMMIT}:tools.js").into()],
                    false,
                )
                .await?;
                if digest(&code) != SEED_SOURCE_SHA256 {
                    return Err("official tool seed does not match its pinned source hash".into());
                }
                Ok::<Source, String>(Source {
                    name: "official_seed".into(),
                    path: format!("git:{SEED_COMMIT}:tools.js"),
                    sha256: digest(&code),
                    code: String::from_utf8(code).map_err(|e| e.to_string())?,
                    repository: Some(Repository {
                        root: ORIGIN.into(),
                        head: Some(SEED_COMMIT.into()),
                        dirty: false,
                    }),
                })
            })
        })
        .await?;
    Bundle {
        schema: SCHEMA.into(),
        rc: None,
        sources: vec![source.clone()],
    }
    .validate()
}

struct Temporary(PathBuf);
impl Temporary {
    fn new(parent: &Path) -> Result<Self, String> {
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random).map_err(|e| e.to_string())?;
        let path = parent.join(format!(".ssmcp-tools-{}", crate::hex::encode(random)));
        crate::platform::private_directory(&path).map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}

fn private_directory(path: &Path) -> Result<(), String> {
    if !path.try_exists().map_err(|e| e.to_string())? {
        let parent = path.parent().ok_or("tool directory has no parent")?;
        if !parent.exists() {
            private_directory(parent)?;
        }
        match crate::platform::private_directory(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("tool storage must be a real private directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "tool directory {} must be owned by this user and mode 0700",
                path.display()
            ));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        };
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|e| e.to_string())?;
        crate::platform::windows::verify_directory(&file).map_err(|e| e.to_string())?;
    }
    Ok(())
}

struct Lock {
    _file: File,
}
impl Lock {
    fn take(directory: &Path) -> Result<Self, String> {
        private_directory(directory)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let file = options
            .open(directory.join(".repository.lock"))
            .map_err(|e| e.to_string())?;
        let metadata = file.metadata().map_err(|e| e.to_string())?;
        if !metadata.is_file() {
            return Err("tool repository lock must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
                || metadata.mode() & 0o077 != 0
            {
                return Err("tool repository lock must be private and singly linked".into());
            }
        }
        #[cfg(windows)]
        crate::platform::windows::verify_file(&file).map_err(|e| e.to_string())?;
        crate::platform::lock_file(&file, true).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                "tool repository is busy; retry later".into()
            } else {
                e.to_string()
            }
        })?;
        Ok(Self { _file: file })
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

async fn ensure_official(directory: &Path) -> Result<PathBuf, String> {
    let _lock = Lock::take(directory)?;
    let official = directory.join("official");
    if official.try_exists().map_err(|e| e.to_string())? {
        for path in [&official, &official.join(".git")] {
            let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("official tools require a real working directory and an independent .git directory".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("official tool directories must not be reparse points".into());
                }
            }
        }
        let identity = git::identity(&official)
            .await?
            .ok_or("official tools are not a Git working tree")?;
        if Path::new(&identity.root) != official.canonicalize().map_err(|e| e.to_string())? {
            return Err("official tool repository is nested in a different Git root".into());
        }
        return official.canonicalize().map_err(|e| e.to_string());
    }
    let stage = Temporary::new(directory)?;
    let bundle = stage.0.join("official.bundle");
    write_new(&bundle, SEED)?;
    let clone = stage.0.join("checkout");
    git::run(
        &stage.0,
        &[
            "clone".into(),
            "--no-hardlinks".into(),
            "--branch".into(),
            "main".into(),
            "--".into(),
            bundle.into_os_string(),
            clone.clone().into_os_string(),
        ],
        false,
    )
    .await?;
    let head = git::text(&clone, &["rev-parse", "HEAD"]).await?;
    if head != SEED_COMMIT {
        return Err("official tool bootstrap commit did not match its pin".into());
    }
    git::run(
        &clone,
        &[
            "remote".into(),
            "set-url".into(),
            "origin".into(),
            ORIGIN.into(),
        ],
        false,
    )
    .await?;
    git::active()?;
    std::fs::rename(&clone, &official).map_err(|e| e.to_string())?;
    official.canonicalize().map_err(|e| e.to_string())
}

/// Check touches only Git objects/refs. Updating never resets local edits.
pub(crate) async fn update(workspace: &Path, apply: bool) -> Result<Value, String> {
    let workspace = workspace.to_owned();
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        git::owned(async move { update_owned(&workspace, apply).await }),
    )
    .await
    .map_err(|_| {
        "tool repository update timed out; owned Git cleanup continues before its lease is released"
            .to_owned()
    })?
}

async fn update_owned(workspace: &Path, apply: bool) -> Result<Value, String> {
    let paths = Paths::current(workspace)?;
    let official = ensure_official(&paths.directory).await?;
    let _lock = Lock::take(&paths.directory)?;
    let before = git::identity(&official)
        .await?
        .ok_or("official tool repository is unavailable")?;
    let before_head = before
        .head
        .as_deref()
        .ok_or("official tool repository has no HEAD")?;
    let branch =
        git::optional_text(&official, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let remote = git::remote_head(&official).await?;
    git::run(
        &official,
        &[
            "fetch".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--no-write-fetch-head".into(),
            ORIGIN.into(),
            format!("+{}:refs/remotes/ssmcp-tools/default", remote.0).into(),
        ],
        true,
    )
    .await?;
    let fetched = git::text(
        &official,
        &["rev-parse", "refs/remotes/ssmcp-tools/default"],
    )
    .await?;
    if fetched != remote.1 {
        return Err(
            "official tools moved during the check; no working files changed, check again".into(),
        );
    }
    let ancestor = git::ancestor(&official, before_head, &fetched).await?;
    let mut status = if before.dirty {
        "dirty"
    } else if branch.is_none() {
        "detached"
    } else if before_head == fetched {
        "current"
    } else if !ancestor {
        "diverged"
    } else {
        "available"
    };
    if apply && status == "available" {
        let now = git::identity(&official)
            .await?
            .ok_or("official tool repository disappeared")?;
        if now.head != before.head
            || now.dirty
            || git::optional_text(&official, &["symbolic-ref", "--quiet", "--short", "HEAD"])
                .await?
                != branch
        {
            return Err(
                "official tool repository changed during update; local work was preserved".into(),
            );
        }
        git::reject_filters(&official).await?;
        git::run(
            &official,
            &[
                "merge".into(),
                "--ff-only".into(),
                "--no-edit".into(),
                "--no-stat".into(),
                fetched.clone().into(),
            ],
            false,
        )
        .await?;
        status = "updated";
    }
    let after = git::identity(&official)
        .await?
        .ok_or("official tool repository disappeared")?;
    if status == "updated" && (after.head.as_deref() != Some(fetched.as_str()) || after.dirty) {
        return Err(
            "tool update finished with unexpected local changes; inspect the repository".into(),
        );
    }
    Ok(json!({
        "schema":"ssmcp-tools-update-v1", "status":status, "applied":status=="updated",
        "origin":ORIGIN, "default_ref":remote.0, "remote_head":fetched,
        "before":before, "after":after, "overrides_changed":false,
        "message":if matches!(status,"dirty"|"detached"|"diverged") {
            "Local work was preserved. Inspect and reconcile this repository yourself; no reset was attempted."
        } else { "Tool sources take effect on a new code-mode call." }
    }))
}

const HELP: &str = "Usage: ssmcp tools [status|check|update]\n\n\
status  Show this client's RC, source hashes and Git state (default)\n\
check   Fetch and inspect the official default branch; preserve working files\n\
update  Fast-forward a clean official checkout; preserve dirty/diverged work\n\n\
RC: $XDG_CONFIG_HOME/ssmcp/ssmcprc.toml\n\
Official: $XDG_DATA_HOME/ssmcp/tools/official\n\
Only RC-registered overrides are loaded. Git is required, including offline bootstrap.\n";

pub(crate) async fn run(args: Vec<OsString>) -> crate::Result<()> {
    let command = match args.as_slice() {
        [] => "status",
        [arg] if arg == "-h" || arg == "--help" => {
            print!("{HELP}");
            return Ok(());
        }
        [arg] if arg == "status" => "status",
        [arg] if arg == "check" => "check",
        [arg] if arg == "update" => "update",
        _ => return Err(HELP.into()),
    };
    let workspace = std::env::current_dir()?;
    let result = if command == "status" {
        let bundle: Bundle = serde_json::from_value(bundle(&workspace).await?)?;
        bundle.provenance()
    } else {
        update(&workspace, command == "update").await?
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

#[cfg(test)]
mod tests;
