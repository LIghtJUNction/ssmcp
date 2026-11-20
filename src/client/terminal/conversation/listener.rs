//! Explicit force-send UI; a receipt never claims that a model has run.
use super::*;
use crate::client::interface::listener::{self as receiver, Receipt, Status};
use tokio::{sync::oneshot, task::JoinHandle};

/// Strip the command and one separator, preserving the message's indentation.
pub(super) fn body(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/send")?;
    let Some(separator) = rest.chars().next() else {
        return Some("");
    };
    separator
        .is_whitespace()
        .then(|| &rest[separator.len_utf8()..])
}

struct Completed {
    receipt: Receipt,
    history_error: Option<String>,
    reference_warnings: Vec<String>,
}

pub(super) struct Task {
    stamp: InputStamp,
    source: String,
    receiver: oneshot::Receiver<Result<Completed, String>>,
    handle: Option<JoinHandle<()>>,
}

impl Task {
    fn start(
        stamp: InputStamp,
        source: String,
        selected_thread: Option<&str>,
    ) -> Result<Self, String> {
        // Check before file preparation, then recheck the same epoch/lease when
        // committing the delivery. A replacement listener cannot inherit it.
        let target = receiver::target(selected_thread)?;
        let (runtime, handle) = crate::client::interface::harness::local_runtime()?;
        let (sender, result) = oneshot::channel();
        let task_source = source.clone();
        let task = handle.spawn(async move {
            let outcome = async {
                let prepared = runtime.prepare_references(&task_source).await?;
                let reference_warnings = prepared.warnings;
                let ticket = receiver::submit(target, task_source.clone(), prepared.text)?;
                let receipt = ticket.wait().await?;
                let history_error = if receipt.status == Status::Accepted {
                    let id = receipt.delivery_id.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::client::interface::history::record_request(&id, &task_source)
                    })
                    .await
                    {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error),
                        Err(_) => Some("history storage worker stopped".into()),
                    }
                } else {
                    None
                };
                Ok(Completed {
                    receipt,
                    history_error,
                    reference_warnings,
                })
            }
            .await;
            let _ = sender.send(outcome);
            crate::client::terminal::invalidate();
        });
        Ok(Self {
            stamp,
            source,
            receiver: result,
            handle: Some(task),
        })
    }

    fn poll(&mut self) -> Option<Result<Completed, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => {
                Some(Err("发送结果未确认：发送任务已结束，不会自动重发".into()))
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

impl State {
    pub(super) fn chat_force_send(&mut self, text: &str) -> Result<(), String> {
        if text.trim().is_empty() {
            return Err("用法：/send <正文>；强制发送给当前会话监听程序".into());
        }
        if self.conversation.native_recipient.is_some() {
            return Err(
                "发送失败：此收件人不支持 /send；请明确选择 Next caller 或已登记的线程，草稿和收件人已保留"
                    .into(),
            );
        }
        if self.preview {
            return Err("预览不会发送消息".into());
        }
        if self.conversation.logout_task.is_some() || self.conversation.exit_requested {
            return Err("发送失败：当前连接正在关闭，草稿已保留".into());
        }
        if self.conversation.editing.is_some() || self.conversation.submitting.is_some() {
            return Err("发送失败：请先完成当前队列消息的编辑或提交，草稿已保留".into());
        }
        if let Some(target) = self
            .conversation
            .target
            .as_deref()
            .filter(|target| *target != "next")
            && receiver::snapshot()["listener"]["thread_id"].as_str() != Some(target)
        {
            return Err("发送失败：没有对应指定 agent 的监听程序；草稿已保留".into());
        }
        if self.conversation.force_task.is_some() {
            return Err("正在等待监听程序确认，草稿已保留".into());
        }
        steering::validate_text(text)?;
        let stamp = self.chat_input_stamp().ok_or("发送失败：当前输入不可用")?;
        let task = Task::start(stamp, text.into(), self.conversation.target.as_deref())?;
        self.conversation.force_task = Some(task);
        self.notice = Some("正在发送给监听程序…等待接收确认，草稿已保留".into());
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_poll_force_send(&mut self) {
        let Some(result) = self.conversation.force_task.as_mut().and_then(Task::poll) else {
            return;
        };
        let task = self
            .conversation
            .force_task
            .take()
            .expect("completed force-send task");
        self.chat_finish_force_send(&task.stamp, &task.source, result);
    }

    fn chat_finish_force_send(
        &mut self,
        stamp: &InputStamp,
        source: &str,
        result: Result<Completed, String>,
    ) {
        let detail = match result {
            Ok(completed) => {
                if completed.receipt.status == Status::Accepted {
                    let unchanged = self.chat_input_stamp().as_ref() == Some(stamp);
                    self.conversation
                        .composer
                        .remember(&format!("/send {source}"));
                    if unchanged {
                        self.conversation.composer.take_text();
                    }
                    let mut record = format!("\n[USER] #{}\n", completed.receipt.delivery_id);
                    for line in markdown::rows(source, self.geometry.left, &self.ui) {
                        let _ = writeln!(record, "{line}");
                    }
                    record.push_str("  监听程序已接收\n\n");
                    self.log_message("user", source, record.as_bytes());
                }
                let mut detail = completed.receipt.detail;
                if completed.receipt.status == Status::Accepted
                    && !completed.reference_warnings.is_empty()
                {
                    detail.push_str("；引用提示：");
                    detail.push_str(&completed.reference_warnings.join("；"));
                }
                if let Some(error) = completed.history_error {
                    let _ = write!(detail, "；历史记录未保存：{error}");
                }
                detail
            }
            Err(error) if error.starts_with("发送") => error,
            Err(error) => format!("发送失败：{error}；草稿已保留"),
        };
        self.chat_listener_notice(&detail);
    }

    pub(super) fn chat_restore_interrupted_force_send(&mut self) {
        if let Some(receipt) = receiver::take_interrupted_sender_receipt() {
            self.chat_show_interrupted_force_send(receipt);
        }
    }

    fn chat_show_interrupted_force_send(&mut self, receipt: Receipt) {
        self.chat_listener_notice(&format!("上次连接：{}", receipt.detail));
    }

    fn chat_listener_notice(&mut self, detail: &str) {
        let detail = panel::safe_text(detail);
        self.log_event(
            "listener",
            &json!({"type":"ssmcp.message","author":"Listener","text":detail}),
            format!("\n[LISTENER] {detail}\n").as_bytes(),
        );
        self.notice = Some(detail);
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(text: &str) -> State {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.events.clear();
        state.conversation.composer.set_text(text);
        state
    }

    fn completed(status: Status) -> Result<Completed, String> {
        Ok(Completed {
            receipt: Receipt {
                cursor: 1,
                delivery_id: "synthetic-delivery".into(),
                connection_id: "synthetic-connection".into(),
                listener_id: "synthetic-listener".into(),
                status,
                code: "fixture".into(),
                detail: match status {
                    Status::Accepted => "监听程序已接收",
                    Status::Rejected => "发送失败：监听程序拒绝接收",
                    Status::Failed => "发送失败：没有程序在监听",
                    Status::Unknown => "发送结果未确认：不会自动重发",
                }
                .into(),
            },
            history_error: None,
            reference_warnings: Vec::new(),
        })
    }

    #[test]
    fn force_listener_command_parser_preserves_the_body_and_reports_empty_input() {
        assert_eq!(body("  /send hello"), Some("hello"));
        assert_eq!(body("/send\n    code\n"), Some("    code\n"));
        assert_eq!(body("/send\t  中文\n"), Some("  中文\n"));
        assert_eq!(body("/send   "), Some("  "));
        assert_eq!(body("/send"), Some(""));
        assert_eq!(body("/sender x"), None);
        for text in ["  /send hello", "/send\n    code\n"] {
            let mut state = preview(text);
            state.chat_submit();
            assert_eq!(state.notice.as_deref(), Some("预览不会发送消息"));
            assert_eq!(state.conversation.composer.text(), text);
            assert!(state.actions.is_empty());
        }
        let mut state = preview("/send   ");
        assert!(
            state
                .chat_command("/send   ")
                .unwrap_err()
                .starts_with("用法：/send")
        );
        assert_eq!(state.conversation.composer.text(), "/send   ");
    }

    #[test]
    fn force_listener_submit_rejects_a_native_recipient_without_sending_or_rerouting() {
        let original = "/send 保留此收件人\n    @missing.txt $missing-skill\n";
        let mut state = preview(original);
        let recipient = format!("client:{}", "a".repeat(64));
        state.conversation.native_recipient = Some(recipient.clone());
        state.conversation.native_account = Some(("github".into(), "fixture-account".into()));
        state.conversation.target = Some("previous-thread".into());
        let stamp = state.chat_input_stamp().unwrap();
        let saved = state.chat_saved_draft();

        state.chat_submit();

        assert!(state.notice.as_deref().is_some_and(|notice| {
            notice.starts_with("发送失败：此收件人不支持 /send")
                && notice.contains("Next caller 或已登记的线程")
        }));
        assert_eq!(state.conversation.composer.text(), original);
        assert_eq!(state.chat_input_stamp().unwrap(), stamp);
        assert_eq!(state.chat_saved_draft(), saved);
        assert_eq!(
            state.conversation.native_recipient.as_deref(),
            Some(recipient.as_str())
        );
        assert_eq!(
            state.conversation.native_account,
            Some(("github".into(), "fixture-account".into()))
        );
        assert!(state.conversation.native_pending.is_none());
        assert!(state.conversation.native_retry.is_none());
        assert!(state.conversation.force_task.is_none());
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        assert!(
            state.events.snapshot()["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn force_listener_accepted_receipt_clears_only_the_unchanged_draft() {
        let source = "@\"中文 空格.txt\"\n    code\n";
        let original = format!("/send {source}");
        let mut unchanged = preview(&original);
        let stamp = unchanged.chat_input_stamp().unwrap();
        unchanged.chat_finish_force_send(&stamp, source, completed(Status::Accepted));
        assert!(unchanged.conversation.composer.is_empty());
        assert_eq!(unchanged.notice.as_deref(), Some("监听程序已接收"));
        assert!(unchanged.actions.is_empty());

        for mode in ["new text", "same text edited again", "new target"] {
            let mut state = preview(&original);
            let stamp = state.chat_input_stamp().unwrap();
            match mode {
                "new text" => {
                    state.conversation.composer.set_text("New unsent draft");
                }
                "same text edited again" => {
                    state.conversation.composer.set_text(&original);
                }
                "new target" => {
                    state.conversation.target = Some("agent_exact".into());
                }
                _ => unreachable!(),
            }
            let after_edit = state.chat_input_stamp().unwrap();
            assert_ne!(after_edit, stamp);
            state.chat_finish_force_send(&stamp, source, completed(Status::Accepted));
            assert_eq!(state.chat_input_stamp().unwrap(), after_edit, "{mode}");
            assert_eq!(state.notice.as_deref(), Some("监听程序已接收"));
            assert!(state.actions.is_empty());
        }
    }

    #[test]
    fn force_listener_failure_and_unknown_preserve_references_and_draft_revision() {
        for status in [Status::Rejected, Status::Failed, Status::Unknown] {
            let original = "/send @\"中文 空格.txt\"\n    code\n";
            let mut state = preview(original);
            let stamp = state.chat_input_stamp().unwrap();
            state.chat_finish_force_send(&stamp, body(original).unwrap(), completed(status));
            assert_eq!(state.chat_input_stamp().unwrap(), stamp);
            assert!(
                state
                    .notice
                    .as_deref()
                    .is_some_and(|text| text.starts_with("发送"))
            );
            assert!(state.actions.is_empty());
            assert!(
                !state.events.snapshot()["events"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[test]
    fn force_listener_reference_notices_follow_only_an_accepted_receipt() {
        for status in [
            Status::Accepted,
            Status::Rejected,
            Status::Failed,
            Status::Unknown,
        ] {
            let original = "/send 请创建 @missing.txt";
            let mut state = preview(original);
            let stamp = state.chat_input_stamp().unwrap();
            let mut result = completed(status).unwrap();
            result.reference_warnings = vec!["@missing.txt: file unavailable".into()];
            state.chat_finish_force_send(&stamp, body(original).unwrap(), Ok(result));
            let notice = state.notice.as_deref().unwrap();
            if status == Status::Accepted {
                assert!(notice.starts_with("监听程序已接收；引用提示："));
                assert!(notice.contains("@missing.txt: file unavailable"));
                assert!(state.conversation.composer.is_empty());
            } else {
                assert!(notice.starts_with("发送"));
                assert!(!notice.contains("引用提示"));
                assert_eq!(state.chat_input_stamp().unwrap(), stamp);
            }
        }
    }

    #[test]
    fn force_listener_does_not_reroute_an_exact_agent_or_edit_a_queued_request() {
        let mut state = preview("/send Keep this recipient");
        state.preview = false;
        state.conversation.target = Some("agent_exact".into());
        let before = state.chat_input_stamp().unwrap();
        assert!(
            state
                .chat_command("/send Keep this recipient")
                .unwrap_err()
                .contains("指定 agent")
        );
        assert_eq!(state.chat_input_stamp().unwrap(), before);
        state.conversation.target = None;
        state.conversation.editing = Some("queued-message".into());
        let before = state.chat_input_stamp().unwrap();
        assert!(
            state
                .chat_command("/send Keep this recipient")
                .unwrap_err()
                .contains("队列消息")
        );
        assert_eq!(state.chat_input_stamp().unwrap(), before);
        assert!(state.actions.is_empty());
        assert!(state.conversation.force_task.is_none());
    }
    #[test]
    fn force_listener_restored_disconnect_notice_preserves_the_current_draft() {
        for status in [Status::Failed, Status::Unknown] {
            let original = "/send Keep the original draft after reconnect";
            let mut state = preview(original);
            let stamp = state.chat_input_stamp().unwrap();
            let mut receipt = completed(status).unwrap().receipt;
            receipt.code = "connection_ended".into();
            receipt.detail = match status {
                Status::Failed => "发送失败：连接已结束，消息未投递",
                Status::Unknown => {
                    "发送结果未确认：已尝试投递，但未收到监听程序的接收确认；不会自动重发"
                }
                _ => unreachable!(),
            }
            .into();
            state.chat_show_interrupted_force_send(receipt);
            assert_eq!(state.chat_input_stamp().unwrap(), stamp);
            assert!(state.actions.is_empty());
            assert!(state.conversation.force_task.is_none());
            assert!(
                state
                    .notice
                    .as_deref()
                    .is_some_and(|detail| detail.starts_with("上次连接：发送"))
            );
            assert_eq!(state.events.snapshot()["retained_events"], 1);
            // Ordinary conversation snapshots expose a generic summary, not
            // their body. Clear the transient notice and verify the retained
            // result through the same Frame/VT path used by the terminal.
            state.ui.layout = Layout::Chat;
            state.ui.sidebar = false;
            state.resize(24, 120);
            state.chat_change_view(View::Activity);
            assert!(state.notice.is_none());
            let frame = state.frame(Instant::now());
            let mut screen = vt100::Parser::new(frame.geometry.rows, frame.geometry.cols, 0);
            screen.process(&frame.diff(None));
            let visible = screen.screen().contents();
            assert_eq!(visible.matches("上次连接").count(), 1, "{visible}");
            assert!(visible.contains("上次连接：发送"), "{visible}");
            match status {
                Status::Failed => {
                    assert!(visible.contains("连接已结束，消息未投递"), "{visible}");
                }
                Status::Unknown => {
                    assert!(visible.contains("发送结果未确认"), "{visible}");
                    assert!(visible.contains("不会自动重发"), "{visible}");
                }
                _ => unreachable!(),
            }
            assert_eq!(state.chat_input_stamp().unwrap(), stamp);
            assert!(state.actions.is_empty());
        }
    }
}
