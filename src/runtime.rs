//! Native backend plus connection-owned workspace utilities.
use crate::{
    backend::{self, Backend},
    context::Context,
    output::OutputStore,
    rsync::Rsync,
    wire::Operation,
    workspace::Workspace,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub(crate) const CATALOG_SNAPSHOT_SECONDS: u64 = 120;
pub(crate) const MAX_CATALOG_SNAPSHOTS: usize = 4;
pub(crate) const MAX_CATALOG_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

struct CatalogSnapshot {
    fingerprint: String,
    tools: Arc<Vec<Value>>,
    bytes: usize,
    created: Instant,
}

#[derive(Default)]
struct Catalog {
    snapshots: Mutex<VecDeque<CatalogSnapshot>>,
}

impl Catalog {
    fn first(&self, tools: Vec<Value>) -> Result<Value, String> {
        let page = backend::page(&tools, None)?;
        let Some(cursor) = page["nextCursor"].as_str() else {
            return Ok(page);
        };
        let fingerprint = cursor.split_once(':').unwrap().0.to_owned();
        let bytes = serde_json::to_vec(&tools).map_err(|e| e.to_string())?.len();
        if bytes > MAX_CATALOG_SNAPSHOT_BYTES {
            return Err("tool catalog exceeds the 64 MiB pagination snapshot limit".into());
        }
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        snapshots.retain(|snapshot| {
            snapshot.created.elapsed() < Duration::from_secs(CATALOG_SNAPSHOT_SECONDS)
                && snapshot.fingerprint != fingerprint
        });
        let mut retained: usize = snapshots.iter().map(|snapshot| snapshot.bytes).sum();
        while snapshots.len() >= MAX_CATALOG_SNAPSHOTS
            || retained + bytes > MAX_CATALOG_SNAPSHOT_BYTES
        {
            if let Some(evicted) = snapshots.pop_front() {
                retained -= evicted.bytes;
            }
        }
        snapshots.push_back(CatalogSnapshot {
            fingerprint,
            tools: Arc::new(tools),
            bytes,
            created: Instant::now(),
        });
        Ok(page)
    }

    fn next(&self, cursor: &str) -> Result<Value, String> {
        self.next_at(cursor, Instant::now())
    }

    fn next_at(&self, cursor: &str, now: Instant) -> Result<Value, String> {
        let (fingerprint, _) = cursor.split_once(':').ok_or("invalid tool cursor")?;
        let tools = {
            let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
            snapshots.retain(|snapshot| {
                now.saturating_duration_since(snapshot.created)
                    < Duration::from_secs(CATALOG_SNAPSHOT_SECONDS)
            });
            snapshots
                .iter()
                .find(|snapshot| snapshot.fingerprint == fingerprint)
                .map(|snapshot| snapshot.tools.clone())
                .ok_or("tool catalog snapshot expired or was evicted; restart tools/list without a cursor")?
        };
        backend::page_with_fingerprint(tools.as_ref(), Some(cursor), fingerprint)
    }
}

pub struct Runtime {
    backend: Arc<Backend>,
    workspace: Workspace,
    context: Context,
    rsync: Rsync,
    output: Arc<OutputStore>,
    catalog: Catalog,
}

impl Runtime {
    pub fn new(backend: Arc<Backend>, root: PathBuf) -> Result<Arc<Self>, String> {
        Self::with_context_roots(backend, root, Vec::new())
    }

    pub fn with_context_roots(
        backend: Arc<Backend>,
        root: PathBuf,
        context_roots: Vec<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            backend,
            workspace: Workspace::new(root.clone())?,
            context: Context::new(root.clone(), context_roots)?,
            rsync: Rsync::new(root)?,
            output: OutputStore::global()?,
            catalog: Catalog::default(),
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = self.backend.tools().await?;
        tools.extend(Workspace::tools());
        tools.extend(Context::tools());
        tools.extend(Rsync::tools());
        tools.extend(OutputStore::tools());
        tools.push(crate::client::reload::tool());
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
            if !names.insert(tool["name"].as_str().ok_or("tool is missing a name")?) {
                return Err("native backend conflicts with a reserved workspace tool name".into());
            }
        }
        Ok(tools)
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            // Freeze the merged native and workspace directory for this scan.
            // A fresh scan may observe native tools/list_changed; continuation
            // pages must not rediscover it and invalidate their own cursor.
            Operation::ListTools { cursor: None } => self.catalog.first(self.tools().await?),
            Operation::ListTools {
                cursor: Some(cursor),
            } => self.catalog.next(&cursor),
            Operation::CallTool { name, arguments }
                if name.starts_with("ssmcp.fs.") || name.starts_with("ssmcp.net.") =>
            {
                self.workspace
                    .perform(&name, arguments)
                    .await
                    .map(backend::result)
            }
            Operation::CallTool { name, arguments }
                if name.starts_with("ssmcp.skills.")
                    || name.starts_with("ssmcp.agents.")
                    || name.starts_with("ssmcp.context.") =>
            {
                let skill = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let result = self.context.perform(&name, arguments).await?;
                if name.starts_with("ssmcp.skills.") {
                    self.workspace
                        .audit_harness_skill("context", &name, skill.as_deref());
                }
                Ok(backend::result(result))
            }
            Operation::CallTool { name, arguments } if name.starts_with("ssmcp.rsync.") => self
                .rsync
                .perform(&name, arguments)
                .await
                .map(backend::result),
            Operation::CallTool { name, arguments } if name.starts_with("ssmcp.output.") => self
                .output
                .perform(&name, arguments)
                .await
                .map(backend::result),
            operation if crate::client::reload::is_reload(&operation) => {
                Err("reload requires a connected foreground client".into())
            }
            // PTY output is guarded at its streaming source, before its ring can
            // discard bytes. Preserve short cursor reads without re-encoding them.
            operation @ Operation::Read { .. } => self.backend.perform(operation).await,
            Operation::CallTool { name, arguments }
                if name == "terminal.read" || name == "ssmcp.exec.read" =>
            {
                self.backend
                    .perform(Operation::CallTool { name, arguments })
                    .await
            }
            Operation::CallTool { name, arguments } => {
                let lower = name.to_ascii_lowercase();
                let harness = if lower.starts_with("codex.") || lower.starts_with("ssmcp.codex.") {
                    "codex"
                } else if lower.starts_with("cc.") || lower.starts_with("ssmcp.cc.") {
                    "claude-code"
                } else if lower.starts_with("pi.") || lower.starts_with("ssmcp.pi.") {
                    "pi"
                } else if lower.starts_with("opencode.") {
                    "opencode"
                } else {
                    "native"
                };
                if lower.contains("skill") {
                    let skill = ["skill", "skill_name", "skillName", "name"]
                        .iter()
                        .find_map(|k| arguments.get(*k).and_then(Value::as_str).map(str::to_owned));
                    let result = self
                        .backend
                        .perform(Operation::CallTool {
                            name: name.clone(),
                            arguments,
                        })
                        .await?;
                    self.workspace
                        .audit_harness_skill(harness, &name, skill.as_deref());
                    return Ok(self.output.guard_result(result));
                }
                let path = ["file_path", "filePath", "path", "file", "target"]
                    .iter()
                    .find_map(|k| arguments.get(*k).and_then(Value::as_str).map(str::to_owned));
                let is_read = path.is_some() && (lower.contains("read") || lower.ends_with(".get"));
                let is_edit = path.is_some()
                    && (lower.contains("edit")
                        || lower.contains("write")
                        || lower.contains("patch")
                        || lower.contains("replace"));
                let before = if is_edit {
                    self.workspace.snapshot_file(path.as_deref().unwrap()).await
                } else {
                    None
                };
                let result = self
                    .backend
                    .perform(Operation::CallTool {
                        name: name.clone(),
                        arguments,
                    })
                    .await?;
                if let Some(path) = path.as_deref() {
                    if is_read {
                        self.workspace
                            .audit_harness_file("fileread", harness, &name, path, None, None);
                    }
                    if is_edit {
                        let after = self.workspace.snapshot_file(path).await;
                        self.workspace.audit_harness_file(
                            "filechange",
                            harness,
                            &name,
                            path,
                            before.as_deref(),
                            after.as_deref(),
                        );
                    }
                }
                Ok(self.output.guard_result(result))
            }
            operation => self
                .backend
                .perform(operation)
                .await
                .map(|value| self.output.guard_result(value)),
        }
    }

    pub async fn shutdown(&self) {
        tokio::join!(
            self.workspace.shutdown(),
            self.rsync.shutdown(),
            self.backend.shutdown()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools(generation: usize) -> Vec<Value> {
        (0..150)
            .map(|n| json!({"name":format!("tool{n}"),"inputSchema":{"type":"object"},"generation":generation}))
            .collect()
    }

    #[test]
    fn concurrent_catalog_scans_keep_their_original_generation_until_the_last_page() {
        let catalog = Catalog::default();
        let first = catalog.first(tools(1)).unwrap();
        let newer = catalog.first(tools(2)).unwrap();
        let second = catalog.next(first["nextCursor"].as_str().unwrap()).unwrap();
        assert_eq!(second["tools"][0]["name"], "tool64");
        assert_eq!(second["tools"][0]["generation"], 1);
        let newer_second = catalog.next(newer["nextCursor"].as_str().unwrap()).unwrap();
        assert_eq!(newer_second["tools"][0]["generation"], 2);
        let last = catalog
            .next(second["nextCursor"].as_str().unwrap())
            .unwrap();
        assert_eq!(last["tools"].as_array().unwrap().len(), 22);
        assert!(
            last["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["generation"] == 1)
        );
        assert!(last.get("nextCursor").is_none());
    }

    #[test]
    fn expired_or_evicted_catalog_cursors_require_a_new_scan() {
        let catalog = Catalog::default();
        let first = catalog.first(tools(0)).unwrap();
        for generation in 1..=MAX_CATALOG_SNAPSHOTS {
            catalog.first(tools(generation)).unwrap();
        }
        assert_eq!(
            catalog.snapshots.lock().unwrap().len(),
            MAX_CATALOG_SNAPSHOTS
        );
        assert!(
            catalog
                .next(first["nextCursor"].as_str().unwrap())
                .unwrap_err()
                .contains("restart tools/list")
        );
        let latest = catalog.first(tools(99)).unwrap();
        assert!(
            catalog
                .next_at(
                    latest["nextCursor"].as_str().unwrap(),
                    Instant::now() + Duration::from_secs(CATALOG_SNAPSHOT_SECONDS + 1),
                )
                .unwrap_err()
                .contains("expired")
        );
        assert!(catalog.next("invalid").is_err());
    }
}
