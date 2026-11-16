//! Native backend plus connection-owned workspace utilities.
use crate::{
    backend::{self, Backend},
    rsync::Rsync,
    wire::Operation,
    workspace::Workspace,
};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

pub struct Runtime {
    backend: Arc<Backend>,
    workspace: Workspace,
    rsync: Rsync,
}

impl Runtime {
    pub fn new(backend: Arc<Backend>, root: PathBuf) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            backend,
            workspace: Workspace::new(root.clone())?,
            rsync: Rsync::new(root)?,
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = self.backend.tools().await?;
        tools.extend(Workspace::tools());
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
