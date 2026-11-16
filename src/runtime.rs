//! Native backend plus connection-owned workspace utilities.
#[cfg(target_os = "linux")]
use crate::rsync::Rsync;
use crate::{
    backend::{self, Backend},
    context::Context,
    output::OutputStore,
    wire::Operation,
    workspace::Workspace,
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

pub struct Runtime {
    backend: Arc<Backend>,
    workspace: Workspace,
    context: Context,
    #[cfg(target_os = "linux")]
    rsync: Rsync,
    output: Arc<OutputStore>,
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
            #[cfg(target_os = "linux")]
            rsync: Rsync::new(root)?,
            output: OutputStore::global()?,
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = self.backend.tools().await?;
        tools.extend(Workspace::tools());
        tools.extend(Context::tools());
        #[cfg(target_os = "linux")]
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
            Operation::ListTools { cursor } => {
                backend::page(self.tools().await?, cursor.as_deref())
            }
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
            #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        tokio::join!(
            self.workspace.shutdown(),
            self.rsync.shutdown(),
            self.backend.shutdown()
        );
        #[cfg(not(target_os = "linux"))]
        tokio::join!(self.workspace.shutdown(), self.backend.shutdown());
    }
}
