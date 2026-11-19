//! Explicit receipt-based delivery to one program on the current connection.
//! Ordinary code_mode replies and collaboration presence never acquire a lease.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, watch};

const LEASE: Duration = Duration::from_secs(30);
const ACK_WAIT: Duration = Duration::from_secs(5);
const MAX_RECEIPTS: usize = 32;
const NO_LISTENER: &str = "发送失败：没有程序在监听";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Accepted,
    Rejected,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Receipt {
    pub delivery_id: String,
    pub connection_id: String,
    pub listener_id: String,
    pub status: Status,
    pub code: String,
    pub detail: String,
}

#[derive(Clone)]
pub(crate) struct Target {
    connection_id: String,
    epoch: u64,
    listener_id: String,
}

struct Connection {
    id: String,
    epoch: u64,
}

// No Debug/Serialize: the credential is returned only by the explicit open call.
struct Lease {
    id: String,
    label: String,
    token: String,
    expires: Instant,
    reading: bool,
}

struct Pending {
    id: String,
    target: Target,
    source: String,
    text: String,
    created_at_ms: u64,
    deadline: Instant,
    offered: bool,
    reply: watch::Sender<Option<Receipt>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AckStatus {
    Accepted,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ack {
    status: AckStatus,
    reason: Option<String>,
}

struct Remembered {
    epoch: u64,
    receipt: Receipt,
    ack: Option<Ack>,
}

#[derive(Default)]
struct State {
    connection: Option<Connection>,
    epoch: u64,
    lease: Option<Lease>,
    pending: Option<Pending>,
    receipts: VecDeque<Remembered>,
}

impl State {
    fn finish(&mut self, status: Status, code: &str, detail: String, ack: Option<Ack>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let receipt = Receipt {
            delivery_id: pending.id,
            connection_id: pending.target.connection_id,
            listener_id: pending.target.listener_id,
            status,
            code: code.into(),
            detail,
        };
        let _ = pending.reply.send_replace(Some(receipt.clone()));
        self.receipts.push_back(Remembered {
            epoch: pending.target.epoch,
            receipt,
            ack,
        });
        while self.receipts.len() > MAX_RECEIPTS {
            self.receipts.pop_front();
        }
    }

    fn lost(&mut self, code: &str, before_offer: &str) {
        let offered = self.pending.as_ref().is_some_and(|value| value.offered);
        self.finish(
            if offered {
                Status::Unknown
            } else {
                Status::Failed
            },
            code,
            if offered {
                "发送结果未确认：消息已交给监听程序，但未收到接收确认；不会自动重发".into()
            } else {
                before_offer.into()
            },
            None,
        );
    }

    fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| lease.expires <= now)
        {
            self.lease = None;
            self.lost("listener_expired", "发送失败：监听程序已离线，消息未投递");
            changed = true;
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|value| value.deadline <= now)
        {
            self.lost("ack_timeout", "发送失败：监听程序没有及时领取消息");
            changed = true;
        }
        changed
    }

    fn authenticate(&self, id: &str, token: &str) -> Result<(), String> {
        let lease = self.lease.as_ref().ok_or("listener lease has expired")?;
        if lease.id != id || !same_token(&lease.token, token) {
            return Err("listener credential is invalid for this connection".into());
        }
        Ok(())
    }
}

#[derive(Default)]
struct Hub {
    state: Mutex<State>,
    changed: Notify,
}

fn global() -> &'static Arc<Hub> {
    static HUB: OnceLock<Arc<Hub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(Hub::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn random(bytes: usize) -> Result<String, String> {
    let mut data = vec![0u8; bytes];
    crate::platform::random_bytes(&mut data).map_err(|_| "cannot allocate listener identity")?;
    Ok(data.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn same_token(expected: &str, supplied: &str) -> bool {
    supplied.len() == 64
        && expected.len() == supplied.len()
        && expected
            .bytes()
            .zip(supplied.bytes())
            .fold(0u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

impl Hub {
    fn changed(&self) {
        self.changed.notify_waiters();
        crate::client::terminal::invalidate();
    }

    fn reset(&self, connection_id: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.lost("connection_ended", "发送失败：连接已结束，消息未投递");
        state.lease = None;
        state.epoch = state.epoch.saturating_add(1);
        state.connection = connection_id.map(|id| Connection {
            id: id.into(),
            epoch: state.epoch,
        });
        drop(state);
        self.changed();
    }

    fn open(&self, label: String) -> Result<Value, String> {
        let label = label.trim();
        if label.is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
            return Err(
                "listener label must contain 1–96 UTF-8 bytes without control characters".into(),
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state.expire(now);
        let connection_id = state
            .connection
            .as_ref()
            .ok_or("client connection is not active")?
            .id
            .clone();
        if state.lease.is_some() {
            return Err("listener_busy: this connection already has a listener".into());
        }
        let id = random(16)?;
        let token = random(32)?;
        state.lease = Some(Lease {
            id: id.clone(),
            label: label.into(),
            token: token.clone(),
            expires: now + LEASE,
            reading: false,
        });
        drop(state);
        self.changed();
        // The field is deliberately named token so automatic source/result
        // audit redaction handles it. It never enters snapshots or UI messages.
        Ok(json!({"schema_version":1,"connection_id":connection_id,
            "listener_id":id,"token":token,"expires_in_ms":LEASE.as_millis(),
            "heartbeat_ms":5000,"ack_timeout_ms":ACK_WAIT.as_millis()}))
    }

    fn snapshot(&self) -> Value {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let expired = state.expire(now);
        let value = json!({"schema_version":1,"connected":state.connection.is_some(),
            "connection_id":state.connection.as_ref().map(|value| &value.id),
            "listener":state.lease.as_ref().map(|lease| json!({
                "listener_id":lease.id,"label":lease.label,"reading":lease.reading,
                "expires_in_ms":lease.expires.saturating_duration_since(now).as_millis()})),
            "pending":state.pending.as_ref().map(|value| json!({
                "delivery_id":value.id,"listener_id":value.target.listener_id,
                "status":if value.offered {"offered"} else {"waiting"},
                "expires_in_ms":value.deadline.saturating_duration_since(now).as_millis()})),
            "receipts":state.receipts.iter().map(|value| &value.receipt).collect::<Vec<_>>()});
        drop(state);
        if expired {
            self.changed();
        }
        value
    }

    fn target(&self) -> Result<Target, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.expire(Instant::now());
        let connection = state.connection.as_ref().ok_or(NO_LISTENER)?;
        let lease = state.lease.as_ref().ok_or(NO_LISTENER)?;
        if state.pending.is_some() {
            return Err("发送失败：上一条消息仍在等待监听程序确认，草稿已保留".into());
        }
        Ok(Target {
            connection_id: connection.id.clone(),
            epoch: connection.epoch,
            listener_id: lease.id.clone(),
        })
    }

    fn submit(
        self: &Arc<Self>,
        target: Target,
        source: String,
        text: String,
    ) -> Result<Ticket, String> {
        crate::client::steering::validate_text(&source)?;
        crate::client::steering::validate_text(&text)?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state.expire(now);
        if !state.connection.as_ref().is_some_and(|connection| {
            connection.id == target.connection_id && connection.epoch == target.epoch
        }) || !state
            .lease
            .as_ref()
            .is_some_and(|lease| lease.id == target.listener_id)
        {
            return Err("发送失败：监听程序或连接已改变，草稿已保留".into());
        }
        if state.pending.is_some() {
            return Err("发送失败：上一条消息仍在等待监听程序确认，草稿已保留".into());
        }
        let id = random(16)?;
        let (reply, receiver) = watch::channel(None);
        let deadline = now + ACK_WAIT;
        state.pending = Some(Pending {
            id: id.clone(),
            target,
            source,
            text,
            created_at_ms: now_ms(),
            deadline,
            offered: false,
            reply,
        });
        drop(state);
        self.changed();
        Ok(Ticket {
            hub: self.clone(),
            id,
            receiver,
            deadline,
            completed: false,
        })
    }

    async fn read(
        self: &Arc<Self>,
        id: String,
        token: String,
        wait_ms: u64,
    ) -> Result<Value, String> {
        if wait_ms > 5000 {
            return Err("listener wait_ms must be at most 5000".into());
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();
            state.expire(now);
            state.authenticate(&id, &token)?;
            let lease = state.lease.as_mut().expect("authenticated lease");
            if lease.reading {
                return Err("read_busy: this listener already has a pending read".into());
            }
            lease.reading = true;
            lease.expires = now + LEASE;
        }
        let _reading = Reading {
            hub: self.clone(),
            id: id.clone(),
        };
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let delivery = {
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.expire(Instant::now());
                state.authenticate(&id, &token)?;
                state.pending.as_mut().map(|pending| {
                    pending.offered = true;
                    json!({"delivery_id":pending.id,"connection_id":pending.target.connection_id,
                        "listener_id":pending.target.listener_id,"source":"client_user",
                        "source_text":pending.source,"text":pending.text,"created_at_ms":pending.created_at_ms})
                })
            };
            if let Some(delivery) = delivery {
                return Ok(json!({"schema_version":1,"delivery":delivery,"timed_out":false}));
            }
            if Instant::now() >= deadline {
                return Ok(json!({"schema_version":1,"delivery":null,"timed_out":true}));
            }
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(deadline.into()) => {},
            }
        }
    }

    fn ack(&self, id: &str, token: &str, delivery_id: &str, ack: Ack) -> Result<Value, String> {
        if ack.reason.as_ref().is_some_and(|reason| {
            reason.trim().is_empty() || reason.len() > 512 || reason.chars().any(char::is_control)
        }) {
            return Err(
                "rejection reason must contain 1–512 UTF-8 bytes without control characters".into(),
            );
        }
        if (ack.status == AckStatus::Accepted && ack.reason.is_some())
            || (ack.status == AckStatus::Rejected && ack.reason.is_none())
        {
            return Err("accepted requires no reason; rejected requires a reason".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state.expire(now);
        state.authenticate(id, token)?;
        let epoch = state
            .connection
            .as_ref()
            .ok_or("client connection ended")?
            .epoch;
        if let Some(previous) = state.receipts.iter().find(|value| {
            value.epoch == epoch
                && value.receipt.listener_id == id
                && value.receipt.delivery_id == delivery_id
        }) {
            if previous.ack.as_ref() != Some(&ack) {
                return Err(
                    "ack_conflict: the receipt is final or identifies another outcome".into(),
                );
            }
            return Ok(json!({"schema_version":1,"receipt":previous.receipt}));
        }
        let pending = state
            .pending
            .as_ref()
            .filter(|value| {
                value.id == delivery_id
                    && value.target.listener_id == id
                    && value.target.epoch == epoch
            })
            .ok_or("delivery is unknown or expired")?;
        if !pending.offered {
            return Err("read the delivery before acknowledging it".into());
        }
        let (status, code, detail) = match ack.status {
            AckStatus::Accepted => (Status::Accepted, "accepted", "监听程序已接收".into()),
            AckStatus::Rejected => (
                Status::Rejected,
                "rejected",
                format!(
                    "发送失败：监听程序拒绝接收（{}）",
                    ack.reason.as_deref().expect("validated reason")
                ),
            ),
        };
        state.finish(status, code, detail, Some(ack));
        state.lease.as_mut().expect("authenticated lease").expires = now + LEASE;
        let receipt = state
            .receipts
            .back()
            .expect("completed receipt")
            .receipt
            .clone();
        drop(state);
        self.changed();
        Ok(json!({"schema_version":1,"receipt":receipt}))
    }

    fn close(&self, id: &str, token: &str) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.expire(Instant::now());
        state.authenticate(id, token)?;
        state.lease = None;
        state.lost("listener_closed", "发送失败：监听程序已退出，消息未投递");
        drop(state);
        self.changed();
        Ok(json!({"schema_version":1,"closed":true}))
    }

    fn abandon(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.id == id)
        {
            state.lost("sender_cancelled", "发送已取消：消息未投递");
        }
        drop(state);
        self.changed();
    }
}

struct Reading {
    hub: Arc<Hub>,
    id: String,
}
impl Drop for Reading {
    fn drop(&mut self) {
        let mut state = self
            .hub
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(lease) = &mut state.lease
            && lease.id == self.id
        {
            lease.reading = false;
        }
        drop(state);
        self.hub.changed();
    }
}

pub(crate) struct Ticket {
    hub: Arc<Hub>,
    id: String,
    receiver: watch::Receiver<Option<Receipt>>,
    deadline: Instant,
    completed: bool,
}

impl Ticket {
    pub(crate) async fn wait(mut self) -> Result<Receipt, String> {
        loop {
            if let Some(receipt) = self.receiver.borrow().clone() {
                self.completed = true;
                return Ok(receipt);
            }
            tokio::select! {
                changed = self.receiver.changed() => {
                    if changed.is_err() && self.receiver.borrow().is_none() {
                        return Err("发送结果未确认：接收通道已结束，不会自动重发".into());
                    }
                },
                _ = tokio::time::sleep_until(self.deadline.into()) => {
                    self.hub.state.lock().unwrap_or_else(|error| error.into_inner()).expire(Instant::now());
                    self.hub.changed();
                },
            }
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.completed {
            self.hub.abandon(&self.id);
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Open {
        label: String,
    },
    Read {
        listener_id: String,
        token: String,
        #[serde(default = "wait_default")]
        wait_ms: u64,
    },
    Ack {
        listener_id: String,
        token: String,
        delivery_id: String,
        status: AckStatus,
        reason: Option<String>,
    },
    Close {
        listener_id: String,
        token: String,
    },
    Status {},
}
fn wait_default() -> u64 {
    5000
}

pub(crate) fn connect(connection_id: &str) {
    global().reset(Some(connection_id));
}
pub(crate) fn disconnect() {
    global().reset(None);
}
pub(crate) fn snapshot() -> Value {
    global().snapshot()
}
pub(crate) fn target() -> Result<Target, String> {
    global().target()
}
pub(crate) fn submit(target: Target, source: String, text: String) -> Result<Ticket, String> {
    global().submit(target, source, text)
}

pub(crate) async fn call(arguments: Value) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "listener requires a valid open/read/ack/close/status request")?;
    match request {
        Request::Open { label } => global().open(label),
        Request::Read {
            listener_id,
            token,
            wait_ms,
        } => global().read(listener_id, token, wait_ms).await,
        Request::Ack {
            listener_id,
            token,
            delivery_id,
            status,
            reason,
        } => global().ack(&listener_id, &token, &delivery_id, Ack { status, reason }),
        Request::Close { listener_id, token } => global().close(&listener_id, &token),
        Request::Status {} => Ok(snapshot()),
    }
}

pub(crate) fn tool() -> Value {
    json!({"name":"ssmcp.ui.listener",
        "description":"Explicit single-program listener for forced user messages on this connection. open acquires a 30-second exclusive lease. read renews it and returns the same delivery until ack; only accepted ACK means the program received it, never that a model ran. Poll read within 5 seconds, acknowledge within 5 seconds, and close when done. The token is secret. Expired/disconnected leases cannot be resumed; offered but unacknowledged messages are unknown and never automatically replayed. Ordinary code_mode requests do not register listeners or consume these messages.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["op"],
            "properties":{"op":{"enum":["open","read","ack","close","status"]},
                "label":{"type":"string","minLength":1,"maxLength":96},
                "listener_id":{"type":"string","minLength":32,"maxLength":32},
                "token":{"type":"string","minLength":64,"maxLength":64},
                "delivery_id":{"type":"string","minLength":32,"maxLength":32},
                "wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":5000},
                "status":{"enum":["accepted","rejected"]},
                "reason":{"type":"string","minLength":1,"maxLength":512}}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}
