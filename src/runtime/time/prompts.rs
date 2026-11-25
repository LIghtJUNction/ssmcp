//! Connection-local, agent-authored prompt schedules. No execution or model API.
//! One Clock-owned pump creates retained events even after an HTTP caller leaves.
use super::{Clock, Owner, Source, civil, millis};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::time::Instant;

pub(crate) const TOOL: &str = "ssmcp.time.prompt";
const MAX_SCHEDULES: usize = 32;
const MAX_RECORDS: usize = 128;
const MAX_TEMPLATE_BYTES: usize = 8192;
const MAX_RENDERED_BYTES: usize = 16384;
const MAX_VARIABLE_BYTES: usize = 4096;
const MAX_LIFETIME_MS: u64 = 86_400_000;
const RETENTION_MS: u64 = 300_000;
const BUILTINS: [&str; 5] = ["timer_id", "scheduled_at", "fired_at", "tick", "missed"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromptStatus {
    Scheduled,
    Paused,
    Completed,
    Cancelled,
    Expired,
    Disconnected,
}

impl PromptStatus {
    fn active(self) -> bool {
        matches!(self, Self::Scheduled | Self::Paused)
    }
}

/// Display metadata only. Never add templates, variables, or rendered bodies.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct PromptSummary {
    pub id: String,
    pub status: PromptStatus,
    pub scope: &'static str,
    pub created_at_unix_ms: i64,
    pub next_fire_unix_ms: Option<i64>,
    pub last_fired_at_unix_ms: Option<i64>,
    pub interval_ms: Option<u64>,
    pub lifetime_ms: u64,
    pub expires_at_unix_ms: i64,
    pub tick: u64,
    pub pending_count: u32,
    pub visibility: &'static str,
    pub missed_count: u64,
    pub retention_after_ack_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
struct Event {
    event_id: String,
    revision: u64,
    timer_id: String,
    provenance: &'static str,
    completion_permission: bool,
    scheduled_at_unix_ms: i64,
    fired_at_unix_ms: i64,
    delay_ms: u64,
    tick: u64,
    missed: u64,
    body: String,
}

fn lifetime_default() -> u64 {
    MAX_LIFETIME_MS
}
fn read_limit_default() -> usize {
    4
}

#[derive(Clone, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Schedule {
        request_id: String,
        template: String,
        #[serde(default)]
        variables: BTreeMap<String, Value>,
        after_ms: Option<u64>,
        at_unix_ms: Option<i64>,
        interval_ms: Option<u64>,
        max_fires: Option<u64>,
        #[serde(default = "lifetime_default")]
        lifetime_ms: u64,
    },
    List,
    Status {
        timer_id: String,
    },
    Pause {
        timer_id: String,
    },
    Resume {
        timer_id: String,
    },
    Cancel {
        timer_id: String,
    },
    Read {
        timer_id: Option<String>,
        #[serde(default = "read_limit_default")]
        limit: usize,
    },
    #[serde(alias = "ack_memory")]
    AckTask {
        job_id: String,
    },
    Ack {
        timer_id: String,
        event_id: String,
        revision: u64,
    },
}

#[derive(Clone, PartialEq)]
struct Spec {
    template: String,
    variables: BTreeMap<String, Value>,
    after_ms: Option<u64>,
    at_unix_ms: Option<i64>,
    interval_ms: Option<u64>,
    max_fires: Option<u64>,
    lifetime_ms: u64,
}

enum Part {
    Literal(String),
    Builtin(usize),
}
struct Template(Vec<Part>);

fn variable_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .enumerate()
            .all(|(i, b)| b == b'_' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()))
}

impl Template {
    fn compile(text: &str, variables: &BTreeMap<String, Value>) -> Result<Self, String> {
        if text.is_empty() || text.len() > MAX_TEMPLATE_BYTES {
            return Err("template must contain 1..=8192 UTF-8 bytes".into());
        }
        if variables.len() > 32
            || serde_json::to_vec(variables)
                .map_err(|e| e.to_string())?
                .len()
                > MAX_VARIABLE_BYTES
        {
            return Err("variables are limited to 32 entries and 4096 JSON bytes".into());
        }
        let mut values = BTreeMap::new();
        for (name, value) in variables {
            if !variable_name(name) {
                return Err(format!("invalid variable name: {name}"));
            }
            if BUILTINS.contains(&name.as_str()) {
                return Err(format!("reserved variable: {name}"));
            }
            let value = match value {
                Value::String(value) => value.clone(),
                Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
                _ => return Err(format!("variable {name} must be a scalar JSON value")),
            };
            if value.len() > 1024 {
                return Err(format!("variable {name} exceeds 1024 bytes"));
            }
            values.insert(name.as_str(), value);
        }
        let mut parts = Vec::new();
        let mut remaining = text;
        let mut bound = 0usize;
        while let Some(start) = remaining.find("{{") {
            let literal = &remaining[..start];
            if literal.contains("}}") {
                return Err("unmatched template closing delimiter".into());
            }
            bound += literal.len();
            parts.push(Part::Literal(literal.into()));
            remaining = &remaining[start + 2..];
            let end = remaining.find("}}").ok_or("unclosed template variable")?;
            let name = &remaining[..end];
            if !variable_name(name) {
                return Err(format!("invalid template variable: {name}"));
            }
            if let Some(index) = BUILTINS.iter().position(|builtin| *builtin == name) {
                // IDs are <=128 bytes; every numeric built-in is <=20 bytes.
                bound += if index == 0 { 128 } else { 20 };
                parts.push(Part::Builtin(index));
            } else {
                let value = values
                    .get(name)
                    .ok_or_else(|| format!("missing variable: {name}"))?;
                bound += value.len();
                parts.push(Part::Literal(value.clone()));
            }
            remaining = &remaining[end + 2..];
        }
        if remaining.contains("}}") {
            return Err("unmatched template closing delimiter".into());
        }
        bound += remaining.len();
        parts.push(Part::Literal(remaining.into()));
        if bound > MAX_RENDERED_BYTES {
            return Err("template can render more than 16384 bytes".into());
        }
        Ok(Self(parts))
    }

    fn render(&self, id: &str, scheduled: i64, fired: i64, tick: u64, missed: u64) -> String {
        let values = [
            id.to_owned(),
            scheduled.to_string(),
            fired.to_string(),
            tick.to_string(),
            missed.to_string(),
        ];
        let mut output = String::new();
        for part in &self.0 {
            output.push_str(match part {
                Part::Literal(text) => text,
                Part::Builtin(index) => &values[*index],
            });
        }
        debug_assert!(output.len() <= MAX_RENDERED_BYTES);
        output
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationState {
    InFlight,
    Published,
    Rejected,
    Unknown,
}

#[derive(Clone)]
struct Publication {
    event: Event,
    key: String,
    source: Source,
    state: PublicationState,
}

impl Publication {
    fn same_event(&self, event: &Event) -> bool {
        self.event.event_id == event.event_id && self.event.revision == event.revision
    }
}

struct Record {
    id: String,
    owner: Owner,
    request_id: String,
    spec: Spec,
    template: Template,
    status: PromptStatus,
    created_at: i64,
    expires_at: i64,
    expires: Instant,
    next: Instant,
    next_at: i64,
    pause_remaining: Duration,
    tick: u64,
    missed: u64,
    last_fired_at: Option<i64>,
    pending: Option<Event>,
    source: Option<Source>,
    publication: Option<Publication>,
    last_ack: Option<(String, u64)>,
    retained_since: Option<Instant>,
}

impl Record {
    fn summary(&self) -> PromptSummary {
        PromptSummary {
            id: self.id.clone(),
            status: self.status,
            scope: "native_connection",
            created_at_unix_ms: self.created_at,
            next_fire_unix_ms: (self.status == PromptStatus::Scheduled).then_some(self.next_at),
            last_fired_at_unix_ms: self.last_fired_at,
            interval_ms: self.spec.interval_ms,
            lifetime_ms: self.spec.lifetime_ms,
            expires_at_unix_ms: self.expires_at,
            tick: self.tick,
            pending_count: u32::from(self.pending.is_some()),
            visibility: self.visibility(),
            missed_count: self.missed,
            retention_after_ack_ms: RETENTION_MS,
        }
    }

    fn visible(&self) -> bool {
        self.pending.as_ref().is_some_and(|event| {
            self.publication.as_ref().is_some_and(|publication| {
                publication.state == PublicationState::Published && publication.same_event(event)
            })
        })
    }

    fn visibility(&self) -> &'static str {
        if self.pending.is_none() {
            return "none";
        }
        match &self.publication {
            Some(publication) => match publication.state {
                PublicationState::InFlight => "in_progress",
                PublicationState::Unknown => "unknown",
                PublicationState::Rejected => "rejected",
                PublicationState::Published if self.visible() => "recorded",
                PublicationState::Published => "queued",
            },
            None => "queued",
        }
    }

    fn publication_ready(&self) -> bool {
        self.source.is_some()
            && self.pending.is_some()
            && self.publication.as_ref().is_none_or(|publication| {
                publication.state == PublicationState::Published && !self.visible()
            })
    }

    fn finish(&mut self, status: PromptStatus, now: Instant) {
        self.status = status;
        if self.pending.is_none() && self.retained_since.is_none() {
            self.retained_since = Some(now);
        }
    }

    fn tick(&mut self, now: Instant, wall: i64) -> bool {
        if !self.status.active() {
            return false;
        }
        let through = now.min(self.expires);
        let mut changed = false;
        if self.status == PromptStatus::Scheduled && self.next <= through {
            let mut count = self.spec.interval_ms.map_or(1, |period| {
                millis(through.duration_since(self.next)) / period + 1
            });
            if let Some(maximum) = self.spec.max_fires {
                count = count.min(maximum.saturating_sub(self.tick));
            }
            if count > 0 {
                let offset = self.spec.interval_ms.unwrap_or(0).saturating_mul(count - 1);
                let scheduled = self.next + Duration::from_millis(offset);
                let scheduled_at = self.next_at + offset as i64;
                let missed = self
                    .pending
                    .as_ref()
                    .map_or(count - 1, |event| event.missed.saturating_add(count));
                self.missed = self
                    .missed
                    .saturating_add(count - 1 + u64::from(self.pending.is_some()));
                self.tick += count;
                let (event_id, revision) = self.pending.as_ref().map_or_else(
                    || (format!("{}_event_{:x}", self.id, self.tick), 1),
                    |event| (event.event_id.clone(), event.revision + 1),
                );
                self.pending = Some(Event {
                    event_id,
                    revision,
                    timer_id: self.id.clone(),
                    provenance: "agent_timer",
                    completion_permission: false,
                    scheduled_at_unix_ms: scheduled_at,
                    fired_at_unix_ms: wall,
                    delay_ms: millis(now.saturating_duration_since(scheduled)),
                    tick: self.tick,
                    missed,
                    body: self
                        .template
                        .render(&self.id, scheduled_at, wall, self.tick, missed),
                });
                self.last_fired_at = Some(wall);
                changed = true;
                if let Some(period) = self.spec.interval_ms {
                    self.next = scheduled + Duration::from_millis(period);
                    self.next_at = scheduled_at + period as i64;
                }
            }
            if self.spec.interval_ms.is_none()
                || self.spec.max_fires.is_some_and(|max| self.tick >= max)
            {
                self.finish(PromptStatus::Completed, now);
                changed = true;
            }
        }
        if now >= self.expires && self.status.active() {
            self.finish(PromptStatus::Expired, now);
            changed = true;
        }
        changed
    }
}

#[derive(Default)]
pub(super) struct State {
    records: BTreeMap<String, Record>,
    next: u64,
}

impl State {
    pub(super) fn tick(&mut self, now: Instant, wall: i64) -> bool {
        let mut changed = false;
        for record in self.records.values_mut() {
            changed |= record.tick(now, wall);
        }
        let before = self.records.len();
        self.records.retain(|_, record| {
            record.retained_since.is_none_or(|at| {
                now.saturating_duration_since(at) < Duration::from_millis(RETENTION_MS)
            })
        });
        changed || before != self.records.len()
    }

    fn next_wake(&self) -> Option<Instant> {
        if self.records.values().any(Record::publication_ready) {
            return Some(Instant::now());
        }
        self.records
            .values()
            .filter_map(|record| match record.status {
                PromptStatus::Scheduled => Some(record.next.min(record.expires)),
                PromptStatus::Paused => Some(record.expires),
                _ => record
                    .retained_since
                    .map(|at| at + Duration::from_millis(RETENTION_MS)),
            })
            .min()
    }

    /// Reserve at most four writes before releasing the clock lock. Unknown
    /// attempts are observed only; their identity/body/source cannot be replaced.
    fn take_publications(&mut self, clock: &str) -> Vec<Publication> {
        let mut batch = Vec::new();
        for record in self
            .records
            .values_mut()
            .filter(|record| record.publication_ready())
            .take(4)
        {
            let event = record
                .pending
                .as_ref()
                .expect("pending publication")
                .clone();
            let mut digest = Sha256::new();
            digest.update(b"ssmcp.timer.input.v1\0");
            digest.update(clock.as_bytes());
            digest.update(b"\0");
            digest.update(event.event_id.as_bytes());
            digest.update(event.revision.to_le_bytes());
            let publication = Publication {
                event,
                key: format!("gi_{}", crate::hex::encode(digest.finalize())),
                source: record.source.as_ref().expect("retained source").clone(),
                state: PublicationState::InFlight,
            };
            record.publication = Some(publication.clone());
            batch.push(publication);
        }
        // An unknown old write cannot starve other timers or cause an immediate
        // wake loop. Fill only the remainder of this bounded batch with reads.
        for publication in self
            .records
            .values()
            .filter_map(|record| record.publication.as_ref())
            .filter(|publication| publication.state == PublicationState::Unknown)
            .take(4 - batch.len())
        {
            batch.push(publication.clone());
        }
        batch
    }

    fn finish_publication(&mut self, attempt: &Publication, outcome: PublicationState) {
        if let Some(record) = self.records.get_mut(&attempt.event.timer_id)
            && let Some(publication) = &mut record.publication
            && publication.key == attempt.key
            && publication.same_event(&attempt.event)
        {
            // This marker describes only the original revision, even if a tick
            // coalesced a newer pending body during display work.
            publication.state = outcome;
        }
    }

    fn read_events(
        &mut self,
        owner: &Owner,
        timer_id: Option<&str>,
        limit: usize,
        visible_only: bool,
    ) -> Result<Value, String> {
        if !(1..=32).contains(&limit) {
            return Err("read limit must be 1..=32".into());
        }
        if let Some(id) = timer_id {
            self.owned(owner, id)?;
        }
        let mut deferred = 0usize;
        let mut events: Vec<_> = self
            .records
            .values()
            .filter_map(|record| {
                if &record.owner != owner || timer_id.is_some_and(|id| id != record.id) {
                    return None;
                }
                let event = record.pending.as_ref()?;
                if visible_only && !record.visible() {
                    deferred += 1;
                    return None;
                }
                Some(event)
            })
            .collect();
        events.sort_by(|a, b| {
            a.fired_at_unix_ms
                .cmp(&b.fired_at_unix_ms)
                .then(a.event_id.cmp(&b.event_id))
        });
        let more = events.len() > limit;
        events.truncate(limit);
        Ok(json!({"events":events,"more":more,"ack_required":true,
            "visibility_deferred":deferred}))
    }

    pub(super) fn summaries(&self, owner: &Option<super::HistoryOwner>) -> Vec<PromptSummary> {
        self.records
            .values()
            .filter(|record| record.owner.visible(owner))
            .map(Record::summary)
            .collect()
    }

    pub(super) fn disconnect(&mut self) {
        // This native connection is the retention boundary, not the HTTP future.
        for record in self.records.values_mut() {
            record.status = PromptStatus::Disconnected;
        }
        self.records.clear();
    }

    fn owned(&mut self, owner: &Owner, id: &str) -> Result<&mut Record, String> {
        self.records
            .get_mut(id)
            .filter(|record| &record.owner == owner)
            .ok_or_else(|| {
                "unknown prompt timer for this exact caller and native connection".into()
            })
    }

    fn request(
        &mut self,
        owner: Owner,
        clock: &str,
        request: Request,
        now: Instant,
        wall: i64,
    ) -> Result<Value, String> {
        let value = match request {
            Request::Schedule {
                request_id,
                template,
                variables,
                after_ms,
                at_unix_ms,
                interval_ms,
                max_fires,
                lifetime_ms,
            } => {
                if request_id.is_empty()
                    || request_id.len() > 128
                    || request_id.chars().any(char::is_control)
                {
                    return Err(
                        "request_id must contain 1..=128 bytes without control characters".into(),
                    );
                }
                if !(1..=MAX_LIFETIME_MS).contains(&lifetime_ms) {
                    return Err("lifetime_ms must be 1..=86400000".into());
                }
                if after_ms.is_some() == at_unix_ms.is_some() {
                    return Err("provide exactly one of after_ms or at_unix_ms".into());
                }
                if interval_ms.is_some_and(|period| !(1000..=MAX_LIFETIME_MS).contains(&period)) {
                    return Err("interval_ms must be 1000..=86400000".into());
                }
                if max_fires == Some(0) || max_fires.is_some_and(|max| max > 86_401) {
                    return Err("max_fires must be 1..=86401".into());
                }
                if interval_ms.is_none() && max_fires.is_some_and(|max| max != 1) {
                    return Err("max_fires greater than one requires interval_ms".into());
                }
                let spec = Spec {
                    template,
                    variables,
                    after_ms,
                    at_unix_ms,
                    interval_ms,
                    max_fires: if interval_ms.is_none() {
                        Some(1)
                    } else {
                        max_fires
                    },
                    lifetime_ms,
                };
                // Deduplicate before converting relative time, checking capacity,
                // or comparing an old absolute deadline with the current clock.
                if let Some(record) = self
                    .records
                    .values()
                    .find(|record| record.owner == owner && record.request_id == request_id)
                {
                    if record.spec != spec {
                        return Err("request_id_conflict: this request_id already names a different schedule".into());
                    }
                    return Ok(envelope(
                        json!({"duplicate":true,"schedule":record.summary()}),
                    ));
                }
                let compiled = Template::compile(&spec.template, &spec.variables)?;
                let scheduled_at = match (after_ms, at_unix_ms) {
                    (Some(delay), None) if delay <= lifetime_ms => {
                        wall.checked_add(delay as i64)
                            .ok_or("schedule timestamp overflow")?
                    }
                    (None, Some(at)) if (civil::MIN_UNIX_MS..=civil::MAX_UNIX_MS).contains(&at) => {
                        at
                    }
                    _ => return Err(
                        "schedule must be a supported UTC timestamp or after_ms within lifetime_ms"
                            .into(),
                    ),
                };
                let delay = scheduled_at.saturating_sub(wall).max(0) as u64;
                if delay > lifetime_ms {
                    return Err("scheduled time exceeds this schedule's lifetime".into());
                }
                let next = if scheduled_at >= wall {
                    now + Duration::from_millis(delay)
                } else {
                    now.checked_sub(Duration::from_millis(
                        wall.saturating_sub(scheduled_at) as u64
                    ))
                    .ok_or("absolute prompt timestamp is outside the monotonic clock range")?
                };
                if self.records.len() >= MAX_RECORDS
                    || self
                        .records
                        .values()
                        .filter(|record| record.status.active())
                        .count()
                        >= MAX_SCHEDULES
                {
                    return Err("prompt_capacity: at most 32 active schedules and 128 retained records; acknowledge pending events".into());
                }
                self.next = self
                    .next
                    .checked_add(1)
                    .ok_or("prompt identifier space exhausted")?;
                let id = format!("prompt_{clock}_{:x}", self.next);
                let record = Record {
                    id: id.clone(),
                    owner,
                    request_id,
                    spec,
                    template: compiled,
                    status: PromptStatus::Scheduled,
                    created_at: wall,
                    expires_at: wall
                        .checked_add(lifetime_ms as i64)
                        .ok_or("lifetime timestamp overflow")?,
                    expires: now + Duration::from_millis(lifetime_ms),
                    next,
                    next_at: scheduled_at,
                    pause_remaining: Duration::ZERO,
                    tick: 0,
                    missed: 0,
                    last_fired_at: None,
                    pending: None,
                    source: None,
                    publication: None,
                    last_ack: None,
                    retained_since: None,
                };
                let summary = record.summary();
                self.records.insert(id, record);
                json!({"duplicate":false,"schedule":summary})
            }
            Request::List => {
                json!({"schedules":self.records.values().filter(|record| record.owner == owner).map(Record::summary).collect::<Vec<_>>()})
            }
            Request::Status { timer_id } => {
                json!({"schedule":self.owned(&owner, &timer_id)?.summary()})
            }
            Request::Read { timer_id, limit } => {
                self.read_events(&owner, timer_id.as_deref(), limit, false)?
            }
            Request::Pause { timer_id } => {
                let record = self.owned(&owner, &timer_id)?;
                if record.status == PromptStatus::Scheduled {
                    record.pause_remaining = record.next.saturating_duration_since(now);
                    record.status = PromptStatus::Paused;
                }
                json!({"schedule":record.summary()})
            }
            Request::Resume { timer_id } => {
                let record = self.owned(&owner, &timer_id)?;
                if record.status == PromptStatus::Paused {
                    record.next = now + record.pause_remaining;
                    record.next_at = wall + millis(record.pause_remaining) as i64;
                    record.status = PromptStatus::Scheduled;
                }
                json!({"schedule":record.summary()})
            }
            Request::Cancel { timer_id } => {
                let record = self.owned(&owner, &timer_id)?;
                if record.status.active() {
                    record.finish(PromptStatus::Cancelled, now);
                }
                json!({"schedule":record.summary()})
            }
            Request::AckTask { .. } => unreachable!("handled by the connection clock"),
            Request::Ack {
                timer_id,
                event_id,
                revision,
            } => {
                let record = self.owned(&owner, &timer_id)?;
                let duplicate = record
                    .last_ack
                    .as_ref()
                    .is_some_and(|(id, rev)| id == &event_id && *rev == revision);
                if !duplicate {
                    let pending = record
                        .pending
                        .as_ref()
                        .ok_or("unknown prompt event or revision")?;
                    if pending.event_id != event_id || pending.revision != revision {
                        return Err("stale_prompt_ack: latest event remains pending; read and process its current revision".into());
                    }
                    record.pending = None;
                    record.last_ack = Some((event_id, revision));
                    if !record.status.active() {
                        record.retained_since = Some(now);
                    }
                }
                json!({"acknowledged":true,"duplicate":duplicate,"schedule":record.summary()})
            }
        };
        Ok(envelope(value))
    }
}

fn envelope(mut value: Value) -> Value {
    let object = value.as_object_mut().expect("internal object");
    object.insert("schema_version".into(), json!(1));
    object.insert("provenance".into(), json!("agent_timer"));
    object.insert("scope".into(), json!("native_connection"));
    object.insert("completion_permission".into(), json!(false));
    value
}

impl Clock {
    pub(super) fn prompt(self: &Arc<Self>, arguments: Value) -> Result<Value, String> {
        let source = Source::capture();
        let owner = Owner::from_source(&source);
        let hook_owner = Owner::for_task_hook(&source);
        let request: Request = serde_json::from_value(arguments)
            .map_err(|error| format!("invalid prompt request: {error}"))?;
        let read = match &request {
            Request::Read { timer_id, limit } => Some((timer_id.clone(), *limit)),
            _ => None,
        };
        let scheduling = matches!(request, Request::Schedule { .. });
        let now = Instant::now();
        let wall = civil::now_ms()?;
        let mut result = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return Err(
                    "prompt_native_connection_closed: schedules cannot move to a new connection"
                        .into(),
                );
            }
            if let Request::AckTask { job_id } = &request {
                let hook_owner = hook_owner
                    .as_ref()
                    .ok_or("unknown task-failure hook for this exact caller and connection")?;
                return state.memory_hooks.ack(hook_owner, job_id).map(envelope);
            }
            state.prompts.tick(now, wall);
            let result = state
                .prompts
                .request(owner.clone(), &self.id, request, now, wall);
            if scheduling
                && let Ok(value) = &result
                && let Some(id) = value["schedule"]["id"].as_str()
                && let Some(record) = state.prompts.records.get_mut(id)
                && record.source.is_none()
            {
                record.source = Some(source);
            }
            result
        };
        // Both entrypoints publish outside the Clock lock, through the same UI
        // message serializer. Schedule mutation remains valid if display rejects.
        self.publish_due();
        if result.is_ok()
            && let Some((timer_id, limit)) = read
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            result = state
                .prompts
                .read_events(&owner, timer_id.as_deref(), limit, true)
                .map(|mut page| {
                    if timer_id.is_none()
                        && let Some(hook_owner) = &hook_owner
                    {
                        for notice in state.memory_hooks.read(hook_owner, limit) {
                            let items = page
                                .as_object_mut()
                                .expect("prompt page")
                                .entry(notice.page_key())
                                .or_insert_with(|| json!([]));
                            items
                                .as_array_mut()
                                .expect("notice list")
                                .push(json!(notice));
                        }
                    }
                    envelope(page)
                });
        }
        self.start_prompt_scheduler();
        self.prompt_changed.notify_one();
        super::changed();
        result
    }

    fn publish_due(&self) {
        use crate::client::interface::{history, messages::InputOrigin, publish_message};
        let batch = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return;
            }
            state.prompts.take_publications(&self.id)
        };
        for attempt in batch {
            if self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .closed
            {
                break;
            }
            let outcome = if attempt.state == PublicationState::Unknown {
                let confirmed = attempt.source.require_owner().ok().is_some_and(|owner| {
                    history::request_identity(&attempt.key, "generated_input")
                        .ok()
                        .and_then(|id| {
                            history::recorded_input(
                                owner,
                                &id,
                                &attempt.event.body,
                                InputOrigin::AgentTimer,
                            )
                            .ok()
                        })
                        == Some(true)
                });
                if confirmed {
                    PublicationState::Published
                } else {
                    PublicationState::Unknown
                }
            } else {
                // Unwinding after a native commit cannot be called rejection.
                // The retained attempt will only be reconciled by positive evidence.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    publish_message(
                        json!({"kind":"input","input_origin":"agent_timer",
                        "idempotency_key":attempt.key,"text":attempt.event.body}),
                        attempt.source.clone(),
                    )
                })) {
                    Ok(Ok(value))
                        if value["accepted"] == true
                            && value["recorded"] == true
                            && value["kind"] == "input"
                            && value["input_origin"] == "agent_timer"
                            && value["completion_permission"] == false =>
                    {
                        PublicationState::Published
                    }
                    // Canonical publisher Result errors occur before the input
                    // history commit; all later display/receipt code is infallible.
                    Ok(Err(_)) => PublicationState::Rejected,
                    _ => PublicationState::Unknown,
                }
            };
            self.state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .prompts
                .finish_publication(&attempt, outcome);
        }
    }

    fn start_prompt_scheduler(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.prompt_scheduler_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let clock = Arc::downgrade(self);
        let changed = self.prompt_changed.clone();
        tokio::spawn(async move {
            loop {
                let Some(clock) = clock.upgrade() else {
                    break;
                };
                // One blocking batch, never one task per timer. No Clock strong
                // reference survives into the sleep below.
                let next = tokio::task::spawn_blocking(move || {
                    let mut state = clock
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if state.closed {
                        return None;
                    }
                    let Ok(wall) = civil::now_ms() else {
                        return Some(Instant::now() + Duration::from_secs(1));
                    };
                    let updated = state.prompts.tick(Instant::now(), wall);
                    drop(state);
                    clock.publish_due();
                    if updated {
                        super::changed();
                    }
                    let state = clock
                        .state
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    state.prompts.next_wake()
                })
                .await;
                let Ok(next) = next else {
                    break;
                };
                if let Some(deadline) = next {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {},
                        _ = changed.notified() => {},
                    }
                } else {
                    changed.notified().await;
                }
            }
        });
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.prompt_changed.notify_one();
    }
}

pub(super) fn tool() -> Value {
    json!({"name":TOOL,
        "description":"Schedule bounded agent-authored prompt hooks, without executing code or invoking a model. Read returns confirmed memory-limit kills in memory_limit_hooks and fatal task signals in task_crash_hooks to the exact originating agent. After processing, use action ack_task with its job_id (ack_memory remains supported). Each connection keeps the latest 128 notices; replies and acknowledgements are idempotent. Required action: schedule/list/status/pause/resume/cancel/read/ack/ack_task. schedule needs request_id, template, and exactly one of after_ms or at_unix_ms; it returns immediately. Same request_id and normalized arguments return the retained schedule; conflicting reuse fails. Optional interval_ms>=1000, max_fires counts logical ticks, lifetime_ms=1..86400000 (default24h). No interval means one shot. Template syntax is literal {{name}} substitution only, never eval, shell or recursive expansion. Scalar JSON variables: <=32, <=4096 bytes total, <=1024 bytes/value. Reserved builtins timer_id, scheduled_at, fired_at, tick, missed; time builtins are decimal Unix milliseconds. Template<=8192 bytes, rendered<=16384. One Clock scheduler queues due events independently of the caller. Late/unacknowledged interval ticks coalesce to one latest event; no catch-up burst. read returns stable event_id+revision, scheduled/actual fire and monotonic delay; acknowledge only after processing with exact timer_id,event_id,revision. Stale ACK leaves newer revisions pending. Pause freezes remaining next delay but not lifetime; cancel only stops future ticks. At most32 active schedules/128 retained records; unacked events are protected; acknowledged terminal metadata/idempotency retained300s. Original native connection+account+bound participant+conversation only, no target argument. HTTP disconnect retains events; native session disconnect/reload destroys this namespace, never remaps IDs. A supporting relay appends due events as prompt_hooks on eligible same-binding responses; older relays require explicit read. Provenance is agent_timer, never human input or approval. Requires negotiated time_observation_v2 and native prompt tool advertisement.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["action"],"properties":{
            "action":{"enum":["schedule","list","status","pause","resume","cancel","read","ack","ack_memory","ack_task"]},
            "request_id":{"type":"string","minLength":1,"maxLength":128},"template":{"type":"string","minLength":1,"maxLength":MAX_TEMPLATE_BYTES},
            "variables":{"type":"object","maxProperties":32,"additionalProperties":{"type":["string","number","boolean","null"]}},
            "after_ms":{"type":"integer","minimum":0,"maximum":MAX_LIFETIME_MS},"at_unix_ms":{"type":"integer","minimum":civil::MIN_UNIX_MS,"maximum":civil::MAX_UNIX_MS},
            "interval_ms":{"type":"integer","minimum":1000,"maximum":MAX_LIFETIME_MS},"max_fires":{"type":"integer","minimum":1,"maximum":86401},
            "lifetime_ms":{"type":"integer","minimum":1,"maximum":MAX_LIFETIME_MS,"default":MAX_LIFETIME_MS},
            "job_id":{"type":"string","minLength":1,"maxLength":96},
            "timer_id":{"type":"string","minLength":1,"maxLength":128},"event_id":{"type":"string","minLength":1,"maxLength":160},
            "revision":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":32,"default":4}},
            "oneOf":[
                {"properties":{"action":{"const":"schedule"}},"required":["request_id","template"],"oneOf":[{"required":["after_ms"],"not":{"required":["at_unix_ms"]}},{"required":["at_unix_ms"],"not":{"required":["after_ms"]}}]},
                {"properties":{"action":{"const":"list"}}},
                {"properties":{"action":{"enum":["status","pause","resume","cancel"]}},"required":["timer_id"]},
                {"properties":{"action":{"const":"read"}}},
                {"properties":{"action":{"enum":["ack_memory","ack_task"]}},"required":["job_id"]},
                {"properties":{"action":{"const":"ack"}},"required":["timer_id","event_id","revision"]}]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(clock: &Arc<Clock>, arguments: Value) -> Result<Value, String> {
        Source::default().blocking(|| clock.prompt(arguments))
    }

    fn owner(connection: u64) -> Owner {
        Owner {
            connection: Some(connection),
            attribution: None,
            thread: None,
            history: None,
        }
    }
    fn schedule(id: &str, after: u64, interval: Option<u64>) -> Request {
        Request::Schedule {
            request_id: id.into(),
            template: "{{tick}}/{{missed}} {{scheduled_at}} {{fired_at}}".into(),
            variables: BTreeMap::new(),
            after_ms: Some(after),
            at_unix_ms: None,
            interval_ms: interval,
            max_fires: None,
            lifetime_ms: MAX_LIFETIME_MS,
        }
    }
    fn add(state: &mut State, owner: Owner, request: Request, now: Instant) -> String {
        state
            .request(owner, "test", request, now, 1_000_000)
            .unwrap()["schedule"]["id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn read(state: &mut State, owner: Owner, id: &str, now: Instant) -> Value {
        state
            .request(
                owner,
                "test",
                Request::Read {
                    timer_id: Some(id.into()),
                    limit: 4,
                },
                now,
                1_000_000,
            )
            .unwrap()["events"]
            .clone()
    }

    #[test]
    fn templates_are_literal_bounded_and_reject_missing_reserved_or_nested_variables() {
        let variables = BTreeMap::from([
            ("value".into(), json!("{{tick}} $(false)")),
            ("n".into(), json!(7)),
        ]);
        let template = Template::compile("{{value}} {{n}} {{tick}}", &variables).unwrap();
        assert_eq!(template.render("id", 1, 2, 3, 0), "{{tick}} $(false) 7 3");
        for (text, vars, error) in [
            ("{{missing}}", BTreeMap::new(), "missing variable"),
            (
                "x",
                BTreeMap::from([("tick".into(), json!(1))]),
                "reserved variable",
            ),
            (
                "x",
                BTreeMap::from([("v".into(), json!({"a":1}))]),
                "scalar JSON",
            ),
            ("{{x.y}}", BTreeMap::new(), "invalid template variable"),
            ("{{x", BTreeMap::new(), "unclosed"),
        ] {
            assert!(
                Template::compile(text, &vars)
                    .err()
                    .unwrap()
                    .contains(error)
            );
        }
        assert!(Template::compile(&"a".repeat(8193), &BTreeMap::new()).is_err());
        assert!(Template::compile(&"{{timer_id}}".repeat(129), &BTreeMap::new()).is_err());
    }

    #[test]
    fn create_deduplicates_before_time_conversion_and_rejects_changed_reuse() {
        let now = Instant::now();
        let mut state = State::default();
        let id = add(&mut state, owner(1), schedule("same", 10, None), now);
        let again = state
            .request(
                owner(1),
                "test",
                schedule("same", 10, None),
                now + Duration::from_secs(1),
                2_000_000,
            )
            .unwrap();
        assert_eq!(again["duplicate"], true);
        assert_eq!(again["schedule"]["id"], id);
        assert!(
            state
                .request(owner(1), "test", schedule("same", 11, None), now, 1_000_000)
                .unwrap_err()
                .contains("request_id_conflict")
        );
        assert_ne!(
            add(&mut state, owner(2), schedule("same", 10, None), now),
            id
        );
        assert!(
            state
                .request(
                    owner(2),
                    "test",
                    Request::Status { timer_id: id },
                    now,
                    1_000_000
                )
                .is_err()
        );
    }

    #[test]
    fn late_ticks_coalesce_and_stale_ack_cannot_discard_a_new_revision() {
        let now = Instant::now();
        let mut state = State::default();
        let id = add(
            &mut state,
            owner(1),
            schedule("repeat", 1000, Some(1000)),
            now,
        );
        state.tick(now + Duration::from_millis(5500), 1_005_500);
        let first = read(&mut state, owner(1), &id, now)[0].clone();
        assert_eq!(first["tick"], 5);
        assert_eq!(first["missed"], 4);
        assert_eq!(first["delay_ms"], 500);
        assert_eq!(first["provenance"], "agent_timer");
        assert_eq!(first["completion_permission"], false);
        state.tick(now + Duration::from_secs(6), 1_006_000);
        let latest = read(&mut state, owner(1), &id, now)[0].clone();
        assert_eq!(first["event_id"], latest["event_id"]);
        assert_eq!(latest["revision"], 2);
        assert_eq!(latest["missed"], 5);
        let ack = |revision| Request::Ack {
            timer_id: id.clone(),
            event_id: first["event_id"].as_str().unwrap().into(),
            revision,
        };
        assert!(
            state
                .request(owner(1), "test", ack(1), now, 1_000_000)
                .unwrap_err()
                .contains("stale_prompt_ack")
        );
        assert_eq!(read(&mut state, owner(1), &id, now)[0], latest);
        assert_eq!(
            state
                .request(owner(1), "test", ack(2), now, 1_000_000)
                .unwrap()["acknowledged"],
            true
        );
        assert_eq!(
            state
                .request(owner(1), "test", ack(2), now, 1_000_000)
                .unwrap()["duplicate"],
            true
        );
        state.tick(now + Duration::from_secs(7), 1_007_000);
        assert_ne!(
            read(&mut state, owner(1), &id, now)[0]["event_id"],
            first["event_id"]
        );
        state
            .request(owner(1), "test", ack(2), now, 1_000_000)
            .unwrap();
        assert_eq!(
            read(&mut state, owner(1), &id, now)
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn pause_cancel_lifetime_and_unacked_retention_are_independent() {
        let now = Instant::now();
        let mut state = State::default();
        let id = add(
            &mut state,
            owner(1),
            schedule("paused", 1000, Some(1000)),
            now,
        );
        state
            .request(
                owner(1),
                "test",
                Request::Pause {
                    timer_id: id.clone(),
                },
                now + Duration::from_millis(400),
                1_000_400,
            )
            .unwrap();
        state.tick(now + Duration::from_secs(10), 1_010_000);
        assert!(
            read(&mut state, owner(1), &id, now)
                .as_array()
                .unwrap()
                .is_empty()
        );
        state
            .request(
                owner(1),
                "test",
                Request::Resume {
                    timer_id: id.clone(),
                },
                now + Duration::from_secs(10),
                1_010_000,
            )
            .unwrap();
        state.tick(now + Duration::from_millis(10600), 1_010_600);
        assert_eq!(
            read(&mut state, owner(1), &id, now)[0]["scheduled_at_unix_ms"],
            1_010_600
        );
        state
            .request(
                owner(1),
                "test",
                Request::Cancel {
                    timer_id: id.clone(),
                },
                now + Duration::from_secs(11),
                1_011_000,
            )
            .unwrap();
        state.tick(now + Duration::from_secs(100_000), 101_000_000);
        assert_eq!(state.records[&id].status, PromptStatus::Cancelled);
        assert!(state.records[&id].pending.is_some());
        state.disconnect();
        assert!(state.records.is_empty());
    }

    #[test]
    fn limits_and_metadata_privacy_are_enforced() {
        let now = Instant::now();
        let mut state = State::default();
        for i in 0..32 {
            add(
                &mut state,
                owner(1),
                schedule(&i.to_string(), 1000, None),
                now,
            );
        }
        assert!(
            state
                .request(
                    owner(2),
                    "test",
                    schedule("full", 1000, None),
                    now,
                    1_000_000
                )
                .unwrap_err()
                .contains("prompt_capacity")
        );
        let display = serde_json::to_string(&state.summaries(&None)).unwrap();
        for private in ["template", "variables", "body", "request_id"] {
            assert!(!display.contains(private));
        }
        assert!(
            serde_json::from_value::<Request>(json!({"action":"read","target":"other-thread"}))
                .is_err()
        );
    }

    #[tokio::test]
    async fn pump_fires_without_a_reader_and_survives_the_scheduling_caller() {
        let clock = Clock::new().unwrap();
        let caller = {
            let clock = clock.clone();
            tokio::spawn(async move {
                prompt(&clock, json!({"action":"schedule","request_id":"independent","after_ms":10,"template":"ping"})).unwrap();
                std::future::pending::<()>().await;
            })
        };
        tokio::task::yield_now().await;
        caller.abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if clock
                    .state
                    .lock()
                    .unwrap()
                    .prompts
                    .records
                    .values()
                    .any(|record| record.pending.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        {
            let state = clock.state.lock().unwrap();
            assert_eq!(state.prompts.records.len(), 1);
            assert!(
                state
                    .prompts
                    .records
                    .values()
                    .next()
                    .unwrap()
                    .pending
                    .is_some()
            );
        }
        clock.close();
        tokio::task::yield_now().await;
        assert!(clock.state.lock().unwrap().prompts.records.is_empty());
    }

    #[tokio::test]
    async fn unowned_timer_retains_event_without_silent_model_delivery() {
        let clock = Clock::new().unwrap();
        let created = prompt(
            &clock,
            json!({"action":"schedule","request_id":"real","after_ms":10,"template":"ping"}),
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if clock
                    .state
                    .lock()
                    .unwrap()
                    .prompts
                    .records
                    .values()
                    .any(|record| record.pending.is_some() && record.visibility() == "rejected")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let args = json!({"action":"read","timer_id":created["schedule"]["id"]});
        let first = prompt(&clock, args.clone()).unwrap();
        let retry = prompt(&clock, args).unwrap();
        assert_eq!(first, retry);
        assert_eq!(first["events"], json!([]));
        assert_eq!(first["visibility_deferred"], 1);
        let state = clock.state.lock().unwrap();
        let record = state.prompts.records.values().next().unwrap();
        assert_eq!(record.pending.as_ref().unwrap().body, "ping");
        assert!(record.last_ack.is_none());
        assert_eq!(record.visibility(), "rejected");
        drop(state);
        clock.close();
    }

    #[test]
    fn publication_unknown_is_observe_only_and_cannot_ack_or_cover_a_new_revision() {
        let now = Instant::now();
        let mut state = State::default();
        let original = owner(1);
        let id = add(
            &mut state,
            original.clone(),
            schedule("visible", 0, Some(1000)),
            now,
        );
        state.records.get_mut(&id).unwrap().source = Some(Source::default());
        state.tick(now, 1_000_000);
        let attempt = state.take_publications("test").pop().unwrap();
        assert!(attempt.key.len() <= 96);
        let pending = serde_json::to_value(&state.records[&id].pending).unwrap();
        let status = state.records[&id].status;
        let tick = state.records[&id].tick;
        state.finish_publication(&attempt, PublicationState::Unknown);
        for _ in 0..3 {
            let observed = state.take_publications("test");
            assert_eq!(observed.len(), 1);
            assert_eq!(observed[0].state, PublicationState::Unknown);
            assert_eq!(observed[0].key, attempt.key);
            assert_eq!(
                state.read_events(&original, Some(&id), 4, true).unwrap()["events"],
                json!([])
            );
        }
        assert_eq!(
            serde_json::to_value(&state.records[&id].pending).unwrap(),
            pending
        );
        assert!(state.records[&id].last_ack.is_none());
        assert_eq!(state.records[&id].status, status);
        assert_eq!(state.records[&id].tick, tick);
        state.tick(now + Duration::from_secs(1), 1_001_000);
        state.finish_publication(&attempt, PublicationState::Published);
        assert!(!state.records[&id].visible());
        assert_eq!(state.records[&id].pending.as_ref().unwrap().revision, 2);
        assert!(state.records[&id].last_ack.is_none());
        let newer = state.take_publications("test").pop().unwrap();
        assert_ne!(newer.key, attempt.key);
        state.finish_publication(&newer, PublicationState::Rejected);
        assert!(state.take_publications("test").is_empty());
        assert_eq!(state.records[&id].pending.as_ref().unwrap().revision, 2);
    }

    #[tokio::test]
    async fn timer_publication_keeps_original_history_while_another_history_is_selected() {
        use crate::client::interface::history;
        let clock = Clock::new().unwrap();
        let root = std::env::temp_dir().join(format!("ssmcp-visible-timer-{}", clock.id));
        history::with_test_history(root.join("a"), async {
            let source = Source::capture();
            let owner_a = source.require_owner().unwrap().clone();
            let created = source.clone().blocking(|| clock.prompt(json!({
                "action":"schedule","request_id":"original","after_ms":10,"template":"original timer"
            }))).unwrap();
            history::with_test_history(root.join("b"), async {
                let owner_b = Source::capture().require_owner().unwrap().clone();
                history::record_owned_request(&owner_b, "b_message", "B stays selected").unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        if clock.state.lock().unwrap().prompts.records.values().any(Record::visible) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.unwrap();
                let args = json!({"action":"read","timer_id":created["schedule"]["id"]});
                let first = source.clone().blocking(|| clock.prompt(args.clone())).unwrap();
                let retry = source.clone().blocking(|| clock.prompt(args)).unwrap();
                assert_eq!(first, retry);
                assert_eq!(first["events"][0]["body"], "original timer");
                assert_eq!(first["visibility_deferred"], 0);
                let other = history::read(owner_b.id()).unwrap();
                assert_eq!(other.messages.len(), 1);
                assert_eq!(other.messages[0].content, "B stays selected");
                assert_eq!(history::current_owner(), Some(owner_b));
            }).await;
            let original = history::read(owner_a.id()).unwrap();
            assert_eq!(original.messages.len(), 1);
            assert_eq!(original.messages[0].role, "tool");
            assert_eq!(original.messages[0].input_origin,
                Some(crate::client::interface::messages::InputOrigin::AgentTimer));
            assert_eq!(original.messages[0].content, "original timer");
            assert!(clock.state.lock().unwrap().prompts.records.values()
                .all(|record| record.pending.is_some() && record.last_ack.is_none()));
        }).await;
        clock.close();
    }

    #[test]
    fn account_and_participant_are_separate_from_connection_and_display_labels() {
        use crate::wire::{ControlAccount, ControlAgent, ControlAttribution};
        let named = |subject: &str, participant: &str| Owner {
            connection: Some(1),
            attribution: Some(ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: "same display".into(),
                },
                owner: None,
                agent: Some(ControlAgent {
                    id: participant.into(),
                    name: "same display".into(),
                }),
            }),
            thread: None,
            history: None,
        };
        let now = Instant::now();
        let mut state = State::default();
        let original = named("account-a", "agent-a");
        let id = add(
            &mut state,
            original.clone(),
            schedule("owner", 0, None),
            now,
        );
        state.tick(now, 1_000_000);
        let event = read(&mut state, original.clone(), &id, now)[0].clone();
        for other in [named("account-b", "agent-a"), named("account-a", "agent-b")] {
            assert!(
                state
                    .request(
                        other.clone(),
                        "test",
                        Request::Read {
                            timer_id: Some(id.clone()),
                            limit: 4
                        },
                        now,
                        1_000_000
                    )
                    .is_err()
            );
            assert!(
                state
                    .request(
                        other.clone(),
                        "test",
                        Request::Cancel {
                            timer_id: id.clone()
                        },
                        now,
                        1_000_000
                    )
                    .is_err()
            );
            assert!(
                state
                    .request(
                        other,
                        "test",
                        Request::Ack {
                            timer_id: id.clone(),
                            event_id: event["event_id"].as_str().unwrap().into(),
                            revision: 1
                        },
                        now,
                        1_000_000
                    )
                    .is_err()
            );
        }
        assert_eq!(read(&mut state, original, &id, now)[0], event);
    }

    #[test]
    fn renamed_display_labels_preserve_timer_dedup_read_cancel_and_ack() {
        use crate::wire::{ControlAccount, ControlAgent, ControlAttribution};
        let mut original = owner(7);
        original.attribution = Some(ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "123".into(),
                login: "controller-before".into(),
            },
            owner: Some(ControlAccount {
                provider: "github".into(),
                subject: "456".into(),
                login: "device-owner-before".into(),
            }),
            agent: Some(ControlAgent {
                id: format!("agent_{}", "a".repeat(32)),
                name: "participant-before".into(),
            }),
        });
        let mut renamed = original.clone();
        let attribution = renamed.attribution.as_mut().unwrap();
        attribution.account.login = "controller-after".into();
        attribution.owner.as_mut().unwrap().login = "device-owner-after".into();
        attribution.agent.as_mut().unwrap().name = "participant-after".into();
        assert_ne!(original.attribution, renamed.attribution);
        assert_eq!(original, renamed);

        let now = Instant::now();
        let mut state = State::default();
        let id = add(
            &mut state,
            original.clone(),
            schedule("rename", 0, Some(1000)),
            now,
        );
        state.tick(now, 1_000_000);
        let event = read(&mut state, original, &id, now)[0].clone();
        assert_eq!(
            add(
                &mut state,
                renamed.clone(),
                schedule("rename", 0, Some(1000)),
                now
            ),
            id
        );
        assert_eq!(state.records.len(), 1);
        assert_eq!(read(&mut state, renamed.clone(), &id, now)[0], event);

        let mut other_device_owner = renamed.clone();
        other_device_owner
            .attribution
            .as_mut()
            .unwrap()
            .owner
            .as_mut()
            .unwrap()
            .subject = "999".into();
        assert!(
            state
                .request(
                    other_device_owner,
                    "test",
                    Request::Read {
                        timer_id: Some(id.clone()),
                        limit: 4,
                    },
                    now,
                    1_000_000
                )
                .is_err()
        );

        state
            .request(
                renamed.clone(),
                "test",
                Request::Cancel {
                    timer_id: id.clone(),
                },
                now,
                1_000_000,
            )
            .unwrap();
        assert_eq!(state.records[&id].status, PromptStatus::Cancelled);
        state
            .request(
                renamed.clone(),
                "test",
                Request::Ack {
                    timer_id: id.clone(),
                    event_id: event["event_id"].as_str().unwrap().into(),
                    revision: event["revision"].as_u64().unwrap(),
                },
                now,
                1_000_000,
            )
            .unwrap();
        assert!(
            read(&mut state, renamed, &id, now)
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn finite_fires_expiry_and_past_absolute_deadline_keep_truthful_timing() {
        let now = Instant::now();
        let mut state = State::default();
        let request = Request::Schedule {
            request_id: "past".into(),
            template: "{{tick}}".into(),
            variables: BTreeMap::new(),
            after_ms: None,
            at_unix_ms: Some(995_000),
            interval_ms: Some(1000),
            max_fires: Some(3),
            lifetime_ms: 5000,
        };
        let id = add(&mut state, owner(1), request, now);
        state.tick(now, 1_000_000);
        let event = read(&mut state, owner(1), &id, now)[0].clone();
        assert_eq!(event["tick"], 3);
        assert_eq!(event["missed"], 2);
        assert_eq!(event["scheduled_at_unix_ms"], 997_000);
        assert_eq!(event["delay_ms"], 3000);
        assert_eq!(state.records[&id].status, PromptStatus::Completed);
        let other = add(
            &mut state,
            owner(1),
            Request::Schedule {
                request_id: "expires".into(),
                template: "tick".into(),
                variables: BTreeMap::new(),
                after_ms: Some(1000),
                at_unix_ms: None,
                interval_ms: Some(1000),
                max_fires: None,
                lifetime_ms: 2500,
            },
            now,
        );
        state.tick(now + Duration::from_secs(10), 1_010_000);
        assert_eq!(state.records[&other].status, PromptStatus::Expired);
        let latest = read(&mut state, owner(1), &other, now)[0].clone();
        assert_eq!(latest["tick"], 2);
        assert_eq!(latest["delay_ms"], 8000);
        state
            .request(
                owner(1),
                "test",
                Request::Ack {
                    timer_id: other.clone(),
                    event_id: latest["event_id"].as_str().unwrap().into(),
                    revision: 1,
                },
                now + Duration::from_secs(10),
                1_010_000,
            )
            .unwrap();
        state.tick(now + Duration::from_secs(311), 1_311_000);
        assert!(!state.records.contains_key(&other));
        assert!(state.records.contains_key(&id));
    }
}
