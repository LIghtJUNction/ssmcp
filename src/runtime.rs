//! Registry-driven native calls and utilities scoped to one authenticated connection.
pub(crate) mod call_scope;
mod capabilities;
pub mod hooks;
pub(crate) mod native_host;
pub(crate) mod native_plugins;
mod plugins;
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
        Arc, Mutex,
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
    files: Arc<Files>,
    desktop: Arc<Desktop>,
    plugins: plugins::Plugins,
    output: Arc<OutputStore>,
    audit: Arc<EventLog>,
    code_audit: CodeAudit,
    call_connection: u64,
    next_call: AtomicU64,
    tool_call_hook: Mutex<Option<Arc<dyn hooks::ToolCallHook>>>,
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
        let constructor = ConstructorContext {
            root: root.clone(),
            context_roots,
            registry: registry.clone(),
            output: output.clone(),
        };
        let files = Files::new(&constructor)?;
        let desktop = Desktop::new(&constructor)?;
        let native = NativeHost::new(
            registry.clone(),
            vec![
                Definition::new(backend.clone()),
                Definition::new(files.clone()),
                Definition::new(desktop.clone()),
            ],
        )?;
        let plugins = plugins::Plugins::new(root, registry.clone());
        registry.replace(Provider::Client, capabilities::tools())?;
        for (provider, tools) in native.catalogs()? {
            registry.replace(provider, tools)?;
        }
        registry.replace(Provider::Plugins, plugins::Plugins::tools())?;
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
            "description":"Read observed tool and skill events for this connection after audit redaction. Wire v6 also includes code_mode source and run/native-call lifecycle outcomes. File operations performed by external harnesses have unknown audit coverage unless their native protocol reports them.",
            "inputSchema":crate::backend::events::schema(),
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        })])?;
        let audit = EventLog::with_output("audit", output.clone());
        let runtime = Arc::new(Self {
            backend,
            registry,
            native,
            files,
            desktop,
            plugins,
            code_audit: CodeAudit::new(audit.clone()),
            call_connection: call_scope::connection(),
            audit,
            output,
            next_call: AtomicU64::new(1),
            tool_call_hook: Mutex::new(None),
            catalog: Catalog::default(),
            sessions,
        });
        crate::client::interface::harness::bind(&runtime);
        crate::client::completion::bind(&runtime);
        Ok(runtime)
    }

    /// Production startup opts in to exact installed skill sources and builtins.
    /// Runtime constructors themselves never inspect the caller's home directory.
    pub fn enable_default_skill_sources(&self) -> Result<(), String> {
        self.files.enable_default_skill_sources()
    }
    /// Install an optional in-process call hook. The default is None.
    /// Each call keeps one snapshot; replacement affects subsequent calls.
    pub fn set_tool_call_hook(&self, hook: Option<Arc<dyn hooks::ToolCallHook>>) {
        *self
            .tool_call_hook
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = hook;
    }

    pub(crate) fn local_registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }
    /// Local frontend methods, deliberately absent from the remote tool registry.
    pub(crate) fn handoff_host_input_local(&self, seconds: u64) -> Result<(), String> {
        self.desktop.handoff_host_input_local(seconds)
    }
    pub(crate) fn revoke_host_input(&self) -> bool {
        self.desktop.revoke_host_input()
    }
    pub(crate) fn host_input_status(&self) -> Value {
        self.desktop.host_input_status()
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
    /// Accept only metadata supplied by the authenticated relay frame, before
    /// entering a provider. Tool arguments cannot create this scope.
    pub(crate) fn call_scope(
        &self,
        request_id: u64,
        attribution: Option<&crate::wire::ControlAttribution>,
        context: Option<&crate::wire::CodeContext>,
    ) -> Result<Arc<call_scope::CallScope>, String> {
        call_scope::CallScope::from_relay(self.call_connection, request_id, attribution, context)
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
        operation.validate()?;
        self.registry.ensure_open()?;
        let (name, arguments) = match operation {
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
            Provider::Plugins => self.plugins.perform(&tool, arguments).await,
            provider if self.native.contains(provider) => {
                let dispatch = self.native.call(&tool, arguments);
                if let Some(call) = &harness_call {
                    call.scope(dispatch).await
                } else {
                    dispatch.await
                }
            }
            _ => Err("provider is not mounted in the native plugin host".into()),
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
                let _ = tokio::task::spawn_blocking(move || {
                    for image in images {
                        crate::client::terminal::image(&image);
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
        let context = tokio::task::spawn_blocking(crate::client::context_memory::snapshot_for_tool)
            .await
            .ok()
            .flatten();
        Ok(attach_context_metadata(value, context))
    }
    pub async fn shutdown(&self) {
        self.registry.close();
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
        self.audit.close();
    }
}

/// Preserve every native field, including opaque provider metadata. Memory is
/// reference data in its own namespace, never a user requirement or tool output.
fn attach_context_metadata(mut result: Value, context: Option<Value>) -> Value {
    let Some(mut context) = context else {
        return result;
    };
    if !result.is_object()
        || result
            .get("_meta")
            .is_some_and(|meta| !meta.is_object() || meta.get("ssmcp.context").is_some())
    {
        return result;
    }
    let base = result.to_string().len();
    let limit = crate::wire::MAX_FRAME.saturating_sub(8192);
    if context.to_string().len() > crate::wire::MAX_CONTEXT_ENTRY_BYTES
        || base
            .saturating_add(context.to_string().len())
            .saturating_add(64)
            > limit
    {
        context = json!({
            "workspace":{"id":context["workspace"]["id"]},
            "revision":context["revision"],
            "snapshot":{"context_omitted":true,"full_read":"ssmcp.context.read"},
            "provenance":"Saved local workspace history; omitted at the native result budget. Read ssmcp.context.read for the complete context."
        });
        if base
            .saturating_add(context.to_string().len())
            .saturating_add(64)
            > limit
        {
            return result;
        }
    }
    let object = result.as_object_mut().expect("object checked");
    object
        .entry("_meta")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("metadata checked")
        .insert("ssmcp.context".into(), context);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn memory_metadata_preserves_native_output_errors_and_vendor_metadata() {
        let context =
            json!({"workspace":{"id":"a".repeat(64)},"revision":4,"summary":"earlier work"});
        for failed in [false, true] {
            let original = json!({
                "content":[{"type":"text","text":"stdout"},{"type":"image","mimeType":"image/png","data":"opaque-image"}],
                "structuredContent":{"exit_code":7},"isError":failed,
                "_meta":{"vendor/result":{"unchanged":true}}
            });
            let mut result = attach_context_metadata(original.clone(), Some(context.clone()));
            assert_eq!(result["_meta"]["ssmcp.context"], context);
            result["_meta"]
                .as_object_mut()
                .unwrap()
                .remove("ssmcp.context");
            assert_eq!(result, original);
        }
        for original in [
            json!({"content":[],"_meta":"opaque"}),
            json!({"content":[],"_meta":{"ssmcp.context":{"provider":"reserved"}}}),
            json!(["native-array"]),
        ] {
            assert_eq!(
                attach_context_metadata(original.clone(), Some(context.clone())),
                original
            );
        }
    }

    #[test]
    fn native_memory_budget_returns_an_explicit_read_reference_without_changing_output() {
        let original = json!({"content":[{"type":"text","text":"original"}],"isError":false});
        let result = attach_context_metadata(
            original.clone(),
            Some(json!({
                "workspace":{"id":"b".repeat(64)},"revision":5,
                "summary":"x".repeat(crate::wire::MAX_CONTEXT_ENTRY_BYTES)
            })),
        );
        assert_eq!(result["content"], original["content"]);
        assert_eq!(result["isError"], original["isError"]);
        assert_eq!(
            result["_meta"]["ssmcp.context"]["snapshot"]["context_omitted"],
            true
        );
        assert_eq!(
            result["_meta"]["ssmcp.context"]["snapshot"]["full_read"],
            "ssmcp.context.read"
        );
        assert_eq!(result["_meta"]["ssmcp.context"]["revision"], 5);
    }

    fn code_context(code: &str) -> crate::wire::CodeContext {
        crate::wire::CodeContext {
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
