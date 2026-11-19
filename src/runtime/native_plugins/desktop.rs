//! Connection-owned browser, isolated desktop and Omarchy providers.
use crate::{
    backend,
    browser::Browser,
    computer::Computer,
    omarchy::Omarchy,
    registry::{Provider, Registry},
    runtime::native_host::{Call, ConstructorContext, NativePlugin, PluginFuture},
};
use serde_json::Value;
use std::sync::Arc;

pub(crate) struct Desktop {
    browser: Browser,
    computer: Computer,
    omarchy: Omarchy,
    registry: Arc<Registry>,
}

impl Desktop {
    pub(crate) fn new(context: &ConstructorContext) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            browser: Browser::new(context.root.clone())?,
            computer: Computer::new(context.root.clone())?,
            omarchy: Omarchy::new(context.root.clone())?,
            registry: context.registry.clone(),
        }))
    }

    // These local-only controls act on the same Computer that handles remote
    // tools. They are deliberately absent from the plugin's native catalog.
    pub(crate) fn handoff_host_input_local(&self, seconds: u64) -> Result<(), String> {
        self.computer.handoff_host_input_local(seconds)
    }

    pub(crate) fn revoke_host_input(&self) -> bool {
        self.computer.revoke_host_input()
    }

    pub(crate) fn host_input_status(&self) -> Value {
        self.computer.host_input_status()
    }

    fn browser_discovery(&self, mut value: Value) -> Result<Value, String> {
        let mut metadata = value
            .get_mut("structuredContent")
            .ok_or("browser discovery returned no metadata")?
            .take();
        metadata["routing"] = crate::browser::control_routes(&self.registry)?;
        Ok(backend::result(metadata))
    }
}

impl NativePlugin for Desktop {
    const ID: &'static str = "desktop";

    fn providers(&self) -> &'static [Provider] {
        &[Provider::Browser, Provider::Computer, Provider::Omarchy]
    }

    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![
            (Provider::Browser, Browser::tools()),
            (Provider::Computer, Computer::tools()),
            (Provider::Omarchy, Omarchy::tools()),
        ]
    }

    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            match call.provider {
                Provider::Browser => {
                    let value = self
                        .browser
                        .perform(&call.native_name, call.arguments)
                        .await?;
                    if call.native_name == "ssmcp.browser.discover" {
                        self.browser_discovery(value)
                    } else {
                        Ok(value)
                    }
                }
                Provider::Computer => {
                    self.computer
                        .perform(&call.native_name, call.arguments)
                        .await
                }
                Provider::Omarchy => {
                    self.omarchy
                        .perform(&call.native_name, call.arguments)
                        .await
                }
                provider => Err(format!(
                    "desktop plugin does not own provider: {}",
                    provider.label()
                )),
            }
        })
    }

    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            let (_, computer, _) = tokio::join!(
                self.browser.shutdown(),
                self.computer.shutdown(),
                self.omarchy.shutdown()
            );
            computer?;
            Ok(Value::Null)
        })
    }
}
