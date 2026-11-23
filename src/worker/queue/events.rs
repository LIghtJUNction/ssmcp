//! A bounded, non-consuming journal written while the queue state is committed.
//! Event payloads contain lifecycle metadata only; task and command bodies stay
//! behind their existing APIs.
use super::*;

const MAX_EVENTS: usize = 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(in crate::worker) struct EventCursor {
    pub(in crate::worker) generation: String,
    pub(in crate::worker) seq: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::worker) struct EventRequest {
    #[serde(default)]
    pub(in crate::worker) after: Option<EventCursor>,
    #[serde(default = "default_limit")]
    pub(in crate::worker) limit: usize,
    #[serde(default)]
    pub(in crate::worker) wait_ms: u64,
}

fn default_limit() -> usize {
    32
}

impl EventRequest {
    pub(in crate::worker) fn validate(&self) -> Result<(), String> {
        if !(1..=128).contains(&self.limit) {
            return Err("worker events limit must be from 1 through 128".into());
        }
        if self.wait_ms > crate::worker::MAX_OBSERVATION_WAIT_MS {
            return Err(format!(
                "worker events wait_ms must be at most {}",
                crate::worker::MAX_OBSERVATION_WAIT_MS
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub(in crate::worker) struct Event {
    seq: u64,
    generation: String,
    #[serde(rename = "type")]
    kind: &'static str,
    at_ms: u64,
    task_id: Option<String>,
    slot: Option<usize>,
    epoch: Option<u64>,
    attempt: Option<u64>,
    participant_id: Option<String>,
    task_status: Option<TaskStatus>,
    activity: Option<ReportEvent>,
}

#[derive(Default)]
pub(super) struct Journal {
    // Keep the account boundary after an unbound idle pool clears registrations.
    owner: Option<Actor>,
    sequence: u64,
    entries: VecDeque<Event>,
}

impl Journal {
    fn push(&mut self, mut event: Event) {
        // An exhausted sequence makes reads fail instead of reusing event IDs
        // or claiming that an incomplete history is complete.
        let Some(seq) = self.sequence.checked_add(1) else {
            return;
        };
        self.sequence = seq;
        event.seq = seq;
        self.entries.push_back(event);
        while self.entries.len() > MAX_EVENTS {
            self.entries.pop_front();
        }
    }
}

#[derive(Debug, Serialize)]
pub(in crate::worker) struct EventPage {
    generation: String,
    pub(in crate::worker) events: Vec<Event>,
    next_cursor: EventCursor,
    oldest_seq: u64,
    latest_seq: u64,
    pub(in crate::worker) gap: bool,
    has_more: bool,
    pub(in crate::worker) closed: bool,
    stopping: bool,
}

impl<T: Clone> Queue<T> {
    /// Reading lifecycle metadata grants no task, manager, or station authority.
    pub(in crate::worker) fn events(
        &self,
        actor: &Actor,
        client_id: &str,
        request: &EventRequest,
    ) -> Result<EventPage, String> {
        request.validate()?;
        let _ = Manager::new(actor, client_id, 0)?;
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let owner = state
            .manager
            .as_ref()
            .map(|manager| &manager.actor)
            .or_else(|| state.tasks.values().next().map(|task| &task.actor))
            .or_else(|| state.slots.iter().find_map(|slot| slot.registered.as_ref()))
            .or(state.events.owner.as_ref());
        if owner.is_some_and(|owner| !lifecycle::same_account(owner, actor)) {
            return Err(
                "worker events belong to a different authenticated account or connection".into(),
            );
        }
        let journal = &state.events;
        if journal.sequence == u64::MAX {
            return Err("worker event sequence exhausted; reconnect for a new generation".into());
        }
        let oldest_seq = journal
            .entries
            .front()
            .map_or(journal.sequence + 1, |event| event.seq);
        let after = match &request.after {
            Some(cursor) => {
                if cursor.generation != self.generation {
                    return Err("worker event cursor belongs to a different pool generation".into());
                }
                if cursor.seq > journal.sequence {
                    return Err("worker event cursor is ahead of the current sequence".into());
                }
                cursor.seq
            }
            None => oldest_seq.saturating_sub(1),
        };
        let gap = request.after.is_some() && after < oldest_seq.saturating_sub(1);
        let events: Vec<_> = journal
            .entries
            .iter()
            .filter(|event| event.seq > after)
            .take(request.limit)
            .cloned()
            .collect();
        let next = events.last().map_or(after, |event| event.seq);
        Ok(EventPage {
            generation: self.generation.clone(),
            events,
            next_cursor: EventCursor {
                generation: self.generation.clone(),
                seq: next,
            },
            oldest_seq,
            latest_seq: journal.sequence,
            gap,
            has_more: next < journal.sequence,
            closed: state.closed,
            stopping: state.stopping,
        })
    }

    pub(super) fn record_task_event(
        &self,
        state: &mut State<T>,
        kind: &'static str,
        task_id: &str,
        activity: Option<ReportEvent>,
        now_ms: u64,
    ) {
        let task = state
            .tasks
            .get(task_id)
            .expect("committed worker event task");
        state.events.owner.get_or_insert_with(|| task.actor.clone());
        state.events.push(Event {
            seq: 0,
            generation: self.generation.clone(),
            kind,
            at_ms: now_ms,
            task_id: Some(task_id.into()),
            slot: task.view.slot,
            epoch: task.claim_epoch,
            attempt: Some(task.view.attempt),
            participant_id: task.claimed_by.clone(),
            task_status: Some(task.view.status),
            activity,
        });
    }

    pub(super) fn record_slot_event(
        &self,
        state: &mut State<T>,
        kind: &'static str,
        slot: usize,
        now_ms: u64,
    ) {
        let worker = &state.slots[slot];
        if state.events.owner.is_none() {
            state.events.owner = worker.registered.clone();
        }
        state.events.push(Event {
            seq: 0,
            generation: self.generation.clone(),
            kind,
            at_ms: now_ms,
            task_id: None,
            slot: Some(slot),
            epoch: Some(worker.epoch),
            attempt: None,
            participant_id: worker
                .registered
                .as_ref()
                .and_then(|actor| actor.participant.clone()),
            task_status: None,
            activity: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(participant: &str) -> Actor {
        Actor {
            provider: "github".into(),
            subject: "42".into(),
            participant: Some(participant.into()),
            connection: 1,
        }
    }

    #[test]
    fn committed_retirement_and_release_are_ordered_without_new_work() {
        let queue = Queue::<()>::new("generation".into(), 1).unwrap();
        let manager = actor("manager");
        let worker = actor("worker");
        let client = "a".repeat(64);
        queue.remote_ready().unwrap();
        queue.attach(&manager, &client, 1).unwrap();
        assert!(queue.claim_remote(&worker, 2).unwrap().is_none());
        queue.resize_managed(&manager, &client, 0).unwrap();
        assert!(queue.claim_remote(&worker, 3).unwrap().is_none());
        let retirement = queue.retirement(&worker).unwrap();
        queue.release_remote(&worker, &retirement, 4).unwrap();
        assert!(queue.claim_remote(&worker, 5).unwrap().is_none());
        let page = queue
            .events(
                &manager,
                &client,
                &EventRequest {
                    after: None,
                    limit: 128,
                    wait_ms: 0,
                },
            )
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec!["worker.idle", "worker.retiring", "worker.offline"]
        );
        assert!(page.events.iter().all(|event| event.task_id.is_none()
            && event.task_status.is_none()
            && event.participant_id.as_deref() == Some("worker")));
        let repeated = queue
            .events(
                &manager,
                &client,
                &EventRequest {
                    after: None,
                    limit: 128,
                    wait_ms: 0,
                },
            )
            .unwrap();
        assert_eq!(
            serde_json::to_value(&page).unwrap(),
            serde_json::to_value(&repeated).unwrap()
        );
        assert!(queue.state.lock().unwrap().tasks.is_empty());
    }

    #[test]
    fn repeated_offline_status_does_not_forge_another_transition() {
        let queue = Queue::<()>::new("generation".into(), 1).unwrap();
        let manager = actor("manager");
        let client = "a".repeat(64);
        queue.attach(&manager, &client, 1).unwrap();
        let epoch = queue.online(0).unwrap();
        queue.offline(0, epoch, "transport closed").unwrap();
        queue.offline(0, epoch, "same closed transport").unwrap();
        let page = queue
            .events(
                &manager,
                &client,
                &EventRequest {
                    after: None,
                    limit: 128,
                    wait_ms: 0,
                },
            )
            .unwrap();
        assert_eq!(
            page.events
                .iter()
                .map(|event| event.kind)
                .collect::<Vec<_>>(),
            vec!["worker.idle", "worker.offline"]
        );
        assert!(
            queue
                .events(
                    &manager,
                    &client,
                    &EventRequest {
                        after: Some(EventCursor {
                            generation: "replacement_generation".into(),
                            seq: 0
                        }),
                        limit: 128,
                        wait_ms: 0,
                    }
                )
                .is_err()
        );
    }
}
