//! Explicit receipt-based delivery to one program on the current connection.
//! Ordinary code_mode replies and collaboration presence never acquire a lease.
#[cfg(test)]
mod scope_tests;
#[cfg(target_os = "linux")]
mod transport;
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
const MAX_OPEN_KEYS: usize = 128;
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
    pub cursor: u64,
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
    generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Owner {
    Remote {
        provider: String,
        subject: String,
        thread: Option<String>,
    },
    Local {
        uid: u32,
        pid: u32,
        started: u64,
    },
}

impl Owner {
    fn thread(&self) -> Option<&str> {
        match self {
            Self::Remote { thread, .. } => thread.as_deref(),
            Self::Local { .. } => None,
        }
    }
}

// Constructed only by the accepted relay CallScope or kernel Unix peer credentials.
#[derive(Clone)]
struct Caller {
    generation: u64,
    session: Option<String>,
    owner: Owner,
}

impl Caller {
    fn remote(scope: &crate::runtime::call_scope::CallScope) -> Result<Self, String> {
        let attribution = scope.attribution().ok_or(
            "listener_auth_required: this operation needs authenticated controller metadata",
        )?;
        if attribution.agent.is_some() && scope.thread().is_none() {
            return Err(
                "listener_thread_required: relay did not negotiate the bound thread".into(),
            );
        }
        Ok(Self {
            generation: scope.connection(),
            session: scope.thread().map(|thread| thread.session_id().into()),
            owner: Owner::Remote {
                provider: attribution.account.provider.clone(),
                subject: attribution.account.subject.clone(),
                thread: scope.thread().map(|thread| thread.thread_id().into()),
            },
        })
    }

    fn validate(&self, connection: Option<&Connection>) -> Result<(), String> {
        let connection = connection.ok_or("client connection is not active")?;
        if self.generation != connection.generation
            || self
                .session
                .as_deref()
                .is_some_and(|id| id != connection.id)
        {
            return Err("listener_scope_expired: caller belongs to another connection".into());
        }
        Ok(())
    }

    fn owns(&self, owner: &Owner) -> bool {
        // A same-UID local process may be explicitly handed a listener token.
        // Remote callers always retain their authenticated account/thread scope.
        matches!(self.owner, Owner::Local { .. }) || &self.owner == owner
    }

    #[cfg(test)]
    fn fixture() -> Self {
        Self {
            generation: 0,
            session: None,
            owner: Owner::Local {
                uid: 1,
                pid: 1,
                started: 1,
            },
        }
    }
}

struct OpenKey {
    owner: Owner,
    key: String,
    label: String,
    listener_id: String,
}

// No Debug/Serialize: the credential is returned only by the explicit open call.
struct Lease {
    id: String,
    label: String,
    token: String,
    expires: Instant,
    reading: bool,
    owner: Owner,
}

struct Pending {
    id: String,
    cursor: u64,
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
    cursor: u64,
    open_keys: VecDeque<OpenKey>,
    local_endpoint: Option<String>,
}

impl State {
    fn finish(&mut self, status: Status, code: &str, detail: String, ack: Option<Ack>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let receipt = Receipt {
            delivery_id: pending.id,
            cursor: pending.cursor,
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
                "发送结果未确认：已尝试投递，但未收到监听程序的接收确认；不会自动重发".into()
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

    fn authenticate(&self, id: &str, token: &str, caller: &Caller) -> Result<(), String> {
        caller.validate(self.connection.as_ref())?;
        let lease = self.lease.as_ref().ok_or("listener lease has expired")?;
        if lease.id != id || !same_token(&lease.token, token) || !caller.owns(&lease.owner) {
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

    fn reset_for(&self, connection_id: Option<&str>, generation: u64) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.lost("connection_ended", "发送失败：连接已结束，消息未投递");
        state.lease = None;
        state.open_keys.clear();
        state.cursor = 0;
        state.local_endpoint = None;
        state.epoch = state.epoch.saturating_add(1);
        state.connection = connection_id.map(|id| Connection {
            id: id.into(),
            epoch: state.epoch,
            generation,
        });
        drop(state);
        self.changed();
    }

    fn open_for(
        &self,
        label: String,
        caller: &Caller,
        key: Option<String>,
    ) -> Result<Value, String> {
        let label = label.trim();
        if label.is_empty() || label.len() > 96 || label.chars().any(char::is_control) {
            return Err(
                "listener label must contain 1–96 UTF-8 bytes without control characters".into(),
            );
        }
        if key.as_ref().is_some_and(|key| {
            key.is_empty()
                || key.len() > 128
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_-.:".contains(&c))
        }) {
            return Err(
                "idempotency_key must contain 1..128 ASCII letters, digits, _, -, . or :".into(),
            );
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        state.expire(now);
        caller.validate(state.connection.as_ref())?;
        let connection_id = state
            .connection
            .as_ref()
            .ok_or("client connection is not active")?
            .id
            .clone();
        if let Some(key) = key.as_ref()
            && let Some(previous) = state
                .open_keys
                .iter()
                .find(|entry| entry.owner == caller.owner && entry.key == *key)
        {
            if previous.label != label {
                return Err(
                    "idempotency_conflict: listener label differs from the original request".into(),
                );
            }
            let lease = state.lease.as_ref().filter(|lease| lease.id == previous.listener_id)
                .ok_or("idempotency_expired: the original listener is closed or expired; use a new key to explicitly register again")?;
            return Ok(Self::opened(&connection_id, lease));
        }
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
            owner: caller.owner.clone(),
        });
        if let Some(key) = key {
            state.open_keys.push_back(OpenKey {
                owner: caller.owner.clone(),
                key,
                label: label.into(),
                listener_id: id,
            });
            while state.open_keys.len() > MAX_OPEN_KEYS {
                state.open_keys.pop_front();
            }
        }
        let value = Self::opened(&connection_id, state.lease.as_ref().expect("new lease"));
        drop(state);
        self.changed();
        // The field is deliberately named token so automatic source/result
        // audit redaction handles it. It never enters snapshots or UI messages.
        Ok(value)
    }

    fn opened(connection_id: &str, lease: &Lease) -> Value {
        json!({"schema_version":1,"connection_id":connection_id,
            "listener_id":lease.id,"token":lease.token,"thread_id":lease.owner.thread(),
            "expires_in_ms":lease.expires.saturating_duration_since(Instant::now()).as_millis(),
            "lease_ms":LEASE.as_millis(),"heartbeat_ms":5000,"ack_timeout_ms":ACK_WAIT.as_millis(),
            "idempotency_retained_keys":MAX_OPEN_KEYS})
    }

    fn snapshot(&self) -> Value {
        self.snapshot_for(None)
            .expect("local status has no remote scope to validate")
    }

    fn snapshot_for(&self, caller: Option<&Caller>) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(caller) = caller {
            // Validation and serialization must use this same guard: a reconnect
            // between separate locks must not expose a new connection's status.
            caller.validate(state.connection.as_ref())?;
        }
        let now = Instant::now();
        let expired = state.expire(now);
        let value = json!({"schema_version":1,"connected":state.connection.is_some(),
            "connection_id":state.connection.as_ref().map(|value| &value.id),
            "local_endpoint":state.local_endpoint,
            "listener":state.lease.as_ref().map(|lease| json!({
                "listener_id":lease.id,"label":lease.label,"reading":lease.reading,
                "thread_id":lease.owner.thread(),
                "expires_in_ms":lease.expires.saturating_duration_since(now).as_millis()})),
            "pending":state.pending.as_ref().map(|value| json!({
                "delivery_id":value.id,"listener_id":value.target.listener_id,
                "cursor":value.cursor,
                "status":if value.offered {"offered"} else {"waiting"},
                "expires_in_ms":value.deadline.saturating_duration_since(now).as_millis()})),
            "receipts":state.receipts.iter().map(|value| &value.receipt).collect::<Vec<_>>()});
        drop(state);
        if expired {
            self.changed();
        }
        Ok(value)
    }

    fn target_for(&self, selected_thread: Option<&str>) -> Result<Target, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.expire(Instant::now());
        let connection = state.connection.as_ref().ok_or(NO_LISTENER)?;
        let lease = state.lease.as_ref().ok_or(NO_LISTENER)?;
        if lease.owner.thread() != selected_thread.filter(|id| *id != "next") {
            return Err(
                "发送失败：监听程序与当前选择的线程不同；请选择对应线程，草稿已保留".into(),
            );
        }
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
        state.cursor = state
            .cursor
            .checked_add(1)
            .ok_or("listener cursor exhausted")?;
        let cursor = state.cursor;
        let (reply, receiver) = watch::channel(None);
        let deadline = now + ACK_WAIT;
        state.pending = Some(Pending {
            id: id.clone(),
            cursor,
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

    async fn read_for(
        self: &Arc<Self>,
        id: String,
        token: String,
        wait_ms: u64,
        caller: Caller,
    ) -> Result<Value, String> {
        if wait_ms > 5000 {
            return Err("listener wait_ms must be at most 5000".into());
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();
            state.expire(now);
            state.authenticate(&id, &token, &caller)?;
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
                state.authenticate(&id, &token, &caller)?;
                state.pending.as_mut().map(|pending| {
                    pending.offered = true;
                    json!({"delivery_id":pending.id,"connection_id":pending.target.connection_id,
                        "cursor":pending.cursor,
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

    fn ack_for(
        &self,
        id: &str,
        token: &str,
        delivery_id: &str,
        ack: Ack,
        caller: &Caller,
    ) -> Result<Value, String> {
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
        state.authenticate(id, token, caller)?;
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

    fn close_for(&self, id: &str, token: &str, caller: &Caller) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.expire(Instant::now());
        state.authenticate(id, token, caller)?;
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
        #[serde(default)]
        idempotency_key: Option<String>,
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
    let runtime = super::harness::local_runtime().ok();
    let generation = runtime
        .as_ref()
        .map_or(0, |(runtime, _)| runtime.connection_generation());
    global().reset_for(Some(connection_id), generation);
    #[cfg(target_os = "linux")]
    transport::replace(
        global().clone(),
        connection_id,
        generation,
        runtime.map(|(_, handle)| handle),
    );
}
pub(crate) fn disconnect() {
    global().reset_for(None, 0);
    #[cfg(target_os = "linux")]
    transport::stop();
}
pub(crate) fn snapshot() -> Value {
    global().snapshot()
}
pub(crate) fn target(selected_thread: Option<&str>) -> Result<Target, String> {
    global().target_for(selected_thread)
}
pub(crate) fn submit(target: Target, source: String, text: String) -> Result<Ticket, String> {
    global().submit(target, source, text)
}

pub(crate) async fn call(arguments: Value) -> Result<Value, String> {
    let scope = crate::runtime::call_scope::current()
        .ok_or("listener_auth_required: no accepted relay call scope")?;
    let caller = Caller::remote(&scope)?;
    global().call_for(caller, arguments).await
}

impl Hub {
    async fn call_for(self: &Arc<Self>, caller: Caller, arguments: Value) -> Result<Value, String> {
        let request: Request = serde_json::from_value(arguments)
            .map_err(|_| "listener requires a valid open/read/ack/close/status request")?;
        match request {
            Request::Open {
                label,
                idempotency_key,
            } => self.open_for(label, &caller, idempotency_key),
            Request::Read {
                listener_id,
                token,
                wait_ms,
            } => self.read_for(listener_id, token, wait_ms, caller).await,
            Request::Ack {
                listener_id,
                token,
                delivery_id,
                status,
                reason,
            } => self.ack_for(
                &listener_id,
                &token,
                &delivery_id,
                Ack { status, reason },
                &caller,
            ),
            Request::Close { listener_id, token } => self.close_for(&listener_id, &token, &caller),
            Request::Status {} => self.snapshot_for(Some(&caller)),
        }
    }
}

#[cfg(test)]
impl Hub {
    fn reset(&self, id: Option<&str>) {
        self.reset_for(id, 0);
    }
    fn open(&self, label: String) -> Result<Value, String> {
        self.open_for(label, &Caller::fixture(), None)
    }
    fn target(&self) -> Result<Target, String> {
        self.target_for(None)
    }
    async fn read(
        self: &Arc<Self>,
        id: String,
        token: String,
        wait_ms: u64,
    ) -> Result<Value, String> {
        self.read_for(id, token, wait_ms, Caller::fixture()).await
    }
    fn ack(&self, id: &str, token: &str, delivery_id: &str, ack: Ack) -> Result<Value, String> {
        self.ack_for(id, token, delivery_id, ack, &Caller::fixture())
    }
    fn close(&self, id: &str, token: &str) -> Result<Value, String> {
        self.close_for(id, token, &Caller::fixture())
    }
}

pub(crate) fn tool() -> Value {
    json!({"name":"ssmcp.ui.listener",
        "description":"Explicit single-program listener for forced user messages on this connection and authenticated thread. open acquires a 30-second exclusive lease for the actual controller account and bound participant (main when unbound). Optional idempotency_key is scoped to that caller; the last 128 keys are retained until disconnect. read renews the lease and returns the same delivery_id/cursor until ack. Only accepted ACK means the program received it, never that a model ran. Poll within 5 seconds and acknowledge within 5 seconds. status exposes a Linux private Unix endpoint using the same state; a same-UID local program needs the secret token to consume a remote lease. Expired/disconnected leases cannot be resumed, and unknown delivery is never automatically replayed. Ordinary code_mode requests do not register listeners or consume these messages.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["op"],
            "properties":{"op":{"enum":["open","read","ack","close","status"]},
                "label":{"type":"string","minLength":1,"maxLength":96},
                 "idempotency_key":{"type":"string","minLength":1,"maxLength":128},
                "listener_id":{"type":"string","minLength":32,"maxLength":32},
                "token":{"type":"string","minLength":64,"maxLength":64},
                "delivery_id":{"type":"string","minLength":32,"maxLength":32},
                "wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":5000},
                "status":{"enum":["accepted","rejected"]},
                "reason":{"type":"string","minLength":1,"maxLength":512}}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn hub() -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        hub.reset(Some("synthetic-connection"));
        hub
    }

    fn credentials(hub: &Hub) -> (String, String) {
        let opened = hub.open("Synthetic receiver".into()).unwrap();
        (
            opened["listener_id"].as_str().unwrap().into(),
            opened["token"].as_str().unwrap().into(),
        )
    }

    fn accepted() -> Ack {
        Ack {
            status: AckStatus::Accepted,
            reason: None,
        }
    }

    fn rejected() -> Ack {
        Ack {
            status: AckStatus::Rejected,
            reason: Some("receiver cannot store this message".into()),
        }
    }

    fn submit(hub: &Arc<Hub>) -> Ticket {
        hub.submit(
            hub.target().unwrap(),
            "@\"中文 空格.txt\"\n    code\n".into(),
            "Expanded synthetic file\n    code\n".into(),
        )
        .unwrap()
    }

    #[test]
    fn force_listener_registration_is_exclusive_and_audit_redacts_the_credential() {
        let hub = hub();
        assert_eq!(hub.target().err().as_deref(), Some(NO_LISTENER));
        let barrier = Arc::new(Barrier::new(3));
        let results = std::thread::scope(|scope| {
            let contenders = (0..2)
                .map(|index| {
                    let hub = hub.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        hub.open(format!("Receiver {index}"))
                    })
                })
                .collect::<Vec<_>>();
            barrier.wait();
            contenders
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(results.iter().filter(|value| value.is_ok()).count(), 1);
        assert!(
            results
                .iter()
                .filter_map(|value| value.as_ref().err())
                .all(|error| error.starts_with("listener_busy"))
        );
        let opened = results.into_iter().find_map(Result::ok).unwrap();
        let token = opened["token"].as_str().unwrap();
        assert_eq!(token.len(), 64);
        assert!(!hub.snapshot().to_string().contains(token));
        let automatic = crate::audit::event(json!({
            "type":"response",
            "arguments":{"op":"read","listener_id":opened["listener_id"],"token":token},
            "result":crate::backend::result(opened.clone()),
        }));
        assert!(
            !automatic.to_string().contains(token),
            "automatic request/result audit must redact the lease credential"
        );
        let id = opened["listener_id"].as_str().unwrap();
        assert!(hub.close(id, &"0".repeat(64)).is_err());
        assert!(hub.snapshot()["listener"].is_object());
        hub.close(id, token).unwrap();
    }

    #[tokio::test]
    async fn force_listener_repeated_reads_and_ack_keep_one_delivery_identity() {
        let hub = hub();
        let (id, token) = credentials(&hub);
        let ticket = submit(&hub);
        let delivery_id = ticket.id.clone();
        assert!(hub.target().is_err());
        assert!(hub.ack(&id, &token, &delivery_id, accepted()).is_err());
        assert!(hub.read(id.clone(), "0".repeat(64), 0).await.is_err());
        assert_eq!(hub.snapshot()["pending"]["status"], "waiting");
        let first = hub.read(id.clone(), token.clone(), 0).await.unwrap();
        let repeated = hub.read(id.clone(), token.clone(), 0).await.unwrap();
        assert_eq!(first, repeated);
        assert_eq!(first["delivery"]["delivery_id"], delivery_id);
        assert_eq!(
            first["delivery"]["source_text"],
            "@\"中文 空格.txt\"\n    code\n"
        );
        assert_eq!(
            first["delivery"]["text"],
            "Expanded synthetic file\n    code\n"
        );
        let receipt = hub.ack(&id, &token, &delivery_id, accepted()).unwrap();
        assert_eq!(
            hub.ack(&id, &token, &delivery_id, accepted()).unwrap(),
            receipt
        );
        assert!(
            hub.ack(&id, &token, &delivery_id, rejected())
                .unwrap_err()
                .starts_with("ack_conflict")
        );
        assert_eq!(ticket.wait().await.unwrap().status, Status::Accepted);
        assert_eq!(hub.snapshot()["receipts"].as_array().unwrap().len(), 1);
        assert!(hub.read(id, token, 0).await.unwrap()["delivery"].is_null());
    }

    #[tokio::test]
    async fn force_listener_cancelled_read_releases_the_exclusive_waiter() {
        let hub = hub();
        let (id, token) = credentials(&hub);
        let waiting = {
            let hub = hub.clone();
            let id = id.clone();
            let token = token.clone();
            tokio::spawn(async move { hub.read(id, token, 5000).await })
        };
        tokio::time::timeout(Duration::from_secs(1), async {
            while !hub.state.lock().unwrap().lease.as_ref().unwrap().reading {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            hub.read(id.clone(), token.clone(), 0)
                .await
                .unwrap_err()
                .starts_with("read_busy")
        );
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(!hub.state.lock().unwrap().lease.as_ref().unwrap().reading);
        let available = hub.read(id.clone(), token.clone(), 0).await.unwrap();
        assert_eq!(available["timed_out"], true);
        hub.close(&id, &token).unwrap();
    }

    #[tokio::test]
    async fn force_listener_reconnect_and_expiry_reject_old_tokens_and_targets() {
        let hub = hub();
        let (old_id, old_token) = credentials(&hub);
        let stale = hub.target().unwrap();
        let ticket = submit(&hub);
        let delivery_id = ticket.id.clone();
        hub.reset(Some("synthetic-next-connection"));
        assert_eq!(ticket.wait().await.unwrap().status, Status::Failed);
        let (id, token) = credentials(&hub);
        assert!(
            hub.submit(stale, "old source".into(), "old text".into())
                .is_err()
        );
        assert!(
            hub.read(old_id.clone(), old_token.clone(), 0)
                .await
                .is_err()
        );
        assert!(
            hub.ack(&old_id, &old_token, &delivery_id, accepted())
                .is_err()
        );
        assert!(hub.close(&old_id, &old_token).is_err());
        assert_eq!(hub.snapshot()["listener"]["listener_id"], id);
        let offered = submit(&hub);
        hub.read(id.clone(), token.clone(), 0).await.unwrap();
        hub.reset(None);
        assert_eq!(offered.wait().await.unwrap().status, Status::Unknown);
        assert_eq!(hub.target().err().as_deref(), Some(NO_LISTENER));
        hub.reset(Some("synthetic-next-connection"));
        let (expired_id, expired_token) = credentials(&hub);
        hub.state.lock().unwrap().lease.as_mut().unwrap().expires =
            Instant::now() - Duration::from_millis(1);
        assert_eq!(hub.target().err().as_deref(), Some(NO_LISTENER));
        assert!(hub.read(expired_id, expired_token, 0).await.is_err());
    }

    #[tokio::test]
    async fn force_listener_unknown_failure_and_rejection_never_replay_automatically() {
        let hub = hub();
        let (id, token) = credentials(&hub);
        let offered = submit(&hub);
        let offered_id = offered.id.clone();
        hub.read(id.clone(), token.clone(), 0).await.unwrap();
        hub.state.lock().unwrap().pending.as_mut().unwrap().deadline =
            Instant::now() - Duration::from_millis(1);
        assert!(hub.snapshot()["pending"].is_null());
        assert_eq!(offered.wait().await.unwrap().status, Status::Unknown);
        assert!(hub.ack(&id, &token, &offered_id, accepted()).is_err());
        assert!(hub.read(id.clone(), token.clone(), 0).await.unwrap()["delivery"].is_null());

        let unread = submit(&hub);
        assert_ne!(unread.id, offered_id);
        hub.state.lock().unwrap().pending.as_mut().unwrap().deadline =
            Instant::now() - Duration::from_millis(1);
        hub.snapshot();
        assert_eq!(unread.wait().await.unwrap().status, Status::Failed);

        let refused = submit(&hub);
        hub.read(id.clone(), token.clone(), 0).await.unwrap();
        hub.ack(&id, &token, &refused.id, rejected()).unwrap();
        let receipt = refused.wait().await.unwrap();
        assert_eq!(receipt.status, Status::Rejected);
        assert!(
            receipt
                .detail
                .contains("receiver cannot store this message")
        );

        let closed = submit(&hub);
        hub.read(id.clone(), token.clone(), 0).await.unwrap();
        hub.close(&id, &token).unwrap();
        assert_eq!(closed.wait().await.unwrap().status, Status::Unknown);
        assert!(hub.snapshot()["pending"].is_null());
        assert_eq!(hub.snapshot()["receipts"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn force_listener_ack_racing_timeout_commits_one_terminal_receipt() {
        let hub = hub();
        let (id, token) = credentials(&hub);
        let ticket = submit(&hub);
        let delivery_id = ticket.id.clone();
        hub.read(id.clone(), token.clone(), 0).await.unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let ack_result = std::thread::scope(|scope| {
            let ack_hub = hub.clone();
            let ack_barrier = barrier.clone();
            let ack = scope.spawn(move || {
                ack_barrier.wait();
                ack_hub.ack(&id, &token, &delivery_id, accepted())
            });
            let expiry_hub = hub.clone();
            let expiry_barrier = barrier.clone();
            let expiry = scope.spawn(move || {
                expiry_barrier.wait();
                expiry_hub
                    .state
                    .lock()
                    .unwrap()
                    .expire(Instant::now() + ACK_WAIT);
            });
            barrier.wait();
            let result = ack.join().unwrap();
            expiry.join().unwrap();
            result
        });
        let receipt = ticket.wait().await.unwrap();
        assert!(matches!(receipt.status, Status::Accepted | Status::Unknown));
        assert_eq!(ack_result.is_ok(), receipt.status == Status::Accepted);
        let snapshot = hub.snapshot();
        assert_eq!(snapshot["receipts"].as_array().unwrap().len(), 1);
        assert!(snapshot["pending"].is_null());
    }
}
