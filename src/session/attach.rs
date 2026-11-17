//! Local terminal frontends for an existing background session. Detaching only
//! releases an input lease; it never changes the session's desired state.
pub(crate) mod input;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io::IsTerminal,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::{
    sync::{Notify, Semaphore, mpsc},
    task::{JoinHandle, JoinSet},
};

use super::ipc;
use crate::{Result, runtime::Runtime};

const ENDPOINT: &str = "attach.json";
const HISTORY_BYTES: usize = 2 * 1024 * 1024;
const EVENT_BYTES: usize = 64 * 1024;
const PAGE_BYTES: usize = 128 * 1024;
const LEASE_SECONDS: u64 = 15;
static CAPTURE: OnceLock<Mutex<Option<Weak<Capture>>>> = OnceLock::new();

#[derive(Default)]
struct Ring {
    next: u64,
    bytes: usize,
    events: VecDeque<(u64, Value, usize)>,
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
    ring: Mutex<Ring>,
    leases: Mutex<BTreeMap<String, Lease>>,
    runtime: Mutex<RuntimeSlot>,
    changed: Notify,
}

impl Capture {
    fn new() -> Result<Self> {
        Ok(Self {
            generation: super::random_suffix()?,
            ring: Mutex::new(Ring::default()),
            leases: Mutex::new(BTreeMap::new()),
            runtime: Mutex::new(RuntimeSlot {
                runtime: None,
                generation: 0,
            }),
            changed: Notify::new(),
        })
    }

    fn push(&self, mut event: Value) {
        let mut bytes = event.to_string().len();
        if bytes > EVENT_BYTES {
            event = json!({"kind":"event","label":"attach","value":{
                "type":"ssmcp.status","level":"Warning",
                "message":format!("One {bytes}-byte event exceeds attach history's per-event limit; use the session log/output artifact for its complete contents.")}});
        }
        let mut ring = self.ring.lock().unwrap_or_else(|e| e.into_inner());
        let cursor = ring.next;
        event["sequence"] = json!(cursor);
        bytes = event.to_string().len();
        while ring.bytes + bytes > HISTORY_BYTES || ring.events.len() >= 4096 {
            let Some((_, _, old)) = ring.events.pop_front() else {
                break;
            };
            ring.bytes -= old;
        }
        ring.next = ring.next.saturating_add(1);
        ring.bytes += bytes;
        ring.events.push_back((cursor, event, bytes));
        drop(ring);
        self.changed.notify_waiters();
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
        loop {
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
                    for (sequence, event, size) in &ring.events {
                        if *sequence < next {
                            continue;
                        }
                        if events.len() >= 128 || bytes + size > PAGE_BYTES {
                            break;
                        }
                        events.push(event.clone());
                        bytes += size;
                        next = sequence + 1;
                    }
                    return Ok(json!({"generation":self.generation,"events":events,
                        "next_cursor":next,"start_cursor":start,"truncated":cursor < start}));
                }
            }
            let _ = tokio::time::timeout_at(deadline, changed).await;
        }
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

/// Called in the renderer's serialized emission path. Native payloads remain
/// unchanged; this private display replay contains the usual redacted copy.
pub(crate) fn record_event(label: &str, value: &Value) {
    if let Some(capture) = capture() {
        capture
            .push(json!({"kind":"event","label":label,"value":crate::audit::event(value.clone())}));
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
    let capture = Arc::new(Capture::new()?);
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
            "Usage: ssmcp attach [list] [--json]\n       ssmcp attach NAME [--read-only]\n\nAttach only to an existing running local session. Ctrl-] detaches; the session keeps running.\nCtrl-C reaches the inner terminal while input is enabled. F7 cycles session terminals.\n--read-only permits another observer without taking input ownership."
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
    let (directory, _) = super::load(&root, &name)?;
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
    let result = attached(&name, directory.clone(), &lease, read_only, &open).await;
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
    name: &str,
    directory: PathBuf,
    lease: &str,
    read_only: bool,
    open: &Value,
) -> Result<()> {
    use crate::client::{render, terminal, ui::Ui};
    let ui = Ui::load(None)?;
    crate::client::interface::initialize_path(None);
    render::configure(ui.clone());
    let display = terminal::Session::start(&ui)?;
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
    let mut keyboard = input::Keyboard::open()?;
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
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = interrupt.recv() => break,
                _ = terminate.recv() => break,
                event = keyboard.next() => match event? {
                    input::Event::Detach | input::Event::End => break,
                    input::Event::Resize(rows, cols) => {
                        let (rows, cols) = terminal::physical_resize(rows,cols);
                        geometry=Some((rows,cols));
                        if !read_only && let Some(generation)=backend_generation { ipc::request(&directory, ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?; }
                    }
                    input::Event::Text(text) => {
                        if read_only && text.contains('\u{3}') { break; }
                        if !read_only && backend_generation.is_none() && !waiting_input_warned {
                            render::status("attach","Warning","Session is connecting. Terminal input is unavailable and is not queued for later execution.");waiting_input_warned=true;
                        }
                        apply_keys(&directory,lease,&mut selected,&terminal_ids,keys.feed(&text,Instant::now(),false),read_only,backend_generation).await?;
                    }
                },
                page = pages.recv() => {
                    let page = page.ok_or("attach output stream ended")?.map_err(|error| format!("attach ended: {error}; the session may be reconnecting, stopped, or restarted"))?;
                    if page["truncated"] == true { render::status("attach","Warning","Earlier display history was evicted; showing the retained tail. Use session logs for older output."); }
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
                        replay(event)?;
                    }
                }
                _ = tick.tick() => {
                    apply_keys(&directory,lease,&mut selected,&terminal_ids,keys.feed("",Instant::now(),false),read_only,backend_generation).await?;
                    let queued = terminal::take_keyboard();
                    if let Some((rows,cols)) = crate::client::interface::apply_pending()? {
                        geometry=Some((rows,cols));
                        if !read_only && let Some(generation)=backend_generation { ipc::request(&directory,ENDPOINT,json!({"op":"resize","lease":lease,"backend_generation":generation,"rows":rows,"cols":cols})).await?; }
                    }
                    if !read_only && !queued.is_empty() && let Some(generation)=backend_generation { send_input(&directory,lease,generation,terminal_ids.get(selected).map(String::as_str).unwrap_or("main"),queued).await?; }
                    for (id,bytes) in terminal::take_session_replies() {
                        if !read_only && let Some(generation)=backend_generation { send_input(&directory,lease,generation,&id,String::from_utf8_lossy(&bytes).into_owned()).await?; }
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

fn replay(event: &Value) -> Result<()> {
    match event["kind"].as_str() {
        Some("event") => crate::client::render::event(
            event["label"].as_str().unwrap_or("session"),
            &event["value"],
        ),
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
                let text = crate::client::terminal::keyboard(&text);
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
