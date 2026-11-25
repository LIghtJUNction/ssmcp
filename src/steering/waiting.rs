//! Readiness reservations do not deliver text. Only the final HTTP response
//! commits an unchanged revision; dropping a run makes its messages eligible.
use super::*;
use tokio::sync::{OwnedSemaphorePermit, watch};

pub(super) struct Reserved {
    pub(super) message_id: String,
    revision: u64,
    run_id: String,
}

pub(crate) struct Reservation {
    queue: Queue,
    run_id: String,
    selected: Vec<(String, u64)>,
    finished: bool,
}

impl Queue {
    pub(crate) fn subscribe(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(crate) fn wait_permit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.waiters
            .clone()
            .try_acquire_owned()
            .map_err(|_| "this client already has four active user waits".into())
    }

    pub(crate) fn wake_waiters(&self) {
        self.changed.send_replace(());
    }

    pub(crate) fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .closed = true;
        self.wake_waiters();
    }

    pub(crate) fn episode(&self, client_id: &str, owner: &str, agent_id: Option<&str>) -> String {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        // Neither text nor credential material is sent to the native notifier.
        // Unrelated exact-agent traffic cannot create a new notification episode.
        let relevant: Vec<_> = state
            .messages
            .iter()
            .filter(|message| message.target == "next" || Some(message.target.as_str()) == agent_id)
            .map(|message| json!([message.id, message.revision, message.state]))
            .collect();
        crate::hex::encode(Sha256::digest(
            json!([client_id, owner, agent_id, relevant])
                .to_string()
                .as_bytes(),
        ))
    }

    pub(crate) fn reserve(
        &self,
        client_id: &str,
        run_id: &str,
        agent_id: Option<&str>,
    ) -> Result<Option<Reservation>, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err("user message connection is closed".into());
        }
        let mut selected = Vec::new();
        let mut bytes = 0usize;
        for message in &state.messages {
            if message.state != SteeringState::Queued
                || (message.target != "next" && Some(message.target.as_str()) != agent_id)
                || state
                    .reservations
                    .iter()
                    .any(|reserved| reserved.message_id == message.id)
            {
                continue;
            }
            if selected.len() == MAX_DELIVERY_MESSAGES {
                break;
            }
            let Ok(revision) = next(message.revision) else {
                break;
            };
            let size = json!({"source":"client_user","client_id":client_id,"message_id":message.id,
                "revision":revision,"text":message.text,"target":message.target,"agent_id":agent_id})
                .to_string().len();
            if bytes.saturating_add(size) > MAX_DELIVERY_BYTES {
                break;
            }
            bytes += size;
            selected.push((message.id.clone(), message.revision));
        }
        if selected.is_empty() {
            return Ok(None);
        }
        state
            .reservations
            .extend(selected.iter().map(|(message_id, revision)| Reserved {
                message_id: message_id.clone(),
                revision: *revision,
                run_id: run_id.into(),
            }));
        Ok(Some(Reservation {
            queue: self.clone(),
            run_id: run_id.into(),
            selected,
            finished: false,
        }))
    }
}

impl Reservation {
    pub(crate) fn commit(
        &mut self,
        response: super::Response<'_>,
        max_messages: usize,
        max_bytes: usize,
    ) -> Vec<Value> {
        let mut state = self
            .queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let valid = !state.closed
            && self.selected.len() <= max_messages
            && self.selected.iter().all(|(id, revision)| {
                state.reservations.iter().any(|entry| {
                    entry.message_id == *id
                        && entry.revision == *revision
                        && entry.run_id == self.run_id
                }) && state.messages.iter().any(|message| {
                    message.id == *id
                        && message.revision == *revision
                        && message.state == SteeringState::Queued
                        && (message.target == "next"
                            || Some(message.target.as_str()) == response.agent_id)
                })
            });
        let bytes = if valid {
            state
                .messages
                .iter()
                .filter(|message| self.selected.iter().any(|(id, _)| id == &message.id))
                .map(|message| {
                    json!({"source":"client_user","client_id":response.client_id,
                    "message_id":message.id,"revision":message.revision + 1,"text":message.text,
                    "target":message.target,"agent_id":response.agent_id})
                    .to_string()
                    .len()
                })
                .sum()
        } else {
            usize::MAX
        };
        state
            .reservations
            .retain(|entry| entry.run_id != self.run_id);
        let claimed = if valid && bytes <= max_bytes {
            state.claim_selected(
                response.client_id,
                &self.run_id,
                response.agent_id,
                max_messages,
                max_bytes,
                Some(&self.selected),
            )
        } else {
            Vec::new()
        };
        self.finished = true;
        drop(state);
        self.queue.wake_waiters();
        claimed
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.queue
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reservations
            .retain(|entry| entry.run_id != self.run_id);
        self.queue.wake_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(queue: &Queue, command: &str, id: &str, revision: u64, text: &str, target: &str) {
        let agents = vec![SteeringAgent {
            agent_id: "agent_a".into(),
            name: "A".into(),
            role: None,
            status: "active".into(),
            dreaming_active: None,
        }];
        queue
            .command(
                "room",
                agents,
                SteeringCommand::Publish {
                    command_id: command.into(),
                    message_id: id.into(),
                    expected_revision: revision,
                    text: text.into(),
                    target: target.into(),
                },
            )
            .unwrap();
    }

    fn response<'a>(run: &'a str, agent: Option<&'a str>) -> super::Response<'a> {
        super::Response {
            client_id: "room",
            run_id: run,
            owner: "authenticated-owner",
            agent_id: agent,
        }
    }

    #[test]
    fn user_wait_reservations_exclude_concurrent_and_ordinary_claims() {
        let queue = Queue::default();
        publish(
            &queue,
            "publish",
            "message",
            0,
            "real local request",
            "next",
        );
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut attempts = Vec::new();
        for run in ["first", "second"] {
            let (queue, barrier) = (queue.clone(), barrier.clone());
            attempts.push(std::thread::spawn(move || {
                barrier.wait();
                queue.reserve("room", run, None).unwrap()
            }));
        }
        let mut reservations: Vec<_> = attempts
            .into_iter()
            .filter_map(|attempt| attempt.join().unwrap())
            .collect();
        assert_eq!(reservations.len(), 1);
        assert!(
            queue
                .claim("room", "ordinary", None, 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
        let run = reservations[0].run_id.clone();
        let messages = reservations[0].commit(response(&run, None), 8, MAX_DELIVERY_BYTES);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["text"], "real local request");
        assert!(
            queue
                .claim("room", "third", None, 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
    }

    #[test]
    fn user_wait_drop_preserves_editable_messages_and_wakes_other_waiters() {
        let queue = Queue::default();
        publish(&queue, "publish", "message", 0, "still pending", "next");
        let reservation = queue.reserve("room", "cancelled", None).unwrap().unwrap();
        let changed = queue.subscribe();
        drop(reservation);
        assert!(changed.has_changed().unwrap());
        assert_eq!(
            queue.claim("room", "replacement", None, 8, MAX_DELIVERY_BYTES)[0]["text"],
            "still pending"
        );
    }

    #[test]
    fn user_wait_edit_supersedes_only_the_old_revision_without_consuming_new_text() {
        let queue = Queue::default();
        publish(&queue, "publish", "message", 0, "old", "next");
        let mut reservation = queue.reserve("room", "old_wait", None).unwrap().unwrap();
        queue
            .command(
                "room",
                vec![],
                SteeringCommand::Takeback {
                    command_id: "takeback".into(),
                    message_id: "message".into(),
                    expected_revision: 1,
                },
            )
            .unwrap();
        publish(&queue, "replace", "message", 2, "edited", "next");
        let replacement = queue.reserve("room", "new_wait", None).unwrap().unwrap();
        assert!(
            reservation
                .commit(response("old_wait", None), 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
        drop(replacement);
        assert_eq!(
            queue.claim("room", "new_response", None, 8, MAX_DELIVERY_BYTES)[0]["text"],
            "edited"
        );
    }

    #[test]
    fn user_wait_respects_exact_agent_target_and_response_budget() {
        let queue = Queue::default();
        publish(
            &queue,
            "private",
            "message",
            0,
            "private agent request",
            "agent_a",
        );
        assert!(queue.reserve("room", "unbound", None).unwrap().is_none());
        assert!(
            queue
                .reserve("room", "other", Some("agent_b"))
                .unwrap()
                .is_none()
        );
        let mut reservation = queue
            .reserve("room", "bound", Some("agent_a"))
            .unwrap()
            .unwrap();
        assert!(
            reservation
                .commit(response("bound", Some("agent_a")), 8, 0)
                .is_empty()
        );
        assert_eq!(
            queue
                .claim("room", "retry", Some("agent_a"), 8, MAX_DELIVERY_BYTES)
                .len(),
            1
        );
    }

    #[test]
    fn user_wait_watch_retains_publish_and_close_without_polling() {
        let queue = Queue::default();
        let mut changed = queue.subscribe();
        publish(&queue, "publish", "message", 0, "wakes waiter", "next");
        assert!(changed.has_changed().unwrap());
        drop(changed.borrow_and_update());
        let mut reservation = queue.reserve("room", "waiting", None).unwrap().unwrap();
        queue.close();
        assert!(changed.has_changed().unwrap());
        assert!(queue.reserve("room", "new_wait", None).is_err());
        assert!(
            reservation
                .commit(response("waiting", None), 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
    }

    #[test]
    fn user_wait_permits_are_bounded_and_notification_episode_is_recipient_scoped() {
        let queue = Queue::default();
        let permits: Vec<_> = (0..4).map(|_| queue.wait_permit().unwrap()).collect();
        assert!(queue.wait_permit().is_err());
        drop(permits);
        assert!(queue.wait_permit().is_ok());
        let first = queue.episode("room", "owner", None);
        assert_eq!(first, queue.episode("room", "owner", None));
        publish(
            &queue,
            "private",
            "private",
            0,
            "other recipient",
            "agent_a",
        );
        assert_eq!(first, queue.episode("room", "owner", None));
        publish(&queue, "next", "next", 0, "new local input", "next");
        assert_ne!(first, queue.episode("room", "owner", None));
        assert_ne!(first, queue.episode("room", "other-owner", None));
        assert_ne!(first, queue.episode("replacement-room", "owner", None));
    }
}
