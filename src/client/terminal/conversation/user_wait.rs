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
        Outcome::Ready => "消息已就绪，等待外层交付",
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
            let first = entries[0];
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
                lines.push(format!("#{} · 本轮剩余 {remaining}s", entry.wait_id));
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
    lines.push("只有外层 user_wait.received 随附用户消息，才表示交付已提交".into());
    lines.join("\n")
}

fn hint(snapshot: &Snapshot) -> Option<String> {
    let episodes = groups(snapshot);
    if episodes.len() > 1 {
        Some(format!("{} 项等待你的回应 · /wait", episodes.len()))
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
        if self.preview
            || self.help
            || self.conversation.logs_open
            || self.conversation.view != View::Activity
            || self.conversation.detail
            || self.chat_contextual_hint("").is_some()
        {
            return None;
        }
        hint(&waiting::snapshot())
    }
}
