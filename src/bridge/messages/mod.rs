//! Relay-authenticated peer candidates. The only constructor of Deliver envelopes.
mod cache;
mod schema;
#[cfg(test)]
mod tests;

use super::{Access, owner, parse};
use crate::{
    broker::Authorization,
    messaging::{self, BoundParticipant, NativeRequest, PeerMessage, Principal, Source, Target},
    wire::{ControlAccount, ControlAgent, Operation},
};
pub(crate) use cache::Router;
use cache::{Batch, Delivery, Guard};
use futures_util::{StreamExt, stream};
pub(crate) use schema::tool;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const FANOUT: usize = 4;
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
enum Selector {
    Session {
        client_id: String,
        session_id: String,
        thread_id: Option<String>,
    },
    Targets {
        targets: Vec<Target>,
    },
    Process {
        process_id: String,
    },
    Global {},
}
impl Selector {
    fn targets(&self) -> Option<Vec<Target>> {
        match self {
            Self::Session {
                client_id,
                session_id,
                thread_id,
            } => Some(vec![Target {
                client_id: client_id.clone(),
                session_id: session_id.clone(),
                thread_id: thread_id.clone(),
            }]),
            Self::Targets { targets } => Some(targets.clone()),
            _ => None,
        }
    }
    fn validate(&mut self) -> Result<(), String> {
        if let Some(targets) = self.targets() {
            if targets.is_empty() || targets.len() > messaging::MAX_BATCH {
                return Err("Select 1..=128 explicit live recipients".into());
            }
            for target in &targets {
                target.validate()?;
            }
        }
        if let Self::Targets { targets } = self {
            targets.sort();
            targets.dedup();
        }
        if let Self::Process { process_id } = self
            && !messaging::valid_id(process_id, "process", 16)
        {
            return Err("Use the native process generation from messages.list".into());
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentProof {
    client_id: String,
    id: String,
    token: String,
}
impl AgentProof {
    fn validate(&self) -> Result<(), String> {
        if !messaging::client_id(&self.client_id)
            || !messaging::valid_id(&self.id, "agent", 16)
            || self.token.len() != 64
            || !self.token.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("Agent credentials must identify a current registered participant".into());
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    to: Selector,
    text: String,
    topic: Option<String>,
    reply_to: Option<String>,
    idempotency_key: String,
    #[serde(default = "default_ttl")]
    ttl_ms: u64,
    from_client_id: Option<String>,
    agent: Option<AgentProof>,
}
fn default_ttl() -> u64 {
    messaging::DEFAULT_TTL_MS
}
fn default_limit() -> usize {
    messaging::MAX_READ_MESSAGES
}
fn default_wait() -> u64 {
    1000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    client_id: Option<String>,
    process_id: Option<String>,
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    include_threads: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    target: Target,
    #[serde(default)]
    after: u64,
    #[serde(default = "default_limit")]
    limit: usize,
    agent: Option<AgentProof>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    target: Target,
    #[serde(default)]
    after: u64,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_wait")]
    wait_ms: u64,
    agent: Option<AgentProof>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    target: Target,
    #[serde(default)]
    ids: Vec<String>,
    through: Option<u64>,
    agent: Option<AgentProof>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    message_id: String,
    #[serde(default)]
    refresh: bool,
}

#[derive(Clone, Deserialize, Serialize)]
struct Capability {
    version: u32,
    process_id: String,
    pid: u32,
    session_id: String,
    state: String,
}
impl Capability {
    fn validate(&self) -> Result<(), String> {
        if self.version != messaging::VERSION
            || self.state != "live"
            || self.pid == 0
            || !messaging::valid_id(&self.process_id, "process", 16)
            || !messaging::valid_id(&self.session_id, "session", 16)
        {
            return Err(
                "Client has no supported live peer candidate inbox; update that client".into(),
            );
        }
        Ok(())
    }
    fn target(&self, id: &str) -> Target {
        Target {
            client_id: id.into(),
            session_id: self.session_id.clone(),
            thread_id: None,
        }
    }
}

fn brief(mut error: String) -> String {
    if error.len() > 512 {
        let mut end = 512;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
    }
    error
}

async fn capability(access: &Access, id: &str) -> Result<Capability, String> {
    let value = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        access.call(id, "ssmcp.client.capabilities", json!({})),
    )
    .await
    .map_err(|_| "Client capability discovery timed out")?
    .map_err(brief)?;
    let capability: Capability = serde_json::from_value(value["peer_messages"].clone()).map_err(
        |_| "Client does not support peer candidate messages; no private operation was sent",
    )?;
    capability.validate()?;
    Ok(capability)
}

async fn current_target(access: &Access, target: &Target) -> Result<(), String> {
    target.validate()?;
    let capability = capability(access, &target.client_id).await?;
    if capability.session_id != target.session_id {
        return Err("Peer session generation changed; refresh messages.list".into());
    }
    if let Some(id) = &target.thread_id {
        let session = access.session(&target.client_id).await?;
        if !session
            .collaboration
            .presence()?
            .iter()
            .any(|agent| &agent.agent_id == id)
        {
            return Err(
                "Target participant is no longer registered on this live connection".into(),
            );
        }
    }
    Ok(())
}

async fn participant(
    access: &Access,
    client_id: &str,
    proof: Option<&AgentProof>,
) -> Result<BoundParticipant, String> {
    let (_, authorization) = access.authorized().await?;
    let owner = owner(&access.user, &authorization)?;
    let session = access.session(client_id).await?;
    let (agent_id, token) = if let Some(proof) = proof {
        proof.validate()?;
        if proof.client_id != client_id {
            return Err("Agent credential belongs to another client".into());
        }
        if let Some(binding) = access
            .audit_run
            .as_ref()
            .and_then(|run| run.binding(client_id))
            && (binding.owner != owner || binding.agent_id != proof.id)
        {
            return Err(
                "Agent credential differs from this run's immutable participant binding".into(),
            );
        }
        (proof.id.clone(), proof.token.clone())
    } else {
        let binding = access
            .audit_run
            .as_ref()
            .and_then(|run| run.binding(client_id))
            .ok_or("Bind this participant in code_mode or supply its private agent credential")?;
        if binding.owner != owner {
            return Err("Participant belongs to another authenticated principal".into());
        }
        (binding.agent_id, binding.token)
    };
    let agent = session.collaboration.bind(&owner, &agent_id, &token)?;
    Ok(BoundParticipant {
        client_id: client_id.into(),
        agent: ControlAgent {
            id: agent.agent_id,
            name: agent.name,
        },
    })
}

async fn authorize_mailbox(
    access: &Access,
    target: &Target,
    proof: Option<&AgentProof>,
) -> Result<(), String> {
    access.session(&target.client_id).await?;
    if let Some(thread_id) = &target.thread_id {
        let bound = participant(access, &target.client_id, proof).await?;
        if &bound.agent.id != thread_id {
            return Err(
                "Reading or acknowledging a participant inbox requires that participant's binding"
                    .into(),
            );
        }
    } else if proof.is_some() {
        // Reject accidental credentials on a different shared mailbox instead of silently ignoring them.
        participant(access, &target.client_id, proof).await?;
    }
    Ok(())
}

async fn source(access: &Access, args: &Send) -> Result<Source, String> {
    let (_, authorization) = access.authorized().await?;
    let principal = match authorization {
        Authorization::Identity(identity) => Principal::Account {
            account: ControlAccount {
                provider: identity.provider,
                subject: identity.subject,
                login: identity.login,
            },
        },
        Authorization::Legacy { user, anchor } => Principal::Connection {
            user,
            anchor_client_id: anchor
                .upgrade()
                .filter(|s| s.is_alive())
                .ok_or("Authentication connection ended")?
                .client_id()
                .into(),
        },
    };
    principal.validate()?;
    let default_client = if args.from_client_id.is_none() && args.agent.is_none() {
        access
            .audit_run
            .as_ref()
            .map(|run| run.peer_source_client())
            .transpose()?
            .flatten()
    } else {
        None
    };
    let client = args
        .from_client_id
        .as_deref()
        .or_else(|| args.agent.as_ref().map(|proof| proof.client_id.as_str()))
        .or(default_client.as_deref());
    let participant = match client {
        Some(client) => Some(participant(access, client, args.agent.as_ref()).await?),
        None => None,
    };
    Ok(Source {
        principal,
        run_id: access
            .audit_run
            .as_ref()
            .map(|run| run.context.run_id.clone()),
        participant,
        controller: access
            .audit_run
            .as_ref()
            .and_then(|run| run.controller.clone()),
    })
}

async fn native(access: &Access, target: &Target, request: NativeRequest) -> Result<Value, String> {
    let (broker, authorization) = access.authorized().await?;
    let session = broker
        .client(&access.user, &authorization, &target.client_id)
        .await?;
    let result = access
        .native(
            &session,
            &authorization,
            Operation::PeerMessages { request },
        )
        .await?;
    access.session(&target.client_id).await?;
    Ok(result)
}

async fn list(access: &Access, args: List) -> Result<Value, String> {
    if !(1..=messaging::MAX_READ_MESSAGES).contains(&args.limit)
        || args
            .client_id
            .as_ref()
            .is_some_and(|id| !messaging::client_id(id))
        || args
            .cursor
            .as_ref()
            .is_some_and(|id| !messaging::client_id(id))
        || args
            .process_id
            .as_ref()
            .is_some_and(|id| !messaging::valid_id(id, "process", 16))
        || (args.include_threads && args.client_id.is_none())
    {
        return Err(
            "list needs exact IDs, a limit of 1..=32, and client_id when include_threads is true"
                .into(),
        );
    }
    let (broker, authorization) = access.authorized().await?;
    let mut sessions = broker.clients(&access.user, &authorization).await?;
    sessions.retain(|session| {
        args.client_id
            .as_ref()
            .is_none_or(|id| id == session.client_id())
            && args
                .cursor
                .as_ref()
                .is_none_or(|cursor| session.client_id() > cursor.as_str())
    });
    let more = sessions.len() > args.limit;
    sessions.truncate(args.limit);
    let cursor = sessions
        .last()
        .map(|session| session.client_id().to_string());
    let mut results = stream::iter(sessions.into_iter().map(|session| async move {
        let result = capability(access, session.client_id()).await;
        (session, result)
    }))
    .buffer_unordered(FANOUT)
    .collect::<Vec<_>>()
    .await;
    results.sort_by(|a, b| a.0.client_id().cmp(b.0.client_id()));
    let mut online = Vec::new();
    let mut unavailable = Vec::new();
    for (session, result) in results {
        // A session that was revoked during discovery contributes no native data.
        if access.session(session.client_id()).await.is_err() {
            continue;
        }
        match result {
            Ok(capability) if args.process_id.as_ref().is_none_or(|id|id==&capability.process_id)=>{
                let info=session.info();
                let threads=if args.include_threads {Some(session.collaboration.presence()?)}else{None};
                online.push(json!({"target":capability.target(session.client_id()),"process_id":capability.process_id,
                    "pid":capability.pid,"name":info.name,"title":info.title,"threads":threads,
                    "thread_kind":"registered collaboration participant","candidate_version":capability.version}));
            }
            Ok(_)=>{},
            Err(error)=>unavailable.push(json!({"client_id":session.client_id(),"state":"unsupported_or_unavailable","error":error})),
        }
    }
    access.authorized().await?;
    Ok(
        json!({"sessions":online,"unavailable":unavailable,"next_cursor":if more {cursor}else{None},
        "snapshot":true,"server_time_ms":messaging::now_ms(),"limits":messaging::limits(),"receipts":Router::limits(),
        "process_scope":"Current runtime sessions sharing one native process generation; not PTYs or historical conversations"}),
    )
}

async fn resolve(access: &Access, selector: &Selector) -> Result<Vec<Delivery>, String> {
    // This single ACL-filtered connection snapshot is never expanded during retry.
    let (broker, authorization) = access.authorized().await?;
    let sessions = broker.clients(&access.user, &authorization).await?;
    let explicit = selector.targets();
    let ids: Vec<String> = if let Some(targets) = &explicit {
        targets
            .iter()
            .map(|t| t.client_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    } else {
        sessions.iter().map(|s| s.client_id().into()).collect()
    };
    if ids.len() > messaging::MAX_BATCH {
        return Err("More than 128 live targets; select explicit recipients or one process".into());
    }
    let authorized: BTreeSet<_> = sessions.iter().map(|s| s.client_id().to_string()).collect();
    let discovered = stream::iter(ids.into_iter().map(|id| {
        let allowed = authorized.contains(&id);
        async move {
            let result = if allowed {
                capability(access, &id).await
            } else {
                Err("Client is unavailable or unauthorized".into())
            };
            (id, result)
        }
    }))
    .buffer_unordered(FANOUT)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let mut recipients = Vec::new();
    if let Some(targets) = explicit {
        for target in targets {
            let mut recipient = Delivery::pending(target.client_id.clone(), Some(target.clone()));
            match &discovered[&target.client_id] {
                Ok(capability) if capability.session_id == target.session_id => {}
                Ok(_) => recipient.fail(
                    "stale",
                    "Target runtime session changed; no message was sent",
                ),
                Err(error) => recipient.fail("unsupported_or_unavailable", error.clone()),
            }
            recipients.push(recipient);
        }
    } else {
        for (id, result) in discovered {
            match result {
                Ok(capability) => {
                    if let Selector::Process { process_id } = selector
                        && &capability.process_id != process_id
                    {
                        continue;
                    }
                    recipients.push(Delivery::pending(id.clone(), Some(capability.target(&id))));
                }
                Err(error) => {
                    // A failed discovery cannot prove process membership. Report it without sending.
                    let mut recipient = Delivery::pending(id, None);
                    recipient.fail("unsupported_or_unavailable", error);
                    recipients.push(recipient);
                }
            }
        }
    }
    Ok(recipients)
}

fn delivery_id(message_id: &str, target: &Target) -> String {
    let mut hash = Sha256::new();
    hash.update(message_id.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(target).expect("fixed peer target serializes"));
    format!("delivery_{:x}", hash.finalize())
}

async fn deliver(access: &Access, batch: &Batch, args: &Send, mut recipient: Delivery) -> Delivery {
    if recipient.state != "pending" {
        return recipient;
    }
    let target = recipient
        .target
        .clone()
        .expect("pending recipient has a validated target");
    // Check target membership once more immediately before admission. No source field comes from arguments.
    let session = match access.session(&target.client_id).await {
        Ok(session) => session,
        Err(error) => {
            recipient.fail("unavailable", error);
            return recipient;
        }
    };
    if let Some(thread_id) = &target.thread_id {
        match session.collaboration.presence() {
            Ok(agents) if agents.iter().any(|agent| &agent.agent_id == thread_id) => {}
            _ => {
                recipient.fail("unavailable", "Target participant is no longer online");
                return recipient;
            }
        }
    }
    let id = recipient.delivery_id.clone().expect("resolved delivery ID");
    let message = PeerMessage {
        message_id: batch.message_id.clone(),
        delivery_id: id,
        source: batch.source.clone(),
        target: target.clone(),
        text: args.text.clone(),
        topic: args.topic.clone(),
        reply_to: args.reply_to.clone(),
        sent_at_ms: batch.sent_at_ms,
        ttl_ms: args.ttl_ms,
    };
    match native(
        access,
        &target,
        NativeRequest::Deliver {
            message: Box::new(message),
        },
    )
    .await
    {
        Ok(result) => {
            if result["state"] == "rejected"
                && result["accepted"] == false
                && result["delivery_id"].as_str() == recipient.delivery_id.as_deref()
            {
                recipient.fail(
                    "rejected",
                    result["error"]
                        .as_str()
                        .unwrap_or("Native inbox refused the message"),
                );
                recipient.receipt = Some(json!({"state":"rejected","accepted":false}));
                return recipient;
            }
            // Retain only bounded acknowledgement fields, never arbitrary client result data.
            if matches!(
                result["state"].as_str(),
                Some("candidate" | "acknowledged" | "expired")
            ) && result["delivery_id"].as_str() == recipient.delivery_id.as_deref()
                && result["sequence"]
                    .as_u64()
                    .is_some_and(|n| n > 0 && n < (1 << 53))
            {
                recipient.state = "accepted".into();
                recipient.receipt = Some(
                    json!({"state":result["state"],"sequence":result["sequence"].as_u64(),"duplicate":result["duplicate"]==true}),
                );
            } else {
                recipient.fail(
                    "unknown",
                    "Client returned no valid delivery acknowledgement",
                );
            }
        }
        Err(error) => recipient.fail("unknown", error),
    }
    recipient
}

async fn send(access: &Access, mut args: Send) -> Result<Value, String> {
    args.to.validate()?;
    messaging::text(&args.text, "peer text", messaging::MAX_TEXT_BYTES, true)?;
    if let Some(topic) = &args.topic {
        messaging::text(topic, "peer topic", 128, false)?;
    }
    if !(1..=messaging::MAX_TTL_MS).contains(&args.ttl_ms)
        || args.reply_to.as_ref().is_some_and(|id| {
            !messaging::valid_id(id, "peer", 16) && !messaging::valid_id(id, "delivery", 32)
        })
    {
        return Err("Invalid peer TTL or reply_to ID".into());
    }
    let source = source(access, &args).await?;
    let (broker, authorization) = access.authorized().await?;
    let owner = owner(&access.user, &authorization)?;
    let participant = source
        .participant
        .as_ref()
        .map(|p| json!({"client_id":p.client_id,"id":p.agent.id}));
    let fingerprint: [u8; 32] = Sha256::digest(
        serde_json::to_vec(&json!({"to":args.to,"text":args.text,"topic":args.topic,
        "reply_to":args.reply_to,"ttl_ms":args.ttl_ms,"participant":participant}))
        .map_err(|e| e.to_string())?,
    )
    .into();
    let (batch, duplicate) =
        broker
            .messages
            .reserve(owner, args.idempotency_key.clone(), fingerprint, source)?;
    if duplicate {
        return Ok(batch.snapshot(true));
    }
    let guard = Guard(batch.clone());
    let mut recipients = match resolve(access, &args.to).await {
        Ok(recipients) => recipients,
        Err(error) => {
            batch.state.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error);
            guard.finish();
            return Ok(batch.snapshot(false));
        }
    };
    if let Err(error) = batch.reserve_recipients(recipients.len()) {
        batch.state.lock().unwrap_or_else(|e| e.into_inner()).error = Some(error);
        guard.finish();
        return Ok(batch.snapshot(false));
    }
    for recipient in &mut recipients {
        if let Some(target) = &recipient.target {
            recipient.delivery_id = Some(delivery_id(&batch.message_id, target));
        }
    }
    {
        let mut state = batch.state.lock().unwrap_or_else(|e| e.into_inner());
        state.resolved = true;
        state.recipients = recipients.clone();
    }
    let mut deliveries = stream::iter(recipients.into_iter().enumerate().map(|(i, recipient)| {
        let batch = batch.clone();
        let args = &args;
        async move { (i, deliver(access, &batch, args, recipient).await) }
    }))
    .buffer_unordered(FANOUT);
    while let Some((i, recipient)) = deliveries.next().await {
        batch
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recipients[i] = recipient;
    }
    access.authorized().await?;
    guard.finish();
    Ok(batch.snapshot(false))
}

async fn status(access: &Access, args: Status) -> Result<Value, String> {
    let (broker, authorization) = access.authorized().await?;
    let owner = owner(&access.user, &authorization)?;
    let batch = broker.messages.get(&owner, &args.message_id)?;
    if args.refresh {
        let recipients = batch
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .recipients
            .clone();
        let mut refreshes = stream::iter(recipients.into_iter().enumerate().filter_map(
            |(i, recipient)| {
                let target = recipient.target?;
                let id = recipient.delivery_id?;
                Some(async move {
                    let result = match current_target(access, &target).await {
                        Ok(()) => {
                            native(
                                access,
                                &target,
                                NativeRequest::Status {
                                    target: target.clone(),
                                    ids: vec![id],
                                },
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    (i, result)
                })
            },
        ))
        .buffer_unordered(FANOUT);
        while let Some((i, result)) = refreshes.next().await {
            let mut state = batch.state.lock().unwrap_or_else(|e| e.into_inner());
            let recipient = &mut state.recipients[i];
            match result {
                Ok(result) => {
                    let delivery = &result["deliveries"][0];
                    if matches!(
                        delivery["state"].as_str(),
                        Some("candidate" | "acknowledged" | "expired" | "unknown")
                    ) && delivery["delivery_id"].as_str() == recipient.delivery_id.as_deref()
                    {
                        recipient.receipt = Some(
                            json!({"state":delivery["state"],"sequence":delivery["sequence"].as_u64()}),
                        );
                    }
                }
                Err(error) => {
                    let original = recipient.state.clone();
                    recipient.fail(&original, error);
                }
            }
        }
    }
    access.authorized().await?;
    Ok(batch.snapshot(false))
}

pub(super) async fn dispatch(
    access: Access,
    action: &str,
    arguments: Value,
) -> Result<Value, String> {
    match action {
        "key" => {
            super::empty(&arguments)?;
            let (broker, _) = access.authorized().await?;
            broker.messages.key()
        }
        "list" => list(&access, parse(arguments)?).await,
        "send" => send(&access, parse(arguments)?).await,
        "status" => status(&access, parse(arguments)?).await,
        "read" | "receive" => {
            let args: Read = parse(arguments)?;
            let request = NativeRequest::Read {
                target: args.target.clone(),
                after: args.after,
                limit: args.limit,
            };
            request.validate()?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            current_target(&access, &args.target).await?;
            let result = native(&access, &args.target, request).await?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            Ok(result)
        }
        "wait" => {
            let args: Wait = parse(arguments)?;
            let request = NativeRequest::Wait {
                target: args.target.clone(),
                after: args.after,
                limit: args.limit,
                wait_ms: args.wait_ms,
            };
            request.validate()?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            current_target(&access, &args.target).await?;
            let result = native(&access, &args.target, request).await?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            Ok(result)
        }
        "ack" => {
            let args: Ack = parse(arguments)?;
            let request = NativeRequest::Ack {
                target: args.target.clone(),
                ids: args.ids,
                through: args.through,
            };
            request.validate()?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            current_target(&access, &args.target).await?;
            let result = native(&access, &args.target, request).await?;
            authorize_mailbox(&access, &args.target, args.agent.as_ref()).await?;
            Ok(result)
        }
        _ => Err("Unknown messages action".into()),
    }
}
