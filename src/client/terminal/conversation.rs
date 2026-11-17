//! Conversation-style local controls. No input is ever forwarded to a hidden PTY.
use super::*;
use crate::client::{composer::Composer, steering};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum View {
    #[default]
    Activity,
    Audit,
    Agents,
    Queue,
}
impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Audit => "code audit",
            Self::Agents => "agents",
            Self::Queue => "queue",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Activity => Self::Audit,
            Self::Audit => Self::Agents,
            Self::Agents => Self::Queue,
            Self::Queue => Self::Activity,
        }
    }
}

#[derive(Default)]
pub(super) struct Conversation {
    pub(super) composer: Composer,
    pub(super) view: View,
    pub(super) content_focus: bool,
    selected: usize,
    detail: bool,
    detail_scroll: usize,
    target: Option<String>,
    editing: Option<String>,
    submitting: Option<(String, String)>,
    parked: Option<steering::SavedDraft>,
    text_pending: Vec<u8>,
    paste: Option<Vec<u8>>,
    paste_overflow: bool,
    restored: bool,
}

impl Conversation {
    pub(super) fn snapshot(&self) -> Value {
        json!({"view":match self.view { View::Activity=>"activity",View::Audit=>"audit",View::Agents=>"agents",View::Queue=>"queue" },
            "focus":if self.content_focus {"content"} else {"input"},
            "selected":self.selected,"target":self.target,"editing":self.editing,
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

fn recipients(value: &Value) -> Vec<Value> {
    let mut entries = vec![
        json!({"agent_id":"next","name":"Next caller","role":"default recipient","status":"first eligible tool response"}),
    ];
    entries.extend(items(value, "agents"));
    entries
}

impl State {
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

    fn chat_data(&self) -> Value {
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

    fn chat_updates(&mut self) {
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
        for event in steering::take_events() {
            match event {
                steering::UiEvent::Accepted { id, text } => {
                    self.conversation.composer.remember(&text);
                    let mut record = format!("\n[USER] #{id}\n");
                    for line in text.lines() {
                        let _ = writeln!(record, "  {}", panel::safe_text(line));
                    }
                    record.push_str("  queued · waiting for the next eligible tool response\n\n");
                    self.log(record.as_bytes());
                    let matching =
                        self.conversation.submitting.as_ref().is_some_and(
                            |(pending, submitted)| pending == &id && submitted == &text,
                        );
                    if matching {
                        if self.conversation.composer.text() == text {
                            self.conversation.composer.take_text();
                            if let Some(parked) = self.conversation.parked.take() {
                                self.chat_restore_draft(parked);
                            }
                        }
                        self.conversation.submitting = None;
                        self.conversation.editing = None;
                    }
                    self.notice =
                        Some("Queued · delivered with the next eligible tool response".into());
                }
                steering::UiEvent::Editable { id, text } => {
                    if self
                        .conversation
                        .editing
                        .as_ref()
                        .is_some_and(|editing| editing != &id)
                    {
                        self.notice = Some(
                            "Another request is held for editing · finish the current draft first"
                                .into(),
                        );
                        continue;
                    }
                    if self.conversation.editing.as_ref() != Some(&id) {
                        if !self.conversation.composer.is_empty()
                            && self.conversation.composer.text() != text
                        {
                            self.conversation.parked = Some(self.chat_saved_draft());
                        }
                        self.conversation.composer.set_text(&text);
                    }
                    let data = steering::snapshot();
                    if let Some(record) = items(&data, "queue")
                        .iter()
                        .find(|item| message_id(item) == id)
                    {
                        self.conversation.target = record
                            .get("target")
                            .and_then(Value::as_str)
                            .filter(|target| *target != "next")
                            .map(str::to_owned);
                    }
                    self.conversation.editing = Some(id);
                    self.conversation.submitting = None;
                    self.conversation.content_focus = false;
                    self.notice =
                        Some("Editing · this request is held until you press Enter".into());
                }
                steering::UiEvent::Failed { id, message } => {
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
        self.conversation.view = view;
        self.conversation.content_focus = view != View::Activity;
        self.conversation.selected = 0;
        self.conversation.detail = false;
        self.conversation.detail_scroll = 0;
        self.help = false;
        self.notice = None;
        self.dirty = true;
    }

    fn chat_command(&mut self, text: &str) -> Result<bool, String> {
        if !text.starts_with('/') {
            return Ok(false);
        }
        let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        match command {
            "/help" => {
                self.help = true;
                self.conversation.content_focus = true;
            }
            "/agents" => self.chat_change_view(View::Agents),
            "/queue" => self.chat_change_view(View::Queue),
            "/audit" => self.chat_change_view(View::Audit),
            "/logs" => self.chat_change_view(View::Activity),
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
                    self.log_level = match level {
                        "all" => 0,
                        "commands" => 1,
                        "errors" => 2,
                        _ => {
                            return Err("Use /filter text, or --all / --commands / --errors".into());
                        }
                    };
                } else {
                    if rest.len() > 256 || rest.chars().any(char::is_control) {
                        return Err("A log filter accepts up to 256 bytes of plain text".into());
                    }
                    self.log_query = rest.into();
                }
                self.filter_offset = 0;
                self.chat_change_view(View::Activity);
            }
            "/clear" => {
                self.logs = vt100::Parser::new(
                    nonzero(self.geometry.log_rows),
                    nonzero(self.geometry.left),
                    512,
                );
                self.log_query.clear();
                self.log_level = 0;
                self.filter_offset = 0;
                self.chat_change_view(View::Activity);
            }
            "/reload" => {
                if !rest.is_empty() {
                    return Err("Use /reload without arguments to restart this session".into());
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

    fn chat_submit(&mut self) {
        if self.conversation.submitting.is_some() {
            self.notice = Some("Waiting for queue confirmation · your draft is preserved".into());
            return;
        }
        let text = self.conversation.composer.text().to_owned();
        if text.trim().is_empty() {
            return;
        }
        if self.conversation.editing.is_none() {
            match self.chat_command(&text) {
                Ok(true) => {
                    if text.split_whitespace().next() != Some("/reload") {
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
            Ok(id) => {
                self.conversation.submitting = Some((id, text));
                self.notice = Some("Sending to queue…".into());
            }
            Err(error) => self.notice = Some(error),
        }
    }

    fn chat_selected(&self) -> Option<Value> {
        let data = self.chat_data();
        let entries = if self.conversation.view == View::Agents {
            recipients(&data)
        } else {
            items(&data, "queue")
        };
        entries.get(self.conversation.selected).cloned()
    }

    fn chat_open(&mut self, edit: bool) {
        if edit
            && self.conversation.view == View::Queue
            && (self.conversation.editing.is_some() || self.conversation.submitting.is_some())
        {
            self.notice =
                Some("Finish the current draft before editing another queued request".into());
            self.conversation.content_focus = false;
            return;
        }
        match self.conversation.view {
            View::Audit => self.audit.open(),
            View::Agents => {
                if let Some(agent) = self.chat_selected() {
                    self.conversation.target = (field(&agent, "agent_id") != "next")
                        .then(|| field(&agent, "agent_id").into());
                    self.conversation.content_focus = false;
                    self.notice = Some(format!("To {}", panel::safe_text(field(&agent, "name"))));
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
            View::Activity => {}
        }
    }

    fn chat_move(&mut self, delta: isize) {
        match self.conversation.view {
            View::Activity => self.scroll_logs(-delta),
            View::Audit => self.audit.scroll_by(delta),
            View::Queue if self.conversation.detail => {
                self.conversation.detail_scroll =
                    self.conversation.detail_scroll.saturating_add_signed(delta);
            }
            view => {
                let data = self.chat_data();
                let count = if view == View::Agents {
                    recipients(&data).len()
                } else {
                    items(&data, "queue").len()
                };
                self.conversation.selected = self
                    .conversation
                    .selected
                    .saturating_add_signed(delta)
                    .min(count.saturating_sub(1));
            }
        }
    }

    fn chat_key(&mut self, key: Key) {
        let content = self.conversation.content_focus;
        match key {
            Key::Help => {
                self.help = !self.help;
                self.conversation.content_focus = self.help;
            }
            Key::Layout => self.chat_change_view(self.conversation.view.next()),
            Key::Sidebar => self.actions.push("sidebar.toggle"),
            Key::Theme => self.actions.push("theme.next"),
            Key::Reload => self.actions.push("reload"),
            Key::Save => self.actions.push("save"),
            Key::Filter => {
                self.log_level = (self.log_level + 1) % 3;
                self.filter_offset = 0;
            }
            Key::FocusBack => self.conversation.content_focus = !content,
            Key::Up => self.chat_move(
                -(isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::Down => self.chat_move(
                (isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::HistoryStart => {
                self.scroll_logs(isize::MAX);
                self.chat_change_view(View::Activity);
            }
            Key::HistoryEnd => {
                self.scroll_logs(isize::MIN);
                self.chat_change_view(View::Activity);
            }
            Key::AuditUp if content => self.chat_move(-1),
            Key::AuditDown if content => self.chat_move(1),
            Key::AuditUp => {
                if self.conversation.submitting.is_some() {
                    self.notice =
                        Some("Waiting for queue confirmation · your draft is preserved".into());
                    return;
                }
                if !self.conversation.composer.up() {
                    let data = self.chat_data();
                    let queued = items(&data, "queue")
                        .into_iter()
                        .rev()
                        .find(|entry| field(entry, "status") == "queued");
                    if self.conversation.editing.is_none() && !self.preview && queued.is_some() {
                        if let Err(error) = steering::take_back(message_id(&queued.unwrap())) {
                            self.notice = Some(error);
                        }
                    } else if self.conversation.editing.is_none() {
                        self.conversation.composer.history_previous();
                    }
                }
            }
            Key::AuditDown => {
                if !self.conversation.composer.down() && self.conversation.editing.is_none() {
                    self.conversation.composer.history_next();
                }
            }
            Key::Back if content => {
                self.audit.back();
                self.conversation.detail = false;
                self.conversation.detail_scroll = 0;
            }
            Key::Back => {
                self.conversation.composer.left();
            }
            Key::Open if content => self.chat_open(false),
            Key::Open => {
                self.conversation.composer.right();
            }
            Key::Home if content => self.chat_move(isize::MIN.saturating_add(1)),
            Key::End if content => self.chat_move(isize::MAX),
            Key::Home => {
                self.conversation.composer.home();
            }
            Key::End => {
                self.conversation.composer.end();
            }
            Key::Delete if content && self.conversation.view == View::Queue => {
                if let Some(message) = self.chat_selected() {
                    if let Err(error) = steering::cancel(message_id(&message)) {
                        self.notice = Some(error);
                    }
                }
            }
            Key::Delete => {
                self.conversation.composer.delete();
            }
            Key::Newline => {
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
                    self.chat_move(if button & 1 == 0 { -3 } else { 3 });
                } else if y > prompt_top(self.geometry.rows) {
                    self.conversation.content_focus = false;
                } else {
                    self.conversation.content_focus = true;
                    if matches!(self.conversation.view, View::Agents | View::Queue)
                        && !self.conversation.detail
                    {
                        self.conversation.selected =
                            usize::from(y.saturating_sub(body_top(self.geometry.rows) + 1)) / 2;
                    }
                    if button == 2 {
                        self.chat_open(false);
                    } else if button == 0 && self.conversation.view == View::Audit {
                        self.audit.back();
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_escape(&mut self) {
        self.help = false;
        if self.conversation.detail {
            self.conversation.detail = false;
        } else {
            self.audit.back();
            self.conversation.content_focus = false;
        }
        self.notice = None;
    }

    fn chat_character(&mut self, character: char) {
        match character {
            'r' | 'R'
                if self.conversation.content_focus && self.conversation.view == View::Queue =>
            {
                if let Some(message) = self.chat_selected() {
                    let recoverable = matches!(
                        field(&message, "status"),
                        "uncertain" | "retarget-required" | "offline"
                    );
                    if self.conversation.submitting.is_some()
                        || self
                            .conversation
                            .editing
                            .as_ref()
                            .is_some_and(|editing| !recoverable || editing != message_id(&message))
                    {
                        self.notice =
                            Some("Finish the current draft before starting another request".into());
                        self.conversation.content_focus = false;
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
                        self.conversation.composer.set_text(field(&message, "text"));
                        self.conversation.editing = None;
                        self.conversation.target = None;
                        self.conversation.content_focus = false;
                        self.notice =
                            Some("Copied to a new draft · Enter sends a new request".into());
                    }
                }
            }
            '\u{1b}' => self.chat_escape(),
            '\t' => {
                let suggestions = self.conversation.composer.suggestions();
                if !self.conversation.content_focus && !suggestions.is_empty() {
                    let completion = format!("{} ", suggestions[0].name);
                    self.conversation.composer.set_text(&completion);
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
                if self.conversation.composer.text().ends_with('\\') {
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
                self.conversation.content_focus = false;
                if !self.conversation.composer.insert(&ch.to_string()) {
                    self.notice = Some("Draft limit reached · 16 KiB".into());
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_output(&mut self, output: &[u8]) {
        if output == b"\x1b[200~" {
            self.conversation.paste = Some(Vec::new());
            self.conversation.paste_overflow = false;
            return;
        }
        if output == b"\x1b[201~" {
            if let Some(paste) = self.conversation.paste.take() {
                if self.conversation.paste_overflow
                    || !self
                        .conversation
                        .composer
                        .paste(&String::from_utf8_lossy(&paste))
                {
                    self.notice = Some(
                        "Paste exceeds the 16 KiB draft limit; the draft was preserved".into(),
                    );
                }
            }
            self.dirty = true;
            return;
        }
        if let Some(paste) = &mut self.conversation.paste {
            if paste.len().saturating_add(output.len()) <= crate::client::composer::MAX_DRAFT_BYTES
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
        self.chat_updates();
        // Keep text and navigation in their original order, including coalesced reads.
        for byte in bytes {
            let (output, keys) = self.keyboard.feed(&[*byte], now, false, true, false);
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        if flush {
            let (output, keys) = self.keyboard.feed(&[], now, true, true, false);
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        self.chat_persist_draft();
        Vec::new()
    }

    fn chat_line(&self, text: &str, tone: PanelTone, width: u16) -> String {
        let plain = fit(text, width);
        self.tone(
            tone,
            &format!(
                "{plain}{}",
                " ".repeat(usize::from(width).saturating_sub(plain.width()))
            ),
        )
    }

    fn chat_list_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let width = self.geometry.left;
        let queue = self.conversation.view == View::Queue;
        let entries = if queue {
            items(data, "queue")
        } else {
            recipients(data)
        };
        self.conversation.selected = self
            .conversation
            .selected
            .min(entries.len().saturating_sub(1));
        let mut rows = vec![self.chat_line(
            if queue {
                "  Requests · Enter edit · → details · Delete cancel · R resend"
            } else {
                "  Connected agents · Enter chooses a recipient"
            },
            PanelTone::Muted,
            width,
        )];
        if entries.is_empty() {
            rows.push(self.chat_line(
                if queue {
                    "  No queued requests. Write a requirement below."
                } else {
                    "  No registered agents. Call collab.join to join this session."
                },
                PanelTone::Normal,
                width,
            ));
            return rows;
        }
        if queue && self.conversation.detail {
            let entry = &entries[self.conversation.selected];
            rows.push(self.chat_line(
                &format!(
                    "  {} · {} · ← back",
                    field(entry, "status"),
                    message_id(entry)
                ),
                PanelTone::Accent,
                width,
            ));
            let mut wrapped = Vec::new();
            for line in field(entry, "text").split('\n') {
                let mut part = String::from("  ");
                let mut used = 2;
                for ch in panel::safe_text(line).chars() {
                    let n = ch.width().unwrap_or(0);
                    if used + n > usize::from(width) && used > 2 {
                        wrapped.push(part);
                        part = "  ".into();
                        used = 2;
                    }
                    part.push(ch);
                    used += n;
                }
                wrapped.push(part);
            }
            self.conversation.detail_scroll = self.conversation.detail_scroll.min(
                wrapped
                    .len()
                    .saturating_sub(usize::from(height).saturating_sub(2)),
            );
            rows.extend(
                wrapped
                    .into_iter()
                    .skip(self.conversation.detail_scroll)
                    .map(|line| self.chat_line(&line, PanelTone::Normal, width)),
            );
            return rows;
        }
        let capacity = usize::from(height.saturating_sub(1) / 2).max(1);
        let start = self.conversation.selected.saturating_sub(capacity - 1);
        for (index, entry) in entries.iter().enumerate().skip(start).take(capacity) {
            let marker = if index == self.conversation.selected {
                "›"
            } else {
                " "
            };
            let title = if queue {
                format!(
                    "{marker} {}  ·  {}",
                    field(entry, "status"),
                    message_id(entry)
                )
            } else {
                format!(
                    "{marker} {}  ·  {}",
                    field(entry, "name"),
                    field(entry, "role")
                )
            };
            rows.push(self.chat_line(
                &title,
                if index == self.conversation.selected {
                    PanelTone::Accent
                } else {
                    PanelTone::Normal
                },
                width,
            ));
            let subtitle = if queue {
                format!("  {}", field(entry, "text").lines().next().unwrap_or(""))
            } else {
                format!(
                    "  {} · reported {}",
                    field(entry, "agent_id"),
                    field(entry, "status")
                )
            };
            rows.push(self.chat_line(&subtitle, PanelTone::Muted, width));
        }
        rows
    }

    pub(super) fn chat_frame(&mut self, now: Instant) -> Frame {
        self.chat_updates();
        let g = self.geometry;
        let data = self.chat_data();
        let agents = items(&data, "agents");
        let queue = items(&data, "queue");
        let pending = queue
            .iter()
            .filter(|item| matches!(field(item, "status"), "queued" | "sending" | "editing"))
            .count();
        let top = body_top(g.rows);
        let height = body_rows(g.rows);
        let mut body = match self.conversation.view {
            View::Activity => self.filtered_logs().unwrap_or_else(|| {
                (0..height)
                    .map(|row| screen_row(self.logs.screen(), row, g.left, &self.ui))
                    .collect()
            }),
            View::Audit => self.audit.render(g.left, height, &self.ui),
            _ => self.chat_list_rows(&data, height),
        };
        if self.help {
            let help = [
                "  ssmcp · keyboard and commands",
                "",
                "  Enter      Queue a new requirement",
                "  Up         Recall queued text or input history",
                "  Ctrl+J     New line · pasted text stays in the draft",
                "  Tab        Switch input / content focus",
                "  Ctrl+O     Source audit · Enter/→ open · ← back",
                "  F2         Activity / audit / agents / queue",
                "  Wheel      Scroll directly · PgUp/PgDn also work",
                "  F3         Collapse sidebar · F4 change theme",
                "  F5         Reload appearance · F8 save appearance",
                "  /filter    Text search · F9 all / commands / errors",
                "  /agents    Select a real registered recipient",
                "  /queue     Inspect, take back, edit or cancel requests",
                "  /reload    Restart client session and restore pending work",
                "",
                "  Queued means waiting. Delivered means included in a response.",
                "  ssmcp does not infer that a model read or executed a request.",
            ];
            body = help
                .iter()
                .map(|line| self.chat_line(line, PanelTone::Normal, g.left))
                .collect();
        }
        body.resize_with(usize::from(height), || {
            self.chat_line("", PanelTone::Normal, g.left)
        });
        body.truncate(usize::from(height));
        let prompt = self
            .conversation
            .composer
            .rows(g.left, prompt_rows(g.rows).max(1), &self.ui);
        let label = self
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
        let mut rows = Vec::new();
        for index in 0..g.rows {
            let line = if g.rows >= 3 && index == g.rows - 1 {
                let status = if self.preview {
                    "preview · sample data"
                } else if !data["connected"].as_bool().unwrap_or(false) {
                    "disconnected"
                } else if !data["supported"].as_bool().unwrap_or(false) {
                    "server update required for queue"
                } else {
                    "connected"
                };
                let hint = self.notice.clone().unwrap_or_else(|| {
                    format!(
                        "{} · {status} · {} agents · {pending} queued · Tab focus · /help",
                        if self.conversation.content_focus {
                            "browse"
                        } else {
                            "input"
                        },
                        agents.len()
                    )
                });
                rows.push(self.chat_line(&format!(" {hint}"), PanelTone::Muted, g.cols));
                continue;
            } else if top > 0 && index == 0 {
                let suffix = if !self.log_query.is_empty() || self.log_level != 0 {
                    format!(
                        " · {} /{}",
                        ["all", "commands", "errors"][usize::from(self.log_level)],
                        self.log_query
                    )
                } else {
                    String::new()
                };
                self.chat_line(
                    &format!("  ssmcp  /  {}{suffix}", self.conversation.view.name()),
                    PanelTone::Heading,
                    g.left,
                )
            } else if index >= top && index < top + height {
                body[usize::from(index - top)].clone()
            } else if index == prompt_top(g.rows) && prompt_rows(g.rows) > 0 {
                let title = format!(
                    "─ {} · {} ",
                    if self.conversation.editing.is_some() {
                        "Editing"
                    } else {
                        "To"
                    },
                    label
                );
                let mut border = fit(&title, g.left);
                border.push_str(&"─".repeat(usize::from(g.left).saturating_sub(border.width())));
                self.chat_line(&border, PanelTone::Muted, g.left)
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
            let mut sidebar = Vec::new();
            for kind in &self.ui.panels {
                let mut section = match kind {
                    crate::client::ui::Panel::Agents => {
                        let mut section =
                            vec![(format!(" Agents · {}", agents.len()), PanelTone::Heading)];
                        for agent in agents.iter().take(4) {
                            section.push((format!(" {}", field(agent, "name")), PanelTone::Accent));
                            section.push((
                                format!("   reported {}", field(agent, "status")),
                                PanelTone::Muted,
                            ));
                        }
                        if agents.is_empty() {
                            section
                                .push((" /agents to choose a recipient".into(), PanelTone::Muted));
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
                        let mut section =
                            panel::render_configured(&self.panel, rect.width, g.rows, now, &ui);
                        if section.first().is_some_and(|row| row.text == "Ready") {
                            section.clear();
                        }
                        while section.last().is_some_and(|row| row.text.trim().is_empty()) {
                            section.pop();
                        }
                        section
                            .into_iter()
                            .map(|row| (row.text, row.tone))
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
                let (text, tone) = sidebar
                    .get(index)
                    .cloned()
                    .unwrap_or((String::new(), PanelTone::Normal));
                line.push_str(&self.tone(PanelTone::Muted, "│"));
                line.push_str(&self.chat_line(&text, tone, rect.width));
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
    fn slash_views_and_arrow_selection_preserve_unsubmitted_text() {
        let mut state = preview(36, 120);
        input(&mut state, b"/agents\r\x1b[B\r");
        assert_eq!(state.conversation.view, View::Agents);
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
    fn mouse_wheel_scrolls_without_focus_switch_or_draft_loss() {
        let mut state = preview(24, 100);
        for index in 0..80 {
            state.log(format!("event {index}\n").as_bytes());
        }
        input(&mut state, b"unfinished");
        assert_eq!(state.logs.screen().scrollback(), 0);
        input(&mut state, b"\x1b[<64;10;5M");
        assert!(state.logs.screen().scrollback() > 0);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "unfinished");
        input(&mut state, b"\x1b[1;5F");
        assert_eq!(state.logs.screen().scrollback(), 0);
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
    fn sidebar_sections_obey_configuration_and_log_filters_keep_results() {
        let mut state = preview(36, 120);
        state.ui.panels = vec![crate::client::ui::Panel::Queue];
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Queue · 1 pending"));
        assert!(!screen.contains("Agents · 2"));
        input(&mut state, b"/filter cargo\r");
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("cargo test --lib"));
        assert!(screen.contains("exit=0"));
        assert!(screen.contains("custom_colors ... ok"));
        assert!(!screen.contains("A quieter workspace"));
        assert!(
            !screen.contains("[RUN]"),
            "conversation presentation uses quiet headings"
        );
    }
}
