//! Typed, bounded observation outside the JavaScript worker pool.
//! Cancelling an observer never cancels its job, acknowledges its inbox, or
//! changes the observed work. Completion approval targets are retired.
use super::*;
use crate::code_mode::user_wait::{Outcome, Recipient};
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
const MAX_RECORDS: usize = 256;
const MAX_ACTOR_RECORDS: usize = 32;
const MAX_ACTIVE_PER_MEMBER: usize = 4;
const MAX_ACTIVE_PER_CONNECTION: usize = 32;
const RETAIN_FOR: Duration = Duration::from_secs(300);

pub(crate) fn limits() -> Value {
    json!({"public_tool":"await","code_mode_helper":"waitFor","max_targets":MAX_TARGETS,
        "min_wait_ms":1000,"max_wait_ms":25000,"max_records":MAX_RECORDS,
        "max_records_per_actor":MAX_ACTOR_RECORDS,"retention_ms":RETAIN_FOR.as_millis(),
        "max_active_per_member":MAX_ACTIVE_PER_MEMBER,"max_active_per_connection":MAX_ACTIVE_PER_CONNECTION,
        "modes":["any","all"],"targets":["exec","job","agent_status","agent_messages","user_input"],
        "server_issued_wait_id":true,"cancel_changes_target":false,"acknowledges_inbox":false,
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
        if !identifier(&args.client_id, 64) || !(1_000..=25_000).contains(&args.timeout_ms) {
            return Err(
                "await client_id must come from list; timeout_ms must be 1000..=25000".into(),
            );
        }
        if args
            .wait_id
            .as_deref()
            .is_some_and(|id| !identifier(id, 128))
        {
            return Err("wait_id must be the server-issued ID from a previous await".into());
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
            (Some(_), None, _) if args.mode.is_none() => {},
            (None, Some(targets), Op::Wait) if (1..=MAX_TARGETS).contains(&targets.len()) => {
                for (index, target) in targets.iter().enumerate() {
                    if targets[..index].contains(target) {
                        return Err("await targets must be distinct".into());
                    }
                    match target {
                        Target::Exec { job_id } if !identifier(job_id, 128) => return Err("invalid execution job_id".into()),
                        Target::Job { id } if !identifier(id, 128) => return Err("invalid relay job id".into()),
                        Target::Finish { .. } => return Err("tool_retired: await finish targets are disabled".into()),
                        Target::AgentStatus { agent_id, statuses } if !identifier(agent_id, 64)
                            || statuses.is_empty() || statuses.len() > 16
                            || statuses.iter().any(|s| s.trim().is_empty() || s.len() > 128 || s.chars().any(char::is_control)) => return Err("agent_status requires a registered agent_id and 1..=16 status strings".into()),
                        Target::AgentMessages { after, limit } if *limit == 0 || *limit > 64
                            || after.is_some_and(|cursor| cursor > (1_u64 << 53) - 1) => return Err("agent_messages requires limit 1..=64 and a valid receive cursor".into()),
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
        self.user == other.user
            && self.owner == other.owner
            && self.client_id == other.client_id
            && self.agent_id == other.agent_id
            && Weak::ptr_eq(&self.connection, &other.connection)
    }
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
}

impl Record {
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
        records.retain(|_, record| {
            record.active.load(Ordering::Acquire)
                || record.awaiting_delivery()
                || now.duration_since(*record.touched.lock().unwrap_or_else(|e| e.into_inner()))
                    < RETAIN_FOR
        });
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
        let at_capacity = |records: &HashMap<String, Arc<Record>>| {
            records.len() >= MAX_RECORDS
                || records
                    .values()
                    .filter(|record| record.actor.same(&actor))
                    .count()
                    >= MAX_ACTOR_RECORDS
        };
        if at_capacity(&records) {
            let actor_full = records
                .values()
                .filter(|record| record.actor.same(&actor))
                .count()
                >= MAX_ACTOR_RECORDS;
            let victim = records
                .values()
                .filter(|record| {
                    !record.active.load(Ordering::Acquire)
                        && !record.awaiting_delivery()
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
            let id = format!(
                "await_{}",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            );
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
        });
        records.insert(id, record.clone());
        Ok(record)
    }

    fn activate(&self, record: &Arc<Record>) -> Result<Active, (Status, &'static str)> {
        let records = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if record.active.load(Ordering::Acquire) {
            return Err((Status::Busy, "await_already_waiting"));
        }
        let active = records
            .values()
            .filter(|other| other.active.load(Ordering::Acquire));
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
            .call_with_authorization(
                Operation::CallTool {
                    name: name.into(),
                    arguments,
                },
                controller,
                &authorization,
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

async fn observe(
    scope: &Scope,
    target: &Target,
    timeout_ms: u64,
    controller: Option<Controller>,
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
                Some(&scope.user_wait_admitted),
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
    let mut results: Vec<_> = record.targets.iter().map(TargetResult::pending).collect();
    if args.op == Op::Cancel {
        broker.awaiting.cancel(&record);
    }
    if *record.cancelled.borrow() {
        for result in &mut results {
            result.status = Status::Cancelled;
        }
        return Ok(envelope(
            Some(&record.id),
            record.mode,
            Status::Cancelled,
            results,
        ));
    }
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
    // Presentation is best effort and follows admission. It owns no native
    // slot, user-input reservation, target result or finish permission.
    let _display = scope.session.show_wait(
        &scope.broker,
        crate::broker::wait_display::Request {
            user: &scope.user,
            token: &scope.token,
            binding: &scope.binding,
            wait_id: &record.id,
            targets: record
                .targets
                .iter()
                .map(|target| match target {
                    Target::Exec { .. } => crate::wire::wait_display::Kind::Exec,
                    Target::Job { .. } => crate::wire::wait_display::Kind::Job,
                    Target::AgentStatus { .. } => crate::wire::wait_display::Kind::AgentStatus,
                    Target::AgentMessages { .. } => crate::wire::wait_display::Kind::AgentMessages,
                    Target::UserInput { .. } => crate::wire::wait_display::Kind::UserInput,
                    Target::Finish { .. } => crate::wire::wait_display::Kind::Finish,
                })
                .collect(),
            timeout_ms: args.timeout_ms,
        },
    );
    let mut cancelled = record.cancelled.subscribe();
    let mut pending = FuturesUnordered::new();
    // Leave a small transport margin so native bounded waits can return their
    // actual final state before the outer deadline closes the observation.
    let target_timeout = args.timeout_ms.saturating_sub(100).max(1_000);
    for (index, target) in record.targets.iter().enumerate() {
        let scope = &scope;
        let controller = args.controller.clone();
        pending.push(async move {
            (
                index,
                observe(scope, target, target_timeout, controller).await,
            )
        });
    }
    let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
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
            _ = tokio::time::sleep_until(deadline) => break Status::TimedOut,
            _ = authority.tick() => {
                if user_wait::authorized(&scope.broker, &scope.user, &scope.token,
                    scope.session.client_id(), &scope.session, &scope.recipient).await.is_err() { break Status::Unauthorized; }
            },
        }
    };
    drop(pending);
    let status = if scope.final_authorization().is_err() {
        Status::Unauthorized
    } else if *record.cancelled.borrow() {
        Status::Cancelled
    } else {
        status
    };
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
    if !matches!(status, Status::TimedOut | Status::Busy) {
        record.terminal.store(true, Ordering::Release);
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
    // Passive object observation has no automatic inbox/question acknowledgement.
    // User input explicitly opts into the existing atomic outer delivery protocol.
    if !is_user_input {
        return Ok(finish_passive_response(&report, &client_id, value).await);
    }
    if value["status"] != "ready" {
        let response = commit_user_delivery(&report, || tool_result(Ok(value)));
        return Ok(user_wait::append(response, &run));
    }
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
    value["status"] = json!(status);
    value["continue_wait"] = json!(status == Status::TimedOut);
    value["results"][0]["status"] = json!(status);
    value["results"][0]["data"] = receipt;
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
    // The bounded summary probe can await; withhold observation data as well as
    // notices if its account, connection or binding changed during that await.
    if !response_authorized(report, client_id) {
        report.run.finish_wait_state(
            client_id,
            Outcome::AuthorizationRevoked,
            "await_authorization_revoked_at_response",
        );
        withhold(&mut value);
    }
    tool_summaries::commit_response(report, tool_result(Ok(value)), eligible)
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
    value["status"] = json!("unauthorized");
    value["continue_wait"] = json!(false);
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
    let response = commit();
    for record in current {
        let receipt = report.run.user_wait_receipt(&record.actor.client_id);
        let retryable = matches!(receipt["status"].as_str(), Some("timed_out" | "busy"));
        record
            .terminal
            .store(*record.cancelled.borrow() || !retryable, Ordering::Release);
        *record
            .delivery_run
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *record.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }
    response
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
        }
    }
}

pub(super) fn tool() -> Value {
    json!({"name":"await","description":"Wait on typed execution jobs, relay jobs, agent status or inbox messages, or user input without occupying a JavaScript worker. Authenticate the current participant with collab credentials. Create with targets; resume/cancel with the server-issued wait_id only. Timeout is bounded to 1000..25000ms. Cancellation stops observation only: it never kills a job or acknowledges messages. any returns when one target is ready; all requires all ready; explicit target failures return immediately with every target represented. Pending entries in an any response were not resolved. user_input must be a single-target wait. Only outer user_wait received plus matching user_messages proves user-input delivery.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["client_id"],"properties":{
            "client_id":{"type":"string","minLength":1,"maxLength":64},"op":{"enum":["wait","cancel"],"default":"wait"},
            "wait_id":{"type":"string","minLength":1,"maxLength":128},"mode":{"enum":["any","all"],"default":"any"},
            "timeout_ms":{"type":"integer","minimum":1000,"maximum":25000,"default":20000},
            "agent":{"type":"object","additionalProperties":false,"required":["id","token"],"properties":{"id":{"type":"string","minLength":1,"maxLength":64},"token":{"type":"string","minLength":64,"maxLength":64}}},
            "targets":{"type":"array","minItems":1,"maxItems":8,"items":{"oneOf":[
                {"type":"object","additionalProperties":false,"required":["type","job_id"],"properties":{"type":{"const":"exec"},"job_id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","id"],"properties":{"type":{"const":"job"},"id":{"type":"string","minLength":1,"maxLength":128}}},
                {"type":"object","additionalProperties":false,"required":["type","agent_id","statuses"],"properties":{"type":{"const":"agent_status"},"agent_id":{"type":"string","minLength":1,"maxLength":64},"statuses":{"type":"array","minItems":1,"maxItems":16,"items":{"type":"string","minLength":1,"maxLength":128}}}},
                {"type":"object","additionalProperties":false,"required":["type"],"properties":{"type":{"const":"agent_messages"},"after":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":64,"default":32}}},
                {"type":"object","additionalProperties":false,"required":["type"],"properties":{"type":{"const":"user_input"},"reason":{"type":"string","minLength":1,"maxLength":512},"notify":{"type":"boolean","default":true}}},
            ]}}},"oneOf":[{"properties":{"op":{"const":"wait"}},"required":["targets"],"not":{"required":["wait_id"]}},
                {"required":["wait_id"],"not":{"anyOf":[{"required":["targets"]},{"required":["mode"]}]}}]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;
