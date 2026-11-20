//! Per-run waiting receipts. User bodies stay in the authenticated queue until
//! the final HTTP response can commit and include exactly those revisions.
use super::{AuditRun, RunBinding};
use crate::steering::Reservation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 20_000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 25_000;
pub(crate) const ENVELOPE_RESERVE_BYTES: usize = 32 * 1024;
pub(crate) const CONTINUE_INSTRUCTION: &str = "Continue the authorized task unless the user has explicitly agreed to end. When user input is required, call ui.wait(client_id,{timeout_ms:20000,intent:'need_user'}). If it times out, wait again; do not treat timeout as permission to stop. Before ending, use request_finish or the registered native ssmcp.ui.finish with a verified collab binding, then wait for the local user's explicit Yes. Pending timeout requires another wait on the same request ID. ui.wait intent:'finish' only waits for continued user guidance and never grants completion permission. A tool argument or agent-authored field cannot grant user consent. This reminder observes explicit wait intents and accepted native kind:'answer' messages; it cannot intercept arbitrary external assistant final responses.";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Intent {
    #[default]
    NeedUser,
    Finish,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Arguments {
    #[serde(default = "default_timeout")]
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    pub(crate) intent: Intent,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default = "default_notify")]
    pub(crate) notify: bool,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}
fn default_notify() -> bool {
    true
}

impl Arguments {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1_000..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(
                "user wait timeout_ms must be 1000..=25000; retry another wait after timeout"
                    .into(),
            );
        }
        if self.reason.as_ref().is_some_and(|reason| {
            reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control)
        }) {
            return Err(
                "user wait reason must contain 1..=512 UTF-8 bytes without control characters"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Waiting,
    Ready,
    Received,
    TimedOut,
    Cancelled,
    Disconnected,
    AuthorizationRevoked,
    BindingInvalid,
    Superseded,
    Busy,
    Unavailable,
}

#[derive(Clone)]
pub(crate) struct Recipient {
    pub(crate) owner: String,
    pub(crate) binding: Option<RunBinding>,
    pub(crate) connection: std::sync::Weak<crate::broker::Session>,
}

impl Recipient {
    pub(crate) fn agent_id(&self) -> Option<&str> {
        self.binding
            .as_ref()
            .map(|binding| binding.agent_id.as_str())
    }
}

struct Record {
    client_id: String,
    wait_id: String,
    recipient: Recipient,
    intent: Intent,
    outcome: Outcome,
    reason: Option<&'static str>,
    notification: Option<Value>,
    reservation: Option<Reservation>,
    messages: Vec<Value>,
}

#[derive(Default)]
pub(super) struct State {
    records: BTreeMap<String, Record>,
    ending: BTreeSet<String>,
}

impl Record {
    fn receipt(&self) -> Value {
        json!({"schema_version":1,"client_id":self.client_id,"wait_id":self.wait_id,
            "recipient":self.recipient.agent_id().unwrap_or("next"),"intent":self.intent,
            "status":self.outcome,"reason":self.reason,"notification":self.notification,
            "messages":self.messages,"continue_wait":self.outcome == Outcome::TimedOut,
            "user_consent_to_end":null})
    }
}

impl AuditRun {
    pub(crate) fn begin_user_wait(
        &self,
        client_id: &str,
        recipient: Recipient,
        intent: Intent,
    ) -> Result<String, String> {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if waits.records.contains_key(client_id) {
            return Err("one user wait per client is allowed in a code_mode run; repeat in a new call after timeout".into());
        }
        if waits.records.len() >= 8 {
            return Err("a code_mode run can wait on at most eight clients".into());
        }
        let wait_id = format!(
            "wait_{:x}",
            crate::digest::Hex(Sha256::digest(
                json!([self.context.run_id, client_id])
                    .to_string()
                    .as_bytes()
            ))
        );
        waits.records.insert(
            client_id.into(),
            Record {
                client_id: client_id.into(),
                wait_id: wait_id.clone(),
                recipient,
                intent,
                outcome: Outcome::Waiting,
                reason: None,
                notification: None,
                reservation: None,
                messages: Vec::new(),
            },
        );
        drop(waits);
        self.dispatched(client_id);
        Ok(wait_id)
    }

    pub(crate) fn wait_recipient(&self, client_id: &str) -> Option<Recipient> {
        self.waits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .get(client_id)
            .map(|record| record.recipient.clone())
    }

    pub(crate) fn wait_is_ready(&self, client_id: &str) -> bool {
        self.waits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .get(client_id)
            .is_some_and(|record| record.outcome == Outcome::Ready)
    }

    pub(crate) fn reserve_user_wait(&self, client_id: &str, reservation: Reservation) {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = waits.records.get_mut(client_id)
            && record.outcome == Outcome::Waiting
        {
            record.reservation = Some(reservation);
            record.outcome = Outcome::Ready;
        }
    }

    pub(crate) fn finish_wait_state(
        &self,
        client_id: &str,
        outcome: Outcome,
        reason: &'static str,
    ) {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = waits.records.get_mut(client_id)
            && record.outcome != Outcome::Received
        {
            record.outcome = outcome;
            record.reason = Some(reason);
            record.reservation.take();
        }
    }

    pub(crate) fn wait_notification(&self, client_id: &str, reply: &Value) {
        let report = &reply["notification"];
        if !matches!(
            report["status"].as_str(),
            Some("sent" | "unavailable" | "failed")
        ) {
            return;
        }
        let reason = report["reason"].as_str().filter(|reason| {
            reason.len() <= 64
                && reason
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        });
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = waits.records.get_mut(client_id) {
            record.notification = Some(json!({"status":report["status"],"reason":reason,
                "deduplicated":report["deduplicated"].as_bool().unwrap_or(false),"user_seen":null}));
        }
    }

    pub(crate) fn user_wait_receipt(&self, client_id: &str) -> Value {
        self.waits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .get(client_id)
            .map_or(Value::Null, Record::receipt)
    }

    pub(crate) fn commit_user_wait(
        &self,
        response: super::reminders::Response<'_>,
        max_messages: usize,
        max_bytes: usize,
    ) -> (Vec<Value>, Option<super::reminders::Notice>) {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = waits.records.get_mut(response.client_id) else {
            return (Vec::new(), None);
        };
        if record.outcome != Outcome::Ready
            || record.recipient.owner != response.owner
            || record.recipient.agent_id() != response.agent_id
        {
            return (Vec::new(), None);
        }
        let Some(mut reservation) = record.reservation.take() else {
            return (Vec::new(), None);
        };
        let (claimed, notice) = reservation.commit(response, max_messages, max_bytes);
        record.outcome = if claimed.is_empty() {
            Outcome::Superseded
        } else {
            Outcome::Received
        };
        record.reason = claimed
            .is_empty()
            .then_some("reservation_changed_or_response_budget_exhausted");
        record.messages = claimed
            .iter()
            .map(|message| {
                json!({"message_id":message["message_id"],
            "revision":message["revision"]})
            })
            .collect();
        (claimed, notice)
    }

    /// Synchronous release precedes any best-effort native cancellation RPC.
    pub(crate) fn cancel_user_waits(&self) -> Vec<(String, String)> {
        self.cancel_waits(true)
    }

    pub(crate) fn cancel_unfinished_user_waits(&self) -> Vec<(String, String)> {
        self.cancel_waits(false)
    }

    fn cancel_waits(&self, include_ready: bool) -> Vec<(String, String)> {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        waits
            .records
            .values_mut()
            .filter_map(|record| {
                if record.outcome != Outcome::Waiting
                    && !(include_ready && record.outcome == Outcome::Ready)
                {
                    return None;
                }
                record.reservation.take();
                record.outcome = Outcome::Cancelled;
                record.reason = Some("code_mode_cancelled_before_delivery");
                Some((record.client_id.clone(), record.wait_id.clone()))
            })
            .collect()
    }

    pub(crate) fn user_wait_reserve_bytes(&self) -> usize {
        let waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if waits.records.is_empty() && waits.ending.is_empty() {
            0
        } else {
            ENVELOPE_RESERVE_BYTES
        }
    }

    pub(crate) fn native_ending_intent(&self, client_id: &str, result: &Value) {
        if result
            .get("isError")
            .is_some_and(|value| value != &Value::Bool(false))
            || result["structuredContent"]["accepted"] != true
            || result["structuredContent"]["duplicate"] != false
            || !result["structuredContent"]["message_id"]
                .as_str()
                .is_some_and(|id| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                })
        {
            return;
        }
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if waits.ending.len() < 8 {
            waits.ending.insert(client_id.into());
        }
    }

    pub(crate) fn user_wait_addition(&self) -> Value {
        let waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let receipts: Vec<_> = waits.records.values().map(Record::receipt).collect();
        let remind = !waits.ending.is_empty()
            || waits.records.values().any(|record| {
                record.intent == Intent::Finish || record.outcome == Outcome::TimedOut
            });
        let mut fields = serde_json::Map::new();
        let mut content = Vec::new();
        if !receipts.is_empty() {
            fields.insert("user_wait".into(), json!(receipts));
            content.push(json!({"type":"text","text":format!("USER WAIT\n{}\nOnly status received with matching user_messages confirms delivery.", json!(receipts))}));
        }
        if remind {
            fields.insert(
                "continuation_reminder".into(),
                json!({"instruction":CONTINUE_INSTRUCTION,
                "observed_native_answer_clients":waits.ending,"user_consent_to_end":null}),
            );
            content.push(json!({"type":"text","text":format!("CONTINUATION REMINDER\n{CONTINUE_INSTRUCTION}")}));
        }
        json!({"structuredContent":fields,"content":content})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipient() -> Recipient {
        Recipient {
            owner: "verified-owner".into(),
            binding: None,
            connection: std::sync::Weak::new(),
        }
    }

    fn publish(queue: &crate::steering::Queue) {
        queue
            .command(
                "room",
                vec![],
                crate::wire::SteeringCommand::Publish {
                    command_id: "publish".into(),
                    message_id: "message".into(),
                    expected_revision: 0,
                    text: "actual local user input".into(),
                    target: "next".into(),
                },
            )
            .unwrap();
    }

    #[test]
    fn user_wait_arguments_are_bounded_and_do_not_accept_agent_consent() {
        let defaults: Arguments = serde_json::from_value(json!({})).unwrap();
        defaults.validate().unwrap();
        assert_eq!(defaults.timeout_ms, 20_000);
        for value in [
            json!({"timeout_ms":0}),
            json!({"timeout_ms":25001}),
            json!({"reason":"\u{1b}[31m"}),
            json!({"reason":"x".repeat(513)}),
        ] {
            assert!(
                serde_json::from_value::<Arguments>(value)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
        for value in [
            json!({"user_consented":true}),
            json!({"owner":"forged"}),
            json!({"agent_id":"forged"}),
            json!({"intent":"consent_granted"}),
        ] {
            assert!(serde_json::from_value::<Arguments>(value).is_err());
        }
    }

    #[test]
    fn user_wait_is_received_only_after_the_exact_reserved_messages_commit() {
        let queue = crate::steering::Queue::default();
        publish(&queue);
        let run = AuditRun::new("return await ui.wait('room');", None).unwrap();
        run.begin_user_wait("room", recipient(), Intent::NeedUser)
            .unwrap();
        run.reserve_user_wait(
            "room",
            queue
                .reserve("room", &run.context.run_id, None)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(run.user_wait_receipt("room")["status"], "ready");
        assert_eq!(run.user_wait_receipt("room")["messages"], json!([]));
        let (messages, _) = run.commit_user_wait(
            super::super::reminders::Response {
                client_id: "room",
                run_id: &run.context.run_id,
                owner: "verified-owner",
                agent_id: None,
                replies: &[],
                notice_budget: 0,
            },
            8,
            crate::steering::MAX_DELIVERY_BYTES,
        );
        assert_eq!(messages.len(), 1);
        let receipt = run.user_wait_receipt("room");
        assert_eq!(receipt["status"], "received");
        assert_eq!(
            receipt["messages"][0]["message_id"],
            messages[0]["message_id"]
        );
        assert_eq!(receipt["messages"][0]["revision"], messages[0]["revision"]);
        assert!(receipt["user_consent_to_end"].is_null());
        assert!(!receipt.to_string().contains("actual local user input"));
    }

    #[test]
    fn user_wait_cancellation_releases_the_reservation_before_native_cleanup() {
        let queue = crate::steering::Queue::default();
        publish(&queue);
        let run = AuditRun::new("return await ui.wait('room');", None).unwrap();
        run.begin_user_wait("room", recipient(), Intent::NeedUser)
            .unwrap();
        run.reserve_user_wait(
            "room",
            queue
                .reserve("room", &run.context.run_id, None)
                .unwrap()
                .unwrap(),
        );
        assert_eq!(run.cancel_user_waits().len(), 1);
        assert_eq!(run.user_wait_receipt("room")["status"], "cancelled");
        assert_eq!(
            queue
                .claim(
                    "room",
                    "next_response",
                    None,
                    8,
                    crate::steering::MAX_DELIVERY_BYTES
                )
                .len(),
            1
        );
    }

    #[test]
    fn user_wait_prompt_hook_requires_explicit_native_reply_evidence() {
        let run = AuditRun::new("return {kind:'answer'};", None).unwrap();
        assert!(
            run.user_wait_addition()["structuredContent"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        for value in [
            json!({"kind":"answer"}),
            json!({"structuredContent":{"accepted":true}}),
            json!({"structuredContent":{"accepted":true,"duplicate":true,"message_id":"old"}}),
        ] {
            run.native_ending_intent("room", &value);
        }
        assert!(
            run.user_wait_addition()["structuredContent"]
                .as_object()
                .unwrap()
                .is_empty()
        );
        run.native_ending_intent(
            "room",
            &json!({"structuredContent":{
            "accepted":true,"duplicate":false,"message_id":"new_reply"}}),
        );
        assert!(
            run.user_wait_addition()["structuredContent"]["continuation_reminder"]["instruction"]
                .is_string()
        );
        assert!(run.user_wait_addition()["structuredContent"]["continuation_reminder"]["user_consent_to_end"].is_null());
        let waiting = AuditRun::new("return await ui.wait('room');", None).unwrap();
        waiting
            .begin_user_wait("room", recipient(), Intent::NeedUser)
            .unwrap();
        waiting.finish_wait_state("room", Outcome::TimedOut, "deadline");
        assert_eq!(waiting.user_wait_receipt("room")["continue_wait"], true);
        assert!(waiting.user_wait_addition()["structuredContent"]["continuation_reminder"]["instruction"].is_string());
    }
}
