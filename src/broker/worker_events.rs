//! Retained native worker observations feed the shared event journal.
//! A page is validated before publication; its cursor advances only after every
//! event was retained. Observers never cancel, resubmit, or reclaim worker work.
use super::{Authorization, Broker, Session};
use crate::{
    code_mode::{self, AuditRun, RunBinding},
    wire::Operation,
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

const MAX_SOURCES: usize = 128;
const MAX_OWNER_SOURCES: usize = 16;
const READ_LIMIT: usize = 64;
const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;
const RETAIN_FOR: Duration = Duration::from_secs(900);
const MIN_EMPTY_OBSERVATION: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Key {
    owner: String,
    client_id: String,
    agent_id: String,
    connection: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Cursor {
    generation: String,
    seq: u64,
}

impl Cursor {
    fn parse(value: &Value) -> Result<Self, &'static str> {
        let generation = value["generation"]
            .as_str()
            .filter(|value| generation_id(value))
            .ok_or("worker_events_invalid_cursor")?;
        let seq = sequence(&value["seq"]).ok_or("worker_events_invalid_cursor")?;
        Ok(Self {
            generation: generation.into(),
            seq,
        })
    }
}

struct Progress {
    status: &'static str,
    cursor: Option<Cursor>,
    error: Option<&'static str>,
}

struct Source {
    key: Key,
    touched: Mutex<Instant>,
    token: Mutex<String>,
    progress: Mutex<Progress>,
    active: AtomicBool,
    cancelled: AtomicBool,
    stopped: Notify,
}

impl Source {
    fn new(key: Key, token: &str, now: Instant) -> Self {
        Self {
            key,
            touched: Mutex::new(now),
            token: Mutex::new(token.into()),
            progress: Mutex::new(Progress {
                status: "connecting",
                cursor: None,
                error: None,
            }),
            active: AtomicBool::new(true),
            cancelled: AtomicBool::new(false),
            stopped: Notify::new(),
        }
    }

    fn stop(&self, status: &'static str, error: Option<&'static str>) {
        state(self, status, error);
        self.cancelled.store(true, Ordering::Release);
        self.stopped.notify_waiters();
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(*self.touched.lock().unwrap_or_else(|e| e.into_inner())) >= RETAIN_FOR
    }
}

#[derive(Default)]
pub(crate) struct Sources {
    entries: Mutex<HashMap<Key, Arc<Source>>>,
}

impl Sources {
    pub(crate) fn touch(&self, owner: &str, client_ids: &[String], token: &str) {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for source in entries
            .values()
            .filter(|entry| entry.key.owner == owner && client_ids.contains(&entry.key.client_id))
        {
            *source.touched.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            *source.token.lock().unwrap_or_else(|e| e.into_inner()) = token.into();
        }
    }

    pub(crate) fn snapshot(&self, owner: &str, client_ids: &[String]) -> Value {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let mut values: Vec<_> = entries
            .values()
            .filter(|entry| entry.key.owner == owner && client_ids.contains(&entry.key.client_id))
            .map(|source| {
                let progress = source.progress.lock().unwrap_or_else(|e| e.into_inner());
                json!({"client_id":source.key.client_id,"agent_id":source.key.agent_id,
                "status":progress.status,"cursor":progress.cursor,"error":progress.error})
            })
            .collect();
        values.sort_by(|a, b| {
            (a["client_id"].as_str(), a["agent_id"].as_str())
                .cmp(&(b["client_id"].as_str(), b["agent_id"].as_str()))
        });
        json!(values)
    }

    /// Match the exact connection, so an old teardown cannot stop a replacement
    /// client that happens to reuse its public ID.
    pub(crate) fn close_connection(&self, session: &Session) {
        let pointer = session as *const Session as usize;
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        for source in entries
            .values()
            .filter(|source| source.key.connection == pointer)
        {
            source.stop("disconnected", Some("events_source_disconnected"));
        }
    }
}

fn check_capacity(
    entries: &HashMap<Key, Arc<Source>>,
    owner: &str,
    keys: impl Iterator<Item = Key>,
) -> Result<(), String> {
    let mut unique = std::collections::HashSet::new();
    for key in keys {
        if key.owner != owner {
            return Err("events worker source owner mismatch".into());
        }
        if !entries.contains_key(&key) {
            unique.insert(key);
        }
    }
    if entries.len().saturating_add(unique.len()) > MAX_SOURCES
        || entries
            .keys()
            .filter(|key| key.owner == owner)
            .count()
            .saturating_add(unique.len())
            > MAX_OWNER_SOURCES
    {
        return Err("events native source capacity is full".into());
    }
    Ok(())
}

/// All new source admissions are checked together before any insertion or
/// spawn. A rejected batch cannot strand a connecting entry without a pump.
pub(crate) fn ensure(
    broker: &Arc<Broker>,
    user: &str,
    token: &str,
    owner: &str,
    sources: &[(Arc<Session>, Option<RunBinding>)],
) -> Result<Value, String> {
    let mut requested = HashMap::new();
    for (session, binding) in sources {
        let Some(binding) = binding else {
            continue;
        };
        if binding.owner != owner {
            return Err("events worker source owner mismatch".into());
        }
        let key = Key {
            owner: owner.into(),
            client_id: session.client_id().into(),
            agent_id: binding.agent_id.clone(),
            connection: Arc::as_ptr(session) as usize,
        };
        requested
            .entry(key)
            .or_insert_with(|| (Arc::downgrade(session), binding.clone()));
    }
    let mut entries = broker
        .worker_events
        .entries
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    entries.retain(|_, source| {
        if source.expired(now) {
            source.stop("expired", None);
            false
        } else {
            true
        }
    });
    check_capacity(&entries, owner, requested.keys().cloned())?;
    let mut created = Vec::new();
    let ids: Vec<_> = sources
        .iter()
        .map(|(session, _)| session.client_id().to_owned())
        .collect();
    for (key, (session, binding)) in requested {
        if let Some(source) = entries.get(&key) {
            *source.touched.lock().unwrap_or_else(|e| e.into_inner()) = now;
            *source.token.lock().unwrap_or_else(|e| e.into_inner()) = token.into();
            let mut progress = source.progress.lock().unwrap_or_else(|e| e.into_inner());
            // A new explicit subscribe may resume a failed observation from its
            // last retained cursor. History gaps/overflow require reconciliation.
            if matches!(progress.status, "unavailable" | "expired")
                && !source.cancelled.load(Ordering::Acquire)
                && source
                    .active
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                progress.status = "connecting";
                // Retain the previous error until a new page is verified.
                created.push((source.clone(), session, binding));
            }
            continue;
        }
        let source = Arc::new(Source::new(key.clone(), token, now));
        entries.insert(key, source.clone());
        created.push((source, session, binding));
    }
    drop(entries);
    for (source, session, binding) in created {
        tokio::spawn(pump(
            Arc::downgrade(broker),
            session,
            user.into(),
            source,
            binding,
        ));
    }
    Ok(broker.worker_events.snapshot(owner, &ids))
}

fn state(source: &Source, status: &'static str, error: Option<&'static str>) {
    let mut progress = source.progress.lock().unwrap_or_else(|e| e.into_inner());
    progress.status = status;
    progress.error = error;
}

struct Active(Arc<Source>);
impl Drop for Active {
    fn drop(&mut self) {
        let mut progress = self.0.progress.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(progress.status, "connecting" | "observing") {
            progress.status = "unavailable";
            progress.error = Some("events_source_observation_unknown");
        }
        self.0.active.store(false, Ordering::Release);
    }
}

fn unpack(mut reply: Value) -> Result<Value, &'static str> {
    if reply["isError"] == true {
        let message = reply["content"]
            .as_array()
            .and_then(|parts| parts.iter().find_map(|part| part["text"].as_str()))
            .unwrap_or("");
        return Err(
            if message.contains("worker event cursor belongs to a different pool generation")
                || message.contains("worker event cursor is ahead of the current sequence")
            {
                "events_source_cursor_changed"
            } else if message.contains("worker event sequence exhausted") {
                "events_source_sequence_gap"
            } else if message.contains("unknown tool")
                || message.contains("tool not found")
                || message.contains("tool is not registered")
                || message.contains("tool not registered")
                || message.contains("unsupported tool")
                || message.contains("tool_not_found")
            {
                "worker_events_unsupported"
            } else {
                "worker_events_unavailable"
            },
        );
    }
    reply
        .as_object_mut()
        .and_then(|object| object.remove("structuredContent"))
        .filter(Value::is_object)
        .ok_or("worker_events_invalid_reply")
}

fn generation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn sequence(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value <= MAX_SEQUENCE)
}

fn safe_event<'a>(event: &'a Value, generation: &str) -> Result<(&'a str, Value), &'static str> {
    let kind = event["type"]
        .as_str()
        .ok_or("worker_events_invalid_event")?;
    if !matches!(
        kind,
        "task.submitted"
            | "worker.started"
            | "worker.progress"
            | "worker.cancelling"
            | "worker.completed"
            | "worker.idle"
            | "worker.unknown"
    ) {
        return Err("worker_events_invalid_kind");
    }
    let seq = sequence(&event["seq"])
        .filter(|seq| *seq > 0)
        .ok_or("worker_events_invalid_sequence")?;
    if event["generation"].as_str() != Some(generation) {
        return Err("worker_events_invalid_generation");
    }
    let mut data = serde_json::Map::new();
    for field in ["task_id", "participant_id", "task_status", "activity"] {
        let value = event.get(field).ok_or("worker_events_invalid_metadata")?;
        if !(value.is_null()
            || value.as_str().is_some_and(|text| {
                !text.is_empty() && text.len() <= 256 && !text.chars().any(char::is_control)
            }))
        {
            return Err("worker_events_invalid_metadata");
        }
        data.insert(field.into(), value.clone());
    }
    for field in ["slot", "epoch", "attempt", "at_ms"] {
        let value = event.get(field).ok_or("worker_events_invalid_metadata")?;
        if !(value.is_null() || sequence(value).is_some())
            || (field == "at_ms" && value.is_null())
            || (field == "slot" && value.as_u64().is_some_and(|slot| slot > 127))
            || (field == "epoch" && value.as_u64() == Some(0))
        {
            return Err("worker_events_invalid_metadata");
        }
        data.insert(field.into(), value.clone());
    }
    let status = event["task_status"].as_str();
    let activity = event["activity"].as_str();
    if status.is_some_and(|status| {
        !matches!(
            status,
            "queued" | "working" | "cancelling" | "completed" | "failed" | "cancelled" | "unknown"
        )
    }) || activity
        .is_some_and(|activity| !matches!(activity, "command_start" | "command_end" | "progress"))
        || event["participant_id"]
            .as_str()
            .is_some_and(|id| !generation_id(id))
    {
        return Err("worker_events_invalid_metadata");
    }
    let consistent = match kind {
        "task.submitted" => status == Some("queued"),
        "worker.started" => status == Some("working"),
        "worker.cancelling" => status == Some("cancelling"),
        "worker.completed" => matches!(status, Some("completed" | "failed" | "cancelled")),
        "worker.unknown" => status == Some("unknown"),
        "worker.idle" => status.is_none() && event["task_id"].is_null(),
        "worker.progress" => status.is_some() && activity.is_some(),
        _ => false,
    };
    if !consistent || (kind != "worker.idle" && !event["task_id"].is_string()) {
        return Err("worker_events_conflicting_status");
    }
    data.insert(
        "source_cursor".into(),
        json!({"generation":generation,"seq":seq}),
    );
    Ok((kind, Value::Object(data)))
}

struct NativeEvent {
    kind: String,
    actor: Option<String>,
    seq: u64,
    data: Value,
}

struct Page {
    events: Vec<NativeEvent>,
    next: Cursor,
    has_more: bool,
    closed: bool,
}

fn boolean(value: &Value, field: &str) -> Result<bool, &'static str> {
    value[field].as_bool().ok_or("worker_events_invalid_reply")
}

fn page(value: &Value, previous: Option<&Cursor>) -> Result<Page, &'static str> {
    let generation = value["generation"]
        .as_str()
        .filter(|value| generation_id(value))
        .ok_or("worker_events_invalid_generation")?;
    let next = Cursor::parse(&value["next_cursor"])?;
    let oldest = sequence(&value["oldest_seq"]).ok_or("worker_events_invalid_reply")?;
    let latest = sequence(&value["latest_seq"]).ok_or("worker_events_invalid_reply")?;
    let gap = boolean(value, "gap")?;
    let has_more = boolean(value, "has_more")?;
    let closed = boolean(value, "closed")?;
    boolean(value, "stopping")?;
    let status = value["status"]
        .as_str()
        .ok_or("worker_events_invalid_reply")?;
    if !matches!(status, "events" | "closed" | "empty" | "timed_out") {
        return Err("worker_events_invalid_reply");
    }
    if gap {
        return Err("events_source_gap");
    }
    if next.generation != generation
        || previous.is_some_and(|cursor| cursor.generation != generation)
    {
        return Err("events_source_cursor_changed");
    }
    if oldest == 0
        || oldest > latest.saturating_add(1)
        || next.seq > latest
        || previous.is_some_and(|cursor| next.seq < cursor.seq)
    {
        return Err("worker_events_invalid_cursor");
    }
    let input = value["events"]
        .as_array()
        .filter(|events| events.len() <= READ_LIMIT)
        .ok_or("worker_events_invalid_reply")?;
    if (input.is_empty() && status == "events")
        || (!input.is_empty() && status != "events")
        || (input.is_empty() && closed && status != "closed")
        || (!closed && status == "closed")
    {
        return Err("worker_events_invalid_reply");
    }
    let mut events = Vec::with_capacity(input.len());
    let mut last = Some(previous.map_or(oldest.saturating_sub(1), |cursor| cursor.seq));
    for event in input {
        let (kind, data) = safe_event(event, generation)?;
        let seq = event["seq"].as_u64().expect("validated event sequence");
        if seq < oldest || seq > latest || last.is_some_and(|last| seq != last.saturating_add(1)) {
            return Err("events_source_sequence_gap");
        }
        last = Some(seq);
        events.push(NativeEvent {
            kind: kind.into(),
            actor: event["participant_id"].as_str().map(str::to_owned),
            seq,
            data,
        });
    }
    if let Some(last) = last {
        if next.seq != last {
            return Err("worker_events_invalid_cursor");
        }
    }
    if has_more != (next.seq < latest) || (has_more && events.is_empty()) {
        return Err("worker_events_invalid_cursor");
    }
    Ok(Page {
        events,
        next,
        has_more,
        closed,
    })
}

fn source_key(client_id: &str, generation: &str, seq: u64) -> String {
    let bytes = serde_json::to_vec(&(client_id, generation, seq)).expect("typed cursor serializes");
    format!(
        "worker_{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn error_state(error: &str) -> &'static str {
    match error {
        "events_source_gap" | "events_source_cursor_changed" | "events_source_sequence_gap" => {
            "gap"
        }
        "worker_events_unsupported" => "unsupported",
        _ => "unavailable",
    }
}

async fn authorize(
    broker: &Broker,
    user: &str,
    token: &str,
    session: &Arc<Session>,
    binding: &RunBinding,
) -> Result<Authorization, &'static str> {
    let authorization = broker
        .authenticate(user, token)
        .await
        .ok_or("events_source_authorization_expired")?;
    let current = broker
        .client(user, &authorization, session.client_id())
        .await
        .map_err(|_| "events_source_access_revoked")?;
    let authorization = broker
        .authenticate_now(user, token)
        .ok_or("events_source_authorization_expired")?;
    if !Arc::ptr_eq(&current, session)
        || !session.is_alive()
        || !authorization.is_live()
        || !authorization.permits(session)
        || !broker.authorized_account(&authorization)
        || crate::bridge::owner(user, &authorization).as_deref() != Ok(binding.owner.as_str())
        || session
            .collaboration
            .bind(&binding.owner, &binding.agent_id, &binding.token)
            .is_err()
    {
        return Err("events_source_access_revoked");
    }
    Ok(authorization)
}

fn observation_run(session: &Arc<Session>, binding: &RunBinding) -> Result<AuditRun, &'static str> {
    // Task-local audit scopes do not follow tokio::spawn. This lightweight scope
    // supplies authenticated thread attribution without allocating a JS slot.
    let run = AuditRun::new("events: observe retained worker lifecycle metadata", None)
        .map_err(|_| "events_source_audit_unavailable")?;
    run.record_connection(session)
        .map_err(|_| "events_source_audit_unavailable")?;
    run.bind(
        session.client_id(),
        &binding.owner,
        &binding.agent_id,
        &binding.token,
    )
    .map_err(|_| "events_source_audit_unavailable")?;
    Ok(run)
}

fn finish_observation(
    broker: &Broker,
    user: &str,
    token: &str,
    session: &Session,
    binding: &RunBinding,
    run: &AuditRun,
    error: Option<&'static str>,
) {
    let Some(authorization) = broker.authenticate_now(user, token) else {
        return;
    };
    if !session.is_alive()
        || !authorization.is_live()
        || !authorization.permits(session)
        || !broker.authorized_account(&authorization)
        || crate::bridge::owner(user, &authorization).as_deref() != Ok(binding.owner.as_str())
        || session
            .collaboration
            .bind(&binding.owner, &binding.agent_id, &binding.token)
            .is_err()
    {
        return;
    }
    let result = error.map_or(Ok(Value::Null), |error| Err(error.to_owned()));
    let _ = session.report_code_mode(run.completion(&result));
}

async fn pump(
    broker: Weak<Broker>,
    connection: Weak<Session>,
    user: String,
    source: Arc<Source>,
    binding: RunBinding,
) {
    let _active = Active(source.clone());
    let mut cursor = source
        .progress
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .cursor
        .clone();
    loop {
        let stopped = source.stopped.notified();
        tokio::pin!(stopped);
        stopped.as_mut().enable();
        if source.cancelled.load(Ordering::Acquire) {
            return;
        }
        if source.expired(Instant::now()) {
            state(&source, "expired", None);
            return;
        }
        let (Some(broker), Some(session)) = (broker.upgrade(), connection.upgrade()) else {
            state(&source, "disconnected", Some("events_source_disconnected"));
            return;
        };
        let token = source
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let authorization = match authorize(&broker, &user, &token, &session, &binding).await {
            Ok(authorization) => authorization,
            Err(error) => {
                state(&source, "unavailable", Some(error));
                return;
            }
        };
        if source.cancelled.load(Ordering::Acquire) {
            return;
        }
        let run = match observation_run(&session, &binding) {
            Ok(run) => run,
            Err(error) => {
                state(&source, "unavailable", Some(error));
                return;
            }
        };
        let mut args = json!({"limit":READ_LIMIT,"wait_ms":if cursor.is_some(){crate::wire::MAX_OBSERVATION_WAIT_MS}else{0}});
        if let Some(cursor) = &cursor {
            args["after"] = json!(cursor);
        }
        let started = Instant::now();
        let result = tokio::select! {
            _ = &mut stopped => {
                finish_observation(&broker,&user,&token,&session,&binding,&run,Some("events_source_observation_cancelled"));
                return;
            },
            result = tokio::time::timeout(Duration::from_millis(crate::wire::MAX_OBSERVATION_WAIT_MS + 1000),
                code_mode::AUDIT_RUN.scope(run.clone(),session.call_with_bound_authorization(
                    Operation::CallTool {name:"ssmcp.worker.events".into(),arguments:args},
                    None,&authorization,Some(binding.clone())))) => result,
        };
        // Revalidate the whole page before publication and refresh both the
        // account token and exact source connection after the native long wait.
        let parsed = match result {
            Ok(Ok(reply)) => unpack(reply).and_then(|value| page(&value, cursor.as_ref())),
            _ => Err("events_source_observation_unknown"),
        };
        let token = source
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Err(error) = authorize(&broker, &user, &token, &session, &binding).await {
            state(&source, "unavailable", Some(error));
            return;
        }
        finish_observation(
            &broker,
            &user,
            &token,
            &session,
            &binding,
            &run,
            parsed.as_ref().err().copied(),
        );
        let page = match parsed {
            Ok(page) => page,
            Err(error) => {
                state(&source, error_state(error), Some(error));
                return;
            }
        };
        if source.cancelled.load(Ordering::Acquire) {
            return;
        }
        if source.expired(Instant::now()) {
            state(&source, "expired", None);
            return;
        }
        for event in &page.events {
            let key = source_key(session.client_id(), &page.next.generation, event.seq);
            if broker
                .events
                .publish_system(
                    &binding.owner,
                    session.client_id(),
                    event.actor.as_deref(),
                    &event.kind,
                    event.data.clone(),
                    None,
                    Some(&key),
                )
                .is_err()
            {
                // A published prefix is recoverable by the stable event keys.
                // Do not advance past the page or replay any underlying work.
                state(&source, "overflow", Some("events_source_journal_full"));
                return;
            }
        }
        cursor = Some(page.next.clone());
        {
            let mut progress = source.progress.lock().unwrap_or_else(|e| e.into_inner());
            progress.cursor = cursor.clone();
            progress.status = if page.closed && !page.has_more {
                "closed"
            } else {
                "observing"
            };
            progress.error = None;
        }
        if page.closed && !page.has_more {
            return;
        }
        // A peer that immediately returns an empty pending page must not turn
        // this bounded observer into a hot polling loop.
        if page.events.is_empty() {
            tokio::select! {
                _ = &mut stopped => return,
                _ = tokio::time::sleep_until(started + MIN_EMPTY_OBSERVATION) => {},
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64) -> Value {
        json!({"type":"worker.progress","seq":seq,"generation":"pool_one",
            "task_id":"task_1","participant_id":"agent_1","task_status":"working",
            "slot":0,"epoch":1,"attempt":1,"activity":"progress","at_ms":100})
    }

    fn response(events: Vec<Value>, next: u64, latest: u64) -> Value {
        let status = if events.is_empty() { "empty" } else { "events" };
        json!({"generation":"pool_one","events":events,
            "next_cursor":{"generation":"pool_one","seq":next},
            "oldest_seq":1,"latest_seq":latest,"gap":false,
            "has_more":next<latest,"closed":false,"stopping":false,"status":status})
    }

    #[test]
    fn native_event_mapping_never_relays_bodies_or_origin_claims() {
        let mut input = event(3);
        input["type"] = json!("worker.completed");
        input["task_status"] = json!("completed");
        input["text"] = json!("private task");
        input["token"] = json!("private token");
        input["output"] = json!("private result");
        input["origin"] = json!("user");
        let (kind, data) = safe_event(&input, "pool_one").unwrap();
        assert_eq!(kind, "worker.completed");
        assert_eq!(data["task_id"], "task_1");
        for field in ["text", "token", "output", "origin"] {
            assert!(data.get(field).is_none());
        }
    }

    #[test]
    fn metadata_types_and_unknown_work_are_preserved() {
        let mut unknown = event(1);
        unknown["type"] = json!("worker.unknown");
        unknown["task_status"] = json!("unknown");
        assert_eq!(
            safe_event(&unknown, "pool_one").unwrap().0,
            "worker.unknown"
        );
        let mut cancelling = event(1);
        cancelling["type"] = json!("worker.cancelling");
        cancelling["task_status"] = json!("cancelling");
        assert_eq!(
            safe_event(&cancelling, "pool_one").unwrap().0,
            "worker.cancelling"
        );
        assert!(safe_event(&json!({"type":"message.sent","seq":1}), "pool_one").is_err());
        for (field, bad) in [
            ("slot", json!("0")),
            ("epoch", json!(-1)),
            ("at_ms", json!(1.5)),
            ("task_id", json!(true)),
            ("participant_id", json!(17)),
            ("activity", json!({"text":"body"})),
        ] {
            let mut input = event(1);
            input[field] = bad;
            assert!(safe_event(&input, "pool_one").is_err(), "{field}");
        }
        let mut input = event(1);
        input["type"] = json!("worker.completed");
        input["task_status"] = json!("unknown");
        assert!(safe_event(&input, "pool_one").is_err());
    }

    #[test]
    fn full_page_is_validated_before_it_can_be_published() {
        let previous = Cursor {
            generation: "pool_one".into(),
            seq: 2,
        };
        assert!(page(&response(vec![event(3), event(4)], 4, 4), Some(&previous)).is_ok());
        for bad in [
            response(vec![event(3), event(5)], 5, 5),
            response(vec![event(3), event(3)], 3, 3),
            response(vec![event(3)], 4, 4),
            response(vec![], 4, 4),
        ] {
            assert!(page(&bad, Some(&previous)).is_err());
        }
        let mut wrong_generation = response(vec![event(3)], 3, 3);
        wrong_generation["events"][0]["generation"] = json!("different_pool");
        assert!(page(&wrong_generation, Some(&previous)).is_err());
        let mut false_more = response(vec![event(3)], 3, 3);
        false_more["has_more"] = json!(true);
        assert!(page(&false_more, Some(&previous)).is_err());
    }

    #[test]
    fn closed_pages_are_drained_and_unchanged_pending_cursor_is_valid() {
        let previous = Cursor {
            generation: "pool_one".into(),
            seq: 2,
        };
        let mut value = response(vec![event(3)], 3, 4);
        value["closed"] = json!(true);
        let parsed = page(&value, Some(&previous)).unwrap();
        assert!(parsed.closed && parsed.has_more);
        assert!(page(&response(vec![], 2, 2), Some(&previous)).is_ok());
        let mut empty = response(vec![], 42, 42);
        empty["oldest_seq"] = json!(43);
        assert!(page(&empty, None).is_ok());
        assert!(page(&response(vec![event(2)], 2, 2), None).is_err());
    }

    #[test]
    fn batch_admission_is_unique_bounded_and_does_not_mutate_on_failure() {
        let mut entries = HashMap::new();
        let key = |index| Key {
            owner: "owner".into(),
            client_id: format!("client_{index}"),
            agent_id: "agent".into(),
            connection: index,
        };
        for i in 0..MAX_OWNER_SOURCES - 1 {
            let id = key(i);
            entries.insert(
                id.clone(),
                Arc::new(Source::new(id, "token", Instant::now())),
            );
        }
        assert!(check_capacity(&entries, "owner", vec![key(20), key(21)].into_iter()).is_err());
        assert_eq!(entries.len(), MAX_OWNER_SOURCES - 1);
        assert!(check_capacity(&entries, "owner", vec![key(20), key(20)].into_iter()).is_ok());
        let mut wrong_owner = key(20);
        wrong_owner.owner = "different".into();
        assert!(check_capacity(&entries, "owner", std::iter::once(wrong_owner)).is_err());
    }

    #[test]
    fn source_event_keys_remain_bounded_for_maximum_generation_identifiers() {
        let key = source_key(&"c".repeat(64), &"g".repeat(128), MAX_SEQUENCE);
        assert!(key.len() <= 128);
        assert_eq!(
            key,
            source_key(&"c".repeat(64), &"g".repeat(128), MAX_SEQUENCE)
        );
        assert_ne!(
            key,
            source_key(&"d".repeat(64), &"g".repeat(128), MAX_SEQUENCE)
        );
    }

    #[tokio::test]
    async fn rejected_source_batch_leaves_no_unstarted_entries() {
        let broker = Arc::new(Broker::new("https://relay.test".into()));
        let now = Instant::now();
        {
            let mut entries = broker.worker_events.entries.lock().unwrap();
            for i in 0..MAX_OWNER_SOURCES - 1 {
                let key = Key {
                    owner: "owner".into(),
                    client_id: format!("existing_{i}"),
                    agent_id: "agent".into(),
                    connection: i,
                };
                entries.insert(key.clone(), Arc::new(Source::new(key, "token", now)));
            }
        }
        let first = broker.test_register("first", &"a".repeat(64)).await;
        let second = broker.test_register("second", &"b".repeat(64)).await;
        let binding = RunBinding {
            owner: "owner".into(),
            agent_id: "agent".into(),
            token: "c".repeat(64),
        };
        assert!(
            ensure(
                &broker,
                "user",
                "token",
                "owner",
                &[(first, Some(binding.clone())), (second, Some(binding))]
            )
            .is_err()
        );
        let entries = broker.worker_events.entries.lock().unwrap();
        assert_eq!(entries.len(), MAX_OWNER_SOURCES - 1);
        assert!(
            entries
                .keys()
                .all(|key| key.client_id.starts_with("existing_"))
        );
    }

    #[tokio::test]
    async fn cancelled_source_wakes_a_registered_observer() {
        let source = Source::new(
            Key {
                owner: "owner".into(),
                client_id: "client".into(),
                agent_id: "agent".into(),
                connection: 1,
            },
            "token",
            Instant::now(),
        );
        let stopped = source.stopped.notified();
        tokio::pin!(stopped);
        stopped.as_mut().enable();
        source.stop("disconnected", Some("events_source_disconnected"));
        tokio::time::timeout(Duration::from_millis(50), stopped)
            .await
            .unwrap();
        assert!(source.cancelled.load(Ordering::Acquire));
        assert_eq!(source.progress.lock().unwrap().status, "disconnected");
    }
}
