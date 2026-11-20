//! A bounded read-only projection of event provenance. JSON payloads and display
//! names never supply the account, connection, main or agent filter.
use super::{Capture, DisplaySource, history_capture};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};

const PAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
pub(super) struct RecordScope {
    client_id: Option<String>,
    agent_id: Option<String>,
    kind: &'static str,
    connection: u64,
    request_id: u64,
    run_id: Option<String>,
    tool: Option<String>,
    account: Option<Value>,
}

impl RecordScope {
    pub(super) fn retained_bytes(&self) -> usize {
        // Count the retained typed copy as well as its encoded frame. The
        // allowance covers strings, optional JSON account fields and nodes.
        384 + self.client_id.as_ref().map_or(0, String::len)
            + self.agent_id.as_ref().map_or(0, String::len)
            + self.run_id.as_ref().map_or(0, String::len)
            + self.tool.as_ref().map_or(0, String::len)
            + self.account.as_ref().map_or(0, |value| {
                256 + value["provider"].as_str().map_or(0, str::len)
                    + value["subject"].as_str().map_or(0, str::len)
            })
    }

    pub(super) fn from_source(source: &DisplaySource) -> Option<Self> {
        let call = source.call.as_ref()?;
        let attribution = call.attribution();
        let thread = call.thread();
        let authenticated = attribution.is_some();
        let exact_thread = thread.filter(|thread| {
            attribution
                .and_then(|value| value.agent.as_ref())
                .is_some_and(|agent| agent.id == thread.thread_id())
        });
        let main = authenticated
            && thread.is_none()
            && attribution.is_some_and(|value| value.agent.is_none())
            && call.client_id().is_some();
        Some(Self {
            client_id: call.client_id().map(str::to_owned),
            agent_id: exact_thread.map(|thread| thread.thread_id().to_owned()),
            kind: if exact_thread.is_some() {
                "thread"
            } else if main {
                "main"
            } else {
                "unattributed"
            },
            connection: call.connection(),
            request_id: call.request_id(),
            run_id: call.run_id().map(str::to_owned),
            tool: call.native_tool().map(str::to_owned),
            account: attribution.map(|value| {
                json!({
                "provider":value.account.provider,"subject":value.account.subject})
            }),
        })
    }
}

pub(super) fn observer(name: &str) -> bool {
    matches!(
        name,
        "ssmcp.ui.history"
            | "ssmcp.ui.get"
            | "ssmcp.ui.tool_summary"
            | "ssmcp.ui.question_delivery"
            | "ssmcp.ui.question_ack"
            | "ssmcp.client.capabilities"
            | "ssmcp.resources.list"
            | "ssmcp.resources.templates"
            | "ssmcp.resources.read"
    )
}

/// One actual dispatch supplies its own stable provenance even when a provider
/// emits no output. History reads and internal observations are transparent.
pub(crate) struct ToolCall {
    source: Option<DisplaySource>,
    name: String,
}

impl ToolCall {
    pub(crate) fn start(source: &crate::runtime::call_scope::Source, name: &str) -> Self {
        let source = (!observer(name) && source.call.is_some()).then(|| source.display());
        if let Some(source) = &source {
            super::record_scoped_event(
                "tool",
                &json!({"type":"ssmcp.tool_call","tool":name,
                "phase":"started","status":"running","text":name}),
                source,
            );
        }
        Self {
            source,
            name: name.into(),
        }
    }

    pub(crate) fn finish(&mut self, success: bool) {
        if let Some(source) = self.source.take() {
            super::record_scoped_event(
                "tool",
                &json!({"type":"ssmcp.tool_call","tool":self.name,
                "phase":"completed","status":if success{"done"}else{"error"},
                "success":success,"text":self.name}),
                &source,
            );
        }
    }
}

impl Drop for ToolCall {
    fn drop(&mut self) {
        if let Some(source) = self.source.take() {
            super::record_scoped_event(
                "tool",
                &json!({"type":"ssmcp.tool_call","tool":self.name,
                "phase":"cancelled","status":"cancelled","text":self.name}),
                &source,
            );
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Selector {
    pub(crate) client_id: String,
    pub(crate) agent_id: Option<String>,
    pub(crate) main: bool,
}

impl Selector {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.main && self.agent_id.is_some() {
            return Err("history main and agent_id are mutually exclusive".into());
        }
        let recipient = self.agent_id.as_ref().map_or_else(
            || format!("client:{}", self.client_id),
            |agent| format!("agent:{}:{agent}", self.client_id),
        );
        crate::wire::messages::parse_recipient(&recipient).map(|_| ())
    }

    fn kind(&self) -> &'static str {
        if self.agent_id.is_some() {
            "thread"
        } else if self.main {
            "main"
        } else {
            "process"
        }
    }

    fn matches(&self, scope: Option<&RecordScope>) -> bool {
        match scope {
            Some(scope) if scope.client_id.as_deref() == Some(self.client_id.as_str()) => {
                if let Some(agent) = &self.agent_id {
                    scope.kind == "thread" && scope.agent_id.as_ref() == Some(agent)
                } else if self.main {
                    scope.kind == "main"
                } else {
                    true
                }
            }
            None
            | Some(RecordScope {
                client_id: None, ..
            }) => !self.main && self.agent_id.is_none(),
            _ => false,
        }
    }

    pub(crate) fn value(&self) -> Value {
        json!({"kind":self.kind(),"client_id":self.client_id,"agent_id":self.agent_id,
            "provenance":"authenticated_call_scope","includes_unattributed":self.kind()=="process"})
    }
}

fn bounded(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut end = maximum;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn project(frame: Value, sequence: u64, scope: Option<&RecordScope>) -> Value {
    let value = &frame["value"];
    let event_type = bounded(value["type"].as_str().unwrap_or("event"), 128);
    let kind = if event_type == "ssmcp.message" {
        match value["kind"].as_str() {
            Some("answer") => "answer",
            _ => "progress",
        }
    } else {
        event_type.as_str()
    };
    let mut text = Vec::new();
    let mut text_lossy = false;
    for key in [
        "text", "message", "summary", "stdout", "stderr", "output", "chunk",
    ] {
        if let Some(part) = value[key].as_str().filter(|text| !text.is_empty()) {
            text.push(bounded(part, 8192));
        }
    }
    if text.is_empty()
        && let Some(encoded) = value["data_base64"]
            .as_str()
            .or_else(|| value.pointer("/params/deltaBase64").and_then(Value::as_str))
        && encoded.len() <= super::EVENT_BYTES.saturating_mul(2)
        && let Ok(bytes) = STANDARD.decode(encoded)
    {
        text_lossy = std::str::from_utf8(&bytes).is_err();
        text.push(bounded(&String::from_utf8_lossy(&bytes), 8192));
    }
    if text.is_empty() {
        text.push(value.get("command").map_or_else(
            || event_type.to_owned(),
            |command| {
                bounded(
                    command
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| command.to_string())
                        .as_str(),
                    8192,
                )
            },
        ));
    }
    let source = scope.map_or_else(
        || json!({"kind":"unattributed","client_id":null,"agent_id":null}),
        |scope| serde_json::to_value(scope).expect("record scope serializes"),
    );
    let mut result = json!({"id":format!("event_{sequence}"),"sequence":sequence,
        "kind":kind,"text":bounded(&text.join("\n"),8192),"source":source,
        "text_lossy":text_lossy,
        "unattributed":scope.is_none_or(|scope|scope.kind=="unattributed"),
        "value":value,"truncated":frame["truncated"]==true});
    for key in ["status", "phase", "success", "exit_code", "job_id"] {
        if let Some(field) = value.get(key).filter(|value| {
            value.is_boolean()
                || value.is_number()
                || value.as_str().is_some_and(|text| text.len() <= 256)
        }) {
            result[key] = field.clone();
        }
    }
    if let Some(command) = value.get("command") {
        result["command_source"] = json!({"command":bounded(&command.to_string(),8192),
            "cwd":value["cwd"].as_str().map(|cwd|bounded(cwd,4096)),
            "job_id":value["job_id"].as_str().map(|id|bounded(id,256))});
    }
    if result.to_string().len() > 32 * 1024 {
        result["value"] = json!({"type":event_type,"truncated":true});
        result["truncated"] = json!(true);
    }
    result
}

impl Capture {
    fn thread_page(
        &self,
        selector: &Selector,
        cursor: Option<u64>,
        generation: Option<&str>,
        limit: usize,
    ) -> Result<Value, String> {
        selector.validate()?;
        if !(1..=64).contains(&limit) {
            return Err("history page limit must be in 1..=64".into());
        }
        if generation.is_some_and(|generation| generation != self.generation) {
            return Err(
                "history cursor belongs to an earlier process generation; restart with no cursor"
                    .into(),
            );
        }
        if cursor.is_some_and(|cursor| cursor > 0) && generation.is_none() {
            return Err("nonzero history cursor requires its returned generation".into());
        }
        let ring = self.ring.lock().unwrap_or_else(|error| error.into_inner());
        let start = ring.events.front().map_or(ring.next, |event| event.0);
        let requested = cursor.unwrap_or(start);
        if requested > ring.next {
            return Err("history cursor exceeds this process event sequence".into());
        }
        let mut next = requested.max(start);
        let mut events = Vec::new();
        let mut bytes = 2048;
        let skip = usize::try_from(next.saturating_sub(start))
            .unwrap_or(ring.events.len())
            .min(ring.events.len());
        for (sequence, encoded, _, scope) in ring.events.iter().skip(skip) {
            if *sequence < next {
                continue;
            }
            if !selector.matches(scope.as_ref()) {
                next = sequence + 1;
                continue;
            }
            let frame: Value =
                serde_json::from_str(encoded.get()).map_err(|_| "cannot decode retained event")?;
            let event = project(frame, *sequence, scope.as_ref());
            let size = event.to_string().len();
            if events.len() >= limit || bytes + size > PAGE_BYTES {
                break;
            }
            bytes += size;
            next = sequence + 1;
            events.push(event);
        }
        Ok(
            json!({"schema_version":1,"scope":selector.value(),"generation":self.generation,
            "events":events,"next_cursor":next,"start_cursor":start,"has_more":next<ring.next,
            "truncated_since_cursor":cursor.is_some()&&requested<start,
            "truncation_scope":"process_journal_window",
            "retention_gap_possible":cursor.is_some()&&requested<start,
            "retention":"live_process_memory"}),
        )
    }
}

pub(crate) fn page(
    selector: &Selector,
    cursor: Option<u64>,
    generation: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    history_capture()
        .ok_or("process event history is unavailable")?
        .thread_page(selector, cursor, generation, limit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_event_journal_does_not_share_attach_input_leases_or_backend_slots() {
        let history = Capture::new().unwrap();
        let first = std::sync::Arc::new(Capture::with_journal(&history));
        let lease = first.open(false).unwrap();
        first.runtime.lock().unwrap().generation = 7;
        first.push(json!({"kind":"event","value":{"text":"retained process event"}}));
        let second = Capture::with_journal(&history);
        assert_eq!(second.generation, first.generation);
        assert_eq!(second.ring.lock().unwrap().next, 1);
        assert!(second.touch(&lease, true).is_err());
        assert_eq!(second.runtime.lock().unwrap().generation, 0);
        let retired = std::sync::Arc::downgrade(&first);
        drop(first);
        assert!(
            retired.upgrade().is_none(),
            "the retained journal must not keep attach authority alive"
        );
        assert_eq!(history.ring.lock().unwrap().next, 1);
    }

    fn selector(agent: Option<&str>, main: bool) -> Selector {
        Selector {
            client_id: "a".repeat(64),
            agent_id: agent.map(str::to_owned),
            main,
        }
    }
    fn scope(agent: Option<&str>) -> RecordScope {
        RecordScope {
            client_id: Some("a".repeat(64)),
            agent_id: agent.map(str::to_owned),
            kind: if agent.is_some() { "thread" } else { "main" },
            connection: 7,
            request_id: 9,
            run_id: Some("run".into()),
            tool: None,
            account: Some(json!({"provider":"github","subject":"owner"})),
        }
    }

    #[tokio::test]
    async fn record_scope_comes_from_real_native_source_and_history_observation_is_transparent() {
        use crate::runtime::call_scope::{CallScope, Source};
        use crate::wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution};
        let capture = Capture::new().unwrap();
        let client = "a".repeat(64);
        let agent = format!("agent_{}", "1".repeat(32));
        let attribution = ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "123".into(),
                login: "same-label".into(),
            },
            owner: None,
            agent: Some(ControlAgent {
                id: agent.clone(),
                name: "same label".into(),
            }),
        };
        let context = CodeContext {
            run_id: "b".repeat(32),
            code: "return null;".into(),
            timestamp_ms: 1,
            session_id: Some(client.clone()),
            thread_id: Some(agent.clone()),
        };
        let call = CallScope::from_relay_for_client(
            7,
            Some(&client),
            1,
            Some(&attribution),
            Some(&context),
        )
        .unwrap();
        call.set_native_tool("ssmcp.exec.read");
        let source = call.scope(async { Source::capture().display() }).await;
        capture.push_scoped_event(
            "stdout",
            &json!({"type":"command.output","text":"actual thread output",
            "source":{"agent_id":"forged"},"agent_id":"wrong"}),
            &source,
        );
        let page = capture
            .thread_page(&selector(Some(&agent), false), None, None, 64)
            .unwrap();
        assert_eq!(page["events"][0]["source"]["agent_id"], agent);
        assert_eq!(page["events"][0]["source"]["account"]["subject"], "123");
        assert_eq!(page["events"][0]["source"]["tool"], "ssmcp.exec.read");
        let count = capture.ring.lock().unwrap().next;
        assert!(observer("ssmcp.ui.history"));
        assert!(
            ToolCall::start(&Source::default(), "ssmcp.ui.history")
                .source
                .is_none()
        );
        assert_eq!(capture.ring.lock().unwrap().next, count);
        let raw = capture.ring.lock().unwrap();
        assert!(raw.bytes <= super::super::HISTORY_BYTES);
        assert!(
            raw.events[0].2 > raw.events[0].1.get().len(),
            "scope allocation is part of the memory budget"
        );
    }

    #[test]
    fn exact_thread_history_never_copies_shared_process_or_other_agent_records() {
        let capture = Capture::new().unwrap();
        let first = format!("agent_{}", "1".repeat(32));
        let second = format!("agent_{}", "2".repeat(32));
        capture.push(
            json!({"kind":"event","value":{"text":"legacy process record","agent_id":first}}),
        );
        capture.push_scoped(
            json!({"kind":"event","value":{"text":"first output","agent_id":second}}),
            Some(scope(Some(&first))),
        );
        capture.push_scoped(
            json!({"kind":"event","value":{"text":"second output"}}),
            Some(scope(Some(&second))),
        );
        capture.push_scoped(
            json!({"kind":"event","value":{"text":"main output"}}),
            Some(scope(None)),
        );
        for (id, text) in [(&first, "first output"), (&second, "second output")] {
            let page = capture
                .thread_page(&selector(Some(id), false), None, None, 64)
                .unwrap();
            assert_eq!(page["events"].as_array().unwrap().len(), 1);
            assert_eq!(page["events"][0]["text"], text);
            assert_eq!(page["events"][0]["source"]["agent_id"], id.as_str());
        }
        let main = capture
            .thread_page(&selector(None, true), None, None, 64)
            .unwrap();
        assert_eq!(main["events"].as_array().unwrap().len(), 1);
        assert_eq!(main["events"][0]["text"], "main output");
        let process = capture
            .thread_page(&selector(None, false), None, None, 64)
            .unwrap();
        assert_eq!(process["events"].as_array().unwrap().len(), 4);
        assert_eq!(process["events"][0]["unattributed"], true);
    }

    #[test]
    fn history_cursor_advances_over_unmatched_events_and_reports_pruning_and_restart() {
        let capture = Capture::new().unwrap();
        let agent = format!("agent_{}", "1".repeat(32));
        for index in 0..3 {
            capture.push_scoped(
                json!({"kind":"event","value":{"text":format!("chunk {index}")}}),
                Some(scope(Some(&agent))),
            );
        }
        let selected = selector(Some(&agent), false);
        let first = capture.thread_page(&selected, None, None, 1).unwrap();
        let cursor = first["next_cursor"].as_u64().unwrap();
        let next = capture
            .thread_page(&selected, Some(cursor), Some(&capture.generation), 1)
            .unwrap();
        assert_ne!(
            first["events"][0]["sequence"],
            next["events"][0]["sequence"]
        );
        assert!(
            capture
                .thread_page(&selected, Some(cursor), Some("old-process"), 1)
                .is_err()
        );
        assert!(
            capture
                .thread_page(&selected, Some(cursor), None, 1)
                .is_err()
        );
        for _ in 0..4096 {
            capture.push(json!({"kind":"event","value":{"text":"unassigned"}}));
        }
        let pruned = capture
            .thread_page(&selected, Some(cursor), Some(&capture.generation), 1)
            .unwrap();
        assert_eq!(pruned["truncated_since_cursor"], true);
        assert!(pruned["events"].as_array().unwrap().is_empty());
        assert_eq!(pruned["has_more"], false);
    }

    #[test]
    fn oversized_events_keep_their_real_scope_and_bounded_projection() {
        let capture = Capture::new().unwrap();
        let agent = format!("agent_{}", "1".repeat(32));
        capture.push_scoped(json!({"kind":"event","local_history_id":"kept","value":{"text":"x".repeat(super::super::EVENT_BYTES+1)}}),Some(scope(Some(&agent))));
        let page = capture
            .thread_page(&selector(Some(&agent), false), None, None, 64)
            .unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["events"][0]["truncated"], true);
        assert_eq!(page["events"][0]["source"]["agent_id"], agent);
        assert!(page.to_string().len() < PAGE_BYTES);
        assert!(selector(Some("display-name"), false).validate().is_err());
        assert!(
            selector(Some(&format!("agent_{}", "1".repeat(32))), true)
                .validate()
                .is_err()
        );
        let unusual = Capture::new().unwrap();
        unusual.push_scoped(
            json!({"kind":"event","value":{"type":"x".repeat(40000),
            "command":["true"],"job_id":{"arbitrary":"y".repeat(8000)}}}),
            Some(scope(Some(&format!("agent_{}", "1".repeat(32))))),
        );
        let bounded = unusual
            .thread_page(
                &selector(Some(&format!("agent_{}", "1".repeat(32))), false),
                None,
                None,
                64,
            )
            .unwrap();
        assert_eq!(bounded["events"].as_array().unwrap().len(), 1);
        assert_eq!(bounded["next_cursor"], 1);
        assert!(bounded.to_string().len() < PAGE_BYTES);
        let output = Capture::new().unwrap();
        output.push_scoped(json!({"kind":"event","value":{"type":"exec.stdout",
            "params":{"deltaBase64":STANDARD.encode("实际命令输出"),"stream":"stdout","offset":0}}}),Some(scope(Some(&format!("agent_{}","1".repeat(32))))));
        let page = output
            .thread_page(
                &selector(Some(&format!("agent_{}", "1".repeat(32))), false),
                None,
                None,
                64,
            )
            .unwrap();
        assert_eq!(page["events"][0]["text"], "实际命令输出");
        assert_eq!(page["events"][0]["text_lossy"], false);
    }

    #[test]
    fn other_thread_sequence_gaps_do_not_claim_selected_history_loss() {
        let capture = Capture::new().unwrap();
        let agent = format!("agent_{}", "1".repeat(32));
        capture.push_scoped(
            json!({"kind":"event","value":{"text":"first"}}),
            Some(scope(Some(&agent))),
        );
        let selected = selector(Some(&agent), false);
        let first = capture.thread_page(&selected, None, None, 1).unwrap();
        let cursor = first["next_cursor"].as_u64().unwrap();
        for _ in 0..20 {
            capture.push_scoped(
                json!({"kind":"event","value":{"text":"other scope"}}),
                Some(scope(None)),
            );
        }
        capture.push_scoped(
            json!({"kind":"event","value":{"text":"next"}}),
            Some(scope(Some(&agent))),
        );
        let next = capture
            .thread_page(&selected, Some(cursor), Some(&capture.generation), 64)
            .unwrap();
        assert_eq!(next["events"].as_array().unwrap().len(), 1);
        assert_eq!(next["retention_gap_possible"], false);
        assert_eq!(next["truncated_since_cursor"], false);
        for _ in 0..4096 {
            capture.push(json!({"kind":"event","value":{"text":"process"}}));
        }
        let gone = capture
            .thread_page(&selected, Some(cursor), Some(&capture.generation), 64)
            .unwrap();
        assert_eq!(gone["retention_gap_possible"], true);
        assert_eq!(gone["truncation_scope"], "process_journal_window");
        assert!(gone.get("matching_events_lost").is_none());
    }
}
