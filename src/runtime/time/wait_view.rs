//! Projection of the native tool's real arguments and typed receipt only.
//! No command output, task result, private message or credential is retained.
use super::{MAX_DURATION_MS, MAX_WAIT_MS, ObservationSpec, WaitView};
use crate::wire::wait_display::{Mode, Outcome};
use serde_json::Value;

fn reference(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
        .map(str::to_owned)
}

pub(super) fn spec(name: &str, arguments: &Value) -> Option<ObservationSpec> {
    if name == "ssmcp.worker.wait_available"
        && arguments.get("after").is_none_or(|after| after.is_null())
    {
        return None;
    }
    let (default, cancellable, label) = match name {
        "ssmcp.exec.wait" => (Some(MAX_WAIT_MS), true, "Command exits"),
        "ssmcp.exec.read" => (Some(0), true, "Command output or exit"),
        "ssmcp.exec.events" => (Some(0), true, "Execution events"),
        "terminal.read" => (Some(0), true, "Terminal output"),
        "ssmcp.output.read" => (Some(0), true, "Output becomes readable"),
        "ssmcp.audit.events" => (Some(0), true, "Audit events"),
        "ssmcp.worker.result" => (Some(0), true, "Worker task result"),
        "ssmcp.worker.wait_for" => (Some(5000), true, "Worker conditions"),
        "ssmcp.worker.wait_available" => (
            Some(
                if arguments.get("after").is_some_and(|after| !after.is_null()) {
                    5000
                } else {
                    0
                },
            ),
            true,
            "Worker capacity changes",
        ),
        // A worker's assignment wait may acquire a claim. It keeps its own
        // cancellation protocol; this passive local UI never releases it.
        "ssmcp.worker.wait" => (Some(5000), false, "Worker assignment or retirement"),
        "ssmcp.worker.events" => (Some(0), true, "Worker events"),
        "ssmcp.ui.questions" if arguments["op"] == "wait" => (None, false, "Requested user input"),
        _ if name.starts_with("ssmcp.time.") || name.starts_with("ssmcp.ui.") => return None,
        _ if name.ends_with(".wait") => (None, false, "Native observation"),
        _ => return None,
    };
    let supplied = arguments
        .get("wait_ms")
        .or_else(|| arguments.get("timeout_ms"));
    let timeout_ms = match supplied {
        Some(Value::Null) if name == "ssmcp.exec.wait" => default,
        Some(value) => Some(value.as_u64().filter(|value| *value <= MAX_DURATION_MS)?),
        None => default,
    };
    if timeout_ms == Some(0) {
        return None;
    }
    // Known bounded tools reject excessive values themselves. Their UI must
    // not turn an invalid argument into a plausible running countdown.
    if default.is_some() && timeout_ms.is_some_and(|ms| ms > MAX_WAIT_MS) {
        return None;
    }
    let mode = match name {
        "ssmcp.worker.wait_for" => match arguments["mode"].as_str()? {
            "any" => Mode::Any,
            "all" => Mode::All,
            _ => return None,
        },
        "ssmcp.worker.wait_available" => Mode::Change,
        _ => Mode::Single,
    };
    let mut targets = vec![label.to_owned()];
    let mut references = Vec::new();
    for key in ["job_id", "task_id", "request_id"] {
        if let Some(id) = reference(&arguments[key]) {
            references.push(format!("{key}: {id}"));
        }
    }
    if name == "ssmcp.worker.wait_for"
        && let Some(conditions) = arguments["conditions"].as_array()
    {
        targets.clear();
        for condition in conditions.iter().take(8) {
            match condition["type"].as_str() {
                Some("task_finished") => {
                    targets.push("Worker task finishes".into());
                    if let Some(id) = reference(&condition["task_id"]) {
                        references.push(format!("task_id: {id}"));
                    }
                }
                Some("worker_idle") => {
                    if let (Some(slot), Some(epoch)) =
                        (condition["slot"].as_u64(), condition["epoch"].as_u64())
                    {
                        targets.push(format!(
                            "Worker #{:02} becomes idle",
                            slot.saturating_add(1)
                        ));
                        references.push(format!("slot {slot}, epoch {epoch}"));
                    }
                }
                _ => {}
            }
        }
        if conditions.len() > 8 {
            targets.push(format!(
                "+{} conditions; full list in the worker wait card",
                conditions.len() - 8
            ));
        }
        if targets.is_empty() {
            return None;
        }
    }
    Some(ObservationSpec {
        timeout_ms,
        cancellable,
        view: WaitView {
            operation: name.into(),
            mode,
            targets,
            references,
            outcome: Outcome::Pending,
            window_ms: timeout_ms,
        },
    })
}

pub(super) fn outcome(name: &str, cursors: (u64, u64), result: &Result<Value, String>) -> Outcome {
    let Ok(result) = result else {
        return Outcome::Unknown;
    };
    if result["isError"] == true {
        return Outcome::Unknown;
    }
    let value = result.get("structuredContent").unwrap_or(result);
    match name {
        "ssmcp.exec.wait" => {
            if value["running"] == true && value["wait_timed_out"] == true {
                Outcome::WindowExpired
            } else if value["running"] == false {
                Outcome::Triggered
            } else {
                Outcome::Unknown
            }
        }
        "ssmcp.exec.read" => {
            // Native read returns the end of each delivered page. The job snapshot
            // may already include later output which this read did not return.
            let page_cursor = |flat: &str, stream: &str| match value.get(flat) {
                Some(cursor) => cursor.as_u64(),
                None => value[stream]["next_cursor"].as_u64(),
            };
            let out = page_cursor("stdout_cursor", "stdout");
            let err = page_cursor("stderr_cursor", "stderr");
            if out.is_some_and(|cursor| cursor > cursors.0)
                || err.is_some_and(|cursor| cursor > cursors.1)
                || value["running"] == false
                || value["snapshot"]["running"] == false
            {
                Outcome::Triggered
            } else if out == Some(cursors.0)
                && err == Some(cursors.1)
                && (value["running"] == true || value["snapshot"]["running"] == true)
            {
                Outcome::WindowExpired
            } else {
                Outcome::Unknown
            }
        }
        "ssmcp.worker.result" => match value["task"]["status"].as_str() {
            Some("queued" | "working" | "cancelling") => Outcome::WindowExpired,
            Some("completed" | "failed" | "cancelled") => Outcome::Triggered,
            _ => Outcome::Unknown,
        },
        _ => match value["status"]
            .as_str()
            .or_else(|| value["wait_status"].as_str())
        {
            Some(
                "ready" | "satisfied" | "changed" | "current" | "assigned" | "retiring"
                | "released" | "already_completed",
            ) => Outcome::Triggered,
            Some("timed_out" | "pending" | "waiting" | "unchanged") => Outcome::WindowExpired,
            Some("cancelled") => Outcome::ObservationStopped,
            Some(_) => Outcome::Unknown,
            None => {
                if let Some(events) = value["events"].as_array() {
                    if events.is_empty() {
                        Outcome::WindowExpired
                    } else {
                        Outcome::Triggered
                    }
                } else {
                    Outcome::Unknown
                }
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actual_native_defaults_and_explicit_windows_are_preserved() {
        assert_eq!(
            spec("ssmcp.exec.wait", &json!({})).unwrap().timeout_ms,
            Some(55_000)
        );
        assert_eq!(
            spec("ssmcp.exec.wait", &json!({"wait_ms":null}))
                .unwrap()
                .timeout_ms,
            Some(55_000)
        );
        assert!(spec("ssmcp.exec.wait", &json!({"wait_ms":0})).is_none());
        assert!(spec("ssmcp.exec.wait", &json!({"wait_ms":55_001})).is_none());
        assert!(spec("ssmcp.worker.result", &json!({"task_id":"task"})).is_none());
        assert_eq!(
            spec(
                "ssmcp.worker.result",
                &json!({"task_id":"task","wait_ms":1234})
            )
            .unwrap()
            .timeout_ms,
            Some(1234)
        );
        assert!(spec("ssmcp.worker.wait_available", &json!({})).is_none());
        assert!(spec("ssmcp.worker.wait_available", &json!({"wait_ms":5000})).is_none());
        let capacity = spec(
            "ssmcp.worker.wait_available",
            &json!({"after":{"generation":"pool","revision":3}}),
        )
        .unwrap();
        assert_eq!(
            (capacity.timeout_ms, capacity.view.mode),
            (Some(5000), Mode::Change)
        );
        assert_eq!(
            spec("ssmcp.worker.wait", &json!({})).unwrap().timeout_ms,
            Some(5000)
        );
        assert!(!spec("ssmcp.worker.wait", &json!({})).unwrap().cancellable);
        for mode in ["any", "all"] {
            let value = spec(
                "ssmcp.worker.wait_for",
                &json!({"mode":mode,
                "conditions":[{"type":"worker_idle","slot":1,"epoch":4}]}),
            )
            .unwrap();
            assert_eq!(value.timeout_ms, Some(5000));
            assert_eq!(value.view.targets, ["Worker #02 becomes idle"]);
            assert_eq!(
                value.view.mode,
                if mode == "any" { Mode::Any } else { Mode::All }
            );
        }
    }

    #[test]
    fn a_returned_pending_receipt_is_a_window_expiry_not_task_completion() {
        assert_eq!(
            outcome(
                "ssmcp.exec.wait",
                (0, 0),
                &Ok(json!({
                    "structuredContent":{"running":true,"wait_timed_out":true,"state":"running"}
                }))
            ),
            Outcome::WindowExpired
        );
        assert_eq!(
            outcome(
                "ssmcp.exec.wait",
                (0, 0),
                &Ok(json!({
                    "structuredContent":{"running":false,"state":"completed","exit_code":9}
                }))
            ),
            Outcome::Triggered
        );
        assert_eq!(
            outcome(
                "ssmcp.worker.result",
                (0, 0),
                &Ok(json!({
                    "task":{"status":"unknown"}
                }))
            ),
            Outcome::Unknown
        );
        assert_eq!(
            outcome(
                "ssmcp.worker.wait_for",
                (0, 0),
                &Ok(json!({
                    "status":"satisfied","mode":"all"
                }))
            ),
            Outcome::Triggered
        );
        assert_eq!(
            outcome(
                "ssmcp.worker.wait_for",
                (0, 0),
                &Ok(json!({
                    "status":"timed_out"
                }))
            ),
            Outcome::WindowExpired
        );
        assert_eq!(
            outcome(
                "ssmcp.worker.wait_for",
                (0, 0),
                &Ok(json!({
                    "status":"cancelled"
                }))
            ),
            Outcome::ObservationStopped
        );
        assert_eq!(
            outcome("ssmcp.exec.wait", (0, 0), &Err("transport lost".into())),
            Outcome::Unknown
        );
    }

    #[test]
    fn read_outcome_uses_only_returned_page_cursors() {
        let classify = |page| outcome("ssmcp.exec.read", (4, 2), &Ok(page));
        let snapshot =
            json!({"running":true,"stdout":{"next_cursor":20},"stderr":{"next_cursor":8}});
        assert_eq!(
            classify(
                json!({"structuredContent":{"stdout_cursor":4,"stderr_cursor":2,"snapshot":snapshot}})
            ),
            Outcome::WindowExpired
        );
        assert_eq!(
            classify(
                json!({"running":true, "stdout":{"next_cursor":4}, "stderr":{"next_cursor":3}})
            ),
            Outcome::Triggered
        );
        for malformed in [
            json!({"running":true,"stdout_cursor":1,"stderr_cursor":2}),
            json!({"running":true,"stdout_cursor":4}),
            json!({"running":true,"stdout_cursor":null,"stdout":{"next_cursor":99},"stderr_cursor":2}),
            json!({"snapshot":snapshot}),
        ] {
            assert_eq!(classify(malformed), Outcome::Unknown);
        }
        assert_eq!(classify(json!({"running":false})), Outcome::Triggered);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_pipe_read_outcome_tracks_each_page_while_the_command_is_running() {
        use crate::backend::shell::Shell;
        let shell = Shell::new();
        let job = shell.call("ssmcp.exec.start", json!({
            "command":["/bin/sh","-c","read -r x && printf abcdef; read -r x && printf err >&2; read -r x; exit 0"],
            "stdin":"pipe","timeout_ms":30_000
        })).await.unwrap();
        let id = job["job_id"].as_str().unwrap();
        let read = |out, err, max_bytes, wait_ms| {
            shell.call("ssmcp.exec.read",
            json!({"job_id":id,"stdout_cursor":out,"stderr_cursor":err,"max_bytes":max_bytes,"wait_ms":wait_ms}))
        };
        let advance = || shell.call("ssmcp.exec.write", json!({"job_id":id,"data":"go\n"}));
        let classify = |out, err, page| outcome("ssmcp.exec.read", (out, err), &Ok(page));
        let untouched = read(0, 0, 2, 5).await.unwrap();
        assert_eq!(classify(0, 0, untouched), Outcome::WindowExpired);
        advance().await.unwrap();
        let page = read(0, 0, 2, 5000).await.unwrap();
        assert_eq!(page["running"], true);
        assert_eq!(page["stdout"], "ab");
        assert_eq!(page["stdout_cursor"], 2);
        assert_eq!(page["stderr_cursor"], 0);
        assert_eq!(classify(0, 0, page.clone()), Outcome::Triggered);
        let replay = read(0, 0, 2, 0).await.unwrap();
        assert_eq!(replay["stdout_base64"], page["stdout_base64"]);
        assert_eq!(classify(0, 0, replay), Outcome::Triggered);
        let tail = read(2, 0, 16, 5000).await.unwrap();
        assert_eq!(tail["stdout"], "cdef");
        assert_eq!(classify(2, 0, tail), Outcome::Triggered);
        let unchanged = read(6, 0, 16, 5).await.unwrap();
        assert_eq!(classify(6, 0, unchanged), Outcome::WindowExpired);
        advance().await.unwrap();
        let err = read(6, 0, 16, 5000).await.unwrap();
        assert_eq!(err["running"], true);
        assert_eq!(err["stdout_cursor"], 6);
        assert_eq!(err["stderr_cursor"], 3);
        assert_eq!(err["stderr"], "err");
        assert_eq!(classify(6, 0, err), Outcome::Triggered);
        shell
            .call("ssmcp.exec.write", json!({"job_id":id,"close_stdin":true}))
            .await
            .unwrap();
        let ended = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        assert_eq!(ended["exit_code"], 0);
        assert_eq!(
            classify(6, 3, read(6, 3, 16, 0).await.unwrap()),
            Outcome::Triggered
        );
    }

    #[test]
    fn read_receipts_use_the_actual_requested_cursors_and_never_output_text() {
        let value = Ok(json!({"running":true,"stdout":{"next_cursor":42},
            "stderr":{"next_cursor":9},"private_message":"must not appear"}));
        assert_eq!(
            outcome("ssmcp.exec.read", (42, 9), &value),
            Outcome::WindowExpired
        );
        assert_eq!(
            outcome("ssmcp.exec.read", (41, 9), &value),
            Outcome::Triggered
        );
        let view = spec(
            "ssmcp.worker.result",
            &json!({"task_id":"task_owned","wait_ms":1000,
            "token":"private-token","text":"private message body"}),
        )
        .unwrap()
        .view;
        let encoded = serde_json::to_string(&view).unwrap();
        assert!(!encoded.contains("private"));
        assert!(encoded.contains("task_owned"));
    }
}
