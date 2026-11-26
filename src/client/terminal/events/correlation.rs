//! Bounded correlation metadata; display text stays in the owning EventView.
use serde_json::Value;

const MAX_KEY_BYTES: usize = 256;
const MAX_KEYS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Status {
    Running,
    Done,
    Exit(i64),
    Cancelled,
    Timeout,
    Signal(i64),
    Failed,
}

impl Status {
    pub(super) fn label(self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::Done => "done".into(),
            Self::Exit(code) => format!("exit={code}"),
            Self::Cancelled => "cancelled".into(),
            Self::Timeout => "timeout".into(),
            Self::Signal(signal) => format!("signal={signal}"),
            Self::Failed => "failed".into(),
        }
    }

    pub(super) fn failed(self) -> bool {
        matches!(self, Self::Exit(code) if code != 0)
            || matches!(
                self,
                Self::Cancelled | Self::Timeout | Self::Signal(_) | Self::Failed
            )
    }

    pub(super) fn tone(self) -> &'static str {
        match self {
            Self::Running => "accent",
            Self::Cancelled | Self::Timeout => "warning",
            status if status.failed() => "error",
            _ => "success",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Kind {
    #[default]
    Log,
    Conversation,
    Tool,
}

impl Kind {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::Conversation => "conversation",
            Self::Tool => "tool",
        }
    }

    fn from_event(event: &Value, rpc: &Value) -> Self {
        let kind = event["type"].as_str().unwrap_or("");
        // Lifecycle notices and backend stderr stay in the full log even when
        // their text or extra fields resemble an assistant reply or tool result.
        if matches!(
            kind,
            "ssmcp.status"
                | "ssmcp.controller.started"
                | "ssmcp.controller.finished"
                | "stderr"
                | "ssmcp.stderr"
                | "skill.discovery.completed"
                | "ssmcp.skill.discovery.completed"
                | "skill.discovered"
                | "ssmcp.skill.discovered"
        ) {
            return Self::Log;
        }
        let method = rpc["method"].as_str().unwrap_or("");
        let tool = matches!(
            method,
            "tools/call" | "command/exec" | "command/exec/outputDelta"
        ) || matches!(
            kind,
            "exec.started"
                | "ssmcp.exec.started"
                | "exec.stdout"
                | "ssmcp.exec.stdout"
                | "exec.stderr"
                | "ssmcp.exec.stderr"
                | "exec.exited"
                | "ssmcp.exec.exited"
                | "ssmcp.exec.finished"
                | "process_output"
                | "ssmcp.terminal_output"
                | "ssmcp.output_archived"
                | "file.read"
                | "ssmcp.file.read"
                | "ssmcp.fileread"
                | "file.write"
                | "ssmcp.file.write"
                | "file.diff"
                | "ssmcp.file.diff"
                | "ssmcp.filechange"
                | "skill.read"
                | "ssmcp.skill.read"
                | "skill.invoked"
                | "ssmcp.skill.invoked"
                | "ssmcp.skill_use"
                | "harness.tool.started"
                | "ssmcp.harness.tool.started"
                | "harness.tool.completed"
                | "ssmcp.harness.tool.completed"
                | "tool_use"
                | "tool_result"
                | "ssmcp.code_mode"
                | "image"
        );
        if tool {
            return Self::Tool;
        }
        // Only native message metadata routes prose to the conversation. Tool
        // bodies are not inspected recursively and cannot promote themselves.
        if matches!(kind, "ssmcp.message" | "ssmcp.user_input")
            || matches!(event["role"].as_str(), Some("assistant" | "user"))
            || matches!(kind, "assistant" | "user")
            || (matches!(kind, "message_start" | "message_update" | "message_end")
                && (matches!(
                    event["message"]["role"].as_str(),
                    Some("assistant" | "user")
                ) || event.get("assistantMessageEvent").is_some()))
            || matches!(method, "item/agentMessage/delta" | "item/userMessage/delta")
        {
            return Self::Conversation;
        }
        Self::Log
    }
}

#[derive(Default)]
pub(super) struct Meta {
    pub(super) scope: Option<std::sync::Arc<crate::runtime::call_scope::CallScope>>,
    pub(super) kind: Kind,
    pub(super) user: bool,
    /// Input-side presentation does not grant human recall or approval rights.
    pub(super) input: bool,
    pub(super) history: crate::runtime::call_scope::DisplayHistory,
    pub(super) history_message: bool,
    pub(super) message_id: Option<String>,
    pub(super) author: String,
    pub(super) raw_message: bool,
    pub(super) notice: Option<super::Notice>,
    pub(super) plain_message: bool,
    pub(super) keys: Vec<String>,
    pub(super) status: Option<Status>,
    pub(super) command: bool,
    pub(super) diff: bool,
    pub(super) started: bool,
    pub(super) skip: bool,
    pub(super) language: &'static str,
    pub(super) read: Option<super::read_activity::Read>,
    pub(super) read_bookkeeping: bool,
    pub(super) prompt_read: bool,
}

impl Meta {
    pub(super) fn from_event(label: &str, event: &Value) -> Self {
        let mut meta = Self::default();
        // The wrapper reports orchestration success, not the command outcome.
        if event["rpc_correlation"] == true && event["command_rpc_completed"] == true {
            meta.skip = true;
            return meta;
        }
        let kind = event["type"].as_str().unwrap_or("");
        let rpc = event
            .get("request")
            .or_else(|| event.get("response"))
            .unwrap_or(event);
        meta.kind = Kind::from_event(event, rpc);
        if meta.kind == Kind::Conversation {
            meta.plain_message = event["format"] == "plain";
            meta.user = event["role"] == "user"
                || matches!(kind, "user" | "ssmcp.user_input")
                || event["message"]["role"] == "user"
                || rpc["method"] == "item/userMessage/delta";
            meta.input = meta.user || (kind == "ssmcp.message" && event["kind"] == "input");
            meta.author = event["author"]
                .as_str()
                .unwrap_or("")
                .chars()
                .filter(|ch| !ch.is_control())
                .take(80)
                .collect();
        }
        let params = rpc["params"].get("arguments").unwrap_or(&rpc["params"]);
        let result = rpc["result"]
            .get("structuredContent")
            .unwrap_or(&rpc["result"]);
        let outcome = result.get("snapshot").unwrap_or(result);
        let job = event["job_id"]
            .as_str()
            .or_else(|| event["process_id"].as_str())
            .or_else(|| {
                event["id"]
                    .as_str()
                    .filter(|_| kind.contains("exec.") || kind == "process_output")
            })
            .or_else(|| outcome["job_id"].as_str())
            .or_else(|| result["job_id"].as_str());
        if let Some(job) = job {
            meta.push_key(label, "job", job);
        }
        if let Some(id) = rpc.get("id").filter(|id| id.is_string() || id.is_number())
            && !(kind.contains("exec.") || kind == "process_output")
        {
            meta.push_key(label, "rpc", &id.to_string());
        }
        for name in ["harness_call_id", "call_id"] {
            if let Some(id) = event
                .get(name)
                .filter(|id| id.is_string() || id.is_number())
            {
                meta.push_key(label, "call", &id.to_string());
            }
        }
        if let Some(process) = params["processId"]
            .as_str()
            .or_else(|| event.pointer("/params/processId").and_then(Value::as_str))
        {
            meta.push_key(label, "process", process);
        }
        meta.started = kind.ends_with(".started")
            || (rpc.get("id").is_some_and(|id| !id.is_null())
                && rpc["method"]
                    .as_str()
                    .is_some_and(|method| !method.starts_with("notifications/")));
        let completed = kind.ends_with(".exited")
            || kind.ends_with(".finished")
            || kind.ends_with(".completed")
            || rpc.get("result").is_some()
            || rpc.get("error").is_some();
        let sources = [event, outcome, result];
        let flag = |name: &str| sources.iter().any(|value| value[name] == true);
        let state = |name: &str| sources.iter().any(|value| value["state"] == name);
        let integer = |names: &[&str]| {
            sources
                .iter()
                .find_map(|value| names.iter().find_map(|name| value[*name].as_i64()))
        };
        let exit = integer(&["exit_code", "exitCode"]);
        let signal = integer(&["exit_signal", "exitSignal"]);
        let failed = has_error(&rpc["error"])
            || sources.iter().any(|value| {
                has_error(&value["error"]) || value["success"] == false || value["isError"] == true
            })
            || rpc["result"]["isError"] == true
            || state("failed");
        meta.status = if flag("cancelled") || state("cancelled") {
            Some(Status::Cancelled)
        } else if flag("timed_out") || flag("timedOut") || state("timed_out") {
            Some(Status::Timeout)
        } else if let Some(signal) = signal {
            Some(Status::Signal(signal))
        } else if let Some(code) = exit.filter(|code| *code != 0) {
            Some(Status::Exit(code))
        } else if failed {
            Some(Status::Failed)
        } else if let Some(code) = exit {
            Some(Status::Exit(code))
        } else if flag("running") || state("running") || meta.started {
            Some(Status::Running)
        } else if completed || state("completed") {
            Some(Status::Done)
        } else {
            None
        };
        meta.command = job.is_some()
            || kind.contains("exec.")
            || params.get("command").is_some()
            || params.get("cmd").is_some()
            || rpc["method"] == "command/exec";
        meta.diff = matches!(kind, "file.diff" | "ssmcp.file.diff")
            || (kind == "ssmcp.filechange" && event["diff"].is_string());
        meta.language = params["path"]
            .as_str()
            .or_else(|| params["file_path"].as_str())
            .or_else(|| event["path"].as_str())
            .map(language_for_path)
            .unwrap_or("");
        meta.read = super::read_activity::Read::from_event(label, event);
        meta.read_bookkeeping = super::read_activity::bookkeeping(event)
            && !meta.command
            && !meta.diff
            && meta
                .status
                .is_none_or(|status| !status.failed() && status != Status::Running);
        meta
    }

    fn push_key(&mut self, label: &str, kind: &str, id: &str) {
        if label.len() + kind.len() + id.len() + 2 > MAX_KEY_BYTES
            || id.is_empty()
            || label.chars().any(char::is_control)
            || id.chars().any(char::is_control)
        {
            return;
        }
        let key = format!("{label}\0{kind}\0{id}");
        if self.keys.len() < MAX_KEYS && !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }

    pub(super) fn set_source(&mut self, source: crate::runtime::call_scope::DisplaySource) {
        self.history = source.history;
        self.scope = source.call;
    }

    pub(super) fn same_source(&self, other: &Self) -> bool {
        if self.history != other.history {
            return false;
        }
        match (&self.scope, &other.scope) {
            (Some(left), Some(right)) => left.same_call(right),
            (None, None) => true,
            _ => false,
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        // A correlated mixed operation is not a standalone read observation.
        if self.read != other.read {
            self.read = None;
        }
        self.read_bookkeeping &= other.read_bookkeeping;
        for key in other.keys {
            if self.keys.len() < MAX_KEYS && !self.keys.contains(&key) {
                self.keys.push(key);
            }
        }
        if let Some(status) = other.status {
            // A polling wrapper and late output cannot overwrite a real exit.
            let authoritative = !matches!(status, Status::Running | Status::Done);
            if authoritative
                || self
                    .status
                    .is_none_or(|old| matches!(old, Status::Running | Status::Done))
            {
                self.status = Some(status);
            }
        }
        if self.kind == Kind::Log {
            self.kind = other.kind;
        }
        self.user |= other.user;
        if self.author.is_empty() {
            self.author = other.author;
        }
        self.command |= other.command;
        self.diff |= other.diff;
        if self.language.is_empty() {
            self.language = other.language;
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.author.len()
            + self
                .notice
                .as_ref()
                .map_or(0, |notice| notice.summary.len())
            + self
                .history
                .local_id()
                .map_or(0, |owner| owner.as_str().len())
            + self.message_id.as_ref().map_or(0, String::len)
            + self.scope.as_ref().map_or(0, |scope| {
                std::mem::size_of::<crate::runtime::call_scope::CallScope>()
                    + scope.run_id().map_or(0, str::len)
                    + scope
                        .attribution()
                        .map_or(0, crate::wire::ControlAttribution::bytes)
            })
            + self.keys.iter().map(String::len).sum::<usize>()
            + self
                .read
                .as_ref()
                .map_or(0, super::read_activity::Read::bytes)
    }
}

fn has_error(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

pub(super) fn language_for_path(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .trim_end_matches([':', ')', ']'])
    {
        "rs" => "rust",
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => "javascript",
        "json" | "jsonl" => "json",
        "py" => "python",
        "sh" | "bash" | "zsh" => "shell",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_metadata_separates_messages_and_tools_from_background_logs() {
        for event in [
            json!({"type":"ssmcp.message","text":"reply"}),
            json!({"role":"user","text":"request"}),
            json!({"role":"assistant","text":"reply"}),
            json!({"type":"message_update","message":{"role":"assistant"},"delta":"hello"}),
        ] {
            assert_eq!(Meta::from_event("native", &event).kind, Kind::Conversation);
        }
        for event in [
            json!({"type":"exec.stderr","job_id":"job","text":"failure detail"}),
            json!({"type":"ssmcp.file.diff","path":"lib.rs"}),
            json!({"type":"ssmcp.harness.tool.started","call_id":3}),
            json!({"request":{"id":4,"method":"tools/call","params":{"name":"fs.read"}}}),
            json!({"method":"command/exec/outputDelta","params":{"processId":"job"}}),
        ] {
            assert_eq!(Meta::from_event("native", &event).kind, Kind::Tool);
        }
        for event in [
            json!({"type":"ssmcp.status","level":"Connected","message":"ready"}),
            json!({"type":"ssmcp.controller.started","request_id":4}),
            json!({"type":"ssmcp.stderr","role":"assistant","text":"[AGENT] not a reply"}),
            json!({"type":"ssmcp.skill.discovered","path":"SKILL.md"}),
            json!({"message":{"role":"assistant","content":"untyped data"}}),
        ] {
            assert_eq!(Meta::from_event("native", &event).kind, Kind::Log);
        }
        let tool_payload = json!({"request":{"id":8,"method":"tools/call",
            "params":{"name":"fs.read","arguments":{"role":"assistant"}}}});
        assert_eq!(Meta::from_event("native", &tool_payload).kind, Kind::Tool);
    }

    #[test]
    fn native_outcomes_are_preserved_inside_mcp_results_and_read_snapshots() {
        for (value, expected) in [
            (json!({"cancelled":true}), Status::Cancelled),
            (json!({"exit_signal":15}), Status::Signal(15)),
            (
                json!({"state":"failed","error":"capture failed"}),
                Status::Failed,
            ),
            (json!({"running":true}), Status::Running),
            (json!({"exit_code":0}), Status::Exit(0)),
            (json!({"exit_code":7}), Status::Exit(7)),
            (json!({"timed_out":true}), Status::Timeout),
        ] {
            for result in [value.clone(), json!({"snapshot":value})] {
                let meta = Meta::from_event(
                    "shell",
                    &json!({"id":1,"result":{"structuredContent":result}}),
                );
                assert_eq!(meta.status, Some(expected));
            }
        }
        let failed = Meta::from_event(
            "shell",
            &json!({"id":2,"result":{"isError":true,"structuredContent":{}}}),
        );
        assert_eq!(failed.status, Some(Status::Failed));
    }

    #[test]
    fn native_keys_bridge_rpc_and_job_without_crossing_provider_boundaries() {
        let request = Meta::from_event(
            "a",
            &json!({"request":{"id":1,"method":"tools/call","params":{"name":"ssmcp.exec.start","arguments":{"command":["true"]}}}}),
        );
        let result = Meta::from_event(
            "a",
            &json!({"id":1,"result":{"structuredContent":{"job_id":"job","running":true}}}),
        );
        let job = Meta::from_event(
            "a",
            &json!({"type":"exec.exited","job_id":"job","exit_code":7}),
        );
        let unrelated = Meta::from_event(
            "b",
            &json!({"type":"exec.exited","job_id":"job","exit_code":0}),
        );
        let matches = |a: &Meta, b: &Meta| a.keys.iter().any(|key| b.keys.contains(key));
        assert!(matches(&request, &result));
        assert!(matches(&job, &result));
        assert!(!matches(&job, &unrelated));
        let mut final_status = job;
        final_status.merge(Meta::from_event("a", &json!({"id":1,"result":{}})));
        assert_eq!(final_status.status, Some(Status::Exit(7)));
    }
}
