//! Broadcast retry reservations share the native mailbox's synchronization.
use super::*;

pub(crate) const MAX_BROADCAST_RECIPIENTS: usize = 32;
pub(crate) const MAX_BROADCASTS_PER_SENDER: usize = 128;
const MAX_BROADCASTS: usize = 4096;

pub(crate) struct Broadcast {
    pub(crate) request_id: String,
    pub(super) fingerprint: [u8; 32],
    pub(super) recipients: Vec<String>,
}

impl Broadcast {
    pub(crate) fn new(
        request_id: String,
        mut recipients: Vec<String>,
        text: &str,
    ) -> Result<Self, String> {
        messages::validate_request_id(&request_id)?;
        messages::validate_text(text)?;
        if recipients.is_empty() || recipients.len() > MAX_BROADCAST_RECIPIENTS {
            return Err("broadcast requires 1..=32 explicitly selected recipient IDs".into());
        }
        for recipient in &recipients {
            messages::parse_recipient(recipient)?;
        }
        recipients.sort();
        if recipients.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err("broadcast recipient IDs must be distinct".into());
        }
        let canonical = serde_json::to_vec(&(&recipients, text)).map_err(|e| e.to_string())?;
        Ok(Self {
            request_id,
            fingerprint: Sha256::digest(canonical).into(),
            recipients,
        })
    }

    pub(crate) fn child_request_id(&self, recipient: &str) -> String {
        let canonical = serde_json::to_vec(&(&self.request_id, recipient))
            .expect("broadcast child key serializes");
        format!(
            "broadcast-{:x}",
            crate::digest::Hex(Sha256::digest(canonical))
        )
    }
}

pub(super) struct Record {
    pub(super) source: Weak<Session>,
    pub(super) sender: Sender,
    pub(super) request_id: String,
    pub(super) fingerprint: [u8; 32],
    pub(super) accepted: HashMap<String, String>,
}

fn member_key(sender: &Sender) -> Option<(String, String, String)> {
    sender.agent.as_ref().map(|agent| {
        (
            sender.account.provider.clone(),
            sender.account.subject.clone(),
            agent.id.clone(),
        )
    })
}

impl Hub {
    pub(super) fn prune_broadcast_members(&self, source: &Arc<Session>) {
        let connection = Arc::downgrade(source);
        let candidates: std::collections::BTreeSet<_> = self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .broadcasts
            .iter()
            .filter(|record| Weak::ptr_eq(&record.source, &connection))
            .filter_map(|record| member_key(&record.sender))
            .collect();
        if candidates.is_empty() {
            return;
        }
        // Membership lookup purges expired leases and takes its own lock.
        // Snapshot existing senders first so a newly joined sender cannot be
        // removed because its membership arrived after this roster snapshot.
        let members = source.collaboration.native_members();
        let invalid: std::collections::BTreeSet<_> = candidates
            .into_iter()
            .filter(|(provider, subject, agent_id)| {
                let suffix = format!("\0{provider}\0{subject}");
                !members.as_ref().is_ok_and(|members| {
                    members.iter().any(|(owner, member)| {
                        member.agent_id == *agent_id && owner.ends_with(&suffix)
                    })
                })
            })
            .collect();
        if invalid.is_empty() {
            return;
        }
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .broadcasts
            .retain(|record| {
                !Weak::ptr_eq(&record.source, &connection)
                    || member_key(&record.sender).is_none_or(|key| !invalid.contains(&key))
            });
    }
}

impl Hub {
    pub(crate) fn reserve_broadcast(
        &self,
        source: &Arc<Session>,
        sender: &Sender,
        broadcast: &Broadcast,
    ) -> Result<(), String> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = state.broadcasts.iter().find(|record| {
            same_sender(&record.sender, sender) && record.request_id == broadcast.request_id
        }) {
            if record.fingerprint != broadcast.fingerprint {
                return Err(
                    "broadcast request_id was already used with different recipients or text"
                        .into(),
                );
            }
            if !Weak::ptr_eq(&record.source, &Arc::downgrade(source)) {
                return Err(
                    "broadcast belongs to an earlier sender connection; it was not resent".into(),
                );
            }
            return Ok(());
        }
        if state.entries.iter().any(|entry| {
            same_sender(&entry.message.sender, sender)
                && entry.message.receipt.request_id == broadcast.request_id
        }) {
            return Err("request_id was already used for a single message".into());
        }
        state.broadcasts.retain(|record| {
            record
                .source
                .upgrade()
                .is_some_and(|source| source.is_alive())
        });
        if state.broadcasts.len() >= MAX_BROADCASTS
            || state
                .broadcasts
                .iter()
                .filter(|record| same_sender(&record.sender, sender))
                .count()
                >= MAX_BROADCASTS_PER_SENDER
        {
            return Err("broadcast retry capacity is full for this connection; existing request IDs remain retryable".into());
        }
        state.broadcasts.push(Record {
            source: Arc::downgrade(source),
            sender: sender.clone(),
            request_id: broadcast.request_id.clone(),
            fingerprint: broadcast.fingerprint,
            accepted: HashMap::new(),
        });
        Ok(())
    }
}

impl Hub {
    pub(crate) fn broadcast_receipt(
        &self,
        sender: &Sender,
        broadcast: &Broadcast,
        recipient_id: &str,
        valid: impl Fn(&Proof, &Recipient) -> bool,
    ) -> Result<Option<Receipt>, String> {
        let child = broadcast.child_request_id(recipient_id);
        self.check_pending(
            |entry| {
                same_sender(&entry.message.sender, sender)
                    && entry.message.receipt.request_id == child
            },
            valid,
        );
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let record = state
            .broadcasts
            .iter()
            .find(|record| {
                same_sender(&record.sender, sender)
                    && record.request_id == broadcast.request_id
                    && record.fingerprint == broadcast.fingerprint
            })
            .ok_or("broadcast reservation is unavailable")?;
        let Some(message_id) = record.accepted.get(recipient_id) else {
            return Ok(None);
        };
        let Some(entry) = state.entries.iter().find(|entry| {
            entry.message.receipt.message_id == *message_id
                && same_sender(&entry.message.sender, sender)
        }) else {
            return Err("broadcast target was already accepted; its receipt is no longer retained and it was not resent".into());
        };
        let mut receipt = entry.message.receipt.clone();
        receipt.duplicate = true;
        Ok(Some(receipt))
    }

    pub(crate) fn enqueue_broadcast(
        &self,
        proof: Proof,
        recipient: Recipient,
        text: String,
        broadcast: &Broadcast,
    ) -> Result<Receipt, String> {
        let request_id = broadcast.child_request_id(&recipient.id);
        self.enqueue_prepared(proof, recipient, text, request_id, Some(broadcast))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_requires_bounded_distinct_explicit_recipients() {
        let one = format!("client:{}", "a".repeat(64));
        let mut invalid = vec![vec![], vec![one.clone(), one.clone()], vec!["*".into()]];
        invalid.push(
            (0..33)
                .map(|index| format!("client:{index:064x}"))
                .collect(),
        );
        for recipients in invalid {
            assert!(Broadcast::new("group".into(), recipients, "message").is_err());
        }
        assert!(Broadcast::new("group".into(), vec![one.clone()], "").is_err());
        assert!(Broadcast::new("bad id".into(), vec![one.clone()], "message").is_err());
        assert!(Broadcast::new("group".into(), vec![one], "message").is_ok());
    }

    #[test]
    fn broadcast_fingerprint_binds_text_and_complete_set_but_not_display_order() {
        let one = format!("client:{}", "a".repeat(64));
        let two = format!("client:{}", "b".repeat(64));
        let first =
            Broadcast::new("group".into(), vec![one.clone(), two.clone()], "message").unwrap();
        let reordered =
            Broadcast::new("group".into(), vec![two.clone(), one.clone()], "message").unwrap();
        assert_eq!(first.fingerprint, reordered.fingerprint);
        assert_eq!(
            first.child_request_id(&one),
            reordered.child_request_id(&one)
        );
        let changed_text =
            Broadcast::new("group".into(), vec![one.clone(), two], "changed").unwrap();
        let changed_targets = Broadcast::new("group".into(), vec![one.clone()], "message").unwrap();
        assert_ne!(first.fingerprint, changed_text.fingerprint);
        assert_ne!(first.fingerprint, changed_targets.fingerprint);
        assert_eq!(
            first.child_request_id(&one),
            changed_text.child_request_id(&one)
        );
    }

    fn fixture_identity(subject: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: format!("fixture-{subject}"),
        }
    }

    fn fixture_sender(
        source: &Arc<Session>,
        identity: &Identity,
        member: Option<&Value>,
    ) -> Sender {
        Sender {
            kind: Origin::Agent,
            client_id: source.client_id().into(),
            account: crate::wire::ControlAccount {
                provider: identity.provider.clone(),
                subject: identity.subject.clone(),
                login: identity.login.clone(),
            },
            agent: member.map(|member| crate::wire::ControlAgent {
                id: member["agent_id"].as_str().unwrap().into(),
                name: "fixture member".into(),
            }),
        }
    }

    fn fixture_broadcast(request_id: &str) -> Broadcast {
        Broadcast::new(
            request_id.into(),
            vec![format!("client:{}", "b".repeat(64))],
            "bounded broadcast fixture",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn retired_members_release_only_their_exact_source_and_account_reservations() {
        let broker = crate::broker::Broker::new("https://example.test".into());
        let source = broker.test_register("owner", &"a".repeat(64)).await;
        let other = broker.test_register("owner", &"b".repeat(64)).await;
        let identity = fixture_identity("42");
        let owner = crate::bridge::owner(
            "owner",
            &crate::broker::Authorization::Identity(identity.clone()),
        )
        .unwrap();
        let retired = source
            .collaboration
            .dispatch(&owner, "join", json!({"name":"retired"}))
            .await
            .unwrap();
        let live = source
            .collaboration
            .dispatch(&owner, "join", json!({"name":"live"}))
            .await
            .unwrap();
        let hub = Hub::default();
        let retired_sender = fixture_sender(&source, &identity, Some(&retired));
        let live_sender = fixture_sender(&source, &identity, Some(&live));
        let anonymous = fixture_sender(&source, &identity, None);
        let retained = fixture_broadcast("live");
        hub.reserve_broadcast(&source, &retired_sender, &fixture_broadcast("retired"))
            .unwrap();
        hub.reserve_broadcast(&source, &live_sender, &retained)
            .unwrap();
        hub.reserve_broadcast(&source, &anonymous, &fixture_broadcast("anonymous"))
            .unwrap();
        let retired_id = retired["agent_id"].as_str().unwrap();
        hub.close_member(&other, &identity, retired_id);
        hub.close_member(&source, &fixture_identity("7"), retired_id);
        assert_eq!(hub.0.lock().unwrap().broadcasts.len(), 3);
        source
            .collaboration
            .dispatch(
                &owner,
                "leave",
                json!({
                    "agent_id":retired_id,"token":retired["token"]
                }),
            )
            .await
            .unwrap();
        hub.close_member(&source, &identity, retired_id);
        assert_eq!(hub.0.lock().unwrap().broadcasts.len(), 2);
        hub.reserve_broadcast(&source, &live_sender, &retained)
            .unwrap();
        let mut changed = retained;
        changed.fingerprint[0] ^= 1;
        assert!(
            hub.reserve_broadcast(&source, &live_sender, &changed)
                .is_err()
        );
    }

    #[tokio::test]
    async fn heartbeat_prunes_expired_broadcast_members_and_keeps_live_retry_limits() {
        let broker = crate::broker::Broker::new("https://example.test".into());
        let source = broker.test_register("owner", &"a".repeat(64)).await;
        let identity = fixture_identity("42");
        let owner = crate::bridge::owner(
            "owner",
            &crate::broker::Authorization::Identity(identity.clone()),
        )
        .unwrap();
        let expired = source
            .collaboration
            .dispatch(&owner, "join", json!({"name":"expired"}))
            .await
            .unwrap();
        let live = source
            .collaboration
            .dispatch(&owner, "join", json!({"name":"live"}))
            .await
            .unwrap();
        let hub = Hub::default();
        let expired_sender = fixture_sender(&source, &identity, Some(&expired));
        let live_sender = fixture_sender(&source, &identity, Some(&live));
        let anonymous = fixture_sender(&source, &identity, None);
        for index in 0..MAX_BROADCASTS_PER_SENDER {
            hub.reserve_broadcast(
                &source,
                &expired_sender,
                &fixture_broadcast(&format!("expired-{index}")),
            )
            .unwrap();
        }
        assert!(
            hub.reserve_broadcast(&source, &expired_sender, &fixture_broadcast("overflow"),)
                .unwrap_err()
                .contains("capacity")
        );
        let retained = fixture_broadcast("live");
        hub.reserve_broadcast(&source, &live_sender, &retained)
            .unwrap();
        hub.reserve_broadcast(&source, &anonymous, &fixture_broadcast("anonymous"))
            .unwrap();
        hub.refresh_connection(&source, |_, _| true);
        assert_eq!(
            hub.0.lock().unwrap().broadcasts.len(),
            MAX_BROADCASTS_PER_SENDER + 2
        );
        source
            .collaboration
            .expire_lease_for_test(&owner, expired["agent_id"].as_str().unwrap());
        hub.refresh_connection(&source, |_, _| true);
        assert_eq!(hub.0.lock().unwrap().broadcasts.len(), 2);
        hub.reserve_broadcast(&source, &live_sender, &retained)
            .unwrap();
        assert!(
            source
                .collaboration
                .bind(
                    &owner,
                    expired["agent_id"].as_str().unwrap(),
                    expired["token"].as_str().unwrap(),
                )
                .is_err()
        );
    }
}
