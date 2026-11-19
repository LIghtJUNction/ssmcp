//! Local presentation and cancellation for authenticated server-owned waits.
//! This module never reads, reserves, or delivers the ordinary user queue.
use crate::client::notification::{Notifier, Report};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const MAX_ACTIVE: usize = 4;
const MAX_COMPLETED: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Intent {
    NeedUser,
    Finish,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Ready,
    TimedOut,
    Cancelled,
    Disconnected,
    AuthorizationRevoked,
    BindingInvalid,
    Superseded,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Entry {
    pub wait_id: String,
    pub episode_id: String,
    pub intent: Intent,
    pub recipient: String,
    pub reason: Option<String>,
    pub remaining_ms: u64,
    pub notification_requested: bool,
    pub notification: Option<Report>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Snapshot {
    pub connected: bool,
    pub connection_id: Option<String>,
    pub active: Vec<Entry>,
    pub recent: Vec<Completion>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Completion {
    pub wait_id: String,
    pub status: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification: Option<Report>,
}

#[derive(Debug, Serialize)]
struct Finish {
    wait_id: String,
    finished: bool,
    status: Option<Outcome>,
}

struct Spec {
    wait_id: String,
    episode_id: String,
    intent: Intent,
    recipient: String,
    reason: Option<String>,
    timeout_ms: u64,
    notify: bool,
}

impl Spec {
    fn validate(mut self) -> Result<Self, String> {
        if !valid_id(&self.wait_id) {
            return Err("wait_id must contain 1–128 visible ASCII bytes without spaces".into());
        }
        if self.episode_id.len() != 64 || !self.episode_id.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err("episode_id must be a 64-character hexadecimal identifier".into());
        }
        if !valid_id(&self.recipient) {
            return Err("recipient must be next or an exact visible ASCII agent identifier".into());
        }
        if !(1000..=25000).contains(&self.timeout_ms) {
            return Err("user wait timeout_ms must be between 1000 and 25000".into());
        }
        if self.reason.as_ref().is_some_and(|reason| {
            reason.len() > 512
                || reason
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        }) {
            return Err(
                "user wait reason must be at most 512 UTF-8 bytes with only newline/tab controls"
                    .into(),
            );
        }
        self.reason = self.reason.map(|reason| {
            crate::audit::event(json!(reason))
                .as_str()
                .unwrap_or("[redacted]")
                .to_owned()
        });
        Ok(self)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|byte| byte.is_ascii_graphic())
}

#[derive(Clone)]
struct Key {
    id: String,
    generation: u64,
}

struct Active {
    key: Key,
    entry: Entry,
    deadline: Instant,
    reply: watch::Sender<Option<Completion>>,
}

#[derive(Default)]
struct State {
    connection_id: Option<String>,
    generation: u64,
    active: BTreeMap<String, Active>,
    completed: VecDeque<Completion>,
}

impl State {
    fn finish(&mut self, key: &Key, outcome: Outcome) -> bool {
        if self
            .active
            .get(&key.id)
            .is_none_or(|value| value.key.generation != key.generation)
        {
            return false;
        }
        let pending = self.active.remove(&key.id).expect("matched wait");
        let result = Completion {
            wait_id: pending.key.id,
            status: outcome,
            notification: pending.entry.notification,
        };
        let _ = pending.reply.send_replace(Some(result.clone()));
        self.completed.push_back(result);
        while self.completed.len() > MAX_COMPLETED {
            self.completed.pop_front();
        }
        true
    }

    fn expire(&mut self, now: Instant) -> bool {
        let expired = self
            .active
            .values()
            .filter(|value| value.deadline <= now)
            .map(|value| value.key.clone())
            .collect::<Vec<_>>();
        for key in &expired {
            self.finish(key, Outcome::TimedOut);
        }
        !expired.is_empty()
    }
}

#[derive(Default)]
struct Hub {
    state: Mutex<State>,
    // Reused across all waits and timeout retries; no per-call notifier/thread.
    notifier: Notifier,
}

fn global() -> &'static Arc<Hub> {
    static HUB: OnceLock<Arc<Hub>> = OnceLock::new();
    HUB.get_or_init(|| Arc::new(Hub::default()))
}

fn changed() {
    crate::client::terminal::invalidate();
}

#[derive(Debug)]
enum BeginError {
    Disconnected,
    Busy,
}

type Started = (Registration, watch::Receiver<Option<Completion>>, Instant);

impl Hub {
    fn connect(&self, connection_id: Option<&str>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let pending = state
            .active
            .values()
            .map(|value| value.key.clone())
            .collect::<Vec<_>>();
        for key in pending {
            state.finish(&key, Outcome::Disconnected);
        }
        // Receivers retain their disconnect result, but the next connection
        // cannot use old acknowledgements as its own completed-wait ledger.
        state.completed.clear();
        state.connection_id = connection_id.map(str::to_owned);
        drop(state);
        changed();
    }

    fn begin(self: &Arc<Self>, spec: &Spec) -> Result<Started, BeginError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        let expired = state.expire(now);
        let result = if state.connection_id.is_none() {
            Err(BeginError::Disconnected)
        } else if state.active.contains_key(&spec.wait_id)
            || state
                .completed
                .iter()
                .any(|value| value.wait_id == spec.wait_id)
            || state.active.len() == MAX_ACTIVE
        {
            Err(BeginError::Busy)
        } else {
            state.generation = state.generation.wrapping_add(1);
            let key = Key {
                id: spec.wait_id.clone(),
                generation: state.generation,
            };
            let deadline = now + Duration::from_millis(spec.timeout_ms);
            let (reply, receiver) = watch::channel(None);
            state.active.insert(
                key.id.clone(),
                Active {
                    key: key.clone(),
                    deadline,
                    reply,
                    entry: Entry {
                        wait_id: key.id.clone(),
                        episode_id: spec.episode_id.clone(),
                        intent: spec.intent,
                        recipient: spec.recipient.clone(),
                        reason: spec.reason.clone(),
                        remaining_ms: spec.timeout_ms,
                        notification_requested: spec.notify,
                        notification: None,
                    },
                },
            );
            Ok((
                Registration {
                    hub: self.clone(),
                    key,
                },
                receiver,
                deadline,
            ))
        };
        drop(state);
        if expired || result.is_ok() {
            changed();
        }
        result
    }

    fn finish_key(&self, key: &Key, outcome: Outcome) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let expired = state.expire(Instant::now());
        let finished = state.finish(key, outcome);
        drop(state);
        if expired || finished {
            changed();
        }
        finished
    }

    fn finish(&self, id: &str, outcome: Outcome) -> Result<Finish, String> {
        if !valid_id(id) {
            return Err("invalid wait_id".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let expired = state.expire(Instant::now());
        let key = state.active.get(id).map(|value| value.key.clone());
        let finished = key.is_some_and(|key| state.finish(&key, outcome));
        let status = state
            .completed
            .iter()
            .rev()
            .find(|value| value.wait_id == id)
            .map(|value| value.status);
        drop(state);
        if expired || finished {
            changed();
        }
        Ok(Finish {
            wait_id: id.into(),
            finished,
            status,
        })
    }

    fn notification(&self, key: &Key, report: Report) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(active) = state.active.get_mut(&key.id)
            && active.key.generation == key.generation
        {
            active.entry.notification = Some(report);
            drop(state);
            changed();
        }
    }

    fn snapshot(&self) -> Snapshot {
        // Called while the terminal frame lock may be held: observation must
        // never invalidate or re-enter that lock. Each wait owns its deadline.
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let now = Instant::now();
        Snapshot {
            connected: state.connection_id.is_some(),
            connection_id: state.connection_id.clone(),
            active: state
                .active
                .values()
                .map(|active| {
                    let mut entry = active.entry.clone();
                    entry.remaining_ms =
                        active.deadline.saturating_duration_since(now).as_millis() as u64;
                    entry
                })
                .collect(),
            recent: state.completed.iter().cloned().collect(),
        }
    }

    fn cancel(&self, prefix: &str) -> Result<String, String> {
        if !valid_id(prefix) {
            return Err("用法：/wait cancel <等待编号或唯一前缀>".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.expire(Instant::now());
        let key = if let Some(active) = state.active.get(prefix) {
            active.key.clone()
        } else {
            let matching = state
                .active
                .values()
                .filter(|active| active.key.id.starts_with(prefix))
                .collect::<Vec<_>>();
            match matching.as_slice() {
                [active] => active.key.clone(),
                [] => return Err("没有匹配的活动等待；未取消任何请求".into()),
                _ => return Err("等待编号前缀不唯一；未取消任何请求，请使用完整编号".into()),
            }
        };
        state.finish(&key, Outcome::Cancelled);
        // This local command runs under the TUI lock; its caller marks dirty.
        // Sending the watch result wakes the server-owned native wait future.
        Ok(key.id)
    }

    async fn wait(self: &Arc<Self>, spec: Spec) -> Result<Value, String> {
        let spec = spec.validate()?;
        let (registration, mut receiver, deadline) = match self.begin(&spec) {
            Ok(started) => started,
            Err(BeginError::Busy) => return Ok(json!({"wait_id":spec.wait_id,"status":"busy"})),
            Err(BeginError::Disconnected) => {
                return Ok(json!({"wait_id":spec.wait_id,"status":"disconnected"}));
            }
        };
        let title = match spec.intent {
            Intent::NeedUser => "ssmcp 正在等待你回复",
            Intent::Finish => "ssmcp 正在等待你确认是否结束",
        };
        let recipient = if spec.recipient == "next" {
            "Next caller"
        } else {
            &spec.recipient
        };
        let body = format!(
            "接收目标：{recipient}\n{}",
            spec.reason
                .as_deref()
                .unwrap_or("请返回当前 ssmcp 会话输入你的下一条消息。")
        );
        let notify = self.notifier.notify_wait(&spec.episode_id, title, &body);
        tokio::pin!(notify);
        let mut notified = !spec.notify;
        loop {
            let complete = receiver.borrow().clone();
            if let Some(result) = complete {
                return serde_json::to_value(result)
                    .map_err(|_| "cannot encode user wait result".into());
            }
            tokio::select! {
                biased;
                result = receiver.changed() => {
                    if result.is_err() {
                        return Err("user wait channel ended before a local result".into());
                    }
                },
                _ = tokio::time::sleep_until(deadline.into()) => {
                    self.finish_key(&registration.key, Outcome::TimedOut);
                },
                report = &mut notify, if !notified => {
                    notified = true;
                    self.notification(&registration.key, report);
                },
            }
        }
    }
}

struct Registration {
    hub: Arc<Hub>,
    key: Key,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.hub.finish_key(&self.key, Outcome::Cancelled);
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Wait {
        wait_id: String,
        episode_id: String,
        intent: Intent,
        recipient: String,
        reason: Option<String>,
        timeout_ms: u64,
        #[serde(default = "yes")]
        notify: bool,
    },
    Finish {
        wait_id: String,
        outcome: Outcome,
    },
    Status {},
}
fn yes() -> bool {
    true
}

pub(crate) fn connect(connection_id: &str) {
    global().connect(Some(connection_id));
}
pub(crate) fn disconnect() {
    global().connect(None);
}
pub(crate) fn snapshot() -> Snapshot {
    global().snapshot()
}
pub(crate) fn cancel(prefix: &str) -> Result<String, String> {
    global().cancel(prefix)
}

pub(crate) async fn call(arguments: Value) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "user wait requires a valid wait/finish/status request")?;
    match request {
        Request::Wait {
            wait_id,
            episode_id,
            intent,
            recipient,
            reason,
            timeout_ms,
            notify,
        } => {
            global()
                .wait(Spec {
                    wait_id,
                    episode_id,
                    intent,
                    recipient,
                    reason,
                    timeout_ms,
                    notify,
                })
                .await
        }
        Request::Finish { wait_id, outcome } => {
            serde_json::to_value(global().finish(&wait_id, outcome)?)
                .map_err(|_| "cannot encode user wait completion".into())
        }
        Request::Status {} => Ok(json!({"schema_version":1,"state":snapshot()})),
    }
}

pub(crate) fn tool() -> Value {
    json!({"name":"ssmcp.ui.wait","description":"Client presentation and local cancellation for the authenticated server ui.wait helper. wait blocks without reading user text or claiming any queue; finish only wakes the exact wait. Native ready means a message is reserved for possible outer delivery, never received. Only the final server HTTP receipt with its user_messages can establish delivery. At most four active waits; connection loss and local deadlines clear them. status is observational. This is independent of the force-send listener and invokes no model.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["op"],"properties":{
            "op":{"enum":["wait","finish","status"]},"wait_id":{"type":"string","minLength":1,"maxLength":128},
            "episode_id":{"type":"string","pattern":"^[A-Fa-f0-9]{64}$"},"intent":{"enum":["need_user","finish"]},
            "recipient":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[!-~]+$"},
            "reason":{"anyOf":[{"type":"string","maxLength":512},{"type":"null"}]},
            "timeout_ms":{"type":"integer","minimum":1000,"maximum":25000},
            "notify":{"type":"boolean","default":true},"outcome":{"enum":["ready","timed_out","cancelled","disconnected","authorization_revoked","binding_invalid","superseded"]}}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}
