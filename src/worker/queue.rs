//! One native worker pool owns a bounded FIFO. A lease is internal authority;
//! display labels and request JSON never select or complete an execution slot.
use super::{SlotLog, SlotView, Snapshot};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Mutex;
use tokio::sync::watch;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    key: String,
    text: String,
    context: T,
    view: TaskView,
}

enum SlotState {
    Offline(String),
    Idle,
    Working(String),
}

struct Slot {
    epoch: u64,
    state: SlotState,
    logs: VecDeque<SlotLog>,
    next_log_sequence: u64,
}

struct State<T> {
    tasks: BTreeMap<String, Task<T>>,
    fifo: VecDeque<String>,
    finished: VecDeque<String>,
    slots: Vec<Slot>,
    sequence: u64,
    revision: u64,
    closed: bool,
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
                tasks: BTreeMap::new(),
                fifo: VecDeque::new(),
                finished: VecDeque::new(),
                slots: (0..count)
                    .map(|_| Slot {
                        epoch: 0,
                        state: SlotState::Offline("executor not connected".into()),
                        logs: VecDeque::new(),
                        next_log_sequence: 1,
                    })
                    .collect(),
                sequence: 0,
                revision: 0,
                closed: false,
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
        state.revision = state.revision.wrapping_add(1);
        self.changed.send_replace(state.revision);
    }

    /// Only the owner calls this after its execution backend is ready. For a
    /// local executor that requires the limited-child handshake; for remote
    /// mode it means the in-process claim service is ready, not a model process.
    pub(crate) fn online(&self, slot: usize) -> Result<u64, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err("worker pool is closed".into());
        }
        let worker = state.slots.get_mut(slot).ok_or("unknown worker slot")?;
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
        let worker = state.slots.get_mut(slot).ok_or("unknown worker slot")?;
        if worker.epoch != epoch {
            return Err("stale worker epoch".into());
        }
        if matches!(worker.state, SlotState::Working(_)) {
            return Err("working slot stays busy until its owned execution is cleaned up".into());
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

    pub(crate) fn submit(
        &self,
        actor: Actor,
        key: &str,
        text: &str,
        context: T,
        now_ms: u64,
    ) -> Result<TaskView, String> {
        if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            return Err(
                "worker request_id must contain 1..128 bytes without control characters".into(),
            );
        }
        if text.trim().is_empty() || text.len() > MAX_TASK_BYTES || text.contains('\0') {
            return Err("worker task text must contain 1..16384 bytes without NUL".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
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
        if state.closed {
            return Err("worker pool is closed; no task was admitted".into());
        }
        if state
            .slots
            .iter()
            .all(|slot| matches!(slot.state, SlotState::Offline(_)))
        {
            return Err("worker pool has no ready execution backend; no task was admitted".into());
        }
        if state.fifo.len() >= MAX_QUEUED {
            return Err("worker FIFO is full; no task was admitted".into());
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or("worker task sequence exhausted")?;
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
        if state.closed {
            return Ok(None);
        }
        let worker = state.slots.get(slot).ok_or("unknown worker slot")?;
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
        if state.closed {
            return Ok(None);
        }
        let Some((slot, epoch)) = state.slots.iter().enumerate().find_map(|(index, slot)| {
            matches!(slot.state, SlotState::Idle).then_some((index, slot.epoch))
        }) else {
            return Ok(None);
        };
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
        let task = state
            .tasks
            .get_mut(&id)
            .expect("FIFO owns an existing queued task");
        if task.view.status != TaskStatus::Queued {
            return Err("worker FIFO state is inconsistent".into());
        }
        task.view.status = TaskStatus::Working;
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
        state.slots[lease.slot].state = if reusable && !closed {
            SlotState::Idle
        } else if source == "Remote agent report" {
            SlotState::Offline("remote coordination closed; external effects may remain".into())
        } else {
            SlotState::Offline("owned executor stopped; reconnect required".into())
        };
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
            if matches!(slot.state, SlotState::Idle) {
                slot.state = SlotState::Offline("connection closed".into());
            }
            log(slot, now_ms, "warning", "Worker connection closed", None);
        }
        trim(&mut state);
        self.changed(&mut state);
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let slots: Vec<_> = state
            .slots
            .iter()
            .enumerate()
            .map(|(index, slot)| {
                let (name, task_id, reason) = match &slot.state {
                    SlotState::Offline(reason) => ("offline", None, Some(reason.clone())),
                    SlotState::Idle => ("idle", None, None),
                    SlotState::Working(id) => ("working", Some(id.clone()), None),
                };
                SlotView {
                    slot: index,
                    epoch: slot.epoch,
                    state: name,
                    task_id,
                    reason,
                    logs: slot.logs.iter().cloned().collect(),
                    next_log_sequence: slot.next_log_sequence,
                }
            })
            .collect();
        Snapshot {
            generation: self.generation.clone(),
            total: slots.len(),
            idle: slots.iter().filter(|slot| slot.state == "idle").count(),
            working: slots.iter().filter(|slot| slot.state == "working").count(),
            offline: slots.iter().filter(|slot| slot.state == "offline").count(),
            queued: state.fifo.len(),
            closed: state.closed,
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
            slot.state = SlotState::Offline("connection closed; owned cleanup complete".into());
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

fn valid<T>(state: &State<T>, generation: &str, lease: &Lease<T>) -> bool {
    lease.generation == generation
        && state.slots.get(lease.slot).is_some_and(|slot| {
            slot.epoch == lease.epoch
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
