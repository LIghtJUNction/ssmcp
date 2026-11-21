//! Nonblocking terminal sender. Only broker receipts mean queued or delivered.
mod groups;
#[cfg(test)]
pub(super) mod test_connection;
#[cfg(test)]
mod tests;
use crate::wire::messages::{Command, Receipt, Recipient, Reply, Status};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot, oneshot::error::TryRecvError};

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_secs(3);
const DIRECTORY_TIMEOUT: &str = "Native recipient directory refresh timed out";
const UNSUPPORTED: &str = "当前连接不支持原生消息；请升级并连接已认证的 SSMCP 客户端和服务器";
static STATE: OnceLock<Mutex<Manager>> = OnceLock::new();

pub(super) struct Request {
    pub id: u64,
    pub command: Command,
}
type DirectoryResult = Result<Vec<Recipient>, String>;

struct Pending {
    command: Command,
    deadline: Instant,
    quiet: bool,
    directory_waiter: Option<oneshot::Sender<DirectoryResult>>,
}

pub(crate) struct DirectoryLookup {
    connection: u64,
    completion: oneshot::Receiver<DirectoryResult>,
}
#[derive(Clone, Serialize)]
struct Outgoing {
    request_id: String,
    recipient_id: String,
    text: String,
    status: String,
    message_id: Option<String>,
    error: Option<String>,
    connection_closed: bool,
    #[serde(skip)]
    confirmed_status: Option<Status>,
    #[serde(skip)]
    receipt_updated_at_ms: u64,
}
#[derive(Default)]
struct Manager {
    connected: bool,
    supported: bool,
    recipients: Vec<Recipient>,
    outgoing: Vec<Outgoing>,
    groups: Vec<groups::Group>,
    group_connection: u64,
    error: Option<String>,
    sender: Option<mpsc::Sender<Request>>,
    pending: BTreeMap<u64, Pending>,
    next_id: u64,
    next_poll: Option<Instant>,
    revision: u64,
    watch_recipients: bool,
}
impl Manager {
    fn changed(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
    fn available(&self) -> bool {
        self.connected && self.supported
    }
    fn snapshot(&self) -> Value {
        json!({
            "available":self.available(), "connected":self.connected,
            "recipients":self.recipients, "outgoing":self.outgoing, "groups":self.group_snapshots(),
            "error":self.error, "pending":self.pending.values().any(|pending|!pending.quiet), "revision":self.revision,
        })
    }
    fn connect(&mut self, supported: bool) -> mpsc::Receiver<Request> {
        self.disconnect();
        self.group_connection = self.group_connection.saturating_add(1);
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
        self.request_with_visibility(command, false).map(|_| ())
    }
    fn request_with_visibility(&mut self, command: Command, quiet: bool) -> Result<u64, String> {
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
                quiet,
                directory_waiter: None,
            },
        );
        if !quiet {
            self.changed();
        }
        Ok(id)
    }
    fn refresh(&mut self) -> Result<(), String> {
        self.refresh_with_visibility(false).map(|_| ())
    }
    fn refresh_with_visibility(&mut self, quiet: bool) -> Result<u64, String> {
        if let Some((&id, _)) = self
            .pending
            .iter()
            .find(|(_, pending)| matches!(pending.command, Command::List {}))
        {
            return Ok(id);
        }
        self.request_with_visibility(Command::List {}, quiet)
    }

    fn lookup_directory(&mut self) -> Result<DirectoryLookup, String> {
        let id = self.refresh_with_visibility(false)?;
        let (sender, completion) = oneshot::channel();
        // The terminal has one active lookup. A newer query can wait on the
        // same List, including an already queued quiet refresh.
        self.pending
            .get_mut(&id)
            .expect("pending List")
            .directory_waiter = Some(sender);
        Ok(DirectoryLookup {
            connection: self.group_connection,
            completion,
        })
    }

    fn directory_result(
        &self,
        lookup: &mut DirectoryLookup,
    ) -> Option<Result<Vec<Recipient>, String>> {
        if !self.available() || lookup.connection != self.group_connection {
            return Some(Err("Native messaging connection closed".into()));
        }
        match lookup.completion.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Closed) => Some(Err("Native recipient lookup cancelled".into())),
        }
    }
    fn send_to_account(
        &mut self,
        recipient_id: &str,
        expected: &(String, String),
        text: &str,
        request_id: &str,
    ) -> Result<(), String> {
        if self.recipients.iter().any(|recipient| {
            recipient.id == recipient_id
                && (recipient.account.provider != expected.0
                    || recipient.account.subject != expected.1)
        }) {
            return Err(
                "Recipient account changed · select the recipient again; draft retained".into(),
            );
        }
        self.send(recipient_id, text, request_id)
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
        let retry = self
            .outgoing
            .iter()
            .position(|message| message.request_id == request_id);
        if let Some(index) = retry {
            let previous = &self.outgoing[index];
            if previous.recipient_id != recipient_id || previous.text != text {
                return Err("同一发送请求编号不能改为不同正文或收件人".into());
            }
            if previous.status != "failed" {
                return self.refresh();
            }
        }
        // Retrying updates the existing record, including its confirmed identity.
        // A new request may evict history only after transport admission succeeds.
        let evict = if retry.is_none() && self.outgoing.len() >= 128 {
            let Some(index) = self.outgoing.iter().position(|message| {
                message.connection_closed
                    || matches!(
                        message.status.as_str(),
                        "acknowledged" | "expired" | "failed"
                    )
            }) else {
                return Err("发送记录已满，请先等待现有消息回执".into());
            };
            Some(index)
        } else {
            None
        };
        self.request(command)?;
        self.sync_group_receipts();
        if let Some(index) = evict {
            self.outgoing.remove(index);
        }
        if let Some(index) = retry {
            let previous = &mut self.outgoing[index];
            previous.status = "submitting".into();
            previous.error = None;
            previous.connection_closed = false;
        } else {
            self.outgoing.push(Outgoing {
                request_id: request_id.into(),
                recipient_id: recipient_id.into(),
                text: text.into(),
                status: "submitting".into(),
                message_id: None,
                error: None,
                connection_closed: false,
                confirmed_status: None,
                receipt_updated_at_ms: 0,
            });
        }
        self.error = None;
        Ok(())
    }
    fn update_receipt(&mut self, receipt: Receipt) -> bool {
        let Some(message) = self.outgoing.iter_mut().find(|message| {
            message.request_id == receipt.request_id && message.recipient_id == receipt.recipient_id
        }) else {
            return false;
        };
        // List polling and send replies may arrive out of order. Once a
        // message is identified, neither another ID nor an older delivery
        // milestone can replace it. Terminal receipts remain terminal.
        if message
            .message_id
            .as_ref()
            .is_some_and(|id| id != &receipt.message_id)
        {
            if message.status == "submitting" {
                message.status = "failed".into();
                message.error = Some(
                    "服务器回执对应不同消息，重试结果未确认；原消息编号已保留，不会自动重发".into(),
                );
                return true;
            }
            return false;
        }
        if !receipt_progresses(message.confirmed_status, receipt.status)
            || (message.confirmed_status == Some(receipt.status)
                && receipt.updated_at_ms < message.receipt_updated_at_ms)
        {
            return false;
        }
        message.confirmed_status = Some(receipt.status);
        message.receipt_updated_at_ms = message.receipt_updated_at_ms.max(receipt.updated_at_ms);
        let (status, mut error) = match receipt.status {
            Status::Queued => ("queued", None),
            Status::Included => ("included", None),
            Status::Acknowledged => ("acknowledged", None),
            Status::Expired => (
                "expired",
                Some(if receipt.included_at_ms.is_some() {
                    "消息曾包含在收件方工具响应中；连接、成员或权限已变化，后续确认已过期，未自动重发"
                        .to_owned()
                } else {
                    "连接、成员或权限已变化；消息已过期，没有重新路由给其他收件人".to_owned()
                }),
            ),
        };
        if message.connection_closed && matches!(receipt.status, Status::Queued | Status::Included)
        {
            // A delayed observation cannot reopen a closed transport or make
            // an included message look safe to resend.
            error = message.error.clone();
        }
        let changed = message.message_id.as_deref() != Some(receipt.message_id.as_str())
            || message.status != status
            || message.error != error;
        message.message_id = Some(receipt.message_id);
        message.error = error;
        message.status = status.into();
        changed
    }
    fn failure(&mut self, command: &Command, error: &str) {
        self.changed();
        self.error = Some(error.into());
        if let Command::Send { request_id, .. } = command
            && let Some(message) = self
                .outgoing
                .iter_mut()
                .find(|message| message.request_id == *request_id)
        {
            // A transport error only describes this request. It cannot
            // retract delivery already confirmed by another overlapping poll.
            message.status = match message.confirmed_status {
                Some(Status::Queued) => "queued",
                Some(Status::Included) => "included",
                Some(Status::Acknowledged) => "acknowledged",
                Some(Status::Expired) => "expired",
                None => "failed",
            }
            .into();
            if message.confirmed_status != Some(Status::Acknowledged) {
                message.error = Some(error.into());
            }
        }
    }
    fn reply(&mut self, id: u64, result: Result<Reply, String>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };
        if !pending.quiet {
            self.changed();
        }
        let result = if pending
            .directory_waiter
            .as_ref()
            .is_some_and(|sender| !sender.is_closed())
            && pending.deadline <= Instant::now()
        {
            Err(DIRECTORY_TIMEOUT.into())
        } else {
            result.and_then(|result| {
                result.validate()?;
                Ok(result)
            })
        };
        let notify_lookup = pending
            .directory_waiter
            .filter(|sender| !sender.is_closed())
            .is_some_and(|sender| {
                sender
                    .send(
                        result
                            .as_ref()
                            .map(|reply| reply.recipients.clone())
                            .map_err(Clone::clone),
                    )
                    .is_ok()
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
                let mut changed = self.error.is_some();
                if matches!(pending.command, Command::List {}) {
                    changed |= self.recipients != result.recipients;
                    self.recipients = result.recipients;
                }
                for receipt in result.receipts {
                    changed |= self.update_receipt(receipt);
                }
                self.error = None;
                if pending.quiet && (changed || notify_lookup) {
                    self.changed();
                }
            }
            Err(error) => self.failure(&pending.command, &error),
        }
        self.sync_group_receipts();
        self.pump_groups();
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
                if let Some(sender) = pending.directory_waiter {
                    let _ = sender.send(Err(DIRECTORY_TIMEOUT.into()));
                }
                self.failure(
                    &pending.command,
                    "原生消息回执超时，结果未确认；刷新回执或使用原请求编号重试",
                );
            }
        }
        if self.available() && self.next_poll.is_some_and(|deadline| deadline <= now) {
            self.next_poll = Some(now + POLL);
            if self.watch_recipients
                || self.outgoing.iter().any(|m| {
                    !m.connection_closed
                        && matches!(m.status.as_str(), "submitting" | "queued" | "included")
                })
            {
                let _ = self.refresh_with_visibility(true);
            }
        }
        self.pump_groups();
        self.sync_group_receipts();
    }
    fn disconnect(&mut self) {
        self.changed();
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
                message.connection_closed = true;
            } else if message.status == "included" {
                // Included is a fact: the receiving tool response contained
                // the text. Disconnect does not undo it or prove a retry is
                // safe, but this connection can no longer poll for an ACK.
                message.connection_closed = true;
                message.error = Some(
                    "消息已包含在收件方工具响应中；原连接已断开，后续确认不可继续查询，未自动重发"
                        .into(),
                );
            }
        }
        self.close_groups();
    }
}
fn receipt_progresses(previous: Option<Status>, next: Status) -> bool {
    match previous {
        None | Some(Status::Queued) => true,
        Some(Status::Included) => next != Status::Queued,
        Some(Status::Acknowledged) => next == Status::Acknowledged,
        Some(Status::Expired) => next == Status::Expired,
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
/// UI visibility hint, safe while the terminal state mutex is held.
pub(crate) fn watch_recipients(visible: bool) {
    state().watch_recipients = visible;
}
pub(crate) fn refresh() -> Result<(), String> {
    state().refresh()
}
pub(crate) fn lookup_directory() -> Result<DirectoryLookup, String> {
    state().lookup_directory()
}
pub(crate) fn directory_result(
    lookup: &mut DirectoryLookup,
) -> Option<Result<Vec<Recipient>, String>> {
    state().directory_result(lookup)
}
pub(crate) fn send(
    recipient_id: &str,
    expected: &(String, String),
    text: &str,
    request_id: &str,
) -> Result<(), String> {
    state().send_to_account(recipient_id, expected, text, request_id)
}

pub(super) fn chat_receipt(request_id: &str) -> Option<(String, Option<String>)> {
    state()
        .outgoing
        .iter()
        .find(|message| message.request_id == request_id)
        .map(|message| (message.status.clone(), message.error.clone()))
}

pub(crate) const MAX_GROUP_RECIPIENTS: usize = groups::MAX_TARGETS;
pub(crate) fn send_group(
    recipients: &[Recipient],
    text: &str,
    request_id: &str,
) -> Result<(), String> {
    state().group_send(recipients, text, request_id)
}
pub(crate) fn retry_group(request_id: &str) -> Result<(), String> {
    state().group_retry(request_id)
}
// Network/lifecycle entry points run outside the terminal state lock.
// UI send/refresh deliberately leave invalidation to their current input event.
fn update<T>(change: impl FnOnce(&mut Manager) -> T) -> T {
    let (value, changed) = {
        let mut manager = state();
        let revision = manager.revision;
        let value = change(&mut manager);
        (value, revision != manager.revision)
    };
    if changed {
        super::terminal::invalidate();
    }
    value
}
pub(super) fn connect(supported: bool) -> mpsc::Receiver<Request> {
    update(|manager| manager.connect(supported))
}
pub(super) fn pending(id: u64) -> bool {
    update(|manager| {
        manager.poll();
        manager.pending.contains_key(&id)
    })
}
pub(super) fn reply(id: u64, result: Result<Reply, String>) {
    update(|manager| manager.reply(id, result));
}
pub(super) fn poll() {
    update(Manager::poll);
}
pub(super) fn disconnect() {
    update(Manager::disconnect);
}
