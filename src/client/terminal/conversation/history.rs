use super::*;
use crate::client::interface::history;

impl State {
    pub(super) fn chat_resume(&mut self) {
        match history::sessions() {
            Ok(sessions) => {
                self.conversation.sessions = sessions;
                self.chat_change_view(View::History);
            }
            Err(error) => {
                self.notice = Some(localized!(
                    self.ui.language,
                    "Could not read history: {error}",
                    "无法读取历史：{error}"
                ))
            }
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
                self.ui
                    .language
                    .text(
                        "Wait for the current submission to be confirmed before switching history",
                        "请等待当前提交得到确认后，再切换历史会话",
                    )
                    .into(),
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
                    self.log_message(&message.role, &message.content, record.as_bytes());
                }
                self.conversation.history = Some(conversation.into());
                self.conversation.history_width = 0;
                self.conversation.history_rows.clear();
                self.conversation.detail = true;
                self.conversation.content_focus = false;
                self.conversation.detail_scroll = usize::MAX;
                self.notice = Some(
                    self.ui
                        .language
                        .text(
                            "History restored · type to continue · no old request was resent",
                            "历史已恢复 · 输入即可继续 · 没有重发旧请求",
                        )
                        .into(),
                );
            }
            Err(error) => {
                self.notice = Some(localized!(
                    self.ui.language,
                    "Could not resume: {error}",
                    "无法恢复：{error}"
                ))
            }
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
                        "assistant" => self.ui.language.text("Agent", "智能体"),
                        "user" => self.ui.language.text("You", "你"),
                        "tool" => self.ui.language.text("Tool", "工具"),
                        _ => self.ui.language.text("Session", "会话"),
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
            self.ui.language.text(
                "  Conversation history · ↑↓ select · Enter resume · Esc back",
                "  会话历史 · ↑↓ 选择 · Enter 恢复 · Esc 返回",
            ),
            PanelTone::Heading,
            width,
        )];
        if self.conversation.sessions.is_empty() {
            rows.push(self.chat_line(
                self.ui.language.text(
                    "  No saved conversations in this workspace yet.",
                    "  当前工作区暂无已保存的会话。",
                ),
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
                &localized!(
                    self.ui.language,
                    "  {} messages · {}{}",
                    "  {} 条消息 · {}{}",
                    session.message_count,
                    session.id,
                    if session.current {
                        self.ui.language.text(" · current", " · 当前")
                    } else {
                        ""
                    }
                ),
                PanelTone::Muted,
                width,
            ));
        }
        rows
    }

    pub(super) fn chat_help_rows(&mut self, height: u16) -> Vec<String> {
        let mut text = String::from(
            self.ui
                .language
                .text("# ssmcp help\n\n", "# ssmcp 帮助\n\n"),
        );
        if let Some(guide) = &self.guide {
            text.push_str(self.ui.language.text("Online guide: ", "在线指南："));
            text.push_str(guide.url());
            text.push_str("\n\n");
        }
        text.push_str(super::help::body(self.ui.language));
        for command in crate::client::composer::SLASH_COMMANDS {
            let _ = writeln!(
                text,
                "{} — {}",
                command.name,
                command.description(self.ui.language)
            );
        }
        text.push_str(self.ui.language.text("\n## Native harnesses\n/pi, /cc, /codex: inspect installed capabilities.\nUse /cc TOOL {JSON} or /codex TOOL {JSON} with the discovered schema.\nUse /pi rpc {\"type\":\"get_commands\",\"args\":{}} to inspect installed commands.\nBackend capabilities and approvals stay with the installed harness.\n\nHelp: Up/Down, PgUp/PgDn, Home/End scroll; Esc returns.\n", "\n## 原生工具后端\n/pi、/cc、/codex：查看已安装的能力。\n按发现的参数定义使用 /cc TOOL {JSON} 或 /codex TOOL {JSON}。\n使用 /pi rpc {\"type\":\"get_commands\",\"args\":{}} 查看已安装的命令。\n后端能力与审批仍由已安装的后端管理。\n\n帮助：↑/↓、PgUp/PgDn、Home/End 滚动；Esc 返回。\n"));
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
