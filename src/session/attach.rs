//! Local terminal frontends for an existing background session. Detaching only
//! releases an input lease; it never changes the session's desired state.
pub(crate) mod input;
pub(crate) mod thread_history;
mod zbridge_adapter;
pub use zbridge_adapter::{EventObserver, Frontend, MAX_OBSERVATION_MS, RemoteFrontend};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io::IsTerminal,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};

use super::ipc;
use crate::{
    Result,
    client::interface::history::{self, HistoryId},
    runtime::{
        Runtime,
        call_scope::{DisplayHistory, DisplaySource},
    },
};

const ENDPOINT: &str = "attach.json";
const HISTORY_BYTES: usize = 2 * 1024 * 1024;
const EVENT_BYTES: usize = 64 * 1024;
const PAGE_BYTES: usize = 128 * 1024;
const LEASE_SECONDS: u64 = 15;
static CAPTURE: OnceLock<Mutex<Option<Weak<Capture>>>> = OnceLock::new();
// Ordinary foreground clients also retain the same bounded event journal.
// No listener, persistent file, model or background service is created here.
static PROCESS_CAPTURE: OnceLock<std::result::Result<Arc<Capture>, String>> = OnceLock::new();

#[derive(Default)]
struct Ring {
    next: u64,
    bytes: usize,
    events: VecDeque<(
        u64,
        Box<RawValue>,
        usize,
        Option<thread_history::RecordScope>,
    )>,
}

struct Lease {
    read_only: bool,
    seen: Instant,
}
struct RuntimeSlot {
    runtime: Option<Arc<Runtime>>,
    generation: u64,
}
struct Capture {
    generation: String,
    ring: Arc<Mutex<Ring>>,
    leases: Mutex<BTreeMap<String, Lease>>,
    runtime: Mutex<RuntimeSlot>,
    changed: Arc<Notify>,
    bridge: zbridge_adapter::Guard,
}

impl Capture {
    fn new() -> Result<Self> {
        Ok(Self {
            generation: super::random_suffix()?,
            ring: Arc::new(Mutex::new(Ring::default())),
            leases: Mutex::new(BTreeMap::new()),
            runtime: Mutex::new(RuntimeSlot {
                runtime: None,
                generation: 0,
            }),
            changed: Arc::new(Notify::new()),
            bridge: zbridge_adapter::Guard::default(),
        })
    }

    fn with_journal(history: &Self) -> Self {
        // History outlives an attach listener; input authority and the Runtime
        // must retain their original listener/connection lifetimes.
        Self {
            generation: history.generation.clone(),
            ring: history.ring.clone(),
            changed: history.changed.clone(),
            leases: Mutex::new(BTreeMap::new()),
            runtime: Mutex::new(RuntimeSlot {
                runtime: None,
                generation: 0,
            }),
            bridge: zbridge_adapter::Guard::default(),
        }
    }

    fn push(&self, event: Value) {
        self.push_scoped(event, None);
    }

    fn push_scoped(&self, mut event: Value, scope: Option<thread_history::RecordScope>) {
        let mut bytes = event.to_string().len();
        if bytes > EVENT_BYTES {
            let history = event.get("local_history_id").cloned();
            event = json!({"kind":"event","label":"attach","value":{
                "type":"ssmcp.status","level":"Warning",
                "message":format!("One {bytes}-byte event exceeds attach history's per-event limit; inspect the live output artifact for retained contents.")}});
            if let Some(history) = history {
                event["local_history_id"] = history;
            }
            event["truncated"] = json!(true);
        }
        if let Some(scope) = &scope {
            event["source"] = serde_json::to_value(scope).expect("record scope serializes");
        }
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = ring.next;
        event["sequence"] = json!(cursor);
        let encoded = event.to_string();
        drop(event);
        bytes = encoded.len()
            + scope
                .as_ref()
                .map_or(0, thread_history::RecordScope::retained_bytes);
        // Keep replay payloads compact; expand only the requested page.
        let event = RawValue::from_string(encoded).expect("serialized attach event is valid JSON");
        while ring.bytes + bytes > HISTORY_BYTES || ring.events.len() >= 4096 {
            let Some((_, _, old, _)) = ring.events.pop_front() else {
                break;
            };
            ring.bytes -= old;
        }
        ring.next = ring.next.saturating_add(1);
        ring.bytes += bytes;
        ring.events.push_back((cursor, event, bytes, scope));
        drop(ring);
        self.changed.notify_waiters();
    }

    fn push_event(&self, label: &str, value: &Value, history: Option<&HistoryId>) {
        let mut frame =
            json!({"kind":"event","label":label,"value":crate::audit::event(value.clone())});
        if let Some(history) = history {
            frame["local_history_id"] = json!(history.as_str());
        }
        self.push(frame);
    }

    fn push_scoped_event(&self, label: &str, value: &Value, source: &DisplaySource) {
        let mut frame =
            json!({"kind":"event","label":label,"value":crate::audit::event(value.clone())});
        if let Some(history) = source.history.local_id() {
            frame["local_history_id"] = json!(history.as_str());
        }
        self.push_scoped(frame, thread_history::RecordScope::from_source(source));
    }

    fn open(&self, read_only: bool) -> Result<String> {
        let mut leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        leases.retain(|_, value| value.seen.elapsed() < Duration::from_secs(LEASE_SECONDS));
        if leases.len() >= 32 {
            return Err("this session already has 32 attached clients".into());
        }
        if !read_only && leases.values().any(|value| !value.read_only) {
            return Err("another attached client owns input; detach it or use --read-only".into());
        }
        let key = format!("{}{}", super::random_suffix()?, super::random_suffix()?);
        leases.insert(
            key.clone(),
            Lease {
                read_only,
                seen: Instant::now(),
            },
        );
        Ok(key)
    }

    fn touch(&self, key: &str, write: bool) -> Result<()> {
        let mut leases = self.leases.lock().unwrap_or_else(|e| e.into_inner());
        leases.retain(|_, value| value.seen.elapsed() < Duration::from_secs(LEASE_SECONDS));
        let lease = leases
            .get_mut(key)
            .ok_or("attach lease expired; attach again")?;
        if write && lease.read_only {
            return Err("this attach connection is read-only".into());
        }
        lease.seen = Instant::now();
        Ok(())
    }

    fn runtime(&self, expected: u64) -> Result<Arc<Runtime>> {
        let slot = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        if slot.generation != expected {
            return Err("session backend changed; input was not replayed, attach again".into());
        }
        slot.runtime
            .clone()
            .ok_or_else(|| "session is connecting; terminal input is not available yet".into())
    }

    fn snapshot(&self) -> (Option<Arc<Runtime>>, u64, u64) {
        let slot = self.runtime.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = self.ring.lock().unwrap_or_else(|e| e.into_inner()).next;
        (slot.runtime.clone(), slot.generation, cursor)
    }

    async fn read(&self, cursor: u64) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(1000);
        let mut page = loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
                if cursor > ring.next {
                    return Err("attach cursor belongs to an earlier session generation".into());
                }
                if cursor < ring.next || tokio::time::Instant::now() >= deadline {
                    let start = ring.events.front().map_or(ring.next, |e| e.0);
                    let mut next = cursor.max(start);
                    let mut bytes = 0;
                    let mut events = Vec::new();
                    for (sequence, event, size, _) in &ring.events {
                        if *sequence < next {
                            continue;
                        }
                        if events.len() >= 128 || bytes + size > PAGE_BYTES {
                            break;
                        }
                        events.push(serde_json::from_str::<Value>(event.get())?);
                        bytes += size;
                        next = sequence + 1;
                    }
                    break json!({"generation":self.generation,"events":events,
                        "next_cursor":next,"start_cursor":start,"truncated":cursor < start});
                }
            }
            let _ = tokio::time::timeout_at(deadline, changed).await;
        };
        // The ring lock is released before observing history. Selection comes
        // from the backend, independently of the owner of any replayed event.
        page["active_history_id"] =
            json!(history::current_owner().as_ref().map(|owner| owner.id()));
        Ok(page)
    }
}

fn capture() -> Option<Arc<Capture>> {
    CAPTURE
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .upgrade()
}

fn history_capture() -> Option<Arc<Capture>> {
    capture().or_else(|| {
        PROCESS_CAPTURE
            .get_or_init(|| {
                Capture::new()
                    .map(Arc::new)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
            .ok()
            .cloned()
    })
}

/// Called in the renderer's serialized emission path. Native payloads remain
/// unchanged; this private display replay contains the usual redacted copy.
pub(crate) fn record_event(label: &str, value: &Value, history: Option<&HistoryId>) {
    if let Some(capture) = history_capture() {
        capture.push_event(label, value, history);
    }
}

pub(crate) fn record_scoped_event(label: &str, value: &Value, source: &DisplaySource) {
    if value["type"] == "ssmcp.code_mode"
        || source
            .call
            .as_ref()
            .and_then(|call| call.native_tool())
            .is_some_and(thread_history::observer)
    {
        return;
    }
    if source.call.is_none() {
        record_event(label, value, source.history.local_id());
        return;
    }
    if let Some(capture) = history_capture() {
        capture.push_scoped_event(label, value, source);
    }
}

pub(crate) fn record_terminal(terminal_id: &str, bytes: &[u8]) {
    if let Some(capture) = capture() {
        let generation = {
            let slot = capture.runtime.lock().unwrap_or_else(|e| e.into_inner());
            // A shell may print its prompt during Backend::start, before the
            // Runtime can be bound. It belongs to the upcoming backend epoch.
            if slot.runtime.is_some() {
                slot.generation
            } else {
                slot.generation.saturating_add(1)
            }
        };
        for chunk in bytes.chunks(16 * 1024) {
            capture.push(json!({"kind":"terminal","terminal_id":terminal_id,"backend_generation":generation,"data_base64":STANDARD.encode(chunk)}));
        }
    }
}

pub(crate) fn record_finish(label: &str) {
    if let Some(capture) = capture() {
        capture.push(json!({"kind":"finish","label":label}));
    }
}

pub struct Handle {
    task: Option<JoinHandle<()>>,
    capture: Arc<Capture>,
}
impl Handle {
    /// Bind an embedded frontend to this real local attach service. A read-only
    /// frontend never takes the exclusive input lease. No relay identity or
    /// history owner is created by this operation.
    pub fn frontend(&self, read_only: bool) -> Result<Frontend> {
        Frontend::open(&self.capture, read_only)
    }

    #[cfg(all(test, unix))]
    pub(crate) fn capture_for_test() -> Self {
        let capture = Arc::new(Capture::new().unwrap());
        let mut slot = CAPTURE.get_or_init(Default::default).lock().unwrap();
        assert!(slot.as_ref().and_then(Weak::upgrade).is_none());
        *slot = Some(Arc::downgrade(&capture));
        Self {
            task: None,
            capture,
        }
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn read_for_test(&self) -> Value {
        self.capture.read(0).await.unwrap()
    }

    pub fn bind_runtime(&self, runtime: Arc<Runtime>) -> Result<RuntimeBinding> {
        let mut slot = self
            .capture
            .runtime
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.runtime.is_some() {
            return Err("an attach backend is already bound".into());
        }
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or("attach backend generation exhausted")?;
        slot.runtime = Some(runtime);
        let generation = slot.generation;
        drop(slot);
        self.capture
            .push(json!({"kind":"backend","active":true,"backend_generation":generation}));
        Ok(RuntimeBinding {
            capture: Arc::downgrade(&self.capture),
            generation,
        })
    }
    pub async fn shutdown(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
pub struct RuntimeBinding {
    capture: Weak<Capture>,
    generation: u64,
}
impl Drop for RuntimeBinding {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.upgrade() {
            let mut slot = capture.runtime.lock().unwrap_or_else(|e| e.into_inner());
            if slot.generation == self.generation {
                slot.runtime = None;
                drop(slot);
                capture.push(
                    json!({"kind":"backend","active":false,"backend_generation":self.generation}),
                );
            }
        }
    }
}
impl Drop for Handle {
    fn drop(&mut self) {
        self.capture.bridge.close();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        if let Some(slot) = CAPTURE.get() {
            let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
            if slot
                .as_ref()
                .and_then(Weak::upgrade)
                .is_some_and(|value| Arc::ptr_eq(&value, &self.capture))
            {
                *slot = None;
            }
        }
    }
}

pub async fn serve(directory: PathBuf, runtime: Option<Arc<Runtime>>) -> Result<Handle> {
    let server = ipc::Server::bind(&directory, ENDPOINT).await?;
    let history = history_capture().ok_or("process event history is unavailable")?;
    let capture = Arc::new(Capture::with_journal(&history));
    if let Some(runtime) = runtime {
        *capture.runtime.lock().unwrap_or_else(|e| e.into_inner()) = RuntimeSlot {
            runtime: Some(runtime),
            generation: 1,
        };
    }
    *CAPTURE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(&capture));
    let shared = capture.clone();
    let task = tokio::spawn(async move {
        let semaphore = Arc::new(Semaphore::new(32));
        let mut handlers = JoinSet::new();
        loop {
            tokio::select! {
                peer = server.accept() => {
                    let Ok(mut peer) = peer else { break };
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else { continue };
                    let shared = shared.clone();
                    handlers.spawn(async move {
                        let _permit = permit;
                        let Ok(request) = peer.read().await else { return };
                        let response = match tokio::time::timeout(Duration::from_secs(10),handle_request(&shared, request)).await {
                            Ok(value)=>value.map_err(|e|e.to_string()),
                            Err(_)=>Err("attach operation timed out; input may have been partially delivered, do not replay it".into()),
                        };
                        let _ = peer.reply(response).await;
                    });
                }
                _ = handlers.join_next(), if !handlers.is_empty() => {}
            }
        }
        handlers.abort_all();
        while handlers.join_next().await.is_some() {}
    });
    Ok(Handle {
        task: Some(task),
        capture,
    })
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open {
        read_only: bool,
    },
    Read {
        lease: String,
        cursor: u64,
    },
    Write {
        lease: String,
        backend_generation: u64,
        terminal_id: String,
        data: String,
    },
    Resize {
        lease: String,
        backend_generation: u64,
        rows: u16,
        cols: u16,
    },
    Terminals {
        lease: String,
    },
    Detach {
        lease: String,
    },
    Safety {
        lease: String,
        generation: String,
        backend_generation: u64,
        cursor: Option<u64>,
    },
    Bridge {
        lease: String,
        request: zbridge::Request,
    },
}

async fn handle_request(capture: &Capture, request: Value) -> Result<Value> {
    let request: Request =
        serde_json::from_value(request).map_err(|_| "invalid attach operation")?;
    match request {
        Request::Open { read_only } => {
            let (runtime, backend_generation, live_cursor) = capture.snapshot();
            let active = runtime.is_some();
            let terminals = if let Some(runtime) = runtime {
                runtime.terminals().call("terminal.list", json!({})).await?["terminals"].clone()
            } else {
                json!([])
            };
            Ok(
                json!({"lease":capture.open(read_only)?,"read_only":read_only,
                "generation":capture.generation,"terminals":terminals,"selected":"main",
                "backend_active":active,"backend_generation":backend_generation,"live_cursor":live_cursor}),
            )
        }
        Request::Read { lease, cursor } => {
            capture.touch(&lease, false)?;
            capture.read(cursor).await
        }
        Request::Write {
            lease,
            backend_generation,
            terminal_id,
            data,
        } => {
            capture.touch(&lease, true)?;
            if data.len() > crate::wire::MAX_WRITE {
                return Err("attach input is too large".into());
            }
            let runtime = capture.runtime(backend_generation)?;
            Ok(runtime
                .terminals()
                .call(
                    "terminal.write",
                    json!({"terminal_id":terminal_id,"data":data}),
                )
                .await?)
        }
        Request::Resize {
            lease,
            backend_generation,
            rows,
            cols,
        } => {
            capture.touch(&lease, true)?;
            if rows == 0 || cols == 0 || rows > 1000 || cols > 1000 {
                return Err("attach size must be 1..=1000".into());
            }
            let runtime = capture.runtime(backend_generation)?;
            runtime.terminals().resize_all(rows, cols).await?;
            Ok(json!({"rows":rows,"cols":cols}))
        }
        Request::Terminals { lease } => {
            capture.touch(&lease, false)?;
            let (runtime, generation, _) = capture.snapshot();
            if let Some(runtime) = runtime {
                let mut result = runtime.terminals().call("terminal.list", json!({})).await?;
                result["backend_active"] = json!(true);
                result["backend_generation"] = json!(generation);
                Ok(result)
            } else {
                Ok(json!({"terminals":[],"backend_active":false,"backend_generation":generation}))
            }
        }
        Request::Detach { lease } => {
            let removed = capture
                .leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&lease)
                .is_some();
            Ok(json!({"detached":removed,"session_stopped":false}))
        }
        Request::Safety {
            lease,
            generation,
            backend_generation,
            cursor,
        } => {
            capture.touch(&lease, false)?;
            if capture.generation != generation {
                return Err("safety session generation changed".into());
            }
            let runtime = capture.runtime(backend_generation)?;
            let (start, limit) = match cursor {
                Some(cursor) => (cursor, 1),
                None => {
                    let head = runtime.safety_events(0, 1).await?;
                    let end = head["end_cursor"]
                        .as_u64()
                        .ok_or("invalid safety stream end")?;
                    (end.saturating_sub(128), 128)
                }
            };
            let page = runtime.safety_events(start, limit).await?;
            capture.runtime(backend_generation)?;
            safety_page(page, &generation, backend_generation, cursor)
        }
        Request::Bridge { lease, request } => Ok(serde_json::to_value(
            zbridge_adapter::request(capture, &lease, request).await,
        )?),
    }
}

fn safety_page(
    page: Value,
    generation: &str,
    backend_generation: u64,
    cursor: Option<u64>,
) -> Result<Value> {
    let rows = page["events"]
        .as_array()
        .ok_or("invalid safety stream page")?;
    let mut events = Vec::new();
    for row in rows {
        let sequence = row["cursor"]
            .as_u64()
            .ok_or("invalid safety event cursor")?;
        let event = &row["event"];
        if cursor.is_some_and(|cursor| cursor != sequence)
            || (cursor.is_none() && event["outcome"] == "succeeded")
        {
            continue;
        }
        events.push(json!({
            "id":format!("{generation}:{backend_generation}:{sequence}"),
            "tool":event["tool"],
            "policy":event["policy"],
            "rules_revision":event["rules_revision"],
            "rules_state":event["rules_state"],
            "decision":event["decision"],
            "outcome":event["outcome"],
        }));
    }
    if cursor.is_some() {
        return events
            .pop()
            .ok_or_else(|| "safety event is unavailable in this session generation".into());
    }
    Ok(json!({"events":events,"window":128,"truncated":page["truncated"]}))
}

pub(crate) async fn safety(name: &str, event: Option<&str>) -> Result<Value> {
    let root = super::session_root(false)?;
    let (directory, _) = super::load(&root, name)?;
    safety_at_directory(directory, event).await
}

pub(crate) async fn safety_at_directory(directory: PathBuf, event: Option<&str>) -> Result<Value> {
    let open = ipc::request(&directory, ENDPOINT, json!({"op":"open","read_only":true})).await?;
    let key = open["lease"]
        .as_str()
        .ok_or("attach endpoint returned no lease")?;
    let mut lease = LiveLogLease {
        directory,
        key: Some(key.to_owned()),
    };
    let result = async {
        if open["read_only"] != true || open["backend_active"] != true {
            return Err("active read-only attach session required".into());
        }
        let generation = open["generation"]
            .as_str()
            .ok_or("missing attach generation")?;
        let backend = open["backend_generation"]
            .as_u64()
            .ok_or("missing backend generation")?;
        let cursor = if let Some(event) = event {
            let prefix = format!("{generation}:{backend}:");
            Some(
                event
                    .strip_prefix(&prefix)
                    .and_then(|value| value.parse::<u64>().ok())
                    .ok_or("safety event ID does not match the active session generation")?,
            )
        } else {
            None
        };
        ipc::request(
            &lease.directory,
            ENDPOINT,
            json!({
                "op":"safety","lease":key,"generation":generation,
                "backend_generation":backend,"cursor":cursor
            }),
        )
        .await
    }
    .await;
    lease.release().await;
    result
}

/// A bounded snapshot of the running process, obtained with an observer lease.
/// The Open cursor is the exclusive end: subsequent output never extends it.
pub(super) async fn live_logs(directory: &Path, count: usize) -> Result<Value> {
    let result: Result<Value> = async {
        let open = ipc::request_with_timeout(
            directory,
            ENDPOINT,
            json!({"op":"open","read_only":true}),
            Duration::from_secs(3),
        )
        .await?;
        let key = open["lease"]
            .as_str()
            .filter(|key| !key.is_empty() && key.len() <= 128)
            .ok_or("attach endpoint returned no usable lease")?
            .to_owned();
        let mut lease = LiveLogLease {
            directory: directory.to_owned(),
            key: Some(key),
        };
        let result = match tokio::time::timeout(
            Duration::from_secs(10),
            read_live_logs(directory, lease.key.as_deref().unwrap(), &open, count),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err("live log snapshot timed out".into()),
        };
        lease.release().await;
        result
    }
    .await;
    result.map_err(|error| {
        format!("live_logs_unavailable: historical session logs are retired; {error}").into()
    })
}

struct LiveLogLease {
    directory: PathBuf,
    key: Option<String>,
}

impl LiveLogLease {
    async fn release(&mut self) {
        if let Some(key) = &self.key {
            let _ = ipc::request_with_timeout(
                &self.directory,
                ENDPOINT,
                json!({"op":"detach","lease":key}),
                Duration::from_secs(2),
            )
            .await;
        }
        self.key = None;
    }
}

impl Drop for LiveLogLease {
    fn drop(&mut self) {
        // Cancellation still attempts Detach. If the runtime or peer has gone,
        // the server's normal lease expiry remains the final bound.
        let Some(key) = self.key.take() else { return };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let directory = self.directory.clone();
        runtime.spawn(async move {
            let _ = ipc::request_with_timeout(
                &directory,
                ENDPOINT,
                json!({"op":"detach","lease":key}),
                Duration::from_secs(2),
            )
            .await;
        });
    }
}

async fn read_live_logs(
    directory: &Path,
    lease: &str,
    open: &Value,
    count: usize,
) -> Result<Value> {
    if open["read_only"] != true {
        return Err("attach endpoint did not grant a read-only lease".into());
    }
    let generation = open["generation"]
        .as_str()
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or("attach endpoint returned no generation")?;
    let live_cursor = open["live_cursor"]
        .as_u64()
        .ok_or("attach endpoint returned no live cursor")?;
    let limit = count.clamp(1, 1000);
    let start_cursor = live_cursor.saturating_sub(limit as u64);
    let mut cursor = start_cursor;
    let mut ring_start = 0;
    let mut events = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    'pages: for _ in 0..64 {
        if cursor >= live_cursor {
            break;
        }
        let mut page = ipc::request_with_timeout(
            directory,
            ENDPOINT,
            json!({"op":"read","lease":lease,"cursor":cursor}),
            Duration::from_secs(2),
        )
        .await?;
        if page["generation"].as_str() != Some(generation) {
            return Err("live log generation changed during the snapshot".into());
        }
        let start = page["start_cursor"]
            .as_u64()
            .ok_or("invalid log start cursor")?;
        let next = page["next_cursor"]
            .as_u64()
            .ok_or("invalid log next cursor")?;
        let evicted = page["truncated"]
            .as_bool()
            .ok_or("invalid log truncation flag")?;
        if start < ring_start || next <= cursor || start > next || evicted != (cursor < start) {
            return Err("live log cursors did not advance monotonically".into());
        }
        ring_start = start;
        truncated |= evicted;
        let page_events = page["events"]
            .as_array_mut()
            .map(std::mem::take)
            .ok_or("invalid live log events")?;
        if page_events.len() > 128 {
            return Err("live log page exceeds its event limit".into());
        }
        let mut expected = cursor.max(start);
        let mut page_bytes = 0usize;
        for event in &page_events {
            if event["sequence"].as_u64() != Some(expected) {
                return Err("live log event sequence is inconsistent".into());
            }
            expected = expected.checked_add(1).ok_or("live log cursor overflow")?;
            page_bytes = page_bytes.saturating_add(serde_json::to_vec(event)?.len());
        }
        if expected != next || page_bytes > PAGE_BYTES {
            return Err("live log page exceeds its cursor or byte limit".into());
        }
        for event in page_events {
            let sequence = event["sequence"].as_u64().unwrap();
            if sequence >= live_cursor {
                break;
            }
            let size = serde_json::to_vec(&event)?.len();
            if events.len() >= limit || bytes.saturating_add(size) > HISTORY_BYTES {
                truncated = true;
                break 'pages;
            }
            bytes += size;
            truncated |= event["truncated"] == true;
            cursor = sequence + 1;
            events.push(event);
        }
        cursor = next.min(live_cursor);
    }
    truncated |= cursor < live_cursor;
    Ok(json!({
        "source":"live", "storage":"memory", "generation":generation,
        "events":events, "limit":limit, "truncated":truncated,
        "start_cursor":start_cursor, "next_cursor":cursor, "live_cursor":live_cursor,
    }))
}

/// Auto attach may fall back only when the saved definition is absent. A
/// present but unreadable/invalid definition must retain its local error.
pub(crate) fn saved_session_exists(name: &str) -> Result<bool> {
    if super::validate_name(name).is_err() {
        return Ok(false);
    }
    let root = match super::session_root(false) {
        Ok(root) => root,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error),
    };
    saved_session_in(&root, name)
}

fn saved_session_in(root: &std::path::Path, name: &str) -> Result<bool> {
    match std::fs::symlink_metadata(super::session_directory(root, name)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
        Ok(_) => {
            super::load(root, name)?;
            Ok(true)
        }
    }
}

pub async fn manage(args: Vec<OsString>) -> Result<()> {
    let args: Vec<String> = args
        .into_iter()
        .map(|v| {
            v.into_string()
                .map_err(|_| "attach arguments must be UTF-8".into())
        })
        .collect::<Result<_>>()?;
    let listing = args.is_empty()
        || args.as_slice() == ["list"]
        || args.as_slice() == ["--json"]
        || args.as_slice() == ["list", "--json"];
    if listing {
        return super::manage(if args.iter().any(|v| v == "--json") {
            vec!["list".into(), "--json".into()]
        } else {
            vec!["list".into()]
        })
        .await;
    }
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!(
            "Usage: ssmcp attach [list] [--json]\n       ssmcp attach --local NAME [--read-only]\n\nAttach to an existing running local session. Ctrl-] detaches; the session keeps running.\nCtrl-C reaches the inner terminal while input is enabled. F7 cycles session terminals.\n--read-only permits another observer without taking input ownership.\nUse ssmcp attach --remote ID_OR_NAME for an authorized online device."
        );
        return Ok(());
    }
    let mut read_only = !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal();
    let mut name = None;
    let mut literal = false;
    for argument in args {
        match argument.as_str() {
            "--" if !literal => literal = true,
            "--read-only" if !literal => read_only = true,
            _ if !literal && argument.starts_with('-') => {
                return Err("usage: ssmcp attach NAME [--read-only]".into());
            }
            _ if name.is_none() => name = Some(argument),
            _ => return Err("attach accepts exactly one session name".into()),
        }
    }
    let name = name.ok_or("attach needs a session name")?;
    super::validate_name(&name)?;
    let root = super::session_root(false)?;
    let (directory, manifest) = super::load(&root, &name)?;
    let open = ipc::request(
        &directory,
        ENDPOINT,
        json!({"op":"open","read_only":read_only}),
    )
    .await
    .map_err(|error| {
        format!("could not attach to {name}: {error}; inspect `ssmcp sessions status {name}`")
    })?;
    let lease = open["lease"]
        .as_str()
        .ok_or("attach endpoint returned no lease")?
        .to_owned();
    let result = attached(&manifest.spec, directory.clone(), &lease, read_only, &open).await;
    // Best-effort lease release is independent of service lifetime. No stop,
    // restart or termination operation is ever sent by this frontend.
    let _ = ipc::request_with_timeout(
        &directory,
        ENDPOINT,
        json!({"op":"detach","lease":lease}),
        Duration::from_secs(2),
    )
    .await;
    result
}

async fn attached(
    spec: &super::SessionSpec,
    directory: PathBuf,
    lease: &str,
    read_only: bool,
    open: &Value,
) -> Result<()> {
    use crate::client::{render, terminal, ui::Ui};
    let name = &spec.name;
    let title = crate::client::session_title(spec)?;
    let ui = Ui::load(None)?;
    crate::client::interface::initialize_path(None);
    render::configure(ui.clone());
    // Use the attached session's workspace, which may differ from the directory
    // where this frontend was launched. This grants input import only.
    let display_enabled = crate::client::saved_chat_enabled(spec);
    let _local_input = display_enabled
        .then(|| crate::client::attachments::LocalInput::bind(spec.cwd.clone()))
        .transpose()?;
    let display = if display_enabled {
        terminal::Session::start(&ui, &spec.cwd.to_string_lossy(), title.as_deref(), None)?
    } else {
        None
    };
    terminal::follow_attach_history(DisplayHistory::NoSelection);
    let mut replay_history = ReplayHistory::Legacy;
    let mut terminal_ids = Vec::new();
    sync_terminals(open, &mut terminal_ids);
    let mut selected = 0usize;
    let mut backend_generation = if open["backend_active"] == true {
        open["backend_generation"].as_u64()
    } else {
        None
    };
    let display_generation_floor = backend_generation.unwrap_or_else(|| {
        open["backend_generation"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1)
    });
    let live_cursor = open["live_cursor"].as_u64().unwrap_or(0);
    let mut geometry = None;
    let mut waiting_input_warned = false;
    let mut keyboard = input::Keyboard::open(read_only)?;
    render::status(
        "attach",
        "Connected",
        &format!(
            "Attached to {name}{} · Ctrl-] detaches · background session keeps running",
            if read_only { " (read-only)" } else { "" }
        ),
    );
    let (pages_tx, mut pages) = mpsc::channel(4);
    let read_directory = directory.clone();
    let read_lease = lease.to_owned();
    let generation = open["generation"]
        .as_str()
        .ok_or("missing attach generation")?
        .to_owned();
    let reader = tokio::spawn(async move {
        let mut cursor = 0;
        loop {
            let result = ipc::request(
                &read_directory,
                ENDPOINT,
                json!({"op":"read","lease":read_lease,"cursor":cursor}),
            )
            .await;
            let result = result
                .and_then(|page| {
                    if page["generation"] != generation {
                        return Err("session generation changed; attach again".into());
                    }
                    cursor = page["next_cursor"]
                        .as_u64()
                        .ok_or("missing attach cursor")?;
                    Ok(page)
                })
                .map_err(|error| error.to_string());
            let failed = result.is_err();
            if pages_tx.send(result).await.is_err() || failed {
                break;
            }
        }
    });
    let mut reader = TaskGuard(Some(reader));
    let mut interrupt = crate::platform::interrupt_signal()?;
    let mut terminate = crate::platform::terminate_signal()?;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut refresh = Instant::now();
    let mut keys = crate::client::input::SessionKeys::default();
    let mut utf8 = input::Utf8Input::default();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = interrupt.recv() => break,
                _ = terminate.recv() => break,
                event = keyboard.next() => match event? {
                    input::Event::Detach => break,
                    input::Event::InputProgress(now) => terminal::paste_input_progress(now),
                    input::Event::PasteExpired => {
                        terminal::discard_image_paste();
                        utf8 = input::Utf8Input::default();
                        keys = crate::client::input::SessionKeys::default();
                    }
                    input::Event::Interrupt => {
                        // Session attach keeps Ctrl-C on the inner PTY even with
                        // the dashboard active. Flush fallback key prefixes in order.
                        let text = utf8.push(b"\x03", false);
                        apply_keys(&directory,lease,&mut selected,&terminal_ids,keys.feed(&text,Instant::now(),false),read_only,backend_generation).await?;
                    }
                    input::Event::End => {
                        let text = utf8.push(&[], true);
                        apply_keys(&directory,lease,&mut selected,&terminal_ids,keys.feed(&text,Instant::now(),true),read_only,backend_generation).await?;
                        break;
                    }
                    input::Event::Resize(rows, cols) => {
                        let (rows, cols) = terminal::physical_resize(rows,cols);
                        geometry=Some((rows,cols));
                        if !read_only && let Some(generation)=backend_generation { ipc::request(&directory, ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?; }
                    }
                    event @ (input::Event::Bytes(_) | input::Event::PasteBytes(_)) => {
                        let (bytes, pasted) = match event {
                            input::Event::Bytes(bytes) => (bytes, false),
                            input::Event::PasteBytes(bytes) => (bytes, true),
                            _ => unreachable!(),
                        };
                        if !read_only && backend_generation.is_none() && !waiting_input_warned {
                            render::status("attach","Warning","Session is connecting. Terminal input is unavailable and is not queued for later execution.");waiting_input_warned=true;
                        }
                        // Preserve original paste bytes until the compositor consumes them.
                        // Its output has already handled dashboard shortcuts; only fallback
                        // text uses SessionKeys to select the existing remote PTY.
                        let events = if let Some(output) = terminal::keyboard_bytes(&bytes) {
                            vec![crate::client::input::InputEvent::Text(utf8.push(&output, false))]
                        } else if pasted {
                            // The outer decoder has already separated the paste.
                            // Keep the text-based fallback, without interpreting
                            // embedded End/F7 bytes as local session shortcuts.
                            vec![crate::client::input::InputEvent::Text(utf8.push(&bytes, false))]
                        } else {
                            keys.feed(&utf8.push(&bytes, false), Instant::now(), false)
                        };
                        apply_keys(&directory,lease,&mut selected,&terminal_ids,events,read_only,backend_generation).await?;
                    }
                },
                page = pages.recv() => {
                    let page = page.ok_or("attach output stream ended")?.map_err(|error| format!("attach ended: {error}; the session may be reconnecting, stopped, or restarted"))?;
                    if page["truncated"] == true { render::status("attach","Warning","Earlier display history was evicted; showing the retained tail. Use session logs for older output."); }
                    if let Some(selection) = replay_history.observe_page(&page) {
                        terminal::follow_attach_history(selection);
                    }
                    for event in page["events"].as_array().ok_or("invalid attach events")? {
                        if event["kind"]=="backend" && event["sequence"].as_u64().unwrap_or(0)>=live_cursor {
                            let generation=event["backend_generation"].as_u64().ok_or("missing backend generation")?;
                            if event["active"]==true {
                                if backend_generation.is_some_and(|current|current!=generation){return Err("session backend changed; attach again to the replacement".into());}
                                backend_generation=Some(generation);
                                if !read_only && let Some((rows,cols))=geometry {ipc::request(&directory,ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?;}
                                render::status("attach","Connected","Session terminal is ready");
                            } else if backend_generation==Some(generation) {return Err("session backend disconnected; attach again to observe the reconnecting session".into());}
                        }
                        if event["kind"]=="terminal" {
                            let epoch=event["backend_generation"].as_u64().unwrap_or(0);
                            if backend_generation.is_some_and(|current|current!=epoch) || epoch<display_generation_floor {continue;}
                        }
                        replay(event, replay_history)?;
                    }
                }
                _ = tick.tick() => {
                    if terminal::exit_requested() { break; }
                    apply_keys(&directory,lease,&mut selected,&terminal_ids,keys.feed("",Instant::now(),true),read_only,backend_generation).await?;
                    let queued = terminal::take_keyboard();
                    if let Some((rows,cols)) = crate::client::interface::apply_pending()? {
                        geometry=Some((rows,cols));
                        if !read_only && let Some(generation)=backend_generation { ipc::request(&directory,ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?; }
                    }
                    if !read_only && !queued.is_empty() && let Some(generation)=backend_generation { send_input(&directory,lease,generation,terminal_ids.get(selected).map(String::as_str).unwrap_or("main"),queued).await?; }
                    for (id,bytes) in terminal::take_session_replies() {
                        if !read_only && let Some(generation)=backend_generation { send_input(&directory,lease,generation,&id,String::from_utf8(bytes).map_err(|_| "invalid virtual terminal reply")?).await?; }
                    }
                    if refresh.elapsed() >= Duration::from_secs(2) {
                        let snapshot = ipc::request(&directory, ENDPOINT, json!({"op":"terminals","lease":lease})).await?;
                        if let Some(current)=backend_generation {
                            if snapshot["backend_active"]!=true || snapshot["backend_generation"].as_u64()!=Some(current) {return Err("session backend disconnected or changed; attach again".into());}
                        } else if snapshot["backend_active"]==true {
                            let generation=snapshot["backend_generation"].as_u64().ok_or("missing backend generation")?;
                            backend_generation=Some(generation);
                            if !read_only && let Some((rows,cols))=geometry {ipc::request(&directory,ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?;}
                        }
                        let selected_id=terminal_ids.get(selected).cloned();
                        sync_terminals(&snapshot,&mut terminal_ids);
                        selected=selected_id.and_then(|id|terminal_ids.iter().position(|candidate|candidate==&id)).unwrap_or(0);
                        terminal::select_session(terminal_ids.get(selected).map(String::as_str).unwrap_or("main"));
                        refresh = Instant::now();
                    }
                }
            }
        }
        Ok(())
    }.await;
    reader.stop().await;
    drop(keyboard);
    drop(display);
    render::status(
        "attach",
        "Result",
        &format!("Detached from {name}; session state is unchanged"),
    );
    result
}

struct TaskGuard(Option<JoinHandle<()>>);
impl TaskGuard {
    async fn stop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

fn sync_terminals(snapshot: &Value, ids: &mut Vec<String>) {
    let Some(terminals) = snapshot["terminals"].as_array() else {
        return;
    };
    let next: Vec<String> = terminals
        .iter()
        .filter_map(|v| v["terminal_id"].as_str().map(str::to_owned))
        .collect();
    for old in ids.iter().filter(|id| !next.contains(id)) {
        crate::client::terminal::close_session(old);
    }
    for item in terminals {
        if let Some(id) = item["terminal_id"].as_str() {
            crate::client::terminal::register_session(id, item["name"].as_str().unwrap_or(id));
        }
    }
    *ids = next;
    if ids.is_empty() {
        ids.push("main".into());
        crate::client::terminal::register_session("main", "main");
    }
}

#[derive(Clone, Copy)]
enum ReplayHistory {
    Legacy,
    Modern,
}

impl ReplayHistory {
    /// The page has already passed the private IPC and capture generation checks.
    fn observe_page(&mut self, page: &Value) -> Option<DisplayHistory> {
        if let Some(selection) = page.get("active_history_id") {
            *self = Self::Modern;
            return Some(
                selection
                    .as_str()
                    .and_then(HistoryId::parse)
                    .map_or(DisplayHistory::NoSelection, DisplayHistory::Local),
            );
        }
        // Once the stream advertises ownership metadata, a missing field
        // cannot turn the viewer back into the legacy shared timeline.
        matches!(self, Self::Legacy).then_some(DisplayHistory::LegacyAttach)
    }

    fn event(self, frame: &Value) -> (&str, &Value, DisplaySource) {
        let (label, value, mut source) = recorded_event(frame);
        if matches!(self, Self::Legacy) {
            source.history = if frame.get("local_history_id").is_none() {
                DisplayHistory::LegacyAttach
            } else {
                DisplayHistory::Global
            };
        }
        (label, value, source)
    }
}

/// Decode display metadata only from the envelope of a checked local attach page.
pub(crate) fn recorded_event(frame: &Value) -> (&str, &Value, DisplaySource) {
    (
        frame["label"].as_str().unwrap_or("session"),
        &frame["value"],
        DisplaySource {
            history: frame["local_history_id"]
                .as_str()
                .and_then(HistoryId::parse)
                .map_or(DisplayHistory::Global, DisplayHistory::Local),
            call: None,
        },
    )
}

fn replay(event: &Value, history: ReplayHistory) -> Result<()> {
    match event["kind"].as_str() {
        Some("event") => {
            let (label, value, source) = history.event(event);
            crate::client::render::replay_event(label, value, source);
        }
        Some("finish") => {
            crate::client::render::finish(event["label"].as_str().unwrap_or("session"))
        }
        Some("backend") => {}
        Some("terminal") => {
            let bytes = STANDARD.decode(
                event["data_base64"]
                    .as_str()
                    .ok_or("invalid terminal display record")?,
            )?;
            crate::client::render::terminal_output_for(
                event["terminal_id"].as_str().unwrap_or("main"),
                &bytes,
            );
        }
        _ => return Err("unknown attach display record".into()),
    }
    Ok(())
}

async fn apply_keys(
    directory: &std::path::Path,
    lease: &str,
    selected: &mut usize,
    ids: &[String],
    events: Vec<crate::client::input::InputEvent>,
    read_only: bool,
    backend_generation: Option<u64>,
) -> Result<()> {
    for event in events {
        match event {
            crate::client::input::InputEvent::Text(text) => {
                if !read_only
                    && !text.is_empty()
                    && let Some(generation) = backend_generation
                {
                    send_input(
                        directory,
                        lease,
                        generation,
                        ids.get(*selected).map(String::as_str).unwrap_or("main"),
                        text,
                    )
                    .await?;
                }
            }
            crate::client::input::InputEvent::NextTerminal => {
                *selected = (*selected + 1) % ids.len().max(1);
                crate::client::terminal::select_session(
                    ids.get(*selected).map(String::as_str).unwrap_or("main"),
                );
            }
        }
    }
    Ok(())
}

async fn send_input(
    directory: &std::path::Path,
    lease: &str,
    backend_generation: u64,
    terminal_id: &str,
    data: String,
) -> Result<()> {
    let mut remaining = data.as_str();
    while !remaining.is_empty() {
        let mut end = remaining.len().min(crate::wire::MAX_WRITE);
        while !remaining.is_char_boundary(end) {
            end -= 1;
        }
        ipc::request(directory,ENDPOINT,json!({"op":"write","lease":lease,"backend_generation":backend_generation,"terminal_id":terminal_id,"data":&remaining[..end]})).await?;
        remaining = &remaining[end..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safety_queue_filters_success_and_inspect_rejects_an_evicted_cursor() {
        let page = json!({"truncated":true,"events":[
            {"cursor":4,"event":{"tool":"ssmcp.fs.write_text","outcome":"blocked","text":"synthetic-private"}},
            {"cursor":5,"event":{"tool":"ssmcp.fs.write_text","outcome":"succeeded"}}
        ]});
        let queue = safety_page(page.clone(), "fixture", 2, None).unwrap();
        assert_eq!(queue["events"].as_array().unwrap().len(), 1);
        assert_eq!(queue["events"][0]["id"], "fixture:2:4");
        assert_eq!(queue["truncated"], true);
        assert!(!queue.to_string().contains("synthetic-private"));
        assert!(safety_page(page.clone(), "fixture", 2, Some(3)).is_err());
        assert_eq!(
            safety_page(page, "fixture", 2, Some(5)).unwrap()["id"],
            "fixture:2:5"
        );
    }

    #[tokio::test]
    async fn safety_endpoint_rejects_an_unknown_lease() {
        let capture = Capture::new().unwrap();
        let error = handle_request(
            &capture,
            json!({
                "op":"safety","lease":"unknown","generation":capture.generation,
                "backend_generation":0,"cursor":null
            }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("lease expired"));
    }

    #[cfg(unix)]
    async fn log_endpoint(
        capture: Arc<Capture>,
        fault: &'static str,
        append_after_open: usize,
    ) -> (PathBuf, JoinHandle<Vec<String>>) {
        let directory = std::env::temp_dir().join(format!(
            "ssmcp-live-logs-{}",
            super::super::random_suffix().unwrap()
        ));
        crate::platform::private_directory(&directory).unwrap();
        let server = ipc::Server::bind(&directory, ENDPOINT).await.unwrap();
        let task = tokio::spawn(async move {
            let mut operations = Vec::new();
            loop {
                let mut peer = server.accept().await.unwrap();
                let request = peer.read().await.unwrap();
                let op = request["op"].as_str().unwrap().to_owned();
                assert!(matches!(op.as_str(), "open" | "read" | "detach"));
                if op == "open" {
                    assert_eq!(request["read_only"], true);
                }
                let cursor = request["cursor"].as_u64().unwrap_or(0);
                operations.push(op.clone());
                let mut response = handle_request(&capture, request)
                    .await
                    .map_err(|error| error.to_string());
                if op == "open" {
                    for index in 0..append_after_open {
                        capture.push(json!({"kind":"event","value":{"late":index}}));
                    }
                    if fault == "open" {
                        response.as_mut().unwrap()["read_only"] = json!(false);
                    }
                }
                if op == "read" {
                    match fault {
                        "generation" => {
                            response.as_mut().unwrap()["generation"] = json!("new-process")
                        }
                        "cursor" => response.as_mut().unwrap()["next_cursor"] = json!(cursor),
                        "sequence" => {
                            response.as_mut().unwrap()["events"][0]["sequence"] = json!(cursor + 1)
                        }
                        "page_bytes" => {
                            response.as_mut().unwrap()["events"][0]["value"] =
                                json!("x".repeat(PAGE_BYTES + 1))
                        }
                        "read_error" => response = Err("fixture read failure".into()),
                        _ => {}
                    }
                }
                peer.reply(response).await.unwrap();
                if op == "detach" {
                    break;
                }
            }
            operations
        });
        (directory, task)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_logs_page_to_the_open_boundary_without_taking_input() {
        let capture = Arc::new(Capture::new().unwrap());
        for index in 0..200 {
            capture.push(json!({"kind":"event","value":{"index":index}}));
        }
        let writer = capture.open(false).unwrap();
        let (directory, task) = log_endpoint(capture.clone(), "", 50).await;
        let result = live_logs(&directory, 200).await.unwrap();
        let operations = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operations, ["open", "read", "read", "detach"]);
        assert_eq!(result["source"], "live");
        assert_eq!(result["storage"], "memory");
        assert_eq!(result["generation"], capture.generation);
        assert_eq!(result["start_cursor"], 0);
        assert_eq!(result["next_cursor"], 200);
        assert_eq!(result["live_cursor"], 200);
        assert_eq!(result["truncated"], false);
        let events = result["events"].as_array().unwrap();
        assert_eq!(events.len(), 200);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event["sequence"], index as u64);
            assert_eq!(event["value"]["index"], index as u64);
            assert!(event["value"].get("late").is_none());
        }
        let leases = capture.leases.lock().unwrap();
        assert_eq!(leases.len(), 1);
        assert!(!leases[&writer].read_only);
        drop(leases);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_logs_report_ring_eviction_and_remain_within_the_memory_budget() {
        let capture = Arc::new(Capture::new().unwrap());
        for index in 0..300 {
            capture.push(json!({"kind":"event","value":"x".repeat(8_000),"index":index}));
        }
        let (directory, task) = log_endpoint(capture.clone(), "", 0).await;
        let result = live_logs(&directory, 1000).await.unwrap();
        let operations = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert!(operations.iter().filter(|op| *op == "read").count() > 1);
        let events = result["events"].as_array().unwrap();
        assert!(events.len() > 128 && events.len() < 300);
        assert!(events[0]["sequence"].as_u64().unwrap() > 0);
        assert_eq!(result["truncated"], true);
        assert_eq!(result["next_cursor"], 300);
        assert_eq!(result["live_cursor"], 300);
        assert!(
            events
                .iter()
                .map(|event| serde_json::to_vec(event).unwrap().len())
                .sum::<usize>()
                <= HISTORY_BYTES
        );
        assert!(capture.leases.lock().unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_logs_detach_after_protocol_and_transport_errors() {
        for fault in [
            "open",
            "generation",
            "cursor",
            "sequence",
            "page_bytes",
            "read_error",
        ] {
            let capture = Arc::new(Capture::new().unwrap());
            capture.push(json!({"kind":"event","value":"fixture"}));
            let (directory, task) = log_endpoint(capture.clone(), fault, 0).await;
            let error = live_logs(&directory, 1).await.unwrap_err();
            assert!(
                error.to_string().contains("live_logs_unavailable"),
                "{fault}: {error}"
            );
            let operations = tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(operations.last().map(String::as_str), Some("detach"));
            assert!(capture.leases.lock().unwrap().is_empty());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn empty_live_logs_do_not_wait_for_new_events() {
        let capture = Arc::new(Capture::new().unwrap());
        let (directory, task) = log_endpoint(capture.clone(), "", 0).await;
        let result = live_logs(&directory, 0).await.unwrap();
        let operations = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operations, ["open", "detach"]);
        assert_eq!(result["events"], json!([]));
        assert_eq!(result["limit"], 1);
        assert_eq!(result["live_cursor"], 0);
        assert_eq!(result["truncated"], false);
        assert!(capture.leases.lock().unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn unavailable_live_logs_never_read_or_change_legacy_files() {
        let directory = std::env::temp_dir().join(format!(
            "ssmcp-retired-logs-{}",
            super::super::random_suffix().unwrap()
        ));
        crate::platform::private_directory(&directory).unwrap();
        let sentinel = b"\xffnot-json: historical user data\n";
        for filename in ["lifecycle.jsonl", "lifecycle.1.jsonl"] {
            std::fs::write(directory.join(filename), sentinel).unwrap();
        }
        let error = live_logs(&directory, 50).await.unwrap_err();
        assert!(error.to_string().contains("live_logs_unavailable"));
        for filename in ["lifecycle.jsonl", "lifecycle.1.jsonl"] {
            assert_eq!(std::fs::read(directory.join(filename)).unwrap(), sentinel);
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn attach_local_probe_keeps_damaged_definitions_local() {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-attach-probe-{}",
            super::super::random_suffix().unwrap()
        ));
        crate::platform::private_directory(&root).unwrap();
        assert!(!saved_session_in(&root, "missing").unwrap());
        let directory = super::super::session_directory(&root, "broken");
        crate::platform::private_directory(&directory).unwrap();
        assert!(saved_session_in(&root, "broken").is_err());
        std::fs::write(directory.join("manifest.json"), "invalid").unwrap();
        assert!(saved_session_in(&root, "broken").is_err());
        #[cfg(unix)]
        {
            let link = super::super::session_directory(&root, "linked");
            std::os::unix::fs::symlink(&directory, &link).unwrap();
            assert!(saved_session_in(&root, "linked").is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pending_session_can_be_observed_but_does_not_queue_input() {
        let capture = Capture::new().unwrap();
        let opened = handle_request(&capture, json!({"op":"open","read_only":false}))
            .await
            .unwrap();
        assert_eq!(opened["backend_active"], false);
        assert_eq!(opened["terminals"], json!([]));
        let lease = opened["lease"].as_str().unwrap();
        let error=handle_request(&capture,json!({"op":"write","lease":lease,"backend_generation":0,"terminal_id":"main","data":"dangerous if replayed\\n"})).await.unwrap_err();
        assert!(error.to_string().contains("connecting"));
        capture.push(json!({"kind":"event","label":"ssh","value":{"message":"Connecting"}}));
        let page = handle_request(&capture, json!({"op":"read","lease":lease,"cursor":0}))
            .await
            .unwrap();
        assert_eq!(page["events"][0]["value"]["message"], "Connecting");
        let closed = handle_request(&capture, json!({"op":"detach","lease":lease}))
            .await
            .unwrap();
        assert_eq!(closed["session_stopped"], false);
        assert!(capture.open(false).is_ok());
    }
    #[tokio::test]
    async fn cursor_replay_is_bounded_and_reports_eviction() {
        let capture = Capture::new().unwrap();
        for index in 0..100 {
            capture.push(json!({"kind":"event","value":"x".repeat(40_000),"index":index}));
        }
        let page = capture.read(0).await.unwrap();
        assert_eq!(page["truncated"], true);
        assert!(page["start_cursor"].as_u64().unwrap() > 0);
        assert!(serde_json::to_vec(&page).unwrap().len() < 256 * 1024);
        let cursor = page["next_cursor"].as_u64().unwrap();
        assert!(
            capture.read(cursor).await.unwrap()["next_cursor"]
                .as_u64()
                .unwrap()
                > cursor
        );
        assert!(capture.read(101).await.is_err());
    }
    #[tokio::test]
    async fn compact_replay_preserves_nested_events_and_page_cursors() {
        let capture = Capture::new().unwrap();
        let mut expected = Vec::new();
        for index in 0..130 {
            let mut event = json!({
                "kind":"event",
                "label":"nested",
                "sequence":999,
                "value":{
                    "index":index,
                    "items":[null,true,{"text":"quote\" slash\\ newline\n 界"}],
                    "minimum":i64::MIN,
                    "maximum":u64::MAX,
                    "decimal":1.25,
                    "empty":{}
                }
            });
            capture.push(event.clone());
            event["sequence"] = json!(index);
            expected.push(event);
        }
        let first = capture.read(0).await.unwrap();
        assert_eq!(first["events"], json!(&expected[..128]));
        assert_eq!(first["start_cursor"], 0);
        assert_eq!(first["next_cursor"], 128);
        assert_eq!(first["truncated"], false);

        let second = capture.read(128).await.unwrap();
        assert_eq!(second["events"], json!(&expected[128..]));
        assert_eq!(second["next_cursor"], 130);
        assert_eq!(second["truncated"], false);
        assert!(capture.read(131).await.is_err());
    }

    #[tokio::test]
    async fn legacy_and_nested_provider_history_fields_do_not_acquire_a_replay_owner() {
        let capture = Capture::new().unwrap();
        let owner = HistoryId::parse("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let payload = json!({"type":"ssmcp.message","text":"fixture",
            "local_history_id":owner.as_str(),"attribution":{"name":"display only"}});
        capture.push_event("agent", &payload, None);
        capture.push_event("agent", &payload, Some(&owner));
        let page = capture.read(0).await.unwrap();
        let legacy = &page["events"][0];
        assert!(legacy.get("local_history_id").is_none());
        let (_, value, source) = recorded_event(legacy);
        assert_eq!(value["local_history_id"], owner.as_str());
        assert_eq!(source.history, DisplayHistory::Global);
        assert!(source.call.is_none());

        let (_, _, source) = recorded_event(&page["events"][1]);
        assert_eq!(source.history, DisplayHistory::Local(owner));
        assert!(source.call.is_none());
        let mut invalid = page["events"][1].clone();
        invalid["local_history_id"] = json!("../foreign");
        assert_eq!(recorded_event(&invalid).2.history, DisplayHistory::Global);
    }

    #[test]
    fn untagged_private_legacy_pages_have_a_shared_display_without_a_local_owner() {
        let page = json!({"events":[{"kind":"event","label":"agent","value":{
            "type":"ssmcp.message","text":"legacy fixture","active_history_id":null,
            "local_history_id":"conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}]});
        let mut history = ReplayHistory::Legacy;
        assert_eq!(
            history.observe_page(&page),
            Some(DisplayHistory::LegacyAttach)
        );
        let (_, _, source) = history.event(&page["events"][0]);
        assert_eq!(source.history, DisplayHistory::LegacyAttach);
        assert!(source.history.local_id().is_none());
        assert!(source.call.is_none());

        let mut unexpected = page["events"][0].clone();
        unexpected["local_history_id"] = json!(null);
        assert_eq!(history.event(&unexpected).2.history, DisplayHistory::Global);
    }

    #[test]
    fn explicit_null_and_invalid_modern_metadata_keep_untagged_frames_global() {
        let untagged = json!({"kind":"event","value":{"type":"ssmcp.message",
            "text":"unowned","legacy_attach":true}});
        let absent = json!({"events":[untagged.clone()]});
        for selection in [Value::Null, json!("foreign"), json!(42)] {
            let mut history = ReplayHistory::Legacy;
            assert_eq!(
                history.observe_page(&json!({"active_history_id":selection})),
                Some(DisplayHistory::NoSelection)
            );
            assert_eq!(history.event(&untagged).2.history, DisplayHistory::Global);
            assert_eq!(history.observe_page(&absent), None);
            assert_eq!(history.event(&untagged).2.history, DisplayHistory::Global);
        }
    }

    #[test]
    fn modern_pages_keep_a_late_frames_distinct_from_the_selected_b_owner() {
        let a = "conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let mut history = ReplayHistory::Legacy;
        assert_eq!(
            history.observe_page(&json!({"active_history_id":a})),
            Some(DisplayHistory::Local(HistoryId::parse(a).unwrap()))
        );
        assert_eq!(
            history.observe_page(&json!({"active_history_id":b})),
            Some(DisplayHistory::Local(HistoryId::parse(b).unwrap()))
        );
        let late = json!({"kind":"event","local_history_id":a,
            "value":{"type":"ssmcp.message","text":"late A","local_history_id":b}});
        let (_, _, source) = history.event(&late);
        assert_eq!(source.history.local_id().unwrap().as_str(), a);
        assert!(source.call.is_none());
        assert_eq!(history.observe_page(&json!({})), None);
        assert_eq!(history.event(&late).2.history, source.history);
    }

    #[test]
    fn input_owner_is_exclusive_observers_do_not_take_it_and_detach_releases_it() {
        let capture = Capture::new().unwrap();
        let writer = capture.open(false).unwrap();
        let viewer = capture.open(true).unwrap();
        assert!(capture.open(false).is_err());
        assert!(capture.touch(&viewer, true).is_err());
        assert!(capture.touch(&writer, true).is_ok());
        capture.leases.lock().unwrap().remove(&writer);
        assert!(capture.open(false).is_ok());
        assert!(capture.touch(&viewer, false).is_ok());
    }
    #[test]
    fn abandoned_writer_lease_expires_without_stopping_session() {
        let capture = Capture::new().unwrap();
        let writer = capture.open(false).unwrap();
        capture
            .leases
            .lock()
            .unwrap()
            .get_mut(&writer)
            .unwrap()
            .seen = Instant::now() - Duration::from_secs(LEASE_SECONDS + 1);
        assert!(capture.touch(&writer, true).is_err());
        assert!(capture.open(false).is_ok());
    }
}
