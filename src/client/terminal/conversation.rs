//! Conversation-style local controls. No input is ever forwarded to a hidden PTY.
use super::*;
use crate::client::{attachments, composer::Composer, references, steering};
use serde_json::{Value, json};
mod access;
mod canvas;
mod hints;
mod history;
mod listener;
mod login;
pub(super) mod logout;
mod questions;
mod queue;
mod tasks;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum View {
    #[default]
    Activity,
    Audit,
    Agents,
    Queue,
    Questions,
    History,
    Logs,
    Tasks,
}
impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Activity => "conversation",
            Self::Audit => "code audit",
            Self::Agents => "agents",
            Self::Queue => "queue",
            Self::Questions => "questions",
            Self::History => "history",
            Self::Logs => "Logs",
            Self::Tasks => "Tasks",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Activity => Self::Audit,
            Self::Audit => Self::Queue,
            Self::Agents => Self::Activity,
            Self::Queue => Self::Questions,
            Self::Questions => Self::History,
            Self::History => Self::Logs,
            Self::Logs => Self::Tasks,
            Self::Tasks => Self::Agents,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputStamp {
    view: View,
    question: Option<String>,
    target: Option<String>,
    editing: Option<String>,
    text: String,
    cursor: usize,
    revision: u64,
}

#[derive(Default)]
pub(super) struct Conversation {
    pub(super) composer: Composer,
    pub(super) view: View,
    pub(super) content_focus: bool,
    canvas: canvas::Canvas,
    pub(super) logs_open: bool,
    pub(super) exit_requested: bool,
    tasks: tasks::TaskPanel,
    selected: usize,
    selected_agent: Option<String>,
    drawn_agents: Vec<String>,
    list_start: usize,
    list_count: usize,
    queue_hit_rows: Vec<Option<String>>,
    reference_rows: Vec<(u16, String)>,
    reference_stamp: Option<(u64, usize, u16, u16)>,
    detail: bool,
    detail_scroll: usize,
    target: Option<String>,
    editing: Option<String>,
    submitting: Option<(String, String)>,
    force_task: Option<listener::Task>,
    parked: Option<steering::SavedDraft>,
    text_pending: Vec<u8>,
    paste: Option<Vec<u8>>,
    paste_overflow: bool,
    input_task: Option<attachments::Task>,
    login_task: Option<crate::client::account::login::Task>,
    access_request: Option<u64>,
    pub(super) logout_task: Option<logout::Task>,
    pub(super) exit_message: Option<String>,
    input_stamp: Option<InputStamp>,
    input_command: Option<String>,
    restored: bool,
    question_id: Option<String>,
    answers: BTreeMap<String, Composer>,
    help_scroll: usize,
    sessions: Vec<crate::client::interface::history::SessionSummary>,
    history: Option<std::sync::Arc<crate::client::interface::history::Conversation>>,
    history_rows: Vec<String>,
    pub(super) history_width: u16,
}

impl Conversation {
    pub(super) fn snapshot(&self) -> Value {
        json!({"view":match self.view { View::Activity=>"activity",View::Audit=>"audit",View::Agents=>"agents",View::Queue=>"queue",View::Questions=>"questions",View::History=>"history",View::Logs=>"logs",View::Tasks=>"tasks" },
            "focus":if self.content_focus {"content"} else {"input"},
            "answering":self.question_id,"pending_questions":crate::client::interface::questions::pending_count(),
            "selected":self.selected,"selected_agent":self.selected_agent,"target":self.target,"editing":self.editing,
            "force_pending":self.force_task.is_some(),
            "canvas":self.canvas.snapshot(),"logs_open":self.view == View::Logs,"tasks":self.tasks.snapshot(),
            "draft":{"text":self.composer.text(),"cursor":self.composer.cursor()}})
    }
}

pub(super) fn prompt_rows(rows: u16) -> u16 {
    if rows >= 10 {
        3
    } else if rows >= 6 {
        1
    } else {
        0
    }
}
pub(super) fn body_rows(rows: u16) -> u16 {
    if rows >= 6 {
        rows.saturating_sub(prompt_rows(rows) + 3).max(1)
    } else {
        rows.saturating_sub(u16::from(rows >= 3)).max(1)
    }
}
pub(super) fn body_top(rows: u16) -> u16 {
    u16::from(rows >= 6)
}
pub(super) fn prompt_top(rows: u16) -> u16 {
    body_top(rows) + body_rows(rows)
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn message_id(value: &Value) -> &str {
    value
        .get("id")
        .or_else(|| value.get("message_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
}
fn items(value: &Value, key: &str) -> Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn recipients(value: &Value, observed: &[crate::client::render::CallerGroup]) -> Vec<Value> {
    let mut entries = vec![
        json!({"agent_id":"next","name":"Next caller","role":"default recipient","status":"first eligible tool response"}),
    ];
    entries.extend(items(value, "agents"));
    entries.extend(observed.iter().map(|caller| {
        json!({
            "kind":"observed", "targetable":false, "key":caller.key,
            "name":caller.name, "model":caller.model, "source":caller.source,
            "run_id":caller.run_id, "tool":caller.tool, "calls":caller.calls,
            "running":caller.running, "unknown":caller.unknown,
            "active":caller.running > 0, "status":caller.status,
        })
    }));
    entries
}

fn agent_key(value: &Value) -> String {
    if field(value, "kind") == "observed" {
        format!("caller:{}", field(value, "key"))
    } else {
        format!("member:{}", field(value, "agent_id"))
    }
}

impl State {
    fn chat_recipients(&mut self, data: &Value) -> Vec<Value> {
        let observed = self.audit.observed_controllers();
        recipients(data, &self.caller_tracker.groups(&observed, data))
    }

    fn chat_saved_draft(&self) -> steering::SavedDraft {
        steering::SavedDraft {
            text: self.conversation.composer.text().into(),
            cursor: self.conversation.composer.cursor(),
            target: self.conversation.target.clone(),
        }
    }

    fn chat_restore_draft(&mut self, draft: steering::SavedDraft) {
        self.conversation.composer.set_text(&draft.text);
        while self.conversation.composer.cursor() > draft.cursor
            && self.conversation.composer.left()
        {}
        self.conversation.target = draft.target;
    }

    pub(super) fn chat_data(&self) -> Value {
        if self.preview {
            json!({"connected":false,"supported":true,"preview":true,
                "agents":[
                    {"agent_id":"example-builder","name":"Builder","role":"implementation","status":"working"},
                    {"agent_id":"example-reviewer","name":"Reviewer","role":"review","status":"available"}],
                "queue":[{"id":"example-request","text":"Keep the interface simple; verify command output and source audit.","status":"queued","target":"next","revision":1}]})
        } else {
            steering::snapshot()
        }
    }

    fn chat_block_replacement(&mut self) {
        self.notice = Some(
            (if self.conversation.submitting.is_some() {
                "Waiting for queue confirmation · your current draft is preserved"
            } else if self.conversation.editing.is_some() {
                "Finish the current edit before opening another request"
            } else {
                "Previous draft is parked · finish this draft or press Esc to restore it"
            })
            .into(),
        );
        self.conversation.content_focus = false;
        self.dirty = true;
    }

    /// Local enqueue has already retained the submitted text in the outbox.
    /// Free the editor immediately so subsequent input belongs to the next draft.
    fn chat_enqueued(&mut self, id: String, text: String) {
        self.conversation.submitting = Some((id, text));
        self.conversation.composer.take_text();
        self.conversation.editing = None;
        if let Some(parked) = self.conversation.parked.take() {
            self.chat_restore_draft(parked);
        }
        self.notice = Some("Sending to queue… · the next draft stays editable".into());
        self.dirty = true;
    }

    fn chat_accepted(&mut self, id: String, text: String) {
        let text = references::source(&text).to_owned();
        self.conversation.composer.remember(&text);
        if self.conversation.view == View::History {
            self.chat_change_view(View::Activity);
        }
        let mut record = format!("\n[USER] #{id}\n");
        for line in markdown::rows(&text, self.geometry.left, &self.ui) {
            let _ = writeln!(record, "{line}");
        }
        record.push_str("  queued · waiting for the next eligible tool response\n\n");
        self.log_message("user", &text, record.as_bytes());
        if self
            .conversation
            .submitting
            .as_ref()
            .is_some_and(|(pending, submitted)| pending == &id && submitted == &text)
        {
            self.conversation.submitting = None;
        }
        // Even identical text may be a deliberately typed next request.
        self.notice = Some("Queued · delivered with the next eligible tool response".into());
        self.dirty = true;
    }

    fn chat_editable(&mut self, id: String, text: String, target: Option<String>) {
        let text = references::source(&text).to_owned();
        let same_edit = self.conversation.editing.as_ref() == Some(&id);
        if self.conversation.submitting.is_some()
            || (!same_edit
                && (self.conversation.editing.is_some() || self.conversation.parked.is_some()))
        {
            self.chat_block_replacement();
            return;
        }
        if !same_edit {
            if !self.conversation.composer.is_empty() {
                self.conversation.parked = Some(self.chat_saved_draft());
            }
            self.conversation.composer.set_text(&text);
        }
        self.conversation.target = target;
        self.conversation.editing = Some(id);
        self.conversation.content_focus = false;
        self.notice = Some("Editing · this request is held until you press Enter".into());
        self.dirty = true;
    }

    fn chat_updates(&mut self) {
        if self.conversation.tasks.poll() {
            self.dirty = true;
        }
        if !self.conversation.restored && !self.preview {
            let (text, cursor) = steering::draft();
            let context = steering::draft_context();
            self.chat_restore_draft(steering::SavedDraft {
                text,
                cursor,
                target: context.target,
            });
            self.conversation.editing = context.editing;
            self.conversation.parked = context.parked;
            self.conversation.restored = true;
        }
        if self.preview {
            return;
        }
        self.chat_poll_input();
        self.chat_poll_login();
        self.chat_poll_access();
        self.chat_poll_logout();
        self.chat_poll_force_send();
        let mut failed_notice = None;
        for event in steering::take_events() {
            match event {
                steering::UiEvent::Accepted { id, text } => {
                    self.chat_accepted(id, text);
                }
                steering::UiEvent::Editable { id, text } => {
                    let data = steering::snapshot();
                    let target = items(&data, "queue")
                        .iter()
                        .find(|item| message_id(item) == id)
                        .and_then(|record| record.get("target").and_then(Value::as_str))
                        .filter(|target| *target != "next")
                        .map(str::to_owned);
                    self.chat_editable(id, text, target);
                }
                steering::UiEvent::Failed { id, message } => {
                    failed_notice = Some(message.clone());
                    if self
                        .conversation
                        .submitting
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == &id)
                    {
                        self.conversation.submitting = None;
                    }
                    self.notice = Some(panel::safe_text(&message));
                }
            }
            self.dirty = true;
        }
        if let Some(id) = self.conversation.editing.clone() {
            let data = steering::snapshot();
            if let Some(record) = items(&data, "queue")
                .iter()
                .find(|record| message_id(record) == id)
            {
                if field(record, "status") == "cancelled" {
                    self.conversation.editing = None;
                    self.conversation.composer.take_text();
                    if let Some(parked) = self.conversation.parked.take() {
                        self.chat_restore_draft(parked);
                    }
                    self.notice = Some("Request cancelled · previous draft restored".into());
                } else if field(record, "status") == "delivered"
                    && self.conversation.submitting.is_none()
                {
                    self.conversation.editing = None;
                    self.notice = Some(
                        "This request was already delivered · current text is kept as a new draft"
                            .into(),
                    );
                }
            }
        }
        if let Some(message) = failed_notice {
            self.notice = Some(message);
        }
        self.chat_persist_draft();
    }

    fn chat_persist_draft(&self) {
        if !self.preview {
            let _ = steering::set_draft(
                self.conversation.composer.text(),
                self.conversation.composer.cursor(),
            );
            let _ = steering::set_draft_context(steering::DraftContext {
                editing: self.conversation.editing.clone(),
                target: self.conversation.target.clone(),
                parked: self.conversation.parked.clone(),
            });
        }
    }

    fn chat_change_view(&mut self, view: View) {
        self.canvas_focus(view);
        self.help = false;
        self.notice = None;
        self.dirty = true;
    }

    pub(super) fn chat_initial_logs(&mut self) {
        self.logs_open();
    }

    #[cfg(test)]
    fn chat_agents(&mut self) {
        self.chat_change_view(View::Agents);
        self.chat_select_target();
    }

    fn chat_select_target(&mut self) {
        let data = self.chat_data();
        let entries = self.chat_recipients(&data);
        let target = self.conversation.target.as_deref().unwrap_or("next");
        self.conversation.selected = entries
            .iter()
            .position(|entry| field(entry, "agent_id") == target)
            .unwrap_or(0);
        self.conversation.selected_agent = entries.get(self.conversation.selected).map(agent_key);
    }

    fn chat_command(&mut self, text: &str) -> Result<bool, String> {
        if let Some(command) = text.strip_prefix('!') {
            if self.preview {
                return Err("Preview does not execute shell commands".into());
            }
            crate::client::interface::harness::submit_shell(command)?;
            self.notice = Some("Shell command started · results appear here".into());
            return Ok(true);
        }
        if !text.starts_with('/') {
            return Ok(false);
        }
        let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        match command {
            "/exit" | "/quit" => {
                if !rest.is_empty() {
                    return Err("Use /exit or /quit without arguments".into());
                }
                self.conversation.exit_requested = true;
                self.notice = Some("Closing this client · restoring the terminal".into());
            }
            "/help" => {
                self.conversation.help_scroll = 0;
                self.help = true;
                self.conversation.content_focus = true;
            }
            "/paste" => {
                if !rest.is_empty() {
                    return Err("Use /paste without arguments to read your local clipboard".into());
                }
                self.chat_start_input(attachments::Source::Clipboard, true)?;
            }
            "/attach" => {
                self.chat_start_input(attachments::source_argument(rest)?, true)?;
            }
            "/copy" => {
                if !rest.is_empty() {
                    return Err(
                        "Use /copy without arguments to copy the latest agent message".into(),
                    );
                }
                let text = crate::client::interface::messages::last_text()
                    .ok_or("No agent message is available to copy")?;
                self.chat_start_input(attachments::Source::Copy(text), true)?;
            }
            "/login" => self.chat_login(rest)?,
            "/allow" => self.chat_allow(rest)?,
            "/logout" => self.chat_logout(rest)?,
            "/send" => self.chat_force_send(rest)?,
            "/handoff" => {
                if self.preview {
                    return Err("Preview cannot export a live conversation".into());
                }
                self.chat_start_input(
                    crate::client::attachments::Source::Handoff(rest.into()),
                    true,
                )?;
            }
            "/context" | "/memory" | "/compact" | "/fork" | "/switch" => {
                self.chat_start_input(
                    attachments::Source::Context {
                        command: command[1..].into(),
                        arguments: rest.into(),
                    },
                    true,
                )?;
            }
            "/resume" => {
                if !rest.is_empty() {
                    return Err("Use /resume to choose a saved conversation".into());
                }
                self.chat_resume();
            }
            "/pi" | "/cc" | "/codex" => {
                if self.preview {
                    return Err("Preview does not invoke installed harnesses".into());
                }
                crate::client::interface::harness::submit(&command[1..], rest)?;
                self.notice = Some("Harness request queued · results appear here".into());
            }
            "/computer" => {
                if self.preview {
                    return Err(
                        "Preview cannot grant host input; use the connected local terminal".into(),
                    );
                }
                self.notice = Some(crate::client::interface::computer::local_command(rest)?);
            }
            "/tasks" => {
                if rest.is_empty() {
                    self.chat_change_view(View::Tasks);
                    self.conversation.content_focus = true;
                    if let Err(error) = self.conversation.tasks.refresh(self.preview) {
                        self.notice = Some(error);
                    }
                } else {
                    let arguments: Vec<_> = rest.split_whitespace().collect();
                    if !matches!(arguments.as_slice(), ["cancel", id] if !id.is_empty()) {
                        return Err("Use /tasks to browse jobs, or /tasks cancel ID".into());
                    }
                    if self.preview {
                        return Err("Preview cannot cancel live tasks".into());
                    }
                    crate::client::interface::harness::submit_tasks(rest)?;
                    self.notice = Some("Task cancellation requested".into());
                }
            }
            "/queue" => self.chat_change_view(View::Queue),
            "/audit" => self.chat_change_view(View::Audit),
            "/diff" => {
                if !rest.is_empty() {
                    self.events.set_diff_mode(events::DiffMode::parse(rest)?);
                    self.dirty = true;
                }
                self.notice = Some(format!(
                    "Diff layout · {}{}",
                    self.events.diff_mode().name(),
                    if self.events.diff_mode() == events::DiffMode::Split {
                        " · narrow screens use single"
                    } else {
                        ""
                    }
                ));
            }
            "/logs" => {
                self.logs_open();
                self.notice = None;
            }
            "/raw" => {
                if !rest.is_empty() {
                    return Err(
                        "Use /raw without arguments to toggle the original transcript".into(),
                    );
                }
                self.logs_toggle();
            }
            "/new" => {
                let view = match rest {
                    "" | "chat" | "conversation" => View::Activity,
                    "agents" => View::Agents,
                    "audit" => View::Audit,
                    "queue" => View::Queue,
                    "questions" => View::Questions,
                    "history" => View::History,
                    _ => return Err("Use /new [chat|agents|audit|queue|questions|history]".into()),
                };
                self.canvas_new(view);
            }
            "/close" => {
                if !rest.is_empty() {
                    return Err("Use /close without arguments to close this view".into());
                }
                self.canvas_close();
            }
            "/questions" => self.chat_questions(),
            "/sidebar" => self.actions.push("sidebar.toggle"),
            "/theme" => {
                if rest.is_empty() {
                    self.actions.push("theme.next");
                } else {
                    let theme = crate::client::ui::Theme::parse(rest)?;
                    // Apply after releasing the terminal lock, just like function keys.
                    crate::client::interface::queue_theme(theme);
                }
            }
            "/filter" => {
                if let Some(level) = rest.strip_prefix("--") {
                    let level = match level {
                        "all" => 0,
                        "commands" => 1,
                        "errors" => 2,
                        _ => {
                            return Err("Use /filter text, or --all / --commands / --errors".into());
                        }
                    };
                    self.logs_open();
                    self.log_level = level;
                } else {
                    if rest.len() > 256 || rest.chars().any(char::is_control) {
                        return Err("A log filter accepts up to 256 bytes of plain text".into());
                    }
                    self.logs_open();
                    self.log_query = rest.into();
                }
                self.filter_offset = 0;
                self.help = false;
                self.notice = None;
            }
            "/clear" => {
                self.events.clear();
                self.logs = vt100::Parser::new(
                    nonzero(self.geometry.log_rows),
                    nonzero(self.geometry.left),
                    SCROLLBACK_ROWS,
                );
                self.log_query.clear();
                self.log_level = 0;
                self.filter_offset = 0;
                self.chat_change_view(View::Activity);
            }
            "/reload" | "/restart" => {
                if !rest.is_empty() {
                    return Err(format!(
                        "Use {command} without arguments to restart this session"
                    ));
                }
                if self.preview {
                    return Err("Preview has no connected session to restart".into());
                }
                // Preserve the parked request before the restart worker can run.
                // The command itself should never reappear as a user requirement.
                let command = self.chat_saved_draft();
                let parked = self.conversation.parked.take();
                self.conversation.composer.take_text();
                if let Some(draft) = parked.clone() {
                    self.chat_restore_draft(draft);
                }
                self.chat_persist_draft();
                if let Err(error) = steering::request_reload() {
                    self.chat_restore_draft(command);
                    self.conversation.parked = parked;
                    self.chat_persist_draft();
                    return Err(error);
                }
                self.notice =
                    Some("Preparing restart · preserving confirmed pending requests".into());
            }
            _ => return Err("Unknown command · /help lists local commands".into()),
        }
        Ok(true)
    }

    fn chat_refresh_completions(&mut self) {
        let enabled = !self.preview
            && !self.selection
            && !self.help
            && self.conversation.view != View::Questions
            && !self.conversation.content_focus;
        self.conversation.composer.refresh_completions(enabled);
    }

    fn chat_menu_visible(&self) -> bool {
        !self.conversation.content_focus
            && (self.conversation.editing.is_none()
                || self.conversation.composer.reference_active())
            && self
                .conversation
                .composer
                .menu_visible(self.geometry.left, prompt_rows(self.geometry.rows))
    }

    fn chat_reference_mouse(&mut self, x: u16, y: u16) -> bool {
        let composer = &self.conversation.composer;
        if self.help
            || self.conversation.view == View::Questions
            || x >= self.geometry.left
            || !self.chat_menu_visible()
            || self.conversation.reference_stamp
                != Some((
                    composer.revision(),
                    composer.cursor(),
                    self.geometry.rows,
                    self.geometry.left,
                ))
        {
            return false;
        }
        let Some((_, name)) = self
            .conversation
            .reference_rows
            .iter()
            .find(|(row, _)| *row == y)
        else {
            return false;
        };
        let name = name.clone();
        self.notice = self.conversation.composer.accept_reference(&name).err();
        self.conversation.content_focus = false;
        self.dirty = true;
        true
    }

    fn chat_wait_for_input(&mut self) -> bool {
        if self.conversation.input_task.is_none() {
            return false;
        }
        self.notice = Some("Preparing local input… · wait for it to finish before sending".into());
        self.dirty = true;
        true
    }

    fn chat_submit(&mut self) {
        // Exit and logout remain local while editing a queued request or awaiting acknowledgement.
        let text = self.conversation.composer.text().to_owned();
        if matches!(
            text.split_whitespace().next(),
            Some("/exit" | "/quit" | "/logout")
        ) {
            if let Err(error) = self.chat_command(text.trim()) {
                self.notice = Some(error);
            }
            self.dirty = true;
            return;
        }
        if self.conversation.logout_task.is_some() {
            self.notice = Some("Signing out · your draft is preserved".into());
            return;
        }
        if self.chat_wait_for_input() {
            return;
        }
        self.chat_refresh_completions();
        if self.conversation.composer.completion_loading() {
            self.notice = Some("Loading reference suggestions · your draft is preserved".into());
            return;
        }
        if self.chat_menu_visible()
            && self
                .conversation
                .composer
                .selected_suggestion()
                .is_some_and(|item| item.reference)
        {
            if let Err(error) = self.conversation.composer.accept_suggestion() {
                self.notice = Some(error);
            } else {
                self.notice = None;
            }
            return;
        }
        if self.conversation.submitting.is_some() {
            self.notice = Some("Waiting for queue confirmation · your draft is preserved".into());
            return;
        }
        let text = if self.chat_menu_visible() {
            self.conversation
                .composer
                .selected_suggestion()
                .expect("a visible command menu has a selection")
                .name
                .to_owned()
        } else {
            self.conversation.composer.text().to_owned()
        };
        if text.trim().is_empty() {
            return;
        }
        if text.split_whitespace().next() == Some("/send") {
            if let Err(error) = self.chat_command(&text) {
                self.notice = Some(error);
            }
            // Force-send retains the exact draft until an accepted receipt.
            return;
        }
        if self.conversation.editing.is_none() {
            match self.chat_command(&text) {
                Ok(true) => {
                    if !matches!(text.split_whitespace().next(), Some("/reload" | "/restart")) {
                        self.conversation.composer.take_text();
                    }
                    return;
                }
                Err(error) => {
                    self.notice = Some(error);
                    return;
                }
                Ok(false) => {}
            }
        }
        if self.preview {
            self.notice = Some("Preview only · connect a session to queue a requirement".into());
            return;
        }
        // The native edit API treats omitted target as "keep the previous one".
        // The composer always shows a deliberate choice, including Next caller.
        let target = Some(self.conversation.target.as_deref().unwrap_or("next"));
        let result = if let Some(id) = self.conversation.editing.clone() {
            steering::publish_edit(&id, &text, target).map(|()| id)
        } else {
            steering::submit(&text, target)
        };
        match result {
            Ok(id) => self.chat_enqueued(id, text),
            Err(error) => self.notice = Some(error),
        }
    }

    fn chat_selected(&mut self) -> Option<Value> {
        let data = self.chat_data();
        let entries = if self.conversation.view == View::Agents {
            self.chat_recipients(&data)
        } else {
            items(&data, "queue")
        };
        if self.conversation.view == View::Agents
            && let Some(selected) = &self.conversation.selected_agent
        {
            // An expired member must never silently resolve to the next row.
            return entries
                .iter()
                .find(|entry| agent_key(entry) == *selected)
                .cloned();
        }
        entries.get(self.conversation.selected).cloned()
    }

    fn chat_open(&mut self, edit: bool) {
        if edit
            && self.conversation.view == View::Queue
            && (self.conversation.editing.is_some()
                || self.conversation.submitting.is_some()
                || self.conversation.parked.is_some())
        {
            self.chat_block_replacement();
            return;
        }
        match self.conversation.view {
            View::Audit => self.audit.open(),
            View::Agents => {
                if let Some(agent) = self.chat_selected() {
                    if field(&agent, "kind") == "observed" {
                        if self.audit.open_run(field(&agent, "run_id")) {
                            self.conversation.detail = true;
                            self.conversation.content_focus = true;
                            self.notice =
                                Some("Observed call · Esc / left click returns to agents".into());
                        } else {
                            self.notice =
                                Some("This call is no longer in the retained audit history".into());
                        }
                        return;
                    }
                    self.conversation.target = (field(&agent, "agent_id") != "next")
                        .then(|| field(&agent, "agent_id").into());
                    self.chat_change_view(View::Activity);
                    self.notice = Some(format!(
                        "To {} · Enter sends · ← at draft start switches agent",
                        panel::safe_text(field(&agent, "name"))
                    ));
                }
            }
            View::Queue => {
                if let Some(message) = self.chat_selected() {
                    if edit
                        && matches!(field(&message, "status"), "queued" | "editing")
                        && !self.preview
                    {
                        if let Err(error) = steering::take_back(message_id(&message)) {
                            self.notice = Some(error);
                        }
                    } else {
                        self.conversation.detail = true;
                        self.conversation.detail_scroll = 0;
                    }
                }
            }
            View::History => self.chat_resume_selected(),
            View::Tasks => {
                self.conversation.tasks.open();
            }
            View::Logs => {}
            View::Activity => {
                if self.raw_logs {
                    self.notice = Some("Raw transcript · /logs returns to tool details".into());
                    return;
                }
                if edit {
                    self.events.toggle();
                } else {
                    self.events.open();
                }
            }
            View::Questions => {}
        }
    }

    fn chat_move(&mut self, delta: isize) {
        match self.conversation.view {
            View::Activity => {
                self.scroll_logs(-delta);
                self.events.scroll_by(delta);
            }
            View::Audit => self.audit.scroll_by(delta),
            View::Logs => self.scroll_logs(-delta),
            View::Tasks => self.conversation.tasks.scroll_by(delta),
            View::Agents if self.conversation.detail => self.audit.scroll_by(delta),
            View::Queue | View::History if self.conversation.detail => {
                self.conversation.detail_scroll =
                    self.conversation.detail_scroll.saturating_add_signed(delta);
            }
            View::History => {
                self.conversation.selected = self
                    .conversation
                    .selected
                    .saturating_add_signed(delta)
                    .min(self.conversation.sessions.len().saturating_sub(1));
            }
            view => {
                let data = self.chat_data();
                let entries = if view == View::Agents {
                    self.chat_recipients(&data)
                } else {
                    items(&data, "queue")
                };
                if view == View::Agents
                    && let Some(selected) = &self.conversation.selected_agent
                    && let Some(index) = entries
                        .iter()
                        .position(|entry| agent_key(entry) == *selected)
                {
                    self.conversation.selected = index;
                }
                self.conversation.selected = self
                    .conversation
                    .selected
                    .saturating_add_signed(delta)
                    .min(entries.len().saturating_sub(1));
                if view == View::Agents {
                    self.conversation.selected_agent =
                        entries.get(self.conversation.selected).map(agent_key);
                }
            }
        }
    }

    fn chat_page(&mut self, delta: isize) {
        if self.conversation.content_focus
            && self.conversation.view == View::Activity
            && !self.raw_logs
            && self.events.scroll_selected_panel(delta)
        {
            return;
        }
        self.chat_move(delta);
    }

    fn chat_key(&mut self, key: Key) {
        self.events.set_scope(events::EventScope::Conversation);
        if matches!(key, Key::Interrupt) {
            request_interrupt();
            return;
        }
        self.chat_refresh_completions();
        if matches!(key, Key::Select) || (self.selection && matches!(key, Key::Escape)) {
            self.selection = !self.selection;
            self.notice = self.selection.then(|| {
                "Copy mode · drag to select · use your terminal Copy · F7/Esc back".into()
            });
            self.dirty = true;
            return;
        }
        if self.selection {
            return;
        }
        if let Key::Mouse {
            button: 0,
            x,
            y,
            release: false,
        } = key
            && self.chat_reference_mouse(x, y)
        {
            return;
        }
        if let Key::Mouse {
            button,
            release: false,
            ..
        } = key
            && button & 3 == 2
            && button & (32 | 64) == 0
        {
            self.logs_toggle();
            return;
        }
        if matches!(key, Key::CanvasLeft | Key::CanvasRight) {
            self.canvas_move(if matches!(key, Key::CanvasLeft) {
                -1
            } else {
                1
            });
            return;
        }
        if self.conversation.logs_open && !self.help {
            let scrolling = match key {
                Key::AuditUp | Key::AuditDown | Key::Home | Key::End => {
                    self.conversation.content_focus
                }
                Key::Up | Key::Down | Key::HistoryStart | Key::HistoryEnd | Key::Filter => true,
                Key::Mouse {
                    button,
                    release: false,
                    ..
                } => button & 64 != 0,
                _ => false,
            };
            if scrolling {
                self.logs_key(key);
                return;
            }
        }
        if !self.help && matches!(key, Key::Back | Key::Open) {
            let editor = if self.conversation.view == View::Questions {
                self.question_editor()
            } else {
                Some(&self.conversation.composer)
            };
            let at_edge = editor.is_none_or(|editor| {
                if matches!(key, Key::Back) {
                    editor.cursor() == 0
                } else {
                    editor.cursor() == editor.text().len()
                }
            });
            if self.conversation.content_focus || at_edge {
                self.canvas_move(if matches!(key, Key::Back) { -1 } else { 1 });
                return;
            }
        }
        if matches!(key, Key::Questions) {
            self.chat_questions();
            return;
        }
        if self.help {
            let page = usize::from(body_rows(self.geometry.rows).saturating_sub(1).max(1));
            match key {
                Key::Help | Key::Escape => self.chat_escape(),
                Key::AuditUp => {
                    self.conversation.help_scroll = self.conversation.help_scroll.saturating_sub(1)
                }
                Key::AuditDown => {
                    self.conversation.help_scroll = self.conversation.help_scroll.saturating_add(1)
                }
                Key::Up => {
                    self.conversation.help_scroll =
                        self.conversation.help_scroll.saturating_sub(page)
                }
                Key::Down => {
                    self.conversation.help_scroll =
                        self.conversation.help_scroll.saturating_add(page)
                }
                Key::Home | Key::HistoryStart => self.conversation.help_scroll = 0,
                Key::End | Key::HistoryEnd => self.conversation.help_scroll = usize::MAX,
                Key::Mouse {
                    button,
                    release: false,
                    ..
                } if button & 64 != 0 => {
                    self.conversation.help_scroll = if button & 1 == 0 {
                        self.conversation.help_scroll.saturating_sub(3)
                    } else {
                        self.conversation.help_scroll.saturating_add(3)
                    };
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        if self.conversation.view == View::Questions {
            self.question_key(key);
            return;
        }
        if self.conversation.view == View::Agents
            && self.conversation.content_focus
            && matches!(key, Key::Delete | Key::Newline)
        {
            self.chat_change_view(View::Activity);
        }
        let content = self.conversation.content_focus;
        match key {
            Key::Help => {
                self.conversation.help_scroll = 0;
                self.help = !self.help;
                self.conversation.content_focus = self.help;
            }
            Key::Layout => match self.conversation.view.next() {
                View::History => self.chat_resume(),
                View::Questions => self.chat_questions(),
                view => self.chat_change_view(view),
            },
            Key::Sidebar => self.actions.push("sidebar.toggle"),
            Key::Theme => self.actions.push("theme.next"),
            Key::Reload => self.actions.push("reload"),
            Key::Save => self.actions.push("save"),
            Key::Filter => {
                self.logs_open();
                self.log_level = (self.log_level + 1) % 3;
                self.filter_offset = 0;
            }
            Key::FocusBack => self.conversation.content_focus = !content,
            Key::Up => self.chat_page(
                -(isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::Down => self.chat_page(
                (isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::HistoryStart if self.conversation.view == View::Tasks => {
                self.conversation
                    .tasks
                    .scroll_by(isize::MIN.saturating_add(1));
            }
            Key::HistoryEnd if self.conversation.view == View::Tasks => {
                self.conversation.tasks.scroll_by(isize::MAX);
            }
            Key::HistoryStart => {
                self.scroll_logs(isize::MAX);
                self.events.scroll_by(isize::MIN.saturating_add(1));
                self.chat_change_view(View::Activity);
            }
            Key::HistoryEnd => {
                self.scroll_logs(isize::MIN);
                self.events.latest();
                self.chat_change_view(View::Activity);
            }
            Key::AuditUp
                if content && self.conversation.view == View::Activity && !self.raw_logs =>
            {
                self.events.select(-1)
            }
            Key::AuditDown
                if content && self.conversation.view == View::Activity && !self.raw_logs =>
            {
                self.events.select(1)
            }
            Key::AuditUp if content && self.conversation.view == View::Tasks => {
                self.conversation.tasks.move_selection(-1);
            }
            Key::AuditDown if content && self.conversation.view == View::Tasks => {
                self.conversation.tasks.move_selection(1);
            }
            Key::AuditUp if content => self.chat_move(-1),
            Key::AuditDown if content => self.chat_move(1),
            Key::AuditUp => {
                if self.chat_menu_visible() {
                    self.conversation.composer.move_suggestion(-1);
                    self.dirty = true;
                    return;
                }
                if self.conversation.submitting.is_some() {
                    self.notice =
                        Some("Waiting for queue confirmation · your draft is preserved".into());
                    return;
                }
                if !self.conversation.composer.up(self.geometry.left) {
                    if self.conversation.editing.is_none() && self.conversation.parked.is_some() {
                        self.chat_block_replacement();
                        return;
                    }
                    let data = self.chat_data();
                    let queued = items(&data, "queue")
                        .into_iter()
                        .rev()
                        .find(|entry| field(entry, "status") == "queued");
                    if self.conversation.editing.is_none()
                        && !self.preview
                        && let Some(queued) = queued
                    {
                        if let Err(error) = steering::take_back(message_id(&queued)) {
                            self.notice = Some(error);
                        }
                    } else if self.conversation.editing.is_none() {
                        self.conversation.composer.history_previous();
                    }
                }
            }
            Key::AuditDown => {
                if self.chat_menu_visible() {
                    self.conversation.composer.move_suggestion(1);
                    self.dirty = true;
                    return;
                }
                if !self.conversation.composer.down(self.geometry.left)
                    && self.conversation.editing.is_none()
                {
                    if self.conversation.parked.is_some() {
                        self.chat_block_replacement();
                    } else {
                        self.conversation.composer.history_next();
                    }
                }
            }
            Key::Back if self.conversation.view == View::Agents && content => self.canvas_move(-1),
            Key::Back if self.conversation.view == View::Activity && content => {
                if self.raw_logs {
                    self.notice = Some("Raw transcript · /logs returns to tool details".into());
                } else if !self.events.back() {
                    self.canvas_move(-1);
                }
            }
            Key::Back if content => {
                if self.conversation.detail {
                    self.audit.back();
                    self.conversation.detail = false;
                    self.conversation.detail_scroll = 0;
                } else {
                    self.canvas_move(-1);
                }
            }
            Key::Back if self.conversation.composer.cursor() == 0 => self.canvas_move(-1),
            Key::Back => {
                self.conversation.composer.left();
            }
            Key::Open if content => self.chat_open(false),
            Key::Open
                if self.conversation.composer.cursor()
                    == self.conversation.composer.text().len() =>
            {
                self.canvas_move(1)
            }
            Key::Open => {
                self.conversation.composer.right();
            }
            Key::Home if content => self.chat_page(isize::MIN.saturating_add(1)),
            Key::End if content => self.chat_page(isize::MAX),
            Key::Home => {
                self.conversation.composer.home();
            }
            Key::End => {
                self.conversation.composer.end();
            }
            Key::Delete if content && self.conversation.view == View::Queue => {
                if let Some(message) = self.chat_selected()
                    && let Err(error) = steering::cancel(message_id(&message))
                {
                    self.notice = Some(error);
                }
            }
            Key::Delete if content && self.conversation.view == View::Tasks => {
                self.notice = Some("Cancel a task explicitly with /tasks cancel ID".into());
            }
            Key::Delete => {
                if self.conversation.view == View::Logs {
                    self.conversation.content_focus = false;
                }
                self.conversation.composer.delete();
            }
            Key::Newline => {
                if matches!(self.conversation.view, View::Tasks | View::Logs) {
                    self.conversation.content_focus = false;
                }
                self.conversation.composer.insert("\n");
            }
            Key::Escape => self.chat_escape(),
            Key::Mouse {
                button,
                x,
                y,
                release,
            } if !release && x < self.geometry.left => {
                if button & 64 != 0 {
                    let delta = if button & 1 == 0 { -3 } else { 3 };
                    if y >= body_top(self.geometry.rows) && y < prompt_top(self.geometry.rows) {
                        let inside_panel = self.conversation.view == View::Activity
                            && !self.raw_logs
                            && self
                                .events
                                .scroll_panel_at(y - body_top(self.geometry.rows), delta);
                        if !inside_panel {
                            self.chat_move(delta);
                        }
                    }
                } else if y > prompt_top(self.geometry.rows) {
                    self.conversation.content_focus = false;
                } else {
                    self.conversation.content_focus = true;
                    if self.conversation.view == View::Tasks {
                        if let Some(row) = y.checked_sub(body_top(self.geometry.rows))
                            && self.conversation.tasks.mouse_select(row)
                            && button == 0
                        {
                            self.conversation.tasks.open();
                        }
                        self.dirty = true;
                        return;
                    }
                    if self.conversation.view == View::Activity
                        && !self.raw_logs
                        && !self.events.is_empty()
                    {
                        if let Some(row) = y.checked_sub(body_top(self.geometry.rows))
                            && self.events.mouse_select(row)
                        {
                            if button == 0 {
                                self.events.toggle();
                            } else if button == 2 {
                                self.events.open();
                            }
                        }
                        self.dirty = true;
                        return;
                    }
                    if matches!(
                        self.conversation.view,
                        View::Agents | View::Queue | View::History
                    ) && !self.conversation.detail
                    {
                        if self.conversation.view == View::Agents
                            && x >= self.chat_agent_list_width()
                        {
                            return;
                        }
                        let Some(index) = self.chat_list_hit(y) else {
                            self.dirty = true;
                            return;
                        };
                        self.conversation.selected = index;
                        self.conversation.selected_agent = if self.conversation.view == View::Agents
                        {
                            let Some(key) = self.conversation.drawn_agents.get(index) else {
                                return;
                            };
                            Some(key.clone())
                        } else {
                            None
                        };
                    }
                    if button == 2 {
                        self.chat_open(false);
                    } else if button == 0
                        && (self.conversation.view == View::Audit
                            || (self.conversation.view == View::Agents && self.conversation.detail))
                    {
                        self.audit.back();
                        self.conversation.detail = false;
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_escape(&mut self) {
        self.dirty = true;
        if self.conversation.view == View::Questions && !self.help {
            self.question_key(Key::Escape);
            return;
        }
        if !self.help && (self.chat_menu_visible() || self.conversation.composer.reference_active())
        {
            self.conversation.composer.dismiss_suggestions();
            self.notice = None;
            return;
        }
        if self.help {
            self.help = false;
            self.conversation.content_focus = false;
            self.notice = None;
            return;
        }
        if self.conversation.logs_open {
            self.logs_close_restore();
            return;
        }
        if self.conversation.view == View::Tasks {
            self.conversation.content_focus = self.conversation.tasks.back();
            self.notice = None;
            return;
        }
        if self.conversation.view == View::History {
            if self.conversation.detail {
                self.conversation.detail = false;
                self.conversation.content_focus = true;
                self.notice = None;
            } else {
                self.chat_change_view(View::Activity);
            }
            return;
        }
        if self.conversation.view == View::Agents {
            if self.conversation.detail {
                self.audit.back();
                self.conversation.detail = false;
                self.conversation.content_focus = true;
                self.notice = None;
            } else {
                self.chat_change_view(View::Activity);
            }
            return;
        }
        if !self.conversation.content_focus
            && !self.conversation.detail
            && self.conversation.editing.is_none()
            && self.conversation.submitting.is_none()
            && let Some(parked) = self.conversation.parked.take()
        {
            self.chat_restore_draft(parked);
            self.notice = Some("Previous draft restored · recipient and cursor preserved".into());
            return;
        }
        self.help = false;
        if self.conversation.detail {
            self.conversation.detail = false;
            if self.conversation.view == View::Agents {
                self.audit.back();
            }
        } else {
            self.audit.back();
            self.conversation.content_focus = false;
        }
        self.notice = None;
    }

    fn chat_character(&mut self, character: char) {
        self.chat_refresh_completions();
        if self.selection {
            if character == '\u{1b}' {
                self.chat_key(Key::Select);
            }
            return;
        }
        if self.help {
            if character == '\u{1b}' {
                self.chat_escape();
            } else if !character.is_control() {
                self.help = false;
                self.conversation.content_focus = false;
                self.notice = None;
                if !self.conversation.composer.insert(&character.to_string()) {
                    self.notice = Some("Draft limit reached · 16 KiB".into());
                }
                self.dirty = true;
            }
            return;
        }
        if character == '\u{16}' {
            if let Err(error) = self.chat_start_input(attachments::Source::Clipboard, false) {
                self.notice = Some(error);
            }
            self.dirty = true;
            return;
        }
        if self.conversation.view == View::Questions {
            self.question_character(character);
            return;
        }
        if self.conversation.view == View::Agents
            && self.conversation.content_focus
            && matches!(
                character,
                '\n' | '\u{1}' | '\u{5}' | '\u{15}' | '\u{b}' | '\u{17}' | '\u{7f}' | '\u{8}'
            )
        {
            self.chat_change_view(View::Activity);
        }
        if matches!(self.conversation.view, View::Tasks | View::Logs)
            && self.conversation.content_focus
            && matches!(
                character,
                '\n' | '\u{1}' | '\u{5}' | '\u{15}' | '\u{b}' | '\u{17}' | '\u{7f}' | '\u{8}'
            )
        {
            self.conversation.content_focus = false;
        }
        match character {
            'i' | 'I'
                if self.conversation.content_focus && self.conversation.view == View::Queue =>
            {
                self.chat_open(false);
            }
            'r' | 'R'
                if self.conversation.content_focus && self.conversation.view == View::Queue =>
            {
                if let Some(message) = self.chat_selected() {
                    let recoverable = matches!(
                        field(&message, "status"),
                        "uncertain" | "retarget-required" | "offline"
                    );
                    let same_retarget = recoverable
                        && self.conversation.editing.as_deref() == Some(message_id(&message));
                    if self.conversation.submitting.is_some()
                        || (self.conversation.parked.is_some() && !same_retarget)
                        || self
                            .conversation
                            .editing
                            .as_ref()
                            .is_some_and(|editing| !recoverable || editing != message_id(&message))
                    {
                        self.chat_block_replacement();
                        return;
                    }
                    if recoverable && !self.preview {
                        if let Err(error) = steering::retarget(message_id(&message), None) {
                            self.notice = Some(error);
                        }
                    } else {
                        if !self.conversation.composer.is_empty() {
                            self.conversation.parked = Some(self.chat_saved_draft());
                        }
                        self.conversation
                            .composer
                            .set_text(references::source(field(&message, "text")));
                        self.conversation.editing = None;
                        self.conversation.target = None;
                        self.conversation.content_focus = false;
                        self.notice = Some(
                            (if self.conversation.parked.is_some() {
                                "Copied to a new draft · Enter sends · Esc restores previous draft"
                            } else {
                                "Copied to a new draft · Enter sends a new request"
                            })
                            .into(),
                        );
                    }
                }
            }
            '\u{1b}' => self.chat_escape(),
            '\t' => {
                if self.chat_menu_visible() {
                    if let Err(error) = self.conversation.composer.accept_suggestion() {
                        self.notice = Some(error);
                    }
                } else if self.conversation.composer.completion_loading() {
                    self.notice =
                        Some("Loading reference suggestions · your draft is preserved".into());
                } else {
                    self.conversation.content_focus = !self.conversation.content_focus;
                }
            }
            '\u{f}' => {
                self.chat_change_view(if self.conversation.view == View::Audit {
                    View::Activity
                } else {
                    View::Audit
                });
            }
            '\r' if self.conversation.content_focus => self.chat_open(true),
            '\r' => {
                if self.conversation.composer.text()[..self.conversation.composer.cursor()]
                    .ends_with('\\')
                {
                    self.conversation.composer.backspace();
                    self.conversation.composer.insert("\n");
                } else {
                    self.chat_submit();
                }
            }
            '\n' => {
                self.conversation.composer.insert("\n");
            }
            '\u{1}' => {
                self.conversation.composer.home();
            }
            '\u{5}' => {
                self.conversation.composer.end();
            }
            '\u{15}' => {
                self.conversation.composer.clear_before();
            }
            '\u{b}' => {
                self.conversation.composer.clear_after();
            }
            '\u{17}' => {
                self.conversation.composer.delete_word();
            }
            '\u{7f}' | '\u{8}' => {
                self.conversation.composer.backspace();
            }
            '?' if self.conversation.composer.is_empty() && !self.conversation.content_focus => {
                self.help = !self.help;
            }
            ch if !ch.is_control() => {
                if self.conversation.view == View::Agents {
                    self.chat_change_view(View::Activity);
                }
                self.conversation.content_focus = false;
                if !self.conversation.composer.insert(&ch.to_string()) {
                    self.notice = Some("Draft limit reached · 16 KiB".into());
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_input_stamp(&self) -> Option<InputStamp> {
        let editor = if self.conversation.view == View::Questions {
            self.question_editor()?
        } else {
            &self.conversation.composer
        };
        Some(InputStamp {
            view: self.conversation.view,
            question: if self.conversation.view == View::Questions {
                self.conversation.question_id.clone()
            } else {
                None
            },
            target: self.conversation.target.clone(),
            editing: self.conversation.editing.clone(),
            text: editor.text().into(),
            cursor: editor.cursor(),
            revision: editor.revision(),
        })
    }

    /// Start exactly one local input job; successful commands are cleared by chat_submit.
    fn chat_start_input(
        &mut self,
        source: attachments::Source,
        command: bool,
    ) -> Result<(), String> {
        if self.preview {
            return Err(
                "Preview cannot read or write the system clipboard or import local files".into(),
            );
        }
        if self.selection {
            return Err("Leave copy mode before importing input".into());
        }
        if self.conversation.input_task.is_some() {
            return Err("A local input job is still running; your draft is preserved".into());
        }
        let copying = matches!(&source, attachments::Source::Copy(_));
        let context = matches!(&source, attachments::Source::Context { .. });
        let inserts = !copying && !context;
        if self.conversation.view == View::Questions && inserts {
            self.question_paste_len()?;
        }
        let mut stamp = self
            .chat_input_stamp()
            .ok_or("No answer editor is open; press Esc to return to your draft")?;
        let used = if command { 0 } else { stamp.text.len() };
        if inserts
            && attachments::REFERENCE_RESERVE
                > crate::client::composer::MAX_DRAFT_BYTES.saturating_sub(used)
        {
            return Err(
                "Not enough draft space for an input reference; your draft was preserved".into(),
            );
        }
        let original = command.then(|| stamp.text.clone());
        if command {
            stamp.text.clear();
            stamp.cursor = 0;
            stamp.revision = stamp.revision.wrapping_add(1);
        }
        let task = attachments::Task::start(source)?;
        self.conversation.input_task = Some(task);
        self.conversation.input_stamp = Some(stamp);
        self.conversation.input_command = original;
        self.notice = Some(
            if copying {
                "Copying the latest agent message…"
            } else if context {
                "Reading local context…"
            } else {
                "Preparing local input… · keep this draft unchanged to insert the result"
            }
            .into(),
        );
        self.dirty = true;
        Ok(())
    }

    fn chat_poll_input(&mut self) {
        let Some(result) = self
            .conversation
            .input_task
            .as_mut()
            .and_then(attachments::Task::poll)
        else {
            return;
        };
        self.conversation.input_task = None;
        let stamp = self.conversation.input_stamp.take();
        let original = self.conversation.input_command.take();
        self.chat_finish_input(stamp.as_ref(), original.as_deref(), result);
    }

    fn chat_finish_input(
        &mut self,
        expected: Option<&InputStamp>,
        original_command: Option<&str>,
        result: Result<attachments::Loaded, String>,
    ) {
        let unchanged = expected.is_some() && self.chat_input_stamp().as_ref() == expected;
        match result {
            Ok(attachments::Loaded::Copied) => {
                self.notice = Some("Latest agent message copied to the system clipboard".into());
            }
            Ok(attachments::Loaded::Context(text)) => {
                let mut record = String::from("\n[CONTEXT]\n");
                for line in markdown::rows(&text, self.geometry.left, &self.ui) {
                    let _ = writeln!(record, "{line}");
                }
                record.push('\n');
                self.log_event(
                    "context",
                    &json!({"type":"ssmcp.message", "author":"Context", "text":text}),
                    record.as_bytes(),
                );
                self.notice = Some(
                    "Local context updated · source history and queue were not replayed".into(),
                );
            }
            Ok(attachments::Loaded::Files {
                text,
                description,
                mut files,
            }) if unchanged => {
                self.notice = match self.chat_paste_text(&text) {
                    Ok(()) => {
                        files.keep();
                        Some(format!("{description} · Enter sends when ready"))
                    }
                    Err(error) => {
                        if let Some(command) = original_command {
                            self.conversation.composer.set_text(command);
                        }
                        Some(format!("{error} · imported copies were rolled back"))
                    }
                };
            }
            Ok(attachments::Loaded::Files { .. }) => {
                self.notice = Some("Draft or recipient changed · imported copies were rolled back; source files are unchanged".into());
            }
            Ok(attachments::Loaded::Text(text)) if unchanged => {
                self.notice = match self.chat_paste_text(&text) {
                    Ok(()) => Some("Clipboard text inserted · Enter sends when ready".into()),
                    Err(error) => Some(error),
                };
            }
            Ok(attachments::Loaded::Reference {
                reference,
                description,
            }) if unchanged => {
                self.notice = match self.chat_paste_text(&format!("\n{reference}\n")) {
                    Ok(()) => Some(format!("{description} · Enter sends when ready")),
                    Err(error) => Some(format!("{error} · recover with {reference}")),
                };
            }
            Ok(attachments::Loaded::Text(_)) => {
                self.notice = Some("Draft or recipient changed · clipboard text was not inserted; paste again when ready".into());
            }
            Ok(attachments::Loaded::Reference { reference, .. }) => {
                self.notice = Some(format!(
                    "Draft or recipient changed · input retained at {reference}; insert it when ready"
                ));
            }
            Err(error) => {
                if unchanged && let Some(command) = original_command {
                    self.conversation.composer.set_text(command);
                }
                self.notice = Some(format!("Local input failed: {error}"));
            }
        }
        self.dirty = true;
    }

    fn chat_paste_text(&mut self, text: &str) -> Result<(), String> {
        let large = text.len() > 8 * 1024;
        let question = self.conversation.view == View::Questions;
        let used = if question {
            self.question_paste_len()?
        } else {
            self.conversation.composer.text().len()
        };
        let required = if large {
            references::paste::REFERENCE_BYTES + 2
        } else {
            text.len()
        };
        if required > crate::client::composer::MAX_DRAFT_BYTES.saturating_sub(used) {
            return Err("Not enough draft space for this paste; your draft was preserved".into());
        }
        if large {
            if text.len() > references::paste::MAX_PASTE_BYTES {
                return Err("Paste exceeds 4 MiB; save it as a file and use @path".into());
            }
            // File creation/fsync happens on the shared async input job. The
            // completion stamp protects edits, answers and recipient changes.
            return self.chat_start_input(attachments::Source::Text(text.to_owned()), false);
        }
        let accepted = if question {
            self.question_paste(text)
        } else {
            self.conversation.composer.paste(text)
        };
        if !accepted {
            return Err("Paste could not be inserted; your draft was preserved".into());
        }
        Ok(())
    }

    pub(super) fn chat_discard_image_paste(&mut self) {
        if self.conversation.paste.take().is_none() {
            return;
        }
        self.notice = Some(if self.conversation.paste_overflow {
            "Image paste exceeds 16 MiB; draft preserved. Use a smaller image or /attach PATH."
        } else {
            "Incomplete image paste discarded; draft preserved. Copy the original image and use Ctrl+V or /paste, or /attach PATH."
        }.into());
        self.conversation.paste_overflow = false;
    }

    fn chat_output(&mut self, output: &[u8]) {
        if output.is_empty() {
            return;
        }
        if output == b"\x1b[200~" && self.conversation.paste.is_none() {
            self.conversation.paste = Some(Vec::new());
            self.conversation.paste_overflow = false;
            return;
        }
        if output == b"\x1b[201~" && !self.keyboard.pasting {
            if let Some(paste) = self.conversation.paste.take() {
                if !paste.is_empty() {
                    if self.conversation.view == View::Agents {
                        self.chat_change_view(View::Activity);
                    }
                    if self.help {
                        self.help = false;
                        self.conversation.content_focus = false;
                    }
                }
                let result = if self.selection {
                    Err("Leave copy mode before pasting".into())
                } else if self.conversation.paste_overflow {
                    Err(
                        "Paste exceeds its limit (4 MiB text / 16 MiB image); use /attach PATH"
                            .into(),
                    )
                } else {
                    match attachments::paste_source(paste) {
                        Ok(attachments::Source::Text(text)) => self.chat_paste_text(&text),
                        Ok(source) => self.chat_start_input(source, false),
                        Err(error) => Err(error),
                    }
                };
                if let Err(error) = result {
                    self.notice = Some(error);
                }
            }
            self.dirty = true;
            return;
        }
        if let Some(paste) = &mut self.conversation.paste {
            if !self.conversation.paste_overflow
                && paste.len().saturating_add(output.len())
                    <= attachments::paste_limit(paste, output)
            {
                paste.extend_from_slice(output);
            } else {
                self.conversation.paste_overflow = true;
            }
            return;
        }
        self.conversation.text_pending.extend_from_slice(output);
        match std::str::from_utf8(&self.conversation.text_pending) {
            Ok(text) => {
                let chars: Vec<char> = text.chars().collect();
                self.conversation.text_pending.clear();
                for ch in chars {
                    self.chat_character(ch);
                }
            }
            Err(error) if error.error_len().is_some() => {
                self.conversation.text_pending.clear();
            }
            Err(_) => {}
        }
    }

    pub(super) fn chat_keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        // Run before accepting the next input, including a real Ctrl-C after a
        // truncated image, and discard only the unfinished paste, never the draft.
        if self.keyboard.image_paste_expired(now) {
            self.discard_image_paste();
        }
        if !bytes.is_empty() && self.keyboard.pasting {
            self.keyboard.paste_last_input = Some(now);
        }
        self.chat_updates();
        // Keep text and navigation in their original order, including coalesced reads.
        for byte in bytes {
            let (output, keys) = self.keyboard.feed(&[*byte], now, false, true, false);
            if !self.preview && (!output.is_empty() || !keys.is_empty()) {
                crate::client::interface::computer::local_input();
            }
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        if flush {
            let (output, keys) = self.keyboard.feed(&[], now, true, true, false);
            if !self.preview && (!output.is_empty() || !keys.is_empty()) {
                crate::client::interface::computer::local_input();
            }
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        self.chat_refresh_completions();
        self.chat_persist_draft();
        Vec::new()
    }

    /// Reuse one bounded row parser; serialize cells, never cursor/erase controls.
    fn chat_fill_body(&self, rows: &mut [String], width: u16) {
        let mut parser = vt100::Parser::new(nonzero(1), nonzero(width), 0);
        for row in rows {
            parser.process(b"\x1b[0m\x1b[2J\x1b[H");
            parser.process(row.as_bytes());
            *row = screen_row(parser.screen(), 0, width, &self.ui);
        }
    }

    fn chat_line(&self, text: &str, tone: PanelTone, width: u16) -> String {
        let plain = fit(text, width);
        let row = ztui::Row::measured(&plain, |text| text.width());
        let mut padded =
            String::with_capacity(plain.len() + usize::from(width).saturating_sub(row.columns()));
        row.write_padded(&mut padded, width)
            .expect("fit bounds the row and writing into String cannot fail");
        self.tone(tone, &padded)
    }

    fn chat_input_line(&self, text: &str, width: u16) -> String {
        let text = fit(text, width);
        let padded = format!(
            "{text}{}",
            " ".repeat(usize::from(width).saturating_sub(text.width()))
        );
        if self.ui.color {
            format!(
                "\x1b[{};{}m{padded}\x1b[0m",
                self.ui.input_background(),
                self.ui.color("muted")
            )
        } else {
            padded
        }
    }

    fn chat_list_window(&mut self, count: usize, height: u16) -> std::ops::Range<usize> {
        self.conversation.selected = self.conversation.selected.min(count.saturating_sub(1));
        let capacity = usize::from(height.saturating_sub(1) / 2).max(1);
        let start = self.conversation.selected.saturating_sub(capacity - 1);
        let count = if height > 1 {
            count.saturating_sub(start).min(capacity)
        } else {
            0
        };
        self.conversation.list_start = start;
        self.conversation.list_count = count;
        start..start + count
    }

    fn chat_agent_list_width(&self) -> u16 {
        let width = self.geometry.left;
        let sidebar = PanelLayout::configured(
            self.geometry.cols,
            self.geometry.rows,
            self.ui.sidebar && !self.ui.panels.is_empty(),
            self.ui.sidebar_width,
        )
        .sidebar
        .is_some();
        if !sidebar && width >= 76 {
            (width.saturating_mul(2) / 5).min(40)
        } else {
            width
        }
    }

    fn chat_agent_details(
        &self,
        selected: Option<&Value>,
        data: &Value,
    ) -> Vec<(String, PanelTone)> {
        let Some(selected) = selected else {
            return vec![
                ("Selection unavailable".into(), PanelTone::Heading),
                ("".into(), PanelTone::Normal),
                ("The selected entry left".into(), PanelTone::Normal),
                ("Your recipient has not changed".into(), PanelTone::Muted),
                ("↑ / ↓ choose another entry".into(), PanelTone::Muted),
            ];
        };
        let observed = field(selected, "kind") == "observed";
        let next = field(selected, "agent_id") == "next";
        let mut details = vec![
            (
                (if observed {
                    "Observed caller"
                } else if next {
                    "Message delivery"
                } else {
                    "Agent"
                })
                .into(),
                PanelTone::Heading,
            ),
            ("".into(), PanelTone::Normal),
            (field(selected, "name").into(), PanelTone::Accent),
        ];
        if observed {
            let running = selected["running"].as_u64().unwrap_or(0);
            let calls = selected["calls"].as_u64().unwrap_or(0);
            let tone = if running > 0 {
                PanelTone::Accent
            } else if selected["unknown"] == true {
                PanelTone::Warning
            } else {
                PanelTone::Muted
            };
            details.extend([
                (field(selected, "model").into(), PanelTone::Normal),
                (
                    format!("{calls} calls in retained history"),
                    PanelTone::Muted,
                ),
                (
                    format!("{running} running · {}", field(selected, "status")),
                    tone,
                ),
                ("".into(), PanelTone::Normal),
                ("Latest tool".into(), PanelTone::Heading),
                (field(selected, "tool").into(), PanelTone::Normal),
                ("Enter / → inspect latest call".into(), PanelTone::Muted),
                ("".into(), PanelTone::Normal),
                ("Messages".into(), PanelTone::Heading),
                ("No direct message address".into(), PanelTone::Normal),
                ("Choose Next caller to queue".into(), PanelTone::Muted),
                ("for the next eligible response".into(), PanelTone::Muted),
                ("".into(), PanelTone::Normal),
                ("Name and model are display labels".into(), PanelTone::Muted),
            ]);
        } else {
            let target = self.conversation.target.as_deref().unwrap_or("next");
            let id = field(selected, "agent_id");
            let pending = items(data, "queue")
                .iter()
                .filter(|item| {
                    field(item, "target") == id
                        && matches!(field(item, "status"), "queued" | "sending" | "editing")
                })
                .count();
            details.extend([
                (
                    (if next {
                        "Automatic delivery"
                    } else {
                        field(selected, "role")
                    })
                    .into(),
                    PanelTone::Muted,
                ),
                (
                    (if next {
                        "Next eligible response"
                    } else {
                        field(selected, "status")
                    })
                    .into(),
                    PanelTone::Normal,
                ),
                ("".into(), PanelTone::Normal),
                ("Messages".into(), PanelTone::Heading),
                (
                    format!("{pending} pending for this recipient"),
                    PanelTone::Normal,
                ),
            ]);
            if id == target {
                details.push(("● Current recipient".into(), PanelTone::Accent));
            }
            details.extend([
                ("Enter / → choose and return".into(), PanelTone::Normal),
                ("Your draft stays in place".into(), PanelTone::Muted),
                ("".into(), PanelTone::Normal),
                (
                    (if next {
                        "Any eligible caller may receive it"
                    } else {
                        "Only this member receives it"
                    })
                    .into(),
                    PanelTone::Muted,
                ),
            ]);
            if !next {
                details.push((id.into(), PanelTone::Muted));
            } else if items(data, "agents").is_empty() {
                details.extend([
                    ("".into(), PanelTone::Normal),
                    ("No registered agents yet".into(), PanelTone::Normal),
                    (
                        "Observed callers can still receive".into(),
                        PanelTone::Muted,
                    ),
                    ("requests through Next caller".into(), PanelTone::Muted),
                ]);
            }
        }
        details
    }

    fn chat_agent_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let entries = self.chat_recipients(data);
        if let Some(selected) = &self.conversation.selected_agent {
            if let Some(index) = entries
                .iter()
                .position(|entry| agent_key(entry) == *selected)
            {
                self.conversation.selected = index;
            } else {
                self.conversation.selected = 0;
                self.notice = Some("Selected member or caller left · choose a recipient".into());
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
        let target = self.conversation.target.as_deref().unwrap_or("next");
        let callers = entries
            .iter()
            .filter(|entry| field(entry, "kind") == "observed")
            .count();
        let mut rows = vec![self.chat_line(
            &format!(
                "  Agents · {} members · {callers} callers",
                items(data, "agents").len()
            ),
            PanelTone::Heading,
            list_width,
        )];
        for index in window {
            let entry = &entries[index];
            let observed = field(entry, "kind") == "observed";
            let marker = if !missing && index == self.conversation.selected {
                "›"
            } else {
                " "
            };
            let current = !observed && field(entry, "agent_id") == target;
            let symbol = if current {
                "● "
            } else if observed {
                "↳ "
            } else {
                ""
            };
            let title = if observed && !field(entry, "model").is_empty() {
                format!(
                    " {marker} {symbol}{} · {}",
                    field(entry, "name"),
                    field(entry, "model")
                )
            } else {
                format!(" {marker} {symbol}{}", field(entry, "name"))
            };
            let subtitle = if observed {
                format!("     {} calls · {}", entry["calls"], field(entry, "status"))
            } else if field(entry, "agent_id") == "next" {
                "     Next eligible response".into()
            } else {
                let duplicate = entries
                    .iter()
                    .filter(|other| {
                        field(other, "kind") != "observed"
                            && field(other, "name") == field(entry, "name")
                    })
                    .count()
                    > 1;
                let id = field(entry, "agent_id");
                let suffix: String = id
                    .chars()
                    .rev()
                    .take(8)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                let status = field(entry, "status");
                format!(
                    "     {}{}",
                    if status.is_empty() { "Member" } else { status },
                    if duplicate {
                        format!(" · …{suffix}")
                    } else {
                        String::new()
                    }
                )
            };
            rows.push(self.chat_line(
                &title,
                if !missing && index == self.conversation.selected {
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
                row.push_str(&self.tone(PanelTone::Muted, " │ "));
                row.push_str(&self.chat_line(detail, tone, width - list_width - 3));
            }
        }
        rows
    }

    fn chat_list_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        self.conversation.queue_hit_rows.clear();
        if self.conversation.view == View::Agents {
            return self.chat_agent_rows(data, height);
        }
        self.chat_queue_rows(data, height)
    }

    fn chat_list_hit(&self, row: u16) -> Option<usize> {
        if self.conversation.view == View::Queue {
            return self.chat_queue_hit(&self.chat_data(), row);
        }
        let relative = row.checked_sub(body_top(self.geometry.rows) + 1)?;
        if relative >= body_rows(self.geometry.rows).saturating_sub(1) {
            return None;
        }
        let index = usize::from(relative / 2);
        (index < self.conversation.list_count).then_some(self.conversation.list_start + index)
    }

    fn chat_scroll_hint(&self) -> Option<String> {
        if !self.conversation.logs_open && (self.help || self.conversation.view != View::Activity) {
            return None;
        }
        if self.conversation.logs_open {
            let below = if self.log_query.is_empty() && self.log_level == 0 {
                self.logs.screen().scrollback()
            } else {
                self.filter_offset
            };
            return (below > 0)
                .then(|| format!("↓ {below} rows below · Ctrl+End to return to latest"));
        }
        let below = self.events.rows_below();
        if below > 0 {
            Some(format!(
                "↓ {below} rows below · Ctrl+End to return to latest"
            ))
        } else if !self.events.is_following() {
            Some("Paused · Ctrl+End to follow new activity".into())
        } else {
            None
        }
    }

    pub(super) fn chat_frame(&mut self, now: Instant) -> Frame {
        self.events.set_scope(events::EventScope::Conversation);
        self.conversation.logs_open = self.conversation.view == View::Logs;
        self.chat_updates();
        if self.conversation.view == View::Tasks
            && let Err(error) = self.conversation.tasks.refresh(self.preview)
        {
            self.notice = Some(error);
        }
        self.chat_refresh_completions();
        let g = self.geometry;
        let data = self.chat_data();
        let agents = items(&data, "agents");
        let observed = self.audit.observed_controllers();
        let grouped_callers = self.caller_tracker.groups(&observed, &data);
        let queue = items(&data, "queue");
        let pending = queue
            .iter()
            .filter(|item| matches!(field(item, "status"), "queued" | "sending" | "editing"))
            .count();
        let top = body_top(g.rows);
        let height = body_rows(g.rows);
        let mut body = match self.conversation.view {
            View::Activity => {
                self.events.filter("", 0);
                self.events
                    .render(g.left, height, &self.ui, self.conversation.content_focus)
            }
            View::History => self.chat_history_rows(height),
            View::Questions => self.question_rows(height),
            View::Logs => self.filtered_logs().unwrap_or_else(|| {
                (0..height)
                    .map(|row| screen_row(self.logs.screen(), row, g.left, &self.ui))
                    .collect()
            }),
            View::Tasks => self.conversation.tasks.render(g.left, height, &self.ui),
            View::Audit => self.audit.render(g.left, height, &self.ui),
            View::Agents if self.conversation.detail => self.audit.render(g.left, height, &self.ui),
            _ => self.chat_list_rows(&data, height),
        };
        if self.help {
            body = self.chat_help_rows(height);
        }
        body.resize_with(usize::from(height), || {
            self.chat_line("", PanelTone::Normal, g.left)
        });
        body.truncate(usize::from(height));
        let selected_agent = if self.conversation.view == View::Agents {
            self.chat_selected()
        } else {
            None
        };
        self.chat_fill_body(&mut body, g.left);
        let empty = Composer::default();
        let input = if self.conversation.view == View::Questions {
            self.question_editor().unwrap_or(&empty)
        } else {
            &self.conversation.composer
        };
        let mut prompt = input.rows(
            g.left,
            prompt_rows(g.rows).max(1),
            &self.ui,
            self.conversation.view != View::Questions
                && !self.conversation.content_focus
                && (self.conversation.editing.is_none() || input.reference_active()),
        );
        let reference_stamp = (input.revision(), input.cursor(), g.rows, g.left);
        if self.conversation.view == View::Questions && input.is_empty() && !prompt.lines.is_empty()
        {
            prompt.lines[0] =
                self.chat_input_line("❯ Type an answer · number selects an option", g.left);
        }
        self.conversation.reference_stamp = Some(reference_stamp);
        self.conversation.reference_rows = std::mem::take(&mut prompt.references)
            .into_iter()
            .filter(|(row, _)| *row < prompt_rows(g.rows))
            .map(|(row, name)| (prompt_top(g.rows) + 1 + row, name))
            .collect();
        prompt
            .lines
            .resize_with(usize::from(prompt_rows(g.rows)), || {
                self.chat_input_line("", g.left)
            });
        let mut label = self
            .conversation
            .target
            .as_ref()
            .map(|target| {
                agents
                    .iter()
                    .find(|agent| field(agent, "agent_id") == target)
                    .map(|agent| field(agent, "name").to_owned())
                    .unwrap_or_else(|| "offline recipient".into())
            })
            .unwrap_or_else(|| "next caller".into());
        if self.conversation.view == View::Questions {
            label = self
                .conversation
                .question_id
                .as_ref()
                .and_then(|id| crate::client::interface::questions::get(id))
                .and_then(|q| q.title)
                .unwrap_or_else(|| "pending question".into());
        }
        let mut rows = Vec::new();
        for index in 0..g.rows {
            let line = if g.rows >= 3 && index == g.rows - 1 {
                let connection = super::footer::Connection::from_snapshot(self.preview, &data);
                let scroll_hint = self.chat_scroll_hint();
                let priority_hint = self.notice.is_some() || scroll_hint.is_some();
                let hint_tone = if scroll_hint.is_some() {
                    PanelTone::Accent
                } else {
                    PanelTone::Muted
                };
                let hint = self.notice.clone().or(scroll_hint).unwrap_or_else(|| {
                    if let Some(hint) = self.chat_contextual_hint("") { return hint; }
                    if self.conversation.logs_open { return "Logs · ←/→ panels · ↑↓ / PgUp/PgDn scroll · F9 filter · right-click / Esc back".into(); }
                    if self.help { return "Help · ↑↓ / PgUp/PgDn scroll · Home/End · Esc back".into(); }
                    if self.conversation.view == View::Tasks {
                        return if self.conversation.content_focus {
                            "Tasks · ↑↓ select · Enter details · PgUp/PgDn scroll · Esc back · ←/→ panels".into()
                        } else {
                            "Tasks · Tab browse · Enter sends draft · /tasks cancel ID".into()
                        };
                    }
                    if self.conversation.view == View::History {
                        return if self.conversation.detail { "Saved conversation · PgUp/PgDn · type to continue · Esc list".into() }
                            else { "↑↓ select · Enter resume · Esc back".into() };
                    }
                    if self.conversation.view == View::Questions {
                        return if self.conversation.detail {
                            "Enter answer · Shift+↑ next · Esc list · Tab browse".into()
                        } else {
                            "↑↓ select · Enter answer · Delete skip · Esc back".into()
                        };
                    }
                    if self.conversation.view == View::Agents {
                        return if self.conversation.detail {
                            "← / Esc  back to agents".into()
                        } else {
                            if selected_agent.as_ref().is_some_and(|entry| field(entry, "kind") == "observed") {
                                "Esc back · ↑↓ select · Enter latest call · ←/→ panels".into()
                            } else {
                                "Esc back · ↑↓ select · Enter message · ←/→ panels".into()
                            }
                        };
                    }
                    format!(
                        "{} · Alt+←/→ panels · right-click logs · /help",
                        if self.conversation.content_focus {
                            "browse"
                        } else {
                            "input"
                        }
                    )
                });
                let text = super::footer::render(
                    &hint,
                    self.server_origin.as_ref(),
                    connection,
                    g.cols,
                    priority_hint,
                );
                rows.push(self.tone(hint_tone, &text));
                continue;
            } else if top > 0 && index == 0 {
                let suffix = if self.conversation.view == View::Logs
                    && (!self.log_query.is_empty() || self.log_level != 0)
                {
                    format!(
                        " · {} /{}",
                        ["all", "commands", "errors"][usize::from(self.log_level)],
                        self.log_query
                    )
                } else {
                    String::new()
                };
                let label = if self.conversation.view == View::Activity
                    && !self.help
                    && self.conversation.canvas.is_default_conversation()
                {
                    format!("conversation{suffix}")
                } else {
                    format!("{}{suffix}", self.conversation.canvas.label(g.left))
                };
                if g.left < 32 {
                    self.chat_line(&format!("ssmcp · {label}"), PanelTone::Muted, g.left)
                } else {
                    let label = fit(&label, g.left.saturating_sub(11));
                    let title = fit(
                        &format!("  ssmcp{}", self.context_label),
                        g.left.saturating_sub(label.width() as u16 + 4),
                    );
                    let gap = usize::from(g.left).saturating_sub(title.width() + label.width() + 2);
                    format!(
                        "{}{}",
                        self.tone(PanelTone::Heading, &title),
                        self.tone(PanelTone::Muted, &format!("{}{label}  ", " ".repeat(gap))),
                    )
                }
            } else if index >= top && index < top + height {
                body[usize::from(index - top)].clone()
            } else if index == prompt_top(g.rows) && prompt_rows(g.rows) > 0 {
                let title = if self.conversation.view == View::Questions {
                    format!(" Answer · {label} ")
                } else if self.conversation.editing.is_some() {
                    format!(" Editing · {label} ")
                } else if self.conversation.target.is_some() {
                    format!(" To · {label} ")
                } else if pending > 0 {
                    format!(" {pending} queued · /queue ")
                } else {
                    String::new()
                };
                let title = fit(title.trim(), g.left.saturating_sub(4));
                let available = usize::from(g.left).saturating_sub(title.width() + 4);
                let detail = if g.left == g.cols {
                    let [account, remaining] = self.account.rows();
                    let full = format!("{} · {}", account.trim(), remaining.trim());
                    if full.width() <= available {
                        full
                    } else if remaining.trim().width() <= available {
                        remaining.trim().into()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let gap = "─".repeat(available.saturating_sub(detail.width()));
                self.chat_line(
                    &format!("  {title}{gap}{detail}  "),
                    PanelTone::Muted,
                    g.left,
                )
            } else {
                prompt
                    .lines
                    .get(usize::from(index.saturating_sub(prompt_top(g.rows) + 1)))
                    .cloned()
                    .unwrap_or_else(|| self.chat_line("", PanelTone::Normal, g.left))
            };
            rows.push(line);
        }
        let layout = PanelLayout::configured(
            g.cols,
            g.rows,
            self.ui.sidebar && !self.ui.panels.is_empty(),
            self.ui.sidebar_width,
        );
        if let Some(rect) = layout.sidebar {
            let limit = usize::from(g.rows.saturating_sub(1));
            let footer = self.guide_rows(rect.width, limit as u16, 0, rect.col);
            let footer_start = limit.saturating_sub(footer.len());
            let agent_view = self.conversation.view == View::Agents;
            let mut sidebar = if agent_view {
                self.chat_agent_details(selected_agent.as_ref(), &data)
                    .into_iter()
                    .map(|(text, tone)| (format!(" {text}"), tone))
                    .collect()
            } else {
                Vec::new()
            };
            for kind in self.ui.panels.iter().filter(|_| !agent_view) {
                let mut section = match kind {
                    crate::client::ui::Panel::Agents => {
                        let mut section =
                            vec![(format!(" Members · {}", agents.len()), PanelTone::Heading)];
                        section.push((" ← choose agent".into(), PanelTone::Muted));
                        for agent in agents.iter().take(4) {
                            section.push((format!(" {}", field(agent, "name")), PanelTone::Accent));
                            section.push((
                                format!("   reported {}", field(agent, "status")),
                                PanelTone::Muted,
                            ));
                        }
                        if !grouped_callers.is_empty() {
                            section.push((
                                format!(" Observed callers · {}", grouped_callers.len()),
                                PanelTone::Heading,
                            ));
                            for controller in grouped_callers.iter().take(3) {
                                let tone = if controller.running > 0 {
                                    PanelTone::Accent
                                } else if controller.unknown {
                                    PanelTone::Warning
                                } else {
                                    PanelTone::Muted
                                };
                                section.push((format!(" ↳ {}", controller.name), tone));
                                section.push((
                                    format!(
                                        "   {} · {} calls · {}",
                                        controller.model, controller.calls, controller.status
                                    ),
                                    tone,
                                ));
                            }
                        }
                        if agents.is_empty() && observed.is_empty() {
                            section.push((" Waiting for agent activity".into(), PanelTone::Muted));
                        }
                        section
                    }
                    crate::client::ui::Panel::Queue => vec![
                        (format!(" Queue · {pending} pending"), PanelTone::Heading),
                        (" /queue to review and edit".into(), PanelTone::Muted),
                    ],
                    _ => {
                        let mut ui = self.ui.clone();
                        ui.panels = vec![*kind];
                        let mut section = panel::render_configured(
                            &self.panel,
                            rect.width.saturating_sub(1),
                            g.rows,
                            now,
                            &ui,
                        );
                        if section.first().is_some_and(|row| row.text == "Ready") {
                            section.clear();
                        }
                        while section.last().is_some_and(|row| row.text.trim().is_empty()) {
                            section.pop();
                        }
                        section
                            .into_iter()
                            .map(|row| (format!(" {}", row.text), row.tone))
                            .collect()
                    }
                };
                if !section.is_empty() {
                    if !sidebar.is_empty() {
                        sidebar.push((String::new(), PanelTone::Normal));
                    }
                    sidebar.append(&mut section);
                }
            }
            for (index, line) in rows.iter_mut().enumerate().take(limit) {
                line.push_str(&self.tone(PanelTone::Muted, "│"));
                if index >= footer_start {
                    line.push_str(&footer[index - footer_start]);
                } else {
                    let (text, tone) = sidebar
                        .get(index)
                        .cloned()
                        .unwrap_or((String::new(), PanelTone::Normal));
                    line.push_str(&self.chat_line(&text, tone, rect.width));
                }
            }
        }
        let cursor = (
            (prompt_top(g.rows) + 1 + prompt.cursor_row).min(g.rows - 1),
            prompt.cursor_col.min(g.left - 1),
        );
        Frame {
            geometry: g,
            rows,
            cursor,
            hidden: self.help || self.conversation.content_focus || prompt_rows(g.rows) == 0,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: true,
            mouse_capture: !self.selection,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(rows: u16, cols: u16) -> State {
        super::super::preview_state(rows, cols, &Ui::default())
    }
    fn input(state: &mut State, bytes: &[u8]) {
        assert!(state.chat_keyboard(bytes, Instant::now(), false).is_empty());
    }
    fn display(state: &mut State) -> vt100::Parser {
        let frame = state.chat_frame(Instant::now());
        let mut parser = vt100::Parser::new(
            nonzero(frame.geometry.rows),
            nonzero(frame.geometry.cols),
            0,
        );
        parser.process(&frame.diff(None));
        parser
    }

    fn command_panel_fixture(rows: u16, cols: u16) -> State {
        let mut state = preview(rows, cols);
        state.events.clear();
        state.log_message(
            "user",
            "Run both checks and keep their logs easy to inspect.",
            b"",
        );
        for (id, command) in [
            ("build", "python3 -c · 3 lines"),
            ("tests", "cargo test --lib"),
        ] {
            let details = if id == "build" {
                "    cwd /workspace/demo\n    script · python3 · 3 lines\n    1 import pathlib\n    2 root = pathlib.Path.cwd()\n    3 print(root.name)\n"
            } else {
                ""
            };
            state.log_event(
                "preview",
                &json!({"type":"exec.started","job_id":id}),
                format!("[RUN] preview {id} {command} started\n{details}").as_bytes(),
            );
            let output = (0..40)
                .map(|n| format!("{id} step {n:02} · complete\n"))
                .collect::<String>();
            state.log_event(
                "preview",
                &json!({"type":"exec.stdout","job_id":id}),
                format!("[STDOUT] preview {id}\n{output}").as_bytes(),
            );
        }
        state.log_event(
            "preview",
            &json!({"type":"exec.exited","job_id":"build","exit_code":0}),
            b"[OK] preview build exit=0\n",
        );
        state.conversation.composer.set_text("Keep this draft");
        state.notice = None;
        state
    }

    fn save_command_panel_preview(state: &mut State, name: &str) {
        if let Some(out) = std::env::var_os("SSMCP_COMMAND_PANEL_PREVIEW_DIR") {
            let out = std::path::PathBuf::from(out);
            std::fs::create_dir_all(&out).unwrap();
            std::fs::write(
                out.join(format!("{name}-{}.ansi", state.geometry.cols)),
                state.chat_frame(Instant::now()).diff(None),
            )
            .unwrap();
        }
    }

    #[test]
    fn command_panel_mouse_and_keyboard_scroll_preserve_other_panels_and_the_draft() {
        for cols in [120, 48] {
            let mut state = command_panel_fixture(28, cols);
            let screen = display(&mut state).screen().contents();
            assert!(
                screen.contains("build step 37") && screen.contains("tests step 37"),
                "{screen}"
            );
            save_command_panel_preview(&mut state, "collapsed");
            let first_y = screen
                .lines()
                .position(|row| row.contains("build step 37"))
                .unwrap() as u16;
            let second_y = screen
                .lines()
                .position(|row| row.contains("tests step 37"))
                .unwrap() as u16;
            let before = state.events.snapshot();
            state.chat_key(Key::Mouse {
                button: 64,
                x: 8,
                y: first_y,
                release: false,
            });
            display(&mut state);
            let first = state.events.snapshot();
            assert_eq!(first["scroll"], before["scroll"]);
            assert_eq!(first["events"][1]["panel"]["scroll"], 34);
            assert_eq!(first["events"][2]["panel"]["scroll"], 37);
            state.chat_key(Key::Mouse {
                button: 64,
                x: 8,
                y: second_y,
                release: false,
            });
            display(&mut state);
            let both = state.events.snapshot();
            assert_eq!(both["events"][1]["panel"]["scroll"], 34);
            assert_eq!(both["events"][2]["panel"]["scroll"], 34);
            save_command_panel_preview(&mut state, "independent-scroll");
            state.chat_key(Key::Mouse {
                button: 64,
                x: 8,
                y: prompt_top(state.geometry.rows) + 1,
                release: false,
            });
            assert_eq!(
                state.events.snapshot(),
                both,
                "wheel over the composer cannot scroll the transcript"
            );
            state.chat_key(Key::Mouse {
                button: 0,
                x: 8,
                y: first_y,
                release: false,
            });
            display(&mut state);
            assert!(
                state.events.snapshot()["events"][1]["expanded"]
                    .as_bool()
                    .unwrap()
            );
            let expanded = display(&mut state).screen().contents();
            assert!(
                expanded.contains("Command") && expanded.contains("1 import pathlib"),
                "{expanded}"
            );
            assert_eq!(state.events.snapshot()["events"][1]["panel"]["scroll"], 0);
            assert_eq!(state.events.snapshot()["events"][2]["panel"]["scroll"], 34);
            save_command_panel_preview(&mut state, "expanded");
            state.chat_key(Key::Down);
            display(&mut state);
            assert!(
                state.events.snapshot()["events"][1]["panel"]["scroll"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            state.chat_key(Key::End);
            assert!(
                state.events.snapshot()["events"][1]["panel"]["following"]
                    .as_bool()
                    .unwrap()
            );
            assert_eq!(state.conversation.composer.text(), "Keep this draft");
            state.chat_key(Key::Select);
            let selection = state.events.snapshot();
            state.chat_key(Key::Mouse {
                button: 64,
                x: 8,
                y: first_y,
                release: false,
            });
            assert!(state.selection);
            assert_eq!(
                state.events.snapshot(),
                selection,
                "terminal selection keeps its existing copy behavior"
            );
            state.chat_key(Key::Escape);
            assert!(!state.selection);
        }
    }

    #[test]
    fn tasks_command_renders_its_own_jobs_without_appending_conversation_or_logs() {
        let mut state = preview(32, 120);
        display(&mut state);
        state.conversation.composer.set_text("保留草稿界");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        state.conversation.editing = Some("editable-request".into());
        let draft = state.conversation.snapshot()["draft"].clone();
        let events = state.events.snapshot()["events"].clone();
        let logs = state.logs.screen().contents_formatted();

        assert!(state.chat_command("/tasks").unwrap());
        assert_eq!(state.conversation.view, View::Tasks);
        assert!(!state.conversation.logs_open);
        let tasks = state.conversation.tasks.snapshot();
        assert_eq!(tasks["source"], "preview_fixture");
        assert_eq!(tasks["selected_id"], "preview-task-build");
        assert_eq!(tasks["jobs"].as_array().unwrap().len(), 2);
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("fictional task data"), "{screen}");
        assert!(screen.contains("cargo check --workspace"), "{screen}");
        assert_eq!(state.events.snapshot()["events"], events);
        assert_eq!(state.logs.screen().contents_formatted(), logs);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("editable-request")
        );
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
    }

    #[test]
    fn logs_and_tasks_are_adjacent_columns_and_escape_restores_the_bookmark() {
        let mut state = preview(30, 120);
        state.conversation.composer.set_text("unfinished 界");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        state.chat_change_view(View::Audit);
        let draft = state.conversation.snapshot()["draft"].clone();
        assert!(state.chat_command("/logs").unwrap());
        assert_eq!(state.conversation.view, View::Logs);
        assert!(
            state.conversation.snapshot()["logs_open"]
                .as_bool()
                .unwrap()
        );
        state.chat_key(Key::Open);
        assert_eq!(state.conversation.view, View::Tasks);
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        state.chat_key(Key::Back);
        assert_eq!(state.conversation.view, View::Logs);
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Audit);
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
    }

    #[test]
    fn tasks_selection_enter_and_escape_keep_the_shared_draft_intact() {
        let mut state = preview(30, 120);
        state.conversation.composer.set_text("draft stays 界");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        let draft = state.conversation.snapshot()["draft"].clone();
        state.chat_command("/tasks").unwrap();
        display(&mut state);
        state.chat_key(Key::AuditDown);
        assert_eq!(
            state.conversation.tasks.snapshot()["selected_id"],
            "preview-task-test"
        );
        state.chat_character('\r');
        assert_eq!(state.conversation.tasks.snapshot()["detail"], true);
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("cargo test --lib"), "{screen}");
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.tasks.snapshot()["detail"], false);
        assert!(state.conversation.content_focus);
        assert_eq!(state.conversation.view, View::Tasks);
        state.chat_key(Key::Delete);
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("/tasks cancel ID")
        );
        state.chat_key(Key::Escape);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.view, View::Tasks);
        assert_eq!(
            state.conversation.tasks.snapshot()["selected_id"],
            "preview-task-test"
        );
        assert_eq!(state.conversation.snapshot()["draft"], draft);
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
    }

    #[test]
    fn task_commands_require_an_explicit_cancellation_id_and_preview_never_cancels() {
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("keep this draft");
        let before = state.chat_input_stamp().unwrap();
        let events = state.events.snapshot()["events"].clone();
        for command in [
            "/tasks unknown",
            "/tasks cancel",
            "/tasks cancel job extra",
            "/tasks cancel preview-task-build",
        ] {
            assert!(state.chat_command(command).is_err(), "{command}");
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
            assert_eq!(state.events.snapshot()["events"], events);
            assert!(state.actions.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_batch_acceptance_is_atomic_and_changed_drafts_roll_back_copies() {
        use crate::client::references::files::{self, SelectedFile};
        use std::{
            fs,
            time::{Duration, SystemTime, UNIX_EPOCH},
        };
        for mode in ["accept", "changed", "full"] {
            let root = std::env::temp_dir().join(format!(
                "ssmcp-draft-files-{}-{}-{mode}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            let imported = files::import(
                &root,
                vec![SelectedFile {
                    name: "fixture.txt".into(),
                    bytes: b"original copy".to_vec(),
                }],
            )
            .unwrap();
            let destination = root.join(&imported.entries[0].path);
            let references = imported.references();
            let mut state = super::super::preview_state(24, 100, &Ui::default());
            let initial = if mode == "full" {
                "x".repeat(crate::client::composer::MAX_DRAFT_BYTES)
            } else {
                "中文草稿".to_owned()
            };
            state.conversation.composer.set_text(&initial);
            let expected = state.chat_input_stamp().unwrap();
            if mode == "changed" {
                state.conversation.composer.set_text("更新后的草稿");
            }
            let before = state.conversation.composer.text().to_owned();
            state.chat_finish_input(
                Some(&expected),
                None,
                Ok(attachments::Loaded::Files {
                    text: references.clone(),
                    description: "fixture import".into(),
                    files: imported,
                }),
            );
            if mode == "accept" {
                assert!(state.conversation.composer.text().contains(&references));
                assert_eq!(fs::read(&destination).unwrap(), b"original copy");
            } else {
                assert_eq!(state.conversation.composer.text(), before);
                let deadline = Instant::now() + Duration::from_secs(2);
                while destination.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                assert!(
                    !destination.exists(),
                    "rejected import must roll back its owned copy"
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn local_memory_result_never_replaces_an_edited_user_draft() {
        let mut state = super::super::preview_state(24, 100, &Ui::default());
        state.conversation.composer.set_text("保留当前用户要求");
        let expected = state.chat_input_stamp().unwrap();
        state.conversation.composer.set_text("后来编辑的新内容");
        state.chat_finish_input(
            Some(&expected),
            Some("/memory"),
            Ok(attachments::Loaded::Context(
                "Memory fixture · local reference".into(),
            )),
        );
        assert_eq!(state.conversation.composer.text(), "后来编辑的新内容");
        assert!(
            display(&mut state)
                .screen()
                .contents()
                .contains("Memory fixture")
        );
    }

    #[test]
    fn header_shows_startup_project_and_optional_custom_title() {
        let ui = Ui {
            sidebar: false,
            ..Ui::default()
        };
        let mut state = super::super::preview_state(24, 100, &ui);
        for (cwd, title, expected) in [
            ("/workspace/alpha-project/", None, "ssmcp  /  alpha-project"),
            (
                "/workspace/终端",
                Some("修复输入框"),
                "ssmcp  /  终端  /  修复输入框",
            ),
            ("", None, "ssmcp"),
        ] {
            state.set_context(cwd, title);
            let screen = display(&mut state).screen().contents();
            let header = screen.lines().next().unwrap().trim();
            assert!(header.starts_with(expected), "{header}");
            assert!(header.ends_with("conversation"), "{header}");
            assert!(!header.contains("agents") && !header.contains("code audit"));
        }
        state.set_context("/workspace/alpha-project", Some("Review uploads"));
        state.chat_change_view(View::Queue);
        let screen = display(&mut state).screen().contents();
        let header = screen.lines().next().unwrap().trim();
        assert!(header.starts_with("ssmcp  /  alpha-project  /  Review uploads"));
        assert!(header.ends_with("queue  4/8"));
    }

    #[test]
    fn header_is_one_safe_row_with_narrow_and_unicode_content() {
        let ui = Ui {
            sidebar: false,
            ..Ui::default()
        };
        for cols in [20, 40, 80, 100] {
            let mut state = super::super::preview_state(24, cols, &ui);
            let before = display(&mut state).screen().contents();
            state.set_context(
                "/workspace/项目\nname",
                Some("修改标题\r\u{1b}[2J\u{202e}extra-long-title"),
            );
            let after = display(&mut state).screen().contents();
            assert_eq!(
                after.lines().skip(1).collect::<Vec<_>>(),
                before.lines().skip(1).collect::<Vec<_>>(),
                "header must not wrap or inject controls at {cols} columns",
            );
            let header = after.lines().next().unwrap();
            assert!(header.width() <= usize::from(cols));
            if cols == 100 {
                assert!(header.contains(r"项目\nname"));
                assert!(header.contains(r"修改标题\r\x1b[2J\u{202e}extra-long-title"));
            }
        }
    }

    #[test]
    fn left_opens_agents_only_at_draft_start_and_escape_keeps_the_draft() {
        let mut state = preview(28, 120);
        input(&mut state, "中文 draft".as_bytes());
        input(&mut state, b"\x1b[D");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.cursor(), "中文 draf".len());
        input(&mut state, b"\x01\x1b[D");
        assert_eq!(state.conversation.view, View::Agents);
        assert!(state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "中文 draft");
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "中文 draft");
        assert_eq!(state.conversation.composer.cursor(), 0);
        input(&mut state, b"\x1b[D");
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:next")
        );
    }

    #[test]
    fn quick_escape_then_arrow_moves_columns_without_inserting_csi_text() {
        let mut state = preview(28, 120);
        state.chat_change_view(View::Queue);
        input(&mut state, b"\x1b\x1b[D");
        assert_eq!(state.conversation.view, View::Audit);
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn session_and_terminal_decoders_preserve_coalesced_escape_arrow_and_paste() {
        use crate::client::input::{InputEvent, SessionKeys};
        for sequence in ["\x1b\x1b[D", "\x1b\x1b[200~literal draft\x1b[201~"] {
            for split in 0..=sequence.len() {
                let mut state = preview(28, 120);
                state.chat_change_view(View::Queue);
                let mut session = SessionKeys::default();
                let now = Instant::now();
                for part in [&sequence[..split], &sequence[split..]] {
                    for event in session.feed(part, now, false) {
                        if let InputEvent::Text(text) = event {
                            state.keyboard(text.as_bytes(), now, false);
                        } else {
                            panic!("unexpected terminal switch");
                        }
                    }
                }
                for event in session.feed("", now + Duration::from_millis(100), true) {
                    if let InputEvent::Text(text) = event {
                        state.keyboard(text.as_bytes(), now + Duration::from_millis(100), false);
                    }
                }
                state.keyboard(&[], now + Duration::from_millis(200), true);
                if sequence.ends_with("[D") {
                    assert_eq!(state.conversation.view, View::Audit);
                    assert!(state.conversation.composer.is_empty());
                } else {
                    assert_eq!(state.conversation.composer.text(), "literal draft");
                }
            }
        }
    }

    #[test]
    fn expired_agent_identity_cannot_select_an_unrelated_row() {
        let mut state = preview(28, 120);
        state.chat_agents();
        state.conversation.selected = 1;
        state.conversation.selected_agent = Some("member:expired".into());
        assert!(state.chat_selected().is_none());
        state.chat_open(true);
        assert!(state.conversation.target.is_none());
        assert_eq!(state.conversation.view, View::Agents);
        let data = state.chat_data();
        state.chat_agent_rows(&data, 18);
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:expired")
        );
        assert!(state.notice.as_ref().unwrap().contains("left"));
        state.chat_open(true);
        assert!(state.conversation.target.is_none());
        assert_eq!(state.conversation.view, View::Agents);
        state.chat_move(1);
        state.chat_open(true);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
    }

    #[test]
    fn duplicate_member_names_show_distinct_identity_suffixes() {
        let mut state = preview(28, 120);
        state.chat_agents();
        let data = json!({"agents":[
            {"agent_id":"agent_prefix_11111111","name":"Builder","status":"working"},
            {"agent_id":"agent_prefix_22222222","name":"Builder","status":"available"}
        ]});
        let rows = state.chat_agent_rows(&data, 18).join("\n");
        assert!(rows.contains("…11111111"));
        assert!(rows.contains("…22222222"));
    }

    #[test]
    fn agent_roster_groups_calls_and_preserves_selection_when_another_call_arrives() {
        let mut state = preview(32, 120);
        state.audit = Default::default();
        for id in ["one", "two", "three"] {
            state.audit.ingest(&json!({
                "type":"ssmcp.code_mode", "run_id":id, "phase":"started",
                "controller":{"name":"Repeated caller","model":"Model","source":"caller-reported"}
            }));
        }
        state.chat_agents();
        let data = state.chat_data();
        let entries = state.chat_recipients(&data);
        assert_eq!(entries.len(), 4, "next + two members + one observed caller");
        let caller = entries
            .iter()
            .position(|entry| field(entry, "kind") == "observed")
            .unwrap();
        assert_eq!(entries[caller]["calls"], 3);
        state.chat_move(caller as isize);
        let selected = state.conversation.selected_agent.clone();
        state.audit.ingest(&json!({
            "type":"ssmcp.code_mode", "run_id":"four", "phase":"started",
            "controller":{"name":"Repeated caller","model":"Model","source":"caller-reported"}
        }));
        let rows = state.chat_agent_rows(&data, 24).join("\n");
        assert_eq!(
            rows.matches("↳ Repeated caller").count(),
            1,
            "the roster groups calls; details may repeat the selected name"
        );
        assert!(rows.contains("4 calls"));
        assert_eq!(state.conversation.selected_agent, selected);
        assert_eq!(state.chat_selected().unwrap()["calls"], 4);
        assert_eq!(state.chat_selected().unwrap()["run_id"], "four");
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("No direct message address"));
        assert!(!screen.contains("Recent commits"));
        assert!(!screen.contains("Last controller"));
    }

    #[test]
    fn sidebar_inspects_selected_member_and_keeps_its_message_count_scoped() {
        let mut state = preview(32, 120);
        state.chat_agents();
        state.chat_move(1);
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Only this member receives it"));
        assert!(screen.contains("example-builder"));
        assert!(!screen.contains("Recent commits"));
        let data = json!({"queue":[
            {"target":"example-builder","status":"queued"},
            {"target":"example-builder","status":"delivered"},
            {"target":"example-reviewer","status":"queued"}
        ]});
        let entry = state.chat_selected().unwrap();
        let details = state.chat_agent_details(Some(&entry), &data);
        assert!(
            details
                .iter()
                .any(|(text, _)| text == "1 pending for this recipient")
        );
    }

    #[test]
    fn mouse_uses_the_drawn_caller_identity_when_new_activity_reorders_the_list() {
        let mut state = preview(32, 120);
        state.audit = Default::default();
        state.audit.ingest(&json!({
            "type":"ssmcp.code_mode", "run_id":"drawn", "phase":"started",
            "controller":{"name":"Original caller","model":"A","source":"caller-reported"}
        }));
        state.chat_agents();
        let data = state.chat_data();
        state.chat_agent_rows(&data, 24);
        let index = state
            .chat_recipients(&data)
            .iter()
            .position(|entry| field(entry, "run_id") == "drawn")
            .unwrap();
        state.audit.ingest(&json!({
            "type":"ssmcp.code_mode", "run_id":"new-first-row", "phase":"started",
            "controller":{"name":"Another caller","model":"B","source":"caller-reported"}
        }));
        state.chat_key(Key::Mouse {
            button: 0,
            x: 2,
            y: body_top(state.geometry.rows) + 1 + index as u16 * 2,
            release: false,
        });
        state.chat_key(Key::Open);
        assert_eq!(state.audit.snapshot()["selected"]["run_id"], "drawn");
        assert!(state.conversation.target.is_none());
    }

    #[test]
    fn narrow_agent_list_keeps_different_models_visible() {
        let mut state = preview(24, 70);
        state.audit = Default::default();
        for model in ["Model A", "Model B"] {
            state.audit.ingest(&json!({
                "type":"ssmcp.code_mode", "run_id":model, "phase":"started",
                "controller":{"name":"ChatGPT","model":model,"source":"caller-reported"}
            }));
        }
        state.chat_agents();
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("ChatGPT · Model A"));
        assert!(screen.contains("ChatGPT · Model B"));
    }

    #[test]
    fn choosing_an_agent_returns_to_chat_and_preserves_the_current_edit() {
        let mut state = preview(28, 120);
        state.conversation.composer.set_text("draft under review");
        state.conversation.editing = Some("existing-request".into());
        state.conversation.composer.home();
        input(&mut state, b"\x1b[D\x1b[B\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
        assert_eq!(state.conversation.composer.text(), "draft under review");
        assert_eq!(state.conversation.composer.cursor(), 0);
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("existing-request")
        );
        assert!(state.conversation.submitting.is_none());
        input(&mut state, b"\x1b[D");
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:example-builder")
        );
        input(&mut state, b"\x1b[B\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
        assert_eq!(state.conversation.composer.text(), "draft under review");
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("existing-request")
        );
        assert_eq!(state.conversation.composer.cursor(), 0);
    }

    #[test]
    fn agent_panel_closes_before_typing_or_pasting_without_switching_recipient() {
        let mut state = preview(28, 120);
        state.conversation.target = Some("example-reviewer".into());
        input(&mut state, b"\x1b[D");
        input(&mut state, "继续".as_bytes());
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "继续");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
        input(&mut state, b"\x01\x1b[D");
        input(&mut state, b"\x1b[200~pasted \x1b[201~");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "pasted 继续");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
    }

    #[test]
    fn removed_agents_command_cannot_open_a_view_or_submit_a_request() {
        let mut state = preview(28, 120);
        assert!(state.chat_command("/agents").is_err());
        input(&mut state, b"/agents\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "/agents");
        assert!(state.conversation.submitting.is_none());
        assert!(
            crate::client::composer::SLASH_COMMANDS
                .iter()
                .all(|command| command.name != "/agents")
        );
    }

    #[test]
    fn agent_panel_adapts_to_small_screens_and_shows_how_to_return() {
        for rows in [1, 3, 6, 10, 18, 36] {
            for cols in [1, 4, 20, 60, 80, 120, 160] {
                let mut state = preview(rows, cols);
                state.chat_agents();
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), usize::from(rows));
                assert!(frame.cursor.0 < rows && frame.cursor.1 < cols);
                let shot = display(&mut state);
                assert_eq!(shot.screen().size(), (nonzero(rows), nonzero(cols)));
                if rows >= 10 && cols >= 80 {
                    let visible = shot.screen().contents();
                    assert!(visible.contains("Agents"));
                    assert!(visible.contains("Esc back"), "{cols}×{rows}: {visible}");
                    assert!(!visible.contains("collab.join"));
                }
            }
        }
    }

    #[test]
    fn agent_details_are_not_mouse_targets_and_editing_keys_reveal_the_draft() {
        let mut state = preview(28, 160);
        state.conversation.composer.set_text("draft");
        state.conversation.composer.home();
        state.chat_agents();
        display(&mut state);
        state.chat_key(Key::Mouse {
            button: 0,
            x: state.chat_agent_list_width() + 5,
            y: body_top(state.geometry.rows) + 5,
            release: false,
        });
        assert_eq!(state.conversation.view, View::Agents);
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:next")
        );
        assert!(state.conversation.target.is_none());
        state.chat_key(Key::Delete);
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "raft");
        state.chat_agents();
        state.chat_character('\u{b}');
        assert_eq!(state.conversation.view, View::Activity);
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn mouse_selects_visible_agents_after_a_list_has_scrolled() {
        let data = json!({
            "agents":(0..24).map(|index| json!({"agent_id":format!("agent_{index}"),"name":format!("Agent {index}"),"role":"worker","status":"working"})).collect::<Vec<_>>(),
            "queue":(0..24).map(|index| json!({"id":format!("request_{index}"),"text":format!("Request {index}"),"status":"queued"})).collect::<Vec<_>>()
        });
        let view = View::Agents;
        let mut state = preview(18, 120);
        state.canvas_focus(view);
        state.conversation.content_focus = true;
        state.conversation.selected = 15;
        state.conversation.selected_agent = None;
        let rows = state.chat_list_rows(&data, body_rows(state.geometry.rows));
        let start = state.conversation.list_start;
        assert!(start > 0);
        assert_eq!(state.conversation.list_count, 5);
        assert!(!rows[1].contains("Agent 0  ·"));
        let first = body_top(state.geometry.rows) + 1;
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: first,
            release: false,
        });
        assert_eq!(state.conversation.selected, start);
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: first + 3,
            release: false,
        });
        assert_eq!(
            state.conversation.selected,
            start + 1,
            "subtitle selects its visible item"
        );
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: body_top(state.geometry.rows),
            release: false,
        });
        assert_eq!(
            state.conversation.selected,
            start + 1,
            "list header is not an entry"
        );
        assert!(!state.conversation.detail);
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: body_top(state.geometry.rows),
            release: false,
        });
        assert_eq!(state.conversation.view, View::Logs);
        assert!(state.conversation.logs_open);
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: prompt_top(state.geometry.rows),
            release: false,
        });
        assert_eq!(
            state.conversation.selected,
            start + 1,
            "prompt border is not an entry"
        );
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.view, view);
    }

    #[test]
    fn mouse_targets_scrolled_request_cards_by_full_id_and_ignores_spacing() {
        let data = json!({
            "queue":(0..24).map(|index| json!({"id":format!("request_{index}"),"text":format!("Request {index}"),"status":"queued"})).collect::<Vec<_>>()
        });
        let mut state = preview(18, 120);
        state.canvas_focus(View::Queue);
        state.conversation.content_focus = true;
        state.conversation.selected = 15;
        let rows = state.chat_list_rows(&data, body_rows(state.geometry.rows));
        let start = state.conversation.list_start;
        assert!(start > 0);
        assert_eq!(state.conversation.list_count, 4);
        assert!(rows[1].contains(&format!("Request {start}")));
        let top = body_top(state.geometry.rows);
        for offset in 0..state.conversation.list_count {
            let first = 1 + offset * 3;
            let expected = start + offset;
            for row in [first, first + 1] {
                let hit = state.chat_queue_hit(&data, top + row as u16);
                assert_eq!(
                    hit,
                    Some(expected),
                    "body and metadata select the same card"
                );
                assert_eq!(
                    message_id(&data["queue"][hit.unwrap()]),
                    format!("request_{expected}"),
                    "the visible row resolves to its complete request ID"
                );
            }
            if offset + 1 < state.conversation.list_count {
                assert_eq!(state.chat_queue_hit(&data, top + (first + 2) as u16), None);
            }
        }
        for row in [top, prompt_top(state.geometry.rows)] {
            assert_eq!(
                state.chat_queue_hit(&data, row),
                None,
                "chrome is not an entry"
            );
        }
        // The live preview has a different queue. A stale rendered ID must not
        // select a same-position request from that replacement snapshot.
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: top + 1,
            release: false,
        });
        assert_eq!(
            state.conversation.selected, 15,
            "removed IDs are not mouse targets"
        );

        // Exercise the real mouse entry point with the same queue it renders.
        let live = state.chat_data();
        state.chat_list_rows(&live, body_rows(state.geometry.rows));
        for row in [top + 1, top + 2] {
            state.conversation.selected = 15;
            state.chat_key(Key::Mouse {
                button: 0,
                x: 1,
                y: row,
                release: false,
            });
            assert_eq!(state.conversation.selected, 0);
            assert_eq!(
                message_id(&state.chat_selected().unwrap()),
                "example-request"
            );
        }
        for row in [top, prompt_top(state.geometry.rows)] {
            state.chat_key(Key::Mouse {
                button: 0,
                x: 1,
                y: row,
                release: false,
            });
            assert_eq!(
                state.conversation.selected, 0,
                "chrome does not move selection"
            );
        }
        assert!(!state.conversation.detail);
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: top,
            release: false,
        });
        assert_eq!(state.conversation.view, View::Logs);
        assert!(state.conversation.logs_open);
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: prompt_top(state.geometry.rows),
            release: false,
        });
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.view, View::Queue);
        assert_eq!(state.conversation.selected, 0);
    }

    #[test]
    fn continuation_only_removes_a_backslash_immediately_before_the_cursor() {
        let mut state = preview(36, 120);
        state.chat_restore_draft(steering::SavedDraft {
            text: "界x\\".into(),
            cursor: "界".len(),
            target: None,
        });
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "界x\\");
        assert_eq!(state.conversation.composer.cursor(), "界".len());
        state.chat_restore_draft(steering::SavedDraft {
            text: "界\\tail".into(),
            cursor: "界\\".len(),
            target: None,
        });
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "界\ntail");
        assert_eq!(state.conversation.composer.cursor(), "界\n".len());
        state.conversation.composer.set_text("last\\");
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "last\n");
    }

    #[test]
    fn queue_inspect_keys_preserve_the_draft_recipient_and_editing_request() {
        let mut state = preview(36, 120);
        state.chat_change_view(View::Queue);
        state.conversation.composer.set_text("Keep this draft 界");
        state.conversation.composer.left();
        state.conversation.target = Some("example-builder".into());
        state.conversation.editing = Some("another-request".into());
        let saved = state.chat_saved_draft();

        for key in ['i', 'I'] {
            state.conversation.detail = false;
            state.conversation.detail_scroll = 17;
            state.chat_character(key);
            assert!(state.conversation.detail);
            assert_eq!(state.conversation.detail_scroll, 0);
            assert_eq!(state.conversation.view, View::Queue);
            assert!(state.conversation.content_focus);
            assert_eq!(state.chat_saved_draft(), saved);
            assert_eq!(
                state.conversation.editing.as_deref(),
                Some("another-request")
            );
        }

        state.conversation.detail = false;
        state.conversation.content_focus = false;
        state.conversation.composer.set_text("typing ");
        state.chat_character('i');
        assert_eq!(state.conversation.composer.text(), "typing i");
        assert!(!state.conversation.detail);
    }

    #[test]
    fn help_blocks_hidden_queue_actions_and_printable_input_returns_to_the_editor() {
        let mut state = preview(36, 120);
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        state.conversation.composer.set_text("draft A");
        state.chat_key(Key::Help);
        state.chat_key(Key::Delete);
        state.chat_character('\r');
        state.chat_key(Key::Open);
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: 2,
            release: false,
        });
        assert!(state.help);
        assert!(!state.conversation.detail);
        assert!(
            state.notice.is_none(),
            "hidden cancel/takeback must not be attempted"
        );
        assert_eq!(state.conversation.composer.text(), "draft A");
        assert!(state.conversation.parked.is_none());
        state.chat_character('R');
        assert!(!state.help);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "draft AR");
        assert!(
            state.conversation.parked.is_none(),
            "R must type rather than copy the hidden queue entry"
        );
    }

    #[test]
    fn a_parked_draft_survives_all_second_replacement_paths_and_escape_restores_it() {
        let mut state = preview(36, 120);
        let original = steering::SavedDraft {
            text: "继续review".into(),
            cursor: "继续re".len(),
            target: Some("example-reviewer".into()),
        };
        state.chat_restore_draft(original.clone());
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        let copied = state.chat_saved_draft();
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        input(&mut state, b"\tR");
        assert_eq!(state.chat_saved_draft(), copied);
        input(&mut state, b"\t\r");
        assert_eq!(state.chat_saved_draft(), copied);
        state.conversation.composer.remember("unrelated history");
        input(&mut state, b"\x1b[A\x1b[B");
        assert_eq!(state.chat_saved_draft(), copied);
        state.chat_editable(
            "another_request".into(),
            "external request".into(),
            Some("example-builder".into()),
        );
        assert_eq!(state.chat_saved_draft(), copied);
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        assert!(state.conversation.editing.is_none());
        state.chat_key(Key::Help);
        state.chat_key(Key::Escape);
        assert_eq!(
            state.chat_saved_draft(),
            copied,
            "closing help must not abandon the copied draft"
        );
        state.chat_key(Key::Escape);
        assert_eq!(state.chat_saved_draft(), original);
        assert!(state.conversation.parked.is_none());
    }

    #[test]
    fn same_request_retarget_keeps_revisions_in_the_editor_and_the_parked_draft() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("original draft A");
        let original = state.chat_saved_draft();
        state.chat_editable(
            "request_b".into(),
            "old B".into(),
            Some("example-builder".into()),
        );
        state.conversation.composer.set_text("revised B");
        state.conversation.composer.left();
        let cursor = state.conversation.composer.cursor();
        state.chat_editable("request_b".into(), "old B".into(), None);
        assert_eq!(state.conversation.composer.text(), "revised B");
        assert_eq!(state.conversation.composer.cursor(), cursor);
        assert!(state.conversation.target.is_none());
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        assert_eq!(state.conversation.editing.as_deref(), Some("request_b"));
    }

    #[test]
    fn successful_local_enqueue_keeps_different_and_identical_following_drafts() {
        for next in ["next", "first"] {
            let mut state = preview(36, 120);
            state.conversation.composer.set_text("first");
            state.chat_enqueued("request_one".into(), "first".into());
            assert!(state.conversation.composer.is_empty());
            input(&mut state, next.as_bytes());
            assert_eq!(state.conversation.composer.text(), next);
            state.chat_accepted("request_one".into(), "first".into());
            assert_eq!(
                state.conversation.composer.text(),
                next,
                "a later ACK cannot erase a new draft"
            );
            assert!(state.conversation.submitting.is_none());
        }
    }

    #[test]
    fn local_enqueue_restores_the_parked_recipient_and_cursor_before_acknowledgement() {
        let mut state = preview(36, 120);
        let parked = steering::SavedDraft {
            text: "原稿 A".into(),
            cursor: "原".len(),
            target: Some("example-reviewer".into()),
        };
        state.conversation.parked = Some(parked.clone());
        state.conversation.editing = Some("request_b".into());
        state.conversation.composer.set_text("edited B");
        state.chat_enqueued("request_b".into(), "edited B".into());
        assert_eq!(state.chat_saved_draft(), parked);
        assert!(state.conversation.editing.is_none());
        assert!(state.conversation.parked.is_none());
        input(&mut state, b"next");
        let following = state.chat_saved_draft();
        state.chat_accepted("request_b".into(), "edited B".into());
        assert_eq!(state.chat_saved_draft(), following);
    }

    #[test]
    fn editing_navigation_cannot_replace_a_taken_back_request_with_history() {
        let mut state = preview(36, 120);
        state.conversation.editing = Some("request-being-edited".into());
        state.conversation.composer.set_text("revised requirement");
        state
            .conversation
            .composer
            .remember("unrelated accepted request");
        input(&mut state, b"\x1b[A\x1b[B");
        assert_eq!(state.conversation.composer.text(), "revised requirement");
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        assert_eq!(state.conversation.composer.text(), "revised requirement");
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("request-being-edited")
        );
        assert!(!state.conversation.content_focus);
    }

    #[test]
    fn temporarily_parked_draft_restores_its_recipient_and_utf8_cursor() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("继续review");
        state.conversation.composer.left();
        state.conversation.target = Some("example-reviewer".into());
        let before = state.chat_saved_draft();
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        assert!(state.conversation.parked.is_some());
        assert!(state.conversation.target.is_none());
        let parked = state.conversation.parked.take().unwrap();
        state.chat_restore_draft(parked);
        assert_eq!(state.conversation.composer.text(), before.text);
        assert_eq!(state.conversation.composer.cursor(), before.cursor);
        assert_eq!(state.conversation.target, before.target);
    }

    #[test]
    fn binary_bracketed_paste_preserves_every_byte_across_read_boundaries() {
        let png = crate::client::input::paste::fixture_png();
        let mut bytes = b"\x1b[200~".to_vec();
        bytes.extend_from_slice(&png);
        for chunk in 1..=bytes.len() {
            let mut state = preview(28, 120);
            state.conversation.composer.set_text("保留原稿");
            for part in bytes.chunks(chunk) {
                input(&mut state, part);
            }
            assert_eq!(state.conversation.paste.as_deref(), Some(png.as_slice()));
            assert_eq!(state.conversation.composer.text(), "保留原稿");
            input(&mut state, b"\x1b[201~");
            assert!(state.conversation.paste.is_none());
            assert_eq!(state.conversation.composer.text(), "保留原稿");
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .starts_with("Preview cannot"),
                "recognized images must reach the guarded import path before UTF-8 decoding"
            );
        }
    }

    #[test]
    fn image_idle_recovery_discards_only_the_paste_and_accepts_the_next_text() {
        use crate::client::input::paste::{IMAGE_IDLE_TIMEOUT, fixture_png};
        let png = fixture_png();
        for payload in [&png[..1], &png[..png.len() - 1], png.as_slice()] {
            let now = Instant::now();
            let mut state = preview(28, 120);
            state.conversation.composer.set_text("保留原稿");
            let source = [b"\x1b[200~".as_slice(), payload].concat();
            state.keyboard(&source, now, false);
            assert!(state.conversation.paste.is_some());
            state.keyboard(b" next", now + IMAGE_IDLE_TIMEOUT, false);
            assert!(state.conversation.paste.is_none());
            assert!(!state.keyboard.pasting);
            assert_eq!(state.conversation.composer.text(), "保留原稿 next");
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("Incomplete image paste")
            );
            assert!(state.conversation.input_task.is_none());
        }
    }

    #[test]
    fn split_image_end_uses_raw_input_progress_across_graphics_and_keyboard() {
        use crate::client::input::paste::fixture_png;
        let now = Instant::now();
        let mut state = preview(28, 120);
        state.conversation.composer.set_text("draft");
        let source = [b"\x1b[200~".as_slice(), &fixture_png()].concat();
        state.keyboard(&source, now, false);
        // Graphics holds this prefix. The composer must still see its progress.
        state.keyboard(b"\x1b", now + Duration::from_millis(1500), false);
        state.keyboard(&[], now + Duration::from_millis(2200), true);
        assert!(state.keyboard.pasting);
        assert!(state.conversation.paste.is_some());
        state.keyboard(b"[201~", now + Duration::from_millis(2500), false);
        assert!(!state.keyboard.pasting);
        assert!(state.conversation.paste.is_none());
        assert_eq!(state.conversation.composer.text(), "draft");
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .starts_with("Preview cannot")
        );
    }

    #[test]
    fn attach_buffered_image_end_progress_keeps_the_compositor_paste_alive() {
        use crate::client::input::paste::fixture_png;
        use crate::session::attach::input::{Decoder, Event};
        fn deliver(state: &mut State, events: Vec<Event>, now: Instant) {
            for event in events {
                match event {
                    Event::Bytes(bytes) | Event::PasteBytes(bytes) => {
                        state.keyboard(&bytes, now, false);
                    }
                    Event::InputProgress(at) => state.paste_input_progress(at),
                    Event::PasteExpired => state.discard_image_paste(),
                    _ => panic!("image bytes must not become attach commands"),
                }
            }
        }

        let now = Instant::now();
        let mut state = preview(28, 120);
        let mut outer = Decoder::default();
        state.conversation.composer.set_text("draft");
        let source = [b"\x1b[200~".as_slice(), &fixture_png()].concat();
        deliver(&mut state, outer.feed_at(&source, false, now), now);
        let progress = now + Duration::from_millis(1900);
        let events = outer.feed_at(b"\x1b", false, progress);
        assert!(matches!(events.as_slice(), [Event::InputProgress(at)] if *at == progress));
        deliver(&mut state, events, progress);
        // The outer has retained every byte in the new chunk. A display tick
        // after the old deadline must still retain the entire image.
        state.keyboard(&[], now + Duration::from_millis(2100), true);
        assert!(state.conversation.paste.is_some());
        assert!(state.keyboard.pasting);
        let end = now + Duration::from_secs(3);
        deliver(&mut state, outer.feed_at(b"[201~", false, end), end);
        assert!(state.conversation.paste.is_none());
        assert!(!state.keyboard.pasting);
        assert_eq!(state.conversation.composer.text(), "draft");
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .starts_with("Preview cannot")
        );
        state.keyboard(b" next", end + Duration::from_millis(1), false);
        assert_eq!(state.conversation.composer.text(), "draft next");
    }

    #[test]
    fn explicit_attach_recovery_resets_all_compositor_layers_before_new_input() {
        use crate::client::input::paste::fixture_png;
        let now = Instant::now();
        let mut state = preview(28, 120);
        state.conversation.composer.set_text("draft");
        let source = [b"\x1b[200~".as_slice(), &fixture_png()].concat();
        state.keyboard(&source, now, false);
        state.keyboard(b"\x1b[20", now + Duration::from_millis(1), false);
        // The outer reader can reach its deadline slightly before this layer.
        state.discard_image_paste();
        state.keyboard(b" next", now + Duration::from_millis(2), false);
        assert_eq!(state.conversation.composer.text(), "draft next");
        assert!(state.conversation.paste.is_none());
        assert!(!state.keyboard.pasting);
    }

    #[test]
    fn coalesced_utf8_navigation_and_multiline_paste_keep_input_local() {
        let mut state = preview(36, 120);
        input(&mut state, "修复界面".as_bytes());
        input(&mut state, b"\x1b[D\x1b[D");
        input(&mut state, "这个".as_bytes());
        assert_eq!(state.conversation.composer.text(), "修复这个界面");
        input(&mut state, b"\x1b[F\x1b[200~\r\n/reload\r\nq\x1b[201~");
        assert_eq!(
            state.conversation.composer.text(),
            "修复这个界面\n/reload\nq"
        );
        assert!(
            state.notice.is_none(),
            "pasted commands must not run or submit"
        );
        assert!(!display(&mut state).screen().hide_cursor());
        input(&mut state, b"\r");
        assert!(state.notice.as_ref().unwrap().contains("Preview only"));
        assert!(state.conversation.composer.text().ends_with("/reload\nq"));
    }

    #[test]
    fn keyboard_views_and_arrow_selection_preserve_unsubmitted_text() {
        let mut state = preview(36, 120);
        input(&mut state, b"\x1b[D\x1b[B\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
        assert!(!state.conversation.content_focus);
        input(&mut state, b"new requirement\x0f");
        assert_eq!(state.conversation.view, View::Audit);
        assert_eq!(state.conversation.composer.text(), "new requirement");
        input(&mut state, b"\x1b[D\x1b[C\x0f");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "new requirement");
    }

    #[test]
    fn wrapped_input_arrows_move_the_cursor_before_recalling_history() {
        let mut state = preview(24, 30);
        state.conversation.composer.remember("previous request");
        let text = "界".repeat(40);
        input(&mut state, text.as_bytes());
        input(&mut state, b"\x1b[A");
        assert_eq!(state.conversation.composer.text(), text);
        assert!(state.conversation.composer.cursor() < text.len());
        assert!(state.conversation.composer.cursor() > 0);
        input(&mut state, b"\x1b[H\x1b[A");
        assert_eq!(state.conversation.composer.text(), "previous request");
    }

    #[test]
    fn slash_menu_arrows_do_not_replace_drafts_and_tab_uses_the_selected_item() {
        let mut state = preview(36, 120);
        state.conversation.composer.remember("previous request");
        input(&mut state, b"/aud\x1b[A");
        assert_eq!(state.conversation.composer.text(), "/aud");
        assert_eq!(
            state
                .conversation
                .composer
                .selected_suggestion()
                .unwrap()
                .name,
            "/audit"
        );
        assert!(state.conversation.editing.is_none());
        assert!(state.conversation.target.is_none());
        assert!(display(&mut state).screen().contents().contains("› /audit"));
        input(&mut state, b"\t");
        assert_eq!(state.conversation.composer.text(), "/audit ");
        assert!(!state.conversation.content_focus);
        assert!(!state.chat_menu_visible());

        state.conversation.composer.set_text("/aud");
        input(&mut state, b"\x1b[B\r");
        assert_eq!(state.conversation.view, View::Audit);
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn escape_closes_command_menu_without_restoring_or_overwriting_a_draft() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("/re");
        state.conversation.parked = Some(steering::SavedDraft {
            text: "parked request".into(),
            cursor: 6,
            target: None,
        });
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.composer.text(), "/re");
        assert!(state.conversation.parked.is_some());
        assert!(!state.chat_menu_visible());
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.composer.text(), "parked request");

        // Re-editing a user requirement beginning with '/' is plain text,
        // including Tab: local command selection must not rewrite its body.
        state.conversation.composer.set_text("/re");
        state.conversation.editing = Some("request-being-edited".into());
        assert!(!state.chat_menu_visible());
        input(&mut state, b"\t");
        assert_eq!(state.conversation.composer.text(), "/re");
    }

    #[test]
    fn restart_alias_uses_the_same_validation_and_preview_guard_as_reload() {
        let mut state = preview(36, 120);
        assert_eq!(
            state.chat_command("/restart"),
            state.chat_command("/reload")
        );
        assert!(
            state
                .chat_command("/restart extra")
                .unwrap_err()
                .contains("without arguments")
        );
        input(&mut state, b"/re\x1b[B\r");
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .contains("no connected session")
        );
        assert_eq!(state.conversation.composer.text(), "/re");
    }

    #[test]
    fn diagnostic_mouse_wheel_scrolls_without_changing_the_draft() {
        let mut state = preview(24, 100);
        for index in 0..80 {
            state.log(format!("event {index}\n").as_bytes());
        }
        input(&mut state, b"unfinished");
        state.chat_command("/logs").unwrap();
        assert!(state.conversation.logs_open);
        assert_eq!(state.logs.screen().scrollback(), 0);
        input(&mut state, b"\x1b[<64;10;5M");
        assert!(state.logs.screen().scrollback() > 0);
        assert_eq!(state.conversation.view, View::Logs);
        assert!(display(&mut state).screen().hide_cursor());
        assert_eq!(state.conversation.composer.text(), "unfinished");
        input(&mut state, b"\x1b[1;5F");
        assert_eq!(state.logs.screen().scrollback(), 0);
    }

    #[test]
    fn diagnostic_log_body_keeps_sidebar_columns_and_theme_background_with_short_unicode_rows() {
        for sidebar in [false, true] {
            let ui = Ui {
                color: true,
                sidebar,
                panels: vec![crate::client::ui::Panel::Agents],
                ..Ui::default()
            };
            let mut state = State::new(18, 120, &ui);
            state.preview = true;
            state.log(b"    \x1b[38;2;20;40;60mshort\x1b[0m\n");
            state.log("    中文界\n".as_bytes());
            state.log(b"    \x1b[38;2;255;0;0;48;2;0;0;255m\xe2\x96\x80\x1b[0m\n");
            state.logs_toggle();
            assert!(state.conversation.logs_open);
            let g = state.geometry;
            let screen = display(&mut state);
            let screen = screen.screen();
            let mut theme = vt100::Parser::new(nonzero(1), nonzero(1), 0);
            theme.process(format!("\x1b[{}m ", ui.background()).as_bytes());
            let background = theme.screen().cell(0, 0).unwrap().bgcolor();
            for row in body_top(g.rows)..body_top(g.rows) + body_rows(g.rows) {
                assert_eq!(
                    screen.cell(row, g.left - 1).unwrap().bgcolor(),
                    background,
                    "body padding must paint the theme background at row {row}"
                );
                if sidebar {
                    assert_eq!(
                        screen.cell(row, g.left).unwrap().contents(),
                        "│",
                        "the sidebar must start at its fixed column on row {row}"
                    );
                }
            }
            let top = body_top(g.rows);
            assert_eq!(
                screen.cell(top, 4).unwrap().fgcolor(),
                vt100::Color::Rgb(20, 40, 60)
            );
            assert_eq!(screen.cell(top + 1, 4).unwrap().contents(), "中");
            assert_eq!(screen.cell(top + 1, 6).unwrap().contents(), "文");
            assert_eq!(screen.cell(top + 1, 8).unwrap().contents(), "界");
            assert_eq!(
                screen.cell(top + 2, 4).unwrap().bgcolor(),
                vt100::Color::Rgb(0, 0, 255)
            );
            assert_eq!(screen.cell(top + 3, 4).unwrap().bgcolor(), background);
        }
    }

    #[test]
    fn every_terminal_size_keeps_frames_and_cursor_inside_the_screen() {
        for rows in [1, 2, 3, 5, 6, 9, 10, 18, 36] {
            for cols in [1, 2, 4, 20, 95, 96, 120] {
                let mut state = preview(rows, cols);
                input(&mut state, "界面\nLong input line".as_bytes());
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), usize::from(rows));
                assert!(frame.cursor.0 < rows && frame.cursor.1 < cols);
                let screen = display(&mut state);
                assert!(screen.screen().cursor_position().0 < rows);
                assert_eq!(screen.screen().size(), (nonzero(rows), nonzero(cols)));
            }
        }
    }

    #[test]
    fn sidebar_sections_obey_configuration_and_diagnostic_filters_keep_results() {
        let mut state = super::super::preview_state(
            36,
            120,
            &Ui {
                sidebar: true,
                ..Ui::default()
            },
        );
        state.ui.panels = vec![crate::client::ui::Panel::Queue];
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Queue · 1 pending"));
        assert!(!screen.contains("Members · 2"));
        input(&mut state, b"/filter cargo test --lib\r");
        assert!(state.conversation.logs_open);
        state.chat_key(Key::HistoryStart);
        let start = display(&mut state).screen().contents();
        assert!(
            start
                .lines()
                .skip(1)
                .any(|row| row.trim_start().starts_with("● #17  cargo test --lib")),
            "filtered diagnostics retain the command heading at Home:\n{start}"
        );
        assert!(
            state
                .logs
                .screen()
                .contents()
                .contains("[RUN] #17  cargo test --lib"),
            "semantic log labels must not rewrite the retained raw transcript"
        );
        assert!(!start.contains("A quieter workspace"));
        state.chat_key(Key::HistoryEnd);
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("exit=0"), "{screen}");
        assert!(screen.contains("custom_colors ... ok"), "{screen}");
        assert!(!screen.contains("A quieter workspace"));
        state.chat_key(Key::Escape);
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.view, View::Activity);
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Improve the client experience"));
        assert!(screen.contains("cargo test --lib"));
        assert!(!screen.contains("A quieter workspace"));
        state.chat_command("/logs").unwrap();
        assert_eq!(state.conversation.view, View::Logs);
        assert_eq!(state.log_query, "cargo test --lib");
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Activity);
    }
    #[test]
    fn observed_agent_calls_open_audit_without_changing_the_message_recipient() {
        let mut state = preview(28, 120);
        state.audit = Default::default();
        for id in ["alpha", "beta"] {
            state.audit.ingest(&json!({
                "type":"ssmcp.code_mode","run_id":id,"phase":"started",
                "code":format!("return '{id}';"),
                "controller":{"name":"ChatGPT","model":"Declared model","source":"caller-reported"}
            }));
        }
        state.conversation.target = Some("chosen-recipient".into());
        state.conversation.composer.insert("draft stays here");
        state.chat_change_view(View::Agents);
        let data = state.chat_data();
        let first_observed = state
            .chat_recipients(&data)
            .iter()
            .position(|entry| field(entry, "kind") == "observed")
            .unwrap();
        state.chat_move(first_observed as isize);
        let selected = field(&state.chat_selected().unwrap(), "run_id").to_owned();
        state.chat_open(true);
        assert!(state.conversation.detail);
        assert_eq!(state.audit.snapshot()["selected"]["run_id"], selected);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("chosen-recipient")
        );
        assert_eq!(state.conversation.composer.text(), "draft stays here");
        assert!(
            display(&mut state)
                .screen()
                .contents()
                .contains("Declared model")
        );
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Agents);
        assert!(!state.conversation.detail);
        state.chat_open(false);
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: 2,
            release: false,
        });
        assert!(!state.conversation.detail);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("chosen-recipient")
        );

        state.audit.ingest(&json!({"type":"ssmcp.code_mode","run_id":"new-call","phase":"started","code":"return 3;"}));
        state.chat_list_rows(&data, body_rows(state.geometry.rows));
        assert_eq!(field(&state.chat_selected().unwrap(), "run_id"), selected);
        state.chat_move(-1);
        assert_ne!(field(&state.chat_selected().unwrap(), "run_id"), selected);
    }
    #[tokio::test]
    async fn enter_waits_for_input_without_changing_draft_recipient_or_edit_revision() {
        for editing in [None, Some("editable-request".to_owned())] {
            let mut state = preview(24, 100);
            state.conversation.composer.set_text("说明：请检查界");
            state.conversation.composer.left();
            state.conversation.target = Some("chosen-agent".into());
            state.conversation.editing = editing.clone();
            let stamp = state.chat_input_stamp().unwrap();
            let (task, sender) = attachments::Task::pending_for_test();
            state.conversation.input_task = Some(task);
            state.conversation.input_stamp = Some(stamp.clone());
            state.dirty = false;

            input(&mut state, b"\r");

            assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
            assert!(state.conversation.submitting.is_none());
            assert!(state.actions.is_empty());
            assert!(state.dirty);
            assert_eq!(
                state.notice.as_deref(),
                Some("Preparing local input… · wait for it to finish before sending")
            );
            assert!(state.conversation.input_task.is_some());

            sender
                .send(Ok(attachments::Loaded::Reference {
                    reference: "@\".ssmcp/inputs/image-fixture.png\"".into(),
                    description: "Prepared local image".into(),
                }))
                .unwrap();
            state.chat_poll_input();
            assert_eq!(
                state.conversation.composer.text(),
                "说明：请检查\n@\".ssmcp/inputs/image-fixture.png\"\n界"
            );
            assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
            assert_eq!(state.conversation.editing, editing);
            assert!(state.conversation.submitting.is_none());
            assert!(!state.chat_wait_for_input());
        }
    }

    #[tokio::test]
    async fn enter_waits_for_attachment_before_submitting_a_question_answer() {
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("保留普通草稿");
        state.conversation.view = View::Questions;
        state.conversation.detail = true;
        state.conversation.question_id = Some("pending-image-question".into());
        let mut answer = Composer::default();
        answer.set_text("回答界");
        answer.left();
        state
            .conversation
            .answers
            .insert("pending-image-question".into(), answer);
        let stamp = state.chat_input_stamp().unwrap();
        let (task, sender) = attachments::Task::pending_for_test();
        state.conversation.input_task = Some(task);
        state.conversation.input_stamp = Some(stamp.clone());

        input(&mut state, b"\r");

        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
        assert_eq!(state.conversation.composer.text(), "保留普通草稿");
        assert_eq!(
            state.notice.as_deref(),
            Some("Preparing local input… · wait for it to finish before sending")
        );
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        sender
            .send(Ok(attachments::Loaded::Reference {
                reference: "@\".ssmcp/inputs/answer-fixture.png\"".into(),
                description: "Prepared answer image".into(),
            }))
            .unwrap();
        state.chat_poll_input();
        assert_eq!(
            state.question_editor().unwrap().text(),
            "回答\n@\".ssmcp/inputs/answer-fixture.png\"\n界"
        );
        assert_eq!(state.conversation.composer.text(), "保留普通草稿");
        assert_eq!(
            state.conversation.question_id.as_deref(),
            Some("pending-image-question")
        );
        assert!(!state.chat_wait_for_input());
    }

    #[test]
    fn async_local_reference_stays_editable_and_keeps_recipient() {
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("请检查 ");
        state.conversation.target = Some("chosen-agent".into());
        let stamp = state.chat_input_stamp().unwrap();
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Reference {
                reference: "@\".ssmcp/inputs/image-fixture.png\"".into(),
                description: "Prepared local image".into(),
            }),
        );
        assert!(state.conversation.composer.text().contains("请检查"));
        assert!(
            state
                .conversation
                .composer
                .text()
                .contains("@\".ssmcp/inputs/image-fixture.png\"")
        );
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
    }

    #[test]
    fn async_local_input_rejects_draft_aba_and_recipient_changes() {
        let mut state = preview(24, 100);
        state.conversation.composer.take_text();
        let stamp = state.chat_input_stamp().unwrap();
        state.conversation.composer.insert("temporary");
        state.conversation.composer.take_text();
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Text("late result".into())),
        );
        assert!(state.conversation.composer.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("not inserted"));

        let stamp = state.chat_input_stamp().unwrap();
        state.conversation.target = Some("other-agent".into());
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Reference {
                reference: "@\".ssmcp/inputs/retained.png\"".into(),
                description: "fixture".into(),
            }),
        );
        assert!(state.conversation.composer.is_empty());
        assert!(state.notice.as_deref().unwrap().contains("retained.png"));
        assert_eq!(state.conversation.target.as_deref(), Some("other-agent"));
    }

    #[test]
    fn async_clipboard_isolated_from_normal_draft_and_changed_question() {
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("普通草稿");
        state.conversation.view = View::Questions;
        state.conversation.detail = true;
        state.conversation.question_id = Some("question-a".into());
        state
            .conversation
            .answers
            .insert("question-a".into(), Composer::default());
        let stamp = state.chat_input_stamp().unwrap();
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Text("回答\r\n下一行".into())),
        );
        assert_eq!(state.question_editor().unwrap().text(), "回答\n下一行");
        assert_eq!(state.conversation.composer.text(), "普通草稿");
        let stamp = state.chat_input_stamp().unwrap();
        state.conversation.question_id = Some("question-b".into());
        state
            .conversation
            .answers
            .insert("question-b".into(), Composer::default());
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Text("迟到".into())),
        );
        assert!(state.question_editor().unwrap().is_empty());
        assert_eq!(
            state.conversation.answers["question-a"].text(),
            "回答\n下一行"
        );
        assert_eq!(state.conversation.composer.text(), "普通草稿");
    }

    #[test]
    fn failed_local_command_restores_only_the_unchanged_command_editor() {
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("/attach missing.png");
        state.conversation.composer.take_text();
        let stamp = state.chat_input_stamp().unwrap();
        state.chat_finish_input(
            Some(&stamp),
            Some("/attach missing.png"),
            Err("File not found".into()),
        );
        assert_eq!(state.conversation.composer.text(), "/attach missing.png");
        let stamp = state.chat_input_stamp().unwrap();
        state.conversation.composer.set_text("新的要求");
        state.chat_finish_input(
            Some(&stamp),
            Some("/attach missing.png"),
            Err("File not found".into()),
        );
        assert_eq!(state.conversation.composer.text(), "新的要求");
    }
    #[test]
    fn scrolling_shows_return_hint_and_control_end_preserves_the_draft() {
        for end in [b"\x1b[1;5F".as_slice(), b"\x1b[4;5~", b"\x1b[8;5~"] {
            let mut state = preview(30, 120);
            state.conversation.composer.set_text("keep this draft");
            for index in 0..80 {
                let text = format!("retained event {index}");
                state.log_message("user", &text, format!("\n[USER] {text}\n").as_bytes());
            }
            display(&mut state);
            input(&mut state, b"\x1b[1;5H");
            let scrolled = display(&mut state);
            assert!(scrolled.screen().contents().contains("Ctrl+End"));
            assert!(state.events.rows_below() > 0);
            input(&mut state, end);
            display(&mut state);
            assert!(state.events.is_following());
            assert_eq!(state.events.rows_below(), 0);
            assert!(state.chat_scroll_hint().is_none());
            assert_eq!(state.conversation.composer.text(), "keep this draft");
        }
    }

    #[test]
    fn explicit_diagnostics_start_in_logs_and_escape_restores_conversation() {
        for ui in [
            Ui {
                log_format: super::super::LogFormat::PrettyVerbose,
                ..Ui::default()
            },
            Ui {
                show_raw_json: true,
                ..Ui::default()
            },
        ] {
            let mut state = super::super::preview_state(40, 120, &ui);
            assert!(state.conversation.logs_open);
            let original = display(&mut state).screen().contents();
            assert!(original.contains("Logs"));
            assert!(original.contains("Command completed successfully."));
            state.chat_command("/logs").unwrap();
            assert!(
                state.conversation.logs_open,
                "/logs opens the same panel idempotently"
            );
            state.chat_key(Key::Escape);
            assert!(!state.conversation.logs_open);
            let conversation = display(&mut state).screen().contents();
            assert!(
                conversation
                    .lines()
                    .next()
                    .unwrap()
                    .contains("conversation")
            );
            assert!(conversation.contains("cargo test --lib"));
            assert!(!conversation.contains("A quieter workspace"));
        }
    }

    #[test]
    fn raw_and_logs_commands_share_diagnostics_without_changing_the_draft() {
        let mut state = preview(30, 120);
        state.conversation.composer.set_text("unsent request");
        assert!(state.chat_command("/raw").unwrap());
        assert!(state.conversation.logs_open);
        assert!(display(&mut state).screen().contents().contains("Logs"));
        assert_eq!(state.conversation.composer.text(), "unsent request");
        assert!(state.chat_command("/raw unexpected").is_err());
        assert!(state.conversation.logs_open);
        assert!(state.chat_command("/logs").unwrap());
        assert!(state.conversation.logs_open);
        assert!(state.chat_command("/raw").unwrap());
        assert!(!state.conversation.logs_open);
        assert_eq!(state.conversation.composer.text(), "unsent request");
        let conversation = display(&mut state).screen().contents();
        assert!(
            conversation
                .lines()
                .next()
                .unwrap()
                .contains("conversation")
        );
        assert!(!conversation.contains("A quieter workspace"));
    }
    #[test]
    fn logs_editor_stays_in_its_column_and_commands_target_that_column() {
        let mut state = preview(36, 120);
        state.canvas_focus(View::Audit);
        state.conversation.target = Some("example-builder".into());
        state.conversation.composer.set_text("abc");
        state.conversation.composer.home();
        state.conversation.composer.right();
        state.logs_open();
        input(&mut state, b"\t");
        assert!(!state.conversation.content_focus);
        state.chat_key(Key::Open);
        assert_eq!(state.conversation.view, View::Logs);
        assert_eq!(state.conversation.composer.cursor(), 2);
        input(&mut state, b"d");
        assert_eq!(state.conversation.composer.text(), "abdc");
        assert!(!display(&mut state).screen().hide_cursor());
        state.chat_key(Key::CanvasRight);
        assert_eq!(state.conversation.view, View::Tasks);
        state.chat_key(Key::CanvasLeft);
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Audit);
        assert_eq!(state.conversation.composer.text(), "abdc");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );

        state.logs_open();
        state.conversation.composer.set_text("ab");
        state.conversation.composer.home();
        state.conversation.content_focus = true;
        state.chat_key(Key::Delete);
        assert_eq!(state.conversation.view, View::Logs);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "b");
        state.conversation.content_focus = true;
        state.chat_key(Key::Newline);
        assert_eq!(state.conversation.view, View::Logs);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "\nb");
        assert!(!display(&mut state).screen().hide_cursor());
        state.conversation.composer.set_text("");
        input(&mut state, b"/close\r");
        assert!(
            !state.conversation.canvas.snapshot()["columns"]
                .as_array()
                .unwrap()
                .iter()
                .any(|column| column == "Logs")
        );
        state.chat_command("/logs").unwrap();
        input(&mut state, b"/new chat\r");
        assert_eq!(state.conversation.view, View::Activity);
        let canvas = state.conversation.canvas.snapshot();
        let columns = canvas["columns"].as_array().unwrap();
        let logs = columns.iter().position(|column| column == "Logs").unwrap();
        assert_eq!(columns[logs + 1], "Tasks");
        assert_eq!(canvas["active"].as_u64().unwrap() as usize, logs + 2);
    }

    #[test]
    fn exit_aliases_request_local_cleanup_without_publishing_a_message() {
        for command in ["/exit", "/quit"] {
            for pending in 0..3 {
                let mut state = preview(36, 120);
                state.events.set_scope(events::EventScope::Conversation);
                if pending > 0 {
                    state.conversation.editing = Some("pending-edit".into());
                }
                if pending > 1 {
                    state.conversation.submitting =
                        Some(("pending-request".into(), "pending-command".into()));
                }
                let editing = state.conversation.editing.clone();
                let submitting = state.conversation.submitting.clone();
                let before = state.events.snapshot();
                input(&mut state, format!("{command} extra\r").as_bytes());
                assert!(!state.conversation.exit_requested);
                assert!(
                    state
                        .notice
                        .as_deref()
                        .unwrap()
                        .contains("without arguments")
                );
                assert_eq!(
                    state.conversation.composer.text(),
                    format!("{command} extra")
                );
                state.conversation.composer.set_text("");
                input(&mut state, format!("{command}\r").as_bytes());
                assert!(state.conversation.exit_requested);
                assert_eq!(state.events.snapshot(), before);
                assert_eq!(state.conversation.editing, editing);
                assert_eq!(state.conversation.submitting, submitting);
            }
        }
    }

    #[test]
    fn server_footer_keeps_canvas_drafts_and_recipients_intact() {
        let mut state = preview(24, 100);
        state.server_origin = super::super::footer::Origin::parse(
            "https://user:password@ssmcp.lmm.best/mcp?token=hidden#private",
        );
        state.conversation.composer.set_text("保留草稿界");
        state.conversation.composer.left();
        state.conversation.target = Some("example-builder".into());
        let draft = state.conversation.snapshot()["draft"].clone();
        let queue = state.chat_data()["queue"].clone();
        for view in [View::Activity, View::Logs, View::Tasks, View::Audit] {
            state.canvas_focus(view);
            state.notice = Some("/exit must be used without arguments".into());
            let frame = state.chat_frame(Instant::now());
            let bottom = frame.rows.last().unwrap();
            assert!(bottom.contains("https://ssmcp.lmm.best"), "{bottom}");
            assert!(bottom.contains("without arguments"), "{bottom}");
            for secret in ["user:password", "token", "hidden", "private"] {
                assert!(!bottom.contains(secret), "{bottom}");
            }
            assert_eq!(state.conversation.snapshot()["draft"], draft);
            assert_eq!(
                state.conversation.target.as_deref(),
                Some("example-builder")
            );
            assert_eq!(state.chat_data()["queue"], queue);
            assert_eq!(frame.rows.len(), 24);
        }
    }

    #[test]
    fn joined_emoji_notices_keep_the_full_chat_footer() {
        for (columns, notice) in [
            (100, "\u{1f469}\u{200d}\u{1f4bb}".to_owned()),
            (20, "\u{1f469}\u{200d}\u{1f4bb}".repeat(8)),
        ] {
            let mut state = preview(24, columns);
            state.ui.color = false;
            state.server_origin = super::super::footer::Origin::parse("https://ssmcp.lmm.best/mcp");
            state.notice = Some(notice.clone());
            let frame = state.chat_frame(Instant::now());
            let bottom = frame.rows.last().unwrap();
            assert!(bottom.contains(notice.as_str()), "{columns}: {bottom:?}");
            assert_eq!(bottom.width(), usize::from(columns), "{bottom:?}");
            if columns == 100 {
                assert!(
                    bottom.trim_end().ends_with("https://ssmcp.lmm.best"),
                    "{bottom:?}"
                );
            }
            // Exercise the measured row encoder as well as the footer string.
            assert!(!frame.diff(None).is_empty());
        }
    }
}
