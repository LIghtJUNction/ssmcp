//! Explicitly tracked workspace files. Unknown files are never adopted or removed.
//!
//! A creator registers only after its successful publication. Registration errors
//! are tracking warnings, not evidence that the preceding file write failed.
//! Active users retain a Registration or FileLease; the independently opened file
//! description is locked until that guard is dropped.
pub mod command;
mod config;
mod native;
mod storage;
#[cfg(test)]
mod tests;

pub use config::{CleanupConfig, CleanupRule};
pub use native::{perform, tools};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use storage::{Access, Directory};

pub const CONFIG_BYTES_LIMIT: usize = 64 * 1024;
const RECORD_BYTES_LIMIT: usize = 16 * 1024;
const MAX_RECORDS: usize = 16_384;
const REGISTRY: &str = "managed-files";
const CONFIG: &str = "cleanup.json";
const LOCK: &str = ".lock";
#[cfg(not(target_os = "linux"))]
const WRITES_LOCK: &str = ".writes.lock";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Temporary,
    PersistentInput,
    Configuration,
    History,
    Credential,
    PersistentArtifact,
    UserFile,
}

/// Stable filesystem object identity. Neither link count nor ctime belongs here:
/// publishing with rename/hardlink legitimately changes them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectIdentity {
    pub volume: u64,
    pub index: u64,
}

impl ObjectIdentity {
    fn from_file(file: &File) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            Ok(Self {
                volume: metadata.dev(),
                index: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: file owns a live handle and information is writable.
            if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                volume: u64::from(information.dwVolumeSerialNumber),
                index: (u64::from(information.nFileIndexHigh) << 32)
                    | u64::from(information.nFileIndexLow),
            })
        }
    }
}

/// A creation snapshot, compared again before cleanup to preserve edited files.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    pub object: ObjectIdentity,
    pub bytes: u64,
    pub modified_at_unix: u64,
    pub modified_subsec_nanos: u32,
}

impl FileIdentity {
    pub fn from_file(file: &File) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("created object must be a regular file"));
        }
        let modified = metadata
            .modified()?
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?;
        Ok(Self {
            object: ObjectIdentity::from_file(file)?,
            bytes: metadata.len(),
            modified_at_unix: modified.as_secs(),
            modified_subsec_nanos: modified.subsec_nanos(),
        })
    }

    pub fn same_file(&self, other: &Self) -> bool {
        self.object == other.object
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema_version: u32,
    pub id: String,
    pub relative_path: String,
    pub category: Category,
    /// Descriptive creation hook, never used as an authorization credential.
    pub source: String,
    /// Descriptive owner; current OS ownership is checked independently.
    pub owner: String,
    pub created_at_unix: u64,
    pub identity: FileIdentity,
    parent_ids: Vec<ObjectIdentity>,
    pub removed_at_unix: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct CleanupIssue {
    pub path: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CleanupDecision {
    pub path: String,
    pub action: String,
    pub reason: String,
    pub bytes: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct CleanupSummary {
    pub enabled: bool,
    pub dry_run: bool,
    pub trigger: String,
    pub registry_present: bool,
    pub busy: bool,
    pub scanned_records: u64,
    pub removed_files: u64,
    pub removed_bytes: u64,
    pub would_remove_files: u64,
    pub would_remove_bytes: u64,
    pub kept_active: u64,
    pub kept_protected: u64,
    pub kept_changed: u64,
    pub kept_by_rule: u64,
    pub missing_files: u64,
    /// Remaining verified, rule-selected temporary bytes, including held leases.
    pub retained_temporary_bytes: u64,
    pub scan_truncated: bool,
    pub errors: Vec<CleanupIssue>,
    pub decisions: Vec<CleanupDecision>,
}

impl CleanupSummary {
    fn new(dry_run: bool, trigger: &str, registry_present: bool) -> Self {
        Self {
            dry_run,
            trigger: trigger.to_owned(),
            registry_present,
            ..Self::default()
        }
    }

    fn issue(&mut self, path: &str, error: impl std::fmt::Display) {
        self.errors.push(CleanupIssue {
            path: path.to_owned(),
            error: error.to_string(),
        });
    }

    fn decision(&mut self, record: &Record, action: &str, reason: &str) {
        self.decisions.push(CleanupDecision {
            path: record.relative_path.clone(),
            action: action.to_owned(),
            reason: reason.to_owned(),
            bytes: record.identity.bytes,
        });
    }
}

#[derive(Debug)]
struct LockedFile(File);

impl LockedFile {
    fn acquire(file: File) -> io::Result<Self> {
        crate::platform::lock_file(&file, true)?;
        Ok(Self(file))
    }
}

impl Drop for LockedFile {
    fn drop(&mut self) {
        // This file is always a fresh open, not a clone of the creator's OFD.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            loop {
                if unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) } == 0
                    || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                {
                    break;
                }
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::{
                Storage::FileSystem::UnlockFileEx,
                System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0},
            };
            let mut overlap = OVERLAPPED {
                Anonymous: OVERLAPPED_0 {
                    Anonymous: OVERLAPPED_0_0 {
                        Offset: u32::MAX,
                        OffsetHigh: 0,
                    },
                },
                ..Default::default()
            };
            // Match exactly the byte range used by platform::lock_file.
            unsafe {
                UnlockFileEx(self.0.as_raw_handle(), 0, 1, 0, &mut overlap);
            }
        }
    }
}

#[derive(Debug)]
pub struct FileLease {
    _locked: LockedFile,
}

#[derive(Debug)]
pub struct Registration {
    record: Record,
    _lease: FileLease,
}

/// Cross-process publication exclusion for portable workspace writers.
/// External editors do not participate in this cooperative lock.
#[cfg(not(target_os = "linux"))]
pub struct WorkspaceWriteLease {
    store: ManagedFiles,
    locked: LockedFile,
}

#[cfg(not(target_os = "linux"))]
impl WorkspaceWriteLease {
    pub fn check_current(&self) -> io::Result<()> {
        self.store.check_named_lock(WRITES_LOCK, &self.locked)
    }
}

impl Registration {
    pub fn record(&self) -> &Record {
        &self.record
    }
}

pub struct ManagedFiles {
    workspace_path: PathBuf,
    workspace: Directory,
    state: Directory,
    registry: Directory,
}

impl ManagedFiles {
    pub fn open(workspace: &Path) -> io::Result<Self> {
        Self::open_impl(workspace, true)?.ok_or_else(|| invalid("missing managed registry"))
    }

    fn open_impl(workspace: &Path, create: bool) -> io::Result<Option<Self>> {
        let Some(store) = Self::open_directories(workspace, create)? else {
            return Ok(None);
        };
        if create {
            store.ensure_lock(LOCK)?;
            #[cfg(not(target_os = "linux"))]
            store.ensure_lock(WRITES_LOCK)?;
            // An existing configuration is never overwritten, including malformed JSON.
            match store.state.read(CONFIG, CONFIG_BYTES_LIMIT, false) {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let bytes = json_bytes(&CleanupConfig::default())?;
                    match store.state.write_new(CONFIG, &bytes) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            store.state.read(CONFIG, CONFIG_BYTES_LIMIT, false)?;
                        }
                        Err(error) => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Ok(Some(store))
    }

    fn open_directories(workspace: &Path, create: bool) -> io::Result<Option<Self>> {
        let workspace_path = std::path::absolute(workspace)?;
        let workspace = Directory::workspace(&workspace_path)?;
        let state = match workspace.child(".ssmcp", create, false) {
            Ok(directory) => directory,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let registry = match state.child(REGISTRY, create, true) {
            Ok(directory) => directory,
            Err(error) if !create && error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let store = Self {
            workspace_path,
            workspace,
            state,
            registry,
        };
        store.check_attached()?;
        Ok(Some(store))
    }

    fn ensure_lock(&self, name: &str) -> io::Result<()> {
        match self.registry.open(name, Access::Create, true) {
            Ok(file) => file.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self
                .registry
                .open(name, Access::ReadWrite, true)
                .map(|_| ()),
            Err(error) => Err(error),
        }
    }

    fn check_attached(&self) -> io::Result<()> {
        let root = Directory::workspace(&self.workspace_path)?;
        let state = root.child(".ssmcp", false, false)?;
        let registry = state.child(REGISTRY, false, true)?;
        if root.identity()? != self.workspace.identity()?
            || state.identity()? != self.state.identity()?
            || registry.identity()? != self.registry.identity()?
        {
            return Err(invalid("managed directory identity changed"));
        }
        Ok(())
    }

    fn lock(&self) -> io::Result<LockedFile> {
        self.check_attached()?;
        let lock = LockedFile::acquire(self.registry.open(LOCK, Access::ReadWrite, true)?)?;
        self.check_lock(&lock)?;
        Ok(lock)
    }

    fn check_lock(&self, lock: &LockedFile) -> io::Result<()> {
        self.check_named_lock(LOCK, lock)
    }

    fn check_named_lock(&self, name: &str, lock: &LockedFile) -> io::Result<()> {
        self.check_attached()?;
        let expected = FileIdentity::from_file(&lock.0)?;
        if !storage::same_at(&self.registry, name, &expected)? {
            return Err(invalid("managed registry lock identity changed"));
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn write_lock(&self) -> io::Result<LockedFile> {
        self.check_attached()?;
        let locked =
            LockedFile::acquire(self.registry.open(WRITES_LOCK, Access::ReadWrite, true)?)?;
        self.check_named_lock(WRITES_LOCK, &locked)?;
        Ok(locked)
    }

    pub fn config(&self) -> io::Result<CleanupConfig> {
        self.check_attached()?;
        match self.state.read(CONFIG, CONFIG_BYTES_LIMIT, false) {
            Ok(bytes) => parse_config(&bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(CleanupConfig::default()),
            Err(error) => Err(error),
        }
    }

    pub fn set_config(&self, config: &CleanupConfig) -> io::Result<()> {
        config.validate()?;
        let bytes = json_bytes(config)?;
        if bytes.len() > CONFIG_BYTES_LIMIT {
            return Err(invalid("cleanup configuration is too large"));
        }
        let lock = self.lock()?;
        // Re-check the existing file without requiring valid old JSON. This allows
        // an explicit --set-config to repair malformed, otherwise safe metadata.
        match self.state.open(CONFIG, Access::Read, false) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.check_lock(&lock)?;
                return self.state.write_new(CONFIG, &bytes);
            }
            Err(error) => return Err(error),
        }
        self.check_lock(&lock)?;
        self.state.write_atomic(CONFIG, &bytes)
    }

    pub fn list(&self) -> io::Result<Vec<Record>> {
        let _lock = self.lock()?;
        let (names, truncated) = self.registry.names(MAX_RECORDS + 1)?;
        if truncated {
            return Err(invalid("managed registry scan limit reached"));
        }
        let mut records = Vec::new();
        for name in names {
            let Some(name) = name.to_str().filter(|name| record_name(name)) else {
                continue;
            };
            records.push(self.read_record(name)?);
        }
        records.sort_by(|left, right| {
            left.relative_path
                .cmp(&right.relative_path)
                .then(left.id.cmp(&right.id))
        });
        Ok(records)
    }

    pub fn register_created(
        &self,
        relative: &Path,
        created: &File,
        category: Category,
        source: &str,
        owner: &str,
    ) -> io::Result<Registration> {
        self.register_expected(
            relative,
            &FileIdentity::from_file(created)?,
            category,
            source,
            owner,
        )
    }

    fn register_expected(
        &self,
        relative: &Path,
        expected: &FileIdentity,
        category: Category,
        source: &str,
        owner: &str,
    ) -> io::Result<Registration> {
        let relative_path = relative_path(relative)?;
        label(source)?;
        label(owner)?;
        if category == Category::Temporary
            && (!Path::new(&relative_path).starts_with(".ssmcp") || protected_path(&relative_path))
        {
            return Err(invalid(
                "temporary registration is restricted to unprotected .ssmcp files",
            ));
        }
        let lock = self.lock()?;
        let (parent, name, parent_ids) = self.workspace.parent_with_ids(&relative_path)?;
        let file = parent.open(&name, Access::Read, false)?;
        let identity = FileIdentity::from_file(&file)?;
        if !identity.same_file(expected) || identity.bytes != expected.bytes {
            return Err(invalid(
                "published file no longer matches its creation identity",
            ));
        }
        // Windows may finalize last-write time only after the publishing writer
        // closes. Keep that finalized snapshot for cleanup; do not confuse it
        // with a different file at the publication boundary.
        #[cfg(unix)]
        if identity != *expected {
            return Err(invalid("published file changed before registration"));
        }
        let lease = FileLease {
            _locked: LockedFile::acquire(file)?,
        };
        let (_, _, current_parent_ids) = self.workspace.parent_with_ids(&relative_path)?;
        if current_parent_ids != parent_ids || !storage::same_at(&parent, &name, &identity)? {
            return Err(invalid("published file changed before registration"));
        }
        let id = record_id(&relative_path, &identity)?;
        let filename = format!("{id}.json");
        let record = match self.registry.read(&filename, RECORD_BYTES_LIMIT, true) {
            Ok(bytes) => {
                let existing = parse_record(&filename, &bytes)?;
                if existing.category != category
                    || existing.removed_at_unix.is_some()
                    || existing.parent_ids != parent_ids
                {
                    return Err(invalid(
                        "existing registration cannot be reclassified or revived",
                    ));
                }
                existing
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let record = Record {
                    schema_version: 1,
                    id,
                    relative_path,
                    category,
                    source: source.to_owned(),
                    owner: owner.to_owned(),
                    created_at_unix: now()?,
                    identity,
                    parent_ids,
                    removed_at_unix: None,
                };
                let bytes = json_bytes(&record)?;
                if bytes.len() > RECORD_BYTES_LIMIT {
                    return Err(invalid("managed record is too large"));
                }
                self.check_lock(&lock)?;
                self.registry.write_new(&filename, &bytes)?;
                record
            }
            Err(error) => return Err(error),
        };
        Ok(Registration {
            record,
            _lease: lease,
        })
    }

    pub fn lease(&self, relative: &Path) -> io::Result<FileLease> {
        let relative = relative_path(relative)?;
        let _lock = self.lock()?;
        let (parent, name, parent_ids) = self.workspace.parent_with_ids(&relative)?;
        let file = parent.open(&name, Access::Read, false)?;
        let identity = FileIdentity::from_file(&file)?;
        let record = self.read_record(&format!("{}.json", record_id(&relative, &identity)?))?;
        if record.parent_ids != parent_ids || record.removed_at_unix.is_some() {
            return Err(invalid(
                "managed file's parent identity or lifecycle changed",
            ));
        }
        let lease = FileLease {
            _locked: LockedFile::acquire(file)?,
        };
        if !storage::same_at(&parent, &name, &identity)? {
            return Err(invalid("managed file changed while acquiring its lease"));
        }
        Ok(lease)
    }

    fn read_record(&self, name: &str) -> io::Result<Record> {
        parse_record(name, &self.registry.read(name, RECORD_BYTES_LIMIT, true)?)
    }

    pub fn cleanup(&self, dry_run: bool, trigger: &str) -> io::Result<CleanupSummary> {
        self.cleanup_at(dry_run, trigger, now()?)
    }

    fn cleanup_at(&self, dry_run: bool, trigger: &str, now: u64) -> io::Result<CleanupSummary> {
        label(trigger)?;
        let mut summary = CleanupSummary::new(dry_run, trigger, true);
        // Portable writers take this OS lock before the registry lock as well.
        // Keep it across the complete identity-check/unlink interval, not merely
        // around the initial scan. Contention is a skip, never an unbounded wait.
        #[cfg(not(target_os = "linux"))]
        let write_lock = match self.write_lock() {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                summary.busy = true;
                return Ok(summary);
            }
            Err(error) => return Err(error),
        };
        let lock = match self.lock() {
            Ok(lock) => lock,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                summary.busy = true;
                return Ok(summary);
            }
            Err(error) => return Err(error),
        };
        let config = self.config()?;
        summary.enabled = config.enabled;
        if !config.enabled {
            return Ok(summary);
        }
        let (names, truncated) = self.registry.names(MAX_RECORDS + 1)?;
        if truncated {
            summary.scan_truncated = true;
            summary.issue(REGISTRY, "registry scan limit reached; nothing removed");
            return Ok(summary);
        }
        let mut candidates = Vec::new();
        let mut total = 0u64;
        for filename in names {
            let Some(filename) = filename.to_str().filter(|name| record_name(name)) else {
                continue;
            };
            let record = match self.read_record(filename) {
                Ok(record) => record,
                Err(error) => {
                    summary.issue(filename, error);
                    summary.kept_changed += 1;
                    continue;
                }
            };
            summary.scanned_records += 1;
            if record.removed_at_unix.is_some() {
                continue;
            }
            if record.category != Category::Temporary
                || protected_path(&record.relative_path)
                || !Path::new(&record.relative_path).starts_with(".ssmcp")
            {
                summary.kept_protected += 1;
                summary.decision(&record, "kept", "protected");
                continue;
            }
            let Some(age) = config.age_for(&record.relative_path, record.category) else {
                summary.kept_by_rule += 1;
                summary.decision(&record, "kept", "not selected by rules");
                continue;
            };
            match self.open_record(&record) {
                Ok((_parent, _name, file)) => {
                    total = total.saturating_add(record.identity.bytes);
                    match LockedFile::acquire(file) {
                        Ok(_lease) => candidates.push((record, age)),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            summary.kept_active += 1;
                            summary.decision(&record, "kept", "active lease");
                        }
                        Err(error) => {
                            summary.issue(&record.relative_path, error);
                            summary.kept_changed += 1;
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    summary.missing_files += 1;
                    summary.decision(&record, "kept", "registered path is absent");
                }
                Err(error) => {
                    summary.kept_changed += 1;
                    summary.issue(&record.relative_path, error);
                    summary.decision(
                        &record,
                        "kept",
                        "identity, link, ownership or permissions changed",
                    );
                }
            }
        }
        candidates.sort_by(|(left, _), (right, _)| {
            left.created_at_unix
                .cmp(&right.created_at_unix)
                .then(left.id.cmp(&right.id))
        });
        for (mut record, max_age) in candidates {
            let last_written = record.created_at_unix.max(record.identity.modified_at_unix);
            let expired = now >= last_written && now - last_written >= max_age;
            if !expired && total <= config.max_total_bytes {
                summary.decision(&record, "kept", "within age and capacity limits");
                continue;
            }
            let reason = if expired {
                "maximum age"
            } else {
                "maximum total bytes"
            };
            #[cfg(not(target_os = "linux"))]
            self.check_named_lock(WRITES_LOCK, &write_lock)?;
            // Reopen and reacquire immediately before deletion. No large scan
            // retains thousands of data-file descriptors or bypasses a new lease.
            let result = self.remove_candidate(&record, &lock, dry_run);
            match result {
                Ok(()) => {
                    total = total.saturating_sub(record.identity.bytes);
                    if dry_run {
                        summary.would_remove_files += 1;
                        summary.would_remove_bytes = summary
                            .would_remove_bytes
                            .saturating_add(record.identity.bytes);
                        summary.decision(&record, "would_remove", reason);
                    } else {
                        summary.removed_files += 1;
                        summary.removed_bytes =
                            summary.removed_bytes.saturating_add(record.identity.bytes);
                        summary.decision(&record, "removed", reason);
                        record.removed_at_unix = Some(now);
                        let saved = json_bytes(&record).and_then(|bytes| {
                            self.registry
                                .write_atomic(&format!("{}.json", record.id), &bytes)
                        });
                        if let Err(error) = saved {
                            summary.issue(&record.relative_path, error);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    summary.kept_active += 1;
                    summary.decision(&record, "kept", "active lease");
                }
                Err(error) => {
                    summary.kept_changed += 1;
                    summary.issue(&record.relative_path, error);
                    summary.decision(&record, "kept", "changed before removal");
                }
            }
        }
        summary.retained_temporary_bytes = total;
        Ok(summary)
    }

    fn open_record(&self, record: &Record) -> io::Result<(Directory, String, File)> {
        let (parent, name, identities) = self.workspace.parent_with_ids(&record.relative_path)?;
        if identities != record.parent_ids {
            return Err(invalid("registered parent directory was replaced"));
        }
        let file = parent.open(&name, Access::Read, false)?;
        if FileIdentity::from_file(&file)? != record.identity {
            return Err(invalid("registered file was replaced or modified"));
        }
        Ok((parent, name, file))
    }

    fn remove_candidate(
        &self,
        record: &Record,
        lock: &LockedFile,
        dry_run: bool,
    ) -> io::Result<()> {
        let (parent, name, file) = self.open_record(record)?;
        // This is the same parent-directory flock used by workspace writers.
        // Acquiring it nonblockingly avoids reversing a blocking lock order.
        #[cfg(target_os = "linux")]
        let _publication_lock = parent.publication_lock()?;
        let _lease = LockedFile::acquire(file)?;
        self.check_lock(lock)?;
        let (current_parent, current_name, identities) =
            self.workspace.parent_with_ids(&record.relative_path)?;
        if identities != record.parent_ids
            || current_parent.identity()? != parent.identity()?
            || current_name != name
            || !storage::same_at(&parent, &name, &record.identity)?
        {
            return Err(invalid("registered path changed before cleanup"));
        }
        if !dry_run {
            parent.remove(&name)?;
        }
        Ok(())
    }
}

/// Internal registry and lock files are not ordinary workspace write targets.
/// cleanup.json deliberately remains editable through the normal file tool.
pub fn validate_user_write(relative: &Path) -> io::Result<()> {
    // Leave ordinary filename validation to the existing workspace API. The
    // narrower portable registry syntax must not reject a legal Unix filename.
    let mut components = relative
        .components()
        .filter(|part| !matches!(part, Component::CurDir));
    if matches!(components.next(), Some(Component::Normal(part))
        if part.to_str().is_some_and(|part| component_eq(part, ".ssmcp")))
        && matches!(components.next(), Some(Component::Normal(part))
            if part.to_str().is_some_and(|part| component_eq(part, REGISTRY)))
    {
        return Err(invalid(
            "managed-files metadata must be changed through its configuration API",
        ));
    }
    Ok(())
}

/// Caller resolves the destination parent first, then retries WouldBlock using
/// its existing bounded write budget. This prepares no default configuration.
#[cfg(not(target_os = "linux"))]
pub fn try_workspace_write_lock(
    root: &Path,
    relative: &Path,
) -> io::Result<Option<WorkspaceWriteLease>> {
    validate_user_write(relative)?;
    let first = relative
        .components()
        .find(|part| !matches!(part, Component::CurDir));
    if !matches!(first, Some(Component::Normal(part))
        if part.to_str().is_some_and(|part| component_eq(part, ".ssmcp")))
    {
        return Ok(None);
    }
    let store = ManagedFiles::open_directories(root, true)?
        .ok_or_else(|| invalid("missing managed workspace"))?;
    store.ensure_lock(LOCK)?;
    store.ensure_lock(WRITES_LOCK)?;
    let locked = store.write_lock()?;
    Ok(Some(WorkspaceWriteLease { store, locked }))
}

/// For successful creation hooks which have already captured the creator's FD.
/// This does not grant a lasting active lease; retain Registration when needed.
pub fn record_created(
    root: &Path,
    relative: &Path,
    kind: Category,
    source: &str,
    expected: &FileIdentity,
) -> io::Result<Record> {
    let store = ManagedFiles::open(root)?;
    let owner = current_owner();
    let registration = store.register_expected(relative, expected, kind, source, &owner)?;
    Ok(registration.record.clone())
}

pub fn cleanup(root: &Path, dry_run: bool, trigger: &str) -> io::Result<CleanupSummary> {
    label(trigger)?;
    match ManagedFiles::open_impl(root, false)? {
        Some(store) => store.cleanup(dry_run, trigger),
        None => Ok(CleanupSummary::new(dry_run, trigger, false)),
    }
}

pub fn list(root: &Path) -> io::Result<Vec<Record>> {
    match ManagedFiles::open_impl(root, false)? {
        Some(store) => store.list(),
        None => Ok(Vec::new()),
    }
}

pub fn config(root: &Path) -> io::Result<CleanupConfig> {
    match ManagedFiles::open_impl(root, false)? {
        Some(store) => store.config(),
        None => {
            let root = Directory::workspace(root)?;
            let state = match root.child(".ssmcp", false, false) {
                Ok(state) => state,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Ok(CleanupConfig::default());
                }
                Err(error) => return Err(error),
            };
            match state.read(CONFIG, CONFIG_BYTES_LIMIT, false) {
                Ok(bytes) => parse_config(&bytes),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Ok(CleanupConfig::default())
                }
                Err(error) => Err(error),
            }
        }
    }
}

pub fn set_config(root: &Path, value: &CleanupConfig) -> io::Result<()> {
    value.validate()?;
    // Repair malformed JSON only through an explicit, already validated update.
    match ManagedFiles::open_impl(root, false)? {
        Some(store) => store.set_config(value),
        None => ManagedFiles::open(root)?.set_config(value),
    }
}

fn current_owner() -> String {
    #[cfg(unix)]
    {
        format!("uid:{}", unsafe { libc::geteuid() })
    }
    #[cfg(windows)]
    {
        "current_windows_user".to_owned()
    }
}

fn parse_config(bytes: &[u8]) -> io::Result<CleanupConfig> {
    let value: CleanupConfig = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    value.validate()?;
    Ok(value)
}

fn parse_record(filename: &str, bytes: &[u8]) -> io::Result<Record> {
    let record: Record = serde_json::from_slice(bytes).map_err(io::Error::other)?;
    if record.schema_version != 1
        || relative_path(Path::new(&record.relative_path))? != record.relative_path
        || record.id != record_id(&record.relative_path, &record.identity)?
        || filename != format!("{}.json", record.id)
        || record.parent_ids.is_empty()
    {
        return Err(invalid("invalid managed file record"));
    }
    label(&record.source)?;
    label(&record.owner)?;
    Ok(record)
}

fn record_id(path: &str, identity: &FileIdentity) -> io::Result<String> {
    let bytes = serde_json::to_vec(&(path, identity)).map_err(io::Error::other)?;
    Ok(crate::hex::encode(Sha256::digest(bytes)))
}

fn record_name(name: &str) -> bool {
    name.len() == 69
        && name.ends_with(".json")
        && name.as_bytes()[..64]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn json_bytes(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn now() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

fn relative_path(path: &Path) -> io::Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        if matches!(component, Component::CurDir) {
            continue;
        }
        let Component::Normal(part) = component else {
            return Err(invalid(
                "managed path must be relative without parent or prefix components",
            ));
        };
        let part = part
            .to_str()
            .ok_or_else(|| invalid("managed path must be UTF-8"))?;
        if part.is_empty()
            || part.contains(['/', '\\', '\0', ':'])
            || part.chars().any(char::is_control)
        {
            return Err(invalid("invalid managed path component"));
        }
        #[cfg(windows)]
        if part.ends_with(['.', ' ']) {
            return Err(invalid(
                "Windows managed paths cannot use trailing-dot or trailing-space aliases",
            ));
        }
        parts.push(part);
    }
    let path = parts.join("/");
    if path.is_empty() || path.len() > 4096 {
        return Err(invalid("invalid managed path length"));
    }
    Ok(path)
}

fn component_eq(left: &str, right: &str) -> bool {
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn protected_path(path: &str) -> bool {
    let path = Path::new(path);
    if path == Path::new(".ssmcp") {
        return true;
    }
    let Some(name) = path
        .strip_prefix(".ssmcp")
        .ok()
        .and_then(|path| path.components().next())
    else {
        return false;
    };
    let name = name.as_os_str().to_string_lossy().into_owned();
    #[cfg(windows)]
    let name = name.to_lowercase();
    if [
        REGISTRY,
        CONFIG,
        "inputs",
        "webhooks.json",
        "skills",
        "config",
        "configs",
        "history",
        "histories",
        "sessions",
        "conversations",
        "credentials",
        "secrets",
        "auth",
        "auth.json",
        "tokens",
        "token.json",
    ]
    .contains(&name.as_str())
    {
        return true;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name == "lock" || name == ".lock" || name.ends_with(".lock") || name.ends_with(".pid")
        })
}

fn label(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(invalid(
            "managed file labels must be nonempty, bounded text",
        ))
    } else {
        Ok(())
    }
}

fn random_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes)?;
    Ok(crate::hex::encode(bytes))
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
