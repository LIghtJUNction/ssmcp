//! Authenticated event publication and observation outside the JavaScript pool.
//! Subscription proofs retain the exact connections and participant bindings.
use crate::{
    broker::{Authorization, Broker, Session},
    code_mode::{self, RunBinding},
    events::{Actor, Filter, Hub},
};
use serde::Deserialize;
pub(crate) mod worker_subscriptions;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_SUBSCRIPTIONS: usize = 256;
const MAX_ACTOR_SUBSCRIPTIONS: usize = 32;
const MAX_DELIVERY_RUNS: usize = 256;
const MAX_RUN_PROOFS: usize = 64;
const MAX_SOURCES: usize = 8;
const RETAIN_FOR: Duration = Duration::from_secs(15 * 60);
const MAX_CURSOR: u64 = (1_u64 << 53) - 1;

fn limit() -> usize {
    32
}
fn wait_ms() -> u64 {
    crate::wire::MAX_OBSERVATION_WAIT_MS
}
fn empty_filter() -> Filter {
    Filter {
        kinds: vec![],
        client_ids: vec![],
        actor_id: None,
        task_id: None,
        include_own_waits: false,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceAgent {
    client_id: String,
    agent_id: String,
    token: String,
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Subscribe {
        request_id: String,
        #[serde(default = "empty_filter")]
        filter: Filter,
        #[serde(default)]
        after: Option<u64>,
        #[serde(default)]
        source_agents: Vec<SourceAgent>,
    },
    Read {
        subscription_id: String,
        #[serde(default = "limit")]
        limit: usize,
    },
    Wait {
        subscription_id: String,
        #[serde(default = "limit")]
        limit: usize,
        #[serde(default = "wait_ms")]
        wait_ms: u64,
    },
    Ack {
        subscription_id: String,
        through: u64,
    },
    Unsubscribe {
        subscription_id: String,
    },
    Subscriptions {},
    Publish {
        request_id: String,
        name: String,
        data: Value,
    },
    Policy {
        #[serde(default)]
        patch: Option<Value>,
    },
}

struct Arguments {
    client_id: String,
    credentials: Option<(String, String)>,
    request: Request,
}

fn identifier(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn credentials(agent_id: &str, token: &str) -> Result<(), String> {
    if !identifier(agent_id, 64)
        || token.len() != 64
        || !token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err("events requires a participant ID and its private collab token".into());
    }
    Ok(())
}

impl Arguments {
    fn parse(value: Value) -> Result<Self, String> {
        let mut fields = value
            .as_object()
            .cloned()
            .ok_or("events arguments must be an object")?;
        let client_id = fields
            .remove("client_id")
            .and_then(|value| value.as_str().map(str::to_owned))
            .filter(|id| identifier(id, 64))
            .ok_or("events requires client_id from list")?;
        let explicit = match (fields.remove("agent_id"), fields.remove("token")) {
            (None, None) => None,
            (Some(Value::String(id)), Some(Value::String(token))) => {
                credentials(&id, &token)?;
                Some((id, token))
            }
            _ => return Err("events agent_id and token must be supplied together".into()),
        };
        if fields.get("action").and_then(Value::as_str) == Some("policy")
            && fields.get("patch").is_some_and(|patch| !patch.is_object())
        {
            return Err("events policy patch must be an object when supplied".into());
        }
        let request: Request = serde_json::from_value(Value::Object(fields))
            .map_err(|error| format!("invalid events arguments: {error}"))?;
        match &request {
            Request::Subscribe {
                request_id,
                filter,
                after,
                source_agents,
            } => {
                if !identifier(request_id, 128) || after.is_some_and(|cursor| cursor > MAX_CURSOR) {
                    return Err(
                        "events subscribe requires a stable request_id and a valid cursor".into(),
                    );
                }
                if filter.client_ids.len() > MAX_SOURCES || source_agents.len() > MAX_SOURCES {
                    return Err("events subscriptions support at most eight source clients".into());
                }
                if filter.kinds.len() > 32 || filter.kinds.iter().any(|kind| kind.len() > 128) {
                    return Err(
                        "events subscriptions support at most 32 bounded event kinds".into(),
                    );
                }
                for (index, id) in filter.client_ids.iter().enumerate() {
                    if !identifier(id, 64) || filter.client_ids[..index].contains(id) {
                        return Err(
                            "events source client_ids must be distinct IDs from list".into()
                        );
                    }
                }
                for (index, source) in source_agents.iter().enumerate() {
                    if !identifier(&source.client_id, 64)
                        || source_agents[..index]
                            .iter()
                            .any(|old| old.client_id == source.client_id)
                    {
                        return Err("events source_agents must name distinct source clients".into());
                    }
                    credentials(&source.agent_id, &source.token)?;
                }
            }
            Request::Read {
                subscription_id,
                limit,
            }
            | Request::Wait {
                subscription_id,
                limit,
                ..
            } => {
                validate_observation(subscription_id, *limit, 0)?;
            }
            Request::Ack {
                subscription_id,
                through,
            } => {
                if !identifier(subscription_id, 128) || *through > MAX_CURSOR {
                    return Err("events ack requires a subscription ID and delivered cursor".into());
                }
            }
            Request::Unsubscribe { subscription_id } if !identifier(subscription_id, 128) => {
                return Err("events unsubscribe requires a subscription ID".into());
            }
            Request::Publish {
                request_id,
                name,
                data,
            } => {
                if !identifier(request_id, 128) || !name.starts_with("custom.") || name.len() > 96 {
                    return Err(
                        "events publish requires a stable request_id and a custom.* event name"
                            .into(),
                    );
                }
                if serde_json::to_vec(data)
                    .map_err(|_| "invalid event data")?
                    .len()
                    > 4096
                {
                    return Err("event data exceeds the 4 KiB limit".into());
                }
            }
            _ => {}
        }
        if let Request::Wait { wait_ms, .. } = &request
            && *wait_ms > crate::wire::MAX_OBSERVATION_WAIT_MS
        {
            return Err(format!(
                "events wait_ms must be 0..={}",
                crate::wire::MAX_OBSERVATION_WAIT_MS
            ));
        }
        Ok(Self {
            client_id,
            credentials: explicit,
            request,
        })
    }
}

fn validate_observation(id: &str, count: usize, timeout: u64) -> Result<(), String> {
    if !identifier(id, 128)
        || !(1..=64).contains(&count)
        || timeout > crate::wire::MAX_OBSERVATION_WAIT_MS
    {
        return Err(format!(
            "events requires a subscription ID, limit 1..=64, and wait_ms 0..={}",
            crate::wire::MAX_OBSERVATION_WAIT_MS
        ));
    }
    Ok(())
}

struct Context {
    user: String,
    actor: Actor,
    session: Arc<Session>,
    binding: RunBinding,
}

impl Context {
    async fn create(
        broker: &Broker,
        user: &str,
        access_token: &str,
        client_id: &str,
        explicit: Option<(&str, &str)>,
    ) -> Result<Self, String> {
        let authorization = authenticate(broker, user, access_token).await?;
        let owner = crate::bridge::owner(user, &authorization)?;
        let session = broker.client(user, &authorization, client_id).await?;
        let run = code_mode::AUDIT_RUN.try_with(Clone::clone).ok();
        let binding = explicit.map(|(id, token)| RunBinding {
            owner: owner.clone(), agent_id: id.into(), token: token.into(),
        }).or_else(|| run.as_ref().and_then(|run| run.binding(client_id)))
            .ok_or("events_thread_required: supply private collab credentials or bind this participant in code_mode")?;
        if binding.owner != owner {
            return Err("events participant account mismatch".into());
        }
        session
            .collaboration
            .bind(&owner, &binding.agent_id, &binding.token)?;
        if let Some(run) = &run {
            run.record_connection(&session)?;
            run.bind_identity(client_id, &owner, &binding.agent_id, &binding.token)?;
        }
        let context = Self {
            user: user.into(),
            actor: Actor {
                owner,
                client_id: client_id.into(),
                agent_id: binding.agent_id.clone(),
            },
            session,
            binding,
        };
        context.final_authorization(broker, access_token)?;
        Ok(context)
    }

    fn final_authorization(
        &self,
        broker: &Broker,
        access_token: &str,
    ) -> Result<Authorization, String> {
        let authorization = broker
            .authenticate_now(&self.user, access_token)
            .filter(|auth| matches!(auth, Authorization::Identity(_)))
            .ok_or("events authorization expired or was revoked")?;
        if !authorization.is_live()
            || !self.session.is_alive()
            || !authorization.permits(&self.session)
            || crate::bridge::owner(&self.user, &authorization).as_deref()
                != Ok(self.actor.owner.as_str())
        {
            return Err("events connection or access changed".into());
        }
        self.session.collaboration.bind(
            &self.binding.owner,
            &self.binding.agent_id,
            &self.binding.token,
        )?;
        Ok(authorization)
    }
}

async fn authenticate(
    broker: &Broker,
    user: &str,
    access_token: &str,
) -> Result<Authorization, String> {
    broker
        .authenticate(user, access_token)
        .await
        .filter(|authorization| matches!(authorization, Authorization::Identity(_)))
        .ok_or_else(|| "events requires a current authenticated account".into())
}

#[derive(Clone)]
struct Source {
    client_id: String,
    connection: Weak<Session>,
    binding: Option<RunBinding>,
}

impl Source {
    fn same(&self, other: &Self) -> bool {
        self.client_id == other.client_id
            && Weak::ptr_eq(&self.connection, &other.connection)
            && match (&self.binding, &other.binding) {
                (None, None) => true,
                (Some(one), Some(two)) => {
                    one.owner == two.owner && one.agent_id == two.agent_id && one.token == two.token
                }
                _ => false,
            }
    }

    fn live(&self, owner: &str, authorization: &Authorization) -> Result<Arc<Session>, String> {
        let session = self
            .connection
            .upgrade()
            .filter(|session| session.is_alive())
            .ok_or("events source connection ended")?;
        if session.client_id() != self.client_id || !authorization.permits(&session) {
            return Err("events source access was revoked".into());
        }
        if let Some(binding) = &self.binding {
            if binding.owner != owner {
                return Err("events source account mismatch".into());
            }
            session
                .collaboration
                .bind(owner, &binding.agent_id, &binding.token)
                .map_err(|_| "events source participant is no longer active")?;
        }
        Ok(session)
    }
}

struct Subscription {
    user: String,
    owner: String,
    client_id: String,
    agent_id: String,
    request_id: String,
    principal: Source,
    sources: Vec<Source>,
    fingerprint: Value,
    native_error: Mutex<Option<String>>,
    touched: Mutex<Instant>,
    active: AtomicBool,
}

impl Subscription {
    fn owns(&self, context: &Context) -> bool {
        self.user == context.user
            && self.owner == context.actor.owner
            && self.client_id == context.actor.client_id
            && self.agent_id == context.actor.agent_id
            && Weak::ptr_eq(
                &self.principal.connection,
                &Arc::downgrade(&context.session),
            )
            && self.principal.binding.as_ref().is_some_and(|binding| {
                binding.owner == context.binding.owner
                    && binding.agent_id == context.binding.agent_id
                    && binding.token == context.binding.token
            })
    }

    fn actor(&self) -> Actor {
        Actor {
            owner: self.owner.clone(),
            client_id: self.client_id.clone(),
            agent_id: self.agent_id.clone(),
        }
    }

    fn touch(&self) {
        *self
            .touched
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Instant::now();
    }

    fn same_delivery_scope(&self, other: &Self) -> bool {
        self.user == other.user
            && self.owner == other.owner
            && self.client_id == other.client_id
            && self.agent_id == other.agent_id
            && self.principal.same(&other.principal)
            && self.sources.len() == other.sources.len()
            && self
                .sources
                .iter()
                .zip(&other.sources)
                .all(|(one, two)| one.same(two))
    }
}

struct RunDelivery {
    context: Weak<crate::wire::CodeContext>,
    proofs: Vec<Arc<Subscription>>,
    touched: Instant,
}

#[derive(Default)]
pub(crate) struct Registry {
    workers: worker_subscriptions::Registry,
    records: Mutex<HashMap<String, Arc<Subscription>>>,
    deliveries: Mutex<HashMap<String, RunDelivery>>,
}

impl Registry {
    fn release_finished_runs(&self) {
        self.deliveries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|_, delivery| delivery.context.strong_count() != 0);
    }

    fn prune(records: &mut HashMap<String, Arc<Subscription>>, hub: &Hub) {
        let now = Instant::now();
        records.retain(|id, record| {
            let expired = now.saturating_duration_since(
                *record
                    .touched
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            ) >= RETAIN_FOR;
            if expired && Arc::strong_count(record) == 1 {
                // Ending an observer does not acknowledge retained events.
                if record.active.swap(false, Ordering::AcqRel) {
                    let _ = hub.unsubscribe(&record.actor(), id);
                }
                false
            } else {
                true
            }
        });
    }

    fn get(&self, hub: &Hub, context: &Context, id: &str) -> Result<Arc<Subscription>, String> {
        let record = self.lookup(hub, context, id, false)?;
        let clients = hub.subscription_clients(&context.actor, id)?;
        if clients
            != record
                .sources
                .iter()
                .map(|source| source.client_id.clone())
                .collect::<Vec<_>>()
        {
            return Err("events_subscription_scope_changed: source proof no longer matches the subscription".into());
        }
        Ok(record)
    }

    fn lookup(
        &self,
        hub: &Hub,
        context: &Context,
        id: &str,
        allow_inactive: bool,
    ) -> Result<Arc<Subscription>, String> {
        self.release_finished_runs();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut records, hub);
        let record = records.get(id).filter(|record| record.owns(context) && (allow_inactive || record.active.load(Ordering::Acquire)))
            .ok_or("events_subscription_unknown: subscription is unavailable for this participant and connection")?.clone();
        record.touch();
        Ok(record)
    }

    fn subscribe(
        &self,
        hub: &Hub,
        context: &Context,
        request_id: &str,
        filter: Filter,
        after: Option<u64>,
        sources: Vec<Source>,
    ) -> Result<Value, String> {
        let fingerprint = json!({"filter":filter,"after":after});
        self.release_finished_runs();
        let mut records = self
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        Self::prune(&mut records, hub);
        if let Some((id, old)) = records
            .iter()
            .find(|(_, record)| record.owns(context) && record.request_id == request_id)
        {
            if old.fingerprint != fingerprint
                || old.sources.len() != sources.len()
                || !old
                    .sources
                    .iter()
                    .zip(&sources)
                    .all(|(one, two)| one.same(two))
            {
                return Err("events_request_conflict: subscription retry changed its filter or source proof".into());
            }
            let result = hub.subscribe(&context.actor, request_id, filter, after)?;
            if result["subscription_id"].as_str() != Some(id.as_str()) {
                return Err("events_receipt_unknown: subscription identity changed; inspect existing subscriptions".into());
            }
            old.touch();
            return Ok(result);
        }
        if records.len() >= MAX_SUBSCRIPTIONS
            || records
                .values()
                .filter(|record| record.owns(context) && record.active.load(Ordering::Acquire))
                .count()
                >= MAX_ACTOR_SUBSCRIPTIONS
        {
            return Err("events_subscription_capacity: unsubscribe or wait for an inactive subscription to expire".into());
        }
        // Synchronous admission under this mutex reserves capacity before the Hub
        // mutates. A concurrent retry cannot replace the original source proofs.
        let result = hub.subscribe(&context.actor, request_id, filter, after)?;
        let id = result["subscription_id"]
            .as_str()
            .filter(|id| identifier(id, 128))
            .ok_or(
                "events_receipt_unknown: subscription may exist; inspect existing subscriptions",
            )?;
        if result["duplicate"] == true || records.contains_key(id) {
            return Err("events_subscription_proof_expired: use a new request only after inspecting the original subscription".into());
        }
        records.insert(
            id.into(),
            Arc::new(Subscription {
                user: context.user.clone(),
                owner: context.actor.owner.clone(),
                client_id: context.actor.client_id.clone(),
                agent_id: context.actor.agent_id.clone(),
                request_id: request_id.into(),
                principal: Source {
                    client_id: context.actor.client_id.clone(),
                    connection: Arc::downgrade(&context.session),
                    binding: Some(context.binding.clone()),
                },
                sources,
                fingerprint,
                native_error: Mutex::new(None),
                touched: Mutex::new(Instant::now()),
                active: AtomicBool::new(true),
            }),
        );
        Ok(result)
    }

    fn remove(&self, id: &str, record: &Arc<Subscription>) {
        let records = self
            .records
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if records
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, record))
        {
            record.active.store(false, Ordering::Release);
            record.touch();
        }
    }

    fn note_delivery(&self, proof: Arc<Subscription>) -> Result<(), String> {
        let Ok(run) = code_mode::AUDIT_RUN.try_with(Clone::clone) else {
            return Ok(());
        };
        let mut deliveries = self
            .deliveries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // A completed request cannot return again once its AuditRun is gone.
        // Keep expired live runs so finalization fails closed instead of treating
        // them as requests that never observed private event data.
        deliveries.retain(|_, delivery| delivery.context.strong_count() != 0);
        let id = &run.context.run_id;
        if !deliveries.contains_key(id) && deliveries.len() >= MAX_DELIVERY_RUNS {
            return Err(
                "events_delivery_capacity: too many event-bearing requests are still active".into(),
            );
        }
        let delivery = deliveries.entry(id.clone()).or_insert_with(|| RunDelivery {
            context: Arc::downgrade(&run.context),
            proofs: vec![],
            touched: Instant::now(),
        });
        if !Weak::ptr_eq(&delivery.context, &Arc::downgrade(&run.context)) {
            return Err("events_delivery_scope_changed: request identity changed".into());
        }
        if !delivery
            .proofs
            .iter()
            .any(|old| old.same_delivery_scope(&proof))
        {
            if delivery.proofs.len() >= MAX_RUN_PROOFS {
                return Err("events_delivery_capacity: too many distinct event source proofs in one request".into());
            }
            delivery.proofs.push(proof);
        }
        delivery.touched = Instant::now();
        Ok(())
    }

    fn note_context(&self, context: &Context) -> Result<(), String> {
        self.note_delivery(Arc::new(Subscription {
            user: context.user.clone(),
            owner: context.actor.owner.clone(),
            client_id: context.actor.client_id.clone(),
            agent_id: context.actor.agent_id.clone(),
            request_id: String::new(),
            principal: Source {
                client_id: context.actor.client_id.clone(),
                connection: Arc::downgrade(&context.session),
                binding: Some(context.binding.clone()),
            },
            sources: vec![],
            fingerprint: Value::Null,
            native_error: Mutex::new(None),
            touched: Mutex::new(Instant::now()),
            active: AtomicBool::new(false),
        }))
    }
}

async fn source_proofs(
    broker: &Broker,
    access_token: &str,
    context: &Context,
    filter: &mut Filter,
    agents: &[SourceAgent],
) -> Result<Vec<Source>, String> {
    if filter.client_ids.is_empty() {
        filter.client_ids.push(context.actor.client_id.clone());
    }
    filter.client_ids.sort();
    filter.kinds.sort();
    filter.kinds.dedup();
    if agents
        .iter()
        .any(|agent| !filter.client_ids.contains(&agent.client_id))
    {
        return Err("events source_agents must belong to the subscription's source clients".into());
    }
    let authorization = context.final_authorization(broker, access_token)?;
    let run = code_mode::AUDIT_RUN.try_with(Clone::clone).ok();
    let mut proofs = Vec::new();
    for id in &filter.client_ids {
        let session = broker.client(&context.user, &authorization, id).await?;
        let explicit = agents.iter().find(|agent| &agent.client_id == id);
        let binding = explicit
            .map(|agent| RunBinding {
                owner: context.actor.owner.clone(),
                agent_id: agent.agent_id.clone(),
                token: agent.token.clone(),
            })
            .or_else(|| {
                if id == &context.actor.client_id {
                    Some(context.binding.clone())
                } else {
                    run.as_ref().and_then(|run| run.binding(id))
                }
            });
        if id == &context.actor.client_id
            && (!Arc::ptr_eq(&session, &context.session)
                || binding.as_ref().is_none_or(|binding| {
                    binding.agent_id != context.binding.agent_id
                        || binding.token != context.binding.token
                }))
        {
            return Err("events primary source binding cannot change".into());
        }
        let source = Source {
            client_id: id.clone(),
            connection: Arc::downgrade(&session),
            binding,
        };
        source.live(&context.actor.owner, &authorization)?;
        if let Some(run) = &run {
            run.record_connection(&session)?;
            if let Some(binding) = &source.binding {
                run.bind_identity(id, &binding.owner, &binding.agent_id, &binding.token)?;
            }
        }
        proofs.push(source);
    }
    let authorization = context.final_authorization(broker, access_token)?;
    for source in &proofs {
        source.live(&context.actor.owner, &authorization)?;
    }
    Ok(proofs)
}

async fn authorize_record(
    broker: &Broker,
    access_token: &str,
    context: &Context,
    record: &Subscription,
    require_active: bool,
) -> Result<Vec<(Arc<Session>, Option<RunBinding>)>, String> {
    if !record.owns(context) || (require_active && !record.active.load(Ordering::Acquire)) {
        return Err("events subscription proof is no longer active".into());
    }
    let authorization = context.final_authorization(broker, access_token)?;
    for source in std::iter::once(&record.principal).chain(&record.sources) {
        let original = source.live(&record.owner, &authorization)?;
        let current = broker
            .client(&context.user, &authorization, &source.client_id)
            .await?;
        if !Arc::ptr_eq(&original, &current) {
            return Err("events source connection changed; retained data withheld".into());
        }
    }
    // No await follows the final account, ACL, and membership checks.
    let authorization = context.final_authorization(broker, access_token)?;
    if require_active && !record.active.load(Ordering::Acquire) {
        return Err("events subscription ended before delivery".into());
    }
    record.principal.live(&record.owner, &authorization)?;
    let sources = record
        .sources
        .iter()
        .map(|source| {
            source
                .live(&record.owner, &authorization)
                .map(|session| (session, source.binding.clone()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    record.touch();
    Ok(sources)
}

fn native_sources(
    broker: &Broker,
    record: &Subscription,
    sources: &[(Arc<Session>, Option<RunBinding>)],
) -> Value {
    let mut snapshot = broker.worker_events.snapshot(&record.owner, sources);
    if let Some(sources) = snapshot.as_array_mut() {
        for source in &record.sources {
            if source.client_id == record.client_id
                && worker_subscriptions::managed_record(broker, record)
            {
                sources.push(json!({"client_id":source.client_id,"status":"server_hub"}));
                continue;
            }
            if source.binding.is_none() {
                sources.push(json!({"client_id":source.client_id,"status":"needs_binding",
                    "message":"Join and bind this source participant separately, or supply source_agents when subscribing."}));
            } else if !sources
                .iter()
                .any(|entry| entry["client_id"].as_str() == Some(source.client_id.as_str()))
            {
                let error = record
                    .native_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                sources.push(json!({"client_id":source.client_id,
                    "status":if error.is_some(){"unavailable"}else{"not_started"},"error":error}));
            }
        }
    }
    // Snapshot fields are source metadata; no participant credential is emitted.
    snapshot
}

fn check_native_sources(sources: &Value) -> Result<(), String> {
    if let Some(sources) = sources.as_array() {
        for source in sources {
            let status = source["status"].as_str().unwrap_or("unknown");
            if matches!(status, "gap" | "overflow") {
                return Err(format!(
                    "events_source_{status}: native event history is incomplete; inspect the original task results and source cursor before continuing"
                ));
            }
        }
    }
    Ok(())
}

fn attach_native_sources(mut result: Value, sources: Value) -> Result<Value, String> {
    check_native_sources(&sources)?;
    let object = result
        .as_object_mut()
        .ok_or("events receipt is not an object")?;
    object.insert("native_sources".into(), sources);
    Ok(result)
}

pub(crate) async fn subscription_sources(
    broker: &Broker,
    user: &str,
    access_token: &str,
    client_id: &str,
    agent_id: &str,
    agent_token: &str,
    subscription_id: &str,
) -> Result<Vec<(Arc<Session>, Option<RunBinding>)>, String> {
    let context = Context::create(
        broker,
        user,
        access_token,
        client_id,
        Some((agent_id, agent_token)),
    )
    .await?;
    let record = broker
        .event_subscriptions
        .get(&broker.events, &context, subscription_id)?;
    authorize_record(broker, access_token, &context, &record, true).await
}

pub(crate) async fn authorize_observation(
    broker: &Broker,
    user: &str,
    access_token: &str,
    client_id: &str,
    agent_id: &str,
    agent_token: &str,
    subscription_id: &str,
) -> Result<(), String> {
    subscription_sources(
        broker,
        user,
        access_token,
        client_id,
        agent_id,
        agent_token,
        subscription_id,
    )
    .await
    .map(|_| ())
}

/// Call after asynchronous connection checks when several event targets share
/// one response. No await may follow the final pass across all target proofs.
pub(crate) fn authorize_observation_now(
    broker: &Broker,
    user: &str,
    access_token: &str,
    client_id: &str,
    agent_id: &str,
    agent_token: &str,
    subscription_id: &str,
) -> Result<(), String> {
    let authorization = broker
        .authenticate_now(user, access_token)
        .filter(|auth| matches!(auth, Authorization::Identity(_)))
        .ok_or("events authorization expired or was revoked")?;
    let owner = crate::bridge::owner(user, &authorization)?;
    let records = broker
        .event_subscriptions
        .records
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let record = records
        .get(subscription_id)
        .filter(|record| {
            record.active.load(Ordering::Acquire)
                && record.user == user
                && record.owner == owner
                && record.client_id == client_id
                && record.agent_id == agent_id
                && record
                    .principal
                    .binding
                    .as_ref()
                    .is_some_and(|binding| binding.token == agent_token)
        })
        .ok_or("events subscription proof is no longer active")?;
    record.principal.live(&owner, &authorization)?;
    for source in &record.sources {
        source.live(&owner, &authorization)?;
    }
    Ok(())
}

pub(crate) async fn observe(
    broker: &Broker,
    (user, access_token): (&str, &str),
    client_id: &str,
    agent_credentials: (&str, &str),
    subscription_id: &str,
    (limit, wait_ms): (usize, u64),
    exclude_origin_wait_id: Option<&str>,
) -> Result<Value, String> {
    validate_observation(subscription_id, limit, wait_ms)?;
    let context = Context::create(
        broker,
        user,
        access_token,
        client_id,
        Some(agent_credentials),
    )
    .await?;
    let record = broker
        .event_subscriptions
        .get(&broker.events, &context, subscription_id)?;
    let sources = authorize_record(broker, access_token, &context, &record, true).await?;
    broker
        .worker_events
        .touch(&record.owner, &sources, access_token);
    check_native_sources(&native_sources(broker, &record, &sources))?;
    let result = broker
        .events
        .wait_excluding(
            &context.actor,
            subscription_id,
            limit,
            wait_ms,
            exclude_origin_wait_id,
        )
        .await?;
    let sources = authorize_record(broker, access_token, &context, &record, true).await?;
    let result = attach_native_sources(result, native_sources(broker, &record, &sources))?;
    broker.event_subscriptions.note_delivery(record)?;
    Ok(result)
}

/// Reauthorize event data after JavaScript has finished using earlier replies.
/// The retained proof is independent of later bindings and unsubscribe calls.
pub(crate) async fn authorize_run(
    broker: &Broker,
    user: &str,
    access_token: &str,
    run: &code_mode::AuditRun,
) -> Result<(), String> {
    let proofs = {
        let deliveries = broker
            .event_subscriptions
            .deliveries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(delivery) = deliveries.get(&run.context.run_id) else {
            return Ok(());
        };
        if !Weak::ptr_eq(&delivery.context, &Arc::downgrade(&run.context))
            || Instant::now().saturating_duration_since(delivery.touched) >= RETAIN_FOR
        {
            return Err("events_delivery_proof_expired: retained event data withheld".into());
        }
        delivery.proofs.clone()
    };
    for proof in &proofs {
        let session = proof
            .principal
            .connection
            .upgrade()
            .ok_or("events delivery source disconnected")?;
        let binding = proof
            .principal
            .binding
            .clone()
            .ok_or("events delivery participant proof missing")?;
        if proof.user != user {
            return Err("events delivery account scope changed".into());
        }
        let context = Context {
            user: user.into(),
            actor: proof.actor(),
            session,
            binding,
        };
        authorize_record(broker, access_token, &context, proof, false).await?;
    }
    let authorization = broker
        .authenticate_now(user, access_token)
        .filter(|auth| matches!(auth, Authorization::Identity(_)))
        .ok_or("events delivery authorization expired or was revoked")?;
    let owner = crate::bridge::owner(user, &authorization)?;
    // Every source is rechecked synchronously after the last asynchronous lookup.
    for proof in proofs {
        if proof.owner != owner {
            return Err("events delivery account scope changed".into());
        }
        proof.principal.live(&owner, &authorization)?;
        for source in &proof.sources {
            source.live(&owner, &authorization)?;
        }
    }
    Ok(())
}

pub(crate) fn wait_transition(
    broker: &Broker,
    owner: &str,
    client_id: &str,
    agent_id: &str,
    wait_id: &str,
    kind: &str,
    data: Value,
) {
    let mut fields = data.as_object().cloned().unwrap_or_default();
    fields.insert("origin_wait_id".into(), json!(wait_id));
    // Journal pressure must not turn an already completed operation into failure.
    let _ = broker.events.publish_system(
        owner,
        client_id,
        Some(agent_id),
        kind,
        Value::Object(fields),
        (None, None),
    );
}

struct DirectObservation<'a> {
    broker: &'a Broker,
    actor: &'a Actor,
    id: String,
    suspended: bool,
    completed: bool,
}

impl<'a> DirectObservation<'a> {
    fn start(broker: &'a Broker, actor: &'a Actor, subscription_id: &str) -> Result<Self, String> {
        let mut bytes = [0u8; 16];
        crate::platform::random_bytes(&mut bytes)
            .map_err(|_| "events observation ID unavailable")?;
        let id = format!("event_wait_{}", crate::hex::encode(bytes));
        wait_transition(
            broker,
            &actor.owner,
            &actor.client_id,
            &actor.agent_id,
            &id,
            "wait.started",
            json!({"api":"events","subscription_id":subscription_id}),
        );
        Ok(Self {
            broker,
            actor,
            id,
            suspended: false,
            completed: false,
        })
    }

    fn transition(&self, kind: &str, data: Value) {
        wait_transition(
            self.broker,
            &self.actor.owner,
            &self.actor.client_id,
            &self.actor.agent_id,
            &self.id,
            kind,
            data,
        );
    }

    fn suspend(&mut self) {
        self.suspended = true;
        self.transition("wait.suspended", json!({"api":"events"}));
    }

    fn finish(&mut self, outcome: &str) {
        if self.suspended {
            self.transition("wait.resumed", json!({"api":"events","outcome":outcome}));
        }
        self.transition("wait.completed", json!({"api":"events","outcome":outcome}));
        self.completed = true;
    }
}

impl Drop for DirectObservation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.transition(
                "wait.completed",
                json!({"api":"events","outcome":"observer_cancelled"}),
            );
        }
    }
}

pub(crate) async fn dispatch(
    broker: Arc<Broker>,
    user: String,
    access_token: String,
    arguments: Value,
) -> Result<Value, String> {
    let args = Arguments::parse(arguments)?;
    let context = Context::create(
        &broker,
        &user,
        &access_token,
        &args.client_id,
        args.credentials
            .as_ref()
            .map(|(id, token)| (id.as_str(), token.as_str())),
    )
    .await?;
    let mut mutation = false;
    let mut result = match args.request {
        Request::Subscribe {
            request_id,
            mut filter,
            after,
            source_agents,
        } => {
            let sources = source_proofs(
                &broker,
                &access_token,
                &context,
                &mut filter,
                &source_agents,
            )
            .await?;
            let mut result = broker.event_subscriptions.subscribe(
                &broker.events,
                &context,
                &request_id,
                filter,
                after,
                sources,
            )?;
            if result["status"] == "unsubscribed" {
                let record = broker.event_subscriptions.lookup(
                    &broker.events,
                    &context,
                    result["subscription_id"]
                        .as_str()
                        .ok_or("events subscription receipt has no ID")?,
                    true,
                )?;
                broker.event_subscriptions.note_delivery(record)?;
                context.final_authorization(&broker, &access_token)?;
                return Ok(result);
            }
            let id = result["subscription_id"]
                .as_str()
                .ok_or("events subscription receipt has no ID")?;
            let record = broker
                .event_subscriptions
                .get(&broker.events, &context, id)?;
            let sources = authorize_record(&broker, &access_token, &context, &record, true)
                .await
                .map_err(|error| {
                    format!("events_response_withheld: subscription may exist; {error}")
                })?;
            if record
                .sources
                .iter()
                .any(|source| source.client_id == context.actor.client_id)
            {
                context.session.seed_user_input_events(
                    &context.actor.owner,
                    &context.actor.agent_id,
                    id,
                );
            }
            *record
                .native_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = crate::broker::worker_events::ensure(
                &broker,
                &user,
                &access_token,
                &context.actor.owner,
                &sources,
            )
            .err()
            .map(|error| error.chars().take(512).collect::<String>());
            result["native_sources"] = native_sources(&broker, &record, &sources);
            broker.event_subscriptions.note_delivery(record)?;
            mutation = true;
            result
        }
        Request::Read {
            subscription_id,
            limit,
        } => {
            let record =
                broker
                    .event_subscriptions
                    .get(&broker.events, &context, &subscription_id)?;
            let sources = authorize_record(&broker, &access_token, &context, &record, true).await?;
            broker
                .worker_events
                .touch(&record.owner, &sources, &access_token);
            check_native_sources(&native_sources(&broker, &record, &sources))?;
            let result = broker
                .events
                .read(&context.actor, &subscription_id, limit)?;
            let sources = authorize_record(&broker, &access_token, &context, &record, true).await?;
            let result = attach_native_sources(result, native_sources(&broker, &record, &sources))?;
            broker.event_subscriptions.note_delivery(record)?;
            result
        }
        Request::Wait {
            subscription_id,
            limit,
            wait_ms,
        } => {
            let record =
                broker
                    .event_subscriptions
                    .get(&broker.events, &context, &subscription_id)?;
            let sources = authorize_record(&broker, &access_token, &context, &record, true).await?;
            broker
                .worker_events
                .touch(&record.owner, &sources, &access_token);
            check_native_sources(&native_sources(&broker, &record, &sources))?;
            let mut observation =
                DirectObservation::start(&broker, &context.actor, &subscription_id)?;
            // The subscription and its sources have been authorized above.
            // Display owns no event cursor, native permit or delivery authority.
            let display = context.session.show_wait(
                &broker,
                crate::broker::wait_display::Request {
                    user: &user,
                    token: &access_token,
                    binding: &context.binding,
                    wait_id: &observation.id,
                    operation: "events.wait",
                    targets: vec![crate::wire::wait_display::Kind::Event],
                    target_labels: vec!["Subscription events".into()],
                    target_details: vec![format!(
                        "Subscription ID {subscription_id}; up to {limit} matching authorized events"
                    )],
                    mode: crate::wire::wait_display::Mode::Single,
                    timeout_ms: wait_ms,
                },
            );
            let started = Instant::now();
            let result: Result<Value, String> = async {
                let first = broker
                    .events
                    .wait_excluding(
                        &context.actor,
                        &subscription_id,
                        limit,
                        0,
                        Some(&observation.id),
                    )
                    .await?;
                let remaining = wait_ms
                    .saturating_sub(started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64);
                let mut result = if first["status"] == "pending" && remaining > 0 {
                    observation.suspend();
                    observe(
                        &broker,
                        (&user, &access_token),
                        &context.actor.client_id,
                        (&context.actor.agent_id, &context.binding.token),
                        &subscription_id,
                        (limit, remaining),
                        Some(&observation.id),
                    )
                    .await?
                } else {
                    let sources =
                        authorize_record(&broker, &access_token, &context, &record, true).await?;
                    let result =
                        attach_native_sources(first, native_sources(&broker, &record, &sources))?;
                    broker.event_subscriptions.note_delivery(record.clone())?;
                    result
                };
                result["observation_id"] = json!(observation.id);
                Ok(result)
            }
            .await;
            let outcome = result
                .as_ref()
                .ok()
                .and_then(|value| value["status"].as_str())
                .unwrap_or("error");
            observation.finish(outcome);
            if let Some(display) = display {
                use crate::wire::wait_display::Outcome;
                display.finish(match outcome {
                    "ready" => Outcome::Triggered,
                    "pending" => Outcome::WindowExpired,
                    _ => Outcome::Unknown,
                });
            }
            result?
        }
        Request::Ack {
            subscription_id,
            through,
        } => {
            let record =
                broker
                    .event_subscriptions
                    .get(&broker.events, &context, &subscription_id)?;
            authorize_record(&broker, &access_token, &context, &record, true).await?;
            let result = broker
                .events
                .ack(&context.actor, &subscription_id, through)?;
            authorize_record(&broker, &access_token, &context, &record, true)
                .await
                .map_err(|error| {
                    format!("events_response_withheld: acknowledgement may have completed; {error}")
                })?;
            broker.event_subscriptions.note_delivery(record)?;
            mutation = true;
            result
        }
        Request::Unsubscribe { subscription_id } => {
            let record = broker.event_subscriptions.lookup(
                &broker.events,
                &context,
                &subscription_id,
                true,
            )?;
            let managed = worker_subscriptions::managed_record(&broker, &record);
            if managed {
                // Revoking one's own observer exposes no source data. The
                // original main proof suffices even after its pool disconnects.
                let authorization = context.final_authorization(&broker, &access_token)?;
                record.principal.live(&record.owner, &authorization)?;
            } else {
                authorize_record(&broker, &access_token, &context, &record, false).await?;
            }
            let result = broker
                .events
                .unsubscribe(&context.actor, &subscription_id)?;
            broker.event_subscriptions.remove(&subscription_id, &record);
            worker_subscriptions::unsubscribed(&broker, &context, &subscription_id);
            if managed {
                let authorization = context.final_authorization(&broker, &access_token)?;
                record.principal.live(&record.owner, &authorization)?;
            } else {
                authorize_record(&broker, &access_token, &context, &record, false)
                    .await
                    .map_err(|error| {
                        format!("events_response_withheld: unsubscribe may have completed; {error}")
                    })?;
                broker.event_subscriptions.note_delivery(record)?;
            }
            mutation = true;
            result
        }
        Request::Subscriptions {} => {
            if let Ok(run) = code_mode::AUDIT_RUN.try_with(Clone::clone) {
                worker_subscriptions::after_bind(&broker, &user, &access_token, &run).await;
            }
            let mut result = broker.events.subscriptions(&context.actor)?;
            let listed = result["subscriptions"]
                .as_array()
                .ok_or("events subscription listing is unavailable")?;
            let mut records = Vec::new();
            for subscription in listed {
                let id = subscription["subscription_id"]
                    .as_str()
                    .ok_or("events subscription listing has an invalid ID")?;
                let record = broker
                    .event_subscriptions
                    .get(&broker.events, &context, id)?;
                authorize_record(&broker, &access_token, &context, &record, true).await?;
                records.push(record);
            }
            let authorization = context.final_authorization(&broker, &access_token)?;
            for record in records {
                if !record.active.load(Ordering::Acquire) {
                    return Err("events subscription ended before listing delivery".into());
                }
                record.principal.live(&record.owner, &authorization)?;
                for source in &record.sources {
                    source.live(&record.owner, &authorization)?;
                }
                broker.event_subscriptions.note_delivery(record)?;
            }
            if let Some(subscriptions) = result["subscriptions"].as_array_mut() {
                for subscription in subscriptions {
                    worker_subscriptions::annotate(&broker, &context, subscription);
                }
            }
            result["worker_subscription"] = worker_subscriptions::status(&broker, &context);
            result
        }
        Request::Publish {
            request_id,
            name,
            data,
        } => {
            mutation = true;
            broker
                .events
                .publish_custom(&context.actor, &request_id, &name, data)?
        }
        Request::Policy { patch } => {
            mutation = patch.is_some();
            let mut result = super::coordination_policy::policy(
                &context.actor.owner,
                &context.actor.client_id,
                &context.actor.agent_id,
                patch.as_ref(),
            )?;
            worker_subscriptions::configure(&broker, &context, patch.as_ref())?;
            if let Ok(run) = code_mode::AUDIT_RUN.try_with(Clone::clone) {
                worker_subscriptions::after_bind(&broker, &user, &access_token, &run).await;
            }
            let managed = worker_subscriptions::status(&broker, &context);
            let applicable = managed["status"] != "not_applicable";
            if applicable {
                // Live managed choices outlast the advisory policy cache.
                // Partial patches preserve unmentioned fields in that live entry.
                result["enabled"] = managed["enabled"].clone();
                result["kinds"] = managed["kinds"].clone();
                result["managed_retention"] = json!("original_session");
            }
            result["advisory"] = json!(!applicable);
            result["advisory_fields"] = json!(["window_ms", "strategy", "dreaming"]);
            result["worker_subscription"] = managed;
            result
        }
    };
    context.final_authorization(&broker, &access_token).map_err(|error| {
        if mutation { format!("events_response_withheld: operation may have completed; inspect its original receipt; {error}") } else { error }
    })?;
    worker_subscriptions::annotate(&broker, &context, &mut result);
    broker.event_subscriptions.note_context(&context)?;
    Ok(result)
}

pub(crate) fn tool() -> Value {
    let mut common = serde_json::Map::new();
    common.insert(
        "client_id".into(),
        json!({"type":"string","minLength":1,"maxLength":64}),
    );
    common.insert(
        "agent_id".into(),
        json!({"type":"string","minLength":1,"maxLength":64}),
    );
    common.insert(
        "token".into(),
        json!({"type":"string","pattern":"^[0-9a-fA-F]{64}$"}),
    );
    let variant = |action: &str, fields: Value, required: &[&str]| {
        let mut properties = common.clone();
        properties.insert("action".into(), json!({"const":action}));
        properties.extend(fields.as_object().unwrap().clone());
        let mut required_fields = vec!["client_id", "action"];
        required_fields.extend_from_slice(required);
        json!({"type":"object","properties":properties,"required":required_fields,"additionalProperties":false,
            "dependentRequired":{"agent_id":["token"],"token":["agent_id"]}})
    };
    let id = json!({"type":"string","minLength":1,"maxLength":128});
    let count = json!({"type":"integer","minimum":1,"maximum":64,"default":32});
    json!({
        "name":"events",
        "description":"Publish custom events, subscribe to authenticated source clients, read or await matching events, and explicitly acknowledge processed cursors. Direct waits use no JavaScript slot and last at most 55000 ms. Keep subscription_id across observation windows; read/wait never acknowledge. Each source connection and bound participant is reauthorized before delivery. source_agents authorizes source operations, not another participant's private event inbox. Lifecycle events are system-owned. For a verified main with a separately bound linked pool, a real worker subscription is created automatically. policy enabled/kinds updates that subscription without ACK; subscriptions reports automatic/configured/manual origin and current cursors. window_ms, strategy and dreaming remain advisory preferences.",
        "inputSchema":{"type":"object","oneOf":[
            variant("subscribe",json!({"request_id":id,"after":{"type":"integer","minimum":0,"maximum":MAX_CURSOR},
                "filter":{"type":"object","additionalProperties":false,"properties":{
                    "kinds":{"type":"array","items":{"type":"string"},"description":"Exact event kinds or trailing .* prefixes; empty matches all kinds."},
                    "client_ids":{"type":"array","maxItems":MAX_SOURCES,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":64}},
                    "actor_id":{"type":["string","null"]},"task_id":{"type":["string","null"]},
                    "include_own_waits":{"type":"boolean","default":false}}},
                "source_agents":{"type":"array","maxItems":MAX_SOURCES,"items":{"type":"object","additionalProperties":false,"required":["client_id","agent_id","token"],"properties":common}}
            }), &["request_id"]),
            variant("read",json!({"subscription_id":id,"limit":count}), &["subscription_id"]),
            variant("wait",json!({"subscription_id":id,"limit":count,"wait_ms":{"type":"integer","minimum":0,"maximum":crate::wire::MAX_OBSERVATION_WAIT_MS,"default":crate::wire::MAX_OBSERVATION_WAIT_MS}}), &["subscription_id"]),
            variant("ack",json!({"subscription_id":id,"through":{"type":"integer","minimum":0,"maximum":MAX_CURSOR}}), &["subscription_id","through"]),
            variant("unsubscribe",json!({"subscription_id":id}), &["subscription_id"]),
            variant("subscriptions",json!({}), &[]),
            variant("publish",json!({"request_id":id,"name":{"type":"string","pattern":"^custom\\.","maxLength":96},"data":{}}), &["request_id","name","data"]),
            variant("policy",json!({"patch":{"type":"object","additionalProperties":false,"properties":{
                "enabled":{"type":"boolean"},"window_ms":{"type":"integer","minimum":1000,"maximum":crate::wire::MAX_OBSERVATION_WAIT_MS},
                "kinds":{"type":"array","minItems":1,"maxItems":16,"uniqueItems":true,"items":{"type":"string"}},
                "strategy":{"type":["string","null"],"maxLength":512},
                "dreaming":{"type":["object","null"],"additionalProperties":false,
                    "required":["round_id","repository","budget_ms"],"properties":{
                    "round_id":{"type":"string","pattern":"^[A-Za-z0-9_-]{1,128}$"},
                    "repository":{"type":"string","enum":["project","global"]},
                    "budget_ms":{"type":"integer","minimum":1000,"maximum":crate::wire::MAX_OBSERVATION_WAIT_MS}}}
            }}}), &[])
        ]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::Identity;

    struct Fixture {
        broker: Arc<Broker>,
        session: Arc<Session>,
        access_token: String,
        owner: String,
        member: Value,
    }

    impl Fixture {
        async fn new() -> Self {
            let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
            let session = broker
                .test_register_identity(
                    "event-fixture",
                    Identity {
                        provider: "github".into(),
                        subject: "42".into(),
                        login: "alice".into(),
                    },
                )
                .await;
            let access_token = broker.oauth.test_identity_tokens("mcp", "42")["access_token"]
                .as_str()
                .unwrap()
                .to_owned();
            let authorization = broker.authenticate("mcp", &access_token).await.unwrap();
            let owner = crate::bridge::owner("mcp", &authorization).unwrap();
            let member = session
                .collaboration
                .dispatch(&owner, "join", json!({"name":"main","role":"main"}))
                .await
                .unwrap();
            Self {
                broker,
                session,
                access_token,
                owner,
                member,
            }
        }

        async fn source(&self) -> (Arc<Session>, Value) {
            let session = self
                .broker
                .test_register_identity(
                    "event-source",
                    Identity {
                        provider: "github".into(),
                        subject: "42".into(),
                        login: "alice".into(),
                    },
                )
                .await;
            let member = session
                .collaboration
                .dispatch(&self.owner, "join", json!({"name":"source"}))
                .await
                .unwrap();
            (session, member)
        }

        async fn call(&self, mut arguments: Value) -> Result<Value, String> {
            arguments["client_id"] = json!(self.session.client_id());
            arguments["agent_id"] = self.member["agent_id"].clone();
            arguments["token"] = self.member["token"].clone();
            dispatch(
                self.broker.clone(),
                "mcp".into(),
                self.access_token.clone(),
                arguments,
            )
            .await
        }

        fn source_args(&self, source: &Session, member: &Value, request: &str) -> Value {
            json!({"action":"subscribe","request_id":request,
                "filter":{"client_ids":[source.client_id()],"kinds":["custom.done"]},
                "source_agents":[{"client_id":source.client_id(),"agent_id":member["agent_id"],"token":member["token"]}]})
        }
    }

    #[test]
    fn parses_only_the_fields_for_the_selected_action() {
        for value in [
            json!({"client_id":"client","action":"read","subscription_id":"sub","ack":true}),
            json!({"client_id":"client","action":"read","subscription_id":"sub","agent_id":"agent"}),
            json!({"client_id":"client","action":"read","subscription_id":"sub","token":"a".repeat(64)}),
            json!({"client_id":"client","action":"wait","subscription_id":"sub","wait_ms":55001}),
            json!({"client_id":"client","action":"read","subscription_id":"sub","limit":65}),
            json!({"client_id":"client","action":"publish","request_id":"request","name":"worker.completed","data":{}}),
            json!({"client_id":"client","action":"policy","patch":null}),
            json!({"client_id":"client","action":"subscribe","request_id":"request","source_agents":[{"client_id":"source","agent_id":"agent","token":"a".repeat(64),"owner":"forged"}]}),
        ] {
            assert!(Arguments::parse(value).is_err());
        }
        let parsed =
            Arguments::parse(json!({"client_id":"client","action":"wait","subscription_id":"sub"}))
                .unwrap();
        assert!(matches!(
            parsed.request,
            Request::Wait {
                wait_ms: 55000,
                limit: 32,
                ..
            }
        ));
        assert!(parsed.credentials.is_none());
    }

    #[tokio::test]
    async fn subscribe_retries_cannot_replace_a_source_participant_proof() {
        let fixture = Fixture::new().await;
        let (source, member) = fixture.source().await;
        let args = fixture.source_args(&source, &member, "same-request");
        let first = fixture.call(args.clone()).await.unwrap();
        let duplicate = fixture.call(args.clone()).await.unwrap();
        assert_eq!(duplicate["subscription_id"], first["subscription_id"]);
        assert_eq!(duplicate["duplicate"], true);
        let another = source
            .collaboration
            .dispatch(
                &fixture.owner,
                "join",
                json!({"name":"another-source-member"}),
            )
            .await
            .unwrap();
        let changed = fixture.source_args(&source, &another, "same-request");
        assert!(
            fixture
                .call(changed)
                .await
                .unwrap_err()
                .starts_with("events_request_conflict")
        );
        assert_eq!(
            fixture
                .call(json!({"action":"subscriptions"}))
                .await
                .unwrap()["subscriptions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn native_source_health_follows_each_subscriptions_explicit_binding() {
        let fixture = Fixture::new().await;
        let (source, first_member) = fixture.source().await;
        let second_member = source
            .collaboration
            .dispatch(
                &fixture.owner,
                "join",
                json!({"name":"second-source-member"}),
            )
            .await
            .unwrap();
        let first = fixture
            .call(fixture.source_args(&source, &first_member, "first-source-health"))
            .await
            .unwrap();
        let second = fixture
            .call(fixture.source_args(&source, &second_member, "second-source-health"))
            .await
            .unwrap();

        for (subscription, member) in [(&first, &first_member), (&second, &second_member)] {
            let read = fixture
                .call(json!({"action":"read","subscription_id":subscription["subscription_id"]}))
                .await
                .unwrap();
            assert_eq!(read["status"], "pending");
            for response in [subscription, &read] {
                let sources = response["native_sources"].as_array().unwrap();
                assert_eq!(sources.len(), 1);
                assert_eq!(sources[0]["client_id"], source.client_id());
                assert_eq!(sources[0]["agent_id"], member["agent_id"]);
            }
        }

        let unbound = fixture
            .call(json!({
                "action":"subscribe","request_id":"unbound-source-health",
                "filter":{"client_ids":[source.client_id()],"kinds":["custom.done"]}
            }))
            .await
            .unwrap();
        let read = fixture
            .call(json!({"action":"read","subscription_id":unbound["subscription_id"]}))
            .await
            .unwrap();
        for response in [unbound, read] {
            let sources = response["native_sources"].as_array().unwrap();
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0]["client_id"], source.client_id());
            assert_eq!(sources[0]["status"], "needs_binding");
            assert!(sources[0].get("agent_id").is_none());
        }
    }

    #[tokio::test]
    async fn explicit_source_bindings_do_not_merge_private_event_audiences() {
        let fixture = Fixture::new().await;
        let (source, member) = fixture.source().await;
        let mut args = fixture.source_args(&source, &member, "source-private");
        args["filter"]["kinds"] = json!(["message.sent"]);
        let subscription = fixture.call(args).await.unwrap();
        fixture
            .broker
            .events
            .publish_system(
                &fixture.owner,
                source.client_id(),
                member["agent_id"].as_str(),
                "message.sent",
                json!({"message_id":"private-message"}),
                (
                    Some(vec![format!(
                        "{}:{}",
                        source.client_id(),
                        member["agent_id"].as_str().unwrap()
                    )]),
                    None,
                ),
            )
            .unwrap();
        let read = fixture
            .call(json!({"action":"read","subscription_id":subscription["subscription_id"]}))
            .await
            .unwrap();
        assert_eq!(read["status"], "pending");
        assert_eq!(read["events"], json!([]));
        let own = dispatch(fixture.broker.clone(), "mcp".into(), fixture.access_token.clone(), json!({
            "client_id":source.client_id(),"agent_id":member["agent_id"],"token":member["token"],
            "action":"subscribe","request_id":"own-private","after":0,"filter":{"kinds":["message.sent"]}
        })).await.unwrap();
        let visible = dispatch(fixture.broker.clone(), "mcp".into(), fixture.access_token.clone(), json!({
            "client_id":source.client_id(),"agent_id":member["agent_id"],"token":member["token"],
            "action":"read","subscription_id":own["subscription_id"]
        })).await.unwrap();
        assert_eq!(
            visible["events"][0]["data"]["message_id"],
            "private-message"
        );
    }

    #[tokio::test]
    async fn data_retained_by_javascript_is_reauthorized_after_unsubscribe() {
        let fixture = Fixture::new().await;
        let (source, member) = fixture.source().await;
        let run = code_mode::AuditRun::new("event delivery fixture", None).unwrap();
        code_mode::AUDIT_RUN.scope(run.clone(), async {
            let subscription = fixture.call(fixture.source_args(&source, &member, "delivery-proof")).await.unwrap();
            fixture.broker.events.publish_custom(&crate::events::Actor {
                owner:fixture.owner.clone(),client_id:source.client_id().into(),
                agent_id:member["agent_id"].as_str().unwrap().into(),
            }, "delivery-proof-event", "custom.done", json!({"task_id":"done"})).unwrap();
            let read = fixture.call(json!({"action":"read","subscription_id":subscription["subscription_id"]})).await.unwrap();
            assert_eq!(read["status"], "ready");
            fixture.call(json!({"action":"unsubscribe","subscription_id":subscription["subscription_id"]})).await.unwrap();
            authorize_run(&fixture.broker, "mcp", &fixture.access_token, &run).await.unwrap();
            source.collaboration.dispatch(&fixture.owner, "leave", json!({"agent_id":member["agent_id"],"token":member["token"]})).await.unwrap();
            assert!(authorize_run(&fixture.broker, "mcp", &fixture.access_token, &run).await.is_err());
        }).await;
    }

    #[tokio::test]
    async fn expiry_of_a_live_delivery_proof_fails_closed() {
        let fixture = Fixture::new().await;
        let run = code_mode::AuditRun::new("expired event delivery fixture", None).unwrap();
        code_mode::AUDIT_RUN
            .scope(run.clone(), async {
                fixture
                    .call(json!({"action":"subscribe","request_id":"expires"}))
                    .await
                    .unwrap();
            })
            .await;
        fixture
            .broker
            .event_subscriptions
            .deliveries
            .lock()
            .unwrap()
            .get_mut(&run.context.run_id)
            .unwrap()
            .touched = Instant::now() - RETAIN_FOR;
        assert!(
            authorize_run(&fixture.broker, "mcp", &fixture.access_token, &run)
                .await
                .unwrap_err()
                .starts_with("events_delivery_proof_expired")
        );
        let unrelated = code_mode::AuditRun::new("no event operations", None).unwrap();
        authorize_run(&fixture.broker, "mcp", "not-a-token", &unrelated)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn admission_is_bounded_before_hub_mutation_and_unsubscribe_is_repeatable() {
        let fixture = Fixture::new().await;
        let mut first = Value::Null;
        for index in 0..MAX_ACTOR_SUBSCRIPTIONS {
            let receipt = fixture
                .call(json!({"action":"subscribe","request_id":format!("request-{index}")}))
                .await
                .unwrap();
            if index == 0 {
                first = receipt;
            }
        }
        let repeated = fixture
            .call(json!({"action":"subscribe","request_id":"request-0"}))
            .await
            .unwrap();
        assert_eq!(repeated["subscription_id"], first["subscription_id"]);
        assert!(
            fixture
                .call(json!({"action":"subscribe","request_id":"capacity-extra"}))
                .await
                .unwrap_err()
                .starts_with("events_subscription_capacity")
        );
        assert_eq!(
            fixture
                .call(json!({"action":"subscriptions"}))
                .await
                .unwrap()["subscriptions"]
                .as_array()
                .unwrap()
                .len(),
            MAX_ACTOR_SUBSCRIPTIONS
        );
        for duplicate in [false, true] {
            let receipt = fixture
                .call(json!({"action":"unsubscribe","subscription_id":first["subscription_id"]}))
                .await
                .unwrap();
            assert_eq!(receipt["duplicate"], duplicate);
        }
        let closed = fixture
            .call(json!({"action":"subscribe","request_id":"request-0"}))
            .await
            .unwrap();
        assert_eq!(closed["status"], "unsubscribed");
        fixture
            .call(json!({"action":"subscribe","request_id":"capacity-after-unsubscribe"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn existing_user_input_is_signalled_without_consuming_its_text() {
        let fixture = Fixture::new().await;
        fixture
            .session
            .steering
            .command(
                fixture.session.client_id(),
                fixture.session.collaboration.presence().unwrap(),
                crate::wire::SteeringCommand::Publish {
                    command_id: "publish-before-subscribe".into(),
                    message_id: "queued-input".into(),
                    expected_revision: 0,
                    text: "private user instruction".into(),
                    target: fixture.member["agent_id"].as_str().unwrap().into(),
                },
            )
            .unwrap();
        let subscription = fixture
            .call(json!({"action":"subscribe","request_id":"existing-input",
            "filter":{"kinds":["user.input_available"]}}))
            .await
            .unwrap();
        let read = fixture
            .call(json!({"action":"read","subscription_id":subscription["subscription_id"]}))
            .await
            .unwrap();
        assert_eq!(read["status"], "ready");
        assert!(!read.to_string().contains("private user instruction"));
        let snapshot = fixture
            .session
            .steering
            .command(
                fixture.session.client_id(),
                vec![],
                crate::wire::SteeringCommand::Snapshot {},
            )
            .unwrap();
        assert_eq!(
            snapshot
                .messages
                .iter()
                .find(|message| message.id == "queued-input")
                .unwrap()
                .state,
            crate::wire::SteeringState::Queued
        );
    }

    #[tokio::test]
    async fn direct_wait_has_lifecycle_events_without_waking_itself() {
        let fixture = Fixture::new().await;
        let subscription = fixture
            .call(json!({"action":"subscribe","request_id":"self-wait",
            "filter":{"kinds":["wait.*"],"include_own_waits":true}}))
            .await
            .unwrap();
        let waited = fixture.call(json!({"action":"wait","subscription_id":subscription["subscription_id"],"wait_ms":5})).await.unwrap();
        assert_eq!(waited["status"], "pending");
        let read = fixture
            .call(json!({"action":"read","subscription_id":subscription["subscription_id"]}))
            .await
            .unwrap();
        let events = read["events"].as_array().unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(
            events
                .iter()
                .map(|event| event["kind"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "wait.started",
                "wait.suspended",
                "wait.resumed",
                "wait.completed"
            ]
        );
        assert!(
            events
                .iter()
                .all(|event| event["data"]["origin_wait_id"] == waited["observation_id"])
        );
    }

    #[test]
    fn incomplete_native_history_is_not_reported_as_a_successful_observation() {
        for status in ["gap", "overflow"] {
            let error = attach_native_sources(
                json!({"status":"ready","events":[{}]}),
                json!([{"client_id":"source","status":status}]),
            )
            .unwrap_err();
            assert!(error.starts_with(&format!("events_source_{status}")));
        }
        assert!(
            attach_native_sources(
                json!({"status":"ready","events":[{}]}),
                json!([{"client_id":"control","status":"unsupported"}])
            )
            .is_ok()
        );
    }
}
