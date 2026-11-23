//! Authenticated native messages across explicitly selected client mailboxes.
mod broadcast;
use super::{Authorization, Broker, IDENTITY_RESOURCE, Session};
use crate::{
    identity::Identity,
    messaging::{Delivery, Proof},
    wire::{
        ControlAccount, ControlAgent,
        messages::{self, Command, Origin, Recipient, RecipientKind, Reply, Sender},
    },
};
use broadcast::{PreparedMessage, Request};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{cell::RefCell, collections::BTreeMap, sync::Arc};

fn account(identity: &Identity) -> ControlAccount {
    ControlAccount {
        provider: identity.provider.clone(),
        subject: identity.subject.clone(),
        login: identity.login.clone(),
    }
}

fn identity(authorization: &Authorization) -> Result<&Identity, String> {
    match authorization {
        Authorization::Identity(identity) => Ok(identity),
        _ => Err("native messaging requires an authenticated account".into()),
    }
}

fn member_identity(session: &Session, owner: &str) -> Option<Identity> {
    session
        .allowed()
        .iter()
        .find(|identity| owner.ends_with(&format!("\0{}\0{}", identity.provider, identity.subject)))
        .cloned()
}

/// A native account mailbox is not an implicit subscription for every agent.
/// Only a real sender, named recipient, or verified receiving participant gets
/// a private event route; display names never select one.
fn message_event_route(
    session: &Session,
    account: &ControlAccount,
    agent_id: Option<&str>,
) -> Option<(String, String)> {
    let id = agent_id?;
    session
        .collaboration
        .native_members()
        .ok()?
        .into_iter()
        .find(|(owner, member)| {
            member.agent_id == id
                && owner.ends_with(&format!("\0{}\0{}", account.provider, account.subject))
        })
        .map(|(owner, _)| (owner, format!("{}:{id}", session.client_id())))
}

fn native_event_data(
    receipt: &messages::Receipt,
    sender: &Sender,
    recipient: &Recipient,
    receiving_agent: Option<&str>,
) -> Value {
    json!({
        "transport":"native", "message_id":receipt.message_id,
        "source_client_id":sender.client_id, "recipient_id":recipient.id,
        "sender_agent_id":sender.agent.as_ref().map(|agent| agent.id.as_str()),
        "receiving_agent_id":receiving_agent, "status":receipt.status,
        "included_at_ms":receipt.included_at_ms,
        "acknowledged_at_ms":receipt.acknowledged_at_ms,
        "read_confirmed":false
    })
}

impl Session {
    fn message_recipients(&self) -> Vec<Recipient> {
        let mut result = Vec::new();
        if let Some(owner) = &self.owner_identity {
            let authorization = Authorization::Identity(owner.clone());
            if authorization.permits(self) {
                result.push(Recipient {
                    id: format!("client:{}", self.client_id()),
                    client_id: self.client_id().into(),
                    agent_id: None,
                    display_name: self
                        .info
                        .title
                        .clone()
                        .unwrap_or_else(|| self.info.name.clone()),
                    kind: RecipientKind::AccountInbox,
                    account: account(owner),
                    online: true,
                    sendable: true,
                });
            }
        }
        for (owner, member) in self.collaboration.native_members().unwrap_or_default() {
            let Some(identity) = member_identity(self, &owner) else {
                continue;
            };
            result.push(Recipient {
                id: format!("agent:{}:{}", self.client_id(), member.agent_id),
                client_id: self.client_id().into(),
                agent_id: Some(member.agent_id),
                display_name: member.name,
                kind: RecipientKind::Agent,
                account: account(&identity),
                online: true,
                sendable: true,
            });
        }
        result
    }

    fn message_recipient(&self, id: &str) -> Result<Recipient, String> {
        self.message_recipients()
            .into_iter()
            .find(|recipient| recipient.id == id)
            .ok_or_else(|| {
                "recipient is unavailable; refresh and explicitly select a current recipient".into()
            })
    }
}

#[derive(Clone, Copy)]
enum Admission<'a> {
    OAuth { user: &'a str, token: &'a str },
    Device(&'a crate::device::Registration),
}

impl Broker {
    fn native_message_event(
        &self,
        source: &Session,
        target: &Session,
        sender: &Sender,
        recipient: &Recipient,
        receipt: &messages::Receipt,
        kind: &str,
        receiving_agent: Option<&str>,
    ) {
        let sender_id = sender.agent.as_ref().map(|agent| agent.id.as_str());
        let recipient_id = recipient.agent_id.as_deref().or(receiving_agent);
        let mut audiences: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (owner, qualified) in [
            message_event_route(source, &sender.account, sender_id),
            message_event_route(target, &recipient.account, recipient_id),
        ]
        .into_iter()
        .flatten()
        {
            let audience = audiences.entry(owner).or_default();
            if !audience.contains(&qualified) {
                audience.push(qualified);
            }
        }
        let (event_client, actor) = if kind == "message.sent" {
            (source.client_id(), sender_id)
        } else {
            (target.client_id(), recipient_id)
        };
        // Inbox replays return the same inclusion receipt. A stable key avoids
        // re-emitting it, including when one account has multiple participants.
        let fingerprint = serde_json::to_vec(&(kind, &receipt.message_id, event_client, actor))
            .expect("typed event key serializes");
        let key = format!(
            "native_{}",
            Sha256::digest(fingerprint)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let data = native_event_data(receipt, sender, recipient, receiving_agent);
        for (owner, audience) in audiences {
            // A successful mailbox commit stays successful if the bounded
            // observation journal cannot retain additional events.
            let _ = self.events.publish_system(
                &owner,
                event_client,
                actor,
                kind,
                data.clone(),
                Some(audience),
                Some(&key),
            );
        }
    }

    fn message_authorized(&self, source: &Session, authorization: &Authorization) -> bool {
        source.is_alive()
            && authorization.is_live()
            && self.authorized_account(authorization)
            && authorization.permits(source)
    }

    fn message_recipient_live(&self, target: &Session, recipient: &Recipient) -> bool {
        let identity = Identity {
            provider: recipient.account.provider.clone(),
            subject: recipient.account.subject.clone(),
            login: recipient.account.login.clone(),
        };
        self.message_authorized(target, &Authorization::Identity(identity))
    }

    fn message_proof_live(
        &self,
        target: &Arc<Session>,
        proof: &Proof,
        recipient: &Recipient,
    ) -> bool {
        let Some(source) = proof.connection.upgrade() else {
            return false;
        };
        if proof
            .destination
            .upgrade()
            .is_none_or(|original| !Arc::ptr_eq(&original, target))
        {
            return false;
        }
        let authorization = Authorization::Identity(proof.identity.clone());
        if target.client_id() != recipient.client_id
            || !self.message_authorized(&source, &authorization)
            || !self.message_authorized(target, &authorization)
            || !self.message_recipient_live(target, recipient)
        {
            return false;
        }
        if let Some(agent) = &proof.sender.agent {
            let present = source
                .collaboration
                .native_members()
                .ok()
                .is_some_and(|members| {
                    members.into_iter().any(|(owner, member)| {
                        member.agent_id == agent.id
                            && owner.ends_with(&format!(
                                "\0{}\0{}",
                                proof.identity.provider, proof.identity.subject
                            ))
                    })
                });
            if !present {
                return false;
            }
        }
        target
            .message_recipient(&recipient.id)
            .is_ok_and(|current| {
                current.account.provider == recipient.account.provider
                    && current.account.subject == recipient.account.subject
                    && current.agent_id == recipient.agent_id
            })
    }

    fn outgoing_message_live(
        &self,
        source: &Arc<Session>,
        proof: &Proof,
        recipient: &Recipient,
    ) -> bool {
        if proof
            .connection
            .upgrade()
            .is_none_or(|original| !Arc::ptr_eq(&original, source))
        {
            return false;
        }
        let Some(target) = proof.destination.upgrade() else {
            return false;
        };
        // The receipt belongs to this sender. Recheck the original destination
        // without enumerating devices or exposing another account's mailbox.
        self.message_proof_live(&target, proof, recipient)
    }

    pub(super) fn refresh_native_connection(&self, connection: &Arc<Session>) {
        // The existing connection heartbeat handles idle membership leases and
        // ACL changes. This reads only retained proofs for this exact Arc.
        self.native_messages
            .refresh_connection(connection, |proof, recipient| {
                proof
                    .destination
                    .upgrade()
                    .is_some_and(|target| self.message_proof_live(&target, proof, recipient))
            });
    }

    pub(crate) fn retire_native_member(
        &self,
        connection: &Arc<Session>,
        identity: &Identity,
        agent_id: &str,
    ) {
        self.native_messages
            .close_member(connection, identity, agent_id);
    }

    /// Common response finalizers call this only after refreshing the caller's
    /// OAuth token and exact participant binding. No JavaScript result is input.
    pub(crate) fn include_native_messages(
        &self,
        target: &Arc<Session>,
        authorization: &Authorization,
        agent_id: Option<&str>,
        maximum: usize,
        budget: usize,
    ) -> Vec<Value> {
        let Ok(identity) = identity(authorization) else {
            return Vec::new();
        };
        if !self.message_authorized(target, authorization) {
            return Vec::new();
        }
        let sources = RefCell::new(BTreeMap::<String, Arc<Session>>::new());
        let received = self.native_messages.receive(
            Delivery {
                client_id: target.client_id(),
                identity,
                agent_id,
                maximum: maximum.min(messages::MAX_DELIVERY),
                budget,
                include_delivered: false,
            },
            |proof, recipient| {
                let valid = self.message_proof_live(target, proof, recipient);
                if valid && let Some(source) = proof.connection.upgrade() {
                    sources
                        .borrow_mut()
                        .insert(source.client_id().into(), source);
                }
                valid
            },
        );
        let sources = sources.into_inner();
        for message in &received {
            if let Some(source) = sources.get(&message.sender.client_id) {
                self.native_message_event(
                    source,
                    target,
                    &message.sender,
                    &message.recipient,
                    &message.receipt,
                    "message.received",
                    agent_id,
                );
            }
        }
        received
            .into_iter()
            .filter_map(|message| serde_json::to_value(message).ok())
            .collect()
    }

    pub(crate) async fn message_request(
        self: &Arc<Self>,
        user: &str,
        access_token: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let mut fields = arguments
            .as_object()
            .cloned()
            .ok_or("messages arguments must be an object")?;
        let client_id = fields
            .remove("client_id")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or("messages requires client_id from list")?;
        let supplied_id = fields.remove("agent_id");
        let supplied_token = fields.remove("agent_token");
        let request = Request::parse(fields)?;
        let authorization = self
            .authenticate(user, access_token)
            .await
            .ok_or("authorization expired")?;
        let caller = identity(&authorization)?.clone();
        let source = self.client(user, &authorization, &client_id).await?;
        let owner = crate::bridge::owner(user, &authorization)?;
        let run = crate::code_mode::AUDIT_RUN.try_with(Clone::clone).ok();
        if let Some(run) = &run {
            run.record_connection(&source)?;
        }
        let bound = run.as_ref().and_then(|run| run.binding(&client_id));
        let binding = match (supplied_id, supplied_token) {
            (None, None) => bound,
            (Some(Value::String(agent_id)), Some(Value::String(token))) => {
                if bound
                    .as_ref()
                    .is_some_and(|binding| binding.agent_id != agent_id || binding.token != token)
                {
                    return Err(
                        "message credentials differ from this run's verified participant".into(),
                    );
                }
                Some(crate::code_mode::RunBinding {
                    owner: owner.clone(),
                    agent_id,
                    token,
                })
            }
            _ => {
                return Err("agent_id and agent_token must be supplied together as strings".into());
            }
        };
        let agent = if let Some(binding) = &binding {
            if binding.owner != owner {
                return Err("message participant belongs to another account".into());
            }
            let member = source
                .collaboration
                .bind(&owner, &binding.agent_id, &binding.token)?;
            Some(ControlAgent {
                id: member.agent_id,
                name: member.name,
            })
        } else {
            None
        };
        let sender = Sender {
            kind: Origin::Agent,
            client_id: client_id.clone(),
            account: account(&caller),
            agent,
        };
        let admission = Admission::OAuth {
            user,
            token: access_token,
        };
        let result = match request {
            Request::Single(command) => serde_json::to_value(
                self.dispatch_message(
                    &source,
                    authorization,
                    sender,
                    binding.as_ref(),
                    command.into(),
                    admission,
                )
                .await?,
            )
            .map_err(|e| e.to_string())?,
            Request::Broadcast(prepared) => {
                self.dispatch_broadcast(
                    &source,
                    authorization,
                    sender,
                    binding.as_ref(),
                    prepared,
                    admission,
                )
                .await?
            }
        };
        if let Some(run) = &run {
            run.question_delivery_server_action(&client_id);
            run.dispatched(&client_id);
        }
        Ok(result)
    }

    pub(super) async fn local_message_command(
        self: &Arc<Self>,
        source: &Arc<Session>,
        registration: Option<&crate::device::Registration>,
        command: Command,
    ) -> Result<Reply, String> {
        command.validate()?;
        let registration =
            registration.ok_or("native messaging requires an authenticated device owner")?;
        if !source.locally_owned_peer || !registration.is_live(self).await {
            return Err("device owner authorization is unavailable".into());
        }
        if source.owner_identity.as_ref().is_none_or(|owner| {
            owner.provider != registration.identity.provider
                || owner.subject != registration.identity.subject
        }) {
            return Err("device registration does not own this connection".into());
        }
        let sender = Sender {
            kind: Origin::LocalUser,
            client_id: source.client_id().into(),
            account: account(&registration.identity),
            agent: None,
        };
        self.dispatch_message(
            source,
            Authorization::Identity(registration.identity.clone()),
            sender,
            None,
            command.into(),
            Admission::Device(registration),
        )
        .await
    }

    async fn dispatch_message(
        self: &Arc<Self>,
        source: &Arc<Session>,
        authorization: Authorization,
        sender: Sender,
        binding: Option<&crate::code_mode::RunBinding>,
        prepared: PreparedMessage<'_>,
        admission: Admission<'_>,
    ) -> Result<Reply, String> {
        let PreparedMessage { command, broadcast } = prepared;
        let resource = match &admission {
            Admission::OAuth { user, .. } => *user,
            Admission::Device(_) => IDENTITY_RESOURCE,
        };
        let sessions = self.clients(resource, &authorization).await?;
        // Complete all awaited admission checks before any mailbox mutation.
        let authorization = match admission {
            Admission::OAuth { user, token } => self
                .authenticate_now(user, token)
                .ok_or("authorization expired before message decision")?,
            Admission::Device(registration) => {
                if !registration.is_live(self).await {
                    return Err("device authorization expired before message decision".into());
                }
                Authorization::Identity(registration.identity.clone())
            }
        };
        if !self.message_authorized(source, &authorization) {
            return Err("source access changed before message decision".into());
        }
        if let Some(binding) = binding {
            source
                .collaboration
                .bind(&binding.owner, &binding.agent_id, &binding.token)?;
        }
        let caller = identity(&authorization)?;
        let mut reply = Reply::default();
        match command {
            Command::List {} => {
                for session in sessions {
                    if !self.message_authorized(&session, &authorization) {
                        continue;
                    }
                    for recipient in session.message_recipients() {
                        if !self.message_recipient_live(&session, &recipient) {
                            continue;
                        }
                        if reply.recipients.len() == messages::MAX_RECIPIENTS {
                            reply.more_recipients = true;
                            break;
                        }
                        reply.recipients.push(recipient);
                    }
                }
                reply.receipts = self
                    .native_messages
                    .outgoing(&sender, |proof, recipient| {
                        self.outgoing_message_live(source, proof, recipient)
                    })
                    .into_iter()
                    .rev()
                    .take(messages::MAX_MESSAGES)
                    .collect();
            }
            Command::Send {
                recipient_id,
                text,
                request_id,
            } => {
                let (client_id, _) = messages::parse_recipient(&recipient_id)?;
                let target = sessions
                    .iter()
                    .find(|session| session.client_id() == client_id)
                    .filter(|session| self.message_authorized(session, &authorization))
                    .ok_or("recipient client is unavailable or no longer authorized")?;
                let recipient = target.message_recipient(&recipient_id)?;
                if !self.message_recipient_live(target, &recipient) {
                    return Err("recipient account is unavailable or no longer authorized".into());
                }
                let proof = Proof {
                    connection: Arc::downgrade(source),
                    destination: Arc::downgrade(target),
                    identity: caller.clone(),
                    sender: sender.clone(),
                };
                let receipt = match broadcast {
                    Some(broadcast) => self.native_messages.enqueue_broadcast(
                        proof,
                        recipient.clone(),
                        text,
                        broadcast,
                    )?,
                    None => {
                        self.native_messages
                            .enqueue(proof, recipient.clone(), text, request_id)?
                    }
                };
                if !receipt.duplicate {
                    self.native_message_event(
                        source,
                        target,
                        &sender,
                        &recipient,
                        &receipt,
                        "message.sent",
                        None,
                    );
                }
                reply.receipts.push(receipt);
            }
            Command::Status { message_id } => {
                let receipt = self
                    .native_messages
                    .status(&sender, &message_id, |proof, recipient| {
                        self.outgoing_message_live(source, proof, recipient)
                    })
                    .ok_or("message receipt is unavailable for this sender")?;
                let (target, _) = messages::parse_recipient(&receipt.recipient_id)?;
                if !matches!(
                    receipt.status,
                    messages::Status::Expired | messages::Status::Acknowledged
                ) && !sessions.iter().any(|session| {
                    session.client_id() == target
                        && self.message_authorized(session, &authorization)
                }) {
                    return Err(
                        "recipient connection or access changed; receipt cannot be refreshed"
                            .into(),
                    );
                }
                reply.receipts.push(receipt);
            }
            Command::Inbox {} => {
                let sources = RefCell::new(BTreeMap::<String, Arc<Session>>::new());
                reply.messages = self.native_messages.receive(
                    Delivery {
                        client_id: source.client_id(),
                        identity: caller,
                        agent_id: sender.agent.as_ref().map(|agent| agent.id.as_str()),
                        maximum: messages::MAX_DELIVERY,
                        budget: 512 * 1024,
                        include_delivered: true,
                    },
                    |proof, recipient| {
                        let valid = self.message_proof_live(source, proof, recipient);
                        if valid && let Some(sender) = proof.connection.upgrade() {
                            sources
                                .borrow_mut()
                                .insert(sender.client_id().into(), sender);
                        }
                        valid
                    },
                );
                let sources = sources.into_inner();
                for message in &reply.messages {
                    if let Some(origin) = sources.get(&message.sender.client_id) {
                        self.native_message_event(
                            origin,
                            source,
                            &message.sender,
                            &message.recipient,
                            &message.receipt,
                            "message.received",
                            sender.agent.as_ref().map(|agent| agent.id.as_str()),
                        );
                    }
                }
            }
            Command::Ack { message_id } => {
                let context = RefCell::new(None);
                let receipt = self.native_messages.acknowledge(
                    source.client_id(),
                    caller,
                    sender.agent.as_ref().map(|agent| agent.id.as_str()),
                    &message_id,
                    |proof, recipient| {
                        let valid = self.message_proof_live(source, proof, recipient);
                        if valid {
                            *context.borrow_mut() = Some((proof.clone(), recipient.clone()));
                        }
                        valid
                    },
                )?;
                if !receipt.duplicate
                    && let Some((proof, recipient)) = context.into_inner()
                    && let Some(origin) = proof.connection.upgrade()
                {
                    self.native_message_event(
                        &origin,
                        source,
                        &proof.sender,
                        &recipient,
                        &receipt,
                        "message.acknowledged",
                        sender.agent.as_ref().map(|agent| agent.id.as_str()),
                    );
                }
                reply.receipts.push(receipt);
            }
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod lifecycle_event_tests {
    use super::*;
    use crate::events::{Actor, Filter};

    async fn member(session: &Session, owner: &str, name: &str) -> String {
        session
            .collaboration
            .dispatch(owner, "join", json!({"name":name}))
            .await
            .unwrap()["agent_id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn subscription(
        broker: &Broker,
        owner: &str,
        client_id: &str,
        agent_id: &str,
        clients: &[String],
    ) -> (Actor, String) {
        let actor = Actor {
            owner: owner.into(),
            client_id: client_id.into(),
            agent_id: agent_id.into(),
        };
        let filter: Filter = serde_json::from_value(json!({
            "kinds":["message.*"], "client_ids":clients
        }))
        .unwrap();
        let result = broker
            .events
            .subscribe(&actor, "native-receipts", filter, None)
            .unwrap();
        (actor, result["subscription_id"].as_str().unwrap().into())
    }

    #[tokio::test]
    async fn automatic_inclusion_emits_one_private_receipt_without_message_body() {
        let broker = Broker::new("https://relay.test".into());
        let identity = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "fixture".into(),
        };
        let authorization = Authorization::Identity(identity.clone());
        let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
        let source = broker
            .test_register_identity("source", identity.clone())
            .await;
        let target = broker
            .test_register_identity("target", identity.clone())
            .await;
        let sender_id = member(&source, &owner, "sender").await;
        let target_id = member(&target, &owner, "target").await;
        let stranger_id = member(&target, &owner, "unrelated").await;
        let clients = vec![source.client_id().into(), target.client_id().into()];
        let sender_events = subscription(&broker, &owner, source.client_id(), &sender_id, &clients);
        let target_events = subscription(&broker, &owner, target.client_id(), &target_id, &clients);
        let stranger_events =
            subscription(&broker, &owner, target.client_id(), &stranger_id, &clients);
        let recipient = target
            .message_recipient(&format!("agent:{}:{target_id}", target.client_id()))
            .unwrap();
        let sender = Sender {
            kind: Origin::Agent,
            client_id: source.client_id().into(),
            account: account(&identity),
            agent: Some(ControlAgent {
                id: sender_id,
                name: "private display text".into(),
            }),
        };
        let receipt = broker
            .native_messages
            .enqueue(
                Proof {
                    connection: Arc::downgrade(&source),
                    destination: Arc::downgrade(&target),
                    identity: identity.clone(),
                    sender,
                },
                recipient,
                "confidential message body".into(),
                "message-request".into(),
            )
            .unwrap();
        let delivered =
            broker.include_native_messages(&target, &authorization, Some(&target_id), 32, 65536);
        assert_eq!(delivered.len(), 1);
        assert!(
            broker
                .include_native_messages(&target, &authorization, Some(&target_id), 32, 65536)
                .is_empty()
        );
        for (actor, id) in [sender_events, target_events] {
            let page = broker.events.read(&actor, &id, 32).unwrap();
            let events = page["events"].as_array().unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0]["kind"], "message.received");
            assert_eq!(events[0]["data"]["message_id"], receipt.message_id);
            assert_eq!(events[0]["data"]["read_confirmed"], false);
            assert!(!page.to_string().contains("confidential message body"));
            assert!(!page.to_string().contains("private display text"));
        }
        let page = broker
            .events
            .read(&stranger_events.0, &stranger_events.1, 32)
            .unwrap();
        assert!(page["events"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn event_routes_require_a_specific_member_and_matching_account() {
        let broker = Broker::new("https://relay.test".into());
        let identity = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "fixture".into(),
        };
        let authorization = Authorization::Identity(identity.clone());
        let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
        let session = broker
            .test_register_identity("source", identity.clone())
            .await;
        let id = member(&session, &owner, "member").await;
        assert_eq!(
            message_event_route(&session, &account(&identity), None),
            None
        );
        assert_eq!(
            message_event_route(
                &session,
                &ControlAccount {
                    provider: "github".into(),
                    subject: "another-account".into(),
                    login: "fixture".into()
                },
                Some(&id)
            ),
            None
        );
        assert_eq!(
            message_event_route(&session, &account(&identity), Some(&id)),
            Some((owner, format!("{}:{id}", session.client_id())))
        );
    }
}
