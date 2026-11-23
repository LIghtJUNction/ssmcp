//! Evidence from native filesystem operations. External file effects are not inferred.
use super::{MAX_FILE_BYTES, Root, sha256, whole_file};
use crate::{backend::events::EventLog, output::OutputStore};
use serde_json::{Value, json};
use std::{collections::HashSet, fs::File, sync::Arc};

const PREVIEW_BYTES: usize = 16 * 1024;
const PREVIEW_LINES: usize = 160;
// A pathological diff must not monopolize the filesystem executor. Exhausting
// either bound produces unknown counts, never a fabricated replacement diff.
const MAX_DIFF_WORK: usize = 8_000_000;
const MAX_TRACE_ENTRIES: usize = 2_000_000;

pub(super) enum Snapshot {
    Missing,
    Text(String),
    Unknown(&'static str),
}

impl Snapshot {
    pub(super) fn from_bytes(path: &str, bytes: &[u8]) -> Self {
        if sensitive_path(path) {
            return Self::Unknown("sensitive_path");
        }
        if bytes.len() > MAX_FILE_BYTES {
            return Self::Unknown("snapshot_size_limit");
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                if text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
                {
                    Self::Unknown("binary_content")
                } else if sensitive_text(text) {
                    Self::Unknown("sensitive_content")
                } else {
                    Self::Text(text.to_owned())
                }
            }
            Err(_) => Self::Unknown("binary_content"),
        }
    }

    pub(super) fn from_file(path: &str, file: &mut File) -> Self {
        if sensitive_path(path) {
            return Self::Unknown("sensitive_path");
        }
        if file
            .metadata()
            .is_ok_and(|m| m.len() > MAX_FILE_BYTES as u64)
        {
            return Self::Unknown("snapshot_size_limit");
        }
        match whole_file(file) {
            Ok(bytes) => Self::from_bytes(path, &bytes),
            Err(_) => Self::Unknown("snapshot_unavailable"),
        }
    }

    fn metadata(&self) -> Value {
        match self {
            Self::Missing => json!({"state":"missing"}),
            Self::Text(text) => {
                json!({"state":"known","bytes":text.len(),"sha256":sha256(text.as_bytes())})
            }
            Self::Unknown(reason) => json!({"state":"unknown","reason":reason}),
        }
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Missing => Some(""),
            Self::Text(text) => Some(text),
            Self::Unknown(_) => None,
        }
    }
}

pub(super) struct FileAudit {
    events: Arc<EventLog>,
    output: Arc<OutputStore>,
}

impl FileAudit {
    pub(super) fn new(events: Arc<EventLog>, output: Arc<OutputStore>) -> Self {
        Self { events, output }
    }

    pub(super) fn written(&self, tool: &str, path: &str, before: Snapshot, committed: &[u8]) {
        // Callers invoke this only after the atomic replace succeeds, while
        // still holding their native directory lock. Content comes from that
        // committed write, not an unrelated subsequent filesystem snapshot.
        let after = Snapshot::from_bytes(path, committed);
        self.events.push(json!({
            "type":"ssmcp.file.write","tool":tool,"path":path,
            "written":committed.len(),"created":matches!(before, Snapshot::Missing),
            "observation":"native_atomic_write","external_operations":"unknown"
        }));
        self.events.push(self.diff_event(tool, path, before, after));
    }

    fn diff_event(&self, tool: &str, path: &str, before: Snapshot, after: Snapshot) -> Value {
        let mut event = json!({
            "type":"ssmcp.file.diff","tool":tool,"path":path,
            "before":before.metadata(),"after":after.metadata(),
            "added_lines":null,"removed_lines":null,"changed":null,
            "diff":null,"preview":null,"preview_truncated":false,
            "diff_artifact":null,"diff_complete":false,
            "observation":"native_atomic_write","external_operations":"unknown",
            "external_writers":"not_serialized"
        });
        let (Some(old), Some(new)) = (before.text(), after.text()) else {
            event["diff_unavailable_reason"] = json!("snapshot_unknown");
            return event;
        };
        let diff = match unified_diff(
            path,
            old,
            new,
            matches!(before, Snapshot::Missing),
            matches!(after, Snapshot::Missing),
        ) {
            Ok(diff) => diff,
            Err(reason) => {
                event["diff_unavailable_reason"] = json!(reason);
                return event;
            }
        };
        event["added_lines"] = json!(diff.added);
        event["removed_lines"] = json!(diff.removed);
        event["changed"] = json!(
            old != new || matches!(before, Snapshot::Missing) != matches!(after, Snapshot::Missing)
        );
        let (preview, truncated) = preview(&diff.text);
        event["preview"] = json!(preview);
        event["preview_truncated"] = json!(truncated);
        let mut capture = self.output.capture("workspace_file_diff");
        let saved = capture
            .push(diff.text.as_bytes())
            .and_then(|_| capture.finish());
        // Finish failed captures too, so partial bytes have honest metadata.
        if saved.is_err() {
            let _ = capture.finish();
        }
        event["diff_complete"] = json!(saved.is_ok());
        if capture.is_spilled() {
            event["diff_artifact"] = json!(capture.metadata());
        } else if saved.is_ok() {
            // Small diffs are fully queryable in fs.events. Larger ones use the
            // same output.read/stat and retention policy as every other source.
            event["diff"] = json!(diff.text);
        }
        if let Some(error) = capture.error() {
            event["output_error"] = json!(error);
        }
        event
    }
}

impl Root {
    pub(super) fn perform_audited(&self, name: &str, args: Value) -> Result<Value, String> {
        self.perform_audited_with_source(name, args, false)
    }

    pub(super) fn read_metadata_for_completion(&self, args: Value) -> Result<Value, String> {
        self.perform_audited_with_source("ssmcp.fs.read", args, true)
    }

    fn perform_audited_with_source(
        &self,
        name: &str,
        args: Value,
        local_discovery: bool,
    ) -> Result<Value, String> {
        if let Some(operation) = super::safety::operation(name) {
            let decision = super::safety::authorize(operation, &args);
            if !decision.is_allowed() {
                self.safety_events
                    .push_local_discovery(super::safety::event(operation, decision, "blocked"));
                return Err("native file mutation blocked by safety policy".into());
            }
            let result = self.perform(operation, args);
            let outcome = if result.is_ok() {
                "succeeded"
            } else {
                "failed"
            };
            self.safety_events
                .push_local_discovery(super::safety::event(operation, decision, outcome));
            return result;
        }
        let path = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let result = self.perform(name, args)?;
        if matches!(
            name,
            "ssmcp.fs.read" | "ssmcp.fs.read_text" | "ssmcp.fs.read_image"
        ) && let Some(path) = path
        {
            let (offset, next_offset) = if name == "ssmcp.fs.read_image" {
                (Some(0), result["structuredContent"]["size"].as_u64())
            } else {
                (result["offset"].as_u64(), result["next_offset"].as_u64())
            };
            let event = json!({
                "type":"ssmcp.file.read","tool":name,"path":path,
                "offset":offset,"next_offset":next_offset,
                "bytes":next_offset.zip(offset).map(|(end, start)| end.saturating_sub(start)),
                "sensitive":sensitive_path(&path),
                "observation":"native_file_read","external_operations":"unknown"
            });
            if local_discovery {
                self.audit.events.push_local_discovery(event);
            } else {
                self.audit.events.push(event);
            }
        }
        Ok(result)
    }
}

// Audit metadata deliberately does not include environment values, arguments,
// or file bodies. Sensitive paths and recognizable secret material also never
// enter diff previews or output artifacts. This is not a claim that arbitrary
// secrets in an otherwise ordinary source file can always be recognized.
fn sensitive_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| {
        let name = part.to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".env"
                | ".envrc"
                | ".ssh"
                | ".aws"
                | ".azure"
                | ".gnupg"
                | ".kube"
                | ".git"
                | ".npmrc"
                | ".netrc"
                | ".pypirc"
                | "auth.json"
                | "oauth.json"
                | ".claude.json"
                | "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
                | "private"
                | "private_keys"
                | "private-keys"
        ) || name.starts_with(".env.")
            || name.trim_start_matches('.').starts_with("credential")
            || name.trim_start_matches('.').starts_with("secret")
            || name.starts_with("tokens.")
            || name.starts_with("token.")
            || (name.starts_with("ssh_host_") && name.ends_with("_key"))
            || [".pem", ".key", ".p12", ".pfx", ".jks", ".kdbx"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
    })
}

fn sensitive_text(text: &str) -> bool {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    for word in text.split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '-' | '_' | '.')) {
        if (word.len() >= 20
            && ["sk-", "ghp_", "github_pat_", "gho_", "ghu_", "ghs_", "ghr_"]
                .iter()
                .any(|prefix| word.starts_with(prefix)))
            || (word.len() >= 16
                && ["xoxb-", "xoxp-", "xoxa-", "xoxr-"]
                    .iter()
                    .any(|p| word.starts_with(p)))
            || (word.len() == 20
                && (word.starts_with("AKIA") || word.starts_with("ASIA"))
                && word
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
        {
            return true;
        }
    }
    text.lines().any(|line| {
        let Some((key, value)) = line.trim().split_once(['=', ':']) else {
            return false;
        };
        let key = key.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
        let value = value.trim().trim_matches(['"', '\'', ',', ';']);
        matches!(
            key.as_str(),
            "api_key"
                | "api-key"
                | "apikey"
                | "access_token"
                | "refresh_token"
                | "token"
                | "accesstoken"
                | "refreshtoken"
                | "authorization"
                | "client_secret"
                | "aws_secret_access_key"
                | "password"
                | "private_key"
        ) && value.len() >= 12
            && !value.starts_with(['$', '{', '<'])
            && !value.contains(' ')
    })
}

fn preview(diff: &str) -> (&str, bool) {
    let mut end = 0;
    for (count, line) in diff.split_inclusive('\n').enumerate() {
        if count == PREVIEW_LINES || end + line.len() > PREVIEW_BYTES {
            break;
        }
        end += line.len();
    }
    (&diff[..end], end != diff.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

fn shortest_edits<'a>(old: &[&'a str], new: &[&'a str]) -> Result<Vec<Edit<'a>>, &'static str> {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let a = &old[prefix..old.len() - suffix];
    let b = &new[prefix..new.len() - suffix];
    let mut edits = Vec::with_capacity(old.len() + new.len());
    edits.extend(old[..prefix].iter().map(|line| Edit::Equal(line)));
    let old_lines: HashSet<_> = a.iter().copied().collect();
    if a.is_empty() || b.is_empty() || !b.iter().any(|line| old_lines.contains(line)) {
        edits.extend(a.iter().map(|line| Edit::Delete(line)));
        edits.extend(b.iter().map(|line| Edit::Insert(line)));
    } else {
        edits.extend(myers(a, b)?);
    }
    edits.extend(
        old[old.len() - suffix..]
            .iter()
            .map(|line| Edit::Equal(line)),
    );
    Ok(edits)
}

// Myers' shortest edit path, with only the reachable diagonals retained in each
// trace row. It keeps separated edits and repeated unchanged lines intact.
fn myers<'a>(old: &[&'a str], new: &[&'a str]) -> Result<Vec<Edit<'a>>, &'static str> {
    let mut trace: Vec<Vec<usize>> = Vec::new();
    let mut work = 0usize;
    let mut entries = 0usize;
    let mut distance = None;
    'search: for d in 0..=old.len() + new.len() {
        entries += d + 1;
        if entries > MAX_TRACE_ENTRIES {
            return Err("diff_complexity_limit");
        }
        let mut row = Vec::with_capacity(d + 1);
        for j in 0..=d {
            work += 1;
            if work > MAX_DIFF_WORK {
                return Err("diff_complexity_limit");
            }
            let k = 2 * j as isize - d as isize;
            let mut x = if d == 0 {
                0
            } else {
                let previous = &trace[d - 1];
                if j == 0 {
                    previous[0]
                } else if j == d {
                    previous[d - 1] + 1
                } else if previous[j - 1] < previous[j] {
                    previous[j]
                } else {
                    previous[j - 1] + 1
                }
            };
            let mut y = x as isize - k;
            while x < old.len() && y >= 0 && (y as usize) < new.len() && old[x] == new[y as usize] {
                x += 1;
                y += 1;
                work += 1;
                if work > MAX_DIFF_WORK {
                    return Err("diff_complexity_limit");
                }
            }
            row.push(x);
            if x >= old.len() && y >= new.len() as isize {
                trace.push(row);
                distance = Some(d);
                break 'search;
            }
        }
        trace.push(row);
    }
    let Some(distance) = distance else {
        return Err("diff_unavailable");
    };
    let (mut x, mut y) = (old.len(), new.len());
    let mut reverse = Vec::with_capacity(x + y);
    for d in (1..=distance).rev() {
        let k = x as isize - y as isize;
        let j = ((k + d as isize) / 2) as usize;
        let previous = &trace[d - 1];
        let insert = j == 0 || (j != d && previous[j - 1] < previous[j]);
        let previous_k = if insert { k + 1 } else { k - 1 };
        let previous_x = previous[((previous_k + d as isize - 1) / 2) as usize];
        let previous_y = (previous_x as isize - previous_k) as usize;
        while x > previous_x && y > previous_y {
            reverse.push(Edit::Equal(old[x - 1]));
            x -= 1;
            y -= 1;
        }
        if insert {
            reverse.push(Edit::Insert(new[y - 1]));
            y -= 1;
        } else {
            reverse.push(Edit::Delete(old[x - 1]));
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        reverse.push(Edit::Equal(old[x - 1]));
        x -= 1;
        y -= 1;
    }
    reverse.reverse();
    Ok(reverse)
}

struct Diff {
    text: String,
    added: usize,
    removed: usize,
}

fn unified_diff(
    path: &str,
    old: &str,
    new: &str,
    old_missing: bool,
    new_missing: bool,
) -> Result<Diff, &'static str> {
    let before: Vec<_> = old.split_inclusive('\n').collect();
    let after: Vec<_> = new.split_inclusive('\n').collect();
    let edits = shortest_edits(&before, &after)?;
    let added = edits
        .iter()
        .filter(|edit| matches!(edit, Edit::Insert(_)))
        .count();
    let removed = edits
        .iter()
        .filter(|edit| matches!(edit, Edit::Delete(_)))
        .count();
    if added == 0 && removed == 0 && old_missing == new_missing {
        return Ok(Diff {
            text: String::new(),
            added,
            removed,
        });
    }
    let old_name = if old_missing {
        "/dev/null".to_owned()
    } else {
        format!("a/{path}")
    };
    let new_name = if new_missing {
        "/dev/null".to_owned()
    } else {
        format!("b/{path}")
    };
    let mut text = format!("--- {old_name}\n+++ {new_name}\n");
    let changes: Vec<_> = edits
        .iter()
        .enumerate()
        .filter_map(|(i, edit)| (!matches!(edit, Edit::Equal(_))).then_some(i))
        .collect();
    let (mut next_change, mut cursor, mut old_line, mut new_line) = (0, 0, 1, 1);
    while next_change < changes.len() {
        let start = changes[next_change].saturating_sub(3);
        let mut last = changes[next_change];
        next_change += 1;
        while next_change < changes.len() && changes[next_change] - last <= 7 {
            last = changes[next_change];
            next_change += 1;
        }
        let end = (last + 4).min(edits.len());
        for edit in &edits[cursor..start] {
            advance(*edit, &mut old_line, &mut new_line);
        }
        let old_count = edits[start..end]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Insert(_)))
            .count();
        let new_count = edits[start..end]
            .iter()
            .filter(|edit| !matches!(edit, Edit::Delete(_)))
            .count();
        let old_start = if old_count == 0 {
            old_line - 1
        } else {
            old_line
        };
        let new_start = if new_count == 0 {
            new_line - 1
        } else {
            new_line
        };
        text.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
        ));
        for edit in &edits[start..end] {
            let (prefix, line) = match edit {
                Edit::Equal(line) => (' ', *line),
                Edit::Delete(line) => ('-', *line),
                Edit::Insert(line) => ('+', *line),
            };
            text.push(prefix);
            text.push_str(line);
            if !line.ends_with('\n') {
                text.push_str("\n\\ No newline at end of file\n");
            }
            advance(*edit, &mut old_line, &mut new_line);
        }
        cursor = end;
    }
    Ok(Diff {
        text,
        added,
        removed,
    })
}

fn advance(edit: Edit<'_>, old: &mut usize, new: &mut usize) {
    if !matches!(edit, Edit::Insert(_)) {
        *old += 1;
    }
    if !matches!(edit, Edit::Delete(_)) {
        *new += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::super::Workspace;
    use super::*;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-audit-{}-{}",
                std::process::id(),
                super::super::random_id().unwrap()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn workspace(&self, output: Arc<OutputStore>) -> Workspace {
            Workspace::with_output(self.0.clone(), output).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn write_after_contention(workspace: &Workspace, args: Value) -> Result<Value, String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match workspace.perform("ssmcp.fs.write_text", args.clone()).await {
                // The portable writer returns this only before reading the preimage,
                // publishing bytes, or emitting audit events. Parallel tests on other
                // workspaces share that bounded lock; all other failures stay fatal.
                Err(error)
                    if error == "workspace is busy; retry the file operation"
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                result => return result,
            }
        }
    }

    #[test]
    fn separated_edits_count_only_changed_lines_and_preserve_newline_status() {
        let diff = unified_diff(
            "src/a.rs",
            "same\nold\nmiddle\nold2\ntail\n",
            "same\nnew\nmiddle\nnew2\ntail\n",
            false,
            false,
        )
        .unwrap();
        assert_eq!((diff.added, diff.removed), (2, 2));
        assert!(diff.text.contains(" middle\n"));
        assert!(!diff.text.contains("-middle"));
        let diff = unified_diff("a", "same", "same\n", false, false).unwrap();
        assert_eq!((diff.added, diff.removed), (1, 1));
        assert!(
            diff.text
                .contains("-same\n\\ No newline at end of file\n+same\n")
        );
        assert!(
            unified_diff("a", "", "new\n", true, false)
                .unwrap()
                .text
                .contains("@@ -0,0 +1,1 @@")
        );
        assert!(
            unified_diff("a", "old\n", "", false, true)
                .unwrap()
                .text
                .contains("@@ -1,1 +0,0 @@")
        );
        assert!(
            unified_diff("a", "same", "same", false, false)
                .unwrap()
                .text
                .is_empty()
        );
    }

    #[test]
    fn distant_changes_keep_real_hunk_positions() {
        let old = (0..24).map(|i| format!("line-{i}\n")).collect::<String>();
        let new = old
            .replace("line-2\n", "changed-2\n")
            .replace("line-20\n", "changed-20\n");
        let diff = unified_diff("a", &old, &new, false, false).unwrap();
        assert_eq!((diff.added, diff.removed), (2, 2));
        assert!(diff.text.contains("@@ -1,6 +1,6 @@"));
        assert!(diff.text.contains("@@ -18,7 +18,7 @@"));
        assert!(!diff.text.contains("line-10"));
    }

    #[test]
    fn edit_paths_reconstruct_inputs_and_match_independent_lcs_counts() {
        let mut samples = vec![Vec::new()];
        for len in 1..=5 {
            for bits in 0..1usize << len {
                samples.push(
                    (0..len)
                        .map(|i| if bits & (1 << i) == 0 { "a\n" } else { "b\n" })
                        .collect::<Vec<_>>(),
                );
            }
        }
        for old in &samples {
            for new in &samples {
                let edits = shortest_edits(old, new).unwrap();
                let reconstructed_old: Vec<_> = edits
                    .iter()
                    .filter_map(|edit| match edit {
                        Edit::Equal(line) | Edit::Delete(line) => Some(*line),
                        _ => None,
                    })
                    .collect();
                let reconstructed_new: Vec<_> = edits
                    .iter()
                    .filter_map(|edit| match edit {
                        Edit::Equal(line) | Edit::Insert(line) => Some(*line),
                        _ => None,
                    })
                    .collect();
                assert_eq!(&reconstructed_old, old);
                assert_eq!(&reconstructed_new, new);
                let mut lengths = vec![vec![0; new.len() + 1]; old.len() + 1];
                for i in 0..old.len() {
                    for j in 0..new.len() {
                        lengths[i + 1][j + 1] = if old[i] == new[j] {
                            lengths[i][j] + 1
                        } else {
                            lengths[i][j + 1].max(lengths[i + 1][j])
                        };
                    }
                }
                assert_eq!(
                    edits.iter().filter(|e| matches!(e, Edit::Equal(_))).count(),
                    lengths[old.len()][new.len()]
                );
            }
        }
    }

    #[test]
    fn unknown_is_never_treated_as_missing_and_sensitive_content_is_not_archived() {
        let output = OutputStore::global().unwrap();
        let audit = FileAudit::new(EventLog::new("test"), output);
        let unknown = audit.diff_event(
            "write",
            "file",
            Snapshot::Unknown("permission_denied"),
            Snapshot::Text("replacement\n".into()),
        );
        assert_eq!(unknown["before"]["state"], "unknown");
        assert!(unknown["added_lines"].is_null());
        assert!(unknown["diff"].is_null());
        assert_eq!(unknown["diff_complete"], false);
        for path in [
            ".env",
            ".env.local",
            ".ssh/id_ed25519",
            "nested/credentials.json",
            "key.pem",
        ] {
            let event = audit.diff_event(
                "write",
                path,
                Snapshot::Missing,
                Snapshot::from_bytes(path, b"private_value_123456789\n"),
            );
            assert!(event["diff_artifact"].is_null());
            assert!(event["preview"].is_null());
            assert!(!event.to_string().contains("private_value_123456789"));
        }
        assert!(matches!(
            Snapshot::from_bytes("normal.txt", b"-----BEGIN PRIVATE KEY-----\nprivate\n"),
            Snapshot::Unknown("sensitive_content")
        ));
    }

    #[tokio::test]
    async fn only_successful_native_operations_emit_and_large_diffs_round_trip() {
        let scratch = Scratch::new();
        let output = OutputStore::global().unwrap();
        let workspace = scratch.workspace(output.clone());
        assert!(
            workspace
                .perform("ssmcp.fs.read_text", json!({"path":"missing"}))
                .await
                .is_err()
        );
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.edit_text",
                    json!({"path":"missing","old_text":"old","new_text":"new"})
                )
                .await
                .is_err()
        );
        assert_eq!(
            workspace
                .perform("ssmcp.fs.events", json!({}))
                .await
                .unwrap()["next_cursor"],
            0
        );
        let text = (0..650)
            .map(|i| format!("row-{i:04} {}\n", "x".repeat(64)))
            .collect::<String>();
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"large.txt","text":text}),
            )
            .await
            .unwrap();
        let page = workspace
            .perform("ssmcp.fs.events", json!({}))
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"]["type"], "ssmcp.file.write");
        let diff = &events[1]["event"];
        assert_eq!(diff["added_lines"], 650);
        assert_eq!(diff["removed_lines"], 0);
        assert_eq!(diff["diff_complete"], true);
        assert_eq!(diff["preview_truncated"], true);
        assert!(diff["diff"].is_null());
        let artifact = &diff["diff_artifact"];
        assert_eq!(artifact["complete"], true);
        let id = artifact["id"].as_str().unwrap();
        let mut restored = Vec::new();
        let mut offset = 0;
        loop {
            let page = output
                .perform(
                    "ssmcp.output.read",
                    json!({"id":id,"offset":offset,"max_bytes":4096}),
                )
                .await
                .unwrap();
            use base64::Engine;
            restored.extend(
                base64::engine::general_purpose::STANDARD
                    .decode(page["data_base64"].as_str().unwrap())
                    .unwrap(),
            );
            if page["eof"] == true {
                break;
            }
            offset = page["next_offset"].as_u64().unwrap();
        }
        assert_eq!(sha256(&restored), artifact["sha256"].as_str().unwrap());
        assert!(
            String::from_utf8(restored)
                .unwrap()
                .ends_with(&format!("+row-0649 {}\n", "x".repeat(64)))
        );
        let cursor = page["next_cursor"].as_u64().unwrap();
        assert!(workspace.perform("ssmcp.fs.write_text",
            json!({"path":"large.txt","text":"bad","overwrite":true,"expected_sha256":"0".repeat(64)})).await.is_err());
        assert_eq!(
            workspace
                .perform("ssmcp.fs.events", json!({"cursor":cursor}))
                .await
                .unwrap()["next_cursor"],
            cursor
        );
    }

    #[tokio::test]
    async fn binary_and_oversized_preimages_have_unknown_counts() {
        let scratch = Scratch::new();
        let output = OutputStore::global().unwrap();
        let workspace = scratch.workspace(output);
        for (path, before) in [
            ("binary", vec![0, 255]),
            ("large", vec![b'x'; MAX_FILE_BYTES + 1]),
        ] {
            std::fs::write(scratch.0.join(path), before).unwrap();
            workspace
                .perform(
                    "ssmcp.fs.write_text",
                    json!({"path":path,"text":"replacement\n","overwrite":true}),
                )
                .await
                .unwrap();
        }
        let events = workspace
            .perform("ssmcp.fs.events", json!({}))
            .await
            .unwrap();
        for item in events["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["event"]["type"] == "ssmcp.file.diff")
        {
            assert_eq!(item["event"]["before"]["state"], "unknown");
            assert!(item["event"]["added_lines"].is_null());
            assert!(item["event"]["diff_artifact"].is_null());
        }
    }
    #[tokio::test]
    async fn read_edit_and_sensitive_events_report_observations_once() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace(OutputStore::global().unwrap());
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":"file","text":"first\nsame\nlast\n"}),
            )
            .await
            .unwrap();
        workspace
            .perform("ssmcp.fs.read_text", json!({"path":"file"}))
            .await
            .unwrap();
        workspace
            .perform(
                "ssmcp.fs.edit_text",
                json!({"path":"file","old_text":"same","new_text":"changed"}),
            )
            .await
            .unwrap();
        workspace
            .perform(
                "ssmcp.fs.write_text",
                json!({"path":".env","text":"TOKEN=private_value_123456789\n"}),
            )
            .await
            .unwrap();
        let page = workspace
            .perform("ssmcp.fs.events", json!({}))
            .await
            .unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 7);
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"]["type"] == "ssmcp.file.read")
                .count(),
            1
        );
        let change = &events[4]["event"];
        assert_eq!(change["type"], "ssmcp.file.diff");
        assert_eq!(change["added_lines"], 1);
        assert_eq!(change["removed_lines"], 1);
        assert!(
            change["diff"]
                .as_str()
                .unwrap()
                .contains("-same\n+changed\n")
        );
        let secret = &events[6]["event"];
        assert_eq!(secret["after"]["reason"], "sensitive_path");
        assert!(secret["diff_artifact"].is_null());
        assert!(!page.to_string().contains("private_value_123456789"));
    }

    #[tokio::test]
    async fn concurrent_native_writes_have_a_consistent_snapshot_chain() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace(OutputStore::global().unwrap());
        write_after_contention(&workspace, json!({"path":"shared","text":"initial\n"}))
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            write_after_contention(
                &workspace,
                json!({"path":"shared","text":"left\n","overwrite":true})
            ),
            write_after_contention(
                &workspace,
                json!({"path":"shared","text":"right\n","overwrite":true})
            ),
        );
        left.unwrap();
        right.unwrap();
        let page = workspace
            .perform("ssmcp.fs.events", json!({}))
            .await
            .unwrap();
        let diffs: Vec<_> = page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["event"]["type"] == "ssmcp.file.diff")
            .map(|item| &item["event"])
            .collect();
        assert_eq!(diffs.len(), 3);
        for pair in diffs.windows(2) {
            assert_eq!(pair[0]["after"]["sha256"], pair[1]["before"]["sha256"]);
            assert_eq!(pair[1]["added_lines"], 1);
            assert_eq!(pair[1]["removed_lines"], 1);
        }
        assert_eq!(
            diffs[2]["after"]["sha256"],
            sha256(&std::fs::read(scratch.0.join("shared")).unwrap())
        );
    }

    #[test]
    fn pathological_edit_distance_reports_unknown_instead_of_false_counts() {
        let old = (0..2200).map(|i| format!("line-{i}\n")).collect::<String>();
        let new = (0..2200)
            .rev()
            .map(|i| format!("line-{i}\n"))
            .collect::<String>();
        let audit = FileAudit::new(EventLog::new("test"), OutputStore::global().unwrap());
        let event = audit.diff_event(
            "write",
            "reversed",
            Snapshot::Text(old),
            Snapshot::Text(new),
        );
        assert_eq!(event["diff_unavailable_reason"], "diff_complexity_limit");
        assert!(event["added_lines"].is_null());
        assert!(event["removed_lines"].is_null());
        assert_eq!(event["diff_complete"], false);
    }

    #[test]
    fn diff_keeps_newline_only_changes_visible() {
        let diff = unified_diff("a", "line", "line\n", false, false).unwrap();
        assert!(
            diff.text
                .contains("-line\n\\ No newline at end of file\n+line\n")
        );
        assert_eq!((diff.added, diff.removed), (1, 1));
        let diff = unified_diff("a", "line\n", "line", false, false).unwrap();
        assert!(
            diff.text
                .contains("-line\n+line\n\\ No newline at end of file\n")
        );
        assert_eq!((diff.added, diff.removed), (1, 1));
    }

    #[tokio::test]
    async fn concurrent_audits_on_distinct_files_keep_their_own_snapshots() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("a"), "before-a\n").unwrap();
        std::fs::write(scratch.0.join("b"), "before-b\n").unwrap();
        let left = scratch.workspace(OutputStore::global().unwrap());
        let right = scratch.workspace(OutputStore::global().unwrap());
        let (a, b) = tokio::join!(
            write_after_contention(
                &left,
                json!({"path":"a","text":"after-a\n","overwrite":true})
            ),
            write_after_contention(
                &right,
                json!({"path":"b","text":"after-b\n","overwrite":true})
            ),
        );
        a.unwrap();
        b.unwrap();
        for (workspace, path) in [(&left, "a"), (&right, "b")] {
            let page = workspace
                .perform("ssmcp.fs.events", json!({}))
                .await
                .unwrap();
            let events = page["events"].as_array().unwrap();
            assert_eq!(events.len(), 2);
            let diff = &events[1]["event"];
            assert_eq!(diff["path"], path);
            assert_eq!(
                diff["before"]["sha256"],
                sha256(format!("before-{path}\n").as_bytes())
            );
            assert_eq!(
                diff["after"]["sha256"],
                sha256(format!("after-{path}\n").as_bytes())
            );
            assert_eq!(diff["added_lines"], 1);
            assert_eq!(diff["removed_lines"], 1);
        }
    }

    #[tokio::test]
    async fn unrelated_readback_is_not_attributed_to_the_edit() {
        let scratch = Scratch::new();
        let events = EventLog::new("test");
        let audit = FileAudit::new(events.clone(), OutputStore::global().unwrap());
        // A later external mutation must not replace the committed bytes in an
        // earlier native write's evidence. No post-write readback is used.
        std::fs::write(scratch.0.join("file"), "another change").unwrap();
        audit.written(
            "write",
            "file",
            Snapshot::Text("before".into()),
            b"our change",
        );
        audit.written("write", "binary", Snapshot::Missing, b"\0\xff");
        let page = events.read(json!({})).await.unwrap();
        let event = &page["events"][1]["event"];
        assert_eq!(event["after"]["sha256"], sha256(b"our change"));
        assert_ne!(
            event["after"]["sha256"],
            sha256(&std::fs::read(scratch.0.join("file")).unwrap())
        );
        let binary = &page["events"][3]["event"];
        assert_eq!(binary["after"]["state"], "unknown");
        assert!(binary["diff"].is_null());
        assert!(binary["added_lines"].is_null());
    }
}
