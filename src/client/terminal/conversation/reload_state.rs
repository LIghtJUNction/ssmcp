//! One local checkpoint for reconnect/reload. Draft ownership remains in steering.
use super::*;
use crate::client::{composer::InputHistory, interface::history as history_store};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::client) struct Snapshot {
    canvas: canvas::ReloadCanvas,
    input_history: InputHistory,
    ui: Ui,
    preferred_input_rows: Option<u16>,
    help: bool,
    help_scroll: usize,
    filter_editing: bool,
    filter_draft: String,
    #[serde(default)]
    groups: groups::ReloadGroups,
}

impl Snapshot {
    pub(in crate::client) fn validate(&self) -> Result<(), String> {
        self.canvas.validate()?;
        self.input_history.validate()?;
        self.groups.validate()?;
        self.ui.validate()?;
        if self.filter_draft.len() > 4096 {
            return Err("reload log filter is too long".into());
        }
        Ok(())
    }
}

struct Pending {
    snapshot: Option<Snapshot>,
    restore_history: bool,
    notice: Option<String>,
}

static PENDING: OnceLock<Mutex<Option<Pending>>> = OnceLock::new();

pub(in crate::client) fn snapshot() -> Option<Snapshot> {
    PENDING
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()?
        .snapshot
        .clone()
}

pub(in crate::client) fn restore(
    snapshot: Option<Snapshot>,
    restore_history: bool,
    notice: Option<String>,
) {
    *PENDING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(Pending {
        snapshot,
        restore_history,
        notice,
    });
}

impl State {
    fn chat_reload_snapshot(&mut self) -> Snapshot {
        Snapshot {
            canvas: self.canvas_reload_snapshot(),
            input_history: self.conversation.composer.reload_history(),
            ui: self.ui.clone(),
            preferred_input_rows: self.conversation.preferred_input_rows,
            help: self.help,
            help_scroll: self.conversation.help_scroll,
            filter_editing: self.filter_editing,
            filter_draft: self.filter_draft.clone(),
            groups: self.conversation.groups.reload_snapshot(),
        }
    }

    pub(in crate::client::terminal) fn chat_capture_reload_state(&mut self) {
        if self.conversation.restored {
            self.chat_persist_draft();
        }
        let snapshot = self.chat_reload_snapshot();
        restore(Some(snapshot), true, None);
    }

    fn chat_apply_reload_snapshot(
        &mut self,
        saved: Snapshot,
        sessions: Vec<history_store::SessionSummary>,
    ) {
        self.ui = saved.ui;
        self.ui.color &= std::env::var_os("NO_COLOR").is_none();
        self.conversation.preferred_input_rows = saved.preferred_input_rows;
        self.resize(self.geometry.rows, self.geometry.cols);
        self.conversation.sessions = sessions;
        self.canvas_restore_reload(saved.canvas);
        self.conversation.reload_input_history = Some(saved.input_history);
        if let Err(error) = self.conversation.groups.restore(saved.groups) {
            self.notice = Some(error);
        }
        self.help = saved.help;
        self.conversation.help_scroll = saved.help_scroll;
        self.filter_editing = saved.filter_editing;
        self.filter_draft = saved.filter_draft;
        self.graphics = images::Graphics::environment(&self.ui);
        self.dirty = true;
    }

    pub(in crate::client::terminal) fn chat_restore_reload_state(&mut self) {
        let pending = PENDING.get().and_then(|pending| {
            pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
        });
        let Some(mut pending) = pending else {
            return;
        };
        // Geometry and appearance must be in place before rendering retained text.
        if let Some(saved) = pending.snapshot.as_ref() {
            self.ui = saved.ui.clone();
            self.ui.color &= std::env::var_os("NO_COLOR").is_none();
            self.conversation.preferred_input_rows = saved.preferred_input_rows;
            self.resize(self.geometry.rows, self.geometry.cols);
        }
        if pending.restore_history {
            match history_store::current() {
                Ok(Some(history)) => self.chat_rehydrate_reload_history(&history),
                Ok(None) => {}
                Err(error) => {
                    pending.notice = Some(format!(
                        "Could not restore conversation: {error} · use /resume to choose history"
                    ))
                }
            }
        }
        if let Some(saved) = pending.snapshot {
            self.chat_apply_reload_snapshot(saved, history_store::sessions().unwrap_or_default());
            crate::client::render::configure(self.ui.clone());
        }
        if let Some(notice) = pending.notice {
            self.log_message(
                "system",
                &notice,
                format!("\n[RELOAD] {notice}\n").as_bytes(),
            );
            self.notice = Some(notice);
        }
    }

    fn chat_rehydrate_reload_history(&mut self, history: &history_store::Conversation) {
        self.events.clear();
        self.logs = vt100::Parser::new(
            nonzero(self.geometry.log_rows),
            nonzero(self.geometry.left),
            SCROLLBACK_ROWS,
        );
        self.previous_cr = false;
        for message in &history.messages {
            let label = match message.role.as_str() {
                "assistant" | "agent" => "AGENT",
                "user" => "USER",
                _ => "HISTORY",
            };
            let mut record = format!("\n[{label}]\n");
            for row in markdown::rows(&message.content, self.geometry.left, &self.ui) {
                let _ = writeln!(record, "{row}");
            }
            record.push('\n');
            self.log_message(&message.role, &message.content, record.as_bytes());
        }
    }

    pub(super) fn chat_restore_reload_input(&mut self) {
        if let Some(history) = self.conversation.reload_input_history.take()
            && let Err(error) = self.conversation.composer.restore_reload_history(history)
        {
            self.notice = Some(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_snapshot_keeps_canvas_and_unicode_history_but_not_connection_handles() {
        let mut before = State::new(28, 100, &Ui::default());
        before.canvas_new(View::Activity);
        before.conversation.content_focus = true;
        before.conversation.preferred_input_rows = Some(7);
        before.ui.theme = crate::client::ui::Theme::Light;
        before.conversation.composer.remember("accepted request 一");
        before.conversation.composer.remember("accepted request 二");
        before.conversation.composer.set_text("unsent draft 界");
        before.conversation.composer.left();
        let draft_cursor = before.conversation.composer.cursor();
        assert!(before.conversation.composer.history_previous());
        before.conversation.native_account =
            Some(("old-client-identity".into(), "old-account".into()));
        before.conversation.selected_agent = Some("old-agent".into());
        before.conversation.question_id = Some("old-question".into());
        before.active_session = "old-process".into();
        let draft = before.chat_saved_draft();
        let canvas = before.conversation.canvas.snapshot();
        let saved = before.chat_reload_snapshot();
        saved.validate().unwrap();
        let bytes = serde_json::to_vec(&saved).unwrap();
        let encoded = String::from_utf8(bytes.clone()).unwrap();
        for stale in [
            "old-client-identity",
            "old-account",
            "old-agent",
            "old-question",
            "old-process",
        ] {
            assert!(!encoded.contains(stale));
        }

        let saved: Snapshot = serde_json::from_slice(&bytes).unwrap();
        let mut after = State::new(20, 70, &Ui::default());
        after.chat_apply_reload_snapshot(saved, Vec::new());
        // The existing outbox restoration still owns the draft and its cursor.
        after.chat_restore_draft(draft);
        after.chat_restore_reload_input();
        assert_eq!(after.conversation.canvas.snapshot(), canvas);
        assert_eq!(after.conversation.view, View::Activity);
        assert!(after.conversation.content_focus);
        assert_eq!(after.conversation.preferred_input_rows, Some(7));
        assert_eq!(after.geometry.input_rows, 7);
        assert_eq!(after.ui.theme, crate::client::ui::Theme::Light);
        assert!(after.conversation.composer.history_next());
        assert_eq!(after.conversation.composer.text(), "unsent draft 界");
        assert_eq!(after.conversation.composer.cursor(), draft_cursor);
        assert!(after.conversation.native_account.is_none());
        assert!(after.conversation.selected_agent.is_none());
        assert!(after.conversation.question_id.is_none());
        assert_eq!(after.active_session, "main");
        assert!(
            !after.conversation.restored,
            "the normal draft/receipt initialization must still run"
        );
    }

    #[test]
    fn reload_checkpoint_accepts_older_snapshots_without_group_configuration() {
        let mut state = State::new(24, 80, &Ui::default());
        let mut value = serde_json::to_value(state.chat_reload_snapshot()).unwrap();
        value.as_object_mut().unwrap().remove("groups");
        let restored: Snapshot = serde_json::from_value(value).unwrap();
        restored.validate().unwrap();
        let mut after = State::new(24, 80, &Ui::default());
        after.chat_apply_reload_snapshot(restored, Vec::new());
        assert!(after.conversation.groups.selected.is_empty());
        assert_eq!(
            serde_json::to_value(after.conversation.groups.reload_snapshot()).unwrap(),
            serde_json::json!({"selected":[], "saved":{}})
        );
    }

    #[test]
    fn reload_snapshot_rejects_a_canvas_without_a_valid_active_column() {
        let mut state = State::new(24, 80, &Ui::default());
        let saved = serde_json::to_value(state.chat_reload_snapshot()).unwrap();
        let mut invalid_filter = saved.clone();
        invalid_filter["canvas"]["columns"][1]["position"]["logs"] =
            serde_json::json!({"scrollback": 0, "query": "", "level": 3, "filter_offset": 0});
        let checkpoint: Snapshot = serde_json::from_value(invalid_filter).unwrap();
        assert!(
            checkpoint.validate().is_err(),
            "the log UI only has categories 0..=2"
        );
        for bad in [
            serde_json::json!({"columns":[], "active":0, "next_id":9, "logs_return":null}),
            serde_json::json!({"columns":[{"id":1,"view":"Activity","position":null}], "active":2, "next_id":9, "logs_return":null}),
            serde_json::json!({"columns":[{"id":1,"view":"Activity","position":null},{"id":1,"view":"History","position":null}], "active":0, "next_id":9, "logs_return":null}),
        ] {
            let mut value = saved.clone();
            value["canvas"] = bad;
            let checkpoint: Snapshot = serde_json::from_value(value).unwrap();
            assert!(checkpoint.validate().is_err());
        }
    }
}
