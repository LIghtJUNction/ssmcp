use super::*;
use crate::client::interface::history;

impl State {
    pub(super) fn chat_resume(&mut self) {
        match history::sessions() {
            Ok(sessions) => {
                self.conversation.sessions = sessions;
                self.chat_change_view(View::History);
            }
            Err(error) => self.notice = Some(format!("Could not read history: {error}")),
        }
        self.dirty = true;
    }

    pub(super) fn chat_resume_selected(&mut self) {
        let sending = self.conversation.submitting.is_some()
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
        match history::resume(&id) {
            Ok(conversation) => {
                for session in &mut self.conversation.sessions {
                    session.current = session.id == id;
                }
                // Restore retained visible context without publishing or recording it again.
                self.events.clear();
                self.logs = vt100::Parser::new(
                    nonzero(self.geometry.log_rows),
                    nonzero(self.geometry.left),
                    SCROLLBACK_ROWS,
                );
                for message in &conversation.messages {
                    let label = if message.role == "assistant" {
                        "AGENT"
                    } else if message.role == "user" {
                        "USER"
                    } else {
                        "HISTORY"
                    };
                    let mut record = format!("\n[{label}]\n");
                    for row in markdown::rows(&message.content, self.geometry.left, &self.ui) {
                        let _ = writeln!(record, "{row}");
                    }
                    record.push('\n');
                    self.log(record.as_bytes());
                }
                self.conversation.history = Some(conversation);
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
        let mut rows = vec![self.chat_line(
            "  Conversation history · ↑↓ select · Enter resume · Esc back",
            PanelTone::Heading,
            width,
        )];
        if self.conversation.sessions.is_empty() {
            rows.push(self.chat_line(
                "  No saved conversations in this workspace yet.",
                PanelTone::Muted,
                width,
            ));
            return rows;
        }
        for index in self.chat_list_window(self.conversation.sessions.len(), height) {
            let session = &self.conversation.sessions[index];
            rows.push(self.chat_line(
                &format!(
                    "{} {}",
                    if index == self.conversation.selected {
                        "›"
                    } else {
                        " "
                    },
                    session.title
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
                    if session.current { " · current" } else { "" }
                ),
                PanelTone::Muted,
                width,
            ));
        }
        rows
    }

    pub(super) fn chat_help_rows(&mut self, height: u16) -> Vec<String> {
        let mut text = String::from("# ssmcp help\n\n");
        if let Some(guide) = &self.guide {
            text.push_str("Online guide: ");
            text.push_str(guide.url());
            text.push_str("\n\n");
        }
        text.push_str(
            "## Write and send\nEnter: queue a requirement; continue writing the next draft.\nCtrl+J / Shift+Enter: newline. Tab / Shift+Tab: switch focus.\nUp/Down: move over wrapped input lines; then history.\nLeft/Right: move cursor. Home/End: line start/end.\nCtrl+A/E: line start/end. Ctrl+U/K: delete before/after cursor.\nCtrl+W: delete word. Backspace/Delete: edit text.\n\n## Agents and questions\nLeft at draft start: open Agents; Up/Down select; Enter/Right choose.\nEsc/Left: return without changing recipient or draft.\nShift+Up or /questions: answer pending questions; repeat for next question.\nEnter a choice number or a free answer; Enter saves it.\nEsc: question list; Delete in list skips; Esc again returns.\nQuestions never pause the agent's other work.\n\n## Shell commands\n!command: execute with your local shell in the workspace.\nUses the ASCII ! prefix; runs asynchronously with output and exit status here.\n\n## Files, skills and paste\n@path or @\"path with spaces\": reference a workspace file/image.\n$skill-name: reference an effective project/global skill.\nPaste over 8 KiB: saved as @file; add your request after it.\nText paste limit: 4 MiB; image paste limit: 16 MiB. Input/expanded requirement limit: 16 KiB.\nCtrl+V or /paste: import clipboard text or image.\n/attach PATH: import an image as a private file reference.\n/copy: copy the latest agent reply to the clipboard.\n\n## Review and copy\nTab: browse content. Enter/Right: open selected record.\nEnter/Right: expand event; Left folds it; click toggles.\nLeft/Esc: back. Wheel/PgUp/PgDn: scroll content.\nCtrl+Home: oldest retained output. Ctrl+End: return to the latest and follow new activity.\nThe footer shows how much output is below your position.\n/raw: original transcript; /logs: compact tool activity.\nF7: freeze display and use terminal mouse selection; F7/Esc returns.\nShift+drag can use your terminal's native selection directly.\nCopy with your terminal shortcut (usually Ctrl+Shift+C).\n\n## Queued requests\n/queue: Up/Down select; Enter take back; Right inspect.\nDelete cancels a revocable request. R copies delivered text.\nFor uncertain/offline items, R opens explicit review before resending.\nTaking back waits for server acknowledgement; delivery may win.\n\n## Appearance and history\nF1: help. F2: next view. F3: sidebar. F4: theme.\nF5 reloads appearance. F8 saves appearance. F9 changes log category.\n/reload restarts the client; /resume reopens saved local conversation.\nConversation history never automatically replays tools or requests.\n/handoff [note]: export current context and prepare an @file draft.\nChoose the recipient, review the draft, then Enter to send.\n\n## Local commands\n",
        );
        for command in crate::client::composer::SLASH_COMMANDS {
            let _ = writeln!(text, "{} — {}", command.name, command.description);
        }
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
