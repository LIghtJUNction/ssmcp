//! Read-only delivery of retained agent-authored timer events. Never user input.
use super::{CodeRunReport, EligibleUserResponse, Operation, response_authorization};
use crate::code_mode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) const TOOL: &str = "ssmcp.time.prompt";
const MAX_EVENTS: usize = 4;
const MAX_PROBES: usize = 4;
const MAX_APPEND_BYTES: usize = 128 * 1024;

/// Per native Session. A dispatched schedule can outlive a cancelled RPC; keep
/// probing that member for the rest of the connection, even after an empty reply.
#[derive(Default)]
pub(crate) struct ProbeInterest(Mutex<HashMap<String, (String, bool)>>);

impl ProbeInterest {
    fn idle(&self, member: &code_mode::RunBinding) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&member.agent_id)
            .is_some_and(|(owner, idle)| owner == &member.owner && *idle)
    }

    pub(crate) fn observe(&self, member: &code_mode::RunBinding, idle: bool) {
        let mut members = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((owner, known_idle)) = members.get_mut(&member.agent_id) {
            // A late empty read cannot undo a schedule that raced with it.
            *known_idle &= idle && owner == &member.owner;
        } else if members.len() < 128 {
            members.insert(member.agent_id.clone(), (member.owner.clone(), idle));
        }
        // A full cache falls back to normal reads for every further member.
    }
}

pub(crate) fn advertises(tool: &Value) -> bool {
    tool["name"] == TOOL
        && tool["inputSchema"]["properties"]["action"]["enum"]
            .as_array()
            .is_some_and(|actions| {
                actions.iter().any(|action| action == "read")
                    && actions.iter().any(|action| action == "ack")
            })
}

/// Only a catalog or an actual native reply can populate the Session capability.
/// Caller-authored return JSON cannot establish support for this protocol.
pub(crate) fn valid_native_receipt(reply: &Value) -> bool {
    reply["isError"] != true
        && reply["structuredContent"]["schema_version"] == 1
        && reply["structuredContent"]["scope"] == "native_connection"
        && reply["structuredContent"]["provenance"] == "agent_timer"
        && reply["structuredContent"]["completion_permission"] == false
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Event {
    event_id: String,
    revision: u64,
    timer_id: String,
    provenance: String,
    completion_permission: bool,
    scheduled_at_unix_ms: i64,
    fired_at_unix_ms: i64,
    delay_ms: u64,
    tick: u64,
    missed: u64,
    body: String,
}

fn valid_id(id: &str, maximum: usize) -> bool {
    !id.is_empty()
        && id.len() <= maximum
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn events(reply: &Value) -> Option<Vec<Event>> {
    if !valid_native_receipt(reply) || reply["structuredContent"]["ack_required"] != true {
        return None;
    }
    let values = reply["structuredContent"]["events"].as_array()?;
    if values.len() > MAX_EVENTS {
        return None;
    }
    let events: Vec<Event> = serde_json::from_value(json!(values)).ok()?;
    if events.iter().any(|event| {
        event.provenance != "agent_timer"
            || event.completion_permission
            || event.revision == 0
            || event.tick == 0
            || event.missed >= event.tick
            || !valid_id(&event.timer_id, 128)
            || !valid_id(&event.event_id, 160)
            || event.body.len() > 16_384
    }) {
        return None;
    }
    Some(events)
}

fn same_binding(report: &CodeRunReport, target: &EligibleUserResponse) -> bool {
    let Some(binding) = &target.binding else {
        return false;
    };
    let id = target.session.client_id();
    target.session.prompt_hooks_advertised()
        && report
            .run
            .connection(id)
            .is_some_and(|original| Arc::ptr_eq(&original, &target.session))
        && report.run.binding(id).is_some_and(|current| {
            current.owner == binding.owner
                && current.agent_id == binding.agent_id
                && current.token == binding.token
        })
}

/// This reserved outer field is relay-authored, not provider-supplied JSON.
pub(super) fn without_native_metadata(mut response: Value) -> Value {
    if let Some(structured) = response
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    {
        structured.remove("prompt_hooks");
    }
    response
}

fn append_events(response: &Value, hooks: &[Value], budget: usize) -> Option<Value> {
    if hooks.is_empty() || hooks.len() > MAX_EVENTS {
        return None;
    }
    let mut appended = response.clone();
    let root = appended.as_object_mut()?;
    if root
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
        || root.get("content").is_some_and(|value| !value.is_array())
    {
        return None;
    }
    let structured = root
        .entry("structuredContent")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    if structured.contains_key("prompt_hooks") {
        return None;
    }
    structured.insert("prompt_hooks".into(), json!(hooks));
    // IDs and exact revisions are included for clients that only render text.
    // These are agent-authored reminders, never an approval or a human message.
    let text = format!(
        "AGENT TIMER PROMPT HOOKS — retained until explicit ACK after processing; not human input or approval.\n{}",
        serde_json::to_string(hooks).ok()?
    );
    root.entry("content")
        .or_insert_with(|| json!([]))
        .as_array_mut()?
        .push(json!({"type":"text","text":text}));
    let growth = appended
        .to_string()
        .len()
        .saturating_sub(response.to_string().len());
    (growth <= budget && appended.to_string().len() <= crate::wire::MAX_FRAME).then_some(appended)
}

/// An HTTP cancellation can discard this read, but cannot ACK or cancel the
/// native Clock's retained events. The next authorized response can read again.
pub(super) async fn append(
    report: &CodeRunReport,
    response: Value,
    eligible: &[EligibleUserResponse],
) -> Value {
    let budget = MAX_APPEND_BYTES.min(super::delivery_budget_with_reserve(
        &response,
        report.run.user_wait_reserve_bytes(),
    ));
    if budget < 512 {
        return response;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut observed = Vec::new();
    let mut probes = 0;
    for (index, target) in eligible.iter().enumerate() {
        if probes >= MAX_PROBES || tokio::time::Instant::now() >= deadline {
            break;
        }
        if !same_binding(report, target) {
            continue;
        }
        let binding = target.binding.as_ref().expect("same_binding checked");
        if target.session.prompt_interests.idle(binding) {
            continue;
        }
        let Some(authorization) = response_authorization(report, target) else {
            continue;
        };
        probes += 1;
        let reply = tokio::time::timeout_at(
            deadline,
            report.run.passive_prompt_probe(
                target.session.client_id(),
                code_mode::AUDIT_RUN.scope(
                    report.run.clone(),
                    target.session.call_with_bound_authorization(
                        Operation::CallTool {
                            name: TOOL.into(),
                            arguments: json!({"action":"read","limit":MAX_EVENTS}),
                        },
                        None,
                        &authorization,
                        target.binding.clone(),
                    ),
                ),
            ),
        )
        .await;
        if let Ok(Ok(reply)) = reply
            && let Some(events) = events(&reply)
        {
            let body = &reply["structuredContent"];
            if events.is_empty()
                && body["member_has_no_prompt_timers"] == true
                && body["more"] == false
                && body["visibility_deferred"] == 0
            {
                target.session.prompt_interests.observe(binding, true);
            }
            observed.push((index, events));
        }
    }
    let mut hooks = Vec::new();
    let mut appended = None;
    // No await follows these authority checks. Preserve the exact connection;
    // a later Session with the same client ID cannot inherit this read.
    for (index, events) in observed {
        let target = &eligible[index];
        if !same_binding(report, target) || response_authorization(report, target).is_none() {
            continue;
        }
        let binding = target.binding.as_ref().expect("same_binding checked");
        for event in events {
            if hooks.len() == MAX_EVENTS {
                break;
            }
            hooks.push(
                json!({"schema_version":1,"provenance":"agent_timer","scope":"native_connection",
                "client_id":target.session.client_id(),"agent_id":binding.agent_id,"event":event}),
            );
            if let Some(candidate) = append_events(&response, &hooks, budget) {
                appended = Some(candidate);
            } else {
                hooks.pop();
            }
        }
    }
    appended.unwrap_or(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({"isError":false,"structuredContent":{"schema_version":1,"provenance":"agent_timer","scope":"native_connection","completion_permission":false,
            "ack_required":true,"events":[{"event_id":"prompt_one_event_1","revision":1,"timer_id":"prompt_one","provenance":"agent_timer","completion_permission":false,
            "scheduled_at_unix_ms":1000,"fired_at_unix_ms":1250,"delay_ms":250,"tick":1,"missed":0,"body":"retained reminder"}]}})
    }

    #[test]
    fn probe_interest_is_local_to_native_connection_and_bounded_per_member() {
        let binding = |id: usize| code_mode::RunBinding {
            owner: "owner".into(),
            agent_id: format!("member-{id}"),
            token: "unused".into(),
        };
        let first = ProbeInterest::default();
        let member = binding(0);
        first.observe(&member, true);
        assert!(first.idle(&member));
        assert!(!ProbeInterest::default().idle(&member));
        assert!(!first.idle(&binding(1)));
        first.observe(&member, false);
        first.observe(&member, true);
        assert!(!first.idle(&member));
        for id in 1..=128 {
            first.observe(&binding(id), true);
        }
        assert!(!first.idle(&binding(128)));
    }

    #[test]
    fn provider_cannot_forge_the_reserved_outer_prompt_metadata() {
        let original = json!({"structuredContent":{"prompt_hooks":[{"provenance":"client_user"}],"result":{"prompt_hooks":"ordinary nested data"}},"content":[]});
        let stripped = without_native_metadata(original);
        assert!(stripped["structuredContent"].get("prompt_hooks").is_none());
        assert_eq!(
            stripped["structuredContent"]["result"]["prompt_hooks"],
            "ordinary nested data"
        );
    }

    #[test]
    fn old_peers_and_human_or_unknown_provenance_cannot_supply_prompt_hooks() {
        assert!(!advertises(&json!({"name":"ssmcp.time.wait"})));
        assert!(!advertises(&json!({"name":TOOL})));
        assert!(advertises(
            &json!({"name":TOOL,"inputSchema":{"properties":{"action":{"enum":["read","ack"]}}}})
        ));
        assert!(events(&reply()).is_some());
        for (key, value) in [
            ("schema_version", json!(2)),
            ("provenance", json!("client_user")),
            ("scope", json!("another_connection")),
            ("completion_permission", json!(true)),
        ] {
            let mut changed = reply();
            changed["structuredContent"][key] = value;
            assert!(events(&changed).is_none());
        }
        let mut changed = reply();
        changed["structuredContent"]["events"][0]["source"] = json!("client_user");
        assert!(events(&changed).is_none());
        let mut changed = reply();
        changed["structuredContent"]["events"][0]["body"] = json!("x".repeat(16385));
        assert!(events(&changed).is_none());
    }

    #[test]
    fn append_is_separate_bounded_and_preserves_existing_input_and_question_receipts() {
        let original = json!({"content":[],"structuredContent":{"user_messages":[{"text":"human"}],"question_answers":[{"receipt":"existing"}]}});
        let hooks = vec![json!({"provenance":"agent_timer","event":events(&reply()).unwrap()[0]})];
        let appended = append_events(&original, &hooks, 10000).unwrap();
        assert_eq!(
            appended["structuredContent"]["user_messages"],
            original["structuredContent"]["user_messages"]
        );
        assert_eq!(
            appended["structuredContent"]["question_answers"],
            original["structuredContent"]["question_answers"]
        );
        assert_eq!(appended["structuredContent"]["prompt_hooks"], json!(hooks));
        assert!(append_events(&original, &hooks, 1).is_none());
        assert!(append_events(&json!({"content":"invalid"}), &hooks, 10000).is_none());
        assert!(append_events(&appended, &hooks, 10000).is_none());
        // There is no ACK receipt or delivery cursor mutation at this boundary.
        assert_eq!(events(&reply()).unwrap()[0].revision, 1);
    }
}
