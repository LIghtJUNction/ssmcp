//! One authenticated shell with independently discovered native providers.
pub mod cc;
pub mod codex;
pub mod events;
pub mod opencode;
pub mod pi;
pub mod playwright;
pub mod process;
pub mod rpc;
pub mod shell;

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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, sync::Notify, task::JoinHandle};

#[derive(Clone)]
enum Driver {
    Codex(Arc<codex::Codex>),
    Claude(Arc<cc::Claude>),
    Pi(Arc<pi::Pi>),
    OpenCode(Arc<opencode::OpenCode>),
    Playwright(Arc<playwright::Playwright>),
}
impl Driver {
    async fn tools(&self) -> Result<Vec<Value>, String> {
        match self {
            Self::Codex(d) => d.background_tools().await,
            Self::Claude(d) => d.tools().await,
            Self::Pi(d) => d.tools().await,
            Self::OpenCode(d) => d.tools().await,
            Self::Playwright(d) => d.tools().await,
        }
    }
    async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match self {
            Self::Codex(d) => d.call(name, args).await,
            Self::Claude(d) => d.call(name, args).await,
            Self::Pi(d) => d.call(name, args).await,
            Self::OpenCode(d) => d.call(name, args).await,
            Self::Playwright(d) => d.call(name, args).await,
        }
    }
    async fn shutdown(&self) {
        match self {
            Self::Codex(d) => d.shutdown().await,
            Self::Claude(d) => d.shutdown().await,
            Self::Pi(d) => d.shutdown().await,
            Self::OpenCode(d) => d.shutdown().await,
            Self::Playwright(d) => d.shutdown().await,
        }
    }
    async fn details(&self) -> Value {
        match self {
            Self::Codex(d) => d.diagnostics().await,
            Self::Pi(d) => d.diagnostics().await,
            Self::Claude(_) => json!({"scope":"native MCP tools"}),
            Self::OpenCode(_) => json!({"scope":"native OpenAPI operations"}),
            Self::Playwright(d) => d.diagnostics(),
        }
    }
}

pub struct Backend {
    terminal: Arc<Terminals>,
    shell: Arc<shell::Shell>,
    pub(crate) registry: Arc<Registry>,
    output: Arc<OutputStore>,
    drivers: StdMutex<BTreeMap<Provider, Driver>>,
    diagnostics: StdMutex<BTreeMap<Provider, Value>>,
    tasks: StdMutex<Vec<JoinHandle<()>>>,
    closed: AtomicBool,
    initial: AtomicUsize,
    discovered: Notify,
}
impl Backend {
    pub async fn start(shell_path: &Path) -> crate::Result<Arc<Self>> {
        Self::with_output(
            shell_path,
            std::env::current_dir()?,
            OutputStore::new_connection()?,
        )
        .await
    }
    pub async fn with_output(
        shell_path: &Path,
        root: PathBuf,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        let terminal = Terminals::new(shell_path, root.clone(), output.clone())?;
        let shell = shell::Shell::with_workspace(root, output.clone())?;
        let registry = Registry::new();
        registry.replace(
            Provider::Terminal,
            terminal_tools().as_array().unwrap().clone(),
        )?;
        registry.replace(Provider::Exec, shell::Shell::tools())?;
        registry.replace(Provider::Harnesses,vec![json!({
            "name":"ssmcp.harnesses.list",
            "description":"Inspect actual discovery status, installed version, protocol scope and diagnostics for each native provider. A ready RPC bridge does not imply enumerable model tools.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        })])?;
        let backend = Arc::new(Self {
            terminal,
            shell,
            registry,
            output,
            drivers: StdMutex::new(BTreeMap::new()),
            diagnostics: StdMutex::new(BTreeMap::new()),
            tasks: StdMutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            initial: AtomicUsize::new(5),
            discovered: Notify::new(),
        });
        for (provider, name) in [
            (Provider::Codex, "codex"),
            (Provider::Claude, "claude"),
            (Provider::Pi, "pi"),
            (Provider::OpenCode, "opencode"),
            (Provider::Playwright, "playwright"),
        ] {
            backend
                .diagnostics
                .lock()
                .unwrap()
                .insert(provider, json!({"status":"discovering","provider":name}));
            let weak = Arc::downgrade(&backend);
            let task = tokio::spawn(async move {
                let mut first = true;
                let mut installed: Option<(PathBuf, u64, std::time::SystemTime)> = None;
                loop {
                    let Some(backend) = weak.upgrade() else { break };
                    if backend.closed.load(Ordering::Acquire) {
                        break;
                    }
                    backend.refresh(provider, name, &mut installed).await;
                    if first {
                        first = false;
                        backend.initial.fetch_sub(1, Ordering::AcqRel);
                        backend.discovered.notify_waiters();
                    }
                    drop(backend);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            });
            backend.tasks.lock().unwrap().push(task);
        }
        Ok(backend)
    }
    pub fn output(&self) -> Arc<OutputStore> {
        self.output.clone()
    }
    pub async fn wait_discovery(&self) {
        loop {
            let notified = self.discovered.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.initial.load(Ordering::Acquire) == 0 || self.closed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        self.registry.catalog()
    }
    pub fn terminal(&self) -> Arc<Terminal> {
        self.terminal.primary()
    }
    pub fn terminals(&self) -> Arc<Terminals> {
        self.terminal.clone()
    }

    async fn refresh(
        &self,
        provider: Provider,
        name: &str,
        installed: &mut Option<(PathBuf, u64, std::time::SystemTime)>,
    ) {
        let mut detected_program = None;
        let mut detected_version = None;
        let outcome = async {
            let program = if provider == Provider::Playwright { playwright::installed()? } else { crate::program::executable(name)? };
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
            if driver.is_none() {
                detected_version = Some(if provider == Provider::Playwright { probe_command(playwright::command(&program).await?).await? } else { probe_version(&program).await? });
                let output = self.output.clone();
                let future = async {
                    match provider {
                        Provider::Playwright => playwright::Playwright::start_with_output(&program, output).await.map(Driver::Playwright),
                        Provider::Codex => codex::Codex::start_with_output(program.as_os_str(), &[], output).await.map(Driver::Codex),
                        Provider::Claude => cc::Claude::start_with_output(program.as_os_str(), &[], output).await.map(Driver::Claude),
                        Provider::Pi => pi::Pi::start_discovered_with_output(program.as_os_str(), &[], output).await.map(Driver::Pi),
                        Provider::OpenCode => {
                            let endpoint = std::env::var("SSMCP_OPENCODE_URL").map_err(|_| "OpenCode is installed; set SSMCP_OPENCODE_URL to an already-running native loopback service. Discovery does not start scheduled work.")?;
                            let password = std::env::var("OPENCODE_PASSWORD").map_err(|_| "OpenCode attachment requires the native OPENCODE_PASSWORD")?;
                            opencode::OpenCode::attach_with_output(&endpoint, &password, output).await.map(Driver::OpenCode)
                        }
                        _ => unreachable!(),
                    }
                };
                let opened = tokio::time::timeout(Duration::from_secs(90), future).await
                    .map_err(|_| "native protocol handshake timed out".to_owned())?
                    .map_err(|e| e.to_string())?;
                driver = Some(opened);
            } else {
                detected_version = self.diagnostics.lock().unwrap().get(&provider)
                    .and_then(|d| d["version"].as_str().map(str::to_owned));
            }
            let driver = driver.unwrap();
            let catalog = match tokio::time::timeout(Duration::from_secs(75), driver.tools()).await {
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
            let published = {
                let mut drivers = self.drivers.lock().unwrap();
                drivers.insert(provider, driver.clone());
                let published = self.registry.replace(provider, catalog);
                if published.is_err() { drivers.remove(&provider); }
                published
            };
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
                "detect",
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
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::ListTools { cursor } => page(self.registry.catalog()?, cursor.as_deref()),
            Operation::CallTool { name, arguments } => {
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
    pub async fn dispatch(&self, tool: &RegisteredTool, arguments: Value) -> Result<Value, String> {
        self.registry.validate(tool)?;
        match tool.provider {
            Provider::Terminal => self
                .terminal
                .call(&tool.native_name, arguments)
                .await
                .map(result),
            Provider::Exec => self
                .shell
                .call(&tool.native_name, arguments)
                .await
                .map(result),
            Provider::Harnesses => {
                if !arguments.as_object().is_some_and(|a| a.is_empty()) {
                    return Err("ssmcp.harnesses.list takes no arguments".into());
                }
                let harnesses: Vec<_> =
                    self.diagnostics.lock().unwrap().values().cloned().collect();
                Ok(result(
                    json!({"harnesses":harnesses,"generation":self.registry.generation()}),
                ))
            }
            provider if provider.is_harness() || provider == Provider::Playwright => {
                let driver = self
                    .drivers
                    .lock()
                    .unwrap()
                    .get(&provider)
                    .cloned()
                    .ok_or("provider is no longer connected; refresh tools")?;
                self.registry.validate(tool)?;
                driver.call(&tool.native_name, arguments).await
            }
            _ => Err("provider belongs to the workspace runtime".into()),
        }
    }
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.registry.close();
        self.discovered.notify_waiters();
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        for task in &tasks {
            task.abort()
        }
        for task in tasks {
            let _ = task.await;
        }
        let drivers = std::mem::take(&mut *self.drivers.lock().unwrap());
        let futures = drivers
            .into_values()
            .map(|driver| async move { driver.shutdown().await });
        tokio::join!(
            self.terminal.shutdown(),
            self.shell.shutdown(),
            futures_util::future::join_all(futures)
        );
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.registry.close();
        for task in self
            .tasks
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
        {
            task.abort()
        }
    }
}

fn diagnostic_changed(previous: &Value, current: &Value) -> bool {
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

/// Convert a shell capability into a validated private wire operation.
pub(crate) fn terminal_operation(name: &str, arguments: Value) -> Result<Operation, String> {
    let op = match name {
        "terminal.read" => "read",
        "terminal.write" => "write",
        "terminal.resize" => "resize",
        "terminal.status" => "status",
        _ => return Err(format!("unknown shell capability: {name}")),
    };
    let mut arguments = arguments
        .as_object()
        .cloned()
        .ok_or("tool arguments must be an object")?;
    if arguments.contains_key("op") {
        return Err("unexpected tool argument: op".into());
    }
    arguments.insert("op".into(), Value::String(op.into()));
    let operation: Operation = serde_json::from_value(Value::Object(arguments))
        .map_err(|e| format!("invalid shell arguments: {e}"))?;
    operation.validate()?;
    Ok(operation)
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

// The cursor binds to catalog names/schema content, so list changes cannot silently skip tools.
pub(crate) fn page(tools: impl AsRef<[Value]>, cursor: Option<&str>) -> Result<Value, String> {
    let tools = tools.as_ref();
    let encoded = serde_json::to_vec(tools).map_err(|e| e.to_string())?;
    // A non-cryptographic fingerprint is sufficient: this is stale-page detection, not auth.
    let hash = encoded.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    page_with_fingerprint(tools, cursor, &format!("{hash:016x}"))
}

pub(crate) fn page_with_fingerprint(
    tools: &[Value],
    cursor: Option<&str>,
    fingerprint: &str,
) -> Result<Value, String> {
    let start = match cursor {
        None => 0,
        Some(cursor) => {
            let (requested, index) = cursor.split_once(':').ok_or("invalid tool cursor")?;
            if requested != fingerprint {
                return Err("tool catalog changed; restart tools/list without a cursor".into());
            }
            index.parse::<usize>().map_err(|_| "invalid tool cursor")?
        }
    };
    if start > tools.len() {
        return Err("tool cursor is beyond the catalog".into());
    }
    let mut end = start;
    let mut bytes = 0;
    while end < tools.len() && end - start < 64 {
        let length = tools[end].to_string().len();
        if length > 896 * 1024 {
            return Err("one tool schema exceeds the transport limit".into());
        }
        if end > start && bytes + length > 512 * 1024 {
            break;
        }
        bytes += length;
        end += 1;
    }
    let mut page = json!({"tools":&tools[start..end]});
    if end < tools.len() {
        page["nextCursor"] = json!(format!("{fingerprint}:{end}"));
    }
    Ok(page)
}

pub fn result(value: Value) -> Value {
    let mut output = json!({"content":[{"type":"text","text":value.to_string()}],"isError":false});
    if value.is_object() {
        output["structuredContent"] = value;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_attempt_metadata_does_not_repeat_a_stable_provider_status() {
        let previous = json!({"provider":"codex","status":"ready","tools":1342,
            "details":{"generation":1,"refreshing":false,"status":"ready","reason":null}});
        let mut polled = previous.clone();
        polled["details"]["generation"] = 99.into();
        polled["details"]["refreshing"] = true.into();
        assert!(!diagnostic_changed(&previous, &polled));
        let mut changed = polled.clone();
        changed["tools"] = 1341.into();
        assert!(diagnostic_changed(&previous, &changed));
        changed = polled;
        changed["details"]["status"] = "unavailable".into();
        changed["details"]["reason"] = "native discovery failed".into();
        assert!(diagnostic_changed(&previous, &changed));
    }

    #[test]
    fn shell_capabilities_use_the_same_validated_dispatch_inside_code_mode() {
        assert!(matches!(
            terminal_operation("terminal.read", json!({})),
            Ok(Operation::Read {
                cursor: 0,
                max_bytes: 16384,
                wait_ms: 0
            })
        ));
        for arguments in [
            json!({"max_bytes":MAX_READ+1}),
            json!({"wait_ms":MAX_WAIT_MS+1}),
            json!({"cursor":-1}),
            json!({"op":"status"}),
            json!({"extra":true}),
        ] {
            assert!(terminal_operation("terminal.read", arguments).is_err());
        }
        assert!(
            terminal_operation("terminal.write", json!({"data":"界".repeat(MAX_WRITE/3+1)}))
                .is_err()
        );
        assert!(terminal_operation("terminal.resize", json!({"rows":0,"cols":80})).is_err());
        assert!(terminal_operation("unknown", json!({})).is_err());
    }

    #[test]
    fn tool_pagination_detects_catalog_changes() {
        let tools: Vec<_> = (0..150)
            .map(|n| json!({"name":format!("tool{n}")}))
            .collect();
        let first = page(tools.clone(), None).unwrap();
        assert_eq!(first["tools"].as_array().unwrap().len(), 64);
        let second = page(tools.clone(), first["nextCursor"].as_str()).unwrap();
        assert_eq!(second["tools"][0]["name"], "tool64");
        let third = page(tools.clone(), second["nextCursor"].as_str()).unwrap();
        assert_eq!(third["tools"].as_array().unwrap().len(), 22);
        assert!(third.get("nextCursor").is_none());
        assert!(page(&tools[1..], first["nextCursor"].as_str()).is_err());
    }
}
