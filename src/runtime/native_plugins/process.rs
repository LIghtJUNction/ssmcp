//! Process, terminal and optional-harness capability plugin.
//! Owns provider state, registration, native dispatch and cleanup.
mod lazy;
use super::super::native_host::{Call, NativePlugin, PluginFuture};
use crate::backend::{peer, result, shell};
use crate::{
    output::OutputStore,
    pty::Terminal,
    registry::{Provider, RegisteredTool, Registry},
    terminals::Terminals,
    wire::{MAX_READ, MAX_WAIT_MS, MAX_WRITE, Operation},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
};

pub struct Process {
    observer: Option<Arc<crate::observer::Pool>>,
    shell: Arc<shell::Shell>,
    pub(crate) registry: Arc<Registry>,
    output: Arc<OutputStore>,
    drivers: StdMutex<BTreeMap<Provider, Arc<peer::Peer>>>,
    diagnostics: StdMutex<BTreeMap<Provider, Value>>,
    loads: lazy::Loads,
    closed: AtomicBool,
    chat_enabled: AtomicBool,
    harness: Option<crate::client::agent::Selection>,
}
impl Process {
    pub async fn start(shell_path: &Path) -> crate::Result<Arc<Self>> {
        Self::with_output(
            shell_path,
            std::env::current_dir()?,
            OutputStore::start_connection()?,
        )
        .await
    }
    pub async fn start_with_tty(shell_path: &Path, tty: Option<&str>) -> crate::Result<Arc<Self>> {
        Self::start_with_harness(shell_path, tty, None).await
    }

    pub(crate) async fn start_with_harness(
        shell_path: &Path,
        tty: Option<&str>,
        harness: Option<&crate::client::agent::Selection>,
    ) -> crate::Result<Arc<Self>> {
        let observer = match tty {
            Some(terminal) => Some(crate::observer::Pool::new(terminal).await?),
            None => None,
        };
        Self::with_observer(
            shell_path,
            std::env::current_dir()?,
            OutputStore::start_connection()?,
            observer,
            harness.cloned(),
        )
        .await
    }

    pub async fn with_output(
        shell_path: &Path,
        root: PathBuf,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        Self::with_observer(shell_path, root, output, None, None).await
    }
    async fn with_observer(
        shell_path: &Path,
        root: PathBuf,
        output: Arc<OutputStore>,
        observer: Option<Arc<crate::observer::Pool>>,
        harness: Option<crate::client::agent::Selection>,
    ) -> crate::Result<Arc<Self>> {
        if let Some(observer) = &observer {
            output.set_observer(observer);
        }
        let shell = shell::Shell::with_terminal(shell_path, root, output.clone())?;
        let registry = Registry::new();
        registry.replace(
            Provider::Terminal,
            terminal_tools().as_array().unwrap().clone(),
        )?;
        registry.replace(Provider::Exec, shell::Shell::tools())?;
        registry.replace(Provider::Ui, crate::client::interface::tools())?;
        registry.replace(Provider::Harnesses, lazy::tools())?;
        let backend = Arc::new(Self {
            observer,
            shell,
            registry,
            output,
            drivers: StdMutex::new(BTreeMap::new()),
            diagnostics: StdMutex::new(lazy::initial_diagnostics()),
            loads: lazy::Loads::default(),
            closed: AtomicBool::new(false),
            chat_enabled: AtomicBool::new(true),
            harness,
        });
        Ok(backend)
    }
    pub(crate) fn configure_chat(&self, enabled: bool) -> Result<(), String> {
        self.registry.replace(
            Provider::Ui,
            crate::client::interface::tools_for_chat(enabled),
        )?;
        self.chat_enabled.store(enabled, Ordering::Release);
        Ok(())
    }

    pub(crate) fn chat_enabled(&self) -> bool {
        self.chat_enabled.load(Ordering::Acquire)
    }

    fn admit_chat_tool(&self, name: &str) -> Result<(), String> {
        if let Some(error) = crate::wire::retired_tool_error(name) {
            return Err(error.into());
        }
        if !self.chat_enabled() && crate::wire::human_chat_tool(name) {
            return Err(crate::wire::CHAT_DISABLED.into());
        }
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn observe_exec_projection(
        &self,
    ) -> std::sync::mpsc::Receiver<crate::backend::events::ProjectedEvent> {
        self.shell.observe_projection()
    }

    pub fn output(&self) -> Arc<OutputStore> {
        self.output.clone()
    }
    /// Wait only for explicitly requested optional loads; startup does not probe them.
    pub async fn wait_discovery(&self) {
        self.loads.wait().await;
    }
    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        self.registry.catalog()
    }

    async fn load(self: &Arc<Self>, provider: Provider, refresh: bool) -> Result<Value, String> {
        self.registry.ensure_open()?;
        let backend = Arc::clone(self);
        let pending = self.loads.request(provider, refresh, async move {
            backend.registry.ensure_open()?;
            let name = provider.label();
            {
                let mut diagnostics = backend.diagnostics.lock().unwrap();
                let diagnostic = diagnostics.entry(provider).or_insert_with(
                    || json!({"provider":name,"tools":0,"executable":null,"version":null}),
                );
                // Retain the last observed version while an explicit refresh
                // reuses a live driver. Observation itself never probes it.
                diagnostic["status"] = json!("loading");
                diagnostic["activation"] = json!("explicit");
            }
            let result = backend.refresh(provider, name).await;
            crate::memory::release_unused_heap();
            result
        })?;
        pending.await
    }
    pub fn terminal(&self) -> Arc<Terminal> {
        self.shell.primary()
    }
    pub fn terminals(&self) -> Arc<Terminals> {
        self.shell.clone()
    }

    async fn refresh(&self, provider: Provider, name: &str) -> Result<Value, String> {
        let outcome = self.refresh_peer().await;
        let diagnostic = match outcome {
            Ok(value) => value,
            Err(error) => {
                self.registry.invalidate(provider);
                let old = self.drivers.lock().unwrap().remove(&provider);
                if let Some(old) = old {
                    old.shutdown().await
                }
                json!({"provider":name,"status":"unavailable","reason":error,"tools":0,"executable":null,"version":null})
            }
        };
        let changed = {
            let mut diagnostics = self.diagnostics.lock().unwrap();
            let changed = diagnostics
                .get(&provider)
                .is_none_or(|previous| diagnostic_changed(previous, &diagnostic));
            diagnostics.insert(provider, diagnostic.clone());
            changed
        };
        if changed {
            crate::client::render::status(
                "harness",
                if diagnostic["status"] == "ready" {
                    "Result"
                } else {
                    "Warning"
                },
                &format!(
                    "{name} {} · {} tools{}",
                    diagnostic["status"].as_str().unwrap_or("unavailable"),
                    diagnostic["tools"],
                    diagnostic["reason"]
                        .as_str()
                        .map(|s| format!(" · {s}"))
                        .unwrap_or_default()
                ),
            );
        }
        if diagnostic["status"] == "ready" {
            self.registry.ensure_open()?;
            Ok(diagnostic)
        } else {
            Err(format!(
                "optional {name} harness is unavailable: {}; after fixing its configuration, explicitly retry ssmcp.harnesses.load with provider={name} and refresh=true",
                diagnostic["reason"]
                    .as_str()
                    .unwrap_or("initialization failed")
            ))
        }
    }

    async fn refresh_peer(&self) -> Result<Value, String> {
        let settings = self.harness.as_ref().and_then(|agent| agent.peer.as_ref())
            .ok_or("ssmcp peer requires explicit startup configuration: --load-harness ssmcp -- --endpoint URL --token-env NAME")?;
        let existing = self.drivers.lock().unwrap().get(&Provider::Peer).cloned();
        let driver = match existing {
            Some(driver) => driver,
            None => peer::Peer::start(settings.clone()).await?,
        };
        let catalog = match driver.compact_tools().await {
            Ok(catalog) => catalog,
            Err(error) => {
                driver.shutdown().await;
                return Err(error);
            }
        };
        let count = catalog.len();
        let details = driver.details();
        let published = {
            let mut drivers = self.drivers.lock().unwrap();
            drivers.insert(Provider::Peer, driver.clone());
            let result = self.registry.replace_catalog(Provider::Peer, catalog);
            if result.is_err() {
                drivers.remove(&Provider::Peer);
            }
            result
        };
        if let Err(error) = published {
            driver.shutdown().await;
            return Err(error);
        }
        Ok(
            json!({"provider":"ssmcp","status":"ready","executable":null,"version":null,
            "tools":count,"details":details}),
        )
    }

    pub async fn perform(self: &Arc<Self>, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::ListTools { cursor } => {
                let tools = self.registry.snapshot()?;
                crate::catalog::page(
                    &tools,
                    cursor.as_deref(),
                    &crate::catalog::fingerprint(&tools),
                )
            }
            Operation::CallTool { name, arguments } => {
                self.admit_chat_tool(&name)?;
                let tool = self.registry.lookup(&name)?;
                self.dispatch(&tool, arguments).await
            }
            operation => {
                if self.closed.load(Ordering::Acquire) {
                    return Err("client connection is closed".into());
                }
                let (name, arguments) = match operation {
                    Operation::Read {
                        cursor,
                        max_bytes,
                        wait_ms,
                    } => (
                        "terminal.read",
                        json!({"cursor":cursor,"max_bytes":max_bytes,"wait_ms":wait_ms}),
                    ),
                    Operation::Write { data } => ("terminal.write", json!({"data":data})),
                    Operation::Resize { rows, cols } => {
                        ("terminal.resize", json!({"rows":rows,"cols":cols}))
                    }
                    Operation::Status {} => ("terminal.status", json!({})),
                    _ => return Err("not a terminal operation".into()),
                };
                self.shell.call(name, arguments).await.map(result)
            }
        }
    }
    /// Compatibility API for callers that already resolved a registered tool.
    pub async fn dispatch(
        self: &Arc<Self>,
        tool: &RegisteredTool,
        arguments: Value,
    ) -> Result<Value, String> {
        self.dispatch_call(Call {
            id: tool.id.clone(),
            native_name: tool.native_name.clone(),
            provider: tool.provider,
            generation: tool.generation,
            arguments,
        })
        .await
    }

    async fn dispatch_call(self: &Arc<Self>, call: Call) -> Result<Value, String> {
        let Call {
            id,
            native_name,
            provider,
            generation,
            arguments,
        } = call;
        self.admit_chat_tool(&native_name)?;
        self.registry
            .validate_route(&id, provider, generation, &native_name)?;
        match provider {
            Provider::Ui => crate::client::interface::call(&native_name, arguments, &self.shell)
                .await
                .map(result),
            Provider::Terminal => self.shell.call(&native_name, arguments).await.map(result),
            Provider::Exec => self
                .shell
                .call_scoped(
                    &native_name,
                    arguments,
                    crate::runtime::call_scope::current_source(),
                )
                .await
                .map(result),
            Provider::Harnesses => match native_name.as_str() {
                "ssmcp.harnesses.list" => {
                    if !arguments.as_object().is_some_and(|a| a.is_empty()) {
                        return Err("ssmcp.harnesses.list takes no arguments".into());
                    }
                    let harnesses: Vec<_> =
                        self.diagnostics.lock().unwrap().values().cloned().collect();
                    Ok(result(json!({
                        "harnesses":harnesses,
                        "generation":self.registry.generation(),
                        "activation":"explicit",
                        "default":"native_tools"
                    })))
                }
                "ssmcp.harnesses.load" => {
                    let (provider, refresh) = lazy::parse(arguments)?;
                    self.load(provider, refresh).await.map(result)
                }
                _ => Err("unknown optional harness primitive".into()),
            },
            provider @ Provider::Peer => {
                let driver = self
                    .drivers
                    .lock()
                    .unwrap()
                    .get(&provider)
                    .cloned()
                    .ok_or("provider is no longer connected; refresh tools")?;
                self.registry
                    .validate_route(&id, provider, generation, &native_name)?;
                driver.call(&native_name, arguments).await
            }
            _ => Err("provider belongs to the workspace runtime".into()),
        }
    }
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.registry.close();
        self.loads.shutdown().await;
        let drivers = std::mem::take(&mut *self.drivers.lock().unwrap());
        let futures = drivers
            .into_values()
            .map(|driver| async move { driver.shutdown().await });
        tokio::join!(
            self.shell.shutdown(),
            futures_util::future::join_all(futures)
        );
        if let Some(observer) = &self.observer {
            observer.shutdown();
        }
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.registry.close();
        self.loads.abort();
    }
}

pub(crate) fn diagnostic_changed(previous: &Value, current: &Value) -> bool {
    fn stable(mut value: Value) -> Value {
        if let Some(details) = value.get_mut("details").and_then(Value::as_object_mut) {
            // Discovery attempt counters and transient polling activity do not
            // change a provider's advertised capability or availability.
            details.remove("generation");
            details.remove("refreshing");
        }
        value
    }
    stable(previous.clone()) != stable(current.clone())
}

fn terminal_tools() -> Value {
    let mut tools = json!([
        {
            "name":"terminal.read",
            "description":"Read terminal output from a byte cursor. Reuse the returned cursor for the next read; output is retained in a bounded buffer.",
            "inputSchema":{"type":"object","properties":{
                "cursor":{"type":"integer","minimum":0,"default":0},
                "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_READ,"default":16384},
                "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":0}
            },"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        },
        {
            "name":"terminal.write",
            "description":"Write text or control characters to the terminal. Append a newline to run a command; use \\u0003 for Ctrl-C. The limit is 16384 UTF-8 bytes.",
            "inputSchema":{"type":"object","properties":{"data":{"type":"string","maxLength":MAX_WRITE}},"required":["data"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}
        },
        {
            "name":"terminal.resize",
            "description":"Set terminal rows and columns.",
            "inputSchema":{"type":"object","properties":{
                "rows":{"type":"integer","minimum":1,"maximum":1000},
                "cols":{"type":"integer","minimum":1,"maximum":1000}
            },"required":["rows","cols"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}
        },
        {
            "name":"terminal.status",
            "description":"Inspect terminal process state and output cursor.",
            "inputSchema":{"type":"object","additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }
    ]);
    for tool in tools.as_array_mut().unwrap() {
        let preferred = match tool["name"].as_str().unwrap() {
            "terminal.read" => "ssmcp.exec.read",
            "terminal.write" => "ssmcp.exec.write",
            "terminal.resize" => "ssmcp.exec.resize",
            _ => "ssmcp.exec.status",
        };
        tool["description"] = json!(format!(
            "Compatibility entry point; prefer {preferred} with the same job_id. {}",
            tool["description"].as_str().unwrap()
        ));
        if tool["inputSchema"].get("properties").is_none() {
            tool["inputSchema"]["properties"] = json!({});
        }
        tool["inputSchema"]["properties"]["terminal_id"] = json!({
            "type":"string","minLength":1,"maxLength":128,"default":"main",
            "description":"PTY job_id from exec.list or compatibility terminal.open/list. Omission always selects the protected main job, even when the local view changes."
        });
    }
    tools
        .as_array_mut()
        .unwrap()
        .extend(Terminals::management_tools());
    tools
}

impl NativePlugin for Process {
    const ID: &'static str = "ssmcp.process";

    fn providers(&self) -> &'static [Provider] {
        &[
            Provider::Terminal,
            Provider::Exec,
            Provider::Ui,
            Provider::Harnesses,
            Provider::Peer,
        ]
    }

    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![
            (
                Provider::Terminal,
                terminal_tools().as_array().unwrap().clone(),
            ),
            (Provider::Exec, shell::Shell::tools()),
            (
                Provider::Ui,
                crate::client::interface::tools_for_chat(self.chat_enabled()),
            ),
            (Provider::Harnesses, lazy::tools()),
        ]
    }

    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move { self.dispatch_call(call).await })
    }

    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            Process::shutdown(&self).await;
            Ok(json!({"status":"stopped"}))
        })
    }
}
