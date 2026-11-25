//! Agent activity is independent of task outcomes and process lifetime.
//! Idle deadlines release unused inbox storage and offer a bounded participant round.
use super::*;
use serde::Serialize;
use std::sync::atomic::Ordering;

pub(crate) const DEFAULT_IDLE_AFTER_MS: u64 = 300_000;
pub(crate) const MAX_IDLE_AFTER_MS: u64 = 86_400_000;
pub(crate) const DEFAULT_MAINTENANCE_BUDGET_MS: u64 = 30_000;
const OFFLINE_RETENTION: Duration = Duration::from_secs(300);
const MAX_TRACKED_WORK: usize = 32;
const MAX_WAKE_KEYS: usize = 128;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Working,
    Idle,
    Offline,
    Dreaming,
    Retiring,
}

impl Status {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Offline => "offline",
            Self::Dreaming => "dreaming",
            Self::Retiring => "retiring",
        }
    }
}

pub(crate) fn status_schema() -> Value {
    json!({"type":"string","enum":["working","idle","offline","dreaming","retiring"]})
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MaintenanceAction {
    Start,
    Complete,
    Fail,
    Cancel,
    Unknown,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MaintenancePatch {
    round_id: String,
    action: MaintenanceAction,
}

#[derive(Default)]
pub(super) struct Patch {
    pub(super) status: Option<Status>,
    pub(super) expected_revision: Option<u64>,
    pub(super) idle_after_ms: Option<u64>,
    pub(super) maintenance_budget_ms: Option<u64>,
    pub(super) maintenance: Option<MaintenancePatch>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoundStatus {
    Requested,
    Running,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Clone)]
struct Round {
    id: String,
    status: RoundStatus,
    budget_ms: u64,
    requested_at_ms: u64,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    deadline: Option<Instant>,
    from_idle_deadline: bool,
    released_inbox_slots: Option<usize>,
}

impl Round {
    fn public(&self) -> Value {
        json!({"round_id":self.id,"status":self.status,"repository":"project",
        "budget_ms":self.budget_ms,"requested_at_ms":self.requested_at_ms,
        "started_at_ms":self.started_at_ms,"finished_at_ms":self.finished_at_ms,
        "source":"agent_lifecycle","execution":"participant_report",
        "local_housekeeping":self.released_inbox_slots.map(|slots| json!({
            "action":"release_empty_inbox","released_slots":slots,
            "released_slot_bytes":slots * std::mem::size_of::<Arc<Message>>()
        }))})
    }

    fn unresolved(&self) -> bool {
        matches!(self.status, RoundStatus::Running | RoundStatus::Unknown)
    }
}

// Copy the native receipt's exact immutable claim, never just its task ID.
// This is observation metadata; the native worker service keeps claim authority.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Claim {
    id: String,
    generation: String,
    slot: usize,
    epoch: u64,
    task_id: String,
    attempt: u64,
}

impl Claim {
    fn parse(value: &Value) -> Option<Self> {
        let claim: Self = serde_json::from_value(value.clone()).ok()?;
        [
            claim.id.as_str(),
            claim.generation.as_str(),
            claim.task_id.as_str(),
        ]
        .iter()
        .all(|value| !value.is_empty() && value.len() <= 256)
        .then_some(claim)
    }

    fn key(&self) -> String {
        fingerprint(&serde_json::to_vec(self).unwrap_or_default())
    }
}

#[derive(Clone)]
struct ClaimRecord {
    claim: Claim,
    unknown: bool,
}

#[derive(Clone)]
enum NativeWork {
    Wait,
    Complete(Claim),
}

#[derive(Clone)]
struct OperationRecord {
    work: NativeWork,
    active: bool,
    unknown: bool,
}

#[derive(Clone)]
pub(super) struct Lifecycle {
    pub(super) status: Status,
    pub(super) revision: u64,
    status_since_ms: u64,
    idle_after_ms: u64,
    maintenance_budget_ms: u64,
    idle_since: Option<Instant>,
    dreaming_since: Option<(Instant, u64)>,
    round: Option<Round>,
    claims: BTreeMap<String, ClaimRecord>,
    operations: BTreeMap<String, OperationRecord>,
    completed_claims: VecDeque<String>,
    wake_keys: VecDeque<String>,
}

pub(super) type Change = (&'static str, Value);

impl Lifecycle {
    pub(super) fn new(now: Instant, wall: u64) -> Self {
        Self {
            status: Status::Idle,
            revision: 1,
            status_since_ms: wall,
            idle_after_ms: DEFAULT_IDLE_AFTER_MS,
            maintenance_budget_ms: DEFAULT_MAINTENANCE_BUDGET_MS,
            idle_since: Some(now),
            dreaming_since: None,
            round: None,
            claims: BTreeMap::new(),
            operations: BTreeMap::new(),
            completed_claims: VecDeque::new(),
            wake_keys: VecDeque::new(),
        }
    }

    fn confirm_completed_claim(&mut self, completed: &Claim) {
        let claim_key = completed.key();
        if self
            .claims
            .get(&claim_key)
            .is_some_and(|known| known.claim == *completed)
        {
            self.claims.remove(&claim_key);
        }
        let completion_key = format!("complete:{claim_key}");
        if self
            .operations
            .get(&completion_key)
            .is_some_and(|operation| {
                !operation.active
                    && operation.unknown
                    && matches!(
                        &operation.work,
                        NativeWork::Complete(expected) if expected == completed
                    )
            })
        {
            self.operations.remove(&completion_key);
        }
        if !self.completed_claims.contains(&claim_key) {
            if self.completed_claims.len() == MAX_WAKE_KEYS {
                self.completed_claims.pop_front();
            }
            self.completed_claims.push_back(claim_key);
        }
    }

    pub(super) fn dreaming_since_ms(&self) -> Option<u64> {
        self.dreaming_since.map(|(_, wall)| wall)
    }

    pub(super) fn public(&self) -> Value {
        json!({"revision":self.revision,"status_since_ms":self.status_since_ms,
            "idle_after_ms":self.idle_after_ms,"maintenance_budget_ms":self.maintenance_budget_ms,
            "dreaming_since_ms":self.dreaming_since_ms(),
            "dreaming_elapsed_ms":self.dreaming_since.map(|(start, _)| start.elapsed().as_millis().min(u64::MAX as u128) as u64),
            "active_claims":self.claims.len(),"in_flight":self.operations.values().filter(|op|op.active).count(),
            "unknown_outcomes":self.unknown_count(),"maintenance":self.round.as_ref().map(Round::public)})
    }

    fn unknown_count(&self) -> usize {
        self.claims.values().filter(|claim| claim.unknown).count()
            + self.operations.values().filter(|op| op.unknown).count()
            + usize::from(
                self.round
                    .as_ref()
                    .is_some_and(|round| round.status == RoundStatus::Unknown),
            )
    }

    fn work_pending(&self) -> bool {
        !self.claims.is_empty()
            || !self.operations.is_empty()
            || self.round.as_ref().is_some_and(Round::unresolved)
    }

    fn maintenance_blocked(&self) -> bool {
        !self.claims.is_empty()
            || self
                .operations
                .values()
                .any(|op| op.unknown || matches!(&op.work, NativeWork::Complete(_)))
            || self.round.as_ref().is_some_and(Round::unresolved)
    }

    fn transition(
        &mut self,
        status: Status,
        source: &str,
        now: Instant,
        wall: u64,
        changes: &mut Vec<Change>,
    ) {
        if self.status == status {
            return;
        }
        let previous = self.status;
        self.status = status;
        if status != Status::Dreaming {
            self.dreaming_since = None;
        }
        self.revision += 1;
        self.status_since_ms = wall;
        self.idle_since = (status == Status::Idle).then_some(now);
        changes.push((
            "agent.status_changed",
            json!({"previous_status":previous,"status":status,
            "revision":self.revision,"status_since_ms":wall,"source":source}),
        ));
    }

    fn round_event(&mut self, kind: &'static str, reason: &str, changes: &mut Vec<Change>) {
        self.revision += 1;
        changes.push((kind, json!({"revision":self.revision,"maintenance":self.round.as_ref().map(Round::public),
            "source":"agent_lifecycle","reason":reason,"completion_permission":false,
            "guidance":"For participant maintenance, choose one bounded, already-authorized action. Report start and actual outcome with this round_id and current expected_revision. Yield to new user input. Preserve unknown operation IDs; never replay unknown work or start a model."})));
    }

    fn preempt_round(&mut self, reason: &str, wall: u64, changes: &mut Vec<Change>) {
        let Some(round) = self.round.as_mut() else {
            return;
        };
        round.status = match round.status {
            RoundStatus::Requested => RoundStatus::Cancelled,
            RoundStatus::Running => RoundStatus::Unknown,
            _ => return,
        };
        round.deadline = None;
        if round.status == RoundStatus::Cancelled {
            round.finished_at_ms = Some(wall);
        }
        self.round_event("agent.maintenance_changed", reason, changes);
    }

    fn request_round(
        &mut self,
        id: &str,
        source: &str,
        now: Instant,
        wall: u64,
        changes: &mut Vec<Change>,
    ) {
        self.transition(Status::Dreaming, source, now, wall, changes);
        self.round = Some(Round {
            id: format!("maint_{id}_{}", self.revision + 1),
            status: RoundStatus::Requested,
            budget_ms: self.maintenance_budget_ms,
            requested_at_ms: wall,
            started_at_ms: None,
            finished_at_ms: None,
            deadline: Some(now + Duration::from_millis(self.maintenance_budget_ms)),
            from_idle_deadline: source == "idle_deadline",
            released_inbox_slots: None,
        });
        self.round_event("agent.maintenance_requested", source, changes);
    }

    fn release_empty_inbox(
        &mut self,
        inbox: &mut VecDeque<Arc<Message>>,
        now: Instant,
        wall: u64,
        changes: &mut Vec<Change>,
    ) {
        let Some(round) = &mut self.round else { return };
        if self.status != Status::Dreaming
            || !round.from_idle_deadline
            || round.released_inbox_slots.is_some()
            || !matches!(round.status, RoundStatus::Requested | RoundStatus::Running)
        {
            return;
        }
        // Acknowledgement already removed the messages. Reclaim one empty allocation
        // without moving pending entries or changing cursors or byte accounting.
        let released = if inbox.is_empty() {
            std::mem::take(inbox).capacity()
        } else {
            0
        };
        round.released_inbox_slots = Some(released);
        self.dreaming_since.get_or_insert((now, wall));
        if let Some((_, data)) = changes
            .last_mut()
            .filter(|(kind, _)| *kind == "agent.maintenance_requested")
        {
            data["maintenance"] = round.public();
        } else {
            // A synchronous lookup may have already published the due round.
            self.round_event("agent.maintenance_changed", "local_inbox_cleanup", changes);
        }
    }

    fn after_round(&self) -> Status {
        if self.maintenance_blocked() {
            Status::Working
        } else if self.dreaming_since.is_some() {
            Status::Dreaming
        } else {
            Status::Idle
        }
    }

    fn settle_retirement(&mut self, now: Instant, wall: u64, changes: &mut Vec<Change>) {
        if self.status == Status::Retiring && !self.work_pending() {
            self.transition(Status::Offline, "retirement_drained", now, wall, changes);
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        let idle = if self.status == Status::Idle && !self.maintenance_blocked() {
            self.idle_since
                .map(|at| at + Duration::from_millis(self.idle_after_ms))
        } else {
            None
        };
        idle.into_iter()
            .chain(self.round.as_ref().and_then(|round| round.deadline))
            .min()
    }

    fn advance(&mut self, id: &str, now: Instant, wall: u64) -> Vec<Change> {
        let mut changes = Vec::new();
        if self
            .round
            .as_ref()
            .and_then(|round| round.deadline)
            .is_some_and(|at| at <= now)
        {
            self.preempt_round("maintenance_budget_elapsed", wall, &mut changes);
            if self.status == Status::Dreaming {
                let status = self.after_round();

                self.transition(
                    status,
                    "maintenance_budget_elapsed",
                    now,
                    wall,
                    &mut changes,
                );
            }
        }
        self.settle_retirement(now, wall, &mut changes);
        if self.status == Status::Idle
            && !self.maintenance_blocked()
            && self
                .idle_since
                .is_some_and(|at| now >= at + Duration::from_millis(self.idle_after_ms))
        {
            self.request_round(id, "idle_deadline", now, wall, &mut changes);
        }
        changes
    }

    pub(super) fn update(
        &mut self,
        id: &str,
        patch: Patch,
        now: Instant,
        wall: u64,
    ) -> Result<Vec<Change>, String> {
        let operational = patch.status.is_some()
            || patch.idle_after_ms.is_some()
            || patch.maintenance_budget_ms.is_some()
            || patch.maintenance.is_some();
        if !operational {
            if patch
                .expected_revision
                .is_some_and(|revision| revision != self.revision)
            {
                return Err(
                    "agent_lifecycle_stale_revision: read the current agent snapshot".into(),
                );
            }
            return Ok(Vec::new());
        }
        if patch.expected_revision != Some(self.revision) {
            return Err(format!(
                "agent_lifecycle_stale_revision: expected_revision must equal {}",
                self.revision
            ));
        }
        if self.revision >= MAX_SEQUENCE - 16 {
            return Err("agent lifecycle revision capacity exhausted".into());
        }
        if patch.status.is_some() && patch.maintenance.is_some() {
            return Err("update status or a maintenance outcome separately".into());
        }
        if patch
            .idle_after_ms
            .is_some_and(|value| !(1000..=MAX_IDLE_AFTER_MS).contains(&value))
            || patch
                .maintenance_budget_ms
                .is_some_and(|value| !(1000..=MAX_WAIT_MS).contains(&value))
        {
            return Err(
                "idle_after_ms must be 1000..86400000; maintenance_budget_ms must be 1000..55000"
                    .into(),
            );
        }
        // Validate on a clone: a bad round/CAS must not partially change config.
        let mut next = self.clone();
        let mut changes = Vec::new();
        let idle = patch.idle_after_ms.unwrap_or(next.idle_after_ms);
        let budget = patch
            .maintenance_budget_ms
            .unwrap_or(next.maintenance_budget_ms);
        if idle != next.idle_after_ms || budget != next.maintenance_budget_ms {
            next.idle_after_ms = idle;
            next.maintenance_budget_ms = budget;
            next.revision += 1;
            changes.push((
                "agent.lifecycle_configured",
                json!({"revision":next.revision,
                "idle_after_ms":idle,"maintenance_budget_ms":budget,"source":"participant"}),
            ));
        }
        if let Some(status) = patch.status {
            let blocked = match status {
                Status::Idle | Status::Dreaming => next.maintenance_blocked(),
                Status::Offline => next.work_pending(),
                Status::Working | Status::Retiring => false,
            };
            if status != next.status && blocked {
                return Err("agent_lifecycle_unresolved_work: drain or reconcile original work before this transition".into());
            }
            if status == Status::Dreaming {
                if next.status != Status::Dreaming
                    || next.round.as_ref().is_some_and(|round| {
                        matches!(
                            round.status,
                            RoundStatus::Completed | RoundStatus::Failed | RoundStatus::Cancelled
                        )
                    })
                {
                    next.request_round(id, "participant", now, wall, &mut changes);
                }
            } else {
                if status == Status::Retiring {
                    if next
                        .round
                        .as_ref()
                        .is_some_and(|round| round.status == RoundStatus::Requested)
                    {
                        next.preempt_round("retirement_requested", wall, &mut changes);
                    }
                } else {
                    next.preempt_round("participant", wall, &mut changes);
                }
                next.transition(status, "participant", now, wall, &mut changes);
            }
        }
        if let Some(patch) = patch.maintenance {
            let round = next
                .round
                .as_mut()
                .filter(|round| round.id == patch.round_id)
                .ok_or("agent_lifecycle_stale_round: preserve the original round_id")?;
            let status =
                match patch.action {
                    MaintenanceAction::Start
                        if round.status == RoundStatus::Requested
                            && next.status == Status::Dreaming =>
                    {
                        next.dreaming_since.get_or_insert((now, wall));
                        round.started_at_ms = Some(wall);
                        round.deadline = Some(now + Duration::from_millis(round.budget_ms));
                        RoundStatus::Running
                    }
                    MaintenanceAction::Cancel
                        if matches!(
                            round.status,
                            RoundStatus::Requested | RoundStatus::Running | RoundStatus::Unknown
                        ) =>
                    {
                        RoundStatus::Cancelled
                    }
                    MaintenanceAction::Complete if round.unresolved() => RoundStatus::Completed,
                    MaintenanceAction::Fail if round.unresolved() => RoundStatus::Failed,
                    MaintenanceAction::Unknown if round.status == RoundStatus::Running => {
                        RoundStatus::Unknown
                    }
                    _ => return Err(
                        "agent_lifecycle_invalid_round_transition: report the actual round outcome"
                            .into(),
                    ),
                };
            round.status = status;
            if status != RoundStatus::Running {
                round.deadline = None;
                if status != RoundStatus::Unknown {
                    round.finished_at_ms = Some(wall);
                }
            }
            next.round_event(
                "agent.maintenance_changed",
                "participant_report",
                &mut changes,
            );
            if next.status == Status::Dreaming && status != RoundStatus::Running {
                let phase = next.after_round();
                next.transition(phase, "maintenance_report", now, wall, &mut changes);
            }
        }
        next.settle_retirement(now, wall, &mut changes);
        *self = next;
        Ok(changes)
    }

    fn wake(&mut self, key: String, source: &str, now: Instant, wall: u64) -> Vec<Change> {
        if self.wake_keys.contains(&key) {
            return Vec::new();
        }
        if self.wake_keys.len() == MAX_WAKE_KEYS {
            self.wake_keys.pop_front();
        }
        self.wake_keys.push_back(key);
        let mut changes = Vec::new();
        if self.status != Status::Retiring {
            self.preempt_round(source, wall, &mut changes);
            self.transition(Status::Working, source, now, wall, &mut changes);
            if changes.is_empty() {
                // A fresh instruction invalidates a prior idle/report CAS even
                // when the visible state was already working.
                self.revision += 1;
                changes.push((
                    "agent.activity",
                    json!({"revision":self.revision,
                    "status":self.status,"source":source}),
                ));
            }
        }
        changes
    }
}

pub(super) struct Offline {
    snapshot: Value,
    expires: Instant,
}

fn fingerprint(bytes: &[u8]) -> String {
    crate::hex::encode(Sha256::digest(bytes))
}

fn publish(
    events: Option<&crate::events::Hub>,
    session: &str,
    agent: &Agent,
    changes: Vec<Change>,
) {
    if changes.is_empty() {
        return;
    }
    for (kind, data) in changes {
        if let Some(events) = events {
            let audience = matches!(
                kind,
                "agent.maintenance_requested" | "agent.maintenance_changed"
            )
            .then(|| vec![format!("{session}:{}", agent.id)]);
            let _ = events.publish_system(
                &agent.owner,
                session,
                Some(&agent.id),
                kind,
                data,
                (audience, None),
            );
        }
    }
    agent.changed.send_replace(());
}

fn retain_offline(
    state: &mut State,
    mut agent: Agent,
    events: Option<&crate::events::Hub>,
    session: &str,
    reason: &str,
    now: Instant,
    wall: u64,
) {
    let mut changes = Vec::new();
    agent.lifecycle.preempt_round(reason, wall, &mut changes);
    for claim in agent.lifecycle.claims.values_mut() {
        claim.unknown = true;
    }
    for operation in agent.lifecycle.operations.values_mut() {
        operation.active = false;
        operation.unknown = true;
    }
    agent
        .lifecycle
        .transition(Status::Offline, reason, now, wall, &mut changes);
    changes.push((
        "agent.left",
        json!({"reason":reason,"status":"offline","revision":agent.lifecycle.revision}),
    ));
    publish(events, session, &agent, changes);
    let mut snapshot = agent.public();
    snapshot["membership_active"] = json!(false);
    snapshot["pending_messages"] = json!(0);
    snapshot["offline_reason"] = json!(reason);
    if state.offline.len() == MAX_AGENTS {
        state.offline.pop_front();
    }
    state.offline.push_back(Offline {
        snapshot,
        expires: now + OFFLINE_RETENTION,
    });
}

fn advance(
    state: &mut State,
    events: Option<&crate::events::Hub>,
    session: &str,
    on_deadline_driver: bool,
) -> Option<Instant> {
    let now = Instant::now();
    let wall = now_ms();
    state.offline.retain(|offline| offline.expires > now);
    for agent in state.purge_expired() {
        retain_offline(state, agent, events, session, "lease_expired", now, wall);
    }
    for agent in state.agents.values_mut() {
        let mut changes = agent.lifecycle.advance(&agent.id, now, wall);
        if on_deadline_driver {
            agent
                .lifecycle
                .release_empty_inbox(&mut agent.inbox, now, wall, &mut changes);
        }
        publish(events, session, agent, changes);
    }
    state
        .agents
        .values()
        .flat_map(|agent| [Some(agent.expires), agent.lifecycle.next_deadline()])
        .flatten()
        .chain(state.offline.iter().map(|offline| offline.expires))
        .min()
}

impl Hub {
    pub(super) fn start_lifecycle(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.lifecycle_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = Arc::downgrade(&self.state);
        let changed = self.lifecycle_changed.clone();
        let events = self.events.as_ref().map(Arc::downgrade);
        let session = self.session_id.clone();
        runtime.spawn(async move {
            loop {
                let notified = changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let deadline = {
                    let Some(state) = state.upgrade() else {
                        return;
                    };
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.closed {
                        return;
                    }
                    let events = events.as_ref().and_then(std::sync::Weak::upgrade);
                    advance(&mut state, events.as_deref(), &session, true)
                };
                if let Some(deadline) = deadline {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {},
                        _ = &mut notified => {},
                    }
                } else {
                    notified.await;
                }
            }
        });
    }

    pub(super) fn advance_lifecycle(&self, state: &mut State) {
        advance(state, self.events.as_deref(), &self.session_id, false);
    }

    pub(super) fn publish_lifecycle(&self, agent: &Agent, changes: Vec<Change>) {
        if changes.is_empty() {
            return;
        }
        publish(self.events.as_deref(), &self.session_id, agent, changes);
        self.lifecycle_changed.notify_one();
    }

    pub(super) fn remove_member(&self, state: &mut State, id: &str, reason: &str) {
        if let Some(agent) = state.remove(id) {
            retain_offline(
                state,
                agent,
                self.events.as_deref(),
                &self.session_id,
                reason,
                Instant::now(),
                now_ms(),
            );
            self.lifecycle_changed.notify_one();
        }
    }

    pub(crate) fn lifecycle_snapshot(
        &self,
        owner: &str,
        id: &str,
        token: &str,
    ) -> Result<Value, String> {
        let mut state = self.locked()?;
        let agent = state.authenticated(owner, id, token)?;
        Ok(json!({"status":agent.lifecycle.status,"lifecycle":agent.lifecycle.public()}))
    }

    /// Caller supplies a verified real worker event and the frozen main binding.
    pub(crate) fn wake_for_worker_event(
        &self,
        owner: &str,
        agent_id: &str,
        token: &str,
        event_id: &str,
    ) -> Result<bool, String> {
        text(event_id, "event_id", 512, false)?;
        let mut state = self.locked()?;
        let agent = state.authenticated(owner, agent_id, token)?;
        let key = format!("worker:{}", fingerprint(event_id.as_bytes()));
        let changes = agent
            .lifecycle
            .wake(key, "worker_event", Instant::now(), now_ms());
        let changed = !changes.is_empty();
        self.publish_lifecycle(agent, changes);
        Ok(changed)
    }

    /// Only the broker's verified queued-input recipient path calls this hook.
    /// Signal availability without reserving, delivering, or acknowledging text.
    pub(crate) fn user_input_queued(
        &self,
        owner: &str,
        agent_id: &str,
        message_id: &str,
        revision: u64,
    ) {
        let Ok(mut state) = self.locked() else {
            return;
        };
        let Some(agent) = state
            .agents
            .get_mut(agent_id)
            .filter(|agent| agent.owner == owner)
        else {
            return;
        };
        let key = format!(
            "input:{}",
            fingerprint(&serde_json::to_vec(&(message_id, revision)).unwrap_or_default())
        );
        let changes = agent
            .lifecycle
            .wake(key, "user_input", Instant::now(), now_ms());
        self.publish_lifecycle(agent, changes);
    }
}

impl State {
    pub(super) fn offline_agents(&self) -> Vec<Value> {
        self.offline
            .iter()
            .map(|offline| offline.snapshot.clone())
            .collect()
    }

    pub(super) fn offline_agent(&self, id: &str) -> Option<Value> {
        self.offline
            .iter()
            .find(|offline| offline.snapshot["agent_id"] == id)
            .map(|offline| offline.snapshot.clone())
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // The deadline task owns only a Weak<State>; wake it to release itself.
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.lifecycle_changed.notify_one();
    }
}

mod main_observation;
use main_observation::DisplayCall;
pub(super) use main_observation::MainObservation;

/// A cancelled HTTP future cannot manufacture a completed native operation.
pub(crate) struct NativeActivity<'a> {
    hub: &'a Hub,
    owner: String,
    id: String,
    token: String,
    key: Option<String>,
    previous: Option<OperationRecord>,
    display_call: Option<DisplayCall>,
    business: bool,
    answer: bool,
    dispatch_revision: u64,
    dispatched: bool,
    finished: bool,
}

impl Hub {
    pub(crate) fn prepare_native_activity(
        &self,
        owner: &str,
        id: &str,
        token: &str,
        operation: &crate::wire::Operation,
    ) -> Result<NativeActivity<'_>, String> {
        let mut work = None;
        let mut business = false;
        let mut answer = false;
        if let crate::wire::Operation::CallTool { name, arguments } = operation {
            match name.as_str() {
                "ssmcp.worker.wait" | "ssmcp.worker.claim" => {
                    let request_id = arguments["request_id"]
                        .as_str()
                        .filter(|id| !id.is_empty() && id.len() <= 128)
                        .ok_or(
                            "worker.wait lifecycle observation requires its original request_id",
                        )?;
                    work = Some((
                        format!("wait:{}", fingerprint(request_id.as_bytes())),
                        NativeWork::Wait,
                    ));
                }
                "ssmcp.worker.complete" => {
                    let claim = Claim::parse(&arguments["claim"])
                        .ok_or("worker.complete requires the exact original claim")?;
                    work = Some((
                        format!("complete:{}", claim.key()),
                        NativeWork::Complete(claim),
                    ));
                }
                _ => {
                    answer = name == "ssmcp.ui.message" && arguments["kind"] == "answer";
                    let generated_input =
                        name == "ssmcp.ui.message" && arguments["kind"] == "input";
                    business = !generated_input
                        && !(name.starts_with("ssmcp.time.")
                            || name.starts_with("ssmcp.events.")
                            || name == crate::code_mode::tool_summaries::TOOL
                            || name.starts_with("ssmcp.worker.")
                            || matches!(
                                name.as_str(),
                                "ssmcp.ui.get"
                                    | "ssmcp.ui.wait"
                                    | "ssmcp.ui.question_delivery"
                                    | "ssmcp.client.capabilities"
                            ));
                }
            }
        }
        let mut state = self.locked()?;
        let agent = state.authenticated(owner, id, token)?;
        let mut previous = None;
        let key = if let Some((key, work)) = work {
            // Native queue retirement remains authoritative. Its wait receipt
            // is also required to release a worker; a display state must not
            // block that protocol or invent/revoke claim authority.
            if agent
                .lifecycle
                .operations
                .get(&key)
                .is_some_and(|op| op.active)
            {
                return Err("agent_lifecycle_in_flight: observe the original native request before retrying".into());
            }
            if !agent.lifecycle.operations.contains_key(&key)
                && agent.lifecycle.operations.len() >= MAX_TRACKED_WORK
            {
                return Err(
                    "agent_lifecycle_capacity: reconcile retained unknown native operations first"
                        .into(),
                );
            }
            if matches!(work, NativeWork::Wait) && agent.lifecycle.claims.len() >= MAX_TRACKED_WORK
            {
                return Err(
                    "agent_lifecycle_capacity: drain retained claims before requesting more".into(),
                );
            }
            previous = agent.lifecycle.operations.insert(
                key.clone(),
                OperationRecord {
                    work,
                    active: true,
                    unknown: false,
                },
            );
            Some(key)
        } else {
            None
        };
        Ok(NativeActivity {
            hub: self,
            owner: owner.into(),
            id: id.into(),
            token: token.into(),
            key,
            previous,
            display_call: DisplayCall::classify(operation),
            business,
            answer,
            dispatch_revision: 0,
            dispatched: false,
            finished: false,
        })
    }
}

impl NativeActivity<'_> {
    pub(crate) fn dispatched(&mut self) {
        self.dispatched = true;
        let Ok(mut state) = self.hub.locked() else {
            return;
        };
        let Ok(agent) = state.authenticated(&self.owner, &self.id, &self.token) else {
            return;
        };
        let mut changes = Vec::new();
        if let Some(call) = self.display_call {
            agent.main_activity.start(call, Instant::now());
        }
        let wait_only = self
            .key
            .as_ref()
            .and_then(|key| agent.lifecycle.operations.get(key))
            .is_some_and(|op| matches!(&op.work, NativeWork::Wait));
        if self.key.is_some() {
            agent.lifecycle.revision += 1;
            changes.push(("agent.work_changed", json!({"revision":agent.lifecycle.revision,"source":"native_dispatch","lifecycle":agent.lifecycle.public()})));
        }
        // Maintenance execution remains dreaming until preempted or reported.
        if (self.business || (self.key.is_some() && !wait_only))
            && !matches!(agent.lifecycle.status, Status::Retiring | Status::Offline)
            && !agent
                .lifecycle
                .round
                .as_ref()
                .is_some_and(|round| round.status == RoundStatus::Running)
        {
            agent
                .lifecycle
                .preempt_round("native_activity", now_ms(), &mut changes);
            agent.lifecycle.transition(
                Status::Working,
                "native_activity",
                Instant::now(),
                now_ms(),
                &mut changes,
            );
        }
        if self.business && changes.is_empty() {
            agent.lifecycle.revision += 1;
            changes.push((
                "agent.activity",
                json!({"revision":agent.lifecycle.revision,
                "status":agent.lifecycle.status,"source":"native_activity"}),
            ));
        }
        self.dispatch_revision = agent.lifecycle.revision;
        self.hub.publish_lifecycle(agent, changes);
    }

    pub(crate) fn finish(mut self, result: &Result<Value, String>) {
        self.record(Some(result));
        self.finished = true;
    }

    fn record(&mut self, result: Option<&Result<Value, String>>) {
        let Ok(mut state) = self.hub.locked() else {
            return;
        };
        let Ok(agent) = state.authenticated(&self.owner, &self.id, &self.token) else {
            return;
        };
        let now = Instant::now();
        let wall = now_ms();
        if self.dispatched
            && let Some(call) = self.display_call
        {
            agent.main_activity.finish(
                call,
                result.is_some_and(|r| r.as_ref().is_ok_and(|v| v["isError"] != true)),
                now,
            );
        }
        let life = &mut agent.lifecycle;
        let may_settle_idle = life.revision == self.dispatch_revision;
        let mut changes = Vec::new();
        if let Some(key) = &self.key {
            let Some(mut op) = life.operations.remove(key) else {
                return;
            };
            if !self.dispatched {
                if let Some(previous) = self.previous.take() {
                    life.operations.insert(key.clone(), previous);
                }
                self.hub.lifecycle_changed.notify_one();
                return;
            }
            let receipt = result
                .and_then(|r| r.as_ref().ok())
                .filter(|v| v["isError"] != true)
                .and_then(native_receipt);
            let mut confirmed = false;
            if let Some(receipt) = receipt {
                match &op.work {
                    NativeWork::Wait => {
                        if receipt["status"] == "assigned" {
                            if let Some(claim) = Claim::parse(&receipt["claim"]) {
                                let claim_key = claim.key();
                                if !life.completed_claims.contains(&claim_key) {
                                    life.claims.insert(
                                        claim_key,
                                        ClaimRecord {
                                            claim,
                                            unknown: false,
                                        },
                                    );
                                    life.preempt_round(
                                        "native_worker_assigned",
                                        wall,
                                        &mut changes,
                                    );
                                    if life.status != Status::Retiring {
                                        life.transition(
                                            Status::Working,
                                            "native_worker_assigned",
                                            now,
                                            wall,
                                            &mut changes,
                                        );
                                    }
                                    if receipt["retire_requested"] == true {
                                        life.transition(
                                            Status::Retiring,
                                            "native_retirement",
                                            now,
                                            wall,
                                            &mut changes,
                                        );
                                    }
                                }
                                confirmed = true;
                            }
                        } else if receipt["status"] == "already_completed" {
                            if let Some(completed) = Claim::parse(&receipt["claim"])
                                && matches!(
                                    receipt["task"]["status"].as_str(),
                                    Some("completed" | "failed" | "cancelled")
                                )
                            {
                                life.confirm_completed_claim(&completed);
                                confirmed = true;
                            }
                        } else if matches!(
                            receipt["status"].as_str(),
                            Some(
                                "waiting"
                                    | "idle"
                                    | "retiring"
                                    | "retired"
                                    | "released"
                                    | "not_accepting"
                                    | "closed"
                            )
                        ) {
                            confirmed = true;
                            if matches!(
                                receipt["status"].as_str(),
                                Some(
                                    "retiring"
                                        | "retired"
                                        | "released"
                                        | "not_accepting"
                                        | "closed"
                                )
                            ) {
                                if life
                                    .round
                                    .as_ref()
                                    .is_some_and(|round| round.status == RoundStatus::Requested)
                                {
                                    life.preempt_round("native_retirement", wall, &mut changes);
                                }
                                life.transition(
                                    Status::Retiring,
                                    "native_retirement",
                                    now,
                                    wall,
                                    &mut changes,
                                );
                            }
                        }
                    }
                    NativeWork::Complete(expected) => {
                        if receipt["status"] == "recorded"
                            && Claim::parse(&receipt["claim"]).as_ref() == Some(expected)
                            && matches!(
                                receipt["task"]["status"].as_str(),
                                Some("completed" | "failed" | "cancelled")
                            )
                        {
                            life.confirm_completed_claim(expected);
                            confirmed = true;
                        }
                    }
                }
            }
            if !confirmed {
                op.active = false;
                op.unknown = true;
                if let NativeWork::Complete(claim) = &op.work {
                    life.claims
                        .entry(claim.key())
                        .and_modify(|known| known.unknown = true)
                        .or_insert_with(|| ClaimRecord {
                            claim: claim.clone(),
                            unknown: true,
                        });
                }
                life.operations.insert(key.clone(), op);
                if matches!(life.status, Status::Idle | Status::Dreaming) {
                    life.preempt_round("native_outcome_unknown", wall, &mut changes);
                    life.transition(
                        Status::Working,
                        "native_outcome_unknown",
                        now,
                        wall,
                        &mut changes,
                    );
                }
            }
            life.revision += 1;
            changes.push(("agent.work_changed", json!({"revision":life.revision,
                "source":if confirmed {"native_receipt"} else {"native_outcome_unknown"},"lifecycle":life.public()})));
            if confirmed
                && may_settle_idle
                && !life.work_pending()
                && life.status == Status::Working
            {
                life.transition(Status::Idle, "native_worker_idle", now, wall, &mut changes);
            }
            life.settle_retirement(now, wall, &mut changes);
        } else if self.dispatched
            && self.answer
            && result.is_some_and(|r| r.as_ref().is_ok_and(|v| v["isError"] != true))
            && life.revision == self.dispatch_revision
            && life.status == Status::Working
            && !life.work_pending()
        {
            life.transition(Status::Idle, "answer_delivered", now, wall, &mut changes);
        } else if self.dispatched && self.business && result.is_none_or(|r| r.is_err()) {
            life.preempt_round("native_outcome_unknown", wall, &mut changes);
            if life.status == Status::Dreaming {
                life.transition(
                    Status::Working,
                    "native_outcome_unknown",
                    now,
                    wall,
                    &mut changes,
                );
            }
        }
        self.hub.publish_lifecycle(agent, changes);
    }
}

impl Drop for NativeActivity<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.record(None);
        }
    }
}

fn native_receipt(value: &Value) -> Option<Value> {
    if let Some(value) = value
        .get("structuredContent")
        .filter(|value| value.is_object())
    {
        return Some(value.clone());
    }
    value.get("content")?.as_array()?.iter().find_map(|block| {
        if block["type"] != "text" {
            return None;
        }
        serde_json::from_str(block["text"].as_str()?).ok()
    })
}

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
