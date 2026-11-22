//! One native worker pool owns a bounded FIFO. A lease is internal authority;
//! display labels and request JSON never select or complete an execution slot.
use super::{ManagerView, SlotLog, SlotView, Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use std::time::Instant;
use tokio::sync::watch;

mod lifecycle;
mod projection;
mod telemetry;
pub(super) use telemetry::{ReportEvent, ReportUpdate, display_text};

const MAX_QUEUED: usize = 128;
const MAX_FINISHED: usize = 128;
const MAX_UNKNOWN_KEYS: usize = 128;
const MAX_TASK_BYTES: usize = 16 * 1024;
const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_LOGS: usize = 64;
const MAX_LOG_BYTES: usize = 16 * 1024;
const MAX_LOG_TEXT: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Actor {
    pub(crate) provider: String,
    pub(crate) subject: String,
    pub(crate) participant: Option<String>,
    pub(crate) connection: u64,
}

struct Manager {
    actor: Actor,
    view: ManagerView,
}

impl Manager {
    fn new(actor: &Actor, client_id: &str, now_ms: u64) -> Result<Self, String> {
        let participant = actor
            .participant
            .as_ref()
            .filter(|id| !id.is_empty())
            .ok_or("worker management requires a verified participant thread")?;
        if client_id.len() != 64 || !client_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("worker management requires the verified native client identity".into());
        }
        Ok(Self {
            actor: actor.clone(),
            view: ManagerView {
                client_id: client_id.into(),
                thread_id: participant.clone(),
                participant_id: participant.clone(),
                connection: actor.connection,
                attached_at_ms: now_ms,
            },
        })
    }

    fn matches(&self, actor: &Actor, client_id: &str) -> bool {
        &self.actor == actor && self.view.client_id == client_id
    }
}

/// Manager listing deliberately omits prompts, expanded inputs and results.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskSummary {
    task_id: String,
    status: TaskStatus,
    slot: Option<usize>,
    attempt: u64,
    created_ms: u64,
    updated_ms: u64,
    replay_safe: bool,
}

impl From<&TaskView> for TaskSummary {
    fn from(task: &TaskView) -> Self {
        Self {
            task_id: task.task_id.clone(),
            status: task.status,
            slot: task.slot,
            attempt: task.attempt,
            created_ms: task.created_ms,
            updated_ms: task.updated_ms,
            replay_safe: task.replay_safe,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct TaskPage {
    tasks: Vec<TaskSummary>,
    next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct IdleSlot {
    slot: usize,
    epoch: u64,
}

/// Only capacity belongs in this projection. Logs, task text, timings and
/// progress remain on the ordinary pool revision and never change this cursor.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
struct AvailabilityProjection {
    idle_slots: Vec<IdleSlot>,
    total: usize,
    idle: usize,
    working: usize,
    offline: usize,
    retiring: usize,
    registered_workers: usize,
    desired_total: usize,
    queued: usize,
    accepting: bool,
    stopping: bool,
    closed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Availability {
    pub(crate) generation: String,
    pub(crate) revision: u64,
    scope: &'static str,
    #[serde(flatten)]
    projection: AvailabilityProjection,
}

impl Availability {
    pub(crate) fn accepting(&self) -> bool {
        self.projection.accepting
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskStatus {
    Queued,
    Working,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

impl TaskStatus {
    fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::Unknown
        )
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct TaskView {
    pub(crate) task_id: String,
    pub(crate) status: TaskStatus,
    pub(crate) slot: Option<usize>,
    pub(crate) attempt: u64,
    pub(crate) created_ms: u64,
    pub(crate) updated_ms: u64,
    pub(crate) output: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) replay_safe: bool,
}

#[derive(Clone)]
pub(crate) struct Lease<T> {
    pub(crate) generation: String,
    pub(crate) slot: usize,
    pub(crate) epoch: u64,
    pub(crate) task_id: String,
    pub(crate) attempt: u64,
    pub(crate) text: String,
    pub(crate) context: T,
}

pub(crate) enum Outcome {
    Completed(String),
    #[cfg(any(target_os = "linux", test))]
    CompletedWithWarning(String, String),
    Failed(String),
    Cancelled,
    Unknown(String),
}

struct Task<T> {
    actor: Actor,
    // Public identity captured from the verified station registration at claim
    // time. A later occupant or manager must never be credited with this work.
    claimed_by: Option<String>,
    claim_epoch: Option<u64>,
    key: String,
    text: String,
    context: T,
    view: TaskView,
}

enum SlotState {
    Offline(String),
    Idle,
    Working(String),
    Retiring,
}

/// A shutdown acknowledgement is authority over exactly one station epoch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retirement {
    pub(super) id: String,
    pub(super) generation: String,
    pub(super) slot: usize,
    pub(super) epoch: u64,
}

struct Slot {
    enabled: bool,
    epoch: u64,
    state: SlotState,
    registered: Option<Actor>,
    retirement: Option<Retirement>,
    logs: VecDeque<SlotLog>,
    next_log_sequence: u64,
    task_summary: Option<String>,
    task_text: Option<String>,
    last_command: Option<String>,
    progress: Option<String>,
    started: Option<Instant>,
    task_started_at_ms: Option<u64>,
    total_work_ms: u64,
    recent_task_durations_ms: VecDeque<u64>,
}

impl Slot {
    fn new() -> Self {
        Self {
            enabled: true,
            epoch: 0,
            state: SlotState::Offline("executor not connected".into()),
            registered: None,
            retirement: None,
            logs: VecDeque::new(),
            next_log_sequence: 1,
            task_summary: None,
            task_text: None,
            last_command: None,
            progress: None,
            started: None,
            task_started_at_ms: None,
            total_work_ms: 0,
            recent_task_durations_ms: VecDeque::new(),
        }
    }
}

struct State<T> {
    manager: Option<Manager>,
    tasks: BTreeMap<String, Task<T>>,
    fifo: VecDeque<String>,
    finished: VecDeque<String>,
    slots: Vec<Slot>,
    desired_total: usize,
    remote_ready: bool,
    sequence: u64,
    revision: u64,
    availability_revision: u64,
    availability: AvailabilityProjection,
    closed: bool,
    stopping: bool,
    unknown_keys: BTreeSet<(Actor, String)>,
    unknown_guard_full: bool,
}

pub(crate) struct Queue<T> {
    generation: String,
    state: Mutex<State<T>>,
    changed: watch::Sender<u64>,
}

impl<T: Clone> Queue<T> {
    pub(crate) fn new(generation: String, count: usize) -> Result<Self, String> {
        if generation.is_empty() || !(1..=super::config::MAX_WORKERS).contains(&count) {
            return Err("invalid worker pool generation or size".into());
        }
        let (changed, _) = watch::channel(0);
        Ok(Self {
            generation,
            state: Mutex::new(State {
                manager: None,
                tasks: BTreeMap::new(),
                fifo: VecDeque::new(),
                finished: VecDeque::new(),
                slots: (0..count).map(|_| Slot::new()).collect(),
                desired_total: count,
                remote_ready: false,
                sequence: 0,
                revision: 0,
                availability_revision: 0,
                availability: AvailabilityProjection {
                    total: count,
                    offline: count,
                    desired_total: count,
                    ..AvailabilityProjection::default()
                },
                closed: false,
                stopping: false,
                unknown_keys: BTreeSet::new(),
                unknown_guard_full: false,
            }),
            changed,
        })
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    fn changed(&self, state: &mut State<T>) {
        // Keep one bounded projection, not an event history. A transient change
        // followed by restoration still advances the cursor, while log-only
        // writes continue to wake existing pool observers without advancing it.
        let availability = availability_projection(state);
        if state.availability != availability {
            state.availability = availability;
            state.availability_revision = state.availability_revision.saturating_add(1);
        }
        state.revision = state.revision.wrapping_add(1);
        self.changed.send_replace(state.revision);
    }

    /// This check never binds an observer or an executor as manager. Submission
    /// repeats it under the admission lock after asynchronous input preparation.
    pub(crate) fn check_submitter(&self, actor: &Actor, client_id: &str) -> Result<(), String> {
        let _ = Manager::new(actor, client_id, 0)?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .manager
            .as_ref()
            .is_some_and(|manager| !manager.matches(actor, client_id))
        {
            return Err("worker pool belongs to a different authenticated manager".into());
        }
        Ok(())
    }

    pub(crate) fn is_manager(&self, actor: &Actor, client_id: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .manager
            .as_ref()
            .is_some_and(|manager| manager.matches(actor, client_id))
    }

    /// Observation never acquires management or registers an external worker.
    pub(crate) fn availability(
        &self,
        actor: &Actor,
        client_id: &str,
    ) -> Result<Availability, String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        if state.availability_revision == u64::MAX {
            return Err(
                "worker availability revision exhausted; reconnect to obtain a new generation"
                    .into(),
            );
        }
        Ok(Availability {
            generation: self.generation.clone(),
            revision: state.availability_revision,
            scope: "coordination_slots",
            projection: state.availability.clone(),
        })
    }

    pub(crate) fn attach(
        &self,
        actor: &Actor,
        client_id: &str,
        now_ms: u64,
    ) -> Result<ManagerView, String> {
        let manager = Manager::new(actor, client_id, now_ms)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(current) = &state.manager {
            return if current.matches(actor, client_id) {
                Ok(current.view.clone())
            } else {
                Err("worker pool belongs to a different authenticated manager".into())
            };
        }
        if state.closed || state.stopping {
            return Err("worker pool is no longer accepting a manager".into());
        }
        let view = manager.view.clone();
        state.manager = Some(manager);
        self.changed(&mut state);
        Ok(view)
    }

    pub(crate) fn tasks(
        &self,
        actor: &Actor,
        client_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<TaskPage, String> {
        if !(1..=128).contains(&limit)
            || cursor
                .is_some_and(|cursor| cursor.len() > 256 || cursor.chars().any(char::is_control))
        {
            return Err("worker tasks requires limit 1..128 and a valid task cursor".into());
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        let mut tasks: Vec<_> = state
            .tasks
            .iter()
            .filter(|(id, task)| {
                &task.actor == actor && cursor.is_none_or(|cursor| id.as_str() > cursor)
            })
            .take(limit + 1)
            .map(|(_, task)| TaskSummary::from(&task.view))
            .collect();
        let more = tasks.len() > limit;
        tasks.truncate(limit);
        let next_cursor = more.then(|| tasks.last().expect("positive page limit").task_id.clone());
        Ok(TaskPage { tasks, next_cursor })
    }

    /// Stop accepting work without consuming a remote lease or claiming that
    /// an external process stopped. Existing claimants can still acknowledge.
    pub(crate) fn stop(&self, actor: &Actor, client_id: &str, now_ms: u64) -> Result<bool, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        if state.stopping || state.closed {
            return Ok(false);
        }
        state.stopping = true;
        while let Some(id) = state.fifo.pop_front() {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.view.status = TaskStatus::Cancelled;
                task.view.updated_ms = now_ms;
                state.finished.push_back(id);
            }
        }
        for task in state
            .tasks
            .values_mut()
            .filter(|task| task.view.status == TaskStatus::Working)
        {
            task.view.status = TaskStatus::Cancelling;
            task.view.updated_ms = now_ms;
        }
        for slot in &mut state.slots {
            if matches!(slot.state, SlotState::Idle) {
                slot.state = SlotState::Offline("stopped by manager; no new tasks accepted".into());
            }
            log(
                slot,
                now_ms,
                "warning",
                "Manager requested stop; active tasks await completion acknowledgement",
                None,
            );
        }
        trim(&mut state);
        self.changed(&mut state);
        Ok(true)
    }

    pub(crate) fn accepting(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        !state.closed && !state.stopping
    }

    /// Only the owner calls this after its execution backend is ready. For a
    /// local executor that requires the limited-child handshake. Remote mode
    /// registers each authenticated external worker during its first wait.
    pub(crate) fn online(&self, slot: usize) -> Result<u64, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.stopping {
            return Err("worker pool is closed or stopped by its manager".into());
        }
        let worker = state
            .slots
            .get_mut(slot)
            .filter(|slot| slot.enabled)
            .ok_or("unknown worker slot")?;
        if !matches!(worker.state, SlotState::Offline(_)) {
            return Err("worker slot must finish owned cleanup before reconnecting".into());
        }
        worker.epoch = worker
            .epoch
            .checked_add(1)
            .ok_or("worker epoch exhausted")?;
        worker.state = SlotState::Idle;
        let epoch = worker.epoch;
        log(
            worker,
            super::now_ms(),
            "info",
            "Slot ready; waiting for a task",
            None,
        );
        self.changed(&mut state);
        Ok(epoch)
    }

    pub(crate) fn offline(&self, slot: usize, epoch: u64, reason: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let worker = state
            .slots
            .get_mut(slot)
            .filter(|slot| slot.enabled)
            .ok_or("unknown worker slot")?;
        if worker.epoch != epoch {
            return Err("stale worker epoch".into());
        }
        if matches!(worker.state, SlotState::Working(_) | SlotState::Retiring) {
            return Err("active or retiring worker requires its exact completion or release acknowledgement".into());
        }
        worker.state = SlotState::Offline(bounded(reason, 1024));
        log(
            worker,
            super::now_ms(),
            "warning",
            "Slot went offline",
            None,
        );
        self.changed(&mut state);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn submit(
        &self,
        actor: Actor,
        key: &str,
        text: &str,
        context: T,
        now_ms: u64,
    ) -> Result<TaskView, String> {
        self.submit_inner(actor, key, text, context, now_ms, None)
    }

    pub(crate) fn submit_managed(
        &self,
        actor: Actor,
        client_id: &str,
        key: &str,
        text: &str,
        context: T,
        now_ms: u64,
    ) -> Result<TaskView, String> {
        self.submit_inner(actor, key, text, context, now_ms, Some(client_id))
    }

    fn submit_inner(
        &self,
        actor: Actor,
        key: &str,
        text: &str,
        context: T,
        now_ms: u64,
        client_id: Option<&str>,
    ) -> Result<TaskView, String> {
        let manager = client_id
            .map(|client_id| Manager::new(&actor, client_id, now_ms))
            .transpose()?;
        if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            return Err(
                "worker request_id must contain 1..128 bytes without control characters".into(),
            );
        }
        if text.trim().is_empty() || text.len() > MAX_TASK_BYTES || text.contains('\0') {
            return Err("worker task text must contain 1..16384 bytes without NUL".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(manager) = &manager
            && state
                .manager
                .as_ref()
                .is_some_and(|current| !current.matches(&actor, &manager.view.client_id))
        {
            return Err("worker pool belongs to a different authenticated manager".into());
        }
        if let Some(task) = state
            .tasks
            .values()
            .find(|task| task.actor == actor && task.key == key)
        {
            return if task.text == text {
                Ok(task.view.clone())
            } else {
                Err("worker request_id was already used with different text".into())
            };
        }
        if state
            .unknown_keys
            .contains(&(actor.clone(), key.to_owned()))
        {
            return Err("this worker request_id has an expired unknown result; effects must be reconciled before submitting a new request_id".into());
        }
        if state.unknown_guard_full {
            return Err("worker unknown-result retry guard is full; no new task was admitted; reconcile unknown effects before starting another connection".into());
        }
        if state.closed || state.stopping {
            return Err("worker pool is closed or stopped; no task was admitted".into());
        }
        if !has_execution_capacity(&state) {
            return Err("worker pool has no ready execution backend; no task was admitted".into());
        }
        if state.fifo.len() >= MAX_QUEUED {
            return Err("worker FIFO is full; no task was admitted".into());
        }
        let sequence = state
            .sequence
            .checked_add(1)
            .ok_or("worker task sequence exhausted")?;
        // Every rejection above leaves ownership untouched. Admission and the
        // first manager binding are one transaction under this state lock.
        if state.manager.is_none() {
            state.manager = manager;
        }
        state.sequence = sequence;
        let id = format!("worker_task_{}_{}", self.generation, state.sequence);
        let view = TaskView {
            task_id: id.clone(),
            status: TaskStatus::Queued,
            slot: None,
            attempt: 0,
            created_ms: now_ms,
            updated_ms: now_ms,
            output: None,
            error: None,
            replay_safe: true,
        };
        state.tasks.insert(
            id.clone(),
            Task {
                actor,
                claimed_by: None,
                claim_epoch: None,
                key: key.into(),
                text: text.into(),
                context,
                view: view.clone(),
            },
        );
        state.fifo.push_back(id);
        self.changed(&mut state);
        Ok(view)
    }

    /// Taking the first task and occupying the slot share one critical section.
    /// There is no broadcast or speculative fanout to other idle slots.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn claim(
        &self,
        slot: usize,
        epoch: u64,
        now_ms: u64,
    ) -> Result<Option<Lease<T>>, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.stopping {
            return Ok(None);
        }
        let worker = state
            .slots
            .get(slot)
            .filter(|slot| slot.enabled)
            .ok_or("unknown worker slot")?;
        if worker.epoch != epoch {
            return Err("stale worker epoch".into());
        }
        if !matches!(worker.state, SlotState::Idle) {
            return Ok(None);
        }
        let Some(id) = state.fifo.pop_front() else {
            return Ok(None);
        };
        self.begin_claim(&mut state, slot, epoch, id, now_ms)
    }

    /// Remote agents may take tasks delegated by the same authenticated account
    /// in this connection. A display name or a supplied JSON actor is never used.
    pub(crate) fn claim_remote(
        &self,
        actor: &Actor,
        now_ms: u64,
    ) -> Result<Option<Lease<T>>, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.stopping || !state.remote_ready {
            return Ok(None);
        }
        // An authenticated caller owns one stable station, even while idle.
        // Foreign-account waits must not consume capacity reserved for the
        // connection's manager or its already delegated tasks.
        let account = state
            .manager
            .as_ref()
            .map(|manager| &manager.actor)
            .or_else(|| state.tasks.values().next().map(|task| &task.actor));
        if account.is_some_and(|owner| !lifecycle::same_account(owner, actor)) {
            return Ok(None);
        }
        let registered = state
            .slots
            .iter()
            .position(|slot| slot.enabled && slot.registered.as_ref() == Some(actor));
        let slot = if let Some(slot) = registered {
            slot
        } else {
            let Some(slot) = state.slots.iter().position(|slot| {
                slot.enabled
                    && slot.registered.is_none()
                    && slot.retirement.is_none()
                    && matches!(slot.state, SlotState::Offline(_))
            }) else {
                return Ok(None);
            };
            let worker = &mut state.slots[slot];
            worker.epoch = worker
                .epoch
                .checked_add(1)
                .ok_or("worker epoch exhausted")?;
            worker.registered = Some(actor.clone());
            worker.state = SlotState::Idle;
            log(
                worker,
                now_ms,
                "info",
                "Authenticated external worker registered; waiting for a task",
                None,
            );
            self.changed(&mut state);
            slot
        };
        let worker = &state.slots[slot];
        if !matches!(worker.state, SlotState::Idle) || worker.retirement.is_some() {
            return Ok(None);
        }
        let epoch = worker.epoch;
        let Some(position) = state.fifo.iter().position(|id| {
            state.tasks.get(id).is_some_and(|task| {
                task.actor.provider == actor.provider
                    && task.actor.subject == actor.subject
                    && task.actor.connection == actor.connection
            })
        }) else {
            return Ok(None);
        };
        let id = state.fifo.remove(position).expect("checked FIFO position");
        self.begin_claim(&mut state, slot, epoch, id, now_ms)
    }

    fn begin_claim(
        &self,
        state: &mut State<T>,
        slot: usize,
        epoch: u64,
        id: String,
        now_ms: u64,
    ) -> Result<Option<Lease<T>>, String> {
        let claimed_by = state.slots[slot]
            .registered
            .as_ref()
            .and_then(|actor| actor.participant.clone());
        let task = state
            .tasks
            .get_mut(&id)
            .expect("FIFO owns an existing queued task");
        if task.view.status != TaskStatus::Queued {
            return Err("worker FIFO state is inconsistent".into());
        }
        task.view.status = TaskStatus::Working;
        task.claimed_by = claimed_by;
        task.claim_epoch = Some(epoch);
        task.view.slot = Some(slot);
        task.view.attempt += 1;
        task.view.updated_ms = now_ms;
        task.view.replay_safe = false;
        let lease = Lease {
            generation: self.generation.clone(),
            slot,
            epoch,
            task_id: id.clone(),
            attempt: task.view.attempt,
            text: task.text.clone(),
            context: task.context.clone(),
        };
        state.slots[slot].state = SlotState::Working(id.clone());
        let worker = &mut state.slots[slot];
        worker.task_summary = Some(telemetry::display_text(&lease.text, 256));
        worker.task_text = Some(lease.text.clone());
        worker.last_command = None;
        worker.progress = None;
        worker.started = Some(Instant::now());
        worker.task_started_at_ms = Some(now_ms);
        log(
            &mut state.slots[slot],
            now_ms,
            "info",
            "Task assigned",
            Some(&id),
        );
        self.changed(state);
        Ok(Some(lease))
    }

    pub(crate) fn result(&self, actor: &Actor, id: &str) -> Result<TaskView, String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let task = state
            .tasks
            .get(id)
            .filter(|task| &task.actor == actor)
            .ok_or("worker task is not retained for this authenticated caller")?;
        Ok(task.view.clone())
    }

    pub(crate) fn cancel(&self, actor: &Actor, id: &str, now_ms: u64) -> Result<TaskView, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let task = state
            .tasks
            .get_mut(id)
            .filter(|task| &task.actor == actor)
            .ok_or("worker task is not retained for this authenticated caller")?;
        let queued = task.view.status == TaskStatus::Queued;
        let cancelling_slot = (task.view.status == TaskStatus::Working)
            .then_some(task.view.slot)
            .flatten();
        if queued {
            task.view.status = TaskStatus::Cancelled;
        } else if task.view.status == TaskStatus::Working {
            task.view.status = TaskStatus::Cancelling;
        }
        task.view.updated_ms = now_ms;
        let view = task.view.clone();
        if let Some(slot) = cancelling_slot {
            log(
                &mut state.slots[slot],
                now_ms,
                "warning",
                "Cancellation requested; awaiting completion acknowledgement",
                Some(id),
            );
        }
        if queued {
            state.fifo.retain(|queued| queued != id);
            state.finished.push_back(id.into());
            trim(&mut state);
        }
        self.changed(&mut state);
        Ok(view)
    }

    pub(crate) fn cancellation_requested(&self, lease: &Lease<T>) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed
            || !valid(&state, &self.generation, lease)
            || state
                .tasks
                .get(&lease.task_id)
                .is_some_and(|task| task.view.status == TaskStatus::Cancelling)
    }

    /// Call only after the owned process tree's actual completion/cleanup.
    /// A failed cleanup cannot make a slot idle or make the task replay-safe.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn finish(
        &self,
        lease: &Lease<T>,
        outcome: Outcome,
        cleanup_complete: bool,
        reusable: bool,
        now_ms: u64,
    ) -> Result<TaskView, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !valid(&state, &self.generation, lease) {
            return Err("stale or already consumed worker lease".into());
        }
        if !cleanup_complete {
            return Err("owned worker cleanup is incomplete; the slot remains busy".into());
        }
        self.finish_locked(
            &mut state,
            lease,
            outcome,
            reusable,
            now_ms,
            "Owned executor",
        )
    }

    /// Complete only the remote coordination lease. There is no owned model or
    /// OS process to reap, and the result is explicitly a remote agent report.
    pub(crate) fn finish_remote(
        &self,
        lease: &Lease<T>,
        outcome: Outcome,
        reusable: bool,
        now_ms: u64,
    ) -> Result<TaskView, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !valid(&state, &self.generation, lease) {
            return Err("stale or already consumed worker lease".into());
        }
        self.finish_locked(
            &mut state,
            lease,
            outcome,
            reusable,
            now_ms,
            "Remote agent report",
        )
    }

    fn finish_locked(
        &self,
        state: &mut State<T>,
        lease: &Lease<T>,
        outcome: Outcome,
        reusable: bool,
        now_ms: u64,
        source: &str,
    ) -> Result<TaskView, String> {
        let closed = state.closed;
        let stopping = state.stopping;
        let task = state
            .tasks
            .get_mut(&lease.task_id)
            .expect("validated lease task");
        let (status, output, error) = match outcome {
            Outcome::Completed(output) => (
                TaskStatus::Completed,
                Some(bounded(&output, MAX_RESULT_BYTES)),
                None,
            ),
            #[cfg(any(target_os = "linux", test))]
            Outcome::CompletedWithWarning(output, warning) => (
                TaskStatus::Completed,
                Some(bounded(&output, MAX_RESULT_BYTES)),
                Some(bounded(&warning, 4096)),
            ),
            Outcome::Failed(error) => (TaskStatus::Failed, None, Some(bounded(&error, 4096))),
            Outcome::Cancelled => (TaskStatus::Cancelled, None, None),
            Outcome::Unknown(error) => (TaskStatus::Unknown, None, Some(bounded(&error, 4096))),
        };
        // A confirmed result remains a confirmed result even if cancel raced
        // with delivery. Never requeue effects whose completion is uncertain.
        task.view.status = status;
        task.view.output = output;
        task.view.error = error;
        task.view.updated_ms = now_ms;
        task.view.replay_safe = false;
        let view = task.view.clone();
        let worker = &mut state.slots[lease.slot];
        telemetry::settle_time(worker, status != TaskStatus::Unknown);
        worker.state = if worker.retirement.is_some() && !closed {
            SlotState::Retiring
        } else if reusable && !closed && !stopping {
            SlotState::Idle
        } else if stopping && !closed {
            SlotState::Offline("stopped by manager; task completion acknowledged".into())
        } else if source == "Remote agent report" {
            SlotState::Offline("remote coordination closed; external effects may remain".into())
        } else {
            SlotState::Offline("owned executor stopped; reconnect required".into())
        };
        if closed {
            worker.registered = None;
            worker.retirement = None;
        }
        let outcome = match status {
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Unknown => "unknown",
            _ => unreachable!("finish creates a terminal result"),
        };
        // Status is readable by authorized participants. Keep task bodies and
        // results behind the existing exact-actor result API, not in shared logs.
        let output_bytes = view.output.as_ref().map_or(0, String::len);
        log(
            &mut state.slots[lease.slot],
            now_ms,
            if status == TaskStatus::Completed {
                "info"
            } else {
                "warning"
            },
            &format!("{source}: {outcome}; output {output_bytes} bytes"),
            Some(&lease.task_id),
        );
        state.finished.push_back(lease.task_id.clone());
        trim(state);
        self.changed(state);
        Ok(view)
    }

    pub(crate) fn close(&self, now_ms: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        while let Some(id) = state.fifo.pop_front() {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.view.status = TaskStatus::Cancelled;
                task.view.updated_ms = now_ms;
                state.finished.push_back(id);
            }
        }
        for task in state
            .tasks
            .values_mut()
            .filter(|task| task.view.status == TaskStatus::Working)
        {
            task.view.status = TaskStatus::Cancelling;
            task.view.updated_ms = now_ms;
        }
        for slot in &mut state.slots {
            if !matches!(slot.state, SlotState::Working(_)) {
                slot.state = SlotState::Offline("connection closed".into());
                slot.registered = None;
                slot.retirement = None;
            }
            log(slot, now_ms, "warning", "Worker connection closed", None);
        }
        trim(&mut state);
        self.changed(&mut state);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.snapshot_locked(&state)
    }

    pub(crate) fn snapshot_with_activity(&self) -> (Snapshot, super::Activity) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        (
            self.snapshot_locked(&state),
            projection::activity(&state, &self.generation),
        )
    }

    fn snapshot_locked(&self, state: &State<T>) -> Snapshot {
        let slots: Vec<_> = state
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.enabled)
            .map(|(index, slot)| {
                let (name, task_id, reason) = match &slot.state {
                    SlotState::Offline(reason) => ("offline", None, Some(reason.clone())),
                    SlotState::Idle => ("idle", None, None),
                    SlotState::Working(id) => ("working", Some(id.clone()), None),
                    SlotState::Retiring => (
                        "retiring",
                        None,
                        Some("waiting for external shutdown acknowledgement".into()),
                    ),
                };
                let elapsed = telemetry::elapsed_ms(slot);
                SlotView {
                    slot: index,
                    epoch: slot.epoch,
                    state: name,
                    task_id,
                    reason,
                    logs: slot.logs.iter().cloned().collect(),
                    next_log_sequence: slot.next_log_sequence,
                    registered: slot.registered.is_some(),
                    retire_requested: slot.retirement.is_some(),
                    task_summary: slot.task_summary.clone(),
                    task_text: slot.task_text.clone(),
                    last_command: slot.last_command.clone(),
                    progress: slot.progress.clone(),
                    total_work_ms: slot.total_work_ms.saturating_add(elapsed.unwrap_or(0)),
                    current_task_elapsed_ms: elapsed,
                    task_started_at_ms: slot.task_started_at_ms,
                    recent_task_durations_ms: slot
                        .recent_task_durations_ms
                        .iter()
                        .copied()
                        .collect(),
                }
            })
            .collect();
        Snapshot {
            process_id: std::process::id(),
            generation: self.generation.clone(),
            manager: state.manager.as_ref().map(|manager| manager.view.clone()),
            total: slots.len(),
            idle: slots.iter().filter(|slot| slot.state == "idle").count(),
            working: slots.iter().filter(|slot| slot.state == "working").count(),
            offline: slots.iter().filter(|slot| slot.state == "offline").count(),
            retiring: slots.iter().filter(|slot| slot.state == "retiring").count(),
            registered_workers: slots.iter().filter(|slot| slot.registered).count(),
            desired_total: state.desired_total,
            queued: state.fifo.len(),
            closed: state.closed,
            stopping: state.stopping,
            slots,
        }
    }

    /// Called only after the entire closed pool's owned scopes have actually
    /// been observed empty. It resolves cleanup-unknown slots after a failed
    /// individual shutdown without inventing a successful task result.
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn cleaned_closed_pool(&self, now_ms: u64) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.closed {
            return Err("worker pool must be closed before final cleanup".into());
        }
        let mut finished = Vec::new();
        for task in state.tasks.values_mut() {
            if matches!(
                task.view.status,
                TaskStatus::Working | TaskStatus::Cancelling
            ) {
                task.view.status = TaskStatus::Unknown;
                task.view.error = Some("owned execution was stopped without a confirmed result; inspect effects before retrying".into());
                task.view.replay_safe = false;
                task.view.updated_ms = now_ms;
                finished.push(task.view.task_id.clone());
            }
        }
        state.finished.extend(finished);
        for slot in &mut state.slots {
            telemetry::settle_time(slot, false);
            slot.state = SlotState::Offline("connection closed; owned cleanup complete".into());
            slot.registered = None;
            slot.retirement = None;
            log(
                slot,
                now_ms,
                "info",
                "Owned execution cleanup confirmed",
                None,
            );
        }
        trim(&mut state);
        self.changed(&mut state);
        Ok(())
    }
}

fn has_execution_capacity<T>(state: &State<T>) -> bool {
    state.slots.iter().any(|slot| {
        slot.enabled
            && slot.retirement.is_none()
            && (state.remote_ready || !matches!(slot.state, SlotState::Offline(_)))
    })
}

fn availability_projection<T>(state: &State<T>) -> AvailabilityProjection {
    let accepting = !state.closed
        && !state.stopping
        && !state.unknown_guard_full
        && state.sequence < u64::MAX
        && state.fifo.len() < MAX_QUEUED
        && has_execution_capacity(state);
    let mut projection = AvailabilityProjection {
        desired_total: state.desired_total,
        queued: state.fifo.len(),
        accepting,
        stopping: state.stopping,
        closed: state.closed,
        ..AvailabilityProjection::default()
    };
    for (index, slot) in state
        .slots
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.enabled)
    {
        projection.total += 1;
        if slot.registered.is_some() {
            projection.registered_workers += 1;
        }
        match &slot.state {
            SlotState::Offline(_) => projection.offline += 1,
            SlotState::Working(_) => projection.working += 1,
            SlotState::Retiring => projection.retiring += 1,
            SlotState::Idle => {
                projection.idle += 1;
                if accepting
                    && slot.retirement.is_none()
                    && (!state.remote_ready || slot.registered.is_some())
                {
                    projection.idle_slots.push(IdleSlot {
                        slot: index,
                        epoch: slot.epoch,
                    });
                }
            }
        }
    }
    projection
}

fn require_manager<T>(state: &State<T>, actor: &Actor, client_id: &str) -> Result<(), String> {
    if state
        .manager
        .as_ref()
        .is_some_and(|manager| manager.matches(actor, client_id))
    {
        Ok(())
    } else {
        Err("this worker management action requires the pool's authenticated manager".into())
    }
}

fn valid<T>(state: &State<T>, generation: &str, lease: &Lease<T>) -> bool {
    lease.generation == generation
        && state.slots.get(lease.slot).is_some_and(|slot| {
            slot.epoch == lease.epoch
                && slot.enabled
                && matches!(&slot.state, SlotState::Working(id) if id == &lease.task_id)
        })
        && state.tasks.get(&lease.task_id).is_some_and(|task| {
            task.view.attempt == lease.attempt
                && task.view.slot == Some(lease.slot)
                && matches!(
                    task.view.status,
                    TaskStatus::Working | TaskStatus::Cancelling
                )
        })
}

fn trim<T>(state: &mut State<T>) {
    while state.finished.len() > MAX_FINISHED {
        if let Some(id) = state.finished.pop_front()
            && state
                .tasks
                .get(&id)
                .is_some_and(|task| task.view.status.terminal())
            && let Some(task) = state.tasks.remove(&id)
            && task.view.status == TaskStatus::Unknown
        {
            if state.unknown_keys.len() < MAX_UNKNOWN_KEYS {
                state.unknown_keys.insert((task.actor, task.key));
            } else {
                // A bounded guard may refuse new work, but must never
                // forget uncertain execution and silently replay its key.
                state.unknown_guard_full = true;
            }
        }
    }
}

fn log(slot: &mut Slot, at_ms: u64, level: &str, text: &str, task_id: Option<&str>) {
    let Some(next) = slot.next_log_sequence.checked_add(1) else {
        return;
    };
    slot.logs.push_back(SlotLog {
        sequence: slot.next_log_sequence,
        at_ms,
        level: level.into(),
        text: bounded(text, MAX_LOG_TEXT),
        task_id: task_id.map(str::to_owned),
    });
    slot.next_log_sequence = next;
    let size = |entry: &SlotLog| {
        entry.text.len() + entry.level.len() + entry.task_id.as_ref().map_or(0, String::len) + 64
    };
    while slot.logs.len() > MAX_LOGS || slot.logs.iter().map(size).sum::<usize>() > MAX_LOG_BYTES {
        slot.logs.pop_front();
    }
}

fn bounded(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.into();
    }
    let mut end = maximum.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests;
