//! Bounded correlation metadata for the existing activity preview.
use serde_json::Value;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Status {
    #[default]
    Unknown,
    Running,
    Done,
    Failed,
    Cancelled,
    Timeout,
    Exit(i64),
    Signal(i64),
}
impl Status {
    pub(super) fn label(self) -> String {
        match self {
            Self::Unknown => String::new(),
            Self::Running => "running".into(),
            Self::Done => "done".into(),
            Self::Failed => "failed".into(),
            Self::Cancelled => "cancelled".into(),
            Self::Timeout => "timeout".into(),
            Self::Exit(code) => format!("exit={code}"),
            Self::Signal(signal) => format!("signal={signal}"),
        }
    }
}

pub(super) struct Activity {
    key: Option<String>,
    alias: Option<String>,
    call: Option<String>,
    pub(super) status: Status,
    pub(super) failed: bool,
    pub(super) started: bool,
    pub(super) diff: bool,
}
impl Activity {
    pub(super) fn matches(&self, next: &Self) -> bool {
        next.key.as_ref().is_some_and(|key| {
            self.key.as_ref() == Some(key)
                || self.alias.as_ref() == Some(key)
                || self.call.as_ref() == Some(key)
        })
    }
    pub(super) fn update(&mut self, next: Self) {
        let conclusive = !matches!(
            self.status,
            Status::Unknown | Status::Running | Status::Done
        );
        if next.status != Status::Unknown
            && !(conclusive && matches!(next.status, Status::Running | Status::Done))
        {
            self.status = next.status;
        }
        self.failed |= next.failed;
        self.diff |= next.diff;
        if self.alias.is_none() {
            self.alias = next.alias;
        }
        if self.call.is_none() {
            self.call = next.call;
        }
    }
}

fn key(label: &str, namespace: &str, id: &Value) -> Option<String> {
    if label.len() > 128 {
        return None;
    }
    let id = match id {
        Value::String(text) if text.len() <= 256 => text.clone(),
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    Some(format!("{label}\0{namespace}\0{id}"))
}

pub(super) fn duplicate_correlation(event: &Value) -> bool {
    event["rpc_correlation"] == true && event["command_rpc_completed"] == true
}

pub(super) fn metadata(label: &str, event: &Value) -> Activity {
    let kind = event["type"].as_str().unwrap_or("");
    let rpc = event
        .get("request")
        .or_else(|| event.get("response"))
        .unwrap_or(event);
    let params = rpc["params"].get("arguments").unwrap_or(&rpc["params"]);
    let job = event
        .get("job_id")
        .or_else(|| event.get("process_id"))
        .or_else(|| (kind.contains("exec.") || kind == "process_output").then(|| &event["id"]));
    let process = event.pointer("/params/processId");
    let identity = job
        .and_then(|id| key(label, "job", id))
        .or_else(|| rpc.get("id").and_then(|id| key(label, "rpc", id)))
        .or_else(|| event.get("call_id").and_then(|id| key(label, "call", id)))
        .or_else(|| process.and_then(|id| key(label, "process", id)));
    let alias = params
        .get("processId")
        .and_then(|id| key(label, "process", id));
    let call = event
        .get("harness_call_id")
        .and_then(|id| key(label, "call", id));
    let result = rpc["result"]
        .get("structuredContent")
        .unwrap_or(&rpc["result"]);
    let exit = event["exit_code"]
        .as_i64()
        .or_else(|| result["exit_code"].as_i64())
        .or_else(|| result["exitCode"].as_i64());
    let signal = event["exit_signal"]
        .as_i64()
        .or_else(|| result["exit_signal"].as_i64());
    let timeout =
        event["timed_out"] == true || result["timed_out"] == true || result["timedOut"] == true;
    let cancelled = event["cancelled"] == true || result["cancelled"] == true;
    let failed = !rpc["error"].is_null()
        || timeout
        || cancelled
        || result["isError"] == true
        || rpc["result"]["isError"] == true
        || result["success"] == false
        || event["success"] == false
        || exit.is_some_and(|code| code != 0)
        || signal.is_some();
    let completed = kind.ends_with(".exited")
        || kind.ends_with(".finished")
        || kind.ends_with(".completed")
        || rpc.get("result").is_some()
        || rpc.get("error").is_some();
    let started = kind.ends_with(".started")
        || (rpc.get("id").is_some_and(|id| !id.is_null())
            && rpc["method"]
                .as_str()
                .is_some_and(|method| !method.starts_with("notifications/")));
    let status = if cancelled {
        Status::Cancelled
    } else if timeout {
        Status::Timeout
    } else if let Some(signal) = signal {
        Status::Signal(signal)
    } else if let Some(exit) = exit {
        Status::Exit(exit)
    } else if failed {
        Status::Failed
    } else if result["running"] == true {
        Status::Running
    } else if completed {
        Status::Done
    } else if started {
        Status::Running
    } else {
        Status::Unknown
    };
    Activity {
        key: identity,
        alias,
        call,
        status,
        failed,
        started,
        diff: matches!(kind, "file.diff" | "ssmcp.file.diff")
            || (kind == "ssmcp.filechange" && event["diff"].as_str().is_some()),
    }
}
