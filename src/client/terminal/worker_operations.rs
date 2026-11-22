//! Read-only coordination details from a real local or validated linked pool.
//! Elapsed time is supplied by the backend; rendering never starts a timer.
use super::{PanelTone, panel};
use serde_json::Value;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn current(preview: bool) -> Option<Value> {
    if preview {
        return None;
    }
    crate::worker::display()
        .and_then(|snapshot| serde_json::to_value(snapshot).ok())
        .or_else(crate::client::worker_window::linked_monitor_snapshot)
}

fn entries(pool: &Value) -> Vec<&Value> {
    if pool["connected"] == false {
        return Vec::new();
    }
    let mut entries: Vec<_> = pool["manager_operations"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    entries.sort_by_key(|operation| {
        (
            operation["state"] != "waiting",
            std::cmp::Reverse(operation["operation_id"].as_u64().unwrap_or(0)),
        )
    });
    entries
}

fn duration(milliseconds: u64) -> String {
    if milliseconds < 60_000 {
        format!(
            "{}.{:01}s",
            milliseconds / 1_000,
            milliseconds % 1_000 / 100
        )
    } else {
        format!(
            "{}m {:02}s",
            milliseconds / 60_000,
            milliseconds / 1_000 % 60
        )
    }
}

fn state(operation: &Value) -> &'static str {
    match operation["state"].as_str() {
        Some("waiting") => "Waiting",
        Some("satisfied") => "Satisfied",
        Some("timed_out") => "Timed out",
        Some("indeterminate") => "Unknown target",
        Some("unavailable") => "Unavailable",
        Some("cancelled") => "Cancelled",
        Some("error") => "Error",
        Some("submitted") => "Submitted",
        Some("assigned") => "Assigned",
        Some("current") => "Current capacity",
        Some("changed") => "Capacity changed",
        Some("unchanged") => "Unchanged",
        Some("not_accepting") => "Not accepting tasks",
        _ => "State unavailable",
    }
}

fn tone(operation: &Value) -> PanelTone {
    match operation["state"].as_str() {
        Some("waiting" | "timed_out" | "indeterminate" | "unavailable" | "not_accepting") => {
            PanelTone::Warning
        }
        Some("error") => PanelTone::Error,
        Some("satisfied" | "assigned" | "changed") => PanelTone::Success,
        _ => PanelTone::Muted,
    }
}

fn mode(operation: &Value) -> &'static str {
    match operation["mode"].as_str() {
        Some("any") => "ANY",
        Some("all") => "ALL",
        Some("single") => "SINGLE",
        Some("change") => "CHANGE",
        _ => "Condition unavailable",
    }
}

fn target(target: &Value) -> String {
    match target["kind"].as_str() {
        Some("task_finished") => format!(
            "Task {} finished",
            target["task_id"].as_str().unwrap_or("unavailable")
        ),
        Some("task_result") => format!(
            "Task {} result",
            target["task_id"].as_str().unwrap_or("unavailable")
        ),
        Some("worker_idle") => match (target["slot"].as_u64(), target["epoch"].as_u64()) {
            (Some(slot), Some(epoch)) => format!(
                "Worker #{:02} idle (slot {slot}, epoch {epoch})",
                slot.saturating_add(1)
            ),
            _ => "Worker idle target unavailable".into(),
        },
        Some("availability_change") => {
            if let Some(revision) = target["revision"].as_u64() {
                format!("Capacity revision changes after {revision}")
            } else {
                "Current capacity observation".into()
            }
        }
        _ => "Condition unavailable".into(),
    }
}

fn title(operation: &Value) -> String {
    let task = operation["task_id"].as_str().unwrap_or("unavailable");
    match operation["kind"].as_str() {
        Some("submit") => format!("Submitted · Task {task} · queued"),
        Some("claim") => {
            if let (Some(slot), Some(epoch)) =
                (operation["slot"].as_u64(), operation["epoch"].as_u64())
            {
                format!(
                    "Assigned · Task {task} · Worker #{:02} (slot {slot}, epoch {epoch})",
                    slot.saturating_add(1)
                )
            } else {
                format!("Assigned · Task {task} · station unavailable")
            }
        }
        kind => {
            let label = match kind {
                Some("wait_for") => "Wait",
                Some("result") => "Result wait",
                Some("wait_available") => "Capacity wait",
                _ => "Operation",
            };
            let elapsed = duration(operation["elapsed_ms"].as_u64().unwrap_or(0));
            let limit = operation["wait_ms"]
                .as_u64()
                .map(|limit| format!(" / {}", duration(limit)))
                .unwrap_or_default();
            format!(
                "{label} #{} · {} · {} · {elapsed}{limit}",
                operation["operation_id"].as_u64().unwrap_or(0),
                mode(operation),
                state(operation),
            )
        }
    }
}

fn details(operation: &Value) -> Vec<String> {
    let mut rows = Vec::new();
    if let Some(summary) = operation["summary"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        rows.push(summary.to_owned());
    }
    rows.extend(
        operation["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(target),
    );
    rows
}

fn clip(text: &str, width: u16) -> String {
    let text = panel::safe_text(text);
    let width = usize::from(width);
    if text.width() <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width - 1 {
            break;
        }
        clipped.push(character);
        used += cells;
    }
    clipped.push('…');
    clipped
}

pub(super) fn compact(pool: &Value, width: u16, limit: usize) -> Vec<(String, PanelTone)> {
    let operations = entries(pool);
    let waiting = operations
        .iter()
        .take_while(|operation| operation["state"] == "waiting")
        .count();
    let selected = if waiting == 0 {
        operations.len().min(1)
    } else {
        waiting
    };
    let mut rows = Vec::new();
    for operation in operations.into_iter().take(selected.min(limit)) {
        let prefix = if waiting > limit && rows.is_empty() {
            format!("{waiting} waits · ")
        } else {
            String::new()
        };
        rows.push((
            clip(&format!("{prefix}{}", title(operation)), width),
            tone(operation),
        ));
        if selected == 1 && limit > 1 {
            let detail = details(operation).join(" · ");
            if !detail.is_empty() {
                rows.push((clip(&detail, width), PanelTone::Normal));
            }
            if rows.len() < limit && limit > 2 {
                rows.push((
                    clip(
                        "Full conditions and retained operations: main Statistics",
                        width,
                    ),
                    PanelTone::Muted,
                ));
            }
        }
    }
    rows.truncate(limit);
    rows
}

/// Semantic lines; the caller wraps IDs to its own scrollable viewport.
pub(super) fn full(pool: &Value) -> Vec<(String, PanelTone)> {
    let operations = entries(pool);
    if operations.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![("Coordinator activity".into(), PanelTone::Heading)];
    if let Some(generation) = pool["generation"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        rows.push((format!("Pool generation {generation}"), PanelTone::Muted));
    }
    rows.push((
        "ANY: at least one condition · ALL: every condition in the same observation".into(),
        PanelTone::Muted,
    ));
    rows.push((
        "Times belong to one tool call; completed, failed or cancelled tasks are finished.".into(),
        PanelTone::Muted,
    ));
    for operation in operations {
        rows.push((String::new(), PanelTone::Normal));
        rows.push((panel::safe_text(&title(operation)), tone(operation)));
        rows.extend(
            details(operation)
                .into_iter()
                .map(|text| (panel::safe_text(&text), PanelTone::Normal)),
        );
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pool() -> Value {
        json!({"generation":"real-pool-generation","manager_operations":[{
            "operation_id":7,"kind":"wait_for","state":"waiting","mode":"all",
            "elapsed_ms":1250,"wait_ms":5000,"started_at_ms":1,
            "targets":[{"kind":"task_finished","task_id":"exact-task-id"},
                {"kind":"worker_idle","slot":1,"epoch":41}]
        }]})
    }

    #[test]
    fn wait_display_uses_backend_time_exact_targets_and_real_terminal_state() {
        let mut pool = pool();
        let rows = full(&pool)
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("Wait #7 · ALL · Waiting · 1.2s / 5.0s"));
        assert!(rows.contains("Task exact-task-id finished"));
        assert!(rows.contains("Worker #02 idle (slot 1, epoch 41)"));
        pool["manager_operations"][0]["state"] = json!("timed_out");
        pool["manager_operations"][0]["elapsed_ms"] = json!(5002);
        let rows = compact(&pool, 120, 2);
        assert!(rows[0].0.contains("Timed out · 5.0s / 5.0s"));
        assert!(!rows[0].0.contains("Waiting"));
        let operation = &mut pool["manager_operations"][0];
        operation["kind"] = json!("result");
        operation["mode"] = json!("single");
        operation["targets"] = json!([{"kind":"task_result","task_id":"legacy-result-task"}]);
        assert!(
            full(&pool)
                .iter()
                .any(|row| row.0 == "Task legacy-result-task result")
        );
        let operation = &mut pool["manager_operations"][0];
        operation["kind"] = json!("wait_available");
        operation["mode"] = json!("change");
        operation["targets"] = json!([{"kind":"availability_change","revision":17}]);
        for (value, label) in [
            ("current", "Current capacity"),
            ("changed", "Capacity changed"),
            ("unchanged", "Unchanged"),
            ("not_accepting", "Not accepting tasks"),
        ] {
            pool["manager_operations"][0]["state"] = json!(value);
            let rows = compact(&pool, 120, 2);
            assert!(rows[0].0.contains(label));
            assert_eq!(rows[1].0, "Capacity revision changes after 17");
        }
    }

    #[test]
    fn missing_or_disconnected_observations_never_keep_a_wait_running() {
        assert!(compact(&json!({}), 80, 2).is_empty());
        let mut pool = pool();
        pool["connected"] = json!(false);
        assert!(full(&pool).is_empty());
        assert!(compact(&pool, 80, 2).is_empty());
    }

    #[test]
    fn a_submission_is_not_an_assignment_and_narrow_details_are_explicitly_clipped() {
        let pool = json!({"manager_operations":[{"operation_id":2,"kind":"submit",
            "state":"submitted","task_id":"task-exact-123456789","summary":"Review the actual patch",
            "targets":[],"elapsed_ms":0}]});
        let rows = compact(&pool, 18, 2);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].0.starts_with("Submitted"));
        assert!(!rows.iter().any(|row| row.0.contains("Assigned")));
        assert!(rows.iter().all(|row| row.0.width() <= 18));
        assert!(rows[0].0.ends_with('…'));
        assert!(
            full(&pool)
                .iter()
                .any(|row| row.0.contains("task-exact-123456789"))
        );
    }
}
