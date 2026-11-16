//! Compact local visibility for file and skill operations; wire results stay unchanged.
use super::{MAX_FILE_BYTES, Root, Workspace};
use serde_json::{Value, json};
use std::path::Path;

impl Workspace {
    pub async fn snapshot_file(&self, path: &str) -> Option<String> {
        let root = self.root.clone();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || root.snapshot_text(&path))
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
        before: Option<&str>,
        after: Option<&str>,
    ) {
        file_event(kind, harness, tool, path, before, after);
    }

    pub fn audit_harness_skill(&self, harness: &str, tool: &str, skill: Option<&str>) {
        crate::client::render::event(
            harness,
            &json!({"type":"ssmcp.skill_use","tool":tool,"skill":skill}),
        );
    }
}

impl Root {
    fn snapshot_text(&self, path: &str) -> Option<String> {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.strip_prefix(&self.path).ok()?
        } else {
            path
        };
        let result = self
            .perform(
                "ssmcp.fs.read_text",
                json!({"path":path,"max_bytes":MAX_FILE_BYTES}),
            )
            .ok()?;
        (result["eof"] == true)
            .then(|| result["text"].as_str().map(str::to_owned))
            .flatten()
    }

    pub(super) fn perform_audited(&self, name: &str, args: Value) -> Result<Value, String> {
        let path = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let write = matches!(
            name,
            "ssmcp.fs.write" | "ssmcp.fs.write_text" | "ssmcp.fs.edit_text"
        );
        let read = matches!(name, "ssmcp.fs.read" | "ssmcp.fs.read_text");
        let before = if write {
            path.as_deref().and_then(|path| self.snapshot_text(path))
        } else {
            None
        };
        let result = self.perform(name, args)?;
        if let Some(path) = path {
            if write {
                let after = self.snapshot_text(&path);
                file_event(
                    "filechange",
                    "files",
                    name,
                    &path,
                    before.as_deref(),
                    after.as_deref(),
                );
            } else if read {
                file_event("fileread", "files", name, &path, None, None);
            }
        }
        Ok(result)
    }
}

fn file_event(
    kind: &str,
    harness: &str,
    tool: &str,
    path: &str,
    before: Option<&str>,
    after: Option<&str>,
) {
    let (diff, truncated) = if kind == "filechange" && (before.is_some() || after.is_some()) {
        unified_diff(path, before.unwrap_or(""), after.unwrap_or(""))
    } else {
        (String::new(), false)
    };
    crate::client::render::event(
        harness,
        &json!({
            "type":format!("ssmcp.{kind}"), "tool":tool, "path":path,
            "diff":diff, "diff_truncated":truncated,
            "preview_available":kind != "filechange" || before.is_some() || after.is_some()
        }),
    );
}

/// A single replacement hunk with context. Bounded display never alters the edit.
fn unified_diff(path: &str, before: &str, after: &str) -> (String, bool) {
    if before == after {
        return (String::new(), false);
    }
    let old: Vec<_> = before.lines().collect();
    let new: Vec<_> = after.lines().collect();
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
        if lines >= 160 || diff.len() + line.len() + 2 > 16 * 1024 {
            truncated = true;
            return;
        }
        diff.push(prefix);
        diff.push_str(line);
        diff.push('\n');
        lines += 1;
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
}
