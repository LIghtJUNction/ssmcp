//! A horizontal strip of views. Columns retain positions, never copies of messages.
use super::*;

pub(super) struct Canvas {
    columns: Vec<Column>,
    active: usize,
    next_id: u64,
    logs_return: Option<u64>,
}

struct Column {
    id: u64,
    view: View,
    position: Option<Position>,
}

#[derive(Default)]
struct LogPosition {
    scrollback: usize,
    query: String,
    level: u8,
    filter_offset: usize,
}

struct Position {
    content_focus: bool,
    selected: usize,
    selected_agent: Option<String>,
    detail: bool,
    detail_scroll: usize,
    question_id: Option<String>,
    history: Option<std::sync::Arc<crate::client::interface::history::Conversation>>,
    history_archived: bool,
    audit: crate::client::audit_view::AuditPosition,
    events: events::EventPosition,
    logs: Option<LogPosition>,
    tasks: Option<tasks::TaskPosition>,
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
                View::Logs,
                View::Tasks,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, view)| Column {
                id: index as u64 + 1,
                view,
                position: None,
            })
            .collect(),
            active: 1,
            next_id: 9,
            logs_return: None,
        }
    }
}

impl Canvas {
    fn index(&self, id: u64) -> Option<usize> {
        self.columns.iter().position(|column| column.id == id)
    }

    fn insert(&mut self, index: usize, view: View) {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("canvas column id exhausted");
        self.columns.insert(
            index,
            Column {
                id,
                view,
                position: None,
            },
        );
        if index <= self.active {
            self.active += 1;
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        json!({
            "active": self.active,
            "columns": self.columns.iter().map(|column| column.view.name()).collect::<Vec<_>>(),
            "column_ids": self.columns.iter().map(|column| column.id).collect::<Vec<_>>(),
            "logs_return": self.logs_return,
        })
    }

    pub(super) fn is_default_conversation(&self) -> bool {
        self.active == 1
            && self.columns.len() == 8
            && self
                .columns
                .iter()
                .enumerate()
                .all(|(index, column)| column.id == index as u64 + 1)
    }

    pub(super) fn label(&self, _width: u16) -> String {
        let current = self.columns[self.active].view.name();
        format!("{current}  {}/{}", self.active + 1, self.columns.len())
    }
}

impl State {
    pub(super) fn canvas_sync_logs(&mut self) {
        self.conversation.logs_open = self.conversation.view == View::Logs;
        self.raw_logs = self.conversation.logs_open;
    }

    fn canvas_save(&mut self) {
        let position = Position {
            content_focus: self.conversation.content_focus,
            selected: self.conversation.selected,
            selected_agent: self.conversation.selected_agent.clone(),
            detail: self.conversation.detail,
            detail_scroll: self.conversation.detail_scroll,
            question_id: self.conversation.question_id.clone(),
            history: self.conversation.history.clone(),
            history_archived: self.conversation.history_archived,
            audit: self.audit.position(),
            events: self.events.position(),
            logs: (self.conversation.view == View::Logs).then(|| LogPosition {
                scrollback: self.logs.screen().scrollback(),
                query: self.log_query.clone(),
                level: self.log_level,
                filter_offset: self.filter_offset,
            }),
            tasks: (self.conversation.view == View::Tasks)
                .then(|| self.conversation.tasks.position()),
        };
        self.conversation.canvas.columns[self.conversation.canvas.active].position = Some(position);
    }

    fn canvas_switch(&mut self, index: usize) {
        if index == self.conversation.canvas.active
            || index >= self.conversation.canvas.columns.len()
        {
            self.canvas_sync_logs();
            return;
        }
        let canvas = &mut self.conversation.canvas;
        if canvas.columns[index].view == View::Logs
            && canvas.columns[canvas.active].view != View::Logs
        {
            canvas
                .logs_return
                .get_or_insert(canvas.columns[canvas.active].id);
        }
        self.canvas_save();
        self.canvas_restore(index);
    }

    fn canvas_restore(&mut self, index: usize) {
        self.conversation.canvas.active = index;
        let (view, position) = {
            let column = &mut self.conversation.canvas.columns[index];
            (column.view, column.position.take())
        };
        self.conversation.view = view;
        if let Some(position) = position {
            self.conversation.content_focus = position.content_focus;
            self.conversation.selected = position.selected;
            self.conversation.selected_agent = position.selected_agent;
            self.conversation.detail = position.detail;
            self.conversation.detail_scroll = position.detail_scroll;
            self.conversation.question_id = position.question_id;
            self.conversation.history = position.history;
            self.conversation.history_archived = position.history_archived;
            self.audit.restore_position(&position.audit);
            self.events.restore_position(&position.events);
            if let Some(logs) = position.logs {
                self.logs.screen_mut().set_scrollback(logs.scrollback);
                self.log_query = logs.query;
                self.log_level = logs.level;
                self.filter_offset = logs.filter_offset;
            }
            if let Some(tasks) = position.tasks {
                self.conversation.tasks.restore_position(&tasks);
            }
        } else {
            self.conversation.content_focus = view != View::Activity;
            self.conversation.selected = 0;
            self.conversation.selected_agent = None;
            self.conversation.detail = false;
            self.conversation.detail_scroll = 0;
            match view {
                View::Questions => self.conversation.question_id = None,
                View::Activity => self
                    .events
                    .restore_position(&events::EventPosition::default()),
                View::Audit => self.audit.restore_position(&Default::default()),
                View::History => {
                    self.conversation.history = None;
                    self.conversation.history_archived = false;
                }
                View::Queue => {
                    self.conversation.selected = 0;
                    self.conversation.detail = false;
                    self.conversation.detail_scroll = 0;
                }
                View::Agents => self.chat_select_target(),
                View::Logs => {
                    self.logs.screen_mut().set_scrollback(0);
                    self.log_query.clear();
                    self.log_level = 0;
                    self.filter_offset = 0;
                }
                View::Tasks => self
                    .conversation
                    .tasks
                    .restore_position(&Default::default()),
            }
        }
        self.canvas_sync_logs();
        self.conversation.history_width = 0;
        self.conversation.history_rows.clear();
        self.conversation.drawn_agents.clear();
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        self.help = false;
        self.notice = None;
        if view == View::History && !self.preview {
            let filter = if self.conversation.history_archived {
                crate::client::interface::history::archive::Filter::Archived
            } else {
                crate::client::interface::history::archive::Filter::Active
            };
            match crate::client::interface::history::archive::sessions(filter) {
                Ok(sessions) => self.conversation.sessions = sessions,
                Err(error) => self.notice = Some(format!("Could not read history: {error}")),
            }
        }
        self.dirty = true;
    }

    /// Dedicated entries restore the canonical pair without copying either data store.
    fn canvas_system_columns(&mut self) {
        let canvas = &mut self.conversation.canvas;
        let active_id = canvas.columns[canvas.active].id;
        if !canvas
            .columns
            .iter()
            .any(|column| column.view == View::Logs)
        {
            let index = canvas
                .columns
                .iter()
                .position(|column| column.view == View::Tasks)
                .unwrap_or(canvas.columns.len());
            canvas.insert(index, View::Logs);
        }
        let logs = canvas
            .columns
            .iter()
            .position(|column| column.view == View::Logs)
            .unwrap();
        if let Some(tasks) = canvas
            .columns
            .iter()
            .position(|column| column.view == View::Tasks)
        {
            if tasks != logs + 1 {
                let column = canvas.columns.remove(tasks);
                let logs = canvas
                    .columns
                    .iter()
                    .position(|column| column.view == View::Logs)
                    .unwrap();
                canvas.columns.insert(logs + 1, column);
            }
        } else {
            canvas.insert(logs + 1, View::Tasks);
        }
        canvas.active = canvas
            .index(active_id)
            .expect("active column remains on the canvas");
    }

    pub(super) fn canvas_focus(&mut self, view: View) {
        if matches!(view, View::Logs | View::Tasks) {
            self.canvas_system_columns();
        }
        if self.conversation.canvas.columns[self.conversation.canvas.active].view == view {
            self.conversation.view = view;
            self.canvas_sync_logs();
            self.dirty = true;
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
        if matches!(view, View::Logs | View::Tasks) {
            self.canvas_focus(view);
            return;
        }
        self.canvas_save();
        let canvas = &self.conversation.canvas;
        let mut index = canvas.active + 1;
        if canvas.columns[canvas.active].view == View::Logs
            && canvas
                .columns
                .get(index)
                .is_some_and(|column| column.view == View::Tasks)
        {
            index += 1;
        }
        self.conversation.canvas.insert(index, view);
        self.canvas_restore(index);
    }

    pub(super) fn canvas_close(&mut self) {
        if self.conversation.canvas.columns.len() == 1 {
            self.notice = Some("Keep one panel open · /new opens another".into());
            return;
        }
        let index = self.conversation.canvas.active;
        let closed = self.conversation.canvas.columns.remove(index);
        if self.conversation.canvas.logs_return == Some(closed.id) {
            self.conversation.canvas.logs_return = None;
        }
        self.canvas_restore(index.saturating_sub(1));
    }

    pub(super) fn logs_open(&mut self) {
        let canvas = &mut self.conversation.canvas;
        if canvas.columns[canvas.active].view != View::Logs {
            canvas.logs_return = Some(canvas.columns[canvas.active].id);
        }
        self.canvas_focus(View::Logs);
        self.help = false;
        self.notice = None;
        self.dirty = true;
    }

    pub(super) fn logs_close_restore(&mut self) {
        if self.conversation.view != View::Logs {
            self.canvas_sync_logs();
            return;
        }
        let canvas = &mut self.conversation.canvas;
        let target = canvas
            .logs_return
            .take()
            .and_then(|id| canvas.index(id))
            .filter(|&index| canvas.columns[index].view != View::Logs)
            .or_else(|| {
                (0..canvas.active)
                    .rev()
                    .find(|&index| canvas.columns[index].view != View::Logs)
            })
            .or_else(|| {
                (canvas.active + 1..canvas.columns.len())
                    .find(|&index| canvas.columns[index].view != View::Logs)
            });
        if let Some(index) = target {
            self.canvas_switch(index);
        } else {
            self.canvas_new(View::Activity);
        }
    }

    pub(super) fn logs_toggle(&mut self) {
        if self.conversation.view == View::Logs {
            self.logs_close_restore();
        } else {
            self.logs_open();
        }
    }

    /// The log column owns scrolling; horizontal arrows still navigate the canvas.
    pub(super) fn logs_key(&mut self, key: Key) {
        let page = isize::try_from(self.geometry.log_rows)
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1);
        match key {
            Key::Escape => self.logs_close_restore(),
            Key::Back => self.canvas_move(-1),
            Key::Open => self.canvas_move(1),
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
        assert_eq!(state.conversation.view, View::Tasks);
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
                archived: false,
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
                archived: false,
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
    #[test]
    fn logs_and_tasks_are_adjacent_real_columns_with_synchronized_navigation() {
        let mut state = state();
        assert_eq!(
            state
                .conversation
                .canvas
                .columns
                .iter()
                .map(|column| column.view)
                .collect::<Vec<_>>(),
            vec![
                View::Agents,
                View::Activity,
                View::Audit,
                View::Queue,
                View::Questions,
                View::History,
                View::Logs,
                View::Tasks
            ],
        );
        state.logs_open();
        assert_eq!(state.conversation.view, View::Logs);
        assert!(state.conversation.logs_open && state.raw_logs);
        state.chat_key(Key::Open);
        assert_eq!(state.conversation.view, View::Tasks);
        assert!(!state.conversation.logs_open && !state.raw_logs);
        state.chat_key(Key::Back);
        assert_eq!(state.conversation.view, View::Logs);
        assert!(state.conversation.logs_open && state.raw_logs);
    }

    #[test]
    fn logs_return_uses_the_column_id_when_other_columns_shift() {
        let mut state = state();
        state.conversation.composer.set_text("保留 draft");
        state.conversation.target = Some("example-builder".into());
        let origin = state.conversation.canvas.columns[state.conversation.canvas.active].id;
        state.logs_open();
        let logs = state.conversation.canvas.columns[state.conversation.canvas.active].id;
        state.conversation.canvas.insert(0, View::Audit);
        assert_eq!(
            state.conversation.canvas.columns[state.conversation.canvas.active].id,
            logs
        );
        assert_eq!(state.conversation.canvas.logs_return, Some(origin));
        state.logs_close_restore();
        assert_eq!(
            state.conversation.canvas.columns[state.conversation.canvas.active].id,
            origin
        );
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "保留 draft");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
    }

    fn canonical_pair(state: &State) -> (usize, usize) {
        let columns = &state.conversation.canvas.columns;
        let logs = columns
            .iter()
            .position(|column| column.view == View::Logs)
            .unwrap();
        let tasks = columns
            .iter()
            .position(|column| column.view == View::Tasks)
            .unwrap();
        assert_eq!(
            columns
                .iter()
                .filter(|column| column.view == View::Logs)
                .count(),
            1
        );
        assert_eq!(
            columns
                .iter()
                .filter(|column| column.view == View::Tasks)
                .count(),
            1
        );
        assert_eq!(tasks, logs + 1);
        (logs, tasks)
    }

    #[test]
    fn new_views_do_not_split_the_canonical_logs_tasks_pair() {
        let mut state = state();
        state.logs_open();
        state.canvas_new(View::Queue);
        let (_, tasks) = canonical_pair(&state);
        assert_eq!(state.conversation.canvas.active, tasks + 1);
        assert_eq!(state.conversation.view, View::Queue);
        state.canvas_focus(View::Tasks);
        state.canvas_new(View::Activity);
        let (_, tasks) = canonical_pair(&state);
        assert_eq!(state.conversation.canvas.active, tasks + 1);
        assert_eq!(state.conversation.view, View::Activity);
    }

    #[test]
    fn dedicated_entries_restore_closed_system_columns_without_duplicates() {
        let mut state = state();
        state.logs_open();
        state.canvas_close();
        assert!(
            !state
                .conversation
                .canvas
                .columns
                .iter()
                .any(|column| column.view == View::Logs)
        );
        state.canvas_focus(View::Tasks);
        canonical_pair(&state);
        assert_eq!(state.conversation.view, View::Tasks);
        state.canvas_close();
        assert!(
            !state
                .conversation
                .canvas
                .columns
                .iter()
                .any(|column| column.view == View::Tasks)
        );
        state.logs_open();
        canonical_pair(&state);
        assert_eq!(state.conversation.view, View::Logs);
        let count = state.conversation.canvas.columns.len();
        state.logs_open();
        state.canvas_focus(View::Tasks);
        assert_eq!(state.conversation.canvas.columns.len(), count);
    }

    #[test]
    fn logs_and_tasks_retain_independent_positions_without_replacing_data() {
        let mut state = state();
        state.logs_open();
        for index in 0..50 {
            state.log(format!("retained diagnostic {index}\n").as_bytes());
        }
        state.scroll_logs(5);
        state.log_query = "diagnostic".into();
        state.log_level = 1;
        state.filter_offset = 7;
        let logs = (
            state.logs.screen().scrollback(),
            state.log_query.clone(),
            state.log_level,
            state.filter_offset,
        );
        let retained = state.logs.screen().scrollback_len();
        state.canvas_move(1);
        assert_eq!(state.conversation.view, View::Tasks);
        state.conversation.tasks.refresh(true).unwrap();
        state.conversation.tasks.open();
        state.conversation.tasks.scroll_by(2);
        let task = state.conversation.tasks.snapshot();
        assert_eq!(task["selected_id"], "preview-task-build");
        assert_eq!(task["detail"], true);
        assert_eq!(task["detail_scroll"], 2);
        state.log_query = "temporary other filter".into();
        state.log_level = 2;
        state.filter_offset = 0;
        state.canvas_move(-1);
        assert_eq!(
            (
                state.logs.screen().scrollback(),
                state.log_query.clone(),
                state.log_level,
                state.filter_offset
            ),
            logs,
        );
        assert_eq!(state.logs.screen().scrollback_len(), retained);
        state.canvas_move(1);
        let restored = state.conversation.tasks.snapshot();
        for field in ["selected_id", "detail", "detail_scroll", "jobs"] {
            assert_eq!(restored[field], task[field], "{field}");
        }
    }
}
