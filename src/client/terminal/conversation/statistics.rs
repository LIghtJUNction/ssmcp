//! Read-only statistics from the connected session and this window's retained audit.
use super::*;
use unicode_width::UnicodeWidthChar;

#[derive(Default, Debug, PartialEq, Eq)]
struct ToolCounts {
    running: usize,
    succeeded: usize,
    failed: usize,
    unknown: usize,
}

impl ToolCounts {
    fn from_audit(audit: &Value) -> Self {
        let mut counts = Self::default();
        for entry in audit["entries"].as_array().into_iter().flatten() {
            for call in entry["calls"].as_array().into_iter().flatten() {
                match (call["finished"].as_bool(), call["success"].as_bool()) {
                    (Some(false), _) => counts.running += 1,
                    (Some(true), Some(true)) => counts.succeeded += 1,
                    (Some(true), Some(false)) => counts.failed += 1,
                    _ => counts.unknown += 1,
                }
            }
        }
        counts
    }
}

// IDs are wrapped, never abbreviated into what could look like a different ID.
fn wrapped(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let text = panel::safe_text(text);
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(character);
        used += cells;
    }
    rows.push(row);
    rows
}

fn linked_manager_thread(linked: &Value) -> Option<&str> {
    let client_id = linked["client_id"].as_str().filter(|id| !id.is_empty())?;
    let manager = linked.get("manager")?.as_object()?;
    (manager.get("client_id")?.as_str()? == client_id)
        .then(|| manager.get("thread_id")?.as_str())
        .flatten()
        .filter(|id| !id.is_empty())
}

// Summaries are display labels only. Exact identities remain in diagnostics.
fn compact(text: &str, width: u16) -> String {
    let safe = panel::safe_text(text);
    let width = usize::from(width);
    if safe.width() <= width {
        return safe;
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for character in safe.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width - 1 {
            break;
        }
        result.push(character);
        used += cells;
    }
    result.push('…');
    result
}

fn summary_rows(
    data: &Value,
    audit: &Value,
    workers: Option<&crate::worker::Snapshot>,
    linked: Option<&Value>,
    context: &str,
    width: u16,
) -> Vec<(String, PanelTone)> {
    let mut output = Vec::new();
    let mut push = |text: &str, tone| output.push((compact(text, width), tone));
    let connected = data["connected"] == true;
    let bound = connected && data["session_id"].as_str().is_some_and(|id| !id.is_empty());
    let members: Vec<_> = data["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|_| bound)
        .filter(|agent| agent["agent_id"].as_str().is_some_and(|id| !id.is_empty()))
        .collect();
    let pending = items(data, "queue")
        .iter()
        .filter(|entry| Receipt::from_entry(entry).pending())
        .count();
    let calls = ToolCounts::from_audit(audit);
    push("Statistics", PanelTone::Heading);
    push(&format!("Session{context}"), PanelTone::Normal);
    push(
        if data["preview"] == true {
            "Preview · sample data"
        } else if bound {
            "Connected"
        } else if connected {
            "Connected · identity unavailable"
        } else {
            "Disconnected"
        },
        PanelTone::Muted,
    );
    push(
        &format!("Members {} · Pending input {pending}", members.len()),
        PanelTone::Normal,
    );
    push("", PanelTone::Normal);
    push("Tool calls · retained in this window", PanelTone::Muted);
    push(
        &format!(
            "Running {} · Succeeded {} · Failed {}",
            calls.running, calls.succeeded, calls.failed
        ),
        PanelTone::Normal,
    );
    if calls.unknown > 0 {
        push(
            &format!("Outcome unknown {}", calls.unknown),
            PanelTone::Muted,
        );
    }
    push("", PanelTone::Normal);
    if let Some(workers) = workers {
        push("Worker pool", PanelTone::Heading);
        push(
            &format!(
                "Workers {} · Working {} · Idle {}",
                workers.total, workers.working, workers.idle
            ),
            PanelTone::Normal,
        );
        push(
            &format!(
                "Queued {} · Offline {} · Retiring {}",
                workers.queued, workers.offline, workers.retiring
            ),
            PanelTone::Muted,
        );
        if workers.closed {
            push("Closed", PanelTone::Muted);
        } else if workers.stopping {
            push("Task intake stopped", PanelTone::Muted);
        }
        for slot in workers.slots.iter().take(6) {
            let task = if slot.task_id.is_some() {
                slot.task_summary
                    .as_deref()
                    .unwrap_or("Task summary unavailable")
            } else {
                "No active task"
            };
            push(
                &format!("#{:02} · {} · {task}", slot.slot + 1, slot.state),
                PanelTone::Normal,
            );
        }
        if workers.slots.len() > 6 {
            push(
                &format!(
                    "{} more stations · Enter diagnostics",
                    workers.slots.len() - 6
                ),
                PanelTone::Muted,
            );
        }
    } else if let Some(linked) = linked.filter(|pool| pool["connected"] == true) {
        push("Linked worker pool", PanelTone::Heading);
        let count = |key: &str| {
            linked[key]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        push(
            &format!(
                "Workers {} · Working {} · Idle {}",
                count("total"),
                count("working"),
                count("idle")
            ),
            PanelTone::Normal,
        );
        push(
            &format!(
                "Queued {} · Offline {} · Retiring {}",
                count("queued"),
                count("offline"),
                count("retiring")
            ),
            PanelTone::Muted,
        );
        if linked["closed"] == true {
            push("Closed", PanelTone::Muted);
        } else if linked["stopping"] == true {
            push("Task intake stopped", PanelTone::Muted);
        }
        let slots = linked["slots"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        for slot in slots.iter().take(6) {
            let number = slot["slot"]
                .as_u64()
                .and_then(|n| n.checked_add(1))
                .map(|n| format!("#{n:02}"))
                .unwrap_or_else(|| "Worker".into());
            let state = slot["state"].as_str().unwrap_or("unavailable");
            let task = if slot["task_id"].as_str().is_some() {
                slot["task_summary"]
                    .as_str()
                    .unwrap_or("Task summary unavailable")
            } else {
                "No active task"
            };
            push(&format!("{number} · {state} · {task}"), PanelTone::Normal);
        }
        if slots.len() > 6 {
            push(
                &format!("{} more stations · Enter diagnostics", slots.len() - 6),
                PanelTone::Muted,
            );
        }
    } else {
        push("No worker pool connected", PanelTone::Muted);
    }
    if !members.is_empty() {
        push("", PanelTone::Normal);
        push("Members", PanelTone::Heading);
        for (index, member) in members.iter().take(8).enumerate() {
            let id = member["agent_id"].as_str().unwrap_or("");
            let name = member["name"]
                .as_str()
                .filter(|name| !name.is_empty() && *name != id)
                .unwrap_or("Unnamed");
            let role = member["role"]
                .as_str()
                .filter(|role| !role.is_empty())
                .unwrap_or("role unavailable");
            let status = member["status"]
                .as_str()
                .filter(|status| !status.is_empty())
                .unwrap_or("status unavailable");
            push(
                &format!("{}. {name} · {role} · {status}", index + 1),
                PanelTone::Normal,
            );
        }
        if members.len() > 8 {
            push(
                &format!("{} more members · Enter diagnostics", members.len() - 8),
                PanelTone::Muted,
            );
        }
    }
    push("", PanelTone::Normal);
    push(
        "Enter diagnostics · full IDs and pool details",
        PanelTone::Muted,
    );
    output
}

fn rows(
    data: &Value,
    audit: &Value,
    workers: Option<&crate::worker::Snapshot>,
    linked: Option<&Value>,
    process_id: u32,
    width: u16,
) -> Vec<(String, PanelTone)> {
    let mut output = Vec::new();
    let mut push = |text: &str, tone| {
        output.extend(wrapped(text, width).into_iter().map(|row| (row, tone)));
    };
    let client_id = (data["connected"] == true)
        .then(|| data["session_id"].as_str())
        .flatten()
        .filter(|id| !id.is_empty());
    let linked = linked.filter(|linked| linked["connected"] == true);
    // A recipient highlight and a recent caller are not a management binding.
    let thread_id = workers
        .and_then(|workers| workers.manager.as_ref())
        .filter(|manager| Some(manager.client_id.as_str()) == client_id)
        .map(|manager| manager.thread_id.as_str());
    push("Diagnostics", PanelTone::Heading);
    push("Enter summary · Esc back", PanelTone::Muted);
    if data["preview"] == true {
        push("Preview · sample data", PanelTone::Muted);
    }
    push("", PanelTone::Normal);
    push("Session process ID", PanelTone::Muted);
    push(client_id.unwrap_or("unbound"), PanelTone::Normal);
    push("Manager thread ID", PanelTone::Muted);
    push(thread_id.unwrap_or("unbound"), PanelTone::Normal);
    push(&format!("OS PID {process_id}"), PanelTone::Muted);
    push("", PanelTone::Normal);
    push("Registered session threads", PanelTone::Muted);
    let threads: Vec<_> = data["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|_| client_id.is_some())
        .filter_map(|agent| agent["agent_id"].as_str())
        .filter(|id| !id.is_empty())
        .collect();
    if threads.is_empty() {
        push("none", PanelTone::Normal);
    } else {
        for thread in &threads {
            push(thread, PanelTone::Normal);
        }
    }
    push("", PanelTone::Normal);
    push(
        &format!("Registered members {}", threads.len()),
        PanelTone::Normal,
    );
    let pending = items(data, "queue")
        .iter()
        .filter(|entry| Receipt::from_entry(entry).pending())
        .count();
    push(&format!("Pending input {pending}"), PanelTone::Normal);
    push("", PanelTone::Normal);
    push("Retained tool calls · this window", PanelTone::Muted);
    let calls = ToolCounts::from_audit(audit);
    push(
        &format!(
            "Running {} · Succeeded {} · Failed {}",
            calls.running, calls.succeeded, calls.failed,
        ),
        PanelTone::Normal,
    );
    if calls.unknown > 0 {
        push(
            &format!("Outcome unknown {}", calls.unknown),
            PanelTone::Muted,
        );
    }
    push("", PanelTone::Normal);
    // Retained audit outcomes and retry suppression do not prove that a native
    // process spawn was avoided. No authoritative host metric exists yet.
    push("Avoided command executions", PanelTone::Muted);
    push(
        "unavailable · host metric not implemented",
        PanelTone::Muted,
    );
    push("", PanelTone::Normal);
    if let Some(workers) = workers {
        push("Pool generation", PanelTone::Muted);
        push(&workers.generation, PanelTone::Normal);
        push(
            &format!(
                "Workers {} · Idle {} · Working {} · Offline {}",
                workers.total, workers.idle, workers.working, workers.offline,
            ),
            PanelTone::Normal,
        );
        push(
            &format!("Queued tasks {}", workers.queued),
            PanelTone::Normal,
        );
        push(
            &format!(
                "Target {} · Registered {} · Retiring {}",
                workers.desired_total, workers.registered_workers, workers.retiring,
            ),
            PanelTone::Normal,
        );
        if workers.closed {
            push("Worker pool closed", PanelTone::Muted);
        } else if workers.stopping {
            push("Task intake stopped", PanelTone::Muted);
        }
    } else if let Some(linked) = linked {
        // This is the child window's validated channel projection, not the
        // current session or a manager identity inferred from a recipient.
        push("Linked worker pool", PanelTone::Heading);
        push("Worker session ID", PanelTone::Muted);
        push(
            linked["client_id"].as_str().unwrap_or("unavailable"),
            PanelTone::Normal,
        );
        push("Pool generation", PanelTone::Muted);
        push(
            linked["generation"].as_str().unwrap_or("unavailable"),
            PanelTone::Normal,
        );
        let count = |key: &str| {
            linked[key]
                .as_u64()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unavailable".into())
        };
        push(
            &format!("Worker OS PID {}", count("process_id")),
            PanelTone::Muted,
        );
        push("Worker manager thread ID", PanelTone::Muted);
        push(
            linked_manager_thread(linked).unwrap_or_else(|| {
                if linked.get("manager") == Some(&Value::Null) {
                    "unbound"
                } else {
                    "unavailable"
                }
            }),
            PanelTone::Normal,
        );
        push(
            &format!(
                "Workers {} · Idle {} · Working {} · Offline {}",
                count("total"),
                count("idle"),
                count("working"),
                count("offline"),
            ),
            PanelTone::Normal,
        );
        push(
            &format!(
                "Target {} · Registered {} · Retiring {}",
                count("desired_total"),
                count("registered_workers"),
                count("retiring"),
            ),
            PanelTone::Normal,
        );
        push(
            &format!("Queued tasks {}", count("queued")),
            PanelTone::Normal,
        );
        if linked["closed"] == true {
            push("Worker pool closed", PanelTone::Muted);
        } else if linked["stopping"] == true {
            push("Task intake stopped", PanelTone::Muted);
        }
    } else {
        push("No worker pool connected", PanelTone::Muted);
    }
    if let Some(workers) = workers {
        for slot in &workers.slots {
            push("", PanelTone::Normal);
            push(
                &format!(
                    "#{:02} · {} · epoch {}",
                    slot.slot + 1,
                    slot.state,
                    slot.epoch
                ),
                PanelTone::Heading,
            );
            push("Task ID", PanelTone::Muted);
            push(slot.task_id.as_deref().unwrap_or("none"), PanelTone::Normal);
            if slot.task_id.is_some() {
                push(
                    slot.task_summary
                        .as_deref()
                        .unwrap_or("Task summary unavailable"),
                    PanelTone::Normal,
                );
            }
        }
    } else if let Some(linked) = linked {
        for slot in linked["slots"].as_array().into_iter().flatten() {
            let number = slot["slot"]
                .as_u64()
                .and_then(|n| n.checked_add(1))
                .map(|n| format!("#{n:02}"))
                .unwrap_or_else(|| "Worker".into());
            push("", PanelTone::Normal);
            push(
                &format!(
                    "{number} · {}",
                    slot["state"].as_str().unwrap_or("unavailable")
                ),
                PanelTone::Heading,
            );
            push("Task ID", PanelTone::Muted);
            push(
                slot["task_id"].as_str().unwrap_or("none"),
                PanelTone::Normal,
            );
            if slot["task_id"].as_str().is_some() {
                push(
                    slot["task_summary"]
                        .as_str()
                        .unwrap_or("Task summary unavailable"),
                    PanelTone::Normal,
                );
            }
        }
    }
    output
}

fn command_rows(
    stats: Option<crate::backend::shell::LocalCommandStats>,
    width: u16,
) -> Vec<(String, PanelTone)> {
    let mut rows = Vec::new();
    let mut push = |text: &str, tone| {
        rows.extend(wrapped(text, width).into_iter().map(|row| (row, tone)));
    };
    push("Command execution", PanelTone::Heading);
    push(
        "Scope · native local execution, including terminal !",
        PanelTone::Muted,
    );
    if let Some(stats) = stats {
        push(
            &format!(
                "Start calls {} · Spawned {} · Start errors {}",
                stats.calls, stats.spawned, stats.start_errors
            ),
            PanelTone::Normal,
        );
        let mut completed: Vec<_> = stats
            .jobs
            .iter()
            .filter(|job| job.state != "running")
            .collect();
        completed.sort_unstable_by_key(|job| std::cmp::Reverse(job.duration_ms));
        push(
            "Longest among the 20 most recent retained jobs",
            PanelTone::Muted,
        );
        for job in completed.into_iter().take(5) {
            push(
                &format!("{} ms · {}", job.duration_ms, job.id),
                PanelTone::Normal,
            );
        }
        push(
            "CPU time rank · Linux wait4 user + system",
            PanelTone::Muted,
        );
        let mut measured: Vec<_> = stats
            .jobs
            .iter()
            .filter_map(|job| job.usage.map(|usage| (job, usage)))
            .collect();
        measured.sort_unstable_by_key(|(_, usage)| std::cmp::Reverse(usage.cpu_us));
        if measured.is_empty() {
            push("CPU time unavailable", PanelTone::Muted);
        }
        for (job, usage) in measured.iter().take(5) {
            push(
                &format!("{} µs · {}", usage.cpu_us, job.id),
                PanelTone::Normal,
            );
        }
        push("Peak RSS rank · Linux wait4", PanelTone::Muted);
        measured.sort_unstable_by_key(|(_, usage)| std::cmp::Reverse(usage.peak_rss_bytes));
        if measured.is_empty() {
            push("Peak RSS unavailable", PanelTone::Muted);
        }
        for (job, usage) in measured.iter().take(5) {
            push(
                &format!("{} KiB · {}", usage.peak_rss_bytes / 1024, job.id),
                PanelTone::Normal,
            );
        }
        push("Recent tasks · local native execution", PanelTone::Muted);
        for job in stats.jobs {
            let disk = match job.disk_delta {
                Some((Some(bytes), false)) => format!("disk +{bytes} logical bytes"),
                Some((Some(bytes), true)) => format!("disk +{bytes} logical bytes (incomplete)"),
                Some((None, _)) => "disk unavailable (incomplete)".into(),
                None if job.disk_tracking => "disk pending completion sample".into(),
                None => "disk tracking off".into(),
            };
            let usage = job.usage.map_or_else(
                || "CPU/RSS unavailable".into(),
                |usage| {
                    format!(
                        "CPU {} µs · peak RSS {} KiB",
                        usage.cpu_us,
                        usage.peak_rss_bytes / 1024
                    )
                },
            );
            push(
                &format!(
                    "{} · {} ms · {} · {disk}",
                    job.state, job.duration_ms, job.id
                ),
                PanelTone::Normal,
            );
            push(&usage, PanelTone::Muted);
        }
    } else {
        push("Local execution data unavailable", PanelTone::Muted);
    }
    for line in [
        "Resource scope · direct command’s wait4 record; Linux may include children it waited for. No live CPU % or tree-wide RSS peak.",
        "Disk net change · Linux /track-command · job-exclusive directory only · logical bytes above empty baseline",
        "Cache hits, refreshes, invalidation and avoided execution · not instrumented",
        "Browser, relay and worker calls · outside this panel’s scope",
        "Queue · native start rejects when capacity is full",
    ] {
        push(line, PanelTone::Muted);
    }
    push("", PanelTone::Normal);
    rows
}

impl State {
    pub(super) fn chat_statistics_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let workers = (!self.preview).then(crate::worker::display).flatten();
        let linked = (!self.preview && workers.is_none())
            .then(crate::client::worker_window::linked_monitor_snapshot)
            .flatten();
        #[cfg(test)]
        let data = self
            .conversation
            .statistics_fixture
            .as_ref()
            .unwrap_or(data);
        let linked = linked.as_ref();
        #[cfg(test)]
        let linked = self
            .conversation
            .worker_activity_fixture
            .as_ref()
            .or(linked);
        let audit = self.audit.snapshot();
        let mut rows = if self.conversation.detail {
            rows(
                data,
                &audit,
                workers.as_ref(),
                linked,
                std::process::id(),
                self.geometry.left,
            )
        } else {
            summary_rows(
                data,
                &audit,
                workers.as_ref(),
                linked,
                &self.context_label,
                self.geometry.left,
            )
        };
        if !self.conversation.detail {
            let commands = (!self.preview)
                .then(crate::client::interface::harness::local_runtime)
                .and_then(Result::ok)
                .map(|(runtime, _)| runtime.local_command_stats());
            rows.extend(command_rows(commands, self.geometry.left));
        }
        let maximum = rows.len().saturating_sub(usize::from(height));
        self.conversation.detail_scroll = self.conversation.detail_scroll.min(maximum);
        rows.into_iter()
            .skip(self.conversation.detail_scroll)
            .take(usize::from(height))
            .map(|(text, tone)| self.chat_line(&text, tone, self.geometry.left))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn density_fixture() -> (Value, Value) {
        let session = "c".repeat(64);
        let pool = "w".repeat(64);
        let manager = format!("agent_{}", "a".repeat(32));
        let data = json!({
            "connected":true,"preview":true,"session_id":session,
            "agents":[
                {"agent_id":manager,"name":"Coordinator","role":"coordinator","status":"active"},
                {"agent_id":format!("agent_{}", "b".repeat(32)),"name":"Layout reviewer","role":"worker","status":"working"}
            ],"queue":[]
        });
        let linked = json!({
            "connected":true,"client_id":pool,"generation":"g".repeat(32),"process_id":42,
            "manager":{"client_id":pool,"thread_id":manager},
            "total":3,"desired_total":3,"registered_workers":3,"retiring":0,
            "working":2,"idle":1,"offline":0,"queued":1,
            "slots":[
                {"slot":0,"state":"working","epoch":1,"task_id":"exact-layout-task","task_summary":"Review narrow terminal layout"},
                {"slot":1,"state":"idle","epoch":1,"task_id":null,"task_summary":"Old finished task should stay out of the current summary"},
                {"slot":2,"state":"working","epoch":2,"task_id":"exact-parser-task","task_summary":"Verify slash command arguments"}
            ]
        });
        (data, linked)
    }

    #[test]
    fn statistics_summary_shows_names_states_and_tasks_without_diagnostic_ids() {
        let (data, linked) = density_fixture();
        let summary = summary_rows(&data, &json!({}), None, Some(&linked), " · ssmcp", 100);
        let text = summary
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Session · ssmcp",
            "Members 2",
            "Coordinator · coordinator · active",
            "Layout reviewer · worker · working",
            "#01 · working · Review narrow terminal layout",
            "#02 · idle · No active task",
            "Enter diagnostics",
        ] {
            assert!(text.contains(expected), "{expected}\n{text}");
        }
        for hidden in [
            data["session_id"].as_str().unwrap(),
            linked["client_id"].as_str().unwrap(),
            linked["generation"].as_str().unwrap(),
            data["agents"][0]["agent_id"].as_str().unwrap(),
            "exact-layout-task",
            "Old finished task",
        ] {
            assert!(!text.contains(hidden), "{hidden}\n{text}");
        }
        for width in [1, 4, 20, 48, 60] {
            let lines = summary_rows(
                &data,
                &json!({}),
                None,
                Some(&linked),
                " · 中文工作区",
                width,
            );
            assert!(
                lines
                    .iter()
                    .all(|(text, _)| text.width() <= usize::from(width))
            );
        }
    }

    #[test]
    fn statistics_diagnostics_keep_exact_ids_and_never_show_a_disconnected_link() {
        let (mut data, mut linked) = density_fixture();
        for width in [20, 48, 100] {
            let text = rows(&data, &json!({}), None, Some(&linked), 42, width)
                .into_iter()
                .map(|(line, _)| line)
                .collect::<Vec<_>>()
                .join("");
            for exact in [
                data["session_id"].as_str().unwrap(),
                linked["client_id"].as_str().unwrap(),
                linked["generation"].as_str().unwrap(),
                data["agents"][0]["agent_id"].as_str().unwrap(),
                "exact-layout-task",
                "exact-parser-task",
            ] {
                assert!(text.contains(exact), "diagnostics lost {exact}");
            }
        }
        data["connected"] = json!(false);
        linked["connected"] = json!(false);
        let text = summary_rows(&data, &json!({}), None, Some(&linked), "", 100)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Members 0"));
        assert!(text.contains("No worker pool connected"));
        assert!(!text.contains("Coordinator") && !text.contains("Review narrow"));
        let missing = json!({"connected":true,"client_id":"pool"});
        let text = summary_rows(&data, &json!({}), None, Some(&missing), "", 120)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Workers unavailable"));
        assert!(!text.contains("Workers 0"));
    }

    #[test]
    fn statistics_enter_and_escape_toggle_diagnostics_without_changing_the_draft() {
        let mut state = crate::client::terminal::preview_state(32, 120, &Ui::default());
        let (data, linked) = density_fixture();
        state.conversation.statistics_fixture = Some(data);
        state.conversation.worker_activity_fixture = Some(linked);
        state.conversation.target = Some("chosen-recipient".into());
        state.conversation.composer.set_text("保留 this draft");
        state.conversation.composer.home();
        let before = state.chat_saved_draft();
        state.chat_change_view(View::Statistics);
        assert!(!state.conversation.detail);
        state.chat_character('\r');
        assert!(state.conversation.detail);
        let data = state.chat_data();
        let text = state.chat_statistics_rows(&data, 100).join("\n");
        assert!(text.contains("Diagnostics"));
        assert!(text.contains(&"c".repeat(64)));
        state.chat_character('\r');
        assert!(!state.conversation.detail);
        assert_eq!(state.chat_saved_draft(), before);
        state.chat_character('\r');
        state.conversation.detail_scroll = 1000;
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Statistics);
        assert!(!state.conversation.detail);
        assert_eq!(state.conversation.detail_scroll, 0);
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.chat_saved_draft(), before);
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
    }

    #[test]
    #[ignore = "writes production statistics frames to SSMCP_WORKER_SNAPSHOT_DIR"]
    fn statistics_density_visual_fixture() {
        let directory = std::path::PathBuf::from(
            std::env::var_os("SSMCP_WORKER_SNAPSHOT_DIR").expect("snapshot output directory"),
        );
        std::fs::create_dir_all(&directory).unwrap();
        for (rows, cols, name) in [(32, 132, "statistics-wide"), (26, 60, "statistics-narrow")] {
            let mut state = crate::client::terminal::preview_state(rows, cols, &Ui::default());
            let (data, linked) = density_fixture();
            state.conversation.statistics_fixture = Some(data);
            state.conversation.worker_activity_fixture = Some(linked);
            state.ui.color = true;
            state.set_context("/tmp/ssmcp", Some("UI review"));
            state.chat_change_view(View::Statistics);
            for suffix in ["", "-detail", "-detail-end"] {
                if suffix == "-detail" {
                    state.chat_character('\r');
                }
                if suffix == "-detail-end" {
                    state.chat_key(Key::End);
                }
                let frame = state.chat_frame(Instant::now());
                std::fs::write(
                    directory.join(format!("{name}{suffix}.ansi")),
                    frame.diff(None),
                )
                .unwrap();
                std::fs::write(
                    directory.join(format!("{name}{suffix}.json")),
                    serde_json::to_vec(&json!({"rows":rows,"cols":cols})).unwrap(),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn statistics_keeps_pool_counts_without_coordination_history() {
        let linked = json!({
            "connected":true, "client_id":"linked-pool", "process_id":42, "total":3, "idle":1,
            "working":2,"offline":0,"desired_total":3,"registered_workers":3,
            "retiring":0,"queued":1,"generation":"pool",
            "manager_operations":[{"operation_id":1,"kind":"claim","state":"assigned",
                "task_id":"task-belongs-in-conversation","summary":"Inspect the layout",
                "slot":2,"epoch":7}]
        });
        let text = rows(&json!({}), &json!({}), None, Some(&linked), 42, 100)
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Workers 3 · Idle 1 · Working 2"));
        assert!(text.contains("Queued tasks 1"));
        assert!(!text.contains("Coordinator activity"));
        assert!(!text.contains("task-belongs-in-conversation"));
        assert!(!text.contains("Inspect the layout"));
    }

    fn worker(client: &str, thread: &str) -> crate::worker::Snapshot {
        crate::worker::Snapshot {
            generation: "pool-not-a-session-id".into(),
            process_id: 31415,
            manager: Some(crate::worker::ManagerView {
                client_id: client.into(),
                thread_id: thread.into(),
                participant_id: thread.into(),
                connection: 1,
                attached_at_ms: 1,
            }),
            manager_operations: Vec::new(),
            stopping: false,
            total: 2,
            idle: 1,
            working: 1,
            offline: 0,
            retiring: 0,
            registered_workers: 2,
            desired_total: 2,
            queued: 3,
            closed: false,
            slots: Vec::new(),
        }
    }

    #[test]
    fn statistics_uses_connected_identity_and_verified_manager_not_roster_selection() {
        let client = "c".repeat(64);
        let thread = "agent_verified_manager";
        let data = json!({"connected":true,"session_id":client,
            "agents":[{"agent_id":"highlighted-recipient"}],"queue":[]});
        let pool = worker(&client, thread);
        let text = rows(&data, &json!({}), Some(&pool), None, 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&client));
        assert!(text.contains(thread));
        assert!(text.contains("OS PID 31415"));
        assert!(text.contains("Workers 2 · Idle 1 · Working 1 · Offline 0"));
        assert!(text.contains("Manager thread ID\nagent_verified_manager"));
        assert!(text.contains("Registered session threads\nhighlighted-recipient"));
        assert!(text.contains("Pool generation\npool-not-a-session-id"));
        assert!(!text.contains("Session process ID\npool-not-a-session-id"));
        for data in [
            json!({"connected":false,"session_id":client}),
            json!({"connected":true,"session_id":"another-client"}),
        ] {
            let text = rows(&data, &json!({}), Some(&pool), None, 31415, 120)
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!text.contains(thread));
            assert!(text.contains("Manager thread ID\nunbound"));
        }
    }

    #[test]
    fn ordinary_statistics_lists_registered_threads_without_guessing_a_manager() {
        let client = "c".repeat(64);
        let first = format!("agent_{}", "1".repeat(32));
        let second = format!("agent_{}", "2".repeat(32));
        let mut data = json!({"connected":true,"session_id":client,
            "agents":[{"agent_id":first,"name":"Same name"},
                      {"agent_id":second,"name":"Same name"}],
            "target":first,"selected_agent":second});
        let text = rows(&data, &json!({}), None, None, 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains(&format!("Session process ID\n{client}")));
        assert!(text.contains("Manager thread ID\nunbound"));
        assert!(text.contains(&format!("Registered session threads\n{first}\n{second}")));
        assert!(text.contains("Registered members 2"));
        data["connected"] = json!(false);
        let text = rows(&data, &json!({}), None, None, 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Registered session threads\nnone"));
        assert!(text.contains("Registered members 0"));
        assert!(!text.contains(&first) && !text.contains(&second));
    }

    #[test]
    fn linked_worker_counts_never_replace_main_identity_or_infer_a_manager() {
        let main_client = "main-client";
        let data = json!({"connected":true,"session_id":main_client,
            "agents":[{"agent_id":"selected-recipient"}],"target":"selected-recipient"});
        let linked = json!({"client_id":"linked-worker-client","process_id":27182,"connected":true,
            "total":4,"desired_total":3,"registered_workers":3,"retiring":1,
            "idle":1,"working":1,"offline":1,"queued":5,"stopping":true});
        let text = rows(&data, &json!({}), None, Some(&linked), 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Session process ID\nmain-client"));
        assert!(text.contains("Manager thread ID\nunbound"));
        assert!(text.contains("OS PID 31415"));
        assert!(text.contains("Linked worker pool\nWorker session ID\nlinked-worker-client"));
        assert!(text.contains("Worker OS PID 27182"));
        assert!(text.contains("Workers 4 · Idle 1 · Working 1 · Offline 1"));
        assert!(text.contains("Target 3 · Registered 3 · Retiring 1"));
        assert!(text.contains("Queued tasks 5"));
        assert!(text.contains("Task intake stopped"));
        let local = worker(main_client, "verified-manager");
        let text = rows(&data, &json!({}), Some(&local), Some(&linked), 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Manager thread ID\nverified-manager"));
        assert!(text.contains("Workers 2 · Idle 1 · Working 1 · Offline 0"));
        assert!(!text.contains("linked-worker-client"));
    }

    #[test]
    fn linked_worker_manager_requires_the_same_live_pool_and_never_borrows_main_identity() {
        let data = json!({"connected":true,"session_id":"main-client",
            "agents":[{"agent_id":"selected-recipient"}],"target":"selected-recipient"});
        let mut linked = json!({"client_id":"worker-client","process_id":27182,"connected":true,
            "manager":{"client_id":"worker-client","thread_id":"verified-pool-manager"}});
        let render = |linked: &Value| {
            rows(&data, &json!({}), None, Some(linked), 31415, 120)
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = render(&linked);
        assert!(text.contains("Session process ID\nmain-client"));
        assert!(text.contains("Manager thread ID\nunbound"));
        assert!(text.contains("OS PID 31415"));
        assert!(text.contains("Worker session ID\nworker-client"));
        assert!(text.contains("Worker OS PID 27182"));
        assert!(text.contains("Worker manager thread ID\nverified-pool-manager"));
        linked["manager"]["client_id"] = json!("main-client");
        let text = render(&linked);
        assert!(text.contains("Worker manager thread ID\nunavailable"));
        assert!(!text.contains("verified-pool-manager"));
        linked["manager"] = Value::Null;
        assert!(render(&linked).contains("Worker manager thread ID\nunbound"));
        linked.as_object_mut().unwrap().remove("manager");
        assert!(render(&linked).contains("Worker manager thread ID\nunavailable"));
        linked["connected"] = json!(false);
        let text = render(&linked);
        assert!(!text.contains("worker-client"));
        assert!(!text.contains("Worker OS PID 27182"));
        assert!(text.contains("No worker pool connected"));
    }

    #[test]
    fn statistics_counts_only_recorded_call_outcomes_and_preserves_full_wrapped_ids() {
        let audit = json!({"entries":[{"calls":[
            {"finished":false,"success":null},
            {"finished":true,"success":true},
            {"finished":true,"success":false},
            {"finished":true,"success":null}
        ]}]});
        assert_eq!(
            ToolCounts::from_audit(&audit),
            ToolCounts {
                running: 1,
                succeeded: 1,
                failed: 1,
                unknown: 1,
            }
        );
        let id = "session_0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        for width in [1, 4, 20, 48, 80, 120] {
            let lines = wrapped(id, width);
            assert_eq!(lines.concat(), id);
            assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        }
    }

    #[test]
    fn statistics_without_host_execution_metrics_reports_unavailable_not_zero() {
        // Old snapshots, disconnected views and unrelated audit success counts
        // cannot manufacture a native avoided-spawn count.
        for data in [
            json!({}),
            json!({"connected":false}),
            json!({"connected":true,"session_id":"client"}),
        ] {
            let audit = json!({"entries":[{"calls":[
                {"finished":true,"success":true},
                {"finished":true,"success":true}
            ]}]});
            let text = rows(&data, &audit, None, None, 123, 120)
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(
                    "Avoided command executions\nunavailable · host metric not implemented"
                )
            );
            assert!(!text.contains("Avoided command executions\n0"));
            assert!(text.contains("Succeeded 2"));
        }
    }

    #[test]
    fn statistics_navigation_keeps_the_draft_recipient_and_group_unchanged() {
        let mut state = crate::client::terminal::preview_state(28, 100, &Ui::default());
        assert_eq!(
            state.conversation.canvas.snapshot()["columns"],
            json!([
                "statistics",
                "conversation",
                "code audit",
                "queue",
                "questions",
                "Logs",
                "Tasks"
            ])
        );
        state.chat_command("/sessions").unwrap();
        assert_eq!(
            state.conversation.canvas.snapshot()["columns"],
            json!([
                "statistics",
                "conversation",
                "recipients",
                "code audit",
                "queue",
                "questions",
                "Logs",
                "Tasks"
            ])
        );
        state.chat_move(1);
        state.chat_group_toggle();
        state.chat_change_view(View::Activity);
        state.conversation.target = Some("example-reviewer".into());
        state.conversation.composer.set_text("保留 draft");
        state.conversation.composer.home();
        let before = state.chat_saved_draft();
        let groups = state.conversation.snapshot()["group_selected"].clone();
        state.chat_key(Key::Back);
        assert_eq!(state.conversation.view, View::Statistics);
        state.chat_key(Key::AuditDown);
        state.chat_key(Key::End);
        state.chat_frame(Instant::now());
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.chat_saved_draft(), before);
        assert_eq!(state.conversation.snapshot()["group_selected"], groups);
        assert!(state.conversation.submitting.is_none());
        assert!(state.chat_command("/new agents").is_err());
        assert!(state.chat_command("/new history").is_err());
        state.chat_command("/new statistics").unwrap();
        assert_eq!(state.conversation.view, View::Statistics);
        // The inserted Statistics column is followed by Recipients here.
        state.chat_key(Key::Open);
        assert_eq!(state.conversation.view, View::Agents);
        assert_eq!(state.chat_saved_draft(), before);
        assert_eq!(state.conversation.snapshot()["group_selected"], groups);
    }

    #[test]
    fn statistics_counts_real_retained_call_updates_without_counting_retries() {
        let mut audit = crate::client::audit_view::AuditView::default();
        for request_id in 1..=4 {
            assert!(
                audit.ingest(&json!({"type":"ssmcp.code_mode","run_id":"stats-run",
                "phase":"call-started","request_id":request_id,"tool":"ssmcp.exec.status"}))
            );
        }
        for (request_id, success) in [
            (2, Some(true)),
            (3, Some(false)),
            (4, None),
            (2, Some(true)),
        ] {
            assert!(
                audit.ingest(&json!({"type":"ssmcp.code_mode","run_id":"stats-run",
                "phase":"call-finished","request_id":request_id,"success":success}))
            );
        }
        assert_eq!(
            ToolCounts::from_audit(&audit.snapshot()),
            ToolCounts {
                running: 1,
                succeeded: 1,
                failed: 1,
                unknown: 1,
            }
        );
        assert!(
            audit.ingest(&json!({"type":"ssmcp.code_mode","run_id":"stats-run",
            "phase":"call-finished","request_id":1,"success":true}))
        );
        assert_eq!(
            ToolCounts::from_audit(&audit.snapshot()),
            ToolCounts {
                running: 0,
                succeeded: 2,
                failed: 1,
                unknown: 1,
            }
        );
    }

    #[test]
    fn statistics_frames_fit_small_screens_and_return_to_the_original_editor() {
        for height in [1, 3, 6, 10, 24, 40] {
            for width in [1, 4, 20, 48, 80, 120] {
                let mut state =
                    crate::client::terminal::preview_state(height, width, &Ui::default());
                state.conversation.composer.set_text("保留 draft");
                let before = state.chat_saved_draft();
                state.chat_change_view(View::Statistics);
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), usize::from(height));
                assert!(frame.cursor.0 < height && frame.cursor.1 < width);
                state.chat_key(Key::End);
                state.chat_frame(Instant::now());
                state.chat_key(Key::Home);
                state.chat_frame(Instant::now());
                assert_eq!(state.conversation.detail_scroll, 0);
                state.chat_key(Key::Escape);
                assert_eq!(state.conversation.view, View::Activity);
                assert_eq!(state.chat_saved_draft(), before);
            }
        }
    }
}
