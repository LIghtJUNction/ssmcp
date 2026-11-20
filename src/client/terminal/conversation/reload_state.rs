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
    #[serde(default)]
    display: events::ReloadDisplay,
}

impl Snapshot {
    pub(in crate::client) fn discard_display(&mut self) {
        self.display.discard();
        self.canvas.discard_display_positions();
    }

    #[cfg(test)]
    pub(in crate::client) fn fixture_for_test() -> Self {
        State::new(24, 80, &Ui::default()).chat_reload_snapshot()
    }

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
    pub(super) fn chat_reload_snapshot(&mut self) -> Snapshot {
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
            display: self.events.reload_display(),
        }
    }

    pub(in crate::client::terminal) fn chat_capture_reload_state(&mut self) {
        if self.conversation.restored {
            self.chat_persist_draft();
        }
        let snapshot = self.chat_reload_snapshot();
        restore(Some(snapshot), true, None);
    }

    pub(super) fn chat_apply_reload_snapshot(
        &mut self,
        saved: Snapshot,
        sessions: Vec<history_store::SessionSummary>,
        history: Option<&history_store::Conversation>,
    ) {
        self.ui = saved.ui;
        self.ui.color &= std::env::var_os("NO_COLOR").is_none();
        self.conversation.preferred_input_rows = saved.preferred_input_rows;
        self.resize(self.geometry.rows, self.geometry.cols);
        self.conversation.sessions = sessions;
        let display_notice = self.events.restore_reload_display(saved.display);
        let log_text = self.events.reload_log_text();
        if !log_text.is_empty() {
            self.log_raw(log_text.as_bytes());
        }
        if let Some(history) = history {
            self.events.reconcile_reload_history(history);
        }
        self.canvas_restore_reload(saved.canvas);
        self.conversation.reload_input_history = Some(saved.input_history);
        if let Err(error) = self.conversation.groups.restore(saved.groups) {
            self.notice = Some(error);
        }
        if let Some(notice) = display_notice {
            self.notice = Some(notice);
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
        self.sync_history_owner();
        let history = if pending.restore_history {
            match history_store::current() {
                Ok(history) => history,
                Err(error) => {
                    pending.notice = Some(format!(
                        "Could not restore conversation: {error} · use /resume to choose history"
                    ));
                    None
                }
            }
        } else {
            None
        };
        if let Some(saved) = pending.snapshot {
            self.chat_apply_reload_snapshot(
                saved,
                history_store::sessions().unwrap_or_default(),
                history.as_ref(),
            );
            crate::client::render::configure(self.ui.clone());
        } else if let Some(history) = history {
            self.chat_rehydrate_history(&history);
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

    pub(super) fn chat_rehydrate_history(&mut self, history: &history_store::Conversation) {
        if let Some(owner) = history_store::current_owner()
            && owner.id() == history.metadata.id
        {
            self.events.rehydrate_history(owner, &history.messages);
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
    fn reload_keeps_history_cards_owner_local_and_logs_global() {
        use crate::{
            client::interface::history::HistoryId,
            runtime::call_scope::{DisplayHistory, DisplaySource},
        };

        let a = HistoryId::parse("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let b = HistoryId::parse("conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let a_message = "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b_recalled = "message_cccccccccccccccccccccccccccccccc";
        let mut before = State::new(24, 100, &Ui::default());
        before.events.set_history_owner(Some(a.clone()));
        before
            .events
            .ingest_identified_message(a_message, "user", "A_PRIVATE_MESSAGE", b"");
        before.events.set_history_owner(Some(b.clone()));
        before
            .events
            .ingest_identified_message(b_recalled, "user", "B_RECALLED_MESSAGE", b"");
        before.events.ingest_scoped_event(
            "shell",
            &json!({"type":"exec.started","job_id":"old-a-job"}),
            b"[RUN] A_LATE_TOOL\n",
            DisplaySource {
                call: None,
                history: DisplayHistory::Local(a.clone()),
            },
        );
        before.events.ingest_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"old-b-job","exit_code":0}),
            b"[OK] B_TOOL\n",
        );
        let encoded = serde_json::to_vec(&before.chat_reload_snapshot()).unwrap();
        let saved: Snapshot = serde_json::from_slice(&encoded).unwrap();
        let history = history_store::Conversation {
            metadata: history_store::SessionMetadata {
                schema: "ssmcp.conversation".into(),
                schema_version: 1,
                id: b.as_str().into(),
                workspace: "/fixture".into(),
                created_at_ms: 1,
                truncated_messages: 0,
            },
            messages: vec![history_store::Message {
                id: "message_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                role: "user".into(),
                content: "B_SAVED_MESSAGE".into(),
                kind: "input".into(),
                created_at_ms: 1,
                truncated: false,
            }],
        };
        let mut after = State::new(24, 100, &Ui::default());
        after.events.set_history_owner(Some(b));
        after.chat_apply_reload_snapshot(saved, Vec::new(), Some(&history));

        let chat = after.events.render(80, 24, &after.ui, false).join("\n");
        assert!(
            chat.contains("B_SAVED_MESSAGE") && chat.contains("B_TOOL"),
            "{chat}"
        );
        for hidden in ["A_PRIVATE_MESSAGE", "A_LATE_TOOL", "B_RECALLED_MESSAGE"] {
            assert!(!chat.contains(hidden), "{chat}");
            after.log_query = hidden.to_ascii_lowercase();
            let logs = after.filtered_logs().unwrap().join("\n");
            assert!(logs.contains(hidden), "{logs}");
        }
        after.events.set_history_owner(Some(a));
        let chat = after.events.render(80, 24, &after.ui, false).join("\n");
        assert!(
            chat.contains("A_PRIVATE_MESSAGE") && chat.contains("A_LATE_TOOL"),
            "{chat}"
        );
        assert!(!chat.contains("B_SAVED_MESSAGE"), "{chat}");
        assert!(after.events.recall_message(a_message));
    }

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
        after.chat_apply_reload_snapshot(saved, Vec::new(), None);
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
        after.chat_apply_reload_snapshot(restored, Vec::new(), None);
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
