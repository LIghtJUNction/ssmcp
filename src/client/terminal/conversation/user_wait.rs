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
        Intent::NeedUser => "等待你的输入",
        Intent::Finish => "等待你确认是否结束",
    }
}

fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ready => "消息已就绪，等待提交",
        Outcome::TimedOut => "本轮等待超时",
        Outcome::Cancelled => "等待已取消，会话继续",
        Outcome::Disconnected => "连接已断开",
        Outcome::AuthorizationRevoked => "权限已撤销",
        Outcome::BindingInvalid => "会话绑定已失效",
        Outcome::Superseded => "等待已被替代",
    }
}

fn detail(snapshot: &Snapshot) -> String {
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
    let finish = snapshot
        .active
        .iter()
        .any(|entry| entry.intent == Intent::Finish);
    if episodes.len() > 1 {
        Some(if finish {
            format!("{} 项等待 · 含结束确认 · /wait", episodes.len())
        } else {
            format!("{} 项等待你的回应 · /wait", episodes.len())
        })
    } else if finish {
        Some(format!("{} · /wait", intent_text(Intent::Finish)))
    } else {
        snapshot
            .active
            .first()
            .map(|entry| format!("{} · /wait", intent_text(entry.intent)))
    }
}

impl State {
    pub(super) fn chat_user_wait(&mut self, arguments: &str) -> Result<(), String> {
        if self.preview {
            return Err("预览不连接真实的用户等待".into());
        }
        let text = match arguments.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => detail(&waiting::snapshot()),
            ["cancel", prefix] => {
                let id = waiting::cancel(prefix)?;
                format!("已取消等待 #{id}；会话继续，消息队列和草稿保持原样")
            }
            _ => return Err("用法：/wait 或 /wait cancel <等待编号或唯一前缀>".into()),
        };
        let text = panel::safe_text(&text);
        self.log_event(
            "user_wait",
            &json!({"type":"ssmcp.message","author":"User wait","text":text}),
            format!("\n[USER WAIT] {text}\n").as_bytes(),
        );
        self.notice = Some(if arguments.is_empty() {
            "等待状态已显示 · /wait cancel <唯一编号> 可取消等待".into()
        } else {
            text
        });
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_user_wait_hint(&self) -> Option<String> {
        if self.help
            || self.selection
            || self.conversation.logs_open
            || self.conversation.view != View::Activity
            || self.conversation.detail
            || self.conversation.content_focus
            || self.chat_menu_visible()
            || self.conversation.composer.completion_loading()
        {
            return None;
        }
        // The real frame regression uses a local snapshot, never process-wide
        // connection state that parallel tests could change.
        #[cfg(test)]
        if let Some(snapshot) = &self.conversation.wait_fixture {
            return hint(snapshot);
        }
        if self.preview {
            return None;
        }
        hint(&waiting::snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::interface::user_wait::Completion;
    use crate::client::notification::{Reason, Report};

    fn entry(id: &str, episode: &str, intent: Intent) -> Entry {
        Entry {
            wait_id: id.into(),
            episode_id: episode.into(),
            intent,
            recipient: "next".into(),
            reason: Some("Synthetic wait reason".into()),
            remaining_ms: 12_500,
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
            Some("2 项等待 · 含结束确认 · /wait")
        );
        let text = detail(&snapshot);
        assert_eq!(text.matches("Synthetic wait reason").count(), 2);
        for id in ["#wait-alpha", "#wait-alpine", "#wait-beta"] {
            assert!(text.contains(id));
        }
        assert!(text.contains("完整编号或唯一前缀"));
        assert!(text.contains("等待你确认是否结束"));
        let mixed = Snapshot {
            active: vec![
                entry("first-input", "same-episode", Intent::NeedUser),
                entry("second-finish", "same-episode", Intent::Finish),
            ],
            ..Snapshot::default()
        };
        assert_eq!(hint(&mixed).as_deref(), Some("等待你确认是否结束 · /wait"));
        let text = detail(&mixed);
        assert!(text.contains("#first-input · 等待你的输入"));
        assert!(text.contains("#second-finish · 等待你确认是否结束"));
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
        fn screen(state: &mut State) -> String {
            let frame = state.chat_frame(Instant::now());
            let rows = std::num::NonZeroU16::new(frame.geometry.rows).expect("nonzero frame rows");
            let cols = std::num::NonZeroU16::new(frame.geometry.cols).expect("nonzero frame cols");
            let mut parser = vt100::Parser::new(rows, cols, 0);
            parser.process(&frame.diff(None));
            parser.screen().contents()
        }
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
            let rendered = screen(&mut state);
            let footer = rendered.lines().last().unwrap();
            assert!(
                footer.contains("等待你确认是否结束") && footer.contains("/wait"),
                "{rendered}"
            );
            assert_eq!(state.conversation.composer.text(), original);

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
                rendered.contains("› /wait") && rendered.contains("查看用户等待"),
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
            assert!(
                !rendered
                    .lines()
                    .last()
                    .unwrap()
                    .contains("等待你确认是否结束")
            );
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
            assert!(!rendered.contains("等待你确认是否结束"));
            assert_eq!(state.conversation.composer.text(), original);
        }
    }
}
