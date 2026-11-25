//! An idle station may ask the current claimant to split off independent work.
//! Only the existing manager can admit that work, with its own task and claim.
use super::*;

const RETAINED: usize = 128;
const MAX_SCOPE: usize = 2048;

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Action {
    Request { request_id: String, task_id: String },
    Accept { share_id: String, text: String },
    Decline { share_id: String },
    List { after: Option<u64> },
}

#[derive(Clone)]
pub(super) struct Destination {
    pub(super) share_id: String,
    pub(super) slot: usize,
    pub(super) epoch: u64,
}

#[derive(PartialEq, Eq)]
enum Status {
    Requested,
    Accepted,
    Declined,
    Expired,
    Dispatched,
}

impl Status {
    fn pending(&self) -> bool {
        matches!(self, Self::Requested | Self::Accepted)
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Accepted => "accepted",
            Self::Declined => "declined",
            Self::Expired => "expired",
            Self::Dispatched => "dispatched",
        }
    }
}

struct Station {
    actor: Actor,
    slot: usize,
    epoch: u64,
}

impl Station {
    fn view(&self) -> Value {
        json!({"participant_id":self.actor.participant,"slot":self.slot,"epoch":self.epoch})
    }
    fn current<'a>(&self, slots: &'a [Slot]) -> Option<&'a Slot> {
        slots.get(self.slot).filter(|s| {
            s.enabled
                && s.epoch == self.epoch
                && s.retirement.is_none()
                && s.registered.as_ref() == Some(&self.actor)
        })
    }
}

struct Request {
    sequence: u64,
    id: String,
    key: String,
    parent: String,
    helper: Station,
    claimant: Station,
    status: Status,
    scope: Option<String>,
    child: Option<String>,
}

impl Request {
    fn view(&self) -> Value {
        json!({"share_id":self.id,"sequence":self.sequence,"status":self.status.name(),
            "parent_task_id":self.parent,"requester":self.helper.view(),"claimant":self.claimant.view(),
            "text":self.scope,"child_task_id":self.child})
    }
}

#[derive(Default)]
pub(super) struct Requests {
    sequence: u64,
    records: VecDeque<Request>,
}

impl Requests {
    pub(super) fn destination(&self, id: &str, text: &str) -> Result<Destination, String> {
        let request = self
            .records
            .iter()
            .find(|r| r.id == id)
            .ok_or("work-sharing request is not retained in this pool")?;
        if request.status != Status::Accepted || request.scope.as_deref() != Some(text) {
            return Err(
                "dispatch requires a live accepted share and its exact approved task text".into(),
            );
        }
        Ok(Destination {
            share_id: id.into(),
            slot: request.helper.slot,
            epoch: request.helper.epoch,
        })
    }
    pub(super) fn dispatched(&mut self, id: &str, child: &str) {
        let record = self
            .records
            .iter_mut()
            .find(|r| r.id == id)
            .expect("validated share");
        record.status = Status::Dispatched;
        record.child = Some(child.into());
    }
}

pub(super) fn expire_requests<T>(state: &mut State<T>) {
    for r in state
        .shares
        .records
        .iter_mut()
        .filter(|r| r.status.pending())
    {
        let helper_idle = r
            .helper
            .current(&state.slots)
            .is_some_and(|s| matches!(s.state, SlotState::Idle));
        let owns_parent = r
            .claimant
            .current(&state.slots)
            .is_some_and(|s| matches!(&s.state, SlotState::Working(id) if id == &r.parent));
        if state.stopping
            || state.closed
            || !helper_idle
            || !owns_parent
            || state
                .tasks
                .get(&r.parent)
                .is_none_or(|t| t.view.status != TaskStatus::Working)
        {
            r.status = Status::Expired;
        }
    }
}

fn has_targeted_work<T>(state: &State<T>, slot: usize, epoch: u64) -> bool {
    state.fifo.iter().any(|id| {
        state.tasks[id]
            .share
            .as_ref()
            .is_some_and(|target| target.slot == slot && target.epoch == epoch)
    })
}

impl<T: Clone> Queue<T> {
    pub(in crate::worker) fn share(
        &self,
        actor: &Actor,
        client: &str,
        args: Value,
    ) -> Result<Value, String> {
        let action: Action = serde_json::from_value(args)
            .map_err(|e| format!("invalid work-sharing action: {e}"))?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let manager = state
            .manager
            .as_ref()
            .ok_or("work sharing requires an attached manager")?;
        if !lifecycle::same_account(&manager.actor, actor) {
            return Err(
                "work sharing requires this pool's authenticated account and connection".into(),
            );
        }
        let is_manager = manager.matches(actor, client);
        let result = match action {
            Action::List { after } => {
                if !is_manager
                    && !state
                        .slots
                        .iter()
                        .any(|s| s.enabled && s.registered.as_ref() == Some(actor))
                {
                    return Err("work sharing requires a registered station or the manager".into());
                }
                let mut visible = state.shares.records.iter().filter(|r| {
                    r.sequence > after.unwrap_or(0)
                        && (is_manager || r.claimant.actor == *actor || r.helper.actor == *actor)
                });
                let page: Vec<_> = visible.by_ref().take(16).collect();
                let next = visible.next().and_then(|_| page.last().map(|r| r.sequence));
                return Ok(
                    json!({"requests":page.into_iter().map(Request::view).collect::<Vec<_>>(),"next_cursor":next}),
                );
            }
            Action::Request {
                request_id,
                task_id,
            } => {
                if request_id.is_empty()
                    || request_id.len() > 128
                    || request_id.chars().any(char::is_control)
                {
                    return Err("work-sharing request_id must contain 1..128 bytes without control characters".into());
                }
                if let Some(r) = state
                    .shares
                    .records
                    .iter()
                    .find(|r| r.helper.actor == *actor && r.key == request_id)
                {
                    return if r.parent == task_id {
                        Ok(r.view())
                    } else {
                        Err("work-sharing request_id already names a different parent task".into())
                    };
                }
                if state.closed || state.stopping {
                    return Err("worker pool is not accepting work-sharing requests".into());
                }
                let helper = state
                    .slots
                    .iter()
                    .enumerate()
                    .find(|(_, s)| {
                        s.enabled
                            && s.retirement.is_none()
                            && s.registered.as_ref() == Some(actor)
                            && matches!(s.state, SlotState::Idle)
                    })
                    .map(|(slot, s)| Station {
                        actor: actor.clone(),
                        slot,
                        epoch: s.epoch,
                    })
                    .ok_or("the requester must be a registered idle station")?;
                if has_targeted_work(&state, helper.slot, helper.epoch) {
                    return Err("a directed child task is already waiting for this station".into());
                }
                let claimant = state
                    .slots
                    .iter()
                    .enumerate()
                    .find(|(_, s)| {
                        s.enabled
                            && s.retirement.is_none()
                            && matches!(&s.state, SlotState::Working(id) if id == &task_id)
                    })
                    .and_then(|(slot, s)| {
                        Some(Station {
                            actor: s.registered.clone()?,
                            slot,
                            epoch: s.epoch,
                        })
                    })
                    .filter(|_| {
                        state
                            .tasks
                            .get(&task_id)
                            .is_some_and(|t| t.view.status == TaskStatus::Working)
                    })
                    .ok_or("target task has no active remote claimant")?;
                if state.shares.records.iter().any(|r| {
                    r.status.pending() && (r.helper.actor == *actor || r.parent == task_id)
                }) {
                    return Err("this idle station or parent task already has an outstanding work-sharing request".into());
                }
                if state.shares.records.len() == RETAINED {
                    let pos = state
                        .shares
                        .records
                        .iter()
                        .position(|r| !r.status.pending())
                        .ok_or("all work-sharing request slots are occupied")?;
                    state.shares.records.remove(pos);
                }
                let sequence = state
                    .shares
                    .sequence
                    .checked_add(1)
                    .ok_or("work-sharing request sequence exhausted")?;
                state.shares.sequence = sequence;
                let r = Request {
                    sequence,
                    id: format!("share_{}_{sequence}", self.generation),
                    key: request_id,
                    parent: task_id,
                    helper,
                    claimant,
                    status: Status::Requested,
                    scope: None,
                    child: None,
                };
                let v = r.view();
                state.shares.records.push_back(r);
                v
            }
            Action::Accept { share_id, text } => {
                if text.trim().is_empty() || text.len() > MAX_SCOPE || text.contains('\0') {
                    return Err(
                        "the independent task must contain 1..2048 bytes without NUL".into(),
                    );
                }
                let (record, changed) = respond(&mut state.shares, actor, &share_id, Some(text))?;
                let receipt = record.view();
                if !changed {
                    return Ok(receipt);
                }
                let slot = record.claimant.slot;
                let parent = record.parent.clone();
                let now = crate::worker::now_ms();
                log(
                    &mut state.slots[slot],
                    now,
                    "info",
                    "Accepted work-sharing request; awaiting manager dispatch",
                    Some(&parent),
                );
                self.record_task_event(
                    &mut state,
                    "worker.progress",
                    &parent,
                    Some(ReportEvent::Progress),
                    now,
                );
                receipt
            }
            Action::Decline { share_id } => {
                let (record, changed) = respond(&mut state.shares, actor, &share_id, None)?;
                if !changed {
                    return Ok(record.view());
                }
                record.view()
            }
        };
        self.changed(&mut state);
        Ok(result)
    }

    pub(in crate::worker) fn share_candidates(&self, actor: &Actor) -> Vec<Value> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.stopping
            || s.closed
            || s.manager
                .as_ref()
                .is_none_or(|m| !lifecycle::same_account(&m.actor, actor))
            || !s.slots.iter().enumerate().any(|(index, slot)| {
                slot.enabled
                    && slot.retirement.is_none()
                    && slot.registered.as_ref() == Some(actor)
                    && matches!(slot.state, SlotState::Idle)
                    && !has_targeted_work(&s, index, slot.epoch)
            })
            || s.shares
                .records
                .iter()
                .any(|r| r.status.pending() && r.helper.actor == *actor)
        {
            return Vec::new();
        }
        s.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let SlotState::Working(task_id) = &slot.state else {
                    return None;
                };
                let claimant = slot.registered.as_ref()?;
                let requested = s
                    .shares
                    .records
                    .iter()
                    .any(|request| request.parent == *task_id && request.status.pending());
                if slot.retirement.is_some()
                    || s.tasks.get(task_id)?.view.status != TaskStatus::Working
                    || requested
                {
                    return None;
                }
                Some(json!({
                    "task_id": task_id, "slot": index, "epoch": slot.epoch,
                    "participant_id": claimant.participant
                }))
            })
            .take(8)
            .collect()
    }

    pub(in crate::worker) fn share_inbox(&self, actor: &Actor) -> Vec<Value> {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.shares
            .records
            .iter()
            .filter(|r| r.status == Status::Requested && r.claimant.actor == *actor)
            .map(Request::view)
            .collect()
    }

    pub(super) fn cancel_unavailable_shares(&self, state: &mut State<T>, now_ms: u64) {
        let unclaimed: Vec<_> = state
            .fifo
            .iter()
            .filter(|id| {
                state.tasks[*id].share.as_ref().is_some_and(|d| {
                    state.slots.get(d.slot).is_none_or(|s| {
                        !s.enabled
                            || s.epoch != d.epoch
                            || s.retirement.is_some()
                            || s.registered.is_none()
                            || matches!(s.state, SlotState::Offline(_) | SlotState::Retiring)
                    })
                })
            })
            .cloned()
            .collect();
        for id in unclaimed {
            state.fifo.retain(|queued| queued != &id);
            let task = state.tasks.get_mut(&id).expect("queued share");
            task.view.status = TaskStatus::Cancelled;
            task.view.error = Some("Directed child task was not claimed before its designated station became unavailable".into());
            task.view.updated_ms = now_ms;
            state.finished.push_back(id.clone());
            self.record_task_event(state, "worker.completed", &id, None, now_ms);
        }
        trim(state);
    }
}

fn respond<'a>(
    shares: &'a mut Requests,
    actor: &Actor,
    id: &str,
    scope: Option<String>,
) -> Result<(&'a Request, bool), String> {
    let record = shares
        .records
        .iter_mut()
        .find(|r| r.id == id && r.claimant.actor == *actor)
        .ok_or("work-sharing response requires the original authenticated claimant")?;
    let changed = record.status == Status::Requested;
    if changed {
        record.status = if scope.is_some() {
            Status::Accepted
        } else {
            Status::Declined
        };
        record.scope = scope;
    } else if record.status == Status::Expired || record.scope != scope {
        return Err("work-sharing request expired or already has a different response".into());
    }
    Ok((record, changed))
}

pub(in crate::worker) fn tool() -> Value {
    let id = json!({"type":"string","minLength":1,"maxLength":256});
    let variant = |action: &str, fields: Value, required: &[&str]| {
        let mut properties = fields;
        properties["action"] = json!({"const":action});
        let mut names = vec!["action"];
        names.extend_from_slice(required);
        json!({"type":"object","properties":properties,"required":names,"additionalProperties":false})
    };
    json!({"name":"ssmcp.worker.share","description":"An idle registered worker requests a split from a busy task owner. The claimant accepts with independent child task text or declines. The verified manager lists requests and dispatches accepted text using worker.submit with share_id. Only the original helper station can acquire the new task through worker.wait. Parent ownership is preserved. Reuse request_id on retry. List is actor-scoped, with at most 16 per page and the last 128 requests retained.",
        "inputSchema":{"type":"object","additionalProperties":false,
            "properties":{"action":{},"task_id":{},"request_id":{},"share_id":{},"text":{},"after":{}},
            "oneOf":[variant("request",json!({"task_id":id,"request_id":{"type":"string","minLength":1,"maxLength":128}}),&["task_id","request_id"]),
            variant("accept",json!({"share_id":id,"text":{"type":"string","minLength":1,"maxLength":MAX_SCOPE}}),&["share_id","text"]),
            variant("decline",json!({"share_id":id}),&["share_id"]),variant("list",json!({"after":{"type":"integer","minimum":0}}),&[])]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn actor(participant: &str) -> Actor {
        Actor {
            provider: "fixture".into(),
            subject: "account".into(),
            connection: 3,
            participant: Some(participant.into()),
        }
    }

    struct PoolFixture {
        queue: Queue<()>,
        manager: Actor,
        donor: Actor,
        idle: Actor,
        other: Actor,
        parent: Lease<()>,
    }

    impl PoolFixture {
        fn new() -> Self {
            let queue = Queue::new("test_generation".into(), 3).unwrap();
            let (manager, donor, other, idle) = (
                actor("manager"),
                actor("donor"),
                actor("other"),
                actor("idle"),
            );
            queue.remote_ready().unwrap();
            queue.attach(&manager, CLIENT, 0).unwrap();
            for worker in [&donor, &other, &idle] {
                assert!(queue.claim_remote(worker, 1).unwrap().is_none());
            }
            queue
                .submit_managed(manager.clone(), CLIENT, "original", "parent scope", (), 2)
                .unwrap();
            let parent = queue.claim_remote(&donor, 3).unwrap().unwrap();
            Self {
                queue,
                manager,
                donor,
                idle,
                other,
                parent,
            }
        }
        fn request(&self) -> Value {
            self.queue
                .share(
                    &self.idle,
                    CLIENT,
                    json!({"action":"request","request_id":"rq1","task_id":self.parent.task_id}),
                )
                .unwrap()
        }
        fn accept(&self, request: &Value) -> Value {
            self.queue
                .share(
                    &self.donor,
                    CLIENT,
                    json!({"action":"accept","share_id":request["share_id"],"text":"child scope"}),
                )
                .unwrap()
        }
        fn dispatch(&self, request: &Value, key: &str) -> Result<TaskView, String> {
            self.queue.submit_inner(
                self.manager.clone(),
                key,
                "child scope",
                (),
                4,
                Some((CLIENT, request["share_id"].as_str())),
            )
        }
    }

    #[test]
    fn share_preserves_parent_and_dispatches_only_to_authenticated_idle_requester() {
        let f = PoolFixture::new();
        assert_eq!(
            f.queue.share_candidates(&f.idle)[0]["task_id"],
            f.parent.task_id
        );
        let r = f.request();
        assert_eq!(f.request(), r);
        assert_eq!(f.queue.share_inbox(&f.donor), vec![r.clone()]);
        assert!(f.dispatch(&r, "split").is_err());
        let accepted = f.accept(&r);
        assert_eq!(f.accept(&r), accepted);
        assert!(f.queue.share_inbox(&f.donor).is_empty());
        assert_eq!(
            f.queue
                .share(&f.manager, CLIENT, json!({"action":"list"}))
                .unwrap()["requests"][0],
            accepted
        );
        let child = f.dispatch(&r, "split").unwrap();
        assert_eq!(f.dispatch(&r, "split").unwrap().task_id, child.task_id);
        assert!(f.dispatch(&r, "different-key").is_err());
        assert!(
            f.queue
                .submit_managed(f.manager.clone(), CLIENT, "split", "child scope", (), 6)
                .is_err()
        );
        assert!(f.queue.claim_remote(&f.other, 7).unwrap().is_none());
        let lease = f.queue.claim_remote(&f.idle, 7).unwrap().unwrap();
        assert_eq!(lease.task_id, child.task_id);
        assert_ne!(child.task_id, f.parent.task_id);
        assert_eq!(
            f.queue
                .result(&f.manager, &f.parent.task_id)
                .unwrap()
                .status,
            TaskStatus::Working
        );
        assert_eq!(
            f.queue
                .finish(
                    &f.parent,
                    Outcome::Completed("original result".into()),
                    true,
                    true,
                    8
                )
                .unwrap()
                .status,
            TaskStatus::Completed
        );
        assert_eq!(
            f.queue.result(&f.manager, &child.task_id).unwrap().status,
            TaskStatus::Working
        );
    }

    #[test]
    fn share_rejects_forgery_and_conflicts_and_expires_when_helper_acquires_other_work() {
        let f = PoolFixture::new();
        let r = f.request();
        let wrong = json!({"action":"accept","share_id":r["share_id"],"text":"child scope"});
        for who in [&f.other, &f.manager, &f.idle] {
            assert!(f.queue.share(who, CLIENT, wrong.clone()).is_err());
        }
        let foreign = Actor {
            subject: "different".into(),
            ..f.donor.clone()
        };
        assert!(
            f.queue
                .share(&foreign, CLIENT, json!({"action":"list"}))
                .is_err()
        );
        assert_eq!(
            f.queue
                .share(&f.other, CLIENT, json!({"action":"list"}))
                .unwrap()["requests"],
            json!([])
        );
        assert!(
            f.queue
                .share(
                    &f.donor,
                    CLIENT,
                    json!({"action":"request","request_id":"busy","task_id":f.parent.task_id})
                )
                .is_err()
        );
        assert!(
            f.queue
                .share(
                    &f.idle,
                    CLIENT,
                    json!({"action":"request","request_id":"rq1","task_id":"changed"})
                )
                .is_err()
        );
        f.accept(&r);
        assert!(
            f.queue
                .share(
                    &f.donor,
                    CLIENT,
                    json!({"action":"decline","share_id":r["share_id"]})
                )
                .is_err()
        );
        assert!(
            f.queue
                .submit_inner(
                    f.manager.clone(),
                    "split",
                    "changed child",
                    (),
                    5,
                    Some((CLIENT, r["share_id"].as_str()))
                )
                .is_err()
        );
        f.queue
            .submit_managed(f.manager.clone(), CLIENT, "urgent", "urgent", (), 5)
            .unwrap();
        f.queue.claim_remote(&f.idle, 6).unwrap().unwrap();
        assert_eq!(f.request()["status"], "expired");
        assert!(f.dispatch(&r, "split").is_err());
    }

    #[test]
    fn share_decline_and_retirement_never_move_unclaimed_work_to_a_replacement() {
        let f = PoolFixture::new();
        let r = f.request();
        let decline = json!({"action":"decline","share_id":r["share_id"]});
        assert_eq!(
            f.queue.share(&f.donor, CLIENT, decline.clone()).unwrap(),
            f.queue.share(&f.donor, CLIENT, decline).unwrap()
        );
        assert!(f.dispatch(&r, "denied").is_err());

        let r = f
            .queue
            .share(
                &f.idle,
                CLIENT,
                json!({"action":"request","task_id":f.parent.task_id,"request_id":"second"}),
            )
            .unwrap();
        f.accept(&r);
        let child = f.dispatch(&r, "split").unwrap();
        f.queue.resize_managed(&f.manager, CLIENT, 2).unwrap();
        assert_eq!(
            f.queue.result(&f.manager, &child.task_id).unwrap().status,
            TaskStatus::Cancelled
        );
        assert_eq!(
            f.queue
                .result(&f.manager, &f.parent.task_id)
                .unwrap()
                .status,
            TaskStatus::Working
        );
        assert!(f.queue.claim_remote(&f.other, 9).unwrap().is_none());
    }

    #[test]
    fn share_expired_parent_is_never_reassigned_or_reopened_on_duplicate_request() {
        let f = PoolFixture::new();
        let r = f.request();
        f.queue.cancel(&f.manager, &f.parent.task_id, 5).unwrap();
        assert_eq!(f.request()["status"], "expired");
        assert!(
            f.queue
                .share(
                    &f.donor,
                    CLIENT,
                    json!({"action":"accept","share_id":r["share_id"],"text":"child scope"})
                )
                .is_err()
        );
        assert!(f.dispatch(&r, "split").is_err());
    }
    #[test]
    fn share_unclaimed_child_is_cancelled_and_journaled_on_confirmed_station_loss() {
        for finishes_non_reusable in [false, true] {
            let f = PoolFixture::new();
            let share = f.request();
            f.accept(&share);
            if finishes_non_reusable {
                f.queue
                    .submit_managed(
                        f.manager.clone(),
                        CLIENT,
                        "first",
                        "earlier queued work",
                        (),
                        4,
                    )
                    .unwrap();
            }
            let child = f.dispatch(&share, "split").unwrap();
            if finishes_non_reusable {
                let first = f.queue.claim_remote(&f.idle, 5).unwrap().unwrap();
                assert_ne!(first.task_id, child.task_id);
                f.queue
                    .finish_remote(&first, Outcome::Unknown("connection lost".into()), false, 6)
                    .unwrap();
            } else {
                f.queue
                    .offline(
                        share["requester"]["slot"].as_u64().unwrap() as usize,
                        share["requester"]["epoch"].as_u64().unwrap(),
                        "operator confirmed station went offline",
                    )
                    .unwrap();
            }
            let cancelled = f.queue.result(&f.manager, &child.task_id).unwrap();
            assert_eq!(cancelled.status, TaskStatus::Cancelled);
            assert_eq!(cancelled.attempt, 0);
            assert_eq!(f.queue.snapshot().queued, 0);
            let events = f
                .queue
                .events(
                    &f.manager,
                    CLIENT,
                    &serde_json::from_value(json!({})).unwrap(),
                )
                .unwrap();
            let events = serde_json::to_value(events.events).unwrap();
            assert_eq!(
                events
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|event| event["task_id"] == child.task_id
                        && event["type"] == "worker.completed"
                        && event["task_status"] == "cancelled")
                    .count(),
                1
            );
            assert_eq!(
                f.queue
                    .result(&f.manager, &f.parent.task_id)
                    .unwrap()
                    .status,
                TaskStatus::Working
            );
        }
    }
}
