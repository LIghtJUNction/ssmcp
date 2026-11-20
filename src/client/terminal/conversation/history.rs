use super::*;
use crate::client::interface::history;

fn history_entry_height(session: &history::SessionSummary) -> usize {
    2 + usize::from(session.summary.is_some())
}

impl State {
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
            "## Write and send\nEnter: queue a requirement; continue writing the next draft.\nCtrl+J / Shift+Enter: newline. Tab / Shift+Tab: switch focus.\nAlt+Up/Down: increase/decrease visible input rows; keeps your draft and cursor.\nUp/Down: move over wrapped input lines; then history.\nEmpty input + Up: take back the latest queued request before input history. Wait for confirmation; delivered requests cannot be taken back.\nCtrl+C: clear the current input; on empty input, show an exit reminder, then press again to exit. Any other input cancels the exit reminder. Other queued requests stay unchanged.\nLeft/Right: move cursor; at a draft boundary, move to the adjacent panel. Home/End: line start/end.\nCtrl+A/E: line start/end. Ctrl+U/K: delete before/after cursor.\nCtrl+W: delete word. Backspace/Delete: edit text.\n\n## Agents and questions\nLeft at draft start: move to Agents; Up/Down select; Enter chooses. Alt+Left/Right always moves between panels.\nEsc: return without changing recipient or draft.\nShift+Up or /questions: answer pending questions; repeat for next question.\nEnter a choice number or a free answer; Enter saves it.\nEsc: question list; Delete in list skips; Esc again returns.\nQuestions never pause the agent's other work.\n\n## Shell commands\n!command: execute with your local shell in the workspace.\nUses the ASCII ! prefix; runs asynchronously with output and exit status here.\n\n## Tasks\n/command [JOB_ID]: view the complete argv and multiline script; scroll to read all lines.\n/tasks: open Tasks immediately to the right of Logs; refreshes while visible.\nUp/Down select a job; Enter opens details; Esc returns to the list or input.\nPgUp/PgDn and the mouse wheel scroll.\n/tasks cancel ID: explicitly cancel that local job.\n\n## Files, skills and paste\n@path or @\"path with spaces\": reference a workspace file/image.\n$skill-name: reference an effective project/global skill.\nPaste over 8 KiB: saved as @file; add your request after it.\nText paste limit: 4 MiB; image paste limit: 16 MiB. Input/expanded requirement limit: 16 KiB.\nCtrl+V or /paste: import clipboard text or image.\n/attach PATH: import an image as a private file reference.\n/copy: copy the latest agent reply to the clipboard.\n\n## GitHub account\n/login: prepare GitHub sign-in and open the browser.\n/login copy: copy the pending login link; /login link: display it here.\nRepeat these commands to use the same pending authorization.\nFinish GitHub sign-in and confirm the device; the current session stays connected.\n\n## Review and copy\nTab: browse content. Enter: open or toggle the selected record.\nLeft/Right: move between canvas panels; Esc: back. Wheel/PgUp/PgDn: scroll content.\nCtrl+Home: oldest retained output. Ctrl+End: return to the latest and follow new activity.\nThe footer shows how much output is below your position.\nRight-click or /logs: move to Logs; Esc/right-click returns to the previous panel.\nLogs and Tasks are adjacent canvas panels; Left/Right moves between them.\n/raw also toggles Logs. Full output stays available there.\n/diff single|split: stack changes or show old/new lines side by side.\nSplit uses a single column on narrow terminals.\n/new [chat|agents|audit|queue|questions|history]: add a panel to the right.\n/close: close this panel; the session, drafts and running tasks are kept.\nCanvas navigation does not wrap; each panel remembers its selection and scroll position.\nAlt+C, F7 or /select: freeze display and use your terminal's native mouse selection.\nCopy using your terminal's Copy shortcut or menu (often Ctrl+Shift+C).\nAlt+C/F7/Esc resumes updates and restores interface scrolling and clicks.\nOn terminals supporting it, Shift+drag selects directly without entering copy mode.\n/copy still copies only the latest agent reply.\n\n## Queued requests\n/queue: Up/Down select; Enter take back; i/I inspect without editing.\nDelete cancels a revocable request. R copies delivered text.\nFor uncertain/offline items, R opens explicit review before resending.\nTaking back waits for server acknowledgement; delivery may win.\n\n## 电脑控制\n/computer on：允许 AI 控制当前桌面的鼠标和键盘。\n/computer off：关闭当前桌面的键鼠控制；同一系统用户的会话同步生效。\n/computer status：查看开关状态；独立桌面与浏览器工具仍可使用。\nSuper+Alt+C：切换电脑控制；正常打字不会改变开关。\nUse the desktop shortcut when another app has focus; terminals that cannot send Super need the desktop binding or /computer on/off.\nTab and Alt+C keep their completion, focus and copy actions.\n\n## Appearance and history\nF1: help. F2: next view. F3: sidebar. F4: theme.\nF5 reloads appearance. F8 saves appearance. F9 changes log category.\n/reload restarts the client; /resume reopens saved local conversation.\n/archive [ID]: archive current or named history without deleting messages.\n/archived: browse archives; Enter restores and resumes the selection.\n/unarchive ID: restore visibility without switching history or resending.\nConversation history never automatically replays tools or requests.\n/handoff [note]: export current context and prepare an @file draft.\nChoose the recipient, review the draft, then Enter to send.\n\n## Local commands\n",
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
