//! Local presentation and cancellation for authenticated server-owned waits.
//! This module never reads, reserves, or delivers the ordinary user queue.
use crate::client::notification::{Notifier, Report};
use super::questions::{self, FinishReceipt, FinishWait};
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
    #[serde(skip_deserializing)]
    Decided,
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
    pub finish_request_id: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_decision: Option<FinishReceipt>,
}

#[derive(Debug, Serialize)]
struct Finish {
    wait_id: String,
    finished: bool,
    status: Option<Outcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notification: Option<Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    finish_decision: Option<FinishReceipt>,
}

struct Spec {
    wait_id: String,
    episode_id: String,
    intent: Intent,
    recipient: String,
    reason: Option<String>,
    finish_request_id: Option<String>,
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
        if (self.intent == Intent::NeedUser && self.finish_request_id.is_some())
            || self.finish_request_id.as_deref().is_some_and(|id| !valid_id(id))
            || (self.intent == Intent::Finish && self.recipient == "next")
        {
            return Err("finish waits require an authenticated thread; only finish accepts a request ID".into());
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
    finish_decision: Option<FinishReceipt>,
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
            finish_request_id: pending.entry.finish_request_id,
            finish_decision: pending.finish_decision,
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
                    finish_decision: None,
                    entry: Entry {
                        wait_id: key.id.clone(),
                        episode_id: spec.episode_id.clone(),
                        intent: spec.intent,
                        recipient: spec.recipient.clone(),
                        reason: spec.reason.clone(),
                        finish_request_id: spec.finish_request_id.clone(),
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
        let completed = state
            .completed
            .iter()
            .rev()
            .find(|value| value.wait_id == id);
        let status = completed.map(|value| value.status);
        let notification = completed.and_then(|value| value.notification);
        let finish_request_id = completed.and_then(|value| value.finish_request_id.clone());
        let finish_decision = completed.and_then(|value| value.finish_decision.clone());
        drop(state);
        if expired || finished {
            changed();
        }
        Ok(Finish {
            wait_id: id.into(),
            finished,
            status,
            notification,
            finish_request_id,
            finish_decision,
        })
    }

    fn decision(&self, key: &Key, receipt: FinishReceipt) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(active) = state.active.get_mut(&key.id)
            && active.key.generation == key.generation
        {
            active.entry.finish_request_id = Some(receipt.request_id.clone());
            active.finish_decision = Some(receipt);
        }
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
        // Creation and guard registration are synchronous inside this one native
        // wait. There is no separate preparation RPC whose reply can be lost
        // before cancellation learns which Human request it owns.
        let mut finish = if spec.intent == Intent::Finish {
            let receipt = questions::prepare_finish(
                spec.reason.as_deref(),
                spec.finish_request_id.as_deref(),
                &spec.recipient,
            )?;
            let finish = FinishWait::begin(&receipt.request_id, &spec.recipient)?;
            self.decision(&registration.key, receipt);
            Some(finish)
        } else {
            None
        };
        let title = if finish.is_some() { "ssmcp 等待你决定是否结束" } else { "ssmcp 正在等待你回复" };
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
            if let Some(mut result) = complete {
                if result.status != Outcome::Decided
                    && let Some(finish) = &mut finish
                {
                    result.finish_decision = finish.end(result.status == Outcome::TimedOut).ok();
                }
                return serde_json::to_value(result)
                    .map_err(|_| "cannot encode user wait result".into());
            }
            if Instant::now() >= deadline {
                self.finish_key(&registration.key, Outcome::TimedOut);
                continue;
            }
            if let Some(finish) = &mut finish {
                match finish.poll(deadline) {
                    Ok(Some(receipt)) => {
                        self.decision(&registration.key, receipt);
                        self.finish_key(&registration.key, Outcome::Decided);
                        continue;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        self.finish_key(&registration.key, Outcome::BindingInvalid);
                        continue;
                    }
                }
            }
            let has_finish = finish.is_some();
            tokio::select! {
                biased;
                result = receiver.changed() => {
                    if result.is_err() {
                        return Err("user wait channel ended before a local result".into());
                    }
                },
                result = async {
                    match &mut finish {
                        Some(finish) => finish.changed().await,
                        None => std::future::pending::<Result<(), String>>().await,
                    }
                }, if has_finish => {
                    if result.is_err() {
                        self.finish_key(&registration.key, Outcome::Disconnected);
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
        #[serde(default)]
        finish_request_id: Option<String>,
        timeout_ms: u64,
        #[serde(default = "yes")]
        notify: bool,
    },
    Finish {
        wait_id: String,
        outcome: Outcome,
        #[serde(default)]
        finish_request_id: Option<String>,
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
            finish_request_id,
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
                    finish_request_id,
                    timeout_ms,
                    notify,
                })
                .await
        }
        Request::Finish { wait_id, outcome, finish_request_id } => {
            let mut result = global().finish(&wait_id, outcome)?;
            if let Some(id) = result.finish_request_id.clone().or(finish_request_id) {
                result.finish_request_id = Some(id.clone());
                if outcome != Outcome::TimedOut {
                    result.finish_decision = Some(questions::cancel_finish(&id, "wait_cancelled")?);
                }
            }
            serde_json::to_value(result).map_err(|_| "cannot encode user wait completion".into())
        }
        Request::Status {} => Ok(json!({"schema_version":1,"state":snapshot()})),
    }
}

pub(crate) fn tool() -> Value {
    json!({"name":"ssmcp.ui.wait","description":"Internal client presentation and local cancellation for the authenticated server ui.wait helper. A finish wait synchronously creates or resumes a real verified-thread Human Yes/No request and consumes an exact Human Yes once. Text, reads, timeout and cancellation never grant completion permission. wait blocks without reading user text or claiming any queue; finish only wakes the exact wait. Native ready means a message is reserved for possible outer delivery, never received. Only the final server HTTP receipt with its user_messages can establish delivery. At most four active waits; connection loss and local deadlines clear them. status is observational. This is independent of the force-send listener and invokes no model.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["op"],"properties":{
            "op":{"enum":["wait","finish","status"]},"wait_id":{"type":"string","minLength":1,"maxLength":128},
            "finish_request_id":{"type":["string","null"],"minLength":1,"maxLength":128},
            "episode_id":{"type":"string","pattern":"^[A-Fa-f0-9]{64}$"},"intent":{"enum":["need_user","finish"]},
            "recipient":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[!-~]+$"},
            "reason":{"anyOf":[{"type":"string","maxLength":512},{"type":"null"}]},
            "timeout_ms":{"type":"integer","minimum":1000,"maximum":25000},
            "notify":{"type":"boolean","default":true},"outcome":{"enum":["ready","timed_out","cancelled","disconnected","authorization_revoked","binding_invalid","superseded"]}}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hub() -> Arc<Hub> {
        let hub = Arc::new(Hub::default());
        hub.connect(Some("synthetic-connection"));
        hub
    }

    fn spec(id: &str) -> Spec {
        Spec {
            wait_id: id.into(),
            episode_id: "a".repeat(64),
            intent: Intent::NeedUser,
            recipient: "next".into(),
            reason: None,
            finish_request_id: None,
            timeout_ms: 25_000,
            notify: false,
        }
    }

    fn result(receiver: &watch::Receiver<Option<Completion>>) -> Completion {
        receiver
            .borrow()
            .clone()
            .expect("wait should have a final result")
    }

    #[tokio::test]
    async fn user_wait_capacity_and_busy_do_not_replace_existing_requests() {
        let hub = hub();
        let started = (0..MAX_ACTIVE)
            .map(|index| hub.begin(&spec(&format!("slot-{index}"))).unwrap())
            .collect::<Vec<_>>();
        for id in ["slot-0", "fifth"] {
            let reply = hub.wait(spec(id)).await.unwrap();
            assert_eq!(reply["wait_id"], id);
            assert_eq!(reply["status"], "busy");
        }
        assert_eq!(hub.snapshot().active.len(), MAX_ACTIVE);
        assert!(
            started
                .iter()
                .all(|(_, receiver, _)| receiver.borrow().is_none())
        );
        drop(started);
        assert!(hub.snapshot().active.is_empty());
    }

    #[test]
    fn user_wait_exact_cancel_and_finish_ack_keep_the_first_terminal_result() {
        let hub = hub();
        let (_alpha, alpha, _) = hub.begin(&spec("wait-alpha")).unwrap();
        let (_alpine, alpine, _) = hub.begin(&spec("wait-alpine")).unwrap();
        let (beta_registration, beta, _) = hub.begin(&spec("wait-beta")).unwrap();
        hub.notification(
            &beta_registration.key,
            Report {
                status: crate::client::notification::Status::Sent,
                reason: crate::client::notification::Reason::Submitted,
                deduplicated: true,
                user_seen: None,
            },
        );
        assert!(hub.cancel("wait-al").unwrap_err().contains("不唯一"));
        assert_eq!(hub.snapshot().active.len(), 3);
        assert!(alpha.borrow().is_none() && alpine.borrow().is_none());
        assert_eq!(hub.cancel("wait-b").unwrap(), "wait-beta");
        assert_eq!(result(&beta).status, Outcome::Cancelled);
        let finish = hub.finish("wait-beta", Outcome::Ready).unwrap();
        assert!(!finish.finished);
        assert_eq!(finish.wait_id, "wait-beta");
        assert_eq!(finish.status, Some(Outcome::Cancelled));
        let notification = finish.notification.expect("cached notification report");
        assert!(matches!(
            notification.status,
            crate::client::notification::Status::Sent
        ));
        assert!(notification.deduplicated);
        assert_eq!(notification.user_seen, None);
        assert_eq!(hub.cancel("wait-alpha").unwrap(), "wait-alpha");
        assert_eq!(result(&alpha).status, Outcome::Cancelled);
        assert!(alpine.borrow().is_none());
        let ready = hub.finish("wait-alpine", Outcome::Ready).unwrap();
        assert!(ready.finished);
        assert_eq!(result(&alpine).status, Outcome::Ready);
        assert!(hub.cancel("wait-alpine").is_err());
        let unknown = hub.finish("never-registered", Outcome::Ready).unwrap();
        assert!(!unknown.finished);
        assert_eq!(unknown.status, None);
    }

    #[test]
    fn user_wait_old_drop_cannot_cancel_a_reused_id_after_reconnect() {
        let hub = hub();
        let (old, old_result, _) = hub.begin(&spec("same-id")).unwrap();
        hub.connect(Some("replacement-connection"));
        assert_eq!(result(&old_result).status, Outcome::Disconnected);
        let (replacement, new_result, _) = hub.begin(&spec("same-id")).unwrap();
        assert_ne!(old.key.generation, replacement.key.generation);
        drop(old);
        assert!(new_result.borrow().is_none());
        assert_eq!(hub.snapshot().active.len(), 1);
        drop(replacement);
        assert_eq!(result(&new_result).status, Outcome::Cancelled);
        hub.connect(None);
        assert!(!hub.snapshot().connected);
        assert!(hub.snapshot().recent.is_empty());
    }

    #[test]
    fn user_wait_deadline_prevents_late_ready_and_completed_metadata_is_bounded() {
        let hub = hub();
        let (_registration, receiver, _) = hub.begin(&spec("expired")).unwrap();
        hub.state
            .lock()
            .unwrap()
            .active
            .get_mut("expired")
            .unwrap()
            .deadline = Instant::now() - Duration::from_millis(1);
        let late = hub.finish("expired", Outcome::Ready).unwrap();
        assert!(!late.finished);
        assert_eq!(late.status, Some(Outcome::TimedOut));
        assert_eq!(result(&receiver).status, Outcome::TimedOut);
        assert!(matches!(hub.begin(&spec("expired")), Err(BeginError::Busy)));
        assert_eq!(
            hub.finish("expired", Outcome::Cancelled).unwrap().status,
            Some(Outcome::TimedOut)
        );
        for index in 0..MAX_COMPLETED + 3 {
            let id = format!("completed-{index}");
            let (_registration, receiver, _) = hub.begin(&spec(&id)).unwrap();
            assert!(hub.finish(&id, Outcome::Ready).unwrap().finished);
            assert_eq!(result(&receiver).status, Outcome::Ready);
        }
        assert_eq!(hub.snapshot().recent.len(), MAX_COMPLETED);
        assert_eq!(hub.finish("expired", Outcome::Ready).unwrap().status, None);
        assert_eq!(
            hub.finish("completed-0", Outcome::Ready).unwrap().status,
            None
        );
    }

    #[test]
    fn user_wait_concurrent_ready_and_local_cancel_have_one_winner() {
        let hub = hub();
        let (_registration, receiver, _) = hub.begin(&spec("racing-wait")).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let ready_hub = hub.clone();
        let ready_barrier = barrier.clone();
        let ready = std::thread::spawn(move || {
            ready_barrier.wait();
            ready_hub.finish("racing-wait", Outcome::Ready).unwrap()
        });
        let cancel_hub = hub.clone();
        let cancel_barrier = barrier.clone();
        let cancel = std::thread::spawn(move || {
            cancel_barrier.wait();
            cancel_hub.cancel("racing-wait")
        });
        barrier.wait();
        let ready = ready.join().unwrap();
        let cancel = cancel.join().unwrap();
        let outcome = result(&receiver).status;
        assert!(matches!(outcome, Outcome::Ready | Outcome::Cancelled));
        assert_eq!(ready.finished, outcome == Outcome::Ready);
        assert_eq!(cancel.is_ok(), outcome == Outcome::Cancelled);
        assert_eq!(
            hub.finish("racing-wait", Outcome::Ready).unwrap().status,
            Some(outcome)
        );
        assert_eq!(hub.snapshot().recent.len(), 1);
    }

    #[tokio::test]
    async fn user_wait_cancelled_future_and_local_deadline_clear_the_real_wait() {
        let hub = hub();
        let worker_hub = hub.clone();
        let worker = tokio::spawn(async move { worker_hub.wait(spec("aborted-future")).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while hub.snapshot().active.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        assert!(hub.snapshot().active.is_empty());
        assert_eq!(
            hub.finish("aborted-future", Outcome::Ready).unwrap().status,
            Some(Outcome::Cancelled)
        );
        let mut short = spec("local-deadline");
        short.timeout_ms = 1000;
        let reply = tokio::time::timeout(Duration::from_secs(3), hub.wait(short))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["wait_id"], "local-deadline");
        assert_eq!(reply["status"], "timed_out");
        assert!(reply.get("notification").is_none());
        assert!(hub.snapshot().active.is_empty());
    }

    #[test]
    fn user_wait_request_accepts_null_reason_but_never_native_received_or_authority_fields() {
        let value = json!({"op":"wait","wait_id":"synthetic","episode_id":"a".repeat(64),
            "intent":"finish","recipient":"agent:exact.id","reason":null,"timeout_ms":1000,"notify":false});
        assert!(matches!(
            serde_json::from_value::<Request>(value.clone()).unwrap(),
            Request::Wait {
                reason: None,
                notify: false,
                ..
            }
        ));
        let mut forged = value;
        forged["owner"] = json!("forged-principal");
        assert!(serde_json::from_value::<Request>(forged).is_err());
        assert!(
            serde_json::from_value::<Request>(
                json!({"op":"finish","wait_id":"synthetic","outcome":"received"})
            )
            .is_err()
        );
        let mut valid = spec("validated");
        valid.recipient = "agent:exact.id".into();
        assert!(valid.validate().is_ok());
        let mut invalid = spec("validated");
        invalid.reason = Some("unsafe\u{1b}[2J".into());
        assert!(invalid.validate().is_err());
        let mut invalid = spec("validated");
        invalid.timeout_ms = 999;
        assert!(invalid.validate().is_err());
    }
}
