//! Lifecycle management for the standalone Zuse CLI.
//! Desktop and browser actions are executed directly by Zuse, outside this provider.
use crate::{
    computer::Computer,
    registry::Provider,
    runtime::native_host::{Call, ConstructorContext, NativePlugin, PluginFuture},
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct Desktop {
    computer: Computer,
}

impl Desktop {
    pub(crate) fn new(context: &ConstructorContext) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            computer: Computer::new(context.root.clone())?,
        }))
    }
}

impl NativePlugin for Desktop {
    const ID: &'static str = "ssmcp.computer";

    fn providers(&self) -> &'static [Provider] {
        &[Provider::Computer]
    }

    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![(Provider::Computer, Computer::tools())]
    }

    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            match call.provider {
                Provider::Computer => {
                    self.computer
                        .perform(&call.native_name, call.arguments)
                        .await
                }
                provider => Err(format!(
                    "computer lifecycle plugin does not own provider: {}",
                    provider.label()
                )),
            }
        })
    }

    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            self.computer.shutdown().await?;
            Ok(Value::Null)
        })
    }
}
