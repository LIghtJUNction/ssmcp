//! Native backend plus connection-owned workspace utilities.
use crate::{
    backend::{self, Backend},
    context::Context,
    rsync::Rsync,
    wire::Operation,
    workspace::Workspace,
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

pub struct Runtime {
    backend: Arc<Backend>,
    workspace: Workspace,
    context: Context,
    rsync: Rsync,
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
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = self.backend.tools().await?;
        tools.extend(Workspace::tools());
        tools.extend(Context::tools());
        tools.extend(Rsync::tools());
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
                self.context
                    .perform(&name, arguments)
                    .await
                    .map(backend::result)
            }
            Operation::CallTool { name, arguments } if name.starts_with("ssmcp.rsync.") => self
                .rsync
                .perform(&name, arguments)
                .await
                .map(backend::result),
            operation => self.backend.perform(operation).await,
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
