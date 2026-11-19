//! Kernel boundary for the packaged native plugins.
//!
//! Registry authority, output policy and connection ownership stay with Runtime.
//! Native state and its execution/cleanup belong to the plugins. All zplugin
//! objects and tasks remain on one lazy local executor, never one thread per tool
//! or plugin. The existing synchronous dynamic-extension workers are separate.
mod abi;

use crate::{
    backend::rpc::HarnessCall,
    output::OutputStore,
    registry::{Provider, RegisteredTool, Registry},
};
use serde_json::Value;
use std::{
    any::Any,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    pin::Pin,
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::{JoinSet, LocalSet},
};

/// Shared, already-authorized resources supplied when constructing a plugin.
/// This Rust-only context never crosses a dynamic-library or wire boundary.
pub(crate) struct ConstructorContext {
    pub root: PathBuf,
    pub context_roots: Vec<PathBuf>,
    pub registry: Arc<Registry>,
    pub output: Arc<OutputStore>,
}

/// The exact registered route, without retaining another copy of its schemas.
pub(crate) struct Call {
    pub id: String,
    pub provider: Provider,
    pub native_name: String,
    pub generation: u64,
    pub arguments: Value,
}

impl Call {
    fn validate(&self, registry: &Registry) -> Result<(), String> {
        registry.validate_route(&self.id, self.provider, self.generation, &self.native_name)
    }
}

pub(crate) type PluginFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + 'static>>;

/// Implementations own capabilities, rather than forwarding names to Runtime.
/// The Rust helper is adapted to the checked, versioned zplugin C ABI below.
pub(crate) trait NativePlugin: Send + Sync + 'static {
    const ID: &'static str;
    fn providers(&self) -> &'static [Provider];
    fn catalog(&self) -> Vec<(Provider, Vec<Value>)>;
    fn call(self: Arc<Self>, call: Call) -> PluginFuture;
    fn shutdown(self: Arc<Self>) -> PluginFuture;
}

#[derive(Clone)]
pub(crate) struct Definition {
    id: &'static str,
    providers: &'static [Provider],
    header: &'static zplugin::abi::Header,
    seed: Arc<dyn Any + Send + Sync>,
}

impl Definition {
    pub fn new<P: NativePlugin>(plugin: Arc<P>) -> Self {
        Self {
            id: P::ID,
            providers: plugin.providers(),
            header: abi::descriptor::<P>(),
            seed: plugin,
        }
    }

    fn instance(&self) -> Result<abi::CapabilityProxy<'static>, String> {
        abi::instantiate(self.header, self.seed.clone())
    }

    fn catalog(&self) -> Result<Vec<(Provider, Vec<Value>)>, String> {
        // An ephemeral ABI instance can read declarations on this thread. It
        // starts no async work and is destroyed here, before returning plain data.
        let instance = self.instance()?;
        let bytes = instance.catalog().map_err(abi::error)?;
        abi::catalog(&bytes, self.id, self.providers)
    }
}

type ShutdownReport = Vec<(String, Result<Value, String>)>;
type LocalInstance = Rc<RefCell<abi::CapabilityProxy<'static>>>;

#[derive(Default)]
struct Completion {
    result: Mutex<Option<ShutdownReport>>,
    ready: Notify,
}

impl Completion {
    fn finish(&self, result: ShutdownReport) {
        *self
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(result);
        self.ready.notify_waiters();
    }

    async fn wait(&self) -> ShutdownReport {
        loop {
            let notified = self.ready.notified();
            // Register before checking the result so shutdown cannot be missed
            // between the check and the first poll of this notification.
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

struct RequestScope {
    harness: Option<HarnessCall>,
    request_id: Option<String>,
}

impl RequestScope {
    fn capture() -> Self {
        Self {
            harness: HarnessCall::current(),
            request_id: crate::observability::REQUEST_ID.try_with(Clone::clone).ok(),
        }
    }

    async fn run<F: Future>(&self, future: F) -> F::Output {
        let observed = async {
            if let Some(harness) = &self.harness {
                harness.scope(future).await
            } else {
                future.await
            }
        };
        if let Some(request_id) = &self.request_id {
            crate::observability::REQUEST_ID
                .scope(request_id.clone(), observed)
                .await
        } else {
            observed.await
        }
    }
}

struct Request {
    number: u64,
    route: usize,
    call: Call,
    scope: RequestScope,
    reply: oneshot::Sender<Result<Value, String>>,
    permit: OwnedSemaphorePermit,
}

const MAX_CALLS: usize = 128;

pub(crate) struct NativeHost {
    registry: Arc<Registry>,
    definitions: Arc<Vec<Definition>>,
    routes: BTreeMap<Provider, usize>,
    next: Arc<AtomicU64>,
    sender: Mutex<Option<mpsc::Sender<Request>>>,
    stop: watch::Sender<bool>,
    completion: Arc<Completion>,
    calls: Arc<Semaphore>,
}

impl NativeHost {
    pub fn new(registry: Arc<Registry>, definitions: Vec<Definition>) -> Result<Self, String> {
        let mut routes = BTreeMap::new();
        let mut names = BTreeSet::new();
        for (index, definition) in definitions.iter().enumerate() {
            if definition.id.is_empty() || !names.insert(definition.id) {
                return Err("native plugin IDs must be nonempty and unique".into());
            }
            for provider in definition.providers {
                if routes.insert(*provider, index).is_some() {
                    return Err(format!(
                        "native provider {} is owned by more than one plugin",
                        provider.label()
                    ));
                }
            }
            abi::validate(definition.header)?;
        }
        let (stop, _) = watch::channel(false);
        Ok(Self {
            registry,
            definitions: Arc::new(definitions),
            routes,
            next: Arc::new(AtomicU64::new(1)),
            sender: Mutex::new(None),
            stop,
            completion: Arc::new(Completion::default()),
            calls: Arc::new(Semaphore::new(MAX_CALLS)),
        })
    }

    pub fn catalogs(&self) -> Result<Vec<(Provider, Vec<Value>)>, String> {
        let mut catalogs = Vec::new();
        for definition in self.definitions.iter() {
            catalogs.extend(definition.catalog()?);
        }
        Ok(catalogs)
    }

    pub fn contains(&self, provider: Provider) -> bool {
        self.routes.contains_key(&provider)
    }

    fn sender(&self) -> Result<mpsc::Sender<Request>, String> {
        let mut sender = self
            .sender
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(sender) = sender.as_ref() {
            return Ok(sender.clone());
        }
        let (tx, rx) = mpsc::channel(MAX_CALLS);
        let definitions = self.definitions.clone();
        let registry = self.registry.clone();
        let next = self.next.clone();
        let stop = self.stop.subscribe();
        let completion = self.completion.clone();
        std::thread::Builder::new()
            .name("ssmcp-native-plugins".into())
            .spawn(move || {
                // ABI instances/tasks are constructed inside this closure, not
                // moved here through an unsafe Send wrapper.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| format!("cannot start native plugin reactor: {error}"))?;
                    let local = LocalSet::new();
                    let report = runtime.block_on(local.run_until(run(
                        definitions,
                        registry,
                        next,
                        rx,
                        stop,
                    )));
                    drop(local);
                    drop(runtime);
                    Ok::<_, String>(report)
                }));
                let report = match outcome {
                    Ok(Ok(report)) => report,
                    Ok(Err(error)) => vec![("native-host".into(), Err(error))],
                    Err(_) => vec![(
                        "native-host".into(),
                        Err("native plugin executor panicked; cleanup is incomplete".into()),
                    )],
                };
                completion.finish(report);
            })
            .map_err(|error| format!("cannot start native plugin executor: {error}"))?;
        *sender = Some(tx.clone());
        Ok(tx)
    }

    pub async fn call(&self, tool: &RegisteredTool, arguments: Value) -> Result<Value, String> {
        self.registry.validate(tool)?;
        if *self.stop.borrow() {
            return Err("native plugin host is shutting down".into());
        }
        let route = *self
            .routes
            .get(&tool.provider)
            .ok_or("provider is not mounted in the native plugin host")?;
        let permit = self
            .calls
            .clone()
            .try_acquire_owned()
            .map_err(|_| "native plugin call limit reached (128 queued or active calls)")?;
        let number = next_number(&self.next)?;
        let (reply, receiver) = oneshot::channel();
        let request = Request {
            number,
            route,
            call: Call {
                id: tool.id.clone(),
                provider: tool.provider,
                native_name: tool.native_name.clone(),
                generation: tool.generation,
                arguments,
            },
            scope: RequestScope::capture(),
            reply,
            permit,
        };
        self.sender()?
            .try_send(request)
            .map_err(|_| "native plugin executor is unavailable or its queue is full")?;
        receiver
            .await
            .map_err(|_| "native plugin executor stopped before replying".to_owned())?
    }

    /// Stop admission, cancel/drop every in-flight ABI task, then concurrently
    /// run each plugin's real cleanup. Repeated callers observe the same report.
    pub async fn shutdown(&self) -> ShutdownReport {
        self.stop.send_replace(true);
        if let Err(error) = self.sender() {
            return vec![("native-host".into(), Err(error))];
        }
        self.completion.wait().await
    }
}

impl Drop for NativeHost {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        // The thread owns plugin seeds until cleanup completes, including when
        // the connection's explicit async shutdown future itself is cancelled.
        let _ = self.sender();
    }
}

fn next_number(next: &AtomicU64) -> Result<u64, String> {
    next.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        value.checked_add(1)
    })
    .map_err(|_| "native plugin request IDs exhausted".into())
}

fn instance(
    index: usize,
    definitions: &[Definition],
    instances: &mut BTreeMap<usize, LocalInstance>,
) -> Result<LocalInstance, String> {
    if let Some(instance) = instances.get(&index) {
        return Ok(instance.clone());
    }
    let instance = Rc::new(RefCell::new(definitions[index].instance()?));
    instances.insert(index, instance.clone());
    Ok(instance)
}

async fn stopping(stop: &mut watch::Receiver<bool>) {
    if !*stop.borrow() {
        let _ = stop.changed().await;
    }
}

async fn run(
    definitions: Arc<Vec<Definition>>,
    registry: Arc<Registry>,
    next: Arc<AtomicU64>,
    mut receiver: mpsc::Receiver<Request>,
    mut stop: watch::Receiver<bool>,
) -> ShutdownReport {
    let mut instances = BTreeMap::new();
    let mut tasks = JoinSet::new();
    let mut failures = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = stopping(&mut stop) => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    failures.push(("native-host".into(), Err(format!("native plugin task failed: {error}"))));
                }
            }
            request = receiver.recv() => {
                let Some(request) = request else { break };
                if request.reply.is_closed() { continue; }
                if let Err(error) = request.call.validate(&registry) {
                    let _ = request.reply.send(Err(error));
                    continue;
                }
                let plugin = match instance(request.route, &definitions, &mut instances) {
                    Ok(plugin) => plugin,
                    Err(error) => {
                        let _ = request.reply.send(Err(error));
                        continue;
                    }
                };
                let registry = registry.clone();
                let stop = stop.clone();
                tasks.spawn_local(async move {
                    let Request {number, call, scope, reply, permit, ..} = request;
                    scope.run(execute(plugin, registry, stop, number, call, reply, permit)).await;
                });
            }
        }
    }
    receiver.close();
    while let Ok(request) = receiver.try_recv() {
        let _ = request
            .reply
            .send(Err("native plugin host is shutting down".into()));
    }
    while let Some(completed) = tasks.join_next().await {
        if let Err(error) = completed {
            failures.push((
                "native-host".into(),
                Err(format!("native plugin task failed: {error}")),
            ));
        }
    }
    let mut cleanup = JoinSet::new();
    for (index, definition) in definitions.iter().enumerate() {
        let plugin = match instance(index, &definitions, &mut instances) {
            Ok(plugin) => plugin,
            Err(error) => {
                failures.push((definition.id.into(), Err(error)));
                continue;
            }
        };
        let number = match next_number(&next) {
            Ok(number) => number,
            Err(error) => {
                failures.push((definition.id.into(), Err(error)));
                continue;
            }
        };
        let id = definition.id.to_owned();
        cleanup.spawn_local(async move {
            let result = async {
                let task = plugin.borrow_mut().shutdown(number).map_err(abi::error)?;
                let completed = task.await.map_err(abi::error);
                let result = plugin.borrow_mut().finish(number).map_err(abi::error);
                completed?;
                abi::result(&result?)
            }
            .await;
            (id, result)
        });
    }
    while let Some(completed) = cleanup.join_next().await {
        match completed {
            Ok(result) => failures.push(result),
            Err(error) => failures.push((
                "native-host".into(),
                Err(format!("native plugin cleanup task failed: {error}")),
            )),
        }
    }
    drop(instances);
    failures.sort_by(|a, b| a.0.cmp(&b.0));
    failures
}

async fn execute(
    plugin: LocalInstance,
    registry: Arc<Registry>,
    mut stop: watch::Receiver<bool>,
    number: u64,
    call: Call,
    mut reply: oneshot::Sender<Result<Value, String>>,
    _permit: OwnedSemaphorePermit,
) {
    let result = async {
        if reply.is_closed() || *stop.borrow() {
            return Err("native plugin call cancelled before dispatch".into());
        }
        // Recheck after the queue and immediately before entering the plugin.
        call.validate(&registry)?;
        let input = abi::input(&call)?;
        let mut task = plugin
            .borrow_mut()
            .start(number, &call.native_name, &input)
            .map_err(abi::error)?;
        drop(input);
        drop(call);
        let completed = tokio::select! {
            biased;
            _ = reply.closed() => Err("native plugin caller cancelled".to_owned()),
            _ = stopping(&mut stop) => Err("native plugin host is shutting down".to_owned()),
            outcome = &mut task => outcome.map_err(abi::error),
        };
        if completed.is_err() {
            let _ = task.cancel();
        }
        drop(task);
        // Finish consumes the plugin-owned bytes even if the receiver vanished
        // at completion. Cancelled pending tasks have already released their slot.
        let result = plugin.borrow_mut().finish(number).map_err(abi::error);
        completed?;
        abi::result(&result?)
    }
    .await;
    let _ = reply.send(result);
}

#[cfg(test)]
mod tests;
