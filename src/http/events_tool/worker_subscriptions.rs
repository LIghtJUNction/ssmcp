//! Automatic, explicitly configurable worker subscriptions for a verified main.
//! The native link is discovery, never source authority. Both original Sessions
//! and both independently authenticated bindings remain necessary for delivery.
use super::*;
use crate::http::{CodeRunReport, EligibleUserResponse, input_projection::Publication};
use crate::wire::Operation;
use serde_json::Map;

pub(crate) const DEFAULT_KINDS: &[&str] = &[
    "user.input_available",
    "agent.maintenance_requested",
    "task.submitted",
    "worker.started",
    "worker.progress",
    "worker.completed",
    "worker.idle",
    "worker.offline",
    "worker.retiring",
    "worker.cancelling",
    "worker.unknown",
];
const MAX_CONFIGURATIONS: usize = 128;
const MAX_PROBES: usize = 4;
const MAX_VISIBLE_MARKERS: usize = 128;
const NOTICE_LIMIT: usize = 4;
const MAX_NOTICE_BYTES: usize = 16 * 1024;
const HEADER: &str = "WORKER STATE EVENTS — retained until explicit events.ack\n";

#[derive(Clone, Eq, Hash, PartialEq)]
struct Key {
    owner: String,
    client_id: String,
    agent_id: String,
    connection: usize,
}
impl Key {
    fn of(context: &Context) -> Self {
        Self {
            owner: context.actor.owner.clone(),
            client_id: context.actor.client_id.clone(),
            agent_id: context.actor.agent_id.clone(),
            connection: Arc::as_ptr(&context.session) as usize,
        }
    }
}

#[derive(Clone)]
struct VisibleMarker {
    sequence: Option<u64>,
    publication: Publication,
}

#[derive(Clone, Copy, PartialEq)]
enum AdvisoryPage {
    Gap(u64),
    Routine {
        cursor: u64,
        through: u64,
        has_more: bool,
    },
}

impl AdvisoryPage {
    fn from_notice(notice: &Value) -> Option<Self> {
        if let Some(cursor) = gap_cursor(notice) {
            return Some(Self::Gap(cursor));
        }
        let events = notice["events"].as_array()?;
        if events.is_empty()
            || !events.iter().all(|event| {
                matches!(
                    event["kind"].as_str(),
                    Some("task.submitted" | "worker.started" | "worker.progress" | "worker.idle")
                ) && matches!(
                    event["data"]["task_status"].as_str(),
                    None | Some("queued" | "working")
                )
            })
        {
            return None;
        }
        Some(Self::Routine {
            cursor: notice["cursor"].as_u64()?,
            through: notice["next_cursor"].as_u64()?,
            has_more: notice["has_more"].as_bool()?,
        })
    }
}

struct SentAdvisory {
    revision: u64,
    page: AdvisoryPage,
    // A narrow outer response may have carried only a read_required receipt.
    // If more of the same page fits later, publish it immediately.
    through: Option<u64>,
    at: std::time::Instant,
}

#[derive(Default)]
struct ModelReminder {
    last: Option<SentAdvisory>,
}

impl ModelReminder {
    // Suppression is local to unsolicited outer responses. A lost first response
    // remains recoverable by explicit read/wait. Only events.ack advances the cursor.
    const RETRY: Duration = Duration::from_secs(5 * 60);

    fn project(
        &mut self,
        revision: u64,
        page: Option<AdvisoryPage>,
        notice: Value,
        now: std::time::Instant,
    ) -> Option<Value> {
        let Some(page) = page else {
            return Some(notice);
        };
        let through = (notice["status"] != "read_required")
            .then(|| notice["next_cursor"].as_u64().unwrap_or(0));
        if self.last.as_ref().is_some_and(|last| {
            last.revision == revision
                && last.page == page
                && last.through >= through
                && now.duration_since(last.at) < Self::RETRY
        }) {
            return None;
        }
        self.last = Some(SentAdvisory {
            revision,
            page,
            through,
            at: now,
        });
        Some(notice)
    }
}

#[derive(Clone)]
struct Entry {
    user: String,
    principal: Source,
    enabled: bool,
    kinds: Vec<String>,
    configured: bool,
    revision: u64,
    request_id: String,
    subscription_id: Option<String>,
    source: Option<Source>,
    pool_hint: Option<String>,
    problem: Option<String>,
    model_reminder: Arc<Mutex<ModelReminder>>,
    visible: Arc<Mutex<HashMap<String, VisibleMarker>>>,
}
impl Entry {
    fn owns(&self, context: &Context) -> bool {
        self.user == context.user && self.principal.same(&principal(context))
    }
    fn management(&self) -> &'static str {
        if self.configured {
            "configured"
        } else {
            "automatic"
        }
    }
    fn selects(&self, kind: &str) -> bool {
        self.kinds.iter().any(|pattern| {
            pattern == kind
                || pattern
                    .strip_suffix('*')
                    .is_some_and(|prefix| kind.starts_with(prefix))
        })
    }
}

#[derive(Default)]
pub(crate) struct Registry {
    entries: Mutex<HashMap<Key, Entry>>,
}

fn principal(context: &Context) -> Source {
    Source {
        client_id: context.actor.client_id.clone(),
        connection: Arc::downgrade(&context.session),
        binding: Some(context.binding.clone()),
    }
}

fn nonce() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    crate::platform::random_bytes(&mut bytes)
        .map_err(|_| "cannot allocate automatic worker subscription ID")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn main_member(context: &Context, allow_retiring: bool) -> bool {
    if !context.session.chat_enabled() {
        return false;
    }
    let Ok(member) = context.session.collaboration.bind(
        &context.actor.owner,
        &context.binding.agent_id,
        &context.binding.token,
    ) else {
        return false;
    };
    let Ok(member) = serde_json::to_value(member) else {
        return false;
    };
    let role = member["role"].as_str().unwrap_or_default();
    ["main", "coordinator", "controller", "lead"]
        .iter()
        .any(|expected| role.eq_ignore_ascii_case(expected))
        && (allow_retiring || !matches!(member["status"].as_str(), Some("retiring" | "offline")))
}

impl Registry {
    fn prepare(&self, context: &Context) -> Result<Entry, String> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Weak ownership prevents a new Session with the same public ID from
        // inheriting credentials or configuration. Live preferences are bounded
        // and are never evicted merely to make room for another participant.
        entries.retain(|_, entry| {
            entry
                .principal
                .connection
                .upgrade()
                .is_some_and(|session| session.is_alive())
        });
        let key = Key::of(context);
        if let Some(entry) = entries.get(&key) {
            if !entry.owns(context) {
                return Err("worker subscription original participant binding changed".into());
            }
            return Ok(entry.clone());
        }
        if entries.len() >= MAX_CONFIGURATIONS {
            return Err("worker_subscription_capacity: live configurations are retained".into());
        }
        let entry = Entry {
            user: context.user.clone(),
            principal: principal(context),
            enabled: true,
            kinds: DEFAULT_KINDS.iter().map(|kind| (*kind).into()).collect(),
            configured: false,
            revision: 0,
            request_id: format!("auto_workers_{}", nonce()?),
            subscription_id: None,
            source: None,
            pool_hint: None,
            problem: None,
            model_reminder: Arc::default(),
            visible: Arc::new(Mutex::new(HashMap::new())),
        };
        entries.insert(key, entry.clone());
        Ok(entry)
    }

    fn get(&self, context: &Context) -> Option<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&Key::of(context))
            .filter(|entry| entry.owns(context))
            .cloned()
    }

    fn current(&self, context: &Context, expected: &Entry) -> bool {
        self.get(context).is_some_and(|entry| {
            entry.enabled
                && entry.revision == expected.revision
                && entry.subscription_id == expected.subscription_id
                && entry
                    .source
                    .as_ref()
                    .zip(expected.source.as_ref())
                    .is_some_and(|(left, right)| left.same(right))
        })
    }

    fn problem(&self, context: &Context, problem: &str) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&Key::of(context))
            .filter(|entry| entry.owns(context))
        {
            entry.problem = Some(problem.chars().take(512).collect());
        }
    }
}

/// Reads only the server-authenticated main's native link. No join, attach,
/// submission, user-input read, or fabricated pool participant is performed.
async fn ensure(
    broker: &Arc<Broker>,
    user: &str,
    token: &str,
    context: &Context,
    run: &code_mode::AuditRun,
) -> Result<(), String> {
    if !main_member(context, false) {
        return Ok(());
    }
    let initial = broker.event_subscriptions.workers.prepare(context)?;
    if !initial.enabled {
        return Ok(());
    }
    let authorization = context.final_authorization(broker, token)?;
    let reply = code_mode::AUDIT_RUN
        .scope(
            run.clone(),
            context.session.call_with_authorization(
                Operation::CallTool {
                    name: "ssmcp.ui.get".into(),
                    arguments: json!({"path":"/worker_pool"}),
                },
                None,
                &authorization,
            ),
        )
        .await?;
    context.final_authorization(broker, token)?;
    let value = &reply["structuredContent"];
    let pool_id = value["value"]["client_id"]
        .as_str()
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .filter(|_| reply["isError"] != true && value["path"] == "/worker_pool")
        .filter(|_| value["value"]["closed"] != true && value["value"]["connected"] != false)
        .ok_or("worker_subscription_link_unavailable: no verified live linked pool")?;
    if pool_id == context.actor.client_id {
        return Err("worker subscription requires distinct main and pool Sessions".into());
    }
    let authorization = context.final_authorization(broker, token)?;
    let pool = broker.client(user, &authorization, pool_id).await?;
    let previous = broker
        .event_subscriptions
        .workers
        .get(context)
        .ok_or("worker subscription scope expired")?;
    let binding = run.binding(pool_id).or_else(|| {
        previous
            .source
            .as_ref()
            .filter(|source| source.client_id == pool_id)
            .filter(|source| {
                source
                    .connection
                    .upgrade()
                    .is_some_and(|original| Arc::ptr_eq(&original, &pool))
            })
            .and_then(|source| source.binding.clone())
    });
    {
        let mut entries = broker
            .event_subscriptions
            .workers
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = entries
            .get_mut(&Key::of(context))
            .filter(|entry| entry.owns(context))
        {
            entry.pool_hint = Some(pool_id.into());
        }
    }
    let binding = binding.ok_or(
        "worker_subscription_needs_binding: bind the linked pool with your own separate private credentials",
    )?;
    let source = Source {
        client_id: pool_id.into(),
        connection: Arc::downgrade(&pool),
        binding: Some(binding),
    };
    let authorization = context.final_authorization(broker, token)?;
    source.live(&context.actor.owner, &authorization)?;
    run.record_connection(&pool)?;
    let binding = source.binding.as_ref().expect("verified source binding");
    run.bind_identity(pool_id, &binding.owner, &binding.agent_id, &binding.token)?;
    let record = {
        let mut entries = broker
            .event_subscriptions
            .workers
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entry = entries
            .get_mut(&Key::of(context))
            .filter(|entry| entry.owns(context))
            .ok_or("worker subscription original scope changed")?;
        if !entry.enabled || entry.revision != initial.revision {
            return Ok(());
        }
        if let Some(previous) = &entry.source
            && !previous.same(&source)
        {
            entry.problem = Some("worker_subscription_source_changed: retained subscription remains scoped to its original pool connection and member".into());
            return Err(entry.problem.clone().expect("assigned error"));
        }
        let id = if let Some(id) = &entry.subscription_id {
            id.clone()
        } else {
            let mut proofs = vec![principal(context), source.clone()];
            proofs.sort_by(|left, right| left.client_id.cmp(&right.client_id));
            let receipt = broker.event_subscriptions.subscribe(
                &broker.events,
                context,
                &entry.request_id,
                Filter {
                    kinds: entry.kinds.clone(),
                    client_ids: proofs.iter().map(|proof| proof.client_id.clone()).collect(),
                    actor_id: None,
                    task_id: None,
                    include_own_waits: false,
                },
                None,
                proofs,
            )?;
            let id = receipt["subscription_id"]
                .as_str()
                .ok_or("worker subscription creation returned no ID")?
                .to_owned();
            entry.subscription_id = Some(id.clone());
            entry.source = Some(source);
            id
        };
        let record = broker
            .event_subscriptions
            .get(&broker.events, context, &id)?;
        entry.problem = None;
        record
    };
    let sources = authorize_record(broker, token, context, &record, true).await?;
    let id = broker
        .event_subscriptions
        .workers
        .get(context)
        .and_then(|entry| entry.subscription_id)
        .ok_or("worker subscription scope changed before source observation")?;
    context
        .session
        .seed_user_input_events(&context.actor.owner, &context.actor.agent_id, &id);
    // Main input and maintenance events already originate in the server Hub.
    // A separate native worker.events observer is needed only for the pool.
    let native: Vec<_> = sources
        .into_iter()
        .filter(|(session, _)| session.client_id() == pool_id)
        .collect();
    *record
        .native_error
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        crate::broker::worker_events::ensure(broker, user, token, &context.actor.owner, &native)
            .err()
            .map(|error| error.chars().take(512).collect());
    broker.event_subscriptions.note_delivery(record)?;
    Ok(())
}

/// Called after an actual collab.bind, before a caller can enter await. Its
/// deadline is one total budget across already-bound main participants.
pub(crate) async fn after_bind(
    broker: &Arc<Broker>,
    user: &str,
    token: &str,
    run: &code_mode::AuditRun,
) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    let mut results = Vec::new();
    for client_id in run.targets() {
        if results.len() == MAX_PROBES || tokio::time::Instant::now() >= deadline {
            break;
        }
        let Some(binding) = run.binding(&client_id) else {
            continue;
        };
        let Some(original) = run.connection(&client_id) else {
            continue;
        };
        let context = Context {
            user: user.into(),
            actor: Actor {
                owner: binding.owner.clone(),
                client_id,
                agent_id: binding.agent_id.clone(),
            },
            session: original,
            binding,
        };
        if !main_member(&context, true) || context.final_authorization(broker, token).is_err() {
            continue;
        }
        let outcome =
            tokio::time::timeout_at(deadline, ensure(broker, user, token, &context, run)).await;
        let error = match outcome {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some("worker_subscription_probe_pending: bounded link probe expired".into()),
        };
        if let Some(error) = error {
            broker.event_subscriptions.workers.problem(&context, &error);
        }
        if context.final_authorization(broker, token).is_ok() {
            results.push(status(broker, &context));
        }
    }
    results
}

pub(super) fn configure(
    broker: &Broker,
    context: &Context,
    patch: Option<&Value>,
) -> Result<(), String> {
    if !main_member(context, true) {
        return Ok(());
    }
    broker.event_subscriptions.workers.prepare(context)?;
    let Some(patch) = patch else { return Ok(()) };
    if patch.get("enabled").is_none() && patch.get("kinds").is_none() {
        return Ok(());
    }
    // The existing policy parser has validated these fields before this hook.
    let mut entries = broker
        .event_subscriptions
        .workers
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let entry = entries
        .get_mut(&Key::of(context))
        .filter(|entry| entry.owns(context))
        .ok_or("worker subscription original participant binding changed")?;
    let enabled = patch["enabled"].as_bool().unwrap_or(entry.enabled);
    let kinds: Vec<String> = patch
        .get("kinds")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|_| "invalid worker subscription kinds")?
        .unwrap_or_else(|| entry.kinds.clone());
    if enabled == entry.enabled
        && kinds == entry.kinds
        && entry.configured
        && entry.subscription_id.as_ref().is_none_or(|id| {
            broker
                .event_subscriptions
                .get(&broker.events, context, id)
                .is_ok()
        })
    {
        return Ok(());
    }
    let revision = entry
        .revision
        .checked_add(1)
        .ok_or("worker subscription configuration revision exhausted")?;
    if let Some(id) = &entry.subscription_id {
        let active = broker.event_subscriptions.get(&broker.events, context, id);
        if enabled && active.is_err() {
            let listed = broker.events.subscriptions(&context.actor)?;
            if listed["subscriptions"]
                .as_array()
                .is_some_and(|subscriptions| {
                    subscriptions
                        .iter()
                        .any(|value| value["subscription_id"].as_str() == Some(id))
                })
            {
                // A retained but invalid scope requires an explicit revoke.
                // Arbitrary authorization/scope errors never discard its cursor.
                return Err(active.err().expect("checked subscription error"));
            }
            // Explicitly enabling an expired or unsubscribed record requests
            // a fresh subscription. It never acknowledges the old source.
            entry.subscription_id = None;
            entry.source = None;
            entry.request_id = format!("auto_workers_{}", nonce()?);
        } else if active.is_ok() && kinds != entry.kinds {
            broker
                .events
                .update_kinds(&context.actor, id, kinds.clone())?;
        }
    }
    entry.enabled = enabled;
    entry.kinds = kinds;
    entry.configured = true;
    entry.revision = revision;
    entry.problem = None;
    Ok(())
}

pub(super) fn unsubscribed(broker: &Broker, context: &Context, id: &str) {
    let mut entries = broker
        .event_subscriptions
        .workers
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(entry) = entries
        .get_mut(&Key::of(context))
        .filter(|entry| entry.owns(context) && entry.subscription_id.as_deref() == Some(id))
    {
        entry.enabled = false;
        entry.configured = true;
        entry.revision = entry.revision.saturating_add(1);
        entry.problem = Some(
            "unsubscribed; explicitly enable events.policy to create a new subscription".into(),
        );
    }
}

pub(super) fn status(broker: &Broker, context: &Context) -> Value {
    if !main_member(context, true) {
        return json!({"status":"not_applicable","management":"manual"});
    }
    let Some(entry) = broker.event_subscriptions.workers.get(context) else {
        return json!({"status":"not_initialized","management":"automatic"});
    };
    let descriptor = entry.subscription_id.as_deref().and_then(|id| {
        broker.events.subscriptions(&context.actor).ok()?["subscriptions"]
            .as_array()?
            .iter()
            .find(|value| value["subscription_id"].as_str() == Some(id))
            .cloned()
    });
    json!({
        "status":if !entry.enabled {"disabled"}
            else if entry.problem.is_some() {"unavailable"}
            else if descriptor.is_some() {"subscribed"} else {"needs_binding"},
        "management":entry.management(),
        "managed_by":"events.policy",
        "enabled":entry.enabled,
        "revision":entry.revision,
        "kinds":entry.kinds,
        "subscription_id":entry.subscription_id,
        "pool_client_id":entry.pool_hint,
        "subscription":descriptor,
        "error":entry.problem,
        "acknowledged_automatically":false,
        "model_started":false,
    })
}

pub(super) fn managed_record(broker: &Broker, record: &Subscription) -> bool {
    broker
        .event_subscriptions
        .workers
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .any(|entry| {
            entry.request_id == record.request_id
                && entry.principal.same(&record.principal)
                && entry.subscription_id.is_some()
        })
}

pub(super) fn annotate(broker: &Broker, context: &Context, value: &mut Value) {
    let Some(id) = value["subscription_id"].as_str().map(str::to_owned) else {
        return;
    };
    let entry = broker
        .event_subscriptions
        .workers
        .get(context)
        .filter(|entry| entry.subscription_id.as_deref() == Some(id.as_str()));
    value["management"] = json!(entry.as_ref().map_or("manual", Entry::management));
    if let Some(entry) = entry {
        value["enabled"] = json!(entry.enabled);
        value["configuration_revision"] = json!(entry.revision);
        value["managed_by"] = json!("events.policy");
    }
}

/// A real, ordered native worker event has already entered the retained Hub.
/// This changes existing member state only; it never creates an external turn.
pub(crate) fn published(
    broker: &Broker,
    user: &str,
    token: &str,
    pool: &Arc<Session>,
    pool_binding: &RunBinding,
    kind: &str,
    event_id: &str,
) {
    let Some(authorization) = broker.authenticate_now(user, token) else {
        return;
    };
    let Ok(owner) = crate::bridge::owner(user, &authorization) else {
        return;
    };
    if owner != pool_binding.owner || !authorization.permits(pool) || !pool.is_alive() {
        return;
    }
    let source = Source {
        client_id: pool.client_id().into(),
        connection: Arc::downgrade(pool),
        binding: Some(pool_binding.clone()),
    };
    if source.live(&owner, &authorization).is_err() {
        return;
    }
    let candidates: Vec<_> = broker
        .event_subscriptions
        .workers
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .filter(|entry| {
            entry.user == user
                && entry.enabled
                && entry.selects(kind)
                && entry.source.as_ref().is_some_and(|old| old.same(&source))
        })
        .cloned()
        .collect();
    for entry in candidates {
        let (Some(session), Some(binding), Some(id)) = (
            entry.principal.connection.upgrade(),
            entry.principal.binding.clone(),
            entry.subscription_id.as_deref(),
        ) else {
            continue;
        };
        let context = Context {
            user: user.into(),
            actor: Actor {
                owner: binding.owner.clone(),
                client_id: session.client_id().into(),
                agent_id: binding.agent_id.clone(),
            },
            session,
            binding,
        };
        if !broker.event_subscriptions.workers.current(&context, &entry)
            || !main_member(&context, false)
            || context.final_authorization(broker, token).is_err()
            || broker
                .event_subscriptions
                .get(&broker.events, &context, id)
                .is_err()
            || broker
                .events
                .contains_pending_event(&context.actor, id, event_id)
                != Ok(true)
        {
            continue;
        }
        let _ = context.session.collaboration.wake_for_worker_event(
            &context.actor.owner,
            &context.binding.agent_id,
            &context.binding.token,
            event_id,
        );
    }
}

async fn read_notice(
    broker: &Arc<Broker>,
    user: &str,
    token: &str,
    context: &Context,
    wait_ms: u64,
    wait_id: Option<&str>,
) -> Option<Value> {
    let entry = broker.event_subscriptions.workers.get(context)?;
    let id = entry.subscription_id.as_deref()?;
    if !entry.enabled || !main_member(context, false) {
        return None;
    }
    let result = observe(
        broker,
        (user, token),
        &context.actor.client_id,
        (&context.actor.agent_id, &context.binding.token),
        id,
        (
            NOTICE_LIMIT,
            wait_ms.min(crate::wire::MAX_OBSERVATION_WAIT_MS),
        ),
        wait_id,
    )
    .await;
    if !broker.event_subscriptions.workers.current(context, &entry)
        || !main_member(context, false)
        || context.final_authorization(broker, token).is_err()
    {
        return None;
    }
    let mut notice = match result {
        Ok(value) if value["status"] == "ready" => value,
        Ok(_) => return None,
        Err(error) if error.starts_with("events_gap:") || error.starts_with("events_source_") => {
            let record = broker
                .event_subscriptions
                .get(&broker.events, context, id)
                .ok()?;
            authorize_record(broker, token, context, &record, true)
                .await
                .ok()?;
            broker.event_subscriptions.note_delivery(record).ok()?;
            json!({"status":"unavailable","subscription_id":id,
                "error":error.chars().take(512).collect::<String>(),
                "events":[],"history_complete":false})
        }
        Err(_) => return None,
    };
    if !broker.event_subscriptions.workers.current(context, &entry)
        || context.final_authorization(broker, token).is_err()
    {
        return None;
    }
    annotate(broker, context, &mut notice);
    notice["client_id"] = json!(context.actor.client_id);
    notice["agent_id"] = json!(context.actor.agent_id);
    notice["pool_client_id"] = json!(entry.pool_hint);
    notice["delivery"] = json!("retained_read; explicitly acknowledge processed next_cursor");
    Some(notice)
}

/// A side observer for an existing await. It returns no readiness claim about
/// that await's original targets and consumes no task, input, inbox or cursor.
pub(crate) async fn wait_for_notice(
    broker: &Arc<Broker>,
    user: &str,
    token: &str,
    session: &Arc<Session>,
    binding: &RunBinding,
    wait_ms: u64,
    wait_id: Option<&str>,
) -> Option<Value> {
    let context = Context {
        user: user.into(),
        actor: Actor {
            owner: binding.owner.clone(),
            client_id: session.client_id().into(),
            agent_id: binding.agent_id.clone(),
        },
        session: session.clone(),
        binding: binding.clone(),
    };
    read_notice(broker, user, token, &context, wait_ms, wait_id).await
}

fn without_native_notices(mut response: Value) -> Value {
    if let Some(object) = response.as_object_mut() {
        object.remove("worker_events");
        if let Some(content) = object
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            content.remove("worker_events");
        }
        if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
            content.retain(|block| {
                !block["text"]
                    .as_str()
                    .is_some_and(|text| text.starts_with(HEADER))
            });
        }
    }
    response
}

fn notice_summary(notice: &Value) -> String {
    let id = notice["subscription_id"].as_str().unwrap_or("subscription");
    if let Some(error) = notice["error"].as_str() {
        return format!("{id}: {error}");
    }
    if notice["status"] == "read_required" {
        return format!("{id}: events.read (limit 1) before ACK.");
    }
    let mut summary = format!("{id}:");
    for event in notice["events"].as_array().into_iter().flatten() {
        use std::fmt::Write;
        let _ = write!(
            summary,
            " {} {}",
            event["seq"],
            event["kind"].as_str().unwrap_or("?")
        );
        if let Some(status @ ("failed" | "unknown" | "cancelled" | "cancelling")) =
            event["data"]["task_status"].as_str()
        {
            let _ = write!(summary, " ({status})");
        }
    }
    summary.push_str(". See structuredContent.worker_events or events.read before ACK.");
    summary
}

fn append_values(response: &Value, notices: &[Value], budget: usize) -> Option<Value> {
    let mut next = response.clone();
    let object = next.as_object_mut()?;
    let summaries = notices
        .iter()
        .map(notice_summary)
        .collect::<Vec<_>>()
        .join("\n");
    let text = format!("{HEADER}{summaries}");
    object.insert("worker_events".into(), json!(notices));
    object
        .entry("structuredContent")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()?
        .insert("worker_events".into(), json!(notices));
    object
        .entry("content")
        .or_insert_with(|| json!([]))
        .as_array_mut()?
        .push(json!({"type":"text","text":text}));
    (next
        .to_string()
        .len()
        .saturating_sub(response.to_string().len())
        <= budget)
        .then_some(next)
}

fn read_required(notice: &Value) -> Value {
    json!({
        "client_id":notice["client_id"], "subscription_id":notice["subscription_id"],
        "status":"read_required", "events":[], "read_limit":1,
        "delivery":"events.read with limit 1 before events.ack",
    })
}

/// Prefer a retained event prefix that fits the actual serialized response.
/// A truncated page may acknowledge only its last included sequence. When even
/// one event cannot fit, return a small actionable read receipt, never an ACK.
fn fit_notice(
    response: &Value,
    previous: &[Value],
    mut notice: Value,
    budget: usize,
) -> Option<(Value, Value)> {
    loop {
        let mut candidates = previous.to_vec();
        candidates.push(notice.clone());
        if let Some(next) = append_values(response, &candidates, budget) {
            return Some((next, notice));
        }
        let events = notice.get_mut("events")?.as_array_mut()?;
        if events.len() <= 1 {
            break;
        }
        events.pop();
        let next_cursor = events.last()?["seq"].clone();
        notice["next_cursor"] = next_cursor;
        notice["has_more"] = json!(true);
    }
    let fallback = read_required(&notice);
    let mut candidates = previous.to_vec();
    candidates.push(fallback.clone());
    append_values(response, &candidates, budget).map(|next| (next, fallback))
}

async fn publication_authorized(
    report: &CodeRunReport,
    context: &Context,
    entry: &Entry,
    deadline: tokio::time::Instant,
) -> bool {
    if !main_member(context, false)
        || !report
            .broker
            .event_subscriptions
            .workers
            .current(context, entry)
        || context
            .final_authorization(&report.broker, &report.access_token)
            .is_err()
    {
        return false;
    }
    let Some(id) = entry.subscription_id.as_deref() else {
        return false;
    };
    let Ok(record) = report
        .broker
        .event_subscriptions
        .get(&report.broker.events, context, id)
    else {
        return false;
    };
    matches!(
        tokio::time::timeout_at(
            deadline,
            authorize_record(&report.broker, &report.access_token, context, &record, true)
        )
        .await,
        Ok(Ok(_))
    )
}

// The retained boundary and latest sequence move while the same subscriber
// is stuck at one cursor. They describe the observation, not a new gap.
fn gap_cursor(notice: &Value) -> Option<u64> {
    if notice["status"] != "unavailable" {
        return None;
    }
    let error = notice["error"]
        .as_str()?
        .strip_prefix("events_gap: cursor ")?;
    let (cursor, _) = error.split_once(" precedes retained cursor ")?;
    cursor.parse().ok()
}

async fn visible_notice(
    report: &CodeRunReport,
    target: &EligibleUserResponse,
    context: &Context,
    entry: &Entry,
    mut notice: Value,
    deadline: tokio::time::Instant,
    attempted: &mut usize,
) -> Option<Value> {
    let id = entry.subscription_id.as_deref()?;
    let events = notice["events"].as_array()?.clone();
    let acknowledged = notice["cursor"].as_u64().unwrap_or(0);
    // Gap/read-required metadata is also visible. It never claims an event ACK.
    let items = if events.is_empty() {
        vec![(None, serde_json::to_string(&notice).ok()?)]
    } else {
        events
            .iter()
            .map(|event| {
                Some((
                    Some(event["seq"].as_u64()?),
                    serde_json::to_string(event).ok()?,
                ))
            })
            .collect::<Option<Vec<_>>>()?
    };
    let gap = gap_cursor(&notice).filter(|_| events.is_empty());
    let mut visible = 0usize;
    for (sequence, body) in items {
        let identity = match (sequence, gap) {
            (Some(sequence), _) => format!("{id}:{sequence}"),
            (None, Some(cursor)) => format!("{id}:gap:{cursor}"),
            (None, None) => format!("{id}:notice:{body}"),
        };
        let text = format!("{HEADER}{body}");
        let publication = {
            let mut markers = entry
                .visible
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            // Only the real Hub cursor proves ACK. Unknown writes are retained
            // even past ACK; absence from a pruned event page is not no-effect.
            markers.retain(|_, marker| {
                !marker.publication.replaceable()
                    || marker
                        .sequence
                        .is_none_or(|sequence| sequence > acknowledged)
            });
            if let Some(marker) = markers.get(&identity) {
                // A gap keeps its original visible diagnostic and receipt. The
                // structured response below still returns the current boundaries.
                if gap.is_none() && !marker.publication.matches(&text, "worker_event") {
                    return None;
                }
                marker.publication.clone()
            } else {
                if markers.len() >= MAX_VISIBLE_MARKERS {
                    return None;
                }
                let publication = Publication::new(target, &identity, text, "worker_event")?;
                markers.insert(
                    identity,
                    VisibleMarker {
                        sequence,
                        publication: publication.clone(),
                    },
                );
                publication
            }
        };
        if !publication.recorded() {
            if *attempted == MAX_PROBES
                || tokio::time::Instant::now() >= deadline
                || !publication_authorized(report, context, entry, deadline).await
            {
                break;
            }
            *attempted += 1;
            publication.start(report, target);
            if !publication.recorded_by(deadline).await {
                break;
            }
        }
        visible += 1;
    }
    if visible == 0 {
        return None;
    }
    if !events.is_empty() && visible < events.len() {
        notice["events"] = json!(&events[..visible]);
        notice["next_cursor"] = events[visible - 1]["seq"].clone();
        notice["has_more"] = json!(true);
    }
    Some(notice)
}

pub(in crate::http) async fn append(
    report: &CodeRunReport,
    response: Value,
    eligible: &[EligibleUserResponse],
) -> Value {
    let response = without_native_notices(response);
    let budget = MAX_NOTICE_BYTES.min(crate::http::delivery_budget(&response));
    if budget < 512 {
        return response;
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut attempted = 0usize;
    let mut notices = Vec::new();
    let mut proof_contexts = Vec::new();
    let mut appended = response.clone();
    for target in eligible {
        if notices.len() == MAX_PROBES || tokio::time::Instant::now() >= deadline {
            break;
        }
        let Some(binding) = &target.binding else {
            continue;
        };
        if crate::http::response_authorization(report, target).is_none() {
            continue;
        }
        let context = Context {
            user: report.user.clone(),
            actor: Actor {
                owner: binding.owner.clone(),
                client_id: target.session.client_id().into(),
                agent_id: binding.agent_id.clone(),
            },
            session: target.session.clone(),
            binding: binding.clone(),
        };
        let notice = tokio::time::timeout_at(
            deadline,
            code_mode::AUDIT_RUN.scope(
                report.run.clone(),
                read_notice(
                    &report.broker,
                    &report.user,
                    &report.access_token,
                    &context,
                    0,
                    None,
                ),
            ),
        )
        .await;
        let Ok(Some(notice)) = notice else { continue };
        let Some(entry) = report.broker.event_subscriptions.workers.get(&context) else {
            continue;
        };
        if notice["configuration_revision"].as_u64() != Some(entry.revision)
            || !report
                .broker
                .event_subscriptions
                .workers
                .current(&context, &entry)
        {
            continue;
        }
        // Classify the retained page before byte limits or UI publication can
        // reduce it to a prefix or a cursorless read_required receipt.
        let advisory = AdvisoryPage::from_notice(&notice);
        let Some((_, fitted)) = fit_notice(&response, &notices, notice, budget) else {
            continue;
        };
        let Some(visible) = visible_notice(
            report,
            target,
            &context,
            &entry,
            fitted,
            deadline,
            &mut attempted,
        )
        .await
        else {
            continue;
        };
        let Some((next, fitted)) = fit_notice(&response, &notices, visible, budget) else {
            continue;
        };
        notices.push(fitted);
        proof_contexts.push((context, entry, advisory));
        appended = next;
    }
    for (context, entry, _) in &proof_contexts {
        if !publication_authorized(report, context, entry, deadline).await {
            return response;
        }
    }
    // Another probe may have awaited after an earlier notice. Freeze the entire
    // delivery proof again before any retained metadata leaves this collector.
    if !matches!(
        tokio::time::timeout_at(
            deadline,
            authorize_run(
                &report.broker,
                &report.user,
                &report.access_token,
                &report.run
            )
        )
        .await,
        Ok(Ok(()))
    ) {
        return response;
    }
    if proof_contexts.iter().any(|(context, entry, _)| {
        context
            .final_authorization(&report.broker, &report.access_token)
            .is_err()
            || !main_member(context, false)
            || !report
                .broker
                .event_subscriptions
                .workers
                .current(context, entry)
    }) {
        return response;
    }
    // A wait woken by a subscription must return its complete reason. Compact
    // only unsolicited repeats after the final authorization checks above.
    if response["structuredContent"]["wake_reason"] == "subscription_event" || notices.is_empty() {
        return appended;
    }
    let now = std::time::Instant::now();
    let projected: Vec<_> = proof_contexts
        .iter()
        .zip(notices)
        .filter_map(|((_, entry, advisory), notice)| {
            entry
                .model_reminder
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .project(entry.revision, *advisory, notice, now)
        })
        .collect();
    if projected.is_empty() {
        return response;
    }
    append_values(&response, &projected, budget).unwrap_or(appended)
}

#[cfg(test)]
mod tests;
