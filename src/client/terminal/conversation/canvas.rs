//! A horizontal strip of views. Columns retain positions, never copies of messages.
use super::*;

pub(super) struct Canvas {
    columns: Vec<Column>,
    active: usize,
}

struct Column {
    view: View,
    position: Option<Position>,
}

struct Position {
    content_focus: bool,
    selected: usize,
    selected_agent: Option<String>,
    detail: bool,
    detail_scroll: usize,
    question_id: Option<String>,
    history: Option<std::sync::Arc<crate::client::interface::history::Conversation>>,
    audit: crate::client::audit_view::AuditPosition,
    events: events::EventPosition,
}

impl Default for Canvas {
    fn default() -> Self {
        Self {
            columns: [
                View::Agents,
                View::Activity,
                View::Audit,
                View::Queue,
                View::Questions,
                View::History,
            ]
            .into_iter()
            .map(|view| Column {
                view,
                position: None,
            })
            .collect(),
            active: 1,
        }
    }
}

impl Canvas {
    pub(super) fn snapshot(&self) -> Value {
        json!({"active":self.active,"columns":self.columns.iter().map(|column| column.view.name()).collect::<Vec<_>>()})
    }

    pub(super) fn label(&self, width: u16) -> String {
        let current = self.columns[self.active].view.name();
        let count = self.columns.len();
        let position = self.active + 1;
        let previous = self
            .active
            .checked_sub(1)
            .map(|index| self.columns[index].view.name());
        let next = self
            .columns
            .get(self.active + 1)
            .map(|column| column.view.name());
        if width >= 86 {
            format!(
                "{}  {current}  {position}/{count}  {}",
                previous
                    .map(|name| format!("‹ {name}   "))
                    .unwrap_or_default(),
                next.map(|name| format!("   {name} ›")).unwrap_or_default()
            )
        } else {
            format!(
                "{} {current}  {position}/{count} {}",
                if previous.is_some() { "‹" } else { "" },
                if next.is_some() { "›" } else { "" }
            )
        }
    }
}

impl State {
    fn canvas_save(&mut self) {
        let position = Position {
            content_focus: self.conversation.content_focus,
            selected: self.conversation.selected,
            selected_agent: self.conversation.selected_agent.clone(),
            detail: self.conversation.detail,
            detail_scroll: self.conversation.detail_scroll,
            question_id: self.conversation.question_id.clone(),
            history: self.conversation.history.clone(),
            audit: self.audit.position(),
            events: self.events.position(),
        };
        self.conversation.canvas.columns[self.conversation.canvas.active].position = Some(position);
    }

    fn canvas_switch(&mut self, index: usize) {
        if index == self.conversation.canvas.active
            || index >= self.conversation.canvas.columns.len()
        {
            return;
        }
        self.canvas_save();
        self.canvas_restore(index);
    }

    fn canvas_restore(&mut self, index: usize) {
        self.conversation.canvas.active = index;
        let column = &mut self.conversation.canvas.columns[index];
        self.conversation.view = column.view;
        if let Some(position) = column.position.take() {
            self.conversation.content_focus = position.content_focus;
            self.conversation.selected = position.selected;
            self.conversation.selected_agent = position.selected_agent;
            self.conversation.detail = position.detail;
            self.conversation.detail_scroll = position.detail_scroll;
            self.conversation.question_id = position.question_id;
            self.conversation.history = position.history;
            self.audit.restore_position(&position.audit);
            self.events.restore_position(&position.events);
        } else {
            self.conversation.content_focus = column.view != View::Activity;
            self.conversation.selected = 0;
            self.conversation.selected_agent = None;
            self.conversation.detail = false;
            self.conversation.detail_scroll = 0;
            if column.view == View::Questions {
                self.conversation.question_id = None;
            }
            if column.view == View::Activity {
                self.events
                    .restore_position(&events::EventPosition::default());
            }
            if column.view == View::Audit {
                self.audit.restore_position(&Default::default());
            }
            if column.view == View::History {
                self.conversation.history = None;
            }
            if column.view == View::Agents {
                self.chat_select_target();
            }
        }
        self.conversation.history_width = 0;
        self.conversation.history_rows.clear();
        self.conversation.drawn_agents.clear();
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        self.help = false;
        self.notice = None;
        if self.conversation.view == View::History && !self.preview {
            match crate::client::interface::history::sessions() {
                Ok(sessions) => self.conversation.sessions = sessions,
                Err(error) => self.notice = Some(format!("Could not read history: {error}")),
            }
        }
        self.dirty = true;
    }

    pub(super) fn canvas_focus(&mut self, view: View) {
        if self.conversation.canvas.columns[self.conversation.canvas.active].view == view {
            if self.conversation.view != view {
                self.conversation.view = view;
                self.dirty = true;
            }
            return;
        }
        let index = self
            .conversation
            .canvas
            .columns
            .iter()
            .position(|column| column.view == view);
        if let Some(index) = index {
            self.canvas_switch(index);
        } else {
            self.canvas_new(view);
        }
    }

    pub(super) fn canvas_move(&mut self, delta: isize) {
        self.conversation.logs_open = false;
        self.raw_logs = false;
        let index = self
            .conversation
            .canvas
            .active
            .saturating_add_signed(delta)
            .min(self.conversation.canvas.columns.len() - 1);
        self.canvas_switch(index);
        self.dirty = true;
    }

    pub(super) fn canvas_new(&mut self, view: View) {
        self.canvas_save();
        let index = self.conversation.canvas.active + 1;
        self.conversation.canvas.columns.insert(
            index,
            Column {
                view,
                position: None,
            },
        );
        self.canvas_restore(index);
    }

    pub(super) fn canvas_close(&mut self) {
        if self.conversation.canvas.columns.len() == 1 {
            self.notice = Some("Keep one panel open · /new opens another".into());
            return;
        }
        let index = self.conversation.canvas.active;
        self.conversation.canvas.columns.remove(index);
        self.canvas_restore(index.saturating_sub(1));
    }

    pub(super) fn logs_toggle(&mut self) {
        self.conversation.logs_open = !self.conversation.logs_open;
        self.raw_logs = self.conversation.logs_open;
        self.notice = None;
        self.dirty = true;
    }

    /// The log panel owns navigation while open; ordinary text returns to the editor.
    pub(super) fn logs_key(&mut self, key: Key) {
        let page = isize::try_from(body_rows(self.geometry.rows))
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1);
        match key {
            Key::Escape => self.logs_toggle(),
            Key::AuditUp => self.scroll_logs(1),
            Key::AuditDown => self.scroll_logs(-1),
            Key::Up => self.scroll_logs(page),
            Key::Down => self.scroll_logs(-page),
            Key::Home | Key::HistoryStart => self.scroll_logs(isize::MAX),
            Key::End | Key::HistoryEnd => self.scroll_logs(isize::MIN),
            Key::Filter => {
                self.log_level = (self.log_level + 1) % 3;
                self.filter_offset = 0;
            }
            Key::Mouse {
                button,
                release: false,
                ..
            } if button & 64 != 0 => {
                self.scroll_logs(if button & 1 == 0 { 3 } else { -3 });
            }
            _ => {}
        }
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let mut state = State::new(
            24,
            100,
            &Ui {
                color: false,
                ..Ui::default()
            },
        );
        state.preview = true;
        state
    }

    fn render_events(state: &mut State) -> Value {
        let _ = state.events.render(80, 7, &state.ui, false);
        let snapshot = state.events.snapshot();
        json!({
            "selected": snapshot["selected"],
            "scroll": snapshot["scroll"],
            "following": snapshot["following"],
        })
    }

    #[test]
    fn chat_columns_restore_independent_reading_positions() {
        let mut state = state();
        state.events.set_scope(events::EventScope::Conversation);
        for index in 0..12 {
            state.events.ingest_event(
                "user",
                &json!({"type":"ssmcp.message","role":"user","text":format!("message {index}\nsecond line")}),
                b"",
            );
        }
        render_events(&mut state);
        state.events.scroll_by(-12);
        let earlier = render_events(&mut state);
        assert_eq!(earlier["following"], false);
        state.canvas_new(View::Activity);
        let latest = render_events(&mut state);
        assert_eq!(latest["following"], true);
        assert!(latest["scroll"].as_u64().unwrap() > earlier["scroll"].as_u64().unwrap());
        state.canvas_move(-1);
        assert_eq!(render_events(&mut state), earlier);
        state.canvas_move(1);
        assert_eq!(render_events(&mut state), latest);

        state.canvas_new(View::Queue);
        state.conversation.selected = 2;
        state.conversation.detail = true;
        state.conversation.detail_scroll = 17;
        state.canvas_new(View::Queue);
        assert_eq!(state.conversation.selected, 0);
        assert!(!state.conversation.detail);
        state.conversation.selected = 5;
        state.conversation.detail_scroll = 3;
        state.canvas_move(-1);
        assert_eq!(state.conversation.selected, 2);
        assert!(state.conversation.detail);
        assert_eq!(state.conversation.detail_scroll, 17);
        state.canvas_move(1);
        assert_eq!(state.conversation.selected, 5);
        assert!(!state.conversation.detail);
        assert_eq!(state.conversation.detail_scroll, 3);
    }

    #[test]
    fn canvas_arrows_stop_at_both_edges() {
        let mut state = state();
        state.canvas_move(isize::MIN);
        assert_eq!(state.conversation.canvas.active, 0);
        state.chat_key(Key::CanvasLeft);
        assert_eq!(state.conversation.canvas.active, 0);
        assert_eq!(state.conversation.view, View::Agents);
        state.canvas_move(isize::MAX);
        let last = state.conversation.canvas.columns.len() - 1;
        assert_eq!(state.conversation.canvas.active, last);
        state.chat_key(Key::CanvasRight);
        assert_eq!(state.conversation.canvas.active, last);
        assert_eq!(state.conversation.view, View::History);
    }

    #[test]
    fn new_and_close_change_views_without_removing_conversation_data() {
        let mut state = state();
        assert!(state.conversation.composer.set_text("保留 draft"));
        assert!(state.conversation.composer.left());
        state.conversation.target = Some("example-builder".into());
        state.conversation.editing = Some("example-request".into());
        state
            .conversation
            .sessions
            .push(crate::client::interface::history::SessionSummary {
                id: "session-kept".into(),
                workspace: "/fixture".into(),
                title: "Retained conversation".into(),
                created_at_ms: 1,
                updated_at_ms: 2,
                message_count: 1,
                truncated_messages: 0,
                current: true,
            });
        let sessions = state.conversation.sessions.clone();
        state.events.ingest_event(
            "agent",
            &json!({"type":"ssmcp.message","text":"A retained reply."}),
            b"",
        );
        let data = state.chat_data();
        assert_eq!(data["queue"][0]["id"], "example-request");
        assert_eq!(data["agents"][0]["agent_id"], "example-builder");
        let draft = (
            state.conversation.composer.text().to_owned(),
            state.conversation.composer.cursor(),
            state.conversation.target.clone(),
            state.conversation.editing.clone(),
        );
        let columns = state.conversation.canvas.columns.len();
        assert!(state.chat_command("/new queue").unwrap());
        assert_eq!(state.conversation.canvas.columns.len(), columns + 1);
        assert_eq!(state.conversation.view, View::Queue);
        assert!(state.chat_command("/close").unwrap());
        assert_eq!(state.conversation.canvas.columns.len(), columns);
        assert_eq!(state.conversation.view, View::Activity);
        while state.conversation.canvas.columns.len() > 1 {
            assert!(state.chat_command("/close").unwrap());
        }
        assert!(state.chat_command("/close").unwrap());
        assert_eq!(state.conversation.canvas.columns.len(), 1);
        assert_eq!(state.chat_data(), data);
        assert_eq!(state.conversation.sessions, sessions);
        assert_eq!(state.events.snapshot()["retained_events"], 1);
        assert_eq!(
            (
                state.conversation.composer.text().to_owned(),
                state.conversation.composer.cursor(),
                state.conversation.target.clone(),
                state.conversation.editing.clone(),
            ),
            draft
        );
    }

    #[test]
    fn right_click_logs_and_escape_preserve_the_underlying_column() {
        let mut state = state();
        state.canvas_focus(View::Queue);
        let data = state.chat_data();
        assert_eq!(data["queue"][0]["id"], "example-request");
        for index in 0..30 {
            state.log(format!("background diagnostic {index}\n").as_bytes());
        }
        state.conversation.content_focus = true;
        state.conversation.selected = data["queue"].as_array().unwrap().len() - 1;
        state.conversation.detail = true;
        state.conversation.detail_scroll = 11;
        state.conversation.target = Some("recipient-kept".into());
        assert!(state.conversation.composer.set_text("继续草稿"));
        let before = (
            state.conversation.canvas.active,
            state.conversation.selected,
            state.conversation.detail,
            state.conversation.detail_scroll,
            state.conversation.content_focus,
            state.conversation.target.clone(),
            state.conversation.composer.text().to_owned(),
        );
        state.chat_key(Key::Mouse {
            button: 2,
            x: 3,
            y: body_top(state.geometry.rows) + 1,
            release: false,
        });
        assert!(state.conversation.logs_open && state.raw_logs);
        state.chat_key(Key::Up);
        state.chat_key(Key::Escape);
        assert!(!state.conversation.logs_open && !state.raw_logs);
        assert_eq!(state.conversation.view, View::Queue);
        assert_eq!(state.chat_data(), data);
        assert_eq!(
            (
                state.conversation.canvas.active,
                state.conversation.selected,
                state.conversation.detail,
                state.conversation.detail_scroll,
                state.conversation.content_focus,
                state.conversation.target.clone(),
                state.conversation.composer.text().to_owned(),
            ),
            before
        );
    }
    #[test]
    fn tool_expansion_belongs_to_each_chat_column() {
        let mut state = state();
        state.events.set_scope(events::EventScope::Conversation);
        state.events.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"job","command":["cargo","test"]}),
            b"[RUN ] cargo test\nretained tool detail\n",
        );
        render_events(&mut state);
        assert!(state.events.open());
        state.canvas_new(View::Activity);
        assert_eq!(state.events.snapshot()["events"][0]["expanded"], false);
        assert!(state.events.open());
        state.canvas_move(-1);
        assert_eq!(state.events.snapshot()["events"][0]["expanded"], true);
        assert!(state.events.back());
        state.canvas_move(1);
        assert_eq!(state.events.snapshot()["events"][0]["expanded"], true);
        state.canvas_move(-1);
        assert_eq!(state.events.snapshot()["events"][0]["expanded"], false);
        assert_eq!(state.events.snapshot()["retained_events"], 1);
    }

    #[test]
    fn new_audit_column_starts_at_the_list_and_restores_the_old_detail() {
        let mut state = state();
        for id in ["audit-one", "audit-two"] {
            assert!(state.audit.ingest(&json!({
                "type":"ssmcp.code_mode", "run_id":id, "phase":"started",
                "code":"let value = 1;\n".repeat(40)
            })));
        }
        state.canvas_focus(View::Audit);
        assert!(state.audit.open_run("audit-one"));
        let _ = state.audit.render(80, 7, &state.ui);
        state.audit.page_down(4);
        let before = state.audit.snapshot();
        assert_eq!(before["detail"], true);
        assert!(before["scroll"].as_u64().unwrap() > 0);
        state.canvas_new(View::Audit);
        let fresh = state.audit.snapshot();
        assert_eq!(fresh["detail"], false);
        assert_eq!(fresh["scroll"], 0);
        assert_eq!(fresh["selected_index"], 0);
        assert_eq!(fresh["stored_bytes"], before["stored_bytes"]);
        state.canvas_move(-1);
        let restored = state.audit.snapshot();
        assert_eq!(restored["selected"], before["selected"]);
        assert_eq!(restored["detail"], true);
        assert_eq!(restored["scroll"], before["scroll"]);
    }

    #[test]
    fn history_columns_restore_their_own_shared_source_and_rebuild_rows() {
        use crate::client::interface::history::{
            Conversation as SavedConversation, Message, SessionMetadata,
        };
        use std::sync::Arc;

        let history = |id: &str, content: &str| {
            Arc::new(SavedConversation {
                metadata: SessionMetadata {
                    schema: "test.history".into(),
                    schema_version: 1,
                    id: id.into(),
                    workspace: "/fixture".into(),
                    created_at_ms: 0,
                    truncated_messages: 0,
                },
                messages: vec![Message {
                    id: format!("{id}-message"),
                    role: "assistant".into(),
                    content: content.into(),
                    kind: "answer".into(),
                    created_at_ms: 0,
                    truncated: false,
                }],
            })
        };
        let first = history("first", "First history body.");
        let second = history("second", "Second history body.");
        let mut state = state();
        state
            .conversation
            .sessions
            .push(crate::client::interface::history::SessionSummary {
                id: "first".into(),
                workspace: "/fixture".into(),
                title: "Fixture history".into(),
                created_at_ms: 0,
                updated_at_ms: 0,
                message_count: 1,
                truncated_messages: 0,
                current: false,
            });
        state.canvas_focus(View::History);
        assert_eq!(state.conversation.sessions[0].id, "first");
        assert!(
            state.notice.is_none(),
            "preview must not read global history"
        );
        state.conversation.history = Some(Arc::clone(&first));
        state.conversation.detail = true;
        assert!(
            state
                .chat_history_rows(10)
                .join("\n")
                .contains("First history body.")
        );
        state.canvas_new(View::History);
        assert!(state.conversation.history.is_none());
        assert!(!state.conversation.detail);
        state.conversation.history = Some(Arc::clone(&second));
        state.conversation.detail = true;
        assert!(
            state
                .chat_history_rows(10)
                .join("\n")
                .contains("Second history body.")
        );
        state.canvas_move(-1);
        assert!(Arc::ptr_eq(
            state.conversation.history.as_ref().unwrap(),
            &first
        ));
        let rows = state.chat_history_rows(10).join("\n");
        assert!(rows.contains("First history body.") && !rows.contains("Second history body."));
        state.canvas_move(1);
        assert!(Arc::ptr_eq(
            state.conversation.history.as_ref().unwrap(),
            &second
        ));
        let rows = state.chat_history_rows(10).join("\n");
        assert!(rows.contains("Second history body.") && !rows.contains("First history body."));
    }

    #[test]
    fn output_home_and_end_keep_the_active_duplicate_chat_and_draft() {
        let mut state = state();
        state.canvas_new(View::Activity);
        let active = state.conversation.canvas.active;
        assert!(state.conversation.composer.set_text("Keep this draft."));
        assert!(state.conversation.composer.left());
        let before = (
            state.conversation.composer.text().to_owned(),
            state.conversation.composer.cursor(),
        );
        for key in [Key::HistoryEnd, Key::HistoryStart] {
            state.chat_key(key);
            assert_eq!(state.conversation.canvas.active, active);
            assert_eq!(state.conversation.view, View::Activity);
            assert_eq!(
                (
                    state.conversation.composer.text().to_owned(),
                    state.conversation.composer.cursor()
                ),
                before
            );
        }
    }
}
