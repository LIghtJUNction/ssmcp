//! Retained delivery receipts for generated conversation inputs.
//! A collector may stop observing; the original native call keeps its identity.
use super::{CodeRunReport, EligibleUserResponse, Operation, response_authorization};
use crate::{broker::Session, code_mode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};
use tokio::{sync::watch, time::Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Pending,
    Recorded,
    Deferred,
    Rejected,
    Unknown,
}

struct Inner {
    key: String,
    message_id: String,
    text: String,
    origin: &'static str,
    session: Weak<Session>,
    binding: code_mode::RunBinding,
    started: AtomicBool,
    outcome: watch::Sender<Outcome>,
}

#[derive(Clone)]
pub(super) struct Publication(Arc<Inner>);

impl Publication {
    pub(super) fn new(
        target: &EligibleUserResponse,
        identity: &str,
        text: String,
        origin: &'static str,
    ) -> Option<Self> {
        if text.trim().is_empty()
            || text.len() > 16_384
            || !matches!(origin, "system_hook" | "worker_event" | "maintenance")
        {
            return None;
        }
        let binding = target.binding.clone()?;
        let mut digest = Sha256::new();
        digest.update(b"ssmcp.generated.input.v1\0");
        digest.update(origin.as_bytes());
        digest.update(b"\0");
        digest.update(identity.as_bytes());
        let key = format!("gi_{}", crate::hex::encode(digest.finalize()));
        let message_id =
            crate::client::interface::history::request_identity(&key, "generated_input").ok()?;
        let (outcome, _) = watch::channel(Outcome::Pending);
        Some(Self(Arc::new(Inner {
            key,
            message_id,
            text,
            origin,
            session: Arc::downgrade(&target.session),
            binding,
            started: AtomicBool::new(false),
            outcome,
        })))
    }

    pub(super) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(super) fn matches(&self, text: &str, origin: &str) -> bool {
        self.0.text == text && self.0.origin == origin
    }

    pub(super) fn alive(&self) -> bool {
        self.0
            .session
            .upgrade()
            .is_some_and(|session| session.is_alive())
    }

    pub(super) fn recorded(&self) -> bool {
        *self.0.outcome.borrow() == Outcome::Recorded
    }

    pub(super) fn settled(&self) -> bool {
        !self.0.started.load(Ordering::Acquire)
            || matches!(
                *self.0.outcome.borrow(),
                Outcome::Recorded | Outcome::Rejected | Outcome::Deferred
            )
    }

    fn original(&self, target: &EligibleUserResponse) -> bool {
        self.0
            .session
            .upgrade()
            .is_some_and(|original| Arc::ptr_eq(&original, &target.session))
            && target.binding.as_ref().is_some_and(|binding| {
                binding.owner == self.0.binding.owner
                    && binding.agent_id == self.0.binding.agent_id
                    && binding.token == self.0.binding.token
            })
    }

    /// Must be called after releasing producer cache/registry locks.
    pub(super) fn start(&self, report: &CodeRunReport, target: &EligibleUserResponse) {
        if !self.original(target)
            || report
                .run
                .connection(target.session.client_id())
                .is_none_or(|original| !Arc::ptr_eq(&original, &target.session))
            || self
                .0
                .started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let Some(authorization) = response_authorization(report, target) else {
            self.0.outcome.send_replace(Outcome::Rejected);
            return;
        };
        self.0.outcome.send_replace(Outcome::Pending);
        let publication = self.clone();
        let session = target.session.clone();
        let binding = self.0.binding.clone();
        let run = report.run.clone();
        tokio::spawn(async move {
            // Panic or cancellation after dispatch is uncertain, not permission
            // to send again. The original source/body/key remain retained.
            let mut guard = UnknownOnDrop {
                outcome: publication.0.outcome.clone(),
                armed: true,
            };
            let reply = code_mode::AUDIT_RUN
                .scope(
                    run,
                    session.call_with_bound_authorization(
                        Operation::CallTool {
                            name: "ssmcp.ui.message".into(),
                            arguments: json!({
                                "kind":"input","input_origin":publication.0.origin,
                                "idempotency_key":publication.0.key,
                                "text":publication.0.text,
                            }),
                        },
                        None,
                        &authorization,
                        Some(binding),
                    ),
                )
                .await;
            let outcome = match reply {
                Ok(reply) if accepted(&reply, &publication.0.message_id, publication.0.origin) => {
                    Outcome::Recorded
                }
                // Native errors can wrap a post-commit blocking-worker failure;
                // an error envelope alone cannot prove that no input was recorded.
                Ok(reply) if reply["isError"] == true => Outcome::Unknown,
                // This exact Session admission error precedes sender.try_send;
                // unlike transport errors it is positive proof of no dispatch.
                Err(error) if error == "client busy; retry later" => Outcome::Deferred,
                _ => Outcome::Unknown,
            };
            publication.0.outcome.send_replace(outcome);
            if outcome == Outcome::Deferred {
                publication.0.started.store(false, Ordering::Release);
            }
            guard.armed = false;
        });
    }

    pub(super) async fn recorded_by(&self, deadline: Instant) -> bool {
        let mut observed = self.0.outcome.subscribe();
        loop {
            match *observed.borrow_and_update() {
                Outcome::Recorded => return true,
                Outcome::Pending => {}
                _ => return false,
            }
            if !matches!(
                tokio::time::timeout_at(deadline, observed.changed()).await,
                Ok(Ok(()))
            ) {
                return false;
            }
        }
    }
}

struct UnknownOnDrop {
    outcome: watch::Sender<Outcome>,
    armed: bool,
}
impl Drop for UnknownOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.outcome.send_replace(Outcome::Unknown);
        }
    }
}

fn accepted(reply: &Value, message_id: &str, origin: &str) -> bool {
    let value = &reply["structuredContent"];
    reply["isError"] != true
        && value["accepted"] == true
        && value["recorded"] == true
        && value["kind"] == "input"
        && value["input_origin"] == origin
        && value["message_id"] == message_id
        && value["completion_permission"] == false
}

pub(super) fn fresh_identity() -> Option<String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).ok()?;
    Some(format!("notice_{}", crate::hex::encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_requires_the_actual_matching_native_receipt() {
        let reply = json!({"isError":false,"structuredContent":{
            "accepted":true,"recorded":true,"kind":"input","input_origin":"worker_event",
            "message_id":"message_exact","completion_permission":false,"shown":false}});
        assert!(accepted(&reply, "message_exact", "worker_event"));
        for (key, wrong) in [
            ("accepted", json!(false)),
            ("recorded", json!(false)),
            ("kind", json!("answer")),
            ("input_origin", json!("human")),
            ("message_id", json!("message_other")),
            ("completion_permission", json!(true)),
        ] {
            let mut bad = reply.clone();
            bad["structuredContent"][key] = wrong;
            assert!(!accepted(&bad, "message_exact", "worker_event"), "{key}");
        }
        let mut error = reply;
        error["isError"] = json!(true);
        assert!(!accepted(&error, "message_exact", "worker_event"));
    }

    #[test]
    fn dropping_the_original_operation_preserves_unknown() {
        let (outcome, _) = watch::channel(Outcome::Pending);
        drop(UnknownOnDrop {
            outcome: outcome.clone(),
            armed: true,
        });
        assert_eq!(*outcome.borrow(), Outcome::Unknown);
    }
}
