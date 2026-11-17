//! Bounded coordination for agents sharing one live broker session.
//!
//! Membership is explicit because HTTP callers can share an OAuth identity.
//! Participant credentials bind a mailbox to that authenticated identity.
//! Messages stay in memory until acknowledged or the membership/session ends.
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Semaphore, watch},
    time::Instant,
};

const LEASE_MS: u64 = 15 * 60 * 1000;
const MAX_AGENTS: usize = 64;
const MAX_PAYLOAD: usize = 16 * 1024;
const MAX_INBOX_MESSAGES: usize = 128;
const MAX_INBOX_BYTES: usize = 256 * 1024;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECEIVE_MESSAGES: usize = 64;
const MAX_RECEIVE_BYTES: usize = 128 * 1024;
const MAX_WAIT_MS: u64 = 30_000;
const MAX_DEDUP: usize = 128;
const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;

pub(crate) fn limits() -> Value {
    json!({
        "max_agents": MAX_AGENTS,
        "lease_ms": LEASE_MS,
        "max_payload_bytes": MAX_PAYLOAD,
        "max_inbox_messages": MAX_INBOX_MESSAGES,
        "max_inbox_bytes": MAX_INBOX_BYTES,
        "max_queued_bytes": MAX_QUEUED_BYTES,
        "max_receive_messages": MAX_RECEIVE_MESSAGES,
        "max_receive_bytes": MAX_RECEIVE_BYTES,
        "max_wait_ms": MAX_WAIT_MS,
        "max_waiters_per_agent": 1,
        "idempotency_window": MAX_DEDUP,
        "max_metadata_bytes": 128,
        "max_capabilities": 16,
        "max_capability_bytes": 64
    })
}

#[derive(Default)]
pub(crate) struct Hub {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    closed: bool,
    sequence: u64,
    queued_bytes: usize,
    agents: BTreeMap<String, Agent>,
}

struct Agent {
    id: String,
    token: String,
    owner: String,
    name: String,
    role: Option<String>,
    status: String,
    capabilities: Vec<String>,
    joined_at_ms: u64,
    last_seen_ms: u64,
    expires_at_ms: u64,
    expires: Instant,
    inbox: VecDeque<Arc<Message>>,
    inbox_bytes: usize,
    delivered: u64,
    acked: u64,
    dedup: VecDeque<Attempt>,
    changed: watch::Sender<()>,
    waiters: Arc<Semaphore>,
}

struct Message {
    seq: u64,
    bytes: usize,
    value: Value,
}

struct Attempt {
    key: Option<String>,
    fingerprint: [u8; 32],
    receipt: Value,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Join {
        name: String,
        role: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    List {},
    Update {
        agent_id: String,
        token: String,
        name: Option<String>,
        role: Option<String>,
        status: Option<String>,
        capabilities: Option<Vec<String>>,
    },
    Send {
        agent_id: String,
        token: String,
        to: String,
        payload: Value,
        topic: Option<String>,
        reply_to: Option<String>,
        idempotency_key: Option<String>,
    },
    Receive {
        agent_id: String,
        token: String,
        after: Option<u64>,
        #[serde(default = "receive_limit")]
        limit: usize,
        #[serde(default)]
        wait_ms: u64,
    },
    Ack {
        agent_id: String,
        token: String,
        through: u64,
    },
    Leave {
        agent_id: String,
        token: String,
    },
}

fn receive_limit() -> usize {
    32
}

impl Agent {
    fn public(&self) -> Value {
        json!({
            "agent_id": self.id,
            "name": self.name,
            "role": self.role,
            "status": self.status,
            "capabilities": self.capabilities,
            "joined_at_ms": self.joined_at_ms,
            "last_seen_ms": self.last_seen_ms,
            "expires_at_ms": self.expires_at_ms
        })
    }

    fn renew(&mut self) {
        self.last_seen_ms = now_ms();
        self.expires_at_ms = self.last_seen_ms.saturating_add(LEASE_MS);
        self.expires = Instant::now() + Duration::from_millis(LEASE_MS);
    }
}

impl State {
    fn purge_expired(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .agents
            .iter()
            .filter(|(_, agent)| agent.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.remove(&id);
        }
    }

    fn remove(&mut self, id: &str) {
        if let Some(agent) = self.agents.remove(id) {
            self.queued_bytes -= agent.inbox_bytes;
            agent.changed.send_replace(());
        }
    }

    fn authenticated(&mut self, owner: &str, id: &str, token: &str) -> Result<&mut Agent, String> {
        let agent = self
            .agents
            .get_mut(id)
            .filter(|agent| {
                agent.owner == owner
                    && token.len() == 64
                    && bool::from(agent.token.as_bytes().ct_eq(token.as_bytes()))
            })
            .ok_or("agent credentials are invalid or the membership expired; join again")?;
        Ok(agent)
    }

    fn public_agents(&self) -> Vec<Value> {
        self.agents.values().map(Agent::public).collect()
    }
}

impl Hub {
    fn locked(&self) -> Result<MutexGuard<'_, State>, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("collaboration session is closed; refresh clients and join again".into());
        }
        state.purge_expired();
        Ok(state)
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        for agent in state.agents.values() {
            agent.changed.send_replace(());
        }
        state.agents.clear();
        state.queued_bytes = 0;
    }

    pub(crate) async fn dispatch(
        &self,
        owner: &str,
        action: &str,
        mut args: Value,
    ) -> Result<Value, String> {
        if owner.is_empty() {
            return Err("collaboration requires an authenticated owner".into());
        }
        let fields = args
            .as_object_mut()
            .ok_or("collaboration arguments must be an object")?;
        if fields.contains_key("action") {
            return Err("action must be supplied separately from collaboration arguments".into());
        }
        if action == "send" && !fields.contains_key("payload") {
            return Err("send requires payload; use null for an empty JSON payload".into());
        }
        fields.insert("action".into(), json!(action));
        let request: Request = serde_json::from_value(args)
            .map_err(|error| format!("invalid collaboration arguments: {error}"))?;
        match request {
            Request::Join {
                name,
                role,
                capabilities,
            } => self.join(owner, name, role, capabilities),
            Request::List {} => {
                let state = self.locked()?;
                Ok(json!({"agents":state.public_agents(), "limits":limits()}))
            }
            Request::Update {
                agent_id,
                token,
                name,
                role,
                status,
                capabilities,
            } => {
                if let Some(name) = &name {
                    text(name, "name", 128, false)?;
                }
                if let Some(role) = &role {
                    text(role, "role", 128, true)?;
                }
                if let Some(status) = &status {
                    text(status, "status", 128, false)?;
                }
                if let Some(capabilities) = &capabilities {
                    validate_capabilities(capabilities)?;
                }
                let mut state = self.locked()?;
                let agent = state.authenticated(owner, &agent_id, &token)?;
                if let Some(name) = name {
                    agent.name = name;
                }
                if let Some(role) = role {
                    agent.role = (!role.is_empty()).then_some(role);
                }
                if let Some(status) = status {
                    agent.status = status;
                }
                if let Some(capabilities) = capabilities {
                    agent.capabilities = capabilities;
                }
                agent.renew();
                Ok(json!({"agent":agent.public(), "lease_ms":LEASE_MS}))
            }
            Request::Send {
                agent_id,
                token,
                to,
                payload,
                topic,
                reply_to,
                idempotency_key,
            } => self.send(
                owner,
                &agent_id,
                &token,
                Send {
                    to,
                    payload,
                    topic,
                    reply_to,
                    idempotency_key,
                },
            ),
            Request::Receive {
                agent_id,
                token,
                after,
                limit,
                wait_ms,
            } => {
                self.receive(owner, &agent_id, &token, after, limit, wait_ms)
                    .await
            }
            Request::Ack {
                agent_id,
                token,
                through,
            } => self.ack(owner, &agent_id, &token, through),
            Request::Leave { agent_id, token } => {
                let mut state = self.locked()?;
                state.authenticated(owner, &agent_id, &token)?;
                state.remove(&agent_id);
                Ok(json!({"agent_id":agent_id, "left":true}))
            }
        }
    }

    fn join(
        &self,
        owner: &str,
        name: String,
        role: Option<String>,
        capabilities: Vec<String>,
    ) -> Result<Value, String> {
        text(&name, "name", 128, false)?;
        if let Some(role) = &role {
            text(role, "role", 128, true)?;
        }
        validate_capabilities(&capabilities)?;
        let mut state = self.locked()?;
        if state.agents.len() >= MAX_AGENTS {
            return Err("collaboration session has reached its agent limit".into());
        }
        let mut id = format!("agent_{}", random(16)?);
        while state.agents.contains_key(&id) {
            id = format!("agent_{}", random(16)?);
        }
        let token = random(32)?;
        let now = now_ms();
        let expires_at_ms = now.saturating_add(LEASE_MS);
        let (changed, _) = watch::channel(());
        let agent = Agent {
            id: id.clone(),
            token: token.clone(),
            owner: owner.into(),
            name,
            role: role.filter(|value| !value.is_empty()),
            status: "available".into(),
            capabilities,
            joined_at_ms: now,
            last_seen_ms: now,
            expires_at_ms,
            expires: Instant::now() + Duration::from_millis(LEASE_MS),
            inbox: VecDeque::new(),
            inbox_bytes: 0,
            delivered: state.sequence,
            acked: state.sequence,
            dedup: VecDeque::new(),
            changed,
            waiters: Arc::new(Semaphore::new(1)),
        };
        state.agents.insert(id.clone(), agent);
        Ok(json!({
            "agent_id":id,
            "token":token,
            "lease_ms":LEASE_MS,
            "expires_at_ms":expires_at_ms,
            "cursor":state.sequence,
            "agents":state.public_agents(),
            "limits":limits()
        }))
    }

    fn send(&self, owner: &str, id: &str, token: &str, args: Send) -> Result<Value, String> {
        text(&args.to, "to", 128, false)?;
        for (name, value) in [
            ("topic", &args.topic),
            ("reply_to", &args.reply_to),
            ("idempotency_key", &args.idempotency_key),
        ] {
            if let Some(value) = value {
                text(value, name, 128, false)?;
            }
        }
        let payload_bytes = serde_json::to_vec(&args.payload).map_err(|e| e.to_string())?;
        if payload_bytes.len() > MAX_PAYLOAD {
            return Err(format!(
                "payload exceeds {MAX_PAYLOAD} serialized UTF-8 bytes"
            ));
        }
        let canonical = serde_json::to_vec(&(&args.to, &args.payload, &args.topic, &args.reply_to))
            .map_err(|e| e.to_string())?;
        let fingerprint: [u8; 32] = Sha256::digest(canonical).into();
        let mut state = self.locked()?;
        let sender = state.authenticated(owner, id, token)?;
        if let Some(key) = &args.idempotency_key
            && let Some(attempt) = sender
                .dedup
                .iter()
                .find(|attempt| attempt.key.as_ref() == Some(key))
        {
            if attempt.fingerprint != fingerprint {
                return Err(
                    "idempotency_key was already used for different message content".into(),
                );
            }
            let mut receipt = attempt.receipt.clone();
            receipt["duplicate"] = json!(true);
            sender.renew();
            return Ok(receipt);
        }
        let recipients: Vec<String> = if args.to == "*" {
            state
                .agents
                .keys()
                .filter(|target| target.as_str() != id)
                .cloned()
                .collect()
        } else if state.agents.contains_key(&args.to) {
            vec![args.to.clone()]
        } else {
            return Err("recipient is unavailable; list agents before retrying".into());
        };
        let seq = state
            .sequence
            .checked_add(1)
            .filter(|seq| *seq <= MAX_SEQUENCE)
            .ok_or("collaboration message sequence is exhausted")?;
        let message_id = format!("msg_{}", random(16)?);
        let value = json!({
            "seq":seq,
            "message_id":message_id,
            "from":id,
            "to":args.to,
            "payload":args.payload,
            "topic":args.topic,
            "reply_to":args.reply_to,
            "sent_at_ms":now_ms()
        });
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len();
        let added_bytes = bytes * recipients.len();
        if state.queued_bytes + added_bytes > MAX_QUEUED_BYTES {
            return Err("session message capacity is full; receivers must acknowledge messages before retrying".into());
        }
        for target in &recipients {
            let agent = &state.agents[target];
            if agent.inbox.len() >= MAX_INBOX_MESSAGES
                || agent.inbox_bytes + bytes > MAX_INBOX_BYTES
            {
                return Err(format!(
                    "recipient {target} inbox is full; receive and acknowledge messages before retrying"
                ));
            }
        }
        let message = Arc::new(Message { seq, bytes, value });
        // All admission checks precede the first mutation: broadcasts either
        // reach every selected mailbox or change none of them.
        state.sequence = seq;
        state.queued_bytes += added_bytes;
        for target in &recipients {
            let agent = state.agents.get_mut(target).expect("validated recipient");
            agent.inbox.push_back(message.clone());
            agent.inbox_bytes += bytes;
            agent.changed.send_replace(());
        }
        let receipt = json!({
            "message_id":message_id,
            "seq":seq,
            "recipients":recipients,
            "duplicate":false
        });
        let sender = state.agents.get_mut(id).expect("authenticated sender");
        sender.dedup.push_back(Attempt {
            key: args.idempotency_key,
            fingerprint,
            receipt: receipt.clone(),
        });
        if sender.dedup.len() > MAX_DEDUP {
            sender.dedup.pop_front();
        }
        sender.renew();
        Ok(receipt)
    }

    async fn receive(
        &self,
        owner: &str,
        id: &str,
        token: &str,
        after: Option<u64>,
        limit: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        if limit == 0 || limit > MAX_RECEIVE_MESSAGES {
            return Err(format!("receive limit must be 1..={MAX_RECEIVE_MESSAGES}"));
        }
        if wait_ms > MAX_WAIT_MS {
            return Err(format!("wait_ms must be 0..={MAX_WAIT_MS}"));
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut changes = None;
        let mut permit = None;
        loop {
            let expires = {
                let mut state = self.locked()?;
                let agent = state.authenticated(owner, id, token)?;
                let cursor = after.unwrap_or(agent.acked);
                if cursor < agent.acked || cursor > agent.delivered {
                    return Err(
                        "after must be between acked_cursor and the highest delivered cursor"
                            .into(),
                    );
                }
                let mut messages = Vec::new();
                // Reserve the bounded response envelope and array separators.
                let mut used = 1024;
                let mut next = cursor;
                let mut more = false;
                for message in agent.inbox.iter().filter(|message| message.seq > cursor) {
                    if messages.len() >= limit || used + message.bytes + 1 > MAX_RECEIVE_BYTES {
                        more = true;
                        break;
                    }
                    used += message.bytes + 1;
                    next = message.seq;
                    messages.push(message.value.clone());
                }
                if !messages.is_empty() || wait_ms == 0 || Instant::now() >= deadline {
                    agent.delivered = agent.delivered.max(next);
                    agent.renew();
                    return Ok(json!({
                        "messages":messages,
                        "next_cursor":next,
                        "has_more":more,
                        "acked_cursor":agent.acked,
                        "lease_ms":LEASE_MS,
                        "expires_at_ms":agent.expires_at_ms
                    }));
                }
                if permit.is_none() {
                    // The owned permit is released when this future completes
                    // or is cancelled, including HTTP/code_mode cancellation.
                    permit = Some(
                        agent
                            .waiters
                            .clone()
                            .try_acquire_owned()
                            .map_err(|_| "this agent already has a waiting receive request")?,
                    );
                    // Subscribe while holding the same lock used by send.
                    // watch retains a change even before changed() is polled.
                    changes = Some(agent.changed.subscribe());
                    agent.renew();
                }
                agent.expires
            };
            let changed = changes.as_mut().expect("waiting receiver");
            if let Ok(Err(_)) =
                tokio::time::timeout_at(deadline.min(expires), changed.changed()).await
            {
                return Err("agent membership or collaboration session ended; join again".into());
            }
        }
    }

    fn ack(&self, owner: &str, id: &str, token: &str, through: u64) -> Result<Value, String> {
        let mut state = self.locked()?;
        let (removed_bytes, result) = {
            let agent = state.authenticated(owner, id, token)?;
            if through > agent.delivered {
                return Err("cannot acknowledge beyond the highest delivered cursor".into());
            }
            let mut removed_bytes = 0;
            let mut removed = 0;
            if through > agent.acked {
                while agent
                    .inbox
                    .front()
                    .is_some_and(|message| message.seq <= through)
                {
                    let message = agent.inbox.pop_front().expect("nonempty inbox");
                    removed_bytes += message.bytes;
                    removed += 1;
                }
                agent.acked = through;
                agent.inbox_bytes -= removed_bytes;
            }
            agent.renew();
            (
                removed_bytes,
                json!({
                    "acked_cursor":agent.acked,
                    "removed":removed,
                    "pending":agent.inbox.len()
                }),
            )
        };
        state.queued_bytes -= removed_bytes;
        Ok(result)
    }
}

struct Send {
    to: String,
    payload: Value,
    topic: Option<String>,
    reply_to: Option<String>,
    idempotency_key: Option<String>,
}

fn text(value: &str, name: &str, maximum: usize, empty: bool) -> Result<(), String> {
    if (!empty && value.is_empty()) || value.len() > maximum || value.chars().any(char::is_control)
    {
        return Err(format!(
            "{name} must contain {}..={maximum} UTF-8 bytes without control characters",
            usize::from(!empty)
        ));
    }
    Ok(())
}

fn validate_capabilities(capabilities: &[String]) -> Result<(), String> {
    if capabilities.len() > 16 {
        return Err("capabilities must contain at most 16 strings".into());
    }
    for value in capabilities {
        text(value, "capability", 64, false)?;
    }
    Ok(())
}

fn random(size: usize) -> Result<String, String> {
    let mut bytes = vec![0; size];
    crate::platform::random_bytes(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX - LEASE_MS)
}

#[cfg(test)]
mod tests;
