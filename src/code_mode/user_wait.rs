//! Per-run waiting receipts. User bodies stay in the authenticated queue until
//! the final HTTP response can commit and include exactly those revisions.
use super::{AuditRun, RunBinding};
use crate::client::interface::questions::{FinishReceipt, FinishStatus};
use crate::steering::Reservation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub(crate) const DEFAULT_TIMEOUT_MS: u64 = 20_000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 25_000;
pub(crate) const ENVELOPE_RESERVE_BYTES: usize = 32 * 1024;

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
    #[serde(default)]
    pub(crate) finish_request_id: Option<String>,
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
            return Err("user wait timeout_ms must be 1000..=25000".into());
        }
        if self.finish_request_id.as_ref().is_some_and(|id| {
            self.intent != Intent::Finish
                || id.is_empty()
                || id.len() > 128
                || !id.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(
                "finish_request_id is only a visible exact request ID for intent finish".into(),
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
    DecisionReady,
    Decided,
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
    finish_request_id: Option<String>,
    finish_decision: Option<FinishReceipt>,
    finish_episode: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    records: BTreeMap<String, Record>,
}

impl Record {
    fn receipt(&self) -> Value {
        let may_finish = self.intent == Intent::Finish
            && self.outcome == Outcome::Decided
            && self
                .finish_decision
                .as_ref()
                .is_some_and(FinishReceipt::human_permission);
        let mut receipt = json!({"schema_version":1,"client_id":self.client_id,"wait_id":self.wait_id,
            "recipient":self.recipient.agent_id().unwrap_or("next"),"intent":self.intent,
            "status":self.outcome,"reason":self.reason,"notification":self.notification,
            "messages":self.messages,"continue_wait":self.outcome == Outcome::TimedOut,
            "finish_request_id":self.finish_request_id,"finish_decision":self.finish_decision,
            "may_finish":may_finish});
        if self.intent == Intent::Finish && !may_finish {
            let input = matches!(self.outcome, Outcome::Ready | Outcome::Received);
            let authority = matches!(
                self.outcome,
                Outcome::AuthorizationRevoked | Outcome::BindingInvalid | Outcome::Disconnected
            );
            let denied = self
                .finish_decision
                .as_ref()
                .is_some_and(|decision| decision.status == FinishStatus::Denied);
            let mut arguments = json!({"intent":"finish","timeout_ms":DEFAULT_TIMEOUT_MS});
            // Terminal decisions return immediately when read again. Only a
            // still-live Pending/Approved request can be resumed as a wait.
            if !authority
                && !input
                && !matches!(self.outcome, Outcome::Cancelled | Outcome::Superseded)
                && let Some(decision) = self.finish_decision.as_ref().filter(|decision| {
                    matches!(
                        decision.status,
                        FinishStatus::Pending | FinishStatus::Approved
                    )
                })
            {
                arguments["finish_request_id"] = json!(decision.request_id);
            }
            let context = if authority {
                "The connection or verified asking thread is no longer authorized. Restore the real authorization and bind that thread before waiting again."
            } else if input {
                "New user input keeps the task active. Process the delivered user_messages; ordinary input is not permission to end."
            } else if self.outcome == Outcome::DecisionReady {
                "This is an inner native result awaiting the final outer authorization and input-epoch check. Inspect the final outer user_wait receipt; this inner result is not permission."
            } else if denied {
                "The user explicitly declined ending this task. Keep it active and continue the requested work; do not treat No as approval."
            } else {
                "No final Human approval was delivered. The task remains active; timeout, cancellation, errors and reading a decision are not permission to end."
            };
            receipt["task_active"] = json!(true);
            receipt["continue_wait"] = json!(!input);
            receipt["next_wait"] = json!({"helper":"ui.wait","client_id":self.client_id,
                "arguments":arguments,"new_tool_call":true});
            receipt["completion_instruction"] = json!(format!(
                "{context} Unless the final outer receipt grants may_finish:true, do not end the task. In a new outer tool call, actually await the indicated ui.wait after handling any new request. Do not use a timer, background sleep, repeated status reads, or a same-code_mode loop instead. Only a later explicit Human Yes consumed for this exact thread can authorize ending."
            ));
        }
        receipt
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
            return Err("one user wait per client is allowed in a code_mode run".into());
        }
        if waits.records.len() >= 8 {
            return Err("a code_mode run can wait on at most eight clients".into());
        }
        let wait_id = format!(
            "wait_{}",
            crate::hex::encode(Sha256::digest(
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
                finish_request_id: None,
                finish_decision: None,
                finish_episode: None,
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
            if matches!(
                outcome,
                Outcome::AuthorizationRevoked | Outcome::BindingInvalid | Outcome::Disconnected
            ) {
                record.finish_decision = None;
            }
        }
    }

    pub(crate) fn begin_finish_wait(
        &self,
        client_id: &str,
        request_id: Option<String>,
        episode: String,
    ) {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = waits.records.get_mut(client_id)
            && record.intent == Intent::Finish
            && record.outcome == Outcome::Waiting
        {
            record.finish_request_id = request_id;
            record.finish_episode = Some(episode);
        }
    }

    pub(crate) fn capture_finish_wait(&self, client_id: &str, data: &Value) -> bool {
        let Ok(receipt) = serde_json::from_value::<FinishReceipt>(data["finish_decision"].clone())
        else {
            return false;
        };
        if data["finish_request_id"] != receipt.request_id
            || receipt.request_id.is_empty()
            || receipt.request_id.len() > 128
            || !receipt
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_graphic())
            || (receipt.consumed && !receipt.human_permission())
            || serde_json::to_vec(&receipt).map_or(true, |bytes| bytes.len() > 1024)
        {
            return false;
        }
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = waits.records.get_mut(client_id) else {
            return false;
        };
        if record.intent != Intent::Finish
            || record.recipient.agent_id().is_none()
            || record
                .finish_request_id
                .as_ref()
                .is_some_and(|id| id != &receipt.request_id)
            || matches!(
                record.outcome,
                Outcome::AuthorizationRevoked | Outcome::BindingInvalid | Outcome::Disconnected
            )
        {
            return false;
        }
        record.finish_request_id = Some(receipt.request_id.clone());
        record.finish_decision = Some(receipt);
        true
    }

    pub(crate) fn finish_request_id(&self, client_id: &str) -> Option<String> {
        self.waits
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .records
            .get(client_id)
            .and_then(|record| record.finish_request_id.clone())
    }

    pub(crate) fn finish_decision_ready(&self, client_id: &str) -> bool {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(record) = waits.records.get_mut(client_id)
            && record.intent == Intent::Finish
            && record.outcome == Outcome::Waiting
            && record.finish_decision.as_ref().is_some_and(|receipt| {
                matches!(
                    receipt.status,
                    FinishStatus::Denied
                        | FinishStatus::Cancelled
                        | FinishStatus::Invalidated
                        | FinishStatus::Completed
                )
            })
        {
            record.outcome = Outcome::DecisionReady;
            true
        } else {
            false
        }
    }

    /// Called only after the final synchronous OAuth/account/binding recheck.
    /// Native completion alone is not the outer response's approval receipt.
    pub(crate) fn commit_finish_wait(&self, client_id: &str, episode: &str) {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = waits.records.get_mut(client_id) else {
            return;
        };
        if record.outcome != Outcome::DecisionReady {
            return;
        }
        if record.finish_episode.as_deref() != Some(episode) {
            record.outcome = Outcome::Superseded;
            record.reason = Some("user_input_changed_before_finish_delivery");
        } else {
            record.outcome = Outcome::Decided;
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
        response: crate::steering::Response<'_>,
        max_messages: usize,
        max_bytes: usize,
    ) -> Vec<Value> {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let Some(record) = waits.records.get_mut(response.client_id) else {
            return Vec::new();
        };
        if record.outcome != Outcome::Ready
            || record.recipient.owner != response.owner
            || record.recipient.agent_id() != response.agent_id
        {
            return Vec::new();
        }
        let Some(mut reservation) = record.reservation.take() else {
            return Vec::new();
        };
        let claimed = reservation.commit(response, max_messages, max_bytes);
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
        claimed
    }

    /// Synchronous release precedes any best-effort native cancellation RPC.
    pub(crate) fn cancel_user_waits(&self) -> Vec<(String, String, Option<String>)> {
        self.cancel_waits(true)
    }

    pub(crate) fn cancel_unfinished_user_waits(&self) -> Vec<(String, String, Option<String>)> {
        self.cancel_waits(false)
    }

    fn cancel_waits(&self, include_ready: bool) -> Vec<(String, String, Option<String>)> {
        let mut waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        waits
            .records
            .values_mut()
            .filter_map(|record| {
                if record.outcome != Outcome::Waiting
                    && !(include_ready
                        && matches!(record.outcome, Outcome::Ready | Outcome::DecisionReady))
                {
                    return None;
                }
                record.reservation.take();
                record.outcome = Outcome::Cancelled;
                record.reason = Some("code_mode_cancelled_before_delivery");
                Some((
                    record.client_id.clone(),
                    record.wait_id.clone(),
                    record.finish_request_id.clone(),
                ))
            })
            .collect()
    }

    pub(crate) fn user_wait_reserve_bytes(&self) -> usize {
        let waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        if waits.records.is_empty() {
            0
        } else {
            ENVELOPE_RESERVE_BYTES
        }
    }

    pub(crate) fn user_wait_addition(&self) -> Value {
        let waits = self.waits.lock().unwrap_or_else(|error| error.into_inner());
        let receipts: Vec<_> = waits.records.values().map(Record::receipt).collect();
        let mut fields = serde_json::Map::new();
        let mut content = Vec::new();
        if !receipts.is_empty() {
            // Keep the complete receipt in structuredContent. The text mirror
            // needs only the decision facts and actionable next wait: copying
            // eight full instructions/decision payloads twice would exceed the
            // existing reserved envelope. Ordinary wait text stays unchanged.
            let text_receipts: Vec<_> = receipts
                .iter()
                .map(|receipt| {
                    if receipt["intent"] != "finish" {
                        return receipt.clone();
                    }
                    json!({"client_id":receipt["client_id"],"wait_id":receipt["wait_id"],
                    "recipient":receipt["recipient"],"intent":receipt["intent"],
                    "status":receipt["status"],"may_finish":receipt["may_finish"],
                    "finish_request_id":receipt["finish_request_id"],
                    "decision_status":receipt["finish_decision"]["status"],
                    "choice":receipt["finish_decision"]["choice"],
                    "actor_origin":receipt["finish_decision"]["actor"]["origin"],
                    "revision":receipt["finish_decision"]["revision"],
                    "consumed":receipt["finish_decision"]["consumed"],
                    "next_wait":receipt.get("next_wait")})
                })
                .collect();
            fields.insert("user_wait".into(), json!(receipts));
            content.push(json!({"type":"text","text":format!("USER WAIT\n{}\nOnly status received with matching user_messages confirms message delivery. End only when this outer receipt has status decided and may_finish true with a consumed Human Yes for the exact thread. Otherwise keep the task active: handle new input or explicit No, then actually await next_wait in a new outer tool call. Reuse only the pending request ID supplied in next_wait, never a terminal decision ID. Timeout, cancellation, reading a decision, timers, background sleep, polling, same-run loops and final answers do not substitute for the blocking wait. Full evidence and completion_instruction are in structuredContent.user_wait.", json!(text_receipts))}));
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
            json!({"finish_request_id":"question_existing"}),
            json!({"intent":"finish","finish_request_id":"invalid id"}),
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
        let messages = run.commit_user_wait(
            crate::steering::Response {
                client_id: "room",
                run_id: &run.context.run_id,
                owner: "verified-owner",
                agent_id: None,
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
        assert!(receipt.get("user_consent_to_end").is_none());
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
    fn ordinary_waits_keep_their_contract_and_finish_waits_require_real_continuation() {
        let plain = AuditRun::new("return {kind:'answer'};", None).unwrap();
        assert_eq!(plain.user_wait_addition()["content"], json!([]));
        assert_eq!(plain.user_wait_reserve_bytes(), 0);
        for intent in [Intent::NeedUser, Intent::Finish] {
            for outcome in [Outcome::TimedOut, Outcome::Cancelled, Outcome::Received] {
                let run = AuditRun::new("return await ui.wait('room');", None).unwrap();
                run.begin_user_wait("room", recipient(), intent).unwrap();
                run.finish_wait_state("room", outcome, "test");
                let addition = run.user_wait_addition();
                let fields = &addition["structuredContent"];
                assert_eq!(fields.as_object().unwrap().len(), 1);
                assert!(fields.get("continuation_reminder").is_none());
                assert!(fields["user_wait"][0].get("user_consent_to_end").is_none());
                assert!(!addition.to_string().contains("CONTINUATION REMINDER"));
                assert_eq!(
                    fields["user_wait"][0]["continue_wait"],
                    outcome == Outcome::TimedOut
                        || (intent == Intent::Finish && outcome == Outcome::Cancelled)
                );
                assert_eq!(
                    fields["user_wait"][0]
                        .get("completion_instruction")
                        .is_some(),
                    intent == Intent::Finish
                );
            }
        }
    }
    fn finish_run() -> AuditRun {
        let run = AuditRun::new("return await ui.wait('room',{intent:'finish'});", None).unwrap();
        let mut recipient = recipient();
        recipient.binding = Some(RunBinding {
            owner: recipient.owner.clone(),
            agent_id: "asking-agent".into(),
            token: "synthetic-token".into(),
        });
        run.begin_user_wait("room", recipient, Intent::Finish)
            .unwrap();
        run.begin_finish_wait(
            "room",
            Some("question_exact".into()),
            "episode_before".into(),
        );
        run
    }

    fn consumed_human_decision() -> Value {
        json!({"finish_request_id":"question_exact","finish_decision":{
            "request_id":"question_exact","status":"completed","choice":"yes",
            "actor":{"origin":"human"},"revision":7,"consumed":true,"invalidation_reason":null
        }})
    }

    #[test]
    fn finish_wait_permission_requires_exact_receipt_and_final_input_epoch() {
        let run = finish_run();
        let mut wrong = consumed_human_decision();
        wrong["finish_request_id"] = json!("question_other");
        assert!(!run.capture_finish_wait("room", &wrong));
        let mut forged = consumed_human_decision();
        forged["finish_decision"]["actor"] = json!({
            "origin":"agent","agent":{"provider":"github","subject":"42",
                "thread_id":"asking-agent","name":"synthetic"}
        });
        assert!(!run.capture_finish_wait("room", &forged));
        assert!(run.capture_finish_wait("room", &consumed_human_decision()));
        assert!(run.finish_decision_ready("room"));
        assert_eq!(run.user_wait_receipt("room")["status"], "decision_ready");
        assert_eq!(run.user_wait_receipt("room")["may_finish"], false);
        run.commit_finish_wait("room", "episode_after_new_input");
        assert_eq!(run.user_wait_receipt("room")["status"], "superseded");
        assert_eq!(run.user_wait_receipt("room")["may_finish"], false);

        let run = finish_run();
        assert!(run.capture_finish_wait("room", &consumed_human_decision()));
        assert!(run.finish_decision_ready("room"));
        run.commit_finish_wait("room", "episode_before");
        let receipt = &run.user_wait_addition()["structuredContent"]["user_wait"][0];
        assert_eq!(receipt["status"], "decided");
        assert_eq!(receipt["may_finish"], true);
        assert_eq!(receipt["finish_decision"]["revision"], 7);
    }

    #[test]
    fn revoked_cancelled_or_read_only_finish_receipts_never_allow_ending() {
        for outcome in [
            Outcome::AuthorizationRevoked,
            Outcome::BindingInvalid,
            Outcome::Disconnected,
            Outcome::Cancelled,
            Outcome::TimedOut,
        ] {
            let run = finish_run();
            assert!(run.capture_finish_wait("room", &consumed_human_decision()));
            assert!(run.finish_decision_ready("room"));
            run.finish_wait_state("room", outcome, "synthetic_terminal");
            run.commit_finish_wait("room", "episode_before");
            assert_eq!(run.user_wait_receipt("room")["may_finish"], false);
        }
        let run = finish_run();
        let mut read = consumed_human_decision();
        read["finish_decision"]["consumed"] = json!(false);
        assert!(run.capture_finish_wait("room", &read));
        assert!(run.finish_decision_ready("room"));
        run.commit_finish_wait("room", "episode_before");
        assert_eq!(run.user_wait_receipt("room")["may_finish"], false);
    }
    #[test]
    fn finish_continuation_only_reuses_a_live_request_and_never_a_terminal_decision() {
        for status in [
            "pending",
            "approved",
            "denied",
            "cancelled",
            "invalidated",
            "completed",
        ] {
            let run = finish_run();
            let mut decision = consumed_human_decision();
            decision["finish_decision"]["status"] = json!(status);
            decision["finish_decision"]["consumed"] = json!(false);
            if status == "denied" {
                decision["finish_decision"]["choice"] = json!("no");
            }
            if status == "pending" {
                decision["finish_decision"]["choice"] = Value::Null;
                decision["finish_decision"]["actor"] = Value::Null;
                decision["finish_decision"]["revision"] = Value::Null;
            }
            assert!(run.capture_finish_wait("room", &decision));
            run.finish_wait_state("room", Outcome::TimedOut, "deadline");
            let receipt = run.user_wait_receipt("room");
            assert_eq!(receipt["may_finish"], false);
            assert_eq!(receipt["task_active"], true);
            assert_eq!(receipt["next_wait"]["helper"], "ui.wait");
            assert_eq!(receipt["next_wait"]["new_tool_call"], true);
            assert_eq!(receipt["next_wait"]["arguments"]["intent"], "finish");
            assert_eq!(receipt["next_wait"]["arguments"]["timeout_ms"], 20_000);
            assert_eq!(
                receipt["next_wait"]["arguments"]["finish_request_id"] == "question_exact",
                matches!(status, "pending" | "approved")
            );
            assert!(
                receipt["completion_instruction"]
                    .as_str()
                    .unwrap()
                    .contains("new outer tool call")
            );
        }

        let run = finish_run();
        let mut no = consumed_human_decision();
        no["finish_decision"]["status"] = json!("denied");
        no["finish_decision"]["choice"] = json!("no");
        no["finish_decision"]["consumed"] = json!(false);
        assert!(run.capture_finish_wait("room", &no));
        assert!(run.finish_decision_ready("room"));
        run.commit_finish_wait("room", "episode_before");
        let receipt = run.user_wait_receipt("room");
        assert_eq!(receipt["status"], "decided");
        assert_eq!(receipt["may_finish"], false);
        assert!(
            receipt["next_wait"]["arguments"]
                .get("finish_request_id")
                .is_none()
        );
        assert!(
            receipt["completion_instruction"]
                .as_str()
                .unwrap()
                .contains("explicitly declined")
        );

        let run = finish_run();
        assert!(run.capture_finish_wait("room", &consumed_human_decision()));
        assert!(run.finish_decision_ready("room"));
        assert_eq!(run.user_wait_receipt("room")["may_finish"], false);
        run.commit_finish_wait("room", "episode_before");
        let receipt = run.user_wait_receipt("room");
        assert_eq!(receipt["may_finish"], true);
        assert!(receipt.get("next_wait").is_none());
        assert!(receipt.get("completion_instruction").is_none());
    }

    #[test]
    fn eight_finish_instructions_fit_the_existing_reserved_outer_envelope() {
        let run = AuditRun::new("return await Promise.all(waits);", None).unwrap();
        let request_id = "q".repeat(128);
        let mut decision = json!({"request_id":request_id,"status":"pending",
            "choice":null,"actor":null,"revision":null,"consumed":false,
            "invalidation_reason":""});
        let remaining = 1024 - serde_json::to_vec(&decision).unwrap().len();
        decision["invalidation_reason"] = json!("x".repeat(remaining));
        assert_eq!(serde_json::to_vec(&decision).unwrap().len(), 1024);
        for index in 0..8 {
            let client = format!("{index:064}");
            let mut recipient = recipient();
            recipient.binding = Some(RunBinding {
                owner: recipient.owner.clone(),
                agent_id: "a".repeat(128),
                token: "synthetic-token".into(),
            });
            run.begin_user_wait(&client, recipient, Intent::Finish)
                .unwrap();
            run.begin_finish_wait(&client, Some(request_id.clone()), "episode".into());
            assert!(run.capture_finish_wait(
                &client,
                &json!({
                    "finish_request_id":request_id,"finish_decision":decision
                })
            ));
            run.wait_notification(
                &client,
                &json!({"notification":{
                    "status":"unavailable","reason":"r".repeat(64),"deduplicated":false
                }}),
            );
            run.finish_wait_state(&client, Outcome::TimedOut, "wait_finished_without_delivery");
        }
        let addition = run.user_wait_addition();
        assert_eq!(
            addition["structuredContent"]["user_wait"]
                .as_array()
                .unwrap()
                .len(),
            8
        );
        let bytes = serde_json::to_vec(&addition).unwrap().len();
        assert!(
            bytes <= ENVELOPE_RESERVE_BYTES,
            "{bytes} exceeds {ENVELOPE_RESERVE_BYTES}"
        );
        assert_eq!(run.user_wait_reserve_bytes(), ENVELOPE_RESERVE_BYTES);
        for receipt in addition["structuredContent"]["user_wait"]
            .as_array()
            .unwrap()
        {
            assert_eq!(
                receipt["next_wait"]["arguments"]["finish_request_id"],
                request_id
            );
            assert_eq!(receipt["finish_decision"], decision);
            assert_eq!(receipt["may_finish"], false);
        }
    }
}
