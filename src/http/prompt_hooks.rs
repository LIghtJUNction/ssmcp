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

/// One native Session. Only a settled generation can certify an idle member.
#[derive(Default)]
pub(crate) struct ProbeInterest(Mutex<HashMap<String, MemberTimers>>);

#[derive(Default)]
struct MemberTimers {
    owner: String,
    dispatched: u64,
    confirmed: u64,
    idle: bool,
    may_report_task_failure: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Skip,
    Read(Option<u64>),
}

impl ProbeInterest {
    fn plan(&self, member: &code_mode::RunBinding) -> Probe {
        let members = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match members.get(&member.agent_id) {
            None if members.len() < 128 => Probe::Read(Some(0)),
            Some(state)
                if state.owner == member.owner
                    && !state.may_report_task_failure
                    && state.dispatched == state.confirmed
                    && state.dispatched != u64::MAX =>
            {
                if state.idle {
                    Probe::Skip
                } else {
                    Probe::Read(Some(state.dispatched))
                }
            }
            _ => Probe::Read(None),
        }
    }

    fn update(&self, member: &code_mode::RunBinding, f: impl FnOnce(&mut MemberTimers)) {
        let mut members = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if members.len() >= 128 && !members.contains_key(&member.agent_id) {
            return;
        }
        let state = members
            .entry(member.agent_id.clone())
            .or_insert_with(|| MemberTimers {
                owner: member.owner.clone(),
                ..Default::default()
            });
        if state.owner == member.owner {
            f(state);
        }
    }

    /// An extension or opaque command can create a native process whose crash
    /// is reported long after the call returns. An idle timer page cannot
    /// certify that such a process has finished. Only known passive operations
    /// can preserve this member's eligibility for the idle-clock shortcut.
    pub(crate) fn observe_operation(&self, member: &code_mode::RunBinding, operation: &Operation) {
        let passive = match operation {
            Operation::Status {} | Operation::Read { .. } | Operation::ListTools { .. } => true,
            Operation::CallTool { name, .. } => matches!(
                name.as_str(),
                TOOL | "ssmcp.time.now"
                    | "ssmcp.time.wait"
                    | "ssmcp.exec.status"
                    | "ssmcp.exec.read"
                    | "ssmcp.exec.wait"
                    | "ssmcp.output.read"
                    | "ssmcp.fs.stat"
                    | "ssmcp.fs.read"
                    | "ssmcp.fs.list"
                    | "ssmcp.client.capabilities"
                    | "ssmcp.ui.events"
                    | "ssmcp.ui.get"
                    | "ssmcp.ui.history"
                    | "ssmcp.ui.questions"
                    | "ssmcp.ui.question_delivery"
                    | "ssmcp.ui.question_ack"
                    | "ssmcp.ui.tool_summary"
                    | "ssmcp.resources.list"
                    | "ssmcp.resources.templates"
                    | "ssmcp.resources.read"
            ),
            _ => false,
        };
        if !passive {
            self.update(member, |state| state.may_report_task_failure = true);
        }
    }

    pub(crate) fn dispatched(&self, member: &code_mode::RunBinding) {
        self.update(member, |state| {
            state.dispatched = state.dispatched.saturating_add(1);
            state.idle = false;
        });
    }

    pub(crate) fn confirmed(&self, member: &code_mode::RunBinding) {
        self.update(member, |state| {
            state.confirmed = state.confirmed.saturating_add(1);
        });
    }

    fn saw_idle(&self, member: &code_mode::RunBinding, generation: u64) {
        self.update(member, |state| {
            if generation == state.dispatched
                && generation == state.confirmed
                && generation != u64::MAX
            {
                state.idle = true;
            }
        });
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
        "PROMPT HOOKS — retained until acknowledgement after processing; no human input or completion approval.\n{}",
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
        let Probe::Read(generation) = target.session.prompt_interests.plan(binding) else {
            continue;
        };
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
            if let Some(generation) = generation
                && events.is_empty()
                && ["memory_limit_hooks", "task_crash_hooks"]
                    .iter()
                    .all(|key| {
                        body.get(key)
                            .is_none_or(|value| value.as_array().is_some_and(Vec::is_empty))
                    })
                && (body["member_has_no_prompt_timers"] == true
                    || body["member_prompt_delivery_idle"] == true)
                && body["more"] == false
                && body["visibility_deferred"] == 0
            {
                target
                    .session
                    .prompt_interests
                    .saw_idle(binding, generation);
            }
            let failures = ["memory_limit_hooks", "task_crash_hooks"]
                .into_iter()
                .flat_map(|key| {
                    body.get(key)
                        .and_then(Value::as_array)
                        .filter(|items| items.len() <= MAX_EVENTS)
                        .into_iter()
                        .flatten()
                        .filter_map(move |item| {
                            let notice: crate::runtime::time::memory_hook::Notice =
                                serde_json::from_value(item.clone()).ok()?;
                            (notice.valid() && notice.page_key() == key).then_some(notice)
                        })
                })
                .take(MAX_EVENTS)
                .collect::<Vec<_>>();
            observed.push((index, events, failures));
        }
    }
    let mut hooks = Vec::new();
    let mut appended = None;
    // No await follows these authority checks. Preserve the exact connection;
    // a later Session with the same client ID cannot inherit this read.
    for (index, events, failures) in observed {
        let target = &eligible[index];
        if !same_binding(report, target) || response_authorization(report, target).is_none() {
            continue;
        }
        let binding = target.binding.as_ref().expect("same_binding checked");
        let failures = failures.into_iter().map(|notice| {
            let mut event = json!(notice);
            event["type"] = json!(notice.event_type());
            event["completion_permission"] = json!(false);
            event["body"] = json!(notice.body());
            let action = if matches!(
                notice,
                crate::runtime::time::memory_hook::Notice::Memory { .. }
            ) {
                "ack_memory"
            } else {
                "ack_task"
            };
            event["ack"] =
                json!({"tool":TOOL,"arguments":{"action":action,"job_id":notice.job_id()}});
            ("system_hook", event)
        });
        let timers = events
            .into_iter()
            .map(|event| ("agent_timer", json!(event)));
        for (provenance, event) in failures.chain(timers) {
            if hooks.len() == MAX_EVENTS {
                break;
            }
            hooks.push(
                json!({"schema_version":1,"provenance":provenance,"scope":"native_connection",
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
        first.saw_idle(&member, 0);
        assert_eq!(first.plan(&member), Probe::Skip);
        assert_eq!(ProbeInterest::default().plan(&member), Probe::Read(Some(0)));
        assert_eq!(first.plan(&binding(1)), Probe::Read(Some(0)));
        first.dispatched(&member);
        first.saw_idle(&member, 0);
        assert_eq!(first.plan(&member), Probe::Read(None));
        first.dispatched(&member);
        first.confirmed(&member);
        assert_eq!(first.plan(&member), Probe::Read(None));
        first.confirmed(&member);
        first.saw_idle(&member, 0);
        assert_eq!(
            first.plan(&member),
            Probe::Read(Some(2)),
            "late empty reply"
        );
        first.saw_idle(&member, 2);
        assert_eq!(first.plan(&member), Probe::Skip);
        for id in 1..=128 {
            first.saw_idle(&binding(id), 0);
        }
        assert_eq!(first.plan(&binding(128)), Probe::Read(None));
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
