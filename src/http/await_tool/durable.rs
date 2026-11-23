//! Server-owned passive observers. Caller futures own only bounded read windows;
//! they cannot restart targets or drop the observer that owns the frozen result.
use super::*;
use crate::wire::wait_display::{DurableObservation, DurableState, DurableStatus};
use std::sync::atomic::AtomicUsize;

pub(super) const RETAIN_FOR: Duration = Duration::from_secs(30 * 60);

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn definition(args: &Arguments) -> String {
    let mut targets = args.targets.clone().expect("validated durable creation");
    for target in &mut targets {
        if let Target::AgentStatus { statuses, .. } = target {
            statuses.sort();
            statuses.dedup();
        }
    }
    let mut targets: Vec<_> = targets
        .iter()
        .map(|target| serde_json::to_string(target).expect("typed target"))
        .collect();
    targets.sort();
    json!({"targets":targets,"mode":args.mode.unwrap_or_default(),"lifetime_ms":args.lifetime_ms})
        .to_string()
}

pub(super) struct Observation {
    pub(super) request_id: String,
    definition: String,
    started: Instant,
    lifetime_ms: u64,
    launched: AtomicBool,
    readers: AtomicUsize,
    completed: watch::Sender<bool>,
    state: Mutex<State>,
    display: Mutex<Option<crate::broker::wait_display::Lease>>,
}

struct State {
    metadata: DurableObservation,
    completed: Option<Instant>,
    results: Option<Vec<TargetResult>>,
    status: Option<Status>,
}

impl Observation {
    pub(super) fn new(args: &Arguments) -> Self {
        let lifetime_ms = args.lifetime_ms.expect("durable lifetime");
        Self {
            request_id: args.request_id.clone().expect("durable request ID"),
            definition: definition(args),
            started: Instant::now(),
            lifetime_ms,
            launched: AtomicBool::new(false),
            readers: AtomicUsize::new(0),
            completed: watch::channel(false).0,
            state: Mutex::new(State {
                metadata: DurableObservation {
                    state: DurableState::Waiting,
                    lifetime_ms,
                    read_window_ms: args.window_ms.expect("durable read window"),
                    started_at_ms: wall_ms(),
                    completed_at_ms: None,
                    first_read_at_ms: None,
                    last_read_at_ms: None,
                    acknowledged_at_ms: None,
                    retained_until_ms: None,
                    elapsed_total_ms: 0,
                    delivery_lag_ms: None,
                    read_windows: 0,
                    replay_count: 0,
                    cursor: 0,
                    result_status: None,
                },
                completed: None,
                results: None,
                status: None,
            }),
            display: Mutex::new(None),
        }
    }

    pub(super) fn matches(&self, args: &Arguments) -> bool {
        self.definition == definition(args)
    }

    pub(super) fn retained(&self, now: Instant) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .completed
            .is_none_or(|completed| now.saturating_duration_since(completed) < RETAIN_FOR)
    }

    pub(super) fn metadata(&self) -> DurableObservation {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut metadata = state.metadata.clone();
        if state.completed.is_none() {
            metadata.elapsed_total_ms =
                millis(Instant::now().saturating_duration_since(self.started));
        }
        metadata
    }

    fn refresh_display(&self) {
        // Serialize projections before sampling state so an older read cannot
        // overwrite a newer completion/ACK. No state lock enters the hub.
        if let Some(display) = self
            .display
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            display.update_durable(self.metadata());
        }
    }

    fn attach_display(&self, lease: Option<crate::broker::wait_display::Lease>) {
        *self.display.lock().unwrap_or_else(|e| e.into_inner()) = lease;
        // A concurrent cancel/read may have advanced metadata during setup.
        self.refresh_display();
    }

    fn note_window(&self, window_ms: u64) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.metadata.read_window_ms = window_ms;
            state.metadata.read_windows = state.metadata.read_windows.saturating_add(1);
        }
        self.refresh_display();
    }
}

fn wire_status(status: Status) -> DurableStatus {
    match status {
        Status::Ready => DurableStatus::Ready,
        Status::TimedOut => DurableStatus::TimedOut,
        Status::Cancelled => DurableStatus::Cancelled,
        Status::Unauthorized => DurableStatus::Unauthorized,
        Status::UnknownTarget => DurableStatus::UnknownTarget,
        Status::Busy => DurableStatus::Busy,
        Status::Pending | Status::Unavailable => DurableStatus::Unavailable,
    }
}

fn failure_results(
    record: &Record,
    status: Status,
    code: &'static str,
    message: &str,
) -> Vec<TargetResult> {
    record
        .targets
        .iter()
        .map(|target| TargetResult::failed(target, status, code, message))
        .collect()
}

/// The state mutex is the one completion/cancellation/ACK linearization point.
/// A terminal result is immutable, including errors, deadlines and cancellation.
fn finish(broker: &Broker, record: &Record, mut status: Status, mut results: Vec<TargetResult>) {
    let durable = record.durable.as_ref().expect("durable record");
    {
        let mut state = durable.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.completed.is_some() {
            return;
        }
        if !serde_json::to_vec(&results).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES) {
            status = Status::Unavailable;
            results = failure_results(
                record,
                status,
                "await_snapshot_overflow",
                "terminal result exceeded the bounded cache; no partial body was retained",
            );
        }
        let now = Instant::now();
        let timestamp = wall_ms();
        state.completed = Some(now);
        state.status = Some(status);
        state.results = Some(results);
        state.metadata.state = DurableState::Cached;
        state.metadata.completed_at_ms = Some(timestamp);
        state.metadata.retained_until_ms = Some(timestamp.saturating_add(millis(RETAIN_FOR)));
        state.metadata.elapsed_total_ms = millis(now.saturating_duration_since(durable.started));
        state.metadata.cursor = 1;
        state.metadata.result_status = Some(wire_status(status));
        record.terminal.store(true, Ordering::Release);
    }
    record.transition(broker, ObservationState::Resumed, json!({"status":status}));
    record.transition(
        broker,
        ObservationState::Completed,
        json!({"status":status}),
    );
    durable.refresh_display();
    durable.completed.send_replace(true);
}

fn metadata_response(record: &Record) -> Value {
    let durable = record.durable.as_ref().expect("durable record");
    let metadata = durable.metadata();
    let status = match metadata.result_status {
        None => Status::Pending,
        Some(DurableStatus::Ready) => Status::Ready,
        Some(DurableStatus::TimedOut) => Status::TimedOut,
        Some(DurableStatus::Cancelled) => Status::Cancelled,
        Some(DurableStatus::Unauthorized) => Status::Unauthorized,
        Some(DurableStatus::UnknownTarget) => Status::UnknownTarget,
        Some(DurableStatus::Unavailable) => Status::Unavailable,
        Some(DurableStatus::Busy) => Status::Busy,
    };
    let mut value = window_envelope(record, status, Vec::new(), metadata.cursor);
    value["request_id"] = json!(durable.request_id);
    value["targets"] = json!(record.targets);
    value["metadata"] = json!(metadata);
    value["metadata_only"] = json!(true);
    value
}

fn authorized_now(scope: &Scope, targets: &[Target]) -> Result<(), String> {
    for target in targets {
        if let Target::Event {
            subscription_id, ..
        } = target
        {
            super::super::events_tool::authorize_observation_now(
                &scope.broker,
                &scope.user,
                &scope.token,
                scope.session.client_id(),
                &scope.binding.agent_id,
                &scope.binding.token,
                subscription_id,
            )?;
        }
    }
    scope.final_authorization()
}

/// Metadata discovery is bounded, scoped to the exact original connection and
/// never acknowledges or reveals a cached body. Revoked subscriptions disappear.
pub(super) async fn list(scope: &Scope) -> Result<Value, String> {
    let actor = scope.actor();
    let records: Vec<_> = {
        let mut records = scope
            .broker
            .awaiting
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        records.retain(|_, record| record.retained(Instant::now()));
        records
            .values()
            .filter(|record| record.durable.is_some() && record.actor.same(&actor))
            .cloned()
            .collect()
    };
    let mut entries = Vec::new();
    let mut authorized = Vec::new();
    for record in records {
        if scope.authorize_events(&record.targets).await.is_ok() {
            authorized.push(record);
        }
    }
    // Recheck all sources after the last asynchronous authorization boundary.
    for record in authorized {
        if authorized_now(scope, &record.targets).is_ok() {
            entries.push(metadata_response(&record));
        }
    }
    scope.final_authorization()?;
    entries.sort_by(|left, right| left["wait_id"].as_str().cmp(&right["wait_id"].as_str()));
    Ok(
        json!({"schema_version":1,"op":"list","waits":entries,"metadata_only":true,"completion_permission":null}),
    )
}

/// Read/ACK is called only after asynchronous AND synchronous authorization.
fn snapshot(record: &Record, after: Option<u64>) -> Result<Value, String> {
    let durable = record.durable.as_ref().expect("durable record");
    let mut state = durable.state.lock().unwrap_or_else(|e| e.into_inner());
    let Some(completed) = state.completed else {
        if after == Some(1) {
            return Err("await cursor is ahead of the retained result".into());
        }
        drop(state);
        return Ok(metadata_response(record));
    };
    let status = state.status.expect("terminal status");
    let acknowledged = state.metadata.state == DurableState::Acknowledged;
    let results = if after == Some(1) {
        state.results.take();
        state.metadata.state = DurableState::Acknowledged;
        state
            .metadata
            .acknowledged_at_ms
            .get_or_insert_with(wall_ms);
        Vec::new()
    } else if acknowledged {
        return Err(
            "await result cursor expired after acknowledgement; use status or repeat after:1"
                .into(),
        );
    } else {
        let timestamp = wall_ms();
        if state.metadata.first_read_at_ms.is_none() {
            state.metadata.first_read_at_ms = Some(timestamp);
            state.metadata.delivery_lag_ms =
                Some(millis(Instant::now().saturating_duration_since(completed)));
        } else {
            state.metadata.replay_count = state.metadata.replay_count.saturating_add(1);
        }
        state.metadata.last_read_at_ms = Some(timestamp);
        state
            .results
            .clone()
            .expect("unacknowledged terminal snapshot")
    };
    let mut value = window_envelope(record, status, results, 1);
    value["request_id"] = json!(durable.request_id);
    value["targets"] = json!(record.targets);
    value["metadata"] = json!(state.metadata);
    value["acknowledged"] = json!(state.metadata.state == DurableState::Acknowledged);
    drop(state);
    durable.refresh_display();
    Ok(value)
}

struct Reader(Arc<Record>);
impl Drop for Reader {
    fn drop(&mut self) {
        self.0
            .durable
            .as_ref()
            .expect("durable reader")
            .readers
            .fetch_sub(1, Ordering::AcqRel);
    }
}

fn reader(scope: &Scope, record: &Arc<Record>) -> Result<Reader, String> {
    let records = scope
        .broker
        .awaiting
        .records
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let count = |other: &Record| {
        other
            .durable
            .as_ref()
            .map_or(0, |durable| durable.readers.load(Ordering::Acquire))
    };
    let member: usize = records
        .values()
        .filter(|other| other.actor.same_identity(&record.actor))
        .map(|record| count(record))
        .sum();
    let connection: usize = records
        .values()
        .filter(|other| Weak::ptr_eq(&other.actor.connection, &record.actor.connection))
        .map(|record| count(record))
        .sum();
    if member >= MAX_ACTIVE_PER_MEMBER || connection >= MAX_ACTIVE_PER_CONNECTION {
        return Err("await_read_concurrency_full".into());
    }
    record
        .durable
        .as_ref()
        .expect("durable reader")
        .readers
        .fetch_add(1, Ordering::AcqRel);
    Ok(Reader(record.clone()))
}

pub(super) async fn wait(
    scope: Scope,
    record: Arc<Record>,
    args: Arguments,
) -> Result<Value, String> {
    let durable = record.durable.as_ref().expect("durable record");
    // No await separates registry creation from spawning its owner. Dropping a
    // lost first HTTP response cannot leave a never-started idempotency record.
    if args.op == Op::Wait {
        start(&scope, &record, args.controller.clone());
    }
    if let Err(error) = scope.authorize_events(&record.targets).await {
        return Ok(window_envelope(
            &record,
            Status::Unauthorized,
            failure_results(
                &record,
                Status::Unauthorized,
                "await_authorization_revoked",
                &error,
            ),
            u64::from(record.terminal.load(Ordering::Acquire)),
        ));
    }
    if args.op == Op::Status {
        authorized_now(&scope, &record.targets)?;
        return Ok(metadata_response(&record));
    }
    if args.op == Op::Cancel {
        authorized_now(&scope, &record.targets)?;
        finish(
            &scope.broker,
            &record,
            Status::Cancelled,
            failure_results(
                &record,
                Status::Cancelled,
                "await_cancelled",
                "observer cancelled; targets were not changed",
            ),
        );
        record.cancelled.send_replace(true);
        return Ok(metadata_response(&record));
    }
    let window_ms = args.window_ms.unwrap_or(args.timeout_ms);
    durable.note_window(window_ms);
    if record.terminal.load(Ordering::Acquire) || args.after == Some(1) {
        authorized_now(&scope, &record.targets)?;
        if args.request_id.is_some() && durable.metadata().state == DurableState::Acknowledged {
            return Ok(metadata_response(&record));
        }
        return snapshot(&record, args.after);
    }
    let _reader = match reader(&scope, &record) {
        Ok(reader) => reader,
        Err(error) => {
            let mut response = metadata_response(&record);
            response["status"] = json!(Status::Busy);
            response["error"] = json!({"code":"await_read_concurrency_full","message":error});
            return Ok(response);
        }
    };
    let mut completed = durable.completed.subscribe();
    let done = *completed.borrow_and_update();
    let mut worker_notice = None;
    if !done {
        let completion =
            tokio::time::timeout(Duration::from_millis(window_ms), completed.changed());
        tokio::pin!(completion);
        let workers = super::super::events_tool::worker_subscriptions::wait_for_notice(
            &scope.broker,
            &scope.user,
            &scope.token,
            &scope.session,
            &scope.binding,
            window_ms,
            Some(&record.id),
        );
        tokio::pin!(workers);
        tokio::select! {
            biased;
            _ = &mut completion => {},
            notice = &mut workers => {
                if notice.is_some() {
                    worker_notice = notice;
                } else {
                    let _ = completion.await;
                }
            }
        }
    }
    if let Err(error) = scope.authorize_events(&record.targets).await {
        return Ok(window_envelope(
            &record,
            Status::Unauthorized,
            failure_results(
                &record,
                Status::Unauthorized,
                "await_authorization_revoked",
                &error,
            ),
            u64::from(record.terminal.load(Ordering::Acquire)),
        ));
    }
    authorized_now(&scope, &record.targets)?;
    let mut response = snapshot(&record, args.after)?;
    if let Some(notice) = worker_notice {
        // Only the caller's read window wakes. The detached observer and its
        // original target readiness, lifetime and acknowledgement remain intact.
        response["wake_reason"] = json!("subscription_event");
        response["worker_subscription_id"] = notice["subscription_id"].clone();
    }
    Ok(response)
}

fn detached_scope(scope: &Scope) -> Result<Scope, String> {
    let mut run = code_mode::AuditRun::new(
        "await: durable passive observer",
        scope.run.controller.clone(),
    )?;
    run.openai_context = scope.run.openai_context.clone();
    run.record_connection(&scope.session)?;
    run.bind(
        scope.session.client_id(),
        &scope.binding.owner,
        &scope.binding.agent_id,
        &scope.binding.token,
    )?;
    Ok(Scope {
        broker: scope.broker.clone(),
        user: scope.user.clone(),
        token: scope.token.clone(),
        session: scope.session.clone(),
        recipient: Recipient {
            owner: scope.recipient.owner.clone(),
            binding: Some(scope.binding.clone()),
            connection: Arc::downgrade(&scope.session),
        },
        binding: scope.binding.clone(),
        run,
        user_wait_admitted: AtomicBool::new(false),
    })
}

struct Observer {
    scope: Arc<Scope>,
    record: Arc<Record>,
    _active: Active,
}

impl Drop for Observer {
    fn drop(&mut self) {
        finish(
            &self.scope.broker,
            &self.record,
            Status::Unavailable,
            failure_results(
                &self.record,
                Status::Unavailable,
                "await_observer_lost",
                "observer ended without a determinate result; no target was replayed",
            ),
        );
        // This run is independent of every HTTP/code_mode read. It observes only
        // passive targets, so it owns no user-input reservation or inbox ACK.
        let value = metadata_response(&self.record);
        let completion = self.scope.run.completion(&Ok(value));
        if self.scope.final_authorization().is_ok()
            && self
                .scope
                .run
                .targets()
                .iter()
                .any(|id| id == self.scope.session.client_id())
        {
            let _ = self.scope.session.report_code_mode(completion);
        }
    }
}

fn start(scope: &Scope, record: &Arc<Record>, controller: Option<Controller>) {
    let durable = record.durable.as_ref().expect("durable record");
    if durable.launched.swap(true, Ordering::AcqRel) {
        return;
    }
    let active = match scope.broker.awaiting.activate(record) {
        Ok(active) => active,
        Err((status, code)) => {
            finish(
                &scope.broker,
                record,
                status,
                failure_results(record, status, code, "durable observer admission failed"),
            );
            return;
        }
    };
    let scope = match detached_scope(scope) {
        Ok(scope) => Arc::new(scope),
        Err(error) => {
            finish(
                &scope.broker,
                record,
                Status::Unavailable,
                failure_results(
                    record,
                    Status::Unavailable,
                    "await_audit_unavailable",
                    &error,
                ),
            );
            return;
        }
    };
    let observer = Observer {
        scope: scope.clone(),
        record: record.clone(),
        _active: active,
    };
    let run = scope.run.clone();
    tokio::spawn(code_mode::AUDIT_RUN.scope(run, async move {
        observe_lifetime(&observer, controller).await;
    }));
}

fn expired_results(record: &Record) -> Vec<TargetResult> {
    let durable = record.durable.as_ref().expect("durable record");
    let previous = {
        let mut state = durable.state.lock().unwrap_or_else(|e| e.into_inner());
        // A competing completion/ACK owns its frozen body.
        if state.completed.is_some() {
            None
        } else {
            state.results.take()
        }
    };
    previous
        .unwrap_or_else(|| record.targets.iter().map(TargetResult::pending).collect())
        .into_iter()
        .map(|result| {
            if result.status == Status::Ready {
                result
            } else {
                TargetResult::failed(
                    &result.target,
                    Status::TimedOut,
                    "await_lifetime_expired",
                    "observer lifetime expired; target state was not changed",
                )
            }
        })
        .collect()
}

async fn observe_lifetime(observer: &Observer, controller: Option<Controller>) {
    let scope = &observer.scope;
    let record = &observer.record;
    let durable = record.durable.as_ref().expect("durable record");
    let deadline = durable.started + Duration::from_millis(durable.lifetime_ms);
    record.transition(
        &scope.broker,
        ObservationState::Suspended,
        json!({"lifetime_ms":durable.lifetime_ms,"mode":record.mode,"durable":true}),
    );
    let mut cancelled = record.cancelled.subscribe();
    let cancelled_at_start = *cancelled.borrow();
    let (mut status, mut results) = if cancelled_at_start {
        (
            Status::Cancelled,
            failure_results(
                record,
                Status::Cancelled,
                "await_cancelled",
                "observer cancelled",
            ),
        )
    } else {
        tokio::select! {
            biased;
            _ = cancelled.changed() => (Status::Cancelled, failure_results(record, Status::Cancelled, "await_cancelled", "observer cancelled")),
            _ = tokio::time::sleep_until(deadline) => (Status::TimedOut, expired_results(record)),
            result = observe_targets(scope, record, controller, deadline) => result,
        }
    };
    if let Err(error) = authorized_now(scope, &record.targets) {
        status = Status::Unauthorized;
        results = failure_results(record, status, "await_authorization_revoked", &error);
    }
    finish(&scope.broker, record, status, results);
}

async fn observe_targets(
    scope: &Scope,
    record: &Record,
    controller: Option<Controller>,
    deadline: Instant,
) -> (Status, Vec<TargetResult>) {
    if let Err(error) = scope.authorize_events(&record.targets).await {
        return (
            Status::Unauthorized,
            failure_results(
                record,
                Status::Unauthorized,
                "await_authorization_revoked",
                &error,
            ),
        );
    }
    let durable = record.durable.as_ref().expect("durable record");
    let (target_labels, target_details) = display_targets(scope, &record.targets).await;
    let lease = scope.session.show_durable_wait(
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
                    Target::Job { .. } | Target::WorkerResult { .. } => {
                        crate::wire::wait_display::Kind::Job
                    }
                    Target::AgentStatus { .. } => crate::wire::wait_display::Kind::AgentStatus,
                    Target::AgentMessages { .. } => crate::wire::wait_display::Kind::AgentMessages,
                    Target::Event { .. } => crate::wire::wait_display::Kind::Event,
                    _ => unreachable!("validated passive targets"),
                })
                .collect(),
            target_labels,
            target_details,
            mode: match record.mode {
                Mode::Any => crate::wire::wait_display::Mode::Any,
                Mode::All => crate::wire::wait_display::Mode::All,
            },
            timeout_ms: durable.lifetime_ms,
        },
        durable.metadata(),
    );
    durable.attach_display(lease);
    let mut results: Vec<_> = record.targets.iter().map(TargetResult::pending).collect();
    let mut pending = FuturesUnordered::new();
    for (index, target) in record.targets.iter().enumerate() {
        let controller = controller.clone();
        pending.push(async move {
            (
                index,
                observe_until(scope, target, controller, &record.id, deadline).await,
            )
        });
    }
    let mut authority = tokio::time::interval(Duration::from_secs(1));
    authority.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            result = pending.next() => {
                let Some((index, result)) = result else { return (Status::TimedOut, results); };
                results[index] = result;
                if !serde_json::to_vec(&results).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES) {
                    return (Status::Unavailable, failure_results(record, Status::Unavailable, "await_snapshot_overflow", "observation exceeded the bounded cache; no partial body was retained"));
                }
                if let Some(status) = resolved(record.mode, &results) { return (status, results); }
                {
                    let mut state = durable.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.completed.is_none() { state.results = Some(results.clone()); }
                }
            },
            _ = authority.tick() => {
                if let Err(error) = authorized_now(scope, &record.targets) {
                    return (Status::Unauthorized, failure_results(record, Status::Unauthorized, "await_authorization_revoked", &error));
                }
            },
        }
    }
}

async fn observe_until(
    scope: &Scope,
    target: &Target,
    controller: Option<Controller>,
    wait_id: &str,
    deadline: Instant,
) -> TargetResult {
    loop {
        let started = Instant::now();
        let remaining = millis(deadline.saturating_duration_since(started));
        if remaining == 0 {
            return TargetResult::failed(
                target,
                Status::TimedOut,
                "await_lifetime_expired",
                "observer lifetime expired",
            );
        }
        let window_ms = remaining.min(MAX_OBSERVATION_WAIT_MS).max(1_000);
        let result = observe(scope, target, window_ms, controller.clone(), true, wait_id).await;
        // Only a documented, determinate no-match window may be renewed. Hard
        // failures, unknown work and malformed responses become cached outcomes.
        if result.status != Status::TimedOut {
            return result;
        }
        let empty = Value::Null;
        let data = result.data.as_ref().unwrap_or(&empty);
        let determinate = match target {
            Target::Exec { .. } => data["running"] == true,
            Target::Job { .. } => matches!(
                data["state"].as_str(),
                Some("starting" | "running" | "finishing")
            ),
            Target::WorkerResult { .. } => matches!(
                data["task"]["status"].as_str(),
                Some("queued" | "working" | "cancelling")
            ),
            Target::AgentStatus { .. } => data["matched"] == false,
            Target::AgentMessages { .. } => data["messages"].as_array().is_some_and(Vec::is_empty),
            Target::Event { .. } => event_status(data) == Status::TimedOut,
            _ => false,
        };
        if !determinate {
            return TargetResult::failed(
                target,
                Status::Unavailable,
                "await_indeterminate_observation",
                "observer response was not a determinate no-match window; no retry was attempted",
            );
        }
        drop(result);
        // A malformed provider that returns early cannot turn the renewal into
        // a tight polling loop. Normal native notification waits already use it.
        tokio::time::sleep_until((started + Duration::from_millis(window_ms)).min(deadline)).await;
    }
}

#[cfg(test)]
mod tests;
