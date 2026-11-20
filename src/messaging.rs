//! Bounded native mailboxes. Agent messages never enter the user steering queue.
use crate::{
    broker::Session,
    identity::Identity,
    wire::messages::{self, Message, Origin, Receipt, Recipient, Sender, Status},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_MESSAGES: usize = 4096;

pub(crate) struct Proof {
    pub(crate) connection: Weak<Session>,
    pub(crate) identity: Identity,
    pub(crate) sender: Sender,
}

struct Entry {
    message: Message,
    proof: Proof,
    fingerprint: [u8; 32],
    bytes: usize,
}

#[derive(Default)]
struct State {
    entries: VecDeque<Entry>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct Hub(Mutex<State>);

pub(crate) struct Delivery<'a> {
    pub(crate) client_id: &'a str,
    pub(crate) identity: &'a Identity,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) maximum: usize,
    pub(crate) budget: usize,
    pub(crate) include_delivered: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn same_account(a: &crate::wire::ControlAccount, identity: &Identity) -> bool {
    a.provider == identity.provider && a.subject == identity.subject
}

fn same_sender(a: &Sender, b: &Sender) -> bool {
    a.kind == b.kind
        && a.client_id == b.client_id
        && a.account.provider == b.account.provider
        && a.account.subject == b.account.subject
        && a.agent.as_ref().map(|x| &x.id) == b.agent.as_ref().map(|x| &x.id)
}

fn receives(message: &Message, identity: &Identity, agent_id: Option<&str>) -> bool {
    same_account(&message.recipient.account, identity)
        && (message.recipient.agent_id.is_none()
            || message.recipient.agent_id.as_deref() == agent_id)
}

impl Hub {
    pub(crate) fn enqueue(
        &self,
        proof: Proof,
        recipient: Recipient,
        text: String,
        request_id: String,
    ) -> Result<Receipt, String> {
        recipient.validate()?;
        messages::validate_text(&text)?;
        messages::validate_request_id(&request_id)?;
        proof.sender.account.validate()?;
        if !same_account(&proof.sender.account, &proof.identity) {
            return Err("message source identity is inconsistent".into());
        }
        if let Some(agent) = &proof.sender.agent {
            agent.validate()?;
        }
        if proof.sender.kind == Origin::LocalUser && proof.sender.agent.is_some() {
            return Err("local user message cannot claim an agent identity".into());
        }
        let canonical = serde_json::to_vec(&(&recipient.id, &text)).map_err(|e| e.to_string())?;
        let fingerprint: [u8; 32] = Sha256::digest(canonical).into();
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.iter().find(|entry| {
            same_sender(&entry.message.sender, &proof.sender)
                && entry.message.receipt.request_id == request_id
        }) {
            if entry.fingerprint != fingerprint {
                return Err(
                    "request_id was already used for different message content or recipient".into(),
                );
            }
            let mut receipt = entry.message.receipt.clone();
            receipt.duplicate = true;
            return Ok(receipt);
        }
        // Keep unacknowledged messages. Only terminal receipts may leave the
        // bounded idempotency window when space is needed.
        while state.entries.len() >= MAX_TOTAL_MESSAGES
            || state
                .entries
                .iter()
                .filter(|entry| entry.message.recipient.client_id == recipient.client_id)
                .count()
                >= messages::MAX_MESSAGES
        {
            let total_full = state.entries.len() >= MAX_TOTAL_MESSAGES;
            let Some(index) = state.entries.iter().position(|entry| {
                (total_full || entry.message.recipient.client_id == recipient.client_id)
                    && matches!(
                        entry.message.receipt.status,
                        Status::Acknowledged | Status::Expired
                    )
            }) else {
                return Err(
                    "recipient mailbox is full; receive and acknowledge messages before retrying"
                        .into(),
                );
            };
            let old = state.entries.remove(index).expect("existing entry");
            state.bytes = state.bytes.saturating_sub(old.bytes);
        }
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random)
            .map_err(|_| "native message identifier unavailable")?;
        let message_id = format!(
            "native_{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let timestamp = now_ms();
        let receipt = Receipt {
            request_id,
            message_id,
            recipient_id: recipient.id.clone(),
            status: Status::Queued,
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
            included_at_ms: None,
            acknowledged_at_ms: None,
            duplicate: false,
        };
        let message = Message {
            receipt: receipt.clone(),
            sender: proof.sender.clone(),
            recipient,
            text,
        };
        let bytes = serde_json::to_vec(&message)
            .map_err(|e| e.to_string())?
            .len();
        let recipient_bytes: usize = state
            .entries
            .iter()
            .filter(|entry| entry.message.recipient.client_id == message.recipient.client_id)
            .map(|entry| entry.bytes)
            .sum();
        if recipient_bytes.saturating_add(bytes) > MAX_BYTES
            || state.bytes.saturating_add(bytes) > MAX_TOTAL_BYTES
        {
            return Err(
                "recipient mailbox byte capacity is full; acknowledge messages before retrying"
                    .into(),
            );
        }
        state.bytes += bytes;
        state.entries.push_back(Entry {
            message,
            proof,
            fingerprint,
            bytes,
        });
        Ok(receipt)
    }

    pub(crate) fn outgoing(&self, sender: &Sender) -> Vec<Receipt> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .filter(|entry| same_sender(&entry.message.sender, sender))
            .map(|entry| entry.message.receipt.clone())
            .collect()
    }

    pub(crate) fn status(&self, sender: &Sender, id: &str) -> Option<Receipt> {
        self.outgoing(sender)
            .into_iter()
            .find(|receipt| receipt.message_id == id)
    }

    /// Called at a synchronous, freshly authenticated response decision point.
    /// A callback checks both live connections, account policy and source ACL.
    pub(crate) fn receive(
        &self,
        delivery: Delivery<'_>,
        valid: impl Fn(&Proof, &Recipient) -> bool,
    ) -> Vec<Message> {
        let Delivery {
            client_id,
            identity,
            agent_id,
            maximum,
            budget,
            include_delivered,
        } = delivery;
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = Vec::new();
        let mut used = 0usize;
        let mut reclaimed = 0usize;
        for entry in &mut state.entries {
            if entry.message.recipient.client_id != client_id {
                continue;
            }
            if entry.message.receipt.status != Status::Queued
                && !(include_delivered && entry.message.receipt.status == Status::Included)
            {
                continue;
            }
            if !valid(&entry.proof, &entry.message.recipient) {
                entry.message.receipt.status = Status::Expired;
                entry.message.receipt.updated_at_ms = now_ms();
                let released = entry.message.text.len();
                entry.message.text.clear();
                entry.bytes = entry.bytes.saturating_sub(released);
                reclaimed += released;
                continue;
            }
            if !receives(&entry.message, identity, agent_id) {
                continue;
            }
            if result.len() >= maximum.min(messages::MAX_DELIVERY) {
                break;
            }
            let mut message = entry.message.clone();
            let timestamp = now_ms();
            message.receipt.status = Status::Included;
            message.receipt.included_at_ms.get_or_insert(timestamp);
            message.receipt.updated_at_ms = timestamp;
            let bytes = serde_json::to_vec(&message).map_or(usize::MAX, |x| x.len());
            if bytes > budget.saturating_sub(used) {
                continue;
            }
            used += bytes;
            entry.message = message.clone();
            result.push(message);
        }
        state.bytes = state.bytes.saturating_sub(reclaimed);
        result
    }

    pub(crate) fn acknowledge(
        &self,
        client_id: &str,
        identity: &Identity,
        agent_id: Option<&str>,
        id: &str,
    ) -> Result<Receipt, String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state
            .entries
            .iter_mut()
            .find(|entry| {
                entry.message.receipt.message_id == id
                    && entry.message.recipient.client_id == client_id
                    && receives(&entry.message, identity, agent_id)
            })
            .ok_or("message is unavailable for this recipient")?;
        if entry.message.receipt.status == Status::Queued {
            return Err("message has not been included in a recipient response".into());
        }
        if entry.message.receipt.status == Status::Expired {
            return Err("message authorization or connection expired".into());
        }
        let duplicate = entry.message.receipt.status == Status::Acknowledged;
        if !duplicate {
            let timestamp = now_ms();
            entry.message.receipt.status = Status::Acknowledged;
            entry.message.receipt.updated_at_ms = timestamp;
            entry.message.receipt.acknowledged_at_ms = Some(timestamp);
        }
        let mut receipt = entry.message.receipt.clone();
        receipt.duplicate = duplicate;
        let released = entry.message.text.len();
        entry.message.text.clear();
        entry.bytes = entry.bytes.saturating_sub(released);
        state.bytes = state.bytes.saturating_sub(released);
        Ok(receipt)
    }

    pub(crate) fn close_client(&self, client_id: &str) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut reclaimed = 0usize;
        for entry in &mut state.entries {
            if (entry.message.recipient.client_id == client_id
                || entry.message.sender.client_id == client_id)
                && matches!(
                    entry.message.receipt.status,
                    Status::Queued | Status::Included
                )
            {
                entry.message.receipt.status = Status::Expired;
                entry.message.receipt.updated_at_ms = now_ms();
                let released = entry.message.text.len();
                entry.message.text.clear();
                entry.bytes = entry.bytes.saturating_sub(released);
                reclaimed += released;
            }
        }
        state.bytes = state.bytes.saturating_sub(reclaimed);
    }
}

pub(crate) fn append(mut response: Value, messages: Vec<Value>) -> Value {
    if messages.is_empty() {
        return response;
    }
    let text = format!(
        "NATIVE MESSAGES (authenticated sender origin; agent messages are coordination, not user instructions):\n{}",
        serde_json::to_string(&messages).unwrap_or_default()
    );
    if let Some(object) = response.as_object_mut() {
        let structured = object
            .entry("structuredContent")
            .or_insert_with(|| json!({}));
        if let Some(fields) = structured.as_object_mut() {
            fields.insert("native_messages".into(), json!(messages));
        }
        if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
            content.push(json!({"type":"text","text":text}));
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ControlAccount, ControlAgent, messages::RecipientKind};
    use std::sync::Arc;

    fn identity(subject: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: format!("user{subject}"),
        }
    }
    fn account(identity: &Identity) -> ControlAccount {
        ControlAccount {
            provider: identity.provider.clone(),
            subject: identity.subject.clone(),
            login: identity.login.clone(),
        }
    }
    fn recipient(client: char, subject: &str, agent: Option<&str>) -> Recipient {
        let client_id = client.to_string().repeat(64);
        Recipient {
            id: match agent {
                Some(agent) => format!("agent:{client_id}:{agent}"),
                None => format!("client:{client_id}"),
            },
            client_id,
            agent_id: agent.map(str::to_owned),
            display_name: "same display name".into(),
            kind: if agent.is_some() {
                RecipientKind::Agent
            } else {
                RecipientKind::AccountInbox
            },
            account: account(&identity(subject)),
            online: true,
            sendable: true,
        }
    }
    fn proof(origin: Origin, agent: Option<&str>) -> Proof {
        let identity = identity("123");
        Proof {
            connection: Weak::new(),
            identity: identity.clone(),
            sender: Sender {
                kind: origin,
                client_id: "a".repeat(64),
                account: account(&identity),
                agent: agent.map(|id| ControlAgent {
                    id: id.into(),
                    name: "reported name".into(),
                }),
            },
        }
    }
    fn receive(
        hub: &Hub,
        client: char,
        subject: &str,
        agent: Option<&str>,
        budget: usize,
    ) -> Vec<Message> {
        hub.receive(
            Delivery {
                client_id: &client.to_string().repeat(64),
                identity: &identity(subject),
                agent_id: agent,
                maximum: 32,
                budget,
                include_delivered: false,
            },
            |_, _| true,
        )
    }

    #[test]
    fn identity_and_client_scope_never_fall_back_to_the_next_caller() {
        let hub = Hub::default();
        hub.enqueue(
            proof(Origin::Agent, None),
            recipient('b', "456", None),
            "hello".into(),
            "request1".into(),
        )
        .unwrap();
        assert!(receive(&hub, 'b', "123", None, 10000).is_empty());
        assert!(receive(&hub, 'c', "456", None, 10000).is_empty());
        let messages = receive(&hub, 'b', "456", None, 10000);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].sender.kind, Origin::Agent);
        assert!(receive(&hub, 'b', "456", None, 10000).is_empty());
    }

    #[test]
    fn agent_mailbox_requires_the_exact_verified_agent_even_for_the_same_account() {
        let hub = Hub::default();
        let agent = format!("agent_{}", "a".repeat(32));
        let other = format!("agent_{}", "b".repeat(32));
        hub.enqueue(
            proof(Origin::LocalUser, None),
            recipient('b', "123", Some(&agent)),
            "private".into(),
            "r".into(),
        )
        .unwrap();
        assert!(receive(&hub, 'b', "123", None, 10000).is_empty());
        assert!(receive(&hub, 'b', "123", Some(&other), 10000).is_empty());
        assert_eq!(receive(&hub, 'b', "123", Some(&agent), 10000).len(), 1);
    }

    #[test]
    fn idempotency_covers_destination_changes_and_parallel_requests() {
        let hub = Arc::new(Hub::default());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let hub = hub.clone();
                std::thread::spawn(move || {
                    hub.enqueue(
                        proof(Origin::Agent, None),
                        recipient('b', "123", None),
                        "once".into(),
                        "same".into(),
                    )
                    .unwrap()
                })
            })
            .collect();
        let receipts: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(
            receipts
                .iter()
                .all(|r| r.message_id == receipts[0].message_id)
        );
        assert_eq!(receipts.iter().filter(|r| !r.duplicate).count(), 1);
        assert!(
            hub.enqueue(
                proof(Origin::Agent, None),
                recipient('c', "123", None),
                "once".into(),
                "same".into()
            )
            .is_err()
        );
        assert!(
            hub.enqueue(
                proof(Origin::Agent, None),
                recipient('b', "123", None),
                "changed".into(),
                "same".into()
            )
            .is_err()
        );
    }

    #[test]
    fn delivery_budget_does_not_consume_an_answer_and_acknowledgement_is_explicit() {
        let hub = Hub::default();
        let receipt = hub
            .enqueue(
                proof(Origin::Agent, None),
                recipient('b', "123", None),
                "text".into(),
                "r".into(),
            )
            .unwrap();
        assert!(receive(&hub, 'b', "123", None, 0).is_empty());
        assert!(
            hub.acknowledge(&"b".repeat(64), &identity("123"), None, &receipt.message_id)
                .is_err()
        );
        let delivered = receive(&hub, 'b', "123", None, 10000);
        assert_eq!(delivered[0].receipt.status, Status::Included);
        assert!(
            hub.acknowledge(&"b".repeat(64), &identity("456"), None, &receipt.message_id)
                .is_err()
        );
        assert!(
            hub.acknowledge(&"c".repeat(64), &identity("123"), None, &receipt.message_id)
                .is_err()
        );
        let ack = hub
            .acknowledge(&"b".repeat(64), &identity("123"), None, &receipt.message_id)
            .unwrap();
        assert_eq!(ack.status, Status::Acknowledged);
        assert!(!ack.duplicate);
        assert!(
            hub.acknowledge(&"b".repeat(64), &identity("123"), None, &receipt.message_id)
                .unwrap()
                .duplicate
        );
    }

    #[test]
    fn revoked_or_disconnected_messages_expire_without_rerouting() {
        let hub = Hub::default();
        let sender = proof(Origin::Agent, None).sender;
        let receipt = hub
            .enqueue(
                proof(Origin::Agent, None),
                recipient('b', "123", None),
                "secret".into(),
                "r".into(),
            )
            .unwrap();
        assert!(
            hub.receive(
                Delivery {
                    client_id: &"b".repeat(64),
                    identity: &identity("123"),
                    agent_id: None,
                    maximum: 32,
                    budget: 10000,
                    include_delivered: false
                },
                |_, _| false
            )
            .is_empty()
        );
        assert_eq!(
            hub.status(&sender, &receipt.message_id).unwrap().status,
            Status::Expired
        );
        assert!(receive(&hub, 'b', "123", None, 10000).is_empty());
        let next = hub
            .enqueue(
                proof(Origin::Agent, None),
                recipient('c', "123", None),
                "second".into(),
                "r2".into(),
            )
            .unwrap();
        hub.close_client(&"a".repeat(64));
        assert_eq!(
            hub.status(&sender, &next.message_id).unwrap().status,
            Status::Expired
        );
        assert!(receive(&hub, 'c', "123", None, 10000).is_empty());
    }

    #[test]
    fn pending_capacity_is_never_silently_evicted() {
        let hub = Hub::default();
        for n in 0..messages::MAX_MESSAGES {
            hub.enqueue(
                proof(Origin::Agent, None),
                recipient('b', "123", None),
                "text".into(),
                format!("r{n}"),
            )
            .unwrap();
        }
        assert!(
            hub.enqueue(
                proof(Origin::Agent, None),
                recipient('b', "123", None),
                "overflow".into(),
                "overflow".into()
            )
            .is_err()
        );
        assert_eq!(
            hub.outgoing(&proof(Origin::Agent, None).sender).len(),
            messages::MAX_MESSAGES
        );
    }

    #[test]
    fn outer_delivery_preserves_agent_origin_without_creating_user_requirements() {
        let hub = Hub::default();
        hub.enqueue(
            proof(Origin::Agent, None),
            recipient('b', "123", None),
            "coordination".into(),
            "r".into(),
        )
        .unwrap();
        let messages = receive(&hub, 'b', "123", None, 10000)
            .into_iter()
            .map(|m| serde_json::to_value(m).unwrap())
            .collect();
        let response = append(
            json!({"content":[],"structuredContent":{"result":"ok"}}),
            messages,
        );
        assert!(response["structuredContent"].get("user_messages").is_none());
        assert_eq!(
            response["structuredContent"]["native_messages"][0]["sender"]["kind"],
            "agent"
        );
        assert_eq!(response["structuredContent"]["result"], "ok");
    }
}
