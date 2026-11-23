//! Process, terminal and optional-harness capability plugin.
//! Owns provider state, registration, native dispatch and cleanup.
mod lazy;
use super::super::native_host::{Call, NativePlugin, PluginFuture};
use crate::backend::{cc, codex, opencode, peer, pi, process, result, shell};
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
    time::Duration,
};
use tokio::io::AsyncReadExt;

#[derive(Clone)]
enum Driver {
    Codex(Arc<codex::Codex>),
    Claude(Arc<cc::Claude>),
    Pi(Arc<pi::Pi>),
    OpenCode(Arc<opencode::OpenCode>),
    Peer(Arc<peer::Peer>),
}
impl Driver {
    async fn tools(&self) -> Result<Vec<crate::catalog::ToolDescriptor>, String> {
        let tools = match self {
            Self::Codex(d) => return d.background_catalog().await,
            Self::Claude(d) => return d.compact_tools().await,
            Self::Pi(d) => d.tools().await,
            Self::OpenCode(d) => return d.compact_tools().await,
            Self::Peer(d) => return d.compact_tools().await,
        }?;
        crate::catalog::compact(tools)
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match self {
            Self::Codex(d) => d.call(name, args).await,
            Self::Claude(d) => d.call(name, args).await,
            Self::Pi(d) => d.call(name, args).await,
            Self::OpenCode(d) => d.call(name, args).await,
            Self::Peer(d) => d.call(name, args).await,
        }
    }
    async fn shutdown(&self) {
        match self {
            Self::Codex(d) => d.shutdown().await,
            Self::Claude(d) => d.shutdown().await,
            Self::Pi(d) => d.shutdown().await,
            Self::OpenCode(d) => d.shutdown().await,
            Self::Peer(d) => d.shutdown().await,
        }
    }
    async fn details(&self) -> Value {
        match self {
            Self::Codex(d) => d.diagnostics().await,
            Self::Pi(d) => d.diagnostics().await,
            Self::Claude(_) => json!({"scope":"native MCP tools"}),
            Self::OpenCode(_) => json!({"scope":"native OpenAPI operations"}),
            Self::Peer(d) => d.details(),
        }
    }
}

type Installed = (PathBuf, u64, std::time::SystemTime);

fn reject_self_program(program: &Path) -> Result<(), String> {
    let own = std::env::current_exe().map_err(|_| "could not identify the running ssmcp binary")?;
    let same_path = program
        .canonicalize()
        .ok()
        .zip(own.canonicalize().ok())
        .is_some_and(|(a, b)| a == b);
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        let selected = std::fs::metadata(program).map_err(|e| e.to_string())?;
        #[cfg(target_os = "linux")]
        let running = std::fs::metadata("/proc/self/exe").map_err(|e| e.to_string())?;
        #[cfg(not(target_os = "linux"))]
        let running = std::fs::metadata(&own).map_err(|e| e.to_string())?;
        selected.dev() == running.dev() && selected.ino() == running.ino()
    };
    #[cfg(not(unix))]
    let same_file = false;
    if same_path || same_file {
        return Err("an agent executable resolves to the running ssmcp; use --load-harness ssmcp with an explicit authenticated MCP peer endpoint".into());
    }
    Ok(())
}

pub struct Process {
    terminal: Arc<Terminals>,
    observer: Option<Arc<crate::observer::Pool>>,
    shell: Arc<shell::Shell>,
    pub(crate) registry: Arc<Registry>,
    output: Arc<OutputStore>,
    drivers: StdMutex<BTreeMap<Provider, Driver>>,
    diagnostics: StdMutex<BTreeMap<Provider, Value>>,
    loads: lazy::Loads,
    installed: StdMutex<BTreeMap<Provider, Installed>>,
    closed: AtomicBool,
    chat_enabled: AtomicBool,
    harness: Option<crate::client::agent::Selection>,
}
impl Process {
    pub(crate) fn local_command_stats(&self) -> shell::LocalCommandStats {
        self.shell.local_command_stats()
    }

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
        let terminal = Terminals::new(shell_path, root.clone(), output.clone())?;
        let shell = shell::Shell::with_workspace(root, output.clone())?;
        let registry = Registry::new();
        registry.replace(
            Provider::Terminal,
            terminal_tools().as_array().unwrap().clone(),
        )?;
        registry.replace(Provider::Exec, shell::Shell::tools())?;
        registry.replace(Provider::Ui, crate::client::interface::tools())?;
        registry.replace(Provider::Harnesses, lazy::tools())?;
        let backend = Arc::new(Self {
            terminal,
            observer,
            shell,
            registry,
            output,
            drivers: StdMutex::new(BTreeMap::new()),
            diagnostics: StdMutex::new(lazy::initial_diagnostics()),
            loads: lazy::Loads::default(),
            installed: StdMutex::new(BTreeMap::new()),
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
            let mut installed = backend.installed.lock().unwrap().get(&provider).cloned();
            let result = backend.refresh(provider, name, &mut installed).await;
            {
                let mut known = backend.installed.lock().unwrap();
                match installed {
                    Some(identity) => {
                        known.insert(provider, identity);
                    }
                    None => {
                        known.remove(&provider);
                    }
                }
            }
            crate::memory::release_unused_heap();
            result
        })?;
        pending.await
    }
    pub fn terminal(&self) -> Arc<Terminal> {
        self.terminal.primary()
    }
    pub fn terminals(&self) -> Arc<Terminals> {
        self.terminal.clone()
    }

    async fn refresh(
        self: &Arc<Self>,
        provider: Provider,
        name: &str,
        installed: &mut Option<Installed>,
    ) -> Result<Value, String> {
        let mut detected_program = None;
        let mut detected_version = None;
        let outcome = async {
            if provider == Provider::Peer {
                return self.refresh_peer().await;
            }
            let arguments = self.harness.as_ref().filter(|agent| agent.provider == provider)
                .map_or(&[][..], |agent| agent.arguments.as_slice());
            let program = crate::program::executable(name)?;
            reject_self_program(&program)?;
            let metadata = std::fs::metadata(&program).map_err(|e| e.to_string())?;
            detected_program = Some(program.clone());
            let identity = (program.clone(), metadata.len(), metadata.modified().map_err(|e| e.to_string())?);
            if installed.as_ref() != Some(&identity) {
                self.registry.invalidate(provider);
                let old = self.drivers.lock().unwrap().remove(&provider);
                if let Some(old) = old { old.shutdown().await; }
                *installed = None;
            }
            let mut driver = self.drivers.lock().unwrap().get(&provider).cloned();
            let new_driver = driver.is_none();
            if new_driver {
                detected_version = Some(probe_version(&program).await?);
                let output = self.output.clone();
                let future = async {
                    match provider {
                        Provider::Codex => codex::Codex::start_with_output(program.as_os_str(), arguments, output).await.map(Driver::Codex),
                        Provider::Claude => cc::Claude::start_with_output(program.as_os_str(), arguments, output).await.map(Driver::Claude),
                        Provider::Pi => pi::Pi::start_discovered_with_output(program.as_os_str(), &[], output).await.map(Driver::Pi),
                        Provider::OpenCode => {
                            let endpoint = std::env::var("SSMCP_OPENCODE_URL").map_err(|_| "OpenCode is installed; set SSMCP_OPENCODE_URL to an already-running native loopback service. Discovery does not start scheduled work.")?;
                            let password = std::env::var("OPENCODE_PASSWORD").map_err(|_| "OpenCode attachment requires the native OPENCODE_PASSWORD")?;
                            opencode::OpenCode::attach_with_output(&endpoint, &password, output).await.map(Driver::OpenCode)
                        }
                        _ => unreachable!(),
                    }
                };
                let opening = async {
                    tokio::time::timeout(Duration::from_secs(90), future).await
                        .map_err(|_| "native protocol handshake timed out".to_owned())?
                        .map_err(|e| e.to_string())
                };
                driver = Some(opening.await?);
            } else {
                detected_version = self.diagnostics.lock().unwrap().get(&provider)
                    .and_then(|d| d["version"].as_str().map(str::to_owned));
            }
            let driver = driver.unwrap();
            let listing = tokio::time::timeout(Duration::from_secs(75), driver.tools());
            let listed = listing.await;
            let catalog = match listed {
                Ok(Ok(catalog)) => catalog,
                result => {
                    driver.shutdown().await;
                    return Err(match result {
                        Ok(Err(error)) => error,
                        _ => "native capability enumeration timed out".into(),
                    });
                }
            };
            let count = catalog.len();
            let details = driver.details().await;
            // Install the destination before publishing its current-generation routes.
            let publish = || {
                let mut drivers = self.drivers.lock().unwrap();
                drivers.insert(provider, driver.clone());
                let published = self.registry.replace_catalog(provider, catalog);
                if published.is_err() { drivers.remove(&provider); }
                published
            };
            let published = publish();
            if let Err(error) = published { driver.shutdown().await; return Err(error); }
            *installed = Some(identity);
            Ok(json!({"provider":name,"status":"ready","executable":program,"version":detected_version,"tools":count,"details":details}))
        }.await;
        let diagnostic = match outcome {
            Ok(value) => value,
            Err(error) => {
                self.registry.invalidate(provider);
                let old = self.drivers.lock().unwrap().remove(&provider);
                if let Some(old) = old {
                    old.shutdown().await
                }
                *installed = None;
                json!({"provider":name,"status":"unavailable","reason":error,"tools":0,"executable":detected_program,"version":detected_version})
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
            None => Driver::Peer(peer::Peer::start(settings.clone()).await?),
        };
        let catalog = match driver.tools().await {
            Ok(catalog) => catalog,
            Err(error) => {
                driver.shutdown().await;
                return Err(error);
            }
        };
        let count = catalog.len();
        let details = driver.details().await;
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
                self.terminal.primary().perform(operation).await.map(result)
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
            Provider::Ui => crate::client::interface::call(&native_name, arguments, &self.terminal)
                .await
                .map(result),
            Provider::Terminal => self
                .terminal
                .call(&native_name, arguments)
                .await
                .map(result),
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
            provider if provider.is_harness() || provider == Provider::Peer => {
                let driver = self
                    .drivers
                    .lock()
                    .unwrap()
                    .get(&provider)
                    .cloned()
                    .ok_or("provider is no longer connected; refresh tools")?;
                self.registry
                    .validate_route(&id, provider, generation, &native_name)?;
                let observation = match &self.observer {
                    Some(pool) => {
                        pool.native_begin(provider.label(), &native_name, &arguments)
                            .await?
                    }
                    None => None,
                };
                let result = driver.call(&native_name, arguments).await;
                if let (Some(pool), Some(lease)) = (&self.observer, observation) {
                    pool.native_finish(lease, &result);
                }
                result
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
            self.terminal.shutdown(),
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

async fn probe_version(program: &Path) -> Result<String, String> {
    probe_command(tokio::process::Command::new(program)).await
}

async fn probe_command(mut command: tokio::process::Command) -> Result<String, String> {
    use std::process::Stdio;
    crate::program::configure(&mut command);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = process::Managed::spawn(&mut command).map_err(|e| e.to_string())?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("version stdout unavailable")?
        .take(16385);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("version stderr unavailable")?
        .take(16385);
    let mut out = Vec::new();
    let mut err = Vec::new();
    let checked = tokio::time::timeout(Duration::from_secs(5), async {
        let (a, b, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            process.wait()
        );
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        let status = status.map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("version check failed ({status})"));
        }
        if out.len() > 16384 || err.len() > 16384 {
            return Err("version output exceeds 16 KiB".into());
        }
        let text = String::from_utf8_lossy(if out.is_empty() { &err } else { &out });
        let version = text
            .lines()
            .find(|line| line.chars().any(|c| c.is_ascii_digit()) && line.contains('.'))
            .filter(|line| line.len() <= 512)
            .ok_or("version check returned no recognizable installed version")?;
        Ok(version.trim().to_owned())
    })
    .await
    .map_err(|_| "version check timed out".to_owned())
    .and_then(|v| v);
    process.shutdown().await;
    checked
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
        if tool["inputSchema"].get("properties").is_none() {
            tool["inputSchema"]["properties"] = json!({});
        }
        tool["inputSchema"]["properties"]["terminal_id"] = json!({
            "type":"string","minLength":1,"maxLength":128,"default":"main",
            "description":"Independent terminal ID from terminal.open/list. Omission always selects main, even when the local view changes."
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
            Provider::Codex,
            Provider::Claude,
            Provider::Pi,
            Provider::OpenCode,
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

#[cfg(test)]
mod agent_tests {
    use super::*;

    #[test]
    fn agent_provider_refuses_the_current_binary_before_any_version_probe() {
        assert!(reject_self_program(&std::env::current_exe().unwrap()).is_err());
        #[cfg(target_os = "linux")]
        assert!(reject_self_program(Path::new("/proc/self/exe")).is_err());
    }
}
