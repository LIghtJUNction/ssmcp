//! Workspace, context and transfer state owned by the packaged files plugin.
use crate::{
    backend,
    client::{attachments, completion, references},
    context::Context,
    registry::Provider,
    rsync::Rsync,
    runtime::native_host::{Call, ConstructorContext, NativePlugin, PluginFuture},
    workspace::Workspace,
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct Files {
    workspace: Workspace,
    context: Context,
    rsync: Rsync,
}

impl Files {
    pub(crate) fn new(context: &ConstructorContext) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            workspace: Workspace::with_output(context.root.clone(), context.output.clone())?,
            context: Context::with_output(
                context.root.clone(),
                context.context_roots.clone(),
                context.output.clone(),
            )?,
            rsync: Rsync::new(context.root.clone())?,
        }))
    }

    // The local composer uses the same granted roots and state as remote calls.
    // These crate-private methods do not add another remotely callable surface.
    pub(crate) fn enable_default_skill_sources(&self) -> Result<(), String> {
        self.context.enable_default_skill_sources()
    }

    pub(crate) async fn expand_references(&self, text: &str) -> Result<String, String> {
        references::expand(text, &self.workspace, &self.context).await
    }

    pub(crate) async fn reference_completions(
        &self,
        key: &completion::Key,
    ) -> Result<completion::Listing, String> {
        completion::enumerate(&self.workspace, &self.context, key).await
    }

    pub(crate) async fn import_input(
        &self,
        source: attachments::Source,
    ) -> Result<attachments::Loaded, String> {
        attachments::load(source, &self.workspace).await
    }

    /// Resource responses retain their raw shape; Runtime applies MCP wrapping.
    pub(crate) async fn context_resource(
        &self,
        name: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        self.context.perform(name, arguments).await
    }
}

impl NativePlugin for Files {
    const ID: &'static str = "files";

    fn providers(&self) -> &'static [Provider] {
        &[Provider::Workspace, Provider::Context, Provider::Rsync]
    }

    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![
            (Provider::Workspace, Workspace::tools()),
            (Provider::Context, Context::tools()),
            (Provider::Rsync, Rsync::tools()),
        ]
    }

    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            let value = match call.provider {
                Provider::Workspace => {
                    self.workspace
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                Provider::Context => {
                    self.context
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                Provider::Rsync => {
                    self.rsync
                        .perform(&call.native_name, call.arguments)
                        .await?
                }
                provider => {
                    return Err(format!(
                        "provider {} does not belong to the files plugin",
                        provider.label()
                    ));
                }
            };
            if call.provider == Provider::Workspace && call.native_name == "ssmcp.fs.read_image" {
                // This backend already returns MCP image/text content blocks.
                Ok(value)
            } else {
                Ok(backend::result(value))
            }
        })
    }

    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            tokio::join!(self.workspace.shutdown(), self.rsync.shutdown());
            Ok(Value::Null)
        })
    }
}
