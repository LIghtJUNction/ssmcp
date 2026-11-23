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

fn is_assignment(operation: &Value) -> bool {
    matches!(operation["kind"].as_str(), Some("submit" | "claim"))
}

/// One cursor for the current, validated pool. Repaints do not replay retained
/// assignments; a replacement generation may reuse its own operation IDs.
#[derive(Default)]
pub(super) struct ConversationFeed {
    generation: Option<String>,
    after: Option<u64>,
    observed: std::collections::BTreeMap<u64, Value>,
}

pub(super) struct ConversationEntry {
    pub(super) key: String,
    pub(super) heading: String,
    pub(super) detail: String,
}

impl ConversationFeed {
    pub(super) fn update(&mut self, pool: &Value) -> Vec<ConversationEntry> {
        if pool["connected"] == false {
            return Vec::new();
        }
        let Some(generation) = pool["generation"].as_str().filter(|text| !text.is_empty()) else {
            return Vec::new();
        };
        if self.generation.as_deref() != Some(generation) {
            self.generation = Some(generation.to_owned());
            self.after = None;
            self.observed.clear();
        }
        let mut updates = entries(pool);
        updates.sort_by_key(|operation| operation["operation_id"].as_u64().unwrap_or(0));
        let retained: std::collections::BTreeSet<_> = updates
            .iter()
            .filter_map(|operation| operation["operation_id"].as_u64())
            .collect();
        self.observed.retain(|id, _| retained.contains(id));
        let mut output = Vec::new();
        for operation in updates {
            let Some(id) = operation["operation_id"].as_u64() else {
                continue;
            };
            let previous = self.observed.get(&id);
            if previous == Some(operation)
                || (previous.is_none() && self.after.is_some_and(|after| id <= after))
            {
                continue;
            }
            self.after = Some(self.after.map_or(id, |after| after.max(id)));
            self.observed.insert(id, operation.clone());
            let heading = if operation["kind"] == "submit" {
                "Task submitted · Queued".into()
            } else if operation["kind"] == "claim" {
                if let Some(slot) = operation["slot"].as_u64() {
                    format!("Task assigned · Worker #{:02}", slot.saturating_add(1))
                } else {
                    "Task assigned · Station unavailable".into()
                }
            } else {
                title(operation)
            };
            let mut detail = details(operation);
            if is_assignment(operation) {
                if detail.is_empty() {
                    detail.push("Task summary unavailable".into());
                }
                detail.push(format!(
                    "Task ID {}",
                    operation["task_id"].as_str().unwrap_or("unavailable")
                ));
                if let (Some(slot), Some(epoch)) =
                    (operation["slot"].as_u64(), operation["epoch"].as_u64())
                {
                    detail.push(format!(
                        "Worker #{:02} · slot {slot} · epoch {epoch}",
                        slot.saturating_add(1)
                    ));
                }
            } else if operation["mode"] == "any" {
                detail.push("ANY: at least one condition must be satisfied.".into());
            } else if operation["mode"] == "all" {
                detail
                    .push("ALL: every condition must be satisfied in the same observation.".into());
            }
            if !is_assignment(operation) {
                detail.extend(target_references(operation));
                detail.push(observation_note(operation).into());
            }
            detail.push(format!("Pool generation {generation} · operation {id}"));
            output.push(ConversationEntry {
                key: format!("worker-operation\0{generation}\0{id}"),
                heading,
                detail: detail
                    .into_iter()
                    .map(|text| panel::safe_text(&text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
        }
        while self.observed.len() > 128 {
            self.observed.pop_first();
        }
        output
    }
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
        Some("satisfied") => "Triggered",
        Some("timed_out") => "Window expired",
        Some("indeterminate") => "Unknown",
        Some("unavailable") => "Unavailable",
        Some("cancelled") => "Observation stopped",
        Some("error") => "Error",
        Some("submitted") => "Submitted",
        Some("assigned") => "Assigned",
        Some("current") => "Current capacity",
        Some("changed") => "Capacity changed",
        Some("unchanged") if operation["wait_ms"] == 0 => "Current capacity unchanged",
        Some("unchanged") => "Window expired · unchanged",
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
    let summary = target["summary"].as_str().filter(|text| !text.is_empty());
    match target["kind"].as_str() {
        Some("task_finished") => summary
            .map(|text| format!("{text} · finishes"))
            .unwrap_or_else(|| "Worker task finishes".into()),
        Some("task_result") => summary
            .map(|text| format!("{text} · result"))
            .unwrap_or_else(|| "Worker task result".into()),
        Some("worker_idle") => match target["slot"].as_u64() {
            Some(slot) => format!("Worker #{:02} becomes idle", slot.saturating_add(1)),
            None => "Worker idle target unavailable".into(),
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

fn target_references(operation: &Value) -> Vec<String> {
    operation["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|target| {
            if let Some(id) = target["task_id"].as_str() {
                Some(format!("Task ID {id}"))
            } else if let (Some(slot), Some(epoch)) =
                (target["slot"].as_u64(), target["epoch"].as_u64())
            {
                Some(format!(
                    "Worker #{:02} · slot {slot}, epoch {epoch}",
                    slot.saturating_add(1)
                ))
            } else {
                None
            }
        })
        .collect()
}

fn observation_note(operation: &Value) -> &'static str {
    match operation["state"].as_str() {
        Some("timed_out" | "unchanged") => {
            "Observation window expired or returned unchanged. This does not cancel execution or establish task completion."
        }
        Some("cancelled") => {
            "Only this observation stopped. Stopping observation does not cancel execution."
        }
        Some("satisfied" | "changed") => {
            "The requested condition triggered a return. This does not establish overall task completion."
        }
        Some("indeterminate" | "unavailable" | "error") => {
            "Target outcome is unknown or unavailable. Do not infer success or replay work."
        }
        Some("waiting") => {
            "Observation is pending. This window's budget is not the task's execution deadline."
        }
        _ => "This is a read-only observation of the requested condition.",
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
    let operations: Vec<_> = entries(pool)
        .into_iter()
        .filter(|operation| !is_assignment(operation))
        .collect();
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
                    clip("Full wait conditions: conversation", width),
                    PanelTone::Muted,
                ));
            }
        }
    }
    rows.truncate(limit);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(pool: &Value) -> Vec<(String, PanelTone)> {
        ConversationFeed::default()
            .update(pool)
            .into_iter()
            .flat_map(|entry| {
                std::iter::once(entry.heading)
                    .chain(entry.detail.lines().map(str::to_owned).collect::<Vec<_>>())
            })
            .map(|text| (text, PanelTone::Normal))
            .collect()
    }

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
        assert!(rows.contains("Worker task finishes"));
        assert!(rows.contains("Task ID exact-task-id"));
        assert!(rows.contains("Worker #02 becomes idle"));
        assert!(rows.contains("slot 1, epoch 41"));
        pool["manager_operations"][0]["state"] = json!("timed_out");
        pool["manager_operations"][0]["elapsed_ms"] = json!(5002);
        let rows = compact(&pool, 120, 2);
        assert!(rows[0].0.contains("Window expired · 5.0s / 5.0s"));
        assert!(!rows[0].0.contains("Waiting"));
        let operation = &mut pool["manager_operations"][0];
        operation["kind"] = json!("result");
        operation["mode"] = json!("single");
        operation["targets"] = json!([{"kind":"task_result","task_id":"legacy-result-task"}]);
        let rows = full(&pool)
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rows.contains("Worker task result"));
        assert!(rows.contains("Task ID legacy-result-task"));
        assert!(
            !compact(&pool, 120, 2)
                .iter()
                .any(|row| row.0.contains("legacy-result-task"))
        );
        let operation = &mut pool["manager_operations"][0];
        operation["kind"] = json!("wait_available");
        operation["mode"] = json!("change");
        operation["targets"] = json!([{"kind":"availability_change","revision":17}]);
        for (value, label) in [
            ("current", "Current capacity"),
            ("changed", "Capacity changed"),
            ("unchanged", "Window expired"),
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
    fn dispatch_feed_keeps_real_states_full_ids_and_generation_cursor() {
        let mut pool = json!({"generation":"pool-a","manager_operations":[{
            "operation_id":0,"kind":"submit","state":"submitted",
            "task_id":"task-exact-123456789","summary":"Review the actual patch",
            "targets":[],"elapsed_ms":0
        }]});
        assert!(compact(&pool, 18, 2).is_empty());
        let mut feed = ConversationFeed::default();
        let submitted = feed.update(&pool);
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].heading, "Task submitted · Queued");
        assert!(submitted[0].detail.contains("task-exact-123456789"));
        assert!(feed.update(&pool).is_empty());
        pool["manager_operations"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "operation_id":1,"kind":"claim","state":"assigned","slot":2,"epoch":7,
                "task_id":"task-exact-123456789","summary":"Review password=fixture-secret"
            }));
        let assigned = feed.update(&pool);
        assert_eq!(assigned.len(), 1);
        assert_eq!(assigned[0].heading, "Task assigned · Worker #03");
        assert!(assigned[0].detail.contains("slot 2 · epoch 7"));
        assert!(assigned[0].detail.contains("[redacted]"));
        assert!(!assigned[0].detail.contains("fixture-secret"));
        assert!(!assigned[0].heading.contains("done"));
        pool["connected"] = json!(false);
        pool["generation"] = json!("pool-b");
        assert!(feed.update(&pool).is_empty());
        pool["connected"] = json!(true);
        assert_eq!(feed.update(&pool).len(), 2);
        assert!(feed.update(&pool).is_empty());
    }
}
