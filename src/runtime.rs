//! Registry-driven native calls and utilities scoped to one authenticated connection.
mod capabilities;
use crate::rsync::Rsync;
use crate::{
    backend::{self, Backend, events::EventLog},
    browser::Browser,
    computer::Computer,
    context::Context,
    omarchy::Omarchy,
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
pub(crate) const CATALOG_SNAPSHOT_MAX_SECONDS: u64 = 15 * 60;
pub(crate) const MAX_CATALOG_SNAPSHOTS: usize = 4;
pub(crate) const MAX_CATALOG_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

struct CatalogSnapshot {
    fingerprint: String,
    tools: Arc<Vec<Value>>,
    bytes: usize,
    created: Instant,
    accessed: Instant,
}

impl CatalogSnapshot {
    fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.accessed)
            >= Duration::from_secs(CATALOG_SNAPSHOT_SECONDS)
            || now.saturating_duration_since(self.created)
                >= Duration::from_secs(CATALOG_SNAPSHOT_MAX_SECONDS)
    }
}

#[derive(Default)]
struct Catalog {
    snapshots: Mutex<VecDeque<CatalogSnapshot>>,
}

impl Catalog {
    fn first(&self, tools: Vec<Value>) -> Result<Value, String> {
        self.first_at(tools, Instant::now())
    }

    fn first_at(&self, tools: Vec<Value>, now: Instant) -> Result<Value, String> {
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
        snapshots.retain(|snapshot| !snapshot.expired(now));
        if let Some(index) = snapshots
            .iter()
            .position(|snapshot| snapshot.fingerprint == fingerprint)
        {
            let mut snapshot = snapshots.remove(index).unwrap();
            snapshot.accessed = now;
            snapshots.push_back(snapshot);
            return Ok(page);
        }
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
            created: now,
            accessed: now,
        });
        Ok(page)
    }

    fn next(&self, cursor: &str) -> Result<Value, String> {
        self.next_at(cursor, Instant::now())
    }

    fn next_at(&self, cursor: &str, now: Instant) -> Result<Value, String> {
        let (fingerprint, _) = cursor.split_once(':').ok_or("invalid tool cursor")?;
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        snapshots.retain(|snapshot| !snapshot.expired(now));
        let index = snapshots
            .iter()
            .position(|snapshot| snapshot.fingerprint == fingerprint)
            .ok_or(
                "tool catalog snapshot expired or was evicted; restart tools/list without a cursor",
            )?;
        // Renew only after validating the complete cursor. A steady multi-page
        // read must not expire at its creation time, while malformed cursors
        // must not pin cache entries. The absolute age and byte/count limits
        // still bound retained snapshots, independently of current call routing.
        let page = backend::page_with_fingerprint(
            snapshots[index].tools.as_ref(),
            Some(cursor),
            fingerprint,
        )?;
        let mut snapshot = snapshots.remove(index).unwrap();
        snapshot.accessed = now;
        snapshots.push_back(snapshot);
        Ok(page)
    }
}

pub struct Runtime {
    backend: Arc<Backend>,
    registry: Arc<Registry>,
    workspace: Workspace,
    browser: Browser,
    computer: Computer,
    omarchy: Omarchy,
    context: Context,
    rsync: Rsync,
    output: Arc<OutputStore>,
    audit: Arc<EventLog>,
    next_call: AtomicU64,
    catalog: Catalog,
    sessions: Option<crate::session::control::Control>,
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
        Self::with_session_control(backend, root, context_roots, None)
    }
    pub fn with_session_control(
        backend: Arc<Backend>,
        root: PathBuf,
        context_roots: Vec<PathBuf>,
        sessions: Option<crate::session::control::Control>,
    ) -> Result<Arc<Self>, String> {
        let output = backend.output();
        let registry = backend.registry.clone();
        let workspace = Workspace::with_output(root.clone(), output.clone())?;
        let context = Context::with_output(root.clone(), context_roots, output.clone())?;
        let browser = Browser::new(root.clone())?;
        let computer = Computer::new(root.clone())?;
        let omarchy = Omarchy::new(root.clone())?;
        let rsync = Rsync::new(root)?;
        registry.replace(Provider::Client, capabilities::tools())?;
        registry.replace(Provider::Workspace, Workspace::tools())?;
        registry.replace(Provider::Browser, Browser::tools())?;
        registry.replace(Provider::Computer, Computer::tools())?;
        registry.replace(Provider::Omarchy, Omarchy::tools())?;
        registry.replace(Provider::Context, Context::tools())?;
        registry.replace(Provider::Rsync, Rsync::tools())?;
        registry.replace(Provider::Output, OutputStore::tools())?;
        registry.replace(Provider::Reload, vec![crate::client::reload::tool()])?;
        if sessions.is_some() {
            registry.replace(
                Provider::Sessions,
                crate::session::control::Control::tools(),
            )?;
        }
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
            browser,
            computer,
            omarchy,
            context,
            rsync,
            audit: EventLog::with_output("audit", output.clone()),
            output,
            next_call: AtomicU64::new(1),
            catalog: Catalog::default(),
            sessions,
        }))
    }
    pub fn output(&self) -> Arc<OutputStore> {
        self.output.clone()
    }
    pub fn terminal(&self) -> Arc<crate::pty::Terminal> {
        self.backend.terminal()
    }
    pub fn terminals(&self) -> Arc<crate::terminals::Terminals> {
        self.backend.terminals()
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
        let harness_call = tool
            .provider
            .is_harness()
            .then(|| backend::rpc::HarnessCall::new(call_id));
        let command_rpc_expected = tool.provider == Provider::Codex
            && (tool.native_name == "codex.command_exec"
                || (tool.native_name == "codex.rpc" && arguments["method"] == "command/exec"));
        if tool.provider.is_harness() {
            self.audit.push(
                json!({"type":"ssmcp.harness.tool.started","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation,
                "rpc_correlation":true,"command_rpc_expected":command_rpc_expected,
                "file_audit":"unknown","approval":"native"}),
            );
        }
        let control = matches!(
            tool.provider,
            Provider::Browser
                | Provider::Computer
                | Provider::Omarchy
                | Provider::Sessions
                | Provider::Playwright
        );
        if control {
            self.audit
                .push(json!({"type":"ssmcp.control.started","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation}));
        }
        let skill_path = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let outcome = match tool.provider {
            Provider::Client => self
                .client_resource_call(&tool.native_name, arguments)
                .await
                .map(backend::result),
            Provider::Browser => self.browser.perform(&tool.native_name, arguments).await,
            Provider::Computer => self.computer.perform(&tool.native_name, arguments).await,
            Provider::Omarchy => self.omarchy.perform(&tool.native_name, arguments).await,
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
            Provider::Sessions => match &self.sessions {
                Some(control) => control
                    .perform(&tool.native_name, arguments)
                    .await
                    .map(backend::result),
                None => Err("session management was not granted with --session-control".into()),
            },
            Provider::Audit => self.audit.read(arguments).await.map(backend::result),
            _ => {
                let dispatch = self.backend.dispatch(&tool, arguments);
                if let Some(call) = &harness_call {
                    call.scope(dispatch).await
                } else {
                    dispatch.await
                }
            }
        };
        if tool.provider.is_harness() {
            self.audit.push(
                json!({"type":"ssmcp.harness.tool.completed","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation,
                "duration_ms":started.elapsed().as_millis() as u64,
                "success":outcome.as_ref().is_ok_and(|result|result["isError"]!=true),
                "rpc_correlation":true,"command_rpc_expected":command_rpc_expected,
                "command_rpc_started":harness_call.as_ref().is_some_and(|call|call.command_started()),
                "command_rpc_completed":harness_call.as_ref().is_some_and(|call|call.command_completed()),
                "file_audit":"unknown","approval":"native"}),
            );
        }
        if control {
            self.audit
                .push(json!({"type":"ssmcp.control.completed","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,
                "duration_ms":started.elapsed().as_millis() as u64,
                "success":outcome.as_ref().is_ok_and(|result| result["isError"] != true)}));
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
                            "root":skill.get("root").unwrap_or(&native["root"]),"path":skill["path"],"name":skill["name"],
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
        // Context already enforces a 1 MiB response and 256 KiB full-document limit.
        // Keep structured contents available to resources/read instead of spilling them.
        if matches!(tool.provider, Provider::Context | Provider::Client)
            || matches!(
                name.as_str(),
                "ssmcp.output.read" | "terminal.read" | "ssmcp.exec.read"
            )
        {
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
        let (_, _, computer, _, _, _) = tokio::join!(
            self.workspace.shutdown(),
            self.browser.shutdown(),
            self.computer.shutdown(),
            self.omarchy.shutdown(),
            self.rsync.shutdown(),
            self.backend.shutdown()
        );
        if let Err(error) = computer {
            // EventLog redacts, renders and retains the failure before closing.
            self.audit.push(json!({
                "type":"ssmcp.control.cleanup_failed",
                "provider":"computer",
                "cleanup_complete":false,
                "error":error
            }));
        }
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

    #[test]
    fn active_large_catalog_scan_renews_idle_expiry_without_changing_its_tools() {
        let catalog = Catalog::default();
        let created = Instant::now();
        let definitions: Vec<_> = (0..1342)
            .map(|n| json!({"name":format!("tool{n}"),"inputSchema":{"type":"object"},"generation":1}))
            .collect();
        let mut page = catalog.first_at(definitions, created).unwrap();
        let mut seen = page["tools"].as_array().unwrap().len();
        let mut elapsed = 0;
        while let Some(cursor) = page["nextCursor"].as_str() {
            elapsed += 30;
            // Publish a different current view after the original 120-second
            // creation deadline; it must not prune the actively read old view.
            if elapsed == 150 {
                catalog
                    .first_at(tools(2), created + Duration::from_secs(elapsed))
                    .unwrap();
            }
            page = catalog
                .next_at(cursor, created + Duration::from_secs(elapsed))
                .unwrap();
            let tools = page["tools"].as_array().unwrap();
            assert!(tools.iter().all(|tool| tool["generation"] == 1));
            seen += tools.len();
        }
        assert!(elapsed > CATALOG_SNAPSHOT_SECONDS);
        assert_eq!(seen, 1342);
    }

    #[test]
    fn snapshot_capacity_evicts_the_least_recent_successful_reader() {
        let catalog = Catalog::default();
        let created = Instant::now();
        let mut pages = Vec::new();
        for generation in 0..MAX_CATALOG_SNAPSHOTS {
            pages.push(catalog.first_at(tools(generation), created).unwrap());
        }
        let active = pages[0]["nextCursor"].as_str().unwrap();
        catalog
            .next_at(active, created + Duration::from_secs(60))
            .unwrap();
        catalog
            .first_at(tools(99), created + Duration::from_secs(61))
            .unwrap();
        assert!(
            catalog
                .next_at(active, created + Duration::from_secs(62))
                .is_ok()
        );
        assert!(
            catalog
                .next_at(
                    pages[1]["nextCursor"].as_str().unwrap(),
                    created + Duration::from_secs(62),
                )
                .is_err()
        );
        assert_eq!(
            catalog.snapshots.lock().unwrap().len(),
            MAX_CATALOG_SNAPSHOTS
        );
    }

    #[test]
    fn invalid_continuations_do_not_extend_snapshot_lifetime() {
        let catalog = Catalog::default();
        let created = Instant::now();
        let page = catalog.first_at(tools(0), created).unwrap();
        let cursor = page["nextCursor"].as_str().unwrap();
        let fingerprint = cursor.split_once(':').unwrap().0;
        assert!(
            catalog
                .next_at(
                    &format!("{fingerprint}:invalid"),
                    created + Duration::from_secs(CATALOG_SNAPSHOT_SECONDS - 1),
                )
                .is_err()
        );
        assert!(
            catalog
                .next_at(
                    cursor,
                    created + Duration::from_secs(CATALOG_SNAPSHOT_SECONDS)
                )
                .unwrap_err()
                .contains("expired")
        );
    }

    #[test]
    fn repeated_valid_pages_cannot_retain_a_snapshot_past_its_absolute_age() {
        let catalog = Catalog::default();
        let created = Instant::now();
        let page = catalog.first_at(tools(0), created).unwrap();
        let cursor = page["nextCursor"].as_str().unwrap();
        for elapsed in (60..CATALOG_SNAPSHOT_MAX_SECONDS).step_by(60) {
            catalog
                .next_at(cursor, created + Duration::from_secs(elapsed))
                .unwrap();
        }
        assert!(
            catalog
                .next_at(
                    cursor,
                    created + Duration::from_secs(CATALOG_SNAPSHOT_MAX_SECONDS),
                )
                .unwrap_err()
                .contains("expired")
        );
    }
}
