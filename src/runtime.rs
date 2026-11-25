//! Registry-driven native calls and utilities scoped to one authenticated connection.
pub(crate) mod call_scope;
mod capabilities;
pub mod hooks;
pub(crate) mod native_host;
pub(crate) mod native_plugins;
mod plugins;
pub(crate) mod time;
use crate::catalog::{self, ToolDescriptor};
use crate::{
    backend::{self, Backend, events::EventLog},
    output::OutputStore,
    registry::{Provider, RegisteredTool, Registry},
    wire::Operation,
};
use native_host::{ConstructorContext, Definition, NativeHost, NativePlugin};
use native_plugins::{desktop::Desktop, files::Files};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const CATALOG_SNAPSHOT_SECONDS: u64 = 120;
pub(crate) const CATALOG_SNAPSHOT_MAX_SECONDS: u64 = 15 * 60;
pub(crate) const MAX_CATALOG_SNAPSHOTS: usize = 4;
pub(crate) const MAX_CATALOG_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_CODE_AUDIT_RUNS: usize = 256;

/// Source is recorded once per run, alongside native-call outcomes. The run's
/// final result is supplied separately by the orchestration server.
struct CodeAudit {
    log: Arc<EventLog>,
    runs: Mutex<VecDeque<(String, bool)>>,
}

impl CodeAudit {
    fn new(log: Arc<EventLog>) -> Self {
        Self {
            log,
            runs: Mutex::new(VecDeque::new()),
        }
    }

    fn begin(
        &self,
        operation: &Operation,
        request_id: u64,
        controller: Option<&crate::wire::Controller>,
        attribution: Option<&crate::wire::ControlAttribution>,
        context: Option<crate::wire::CodeContext>,
    ) -> Result<Option<CodeCall>, String> {
        let Some(context) = context else {
            return Ok(None);
        };
        context.validate()?;
        let mut runs = self.runs.lock().unwrap_or_else(|error| error.into_inner());
        if !runs.iter().any(|(id, _)| id == &context.run_id) {
            if runs.len() >= MAX_CODE_AUDIT_RUNS {
                runs.pop_front();
            }
            runs.push_back((context.run_id.clone(), false));
            self.log
                .push_control(json!({"type":"ssmcp.code_mode","phase":"started",
                "run_id":context.run_id,"code":context.code,
                "timestamp_ms":context.timestamp_ms,"controller":controller}));
        }
        drop(runs);
        let tool = operation.name().to_owned();
        self.log
            .push_control(json!({"type":"ssmcp.code_mode","phase":"call-started",
            "scope":"native-call","run_id":context.run_id,"request_id":request_id,"tool":tool,"attribution":attribution}));
        Ok(Some(CodeCall {
            log: self.log.clone(),
            run_id: context.run_id,
            request_id,
            tool,
            attribution: attribution.cloned(),
            started: Instant::now(),
            finished: false,
        }))
    }

    fn finish(&self, completion: crate::wire::CodeCompletion) -> Result<(), String> {
        completion.validate()?;
        let mut runs = self.runs.lock().unwrap_or_else(|error| error.into_inner());
        let Some((_, finished)) = runs.iter_mut().find(|(id, _)| id == &completion.run_id) else {
            return Ok(());
        };
        if *finished {
            return Ok(());
        }
        *finished = true;
        self.log
            .push_control(json!({"type":"ssmcp.code_mode","phase":"finished",
            "run_id":completion.run_id,"success":completion.success,"status":completion.status,
            "duration_ms":completion.duration_ms,"error":completion.error}));
        Ok(())
    }
}

pub(crate) struct CodeCall {
    log: Arc<EventLog>,
    run_id: String,
    request_id: u64,
    tool: String,
    attribution: Option<crate::wire::ControlAttribution>,
    started: Instant,
    finished: bool,
}

impl CodeCall {
    pub(crate) fn finish(&mut self, result: &Result<Value, String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let success = result.as_ref().is_ok_and(|value| value["isError"] != true);
        let mut event = json!({"type":"ssmcp.code_mode","phase":"call-finished",
            "scope":"native-call","run_id":self.run_id,"request_id":self.request_id,"tool":self.tool,
            "attribution":self.attribution,
            "success":success,"duration_ms":self.started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64});
        let error = match result {
            Err(error) => Some(error.as_str()),
            Ok(value) if !success => value["content"].as_array().and_then(|content| {
                content.iter().find_map(|item| {
                    (item["type"] == "text")
                        .then(|| item["text"].as_str())
                        .flatten()
                })
            }),
            _ => None,
        };
        if let Some(error) = error {
            let mut end = error.len().min(4096);
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            event["error"] = json!(&error[..end]);
        }
        if let Ok(value) = result {
            let native = &value["structuredContent"];
            for field in ["exit_code", "job_id", "state", "running"] {
                let value = native.get(field).or_else(|| native["snapshot"].get(field));
                if let Some(value) = value.filter(|value| {
                    !value.is_null()
                        && (value.is_number()
                            || value.is_boolean()
                            || value.as_str().is_some_and(|text| text.len() <= 256))
                }) {
                    event[field] = value.clone();
                }
            }
        }
        self.log.push_control(event);
    }
}

impl Drop for CodeCall {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(&Err(
                "native request cancelled before its outcome was received".into(),
            ));
        }
    }
}

struct CatalogSnapshot {
    fingerprint: String,
    tools: Arc<Vec<ToolDescriptor>>,
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
    fn first(&self, tools: Vec<ToolDescriptor>) -> Result<Value, String> {
        self.first_at(tools, Instant::now())
    }

    fn first_at(&self, tools: Vec<ToolDescriptor>, now: Instant) -> Result<Value, String> {
        let fingerprint = catalog::fingerprint(&tools);
        let page = catalog::page(&tools, None, &fingerprint)?;
        let Some(cursor) = page["nextCursor"].as_str() else {
            return Ok(page);
        };
        let fingerprint = cursor.split_once(':').unwrap().0.to_owned();
        let bytes = catalog::encoded_len(&tools);
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
        let page = catalog::page(snapshots[index].tools.as_ref(), Some(cursor), fingerprint)?;
        let mut snapshot = snapshots.remove(index).unwrap();
        snapshot.accessed = now;
        snapshots.push_back(snapshot);
        Ok(page)
    }
}

pub struct Runtime {
    backend: Arc<Backend>,
    registry: Arc<Registry>,
    native: NativeHost,
    time: Arc<time::Clock>,
    files: Arc<Files>,
    plugins: plugins::Plugins,
    output: Arc<OutputStore>,
    audit: Arc<EventLog>,
    code_audit: CodeAudit,
    call_connection: u64,
    client_id: OnceLock<String>,
    next_call: AtomicU64,
    tool_call_hook: Mutex<Option<Arc<dyn hooks::ToolCallHook>>>,
    catalog: Catalog,
    sessions: Option<crate::session::control::Control>,
    tool_root: PathBuf,
    workers: Mutex<Option<Arc<crate::worker::Pool>>>,
}
impl Runtime {
    pub(crate) fn local_command_stats(&self) -> crate::backend::shell::LocalCommandStats {
        self.backend.local_command_stats()
    }

    pub(crate) fn local_shell(&self) -> Arc<crate::backend::shell::Shell> {
        self.backend.terminals()
    }

    pub(crate) async fn safety_events(&self, cursor: u64, limit: usize) -> Result<Value, String> {
        self.files.safety_events(cursor, limit).await
    }

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
        let constructor = ConstructorContext {
            root: root.clone(),
            context_roots,
            output: output.clone(),
        };
        let files = Files::new(&constructor)?;
        let desktop = Desktop::new(&constructor)?;
        let time = time::Clock::new()?;
        let native = NativeHost::new(
            registry.clone(),
            vec![
                Definition::new(backend.clone()),
                Definition::new(files.clone()),
                Definition::new(desktop),
                Definition::new(time.clone()),
            ],
        )?;
        let plugins = plugins::Plugins::new(root.clone(), registry.clone());
        registry.replace(Provider::Client, capabilities::tools())?;
        for (provider, tools) in native.catalogs()? {
            // Time passivity is a server delivery guarantee. A new client must
            // not advertise it to an older, unnegotiated relay.
            if provider != Provider::Time {
                registry.replace(provider, tools)?;
            }
        }
        registry.replace(Provider::Plugins, plugins::Plugins::tools())?;
        registry.replace(Provider::Output, OutputStore::tools())?;
        registry.replace(Provider::Reload, vec![crate::client::reload::tool()])?;
        registry.replace(Provider::ManagedFiles, crate::managed_files::tools())?;
        if sessions.is_some() {
            registry.replace(
                Provider::Sessions,
                crate::session::control::Control::tools(),
            )?;
        }
        registry.replace(Provider::Audit,vec![json!({
            "name":"ssmcp.audit.events",
            "description":"Read observed tool and skill events for this connection after audit redaction. Wire v6 also includes code_mode source and run/native-call lifecycle outcomes. File operations performed by external harnesses have unknown audit coverage unless their native protocol reports them.",
            "inputSchema":crate::backend::events::schema(),
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        })])?;
        let audit = EventLog::with_output("audit", output.clone());
        let runtime = Arc::new(Self {
            backend,
            registry,
            native,
            time,
            files,
            plugins,
            code_audit: CodeAudit::new(audit.clone()),
            call_connection: call_scope::connection(),
            client_id: OnceLock::new(),
            audit,
            output,
            next_call: AtomicU64::new(1),
            tool_call_hook: Mutex::new(None),
            catalog: Catalog::default(),
            sessions,
            tool_root: root,
            workers: Mutex::new(None),
        });
        crate::client::interface::harness::bind(&runtime);
        crate::client::completion::bind(&runtime);
        runtime.time.bind();
        Ok(runtime)
    }

    /// Production startup opts in to exact installed skill sources and builtins.
    /// Runtime constructors themselves never inspect the caller's home directory.
    pub fn enable_default_skill_sources(&self) -> Result<(), String> {
        self.files.enable_default_skill_sources()
    }
    /// Schedule one cleanup scan for a live client session. The client retains
    /// and joins this handle, so reconnect/reload cannot leave detached scans.
    pub(crate) fn start_managed_file_cleanup(
        &self,
        trigger: &'static str,
    ) -> tokio::task::JoinHandle<()> {
        let root = self.tool_root.clone();
        let audit = self.audit.clone();
        #[cfg(target_os = "linux")]
        let shell = self.local_shell();
        tokio::spawn(async move {
            #[cfg(target_os = "linux")]
            shell.reclaim_released_workspaces().await;
            let result = tokio::task::spawn_blocking(move || {
                crate::managed_files::cleanup(&root, false, trigger)
            })
            .await;
            let warning = match result {
                Ok(Ok(summary)) => {
                    let recoveries = summary.recoveries.len() + summary.retention.recoveries.len();
                    let warning = if !summary.errors.is_empty()
                        || recoveries != 0
                        || summary.scan_truncated
                    {
                        Some(format!(
                            "Managed-file {trigger} cleanup kept unresolved files ({} errors, {recoveries} recoveries, scan_truncated={}); inspect ssmcp cleanup --list",
                            summary.errors.len(),
                            summary.scan_truncated,
                        ))
                    } else {
                        None
                    };
                    audit.push(json!({
                        "type": "ssmcp.files.cleanup",
                        "trigger": trigger,
                        "summary": summary,
                    }));
                    warning
                }
                Ok(Err(error)) => Some(format!("Managed-file {trigger} cleanup failed: {error}")),
                Err(error) => Some(format!(
                    "Managed-file {trigger} cleanup task failed: {error}"
                )),
            };
            if let Some(warning) = warning {
                audit.push(json!({
                    "type": "ssmcp.files.cleanup_warning",
                    "trigger": trigger,
                    "message": warning,
                }));
                crate::client::render::status("cleanup", "Warning", &warning);
            }
        })
    }

    /// Install an optional in-process call hook. The default is None.
    /// Each call keeps one snapshot; replacement affects subsequent calls.
    pub fn set_tool_call_hook(&self, hook: Option<Arc<dyn hooks::ToolCallHook>>) {
        *self
            .tool_call_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = hook;
    }

    pub(crate) fn overview_jobs(&self) -> Value {
        self.backend.overview_jobs()
    }

    pub(crate) fn local_registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }

    pub(crate) fn configure_chat(&self, enabled: bool) -> Result<(), String> {
        self.backend.configure_chat(enabled)
    }

    pub(crate) fn chat_enabled(&self) -> bool {
        self.backend.chat_enabled()
    }

    pub(crate) fn configure_timing(
        &self,
        time_tools: bool,
        wait_display: bool,
    ) -> Result<(), String> {
        self.time.configure(time_tools, wait_display);
        self.registry.replace(
            Provider::Time,
            if time_tools {
                time::tools()
            } else {
                Vec::new()
            },
        )?;
        Ok(())
    }

    pub(crate) fn configure_timing_version(
        &self,
        time_tools: bool,
        version: Option<crate::wire::wait_display::Version>,
    ) -> Result<(), String> {
        self.time.configure_version(time_tools, version);
        self.registry.replace(
            Provider::Time,
            if time_tools {
                time::tools()
            } else {
                Vec::new()
            },
        )?;
        Ok(())
    }

    pub(crate) fn wait_display(
        &self,
        snapshot: crate::wire::wait_display::Snapshot,
    ) -> Result<(), String> {
        self.time.relay_display(snapshot)
    }

    pub(crate) fn disconnect_timing(&self) {
        self.time.disconnect();
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
    pub(crate) fn validate_peer_ports(&self) -> Result<(), String> {
        // A direct stream cannot invoke a native call hook for each payload chunk.
        if self
            .tool_call_hook
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return Err("peer ports require the unhooked workspace network provider".into());
        }
        for name in [
            "ssmcp.net.listen",
            "ssmcp.net.accept",
            "ssmcp.net.connect",
            "ssmcp.net.read",
            "ssmcp.net.write",
            "ssmcp.net.shutdown",
            "ssmcp.net.close",
        ] {
            let tool = self.registry.lookup(name)?;
            if tool.provider != Provider::Workspace {
                return Err("workspace network provider unavailable".into());
            }
            self.registry.validate(&tool)?;
        }
        Ok(())
    }

    pub fn validate_reload(&self) -> Result<(), String> {
        if let Some(pool) = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            let state = pool.queue.snapshot();
            if state.working != 0 || state.queued != 0 {
                return Err("finish or cancel worker tasks and wait for owned cleanup before reloading this connection".into());
            }
        }
        let tool = self.registry.lookup(crate::client::reload::TOOL)?;
        if tool.provider != Provider::Reload {
            return Err("reload provider mismatch".into());
        }
        self.registry.validate(&tool)
    }
    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        self.registry.catalog()
    }
    /// Opaque runtime generation used to reject work retained across reconnects.
    pub(crate) fn connection_generation(&self) -> u64 {
        self.call_connection
    }

    /// The registered connection identity, never an identity supplied by a GUI.
    pub(crate) fn frontend_client_id(&self) -> Option<&str> {
        self.client_id.get().map(String::as_str)
    }

    pub(crate) fn configure_workers(
        &self,
        config: crate::worker::Config,
        agent: &crate::client::chat_io::Agent,
    ) -> Result<(), String> {
        let pool = crate::worker::Pool::new(config, agent, self.tool_root.clone())?;
        self.registry
            .replace(Provider::Worker, crate::worker::Pool::tools())?;
        *self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(pool);
        Ok(())
    }

    pub(crate) fn resize_workers(&self, total: usize) -> Result<Value, String> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or("this connection has no worker pool")?;
        pool.resize(total)
    }

    pub(crate) fn human_resize_workers(&self, total: usize) -> Result<Value, String> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or("this connection has no worker pool")?;
        pool.human_resize(total)
    }

    pub(crate) fn human_select_pending_retirement(&self, target: usize) -> Result<Value, String> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or("this connection has no worker pool")?;
        pool.human_select_pending_retirement(target, self.call_connection)
    }

    pub(crate) fn force_worker_retirements(
        &self,
        targets: &[crate::worker::Retirement],
    ) -> Result<Value, String> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or("this connection has no worker pool")?;
        pool.force_worker_retirements(targets)
    }

    pub(crate) fn worker_duration_notices(&self) -> Vec<String> {
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| {
                p.queue
                    .overwork_notices(std::time::Instant::now(), crate::worker::now_ms())
            })
            .unwrap_or_default()
    }

    /// Exact-runtime projection for a protected local frontend. This never uses
    /// the process-wide display cache or binds a remote worker manager.
    pub(crate) fn frontend_has_workers(&self) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
    }

    pub(crate) fn has_registered_workers(&self) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|p| p.queue.has_registered_workers())
    }

    pub(crate) fn reconcile_worker_roster(
        &self,
        sent: std::time::Instant,
        agents: &[crate::wire::SteeringAgent],
    ) {
        if let Some(pool) = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            pool.reconcile_roster(self.call_connection, sent, agents);
        }
    }

    pub(crate) fn frontend_workers(&self) -> Option<crate::worker::Snapshot> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()?;
        Some(pool.queue.snapshot())
    }

    /// A local input lease is checked by the attach adapter. The pool generation
    /// also prevents an old GUI selection from resizing a replacement pool.
    pub(crate) fn frontend_resize_workers(
        &self,
        generation: &str,
        total: usize,
    ) -> Result<Value, String> {
        let pool = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .ok_or("this connection has no worker pool")?;
        if pool.generation != generation {
            return Err("worker pool changed; refresh worker state before resizing".into());
        }
        let mut result = pool.resize(total)?;
        if let Some(worker) = result.get_mut("worker").and_then(Value::as_object_mut) {
            // The receipt retains real counts and pending retirement. Slot logs
            // remain in their native source and are read through bounded state.
            worker.remove("slots");
            worker.insert("slots_omitted".into(), Value::Bool(true));
        }
        Ok(result)
    }

    pub(crate) fn start_workers(&self) {
        if let Some(pool) = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        {
            pool.start();
        }
    }

    pub(crate) fn stop_workers(&self) {
        if let Some(pool) = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            pool.queue.close(crate::worker::now_ms());
        }
    }

    pub(crate) fn bind_client_id(&self, client_id: &str) -> Result<(), String> {
        if client_id.len() != 64 || !client_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("invalid registered client identity".into());
        }
        if let Some(previous) = self.client_id.get() {
            return if previous == client_id {
                Ok(())
            } else {
                Err("native connection identity is already bound".into())
            };
        }
        self.client_id
            .set(client_id.to_owned())
            .map_err(|_| "native connection identity is already bound".to_owned())
    }

    /// Accept only metadata supplied by the authenticated relay frame, before
    /// entering a provider. Tool arguments cannot create this scope.
    pub(crate) fn call_scope(
        &self,
        request_id: u64,
        attribution: Option<&crate::wire::ControlAttribution>,
        context: Option<&crate::wire::CodeContext>,
    ) -> Result<Arc<call_scope::CallScope>, String> {
        call_scope::CallScope::from_relay_for_client(
            self.call_connection,
            self.client_id.get().map(String::as_str),
            request_id,
            attribution,
            context,
        )
    }

    pub(crate) fn relay_call_scope(
        &self,
        request_id: u64,
        attribution: Option<&crate::wire::ControlAttribution>,
        context: Option<&crate::wire::CodeContext>,
        openai: Option<crate::wire::openai::OpenAiContext>,
    ) -> Result<
        (
            Arc<call_scope::CallScope>,
            Option<call_scope::OpenAiObservation>,
        ),
        String,
    > {
        let scope = self.call_scope(request_id, attribution, context)?;
        let observation = openai
            .zip(context)
            .and_then(|(openai, context)| scope.openai_observation(openai, context.timestamp_ms));
        Ok((scope, observation))
    }

    pub(crate) fn begin_code_call(
        &self,
        operation: &Operation,
        request_id: u64,
        controller: Option<&crate::wire::Controller>,
        attribution: Option<&crate::wire::ControlAttribution>,
        context: Option<crate::wire::CodeContext>,
    ) -> Result<Option<CodeCall>, String> {
        self.code_audit
            .begin(operation, request_id, controller, attribution, context)
    }
    pub(crate) fn code_mode_finished(
        &self,
        completion: crate::wire::CodeCompletion,
    ) -> Result<(), String> {
        self.code_audit.finish(completion)
    }
    pub(crate) async fn prepare_references(
        &self,
        text: &str,
    ) -> Result<crate::client::references::PreparedReferences, String> {
        self.files.prepare_references(text).await
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(crate) async fn expand_references(&self, text: &str) -> Result<String, String> {
        self.files.expand_references(text).await
    }

    pub(crate) async fn reference_completions(
        &self,
        key: &crate::client::completion::Key,
    ) -> Result<crate::client::completion::Listing, String> {
        self.files.reference_completions(key).await
    }

    pub(crate) async fn import_input(
        &self,
        source: crate::client::attachments::Source,
    ) -> Result<crate::client::attachments::Loaded, String> {
        self.files.import_input(source).await
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        let source = call_scope::Source::capture();
        if let Some(pool) = self
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            pool.authenticated_contact(&source);
        }
        // Only ordinary work reads or execution provide a reporting opportunity.
        // Internal UI, time and reminder probes stay passive.
        let work_call = matches!(&operation, Operation::CallTool { name, .. }
            if crate::worker::progress_opportunity(name));
        let reported = match &operation {
            Operation::CallTool { name, arguments }
                if name == "ssmcp.worker.report" && arguments["event"] == "progress" =>
            {
                arguments["claim"]["id"].as_str().map(str::to_owned)
            }
            _ => None,
        };
        let mut result = self.perform_inner(operation).await;
        crate::client::terminal::tool_summary_call_finished(
            &source,
            result.as_ref().is_ok_and(|value| value["isError"] != true),
        );
        if let Ok(value) = &mut result {
            // Reserve the reminder metadata for the host. It contains no task text.
            if let Some(meta) = value.get_mut("_meta").and_then(Value::as_object_mut) {
                meta.remove(crate::worker::PROGRESS_META);
            }
            if value["isError"] != true && value["content"].is_array() {
                let marker = if let Some(id) =
                    reported.filter(|_| value["structuredContent"]["duplicate"] == false)
                {
                    Some(json!({"reported":id}))
                } else if work_call {
                    self.workers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .and_then(|pool| pool.progress_reminder(&source))
                        .map(|id| json!({"due":id}))
                } else {
                    None
                };
                if let Some(marker) = marker
                    && (value["_meta"].is_null() || value["_meta"].is_object())
                {
                    if marker["due"].is_string()
                        && value["structuredContent"]["status"] == "assigned"
                    {
                        value["structuredContent"]["progress_hint"] =
                            json!(crate::worker::PROGRESS_REMINDER);
                    }
                    value["_meta"][crate::worker::PROGRESS_META] = marker;
                }
            }
            // This reserved metadata is generated by the host, never providers.
            crate::client::interface::tool_summary::strip_metadata(value);
            let reminder = value
                .is_object()
                .then(|| crate::client::terminal::take_tool_summary_reminder(&source))
                .flatten();
            *value = crate::client::interface::tool_summary::attach_metadata(
                std::mem::take(value),
                reminder,
            );
        }
        result
    }

    async fn perform_inner(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        self.registry.ensure_open()?;
        let (name, arguments) = match operation {
            Operation::PeerPort { .. } => {
                return Err("peer control requires a negotiated relay connection".into());
            }
            // Continuations retain an immutable view of the one registry. Calls
            // still validate the current provider and registration generation.
            Operation::ListTools { cursor: None } => {
                return self.catalog.first(self.registry.snapshot()?);
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
        if matches!(
            name.as_str(),
            "ssmcp.time.now" | "ssmcp.time.wait" | "ssmcp.time.prompt"
        ) && !self.time.time_tools_enabled()
        {
            return Err("time_observation_not_negotiated: remote time tools require an explicit time_observation_v2 server acknowledgement; old server delivery policies still apply to hard-coded unsupported calls".into());
        }
        if !self.chat_enabled() && crate::wire::human_chat_tool(&name) {
            return Err(crate::wire::CHAT_DISABLED.into());
        }
        let tool = self.registry.lookup(&name)?;
        self.registry.validate(&tool)?;
        let call_id = self.next_call.fetch_add(1, Ordering::Relaxed);
        let hook = self
            .tool_call_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(hook) = hook else {
            return self
                .perform_registered(name, arguments, tool, call_id)
                .await;
        };
        let call = hooks::ToolCall {
            id: call_id,
            name: name.clone(),
            native_name: tool.native_name.clone(),
            provider: tool.provider,
            generation: tool.generation,
            arguments: arguments.clone(),
        };
        let decision =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook.before_call(&call)));
        let result = match decision {
            Ok(Ok(())) => match self.registry.validate(&tool) {
                Ok(()) => {
                    self.perform_registered(name, arguments, tool, call_id)
                        .await
                }
                Err(error) => Err(error),
            },
            Ok(Err(reason)) => Err(format!("client tool hook denied {}: {reason}", call.name)),
            Err(_) => Err(format!(
                "client tool hook panicked before dispatch; {} rejected",
                call.name
            )),
        };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            hook.after_call(&call, &result)
        }))
        .is_err()
        {
            self.audit.push(json!({
                "type":"ssmcp.status","level":"Warning","call_id":call_id,"tool":call.name,
                "message":format!("Client tool hook panicked after {}; native result was preserved", call.name),
            }));
        }
        result
    }

    async fn perform_registered(
        &self,
        name: String,
        arguments: Value,
        tool: RegisteredTool,
        call_id: u64,
    ) -> Result<Value, String> {
        let started = std::time::Instant::now();
        let source = call_scope::Source::capture();
        if let Some(call) = &source.call {
            call.set_native_tool(&name);
        }
        let mut history_call =
            crate::session::attach::thread_history::ToolCall::start(&source, &name);
        let prompt_activity = (tool.provider == Provider::Time
            && tool.native_name == "ssmcp.time.prompt")
            .then(|| crate::client::terminal::prompt_activity::PromptActivity::new(&arguments));
        if prompt_activity.is_none() {
            crate::client::terminal::tool_summary_call_started(&source, &name, None);
        }
        let caller = source.call.clone();
        let control = matches!(tool.provider, Provider::Computer | Provider::Sessions);
        if control {
            self.audit
                .push(json!({"type":"ssmcp.control.started","call_id":call_id,
                "provider":tool.provider.label(),"tool":name,"generation":tool.generation}));
        }
        let skill_path = arguments
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let mut observation = self.time.observe(&name, &arguments, &source);
        let dispatched = async {
            match tool.provider {
                Provider::Client => self
                    .client_resource_call(&tool.native_name, arguments)
                    .await
                    .map(backend::result),
                Provider::Output => self
                    .output
                    .perform(&tool.native_name, arguments)
                    .await
                    .map(backend::result),
                Provider::Reload => {
                    Err("reload requires an acknowledged foreground connection".into())
                }
                Provider::Sessions => match &self.sessions {
                    Some(control) => control
                        .perform(&tool.native_name, arguments)
                        .await
                        .map(backend::result),
                    None => Err("session management was not granted with --session-control".into()),
                },
                Provider::Audit => self.audit.read(arguments).await.map(backend::result),
                Provider::ManagedFiles => {
                    let root = self.tool_root.clone();
                    let name = tool.native_name.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::managed_files::perform(&root, &name, arguments)
                    })
                    .await
                    .map_err(|error| format!("managed-file operation failed: {error}"))?
                    .map(backend::result)
                }
                Provider::Worker => {
                    let pool = self
                        .workers
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .clone()
                        .ok_or("worker mode was not configured")?;
                    pool.perform(&tool.native_name, arguments, self)
                        .await
                        .map(backend::result)
                }
                Provider::Plugins => self.plugins.perform(&tool, arguments).await,
                provider if self.native.contains(provider) => {
                    source
                        .clone()
                        .scope(self.native.call(&tool, arguments))
                        .await
                }
                _ => Err("provider is not mounted in the native plugin host".into()),
            }
        };
        let mut observation_stopped = false;
        let outcome = if let Some(waiting) = observation.as_mut() {
            tokio::select! {
                biased;
                _ = waiting.cancelled() => {
                    observation_stopped = true;
                    Err("native wait observation cancelled; the underlying job was not cancelled".into())
                },
                outcome = dispatched => outcome,
            }
        } else {
            dispatched.await
        };
        if let Some(waiting) = observation.as_mut() {
            waiting.finish(&outcome, observation_stopped);
        }
        drop(observation);
        if let Some(label) = prompt_activity.and_then(|activity| activity.label(&outcome)) {
            crate::client::terminal::tool_summary_call_started(&source, &name, Some(&label));
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
        history_call.finish(outcome.as_ref().is_ok_and(|value| value["isError"] != true));
        let mut value = match outcome {
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
        // Remove forged reserved fields before any response is archived or spilled.
        crate::client::interface::tool_summary::strip_metadata(&mut value);
        // Only this verified native route may retain read output for local
        // expansion. Generic RPC/audit responses remain metadata-only.
        if tool.provider == Provider::Exec && tool.native_name == "ssmcp.exec.read" {
            crate::client::terminal::tool_summary_exec_read_result(&source, &value);
        }
        if crate::client::terminal::active()
            && let Some(content) = value.get("content").and_then(Value::as_array)
        {
            let images: Vec<Value> = content
                .iter()
                .filter(|block| block["type"] == "image")
                .take(4)
                .cloned()
                .collect();
            if !images.is_empty() {
                let source = source.display();
                let _ = tokio::task::spawn_blocking(move || {
                    for image in images {
                        crate::client::terminal::image(&image, source.clone());
                    }
                })
                .await;
            }
        }
        // Retrieval pages already point to the original complete capture. Guarding
        // them again would recursively archive the artifact-read response itself.
        // Context already enforces a 1 MiB response and 256 KiB full-document limit.
        // Keep structured contents available to resources/read instead of spilling them.
        let value = if matches!(tool.provider, Provider::Context | Provider::Client)
            || matches!(
                name.as_str(),
                "ssmcp.output.read" | "terminal.read" | "ssmcp.exec.read"
            ) {
            value
        } else {
            self.output.guard_result(value)
        };
        // Only an actually dispatched, authorized operation reaches this boundary.
        // Add local history after spill handling so the metadata stays available
        // even when the original native output is an artifact reference.
        Ok(crate::client::interface::questions::attach_answers(
            value,
            caller.as_deref(),
            &name,
            self.chat_enabled(),
        ))
    }
    pub async fn shutdown(&self) {
        self.shutdown_inner(false).await;
    }

    pub(crate) async fn shutdown_for_reload(&self) {
        self.shutdown_inner(true).await;
    }

    async fn shutdown_inner(&self, reload: bool) {
        self.registry.close();
        {
            let pool = self
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            if let Some(pool) = pool
                && let Err(error) = pool.shutdown().await
            {
                self.audit.push(json!({"type":"ssmcp.worker.cleanup_failed","cleanup_complete":false,"error":error}));
            }
        }
        self.catalog
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        let (native, plugins) = tokio::join!(self.native.shutdown(), self.plugins.shutdown(),);
        for (plugin, outcome) in native {
            if let Err(error) = outcome {
                // Keep the established Computer cleanup category while retaining
                // the exact plugin identity for other failures.
                let provider = if plugin == Desktop::ID {
                    "computer"
                } else {
                    &plugin
                };
                self.audit.push(json!({
                    "type":"ssmcp.control.cleanup_failed",
                    "provider":provider,
                    "plugin":plugin,
                    "cleanup_complete":false,
                    "error":error
                }));
            }
        }
        if let Err(error) = plugins {
            self.audit.push(json!({
                "type":"ssmcp.control.cleanup_failed", "provider":"plugins",
                "cleanup_complete":false, "error":error
            }));
        }
        if reload {
            // Requests and the previous scheduled scan were joined by the client;
            // workers, native backends and plugins above have now stopped.
            if let Err(error) = self.start_managed_file_cleanup("reload").await {
                let warning = format!("Managed-file reload cleanup task failed: {error}");
                self.audit.push(json!({
                    "type": "ssmcp.files.cleanup_warning",
                    "trigger": "reload",
                    "message": warning,
                }));
                crate::client::render::status("cleanup", "Warning", &warning);
            }
        }
        self.audit.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct ManagedCleanupRoot(PathBuf);

    impl ManagedCleanupRoot {
        fn new() -> Self {
            let mut random = [0u8; 8];
            crate::platform::random_bytes(&mut random).unwrap();
            let suffix = u64::from_ne_bytes(random);
            let root = std::env::temp_dir().join(format!(
                "ssmcp-managed-lifecycle-{}-{suffix:016x}",
                std::process::id()
            ));
            crate::platform::private_directory(&root).unwrap();
            Self(std::fs::canonicalize(root).unwrap())
        }

        async fn runtime(&self) -> Arc<Runtime> {
            let output = OutputStore::start_connection().unwrap();
            #[cfg(unix)]
            let shell = PathBuf::from("/bin/sh");
            #[cfg(windows)]
            let shell = crate::platform::default_shell();
            let backend = Backend::with_output(&shell, self.0.clone(), output)
                .await
                .unwrap();
            Runtime::new(backend, self.0.clone()).unwrap()
        }
    }

    impl Drop for ManagedCleanupRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn safety_attach_reads_only_the_selected_live_runtime() {
        use crate::session::attach;

        let root = ManagedCleanupRoot::new();
        let sibling = ManagedCleanupRoot::new();
        let runtime = root.runtime().await;
        let other = sibling.runtime().await;
        let endpoint = root.0.join("attach");
        crate::platform::private_directory(&endpoint).unwrap();
        let _server = attach::serve(endpoint.clone(), Some(runtime.clone()))
            .await
            .unwrap();
        let operation = || Operation::CallTool {
            name: "ssmcp.fs.write_text".into(),
            arguments: json!({
                "path":"../synthetic-private-target",
                "text":"synthetic-private",
                "create_parents":true
            }),
        };
        assert!(other.perform(operation()).await.is_err());
        let empty = attach::safety_at_directory(endpoint.clone(), None)
            .await
            .unwrap();
        assert_eq!(empty["events"], json!([]));
        assert!(runtime.perform(operation()).await.is_err());

        let queue = attach::safety_at_directory(endpoint.clone(), None)
            .await
            .unwrap();
        let event = &queue["events"][0];
        assert_eq!(event["decision"], "deny");
        assert!(!queue.to_string().contains("synthetic-private"));
        let id = event["id"].as_str().unwrap();
        assert_eq!(
            attach::safety_at_directory(endpoint.clone(), Some(id))
                .await
                .unwrap(),
            *event
        );
        assert!(
            attach::safety_at_directory(endpoint, Some("expired:1:0"))
                .await
                .is_err()
        );
        runtime.shutdown().await;
        other.shutdown().await;
    }

    #[tokio::test]
    async fn managed_file_startup_reports_empty_cleanup_without_creating_a_registry() {
        let root = ManagedCleanupRoot::new();
        let runtime = root.runtime().await;
        runtime.start_managed_file_cleanup("startup").await.unwrap();
        runtime.shutdown().await;

        let page = runtime.audit.read(json!({})).await.unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0]["event"];
        assert_eq!(event["type"], "ssmcp.files.cleanup");
        assert_eq!(event["trigger"], "startup");
        assert_eq!(event["summary"]["trigger"], "startup");
        assert_eq!(event["summary"]["registry_present"], false);
        assert_eq!(event["summary"]["enabled"], false);
        assert_eq!(event["summary"]["dry_run"], false);
        assert_eq!(event["summary"]["removed_files"], 0);
        assert_eq!(event["summary"]["removed_bytes"], 0);
        assert_eq!(event["summary"]["errors"], json!([]));
        assert_eq!(event["summary"]["recoveries"], json!([]));
        assert_eq!(page["closed"], true);
        assert!(page["archive_error"].is_null());
        assert!(!root.0.join(".ssmcp/managed-files").exists());
    }

    #[tokio::test]
    async fn managed_file_reload_cleans_released_files_before_closing_its_audit() {
        use crate::managed_files::{Category, CleanupConfig, FileIdentity, ManagedFiles};

        let root = ManagedCleanupRoot::new();
        let store = ManagedFiles::open(&root.0).unwrap();
        store
            .set_config(&CleanupConfig {
                max_age_seconds: 0,
                ..CleanupConfig::default()
            })
            .unwrap();
        crate::platform::private_directory(&root.0.join(".ssmcp/tmp")).unwrap();
        let relative = std::path::Path::new(".ssmcp/tmp/lifecycle.txt");
        let path = root.0.join(relative);
        let bytes = b"owned temporary data";
        std::fs::write(&path, bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let identity = FileIdentity::from_file(&file).unwrap();
        crate::managed_files::record_created(
            &root.0,
            relative,
            Category::Temporary,
            "lifecycle-test",
            &identity,
        )
        .unwrap();
        drop(file);

        let lease = store.lease(relative).unwrap();
        let runtime = root.runtime().await;
        runtime.start_managed_file_cleanup("startup").await.unwrap();
        let retained = std::fs::read(&path);
        drop(lease);
        runtime.shutdown_for_reload().await;

        assert_eq!(retained.unwrap(), bytes);
        let page = runtime.audit.read(json!({})).await.unwrap();
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"]["trigger"], "startup");
        assert_eq!(events[0]["event"]["summary"]["kept_active"], 1);
        assert_eq!(events[0]["event"]["summary"]["removed_files"], 0);
        assert_eq!(events[1]["event"]["type"], "ssmcp.files.cleanup");
        assert_eq!(events[1]["event"]["trigger"], "reload");
        assert_eq!(events[1]["event"]["summary"]["removed_files"], 1);
        assert_eq!(
            events[1]["event"]["summary"]["removed_bytes"],
            bytes.len() as u64
        );
        assert!(!path.exists());
        assert_eq!(page["closed"], true);
        assert!(page["archive_error"].is_null());
    }

    fn code_context(code: &str) -> crate::wire::CodeContext {
        crate::wire::CodeContext {
            session_id: None,
            thread_id: None,
            run_id: "a".repeat(32),
            code: code.into(),
            timestamp_ms: 1234,
        }
    }

    #[tokio::test]
    async fn code_audit_preserves_full_source_and_keeps_job_call_and_run_outcomes_separate() {
        let log = EventLog::new("code-audit-test");
        let audit = CodeAudit::new(log.clone());
        let source = format!(
            "// {}\nconst job = await exec.start('one', {{command:['true']}});\nreturn job;\n",
            "source line ".repeat(5000)
        );
        let context = code_context(&source);
        let operation = Operation::CallTool {
            name: "ssmcp.exec.start".into(),
            arguments: json!({"command":["true"]}),
        };
        let mut first = audit
            .begin(&operation, 1, None, None, Some(context.clone()))
            .unwrap()
            .unwrap();
        first.finish(&Ok(backend::result(
            json!({"job_id":"job-a","state":"running","running":true}),
        )));
        let mut second = audit
            .begin(&operation, 2, None, None, Some(context))
            .unwrap()
            .unwrap();
        second.finish(&Ok(backend::result(
            json!({"job_id":"job-a","state":"completed","running":false,"exit_code":7}),
        )));
        let completion = crate::wire::CodeCompletion {
            run_id: "a".repeat(32),
            success: false,
            status: "failed".into(),
            duration_ms: 20,
            error: Some("script failed after native completion".into()),
        };
        audit.finish(completion.clone()).unwrap();
        audit.finish(completion).unwrap();
        let events = log.read(json!({})).await.unwrap()["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["event"].clone())
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 6);
        assert_eq!(events[0]["code"], source);
        assert_eq!(events[0]["timestamp_ms"], 1234);
        assert_eq!(events[2]["phase"], "call-finished");
        assert_eq!(events[2]["scope"], "native-call");
        assert_eq!(events[2]["success"], true);
        assert_eq!(events[2]["state"], "running");
        assert!(events[2].get("exit_code").is_none());
        assert_eq!(events[4]["exit_code"], 7);
        assert_eq!(events[5]["phase"], "finished");
        assert_eq!(events[5]["success"], false);
        assert_eq!(events[5]["error"], "script failed after native completion");
    }

    #[tokio::test]
    async fn code_audit_redacts_source_secrets_and_reports_cancellation_without_raw_arguments() {
        let log = EventLog::new("code-audit-redaction-test");
        let audit = CodeAudit::new(log.clone());
        let source = "const token = 'sensitive-source-token';\nconst n = 17;\nreturn await call('one','tool.run',{});\n";
        let operation = Operation::CallTool {
            name: "tool.run".into(),
            arguments: json!({"password":"sensitive-native-argument"}),
        };
        drop(
            audit
                .begin(&operation, 5, None, None, Some(code_context(source)))
                .unwrap(),
        );
        let page = log.read(json!({})).await.unwrap();
        let events = page["events"].as_array().unwrap();
        let clean = events[0]["event"]["code"].as_str().unwrap();
        assert!(!clean.contains("sensitive-source-token"));
        assert!(clean.contains("[redacted]"));
        assert!(clean.contains("const n = 17;\n"));
        assert!(!page.to_string().contains("sensitive-native-argument"));
        assert_eq!(events[2]["event"]["success"], false);
        assert!(
            events[2]["event"]["error"]
                .as_str()
                .unwrap()
                .contains("cancelled")
        );
        assert!(
            audit
                .begin(&operation, 6, None, None, None)
                .unwrap()
                .is_none()
        );
    }

    fn tools(generation: usize) -> Vec<ToolDescriptor> {
        (0..150)
            .map(|n| ToolDescriptor::from_value(json!({"name":format!("tool{n}"),"inputSchema":{"type":"object"},"generation":generation})).unwrap())
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
            .map(|n| ToolDescriptor::from_value(json!({"name":format!("tool{n}"),"inputSchema":{"type":"object"},"generation":1})).unwrap())
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
