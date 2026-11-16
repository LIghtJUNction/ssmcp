//! Registry-driven native calls and utilities scoped to one authenticated connection.
use crate::rsync::Rsync;
use crate::{
    backend::{self, Backend, events::EventLog},
    context::Context,
    output::OutputStore,
    registry::{Provider, Registry},
    wire::Operation,
    workspace::Workspace,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
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
    registry: Arc<Registry>,
    workspace: Workspace,
    context: Context,
    rsync: Rsync,
    output: Arc<OutputStore>,
    audit: Arc<EventLog>,
    next_call: AtomicU64,
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
        let output = backend.output();
        let registry = backend.registry.clone();
        let workspace = Workspace::with_output(root.clone(), output.clone())?;
        let context = Context::with_output(root.clone(), context_roots, output.clone())?;
        let rsync = Rsync::new(root)?;
        registry.replace(Provider::Workspace, Workspace::tools())?;
        registry.replace(Provider::Context, Context::tools())?;
        registry.replace(Provider::Rsync, Rsync::tools())?;
        registry.replace(Provider::Output, OutputStore::tools())?;
        registry.replace(Provider::Reload, vec![crate::client::reload::tool()])?;
        registry.replace(Provider::Audit,vec![json!({
            "name":"ssmcp.audit.events",
            "description":"Read observed tool and skill events for this connection. File operations performed by external harnesses have unknown audit coverage unless their native protocol reports them.",
            "inputSchema":crate::backend::events::schema(),
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        })])?;
        Ok(Arc::new(Self {
            backend,
            registry,
            workspace,
            context,
            rsync,
            audit: EventLog::with_output("audit", output.clone()),
            output,
            next_call: AtomicU64::new(1),
            catalog: Catalog::default(),
        }))
    }
    pub fn output(&self) -> Arc<OutputStore> {
        self.output.clone()
    }
    pub fn terminal(&self) -> Arc<crate::pty::Terminal> {
        self.backend.terminal()
    }
    pub async fn wait_discovery(&self) {
        self.backend.wait_discovery().await
    }
    pub fn validate_reload(&self) -> Result<(), String> {
        let tool = self.registry.lookup(crate::client::reload::TOOL)?;
        if tool.provider != Provider::Reload {
            return Err("reload provider mismatch".into());
        }
        self.registry.validate(&tool)
    }
    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        self.registry.catalog()
    }
    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        self.registry.ensure_open()?;
        let (name, arguments) = match operation {
            // Continuations retain an immutable view of the one registry. Calls
            // still validate the current provider and registration generation.
            Operation::ListTools { cursor: None } => {
                return self.catalog.first(self.registry.catalog()?);
            }
            Operation::ListTools {
                cursor: Some(cursor),
            } => {
                return self.catalog.next(&cursor);
            }
            Operation::CallTool { name, arguments } => (name, arguments),
            Operation::Read {
                cursor,
                max_bytes,
                wait_ms,
            } => (
                "terminal.read".into(),
                json!({"cursor":cursor,"max_bytes":max_bytes,"wait_ms":wait_ms}),
            ),
            Operation::Write { data } => ("terminal.write".into(), json!({"data":data})),
            Operation::Resize { rows, cols } => {
                ("terminal.resize".into(), json!({"rows":rows,"cols":cols}))
            }
            Operation::Status {} => ("terminal.status".into(), json!({})),
        };
        let tool = self.registry.lookup(&name)?;
        self.registry.validate(&tool)?;
        let call_id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        if tool.provider.is_harness() {
            self.audit.push(
                json!({"type":"ssmcp.harness.tool.started","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation,
                "file_audit":"unknown","approval":"native"}),
            );
        }
        let skill_path = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let outcome = match tool.provider {
            Provider::Workspace => self
                .workspace
                .perform(&tool.native_name, arguments)
                .await
                .map(backend::result),
            Provider::Context => self
                .context
                .perform(&tool.native_name, arguments)
                .await
                .map(backend::result),
            Provider::Rsync => self
                .rsync
                .perform(&tool.native_name, arguments)
                .await
                .map(backend::result),
            Provider::Output => self
                .output
                .perform(&tool.native_name, arguments)
                .await
                .map(backend::result),
            Provider::Reload => Err("reload requires an acknowledged foreground connection".into()),
            Provider::Audit => self.audit.read(arguments).await.map(backend::result),
            _ => self.backend.dispatch(&tool, arguments).await,
        };
        if tool.provider.is_harness() {
            self.audit.push(
                json!({"type":"ssmcp.harness.tool.completed","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation,
                "duration_ms":started.elapsed().as_millis() as u64,
                "success":outcome.as_ref().is_ok_and(|result|result["isError"]!=true),
                "file_audit":"unknown","approval":"native"}),
            );
        }
        if let Ok(value) = &outcome
            && value["isError"] != true
            && tool.provider == Provider::Context
        {
            let native = &value["structuredContent"];
            if name == "ssmcp.skills.list" {
                if let Some(skills) = native["skills"].as_array() {
                    for skill in skills {
                        self.audit
                            .push(json!({"type":"ssmcp.skill.discovered","tool":name,
                            "root":native["root"],"path":skill["path"],"name":skill["name"],
                            "scope":"ssmcp context operation","invoked":false}));
                    }
                }
            } else if name == "ssmcp.skills.view" {
                self.audit
                    .push(json!({"type":"ssmcp.skill.discovery.completed","tool":name,
                    "root":native["root"],"shown":native["shown"],"invoked":false}));
            } else if name == "ssmcp.skills.read" {
                self.audit.push(
                    json!({"type":"ssmcp.skill.read","tool":name,"root":native["root"],
                    "path":native.get("path").cloned().unwrap_or_else(||json!(skill_path)),
                    "scope":"ssmcp context operation","invoked":false}),
                );
            }
        }
        let value = match outcome {
            Ok(value) => value,
            Err(error) => {
                if let Some(native) = error
                    .strip_prefix("native RPC error: ")
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                {
                    json!({"isError":true,"content":[],"nativeError":native})
                } else {
                    return Err(error);
                }
            }
        };
        // Retrieval pages already point to the original complete capture. Guarding
        // them again would recursively archive the artifact-read response itself.
        if matches!(
            name.as_str(),
            "ssmcp.output.read" | "terminal.read" | "ssmcp.exec.read"
        ) {
            Ok(value)
        } else {
            Ok(self.output.guard_result(value))
        }
    }
    pub async fn shutdown(&self) {
        self.registry.close();
        self.catalog
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        tokio::join!(
            self.workspace.shutdown(),
            self.rsync.shutdown(),
            self.backend.shutdown()
        );
        self.audit.close();
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
