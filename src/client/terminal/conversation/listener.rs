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
                let text = runtime.expand_references(&task_source).await?;
                let ticket = receiver::submit(target, task_source.clone(), text)?;
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
            return Err(self
                .ui
                .language
                .text(
                    "Use /send <text> to send to this session’s listener",
                    "用法：/send <正文>；强制发送给当前会话监听程序",
                )
                .into());
        }
        if self.preview {
            return Err(self
                .ui
                .language
                .text("Preview does not send messages", "预览不会发送消息")
                .into());
        }
        if self.conversation.logout_task.is_some() || self.conversation.exit_requested {
            return Err(self
                .ui
                .language
                .text(
                    "Send failed: the connection is closing. Draft preserved",
                    "发送失败：当前连接正在关闭，草稿已保留",
                )
                .into());
        }
        if self.conversation.editing.is_some() || self.conversation.submitting.is_some() {
            return Err(self.ui.language.text("Send failed: finish editing or submitting the current queued message first. Draft preserved", "发送失败：请先完成当前队列消息的编辑或提交，草稿已保留").into());
        }
        if let Some(target) = self
            .conversation
            .target
            .as_deref()
            .filter(|target| *target != "next")
            && receiver::snapshot()["listener"]["thread_id"].as_str() != Some(target)
        {
            return Err(self
                .ui
                .language
                .text(
                    "Send failed: no listener matches the selected agent. Draft preserved",
                    "发送失败：没有对应指定 agent 的监听程序；草稿已保留",
                )
                .into());
        }
        if self.conversation.force_task.is_some() {
            return Err(self
                .ui
                .language
                .text(
                    "Waiting for listener acknowledgement. Draft preserved",
                    "正在等待监听程序确认，草稿已保留",
                )
                .into());
        }
        steering::validate_text(text)?;
        let stamp = self.chat_input_stamp().ok_or(self.ui.language.text(
            "Send failed: the current input is unavailable",
            "发送失败：当前输入不可用",
        ))?;
        let task = Task::start(stamp, text.into(), self.conversation.target.as_deref())?;
        self.conversation.force_task = Some(task);
        self.notice = Some(
            self.ui
                .language
                .text(
                    "Sending to the listener… awaiting acknowledgement. Draft preserved",
                    "正在发送给监听程序…等待接收确认，草稿已保留",
                )
                .into(),
        );
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
                    record.push_str(
                        self.ui
                            .language
                            .text("  Accepted by the listener\n\n", "  监听程序已接收\n\n"),
                    );
                    self.log_message("user", source, record.as_bytes());
                }
                if let Some(error) = completed.history_error {
                    localized!(
                        self.ui.language,
                        "{}; history was not saved: {error}",
                        "{}；历史记录未保存：{error}",
                        self.ui.language.local_message(&completed.receipt.detail)
                    )
                } else {
                    completed.receipt.detail
                }
            }
            Err(error) if error.starts_with("发送") => error,
            Err(error) => localized!(
                self.ui.language,
                "Send failed: {error}; draft preserved",
                "发送失败：{error}；草稿已保留"
            ),
        };
        let detail = panel::safe_text(&self.ui.language.local_message(&detail));
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
        let mut state = super::super::super::preview_state(
            24,
            100,
            &Ui {
                language: Language::Chinese,
                ..Ui::default()
            },
        );
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
}
