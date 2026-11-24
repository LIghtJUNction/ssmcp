//! Local controls for server-owned user waits; cancellation is not session exit.
use super::*;
use crate::client::interface::user_wait::{self as waiting, Entry, Intent, Outcome, Snapshot};
use crate::client::notification::Status;
use std::collections::BTreeMap;

pub(super) fn arguments(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/wait")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim())
    } else {
        None
    }
}

fn groups(snapshot: &Snapshot) -> BTreeMap<&str, Vec<&Entry>> {
    let mut episodes = BTreeMap::<&str, Vec<&Entry>>::new();
    for entry in &snapshot.active {
        episodes
            .entry(entry.episode_id.as_str())
            .or_default()
            .push(entry);
    }
    episodes
}

fn intent_text(intent: Intent) -> &'static str {
    match intent {
        Intent::NeedUser | Intent::Finish => "等待你的输入",
    }
}

fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ready => "消息已就绪，等待提交",
        Outcome::Decided => "本轮结束决策已返回",
        Outcome::TimedOut => "本轮等待超时",
        Outcome::Cancelled => "等待已取消，会话继续",
        Outcome::Disconnected => "连接已断开",
        Outcome::AuthorizationRevoked => "权限已撤销",
        Outcome::BindingInvalid => "会话绑定已失效",
        Outcome::Superseded => "等待已被替代",
    }
}

pub(super) fn detail(snapshot: &Snapshot) -> String {
    let episodes = groups(snapshot);
    let mut lines = Vec::new();
    if episodes.is_empty() {
        lines.push(if snapshot.connected {
            "当前没有程序在等待你的输入".to_owned()
        } else {
            "当前连接未就绪，没有活动等待".to_owned()
        });
    } else {
        lines.push(format!(
            "{} 项等待 · {} 个独立请求",
            episodes.len(),
            snapshot.active.len()
        ));
        for entries in episodes.values() {
            let first = entries
                .iter()
                .copied()
                .find(|entry| entry.intent == Intent::Finish)
                .unwrap_or(entries[0]);
            let mixed = entries.iter().any(|entry| entry.intent != first.intent);
            let recipient = if first.recipient == "next" {
                "Next caller"
            } else {
                &first.recipient
            };
            lines.push(String::new());
            lines.push(format!("{} · 目标 {recipient}", intent_text(first.intent)));
            if let Some(reason) = &first.reason {
                lines.push(reason.clone());
            }
            for entry in entries {
                let remaining = entry.remaining_ms.div_ceil(1000);
                lines.push(if mixed {
                    format!(
                        "#{} · {} · 本轮剩余 {remaining}s",
                        entry.wait_id,
                        intent_text(entry.intent)
                    )
                } else {
                    format!("#{} · 本轮剩余 {remaining}s", entry.wait_id)
                });
                let notification = match entry.notification {
                    Some(report) => {
                        let status = match report.status {
                            Status::Sent => "系统通知已提交，未确认已读",
                            Status::Unavailable => "系统通知不可用",
                            Status::Failed => "系统通知未完成",
                        };
                        format!(
                            "{status}{}",
                            if report.deduplicated {
                                "（已去重）"
                            } else {
                                ""
                            }
                        )
                    }
                    None if entry.notification_requested => "系统通知处理中，未确认已读".into(),
                    None => "未请求系统通知".into(),
                };
                lines.push(notification);
            }
        }
        lines.push(String::new());
        lines.push("/wait cancel <完整编号或唯一前缀> 只取消对应等待，会话继续".into());
    }
    if !snapshot.recent.is_empty() {
        lines.push(String::new());
        lines.push("最近的本机等待结果：".into());
        for completed in snapshot.recent.iter().rev().take(4) {
            lines.push(format!(
                "#{} · {}",
                completed.wait_id,
                outcome_text(completed.status)
            ));
        }
    }
    lines.push("取消只结束等待；你的消息仍按队列状态处理".into());
    lines.join("\n")
}

fn hint(snapshot: &Snapshot) -> Option<String> {
    let episodes = groups(snapshot);
    let remaining = snapshot
        .active
        .iter()
        .map(|entry| entry.remaining_ms)
        .min()?
        .div_ceil(1000);
    let label = if episodes.len() > 1 {
        format!("{} 项等待你的输入", episodes.len())
    } else {
        "等待你的输入".into()
    };
    // This is the current request's remaining budget, not elapsed user time.
    Some(format!(
        "! {label} · 剩余 {:02}:{:02} · /wait",
        remaining / 60,
        remaining % 60
    ))
}

impl State {
    pub(in crate::client::terminal) fn user_wait_alert(snapshot: &Snapshot) -> Option<String> {
        hint(snapshot)
    }

    pub(super) fn chat_user_wait(&mut self, arguments: &str) -> Result<(), String> {
        if self.preview {
            return Err("预览不连接真实的用户等待".into());
        }
        let text = match arguments.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => {
                self.chat_change_view(View::Waits);
                self.conversation.content_focus = true;
                self.notice = None;
                self.dirty = true;
                return Ok(());
            }
            ["status"] => {
                let mut text = detail(&waiting::snapshot());
                if let Some(timing) = self.chat_timing_detail() {
                    text.push_str("\n\n");
                    text.push_str(&timing);
                }
                text
            }
            ["cancel", prefix] => {
                let id = if prefix.starts_with("timer_") || prefix.starts_with("watch_") {
                    crate::runtime::time::cancel_local(prefix)?;
                    (*prefix).to_owned()
                } else {
                    waiting::cancel(prefix)?
                };
                format!("已取消等待 #{id}；会话继续，消息队列和草稿保持原样")
            }
            _ => return Err(
                "用法：/wait 打开面板，/wait status 查看快照，/wait cancel <等待编号或唯一前缀>"
                    .into(),
            ),
        };
        self.chat_show_user_wait(&text, arguments.trim() == "status");
        Ok(())
    }

    fn chat_show_user_wait(&mut self, text: &str, inspecting: bool) {
        let text = text
            .split('\n')
            .map(panel::safe_text)
            .collect::<Vec<_>>()
            .join("\n");
        self.log_event(
            "user_wait",
            &json!({"type":"ssmcp.message","author":"User wait","text":text}),
            format!("\n[USER WAIT] {text}\n").as_bytes(),
        );
        self.notice = Some(if inspecting {
            "等待状态已显示 · /wait cancel <唯一编号> 可取消等待".into()
        } else {
            text
        });
        self.dirty = true;
    }

    fn chat_wait_hint_visible(&self) -> bool {
        !self.help
            && !self.selection
            && !self.conversation.logs_open
            && self.conversation.view == View::Activity
            && !self.conversation.detail
            && !self.conversation.content_focus
            && !self.chat_menu_visible()
            && !self.conversation.composer.completion_loading()
    }

    pub(super) fn chat_user_wait_hint(&self) -> Option<(String, PanelTone)> {
        if !self.chat_wait_hint_visible() {
            return None;
        }
        hint(&self.chat_user_wait_snapshot()).map(|text| (text, PanelTone::Error))
    }

    pub(super) fn chat_timing_footer_hint(&self) -> Option<String> {
        if !self.chat_wait_hint_visible() {
            return None;
        }
        self.chat_timing_hint()
    }

    pub(super) fn chat_user_wait_snapshot(&self) -> Snapshot {
        // The real frame regression uses a local snapshot, never process-wide
        // connection state that parallel tests could change.
        #[cfg(test)]
        if let Some(snapshot) = &self.conversation.wait_fixture {
            return snapshot.clone();
        }
        if self.preview {
            return Snapshot::default();
        }
        waiting::snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::interface::user_wait::Completion;
    use crate::client::notification::{Reason, Report};

    fn parse(state: &mut State) -> vt100::Parser {
        let frame = state.chat_frame(Instant::now());
        let rows = std::num::NonZeroU16::new(frame.geometry.rows).expect("nonzero frame rows");
        let cols = std::num::NonZeroU16::new(frame.geometry.cols).expect("nonzero frame cols");
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(&frame.diff(None));
        parser
    }

    fn screen(state: &mut State) -> String {
        parse(state).screen().contents()
    }

    fn entry(id: &str, episode: &str, intent: Intent) -> Entry {
        Entry {
            wait_id: id.into(),
            episode_id: episode.into(),
            intent,
            recipient: "next".into(),
            reason: Some("Synthetic wait reason".into()),
            remaining_ms: 12_500,
            finish_request_id: None,
            notification_requested: false,
            notification: None,
        }
    }

    #[test]
    fn user_wait_commands_stay_local_and_preserve_an_edited_draft() {
        assert_eq!(
            arguments("  /wait\n cancel exact-id  "),
            Some("cancel exact-id")
        );
        assert_eq!(arguments("/wait"), Some(""));
        assert_eq!(arguments("/waiting for text"), None);
        assert_eq!(arguments("/waiter"), None);
        for draft in ["/wait", "  /wait cancel exact-id"] {
            let mut state = super::super::super::preview_state(24, 48, &Ui::default());
            state.conversation.composer.set_text(draft);
            state.conversation.editing = Some("untouched-queue-id".into());
            state.conversation.target = Some("untouched-agent".into());
            state.chat_submit();
            assert_eq!(state.notice.as_deref(), Some("预览不连接真实的用户等待"));
            assert_eq!(state.conversation.composer.text(), draft);
            assert_eq!(
                state.conversation.editing.as_deref(),
                Some("untouched-queue-id")
            );
            assert_eq!(
                state.conversation.target.as_deref(),
                Some("untouched-agent")
            );
            assert!(!state.conversation.exit_requested);
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn user_wait_display_groups_retry_episodes_and_keeps_exact_cancel_ids() {
        let snapshot = Snapshot {
            connected: true,
            active: vec![
                entry("wait-alpha", "episode-a", Intent::NeedUser),
                entry("wait-alpine", "episode-a", Intent::NeedUser),
                entry("wait-beta", "episode-b", Intent::Finish),
            ],
            ..Snapshot::default()
        };
        assert_eq!(
            hint(&snapshot).as_deref(),
            Some("! 2 项等待你的输入 · 剩余 00:13 · /wait")
        );
        let text = detail(&snapshot);
        assert_eq!(text.matches("Synthetic wait reason").count(), 2);
        for id in ["#wait-alpha", "#wait-alpine", "#wait-beta"] {
            assert!(text.contains(id));
        }
        assert!(text.contains("完整编号或唯一前缀"));
        assert!(text.contains("等待你的输入"));
        let mixed = Snapshot {
            active: vec![
                entry("first-input", "same-episode", Intent::NeedUser),
                entry("second-finish", "same-episode", Intent::Finish),
            ],
            ..Snapshot::default()
        };
        assert_eq!(
            hint(&mixed).as_deref(),
            Some("! 等待你的输入 · 剩余 00:13 · /wait")
        );
        let text = detail(&mixed);
        assert!(text.contains("#first-input · 等待你的输入"));
        assert!(text.contains("#second-finish · 等待你的输入"));
        assert!(!text.contains("user_wait.received"));
    }

    #[test]
    fn user_wait_ready_and_notification_never_claim_user_delivery_or_seen() {
        let mut active = entry("wait-visible", "episode", Intent::Finish);
        active.notification_requested = true;
        active.notification = Some(Report {
            status: Status::Sent,
            reason: Reason::Submitted,
            deduplicated: true,
            user_seen: None,
        });
        let snapshot = Snapshot {
            connected: true,
            active: vec![active],
            recent: vec![Completion {
                wait_id: "previous-wait".into(),
                status: Outcome::Ready,
                notification: None,
                finish_request_id: None,
                finish_decision: None,
            }],
            ..Snapshot::default()
        };
        let text = detail(&snapshot);
        assert!(text.contains("未确认已读"));
        assert!(text.contains("消息已就绪，等待提交"));
        assert!(!text.contains("已收到") && !text.contains("用户已读"));
        assert!(unicode_width::UnicodeWidthStr::width(hint(&snapshot).unwrap().as_str()) <= 48);
    }

    #[test]
    fn user_wait_real_frame_keeps_wait_visible_with_a_draft_and_preserves_specific_controls() {
        for width in [120, 48] {
            let mut state = super::super::super::preview_state(28, width, &Ui::default());
            state.events.clear();
            state.notice = None;
            state
                .conversation
                .composer
                .set_text("Unsubmitted @file.txt\n    preserve indentation");
            state.conversation.wait_fixture = Some(Snapshot {
                connected: true,
                active: vec![entry("real-frame-wait", "same-episode", Intent::Finish)],
                ..Snapshot::default()
            });
            let original = state.conversation.composer.text().to_owned();
            let parsed = parse(&mut state);
            let rendered = parsed.screen().contents();
            let footer = rendered.lines().last().unwrap();
            assert!(
                footer.contains("! 等待你的输入 · 剩余 00:13 · /wait"),
                "{rendered}"
            );
            let cell = parsed.screen().cell(27, 1).unwrap();
            assert_eq!(cell.contents(), "!");
            assert_eq!(cell.fgcolor(), vt100::Color::Rgb(242, 139, 130));
            assert!(!cell.dim());
            assert_eq!(state.conversation.composer.text(), original);

            state.conversation.wait_fixture.as_mut().unwrap().active[0].remaining_ms = 10_999;
            let parsed = parse(&mut state);
            assert!(parsed.screen().contents().contains("剩余 00:11"));
            assert_eq!(
                parsed.screen().cell(27, 1).unwrap().fgcolor(),
                vt100::Color::Rgb(242, 139, 130)
            );

            state.notice = Some("Synthetic action failed".into());
            let rendered = screen(&mut state);
            assert!(
                rendered
                    .lines()
                    .last()
                    .unwrap()
                    .contains("Synthetic action failed"),
                "{rendered}"
            );
            state.notice = None;
            state.conversation.composer.set_text("/wa");
            let rendered = screen(&mut state);
            assert!(
                rendered.contains("› /wait") && rendered.contains("查看计时和用户等待"),
                "{rendered}"
            );
            assert!(
                state
                    .chat_contextual_hint("")
                    .unwrap()
                    .contains("Tab insert")
            );
            assert!(state.chat_user_wait_hint().is_none());
            // A narrow footer may retain only the existing connection label;
            // the selected command remains visible above it.
            if width == 120 {
                assert!(
                    rendered.lines().last().unwrap().contains("Tab insert"),
                    "{rendered}"
                );
            }
            assert!(!rendered.lines().last().unwrap().contains("等待你的输入"));
            state.conversation.composer.set_text(&original);
            state.conversation.content_focus = true;
            let rendered = screen(&mut state);
            assert!(
                state
                    .chat_contextual_hint("")
                    .unwrap()
                    .contains("Enter expand")
            );
            assert!(state.chat_user_wait_hint().is_none());
            if width == 120 {
                assert!(
                    rendered.lines().last().unwrap().contains("Enter expand"),
                    "{rendered}"
                );
            }
            assert!(!rendered.contains("等待你的输入"));
            assert_eq!(state.conversation.composer.text(), original);

            // Removing the real wait must also remove its red alert. A live
            // background task still has a clock, but is not waiting for a human.
            state.conversation.content_focus = false;
            state
                .conversation
                .wait_fixture
                .as_mut()
                .unwrap()
                .active
                .clear();
            state.panel.tasks = vec![crate::client::panel::TaskView {
                id: "#background".into(),
                description: "Ordinary background work".into(),
                cwd: None,
                started: Instant::now(),
                deadline: None,
            }];
            let parsed = parse(&mut state);
            assert!(!parsed.screen().contents().contains("等待你的输入"));
            assert_ne!(
                parsed.screen().cell(27, 1).unwrap().fgcolor(),
                vt100::Color::Rgb(242, 139, 130)
            );
            assert_eq!(state.conversation.composer.text(), original);
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn background_time_keeps_native_receipts_visible_but_human_wait_has_priority() {
        for width in [120, 48] {
            let mut state = super::super::super::preview_state(28, width, &Ui::default());
            state.events.clear();
            state.notice = None;
            state.conversation.wait_fixture = Some(Snapshot::default());
            state.conversation.timing_fixture = Some(crate::runtime::time::Snapshot::default());
            state.panel.tasks = vec![crate::client::panel::TaskView {
                id: "#background".into(),
                description: "Ordinary background work".into(),
                cwd: None,
                started: Instant::now() - Duration::from_secs(5),
                deadline: None,
            }];
            let recipient =
                "client:1111111111111111111111111111111111111111111111111111111111111111";
            state.conversation.native_recipient = Some(recipient.into());
            state.conversation.native_receipt_background = Some(state.chat_background_ids());
            state.conversation.composer.set_text("Unsent draft 界");
            state.conversation.composer.left();
            let draft = state.conversation.snapshot()["draft"].clone();
            for (status, label) in [
                ("queued", "Queued by service"),
                ("included", "Included in recipient tool response"),
                ("acknowledged", "Acknowledged by recipient"),
            ] {
                let receipt = json!({
                    "recipient_id": recipient,
                    "message_id": "native-exact-receipt",
                    "request_id": "ui-exact-request",
                    "text": "Previously submitted text",
                    "status": status
                });
                state.conversation.native_last = Some(receipt.clone());
                state.conversation.native_fixture = Some(json!({
                    "available": true, "connected": true,
                    "recipients": [], "outgoing": [receipt]
                }));
                let rendered = screen(&mut state);
                assert!(
                    rendered.lines().last().unwrap().contains(label),
                    "{rendered}"
                );
                assert!(rendered.contains("运行 00:"), "{rendered}");
                assert_eq!(state.conversation.snapshot()["draft"], draft);
                assert_eq!(
                    state.conversation.native_recipient.as_deref(),
                    Some(recipient)
                );

                state.conversation.wait_fixture.as_mut().unwrap().active =
                    vec![entry("human-wait", "human-episode", Intent::Finish)];
                let rendered = screen(&mut state);
                assert!(
                    rendered.lines().last().unwrap().contains("等待你的输入"),
                    "{rendered}"
                );
                assert!(rendered.contains("等待 00:13"), "{rendered}");
                assert_eq!(state.conversation.snapshot()["draft"], draft);
                state
                    .conversation
                    .wait_fixture
                    .as_mut()
                    .unwrap()
                    .active
                    .clear();
            }
            state.conversation.native_recipient = None;
            let rendered = screen(&mut state);
            assert!(
                rendered.lines().last().unwrap().contains("任务 · 运行"),
                "{rendered}"
            );
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn user_wait_real_detail_frame_preserves_lines_and_escapes_terminal_controls() {
        let mut active = entry("wait-lines", "same-episode", Intent::NeedUser);
        active.reason = Some("Synthetic reason \u{1b}[2J\u{7}".into());
        let snapshot = Snapshot {
            connected: true,
            active: vec![active],
            ..Snapshot::default()
        };
        for width in [120, 48] {
            let mut state = super::super::super::preview_state(28, width, &Ui::default());
            state.events.clear();
            state.conversation.composer.set_text("/wait");
            state.chat_show_user_wait(&detail(&snapshot), true);
            let rendered = screen(&mut state);
            assert!(!rendered.contains("\\n"), "{rendered}");
            assert!(
                rendered.contains("\\x1b[2J") && rendered.contains("\\u{7}"),
                "{rendered}"
            );
            let lines = rendered.lines().collect::<Vec<_>>();
            let reason = lines
                .iter()
                .position(|line| line.contains("Synthetic reason"))
                .expect("the reason is visible");
            let wait_id = lines
                .iter()
                .position(|line| line.contains("#wait-lines"))
                .expect("the precise wait ID is visible");
            let notification = lines
                .iter()
                .position(|line| line.contains("未请求系统通知"))
                .expect("the notification state is visible");
            assert!(reason < wait_id && wait_id < notification, "{rendered}");
            assert_eq!(state.conversation.composer.text(), "/wait");
        }
    }
}
