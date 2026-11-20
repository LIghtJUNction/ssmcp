//! Connection-local timers and passive native-wait presentation.
//! Deadlines use Instant; civil inputs are converted once when a timer starts.
mod civil;
mod execution;
mod relay;
use super::{
    call_scope::{Source, ThreadScope},
    native_host::{Call, NativePlugin, PluginFuture},
};
use crate::{
    client::interface::history::{self, HistoryOwner},
    registry::Provider,
    wire::ControlAttribution,
};
pub(crate) use execution::ExecutionTiming;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const MAX_TIMERS: usize = 32;
const MAX_RECORDS: usize = 128;
const MAX_OBSERVATIONS: usize = 128;
const MAX_DURATION_MS: u64 = 86_400_000;
const MAX_WAIT_MS: u64 = 25_000;
const RETENTION: Duration = Duration::from_secs(300);

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn remaining_millis(deadline: Instant, now: Instant) -> u64 {
    deadline
        .saturating_duration_since(now)
        .as_nanos()
        .div_ceil(1_000_000)
        .min(u128::from(u64::MAX)) as u64
}

fn changed() {
    crate::client::terminal::invalidate();
}

fn display_clock() -> &'static Mutex<Weak<Clock>> {
    static DISPLAY: OnceLock<Mutex<Weak<Clock>>> = OnceLock::new();
    DISPLAY.get_or_init(Mutex::default)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Owner {
    connection: Option<u64>,
    attribution: Option<ControlAttribution>,
    thread: Option<ThreadScope>,
    history: Option<HistoryOwner>,
}

impl Owner {
    fn from_source(source: &Source) -> Self {
        Self {
            connection: source.call.as_ref().map(|call| call.connection()),
            attribution: source
                .call
                .as_ref()
                .and_then(|call| call.attribution().cloned()),
            thread: source.call.as_ref().and_then(|call| call.thread().cloned()),
            history: source.owner().cloned(),
        }
    }

    fn visible(&self, history: &Option<HistoryOwner>) -> bool {
        &self.history == history
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Running,
    Elapsed,
    Cancelled,
    Expired,
    Disconnected,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Entry {
    pub id: String,
    pub kind: &'static str,
    pub label: String,
    pub status: Status,
    pub elapsed_ms: u64,
    pub remaining_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_age_ms: Option<u64>,
    pub cancellable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<ControlAttribution>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Snapshot {
    pub active: Vec<Entry>,
    pub recent: Vec<Entry>,
}

struct Timer {
    id: String,
    owner: Owner,
    label: String,
    started: Instant,
    deadline: Option<Instant>,
    deadline_unix_ms: Option<i64>,
    ended: Option<Instant>,
    status: Status,
    changed: watch::Sender<Status>,
}

impl Timer {
    fn finish(&mut self, status: Status, now: Instant) {
        if self.status == Status::Running {
            self.status = status;
            self.ended = Some(now);
            self.changed.send_replace(status);
        }
    }

    fn tick(&mut self, now: Instant) {
        if let Some(deadline) = self.deadline {
            if now >= deadline {
                self.finish(Status::Elapsed, deadline);
            }
        } else if now.saturating_duration_since(self.started)
            >= Duration::from_millis(MAX_DURATION_MS)
        {
            // An open-ended elapsed timer also has a finite local lifetime.
            self.finish(
                Status::Expired,
                self.started + Duration::from_millis(MAX_DURATION_MS),
            );
        }
    }

    fn entry(&self, now: Instant) -> Entry {
        let through = self.ended.unwrap_or(now);
        Entry {
            id: self.id.clone(),
            kind: "timer",
            label: self.label.clone(),
            status: self.status,
            elapsed_ms: millis(through.saturating_duration_since(self.started)),
            remaining_ms: self
                .deadline
                .map(|deadline| remaining_millis(deadline, through)),
            deadline_unix_ms: self.deadline_unix_ms,
            finished_age_ms: self
                .ended
                .map(|ended| millis(now.saturating_duration_since(ended))),
            cancellable: self.status == Status::Running,
            episode: None,
            attribution: None,
        }
    }
}

struct Observation {
    id: String,
    owner: Owner,
    label: String,
    started: Instant,
    deadline: Option<Instant>,
    cancellable: bool,
    cancel: watch::Sender<bool>,
}

impl Observation {
    fn entry(&self, now: Instant) -> Entry {
        Entry {
            id: self.id.clone(),
            kind: "observation",
            label: self.label.clone(),
            status: Status::Running,
            elapsed_ms: millis(now.saturating_duration_since(self.started)),
            remaining_ms: self
                .deadline
                .map(|deadline| remaining_millis(deadline, now)),
            deadline_unix_ms: None,
            finished_age_ms: None,
            cancellable: self.cancellable,
            episode: None,
            attribution: None,
        }
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    time_tools: bool,
    relay: relay::State,
    next: u64,
    timers: BTreeMap<String, Timer>,
    observations: BTreeMap<String, Observation>,
}

impl State {
    fn tick(&mut self, now: Instant) {
        for timer in self.timers.values_mut() {
            timer.tick(now);
        }
        self.timers.retain(|_, timer| {
            timer
                .ended
                .is_none_or(|ended| now.saturating_duration_since(ended) < RETENTION)
        });
    }

    fn id(&mut self, clock: &str, prefix: &str) -> Result<String, String> {
        self.next = self
            .next
            .checked_add(1)
            .ok_or("time identifier space exhausted")?;
        Ok(format!("{prefix}_{clock}_{:x}", self.next))
    }
}

pub(crate) struct Clock {
    id: String,
    started: Instant,
    state: Mutex<State>,
}

impl Clock {
    pub(super) fn configure(&self, time_tools: bool, wait_display: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.time_tools = time_tools && !state.closed;
        let display_enabled = wait_display && !state.closed;
        state.relay.configure(display_enabled);
    }

    pub(super) fn time_tools_enabled(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .time_tools
    }

    pub(super) fn relay_display(
        &self,
        snapshot: crate::wire::wait_display::Snapshot,
    ) -> Result<(), String> {
        let changed_display = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .relay
            .apply(snapshot, Instant::now())?;
        if changed_display {
            changed();
        }
        Ok(())
    }

    pub(super) fn disconnect(&self) {
        self.close();
    }

    pub(super) fn new() -> Result<Arc<Self>, String> {
        // Process-local counters restart during reload. An OS-generated scope
        // prevents a retained timer/watch ID from naming a new connection's
        // first object, even when the original caller and history match.
        let mut bytes = [0_u8; 16];
        crate::platform::random_bytes(&mut bytes)
            .map_err(|error| format!("could not allocate a time namespace: {error}"))?;
        Ok(Arc::new(Self {
            id: bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
            started: Instant::now(),
            state: Mutex::new(State::default()),
        }))
    }

    pub(super) fn bind(self: &Arc<Self>) {
        *display_clock()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Arc::downgrade(self);
    }

    fn now(&self, mut arguments: Value) -> Result<Value, String> {
        if let Some(object) = arguments.as_object_mut()
            && let Some(op) = object.remove("op")
        {
            match op.as_str() {
                // Keep the existing tool name: old relays classify now/wait
                // by exact name when negotiating time_observation_v1.
                Some("interval") => return civil::interval(arguments, civil::now_ms()?),
                Some("now") => {}
                _ => return Err("time.now op must be now or interval".into()),
            }
        }
        if !arguments.as_object().is_some_and(|args| args.is_empty()) {
            return Err("time.now takes no time points unless op is interval".into());
        }
        let unix = civil::now_ms()?;
        Ok(
            json!({"schema_version":1,"unix_ms":unix,"unix_seconds":unix.div_euclid(1000),
            "utc_rfc3339":civil::format(unix)?,
            "wall_clock":"UTC Unix epoch","monotonic_ms":millis(self.started.elapsed()),
            "monotonic_scope":format!("clock_{}",self.id),"deadline_clock":"monotonic"}),
        )
    }

    fn start(
        &self,
        owner: Owner,
        duration_ms: Option<u64>,
        label: Option<String>,
        now: Instant,
    ) -> Result<String, String> {
        self.start_inner(owner, duration_ms, None, label, now)
    }

    fn start_absolute(
        &self,
        owner: Owner,
        deadline_unix_ms: i64,
        label: Option<String>,
        now: Instant,
        wall_now_ms: i64,
    ) -> Result<String, String> {
        civil::format(deadline_unix_ms)?;
        let duration_ms = deadline_unix_ms.saturating_sub(wall_now_ms).max(0) as u64;
        self.start_inner(owner, Some(duration_ms), Some(deadline_unix_ms), label, now)
    }

    fn start_inner(
        &self,
        owner: Owner,
        duration_ms: Option<u64>,
        deadline_unix_ms: Option<i64>,
        label: Option<String>,
        now: Instant,
    ) -> Result<String, String> {
        if duration_ms.is_some_and(|value| value > MAX_DURATION_MS) {
            return Err(
                "duration_ms must be 0..=86400000; omit it for a bounded elapsed timer".into(),
            );
        }
        let label = clean_label(label)?.unwrap_or_else(|| {
            if duration_ms.is_some() {
                "Timer"
            } else {
                "Elapsed timer"
            }
            .into()
        });
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        if state.closed {
            return Err("time connection is closed".into());
        }
        if state
            .timers
            .values()
            .filter(|timer| timer.status == Status::Running)
            .count()
            >= MAX_TIMERS
        {
            return Err("time timer limit reached (32 active timers)".into());
        }
        if state.timers.len() >= MAX_RECORDS {
            let oldest = state
                .timers
                .values()
                .filter_map(|timer| timer.ended.map(|ended| (ended, timer.id.clone())))
                .min()
                .map(|(_, id)| id);
            if let Some(id) = oldest {
                state.timers.remove(&id);
            }
        }
        let id = state.id(&self.id, "timer")?;
        let deadline = duration_ms.map(|value| now + Duration::from_millis(value));
        let (changed, _) = watch::channel(Status::Running);
        let mut timer = Timer {
            id: id.clone(),
            owner,
            label,
            started: now,
            deadline,
            deadline_unix_ms,
            ended: None,
            status: Status::Running,
            changed,
        };
        timer.tick(now);
        state.timers.insert(id.clone(), timer);
        Ok(id)
    }

    fn list(&self, owner: &Owner, now: Instant) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        if state.closed {
            return Err("time connection is closed".into());
        }
        let mut timers = state
            .timers
            .values()
            .filter(|timer| &timer.owner == owner)
            .collect::<Vec<_>>();
        timers.sort_by_key(|timer| {
            (
                timer.status != Status::Running,
                std::cmp::Reverse(timer.started),
            )
        });
        let entries = |running| {
            timers
                .iter()
                .filter(|timer| (timer.status == Status::Running) == running)
                .map(|timer| receipt(timer.entry(now), false))
                .collect::<Vec<_>>()
        };
        Ok(
            json!({"schema_version":1,"active":entries(true),"recent":entries(false),
            "deadline_clock":"monotonic","retention_ms":millis(RETENTION),"completion_permission":false}),
        )
    }

    fn read(
        &self,
        owner: &Owner,
        id: &str,
        now: Instant,
    ) -> Result<(Entry, watch::Receiver<Status>, Option<Instant>), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        if state.closed {
            return Err("time connection is closed".into());
        }
        let timer = state
            .timers
            .get(id)
            .filter(|timer| &timer.owner == owner)
            .ok_or("time timer is unknown, expired, or belongs to another caller")?;
        Ok((
            timer.entry(now),
            timer.changed.subscribe(),
            timer
                .deadline
                .or(Some(timer.started + Duration::from_millis(MAX_DURATION_MS))),
        ))
    }

    fn cancel_timer(&self, owner: &Owner, id: &str, now: Instant) -> Result<Entry, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        let timer = state
            .timers
            .get_mut(id)
            .filter(|timer| &timer.owner == owner)
            .ok_or("time timer is unknown, expired, or belongs to another caller")?;
        timer.finish(Status::Cancelled, now);
        Ok(timer.entry(now))
    }

    async fn wait(&self, mut arguments: Value) -> Result<Value, String> {
        if let Some(object) = arguments.as_object_mut() {
            object.entry("op").or_insert_with(|| json!("start"));
        }
        let request: Request = serde_json::from_value(arguments)
            .map_err(|_| "time.wait expects start(duration_ms? or deadline_unix_ms?, label?, wait_ms?), status(timer_id, wait_ms?), cancel(timer_id), or list()")?;
        let owner = Owner::from_source(&Source::capture());
        let (id, wait_ms) = match request {
            Request::Start {
                duration_ms,
                deadline_unix_ms,
                label,
                wait_ms,
            } => {
                validate_wait(wait_ms)?;
                if duration_ms.is_some() && deadline_unix_ms.is_some() {
                    return Err("use duration_ms or deadline_unix_ms, never both".into());
                }
                let id = if let Some(deadline) = deadline_unix_ms {
                    // Sample wall time once, then keep only the monotonic
                    // deadline for elapsed/remaining and waiting behavior.
                    let wall_now_ms = civil::now_ms()?;
                    self.start_absolute(
                        owner.clone(),
                        deadline,
                        label,
                        Instant::now(),
                        wall_now_ms,
                    )?
                } else {
                    self.start(owner.clone(), duration_ms, label, Instant::now())?
                };
                changed();
                (id, wait_ms)
            }
            Request::Status { timer_id, wait_ms } => {
                validate_wait(wait_ms)?;
                validate_id(&timer_id)?;
                (timer_id, wait_ms)
            }
            Request::Cancel { timer_id } => {
                validate_id(&timer_id)?;
                let entry = self.cancel_timer(&owner, &timer_id, Instant::now())?;
                changed();
                return Ok(receipt(entry, false));
            }
            Request::List {} => return self.list(&owner, Instant::now()),
        };
        let (entry, mut receiver, deadline) = self.read(&owner, &id, Instant::now())?;
        if entry.status != Status::Running || wait_ms == 0 {
            return Ok(receipt(entry, false));
        }
        let until = Instant::now() + Duration::from_millis(wait_ms);
        let until = deadline.map_or(until, |deadline| until.min(deadline));
        tokio::select! {
            _ = receiver.changed() => {},
            _ = tokio::time::sleep_until(until.into()) => {},
        }
        let (entry, _, _) = self.read(&owner, &id, Instant::now())?;
        let timed_out = entry.status == Status::Running;
        changed();
        Ok(receipt(entry, timed_out))
    }

    pub(super) fn observe(
        self: &Arc<Self>,
        name: &str,
        arguments: &Value,
        source: &Source,
    ) -> Option<ObservationGuard> {
        let spec = observation(name, arguments)?;
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.observations.len() >= MAX_OBSERVATIONS {
            return None;
        }
        let id = state.id(&self.id, "watch").ok()?;
        let (cancel, receiver) = watch::channel(false);
        state.observations.insert(
            id.clone(),
            Observation {
                id: id.clone(),
                owner: Owner::from_source(source),
                label: name.into(),
                started: now,
                deadline: spec
                    .timeout_ms
                    .map(|duration| now + Duration::from_millis(duration)),
                cancellable: spec.cancellable,
                cancel,
            },
        );
        drop(state);
        changed();
        Some(ObservationGuard {
            clock: self.clone(),
            id,
            receiver,
        })
    }

    fn snapshot(&self, owner: &Option<HistoryOwner>, now: Instant) -> Snapshot {
        // A frame may hold the compositor lock. Never call invalidate here.
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        if state.closed {
            return Snapshot::default();
        }
        let mut snapshot = Snapshot::default();
        for timer in state
            .timers
            .values()
            .filter(|timer| timer.owner.visible(owner))
        {
            if timer.status == Status::Running {
                snapshot.active.push(timer.entry(now));
            } else {
                snapshot.recent.push(timer.entry(now));
            }
        }
        snapshot.active.extend(
            state
                .observations
                .values()
                .filter(|wait| wait.owner.visible(owner))
                .map(|wait| wait.entry(now)),
        );
        // Relay observations have explicit account/agent labels and are global
        // display only. They never borrow the selected conversation's owner.
        snapshot.active.extend(state.relay.entries(now));
        snapshot.recent.sort_by_key(|entry| entry.finished_age_ms);
        snapshot.recent.truncate(4);
        snapshot
    }

    fn cancel_local(
        &self,
        id: &str,
        owner: &Option<HistoryOwner>,
        now: Instant,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.tick(now);
        if let Some(timer) = state
            .timers
            .get_mut(id)
            .filter(|timer| timer.owner.visible(owner))
        {
            if timer.status != Status::Running {
                return Err("This timer has already finished".into());
            }
            timer.finish(Status::Cancelled, now);
            return Ok(());
        }
        if let Some(wait) = state
            .observations
            .get(id)
            .filter(|wait| wait.owner.visible(owner))
        {
            if !wait.cancellable {
                return Err("This wait is display-only; use its own cancellation control".into());
            }
            wait.cancel.send_replace(true);
            return Ok(());
        }
        Err("No active timer or observation with this exact ID in the current conversation".into())
    }

    fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.time_tools = false;
        state.relay.configure(false);
        let now = Instant::now();
        for timer in state.timers.values_mut() {
            timer.finish(Status::Disconnected, now);
        }
        for wait in state.observations.values() {
            wait.cancel.send_replace(true);
        }
        state.timers.clear();
        state.observations.clear();
        drop(state);
        changed();
    }
}

fn receipt(entry: Entry, timed_out: bool) -> Value {
    json!({"schema_version":1,"timer_id":entry.id,"status":entry.status,"label":entry.label,
        "elapsed_ms":entry.elapsed_ms,"remaining_ms":entry.remaining_ms,"deadline_clock":"monotonic",
        "deadline_unix_ms":entry.deadline_unix_ms,
        "wait_timed_out":timed_out,"completion_permission":false})
}

fn validate_wait(wait_ms: u64) -> Result<(), String> {
    if wait_ms > MAX_WAIT_MS {
        Err("wait_ms must be 0..=25000; reuse timer_id to continue observing".into())
    } else {
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        Err("timer_id must be the exact server-issued timer ID".into())
    } else {
        Ok(())
    }
}

fn clean_label(label: Option<String>) -> Result<Option<String>, String> {
    label
        .map(|label| {
            let label = label.trim();
            if label.is_empty()
                || label.len() > 256
                || label.chars().count() > 80
                || label
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
            {
                return Err(
                    "timer label must be 1..=80 characters / 256 bytes without control characters"
                        .into(),
                );
            }
            let redacted = crate::audit::event(json!(label));
            let mut bounded = String::new();
            for ch in redacted.as_str().unwrap_or("[redacted]").chars().take(80) {
                if bounded.len() + ch.len_utf8() > 256 {
                    break;
                }
                bounded.push(ch);
            }
            Ok(bounded)
        })
        .transpose()
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Start {
        duration_ms: Option<u64>,
        deadline_unix_ms: Option<i64>,
        label: Option<String>,
        #[serde(default)]
        wait_ms: u64,
    },
    Status {
        timer_id: String,
        #[serde(default)]
        wait_ms: u64,
    },
    Cancel {
        timer_id: String,
    },
    List {},
}

struct ObservationSpec {
    timeout_ms: Option<u64>,
    cancellable: bool,
}

fn observation(name: &str, arguments: &Value) -> Option<ObservationSpec> {
    if name.starts_with("ssmcp.time.") {
        return None;
    }
    let (default, cancellable) = match name {
        "ssmcp.exec.wait" => (Some(5000), true),
        "ssmcp.exec.read" | "terminal.read" | "ssmcp.output.read" | "ssmcp.audit.events" => {
            (Some(0), true)
        }
        "ssmcp.ui.finish" => (None, false),
        "ssmcp.ui.questions" if arguments["op"] == "wait" => (None, false),
        _ if name.starts_with("ssmcp.ui.") => return None,
        _ if name.ends_with(".wait") => (None, false),
        _ => return None,
    };
    let timeout = arguments
        .get("wait_ms")
        .or_else(|| arguments.get("timeout_ms"));
    // Invalid or excessive input is validated by the actual operation; the
    // presentation hook neither invents a deadline nor changes its outcome.
    let timeout_ms = match timeout {
        Some(value) => value.as_u64().filter(|value| *value <= MAX_DURATION_MS),
        None => default,
    };
    if timeout_ms == Some(0) && (default.is_some() || arguments.get("wait_ms").is_some()) {
        return None;
    }
    Some(ObservationSpec {
        timeout_ms: timeout_ms.filter(|value| *value != 0),
        cancellable,
    })
}

pub(super) struct ObservationGuard {
    clock: Arc<Clock>,
    id: String,
    receiver: watch::Receiver<bool>,
}

impl ObservationGuard {
    pub(super) async fn cancelled(&mut self) {
        if !*self.receiver.borrow() {
            let _ = self.receiver.changed().await;
        }
    }
}

impl Drop for ObservationGuard {
    fn drop(&mut self) {
        self.clock
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .observations
            .remove(&self.id);
        changed();
    }
}

pub(crate) fn snapshot() -> Snapshot {
    snapshot_at(Instant::now())
}

pub(crate) fn snapshot_at(now: Instant) -> Snapshot {
    let clock = display_clock()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .upgrade();
    clock.map_or_else(Snapshot::default, |clock| {
        clock.snapshot(&history::current_owner(), now)
    })
}

pub(crate) fn utc_now() -> Result<String, String> {
    civil::format(civil::now_ms()?)
}

pub(crate) fn cancel_local(id: &str) -> Result<(), String> {
    validate_id(id)?;
    let clock = display_clock()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .upgrade()
        .ok_or("No live timer connection")?;
    // Caller owns the compositor lock and marks it dirty. No invalidation here.
    clock.cancel_local(id, &history::current_owner(), Instant::now())
}

pub(super) fn tools() -> Vec<Value> {
    vec![
        json!({"name":"ssmcp.time.now","description":"Read time without consuming input or acknowledging answers. Account connections require a parent offer and server Ready echo of time_observation_v1; same-process private local hosts enable it directly. Default op=now returns UTC RFC3339, Unix milliseconds, and a separate connection-local monotonic counter. op=interval takes start and optional end; each point is exactly {unix_ms: integer} or {rfc3339: string}. End defaults to a single current wall-clock sample. Returns signed end-minus-start interval_ms, absolute_ms and days/hours/minutes/seconds/milliseconds. RFC3339 requires Z or a numeric offset and exact millisecond precision; leap seconds and nonzero sub-millisecond digits are rejected. Each duration day is exactly 86400000 ms, not a calendar month or local daylight-saving day. UTC years 0000..9999 are supported. Wall-clock intervals do not set or advance monotonic timers.",
            "inputSchema":{"type":"object","additionalProperties":false,"properties":{
                "op":{"enum":["now","interval"],"default":"now"},"start":civil::point_schema(),"end":civil::point_schema()},
                "oneOf":[{"properties":{"op":{"const":"now"}},"not":{"anyOf":[{"required":["start"]},{"required":["end"]}]}},
                    {"required":["op","start"],"properties":{"op":{"const":"interval"}}}]},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}}),
        json!({"name":"ssmcp.time.wait","description":"Create, list, observe or cancel connection-local timers without consuming user messages, acknowledging answers or invoking a model. Account connections require time_observation_v1 at both endpoints; unnegotiated calls remain unavailable. start returns timer_id immediately unless wait_ms is supplied. Use duration_ms (0..86400000) OR deadline_unix_ms: absolute deadlines are converted once to a monotonic deadline, at most 24 hours ahead; a past deadline elapses immediately. Omit both for an elapsed timer with a 24-hour lifetime. list returns only this exact caller's active/recent timers. status(timer_id, wait_ms=0..25000) observes the existing timer; a wait timeout leaves it running. cancel stops only that timer, never an exec job. IDs are scoped to the original connection, account, participant and conversation. Disconnect/reload clears them. At most 32 running timers and 128 total retained records; finished records remain for up to five minutes. Local /wait shows current time, timers and running work. Timer expiry does not mean an exec job has completed.",
            "inputSchema":{"type":"object","additionalProperties":false,"properties":{
                "op":{"enum":["start","status","cancel","list"],"default":"start"},"duration_ms":{"type":"integer","minimum":0,"maximum":MAX_DURATION_MS},
                "deadline_unix_ms":{"type":"integer","minimum":civil::MIN_UNIX_MS,"maximum":civil::MAX_UNIX_MS},
                "label":{"type":"string","minLength":1,"maxLength":80},"timer_id":{"type":"string","minLength":1,"maxLength":128},
                "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":0}},
                "oneOf":[{"not":{"anyOf":[{"required":["timer_id"]},{"required":["duration_ms","deadline_unix_ms"]}]},"properties":{"op":{"const":"start"}}},
                    {"required":["op","timer_id"],"properties":{"op":{"const":"status"}},"not":{"anyOf":[{"required":["duration_ms"]},{"required":["deadline_unix_ms"]},{"required":["label"]}]}},
                    {"required":["op","timer_id"],"properties":{"op":{"const":"cancel"}},"not":{"anyOf":[{"required":["duration_ms"]},{"required":["deadline_unix_ms"]},{"required":["label"]},{"required":["wait_ms"]}]}},
                    {"required":["op"],"properties":{"op":{"const":"list"}},"not":{"anyOf":[{"required":["timer_id"]},{"required":["duration_ms"]},{"required":["deadline_unix_ms"]},{"required":["label"]},{"required":["wait_ms"]}]}}]},
            "annotations":{"readOnlyHint":false,"destructiveHint":false}}),
    ]
}

impl NativePlugin for Clock {
    const ID: &'static str = "ssmcp.time";
    fn providers(&self) -> &'static [Provider] {
        &[Provider::Time]
    }
    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![(Provider::Time, tools())]
    }
    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            if !self.time_tools_enabled() {
                return Err("time_observation_not_negotiated: remote time tools require an explicit time_observation_v1 server acknowledgement".into());
            }
            let value = match call.native_name.as_str() {
                "ssmcp.time.now" => self.now(call.arguments),
                "ssmcp.time.wait" => self.wait(call.arguments).await,
                _ => Err("unknown time primitive".into()),
            }?;
            Ok(crate::backend::result(value))
        })
    }
    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            self.close();
            Ok(json!({"status":"stopped"}))
        })
    }
}

#[cfg(test)]
mod tests;
