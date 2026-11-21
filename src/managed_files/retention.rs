//! Bounded maintenance of completed registry metadata, never workspace data.
use super::{
    CleanupIssue, CleanupRecovery, FileIdentity, LockedFile, ManagedFiles, Record, RemovalOutcome,
    record_name,
};
use serde::Serialize;
use std::io;

const RETAIN_COMPLETED: usize = 1024;
const RAW_SCAN_LIMIT: usize = 2 * super::MAX_RECORDS;
const PRUNE_LIMIT: usize = 1024;

#[derive(Clone, Copy)]
struct Limits {
    retain_completed: usize,
    raw_entries: usize,
    active_records: usize,
    prune_records: usize,
}

impl Limits {
    const DEFAULT: Self = Self {
        retain_completed: RETAIN_COMPLETED,
        raw_entries: RAW_SCAN_LIMIT,
        active_records: super::MAX_RECORDS,
        prune_records: PRUNE_LIMIT,
    };
}

#[derive(Debug, Serialize)]
pub struct RetentionSummary {
    pub retain_completed: usize,
    pub raw_scan_limit: usize,
    pub active_record_limit: usize,
    pub prune_limit: usize,
    pub pruning_requested: bool,
    pub scanned_entries: usize,
    pub active_records: usize,
    pub invalid_records: usize,
    pub completed_records: usize,
    pub pruned_records: usize,
    /// Eligible completed records not confirmed removed from this sample.
    /// Recoveries locate isolated records; a truncated scan may hide more work.
    pub pending_completed_records: usize,
    pub raw_scan_truncated: bool,
    pub active_limit_exceeded: bool,
    pub state_changed: bool,
    pub errors: Vec<CleanupIssue>,
    pub recoveries: Vec<CleanupRecovery>,
}

impl Default for RetentionSummary {
    fn default() -> Self {
        Self::new(Limits::DEFAULT, false)
    }
}

impl RetentionSummary {
    fn new(limits: Limits, pruning_requested: bool) -> Self {
        Self {
            retain_completed: limits.retain_completed,
            raw_scan_limit: limits.raw_entries,
            active_record_limit: limits.active_records,
            prune_limit: limits.prune_records,
            pruning_requested,
            scanned_entries: 0,
            active_records: 0,
            invalid_records: 0,
            completed_records: 0,
            pruned_records: 0,
            pending_completed_records: 0,
            raw_scan_truncated: false,
            active_limit_exceeded: false,
            state_changed: false,
            errors: Vec::new(),
            recoveries: Vec::new(),
        }
    }

    fn issue(&mut self, path: String, error: impl std::fmt::Display) {
        self.errors.push(CleanupIssue {
            path,
            error: error.to_string(),
        });
    }
}

pub(super) struct Scan {
    /// None means the view is incomplete or its registry identity changed.
    /// Callers must not perform data cleanup or return a complete list then.
    pub records: Option<Vec<Record>>,
    pub summary: RetentionSummary,
}

struct Entry {
    name: String,
    record: Record,
    metadata_identity: FileIdentity,
    pruned: bool,
}

/// The caller already holds the registry lock and, where required, the portable
/// publication lock. Only enabled, non-dry-run cleanup passes prune_completed=true.
/// Registration never calls this scan; list and preview pass false.
pub(super) fn scan(
    store: &ManagedFiles,
    lock: &LockedFile,
    prune_completed: bool,
) -> io::Result<Scan> {
    scan_with_limits(store, lock, prune_completed, Limits::DEFAULT)
}

fn scan_with_limits(
    store: &ManagedFiles,
    lock: &LockedFile,
    prune_completed: bool,
    limits: Limits,
) -> io::Result<Scan> {
    store.check_lock(lock)?;
    let (names, truncated) = store.registry.names(limits.raw_entries)?;
    let mut summary = RetentionSummary::new(limits, prune_completed);
    summary.scanned_entries = names.len();
    summary.raw_scan_truncated = truncated;
    let mut entries = Vec::new();
    for name in names {
        let Some(name) = name.to_str().filter(|name| record_name(name)) else {
            // Unknown names, lock files and recovery artifacts are not adopted.
            continue;
        };
        match store.read_record_snapshot(name) {
            Ok((record, metadata_identity)) => {
                if record.removed_at_unix.is_some() {
                    summary.completed_records += 1;
                } else {
                    summary.active_records += 1;
                }
                entries.push(Entry {
                    name: name.to_owned(),
                    record,
                    metadata_identity,
                    pruned: false,
                });
            }
            Err(error) => {
                summary.invalid_records += 1;
                summary.issue(metadata_path(name), error);
            }
        }
    }
    // Unreadable/unparseable record-shaped entries consume the active budget;
    // calling them completed would silently bypass the original safety limit.
    summary.active_limit_exceeded = summary
        .active_records
        .saturating_add(summary.invalid_records)
        > limits.active_records;

    let mut completed: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| entry.record.removed_at_unix.map(|_| index))
        .collect();
    // Timestamp ties use record ID, so bulk cleanup in one second still has a
    // deterministic newest-N set. Filesystem enumeration order is irrelevant.
    completed.sort_by(|left, right| {
        let left = &entries[*left].record;
        let right = &entries[*right].record;
        right
            .removed_at_unix
            .cmp(&left.removed_at_unix)
            .then(right.id.cmp(&left.id))
    });
    let eligible = completed.len().saturating_sub(limits.retain_completed);
    if let Err(error) = store.check_lock(lock) {
        summary.state_changed = true;
        summary.issue(metadata_path(""), error);
    }
    if prune_completed && !summary.state_changed {
        // Even a truncated sample can prove an entry is outside the global
        // newest-N: it contains N strictly higher (timestamp, ID) witnesses.
        for index in completed
            .into_iter()
            .skip(limits.retain_completed)
            .take(limits.prune_records)
        {
            if let Err(error) = store.check_lock(lock) {
                summary.state_changed = true;
                summary.issue(metadata_path(""), error);
                break;
            }
            let entry = &mut entries[index];
            match store.prune_record(&entry.name, &entry.metadata_identity, lock) {
                Ok(RemovalOutcome::Removed { warning }) => {
                    entry.pruned = true;
                    summary.pruned_records += 1;
                    if let Some(warning) = warning {
                        summary.issue(metadata_path(&entry.name), warning);
                    }
                }
                Ok(RemovalOutcome::Preserved(recovery)) => {
                    summary.issue(
                        metadata_path(&entry.name),
                        "metadata preserved; see recovery details",
                    );
                    summary.recoveries.push(recovery);
                }
                Ok(RemovalOutcome::Eligible) => {
                    summary.issue(
                        metadata_path(&entry.name),
                        "metadata removal was not confirmed",
                    );
                }
                Err(error) => summary.issue(metadata_path(&entry.name), error),
            }
        }
    }
    summary.pending_completed_records = eligible.saturating_sub(summary.pruned_records);
    if !summary.state_changed
        && let Err(error) = store.check_lock(lock)
    {
        summary.state_changed = true;
        summary.issue(metadata_path(""), error);
    }
    let records =
        if summary.raw_scan_truncated || summary.active_limit_exceeded || summary.state_changed {
            None
        } else {
            Some(
                entries
                    .into_iter()
                    .filter(|entry| !entry.pruned)
                    .map(|entry| entry.record)
                    .collect(),
            )
        };
    Ok(Scan { records, summary })
}

fn metadata_path(name: &str) -> String {
    if name.is_empty() {
        format!(".ssmcp/{}", super::REGISTRY)
    } else {
        format!(".ssmcp/{}/{name}", super::REGISTRY)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Category, json_bytes};
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
    };

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssmcp-retention-{}-{stamp}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
            ));
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }

        fn register(&self, store: &ManagedFiles, name: &str, category: Category) -> Record {
            let relative = format!(".ssmcp/tmp/{name}");
            let path = self.0.join(&relative);
            let mut options = fs::OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&path).unwrap();
            file.write_all(name.as_bytes()).unwrap();
            file.sync_all().unwrap();
            drop(file); // Finalize Windows last-write metadata before registration.
            let file = fs::File::open(path).unwrap();
            let registered = store
                .register_created(
                    Path::new(&relative),
                    &file,
                    category,
                    "retention fixture",
                    "unit owner",
                )
                .unwrap();
            registered.record().clone()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(root: &Scratch) -> (ManagedFiles, Record, Record, Record) {
        let store = ManagedFiles::open(&root.0).unwrap();
        crate::platform::private_directory(&root.0.join(".ssmcp/tmp")).unwrap();
        let mut config = store.config().unwrap();
        config.max_age_seconds = 0;
        store.set_config(&config).unwrap();
        let old = root.register(&store, "old.txt", Category::Temporary);
        let first_time = old.created_at_unix.max(old.identity.modified_at_unix) + 1;
        assert_eq!(
            store
                .cleanup_at(false, "retention-fixture", first_time)
                .unwrap()
                .removed_files,
            1
        );
        let newer = root.register(&store, "newer.txt", Category::Temporary);
        let second_time =
            first_time.max(newer.created_at_unix.max(newer.identity.modified_at_unix)) + 1;
        assert_eq!(
            store
                .cleanup_at(false, "retention-fixture", second_time)
                .unwrap()
                .removed_files,
            1
        );
        let active = root.register(&store, "active.txt", Category::UserFile);
        (store, old, newer, active)
    }

    fn small_limits() -> Limits {
        Limits {
            retain_completed: 1,
            raw_entries: 16,
            active_records: 4,
            prune_records: 1,
        }
    }

    fn metadata_bytes(store: &ManagedFiles, record: &Record) -> Vec<u8> {
        store
            .registry
            .read(
                &format!("{}.json", record.id),
                super::super::RECORD_BYTES_LIMIT,
                true,
            )
            .unwrap()
    }

    #[test]
    fn retention_removes_only_old_completed_metadata_and_preserves_other_files() {
        let root = Scratch::new();
        let (store, old, newer, active) = fixture(&root);
        let bad_name = format!("{}.json", "f".repeat(64));
        store.registry.write_new(&bad_name, b"{bad JSON").unwrap();
        store
            .registry
            .write_new("unknown.keep", b"unknown metadata")
            .unwrap();
        // Reusing a completed record's data path does not make that new file
        // part of metadata retention. It must remain byte-for-byte intact.
        fs::write(root.0.join(&old.relative_path), b"new unregistered data").unwrap();
        let lock = store.lock().unwrap();
        let scanned = scan_with_limits(&store, &lock, true, small_limits()).unwrap();
        assert_eq!(scanned.summary.pruned_records, 1);
        assert_eq!(scanned.summary.pending_completed_records, 0);
        assert_eq!(scanned.summary.active_records, 1);
        assert_eq!(scanned.summary.invalid_records, 1);
        assert_eq!(scanned.summary.errors.len(), 1);
        let records = scanned.records.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.id == newer.id));
        assert!(records.iter().any(|record| record.id == active.id));
        assert_eq!(
            store
                .read_record(&format!("{}.json", old.id))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        assert!(
            store
                .read_record(&format!("{}.json", newer.id))
                .unwrap()
                .removed_at_unix
                .is_some()
        );
        assert!(
            store
                .read_record(&format!("{}.json", active.id))
                .unwrap()
                .removed_at_unix
                .is_none()
        );
        assert_eq!(
            store.registry.read(&bad_name, 64, true).unwrap(),
            b"{bad JSON"
        );
        assert_eq!(
            store.registry.read("unknown.keep", 64, true).unwrap(),
            b"unknown metadata"
        );
        assert_eq!(
            fs::read(root.0.join(&old.relative_path)).unwrap(),
            b"new unregistered data"
        );
        assert_eq!(
            fs::read(root.0.join(&active.relative_path)).unwrap(),
            b"active.txt"
        );
    }

    #[test]
    fn retention_preview_is_read_only_and_limits_never_return_a_partial_view() {
        let root = Scratch::new();
        let (store, old, newer, active) = fixture(&root);
        let before = [
            metadata_bytes(&store, &old),
            metadata_bytes(&store, &newer),
            metadata_bytes(&store, &active),
        ];
        let lock = store.lock().unwrap();
        let preview = scan_with_limits(&store, &lock, false, small_limits()).unwrap();
        assert_eq!(preview.summary.pruned_records, 0);
        assert_eq!(preview.summary.pending_completed_records, 1);
        assert_eq!(preview.records.unwrap().len(), 3);
        assert_eq!(
            before,
            [
                metadata_bytes(&store, &old),
                metadata_bytes(&store, &newer),
                metadata_bytes(&store, &active)
            ]
        );
        let short = scan_with_limits(
            &store,
            &lock,
            false,
            Limits {
                raw_entries: 1,
                ..small_limits()
            },
        )
        .unwrap();
        assert!(short.summary.raw_scan_truncated);
        assert!(short.records.is_none());
        assert_eq!(short.summary.pruned_records, 0);
        // A valid-looking filename with unparseable contents consumes the live
        // budget instead of being discarded as a completed/unknown record.
        let bad_name = format!("{}.json", "e".repeat(64));
        store.registry.write_new(&bad_name, b"not JSON").unwrap();
        let capped = scan_with_limits(
            &store,
            &lock,
            false,
            Limits {
                active_records: 1,
                ..small_limits()
            },
        )
        .unwrap();
        assert_eq!(capped.summary.active_records, 1);
        assert_eq!(capped.summary.invalid_records, 1);
        assert!(capped.summary.active_limit_exceeded);
        assert!(capped.records.is_none());
        assert_eq!(
            before,
            [
                metadata_bytes(&store, &old),
                metadata_bytes(&store, &newer),
                metadata_bytes(&store, &active)
            ]
        );
    }

    #[test]
    fn equal_completion_times_use_a_stable_record_id_tie_break() {
        let root = Scratch::new();
        let (store, old, newer, active) = fixture(&root);
        let lock = store.lock().unwrap();
        let completed_at = store
            .read_record(&format!("{}.json", newer.id))
            .unwrap()
            .removed_at_unix
            .unwrap();
        for record in [&old, &newer] {
            let name = format!("{}.json", record.id);
            let mut completed = store.read_record(&name).unwrap();
            completed.removed_at_unix = Some(completed_at);
            store
                .registry
                .write_atomic(&name, &json_bytes(&completed).unwrap())
                .unwrap();
        }
        let scanned = scan_with_limits(&store, &lock, true, small_limits()).unwrap();
        assert_eq!(scanned.summary.pruned_records, 1);
        let records = scanned.records.unwrap();
        let retained = old.id.max(newer.id);
        assert_eq!(
            records
                .iter()
                .filter(|record| record.removed_at_unix.is_some())
                .count(),
            1
        );
        assert!(records.iter().any(|record| record.id == retained));
        assert!(records.iter().any(|record| record.id == active.id));
    }
}
