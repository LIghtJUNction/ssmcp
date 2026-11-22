//! Actual manager calls, separate from capacity changes and task authority.
use super::{Actor, Queue, SlotState, State, TaskStatus, TaskView, require_manager, telemetry};
use crate::worker::{ManagerOperationView, ManagerTargetView};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    time::Instant,
};

const MAX_ACTIVE: usize = 16;
const MAX_ACTIVE_TARGETS: usize = 128;
const MAX_ACTIVE_BYTES: usize = 32 * 1024 - 2;
const MAX_HISTORY: usize = 64;
const MAX_HISTORY_BYTES: usize = 16 * 1024 - 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::worker) enum WaitMode {
    Any,
    All,
}

impl WaitMode {
    pub(in crate::worker) fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::worker) enum WaitCondition {
    TaskFinished { task_id: String },
    WorkerIdle { slot: usize, epoch: u64 },
}

impl WaitCondition {
    fn target(&self) -> ManagerTargetView {
        match self {
            Self::TaskFinished { task_id } => ManagerTargetView {
                kind: "task_finished",
                task_id: Some(task_id.clone()),
                slot: None,
                epoch: None,
                revision: None,
            },
            Self::WorkerIdle { slot, epoch } => ManagerTargetView {
                kind: "worker_idle",
                task_id: None,
                slot: Some(*slot),
                epoch: Some(*epoch),
                revision: None,
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::worker) struct WaitRequest {
    pub(in crate::worker) generation: String,
    pub(in crate::worker) mode: WaitMode,
    pub(in crate::worker) conditions: Vec<WaitCondition>,
    #[serde(default = "default_wait")]
    pub(in crate::worker) wait_ms: u64,
}

fn default_wait() -> u64 {
    5000
}

#[derive(Debug, Serialize)]
pub(in crate::worker) struct ConditionView {
    #[serde(flatten)]
    condition: WaitCondition,
    pub(in crate::worker) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
}

impl WaitRequest {
    fn validate(&self) -> Result<(), String> {
        if self.generation.is_empty()
            || self.generation.len() > 128
            || self.generation.chars().any(char::is_control)
        {
            return Err("worker wait_for generation is invalid".into());
        }
        if self.wait_ms > 5000 || self.conditions.is_empty() || self.conditions.len() > 128 {
            return Err("worker wait_for requires 1..128 conditions and wait_ms 0..5000".into());
        }
        let unique: BTreeSet<_> = self.conditions.iter().collect();
        if unique.len() != self.conditions.len() {
            return Err("worker wait_for conditions must not contain duplicates".into());
        }
        for condition in &self.conditions {
            match condition {
                WaitCondition::TaskFinished { task_id }
                    if task_id.is_empty()
                        || task_id.len() > 256
                        || task_id.chars().any(char::is_control) =>
                {
                    return Err("worker wait_for task_id is invalid".into());
                }
                WaitCondition::WorkerIdle { slot, epoch }
                    if *slot >= crate::worker::config::MAX_WORKERS || *epoch == 0 =>
                {
                    return Err(
                        "worker wait_for requires a valid slot and its nonzero epoch".into(),
                    );
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// All conditions use one instant state; previous matches are never latched.
    pub(in crate::worker) fn outcome(&self, conditions: &[ConditionView]) -> Option<&'static str> {
        let satisfied = conditions
            .iter()
            .filter(|item| item.state == "satisfied")
            .count();
        if match self.mode {
            WaitMode::Any => satisfied != 0,
            WaitMode::All => satisfied == conditions.len(),
        } {
            return Some("satisfied");
        }
        let indeterminate = conditions.iter().any(|item| item.state == "indeterminate");
        let unavailable = conditions.iter().any(|item| item.state == "unavailable");
        let pending = conditions.iter().any(|item| item.state == "pending");
        match self.mode {
            WaitMode::All if indeterminate => Some("indeterminate"),
            WaitMode::All if unavailable => Some("unavailable"),
            WaitMode::Any if !pending && indeterminate => Some("indeterminate"),
            WaitMode::Any if !pending => Some("unavailable"),
            _ => None,
        }
    }
}

pub(in crate::worker) struct WaitSpec {
    pub(in crate::worker) kind: &'static str,
    pub(in crate::worker) mode: &'static str,
    pub(in crate::worker) targets: Vec<ManagerTargetView>,
    pub(in crate::worker) wait_ms: u64,
}

struct Active {
    view: ManagerOperationView,
    started: Instant,
    reserved_bytes: usize,
}

#[derive(Default)]
pub(super) struct Operations {
    sequence: u64,
    active: BTreeMap<u64, Active>,
    history: VecDeque<(ManagerOperationView, usize)>,
    active_bytes: usize,
    active_targets: usize,
    history_bytes: usize,
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn encoded_bytes(view: &ManagerOperationView) -> Result<usize, String> {
    serde_json::to_vec(view)
        .map(|bytes| bytes.len() + 1)
        .map_err(|error| error.to_string())
}

impl Operations {
    fn begin(&mut self, spec: WaitSpec, at_ms: u64) -> Result<u64, String> {
        let operation_id = self
            .sequence
            .checked_add(1)
            .ok_or("worker operation sequence exhausted")?;
        let view = ManagerOperationView {
            operation_id,
            kind: spec.kind,
            state: "waiting",
            mode: Some(spec.mode),
            targets: spec.targets,
            task_id: None,
            slot: None,
            epoch: None,
            summary: None,
            started_at_ms: at_ms,
            elapsed_ms: 0,
            wait_ms: Some(spec.wait_ms),
        };
        let mut largest = view.clone();
        largest.elapsed_ms = u64::MAX;
        largest.state = "indeterminate";
        let reserved_bytes = encoded_bytes(&largest)?;
        if self.active.len() >= MAX_ACTIVE
            || self.active_targets + view.targets.len() > MAX_ACTIVE_TARGETS
            || self.active_bytes + reserved_bytes > MAX_ACTIVE_BYTES
            || reserved_bytes > MAX_HISTORY_BYTES
        {
            return Err("worker manager wait display is full; no wait was registered (16 calls, 128 total targets, bounded bytes)".into());
        }
        self.sequence = operation_id;
        self.active_bytes += reserved_bytes;
        self.active_targets += view.targets.len();
        self.active.insert(
            operation_id,
            Active {
                view,
                started: Instant::now(),
                reserved_bytes,
            },
        );
        Ok(operation_id)
    }

    fn finish(&mut self, id: u64, state: &'static str) -> Option<ManagerOperationView> {
        let active = self.active.remove(&id)?;
        self.active_bytes -= active.reserved_bytes;
        self.active_targets -= active.view.targets.len();
        let mut view = active.view;
        view.state = state;
        view.elapsed_ms = elapsed(active.started);
        self.retain(view.clone());
        Some(view)
    }

    fn retain(&mut self, view: ManagerOperationView) {
        let Ok(bytes) = encoded_bytes(&view) else {
            return;
        };
        if bytes > MAX_HISTORY_BYTES {
            return;
        }
        self.history_bytes += bytes;
        self.history.push_back((view, bytes));
        while self.history.len() > MAX_HISTORY || self.history_bytes > MAX_HISTORY_BYTES {
            if let Some((_, bytes)) = self.history.pop_front() {
                self.history_bytes -= bytes;
            }
        }
    }

    pub(super) fn dispatch(
        &mut self,
        kind: &'static str,
        task: &TaskView,
        epoch: Option<u64>,
        text: &str,
        at_ms: u64,
    ) {
        let Some(operation_id) = self.sequence.checked_add(1) else {
            return;
        };
        self.sequence = operation_id;
        self.retain(ManagerOperationView {
            operation_id,
            kind,
            state: if kind == "submit" {
                "submitted"
            } else {
                "assigned"
            },
            mode: None,
            targets: Vec::new(),
            task_id: Some(task.task_id.clone()),
            slot: task.slot,
            epoch,
            summary: Some(telemetry::display_text(text, 160)),
            started_at_ms: at_ms,
            elapsed_ms: 0,
            wait_ms: None,
        });
    }

    pub(super) fn views(&self) -> Vec<ManagerOperationView> {
        let mut views: Vec<_> = self.history.iter().map(|(view, _)| view.clone()).collect();
        views.extend(self.active.values().map(|active| {
            let mut view = active.view.clone();
            view.elapsed_ms = elapsed(active.started);
            view
        }));
        views.sort_by_key(|view| view.operation_id);
        views
    }
}

pub(in crate::worker) struct WaitGuard<'a, T> {
    queue: &'a Queue<T>,
    id: u64,
    finished: bool,
}

impl<T> WaitGuard<'_, T> {
    pub(in crate::worker) fn finish(mut self, state: &'static str) -> ManagerOperationView {
        let view = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .operations
            .finish(self.id, state)
            .expect("wait guard owns its exact active operation");
        self.finished = true;
        view
    }
}

impl<T> Drop for WaitGuard<'_, T> {
    fn drop(&mut self) {
        if !self.finished {
            // Dropping this future ends only its observation. It never releases
            // a claim, cancels a task, or changes the availability revision.
            let _ = self
                .queue
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .operations
                .finish(self.id, "cancelled");
        }
    }
}

impl<T> Queue<T> {
    pub(in crate::worker) fn begin_wait(
        &self,
        actor: &Actor,
        client_id: &str,
        spec: WaitSpec,
        at_ms: u64,
    ) -> Result<WaitGuard<'_, T>, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        let id = state.operations.begin(spec, at_ms)?;
        Ok(WaitGuard {
            queue: self,
            id,
            finished: false,
        })
    }

    pub(in crate::worker) fn begin_conditions(
        &self,
        actor: &Actor,
        client_id: &str,
        request: &WaitRequest,
        at_ms: u64,
    ) -> Result<WaitGuard<'_, T>, String> {
        request.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        if request.generation != self.generation {
            return Err("worker wait_for belongs to another pool generation".into());
        }
        let _ = evaluate(&state, actor, &request.conditions, true)?;
        let id = state.operations.begin(
            WaitSpec {
                kind: "wait_for",
                mode: request.mode.name(),
                targets: request
                    .conditions
                    .iter()
                    .map(WaitCondition::target)
                    .collect(),
                wait_ms: request.wait_ms,
            },
            at_ms,
        )?;
        Ok(WaitGuard {
            queue: self,
            id,
            finished: false,
        })
    }

    pub(in crate::worker) fn conditions(
        &self,
        actor: &Actor,
        client_id: &str,
        request: &WaitRequest,
    ) -> Result<Vec<ConditionView>, String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        require_manager(&state, actor, client_id)?;
        if request.generation != self.generation {
            return Err("worker wait_for belongs to another pool generation".into());
        }
        evaluate(&state, actor, &request.conditions, false)
    }
}

fn evaluate<T>(
    state: &State<T>,
    actor: &Actor,
    conditions: &[WaitCondition],
    initial: bool,
) -> Result<Vec<ConditionView>, String> {
    conditions
        .iter()
        .map(|condition| {
            let (status, reason) =
                match condition {
                    WaitCondition::TaskFinished { task_id } => match state.tasks.get(task_id) {
                        Some(task) if &task.actor != actor => return Err(
                            "worker wait_for task is not retained for this authenticated manager"
                                .into(),
                        ),
                        Some(task) => match task.view.status {
                            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled => {
                                ("satisfied", None)
                            }
                            TaskStatus::Unknown => (
                                "indeterminate",
                                Some("task effects are unknown; completion was not confirmed"),
                            ),
                            _ => ("pending", None),
                        },
                        None if initial => return Err(
                            "worker wait_for task is not retained for this authenticated manager"
                                .into(),
                        ),
                        None => ("unavailable", Some("task is no longer retained")),
                    },
                    WaitCondition::WorkerIdle { slot, epoch } => {
                        match state
                            .slots
                            .get(*slot)
                            .filter(|worker| worker.enabled && worker.epoch == *epoch)
                        {
                            None if initial => return Err(
                                "worker wait_for slot and epoch do not identify a current worker"
                                    .into(),
                            ),
                            None => ("unavailable", Some("worker slot or epoch changed")),
                            Some(_) if state.closed || state.stopping => {
                                ("unavailable", Some("worker pool is closed or stopping"))
                            }
                            Some(worker) if worker.retirement.is_some() => {
                                ("unavailable", Some("worker is retiring"))
                            }
                            Some(worker)
                                if matches!(
                                    worker.state,
                                    SlotState::Offline(_) | SlotState::Retiring
                                ) =>
                            {
                                ("unavailable", Some("worker is offline or retiring"))
                            }
                            Some(worker) if state.remote_ready && worker.registered.is_none() => {
                                ("unavailable", Some("remote worker is not registered"))
                            }
                            Some(worker)
                                if matches!(worker.state, SlotState::Idle)
                                    && state.availability.accepting =>
                            {
                                ("satisfied", None)
                            }
                            Some(_) => ("pending", None),
                        }
                    }
                };
            Ok(ConditionView {
                condition: condition.clone(),
                state: status,
                reason,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::queue::Outcome;
    use serde_json::json;
    use std::time::Duration;

    const CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn fixture(count: usize) -> (Queue<()>, Actor) {
        let queue = Queue::new("operation-fixture".into(), count).unwrap();
        let owner = Actor {
            provider: "fixture".into(),
            subject: "account".into(),
            participant: Some("manager".into()),
            connection: 7,
        };
        for slot in 0..count {
            queue.online(slot).unwrap();
        }
        queue.attach(&owner, CLIENT, 1).unwrap();
        (queue, owner)
    }

    fn request(queue: &Queue<()>, mode: &str, conditions: serde_json::Value) -> WaitRequest {
        serde_json::from_value(json!({"generation":queue.generation,"mode":mode,"conditions":conditions,"wait_ms":5000})).unwrap()
    }

    #[test]
    fn conditions_use_one_state_and_never_treat_unknown_or_stopping_as_idle() {
        let (queue, owner) = fixture(2);
        let a = queue
            .submit_managed(owner.clone(), CLIENT, "a", "A", (), 2)
            .unwrap();
        let b = queue
            .submit_managed(owner.clone(), CLIENT, "b", "B", (), 3)
            .unwrap();
        let lease_a = queue.claim(0, 1, 4).unwrap().unwrap();
        let lease_b = queue.claim(1, 1, 5).unwrap().unwrap();
        let both_tasks = json!([{"type":"task_finished","task_id":a.task_id},{"type":"task_finished","task_id":b.task_id}]);
        let any = request(&queue, "any", both_tasks.clone());
        let all = request(&queue, "all", both_tasks);
        queue
            .finish(&lease_a, Outcome::Completed("done".into()), true, true, 6)
            .unwrap();
        assert_eq!(
            any.outcome(&queue.conditions(&owner, CLIENT, &any).unwrap()),
            Some("satisfied")
        );
        assert_eq!(
            all.outcome(&queue.conditions(&owner, CLIENT, &all).unwrap()),
            None
        );

        let idle = request(
            &queue,
            "all",
            json!([{"type":"worker_idle","slot":0,"epoch":1},{"type":"worker_idle","slot":1,"epoch":1}]),
        );
        assert_eq!(
            idle.outcome(&queue.conditions(&owner, CLIENT, &idle).unwrap()),
            None
        );
        queue
            .submit_managed(owner.clone(), CLIENT, "c", "C", (), 7)
            .unwrap();
        let lease_c = queue.claim(0, 1, 8).unwrap().unwrap();
        queue
            .finish(&lease_b, Outcome::Completed("done".into()), true, true, 9)
            .unwrap();
        // Each station has been idle, but they have not both been idle together.
        assert_eq!(
            idle.outcome(&queue.conditions(&owner, CLIENT, &idle).unwrap()),
            None
        );
        let pending_c = request(
            &queue,
            "all",
            json!([{"type":"task_finished","task_id":lease_c.task_id}]),
        );
        queue.stop(&owner, CLIENT, 10).unwrap();
        assert_eq!(
            pending_c.outcome(&queue.conditions(&owner, CLIENT, &pending_c).unwrap()),
            None
        );
        assert_eq!(queue.snapshot().working, 1);
        assert_eq!(
            idle.outcome(&queue.conditions(&owner, CLIENT, &idle).unwrap()),
            Some("unavailable")
        );
        queue
            .finish(
                &lease_c,
                Outcome::Unknown("unconfirmed effects".into()),
                true,
                false,
                11,
            )
            .unwrap();
        assert_eq!(
            pending_c.outcome(&queue.conditions(&owner, CLIENT, &pending_c).unwrap()),
            Some("indeterminate")
        );
    }

    #[test]
    fn wait_guard_drop_removes_only_its_operation_without_changing_capacity() {
        let (queue, owner) = fixture(1);
        let task = queue
            .submit_managed(owner.clone(), CLIENT, "pending", "pending work", (), 2)
            .unwrap();
        let request = request(
            &queue,
            "all",
            json!([{"type":"task_finished","task_id":task.task_id}]),
        );
        let before = serde_json::to_value(queue.availability(&owner, CLIENT).unwrap()).unwrap();
        let wake = queue.subscribe();
        let guard = queue
            .begin_conditions(&owner, CLIENT, &request, u64::MAX)
            .unwrap();
        let second = queue.begin_conditions(&owner, CLIENT, &request, 0).unwrap();
        {
            let mut state = queue.state.lock().unwrap();
            state.operations.active.get_mut(&guard.id).unwrap().started =
                Instant::now() - Duration::from_millis(1250);
        }
        let shown = queue.snapshot().manager_operations;
        let row = shown
            .iter()
            .find(|row| row.operation_id == guard.id)
            .unwrap();
        assert_eq!(row.state, "waiting");
        assert!(row.elapsed_ms >= 1250);
        assert_eq!(row.started_at_ms, u64::MAX); // Wall clock never determines elapsed.
        let id = guard.id;
        drop(guard);
        let shown = queue.snapshot().manager_operations;
        assert_eq!(shown.iter().filter(|row| row.state == "waiting").count(), 1);
        assert_eq!(
            shown
                .iter()
                .find(|row| row.operation_id == id)
                .unwrap()
                .state,
            "cancelled"
        );
        assert!(!wake.has_changed().unwrap());
        assert_eq!(
            serde_json::to_value(queue.availability(&owner, CLIENT).unwrap()).unwrap(),
            before
        );
        assert_eq!(
            queue.result(&owner, &task.task_id).unwrap().status,
            TaskStatus::Queued
        );
        second.finish("timed_out");
        assert!(
            queue
                .snapshot()
                .manager_operations
                .iter()
                .all(|row| row.state != "waiting")
        );
    }

    #[test]
    fn invalid_manager_conditions_never_register_and_dispatch_is_not_assignment() {
        let (queue, owner) = fixture(1);
        let text = "Unicode 中文\n\u{001b}\u{0007} ".repeat(30);
        let task = queue
            .submit_managed(owner.clone(), CLIENT, "exact", &text, (), 2)
            .unwrap();
        queue
            .submit_managed(owner.clone(), CLIENT, "exact", &text, (), 3)
            .unwrap();
        let views = queue.snapshot().manager_operations;
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, "submitted");
        assert!(views[0].slot.is_none());
        let summary = views[0].summary.as_ref().unwrap();
        assert!(summary.len() <= 160 && !summary.chars().any(char::is_control));
        let valid = request(
            &queue,
            "all",
            json!([{"type":"task_finished","task_id":task.task_id}]),
        );
        let foreign = Actor {
            participant: Some("observer".into()),
            ..owner.clone()
        };
        assert!(queue.begin_conditions(&foreign, CLIENT, &valid, 4).is_err());
        for bad in [
            json!({"generation":"old","mode":"all","conditions":[{"type":"task_finished","task_id":task.task_id}]}),
            json!({"generation":queue.generation,"mode":"all","conditions":[]}),
            json!({"generation":queue.generation,"mode":"all","conditions":[{"type":"worker_idle","slot":0,"epoch":2}]}),
            json!({"generation":queue.generation,"mode":"any","conditions":[{"type":"task_finished","task_id":"not-owned"}]}),
            json!({"generation":queue.generation,"mode":"any","conditions":[{"type":"worker_idle","slot":0,"epoch":1},{"type":"worker_idle","slot":0,"epoch":1}]}),
        ] {
            let bad: WaitRequest = serde_json::from_value(bad).unwrap();
            assert!(queue.begin_conditions(&owner, CLIENT, &bad, 4).is_err());
        }
        assert_eq!(queue.snapshot().manager_operations.len(), 1);
        let lease = queue.claim(0, 1, 5).unwrap().unwrap();
        let views = queue.snapshot().manager_operations;
        assert_eq!(views.len(), 2);
        assert_eq!(views[1].state, "assigned");
        assert_eq!(views[1].task_id.as_deref(), Some(lease.task_id.as_str()));
        assert_eq!(views[1].slot, Some(lease.slot));
        assert_eq!(views[1].epoch, Some(lease.epoch));
    }

    #[test]
    fn wait_targets_and_serialized_history_have_independent_hard_bounds() {
        let (queue, owner) = fixture(1);
        let mut conditions = Vec::new();
        for index in 0..128 {
            let task = queue
                .submit_managed(
                    owner.clone(),
                    CLIENT,
                    &format!("task-{index}"),
                    "body",
                    (),
                    index,
                )
                .unwrap();
            conditions.push(json!({"type":"task_finished","task_id":task.task_id}));
        }
        let request = request(&queue, "all", json!(conditions));
        let guard = queue.begin_conditions(&owner, CLIENT, &request, 1).unwrap();
        assert!(queue.begin_conditions(&owner, CLIENT, &request, 2).is_err());
        assert!(
            serde_json::to_vec(&queue.snapshot().manager_operations)
                .unwrap()
                .len()
                <= 48 * 1024
        );
        drop(guard);
        let single = WaitRequest {
            conditions: vec![request.conditions[0].clone()],
            ..request
        };
        let guards: Vec<_> = (0..16)
            .map(|_| queue.begin_conditions(&owner, CLIENT, &single, 3).unwrap())
            .collect();
        assert!(queue.begin_conditions(&owner, CLIENT, &single, 4).is_err());
        drop(guards);
        for _ in 0..80 {
            queue
                .begin_conditions(&owner, CLIENT, &single, 5)
                .unwrap()
                .finish("timed_out");
        }
        let views = queue.snapshot().manager_operations;
        assert!(views.len() <= 64);
        assert!(
            views
                .windows(2)
                .all(|rows| rows[0].operation_id < rows[1].operation_id)
        );
        assert!(serde_json::to_vec(&views).unwrap().len() <= 16 * 1024);
        assert_eq!(queue.snapshot().queued, 128);
    }
}
