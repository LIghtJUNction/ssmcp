//! Bounded file and skill audit events, mirrored to the local client display.
use super::{Root, Workspace, whole_file};
use crate::backend::events::EventLog;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
};

type AuditLocks = Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>;

fn write_audit_lock(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<AuditLocks> = OnceLock::new();
    // Components folds `.` aliases; the native file operation validates traversal.
    let key: PathBuf = path.components().collect();
    #[cfg(windows)]
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

impl Workspace {
    pub async fn snapshot_file(&self, path: &str) -> Option<Vec<u8>> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || root.snapshot(&path))
            .await
            .ok()
            .flatten()
    }

    pub fn audit_harness_file(
        &self,
        kind: &str,
        harness: &str,
        tool: &str,
        path: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.events
            .push(file_event(kind, harness, tool, path, before, after));
    }

    pub fn audit_harness_skill(&self, harness: &str, tool: &str, skill: Option<&str>) {
        self.events
            .push(json!({"type":"ssmcp.skill_use","harness":harness,
            "tool":tool,"skill":skill}));
    }
}

impl Root {
    fn snapshot(&self, path: &str) -> Option<Vec<u8>> {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.strip_prefix(&self.path).ok()?
        } else {
            path
        };
        let mut file = self.file(path.to_str()?).ok()?;
        // whole_file rejects special files, concurrent changes and files over
        // MAX_FILE_BYTES before allocating an unbounded preview.
        whole_file(&mut file).ok()
    }

    pub(super) fn perform_audited(
        &self,
        name: &str,
        args: Value,
        events: &EventLog,
    ) -> Result<Value, String> {
        let path = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let write = matches!(
            name,
            "ssmcp.fs.write" | "ssmcp.fs.write_text" | "ssmcp.fs.edit_text"
        );
        let read = matches!(name, "ssmcp.fs.read" | "ssmcp.fs.read_text");
        // Keep each native write and its two snapshots together. Different files
        // can still be written concurrently, without borrowing another edit's diff.
        let lock = path
            .as_deref()
            .filter(|_| write)
            .map(|path| write_audit_lock(&self.path.join(path)));
        let _guard = lock
            .as_ref()
            .map(|lock| lock.lock().unwrap_or_else(|error| error.into_inner()));
        let create_only = matches!(name, "ssmcp.fs.write" | "ssmcp.fs.write_text")
            && args.get("overwrite").and_then(Value::as_bool) != Some(true);
        let before = if write {
            path.as_deref().and_then(|path| self.snapshot(path))
        } else {
            None
        };
        let mut result = self.perform(name, args)?;
        if let Some(path) = path {
            if write {
                // Success with overwrite=false proves the old file was absent.
                // A missing snapshot otherwise means unknown, not empty bytes.
                let before = before
                    .or_else(|| create_only.then(Vec::new))
                    .filter(|bytes| snapshot_matches(bytes, &result, "previous_sha256"));
                let after = self
                    .snapshot(&path)
                    .filter(|bytes| snapshot_matches(bytes, &result, "sha256"));
                let event = file_event(
                    "filechange",
                    "files",
                    name,
                    &path,
                    before.as_deref(),
                    after.as_deref(),
                );
                result["diff"] = event["diff"].clone();
                result["diff_truncated"] = event["diff_truncated"].clone();
                result["preview_available"] = event["preview_available"].clone();
                events.push(event);
            } else if read {
                events.push(file_event("fileread", "files", name, &path, None, None));
            }
        }
        Ok(result)
    }
}

fn snapshot_matches(bytes: &[u8], result: &Value, field: &str) -> bool {
    result[field]
        .as_str()
        .is_none_or(|expected| super::sha256(bytes) == expected)
}

fn file_event(
    kind: &str,
    harness: &str,
    tool: &str,
    path: &str,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> Value {
    let available = before.is_some() && after.is_some();
    let (diff, truncated) = match (kind, before, after) {
        ("filechange", Some(before), Some(after)) if before != after => {
            match (std::str::from_utf8(before), std::str::from_utf8(after)) {
                (Ok(before), Ok(after)) => unified_diff(path, before, after),
                _ => ("Binary file changed".into(), false),
            }
        }
        _ => (String::new(), false),
    };
    json!({"type":format!("ssmcp.{kind}"),"harness":harness,"tool":tool,"path":path,
        "diff":diff,"diff_truncated":truncated,
        "preview_available":kind != "filechange" || available})
}

/// A single replacement hunk with context. Bounded display never alters the edit.
fn unified_diff(path: &str, before: &str, after: &str) -> (String, bool) {
    if before == after {
        return (String::new(), false);
    }
    let old: Vec<_> = before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let start = prefix.saturating_sub(3);
    let old_end = (old.len() - suffix + 3).min(old.len());
    let new_end = (new.len() - suffix + 3).min(new.len());
    let old_start = if old_end == start { start } else { start + 1 };
    let new_start = if new_end == start { start } else { start + 1 };
    let mut diff = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{old_start},{} +{new_start},{} @@\n",
        old_end - start,
        new_end - start
    );
    let mut lines = 0;
    let mut truncated = false;
    let mut append = |prefix: char, line: &str| {
        let terminated = line.ends_with('\n');
        let marker = if terminated {
            ""
        } else {
            "\\ No newline at end of file\n"
        };
        let count = 1 + usize::from(!terminated);
        if lines + count > 160 || diff.len() + line.len() + marker.len() + 2 > 16 * 1024 {
            truncated = true;
            return;
        }
        diff.push(prefix);
        diff.push_str(line.strip_suffix('\n').unwrap_or(line));
        diff.push('\n');
        diff.push_str(marker);
        lines += count;
    };
    for line in &old[start..prefix] {
        append(' ', line);
    }
    for line in &old[prefix..old.len() - suffix] {
        append('-', line);
    }
    for line in &new[prefix..new.len() - suffix] {
        append('+', line);
    }
    for line in &old[old.len() - suffix..old_end] {
        append(' ', line);
    }
    if truncated {
        diff.push_str("… diff preview folded; the complete change is saved in the file\n");
    }
    (diff, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_has_context_and_keeps_real_line_numbers() {
        let (diff, truncated) = unified_diff("src/a.rs", "same\nold\ntail\n", "same\nnew\ntail\n");
        assert!(diff.contains("@@ -1,3 +1,3 @@\n same\n-old\n+new\n tail\n"));
        assert!(!truncated);
        assert!(unified_diff("a", "", "new\n").0.contains("@@ -0,0 +1,1 @@"));
        assert!(unified_diff("a", "old\n", "").0.contains("@@ -1,1 +0,0 @@"));
        assert!(unified_diff("a", "same", "same").0.is_empty());
    }

    #[test]
    fn diff_keeps_newline_only_changes_visible() {
        let (diff, truncated) = unified_diff("a", "line", "line\n");
        assert!(diff.contains("-line\n\\ No newline at end of file\n+line\n"));
        assert!(!truncated);
        let (diff, _) = unified_diff("a", "line\n", "line");
        assert!(diff.contains("-line\n+line\n\\ No newline at end of file\n"));
    }

    #[test]
    fn concurrent_audits_share_only_the_same_file_lock() {
        let root = std::env::temp_dir().join("ssmcp-audit-lock-test");
        let one = write_audit_lock(&root.join("a"));
        let same = write_audit_lock(&root.join("./a"));
        let different = write_audit_lock(&root.join("b"));
        assert!(Arc::ptr_eq(&one, &same));
        assert!(!Arc::ptr_eq(&one, &different));
        let _guard = one.lock().unwrap();
        assert!(same.try_lock().is_err());
        assert!(different.try_lock().is_ok());
    }

    #[test]
    fn unrelated_readback_is_not_attributed_to_the_edit() {
        let result = json!({"sha256":super::super::sha256(b"our change")});
        assert!(snapshot_matches(b"our change", &result, "sha256"));
        assert!(!snapshot_matches(b"another change", &result, "sha256"));
        let binary = json!({"sha256":super::super::sha256(b"\0\xff")});
        assert!(snapshot_matches(b"\0\xff", &binary, "sha256"));
        assert!(!snapshot_matches(b"\0\xef\xbf\xbd", &binary, "sha256"));
    }
}
