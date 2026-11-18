//! Local operator messages, atomically editable until claimed by an HTTP reply.
use crate::wire::{
    MAX_USER_MESSAGE_BYTES, MAX_USER_MESSAGES, SteeringAgent, SteeringCommand, SteeringDelivery,
    SteeringMessage, SteeringSnapshot, SteeringState,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_HISTORY: usize = 64;
const MAX_RETAINED_BYTES: usize = 512 * 1024;
const MAX_COMMANDS: usize = 128;
const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;
pub(crate) const MAX_DELIVERY_MESSAGES: usize = 8;
pub(crate) const MAX_DELIVERY_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct Queue {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    revision: u64,
    messages: VecDeque<SteeringMessage>,
    commands: VecDeque<(String, [u8; 32])>,
}

fn pending(message: &SteeringMessage) -> bool {
    matches!(
        message.state,
        SteeringState::Queued | SteeringState::Editing
    )
}

fn cost(message: &SteeringMessage) -> usize {
    // Reserve metadata/delivery overhead now, so marking delivery cannot exceed
    // the queue budget or force eviction of an undelivered message.
    serde_json::to_string(&message.text).map_or(usize::MAX, |text| text.len().saturating_add(1024))
}

fn identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(format!(
            "{field} must contain 1..=128 ASCII letters, digits, underscores or hyphens"
        ));
    }
    Ok(())
}

fn next(value: u64) -> Result<u64, String> {
    value
        .checked_add(1)
        .filter(|value| *value <= MAX_SEQUENCE)
        .ok_or_else(|| "user message revision exhausted".into())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

impl State {
    fn trim(&mut self) {
        while self
            .messages
            .iter()
            .filter(|message| !pending(message))
            .count()
            > MAX_HISTORY
            || self.messages.iter().map(cost).sum::<usize>() > MAX_RETAINED_BYTES
        {
            let Some(index) = self.messages.iter().position(|message| !pending(message)) else {
                break;
            };
            self.messages.remove(index);
        }
    }

    fn snapshot(&self, client_id: &str, agents: Vec<SteeringAgent>) -> SteeringSnapshot {
        SteeringSnapshot {
            schema_version: 1,
            client_id: client_id.into(),
            revision: self.revision,
            agents,
            messages: self.messages.iter().cloned().collect(),
        }
    }
}

impl Queue {
    pub(crate) fn command(
        &self,
        client_id: &str,
        agents: Vec<SteeringAgent>,
        command: SteeringCommand,
    ) -> Result<SteeringSnapshot, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if matches!(command, SteeringCommand::Snapshot {}) {
            return Ok(state.snapshot(client_id, agents));
        }
        let (command_id, message_id, expected_revision) = match &command {
            SteeringCommand::Publish {
                command_id,
                message_id,
                expected_revision,
                ..
            }
            | SteeringCommand::Takeback {
                command_id,
                message_id,
                expected_revision,
            }
            | SteeringCommand::Cancel {
                command_id,
                message_id,
                expected_revision,
            } => (command_id, message_id, *expected_revision),
            SteeringCommand::Snapshot {} => unreachable!(),
        };
        identifier(command_id, "command_id")?;
        identifier(message_id, "message_id")?;
        let fingerprint: [u8; 32] =
            Sha256::digest(serde_json::to_vec(&command).map_err(|error| error.to_string())?).into();
        if let Some((_, previous)) = state.commands.iter().find(|(id, _)| id == command_id) {
            if previous != &fingerprint {
                return Err(
                    "command_id was already used for different user message content".into(),
                );
            }
            // A retry must show any delivery that happened since the first
            // command; returning an obsolete queued snapshot would be unsafe.
            return Ok(state.snapshot(client_id, agents));
        }
        let command_id = command_id.clone();
        let index = state
            .messages
            .iter()
            .position(|message| message.id == *message_id);
        if let Some(index) = index {
            let message = &state.messages[index];
            if message.state == SteeringState::Delivered {
                return Err(format!(
                    "already_delivered: message {} revision {} is immutable; send a new message",
                    message.id, message.revision
                ));
            }
            if message.state == SteeringState::Cancelled {
                return Err("message was cancelled; publish with a new message_id".into());
            }
            if message.revision != expected_revision {
                return Err(format!(
                    "revision_conflict: expected {expected_revision}, current {}; refresh the queue",
                    message.revision
                ));
            }
        } else if expected_revision != 0 || !matches!(command, SteeringCommand::Publish { .. }) {
            return Err(
                "message is not in this session's retained queue; refresh before retrying".into(),
            );
        }
        let revision = next(expected_revision)?;
        let queue_revision = next(state.revision)?;
        let now = now_ms();
        match command {
            SteeringCommand::Publish {
                message_id,
                text,
                target,
                ..
            } => {
                if text.trim().is_empty()
                    || text.len() > MAX_USER_MESSAGE_BYTES
                    || text
                        .chars()
                        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
                {
                    return Err("user message must contain 1..=16384 UTF-8 bytes of text; only newline and tab control characters are allowed".into());
                }
                if target != "next" && !agents.iter().any(|agent| agent.agent_id == target) {
                    return Err(
                        "target agent is unavailable; select an active agent or next".into(),
                    );
                }
                let message = SteeringMessage {
                    id: message_id,
                    revision,
                    text,
                    target,
                    state: SteeringState::Queued,
                    created_at_ms: index.map_or(now, |index| state.messages[index].created_at_ms),
                    updated_at_ms: now,
                    delivery: None,
                };
                let pending_count = state
                    .messages
                    .iter()
                    .filter(|message| pending(message))
                    .count();
                let retained_pending: usize = state
                    .messages
                    .iter()
                    .enumerate()
                    .filter(|(at, message)| Some(*at) != index && pending(message))
                    .map(|(_, message)| cost(message))
                    .sum();
                if (index.is_none() && pending_count >= MAX_USER_MESSAGES)
                    || retained_pending.saturating_add(cost(&message)) > MAX_RETAINED_BYTES
                {
                    return Err("user message queue is full; take back or cancel a pending message before publishing another".into());
                }
                if let Some(index) = index {
                    state.messages[index] = message;
                } else {
                    state.messages.push_back(message);
                }
            }
            SteeringCommand::Takeback { .. } | SteeringCommand::Cancel { .. } => {
                let message = &mut state.messages[index.expect("validated message")];
                message.state = if matches!(command, SteeringCommand::Takeback { .. }) {
                    SteeringState::Editing
                } else {
                    SteeringState::Cancelled
                };
                message.revision = revision;
                message.updated_at_ms = now;
            }
            SteeringCommand::Snapshot {} => unreachable!(),
        }
        state.revision = queue_revision;
        state.commands.push_back((command_id, fingerprint));
        if state.commands.len() > MAX_COMMANDS {
            state.commands.pop_front();
        }
        state.trim();
        Ok(state.snapshot(client_id, agents))
    }

    /// Claim and CAS edits share one lock. Once text is returned here no later
    /// successful takeback can claim to have withdrawn that same revision.
    pub(crate) fn claim(
        &self,
        client_id: &str,
        run_id: &str,
        agent_id: Option<&str>,
        max_messages: usize,
        max_bytes: usize,
    ) -> Vec<Value> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut claimed = Vec::new();
        let mut used = 0usize;
        for message in &mut state.messages {
            if message.state != SteeringState::Queued
                || (message.target != "next" && Some(message.target.as_str()) != agent_id)
            {
                continue;
            }
            if claimed.len() >= max_messages {
                break;
            }
            let Ok(revision) = next(message.revision) else {
                break;
            };
            let value = json!({"source":"client_user","client_id":client_id,"message_id":message.id,
                "revision":revision,"text":message.text,"target":message.target,"agent_id":agent_id});
            let bytes = value.to_string().len();
            if used.saturating_add(bytes) > max_bytes {
                break;
            }
            used += bytes;
            message.state = SteeringState::Delivered;
            message.revision = revision;
            message.updated_at_ms = now_ms();
            message.delivery = Some(SteeringDelivery {
                run_id: run_id.into(),
                agent_id: agent_id.map(str::to_owned),
                included_at_ms: message.updated_at_ms,
            });
            claimed.push(value);
        }
        if !claimed.is_empty() {
            state.revision = state.revision.saturating_add(1).min(MAX_SEQUENCE);
            state.trim();
        }
        claimed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn publish(
        command: &str,
        id: &str,
        revision: u64,
        text: &str,
        target: &str,
    ) -> SteeringCommand {
        SteeringCommand::Publish {
            command_id: command.into(),
            message_id: id.into(),
            expected_revision: revision,
            text: text.into(),
            target: target.into(),
        }
    }
    fn snapshot(queue: &Queue) -> SteeringSnapshot {
        queue
            .command("room", vec![], SteeringCommand::Snapshot {})
            .unwrap()
    }

    #[test]
    fn successful_takeback_prevents_old_text_delivery_and_publish_requires_current_revision() {
        let queue = Queue::default();
        queue
            .command("room", vec![], publish("one", "m", 0, "old text", "next"))
            .unwrap();
        queue
            .command(
                "room",
                vec![],
                SteeringCommand::Takeback {
                    command_id: "two".into(),
                    message_id: "m".into(),
                    expected_revision: 1,
                },
            )
            .unwrap();
        assert!(
            queue
                .claim("room", "run", None, 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
        assert!(
            queue
                .command("room", vec![], publish("three", "m", 1, "stale", "next"))
                .is_err()
        );
        queue
            .command(
                "room",
                vec![],
                publish("four", "m", 2, "edited text", "next"),
            )
            .unwrap();
        let delivered = queue.claim("room", "run", None, 8, MAX_DELIVERY_BYTES);
        assert_eq!(delivered[0]["text"], "edited text");
        assert_eq!(snapshot(&queue).messages[0].state, SteeringState::Delivered);
        assert!(
            queue
                .command(
                    "room",
                    vec![],
                    SteeringCommand::Takeback {
                        command_id: "five".into(),
                        message_id: "m".into(),
                        expected_revision: 3
                    }
                )
                .unwrap_err()
                .contains("already_delivered")
        );
        assert!(
            queue
                .claim("room", "run2", None, 8, MAX_DELIVERY_BYTES)
                .is_empty()
        );
    }

    #[test]
    fn exact_agent_targets_and_retries_preserve_fifo_and_do_not_restore_delivered_records() {
        let queue = Queue::default();
        let agents = vec![SteeringAgent {
            agent_id: "agent_a".into(),
            name: "same name".into(),
            role: None,
            status: "available".into(),
        }];
        let first = publish("one", "a", 0, "private", "agent_a");
        queue
            .command("room", agents.clone(), first.clone())
            .unwrap();
        queue
            .command(
                "room",
                agents.clone(),
                publish("two", "b", 0, "next", "next"),
            )
            .unwrap();
        assert_eq!(
            queue.claim("room", "run", None, 8, MAX_DELIVERY_BYTES)[0]["message_id"],
            "b"
        );
        assert_eq!(
            queue.claim("room", "run", Some("agent_a"), 8, MAX_DELIVERY_BYTES)[0]["message_id"],
            "a"
        );
        let retry = queue.command("room", agents.clone(), first).unwrap();
        assert!(
            retry
                .messages
                .iter()
                .all(|message| message.state == SteeringState::Delivered)
        );
        assert!(
            queue
                .command(
                    "room",
                    agents,
                    publish("one", "a", 0, "different", "agent_a")
                )
                .unwrap_err()
                .contains("command_id")
        );
    }

    #[test]
    fn capacity_rejects_new_pending_messages_without_losing_old_ones() {
        let queue = Queue::default();
        for index in 0..MAX_USER_MESSAGES {
            queue
                .command(
                    "room",
                    vec![],
                    publish(
                        &format!("c{index}"),
                        &format!("m{index}"),
                        0,
                        "pending",
                        "next",
                    ),
                )
                .unwrap();
        }
        assert!(
            queue
                .command(
                    "room",
                    vec![],
                    publish("overflow", "overflow", 0, "pending", "next")
                )
                .is_err()
        );
        assert_eq!(snapshot(&queue).messages.len(), MAX_USER_MESSAGES);
        assert!(
            queue
                .command(
                    "room",
                    vec![],
                    publish(
                        "huge",
                        "m0",
                        1,
                        &"x".repeat(MAX_USER_MESSAGE_BYTES + 1),
                        "next"
                    )
                )
                .is_err()
        );
        assert_eq!(snapshot(&queue).messages[0].text, "pending");
    }
}
