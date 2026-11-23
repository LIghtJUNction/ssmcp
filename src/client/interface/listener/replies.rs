//! A local receiver's execution result is not an agent task-completion approval.
use super::*;
use sha2::{Digest, Sha256};

pub(super) const MAX_REPLY_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionState {
    Completed,
    Failed,
}

impl ExecutionState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ReplyReceipt {
    pub delivery_id: String,
    pub connection_id: String,
    pub listener_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub execution_state: ExecutionState,
    pub recorded: bool,
    pub storage: &'static str,
    pub shown: bool,
    #[serde(skip)]
    epoch: u64,
}

pub(super) struct RememberedReply {
    digest: [u8; 32],
    receipt: ReplyReceipt,
}

struct Reply<'a> {
    delivery_id: &'a str,
    text: &'a str,
    execution_state: ExecutionState,
}

trait Sink {
    fn current(&self, conversation_id: &str) -> Result<(), String>;
    fn commit(
        &self,
        conversation_id: &str,
        delivery_id: &str,
        source: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), String>;
    fn publish(&self, receipt: &ReplyReceipt, label: &str, source: &str, text: &str) -> bool;
}

struct ConversationSink;
impl Sink for ConversationSink {
    fn current(&self, id: &str) -> Result<(), String> {
        super::super::history::with_listener_conversation(id, || ())
    }

    fn commit(
        &self,
        conversation_id: &str,
        delivery_id: &str,
        source: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), String> {
        super::super::history::record_listener_result(
            conversation_id,
            delivery_id,
            source,
            message_id,
            text,
        )
    }

    fn publish(&self, receipt: &ReplyReceipt, label: &str, source: &str, text: &str) -> bool {
        crate::client::terminal::listener_result(receipt, label, source, text)
    }
}

impl Hub {
    pub(super) fn reply_for(
        &self,
        id: &str,
        token: &str,
        delivery_id: &str,
        text: &str,
        execution_state: ExecutionState,
        caller: &Caller,
    ) -> Result<Value, String> {
        self.reply_with(
            id,
            token,
            Reply {
                delivery_id,
                text,
                execution_state,
            },
            caller,
            &ConversationSink,
        )
    }

    fn reply_with(
        &self,
        id: &str,
        token: &str,
        reply: Reply<'_>,
        caller: &Caller,
        sink: &dyn Sink,
    ) -> Result<Value, String> {
        if !matches!(caller.owner, Owner::Local { .. }) {
            return Err("listener_reply_local_only: use the private Unix transport".into());
        }
        if reply.text.trim().is_empty()
            || reply.text.len() > MAX_REPLY_BYTES
            || reply
                .text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err(
                "listener result requires 1..=16384 UTF-8 bytes without unsupported controls"
                    .into(),
            );
        }
        let digest: [u8; 32] = Sha256::digest(reply.text.as_bytes()).into();
        // Duplicate requests cannot race publication. No caller of the UI holds this lock.
        let _serial = self.reply_serial.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.expire(Instant::now());
        state.authenticate(id, token, caller)?;
        let lease = state.lease.as_ref().expect("authenticated lease");
        if !lease.enable_reply {
            return Err(
                "listener_reply_disabled: open a new local lease with enable_reply=true".into(),
            );
        }
        let label = lease.label.clone();
        let connection = state.connection.as_ref().expect("authenticated connection");
        let epoch = connection.epoch;
        let connection_id = connection.id.clone();
        let remembered = state.receipts.iter_mut().find(|entry| {
            entry.epoch == epoch
                && entry.receipt.listener_id == id
                && entry.receipt.delivery_id == reply.delivery_id
        }).ok_or("listener_reply_expired: delivery is unknown or outside the retained receipt window")?;
        if remembered.receipt.status != Status::Accepted {
            return Err(
                "listener_reply_unaccepted: acknowledge receipt before publishing a result".into(),
            );
        }
        let conversation_id = remembered
            .conversation_id
            .as_deref()
            .ok_or("listener_reply_unbound: delivery has no original conversation")?;
        sink.current(conversation_id)?;
        if let Some(previous) = &remembered.result {
            if previous.digest != digest
                || previous.receipt.execution_state != reply.execution_state
            {
                return Err(
                    "listener_reply_conflict: this delivery already has a different result".into(),
                );
            }
            return Ok(json!({"schema_version":1,"reply":previous.receipt,"duplicate":true}));
        }
        let source = remembered
            .source
            .as_deref()
            .ok_or("listener_reply_expired: original delivery source is no longer retained")?;
        let mut identity = Sha256::new();
        identity.update(b"ssmcp.listener.reply.v1\0");
        identity.update(connection_id.as_bytes());
        identity.update(epoch.to_be_bytes());
        identity.update(reply.delivery_id.as_bytes());
        let suffix = crate::hex::encode(&identity.finalize()[..16]);
        let message_id = format!("message_{suffix}");
        // Lock order here is Hub -> in-memory history. This function never renders under either.
        // The user source is committed first, even when a fast provider beats the sender task.
        sink.commit(
            conversation_id,
            reply.delivery_id,
            source,
            &message_id,
            reply.text,
        )?;
        // The source stays private to this publication callback, never in the receipt.
        let source = source.to_owned();
        let mut receipt = ReplyReceipt {
            delivery_id: reply.delivery_id.into(),
            connection_id,
            listener_id: id.into(),
            conversation_id: conversation_id.into(),
            message_id,
            execution_state: reply.execution_state,
            recorded: true,
            storage: "memory",
            shown: false,
            epoch,
        };
        remembered.result = Some(RememberedReply {
            digest,
            receipt: receipt.clone(),
        });
        remembered.source = None;
        drop(state);

        let safe = crate::audit::event(json!(reply.text));
        let text = safe.as_str().unwrap_or("[redacted]");
        // Publication rechecks the connection, lease, retained result and active history
        // while holding the UI lock. A reconnect or conversation switch cannot receive an old result.
        receipt.shown = sink.publish(&receipt, &label, &source, text);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = state
            .receipts
            .iter_mut()
            .find(|entry| entry.epoch == epoch && entry.receipt.delivery_id == reply.delivery_id)
            .and_then(|entry| entry.result.as_mut())
        {
            result.receipt.shown = receipt.shown;
        }
        Ok(json!({"schema_version":1,"reply":receipt,"duplicate":false}))
    }
}

/// The caller owns the terminal lock first. Keep Hub and History stable through
/// the small display append; no history write, callback to render, or model runs here.
pub(crate) fn with_current_reply<T>(
    receipt: &ReplyReceipt,
    publish: impl FnOnce() -> T,
) -> Option<T> {
    let state = global().state.lock().unwrap_or_else(|e| e.into_inner());
    let connection = state.connection.as_ref()?;
    let lease = state.lease.as_ref()?;
    if connection.id != receipt.connection_id
        || connection.epoch != receipt.epoch
        || lease.id != receipt.listener_id
        || lease.expires <= Instant::now()
        || !lease.enable_reply
        || !state.receipts.iter().any(|entry| {
            entry.epoch == receipt.epoch
                && entry
                    .result
                    .as_ref()
                    .is_some_and(|result| result.receipt.message_id == receipt.message_id)
        })
    {
        return None;
    }
    super::super::history::with_listener_conversation(&receipt.conversation_id, publish).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    const CONVERSATION: &str = "conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    struct FakeSink {
        current: Mutex<String>,
        committed: Mutex<Vec<(String, String, String)>>,
        published: AtomicUsize,
        fail_commit: AtomicBool,
    }
    impl Default for FakeSink {
        fn default() -> Self {
            Self {
                current: Mutex::new(CONVERSATION.into()),
                committed: Mutex::new(Vec::new()),
                published: AtomicUsize::new(0),
                fail_commit: AtomicBool::new(false),
            }
        }
    }
    impl Sink for FakeSink {
        fn current(&self, id: &str) -> Result<(), String> {
            if *self.current.lock().unwrap() == id {
                Ok(())
            } else {
                Err("listener_conversation_changed".into())
            }
        }
        fn commit(
            &self,
            conversation: &str,
            _: &str,
            source: &str,
            id: &str,
            text: &str,
        ) -> Result<(), String> {
            self.current(conversation)?;
            self.committed
                .lock()
                .unwrap()
                .push((id.into(), source.into(), text.into()));
            if self.fail_commit.swap(false, Ordering::Relaxed) {
                Err("fixture disk failure".into())
            } else {
                Ok(())
            }
        }
        fn publish(&self, receipt: &ReplyReceipt, _: &str, _: &str, _: &str) -> bool {
            if self.current(&receipt.conversation_id).is_err() {
                return false;
            }
            self.published.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    fn open(hub: &Hub, caller: &Caller) -> (String, String) {
        let opened = hub
            .open_with_reply("zcalld".into(), caller, None, true)
            .unwrap();
        assert_eq!(opened["reply_enabled"], true);
        (
            opened["listener_id"].as_str().unwrap().into(),
            opened["token"].as_str().unwrap().into(),
        )
    }

    async fn accepted() -> (Arc<Hub>, Caller, (String, String), String, FakeSink) {
        let hub = Arc::new(Hub::default());
        hub.reset_for(Some("reply_connection"), 0);
        let caller = Caller::fixture();
        let lease = open(&hub, &caller);
        let mut target = hub.target_for(None).unwrap();
        target.conversation_id = Some(CONVERSATION.into());
        let ticket = hub
            .submit(
                target,
                "  original @missing $skill\nbody".into(),
                "expanded transport text".into(),
            )
            .unwrap();
        let id = ticket.id.clone();
        hub.read_for(lease.0.clone(), lease.1.clone(), 0, caller.clone())
            .await
            .unwrap();
        hub.ack_for(
            &lease.0,
            &lease.1,
            &id,
            Ack {
                status: AckStatus::Accepted,
                reason: None,
            },
            &caller,
        )
        .unwrap();
        assert_eq!(ticket.wait().await.unwrap().status, Status::Accepted);
        (hub, caller, lease, id, FakeSink::default())
    }

    fn reply(
        hub: &Hub,
        caller: &Caller,
        lease: &(String, String),
        id: &str,
        text: &str,
        state: ExecutionState,
        sink: &FakeSink,
    ) -> Result<Value, String> {
        hub.reply_with(
            &lease.0,
            &lease.1,
            Reply {
                delivery_id: id,
                text,
                execution_state: state,
            },
            caller,
            sink,
        )
    }

    #[tokio::test]
    async fn listener_result_is_once_per_accepted_delivery_and_keeps_ack_separate() {
        let (hub, caller, lease, id, sink) = accepted().await;
        let first = reply(
            &hub,
            &caller,
            &lease,
            &id,
            "模型结果\n  retained spaces",
            ExecutionState::Completed,
            &sink,
        )
        .unwrap();
        let duplicate = reply(
            &hub,
            &caller,
            &lease,
            &id,
            "模型结果\n  retained spaces",
            ExecutionState::Completed,
            &sink,
        )
        .unwrap();
        assert_eq!(first["reply"], duplicate["reply"]);
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(first["reply"]["execution_state"], "completed");
        assert_eq!(first["reply"]["conversation_id"], CONVERSATION);
        assert_eq!(sink.committed.lock().unwrap().len(), 1);
        assert_eq!(
            sink.committed.lock().unwrap()[0].1,
            "  original @missing $skill\nbody"
        );
        assert_eq!(sink.published.load(Ordering::Relaxed), 1);
        let state = hub.state.lock().unwrap();
        assert_eq!(
            state.receipts.back().unwrap().receipt.status,
            Status::Accepted
        );
        assert!(state.receipts.back().unwrap().source.is_none());
        drop(state);
        for (text, execution) in [
            ("changed", ExecutionState::Completed),
            ("模型结果\n  retained spaces", ExecutionState::Failed),
        ] {
            assert!(
                reply(&hub, &caller, &lease, &id, text, execution, &sink)
                    .unwrap_err()
                    .contains("conflict")
            );
        }
        assert_eq!(sink.committed.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn listener_result_rejects_other_conversation_and_keeps_original_source() {
        let (hub, caller, lease, id, sink) = accepted().await;
        *sink.current.lock().unwrap() = "conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        assert!(
            reply(
                &hub,
                &caller,
                &lease,
                &id,
                "late result",
                ExecutionState::Completed,
                &sink
            )
            .unwrap_err()
            .contains("conversation_changed")
        );
        assert!(sink.committed.lock().unwrap().is_empty());
        assert_eq!(sink.published.load(Ordering::Relaxed), 0);
        assert!(
            hub.state
                .lock()
                .unwrap()
                .receipts
                .back()
                .unwrap()
                .source
                .is_some()
        );
    }

    #[tokio::test]
    async fn listener_result_rejects_closed_expired_replaced_and_disconnected_leases() {
        for mode in 0..4 {
            let (hub, caller, lease, id, sink) = accepted().await;
            match mode {
                0 => {
                    hub.close_for(&lease.0, &lease.1, &caller).unwrap();
                }
                1 => {
                    hub.state.lock().unwrap().lease.as_mut().unwrap().expires = Instant::now();
                }
                2 => {
                    hub.close_for(&lease.0, &lease.1, &caller).unwrap();
                    let _ = open(&hub, &caller);
                }
                _ => hub.reset_for(Some("new_connection"), 1),
            }
            assert!(
                reply(
                    &hub,
                    &caller,
                    &lease,
                    &id,
                    "late result",
                    ExecutionState::Completed,
                    &sink
                )
                .is_err()
            );
            assert!(sink.committed.lock().unwrap().is_empty());
            assert_eq!(sink.published.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn listener_result_requires_local_opt_in_and_explicit_acceptance() {
        let hub = Arc::new(Hub::default());
        hub.reset_for(Some("reply_connection"), 0);
        let remote = Caller {
            generation: 0,
            session: None,
            owner: Owner::Remote {
                provider: "github".into(),
                subject: "42".into(),
                thread: None,
            },
        };
        assert!(
            hub.open_with_reply("zcalld".into(), &remote, None, true)
                .unwrap_err()
                .contains("local_only")
        );
        let caller = Caller::fixture();
        let lease = open(&hub, &caller);
        let mut target = hub.target_for(None).unwrap();
        target.conversation_id = Some(CONVERSATION.into());
        let ticket = hub
            .submit(target, "source".into(), "source".into())
            .unwrap();
        let sink = FakeSink::default();
        assert!(
            reply(
                &hub,
                &caller,
                &lease,
                &ticket.id,
                "before ACK",
                ExecutionState::Completed,
                &sink
            )
            .is_err()
        );
        hub.read_for(lease.0.clone(), lease.1.clone(), 0, caller.clone())
            .await
            .unwrap();
        hub.ack_for(
            &lease.0,
            &lease.1,
            &ticket.id,
            Ack {
                status: AckStatus::Rejected,
                reason: Some("busy".into()),
            },
            &caller,
        )
        .unwrap();
        assert!(
            reply(
                &hub,
                &caller,
                &lease,
                &ticket.id,
                "after rejection",
                ExecutionState::Completed,
                &sink
            )
            .unwrap_err()
            .contains("unaccepted")
        );
        assert!(
            reply(
                &hub,
                &remote,
                &lease,
                &ticket.id,
                "remote",
                ExecutionState::Completed,
                &sink
            )
            .unwrap_err()
            .contains("local_only")
        );
        assert!(sink.committed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn listener_result_failure_retry_keeps_identity_and_bounds_text() {
        let (hub, caller, lease, id, sink) = accepted().await;
        sink.fail_commit.store(true, Ordering::Relaxed);
        assert!(
            reply(
                &hub,
                &caller,
                &lease,
                &id,
                "generation failed",
                ExecutionState::Failed,
                &sink
            )
            .is_err()
        );
        let saved = reply(
            &hub,
            &caller,
            &lease,
            &id,
            "generation failed",
            ExecutionState::Failed,
            &sink,
        )
        .unwrap();
        let commits = sink.committed.lock().unwrap();
        assert_eq!(commits[0].0, commits[1].0);
        assert_eq!(saved["reply"]["execution_state"], "failed");
        assert_eq!(
            hub.state
                .lock()
                .unwrap()
                .receipts
                .back()
                .unwrap()
                .receipt
                .status,
            Status::Accepted
        );
        drop(commits);
        for invalid in [
            "\u{1b}[2J".into(),
            "界".repeat(MAX_REPLY_BYTES / 3 + 1),
            " ".into(),
        ] {
            assert!(
                reply(
                    &hub,
                    &caller,
                    &lease,
                    &id,
                    &invalid,
                    ExecutionState::Completed,
                    &sink
                )
                .is_err()
            );
        }
        assert_eq!(sink.published.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn listener_reply_opt_in_is_part_of_open_idempotency() {
        let hub = Hub::default();
        hub.reset_for(Some("reply_connection"), 0);
        let caller = Caller::fixture();
        hub.open_for("zcalld".into(), &caller, Some("open_1".into()))
            .unwrap();
        assert!(
            hub.open_with_reply("zcalld".into(), &caller, Some("open_1".into()), true)
                .unwrap_err()
                .contains("conflict")
        );
    }
}
