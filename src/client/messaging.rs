//! Nonblocking terminal sender. Only broker receipts mean queued or delivered.
use crate::wire::messages::{Command, Receipt, Recipient, Reply};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(3);
const UNSUPPORTED: &str = "当前连接不支持原生消息；请升级并连接已认证的 SSMCP 客户端和服务器";
static STATE: OnceLock<Mutex<Manager>> = OnceLock::new();

pub(super) struct Request {
    pub id: u64,
    pub command: Command,
}
struct Pending {
    command: Command,
    deadline: Instant,
}
#[derive(Clone, Serialize)]
struct Outgoing {
    request_id: String,
    recipient_id: String,
    text: String,
    status: String,
    message_id: Option<String>,
    error: Option<String>,
}
#[derive(Default)]
struct Manager {
    connected: bool,
    supported: bool,
    recipients: Vec<Recipient>,
    outgoing: Vec<Outgoing>,
    error: Option<String>,
    sender: Option<mpsc::Sender<Request>>,
    pending: BTreeMap<u64, Pending>,
    next_id: u64,
    next_poll: Option<Instant>,
}
impl Manager {
    fn available(&self) -> bool {
        self.connected && self.supported
    }
    fn snapshot(&self) -> Value {
        json!({
            "available":self.available(), "connected":self.connected,
            "recipients":self.recipients, "outgoing":self.outgoing,
            "error":self.error, "pending":!self.pending.is_empty(),
        })
    }
    fn connect(&mut self, supported: bool) -> mpsc::Receiver<Request> {
        self.disconnect();
        let (sender, receiver) = mpsc::channel(16);
        self.sender = Some(sender);
        self.connected = true;
        self.supported = supported;
        self.error = None;
        self.next_poll = Some(Instant::now() + POLL);
        if supported {
            let _ = self.refresh();
        }
        receiver
    }
    fn request(&mut self, command: Command) -> Result<(), String> {
        if !self.available() {
            return Err(UNSUPPORTED.into());
        }
        command.validate()?;
        if self.pending.len() >= 16 {
            return Err("原生消息请求正在处理中，请稍后重试".into());
        }
        let id = self
            .next_id
            .checked_add(1)
            .ok_or("原生消息请求编号已耗尽")?;
        self.sender
            .as_ref()
            .ok_or("原生消息连接已关闭")?
            .try_send(Request {
                id,
                command: command.clone(),
            })
            .map_err(|_| "原生消息尚未送出，请稍后重试")?;
        self.next_id = id;
        self.pending.insert(
            id,
            Pending {
                command,
                deadline: Instant::now() + TIMEOUT,
            },
        );
        Ok(())
    }
    fn refresh(&mut self) -> Result<(), String> {
        if self
            .pending
            .values()
            .any(|pending| matches!(pending.command, Command::List {}))
        {
            return Ok(());
        }
        self.request(Command::List {})
    }
    fn send(&mut self, recipient_id: &str, text: &str, request_id: &str) -> Result<(), String> {
        let command = Command::Send {
            recipient_id: recipient_id.into(),
            text: text.into(),
            request_id: request_id.into(),
        };
        command.validate()?;
        if !self
            .recipients
            .iter()
            .any(|recipient| recipient.id == recipient_id && recipient.online && recipient.sendable)
        {
            return Err("请刷新并明确选择当前收件人；不会把名称或文件路径自动转换为收件人".into());
        }
        if let Some(previous) = self
            .outgoing
            .iter()
            .find(|message| message.request_id == request_id)
        {
            if previous.recipient_id != recipient_id || previous.text != text {
                return Err("同一发送请求编号不能改为不同正文或收件人".into());
            }
            if previous.status != "failed" {
                return self.refresh();
            }
        }
        if self.outgoing.len() >= 128 {
            let Some(index) = self
                .outgoing
                .iter()
                .position(|message| matches!(message.status.as_str(), "acknowledged" | "failed"))
            else {
                return Err("发送记录已满，请先等待现有消息回执".into());
            };
            self.outgoing.remove(index);
        }
        self.request(command)?;
        if let Some(previous) = self
            .outgoing
            .iter_mut()
            .find(|message| message.request_id == request_id)
        {
            previous.status = "submitting".into();
            previous.error = None;
        } else {
            self.outgoing.push(Outgoing {
                request_id: request_id.into(),
                recipient_id: recipient_id.into(),
                text: text.into(),
                status: "submitting".into(),
                message_id: None,
                error: None,
            });
        }
        self.error = None;
        Ok(())
    }
    fn update_receipt(&mut self, receipt: Receipt) {
        if let Some(message) = self.outgoing.iter_mut().find(|message| {
            message.request_id == receipt.request_id && message.recipient_id == receipt.recipient_id
        }) {
            message.message_id = Some(receipt.message_id);
            message.error = None;
            message.status = match receipt.status {
                crate::wire::messages::Status::Queued => "queued",
                crate::wire::messages::Status::Included => "included",
                crate::wire::messages::Status::Acknowledged => "acknowledged",
                crate::wire::messages::Status::Expired => {
                    message.error =
                        Some("连接、成员或权限已变化；消息没有重新路由给其他收件人".into());
                    "failed"
                }
            }
            .into();
        }
    }
    fn failure(&mut self, command: &Command, error: &str) {
        self.error = Some(error.into());
        if let Command::Send { request_id, .. } = command
            && let Some(message) = self
                .outgoing
                .iter_mut()
                .find(|message| message.request_id == *request_id)
        {
            message.status = "failed".into();
            message.error = Some(error.into());
        }
    }
    fn reply(&mut self, id: u64, result: Result<Reply, String>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        let result = result.and_then(|result| {
            result.validate()?;
            Ok(result)
        });
        match result {
            Ok(result) => {
                if let Command::Send {
                    request_id,
                    recipient_id,
                    ..
                } = &pending.command
                    && !result.receipts.iter().any(|receipt| {
                        receipt.request_id == *request_id && receipt.recipient_id == *recipient_id
                    })
                {
                    self.failure(
                        &pending.command,
                        "服务器回执未匹配发送请求；发送结果尚未确认",
                    );
                    return;
                }
                if matches!(pending.command, Command::List {}) {
                    self.recipients = result.recipients;
                }
                for receipt in result.receipts {
                    self.update_receipt(receipt);
                }
                self.error = None;
            }
            Err(error) => self.failure(&pending.command, &error),
        }
    }
    fn poll(&mut self) {
        let now = Instant::now();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(pending) = self.pending.remove(&id) {
                self.failure(
                    &pending.command,
                    "原生消息回执超时，结果未确认；刷新回执或使用原请求编号重试",
                );
            }
        }
        if self.available() && self.next_poll.is_some_and(|deadline| deadline <= now) {
            self.next_poll = Some(now + POLL);
            if self
                .outgoing
                .iter()
                .any(|m| matches!(m.status.as_str(), "submitting" | "queued" | "included"))
            {
                let _ = self.refresh();
            }
        }
    }
    fn disconnect(&mut self) {
        self.connected = false;
        self.supported = false;
        self.sender = None;
        self.pending.clear();
        self.recipients.clear();
        self.next_poll = None;
        for message in &mut self.outgoing {
            if matches!(message.status.as_str(), "submitting" | "queued") {
                message.status = "failed".into();
                message.error =
                    Some("原连接已断开；请保留正文并重新选择明确收件人，未自动重发".into());
            }
        }
    }
}
fn state() -> std::sync::MutexGuard<'static, Manager> {
    STATE
        .get_or_init(|| Mutex::new(Manager::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
pub(crate) fn available() -> bool {
    state().available()
}
pub(crate) fn snapshot() -> Value {
    state().snapshot()
}
pub(crate) fn refresh() -> Result<(), String> {
    state().refresh()
}
pub(crate) fn send(recipient_id: &str, text: &str, request_id: &str) -> Result<(), String> {
    state().send(recipient_id, text, request_id)
}
pub(super) fn connect(supported: bool) -> mpsc::Receiver<Request> {
    state().connect(supported)
}
pub(super) fn pending(id: u64) -> bool {
    let mut state = state();
    state.poll();
    state.pending.contains_key(&id)
}
pub(super) fn reply(id: u64, result: Result<Reply, String>) {
    state().reply(id, result);
}
pub(super) fn poll() {
    state().poll();
}
pub(super) fn disconnect() {
    state().disconnect();
}
