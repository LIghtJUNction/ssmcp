//! Project Z frontends use the same protected local attach authority as the
//! terminal. A lease is a host capability, never an account or agent identity.
mod chat;
mod observation;
mod remote;
pub use observation::{EventObserver, MAX_OBSERVATION_MS};
pub use remote::RemoteFrontend;

use super::{Capture, Runtime};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::Semaphore;
use zbridge::{
    Access, BridgeError, Capabilities, Command, CommandResult, Counter, ErrorCode, Event,
    EventPage, Host, HostFuture, Id, Mode, Request, Resource, Response, ResponseBody, State,
};

pub(super) struct Guard {
    instance: u64,
    closed: AtomicBool,
    inflight: Semaphore,
    chat_receipts: chat::Receipts,
}

impl Default for Guard {
    fn default() -> Self {
        static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);
        let instance = NEXT_INSTANCE
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("attach bridge instance counter exhausted");
        Self {
            instance,
            closed: AtomicBool::new(false),
            inflight: Semaphore::new(crate::wire::MAX_INFLIGHT),
            chat_receipts: chat::Receipts::default(),
        }
    }
}

impl Guard {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.inflight.close();
    }

    fn check(&self) -> Result<(), BridgeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "attach frontend host is closed",
            ));
        }
        Ok(())
    }
}

/// An embedded frontend connection minted by `attach::Handle::frontend`.
/// It owns one actual attach lease and releases it on drop. It is deliberately
/// neither serializable nor constructible from caller-supplied identity fields.
pub struct Frontend {
    capture: Weak<Capture>,
    lease: String,
}

impl Frontend {
    pub(super) fn open(capture: &Arc<Capture>, read_only: bool) -> crate::Result<Self> {
        capture.bridge.check()?;
        let lease = capture.open(read_only)?;
        Ok(Self {
            capture: Arc::downgrade(capture),
            lease,
        })
    }

    pub async fn request(&self, request: Request) -> Response {
        let Some(capture) = self.capture.upgrade() else {
            return failure(
                request.id,
                ErrorCode::Closed,
                "attach frontend host is closed",
            );
        };
        self::request(&capture, &self.lease, request).await
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        if let Some(capture) = self.capture.upgrade() {
            capture
                .leases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(&self.lease);
        }
    }
}

fn failure(id: Id, code: ErrorCode, message: &str) -> Response {
    Response {
        version: zbridge::VERSION,
        id,
        generation: None,
        body: ResponseBody::Error(BridgeError::new(code, message)),
    }
}

pub(super) async fn request(capture: &Capture, lease: &str, request: Request) -> Response {
    if let Err(error) = capture.bridge.check() {
        return failure(request.id, error.code, &error.message);
    }
    let Ok(_permit) = capture.bridge.inflight.try_acquire() else {
        return failure(
            request.id,
            ErrorCode::Unavailable,
            "attach bridge request limit reached",
        );
    };
    let backend_generation = capture
        .runtime
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .generation;
    let command = request.operation.is_command();
    let mut response = zbridge::dispatch(
        &BoundHost {
            capture,
            lease,
            backend_generation,
        },
        request,
    )
    .await;
    // The existing IPC adds its own reply envelope outside the portable frame.
    // Reserve room for it rather than discovering overflow after a mutation.
    if let Err(error) = zbridge::encoded_len(&response, zbridge::MAX_FRAME_BYTES - 1024) {
        response.body = ResponseBody::Error(if command { error.uncertain() } else { error });
    }
    response
}

struct BoundHost<'a> {
    capture: &'a Capture,
    lease: &'a str,
    backend_generation: u64,
}

impl BoundHost<'_> {
    fn mode(&self) -> Result<Mode, BridgeError> {
        self.authorize(Access::Observe)?;
        let leases = self
            .capture
            .leases
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let lease = leases
            .get(self.lease)
            .ok_or_else(|| BridgeError::new(ErrorCode::Denied, "attach lease expired"))?;
        Ok(if lease.read_only {
            Mode::Observe
        } else {
            Mode::Control
        })
    }

    fn runtime(&self) -> Result<Arc<Runtime>, BridgeError> {
        self.capture
            .runtime(self.backend_generation)
            .map_err(|error| BridgeError::new(ErrorCode::Unavailable, error.to_string()))
    }

    fn optional_runtime(&self) -> Result<Option<Arc<Runtime>>, BridgeError> {
        let slot = self
            .capture
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.generation != self.backend_generation {
            return Err(BridgeError::new(
                ErrorCode::StaleGeneration,
                "attach backend changed",
            ));
        }
        Ok(slot.runtime.clone())
    }
}

fn id(value: &str) -> Id {
    Id::new(value).expect("static bridge identifier")
}

fn no_query(query: Value) -> Result<(), BridgeError> {
    if !query.as_object().is_some_and(|object| object.is_empty()) {
        return Err(BridgeError::invalid(
            "this state resource accepts an empty query",
        ));
    }
    Ok(())
}

fn native(error: String) -> BridgeError {
    BridgeError::new(ErrorCode::Unavailable, error)
}

impl Host for BoundHost<'_> {
    fn authorize(&self, access: Access) -> Result<(), BridgeError> {
        self.capture.bridge.check()?;
        if self.lease.len() > 128 {
            return Err(BridgeError::new(ErrorCode::Denied, "invalid attach lease"));
        }
        self.capture
            .touch(self.lease, access == Access::Control)
            .map_err(|error| BridgeError::new(ErrorCode::Denied, error.to_string()))
    }

    fn generation(&self) -> Result<Id, BridgeError> {
        let slot = self
            .capture
            .runtime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let connection = slot
            .runtime
            .as_ref()
            .map_or(0, |runtime| runtime.connection_generation());
        // The journal can outlive a listener and a backend. Include their own
        // actual lifetimes so neither listener recreation nor unbinding can
        // make an old command or late result current again.
        Id::new(format!(
            "{}:{}:{}:{}",
            self.capture.generation, self.capture.bridge.instance, slot.generation, connection
        ))
    }

    fn capabilities(&self) -> Result<Capabilities, BridgeError> {
        let mode = self.mode()?;
        let runtime = self.optional_runtime()?;
        let mut resources = vec![Resource {
            id: id("runtime"),
            schema_version: 1,
            description: "Actual backend presence, registry summary and live event cursor".into(),
        }];
        let mut commands = Vec::new();
        if let Some(runtime) = runtime {
            runtime.local_registry().ensure_open().map_err(native)?;
            resources.push(Resource {
                id: id("terminals"),
                schema_version: 1,
                description: "The current backend's actual terminal list".into(),
            });
            if runtime.frontend_client_id().is_some() {
                resources.push(Resource { id:id("conversation"), schema_version:1,
                    description:"Bounded retained conversation and tool events from authenticated provenance; optional thread_id or main filter, decimal-string cursor, limit 1..64".into() });
            }
            if let Some(client_id) = runtime.frontend_client_id()
                && let Ok(metadata) = crate::client::steering::frontend_metadata(client_id)
            {
                resources.push(Resource {
                    id: id("threads"),
                    schema_version: 1,
                    description:
                        "Actual connected agent threads and chat readiness; offset and limit 1..32"
                            .into(),
                });
                resources.push(Resource { id:id("chat.queue"), schema_version:1,
                    description:"The existing local request outbox with actual delivery states; offset and limit 1..32, at most 64 KiB per page".into() });
                if mode == Mode::Control
                    && metadata["chat_enabled"] == true
                    && metadata["supported"] == true
                {
                    commands.push(chat::command());
                }
            }
            let workers = runtime.frontend_has_workers();
            if workers {
                resources.push(Resource { id:id("workers"), schema_version:1,
                    description:"Actual worker registration and bounded station state; query offset and limit (1..32), with logs omitted".into() });
            }
            if mode == Mode::Control {
                commands.push(Command { id:id("terminal.write"),
                    description:"Write explicit UTF-8 input to one real terminal under the exclusive local input lease; never automatically retry a lost receipt".into(),
                    input_schema:json!({"type":"object","additionalProperties":false,
                        "properties":{"terminal_id":{"type":"string","minLength":1,"maxLength":128},
                        "data":{"type":"string","maxLength":crate::wire::MAX_WRITE}},"required":["terminal_id","data"]}) });
                commands.push(Command { id:id("terminal.resize"),
                    description:"Resize the current backend's terminals under the exclusive local input lease".into(),
                    input_schema:json!({"type":"object","additionalProperties":false,
                        "properties":{"rows":{"type":"integer","minimum":1,"maximum":1000},
                        "cols":{"type":"integer","minimum":1,"maximum":1000}},"required":["rows","cols"]}) });
                if workers {
                    commands.push(Command { id:id("workers.resize"),
                        description:"Set this remote coordination pool's desired size; requires its current pool_generation. Native retirement acknowledgements remain mandatory and no agent is started".into(),
                        input_schema:json!({"type":"object","additionalProperties":false,
                            "properties":{"pool_generation":{"type":"string","minLength":1,"maxLength":128},
                            "total":{"type":"integer","minimum":0,"maximum":128}},"required":["pool_generation","total"]}) });
                }
            }
        }
        Ok(Capabilities {
            application: id("ssmcp"),
            mode,
            resources,
            commands,
            events: true,
            max_frame_bytes: (zbridge::MAX_FRAME_BYTES - 1024) as u32,
            max_event_page_bytes: zbridge::MAX_PAGE_BYTES as u32,
        })
    }

    fn state(&self, resource: Id, query: Value) -> HostFuture<'_, State> {
        Box::pin(async move {
            self.authorize(Access::Observe)?;
            let value = match resource.as_str() {
                "runtime" => {
                    no_query(query)?;
                    let runtime = self.optional_runtime()?;
                    let mut capabilities = runtime
                        .as_ref()
                        .map(|runtime| runtime.local_registry().capabilities())
                        .transpose()
                        .map_err(native)?;
                    if let Some(value) = capabilities.as_mut()
                        && let Some(generation) = value.get("generation").and_then(Value::as_u64)
                    {
                        value["generation"] = json!(Counter(generation));
                    }
                    let cursor = self
                        .capture
                        .ring
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .next;
                    json!({"backend_active":runtime.is_some(),"backend_generation":Counter(self.backend_generation),
                        "native_capabilities":capabilities,"live_cursor":Counter(cursor),"retention":"live_process_memory"})
                }
                "terminals" => {
                    no_query(query)?;
                    self.runtime()?
                        .terminals()
                        .call("terminal.list", json!({}))
                        .await
                        .map_err(native)?
                }
                "workers" => {
                    let query: WorkerQuery = serde_json::from_value(query)
                        .map_err(|_| BridgeError::invalid("invalid worker state query"))?;
                    if query.limit == 0 || query.limit > 32 {
                        return Err(BridgeError::invalid(
                            "worker state page limit must be 1..32",
                        ));
                    }
                    let snapshot = self.runtime()?.frontend_workers().ok_or_else(|| {
                        BridgeError::new(ErrorCode::Unavailable, "this backend has no worker pool")
                    })?;
                    worker_state(snapshot, query)?
                }
                "threads" | "chat.queue" | "conversation" => {
                    chat::state(self, resource.as_str(), query)?
                }
                _ => {
                    return Err(BridgeError::new(
                        ErrorCode::Unavailable,
                        "unknown bridge state resource",
                    ));
                }
            };
            Ok(State {
                resource,
                schema_version: 1,
                revision: None,
                value,
            })
        })
    }

    fn events(
        &self,
        cursor: Option<Counter>,
        limit: u16,
        wait_ms: u16,
    ) -> HostFuture<'_, EventPage> {
        Box::pin(async move {
            self.authorize(Access::Observe)?;
            let requested = cursor.map_or_else(
                || {
                    self.capture
                        .ring
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .next
                },
                |cursor| cursor.0,
            );
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_millis(u64::from(wait_ms));
            loop {
                let changed = self.capture.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(page) = event_page(
                    self.capture,
                    requested,
                    limit,
                    wait_ms == 0 || tokio::time::Instant::now() >= deadline,
                )? {
                    return Ok(page);
                }
                let _ = tokio::time::timeout_at(deadline, changed).await;
                self.authorize(Access::Observe)?;
                // Generation changes during a bounded wait are checked before
                // a page is returned as well as by zbridge's final gate.
                self.optional_runtime()?;
            }
        })
    }

    fn command(&self, command: Id, arguments: Value) -> HostFuture<'_, CommandResult> {
        Box::pin(async move {
            self.authorize(Access::Control)?;
            let runtime = self.runtime()?;
            runtime.local_registry().ensure_open().map_err(native)?;
            let value = match command.as_str() {
                "terminal.write" => {
                    let args: Write = serde_json::from_value(arguments)
                        .map_err(|_| BridgeError::invalid("invalid terminal write"))?;
                    if args.terminal_id.is_empty()
                        || args.terminal_id.len() > 128
                        || args.data.len() > crate::wire::MAX_WRITE
                    {
                        return Err(BridgeError::invalid("terminal write exceeds its bounds"));
                    }
                    self.authorize(Access::Control)?;
                    self.runtime()?;
                    runtime
                        .terminals()
                        .call(
                            "terminal.write",
                            json!({"terminal_id":args.terminal_id,"data":args.data}),
                        )
                        .await
                        .map_err(|error| native(error).uncertain())?
                }
                "terminal.resize" => {
                    let args: Resize = serde_json::from_value(arguments)
                        .map_err(|_| BridgeError::invalid("invalid terminal resize"))?;
                    if args.rows == 0 || args.cols == 0 || args.rows > 1000 || args.cols > 1000 {
                        return Err(BridgeError::invalid("terminal size must be 1..1000"));
                    }
                    self.authorize(Access::Control)?;
                    self.runtime()?;
                    runtime
                        .terminals()
                        .resize_all(args.rows, args.cols)
                        .await
                        .map_err(|error| native(error).uncertain())?;
                    json!({"rows":args.rows,"cols":args.cols})
                }
                "workers.resize" => {
                    let args: WorkerResize = serde_json::from_value(arguments)
                        .map_err(|_| BridgeError::invalid("invalid worker resize"))?;
                    if args.total > 128
                        || args.pool_generation.is_empty()
                        || args.pool_generation.len() > 128
                    {
                        return Err(BridgeError::invalid(
                            "invalid worker pool generation or total",
                        ));
                    }
                    self.authorize(Access::Control)?;
                    self.runtime()?;
                    runtime
                        .frontend_resize_workers(&args.pool_generation, usize::from(args.total))
                        .map_err(native)?
                }
                "chat.send" => chat::send(self, &runtime, arguments)?,
                _ => {
                    return Err(BridgeError::new(
                        ErrorCode::Denied,
                        "bridge command was not granted",
                    ));
                }
            };
            Ok(CommandResult { command, value })
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Write {
    terminal_id: String,
    data: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resize {
    rows: u16,
    cols: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerResize {
    pool_generation: String,
    total: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerQuery {
    #[serde(default)]
    offset: u16,
    #[serde(default = "default_worker_limit")]
    limit: u16,
}
fn default_worker_limit() -> u16 {
    16
}

fn worker_state(
    mut snapshot: crate::worker::Snapshot,
    query: WorkerQuery,
) -> Result<Value, BridgeError> {
    let count = snapshot.slots.len();
    let offset = usize::from(query.offset);
    if offset > count {
        return Err(BridgeError::invalid(
            "worker state offset exceeds current stations",
        ));
    }
    let end = (offset + usize::from(query.limit)).min(count);
    let mut slots = Vec::new();
    for slot in snapshot.slots.drain(offset..end) {
        slots.push(json!({"slot":slot.slot,"epoch":Counter(slot.epoch),"state":slot.state,
            "task_id":slot.task_id,"reason":slot.reason,"registered":slot.registered,
            "retire_requested":slot.retire_requested,"task_summary":slot.task_summary,
            "last_command":slot.last_command,"progress":slot.progress,
            "total_work_ms":Counter(slot.total_work_ms),
            "current_task_elapsed_ms":slot.current_task_elapsed_ms.map(Counter),
            "task_started_at_ms":slot.task_started_at_ms.map(Counter),
            "recent_task_durations_ms":slot.recent_task_durations_ms.into_iter().map(Counter).collect::<Vec<_>>(),
            "next_log_sequence":Counter(slot.next_log_sequence),"logs_omitted":true}));
    }
    let manager = snapshot.manager.map(|manager| {
        json!({"client_id":manager.client_id,
        "thread_id":manager.thread_id,"participant_id":manager.participant_id,
        "connection":Counter(manager.connection),"attached_at_ms":Counter(manager.attached_at_ms)})
    });
    Ok(
        json!({"process_id":snapshot.process_id,"pool_generation":snapshot.generation,"manager":manager,
        "total":snapshot.total,"desired_total":snapshot.desired_total,"idle":snapshot.idle,
        "working":snapshot.working,"offline":snapshot.offline,"retiring":snapshot.retiring,
        "registered_workers":snapshot.registered_workers,"queued":snapshot.queued,
        "closed":snapshot.closed,"stopping":snapshot.stopping,"slots":slots,"slot_count":count,
        "offset":offset,"next_offset":(end<count).then_some(end),"logs_omitted":true}),
    )
}

fn event_page(
    capture: &Capture,
    requested: u64,
    limit: u16,
    expired: bool,
) -> Result<Option<EventPage>, BridgeError> {
    let ring = capture
        .ring
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if ring.next == u64::MAX {
        return Err(BridgeError::new(
            ErrorCode::Closed,
            "event sequence exhausted",
        ));
    }
    if requested > ring.next {
        return Err(BridgeError::new(
            ErrorCode::InvalidCursor,
            "event cursor is ahead of this live process",
        ));
    }
    if requested == ring.next && !expired {
        return Ok(None);
    }
    let start = ring.events.front().map_or(ring.next, |event| event.0);
    let mut next = requested.max(start);
    let mut events = Vec::new();
    let mut bytes = 1024;
    for (sequence, raw, _, _) in &ring.events {
        if *sequence < next {
            continue;
        }
        if events.len() >= usize::from(limit) {
            break;
        }
        let mut value: Value = serde_json::from_str(raw.get()).map_err(|_| {
            BridgeError::new(ErrorCode::Internal, "cannot decode retained native event")
        })?;
        value["sequence"] = json!(Counter(*sequence));
        if let Some(source) = value.get_mut("source").and_then(Value::as_object_mut) {
            for key in ["connection", "request_id"] {
                if let Some(counter) = source.get(key).and_then(Value::as_u64) {
                    source.insert(key.into(), json!(Counter(counter)));
                }
            }
        }
        let topic = value
            .get("kind")
            .and_then(Value::as_str)
            .and_then(|kind| Id::new(format!("ssmcp.{kind}")).ok())
            .unwrap_or_else(|| id("ssmcp.event"));
        let event = Event {
            sequence: Counter(*sequence),
            topic,
            value,
        };
        let size = zbridge::encoded_len(&event, zbridge::MAX_PAGE_BYTES - 2048)?;
        if bytes + size > zbridge::MAX_PAGE_BYTES {
            break;
        }
        bytes += size;
        next = sequence + 1;
        events.push(event);
    }
    Ok(Some(EventPage {
        events,
        start_cursor: Counter(start),
        next_cursor: Counter(next),
        live_cursor: Counter(ring.next),
        retention_gap: requested < start,
        has_more: next < ring.next,
    }))
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
#[path = "zbridge_adapter/gui_gateway_tests.rs"]
mod gui_gateway_tests;
