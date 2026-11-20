use super::*;
use crate::client::interface::history;
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
            "Main caller history".into()
        } else {
            "Process history".into()
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
        return Err("This display label has no authenticated thread history".into());
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
        return Err("This process does not retain that client's history; use the authenticated attach view for the selected client".into());
    }
    Ok(selector)
}

fn history_entry_height(session: &history::SessionSummary) -> usize {
    2 + usize::from(session.summary.is_some())
}

impl State {
    pub(super) fn chat_thread_history_mouse(&mut self, x: u16, y: u16) {
        if x >= self.geometry.left
            || y < body_top(self.geometry.rows)
            || y >= prompt_top(self.geometry)
        {
            return;
        }
        if !self.conversation.detail && x < self.chat_agent_list_width() {
            let Some(index) = self.chat_list_hit(y) else {
                return;
            };
            let Some(key) = self.conversation.drawn_agents.get(index).cloned() else {
                return;
            };
            // Use the row the user actually clicked, even without a prior left-click.
            // chat_selected resolves this stable key and never falls back to a new occupant.
            self.conversation.selected = index;
            self.conversation.selected_agent = Some(key);
        }
        self.chat_thread_history_selected();
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
                self.conversation.history = None;
                self.conversation.detail = true;
                self.conversation.content_focus = true;
                self.conversation.detail_scroll = usize::MAX;
                self.notice = Some(format!(
                    "{title} · read-only · Esc or right-click returns to Agents"
                ));
            }
            Err(error) => self.notice = Some(format!("Thread history unavailable: {error}")),
        }
        self.dirty = true;
    }

    fn chat_scoped_history_rows(&mut self, height: u16) -> Vec<String> {
        let width = self.geometry.left;
        let shared = self
            .conversation
            .scoped_history
            .as_ref()
            .expect("scoped history")
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
                    "## Process history\nClient {}\nUnattributed records stay in this process view.",
                    view.selector.client_id
                ),
            };
            let mut rows = markdown::rows(&heading, width, &self.ui);
            if view.truncated {
                rows.extend(markdown::rows(
                    "Earlier events may be outside the retained history window.",
                    width,
                    &self.ui,
                ));
            }
            if let Some(error) = &view.error {
                rows.extend(markdown::rows(
                    &format!("History unavailable: {error}"),
                    width,
                    &self.ui,
                ));
            }
            if view.messages.is_empty() && view.error.is_none() {
                rows.extend(markdown::rows("No events with this verified identity are retained yet. Older unassigned records remain in the process history.",width,&self.ui));
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

    pub(super) fn chat_resume(&mut self) {
        self.chat_history_list(false);
    }

    fn chat_history_list(&mut self, archived: bool) {
        let filter = if archived {
            history::archive::Filter::Archived
        } else {
            history::archive::Filter::Active
        };
        match history::archive::sessions(filter) {
            Ok(sessions) => {
                self.chat_change_view(View::History);
                self.conversation.scoped_history = None;
                self.conversation.content_focus = true;
                self.conversation.sessions = sessions;
                self.conversation.history_archived = archived;
                self.conversation.selected = 0;
                self.conversation.detail = false;
                self.conversation.detail_scroll = 0;
            }
            Err(error) => self.notice = Some(format!("Could not read history: {error}")),
        }
        self.dirty = true;
    }

    pub(super) fn chat_archive_command(&mut self, command: &str, id: &str) -> Result<(), String> {
        if self.preview {
            return Err("Preview does not archive or restore saved conversations".into());
        }
        if command == "/archived" {
            if !id.is_empty() {
                return Err("Use /archived to browse saved archives".into());
            }
            self.chat_history_list(true);
            return Ok(());
        }
        if command == "/unarchive" && id.is_empty() {
            return Err("Use /unarchive CONVERSATION_ID, or /archived then Enter".into());
        }
        if self.conversation.submitting.is_some()
            || self.conversation.native_pending.is_some()
            || self.conversation.force_task.is_some()
            || self.conversation.goal.saving()
        {
            return Err(
                "Wait for the current submission to finish before archiving history".into(),
            );
        }
        let archived = command == "/archive";
        let result = history::archive::change((!id.is_empty()).then_some(id), archived)?;
        self.sync_history_owner();
        self.chat_history_list(false);
        self.notice = Some(if archived {
            format!(
                "Conversation archived · {} · /archived to restore · messages kept",
                result["id"].as_str().unwrap_or("")
            )
        } else {
            "Conversation restored · choose it in /resume to continue · no request resent".into()
        });
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_resume_selected(&mut self) {
        if self.conversation.scoped_history.is_some() {
            self.notice = Some("Thread history is read-only · Esc returns to Agents".into());
            self.dirty = true;
            return;
        }
        let sending = self.conversation.submitting.is_some()
            || self.conversation.force_task.is_some()
            || self.conversation.goal.saving()
            || items(&steering::snapshot(), "queue")
                .iter()
                .any(|item| field(item, "status") == "sending");
        if sending {
            self.notice = Some(
                "Wait for the current submission to be confirmed before switching history".into(),
            );
            return;
        }
        let Some(id) = self
            .conversation
            .sessions
            .get(self.conversation.selected)
            .map(|s| s.id.clone())
        else {
            return;
        };
        if self
            .conversation
            .sessions
            .get(self.conversation.selected)
            .is_some_and(|session| session.archived)
            && let Err(error) = history::archive::change(Some(&id), false)
        {
            self.notice = Some(format!("Could not restore archive: {error}"));
            self.dirty = true;
            return;
        }
        match history::resume(&id) {
            Ok(conversation) => {
                if self.conversation.history_archived {
                    if let Ok(sessions) = history::sessions() {
                        self.conversation.sessions = sessions;
                        self.conversation.selected = self
                            .conversation
                            .sessions
                            .iter()
                            .position(|session| session.id == id)
                            .unwrap_or(0);
                    }
                    self.conversation.history_archived = false;
                }
                for session in &mut self.conversation.sessions {
                    session.current = session.id == id;
                }
                self.chat_rehydrate_history(&conversation);
                self.conversation.history = Some(conversation.into());
                self.conversation.history_width = 0;
                self.conversation.history_rows.clear();
                self.conversation.detail = true;
                self.conversation.content_focus = false;
                self.conversation.detail_scroll = usize::MAX;
                self.notice =
                    Some("History restored · type to continue · no old request was resent".into());
            }
            Err(error) => self.notice = Some(format!("Could not resume: {error}")),
        }
        self.dirty = true;
    }

    #[cfg(all(test, unix))]
    pub(in crate::client::terminal) fn resume_history_for_test(&mut self, id: &str) {
        self.chat_resume();
        self.conversation.selected = self
            .conversation
            .sessions
            .iter()
            .position(|session| session.id == id)
            .expect("fixture history must be selectable");
        self.chat_resume_selected();
        assert_eq!(
            self.conversation
                .history
                .as_ref()
                .map(|history| history.metadata.id.as_str()),
            Some(id),
            "{:?}",
            self.notice
        );
        self.chat_change_view(View::Activity);
    }

    pub(super) fn chat_history_rows(&mut self, height: u16) -> Vec<String> {
        if self.conversation.scoped_history.is_some() {
            return self.chat_scoped_history_rows(height);
        }
        let width = self.geometry.left;
        if self.conversation.detail
            && let Some(history) = self.conversation.history.as_ref()
        {
            if self.conversation.history_width != width {
                let mut rows = Vec::new();
                for message in &history.messages {
                    let label = match message.role.as_str() {
                        "assistant" => "Agent",
                        "user" => "You",
                        "tool" => "Tool",
                        _ => "Session",
                    };
                    rows.extend(markdown::rows(&format!("## {label}"), width, &self.ui));
                    rows.extend(markdown::rows(&message.content, width, &self.ui));
                    rows.push(self.chat_line("", PanelTone::Normal, width));
                    if rows.len() > 4096 {
                        rows.drain(..rows.len() - 4096);
                    }
                }
                self.conversation.history_rows = rows;
                self.conversation.history_width = width;
            }
            self.conversation.detail_scroll = self.conversation.detail_scroll.min(
                self.conversation
                    .history_rows
                    .len()
                    .saturating_sub(usize::from(height)),
            );
            return self
                .conversation
                .history_rows
                .iter()
                .skip(self.conversation.detail_scroll)
                .take(usize::from(height))
                .cloned()
                .collect();
        }
        if std::mem::take(&mut self.conversation.history_list_stale) && !self.preview {
            let selected = self
                .conversation
                .sessions
                .get(self.conversation.selected)
                .map(|session| session.id.clone());
            let filter = if self.conversation.history_archived {
                history::archive::Filter::Archived
            } else {
                history::archive::Filter::Active
            };
            match history::archive::sessions(filter) {
                Ok(sessions) => {
                    self.conversation.selected = selected
                        .and_then(|id| sessions.iter().position(|session| session.id == id))
                        .unwrap_or_default();
                    self.conversation.sessions = sessions;
                }
                Err(error) => self.notice = Some(format!("Could not read history: {error}")),
            }
        }
        self.conversation.list_count = 0;
        let mut rows = vec![self.chat_line(
            if self.conversation.history_archived {
                "  Archived conversations · ↑↓ select · Enter restore and resume · Esc back"
            } else {
                "  Conversation history · ↑↓ select · Enter resume · /archive [ID] · Esc back"
            },
            PanelTone::Heading,
            width,
        )];
        if self.conversation.sessions.is_empty() {
            rows.push(self.chat_line(
                if self.conversation.history_archived {
                    "  No archived conversations in this workspace."
                } else {
                    "  No active history. /archived shows saved archives."
                },
                PanelTone::Muted,
                width,
            ));
            return rows;
        }
        let sessions = &self.conversation.sessions;
        let selected = self.conversation.selected.min(sessions.len() - 1);
        self.conversation.selected = selected;
        let mut start = selected;
        let mut used = history_entry_height(&sessions[selected]);
        let capacity = usize::from(height.saturating_sub(1));
        while start > 0 {
            let previous = history_entry_height(&sessions[start - 1]);
            if used + previous > capacity {
                break;
            }
            used += previous;
            start -= 1;
        }
        self.conversation.list_start = start;
        let height = usize::from(height);
        for (index, session) in sessions.iter().enumerate().skip(start) {
            if rows.len() >= height
                || (index > start && rows.len() + history_entry_height(session) > height)
            {
                break;
            }
            self.conversation.list_count += 1;
            rows.push(self.chat_line(
                &format!(
                    "{} {}",
                    if index == self.conversation.selected {
                        "›"
                    } else {
                        " "
                    },
                    panel::safe_text(&session.title)
                ),
                if index == self.conversation.selected {
                    PanelTone::Accent
                } else {
                    PanelTone::Normal
                },
                width,
            ));
            rows.push(self.chat_line(
                &format!(
                    "  {} messages · {}{}",
                    session.message_count,
                    session.id,
                    if session.archived {
                        " · archived"
                    } else if session.current {
                        " · current"
                    } else {
                        ""
                    }
                ),
                PanelTone::Muted,
                width,
            ));
            if let Some(summary) = &session.summary {
                rows.push(self.chat_line(
                    &format!(
                        "  {}",
                        panel::fit(summary, usize::from(width).saturating_sub(2))
                    ),
                    PanelTone::Muted,
                    width,
                ));
            }
        }
        rows.truncate(height);
        rows
    }

    pub(super) fn chat_history_list_hit(&self, mut row: usize) -> Option<usize> {
        let start = self.conversation.list_start;
        for (index, session) in self
            .conversation
            .sessions
            .iter()
            .enumerate()
            .skip(start)
            .take(self.conversation.list_count)
        {
            let height = history_entry_height(session);
            if row < height {
                return Some(index);
            }
            row -= height;
        }
        None
    }

    pub(super) fn chat_help_rows(&mut self, height: u16) -> Vec<String> {
        let mut text = String::from("# ssmcp help\n\n");
        if let Some(guide) = &self.guide {
            text.push_str("Online guide: ");
            text.push_str(guide.url());
            text.push_str("\n\n");
        }
        text.push_str(
            "## Write and send\nEnter: queue a requirement; continue writing the next draft.\nCtrl+J / Shift+Enter: newline. Tab / Shift+Tab: switch focus.\nAlt+Up/Down: increase/decrease visible input rows; keeps your draft and cursor.\nUp/Down: move over wrapped input lines; then history.\nEmpty input + Up: take back the latest queued request before input history. Wait for confirmation; delivered requests cannot be taken back.\nCtrl+C: clear the current input; on empty input, show an exit reminder, then press again to exit. Any other input cancels the exit reminder. Other queued requests stay unchanged.\nLeft/Right: move cursor; at a draft boundary, move to the adjacent panel. Home/End: line start/end.\nCtrl+A/E: line start/end. Ctrl+U/K: delete before/after cursor.\nCtrl+W: delete word. Backspace/Delete: edit text.\n\n## Agents and questions\nLeft at draft start: move to Agents; Up/Down select; Enter chooses. Alt+Left/Right always moves between panels.\nEsc: return without changing recipient or draft.\nShift+Up or /questions: answer pending questions; repeat for next question.\nEnter a choice number or a free answer; Enter saves it.\nEsc: question list; Delete in list skips; Esc again returns.\nQuestions never pause the agent's other work.\n\n## Shell commands\n!command: execute with your local shell in the workspace.\nUses the ASCII ! prefix; runs asynchronously with output and exit status here.\n\n## Tasks\n/command [JOB_ID]: view the complete argv and multiline script; scroll to read all lines.\n/tasks: open Tasks immediately to the right of Logs; refreshes while visible.\nUp/Down select a job; Enter opens details; Esc returns to the list or input.\nPgUp/PgDn and the mouse wheel scroll.\n/tasks cancel ID: explicitly cancel that local job.\n\n## Files, skills and paste\n@path or @\"path with spaces\": reference a workspace file/image.\n$skill-name: reference an effective project/global skill.\nPaste over 8 KiB: saved as @file; add your request after it.\nText paste limit: 4 MiB; image paste limit: 16 MiB. Input/expanded requirement limit: 16 KiB.\nCtrl+V or /paste: import clipboard text or image.\n/attach PATH: import an image as a private file reference.\n/copy: copy the latest agent reply to the clipboard.\n\n## GitHub account\n/login: prepare GitHub sign-in and open the browser.\n/login copy: copy the pending login link; /login link: display it here.\nRepeat these commands to use the same pending authorization.\nFinish GitHub sign-in and confirm the device; the current session stays connected.\n\n## Review and copy\nTab: browse content. Enter: open or toggle the selected record.\nLeft/Right: move between canvas panels; Esc: back. Wheel/PgUp/PgDn: scroll content.\nCtrl+Home: oldest retained output. Ctrl+End: return to the latest and follow new activity.\nThe footer shows how much output is below your position.\nRight-click or /logs: move to Logs; Esc/right-click returns to the previous panel.\nLogs and Tasks are adjacent canvas panels; Left/Right moves between them.\n/raw also toggles Logs. Full output stays available there.\n/diff single|split: stack changes or show old/new lines side by side.\nSplit uses a single column on narrow terminals.\n/new [chat|agents|audit|queue|questions|history]: add a panel to the right.\n/close: close this panel; the session, drafts and running tasks are kept.\nCanvas navigation does not wrap; each panel remembers its selection and scroll position.\nAlt+C, F7 or /select: freeze display and use your terminal's native mouse selection.\nCopy using your terminal's Copy shortcut or menu (often Ctrl+Shift+C).\nAlt+C/F7/Esc resumes updates and restores interface scrolling and clicks.\nOn terminals supporting it, Shift+drag selects directly without entering copy mode.\n/copy still copies only the latest agent reply.\n\n## Queued requests\n/queue: Up/Down select; Enter take back; i/I inspect without editing.\nDelete cancels a revocable request. R copies delivered text.\nFor uncertain/offline items, R opens explicit review before resending.\nTaking back waits for server acknowledgement; delivery may win.\n\n## Computer input\n/computer on: enable foreground input for this OS user's ssmcp sessions on this machine.\n/computer off: disable foreground input for all those sessions.\n/computer status: inspect the shared state; background isolated desktop/browser stays on.\nSuper+Alt+C: toggle the same foreground control. Ordinary typing does not switch it off.\nUse the desktop shortcut when another app has focus; terminals that cannot send Super need the desktop binding or /computer on/off.\nTab and Alt+C keep their completion, focus and copy actions.\n\n## Appearance and history\nF1: help. F2: next view. F3: sidebar. F4: theme.\nF5 reloads appearance. F8 saves appearance. F9 changes log category.\n/reload restarts the client; /resume reopens saved local conversation.\n/archive [ID]: archive current or named history without deleting messages.\n/archived: browse archives; Enter restores and resumes the selection.\n/unarchive ID: restore visibility without switching history or resending.\nConversation history never automatically replays tools or requests.\n/handoff [note]: export current context and prepare an @file draft.\nChoose the recipient, review the draft, then Enter to send.\n\n## Local commands\n",
        );
        for command in crate::client::composer::SLASH_COMMANDS {
            let _ = writeln!(text, "{} — {}", command.name, command.description);
        }
        text.push_str("\n## Conversation goal\n/goal: show the current conversation goal.\n/goal set <text> (or /goal <text>): set a single-line goal, up to 256 characters / 1024 UTF-8 bytes.\n/goal clear: remove the goal and continue the conversation.\nThe saved goal follows this conversation through resume/reload and is readable in ui.get.\nSetting a goal does not send a request or approve task completion.\n");
        text.push_str("\n## Native harnesses\n/pi, /cc, /codex: inspect installed capabilities.\nUse /cc TOOL {JSON} or /codex TOOL {JSON} with the discovered schema.\nUse /pi rpc {\"type\":\"get_commands\",\"args\":{}} to inspect installed commands.\nBackend capabilities and approvals stay with the installed harness.\n\nHelp: Up/Down, PgUp/PgDn, Home/End scroll; Esc returns.\n");
        let lines = markdown::rows(&text, self.geometry.left, &self.ui);
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
mod archive_tests {
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
        state.conversation.sessions = vec![history::SessionSummary {
            id: "conversation_00000000000000000000000000000000".into(),
            workspace: "/fixture".into(),
            title: "Unrelated saved conversation".into(),
            summary: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 3,
            truncated_messages: 0,
            current: false,
            archived: false,
        }];
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
        state.conversation.history = None;
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
        assert!(state.conversation.history.is_none());
        assert_eq!(state.chat_saved_draft(), draft);
        state.chat_resume_selected();
        assert!(state.notice.as_deref().unwrap().contains("read-only"));
        assert!(state.conversation.history.is_none());
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
        let first_row = body_top(state.geometry.rows) + 1;
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

    #[tokio::test]
    async fn history_picker_commands_refocus_the_existing_column_after_submit() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-history-picker-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir(&path).unwrap();
        let root = Scratch(path);
        history::with_test_history(root.0.join("history"), async {
            let mut state = State::new(24, 120, &Ui::default());
            state.canvas_focus(View::History);
            let canvas = state.conversation.canvas.snapshot();
            for (command, archived) in [("/resume", false), ("/archived", true), ("/resume", false)]
            {
                for character in command.chars() {
                    state.chat_character(character);
                }
                assert!(!state.conversation.content_focus);
                assert_eq!(state.conversation.composer.text(), command);

                state.chat_character('\r');

                assert_eq!(state.conversation.canvas.snapshot(), canvas);
                assert_eq!(state.conversation.view, View::History);
                assert!(state.conversation.content_focus, "{command}");
                assert_eq!(state.conversation.history_archived, archived);
                assert!(state.conversation.composer.is_empty());
                assert!(state.actions.is_empty());
            }
        })
        .await;
    }

    #[test]
    fn history_archive_commands_do_not_mutate_or_send_from_preview() {
        let mut state = super::super::super::preview_state(24, 120, &Ui::default());
        state.conversation.composer.set_text("unsent draft");
        let stamp = state.chat_input_stamp().unwrap();
        for command in [
            "/archive",
            "/archived",
            "/unarchive conversation_00000000000000000000000000000000",
        ] {
            assert!(state.chat_command(command).unwrap_err().contains("Preview"));
            assert_eq!(state.chat_input_stamp().unwrap(), stamp);
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn archived_history_view_explains_restore_and_keeps_selection() {
        let mut state = super::super::super::preview_state(24, 120, &Ui::default());
        state.canvas_focus(View::History);
        state.conversation.history_archived = true;
        state.canvas_focus(View::Activity);
        state.canvas_focus(View::History);
        assert!(state.conversation.history_archived);
        state.conversation.sessions = vec![history::SessionSummary {
            id: "conversation_00000000000000000000000000000000".into(),
            workspace: "/fixture".into(),
            title: "Saved original conversation".into(),
            summary: None,
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 3,
            truncated_messages: 0,
            current: false,
            archived: true,
        }];
        let rows = state.chat_history_rows(20).join("\n");
        assert!(rows.contains("Archived conversations"));
        assert!(rows.contains("Enter restore and resume"));
        assert!(rows.contains("Saved original conversation"));
        assert!(rows.contains("3 messages"));
        assert!(rows.contains("archived"));
        assert_eq!(state.conversation.selected, 0);
        assert!(state.actions.is_empty());
    }

    #[test]
    fn history_list_places_summary_with_its_exact_session_across_pages() {
        for columns in [48, 120] {
            let mut state = super::super::super::preview_state(
                24,
                columns,
                &Ui {
                    layout: Layout::Chat,
                    sidebar: false,
                    color: true,
                    ..Ui::default()
                },
            );
            state.canvas_focus(View::History);
            state.conversation.sessions = (0..3)
                .map(|index| history::SessionSummary {
                    id: format!("conversation_{index:032x}"),
                    workspace: "/fixture".into(),
                    title: if index == 1 {
                        "Custom **title**".into()
                    } else {
                        format!("History {index}")
                    },
                    summary: (index == 1).then(|| "Review [the saved context]".into()),
                    created_at_ms: 1,
                    updated_at_ms: 2,
                    message_count: index + 1,
                    truncated_messages: 0,
                    current: false,
                    archived: false,
                })
                .collect();
            let rows = state.chat_history_rows(6);
            assert_eq!(rows.len(), 6);
            assert!(rows[3].contains("Custom **title**"));
            assert!(rows[4].contains("2 messages"));
            assert!(rows[5].contains("Review [the saved context]"));
            let first_row = body_top(state.geometry.rows) + 1;
            assert_eq!(state.chat_list_hit(first_row + 4), Some(1));
            assert_eq!(state.chat_list_hit(first_row + 5), None);

            state.chat_key(Key::Down);
            let rows = state.chat_history_rows(6);
            assert!(rows[1].contains("Custom **title**"));
            assert!(rows[4].contains("› History 2"));
            assert_eq!(state.chat_list_hit(first_row + 2), Some(1));
            assert_eq!(state.chat_list_hit(first_row + 3), Some(2));
            if columns == 120 {
                assert!(rows[2].contains(&state.conversation.sessions[1].id));
            }
        }
    }
}
