//! Explicit force-send UI; a receipt never claims that a model has run.
use super::*;
use crate::client::interface::listener::{self as receiver, Receipt, Status};
use tokio::{sync::oneshot, task::JoinHandle};

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
    fn start(stamp: InputStamp, source: String) -> Result<Self, String> {
        // Check before file preparation, then recheck the same epoch/lease when
        // committing the delivery. A replacement listener cannot inherit it.
        let target = receiver::target()?;
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
        if self.preview {
            return Err("预览不会发送消息".into());
        }
        if self.conversation.logout_task.is_some() || self.conversation.exit_requested {
            return Err("发送失败：当前连接正在关闭，草稿已保留".into());
        }
        if self.conversation.editing.is_some() || self.conversation.submitting.is_some() {
            return Err("发送失败：请先完成当前队列消息的编辑或提交，草稿已保留".into());
        }
        if self
            .conversation
            .target
            .as_deref()
            .is_some_and(|target| target != "next")
        {
            return Err(
                "发送失败：不能把指定 agent 的消息改投监听程序；请先选择 Next caller，草稿已保留"
                    .into(),
            );
        }
        if self.conversation.force_task.is_some() {
            return Err("正在等待监听程序确认，草稿已保留".into());
        }
        if text.trim().is_empty() {
            return Err("用法：/send <正文>；强制发送给当前会话监听程序".into());
        }
        steering::validate_text(text)?;
        let stamp = self.chat_input_stamp().ok_or("发送失败：当前输入不可用")?;
        let task = Task::start(stamp, text.into())?;
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
                if let Some(error) = completed.history_error {
                    format!("{}；历史记录未保存：{error}", completed.receipt.detail)
                } else {
                    completed.receipt.detail
                }
            }
            Err(error) if error.starts_with("发送") => error,
            Err(error) => format!("发送失败：{error}；草稿已保留"),
        };
        let detail = panel::safe_text(&detail);
        self.log_event(
            "listener",
            &json!({"type":"ssmcp.message","author":"Listener","text":detail}),
            format!("\n[LISTENER] {detail}\n").as_bytes(),
        );
        self.notice = Some(detail);
        self.dirty = true;
    }
}
