//! Authenticated remote task leases. This service never starts a model, shell,
//! network callback or OS executor. An external agent explicitly claims work,
//! performs its own tool calls and reports a result; delivery is not completion.
use super::queue::{Actor, Lease, Outcome, Queue, TaskView};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::Mutex,
};

const MAX_RECEIPTS: usize = 128;
pub(super) const WAIT_HINT: &str = "Call ssmcp.worker.wait again to block for more work. A timeout is only an idle wait; it does not finish a task. Do not repeat effects after an uncertain result.";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Claim {
    pub(super) id: String,
    pub(super) generation: String,
    pub(super) slot: usize,
    pub(super) epoch: u64,
    pub(super) task_id: String,
    pub(super) attempt: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Completion {
    pub(super) claim: Claim,
    pub(super) status: Status,
    #[serde(default)]
    pub(super) output: Option<String>,
    #[serde(default)]
    pub(super) error: Option<String>,
}

impl Completion {
    fn outcome(&self) -> Result<Outcome, String> {
        if self
            .output
            .as_ref()
            .is_some_and(|text| text.len() > 64 * 1024)
            || self.error.as_ref().is_some_and(|text| text.len() > 4096)
        {
            return Err("remote result exceeds 64 KiB output or 4096 bytes of error".into());
        }
        match (&self.status, &self.output, &self.error) {
            (Status::Completed, Some(output), None) => Ok(Outcome::Completed(output.clone())),
            (Status::Failed, None, Some(error)) if !error.trim().is_empty() => {
                Ok(Outcome::Failed(error.clone()))
            }
            (Status::Cancelled, None, None) => Ok(Outcome::Cancelled),
            _ => Err("completed requires output only; failed requires a nonempty error only; cancelled accepts neither".into()),
        }
    }
}

pub(super) enum Claimed<T> {
    Waiting,
    Assigned { claim: Claim, lease: Lease<T> },
    Completed { claim: Claim, task: TaskView },
}

struct Assignment<T> {
    actor: Actor,
    request_id: String,
    claim: Claim,
    lease: Lease<T>,
}

struct Receipt {
    actor: Actor,
    request_id: String,
    completion: Completion,
    task: TaskView,
}

struct State<T> {
    ready: bool,
    active: BTreeMap<String, Assignment<T>>,
    completed: VecDeque<Receipt>,
}

pub(super) struct Remote<T> {
    state: Mutex<State<T>>,
}

impl<T: Clone> Remote<T> {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(State {
                ready: false,
                active: BTreeMap::new(),
                completed: VecDeque::new(),
            }),
        }
    }

    pub(super) fn start(&self, queue: &Queue<T>) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.ready {
            return Ok(());
        }
        if queue.snapshot().closed {
            return Err("remote worker connection is closed".into());
        }
        for slot in queue.snapshot().slots {
            queue.online(slot.slot)?;
        }
        state.ready = true;
        Ok(())
    }

    pub(super) fn claim(
        &self,
        queue: &Queue<T>,
        actor: &Actor,
        request_id: &str,
        now_ms: u64,
    ) -> Result<Claimed<T>, String> {
        if request_id.is_empty()
            || request_id.len() > 128
            || request_id.chars().any(char::is_control)
        {
            return Err(
                "worker wait request_id must contain 1..128 bytes without control characters"
                    .into(),
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(receipt) = state
            .completed
            .iter()
            .find(|receipt| &receipt.actor == actor && receipt.request_id == request_id)
        {
            return Ok(Claimed::Completed {
                claim: receipt.completion.claim.clone(),
                task: receipt.task.clone(),
            });
        }
        if !state.ready || queue.snapshot().closed {
            return Err("remote worker connection is not ready or has closed".into());
        }
        if let Some(active) = state
            .active
            .values()
            .find(|active| &active.actor == actor && active.request_id == request_id)
        {
            return Ok(Claimed::Assigned {
                claim: active.claim.clone(),
                lease: active.lease.clone(),
            });
        }
        // Generate the receipt identity before changing the queue. Failure to
        // obtain randomness cannot strand a claimed task without its receipt.
        let id = nonce()?;
        let Some(lease) = queue.claim_remote(actor, now_ms)? else {
            return Ok(Claimed::Waiting);
        };
        let claim = Claim {
            id: id.clone(),
            generation: lease.generation.clone(),
            slot: lease.slot,
            epoch: lease.epoch,
            task_id: lease.task_id.clone(),
            attempt: lease.attempt,
        };
        state.active.insert(
            id,
            Assignment {
                actor: actor.clone(),
                request_id: request_id.into(),
                claim: claim.clone(),
                lease: lease.clone(),
            },
        );
        Ok(Claimed::Assigned { claim, lease })
    }

    pub(super) fn complete(
        &self,
        queue: &Queue<T>,
        actor: &Actor,
        completion: Completion,
        now_ms: u64,
    ) -> Result<(TaskView, bool), String> {
        let outcome = completion.outcome()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(receipt) = state
            .completed
            .iter()
            .find(|receipt| receipt.completion.claim.id == completion.claim.id)
        {
            if &receipt.actor != actor || receipt.completion != completion {
                return Err(
                    "remote completion does not match its authenticated retained receipt".into(),
                );
            }
            return Ok((receipt.task.clone(), true));
        }
        if !state.ready || queue.snapshot().closed {
            return Err("remote worker connection is not ready or has closed".into());
        }
        let active = state
            .active
            .get(&completion.claim.id)
            .filter(|active| &active.actor == actor && active.claim == completion.claim)
            .ok_or("remote claim is not current for this exact authenticated participant")?;
        let task = queue.finish_remote(&active.lease, outcome, true, now_ms)?;
        let active = state
            .active
            .remove(&completion.claim.id)
            .expect("validated remote claim");
        state.completed.push_back(Receipt {
            actor: active.actor,
            request_id: active.request_id,
            completion,
            task: task.clone(),
        });
        while state.completed.len() > MAX_RECEIPTS {
            state.completed.pop_front();
        }
        Ok((task, false))
    }

    pub(super) fn close(&self, queue: &Queue<T>, now_ms: u64) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.ready = false;
        let active = std::mem::take(&mut state.active);
        let mut first_error = None;
        for assignment in active.into_values() {
            if let Err(error) = queue.finish_remote(&assignment.lease,
                Outcome::Unknown("remote connection closed without a completion report; external effects may remain".into()),
                false, now_ms)
            {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub(super) fn nonce() -> Result<String, String> {
    let mut bytes = [0; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|error| error.to_string())?;
    Ok(crate::hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::queue::TaskStatus;

    fn actor(id: &str) -> Actor {
        Actor {
            provider: "fixture".into(),
            subject: "owner".into(),
            participant: Some(id.into()),
            connection: 7,
        }
    }

    fn service(count: usize) -> (Queue<&'static str>, Remote<&'static str>) {
        let queue = Queue::new("remote-fixture".into(), count).unwrap();
        let remote = Remote::new();
        assert_eq!(queue.snapshot().idle, 0);
        remote.start(&queue).unwrap();
        (queue, remote)
    }

    fn claimed(
        remote: &Remote<&'static str>,
        queue: &Queue<&'static str>,
        actor: &Actor,
        request: &str,
    ) -> Claim {
        let Claimed::Assigned { claim, .. } = remote.claim(queue, actor, request, 2).unwrap()
        else {
            panic!("expected assigned task");
        };
        claim
    }

    fn success(claim: Claim) -> Completion {
        Completion {
            claim,
            status: Status::Completed,
            output: Some("confirmed external result".into()),
            error: None,
        }
    }

    #[test]
    fn manager_stop_preserves_remote_claim_until_exact_acknowledgement() {
        let (queue, remote) = service(1);
        let client = "a".repeat(64);
        let owner = actor("manager");
        let executor = actor("executor");
        let first = queue.submit_managed(owner.clone(), &client, "first", "work", "prepared", 1).unwrap();
        let pending = queue.submit_managed(owner.clone(), &client, "pending", "queued", "prepared", 1).unwrap();
        let claim = claimed(&remote, &queue, &executor, "take-first");
        assert_eq!(claim.task_id, first.task_id);
        assert!(queue.stop(&executor, &client, 2).is_err());
        assert!(queue.stop(&owner, &client, 3).unwrap());
        assert!(!queue.stop(&owner, &client, 4).unwrap());
        assert!(queue.snapshot().stopping);
        assert!(!queue.snapshot().closed);
        assert_eq!(queue.snapshot().working, 1);
        assert_eq!(queue.snapshot().idle, 0);
        assert_eq!(queue.snapshot().queued, 0);
        assert_eq!(queue.result(&owner, &first.task_id).unwrap().status, TaskStatus::Cancelling);
        assert_eq!(queue.result(&owner, &pending.task_id).unwrap().status, TaskStatus::Cancelled);
        assert!(queue.submit_managed(owner.clone(), &client, "new", "work", "prepared", 5).is_err());
        let Claimed::Assigned { claim: retry, lease } = remote.claim(&queue, &executor, "take-first", 5).unwrap() else { panic!("active claim was lost after stop") };
        assert_eq!(retry, claim);
        assert!(queue.cancellation_requested(&lease));
        let completion = Completion { claim, status: Status::Cancelled, output: None, error: None };
        assert!(remote.complete(&queue, &owner, completion.clone(), 6).is_err());
        assert_eq!(queue.snapshot().working, 1);
        let (task, duplicate) = remote.complete(&queue, &executor, completion.clone(), 7).unwrap();
        assert!(!duplicate);
        assert_eq!(task.status, TaskStatus::Cancelled);
        assert!(!task.replay_safe);
        assert_eq!(queue.snapshot().working, 0);
        assert_eq!(queue.snapshot().offline, 1);
        assert_eq!(queue.snapshot().idle, 0);
        assert!(remote.complete(&queue, &executor, completion, 8).unwrap().1);
    }

    #[test]
    fn reconnect_requires_a_new_manager_and_never_replays_unknown_work() {
        let (queue, remote) = service(1);
        let client = "a".repeat(64);
        let owner = actor("old-manager");
        let executor = actor("executor");
        let task = queue.submit_managed(owner.clone(), &client, "old-work", "work", "prepared", 1).unwrap();
        let claim = claimed(&remote, &queue, &executor, "old-claim");
        queue.stop(&owner, &client, 3).unwrap();
        queue.close(4);
        remote.close(&queue, 4).unwrap();
        let old = queue.result(&owner, &task.task_id).unwrap();
        assert_eq!(old.status, TaskStatus::Unknown);
        assert!(!old.replay_safe);
        let fresh = Queue::new("new-native-generation".into(), 1).unwrap();
        let remote_new = Remote::<&'static str>::new();
        remote_new.start(&fresh).unwrap();
        assert!(fresh.snapshot().manager.is_none());
        assert_eq!(fresh.snapshot().queued, 0);
        let mut new_owner = actor("new-manager");
        new_owner.connection += 1;
        let new_client = "b".repeat(64);
        fresh.attach(&new_owner, &new_client, 5).unwrap();
        assert!(fresh.is_manager(&new_owner, &new_client));
        assert!(!fresh.is_manager(&owner, &client));
        assert!(remote_new.complete(&fresh, &executor, success(claim), 6).is_err());
        assert_eq!(fresh.snapshot().idle, 1);
        assert_eq!(fresh.snapshot().queued, 0);
    }

    #[test]
    fn remote_claims_are_exclusive_and_retries_keep_the_exact_lease() {
        let (queue, remote) = service(2);
        let a = queue
            .submit(actor("submitter"), "a", "first", "prepared first", 1)
            .unwrap();
        let b = queue
            .submit(actor("submitter"), "b", "second", "prepared second", 1)
            .unwrap();
        let executor = actor("executor");
        let first = claimed(&remote, &queue, &executor, "request-a");
        assert_eq!(first.task_id, a.task_id);
        assert_eq!(claimed(&remote, &queue, &executor, "request-a"), first);
        let second = claimed(&remote, &queue, &actor("other-executor"), "request-b");
        assert_eq!(second.task_id, b.task_id);
        assert_ne!(first.slot, second.slot);
        assert!(matches!(
            remote.claim(&queue, &executor, "request-c", 3).unwrap(),
            Claimed::Waiting
        ));
        let done = success(first.clone());
        let (view, duplicate) = remote.complete(&queue, &executor, done.clone(), 4).unwrap();
        assert_eq!(view.status, TaskStatus::Completed);
        assert!(!duplicate);
        assert!(remote.complete(&queue, &executor, done, 5).unwrap().1);
        assert!(matches!(
            remote.claim(&queue, &executor, "request-a", 6).unwrap(),
            Claimed::Completed { .. }
        ));
        assert_eq!(queue.snapshot().working, 1);
        assert_eq!(queue.snapshot().idle, 1);
    }

    #[test]
    fn remote_results_require_exact_actor_generation_epoch_task_and_attempt() {
        let (queue, remote) = service(1);
        let owner = actor("submitter");
        let task = queue
            .submit(owner.clone(), "work", "private body", "prepared", 1)
            .unwrap();
        let mut foreign = actor("executor");
        foreign.subject = "other-account".into();
        assert!(matches!(
            remote.claim(&queue, &foreign, "foreign", 2).unwrap(),
            Claimed::Waiting
        ));
        assert_eq!(
            queue.result(&owner, &task.task_id).unwrap().status,
            TaskStatus::Queued
        );
        let executor = actor("executor");
        let claim = claimed(&remote, &queue, &executor, "own");
        assert!(
            remote
                .complete(&queue, &owner, success(claim.clone()), 3)
                .is_err()
        );
        let mut reconnect = executor.clone();
        reconnect.connection += 1;
        assert!(
            remote
                .complete(&queue, &reconnect, success(claim.clone()), 3)
                .is_err()
        );
        for field in 0..5 {
            let mut altered = claim.clone();
            match field {
                0 => altered.generation.push('x'),
                1 => altered.slot += 1,
                2 => altered.epoch += 1,
                3 => altered.task_id.push('x'),
                _ => altered.attempt += 1,
            }
            assert!(
                remote
                    .complete(&queue, &executor, success(altered), 3)
                    .is_err()
            );
        }
        assert_eq!(queue.snapshot().working, 1);
        remote
            .complete(&queue, &executor, success(claim.clone()), 4)
            .unwrap();
        let mut changed = success(claim);
        changed.output = Some("different result".into());
        assert!(remote.complete(&queue, &executor, changed, 5).is_err());
        assert!(queue.result(&executor, &task.task_id).is_err());
    }

    #[test]
    fn remote_cancel_needs_acknowledgement_and_disconnect_does_not_replay() {
        let (queue, remote) = service(1);
        let owner = actor("submitter");
        let executor = actor("executor");
        let task = queue
            .submit(owner.clone(), "work", "body", "prepared", 1)
            .unwrap();
        let claim = claimed(&remote, &queue, &executor, "wait");
        queue.cancel(&owner, &task.task_id, 3).unwrap();
        assert_eq!(queue.snapshot().idle, 0);
        let Claimed::Assigned { lease, .. } = remote.claim(&queue, &executor, "wait", 4).unwrap()
        else {
            panic!("retained claim");
        };
        assert!(queue.cancellation_requested(&lease));
        remote
            .complete(
                &queue,
                &executor,
                Completion {
                    claim: claim.clone(),
                    status: Status::Cancelled,
                    output: None,
                    error: None,
                },
                5,
            )
            .unwrap();
        assert_eq!(queue.snapshot().idle, 1);
        let next = queue
            .submit(owner.clone(), "second", "next", "prepared", 6)
            .unwrap();
        let late = claimed(&remote, &queue, &executor, "next-wait");
        queue.close(7);
        remote.close(&queue, 7).unwrap();
        assert_eq!(
            queue.result(&owner, &next.task_id).unwrap().status,
            TaskStatus::Unknown
        );
        assert!(!queue.result(&owner, &next.task_id).unwrap().replay_safe);
        assert!(
            remote
                .complete(&queue, &executor, success(late), 8)
                .is_err()
        );
        assert_eq!(queue.snapshot().working, 0);
        assert_eq!(queue.snapshot().offline, 1);
        assert!(remote.start(&queue).is_err());
    }

    #[test]
    fn simultaneous_remote_claimants_cannot_share_a_slot_or_a_task() {
        let (queue, remote) = service(2);
        for key in ["one", "two"] {
            queue
                .submit(actor("submitter"), key, key, "prepared", 1)
                .unwrap();
        }
        let barrier = std::sync::Barrier::new(3);
        let claims = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                barrier.wait();
                claimed(&remote, &queue, &actor("first"), "first-wait")
            });
            let b = scope.spawn(|| {
                barrier.wait();
                claimed(&remote, &queue, &actor("second"), "second-wait")
            });
            barrier.wait();
            [a.join().unwrap(), b.join().unwrap()]
        });
        assert_ne!(claims[0].slot, claims[1].slot);
        assert_ne!(claims[0].task_id, claims[1].task_id);
        assert_eq!(queue.snapshot().working, 2);
        assert_eq!(queue.snapshot().queued, 0);
    }

    #[test]
    fn completion_arguments_cannot_supply_identity_or_confuse_result_status() {
        let value = serde_json::json!({"claim":{"id":"id","generation":"pool","slot":0,"epoch":1,"task_id":"task","attempt":1},"status":"completed","output":"done"});
        let good: Completion = serde_json::from_value(value.clone()).unwrap();
        assert!(good.outcome().is_ok());
        for field in ["actor", "provider", "subject", "agent_id", "connection"] {
            let mut forged = value.clone();
            forged[field] = serde_json::json!("forged");
            assert!(serde_json::from_value::<Completion>(forged).is_err());
        }
        for (status, output, error) in [
            (Status::Completed, None, None),
            (Status::Failed, None, Some(" ".into())),
            (Status::Cancelled, Some("done".into()), None),
            (Status::Completed, Some("x".repeat(64 * 1024 + 1)), None),
        ] {
            assert!(
                Completion {
                    claim: good.claim.clone(),
                    status,
                    output,
                    error
                }
                .outcome()
                .is_err()
            );
        }
    }
}
