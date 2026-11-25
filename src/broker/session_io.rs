//! Account-owned chat requests with exact authenticated thread recipients.
//! Receipt completion ends one input, never a participant or its task lifecycle.
use super::{Authorization, Broker, Session};
use crate::identity::Identity;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

const MAX_REQUESTS: usize = 1024;
const MAX_ACCOUNT_REQUESTS: usize = 128;
const MAX_RECIPIENTS: usize = 32;
const MAX_TEXT: usize = 16 * 1024;
// JSON escapes for permitted text cost at most 2x UTF-8 bytes; metadata fits 4 KiB.
const FINAL_BYTES: usize = MAX_TEXT * 2 + 4096;
const MAX_OUTPUTS: usize = 256;
const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const RETENTION_MS: u64 = 60 * 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn same_account(a: &Identity, b: &Identity) -> bool {
    a.provider == b.provider && a.subject == b.subject
}
fn authorized(broker: &Broker, session: &Session, actor: &Identity) -> bool {
    let auth = Authorization::Identity(actor.clone());
    session.is_alive() && broker.authorized_account(&auth) && auth.permits(session)
}
fn member(session: &Session, thread_id: &str) -> Option<Identity> {
    session
        .collaboration
        .native_members()
        .ok()?
        .into_iter()
        .find_map(|(owner, agent)| {
            if agent.agent_id != thread_id {
                return None;
            }
            session
                .allowed()
                .iter()
                .find(|identity| {
                    owner.ends_with(&format!("\0{}\0{}", identity.provider, identity.subject))
                })
                .cloned()
        })
}
fn identifier(prefix: &str) -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|_| "cannot allocate chat I/O identity")?;
    Ok(format!("{prefix}_{}", crate::hex::encode(bytes)))
}
fn text_valid(text: &str) -> Result<(), String> {
    if text.trim().is_empty()
        || text.len() > MAX_TEXT
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err("chat text must be nonempty, at most 16384 UTF-8 bytes, and contain no control characters except newline or tab".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Recipient {
    pub(crate) client_id: String,
    #[serde(default)]
    pub(crate) thread_id: Option<String>,
}
impl Recipient {
    fn id(&self) -> String {
        match &self.thread_id {
            Some(thread) => format!("agent:{}:{}", self.client_id, thread),
            None => format!("client:{}", self.client_id),
        }
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        crate::wire::messages::parse_recipient(&self.id()).map(|_| ())
    }
}

pub(crate) struct Target {
    pub(crate) recipient: Recipient,
    pub(crate) session: Arc<Session>,
    pub(crate) account: Identity,
}
impl Target {
    pub(crate) fn resolve(
        broker: &Broker,
        session: Arc<Session>,
        actor: &Identity,
        recipient: Recipient,
    ) -> Result<Self, String> {
        recipient.validate()?;
        session.require_chat()?;
        if !session.io_reply_advertised() {
            return Err("io_reply_unsupported: selected client has not advertised ssmcp.ui.reply; update the client before sending chat input".into());
        }
        if session.client_id() != recipient.client_id || !authorized(broker, &session, actor) {
            return Err("chat recipient connection is no longer authorized".into());
        }
        let account = match &recipient.thread_id {
            Some(thread) => member(&session, thread)
                .ok_or("chat thread is no longer registered; run ssmcp list again")?,
            None => session
                .owner_identity
                .clone()
                .unwrap_or_else(|| actor.clone()),
        };
        if !authorized(broker, &session, &account) {
            return Err("chat thread account is no longer authorized".into());
        }
        Ok(Self {
            recipient,
            session,
            account,
        })
    }
}

/// Only live collaboration membership is a thread. Controller labels are absent.
pub(crate) fn directory(broker: &Broker, session: &Arc<Session>, actor: &Identity) -> Vec<Value> {
    if !authorized(broker, session, actor) {
        return Vec::new();
    }
    let mut result = Vec::new();
    for (_, entry) in session.collaboration.native_members().unwrap_or_default() {
        let Some(account) = member(session, &entry.agent_id) else {
            continue;
        };
        if !authorized(broker, session, &account) {
            continue;
        }
        result.push(json!({"kind":"thread","client_id":session.client_id(),"thread_id":entry.agent_id,
            "agent_id":entry.agent_id,"name":entry.name,"id":format!("agent:{}:{}",session.client_id(),entry.agent_id),
            "account":{"provider":account.provider,"subject":account.subject,"login":account.login},
            "online":true,"sendable":session.io_reply_advertised(),"reply_native_supported":session.io_reply_advertised()}));
    }
    result.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    result
}

pub(crate) fn process_directory(
    broker: &Broker,
    session: &Arc<Session>,
    actor: &Identity,
) -> Value {
    let info = session.info();
    let mut threads = directory(broker, session, actor);
    let main_account = session.owner_identity.as_ref().unwrap_or(actor);
    let main_sendable = session.io_reply_advertised()
        && authorized(broker, session, actor)
        && authorized(broker, session, main_account);
    threads.insert(0,json!({"id":format!("client:{}",info.id),"kind":"thread","client_id":info.id,"thread_id":null,"agent_id":null,
        "name":"Main / next unbound caller","online":true,"sendable":main_sendable,
        "account":{"provider":main_account.provider,"subject":main_account.subject,"login":main_account.login}}));
    json!({"id":info.id,"name":info.name,"title":info.title,"type":info.mode.name(),"hostname":info.hostname,"cwd":info.cwd,
        "kind":"process","threads":threads,"chat_io_version":1,"reply_native_supported":session.io_reply_advertised(),
        "events_thread_supported":session.thread_events_advertised(),"unavailable_reason":if !session.chat_enabled(){json!(crate::wire::CHAT_DISABLED)}else if session.io_reply_advertised(){Value::Null}else{json!("Native ssmcp.ui.reply is not advertised; send verifies the catalog and otherwise requires a client update.")}})
}

struct Input {
    id: String,
    recipient: Recipient,
    connection: Weak<Session>,
    account: Identity,
    state: &'static str,
}
impl Input {
    fn terminal(&self) -> bool {
        matches!(self.state, "done" | "error" | "expired")
    }
    fn live(&self, broker: &Broker, owner: &Identity) -> bool {
        self.connection.upgrade().is_some_and(|session| {
            authorized(broker, &session, owner)
                && authorized(broker, &session, &self.account)
                && self.recipient.thread_id.as_ref().is_none_or(|thread| {
                    member(&session, thread)
                        .is_some_and(|current| same_account(&current, &self.account))
                })
        })
    }
    fn matches(&self, session: &Arc<Session>, actor: &Identity, thread_id: Option<&str>) -> bool {
        self.recipient.thread_id.as_deref() == thread_id
            && same_account(&self.account, actor)
            && self
                .connection
                .upgrade()
                .is_some_and(|connection| Arc::ptr_eq(&connection, session))
    }
    fn receipt(&self) -> Value {
        json!({"recipient_id":self.recipient.id(),"client_id":self.recipient.client_id,"thread_id":self.recipient.thread_id,
            "input_id":self.id,"state":self.state})
    }
}
struct Request {
    id: String,
    owner: Identity,
    text: String,
    inputs: Vec<Input>,
    outputs: Vec<Value>,
    created_at_ms: u64,
    bytes: usize,
    reserved: usize,
}
impl Request {
    fn refresh(&mut self, broker: &Broker) -> usize {
        let mut released = 0;
        for input in &mut self.inputs {
            if !input.terminal() && !input.live(broker, &self.owner) {
                input.state = "expired";
                released += FINAL_BYTES;
            }
        }
        self.reserved = self
            .reserved
            .checked_sub(released)
            .expect("final output reservation");
        released
    }
    fn page(&self, after: u64, limit: usize, duplicate: bool) -> Value {
        let mut outputs = Vec::new();
        let mut bytes = 4096 + self.inputs.len() * 256;
        let mut next = after;
        for output in self
            .outputs
            .iter()
            .filter(|o| o["seq"].as_u64().unwrap_or(0) > after)
            .take(limit)
        {
            let size = serde_json::to_vec(output).map_or(MAX_TEXT * 6, |v| v.len());
            if !outputs.is_empty() && bytes + size > 128 * 1024 {
                break;
            }
            bytes += size;
            next = output["seq"].as_u64().unwrap_or(next);
            outputs.push(output.clone());
        }
        json!({"schema_version":1,"request_id":self.id,"created_at_ms":self.created_at_ms,
            "expires_at_ms":self.created_at_ms.saturating_add(RETENTION_MS),"retention":"live_server_memory",
            "recipients":self.inputs.iter().map(Input::receipt).collect::<Vec<_>>(),"outputs":outputs,
            "next_after":next,"has_more":self.outputs.last().is_some_and(|v|v["seq"].as_u64().unwrap_or(0)>next),
            "duplicate":duplicate})
    }
}
#[derive(Default)]
struct State {
    requests: VecDeque<Request>,
    bytes: usize,
    sequence: u64,
    generation: Option<String>,
    pruned_through: u64,
}
impl State {
    fn prune(&mut self) {
        let now = now_ms();
        for request in &self.requests {
            if now.saturating_sub(request.created_at_ms) >= RETENTION_MS {
                self.pruned_through = self.pruned_through.max(
                    request
                        .outputs
                        .last()
                        .and_then(|o| o["global_sequence"].as_u64())
                        .unwrap_or(0),
                );
            }
        }
        self.requests
            .retain(|r| now.saturating_sub(r.created_at_ms) < RETENTION_MS);
        self.bytes = self.requests.iter().map(|r| r.bytes).sum();
    }
}
pub(crate) struct OutputQuery<'a> {
    pub(crate) thread_id: Option<&'a str>,
    pub(crate) main: bool,
    pub(crate) cursor: u64,
    pub(crate) generation: Option<&'a str>,
    pub(crate) limit: usize,
}
#[derive(Default)]
pub(crate) struct Hub {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Proposal {
    request_id: String,
    input_id: String,
    output_id: String,
    kind: String,
    state: String,
    text: String,
}
impl Proposal {
    fn parse(value: Value) -> Result<Self, String> {
        let p: Self = serde_json::from_value(value).map_err(|_| "invalid chat reply fields")?;
        crate::wire::messages::validate_request_id(&p.request_id)?;
        for (id, prefix) in [(&p.input_id, "input_"), (&p.output_id, "output_")] {
            let suffix = id
                .strip_prefix(prefix)
                .ok_or("invalid chat input/output identifier")?;
            if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("invalid chat input/output identifier".into());
            }
        }
        if !matches!(
            (p.kind.as_str(), p.state.as_str()),
            ("progress", "running") | ("result", "done" | "error")
        ) {
            return Err("progress requires running; result requires done or error".into());
        }
        text_valid(&p.text)?;
        Ok(p)
    }
    fn matches(&self, output: &Value) -> bool {
        output["request_id"] == self.request_id
            && output["input_id"] == self.input_id
            && output["output_id"] == self.output_id
            && output["kind"] == self.kind
            && output["state"] == self.state
            && output["text"] == self.text
    }
}

impl Hub {
    pub(crate) fn enqueue(
        &self,
        broker: &Broker,
        owner: &Identity,
        request_id: &str,
        mut targets: Vec<Target>,
        text: &str,
    ) -> Result<Value, String> {
        crate::wire::messages::validate_request_id(request_id)?;
        text_valid(text)?;
        if targets.is_empty() || targets.len() > MAX_RECIPIENTS {
            return Err("chat send requires 1..=32 exact main/registered thread recipients".into());
        }
        targets.sort_by_key(|t| t.recipient.id());
        if targets.windows(2).any(|p| p[0].recipient == p[1].recipient) {
            return Err("chat recipient IDs must be distinct".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune();
        if let Some(previous) = state
            .requests
            .iter_mut()
            .find(|r| r.id == request_id && same_account(&r.owner, owner))
        {
            if previous.text != text
                || previous.inputs.len() != targets.len()
                || !previous
                    .inputs
                    .iter()
                    .zip(&targets)
                    .all(|(a, b)| a.recipient == b.recipient)
            {
                return Err("request_id already identifies different text or recipient set".into());
            }
            previous.refresh(broker);
            return Ok(previous.page(0, 64, true));
        }
        for request in &mut state.requests {
            request.refresh(broker);
        }
        let bytes = text.len() + targets.len() * 1024;
        let reserved = targets.len() * FINAL_BYTES;
        let existing_reserve: usize = state.requests.iter().map(|r| r.reserved).sum();
        if state.requests.len() >= MAX_REQUESTS
            || state
                .requests
                .iter()
                .filter(|r| same_account(&r.owner, owner))
                .count()
                >= MAX_ACCOUNT_REQUESTS
            || state
                .bytes
                .saturating_add(existing_reserve)
                .saturating_add(bytes)
                .saturating_add(reserved)
                > MAX_TOTAL_BYTES
        {
            return Err(
                "chat request retention is full; existing request IDs remain retryable".into(),
            );
        }
        // Every target is rechecked at the same no-await decision boundary.
        let mut inputs = Vec::with_capacity(targets.len());
        for target in targets {
            if !target.session.io_reply_advertised()
                || !authorized(broker, &target.session, owner)
                || !authorized(broker, &target.session, &target.account)
                || target.recipient.thread_id.as_ref().is_some_and(|thread| {
                    !member(&target.session, thread)
                        .is_some_and(|a| same_account(&a, &target.account))
                })
            {
                return Err(
                    "chat recipient access changed before enqueue; no input was queued".into(),
                );
            }
            inputs.push(Input {
                id: identifier("input")?,
                recipient: target.recipient,
                connection: Arc::downgrade(&target.session),
                account: target.account,
                state: "queued",
            });
        }
        let request = Request {
            id: request_id.into(),
            owner: owner.clone(),
            text: text.into(),
            inputs,
            outputs: Vec::new(),
            created_at_ms: now_ms(),
            bytes,
            reserved,
        };
        let result = request.page(0, 64, false);
        state.bytes += bytes;
        state.requests.push_back(request);
        drop(state);
        self.changed.notify_waiters();
        Ok(result)
    }

    /// Called only after the response finalizer verifies the actual account and binding.
    pub(crate) fn receive(
        &self,
        broker: &Broker,
        session: &Arc<Session>,
        actor: &Identity,
        thread_id: Option<&str>,
        maximum: usize,
        budget: usize,
    ) -> Vec<Value> {
        if !session.io_reply_advertised()
            || !authorized(broker, session, actor)
            || thread_id.is_some_and(|thread| {
                !member(session, thread).is_some_and(|a| same_account(&a, actor))
            })
        {
            return Vec::new();
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune();
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for request in &mut state.requests {
            request.refresh(broker);
            for input in &mut request.inputs {
                if input.state != "queued" || !input.matches(session, actor, thread_id) {
                    continue;
                }
                let value = json!({"schema_version":1,"request_id":request.id,"input_id":input.id,
                    "recipient_id":input.recipient.id(),"client_id":input.recipient.client_id,"thread_id":input.recipient.thread_id,
                    "text":request.text,"created_at_ms":request.created_at_ms,
                    "source":{"kind":"account_cli","account":{"provider":request.owner.provider,"subject":request.owner.subject}},
                    "reply_tool":"ssmcp.ui.reply","completion_scope":"this_input_only"});
                let size = serde_json::to_vec(&value).map_or(usize::MAX, |b| b.len());
                if result.len() >= maximum.min(MAX_RECIPIENTS)
                    || bytes.saturating_add(size) > budget
                {
                    return result;
                }
                bytes += size;
                input.state = "included";
                result.push(value);
            }
        }
        result
    }

    /// Only the host's successful native reply boundary supplies the actor arguments.
    pub(crate) fn reply(
        &self,
        broker: &Broker,
        session: &Arc<Session>,
        actor: &Identity,
        thread_id: Option<&str>,
        proposal: Value,
    ) -> Result<Value, String> {
        let proposal = Proposal::parse(proposal)?;
        if !session.io_reply_advertised()
            || !authorized(broker, session, actor)
            || thread_id.is_some_and(|thread| {
                !member(session, thread).is_some_and(|a| same_account(&a, actor))
            })
        {
            return Err("chat reply requires a live authenticated thread".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune();
        if state.requests.iter().flat_map(|r| &r.outputs).any(|o| {
            o["output_id"] == proposal.output_id
                && (o["request_id"] != proposal.request_id || o["input_id"] != proposal.input_id)
        }) {
            return Err("output_id already identifies a different input".into());
        }
        let next_global = state
            .sequence
            .checked_add(1)
            .ok_or("chat output sequence exhausted")?;
        let index = state
            .requests
            .iter()
            .position(|r| {
                r.id == proposal.request_id
                    && r.inputs
                        .iter()
                        .any(|i| i.id == proposal.input_id && i.matches(session, actor, thread_id))
            })
            .ok_or("chat input is unavailable for this authenticated thread")?;
        let total = state.bytes;
        let total_reserved: usize = state.requests.iter().map(|r| r.reserved).sum();
        let request = &mut state.requests[index];
        let released = request.refresh(broker);
        let input = request
            .inputs
            .iter_mut()
            .find(|i| i.id == proposal.input_id)
            .ok_or("chat input unavailable")?;
        if let Some(previous) = request
            .outputs
            .iter()
            .find(|o| o["output_id"] == proposal.output_id)
        {
            if !proposal.matches(previous) {
                return Err("output_id already identifies different output".into());
            }
            return Ok(
                json!({"schema_version":1,"accepted":true,"duplicate":true,"output":previous}),
            );
        }
        if input.state != "included" {
            return Err("chat input must be delivered and unfinished before replying".into());
        }
        let final_output = proposal.kind == "result";
        let final_slots = request.reserved / FINAL_BYTES;
        if !final_output && request.outputs.len() + final_slots >= MAX_OUTPUTS {
            return Err("chat progress capacity reached; final result capacity is reserved".into());
        }
        let output = json!({"seq":request.outputs.len()+1,"global_sequence":next_global,"output_id":proposal.output_id,"request_id":request.id,
            "input_id":input.id,"recipient_id":input.recipient.id(),"client_id":input.recipient.client_id,"thread_id":input.recipient.thread_id,
            "kind":proposal.kind,"state":proposal.state,"text":proposal.text,"created_at_ms":now_ms()});
        let bytes = serde_json::to_vec(&output)
            .map_err(|_| "cannot encode chat output")?
            .len();
        let release_final = if final_output { FINAL_BYTES } else { 0 };
        if final_output && bytes > FINAL_BYTES {
            return Err("final output exceeds its reserved envelope; reduce reply text".into());
        }
        let request_reserved = request
            .reserved
            .checked_sub(release_final)
            .expect("pending input reservation");
        let remaining_global = total_reserved
            .checked_sub(released + release_final)
            .expect("global final reservation");
        if request.outputs.len() >= MAX_OUTPUTS
            || request
                .bytes
                .saturating_add(request_reserved)
                .saturating_add(bytes)
                > MAX_REQUEST_BYTES
            || total.saturating_add(remaining_global).saturating_add(bytes) > MAX_TOTAL_BYTES
        {
            return Err(
                "chat progress retention is full; capacity for final replies remains reserved"
                    .into(),
            );
        }
        request.reserved = request_reserved;
        if proposal.state == "done" {
            input.state = "done";
        } else if proposal.state == "error" {
            input.state = "error";
        }
        request.bytes += bytes;
        request.outputs.push(output.clone());
        state.bytes += bytes;
        state.sequence = next_global;
        drop(state);
        self.changed.notify_waiters();
        Ok(json!({"schema_version":1,"accepted":true,"duplicate":false,"output":output}))
    }

    /// Keep enqueue and this idle decision ordered for the exact recipient.
    pub(crate) fn if_thread_drained(
        &self,
        session: &Arc<Session>,
        actor: &Identity,
        thread: &str,
        settle: impl FnOnce(),
    ) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .requests
            .iter()
            .flat_map(|r| &r.inputs)
            .any(|i| !i.terminal() && i.matches(session, actor, Some(thread)))
        {
            settle();
        }
    }

    pub(crate) fn poll(
        &self,
        broker: &Broker,
        owner: &Identity,
        request_id: &str,
        after: u64,
        limit: usize,
    ) -> Result<Value, String> {
        crate::wire::messages::validate_request_id(request_id)?;
        if !(1..=64).contains(&limit) {
            return Err("chat output page limit must be in 1..=64".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune();
        let request = state
            .requests
            .iter_mut()
            .find(|r| r.id == request_id && same_account(&r.owner, owner))
            .ok_or("chat request is unavailable for this account or its retention expired")?;
        request.refresh(broker);
        if after > request.outputs.len() as u64 {
            return Err("chat cursor exceeds this request's output sequence".into());
        }
        Ok(request.page(after, limit, false))
    }
    pub(crate) fn outputs(
        &self,
        broker: &Broker,
        session: &Arc<Session>,
        actor: &Identity,
        query: OutputQuery<'_>,
    ) -> Result<Value, String> {
        let OutputQuery {
            thread_id,
            main,
            cursor,
            generation,
            limit,
        } = query;
        if !(1..=64).contains(&limit) || (main && thread_id.is_some()) {
            return Err("invalid chat output scope or limit".into());
        }
        if !authorized(broker, session, actor) {
            return Err("chat output connection is no longer authorized".into());
        }
        if thread_id.is_some_and(|thread| member(session, thread).is_none()) {
            return Err("chat output thread is unavailable".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.prune();
        if state.generation.is_none() {
            state.generation = Some(identifier("io_generation")?);
        }
        let current = state.generation.as_deref().expect("output generation");
        if generation.is_some_and(|g| g != current) {
            return Err(
                "chat output generation changed; explicitly reconnect before reading a new stream"
                    .into(),
            );
        }
        if cursor > state.sequence {
            return Err("chat output cursor exceeds current stream".into());
        }
        let mut matching = Vec::new();
        for request in &state.requests {
            if !same_account(&request.owner, actor) {
                continue;
            }
            for input in &request.inputs {
                if !input
                    .connection
                    .upgrade()
                    .is_some_and(|s| Arc::ptr_eq(&s, session))
                    || !input.live(broker, actor)
                {
                    continue;
                }
                if main && input.recipient.thread_id.is_some() {
                    continue;
                }
                if thread_id.is_some() && input.recipient.thread_id.as_deref() != thread_id {
                    continue;
                }
                for output in request.outputs.iter().filter(|o| o["input_id"] == input.id) {
                    let sequence = output["global_sequence"].as_u64().unwrap_or(0);
                    if sequence > cursor {
                        matching.push(output);
                    }
                }
            }
        }
        matching.sort_by_key(|o| o["global_sequence"].as_u64().unwrap_or(0));
        let mut outputs = Vec::new();
        let mut bytes = 4096;
        let mut next = cursor;
        for &output in &matching {
            let size = serde_json::to_vec(output)
                .map_err(|_| "cannot encode chat output")?
                .len();
            if outputs.len() >= limit || (!outputs.is_empty() && bytes + size > 128 * 1024) {
                break;
            }
            bytes += size;
            next = output["global_sequence"].as_u64().unwrap_or(next);
            outputs.push(output);
        }
        let more = outputs.len() < matching.len();
        if !more {
            next = state.sequence;
        }
        Ok(
            json!({"schema_version":1,"scope":{"client_id":session.client_id(),"thread_id":thread_id,"kind":if main{"main"}else if thread_id.is_some(){"thread"}else{"process"},"account":"request_owner"},
            "generation":current,"outputs":outputs,"next_cursor":next,"has_more":more,"retention":"live_server_memory","retention_gap_possible":cursor<state.pruned_through,"retention_gap_basis":"server_global_prune_watermark"}),
        )
    }
    pub(crate) async fn wait(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.changed.notified()).await;
    }
}

#[cfg(test)]
pub(crate) struct TestFixture {
    pub(crate) broker: Arc<Broker>,
    pub(crate) session: Arc<Session>,
    pub(crate) actor: Identity,
    pub(crate) access_token: String,
    calls: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<super::Call>>,
    _reports: tokio::sync::mpsc::Receiver<crate::wire::CodeCompletion>,
}
#[cfg(test)]
impl TestFixture {
    pub(crate) async fn new() -> Self {
        let broker = Arc::new(Broker::new("https://ssmcp.test".into()));
        let actor = Identity {
            provider: "github".into(),
            subject: "42".into(),
            login: "octocat".into(),
        };
        let mut session =
            super::tests::allowed_session(&"a".repeat(64), "fixture", vec![actor.clone()]);
        let (sender, calls) = tokio::sync::mpsc::channel(8);
        let (reports, reports_rx) = tokio::sync::mpsc::channel(8);
        let inner = Arc::get_mut(&mut session).expect("isolated fixture session");
        inner.sender = sender;
        inner.reports = reports;
        broker
            .sessions
            .write()
            .await
            .entry(super::IDENTITY_RESOURCE.into())
            .or_default()
            .insert(session.client_id().into(), session.clone());
        let access_token = broker
            .oauth
            .test_identity_tokens(super::IDENTITY_RESOURCE, "42")["access_token"]
            .as_str()
            .unwrap()
            .into();
        Self {
            broker,
            session,
            actor,
            access_token,
            calls: tokio::sync::Mutex::new(calls),
            _reports: reports_rx,
        }
    }
    pub(crate) async fn join(&self, name: &str) -> Value {
        let owner = crate::bridge::owner(
            super::IDENTITY_RESOURCE,
            &Authorization::Identity(self.actor.clone()),
        )
        .unwrap();
        self.session
            .collaboration
            .dispatch(&owner, "join", json!({"name":name}))
            .await
            .unwrap()
    }
    pub(crate) async fn advertise(&mut self) {
        let session = self.session.clone();
        let authorization = Authorization::Identity(self.actor.clone());
        let call = session.call_with_authorization(
            crate::wire::Operation::ListTools { cursor: None },
            None,
            &authorization,
        );
        let reply=self.native_reply(json!({"tools":[{"name":"ssmcp.ui.reply","inputSchema":{"type":"object"}},
            {"name":"ssmcp.ui.events","inputSchema":{"type":"object","properties":{"action":{"const":"thread"}}}}]}));
        let (result, operation) = tokio::join!(call, reply);
        result.unwrap();
        assert!(matches!(
            serde_json::from_value::<crate::wire::Operation>(operation).unwrap(),
            crate::wire::Operation::ListTools { .. }
        ));
        assert!(self.session.io_reply_advertised());
        assert!(self.session.thread_events_advertised());
    }
    pub(crate) fn enqueue(&self, request_id: &str, threads: &[Option<&str>]) -> Value {
        let targets = threads
            .iter()
            .map(|thread| {
                Target::resolve(
                    &self.broker,
                    self.session.clone(),
                    &self.actor,
                    Recipient {
                        client_id: self.session.client_id().into(),
                        thread_id: thread.map(str::to_owned),
                    },
                )
                .unwrap()
            })
            .collect();
        self.broker
            .session_io
            .enqueue(
                &self.broker,
                &self.actor,
                request_id,
                targets,
                "fixture chat input",
            )
            .unwrap()
    }
    /// Real Session call channel with an isolated native responder, no provider/model.
    pub(crate) fn revoke_access(&self) {
        Self::revoke_session_access(&self.session);
    }
    pub(crate) fn revoke_session_access(session: &Session) {
        session.allowed().clear();
    }
    pub(crate) async fn native_reply_with(&self, reply: impl FnOnce(&Value) -> Value) -> Value {
        let call = self.calls.lock().await.recv().await.expect("native call");
        let operation = serde_json::to_value(&call.operation).unwrap();
        let result = reply(&operation);
        let _ = call.reply.send(Ok(result));
        operation
    }

    pub(crate) fn input_receipt(operation: &Value) -> Value {
        assert_eq!(operation["name"], "ssmcp.ui.message");
        let args = &operation["arguments"];
        assert_eq!(args["kind"], "input");
        assert!(matches!(
            args["input_origin"].as_str(),
            Some("system_hook" | "worker_event" | "maintenance")
        ));
        assert_ne!(args["finish_task"], true);
        let id = crate::client::interface::history::request_identity(
            args["idempotency_key"].as_str().unwrap(),
            "generated_input",
        )
        .unwrap();
        json!({"isError":false,"structuredContent":{
            "accepted":true,"recorded":true,"shown":false,"kind":"input",
            "input_origin":args["input_origin"],"message_id":id,
            "completion_permission":false
        }})
    }

    pub(crate) async fn native_reply(&mut self, result: Value) -> Value {
        self.native_reply_after(result, || {}).await
    }
    pub(crate) async fn native_reply_after(
        &mut self,
        result: Value,
        before: impl FnOnce(),
    ) -> Value {
        let call = self.calls.lock().await.recv().await.expect("native call");
        let operation = serde_json::to_value(&call.operation).unwrap();
        before();
        let _ = call.reply.send(Ok(result));
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replies_require_explicit_request_input_output_and_completion_scope() {
        let valid = json!({"request_id":"io_fixture","input_id":format!("input_{}","1".repeat(32)),"output_id":format!("output_{}","2".repeat(32)),"kind":"result","state":"done","text":"actual response"});
        assert!(Proposal::parse(valid.clone()).is_ok());
        for (key, value) in [
            ("agent_id", json!("spoof")),
            ("state", json!("acknowledged")),
            ("kind", json!("answer")),
            ("text", json!("\u{1b}[2J")),
        ] {
            let mut bad = valid.clone();
            bad[key] = value;
            assert!(Proposal::parse(bad).is_err());
        }
        let mut progress = valid;
        progress["kind"] = json!("progress");
        progress["state"] = json!("running");
        assert!(Proposal::parse(progress).is_ok());
    }
    #[test]
    fn chat_targets_are_canonical_registered_threads_not_labels_or_process_inboxes() {
        let valid = Recipient {
            client_id: "a".repeat(64),
            thread_id: Some(format!("agent_{}", "b".repeat(32))),
        };
        assert!(valid.validate().is_ok());
        assert!(
            Recipient {
                thread_id: Some("assistant".into()),
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            Recipient {
                thread_id: None,
                ..valid
            }
            .validate()
            .is_ok()
        );
    }
    fn reply(input: &Value, index: usize, kind: &str, state: &str, text: &str) -> Value {
        json!({"request_id":input["request_id"],"input_id":input["input_id"],"output_id":format!("output_{index:032x}"),"kind":kind,"state":state,"text":text})
    }
    #[tokio::test]
    async fn chat_io_exact_main_thread_fanout_correlated_results_and_retries() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        let first = fixture.join("same label").await;
        let second = fixture.join("same label").await;
        let one = first["agent_id"].as_str().unwrap();
        let two = second["agent_id"].as_str().unwrap();
        let sent = fixture.enqueue("io_isolation", &[None, Some(one), Some(two)]);
        assert_eq!(sent["recipients"].as_array().unwrap().len(), 3);
        let repeated = fixture.enqueue("io_isolation", &[Some(two), None, Some(one)]);
        assert_eq!(sent["recipients"], repeated["recipients"]);
        assert_eq!(repeated["duplicate"], true);
        let hub = &fixture.broker.session_io;
        let mine = hub.receive(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            Some(one),
            32,
            100_000,
        );
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["thread_id"], one);
        assert!(
            hub.receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                Some(one),
                32,
                100_000
            )
            .is_empty()
        );
        let main = hub.receive(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            None,
            32,
            100_000,
        );
        assert_eq!(main.len(), 1);
        assert!(main[0]["thread_id"].is_null());
        assert!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                Some(one),
                reply(&main[0], 1, "result", "done", "stolen main")
            )
            .is_err()
        );
        assert!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                reply(&mine[0], 1, "result", "done", "stolen thread")
            )
            .is_err()
        );
        let progress = reply(&main[0], 1, "progress", "running", "working");
        hub.reply(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            None,
            progress,
        )
        .unwrap();
        let page = hub
            .poll(&fixture.broker, &fixture.actor, "io_isolation", 0, 64)
            .unwrap();
        assert_eq!(page["outputs"][0]["kind"], "progress");
        assert!(
            page["recipients"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["state"] != "done")
        );
        let final_reply = reply(&main[0], 2, "result", "done", "actual main answer");
        assert_eq!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                final_reply.clone()
            )
            .unwrap()["duplicate"],
            false
        );
        assert_eq!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                final_reply.clone()
            )
            .unwrap()["duplicate"],
            true
        );
        let mut changed = final_reply;
        changed["text"] = json!("different");
        assert!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                changed
            )
            .is_err()
        );
        hub.reply(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            Some(one),
            reply(&mine[0], 3, "result", "error", "actual thread error"),
        )
        .unwrap();
        let page = hub
            .poll(&fixture.broker, &fixture.actor, "io_isolation", 0, 64)
            .unwrap();
        assert_eq!(page["outputs"].as_array().unwrap().len(), 3);
        assert!(
            page["recipients"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["thread_id"] == two && r["state"] == "queued"),
            "another completed recipient must not cancel pending work"
        );
        let only_main = hub
            .outputs(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                OutputQuery {
                    thread_id: None,
                    main: true,
                    cursor: 0,
                    generation: None,
                    limit: 64,
                },
            )
            .unwrap();
        assert_eq!(only_main["outputs"].as_array().unwrap().len(), 2);
        assert!(
            only_main["outputs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|o| o["thread_id"].is_null())
        );
        let stranger = Identity {
            subject: "7".into(),
            ..fixture.actor.clone()
        };
        assert!(
            hub.poll(&fixture.broker, &stranger, "io_isolation", 0, 64)
                .is_err()
        );
        assert!(
            hub.receive(
                &fixture.broker,
                &fixture.session,
                &stranger,
                None,
                32,
                100_000
            )
            .is_empty()
        );
        fixture.session.allowed().clear();
        let expired = hub
            .poll(&fixture.broker, &fixture.actor, "io_isolation", 3, 64)
            .unwrap();
        assert!(
            expired["recipients"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["thread_id"] == two && r["state"] == "expired")
        );
    }
    #[tokio::test]
    async fn chat_io_budget_and_replaced_connection_do_not_consume_or_complete() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        fixture.enqueue("io_budget", &[None]);
        let hub = &fixture.broker.session_io;
        assert!(
            hub.receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                32,
                1
            )
            .is_empty()
        );
        let page = hub
            .poll(&fixture.broker, &fixture.actor, "io_budget", 0, 1)
            .unwrap();
        assert_eq!(page["recipients"][0]["state"], "queued");
        let delivered = hub.receive(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            None,
            32,
            100_000,
        );
        assert_eq!(delivered.len(), 1);
        let replaced = super::super::tests::allowed_session(
            fixture.session.client_id(),
            "fixture",
            vec![fixture.actor.clone()],
        );
        assert!(
            hub.reply(
                &fixture.broker,
                &replaced,
                &fixture.actor,
                None,
                reply(&delivered[0], 1, "result", "done", "wrong connection")
            )
            .is_err()
        );
        assert!(
            hub.poll(&fixture.broker, &fixture.actor, "io_budget", 1, 64)
                .is_err()
        );
    }
    #[tokio::test]
    async fn chat_io_progress_reserves_a_full_final_reply_without_sequence_gaps() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        let hub = &fixture.broker.session_io;
        fixture.enqueue("io_byte_reserve", &[None]);
        let input = hub
            .receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                32,
                100_000,
            )
            .pop()
            .unwrap();
        let large = "\"".repeat(MAX_TEXT);
        let mut count = 0usize;
        for index in 1..MAX_OUTPUTS {
            if hub
                .reply(
                    &fixture.broker,
                    &fixture.session,
                    &fixture.actor,
                    None,
                    reply(&input, index, "progress", "running", &large),
                )
                .is_err()
            {
                break;
            }
            count += 1;
        }
        assert!(
            count > 0 && count < MAX_OUTPUTS - 1,
            "byte reserve should bound progress before all slots fill"
        );
        let completed = hub
            .reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                reply(&input, 1000, "result", "done", &large),
            )
            .unwrap();
        assert_eq!(completed["output"]["seq"], count + 1);
        assert!(serde_json::to_vec(&completed["output"]).unwrap().len() <= FINAL_BYTES);
        fixture.enqueue("io_slot_reserve", &[None]);
        let input = hub
            .receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                32,
                100_000,
            )
            .pop()
            .unwrap();
        for index in 1..MAX_OUTPUTS {
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                reply(&input, 2000 + index, "progress", "running", "p"),
            )
            .unwrap();
        }
        assert!(
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                reply(&input, 3000, "progress", "running", "one too many")
            )
            .is_err()
        );
        let completed = hub
            .reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                reply(&input, 3001, "result", "error", &large),
            )
            .unwrap();
        assert_eq!(completed["output"]["seq"], MAX_OUTPUTS);
        let state = hub.state.lock().unwrap();
        assert!(state.requests.iter().all(|r| r.reserved == 0));
        assert!(state.bytes <= MAX_TOTAL_BYTES);
    }
    #[tokio::test]
    async fn chat_io_filtered_sequence_gaps_are_not_retention_loss() {
        let mut fixture = TestFixture::new().await;
        fixture.advertise().await;
        let agent = fixture.join("worker").await;
        let agent = agent["agent_id"].as_str().unwrap();
        fixture.enqueue("io_other_scope", &[Some(agent)]);
        let hub = &fixture.broker.session_io;
        let other = hub
            .receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                Some(agent),
                32,
                100_000,
            )
            .pop()
            .unwrap();
        hub.reply(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            Some(agent),
            reply(&other, 1, "progress", "running", "other scope"),
        )
        .unwrap();
        let empty = hub
            .outputs(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                OutputQuery {
                    thread_id: None,
                    main: true,
                    cursor: 0,
                    generation: None,
                    limit: 64,
                },
            )
            .unwrap();
        assert!(empty["outputs"].as_array().unwrap().is_empty());
        assert_eq!(empty["next_cursor"], 1);
        for index in 2..5 {
            hub.reply(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                Some(agent),
                reply(
                    &other,
                    index,
                    "progress",
                    "running",
                    "unrelated scope output",
                ),
            )
            .unwrap();
        }
        fixture.enqueue("io_first_main", &[None]);
        let main = hub
            .receive(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                None,
                32,
                100_000,
            )
            .pop()
            .unwrap();
        hub.reply(
            &fixture.broker,
            &fixture.session,
            &fixture.actor,
            None,
            reply(&main, 5, "result", "done", "first main output"),
        )
        .unwrap();
        let current = hub
            .outputs(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                OutputQuery {
                    thread_id: None,
                    main: true,
                    cursor: 1,
                    generation: empty["generation"].as_str(),
                    limit: 64,
                },
            )
            .unwrap();
        assert_eq!(current["retention_gap_possible"], false);
        hub.state
            .lock()
            .unwrap()
            .requests
            .iter_mut()
            .find(|r| r.id == "io_other_scope")
            .unwrap()
            .created_at_ms = now_ms().saturating_sub(RETENTION_MS + 1);
        let pruned = hub
            .outputs(
                &fixture.broker,
                &fixture.session,
                &fixture.actor,
                OutputQuery {
                    thread_id: None,
                    main: true,
                    cursor: 0,
                    generation: None,
                    limit: 64,
                },
            )
            .unwrap();
        assert_eq!(pruned["retention_gap_possible"], true);
        assert_eq!(
            pruned["retention_gap_basis"],
            "server_global_prune_watermark"
        );
    }
}
