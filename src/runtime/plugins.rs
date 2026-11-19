//! Explicit trusted native extensions. No discovery, worker, or library at startup.
//! Native code is not a sandbox and synchronous calls cannot be forcibly cancelled.
use crate::{
    backend,
    registry::{Provider, RegisteredTool, Registry},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use zharness::plugin::ExtensionApi;

const MAX_PLUGINS: usize = 4;
const MAX_NATIVE_LOADS: usize = 16;
static NATIVE_LOADS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

struct WorkerPermit<'a>(&'a AtomicUsize);
impl<'a> WorkerPermit<'a> {
    fn reserve(counter: &'a AtomicUsize) -> Result<Self, String> {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                if used < MAX_PLUGINS {
                    Some(used + 1)
                } else {
                    None
                }
            })
            .map(|_| Self(counter))
            .map_err(|_| {
                "native plugin worker limit reached (4 active or stopping workers per process)"
                    .into()
            })
    }
}
impl Drop for WorkerPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn reserve_load(counter: &AtomicUsize) -> Result<(), String> {
    counter.try_update(Ordering::AcqRel, Ordering::Acquire, |used|
        if used < MAX_NATIVE_LOADS { Some(used + 1) } else { None })
        .map(|_| ())
        .map_err(|_| "native library load budget exhausted (16 attempts per process); restart the client explicitly to reclaim pinned images".into())
}

const MAX_TOOLS: usize = 64;
const MAX_INPUT: usize = 64 * 1024;
const MAX_OUTPUT: usize = 256 * 1024;
const QUEUE: usize = 4;
const TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_OPERATION: &str = "ssmcp.catalog.v1";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Load {
    path: String,
    namespace: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Unload {
    namespace: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Catalog {
    schema_version: u32,
    tools: Vec<Value>,
}

struct Prepared {
    tools: Vec<Value>,
    routes: BTreeMap<String, String>,
}
enum Command {
    Call {
        operation: String,
        input: Vec<u8>,
        reply: oneshot::Sender<Result<Value, String>>,
    },
    Stop,
}
type Ready = oneshot::Receiver<Result<Prepared, String>>;

struct Worker {
    sender: mpsc::Sender<Command>,
    stop: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
}
struct LoadLease(Option<Arc<Worker>>);
impl Drop for LoadLease {
    fn drop(&mut self) {
        if let Some(worker) = &self.0 {
            worker.stop();
        }
    }
}
struct Running<'a> {
    running: Arc<AtomicBool>,
    _permit: WorkerPermit<'a>,
}
impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        // The permit is released after the worker has returned and all native
        // ABI values have been dropped, never when its manager is dropped.
    }
}
impl Worker {
    fn start(path: PathBuf, namespace: String) -> Result<(Arc<Self>, Ready), String> {
        let (sender, receiver) = mpsc::channel(QUEUE);
        let (initial, ready) = oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let worker = Arc::new(Self {
            sender,
            stop: stop.clone(),
            running: running.clone(),
        });
        let permit = WorkerPermit::reserve(&ACTIVE_WORKERS)?;
        reserve_load(&NATIVE_LOADS)?;
        let result = std::thread::Builder::new()
            .name(format!("plugin-{namespace}"))
            .spawn(move || {
                let _running = Running {
                    running,
                    _permit: permit,
                };
                run_worker(&path, &namespace, receiver, initial, &stop);
            });
        if let Err(error) = result {
            NATIVE_LOADS.fetch_sub(1, Ordering::AcqRel);
            worker.running.store(false, Ordering::Release);
            return Err(format!("cannot start plugin worker: {error}"));
        }
        // The thread owns all !Send/!Sync ABI values. The detached handle is not
        // a claim that native code can be joined or killed within a deadline.
        Ok((worker, ready))
    }
    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.sender.try_send(Command::Stop);
    }
    async fn call(&self, operation: String, arguments: Value) -> Result<Value, String> {
        if self.stop.load(Ordering::Acquire) {
            return Err("plugin is stopping".into());
        }
        let input = serde_json::to_vec(&arguments).map_err(|error| error.to_string())?;
        if input.len() > MAX_INPUT {
            return Err("plugin arguments exceed 64 KiB".into());
        }
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(Command::Call {
                operation,
                input,
                reply,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => "plugin request queue is full".to_owned(),
                mpsc::error::TrySendError::Closed(_) => "plugin worker is stopped".to_owned(),
            })?;
        tokio::time::timeout(TIMEOUT, receiver).await
            .map_err(|_| "plugin call timed out; native code may still be running; no automatic retry or rollback".to_owned())?
            .map_err(|_| "plugin worker stopped before replying".to_owned())?
    }
}

fn run_worker(
    path: &Path,
    namespace: &str,
    mut receiver: mpsc::Receiver<Command>,
    initial: oneshot::Sender<Result<Prepared, String>>,
    stop: &AtomicBool,
) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    // SAFETY: loading is exposed only by the explicit trusted-code API. The
    // caller selects the library and accepts its initializers/ABI obligations.
    // ABI validation does not confine this native code's OS privileges.
    let library = match unsafe { zplugin::Plugin::load_path(path, zplugin::UnloadPolicy::Pinned) } {
        Ok(library) => library,
        Err(error) => {
            let _ = initial.send(Err(format!("native plugin load failed: {error}")));
            return;
        }
    };
    let mut extension = match ExtensionApi::create(&library) {
        Ok(extension) => extension,
        Err(error) => {
            let _ = initial.send(Err(format!("plugin extension ABI mismatch: {error}")));
            return;
        }
    };
    let mut output = vec![0; MAX_OUTPUT];
    let catalog_input = json!({"namespace":namespace}).to_string();
    let prepared = extension
        .call_text(CATALOG_OPERATION, catalog_input.as_bytes(), &mut output)
        .map_err(|error| format!("plugin catalog call failed: {error}"))
        .and_then(|text| prepare_catalog(namespace, text));
    let successful = prepared.is_ok();
    if initial.send(prepared).is_err() || !successful || stop.load(Ordering::Acquire) {
        return;
    }
    while let Some(command) = receiver.blocking_recv() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Command::Call {
            operation,
            input,
            reply,
        } = command
        else {
            break;
        };
        // A cancelled queued request has not started and can safely be skipped.
        // Cancellation after this check cannot undo an in-progress native call.
        if reply.is_closed() {
            continue;
        }
        let result = extension
            .call_text(&operation, &input, &mut output)
            .map_err(|error| format!("plugin operation failed: {error}"))
            .and_then(parse_result);
        let _ = reply.send(result);
    }
    // proxy drops before library. Pinned native images remain resident until
    // process exit; stopping this worker is not dlclose or forced cancellation.
}

fn parse_result(text: &str) -> Result<Value, String> {
    let result: Value = serde_json::from_str(text)
        .map_err(|error| format!("invalid plugin result JSON: {error}"))?;
    if !result.is_object()
        || !result.get("content").is_some_and(Value::is_array)
        || result
            .get("isError")
            .is_some_and(|value| !value.is_boolean())
    {
        return Err(
            "plugin result must be an MCP object with content array and optional boolean isError"
                .into(),
        );
    }
    Ok(result)
}
fn valid_component(value: &str, maximum: usize, dots: bool) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || b"_-".contains(&byte) || (dots && byte == b'.')
        })
}
fn prepare_catalog(namespace: &str, text: &str) -> Result<Prepared, String> {
    if !valid_component(namespace, 32, false) {
        return Err("plugin namespace must contain 1–32 ASCII letters, digits, _ or -".into());
    }
    if text.len() > MAX_OUTPUT {
        return Err("plugin catalog exceeds 256 KiB".into());
    }
    let catalog: Catalog =
        serde_json::from_str(text).map_err(|error| format!("invalid plugin catalog: {error}"))?;
    if catalog.schema_version != 1 || catalog.tools.len() > MAX_TOOLS {
        return Err("plugin catalog requires schema_version 1 and at most 64 tools".into());
    }
    let mut tools = Vec::with_capacity(catalog.tools.len());
    let mut routes = BTreeMap::new();
    let mut names = BTreeSet::new();
    for mut descriptor in catalog.tools {
        let native = descriptor
            .get("name")
            .and_then(Value::as_str)
            .ok_or("plugin tool has no name")?
            .to_owned();
        if !valid_component(&native, 80, true) || native == CATALOG_OPERATION {
            return Err("plugin tool name is invalid or reserved".into());
        }
        if !descriptor.get("inputSchema").is_some_and(Value::is_object) {
            return Err("plugin tool requires its original inputSchema object".into());
        }
        if !names.insert(native.clone()) {
            return Err(format!("duplicate plugin tool: {native}"));
        }
        let public = format!("plugins.{namespace}.{native}");
        if public.len() > 128 {
            return Err("plugin route exceeds 128 bytes".into());
        }
        descriptor["name"] = public.clone().into();
        routes.insert(public, native);
        tools.push(descriptor);
    }
    Ok(Prepared { tools, routes })
}
fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("explicit plugin library path is required".into());
    }
    let input = Path::new(path);
    let relative = !input.is_absolute();
    if relative
        && input
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err("relative plugin path cannot contain parent or platform prefixes".into());
    }
    let root = std::fs::canonicalize(root).map_err(|error| format!("workspace root: {error}"))?;
    let path = std::fs::canonicalize(if relative {
        root.join(input)
    } else {
        input.to_owned()
    })
    .map_err(|error| format!("plugin library path: {error}"))?;
    if relative && !path.starts_with(&root) {
        return Err("relative plugin library escapes the workspace".into());
    }
    let metadata = std::fs::metadata(&path).map_err(|error| format!("plugin library: {error}"))?;
    if !metadata.is_file() {
        return Err("plugin library must be a regular file".into());
    }
    if path.to_str().is_none() {
        return Err("resolved plugin library path must be valid UTF-8".into());
    }
    Ok(path)
}

struct Slot {
    path: PathBuf,
    phase: &'static str,
    error: Option<String>,
    worker: Arc<Worker>,
    prepared: Prepared,
}
#[derive(Default)]
struct State {
    closed: bool,
    slots: BTreeMap<String, Slot>,
}
pub(super) struct Plugins {
    root: PathBuf,
    registry: Arc<Registry>,
    state: Mutex<State>,
}
impl Plugins {
    pub(super) fn new(root: PathBuf, registry: Arc<Registry>) -> Self {
        Self {
            root,
            registry,
            state: Mutex::new(State::default()),
        }
    }
    pub(super) fn tools() -> Vec<Value> {
        vec![
            json!({"name":"ssmcp.plugins.list","description":"List explicitly loaded native plugins without scanning files or starting workers.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
            json!({"name":"ssmcp.plugins.load","description":"Explicitly load trusted native code at an absolute path or workspace-relative path. Its initializers run with client privileges; ABI checks are not a sandbox. Uses the zharness Extension v1 ABI and ssmcp.catalog.v1 tool catalog. No automatic discovery, model, package installation, or retry.","inputSchema":{"type":"object","required":["path","namespace"],"properties":{"path":{"type":"string","minLength":1},"namespace":{"type":"string","minLength":1,"maxLength":32}},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
            json!({"name":"ssmcp.plugins.unload","description":"Disable one plugin's routes and request its worker to stop. In-progress native calls cannot be forcibly cancelled; pinned library images remain resident until process exit.","inputSchema":{"type":"object","required":["namespace"],"properties":{"namespace":{"type":"string"}},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        ]
    }
    fn catalog(state: &State, excluding: Option<&str>) -> Vec<Value> {
        let mut tools = Self::tools();
        for (name, slot) in &state.slots {
            if slot.phase == "ready" && excluding != Some(name.as_str()) {
                tools.extend(slot.prepared.tools.iter().cloned());
            }
        }
        tools
    }
    fn receipt(namespace: &str, slot: &Slot) -> Value {
        json!({"namespace":namespace,"path":slot.path,"status":slot.phase,
               "error":slot.error,"tools":slot.prepared.tools.len(),
               "worker_running":slot.worker.running.load(Ordering::Acquire),
               "trusted_native_code":true,"library_resident_until_process_exit":true})
    }
    pub(super) async fn perform(
        &self,
        tool: &RegisteredTool,
        arguments: Value,
    ) -> Result<Value, String> {
        match tool.native_name.as_str() {
            "ssmcp.plugins.list" => {
                let _: Empty = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                Ok(backend::result(
                    json!({"plugins":state.slots.iter().map(|(n,s)|Self::receipt(n,s)).collect::<Vec<_>>(),
                    "closed":state.closed,"limits":{"plugins":MAX_PLUGINS,"active_workers_per_process":MAX_PLUGINS,"active_workers_used":ACTIVE_WORKERS.load(Ordering::Acquire),"tools_per_plugin":MAX_TOOLS,
                    "queued_calls_per_plugin":QUEUE,"input_bytes":MAX_INPUT,"output_bytes":MAX_OUTPUT,"native_load_attempts_per_process":MAX_NATIVE_LOADS,"native_load_attempts_used":NATIVE_LOADS.load(Ordering::Acquire)}}),
                ))
            }
            "ssmcp.plugins.load" => {
                let args: Load = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.load(args).await.map(backend::result)
            }
            "ssmcp.plugins.unload" => {
                let args: Unload = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                self.unload(&args.namespace).map(backend::result)
            }
            _ => {
                let (worker, native) = {
                    let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.closed {
                        return Err("plugin manager is closed".into());
                    }
                    self.registry.validate(tool)?;
                    state
                        .slots
                        .values()
                        .find_map(|slot| {
                            (slot.phase == "ready")
                                .then(|| {
                                    slot.prepared
                                        .routes
                                        .get(&tool.native_name)
                                        .map(|native| (slot.worker.clone(), native.clone()))
                                })
                                .flatten()
                        })
                        .ok_or("plugin tool is no longer registered")?
                };
                worker.call(native, arguments).await
            }
        }
    }
    async fn load(&self, args: Load) -> Result<Value, String> {
        if !valid_component(&args.namespace, 32, false) {
            return Err("invalid plugin namespace".into());
        }
        let root = self.root.clone();
        let path = tokio::task::spawn_blocking(move || resolve(&root, &args.path))
            .await
            .map_err(|error| error.to_string())??;
        let namespace = args.namespace;
        let (worker, ready) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.closed {
                return Err("plugin manager is closed".into());
            }
            // Failed/stopping workers continue occupying a slot until they really
            // exit. Repeated timeouts therefore cannot create unlimited threads.
            state.slots.retain(|_, slot| {
                slot.phase == "ready" || slot.worker.running.load(Ordering::Acquire)
            });
            if let Some(slot) = state.slots.get(&namespace) {
                if slot.phase == "ready" && slot.path == path {
                    return Ok(Self::receipt(&namespace, slot));
                }
                return Err(
                    "plugin namespace is already in use; unload it and wait for its worker to stop"
                        .into(),
                );
            }
            if state.slots.len() >= MAX_PLUGINS {
                return Err("native plugin limit reached (4 active or stopping workers)".into());
            }
            let (worker, ready) = Worker::start(path.clone(), namespace.clone())?;
            state.slots.insert(
                namespace.clone(),
                Slot {
                    path,
                    phase: "loading",
                    error: None,
                    worker: worker.clone(),
                    prepared: Prepared {
                        tools: Vec::new(),
                        routes: BTreeMap::new(),
                    },
                },
            );
            (worker, ready)
        };
        let mut lease = LoadLease(Some(worker.clone()));
        let prepared = match tokio::time::timeout(TIMEOUT, ready).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("plugin worker stopped during loading".into()),
            Err(_) => Err("plugin load timed out; native code may still be running; the reserved slot remains bounded".into()),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed
            || !state
                .slots
                .get(&namespace)
                .is_some_and(|slot| Arc::ptr_eq(&slot.worker, &worker) && slot.phase == "loading")
        {
            worker.stop();
            return Err("plugin load was cancelled by unload or shutdown".into());
        }
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                worker.stop();
                let slot = state.slots.get_mut(&namespace).unwrap();
                slot.phase = "failed";
                slot.error = Some(error.clone());
                return Err(error);
            }
        };
        let mut catalog = Self::catalog(&state, None);
        catalog.extend(prepared.tools.iter().cloned());
        // Preserve the previous plugin catalog atomically on any rejected update.
        if let Err(error) = self.registry.replace_preserving(Provider::Plugins, catalog) {
            worker.stop();
            let slot = state.slots.get_mut(&namespace).unwrap();
            slot.phase = "failed";
            slot.error = Some(error.clone());
            return Err(error);
        }
        let slot = state.slots.get_mut(&namespace).unwrap();
        slot.prepared = prepared;
        slot.phase = "ready";
        lease.0 = None;
        Ok(Self::receipt(&namespace, slot))
    }
    fn unload(&self, namespace: &str) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("plugin manager is closed".into());
        }
        if !state.slots.contains_key(namespace) {
            return Err("unknown plugin namespace".into());
        }
        self.registry
            .replace_preserving(Provider::Plugins, Self::catalog(&state, Some(namespace)))?;
        let slot = state.slots.get_mut(namespace).unwrap();
        slot.phase = "stopping";
        slot.prepared = Prepared {
            tools: Vec::new(),
            routes: BTreeMap::new(),
        };
        slot.worker.stop();
        Ok(Self::receipt(namespace, slot))
    }
    pub(super) async fn shutdown(&self) -> Result<(), String> {
        let workers: Vec<_> = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.closed = true;
            state
                .slots
                .values_mut()
                .map(|slot| {
                    slot.phase = "stopping";
                    slot.worker.stop();
                    slot.worker.clone()
                })
                .collect()
        };
        self.registry.invalidate(Provider::Plugins);
        let stopped = tokio::time::timeout(Duration::from_millis(250), async {
            while workers
                .iter()
                .any(|worker| worker.running.load(Ordering::Acquire))
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        stopped.map_err(|_| "native plugin workers still executing after shutdown request; no forced cancellation or unload".into())
    }
}
impl Drop for Plugins {
    fn drop(&mut self) {
        let state = self.state.get_mut().unwrap_or_else(|e| e.into_inner());
        for slot in state.slots.values() {
            slot.worker.stop();
        }
    }
}
#[cfg(test)]
mod tests;
