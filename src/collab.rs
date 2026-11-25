//! Bounded coordination for agents sharing one live broker session.
//!
//! Membership is explicit because HTTP callers can share an OAuth identity.
//! Participant credentials bind a mailbox to that authenticated identity.
//! Messages stay in memory until acknowledged or the membership/session ends.
pub(crate) mod lifecycle;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, MutexGuard, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{
    sync::{Notify, Semaphore, watch},
    time::Instant,
};

pub(crate) const DREAMING_POLICY: &str = include_str!("collab/dreaming_policy.txt");

const LEASE_MS: u64 = 15 * 60 * 1000;
const MAX_AGENTS: usize = 64;
const MAX_PAYLOAD: usize = 16 * 1024;
const MAX_INBOX_MESSAGES: usize = 128;
const MAX_INBOX_BYTES: usize = 256 * 1024;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const MAX_RECEIVE_MESSAGES: usize = 64;
const MAX_RECEIVE_BYTES: usize = 128 * 1024;
const MAX_WAIT_MS: u64 = crate::wire::MAX_OBSERVATION_WAIT_MS;
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
        "max_capability_bytes": 64,
        "agent_states": ["working","idle","offline","dreaming","retiring"],
        "default_idle_after_ms": lifecycle::DEFAULT_IDLE_AFTER_MS,
        "default_maintenance_budget_ms": lifecycle::DEFAULT_MAINTENANCE_BUDGET_MS,
        "max_offline_summaries": MAX_AGENTS,
        "offline_retention_ms": 300_000
    })
}

#[derive(Default)]
pub(crate) struct Hub {
    session_id: String,
    state: Arc<Mutex<State>>,
    events: Option<Arc<crate::events::Hub>>,
    lifecycle_changed: Arc<Notify>,
    lifecycle_started: AtomicBool,
}

#[derive(Default)]
struct State {
    closed: bool,
    sequence: u64,
    queued_bytes: usize,
    agents: BTreeMap<String, Agent>,
    offline: VecDeque<lifecycle::Offline>,
}

struct Agent {
    id: String,
    session_id: String,
    token: String,
    owner: String,
    name: String,
    role: Option<String>,
    lifecycle: lifecycle::Lifecycle,
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
    sender_id: String,
    sender_owner: String,
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
        #[serde(alias = "summary")]
        role: Option<String>,
        #[serde(default)]
        capabilities: Vec<String>,
    },
    List {},
    Update {
        #[serde(alias = "thread_id")]
        agent_id: String,
        token: String,
        name: Option<String>,
        #[serde(alias = "summary")]
        role: Option<String>,
        status: Option<lifecycle::Status>,
        expected_revision: Option<u64>,
        idle_after_ms: Option<u64>,
        maintenance_budget_ms: Option<u64>,
        maintenance: Option<lifecycle::MaintenancePatch>,
        capabilities: Option<Vec<String>>,
    },
    Send {
        #[serde(alias = "thread_id")]
        agent_id: String,
        token: String,
        to: String,
        payload: Value,
        topic: Option<String>,
        reply_to: Option<String>,
        idempotency_key: Option<String>,
    },
    Receive {
        #[serde(alias = "thread_id")]
        agent_id: String,
        token: String,
        after: Option<u64>,
        #[serde(default = "receive_limit")]
        limit: usize,
        #[serde(default)]
        wait_ms: u64,
    },
    Ack {
        #[serde(alias = "thread_id")]
        agent_id: String,
        token: String,
        through: u64,
    },
    Leave {
        #[serde(alias = "thread_id")]
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
            "thread_id": self.id,
            "session_id": self.session_id,
            "client_id": self.session_id,
            "pending_messages": self.inbox.len(),
            "name": self.name,
            "role": self.role,
            "summary": self.role,
            "status": self.lifecycle.status,
            "membership_active": true,
            "lifecycle": self.lifecycle.public(),
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
    fn purge_expired(&mut self) -> Vec<Agent> {
        let now = Instant::now();
        let expired: Vec<_> = self
            .agents
            .iter()
            .filter(|(_, agent)| agent.expires <= now)
            .map(|(id, _)| id.clone())
            .collect();
        expired
            .into_iter()
            .filter_map(|id| self.remove(&id))
            .collect()
    }

    fn remove(&mut self, id: &str) -> Option<Agent> {
        let agent = self.agents.remove(id)?;
        self.queued_bytes -= agent.inbox_bytes;
        agent.changed.send_replace(());
        Some(agent)
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
    pub(crate) fn for_session(session_id: &str) -> Self {
        Self {
            session_id: session_id.into(),
            state: Arc::default(),
            events: None,
            lifecycle_changed: Arc::default(),
            lifecycle_started: AtomicBool::new(false),
        }
    }

    pub(crate) fn with_events(session_id: &str, events: Arc<crate::events::Hub>) -> Self {
        let mut hub = Self::for_session(session_id);
        hub.events = Some(events);
        hub
    }

    fn agent_event(&self, owner: &str, id: &str, kind: &str, data: Value) {
        if let Some(events) = &self.events {
            // The membership mutation has already committed. Journal capacity
            // must never turn it into a failed operation that a caller retries.
            let _ =
                events.publish_system(owner, &self.session_id, Some(id), kind, data, (None, None));
        }
    }

    fn message_event(
        &self,
        kind: &str,
        message: &Message,
        recipient_owner: &str,
        recipient_id: &str,
    ) {
        let Some(events) = &self.events else {
            return;
        };
        let mut audience = vec![
            format!("{}:{}", self.session_id, message.sender_id),
            format!("{}:{recipient_id}", self.session_id),
        ];
        audience.sort();
        audience.dedup();
        let actor = if kind == "message.sent" {
            &message.sender_id
        } else {
            recipient_id
        };
        let data = json!({
            "transport":"collab", "message_id":message.value["message_id"],
            "message_seq":message.seq, "sender_id":message.sender_id,
            "recipient_id":recipient_id,
            "delivery":match kind {
                "message.sent" => "queued",
                "message.received" => "included_in_receive_result",
                _ => "acknowledged",
            },
            "read_confirmed":false
        });
        // Publish the same private receipt separately for the two real owners.
        // An owner key or matching bare agent ID never widens the audience.
        let _ = events.publish_system(
            &message.sender_owner,
            &self.session_id,
            Some(actor),
            kind,
            data.clone(),
            (Some(audience.clone()), None),
        );
        if recipient_owner != message.sender_owner {
            let _ = events.publish_system(
                recipient_owner,
                &self.session_id,
                Some(actor),
                kind,
                data,
                (Some(audience), None),
            );
        }
    }

    /// Public roster for the connected local client's UI. Mailboxes, owner keys
    /// and private participant tokens never cross this interface.
    pub(crate) fn presence(&self) -> Result<Vec<crate::wire::SteeringAgent>, String> {
        let state = self.locked()?;
        Ok(state
            .agents
            .values()
            .map(|agent| crate::wire::SteeringAgent {
                agent_id: agent.id.clone(),
                name: agent.name.clone(),
                role: agent.role.clone(),
                status: agent.lifecycle.status.as_str().into(),
                dreaming_active: {
                    let active = agent.lifecycle.dreaming_since_ms().is_some();
                    (active || agent.lifecycle.status.as_str() == "dreaming").then_some(active)
                },
            })
            .collect())
    }

    /// Server-only recipient lookup. Private tokens never leave the registry.
    pub(crate) fn native_members(
        &self,
    ) -> Result<Vec<(String, crate::wire::SteeringAgent)>, String> {
        let state = self.locked()?;
        Ok(state
            .agents
            .values()
            .map(|agent| {
                (
                    agent.owner.clone(),
                    crate::wire::SteeringAgent {
                        agent_id: agent.id.clone(),
                        name: agent.name.clone(),
                        role: agent.role.clone(),
                        status: agent.lifecycle.status.as_str().into(),
                        dreaming_active: None,
                    },
                )
            })
            .collect())
    }

    pub(crate) fn bind(
        &self,
        owner: &str,
        id: &str,
        token: &str,
    ) -> Result<crate::wire::SteeringAgent, String> {
        let mut state = self.locked()?;
        let agent = state.authenticated(owner, id, token)?;
        agent.renew();
        Ok(crate::wire::SteeringAgent {
            agent_id: agent.id.clone(),
            name: agent.name.clone(),
            role: agent.role.clone(),
            status: agent.lifecycle.status.as_str().into(),
            dreaming_active: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn expire_lease_for_test(&self, owner: &str, id: &str) {
        // Test only the passage of time for an existing, real membership. Its
        // normal lookup/purge path still decides whether it is valid afterward.
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let member = state
            .agents
            .get_mut(id)
            .filter(|member| member.owner == owner)
            .expect("existing membership for this verified fixture owner");
        member.expires = Instant::now();
        member.expires_at_ms = now_ms();
        self.lifecycle_changed.notify_one();
    }

    fn locked(&self) -> Result<MutexGuard<'_, State>, String> {
        self.start_lifecycle();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("collaboration session is closed; refresh clients and join again".into());
        }
        self.advance_lifecycle(&mut state);
        Ok(state)
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        let ids: Vec<_> = state.agents.keys().cloned().collect();
        for id in ids {
            self.remove_member(&mut state, &id, "session_closed");
        }
        state.offline.clear();
        self.lifecycle_changed.notify_one();
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
                Ok(
                    json!({"session_id":self.session_id, "client_id":self.session_id, "agents":state.public_agents(), "offline_agents":state.offline_agents(), "limits":limits()}),
                )
            }
            Request::Update {
                agent_id,
                token,
                name,
                role,
                status,
                expected_revision,
                idle_after_ms,
                maintenance_budget_ms,
                maintenance,
                capabilities,
            } => {
                if let Some(name) = &name {
                    text(name, "name", 128, false)?;
                }
                if let Some(role) = &role {
                    text(role, "role", 128, true)?;
                }
                if let Some(capabilities) = &capabilities {
                    validate_capabilities(capabilities)?;
                }
                let mut state = self.locked()?;
                let agent = state.authenticated(owner, &agent_id, &token)?;
                let changes = agent.lifecycle.update(
                    &agent_id,
                    lifecycle::Patch {
                        status,
                        expected_revision,
                        idle_after_ms,
                        maintenance_budget_ms,
                        maintenance,
                    },
                    Instant::now(),
                    now_ms(),
                )?;
                if let Some(name) = name {
                    agent.name = name;
                }
                if let Some(role) = role {
                    agent.role = (!role.is_empty()).then_some(role);
                }
                if let Some(capabilities) = capabilities {
                    agent.capabilities = capabilities;
                }
                agent.renew();
                agent.changed.send_replace(());
                self.publish_lifecycle(agent, changes);
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
                self.remove_member(&mut state, &agent_id, "left");
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
            session_id: self.session_id.clone(),
            token: token.clone(),
            owner: owner.into(),
            name,
            role: role.filter(|value| !value.is_empty()),
            lifecycle: lifecycle::Lifecycle::new(Instant::now(), now),
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
        self.agent_event(
            owner,
            &id,
            "agent.joined",
            json!({"status":"idle","revision":1}),
        );
        self.lifecycle_changed.notify_one();
        Ok(json!({
            "agent_id":id,
            "thread_id":id,
            "session_id":self.session_id,
            "client_id":self.session_id,
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
            "from_thread_id":id,
            "session_id":self.session_id,
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
        let message = Arc::new(Message {
            seq,
            bytes,
            value,
            sender_id: id.into(),
            sender_owner: owner.into(),
        });
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
            "session_id":self.session_id,
            "delivery":"queued",
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
        for target in &recipients {
            self.message_event(
                "message.sent",
                &message,
                &state.agents[target].owner,
                target,
            );
        }
        Ok(receipt)
    }

    pub(crate) async fn receive(
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
                let mut new_deliveries = Vec::new();
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
                    if message.seq > agent.delivered {
                        new_deliveries.push(message.clone());
                    }
                }
                if !messages.is_empty() || wait_ms == 0 || Instant::now() >= deadline {
                    agent.delivered = agent.delivered.max(next);
                    agent.renew();
                    for message in new_deliveries {
                        self.message_event("message.received", &message, owner, id);
                    }
                    return Ok(json!({
                        "messages":messages,
                        "session_id":self.session_id,
                        "thread_id":id,
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

    /// Observe a visible member without consuming messages or changing its task.
    /// The watcher subscribes under the same lock used by update/remove.
    pub(crate) async fn wait_status(
        &self,
        owner: &str,
        id: &str,
        token: &str,
        target: &str,
        statuses: &[String],
        wait_ms: u64,
    ) -> Result<Value, String> {
        if statuses.is_empty() || statuses.len() > 16 || wait_ms > MAX_WAIT_MS {
            return Err("invalid agent status wait bounds".into());
        }
        for status in statuses {
            serde_json::from_value::<lifecycle::Status>(json!(status))
                .map_err(|_| "agent status must be working, idle, offline, dreaming or retiring")?;
        }
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        let mut changes = None;
        loop {
            let expires = {
                let mut state = self.locked()?;
                state.authenticated(owner, id, token)?.renew();
                let Some(agent) = state.agents.get(target) else {
                    if statuses.iter().any(|status| status == "offline")
                        && let Some(agent) = state.offline_agent(target)
                    {
                        return Ok(json!({"agent":agent,"matched":true}));
                    }
                    return Err("await_unknown_target: agent is absent or its lease expired".into());
                };
                let matched = statuses
                    .iter()
                    .any(|status| status == agent.lifecycle.status.as_str());
                if matched || Instant::now() >= deadline {
                    return Ok(json!({"agent":agent.public(),"matched":matched}));
                }
                if changes.is_none() {
                    changes = Some(agent.changed.subscribe());
                }
                agent.expires
            };
            let changed = changes.as_mut().expect("subscribed status observer");
            let _ = tokio::time::timeout_at(deadline.min(expires), changed.changed()).await;
        }
    }

    fn ack(&self, owner: &str, id: &str, token: &str, through: u64) -> Result<Value, String> {
        let mut state = self.locked()?;
        let mut acknowledged = Vec::new();
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
                    acknowledged.push(message);
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
        for message in acknowledged {
            self.message_event("message.acknowledged", &message, owner, id);
        }
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
    Ok(crate::hex::encode(bytes))
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
mod await_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_event_tests {
    use super::*;
    use crate::events::{Actor, Filter};

    fn member(hub: &Hub, owner: &str, name: &str) -> (String, String) {
        let joined = hub.join(owner, name.into(), None, vec![]).unwrap();
        (
            joined["agent_id"].as_str().unwrap().into(),
            joined["token"].as_str().unwrap().into(),
        )
    }

    fn subscribe(
        events: &crate::events::Hub,
        owner: &str,
        id: &str,
        kind: &str,
    ) -> (Actor, String) {
        let actor = Actor {
            owner: owner.into(),
            client_id: "a".repeat(64),
            agent_id: id.into(),
        };
        let filter: Filter = serde_json::from_value(json!({"kinds":[kind]})).unwrap();
        let result = events
            .subscribe(&actor, "subscription", filter, None)
            .unwrap();
        (actor, result["subscription_id"].as_str().unwrap().into())
    }

    #[tokio::test]
    async fn message_commit_events_are_private_and_do_not_repeat_on_recovery() {
        let events = Arc::new(crate::events::Hub::default());
        let hub = Hub::with_events(&"a".repeat(64), events.clone());
        let (sender, sender_token) = member(&hub, "owner-a", "sender");
        let (recipient, recipient_token) = member(&hub, "owner-b", "recipient");
        let (stranger, _) = member(&hub, "owner-b", "stranger");
        let sent_events = subscribe(&events, "owner-a", &sender, "message.*");
        let received_events = subscribe(&events, "owner-b", &recipient, "message.*");
        let unrelated_events = subscribe(&events, "owner-b", &stranger, "message.*");
        let message = || Send {
            to: recipient.clone(),
            payload: json!({"text":"private body"}),
            topic: None,
            reply_to: None,
            idempotency_key: Some("same-request".into()),
        };
        let receipt = hub
            .send("owner-a", &sender, &sender_token, message())
            .unwrap();
        let duplicate = hub
            .send("owner-a", &sender, &sender_token, message())
            .unwrap();
        assert_eq!(receipt["message_id"], duplicate["message_id"]);
        assert_eq!(duplicate["duplicate"], true);
        let first = hub
            .receive("owner-b", &recipient, &recipient_token, None, 32, 0)
            .await
            .unwrap();
        let replay = hub
            .receive("owner-b", &recipient, &recipient_token, None, 32, 0)
            .await
            .unwrap();
        assert_eq!(first["messages"], replay["messages"]);
        let through = first["next_cursor"].as_u64().unwrap();
        hub.ack("owner-b", &recipient, &recipient_token, through)
            .unwrap();
        hub.ack("owner-b", &recipient, &recipient_token, through)
            .unwrap();
        for (actor, subscription) in [sent_events, received_events] {
            let page = events.read(&actor, &subscription, 32).unwrap();
            let kinds: Vec<_> = page["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["kind"].as_str().unwrap())
                .collect();
            assert_eq!(
                kinds,
                ["message.sent", "message.received", "message.acknowledged"]
            );
            let serialized = page.to_string();
            assert!(!serialized.contains("private body"));
            assert!(!serialized.contains(&sender_token));
            assert!(!serialized.contains(&recipient_token));
        }
        let page = events
            .read(&unrelated_events.0, &unrelated_events.1, 32)
            .unwrap();
        assert!(page["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn status_events_follow_real_changes_and_expiry_removal() {
        let events = Arc::new(crate::events::Hub::default());
        let hub = Hub::with_events(&"a".repeat(64), events.clone());
        let (observer, _) = member(&hub, "owner", "observer");
        let (actor, subscription) = subscribe(&events, "owner", &observer, "agent.*");
        let (id, token) = member(&hub, "owner", "worker");
        let mut update =
            json!({"agent_id":id,"token":token,"status":"working","expected_revision":1});
        hub.dispatch("owner", "update", update.clone())
            .await
            .unwrap();
        update["expected_revision"] = json!(2);
        hub.dispatch("owner", "update", update).await.unwrap();
        hub.expire_lease_for_test("owner", &id);
        hub.presence().unwrap();
        hub.presence().unwrap();
        let page = events.read(&actor, &subscription, 32).unwrap();
        let entries = page["events"].as_array().unwrap();
        let kinds: Vec<_> = entries
            .iter()
            .map(|event| event["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "agent.joined",
                "agent.status_changed",
                "agent.status_changed",
                "agent.left"
            ]
        );
        assert_eq!(entries[1]["data"]["previous_status"], "idle");
        assert_eq!(entries[2]["data"]["status"], "offline");
        assert_eq!(entries[3]["data"]["reason"], "lease_expired");
    }
}
