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
    // A recipient highlight and a recent caller are not a management binding.
    let thread_id = workers
        .and_then(|workers| workers.manager.as_ref())
        .filter(|manager| Some(manager.client_id.as_str()) == client_id)
        .map(|manager| manager.thread_id.as_str());
    push("Statistics", PanelTone::Heading);
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
        for thread in threads {
            push(thread, PanelTone::Normal);
        }
    }
    push("", PanelTone::Normal);
    push(
        &format!("Registered members {}", items(data, "agents").len()),
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
    if let Some(workers) = workers {
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
    output
}

impl State {
    pub(super) fn chat_statistics_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let workers = (!self.preview).then(crate::worker::display).flatten();
        let linked = (!self.preview && workers.is_none())
            .then(crate::client::worker_window::linked_snapshot)
            .flatten();
        let rows = rows(
            data,
            &self.audit.snapshot(),
            workers.as_ref(),
            linked.as_ref(),
            std::process::id(),
            self.geometry.left,
        );
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
        assert!(!text.contains("pool-not-a-session-id"));
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
        data["connected"] = json!(false);
        let text = rows(&data, &json!({}), None, None, 31415, 120)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Registered session threads\nnone"));
        assert!(!text.contains(&first) && !text.contains(&second));
    }

    #[test]
    fn linked_worker_counts_never_replace_main_identity_or_infer_a_manager() {
        let main_client = "main-client";
        let data = json!({"connected":true,"session_id":main_client,
            "agents":[{"agent_id":"selected-recipient"}],"target":"selected-recipient"});
        let linked = json!({"client_id":"linked-worker-client","process_id":27182,
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
    fn statistics_navigation_keeps_the_draft_recipient_and_group_unchanged() {
        let mut state = crate::client::terminal::preview_state(28, 100, &Ui::default());
        state.chat_command("/sessions").unwrap();
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
        state.chat_command("/new statistics").unwrap();
        assert_eq!(state.conversation.view, View::Statistics);
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
