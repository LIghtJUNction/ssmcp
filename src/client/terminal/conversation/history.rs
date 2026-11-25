use super::*;
use crate::session::attach::thread_history::{self, Selector};

pub(super) type ScopedHistoryHandle = std::sync::Arc<std::sync::Mutex<ScopedHistory>>;

pub(super) struct ScopedHistory {
    selector: Selector,
    generation: Option<String>,
    cursor: Option<u64>,
    messages: std::collections::VecDeque<String>,
    bytes: usize,
    rows: Vec<String>,
    width: u16,
    refreshed: Option<std::time::Instant>,
    truncated: bool,
    error: Option<String>,
}

impl ScopedHistory {
    fn new(selector: Selector) -> Result<Self, String> {
        selector.validate()?;
        let mut view = Self {
            selector,
            generation: None,
            cursor: None,
            messages: Default::default(),
            bytes: 0,
            rows: Vec::new(),
            width: 0,
            refreshed: None,
            truncated: false,
            error: None,
        };
        view.refresh();
        if let Some(error) = &view.error {
            return Err(error.clone());
        }
        Ok(view)
    }

    pub(super) fn snapshot(&self) -> Value {
        json!({"scope":self.selector.value(),"generation":self.generation,"next_cursor":self.cursor,
            "retained_events":self.messages.len(),"retained_text_bytes":self.bytes,
            "truncated":self.truncated,"error":self.error,"read_only":true})
    }

    pub(super) fn title(&self) -> String {
        if let Some(agent) = &self.selector.agent_id {
            format!("Thread {agent}")
        } else if self.selector.main {
            "Main caller live logs".into()
        } else {
            "Process live logs".into()
        }
    }

    fn refresh(&mut self) {
        if self
            .refreshed
            .is_some_and(|time| time.elapsed() < std::time::Duration::from_millis(250))
        {
            return;
        }
        self.refreshed = Some(std::time::Instant::now());
        let page =
            match thread_history::page(&self.selector, self.cursor, self.generation.as_deref(), 64)
            {
                Ok(page) => page,
                Err(error) => {
                    if self.error.as_ref() != Some(&error) {
                        self.width = 0;
                    }
                    self.error = Some(error);
                    return;
                }
            };
        if self.error.take().is_some() {
            self.width = 0;
        }
        self.generation = page["generation"].as_str().map(str::to_owned);
        self.cursor = page["next_cursor"].as_u64();
        if page["retention_gap_possible"] == true {
            self.truncated = true;
            self.width = 0;
        }
        for event in page["events"].as_array().into_iter().flatten() {
            let text = format!(
                "### {} · {}{}\n{}",
                event["sequence"],
                panel::safe_text(event["kind"].as_str().unwrap_or("event")),
                event["status"]
                    .as_str()
                    .map_or_else(String::new, |status| format!(
                        " · {}",
                        panel::safe_text(status)
                    )),
                event["text"].as_str().unwrap_or("")
            );
            self.bytes += text.len();
            self.messages.push_back(text);
            while self.messages.len() > 256 || self.bytes > 256 * 1024 {
                let Some(old) = self.messages.pop_front() else {
                    break;
                };
                self.bytes = self.bytes.saturating_sub(old.len());
                self.truncated = true;
            }
            self.width = 0;
        }
    }
}

fn history_selector(entry: &Value) -> Result<Selector, String> {
    let role = field(entry, "session_role");
    if !matches!(role, "process" | "thread" | "main") || field(entry, "kind") == "observed" {
        return Err("This display label has no authenticated live thread".into());
    }
    let selector = Selector {
        client_id: field(entry, "client_id").into(),
        agent_id: (role == "thread").then(|| field(entry, "agent_id").to_owned()),
        main: role == "main",
    };
    selector.validate()?;
    Ok(selector)
}

fn local_history_selector(entry: &Value, local_client_id: &str) -> Result<Selector, String> {
    let selector = history_selector(entry)?;
    if selector.client_id != local_client_id {
        return Err("This process does not retain that client's live logs; use the authenticated attach view for the selected client".into());
    }
    Ok(selector)
}

impl State {
    pub(super) fn chat_thread_history_mouse(&mut self, x: u16, y: u16) -> bool {
        if x >= self.geometry.left
            || y < self.geometry.chat_body_top()
            || y >= prompt_top(self.geometry)
        {
            return false;
        }
        if !self.conversation.detail && x < self.chat_agent_list_width() {
            let Some(index) = self.chat_list_hit(y) else {
                return false;
            };
            let Some(key) = self.conversation.drawn_agents.get(index).cloned() else {
                return false;
            };
            // Use the row the user actually clicked, even without a prior left-click.
            // chat_selected resolves this stable key and never falls back to a new occupant.
            self.conversation.selected = index;
            self.conversation.selected_agent = Some(key);
        }
        self.chat_session_history();
        true
    }

    pub(super) fn chat_thread_history_selected(&mut self) {
        let Some(entry) = self.chat_selected() else {
            self.notice = Some("The selected thread is no longer available".into());
            self.dirty = true;
            return;
        };
        match local_history_selector(&entry, &self.conversation.process_id)
            .and_then(ScopedHistory::new)
        {
            Ok(view) => {
                let title = view.title();
                self.chat_change_view(View::History);
                self.conversation.scoped_history =
                    Some(std::sync::Arc::new(std::sync::Mutex::new(view)));
                self.conversation.detail = true;
                self.conversation.content_focus = true;
                self.conversation.detail_scroll = usize::MAX;
                self.notice = Some(format!(
                    "{title} · read-only · Esc or right-click returns to Recipients"
                ));
            }
            Err(error) => self.notice = Some(format!("Live thread logs unavailable: {error}")),
        }
        self.dirty = true;
    }

    fn chat_scoped_history_rows(&mut self, height: u16) -> Vec<String> {
        let width = self.geometry.left;
        let shared = self
            .conversation
            .scoped_history
            .as_ref()
            .expect("scoped live logs")
            .clone();
        let mut view = shared.lock().unwrap_or_else(|error| error.into_inner());
        view.refresh();
        if view.width != width {
            let heading = match view.selector.agent_id.as_deref() {
                Some(agent) => format!("## Thread {agent}\nClient {}", view.selector.client_id),
                None if view.selector.main => {
                    format!("## Main caller\nClient {}", view.selector.client_id)
                }
                None => format!(
                    "## Process live logs\nClient {}\nUnattributed records stay in this process view.",
                    view.selector.client_id
                ),
            };
            let mut rows = markdown::rows(&heading, width, &self.ui);
            if view.truncated {
                rows.extend(markdown::rows(
                    "Earlier events may be outside this process's retained log window.",
                    width,
                    &self.ui,
                ));
            }
            if let Some(error) = &view.error {
                rows.extend(markdown::rows(
                    &format!("Live logs unavailable: {error}"),
                    width,
                    &self.ui,
                ));
            }
            if view.messages.is_empty() && view.error.is_none() {
                rows.extend(markdown::rows("No events with this verified identity are retained yet. Unassigned records remain in the process live logs.",width,&self.ui));
            }
            for text in &view.messages {
                rows.extend(markdown::rows(text, width, &self.ui));
                if rows.len() > 4096 {
                    rows.drain(..rows.len() - 4096);
                }
            }
            view.rows = rows;
            view.width = width;
        }
        self.conversation.detail_scroll = self
            .conversation
            .detail_scroll
            .min(view.rows.len().saturating_sub(usize::from(height)));
        view.rows
            .iter()
            .skip(self.conversation.detail_scroll)
            .take(usize::from(height))
            .cloned()
            .collect()
    }

    #[cfg(all(test, unix))]
    pub(in crate::client::terminal) fn resume_history_for_test(&mut self, id: &str) {
        // Rendering fixtures may select a current-process owner. This path is absent
        // from production and never reads or restores a saved conversation.
        let history = crate::client::interface::history::resume_test_history(id)
            .expect("fixture owner must remain in memory");
        let owner = crate::client::interface::history::current_owner()
            .expect("fixture owner must be active");
        assert_eq!(owner.id(), history.metadata.id);
        self.events.rehydrate_history(owner, &history.messages);
        self.sync_history_owner();
        self.chat_change_view(View::Activity);
        self.dirty = true;
    }

    pub(super) fn chat_history_rows(&mut self, height: u16) -> Vec<String> {
        if self.conversation.scoped_history.is_some() {
            return self.chat_scoped_history_rows(height);
        }
        self.conversation.list_count = 0;
        markdown::rows(
            "No live thread selected. Return to Recipients to select a current member.",
            self.geometry.left,
            &self.ui,
        )
        .into_iter()
        .take(usize::from(height))
        .collect()
    }

    pub(super) fn chat_help_rows(&mut self, height: u16) -> Vec<String> {
        if self.conversation.plugin_help {
            let lines = markdown::rows(
                crate::client::interface::plugins::HELP,
                self.geometry.left,
                &self.ui,
            );
            return self.chat_help_page(lines, height);
        }
        if self.conversation.about {
            let text = format!(
                "# About {}\n\nVersion: {}\n\n{}\n\nLicense: {}\nProject: {}\n\n/help: keyboard shortcuts and local commands.\n\nUp/Down, PgUp/PgDn, Home/End: scroll. Esc or F1: close.\n",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_DESCRIPTION"),
                env!("CARGO_PKG_LICENSE"),
                env!("CARGO_PKG_REPOSITORY"),
            );
            let lines = markdown::rows(&text, self.geometry.left, &self.ui);
            return self.chat_help_page(lines, height);
        }
        if let Some(lines) = self.chat_openai_rows() {
            return self.chat_help_page(lines, height);
        }
        let mut text = String::from("# SSMCP main help\n\n");
        if let Some(guide) = &self.guide {
            text.push_str("Online guide: ");
            text.push_str(guide.url());
            text.push_str("\n\n");
        }
        text.push_str(
            "## Write and send\nEnter: queue a requirement; continue writing the next draft.\nCtrl+J / Shift+Enter: newline. Tab / Shift+Tab: switch focus.\nAlt+Up/Down: increase/decrease visible input rows; keeps your draft and cursor.\nUp/Down: move over wrapped input lines; then recent inputs from this process.\nEmpty input + Up: take back the latest queued request before recent inputs. Wait for confirmation; delivered requests cannot be taken back.\nCtrl+C: clear the current input; on empty input, show an exit reminder, then press again to exit. Any other input cancels the exit reminder. Other queued requests stay unchanged.\nLeft/Right: move cursor; at a draft boundary, move to the adjacent panel. Home/End: line start/end.\nCtrl+A/E: line start/end. Ctrl+U/K: delete before/after cursor.\nCtrl+W: delete word. Backspace/Delete: edit text.\n\n## Members, sessions and questions\nFrom the initial conversation, Left at draft start opens Statistics; /sessions or /message chooses recipients.\nh or right-click on a member: view this process's verified live thread logs without changing your draft or recipient. Observed callers open their call audit instead.\nIn the sidebar, left-click highlights that member or caller in Recipients; right-click opens its live logs.\nAlt+Left/Right always moves between panels.\nEsc: return without changing recipient or draft.\nShift+Up or /questions: answer pending questions; repeat for next question.\nEnter a choice number or a free answer; Enter saves it.\nEsc: question list; Delete in list skips; Esc again returns.\nQuestions never pause the agent's other work.\n\n## Shell commands\n!command: execute with your local shell in the workspace.\nUses the ASCII ! prefix; runs asynchronously with output and exit status here.\n\n## Tasks\n/command [JOB_ID]: view the complete argv and multiline script; scroll to read all lines.\n/tasks: open Tasks immediately to the right of Logs; refreshes while visible.\nUp/Down select a job; Enter opens details; Esc returns to the list or input.\nPgUp/PgDn and the mouse wheel scroll.\n/tasks cancel ID: explicitly cancel that local job.\n\n## Files, skills and paste\n@path or @\"path with spaces\": reference a workspace file/image.\n$skill-name: reference an effective project/global skill.\nPaste over 8 KiB: saved as @file; add your request after it.\nText paste limit: 4 MiB; image paste limit: 16 MiB. Input/expanded requirement limit: 16 KiB.\nCtrl+V or /paste: import clipboard text or image.\n/attach PATH: import an image as a private file reference.\n/copy: copy the latest agent reply to the clipboard.\n\n## MCP access\nUse /logs to locate this session’s MCP URL. Authorize access in the controlling MCP app.\n\n## Review and copy\nTab: browse content. Enter: open or toggle the selected record.\nLeft/Right: move between canvas panels; Esc: back. Wheel/PgUp/PgDn: scroll content.\nCtrl+Home: oldest retained output. Ctrl+End: return to the latest and follow new activity.\nThe footer shows how much output is below your position.\nRight-click or /logs: move to Logs; Esc/right-click returns to the previous panel.\nLogs and Tasks are adjacent canvas panels; Left/Right moves between them.\n/raw also toggles Logs. Full output stays available there.\n/diff single|split: stack changes or show old/new lines side by side.\nSplit uses a single column on narrow terminals.\n/new [chat|statistics|audit|queue|questions]: add a panel to the right.\n/close: close this panel; the session, drafts and running tasks are kept.\nCanvas navigation does not wrap; each panel remembers its selection and scroll position.\nAlt+C, F7 or /select: freeze display and use your terminal's native mouse selection.\nCopy using your terminal's Copy shortcut or menu (often Ctrl+Shift+C).\nAlt+C/F7/Esc resumes updates and restores interface scrolling and clicks.\nOn terminals supporting it, Shift+drag selects directly without entering copy mode.\n/copy still copies only the latest agent reply.\n\n## Queued requests\n/queue: Up/Down select; Enter take back; i/I inspect without editing.\nDelete cancels a revocable request. R copies delivered text.\nFor uncertain/offline items, R opens explicit review before resending.\nTaking back waits for server acknowledgement; delivery may win.\n\n## Appearance and live logs\nF1: help. F2: next view. F3: sidebar. F4: theme.\nF5 reloads appearance. F8 saves appearance. F9 changes log category.\n/reload restarts the client. Current-process logs are discarded on exit.\nConversation logs stay in bounded memory; they are never saved or restored.\n/handoff [note]: export the current live conversation on request and prepare an @file draft.\nChoose the recipient, review the draft, then Enter to send.\n\n## Local commands\n",
        );
        for command in crate::client::composer::SLASH_COMMANDS {
            let _ = writeln!(text, "{} — {}", command.name, command.description);
        }
        text.push_str("\n## Conversation goal\n/goal: show the current conversation goal.\n/goal set <text> (or /goal <text>): set a single-line goal, up to 256 characters / 1024 UTF-8 bytes.\n/goal clear: remove the goal and continue the conversation.\nThe goal stays in this process and is readable in ui.get; exit or reload clears it.\nSetting a goal does not send a request or approve task completion.\n");
        text.push_str("\nHelp: Up/Down, PgUp/PgDn, Home/End scroll; Esc returns.\n");
        let width = self.geometry.left;
        let mut lines = Vec::new();
        if !self.announcements.is_empty() {
            lines.push(self.chat_line("Announcements", PanelTone::Heading, width));
            for notice in &self.announcements {
                for line in crate::client::presentation::wrap(notice, usize::from(width)) {
                    lines.push(self.chat_line(&line, PanelTone::Normal, width));
                }
            }
        }
        lines.extend(markdown::rows(&text, width, &self.ui));
        self.chat_help_page(lines, height)
    }

    fn chat_help_page(&mut self, lines: Vec<String>, height: u16) -> Vec<String> {
        self.conversation.help_scroll = self
            .conversation
            .help_scroll
            .min(lines.len().saturating_sub(usize::from(height)));
        lines
            .into_iter()
            .skip(self.conversation.help_scroll)
            .take(usize::from(height))
            .collect()
    }
}

#[cfg(test)]
mod live_log_tests {
    use super::*;

    #[test]
    fn scoped_history_escape_and_canvas_return_keep_the_exact_thread_and_cache() {
        let mut state = super::super::super::preview_state(24, 120, &Ui::default());
        state
            .conversation
            .composer
            .set_text("keep this unsent draft");
        state.conversation.composer.left();
        let draft = state.chat_saved_draft();
        state.canvas_focus(View::Agents);
        state.canvas_focus(View::History);
        let agent = format!("agent_{}", "1".repeat(32));
        let text = "Only this verified thread".to_owned();
        // This local fixture never invokes the process capture or a native tool.
        let handle = std::sync::Arc::new(std::sync::Mutex::new(ScopedHistory {
            selector: Selector {
                client_id: "a".repeat(64),
                agent_id: Some(agent.clone()),
                main: false,
            },
            generation: Some("fixture-generation".into()),
            cursor: Some(42),
            bytes: text.len(),
            messages: std::collections::VecDeque::from(vec![text.clone()]),
            rows: vec![text],
            width: state.geometry.left,
            refreshed: Some(std::time::Instant::now()),
            truncated: false,
            error: None,
        }));
        state.conversation.scoped_history = Some(handle.clone());
        state.conversation.detail = true;
        state.conversation.content_focus = true;
        state.conversation.detail_scroll = 7;

        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Agents);
        assert!(state.conversation.scoped_history.is_none());
        state.canvas_focus(View::History);

        let restored = state.conversation.scoped_history.as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(restored, &handle));
        let view = restored.lock().unwrap();
        assert_eq!(view.selector.agent_id.as_deref(), Some(agent.as_str()));
        assert_eq!(view.cursor, Some(42));
        assert_eq!(view.messages.front().unwrap(), "Only this verified thread");
        drop(view);
        assert!(state.conversation.detail && state.conversation.content_focus);
        assert_eq!(state.conversation.detail_scroll, 7);
        assert_eq!(state.chat_saved_draft(), draft);
        assert!(state.actions.is_empty());
    }

    #[test]
    fn right_click_history_uses_the_hit_row_without_prior_left_click() {
        let mut state = super::super::super::preview_state(24, 120, &Ui::default());
        state.canvas_focus(View::Agents);
        // These keys are deliberately absent from preview's live roster. A vanished
        // member must report unavailable rather than select whoever took its index.
        let keys: Vec<String> = (1..=4)
            .map(|index| format!("agent:{}:agent_{index:032x}", "a".repeat(64)))
            .collect();
        state.conversation.drawn_agents = keys.clone();
        state.conversation.list_start = 0;
        state.conversation.list_count = keys.len();
        state.conversation.selected = 0;
        state.conversation.selected_agent = Some(keys[0].clone());
        state.conversation.detail = false;
        let first_row = state.geometry.chat_body_top() + 1;
        for (index, key) in keys.iter().enumerate() {
            state.chat_key(Key::Mouse {
                button: 2,
                x: 1,
                y: first_row + (2 * index) as u16,
                release: false,
            });
            assert_eq!(state.conversation.selected, index);
            assert_eq!(state.conversation.selected_agent.as_ref(), Some(key));
            assert_eq!(state.conversation.view, View::Agents);
            assert!(state.conversation.scoped_history.is_none());
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("no longer available")
            );
            assert!(state.actions.is_empty());
        }
        let selected = state.conversation.selected_agent.clone();
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: first_row + 8,
            release: false,
        });
        assert_eq!(state.conversation.view, View::Logs);
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Agents);
        assert_eq!(state.conversation.selected_agent, selected);
        assert!(state.conversation.scoped_history.is_none());
    }

    #[test]
    fn right_click_thread_selector_uses_real_ids_even_when_all_labels_match() {
        let client = "a".repeat(64);
        let mut seen = std::collections::BTreeSet::new();
        for index in 1..=4 {
            let agent = format!("agent_{index:032x}");
            let entry = json!({"session_role":"thread","client_id":client,"agent_id":agent,
                "name":"same display name","author":"same display name"});
            let selected = history_selector(&entry).unwrap();
            assert_eq!(selected.client_id, client);
            assert_eq!(selected.agent_id.as_deref(), Some(agent.as_str()));
            seen.insert(selected.agent_id.unwrap());
        }
        assert_eq!(seen.len(), 4);
        assert!(history_selector(&json!({"session_role":"observed","kind":"observed","client_id":client,"agent_id":format!("agent_{}","1".repeat(32)),"name":"same display name"})).is_err());
        assert!(history_selector(&json!({"client_id":client,"name":"same display name"})).is_err());
        let process = history_selector(
            &json!({"session_role":"process","client_id":client,"agent_id":"next"}),
        )
        .unwrap();
        assert!(process.agent_id.is_none());
        assert!(!process.main);
        let main = history_selector(&json!({"session_role":"main","client_id":client})).unwrap();
        assert!(main.agent_id.is_none());
        assert!(main.main);
    }

    #[test]
    fn local_history_never_labels_this_process_journal_as_a_foreign_client() {
        let local = "a".repeat(64);
        let foreign = "b".repeat(64);
        for role in ["process", "main", "thread"] {
            let mut entry = json!({"session_role":role,"client_id":local,
                "agent_id":format!("agent_{}", "1".repeat(32))});
            assert!(local_history_selector(&entry, &local).is_ok());
            entry["client_id"] = json!(foreign);
            assert!(
                local_history_selector(&entry, &local)
                    .unwrap_err()
                    .contains("authenticated attach")
            );
        }
    }
}
