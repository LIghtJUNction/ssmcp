//! Bounded relay metadata for reminding an actual recipient to answer locally.
//! No user text, reply body, credentials, or conversation history is retained.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, time::Instant};

pub(crate) const MAX_PENDING: usize = 128;
pub(crate) const MAX_NOTICES: usize = 8;
pub(crate) const MAX_NOTICE_BYTES: usize = 8 * 1024;
/// Covers the new structured-content key and container punctuation.
pub(crate) const NOTICE_ENVELOPE_BYTES: usize = 256;

#[derive(Clone, Debug)]
pub(crate) struct AcceptedReply {
    pub agent_id: Option<String>,
    pub received_at: Instant,
}

#[derive(Default)]
pub(crate) struct Receipts {
    entries: Vec<(String, AcceptedReply)>,
}

impl Receipts {
    /// Only Session's native reply path calls this; JavaScript return values and
    /// logs never enter this collection. Duplicate receipts are not new replies.
    pub(crate) fn observe(
        &mut self,
        client_id: &str,
        agent_id: Option<&str>,
        tool: Option<&str>,
        result: &Value,
        received_at: Instant,
    ) {
        if !matches!(tool, Some("ssmcp.ui.message" | "ssmcp.ui.present"))
            || result
                .get("isError")
                .is_some_and(|value| value != &Value::Bool(false))
            || result["structuredContent"]["accepted"] != true
            || result["structuredContent"]["duplicate"] != false
            || !result["structuredContent"]["message_id"]
                .as_str()
                .is_some_and(identifier)
            || !identifier(client_id)
            || agent_id.is_some_and(|id| !identifier(id))
        {
            return;
        }
        if let Some((_, reply)) = self
            .entries
            .iter_mut()
            .find(|(client, reply)| client == client_id && reply.agent_id.as_deref() == agent_id)
        {
            reply.received_at = reply.received_at.max(received_at);
        } else if self.entries.len() < super::MAX_CALLS {
            self.entries.push((
                client_id.into(),
                AcceptedReply {
                    agent_id: agent_id.map(str::to_owned),
                    received_at,
                },
            ));
        }
    }

    pub(crate) fn for_client(&self, client_id: &str) -> Vec<AcceptedReply> {
        self.entries
            .iter()
            .filter(|(client, _)| client == client_id)
            .map(|(_, reply)| reply.clone())
            .collect()
    }
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

pub(crate) struct Response<'a> {
    pub client_id: &'a str,
    pub run_id: &'a str,
    /// Verified bridge owner includes the authorization resource.
    pub owner: &'a str,
    /// Actual recipient, including Next requirements claimed by a bound agent.
    pub agent_id: Option<&'a str>,
    pub replies: &'a [AcceptedReply],
    pub notice_budget: usize,
}

#[derive(Debug)]
pub(crate) struct Notice {
    pub value: Value,
    pub content: Value,
    pub bytes: usize,
}

impl Notice {
    fn new(client_id: &str, pending: &Pending) -> Self {
        let value = json!({
            "kind":"terminal_reply_missing",
            "client_id":client_id,
            "agent_id":pending.agent_id,
            "after_run_id":pending.first_run_id,
            "responses_without_reply":pending.age,
        });
        let text = format!(
            "CONVERSATION REMINDER\n[Client {} · recipient {}]\n已收到终端用户消息，此后未观察到本终端接受新的对话回复；请用 ssmcp.ui.message 或 ssmcp.ui.present 在本终端发送进度或结果。外部聊天内容可能不会显示在此终端。\n",
            client_id,
            pending.agent_id.as_deref().unwrap_or("Next caller"),
        );
        let content = json!({"type":"text","text":text});
        // Both wire representations, plus their array separators.
        let bytes = value.to_string().len() + content.to_string().len() + 2;
        Self {
            value,
            content,
            bytes,
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    pending: VecDeque<Pending>,
}

struct Pending {
    owner: [u8; 32],
    agent_id: Option<String>,
    first_run_id: String,
    last_run_id: String,
    /// Latest claim, so an old concurrent ACK cannot erase newer requirements.
    delivered_at: Instant,
    age: u8,
    notified: bool,
}

impl Pending {
    fn belongs_to(&self, owner: &[u8; 32], agent_id: Option<&str>) -> bool {
        self.owner == *owner && self.agent_id.as_deref() == agent_id
    }
}

impl State {
    /// Called once at HTTP response finalization, under the owning Queue lock.
    /// A newly accepted reply clears previous requirements before this response's
    /// newly claimed requirements arm another episode.
    pub(crate) fn finish(
        &mut self,
        response: &Response<'_>,
        claimed: bool,
        now: Instant,
    ) -> Option<Notice> {
        if !identifier(response.client_id)
            || !identifier(response.run_id)
            || response.agent_id.is_some_and(|id| !identifier(id))
        {
            return None;
        }
        let owner: [u8; 32] = Sha256::digest(response.owner.as_bytes()).into();
        self.pending.retain(|pending| {
            !response.replies.iter().any(|reply| {
                pending.belongs_to(&owner, reply.agent_id.as_deref())
                    && reply.received_at >= pending.delivered_at
            })
        });
        let index = self
            .pending
            .iter()
            .position(|pending| pending.belongs_to(&owner, response.agent_id));
        let mut notice = None;
        if let Some(index) = index {
            // Recent use also bounds eviction fairly when many members share a
            // client; eviction drops reminder metadata, never user requirements.
            let mut pending = self.pending.remove(index).expect("known pending index");
            if pending.last_run_id != response.run_id {
                pending.last_run_id = response.run_id.into();
                pending.age = pending.age.saturating_add(1).min(2);
            }
            if pending.age >= 2 && !pending.notified {
                let candidate = Notice::new(response.client_id, &pending);
                if candidate.bytes <= response.notice_budget {
                    pending.notified = true;
                    notice = Some(candidate);
                }
            }
            if claimed {
                // More unanswered requirements must not postpone the reminder.
                pending.delivered_at = now;
            }
            self.pending.push_back(pending);
        } else if claimed {
            if self.pending.len() >= MAX_PENDING {
                self.pending.pop_front();
            }
            self.pending.push_back(Pending {
                owner,
                agent_id: response.agent_id.map(str::to_owned),
                first_run_id: response.run_id.into(),
                last_run_id: response.run_id.into(),
                delivered_at: now,
                age: 0,
                notified: false,
            });
        }
        notice
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn response<'a>(run_id: &'a str) -> Response<'a> {
        Response {
            client_id: "client",
            run_id,
            owner: "resource\0github\0subject",
            agent_id: None,
            replies: &[],
            notice_budget: MAX_NOTICE_BYTES - NOTICE_ENVELOPE_BYTES,
        }
    }

    fn accepted() -> Value {
        json!({"structuredContent":{
            "accepted":true,"duplicate":false,"message_id":"message_test"
        }})
    }

    #[test]
    fn reminders_follow_n_plus_two_once_and_defer_without_budget() {
        let mut state = State::default();
        let now = Instant::now();
        assert!(state.finish(&response("n"), true, now).is_none());
        assert!(state.finish(&response("n"), false, now).is_none());
        assert!(state.finish(&response("n1"), false, now).is_none());
        let mut small = response("n2");
        small.notice_budget = 0;
        assert!(state.finish(&small, false, now).is_none());
        let notice = state.finish(&response("n3"), false, now).unwrap();
        assert_eq!(notice.value["after_run_id"], "n");
        assert_eq!(notice.value["responses_without_reply"], 2);
        assert!(
            notice.content["text"]
                .as_str()
                .unwrap()
                .contains("未观察到")
        );
        assert!(state.finish(&response("n4"), false, now).is_none());
    }

    #[test]
    fn more_user_messages_do_not_postpone_a_pending_reminder() {
        let mut state = State::default();
        let now = Instant::now();
        let _ = state.finish(&response("n"), true, now);
        assert!(state.finish(&response("n1"), true, now).is_none());
        assert!(state.finish(&response("n2"), true, now).is_some());
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn reply_clears_old_episode_but_new_claim_in_that_response_starts_another() {
        let mut state = State::default();
        let now = Instant::now();
        let _ = state.finish(&response("n"), true, now);
        let replies = [AcceptedReply {
            agent_id: None,
            received_at: now + Duration::from_secs(1),
        }];
        let mut answered = response("n1");
        answered.replies = &replies;
        assert!(
            state
                .finish(&answered, true, now + Duration::from_secs(2))
                .is_none()
        );
        assert!(state.finish(&response("n2"), false, now).is_none());
        let notice = state.finish(&response("n3"), false, now).unwrap();
        assert_eq!(notice.value["after_run_id"], "n1");
        let mut cleared = response("n4");
        let replies = [AcceptedReply {
            agent_id: None,
            received_at: now + Duration::from_secs(3),
        }];
        cleared.replies = &replies;
        assert!(state.finish(&cleared, false, now).is_none());
        assert!(state.pending.is_empty());
    }

    #[test]
    fn a_stale_concurrent_ack_cannot_clear_a_later_claim() {
        let mut state = State::default();
        let now = Instant::now();
        let _ = state.finish(&response("n"), true, now);
        let _ = state.finish(&response("n1"), true, now + Duration::from_secs(2));
        let replies = [AcceptedReply {
            agent_id: None,
            received_at: now + Duration::from_secs(1),
        }];
        let mut late = response("n2");
        late.replies = &replies;
        assert!(
            state
                .finish(&late, false, now + Duration::from_secs(3))
                .is_some()
        );
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn owner_resource_and_actual_agent_are_separate_from_unbound_next() {
        let mut state = State::default();
        let now = Instant::now();
        let _ = state.finish(&response("n"), true, now);
        let mut agent = response("a");
        agent.agent_id = Some("agent_a");
        let _ = state.finish(&agent, true, now);
        let mut other = response("other1");
        other.owner = "different_resource\0github\0subject";
        let _ = state.finish(&other, false, now);
        other.run_id = "other2";
        assert!(state.finish(&other, false, now).is_none());
        let replies = [AcceptedReply {
            agent_id: Some("agent_b".into()),
            received_at: now,
        }];
        let mut next = response("n1");
        next.replies = &replies;
        let _ = state.finish(&next, false, now);
        let notice = state.finish(&response("n2"), false, now).unwrap();
        assert!(notice.value["agent_id"].is_null());
        agent.run_id = "a1";
        assert!(state.finish(&agent, false, now).is_none());
        agent.run_id = "a2";
        assert_eq!(
            state.finish(&agent, false, now).unwrap().value["agent_id"],
            "agent_a"
        );
    }

    #[test]
    fn actual_reply_binding_is_not_replaced_by_the_final_binding() {
        let mut state = State::default();
        let now = Instant::now();
        let _ = state.finish(&response("n"), true, now);
        let replies = [AcceptedReply {
            agent_id: None,
            received_at: now,
        }];
        let mut later_bound = response("n1");
        later_bound.agent_id = Some("agent_a");
        later_bound.replies = &replies;
        let _ = state.finish(&later_bound, true, now);
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.pending[0].agent_id.as_deref(), Some("agent_a"));
    }

    #[test]
    fn reminder_metadata_is_bounded_and_contains_no_user_body() {
        let mut state = State::default();
        let now = Instant::now();
        for i in 0..MAX_PENDING + 10 {
            let owner = format!("owner_{i}");
            let mut item = response("n");
            item.owner = &owner;
            let _ = state.finish(&item, true, now);
        }
        assert_eq!(state.pending.len(), MAX_PENDING);
        assert!(
            state
                .pending
                .iter()
                .all(|pending| pending.first_run_id == "n")
        );
    }

    #[test]
    fn only_new_accepted_native_dialogue_receipts_count() {
        let now = Instant::now();
        for tool in ["ssmcp.ui.message", "ssmcp.ui.present"] {
            let mut receipts = Receipts::default();
            receipts.observe("client", None, Some(tool), &accepted(), now);
            assert_eq!(receipts.for_client("client").len(), 1);
            assert!(receipts.for_client("other").is_empty());
        }
        let mut invalid = Vec::new();
        let mut duplicate = accepted();
        duplicate["structuredContent"]["duplicate"] = json!(true);
        invalid.push(duplicate);
        let mut error = accepted();
        error["isError"] = json!(true);
        invalid.push(error);
        let mut rejected = accepted();
        rejected["structuredContent"]["accepted"] = json!(false);
        invalid.push(rejected);
        let mut missing_duplicate = accepted();
        missing_duplicate["structuredContent"]
            .as_object_mut()
            .unwrap()
            .remove("duplicate");
        invalid.push(missing_duplicate);
        let mut missing_id = accepted();
        missing_id["structuredContent"]["message_id"] = json!("");
        invalid.push(missing_id);
        invalid.push(json!({"result":accepted()}));
        for value in invalid {
            let mut receipts = Receipts::default();
            receipts.observe("client", None, Some("ssmcp.ui.message"), &value, now);
            assert!(receipts.for_client("client").is_empty());
        }
        let mut receipts = Receipts::default();
        receipts.observe("client", None, Some("ssmcp.exec.run"), &accepted(), now);
        receipts.observe("client", None, None, &accepted(), now);
        assert!(receipts.entries.is_empty());
    }

    #[test]
    fn native_receipts_are_bounded_and_retain_the_latest_real_ack() {
        let mut receipts = Receipts::default();
        let now = Instant::now();
        for i in 0..super::super::MAX_CALLS + 10 {
            receipts.observe(
                &format!("client_{i}"),
                None,
                Some("ssmcp.ui.message"),
                &accepted(),
                now,
            );
        }
        assert_eq!(receipts.entries.len(), super::super::MAX_CALLS);
        let later = now + Duration::from_secs(1);
        receipts.observe(
            "client_0",
            None,
            Some("ssmcp.ui.message"),
            &accepted(),
            later,
        );
        receipts.observe("client_0", None, Some("ssmcp.ui.message"), &accepted(), now);
        assert_eq!(receipts.for_client("client_0")[0].received_at, later);
    }
}
