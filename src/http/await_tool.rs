//! Typed, bounded observation outside the JavaScript worker pool.
//! Cancelling an observer never cancels its job, acknowledges its inbox, or
//! changes the observed work. Completion approval targets are retired.
use super::*;
use crate::code_mode::user_wait::{Outcome, Recipient};
use crate::wire::MAX_OBSERVATION_WAIT_MS;
use futures_util::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{
        Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    sync::watch,
    time::{Instant, MissedTickBehavior},
};

const MAX_TARGETS: usize = 8;
const MAX_SNAPSHOT_BYTES: usize = 128 * 1024;
const MAX_RECORDS: usize = 256;
const MAX_ACTOR_RECORDS: usize = 32;
const MAX_ACTIVE_PER_MEMBER: usize = 4;
const MAX_ACTIVE_PER_CONNECTION: usize = 32;
const RETAIN_FOR: Duration = Duration::from_secs(300);
mod durable;

pub(crate) fn limits() -> Value {
    json!({"public_tool":"await","code_mode_helper":"waitFor","max_targets":MAX_TARGETS,
        "min_wait_ms":1000,"max_wait_ms":MAX_OBSERVATION_WAIT_MS,"max_records":MAX_RECORDS,
        "max_records_per_actor":MAX_ACTOR_RECORDS,"retention_ms":RETAIN_FOR.as_millis(),
        "max_lifetime_ms":crate::wire::wait_display::MAX_LIFETIME_MS,
        "durable_retention_ms":durable::RETAIN_FOR.as_millis(),
        "durable_requires_request_id":true,"durable_terminal_cursor":1,
        "durable_targets":["exec","job","worker_result","agent_status","agent_messages","event"],
        "max_active_per_member":MAX_ACTIVE_PER_MEMBER,"max_active_per_connection":MAX_ACTIVE_PER_CONNECTION,
        "modes":["any","all"],"targets":["exec","job","agent_status","agent_messages","worker_result","worker_assignment","event","user_input"],
        "server_issued_wait_id":true,"cancel_changes_target":false,"acknowledges_inbox":false,
        "acknowledges_event_subscription":false,
        "grants_completion_permission":false,"public_wait_uses_javascript_slot":false,
        "helper_uses_enclosing_javascript_slot":true})
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    #[default]
    Any,
    All,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Op {
    #[default]
    Wait,
    Cancel,
    List,
    Status,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    Exec {
        job_id: String,
    },
    Job {
        id: String,
    },
    WorkerResult {
        task_id: String,
    },
    WorkerAssignment {
        request_id: String,
    },
    AgentStatus {
        agent_id: String,
        statuses: Vec<String>,
    },
    AgentMessages {
        #[serde(default)]
        after: Option<u64>,
        #[serde(default = "message_limit")]
        limit: usize,
    },
    Event {
        subscription_id: String,
        #[serde(default = "message_limit")]
        limit: usize,
    },
    UserInput {
        #[serde(default)]
        reason: Option<String>,
        #[serde(default = "notify_user")]
        notify: bool,
    },
    Finish {
        request_id: String,
    },
}

fn message_limit() -> usize {
    32
}
fn notify_user() -> bool {
    true
}
fn default_timeout() -> u64 {
    20_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Agent {
    id: String,
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    client_id: String,
    #[serde(default)]
    op: Op,
    #[serde(default)]
    wait_id: Option<String>,
    #[serde(default)]
    targets: Option<Vec<Target>>,
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    window_ms: Option<u64>,
    #[serde(default)]
    lifetime_ms: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    agent: Option<Agent>,
    #[serde(skip)]
    pub(super) controller: Option<Controller>,
    #[serde(skip)]
    pub(super) openai_context: Option<crate::wire::openai::OpenAiContext>,
}

fn identifier(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

impl Arguments {
    pub(super) fn parse(value: Value) -> Result<Self, String> {
        let args: Self = serde_json::from_value(value).map_err(|error|
            format!("await expects client_id, typed targets or an existing wait_id, mode, timeout_ms, and optional agent credentials: {error}"))?;
        if !identifier(&args.client_id, 64)
            || !(1_000..=MAX_OBSERVATION_WAIT_MS).contains(&args.timeout_ms)
        {
            return Err(format!(
                "await client_id must come from list; timeout_ms must be 1000..={MAX_OBSERVATION_WAIT_MS}"
            ));
        }
        if args
            .wait_id
            .as_deref()
            .is_some_and(|id| !identifier(id, 128))
        {
            return Err("wait_id must be the server-issued ID from a previous await".into());
        }
        if args
            .window_ms
            .is_some_and(|ms| !(1_000..=MAX_OBSERVATION_WAIT_MS).contains(&ms))
            || args.after.is_some_and(|after| after > 1)
            || (args.after.is_some() && (args.wait_id.is_none() || args.op != Op::Wait))
            || (args.window_ms.is_some() && args.op != Op::Wait)
        {
            return Err("invalid await observation window or result cursor".into());
        }
        if args
            .request_id
            .as_deref()
            .is_some_and(|id| !identifier(id, 128))
            || args.lifetime_ms.is_some_and(|ms| {
                !(1_000..=crate::wire::wait_display::MAX_LIFETIME_MS).contains(&ms)
            })
            || args.lifetime_ms.is_some() != args.request_id.is_some()
            || (args.lifetime_ms.is_some()
                && (args.op != Op::Wait || args.wait_id.is_some() || args.window_ms.is_none()))
        {
            return Err("durable creation requires request_id, window_ms and lifetime_ms in 1000..=86400000; resume uses only wait_id".into());
        }
        if args.agent.as_ref().is_some_and(|agent| {
            !identifier(&agent.id, 64)
                || agent.token.len() != 64
                || !agent.token.bytes().all(|b| b.is_ascii_hexdigit())
        }) {
            return Err(
                "agent must contain the ID and private token from this participant's collab.join"
                    .into(),
            );
        }
        match (&args.wait_id, &args.targets, args.op) {
            (None, None, Op::List) if args.mode.is_none() => {},
            (Some(_), None, Op::Wait | Op::Cancel | Op::Status) if args.mode.is_none() => {},
            (None, Some(targets), Op::Wait) if (1..=MAX_TARGETS).contains(&targets.len()) => {
                for (index, target) in targets.iter().enumerate() {
                    if args.lifetime_ms.is_some()
                        && matches!(target, Target::UserInput { .. } | Target::WorkerAssignment { .. } | Target::Finish { .. })
                    {
                        return Err("durable waits accept passive targets only; user_input and worker_assignment retain their original transaction".into());
                    }
                    if targets[..index].contains(target) {
                        return Err("await targets must be distinct".into());
                    }
                    if let Target::Event { subscription_id, .. } = target
                        && targets[..index].iter().any(|other| matches!(other,
                            Target::Event { subscription_id: previous, .. } if previous == subscription_id))
                    {
                        return Err("event subscriptions must be distinct within an await".into());
                    }
                    match target {
                        Target::Exec { job_id } if !identifier(job_id, 128) => return Err("invalid execution job_id".into()),
                        Target::Job { id } if !identifier(id, 128) => return Err("invalid relay job id".into()),
                        Target::WorkerResult { task_id } if !identifier(task_id, 128) => return Err("invalid worker task_id".into()),
                        Target::WorkerAssignment { request_id } if !identifier(request_id, 128) => return Err("invalid worker request_id".into()),
                        Target::WorkerResult { .. } if args.window_ms.is_none() => return Err("worker_result await requires window_ms".into()),
                        Target::WorkerAssignment { .. } if args.window_ms.is_none() || targets.len() != 1 => return Err("worker_assignment await requires window_ms and exactly one target".into()),
                        Target::Finish { .. } => return Err("tool_retired: await finish targets are disabled".into()),
                        Target::AgentStatus { agent_id, statuses } if !identifier(agent_id, 64)
                            || statuses.is_empty() || statuses.len() > 16
                            || statuses.iter().any(|s| s.len() > 128 || serde_json::from_value::<crate::collab::lifecycle::Status>(json!(s)).is_err()) => return Err("agent_status requires a registered agent_id and 1..=16 lifecycle values: working, idle, offline, dreaming, retiring".into()),
                        Target::AgentMessages { after, limit } if *limit == 0 || *limit > 64
                            || after.is_some_and(|cursor| cursor > (1_u64 << 53) - 1) => return Err("agent_messages requires limit 1..=64 and a valid receive cursor".into()),
                        Target::Event { subscription_id, limit } if !identifier(subscription_id, 128)
                            || *limit == 0 || *limit > 64 => return Err("event requires a server-issued subscription_id and limit 1..=64".into()),
                        Target::UserInput { reason: Some(reason), .. } if reason.trim().is_empty()
                            || reason.len() > 512 || reason.chars().any(char::is_control) => return Err("user_input reason must contain 1..=512 bytes without controls".into()),
                        _ => {},
                    }
                }
                if targets.len() != 1 && targets.iter().any(|target| matches!(target, Target::UserInput { .. })) {
                    return Err("user_input is a separate single-target transaction".into());
                }
                if targets.iter().filter(|target| matches!(target, Target::AgentMessages { .. })).count() > 1 {
                    return Err("one inbox observation is allowed per await".into());
                }
            },
            _ => return Err("create with targets, or resume/cancel with only the returned wait_id; existing waits keep their original targets and mode".into()),
        }
        Ok(args)
    }
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Pending,
    Ready,
    TimedOut,
    Cancelled,
    Unauthorized,
    UnknownTarget,
    Unavailable,
    Busy,
}

#[derive(Clone, Debug, Serialize)]
struct Problem {
    code: &'static str,
    message: String,
}

#[derive(Clone, Debug, Serialize)]
struct TargetResult {
    target: Target,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Problem>,
}

impl TargetResult {
    fn can_repeat_assignment(&self) -> bool {
        self.status == Status::TimedOut
            || (self.status == Status::Busy
                && self
                    .error
                    .as_ref()
                    .is_some_and(|error| error.message == "client busy; retry later"))
    }

    fn pending(target: &Target) -> Self {
        Self {
            target: target.clone(),
            status: Status::Pending,
            data: None,
            error: None,
        }
    }
    fn failed(
        target: &Target,
        status: Status,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            target: target.clone(),
            status,
            data: None,
            error: Some(Problem {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Clone)]
struct Actor {
    user: String,
    owner: String,
    client_id: String,
    agent_id: String,
    connection: Weak<Session>,
}
impl Actor {
    fn same(&self, other: &Self) -> bool {
        self.same_identity(other) && Weak::ptr_eq(&self.connection, &other.connection)
    }

    fn same_identity(&self, other: &Self) -> bool {
        self.user == other.user
            && self.owner == other.owner
            && self.client_id == other.client_id
            && self.agent_id == other.agent_id
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObservationState {
    Created,
    Suspended,
    Resumed,
    Completed,
}

struct Record {
    id: String,
    actor: Actor,
    targets: Vec<Target>,
    mode: Mode,
    touched: Mutex<Instant>,
    active: AtomicBool,
    terminal: AtomicBool,
    cancelled: watch::Sender<bool>,
    delivery_run: Mutex<Option<String>>,
    windowed: bool,
    snapshot: Mutex<Option<Vec<TargetResult>>>,
    assignment: Mutex<Option<watch::Receiver<Option<TargetResult>>>>,
    acknowledged: AtomicBool,
    snapshot_overflow: AtomicBool,
    observation_state: Mutex<ObservationState>,
    durable: Option<durable::Observation>,
}

impl Record {
    fn assignment_in_flight(&self) -> bool {
        self.assignment
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|result| result.borrow().is_none() && result.has_changed().is_ok())
    }

    fn retained(&self, now: Instant) -> bool {
        if self.active.load(Ordering::Acquire) || self.assignment_in_flight() {
            return true;
        }
        if let Some(durable) = &self.durable {
            return durable.retained(now);
        }
        self.active.load(Ordering::Acquire)
            || self.awaiting_delivery()
            || now
                .saturating_duration_since(*self.touched.lock().unwrap_or_else(|e| e.into_inner()))
                < RETAIN_FOR
    }

    fn awaiting_delivery(&self) -> bool {
        self.delivery_run
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
            && self
                .actor
                .connection
                .upgrade()
                .is_some_and(|session| session.is_alive())
    }

    fn has_unacknowledged_result(&self) -> bool {
        self.windowed
            && !self.acknowledged.load(Ordering::Acquire)
            && !*self.cancelled.borrow()
            && self
                .snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|results| resolved(self.mode, results) == Some(Status::Ready))
    }

    fn transition(&self, broker: &Broker, next: ObservationState, data: Value) {
        let mut state = self
            .observation_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let kind = match (*state, next) {
            (
                ObservationState::Created | ObservationState::Resumed,
                ObservationState::Suspended,
            ) => "wait.suspended",
            (ObservationState::Suspended, ObservationState::Resumed) => "wait.resumed",
            (previous, ObservationState::Completed) if previous != ObservationState::Completed => {
                "wait.completed"
            }
            _ => return,
        };
        if *state == ObservationState::Created {
            super::events_tool::wait_transition(
                broker,
                &self.actor.owner,
                &self.actor.client_id,
                &self.actor.agent_id,
                &self.id,
                "wait.started",
                json!({"mode":self.mode,"target_count":self.targets.len()}),
            );
        }
        *state = next;
        // The synchronous hook preserves transition order without retaining a
        // lock across an await. Replayed snapshots never enter this path.
        super::events_tool::wait_transition(
            broker,
            &self.actor.owner,
            &self.actor.client_id,
            &self.actor.agent_id,
            &self.id,
            kind,
            data,
        );
    }
}

struct ObservationEvents<'a> {
    broker: &'a Broker,
    record: &'a Record,
}

impl Drop for ObservationEvents<'_> {
    fn drop(&mut self) {
        // Dropping an observation future releases only observation ownership.
        // A native operation whose response was lost keeps its actual state.
        self.record.transition(
            self.broker,
            ObservationState::Resumed,
            json!({"status":"unknown","reason":"observer_dropped"}),
        );
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    records: Mutex<HashMap<String, Arc<Record>>>,
}

impl Registry {
    fn record(
        &self,
        actor: Actor,
        args: &Arguments,
    ) -> Result<Arc<Record>, (Status, &'static str)> {
        let mut records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        records.retain(|_, record| record.retained(now));
        if let Some(id) = &args.wait_id {
            let record = records
                .get(id)
                .ok_or((Status::UnknownTarget, "await_unknown_wait"))?;
            if !record.actor.same(&actor) {
                return Err((Status::Unauthorized, "await_owner_mismatch"));
            }
            *record.touched.lock().unwrap_or_else(|e| e.into_inner()) = now;
            return Ok(record.clone());
        }
        if let Some(request_id) = &args.request_id
            && let Some(record) = records.values().find(|record| {
                record.actor.same_identity(&actor)
                    && record
                        .durable
                        .as_ref()
                        .is_some_and(|value| &value.request_id == request_id)
            })
        {
            if !record.actor.same(&actor) {
                return Err((Status::Unauthorized, "await_connection_changed"));
            }
            if !record
                .durable
                .as_ref()
                .expect("durable request")
                .matches(args)
            {
                return Err((Status::Unavailable, "await_request_conflict"));
            }
            return Ok(record.clone());
        }
        let at_capacity = |records: &HashMap<String, Arc<Record>>| {
            records.len() >= MAX_RECORDS
                || records
                    .values()
                    .filter(|record| record.actor.same_identity(&actor))
                    .count()
                    >= MAX_ACTOR_RECORDS
        };
        if at_capacity(&records) {
            let actor_full = records
                .values()
                .filter(|record| record.actor.same_identity(&actor))
                .count()
                >= MAX_ACTOR_RECORDS;
            let victim = records
                .values()
                .filter(|record| {
                    !record.active.load(Ordering::Acquire)
                        && !record.assignment_in_flight()
                        && record.durable.is_none()
                        && !record.awaiting_delivery()
                        && !record.has_unacknowledged_result()
                        && record.terminal.load(Ordering::Acquire)
                        && (!actor_full || record.actor.same(&actor))
                })
                .min_by_key(|record| *record.touched.lock().unwrap_or_else(|e| e.into_inner()))
                .map(|record| record.id.clone());
            if let Some(id) = victim {
                records.remove(&id);
            }
        }
        if at_capacity(&records) {
            return Err((Status::Busy, "await_registry_full"));
        }
        let id = loop {
            let mut bytes = [0_u8; 16];
            crate::platform::random_bytes(&mut bytes)
                .map_err(|_| (Status::Unavailable, "await_id_unavailable"))?;
            let id = format!("await_{}", crate::hex::encode(bytes));
            if !records.contains_key(&id) {
                break id;
            }
        };
        let record = Arc::new(Record {
            id: id.clone(),
            actor,
            targets: args.targets.clone().expect("validated creation"),
            mode: args.mode.unwrap_or_default(),
            touched: Mutex::new(now),
            active: AtomicBool::new(false),
            terminal: AtomicBool::new(false),
            cancelled: watch::channel(false).0,
            delivery_run: Mutex::new(None),
            windowed: args.window_ms.is_some(),
            snapshot: Mutex::new(None),
            assignment: Mutex::new(None),
            acknowledged: AtomicBool::new(false),
            snapshot_overflow: AtomicBool::new(false),
            observation_state: Mutex::new(ObservationState::Created),
            durable: args.lifetime_ms.map(|_| durable::Observation::new(args)),
        });
        records.insert(id, record.clone());
        Ok(record)
    }

    fn activate(&self, record: &Arc<Record>) -> Result<Active, (Status, &'static str)> {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if record.active.load(Ordering::Acquire) {
            return Err((Status::Busy, "await_already_waiting"));
        }
        let active = records.values().filter(|other| {
            !Arc::ptr_eq(other, record)
                && (other.active.load(Ordering::Acquire) || other.assignment_in_flight())
        });
        let member = active
            .clone()
            .filter(|other| other.actor.same(&record.actor))
            .count();
        let connection = active
            .filter(|other| Weak::ptr_eq(&other.actor.connection, &record.actor.connection))
            .count();
        if member >= MAX_ACTIVE_PER_MEMBER || connection >= MAX_ACTIVE_PER_CONNECTION {
            return Err((Status::Busy, "await_concurrency_full"));
        }
        if record
            .active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err((Status::Busy, "await_already_waiting"));
        }
        let guard = Active(record.clone());
        // Completion publishes terminal before releasing active. Recheck only
        // after owning the lease so a stale pre-CAS read cannot restart it.
        if *record.cancelled.borrow() {
            return Err((Status::Cancelled, "await_cancelled"));
        }
        if record.terminal.load(Ordering::Acquire) {
            return Err((Status::Unavailable, "await_already_resolved"));
        }
        Ok(guard)
    }

    fn cancel(&self, record: &Record) {
        let _records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        record.cancelled.send_replace(true);
        record.terminal.store(true, Ordering::Release);
    }
}

struct Active(Arc<Record>);
impl Drop for Active {
    fn drop(&mut self) {
        *self.0.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        self.0.active.store(false, Ordering::Release);
    }
}

struct Scope {
    broker: Arc<Broker>,
    user: String,
    token: String,
    session: Arc<Session>,
    recipient: Recipient,
    binding: code_mode::RunBinding,
    run: code_mode::AuditRun,
    user_wait_admitted: AtomicBool,
}

impl Scope {
    async fn create(
        broker: Arc<Broker>,
        user: String,
        token: String,
        args: &Arguments,
    ) -> Result<Self, String> {
        let authorization = broker
            .authenticate(&user, &token)
            .await
            .ok_or("authorization expired or revoked")?;
        if !matches!(authorization, Authorization::Identity(_)) {
            return Err("await requires an authenticated account and the current participant's collab binding".into());
        }
        let session = broker
            .client(&user, &authorization, &args.client_id)
            .await?;
        let owner = crate::bridge::owner(&user, &authorization)?;
        let run = code_mode::AUDIT_RUN
            .try_with(Clone::clone)
            .map_err(|_| "await audit scope unavailable")?;
        let binding = args.agent.as_ref().map(|agent| code_mode::RunBinding {
            owner: owner.clone(), agent_id: agent.id.clone(), token: agent.token.clone(),
        }).or_else(|| run.binding(&args.client_id))
            .ok_or("await_thread_required: collab.join registers this participant without starting a model; bind its returned private credentials")?;
        if binding.owner != owner {
            return Err("await account mismatch".into());
        }
        session
            .collaboration
            .bind(&owner, &binding.agent_id, &binding.token)?;
        run.record_connection(&session)?;
        run.bind(&args.client_id, &owner, &binding.agent_id, &binding.token)?;
        let recipient = Recipient {
            owner,
            binding: Some(binding.clone()),
            connection: Arc::downgrade(&session),
        };
        Ok(Self {
            broker,
            user,
            token,
            session,
            recipient,
            binding,
            run,
            user_wait_admitted: AtomicBool::new(false),
        })
    }

    fn actor(&self) -> Actor {
        Actor {
            user: self.user.clone(),
            owner: self.recipient.owner.clone(),
            client_id: self.session.client_id().into(),
            agent_id: self.binding.agent_id.clone(),
            connection: Arc::downgrade(&self.session),
        }
    }

    fn final_authorization(&self) -> Result<(), String> {
        let authorization = self
            .broker
            .authenticate_now(&self.user, &self.token)
            .ok_or("authorization expired or revoked")?;
        if !self.session.is_alive()
            || !authorization.is_live()
            || !authorization.permits(&self.session)
            || crate::bridge::owner(&self.user, &authorization).as_deref()
                != Ok(self.recipient.owner.as_str())
        {
            return Err("await connection or access changed".into());
        }
        self.session.collaboration.bind(
            &self.binding.owner,
            &self.binding.agent_id,
            &self.binding.token,
        )?;
        Ok(())
    }

    async fn authorize_events(&self, targets: &[Target]) -> Result<(), String> {
        for target in targets {
            if let Target::Event {
                subscription_id, ..
            } = target
            {
                super::events_tool::authorize_observation(
                    &self.broker,
                    &self.user,
                    &self.token,
                    self.session.client_id(),
                    &self.binding.agent_id,
                    &self.binding.token,
                    subscription_id,
                )
                .await?;
            }
        }
        for target in targets {
            if let Target::Event {
                subscription_id, ..
            } = target
            {
                super::events_tool::authorize_observation_now(
                    &self.broker,
                    &self.user,
                    &self.token,
                    self.session.client_id(),
                    &self.binding.agent_id,
                    &self.binding.token,
                    subscription_id,
                )?;
            }
        }
        self.final_authorization()
    }

    async fn native(
        &self,
        name: &str,
        arguments: Value,
        controller: Option<Controller>,
    ) -> Result<Value, String> {
        let authorization = user_wait::authorized(
            &self.broker,
            &self.user,
            &self.token,
            self.session.client_id(),
            &self.session,
            &self.recipient,
        )
        .await
        .map_err(|_| "await_authorization_revoked".to_owned())?;
        let result = self
            .session
            .call_with_bound_authorization(
                Operation::CallTool {
                    name: name.into(),
                    arguments,
                },
                controller,
                &authorization,
                Some(self.binding.clone()),
            )
            .await?;
        self.final_authorization()?;
        if result["isError"] == true {
            return Err(result["content"]
                .as_array()
                .and_then(|parts| parts.iter().find_map(|p| p["text"].as_str()))
                .unwrap_or("native wait failed")
                .chars()
                .take(4096)
                .collect());
        }
        result
            .get("structuredContent")
            .filter(|data| data.is_object())
            .cloned()
            .ok_or_else(|| {
                "native wait returned no structured result; upgrade the selected client".into()
            })
    }
}

fn envelope(id: Option<&str>, mode: Mode, status: Status, results: Vec<TargetResult>) -> Value {
    json!({"schema_version":1,"wait_id":id,"mode":mode,"status":status,"results":results,
        "continue_wait":status == Status::TimedOut,
        "completion_permission":null,
        "note":"Ready describes only the named target. Job exit, agent status, inbox messages and user input never approve completion. Finish permission requires a separate retained human Yes and is consumed only by an explicit native final answer."})
}

fn failure(args: &Arguments, status: Status, code: &'static str, message: &str) -> Value {
    let results = args
        .targets
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|target| TargetResult::failed(target, status, code, message))
        .collect();
    let mut value = envelope(
        args.wait_id.as_deref(),
        args.mode.unwrap_or_default(),
        status,
        results,
    );
    value["error"] = json!({"code":code,"message":message});
    value
}

fn target_error_status(target: &Target, error: &str) -> Status {
    if error.starts_with("await_unknown_target:")
        || error.starts_with("finish_request_not_found:")
        || error.contains("unknown or expired background job")
        || matches!(target, Target::Job { .. })
    {
        Status::UnknownTarget
    } else if error == "client busy; retry later" || error.contains("already has a waiting receive")
    {
        // This exact native capacity error precedes dispatch. An unknown
        // outcome after dispatch must not become a retryable observation.
        Status::Busy
    } else {
        Status::Unavailable
    }
}

// An HTTP read can expire while a real assignment is travelling back from the
// native pool. Keep exactly that authenticated request alive across read windows.
async fn observe_assignment(
    scope: &Arc<Scope>,
    record: &Record,
    target: &Target,
    timeout_ms: u64,
    controller: Option<Controller>,
) -> TargetResult {
    let mut receiver = {
        let mut saved = record.assignment.lock().unwrap_or_else(|e| e.into_inner());
        if saved.as_ref().is_some_and(|r| {
            r.borrow()
                .as_ref()
                .is_some_and(TargetResult::can_repeat_assignment)
        }) {
            // The pool confirmed an empty wait or rejected dispatch for capacity.
            *saved = None;
        }
        saved
            .get_or_insert_with(|| {
                let (sender, receiver) = watch::channel(None);
                let scope = Arc::clone(scope);
                let target = target.clone();
                let id = record.id.clone();
                let cancelled = record.cancelled.subscribe();
                tokio::spawn(code_mode::AUDIT_RUN.scope(scope.run.clone(), async move {
                    let result = if *cancelled.borrow() {
                        TargetResult::failed(
                            &target,
                            Status::Cancelled,
                            "await_cancelled",
                            "wait cancelled",
                        )
                    } else {
                        observe(&scope, &target, timeout_ms, controller, true, &id).await
                    };
                    let result = if serde_json::to_vec(&result)
                        .is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES)
                    {
                        result
                    } else {
                        TargetResult::failed(
                            &target,
                            Status::Unavailable,
                            "await_snapshot_overflow",
                            "bounded wait snapshot exceeded its byte limit",
                        )
                    };
                    // Cancellation stops delivery, not a claim the pool already accepted.
                    // The native request_id remains the claimant's recovery identity.
                    if !*cancelled.borrow() {
                        sender.send_replace(Some(result));
                    }
                }));
                receiver
            })
            .clone()
    };
    loop {
        if let Some(result) = receiver.borrow_and_update().clone() {
            return result;
        }
        if receiver.changed().await.is_err() {
            return TargetResult::failed(
                target,
                Status::Unavailable,
                "await_worker_outcome_unknown",
                "original assignment result unavailable",
            );
        }
    }
}

fn target_wait_ms(target: &Target, window_ms: u64, windowed: bool) -> u64 {
    // Worker waits accept a sub-second native budget. Keep transport time even
    // in the shortest outer window so an empty assignment returns its receipt.
    let with_margin = window_ms.saturating_sub(100);
    match target {
        Target::WorkerAssignment { .. } => with_margin,
        Target::UserInput { notify: false, .. } if windowed => window_ms,
        _ => with_margin.max(1000),
    }
}

async fn observe(
    scope: &Scope,
    target: &Target,
    timeout_ms: u64,
    controller: Option<Controller>,
    windowed: bool,
    wait_id: &str,
) -> TargetResult {
    let data = match target {
        Target::Exec { job_id } => {
            scope
                .native(
                    "ssmcp.exec.wait",
                    json!({"job_id":job_id,"wait_ms":timeout_ms}),
                    controller,
                )
                .await
        }
        Target::Job { id } => {
            scope
                .broker
                .bridges
                .observe_job(&scope.binding.owner, id, timeout_ms)
                .await
        }
        Target::WorkerResult { task_id } => {
            scope
                .native(
                    "ssmcp.worker.result",
                    json!({"task_id":task_id,"wait_ms":timeout_ms}),
                    controller,
                )
                .await
        }
        Target::WorkerAssignment { request_id } => {
            scope
                .native(
                    "ssmcp.worker.wait",
                    json!({"request_id":request_id,"wait_ms":timeout_ms}),
                    controller,
                )
                .await
        }
        Target::AgentStatus { agent_id, statuses } => {
            scope
                .session
                .collaboration
                .wait_status(
                    &scope.binding.owner,
                    &scope.binding.agent_id,
                    &scope.binding.token,
                    agent_id,
                    statuses,
                    timeout_ms,
                )
                .await
        }
        Target::AgentMessages { after, limit } => {
            scope
                .session
                .collaboration
                .receive(
                    &scope.binding.owner,
                    &scope.binding.agent_id,
                    &scope.binding.token,
                    *after,
                    *limit,
                    timeout_ms,
                )
                .await
        }
        Target::Event {
            subscription_id,
            limit,
        } => {
            super::events_tool::observe(
                &scope.broker,
                (&scope.user, &scope.token),
                scope.session.client_id(),
                (&scope.binding.agent_id, &scope.binding.token),
                subscription_id,
                (*limit, timeout_ms),
                Some(wait_id),
            )
            .await
        }
        Target::Finish { .. } => Err("tool_retired: await finish targets are disabled".into()),
        Target::UserInput { reason, notify } => {
            user_wait::wait_observed(
                scope.broker.clone(),
                scope.user.clone(),
                scope.token.clone(),
                scope.session.client_id().into(),
                code_mode::user_wait::Arguments {
                    timeout_ms,
                    intent: code_mode::user_wait::Intent::NeedUser,
                    reason: reason.clone(),
                    notify: *notify,
                    finish_request_id: None,
                },
                controller,
                Some((&scope.user_wait_admitted, windowed)),
            )
            .await
        }
    };
    if let Err(error) = scope.final_authorization() {
        return TargetResult::failed(
            target,
            Status::Unauthorized,
            "await_authorization_revoked",
            error,
        );
    }
    let data = match data {
        Ok(data) => data,
        Err(error) => {
            let status = target_error_status(target, &error);
            return TargetResult::failed(target, status, "await_target_error", error);
        }
    };
    let status = match target {
        Target::Exec { .. } => match data["running"].as_bool() {
            Some(false) => Status::Ready,
            Some(true) => Status::TimedOut,
            None => Status::Unavailable,
        },
        Target::Job { .. } => {
            if matches!(
                data["state"].as_str(),
                Some("starting" | "running" | "finishing")
            ) {
                Status::TimedOut
            } else {
                Status::Ready
            }
        }
        Target::WorkerResult { .. } => worker_result_status(&data),
        Target::WorkerAssignment { .. } => match data["status"].as_str() {
            Some("waiting") => Status::TimedOut,
            Some("assigned" | "already_completed" | "retiring" | "released" | "not_accepting") => {
                Status::Ready
            }
            _ => Status::Unavailable,
        },
        Target::AgentStatus { .. } => {
            if data["matched"] == true {
                Status::Ready
            } else {
                Status::TimedOut
            }
        }
        Target::AgentMessages { .. } => {
            if data["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty())
            {
                Status::Ready
            } else {
                Status::TimedOut
            }
        }
        Target::Event { .. } => event_status(&data),
        Target::Finish { .. } => {
            return TargetResult::failed(
                target,
                Status::Unavailable,
                "tool_retired",
                "await finish targets are disabled",
            );
        }
        Target::UserInput { .. } => match data["status"].as_str() {
            Some("ready" | "received") => Status::Ready,
            Some("timed_out") => Status::TimedOut,
            Some("cancelled" | "superseded" | "disconnected") => Status::Cancelled,
            Some("authorization_revoked" | "binding_invalid") => Status::Unauthorized,
            Some("busy") => Status::Busy,
            _ => Status::Unavailable,
        },
    };
    TargetResult {
        target: target.clone(),
        status,
        data: Some(data),
        error: None,
    }
}

fn worker_result_status(data: &Value) -> Status {
    match data["task"]["status"].as_str() {
        Some("queued" | "working" | "cancelling") => Status::TimedOut,
        Some("completed" | "failed" | "cancelled") => Status::Ready,
        Some("unknown") => Status::UnknownTarget,
        _ => Status::Unavailable,
    }
}

fn event_status(data: &Value) -> Status {
    match (data["status"].as_str(), data["events"].as_array()) {
        (Some("ready"), Some(events)) if !events.is_empty() => Status::Ready,
        (Some("pending"), Some(events)) if events.is_empty() => Status::TimedOut,
        _ => Status::Unavailable,
    }
}

fn resolved(mode: Mode, results: &[TargetResult]) -> Option<Status> {
    // A typed failure is never swallowed by an unrelated ready target.
    if let Some(result) = results.iter().find(|result| {
        matches!(
            result.status,
            Status::Cancelled
                | Status::Unauthorized
                | Status::UnknownTarget
                | Status::Unavailable
                | Status::Busy
        )
    }) {
        return Some(result.status);
    }
    if (mode == Mode::Any && results.iter().any(|r| r.status == Status::Ready))
        || results.iter().all(|r| r.status == Status::Ready)
    {
        return Some(Status::Ready);
    }
    if results.iter().all(|r| r.status != Status::Pending) {
        return Some(Status::TimedOut);
    }
    None
}

fn window_envelope(
    record: &Record,
    status: Status,
    results: Vec<TargetResult>,
    cursor: u64,
) -> Value {
    let mut value = envelope(Some(&record.id), record.mode, status, results);
    value["cursor"] = json!(cursor);
    value["continue_wait"] = json!(status == Status::Pending);
    value
}

fn read_window_snapshot(record: &Record, after: Option<u64>) -> Result<Option<Value>, String> {
    let mut snapshot = record.snapshot.lock().unwrap_or_else(|e| e.into_inner());
    if !record.terminal.load(Ordering::Acquire) {
        return if after == Some(1) {
            Err("await cursor is ahead of the retained result".into())
        } else {
            Ok(None)
        };
    }
    if record.snapshot_overflow.load(Ordering::Acquire) {
        return Err(
            "await_snapshot_overflow: the original delivery exceeded the replay byte limit".into(),
        );
    }
    if record.acknowledged.load(Ordering::Acquire) {
        if after != Some(1) {
            return Err("await result cursor expired after acknowledgement".into());
        }
        let mut receipt = window_envelope(record, Status::Ready, Vec::new(), 1);
        receipt["acknowledged"] = json!(true);
        return Ok(Some(receipt));
    }
    let Some(results) = snapshot.as_ref() else {
        return Ok(None);
    };
    // Only a ready result earns cursor 1. A legacy failure must never
    // disclose a previously retained partial result.
    if resolved(record.mode, results) != Some(Status::Ready) {
        return Ok(None);
    }
    if after == Some(1) {
        snapshot.take();
        record
            .assignment
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        record.acknowledged.store(true, Ordering::Release);
        let mut receipt = window_envelope(record, Status::Ready, Vec::new(), 1);
        receipt["acknowledged"] = json!(true);
        return Ok(Some(receipt));
    }
    Ok(Some(window_envelope(
        record,
        Status::Ready,
        results.clone(),
        1,
    )))
}

fn display_text(text: &str, maximum: usize) -> String {
    let clean: String = text
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    let redacted = crate::audit::event(json!(clean));
    let text = redacted.as_str().unwrap_or("[redacted]");
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut end = maximum.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Read public member metadata only through the existing authenticated observer
/// API. Other references are the caller's declared identifiers, not claims that
/// those objects exist or are accessible. Never inspect result or inbox bodies.
async fn display_targets(scope: &Scope, targets: &[Target]) -> (Vec<String>, Vec<String>) {
    let mut labels = Vec::new();
    let mut details = Vec::new();
    for target in targets {
        let (label, detail) = match target {
            Target::Exec { job_id } => (
                "Command exits".into(),
                format!("Requested execution job ID {job_id}"),
            ),
            Target::Job { id } => (
                "Background job returns".into(),
                format!("Requested background job ID {id}"),
            ),
            Target::WorkerResult { task_id } => (
                "Worker task result".into(),
                format!("Requested task ID {task_id}"),
            ),
            Target::WorkerAssignment { request_id } => (
                "Worker assignment or retirement".into(),
                format!("Assignment request ID {request_id}"),
            ),
            Target::AgentStatus { agent_id, statuses } => {
                let public = scope
                    .session
                    .collaboration
                    .wait_status(
                        &scope.binding.owner,
                        &scope.binding.agent_id,
                        &scope.binding.token,
                        agent_id,
                        statuses,
                        0,
                    )
                    .await
                    .ok();
                let name = public
                    .as_ref()
                    .and_then(|value| value["agent"]["name"].as_str());
                let label = name
                    .map(|name| format!("{name}: requested status"))
                    .unwrap_or_else(|| "Requested member status".into());
                (
                    label,
                    format!(
                        "Requested member ID {agent_id}; status must equal one of: {}",
                        statuses.join(" | ")
                    ),
                )
            }
            Target::AgentMessages { after, limit } => (
                "Observer inbox messages".into(),
                format!(
                    "New messages after {}; return limit {limit}. Message bodies are not displayed.",
                    after
                        .map(|cursor| cursor.to_string())
                        .unwrap_or_else(|| "the observer's current inbox cursor".into())
                ),
            ),
            Target::Event {
                subscription_id,
                limit,
            } => {
                let authorized = super::events_tool::authorize_observation_now(
                    &scope.broker,
                    &scope.user,
                    &scope.token,
                    scope.session.client_id(),
                    &scope.binding.agent_id,
                    &scope.binding.token,
                    subscription_id,
                )
                .is_ok();
                (
                    "Subscription events".into(),
                    if authorized {
                        format!(
                            "Authorized subscription ID {subscription_id}; return limit {limit}. Event bodies are not displayed."
                        )
                    } else {
                        "Subscription details unavailable until authorization is established."
                            .into()
                    },
                )
            }
            Target::UserInput { notify, .. } => (
                "Requested user input".into(),
                format!("Wait for user input; notification requested: {notify}."),
            ),
            Target::Finish { .. } => ("Finish decision".into(), String::new()),
        };
        labels.push(display_text(&label, 256));
        details.push(display_text(&detail, 4096));
    }
    (labels, details)
}

pub(super) async fn wait(
    broker: Arc<Broker>,
    user: String,
    token: String,
    args: Arguments,
) -> Result<Value, String> {
    let scope = match Scope::create(broker.clone(), user, token, &args).await {
        Ok(scope) => scope,
        Err(error) => {
            return Ok(failure(
                &args,
                Status::Unauthorized,
                "await_unauthorized",
                &error,
            ));
        }
    };
    if args.op == Op::List {
        return durable::list(&scope).await;
    }
    if !scope.session.chat_enabled()
        && args
            .targets
            .iter()
            .flatten()
            .any(|target| matches!(target, Target::UserInput { .. }))
    {
        return Err(crate::wire::CHAT_DISABLED.into());
    }
    let record = match broker.awaiting.record(scope.actor(), &args) {
        Ok(record) => record,
        Err((status, code)) => {
            return Ok(failure(
                &args,
                status,
                code,
                "wait is unknown, expired, owned by another participant, or the registry is full",
            ));
        }
    };
    if record.durable.is_some() {
        return durable::wait(scope, record, args).await;
    }
    if args.op == Op::Status {
        return Err("await status metadata is available for durable waits only".into());
    }
    let scope = Arc::new(scope);
    let mut results: Vec<_> = record.targets.iter().map(TargetResult::pending).collect();
    if args.op == Op::Cancel {
        broker.awaiting.cancel(&record);
    }
    if *record.cancelled.borrow() {
        for result in &mut results {
            result.status = Status::Cancelled;
        }
        record.transition(
            &broker,
            ObservationState::Resumed,
            json!({"status":"cancelled"}),
        );
        record.transition(
            &broker,
            ObservationState::Completed,
            json!({"status":"cancelled"}),
        );
        return Ok(envelope(
            Some(&record.id),
            record.mode,
            Status::Cancelled,
            results,
        ));
    }
    let passive_window = record.windowed
        && !record
            .targets
            .iter()
            .any(|target| matches!(target, Target::UserInput { .. }));
    if (!record.windowed && args.window_ms.is_some()) || (args.after.is_some() && !record.windowed)
    {
        return Err("observation mode and result cursor must match the original await".into());
    }
    if passive_window || args.after.is_some() {
        if record.terminal.load(Ordering::Acquire)
            && let Err(error) = scope.authorize_events(&record.targets).await
        {
            let results = record
                .targets
                .iter()
                .map(|target| {
                    TargetResult::failed(
                        target,
                        Status::Unauthorized,
                        "await_authorization_revoked",
                        error.clone(),
                    )
                })
                .collect();
            return Ok(envelope(
                Some(&record.id),
                record.mode,
                Status::Unauthorized,
                results,
            ));
        }
        if let Some(response) = read_window_snapshot(&record, args.after)? {
            return Ok(response);
        }
    }
    let window_ms = args.window_ms.unwrap_or(args.timeout_ms);
    let _active = match broker.awaiting.activate(&record) {
        Ok(active) => active,
        Err((status, code)) => {
            for result in &mut results {
                *result = TargetResult::failed(
                    &result.target,
                    status,
                    code,
                    "observation is already active, cancelled, resolved, or at its bounded concurrency limit",
                );
            }
            return Ok(envelope(Some(&record.id), record.mode, status, results));
        }
    };
    let (target_labels, target_details) = display_targets(&scope, &record.targets).await;
    let deadline = Instant::now() + Duration::from_millis(window_ms);
    record.transition(
        &broker,
        ObservationState::Suspended,
        json!({"window_ms":window_ms,"mode":record.mode}),
    );
    let _observation_events = ObservationEvents {
        broker: &broker,
        record: &record,
    };
    // Presentation is best effort and follows admission. It owns no native
    // slot, user-input reservation, target result or finish permission.
    let display = scope.session.show_wait(
        &scope.broker,
        crate::broker::wait_display::Request {
            user: &scope.user,
            token: &scope.token,
            binding: &scope.binding,
            wait_id: &record.id,
            operation: "await",
            targets: record
                .targets
                .iter()
                .map(|target| match target {
                    Target::Exec { .. } => crate::wire::wait_display::Kind::Exec,
                    Target::Job { .. }
                    | Target::WorkerResult { .. }
                    | Target::WorkerAssignment { .. } => crate::wire::wait_display::Kind::Job,
                    Target::AgentStatus { .. } => crate::wire::wait_display::Kind::AgentStatus,
                    Target::AgentMessages { .. } => crate::wire::wait_display::Kind::AgentMessages,
                    Target::Event { .. } => crate::wire::wait_display::Kind::Event,
                    Target::UserInput { .. } => crate::wire::wait_display::Kind::UserInput,
                    Target::Finish { .. } => crate::wire::wait_display::Kind::Finish,
                })
                .collect(),
            target_labels,
            target_details,
            mode: match record.mode {
                Mode::Any => crate::wire::wait_display::Mode::Any,
                Mode::All => crate::wire::wait_display::Mode::All,
            },
            timeout_ms: window_ms,
        },
    );
    let mut cancelled = record.cancelled.subscribe();
    let mut pending = FuturesUnordered::new();
    if passive_window
        && let Some(previous) = record
            .snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    {
        results = previous;
    }
    for (index, target) in record.targets.iter().enumerate() {
        if results[index].status == Status::Ready {
            continue;
        }

        let scope = &scope;
        let controller = args.controller.clone();
        let windowed = record.windowed;
        let wait_id = record.id.as_str();
        let target_timeout = target_wait_ms(target, window_ms, windowed);
        let record = &record;
        pending.push(async move {
            let result = if matches!(target, Target::WorkerAssignment { .. }) {
                observe_assignment(scope, record, target, target_timeout, controller).await
            } else {
                observe(scope, target, target_timeout, controller, windowed, wait_id).await
            };
            (index, result)
        });
    }
    let workers = super::events_tool::worker_subscriptions::wait_for_notice(
        &scope.broker,
        &scope.user,
        &scope.token,
        &scope.session,
        &scope.binding,
        window_ms,
        Some(&record.id),
    );
    tokio::pin!(workers);
    let mut worker_observation_open = true;
    let mut worker_notice = None;
    let mut authority = tokio::time::interval(Duration::from_secs(1));
    authority.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let status = loop {
        if *cancelled.borrow() {
            break Status::Cancelled;
        }
        tokio::select! {
            biased;
            _ = cancelled.changed() => { if *cancelled.borrow() { break Status::Cancelled; } },
            result = pending.next() => {
                let Some((index, result)) = result else { break Status::TimedOut; };
                results[index] = result;
                if let Some(status) = resolved(record.mode, &results) { break status; }
            },
            notice = &mut workers, if worker_observation_open => {
                worker_observation_open = false;
                if let Some(notice) = notice {
                    worker_notice = Some(notice);
                    break Status::Pending;
                }
            },
            _ = tokio::time::sleep_until(deadline) => break Status::TimedOut,
            _ = authority.tick() => {
                match tokio::time::timeout_at(deadline, user_wait::authorized(
                    &scope.broker, &scope.user, &scope.token,
                    scope.session.client_id(), &scope.session, &scope.recipient)).await {
                    Ok(Ok(_)) => {},
                    Ok(Err(_)) => break Status::Unauthorized,
                    Err(_) => break Status::TimedOut,
                }
            },
        }
    };
    drop(pending);
    let status = if scope.authorize_events(&record.targets).await.is_err() {
        Status::Unauthorized
    } else if *record.cancelled.borrow() {
        Status::Cancelled
    } else {
        status
    };
    record.transition(&broker, ObservationState::Resumed, json!({"status":status}));
    for result in &mut results {
        if status == Status::Unauthorized {
            *result = TargetResult::failed(
                &result.target,
                status,
                "await_authorization_revoked",
                "observation withheld because authentication, connection, or membership changed",
            );
        } else if result.status == Status::Pending
            && matches!(status, Status::Cancelled | Status::TimedOut)
        {
            result.status = status;
        }
    }
    if status != Status::Ready
        && scope.user_wait_admitted.load(Ordering::Acquire)
        && record
            .targets
            .iter()
            .any(|target| matches!(target, Target::UserInput { .. }))
    {
        let outcome = match status {
            Status::Unauthorized => Outcome::AuthorizationRevoked,
            Status::TimedOut => Outcome::TimedOut,
            _ => Outcome::Cancelled,
        };
        scope
            .run
            .finish_wait_state(scope.session.client_id(), outcome, "await_observer_stopped");
    }
    if status == Status::Ready
        && record
            .targets
            .iter()
            .any(|target| matches!(target, Target::UserInput { .. }))
    {
        *record
            .delivery_run
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(scope.run.context.run_id.clone());
    }
    // A retryable observation must not overwrite a concurrent cancel's terminal
    // transition. Final user-delivery retries are settled under the registry lock.
    if passive_window && matches!(status, Status::Ready | Status::TimedOut | Status::Pending) {
        let mut retained = results.clone();
        for result in &mut retained {
            if result.status == Status::TimedOut {
                *result = TargetResult::pending(&result.target);
            }
        }
        if serde_json::to_vec(&retained).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES) {
            *record.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(retained);
            if status == Status::Ready {
                // The frozen, cursor-addressable snapshot now owns the receipt.
                record
                    .assignment
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
            }
        } else {
            record.terminal.store(true, Ordering::Release);
            record.snapshot_overflow.store(true, Ordering::Release);
            record.transition(
                &broker,
                ObservationState::Completed,
                json!({"status":"unavailable","reason":"await_snapshot_overflow"}),
            );
            let overflow = record
                .targets
                .iter()
                .map(|target| {
                    TargetResult::failed(
                        target,
                        Status::Unavailable,
                        "await_snapshot_overflow",
                        "bounded wait snapshot exceeded its byte limit",
                    )
                })
                .collect();
            if let Some(display) = display {
                display.finish(crate::wire::wait_display::Outcome::Unknown);
            }
            return Ok(envelope(
                Some(&record.id),
                record.mode,
                Status::Unavailable,
                overflow,
            ));
        }
    }
    if let Some(display) = display {
        use crate::wire::wait_display::Outcome as DisplayOutcome;
        display.finish(match status {
            Status::Pending if worker_notice.is_some() => DisplayOutcome::Triggered,
            Status::Ready => DisplayOutcome::Triggered,
            Status::TimedOut => DisplayOutcome::WindowExpired,
            Status::Cancelled => DisplayOutcome::ObservationStopped,
            _ => DisplayOutcome::Unknown,
        });
    }
    if !matches!(status, Status::TimedOut | Status::Busy | Status::Pending) {
        record.terminal.store(true, Ordering::Release);
        // User input is committed later at the atomic outer response boundary.
        // A ready reservation can still become a retryable delivery outcome.
        if status != Status::Ready
            || !record
                .targets
                .iter()
                .any(|target| matches!(target, Target::UserInput { .. }))
        {
            record.transition(
                &broker,
                ObservationState::Completed,
                json!({"status":status}),
            );
        }
    }
    if let Some(notice) = worker_notice
        && status == Status::Pending
    {
        let mut response = if record.windowed {
            window_envelope(&record, Status::Pending, results, 0)
        } else {
            envelope(Some(&record.id), record.mode, Status::Pending, results)
        };
        // A subscription wake ends only this read window, including ordinary
        // non-windowed waits. The same wait_id still has unfinished targets.
        response["continue_wait"] = json!(true);
        response["wake_reason"] = json!("subscription_event");
        response["worker_subscription_id"] = notice["subscription_id"].clone();
        return Ok(response);
    }
    if record.windowed && status == Status::TimedOut {
        for result in &mut results {
            if result.status == Status::TimedOut {
                *result = TargetResult::pending(&result.target);
            }
        }
        return Ok(window_envelope(&record, Status::Pending, results, 0));
    }
    if record.windowed && status == Status::Ready {
        return Ok(window_envelope(
            &record,
            Status::Ready,
            results,
            u64::from(passive_window),
        ));
    }
    Ok(envelope(Some(&record.id), record.mode, status, results))
}

pub(super) async fn execute(
    broker: Arc<Broker>,
    user: String,
    token: String,
    args: Arguments,
) -> Result<Value, String> {
    let client_id = args.client_id.clone();
    let mut run =
        code_mode::AuditRun::new("await: typed passive observation", args.controller.clone())?;
    run.openai_context = args.openai_context.clone();
    let mut report = CodeRunReport {
        broker: broker.clone(),
        user: user.clone(),
        access_token: token.clone(),
        run: run.clone(),
        delivered: false,
    };
    let result = code_mode::AUDIT_RUN
        .scope(run.clone(), wait(broker, user, token, args))
        .await;
    report.finish(&result).await;
    let mut value = result?;
    if !response_authorized(&report, &client_id) {
        run.finish_wait_state(
            &client_id,
            Outcome::AuthorizationRevoked,
            "await_authorization_revoked_at_response",
        );
        withhold(&mut value);
    }
    let is_user_input = value["results"].as_array().is_some_and(|results| {
        results
            .iter()
            .any(|result| result["target"]["type"] == "user_input")
    });
    if is_user_input
        && let Some(messages) = value["results"][0]["data"]
            .as_object_mut()
            .and_then(|data| data.remove("replayed_user_messages"))
            .and_then(|messages| messages.as_array().cloned())
    {
        let receipt = value["results"][0]["data"].clone();
        let mut response = append_user_messages(tool_result(Ok(value)), messages);
        response["structuredContent"]["user_wait"] = json!([receipt]);
        return Ok(response);
    }
    // Passive object observation has no automatic inbox/question acknowledgement.
    // User input explicitly opts into the existing atomic outer delivery protocol.
    if !is_user_input {
        return Ok(finish_passive_response(&report, &client_id, value).await);
    }
    if value["status"] != "ready" {
        let response = if value.get("worker_subscription_id").is_some() {
            finish_passive_response(&report, &client_id, value).await
        } else {
            tool_result(Ok(value))
        };
        let response = commit_user_delivery(&report, || response);
        return Ok(user_wait::append(response, &run));
    }
    let windowed = value.get("cursor").is_some();
    let mut response = finish_user_response(&report, tool_result(Ok(value.clone()))).await;
    let mut receipt = run.user_wait_receipt(&client_id);
    // The authoritative bodies are in outer user_messages; never duplicate a
    // reservation or relabel it as committed delivery in the target result.
    if let Some(fields) = receipt.as_object_mut() {
        fields.remove("messages");
    }
    let status = match receipt["status"].as_str() {
        Some("received") => Status::Ready,
        Some("timed_out") => Status::TimedOut,
        Some("authorization_revoked" | "binding_invalid") => Status::Unauthorized,
        Some("cancelled" | "disconnected" | "superseded") => Status::Cancelled,
        Some("busy") => Status::Busy,
        _ => Status::Unavailable,
    };
    let status = if status == Status::TimedOut && windowed {
        Status::Pending
    } else {
        status
    };
    value["status"] = json!(status);
    value["continue_wait"] = json!(matches!(status, Status::Pending | Status::TimedOut));
    value["results"][0]["status"] = json!(status);
    value["results"][0]["data"] = receipt;
    if windowed && status == Status::Ready {
        if response["structuredContent"].get("replay_error").is_none() {
            value["cursor"] = json!(1);
        } else {
            value["replay_error"] = response["structuredContent"]["replay_error"].clone();
        }
        response["structuredContent"]["cursor"] = value["cursor"].clone();
    }
    for key in ["status", "continue_wait", "results"] {
        response["structuredContent"][key] = value[key].clone();
    }
    if let Some(first) = response["content"]
        .as_array_mut()
        .and_then(|parts| parts.first_mut())
    {
        *first = json!({"type":"text","text":value.to_string()});
    }
    Ok(user_wait::append(response, &run))
}

/// Passive observations still deliver earned tool summaries, without opting
/// into the independent user-message, question or native-inbox delivery paths.
async fn finish_passive_response(
    report: &CodeRunReport,
    client_id: &str,
    mut value: Value,
) -> Value {
    tool_summaries::collect(report).await;
    let eligible = eligible_user_response(report).await.unwrap_or_default();
    let response =
        super::prompt_hooks::append(report, tool_result(Ok(value.clone())), &eligible).await;
    let mut response =
        super::events_tool::worker_subscriptions::append(report, response, &eligible).await;
    if let Some(notices) = response.get("worker_events") {
        value["worker_events"] = notices.clone();
    }
    // The bounded summary, timer and worker probes can await; withhold observation data as well as
    // notices if its account, connection or binding changed during that await.
    if !response_events_authorized(report, client_id, &value).await
        || !response_authorized(report, client_id)
    {
        report.run.finish_wait_state(
            client_id,
            Outcome::AuthorizationRevoked,
            "await_authorization_revoked_at_response",
        );
        withhold(&mut value);
        response = tool_result(Ok(value));
    }
    tool_summaries::commit_response(report, response, eligible)
}

async fn response_events_authorized(
    report: &CodeRunReport,
    client_id: &str,
    value: &Value,
) -> bool {
    let mut subscriptions: Vec<_> = value["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|result| result["target"]["type"] == "event" && result.get("data").is_some())
        .filter_map(|result| result["target"]["subscription_id"].as_str())
        .collect();
    if let Some(id) = value["worker_subscription_id"].as_str() {
        subscriptions.push(id);
    }
    subscriptions.extend(
        value["worker_events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|notice| notice["subscription_id"].as_str()),
    );
    // Durable status/list receipts also recheck their declared subscriptions
    // after the final asynchronous summary probe, despite containing no bodies.
    for receipt in std::iter::once(value).chain(value["waits"].as_array().into_iter().flatten()) {
        subscriptions.extend(
            receipt["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|target| target["type"] == "event")
                .filter_map(|target| target["subscription_id"].as_str()),
        );
    }
    subscriptions.sort_unstable();
    subscriptions.dedup();
    if subscriptions.is_empty() {
        return true;
    }
    let Some(binding) = report.run.binding(client_id) else {
        return false;
    };
    for &subscription_id in &subscriptions {
        if super::events_tool::authorize_observation(
            &report.broker,
            &report.user,
            &report.access_token,
            client_id,
            &binding.agent_id,
            &binding.token,
            subscription_id,
        )
        .await
        .is_err()
        {
            return false;
        }
    }
    for subscription_id in subscriptions {
        if super::events_tool::authorize_observation_now(
            &report.broker,
            &report.user,
            &report.access_token,
            client_id,
            &binding.agent_id,
            &binding.token,
            subscription_id,
        )
        .is_err()
        {
            return false;
        }
    }
    true
}

fn response_authorized(report: &CodeRunReport, client_id: &str) -> bool {
    let Some(authorization) = report
        .broker
        .authenticate_now(&report.user, &report.access_token)
    else {
        return false;
    };
    let Some(session) = report.run.connection(client_id) else {
        return false;
    };
    let Some(binding) = report.run.binding(client_id) else {
        return false;
    };
    session.is_alive()
        && authorization.is_live()
        && authorization.permits(&session)
        && crate::bridge::owner(&report.user, &authorization).as_deref()
            == Ok(binding.owner.as_str())
        && session
            .collaboration
            .bind(&binding.owner, &binding.agent_id, &binding.token)
            .is_ok()
}

fn withhold(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("worker_events");
        object.remove("worker_subscription_id");
        object.remove("wake_reason");
    }
    value["status"] = json!("unauthorized");
    value["continue_wait"] = json!(false);
    if let Some(fields) = value.as_object_mut() {
        for key in ["waits", "metadata", "targets", "request_id"] {
            fields.remove(key);
        }
    }
    if let Some(results) = value["results"].as_array_mut() {
        for result in results {
            result["status"] = json!("unauthorized");
            if let Some(fields) = result.as_object_mut() {
                fields.remove("data");
            }
            result["error"] = json!({"code":"await_authorization_revoked","message":"observation withheld at the final response boundary"});
        }
    }
}

/// Runs at the common synchronous response boundary, including code_mode's
/// waitFor helper. The registry lock linearizes observer cancellation against
/// input commitment; no await occurs while the registry lock is held.
pub(super) fn commit_user_delivery(
    report: &CodeRunReport,
    commit: impl FnOnce() -> Value,
) -> Value {
    let records = report
        .broker
        .awaiting
        .records
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let current: Vec<_> = records
        .values()
        .filter(|record| {
            record.actor.user == report.user
                && record
                    .delivery_run
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_deref()
                    == Some(report.run.context.run_id.as_str())
                && report
                    .run
                    .binding(&record.actor.client_id)
                    .is_some_and(|binding| {
                        binding.owner == record.actor.owner
                            && binding.agent_id == record.actor.agent_id
                    })
                && report
                    .run
                    .connection(&record.actor.client_id)
                    .is_some_and(|session| {
                        Weak::ptr_eq(&record.actor.connection, &Arc::downgrade(&session))
                    })
        })
        .collect();
    for record in &current {
        if *record.cancelled.borrow() {
            report.run.finish_wait_state(
                &record.actor.client_id,
                Outcome::Cancelled,
                "await_cancelled_before_delivery",
            );
        }
    }
    let mut response = commit();
    for record in current {
        let receipt = report.run.user_wait_receipt(&record.actor.client_id);
        if record.windowed && receipt["status"] == "received" && !*record.cancelled.borrow() {
            retain_user_delivery(record, receipt.clone(), &mut response);
        }
        let retryable = matches!(receipt["status"].as_str(), Some("timed_out" | "busy"));
        record
            .terminal
            .store(*record.cancelled.borrow() || !retryable, Ordering::Release);
        if record.terminal.load(Ordering::Acquire) {
            record.transition(
                &report.broker,
                ObservationState::Completed,
                json!({"status":if *record.cancelled.borrow() {
                    "cancelled"
                } else {
                    receipt["status"].as_str().unwrap_or("unknown")
                }}),
            );
        }
        *record
            .delivery_run
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *record.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
    response
}

fn retain_user_delivery(record: &Record, mut receipt: Value, response: &mut Value) {
    let identities = receipt["messages"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let messages: Vec<Value> = response["structuredContent"]["user_messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| {
            message["client_id"] == record.actor.client_id
                && message["agent_id"] == record.actor.agent_id
                && identities.iter().any(|id| {
                    message["message_id"] == id["message_id"]
                        && message["revision"] == id["revision"]
                })
        })
        .cloned()
        .collect();
    if messages.is_empty() || messages.len() != identities.len() {
        record.snapshot_overflow.store(true, Ordering::Release);
        response["structuredContent"]["replay_error"] = json!("await_receipt_unavailable");
        return;
    }
    receipt["replayed_user_messages"] = json!(messages);
    let snapshot = vec![TargetResult {
        target: record.targets[0].clone(),
        status: Status::Ready,
        data: Some(receipt),
        error: None,
    }];
    if serde_json::to_vec(&snapshot).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES) {
        *record.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot);
        response["structuredContent"]["await_delivery_cursor"] =
            json!({"wait_id":record.id,"cursor":1});
    } else {
        record.snapshot_overflow.store(true, Ordering::Release);
        response["structuredContent"]["replay_error"] = json!("await_snapshot_overflow");
    }
}

/// A dropped HTTP/code_mode response cannot leave a protected reservation
/// record forever. CodeRunReport has already released its user reservations;
/// this only ends observers registered by that server-generated audit run.
pub(super) fn abandon_user_delivery(report: &CodeRunReport) {
    let records = report
        .broker
        .awaiting
        .records
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for record in records
        .values()
        .filter(|record| record.actor.user == report.user)
    {
        let mut delivery = record
            .delivery_run
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if delivery.as_deref() == Some(report.run.context.run_id.as_str()) {
            record.cancelled.send_replace(true);
            record.terminal.store(true, Ordering::Release);
            *delivery = None;
            record.transition(
                &report.broker,
                ObservationState::Completed,
                json!({"status":"cancelled","reason":"outer_response_dropped"}),
            );
        }
    }
}

pub(super) fn tool() -> Value {
    let targets = json!([
                {"type":"object","additionalProperties":false,"required":["type","job_id"],"properties":{"type":{"const":"exec"},"job_id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","id"],"properties":{"type":{"const":"job"},"id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","task_id"],"properties":{"type":{"const":"worker_result"},"task_id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","request_id"],"properties":{"type":{"const":"worker_assignment"},"request_id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","agent_id","statuses"],"properties":{"type":{"const":"agent_status"},"agent_id":{"type":"string","minLength":1,"maxLength":64},"statuses":{"type":"array","minItems":1,"maxItems":16,"items":crate::collab::lifecycle::status_schema()}}},
                {"type":"object","additionalProperties":false,"required":["type"],"properties":{"type":{"const":"agent_messages"},"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":64,"default":32}}},
                {"type":"object","additionalProperties":false,"required":["type","subscription_id"],"properties":{"type":{"const":"event"},"subscription_id":{"type":"string","minLength":1,"maxLength":128},"limit":{"type":"integer","minimum":1,"maximum":64,"default":32}}},
                {"type":"object","additionalProperties":false,"required":["type"],"properties":{"type":{"const":"user_input"},"reason":{"type":"string","minLength":1,"maxLength":512},"notify":{"type":"boolean","default":true}}},
    ]);
    json!({"name":"await","description":format!("Wait on typed execution jobs, relay jobs, worker results, agent status or inbox messages, event subscriptions, or user input without occupying a JavaScript worker. Authenticate the current participant with collab credentials. Create with targets; resume/cancel with the server-issued wait_id only. Read windows are bounded to 1000..{MAX_OBSERVATION_WAIT_MS}ms. For a server-owned passive observer, create with lifetime_ms (1000..86400000), request_id and window_ms. Same request_id with the same normalized targets/mode/lifetime returns the same wait_id even after a lost response; read budgets may change. HTTP/code_mode cancellation stops only that read window. Every terminal outcome is cached for at least 30 minutes with cursor 1; after:1 acknowledges only that frozen snapshot. op:list lists owned durable metadata and op:status reads metadata for one wait_id without reading or acknowledging bodies. Cancellation stops observation only: it never kills a job or acknowledges messages or event subscriptions. any returns when one target is ready; all requires all ready; explicit target failures return immediately with every target represented. Unknown or unavailable outcomes never restart a target. Worker targets require window_ms; worker_assignment and user_input must each be single-target waits and cannot use lifetime_ms. Only outer user_wait received plus matching user_messages proves user-input delivery."),
        "inputSchema":{"type":"object","additionalProperties":false,"required":["client_id"],"properties":{
            "client_id":{"type":"string","minLength":1,"maxLength":64},"op":{"enum":["wait","cancel","list","status"],"default":"wait"},
            "wait_id":{"type":"string","minLength":1,"maxLength":128},"mode":{"enum":["any","all"],"default":"any"},
            "timeout_ms":{"type":"integer","minimum":1000,"maximum":MAX_OBSERVATION_WAIT_MS,"default":20000},
            "window_ms":{"type":"integer","minimum":1000,"maximum":MAX_OBSERVATION_WAIT_MS,"description":"Opt into resumable observation windows of up to 55 seconds. Window expiry returns pending; targets are not cancelled."},
            "lifetime_ms":{"type":"integer","minimum":1000,"maximum":crate::wire::wait_display::MAX_LIFETIME_MS,"description":"On creation only, keep one server-owned passive observer alive beyond caller cancellation; requires request_id and window_ms. Never accepted for user_input or worker_assignment."},
            "request_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$","description":"Required with lifetime_ms for idempotent creation, private to this account/member/native connection. Reuse unchanged on a lost create response; changed targets/mode/lifetime are rejected."},
            "after":{"type":"integer","minimum":0,"maximum":1,"description":"On a resume, 0 retries the retained result and 1 acknowledges only this await snapshot. Inbox messages and event subscriptions require their own explicit acknowledgement."},
            "agent":{"type":"object","additionalProperties":false,"required":["id","token"],"properties":{"id":{"type":"string","minLength":1,"maxLength":64},"token":{"type":"string","minLength":64,"maxLength":64}}},
            "targets":{"type":"array","minItems":1,"maxItems":8,"items":{"oneOf":targets}}},"oneOf":[{"properties":{"op":{"const":"wait"}},"required":["targets"],"not":{"required":["wait_id"]}},
                {"properties":{"op":{"enum":["wait","cancel","status"]}},"required":["wait_id"],"not":{"anyOf":[{"required":["targets"]},{"required":["mode"]},{"required":["lifetime_ms"]},{"required":["request_id"]}]}},
                {"properties":{"op":{"const":"list"}},"required":["op"],"not":{"anyOf":[{"required":["wait_id"]},{"required":["targets"]},{"required":["mode"]},{"required":["window_ms"]},{"required":["after"]},{"required":["lifetime_ms"]},{"required":["request_id"]}]}}]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;
