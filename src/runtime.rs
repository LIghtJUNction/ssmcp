//! Registry-driven native calls and utilities scoped to one authenticated connection.
#[cfg(target_os = "linux")]
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
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub struct Runtime {
    backend: Arc<Backend>,
    registry: Arc<Registry>,
    workspace: Workspace,
    context: Context,
    #[cfg(target_os = "linux")]
    rsync: Rsync,
    output: Arc<OutputStore>,
    audit: Arc<EventLog>,
    next_call: AtomicU64,
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
        #[cfg(target_os = "linux")]
        let rsync = Rsync::new(root)?;
        registry.replace(Provider::Workspace, Workspace::tools())?;
        registry.replace(Provider::Context, Context::tools())?;
        #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "linux")]
            rsync,
            audit: EventLog::with_output("audit", output.clone()),
            output,
            next_call: AtomicU64::new(1),
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
        let (name, arguments) = match operation {
            Operation::ListTools { cursor } => {
                return backend::page(self.registry.catalog()?, cursor.as_deref());
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
            #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        tokio::join!(
            self.workspace.shutdown(),
            self.rsync.shutdown(),
            self.backend.shutdown()
        );
        #[cfg(not(target_os = "linux"))]
        tokio::join!(self.workspace.shutdown(), self.backend.shutdown());
        self.audit.close();
    }
}
