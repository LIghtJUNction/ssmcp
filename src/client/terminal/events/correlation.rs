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

#[derive(Default)]
pub(super) struct Meta {
    pub(super) keys: Vec<String>,
    pub(super) status: Option<Status>,
    pub(super) command: bool,
    pub(super) diff: bool,
    pub(super) started: bool,
    pub(super) skip: bool,
    pub(super) language: &'static str,
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

    pub(super) fn merge(&mut self, other: Self) {
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
        self.command |= other.command;
        self.diff |= other.diff;
        if self.language.is_empty() {
            self.language = other.language;
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.keys.iter().map(String::len).sum()
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
