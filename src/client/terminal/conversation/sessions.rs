//! Two-level session browser: a process contains registered conversation threads.
use super::*;
use std::collections::BTreeMap;

fn attach_native(entry: &mut Value, native: &Value) {
    entry["group_id"] = native["id"].clone();
    entry["group_account"] = native["account"].clone();
}

pub(super) fn arrange(mut local: Vec<Value>, native: Vec<Value>, local_id: &str) -> Vec<Value> {
    let mut processes: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for entry in native {
        processes
            .entry(field(&entry, "client_id").into())
            .or_default()
            .push(entry);
    }
    let mut own = processes.remove(local_id).unwrap_or_default();
    let mut output = Vec::new();
    for (index, mut entry) in local.drain(..).enumerate() {
        entry["client_id"] = json!(local_id);
        entry["session_role"] = json!(if index == 0 {
            "process"
        } else if field(&entry, "kind") == "observed" {
            "observed"
        } else {
            "thread"
        });
        entry["session_depth"] = json!(usize::from(index != 0));
        if index == 0 {
            entry["name"] = json!("This process · Next caller");
        }
        let matching = own.iter().position(|candidate| {
            if index == 0 {
                field(candidate, "native_kind") == "account_inbox"
            } else {
                field(candidate, "native_kind") == "agent"
                    && field(candidate, "agent_id") == field(&entry, "agent_id")
            }
        });
        if let Some(index) = matching {
            attach_native(&mut entry, &own.remove(index));
        }
        output.push(entry);
    }
    for mut entry in own {
        entry["session_role"] = json!("thread");
        entry["session_depth"] = json!(1);
        output.push(entry);
    }
    for (client_id, mut entries) in processes {
        entries.sort_by(|a, b| field(a, "id").cmp(field(b, "id")));
        let parent = entries
            .iter()
            .position(|entry| field(entry, "native_kind") == "account_inbox");
        let mut process = if let Some(index) = parent {
            entries.remove(index)
        } else {
            json!({"kind":"process","client_id":client_id,
                "name":"Session process","targetable":false})
        };
        process["session_role"] = json!("process");
        process["session_depth"] = json!(0);
        output.push(process);
        for mut entry in entries {
            entry["session_role"] = json!("thread");
            entry["session_depth"] = json!(1);
            output.push(entry);
        }
    }
    output
}

pub(super) fn group_id(entry: &Value) -> Option<&str> {
    let id = if field(entry, "kind") == "native" {
        field(entry, "id")
    } else {
        field(entry, "group_id")
    };
    (!id.is_empty()).then_some(id)
}

fn short_id(value: &str) -> String {
    if value.is_empty() {
        return "ID unavailable".into();
    }
    let value = value.strip_prefix("agent_").unwrap_or(value);
    let safe = panel::safe_text(value);
    let characters: Vec<_> = safe.chars().collect();
    let head: String = characters.iter().take(10).collect();
    if characters.len() <= 10 {
        return format!("#{head}");
    }
    let tail: String = characters[characters.len() - 8..].iter().collect();
    format!("#{head}…{tail}")
}

fn identity_rows(rows: &mut Vec<(String, PanelTone)>, label: &str, id: &str, width: usize) {
    rows.push((label.into(), PanelTone::Muted));
    let value = if id.is_empty() {
        "Not connected".into()
    } else {
        panel::safe_text(id)
    };
    for line in events::styled_rows(&value, width.max(1), false) {
        rows.push((line, PanelTone::Normal));
    }
}

fn session_summary(entry: &Value) -> Option<&str> {
    [field(entry, "summary"), field(entry, "role")]
        .into_iter()
        .find(|value| !value.trim().is_empty())
}

impl State {
    pub(super) fn chat_observed_history(&mut self, run_id: &str) {
        if self.audit.open_run(run_id) {
            self.conversation.detail = true;
            self.conversation.content_focus = true;
            self.notice = Some("Observed call · Esc returns to Recipients".into());
        } else {
            self.notice = Some("This call is no longer in the retained audit history".into());
        }
        self.dirty = true;
    }

    pub(super) fn chat_session_history(&mut self) {
        if let Some(entry) = self.chat_selected()
            && field(&entry, "kind") == "observed"
        {
            self.chat_observed_history(field(&entry, "run_id"));
        } else {
            self.chat_thread_history_selected();
        }
    }

    pub(super) fn chat_sidebar_agent_mouse(&mut self, button: u16, x: u16, y: u16) -> bool {
        if button & (32 | 64) != 0
            || !matches!(button & 3, 0 | 2)
            || self.ui.layout != Layout::Chat
            || !self.ui.sidebar
            || !self.ui.panels.contains(&crate::client::ui::Panel::Agents)
            || self.help
            || self.selection
            || self.sleep.visible
        {
            return false;
        }
        let Some(rect) = PanelLayout::configured(
            self.geometry.cols,
            self.geometry.rows,
            self.ui.sidebar,
            self.ui.sidebar_width,
        )
        .sidebar
        else {
            return false;
        };
        let Some(hit) = self
            .conversation
            .sidebar_agents
            .iter()
            .find(|hit| {
                hit.rect == rect
                    && hit.row == y
                    && x >= rect.col
                    && x < rect.col.saturating_add(rect.width)
            })
            .cloned()
        else {
            return false;
        };
        if hit.view != self.conversation.view
            || hit.process_generation != self.conversation.process_generation
        {
            self.notice = Some(
                "Recipients changed · choose a newly displayed row; draft and recipient kept"
                    .into(),
            );
            self.dirty = true;
            return true;
        }
        // A painted row is an identity, not an index into a refreshed roster.
        let data = self.chat_data();
        let entries = self.chat_recipients(&data);
        let Some(index) = entries
            .iter()
            .position(|entry| agent_key(entry) == hit.identity.key)
        else {
            self.notice = Some(
                "This member or caller is no longer available · draft and recipient kept".into(),
            );
            self.dirty = true;
            return true;
        };
        self.chat_change_view(View::Agents);
        self.conversation.selected = index;
        self.conversation.selected_agent = Some(hit.identity.key);
        self.conversation.content_focus = true;
        self.conversation.detail = false;
        if button & 3 == 2 {
            if let Some(run_id) = hit.identity.run_id {
                // Inspect the exact call that was drawn, even if its display group has a newer call.
                self.chat_observed_history(&run_id);
            } else {
                self.chat_thread_history_selected();
            }
        }
        true
    }

    pub(super) fn session_details(&self, entry: &Value, data: &Value) -> Vec<(String, PanelTone)> {
        let process = field(entry, "session_role") == "process";
        let width = if self.chat_agent_list_width() < self.geometry.left {
            self.geometry.left - self.chat_agent_list_width() - 3
        } else {
            self.geometry.left
        };
        let mut rows = vec![
            (
                (if process {
                    "Session process"
                } else {
                    "Session thread"
                })
                .into(),
                PanelTone::Heading,
            ),
            ("".into(), PanelTone::Normal),
            (panel::safe_text(field(entry, "name")), PanelTone::Accent),
            ("".into(), PanelTone::Normal),
        ];
        identity_rows(
            &mut rows,
            "Client ID",
            field(entry, "client_id"),
            usize::from(width),
        );
        if !process {
            identity_rows(
                &mut rows,
                "Thread ID",
                field(entry, "agent_id"),
                usize::from(width),
            );
            if let Some(summary) = session_summary(entry) {
                identity_rows(&mut rows, "Summary", summary, usize::from(width));
            }
        }
        rows.push(("".into(), PanelTone::Normal));
        if !process && field(entry, "kind") != "native" {
            let pending = items(data, "queue")
                .iter()
                .filter(|message| {
                    field(message, "target") == field(entry, "agent_id")
                        && matches!(field(message, "status"), "queued" | "sending" | "editing")
                })
                .count();
            rows.push((
                format!("{pending} pending for this recipient"),
                PanelTone::Normal,
            ));
            rows.push(("Only this member receives it".into(), PanelTone::Muted));
        }
        if let Some(id) = group_id(entry) {
            let selected = self.conversation.groups.selected.contains_key(id);
            rows.push((
                (if selected {
                    "Selected for group"
                } else {
                    "Space · select for group"
                })
                .into(),
                PanelTone::Accent,
            ));
            rows.push(("Enter · choose message recipient".into(), PanelTone::Normal));
            rows.push(("g · compose to selected group".into(), PanelTone::Muted));
            rows.push(("i · show full IDs".into(), PanelTone::Muted));
            rows.push(("".into(), PanelTone::Normal));
            rows.push((
                (if process {
                    "Process selection addresses its inbox"
                } else {
                    "Thread selection addresses only this ID"
                })
                .into(),
                PanelTone::Muted,
            ));
        } else {
            rows.push(("Enter · choose message recipient".into(), PanelTone::Normal));
            rows.push(("i · show available IDs".into(), PanelTone::Muted));
            rows.push((
                "Group sending needs native messaging".into(),
                PanelTone::Muted,
            ));
        }
        rows.push((
            "h / right-click · read-only history".into(),
            PanelTone::Normal,
        ));
        rows.push((
            "History keeps your draft and recipient".into(),
            PanelTone::Muted,
        ));
        rows
    }

    pub(super) fn chat_session_info(&mut self) {
        let Some(entry) = self.chat_selected() else {
            return;
        };
        let client_id = field(&entry, "client_id");
        let client_id = if client_id.is_empty() {
            "unavailable".into()
        } else {
            panel::safe_text(client_id)
        };
        let name = panel::safe_text(field(&entry, "name"));
        let mut text = format!("Session: {name}\nClient ID: {client_id}");
        if field(&entry, "session_role") == "thread" {
            let _ = write!(
                text,
                "\nThread ID: {}",
                panel::safe_text(field(&entry, "agent_id"))
            );
            if let Some(summary) = session_summary(&entry) {
                let _ = write!(text, "\nSummary: {}", panel::safe_text(summary));
            }
        }
        if let Some(id) = group_id(&entry) {
            let _ = write!(text, "\nExact recipient: {}", panel::safe_text(id));
        }
        let event = json!({"type":"ssmcp.message","role":"system","author":"Recipients",
            "format":"plain","text":text});
        self.log_event("session", &event, format!("{text}\n").as_bytes());
        self.chat_follow_latest();
        self.conversation.content_focus = false;
    }

    pub(super) fn chat_session_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let entries = self.chat_recipients(data);
        if let Some(key) = &self.conversation.selected_agent {
            if let Some(index) = entries.iter().position(|entry| agent_key(entry) == *key) {
                self.conversation.selected = index;
            } else {
                self.conversation.selected = 0;
                self.notice = Some("Selected session left · choose an exact recipient".into());
            }
        }
        let missing = self
            .conversation
            .selected_agent
            .as_ref()
            .is_some_and(|key| !entries.iter().any(|entry| agent_key(entry) == *key));
        let window = self.chat_list_window(entries.len(), height);
        self.conversation.drawn_agents = entries.iter().map(agent_key).collect();
        let selected = &entries[self.conversation.selected];
        if !missing {
            self.conversation.selected_agent = Some(agent_key(selected));
        }
        let width = self.geometry.left;
        let list_width = self.chat_agent_list_width();
        let processes = entries
            .iter()
            .filter(|entry| field(entry, "session_role") == "process")
            .count();
        let threads = entries
            .iter()
            .filter(|entry| field(entry, "session_role") == "thread")
            .count();
        let mut rows = vec![self.chat_line(
            &format!("  Recipients · {processes} processes · {threads} threads"),
            PanelTone::Heading,
            list_width,
        )];
        for index in window {
            let entry = &entries[index];
            let process = field(entry, "session_role") == "process";
            let observed = field(entry, "kind") == "observed";
            let selected = !missing && index == self.conversation.selected;
            let marker = if selected { "›" } else { " " };
            let checked = group_id(entry)
                .is_some_and(|id| self.conversation.groups.selected.contains_key(id));
            let check = if checked {
                "[x]"
            } else if group_id(entry).is_some() {
                "[ ]"
            } else if observed {
                "↳"
            } else {
                "   "
            };
            let indent = if process { "" } else { "  " };
            let name = panel::safe_text(field(entry, "name"));
            let model = panel::safe_text(field(entry, "model"));
            let label = if observed && !model.is_empty() {
                format!("{name} · {model}")
            } else {
                name
            };
            let title = format!(" {marker} {indent}{check} {label}");
            let id = if process {
                field(entry, "client_id")
            } else if observed {
                field(entry, "run_id")
            } else {
                field(entry, "agent_id")
            };
            let role = if process {
                "process"
            } else if observed {
                "observed call"
            } else {
                "thread"
            };
            let identity = if !process
                && !observed
                && entries
                    .iter()
                    .filter(|other| {
                        field(other, "kind") != "observed"
                            && field(other, "name") == field(entry, "name")
                    })
                    .count()
                    > 1
            {
                // Keep same-name threads distinguishable even when their IDs share a prefix.
                let suffix: String = id
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                format!("…{suffix}")
            } else {
                short_id(id)
            };
            let subtitle = if observed {
                format!(
                    "     {indent}{identity} · {} calls · {}",
                    entry["calls"],
                    field(entry, "status")
                )
            } else if !process && let Some(summary) = session_summary(entry) {
                let prefix = format!("     {indent}{identity} · ");
                let available = usize::from(list_width).saturating_sub(prefix.width());
                format!("{prefix}{}", panel::fit(summary, available))
            } else {
                format!("     {indent}{identity} · {role}")
            };
            rows.push(self.chat_line(
                &title,
                if selected {
                    PanelTone::Accent
                } else {
                    PanelTone::Normal
                },
                list_width,
            ));
            rows.push(self.chat_line(&subtitle, PanelTone::Muted, list_width));
        }
        if list_width < width {
            let details = self.chat_agent_details((!missing).then_some(selected), data);
            rows.resize_with(usize::from(height), || {
                self.chat_line("", PanelTone::Normal, list_width)
            });
            for (index, row) in rows.iter_mut().enumerate() {
                let (detail, tone) = details
                    .get(index)
                    .map(|(text, tone)| (text.as_str(), *tone))
                    .unwrap_or(("", PanelTone::Normal));
                row.push_str(&self.tone(PanelTone::Muted, "   "));
                row.push_str(&self.chat_line(detail, tone, width - list_width - 3));
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests;
