//! Account-scoped event metadata retained across bounded observation calls.
//!
//! Callers authenticate the actor and every selected client before each read
//! and before delivery. Publishers supply metadata without message bodies or
//! credentials. Audience checks and explicit acknowledgement stay in this hub.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

const RETENTION: Duration = Duration::from_secs(15 * 60);
const MAX_OWNERS: usize = 64;
const MAX_EVENTS: usize = 1024;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;
const MAX_EVENT_BYTES: usize = 4096;
const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_SUBSCRIBE_RECEIPTS: usize = 128;
const MAX_SUBSCRIBE_BYTES: usize = 128 * 1024;
const MAX_PUBLISH_RECEIPTS: usize = 1024;
const MAX_RECEIPT_BYTES: usize = 1024 * 1024;
const MAX_READ_EVENTS: usize = 64;
const MAX_READ_BYTES: usize = 64 * 1024;
const MAX_WAITERS: usize = 128;
const MAX_WAIT_MS: u64 = 55_000;
const MAX_SEQUENCE: u64 = (1_u64 << 53) - 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Actor {
    pub owner: String,
    pub client_id: String,
    pub agent_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Filter {
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default)]
    pub client_ids: Vec<String>,
    #[serde(default)]
    pub actor_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub include_own_waits: bool,
}

#[derive(Default)]
pub(crate) struct Hub {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    owners: BTreeMap<String, Journal>,
    waiters: usize,
}

struct Journal {
    generation: String,
    sequence: u64,
    oldest_cursor: u64,
    events: VecDeque<Event>,
    event_bytes: usize,
    subscriptions: BTreeMap<String, Subscription>,
    subscribed: VecDeque<SubscribeReceipt>,
    published: VecDeque<PublishReceipt>,
}

struct Event {
    seq: u64,
    client_id: String,
    actor_id: Option<String>,
    kind: String,
    audience: Option<Vec<String>>,
    value: Value,
    value_bytes: usize,
    retained_bytes: usize,
    at: Instant,
}

struct Subscription {
    actor: Actor,
    filter: Filter,
    cursor: u64,
    delivered: u64,
    expires: Instant,
    closed: bool,
}

struct SubscribeReceipt {
    actor: Actor,
    request_id: String,
    filter: Filter,
    after: Option<u64>,
    subscription_id: String,
    initial_cursor: u64,
    bytes: usize,
    expires: Instant,
}

#[derive(Clone, PartialEq, Eq)]
struct PublishKey {
    client_id: String,
    agent_id: Option<String>,
    request_id: String,
}

#[derive(Clone, PartialEq)]
struct Publication {
    client_id: String,
    actor_id: Option<String>,
    kind: String,
    origin: &'static str,
    data: Value,
    audience: Option<Vec<String>>,
}

struct PublishReceipt {
    key: PublishKey,
    publication: Publication,
    response: Value,
    bytes: usize,
    expires: Instant,
}

impl Journal {
    fn new() -> Result<Self, String> {
        Ok(Self {
            generation: nonce()?,
            sequence: 0,
            oldest_cursor: 0,
            events: VecDeque::new(),
            event_bytes: 0,
            subscriptions: BTreeMap::new(),
            subscribed: VecDeque::new(),
            published: VecDeque::new(),
        })
    }

    fn prune(&mut self, now: Instant) {
        while self
            .events
            .front()
            .is_some_and(|event| now.duration_since(event.at) >= RETENTION)
        {
            self.evict_front();
        }
        self.subscriptions
            .retain(|_, subscription| subscription.expires > now);
        self.subscribed.retain(|receipt| receipt.expires > now);
        self.published.retain(|receipt| receipt.expires > now);
    }

    fn evict_front(&mut self) {
        if let Some(event) = self.events.pop_front() {
            self.event_bytes -= event.retained_bytes;
            self.oldest_cursor = event.seq;
        }
    }

    fn system_gap(&mut self, seq: u64) {
        // The successful source operation has an event we could not retain.
        // Advance an explicit loss boundary; never make an old cursor appear
        // current or synthesize a completed task from the missing metadata.
        self.sequence = seq;
        self.oldest_cursor = seq;
        self.events.clear();
        self.event_bytes = 0;
    }

    fn check_cursor(&self, cursor: u64) -> Result<(), String> {
        if cursor < self.oldest_cursor {
            return Err(format!(
                "events_gap: cursor {cursor} precedes retained cursor {}; latest cursor {}; resubscribe with an explicit retained cursor after reconciling the gap",
                self.oldest_cursor, self.sequence
            ));
        }
        if cursor > self.sequence {
            return Err(format!(
                "events_cursor_ahead: cursor {cursor} exceeds latest cursor {}",
                self.sequence
            ));
        }
        Ok(())
    }

    fn subscription(&self, actor: &Actor, id: &str) -> Result<&Subscription, String> {
        self.subscriptions
            .get(id)
            .filter(|subscription| &subscription.actor == actor)
            .ok_or_else(|| "events_subscription_unavailable: subscription is absent, expired, or belongs to another authenticated actor".into())
    }

    fn touch(&mut self, id: &str, now: Instant) {
        if let Some(subscription) = self.subscriptions.get_mut(id) {
            subscription.expires = now + RETENTION;
        }
        if let Some(receipt) = self
            .subscribed
            .iter_mut()
            .find(|receipt| receipt.subscription_id == id)
        {
            receipt.expires = now + RETENTION;
        }
    }

    fn descriptor(&self, id: &str, subscription: &Subscription) -> Value {
        json!({
            "subscription_id":id,
            "status":if subscription.closed {"closed"} else {"subscribed"},
            "filter":subscription.filter,
            "cursor":subscription.cursor,
            "next_cursor":subscription.delivered,
            "oldest_cursor":self.oldest_cursor,
            "latest_cursor":self.sequence,
            "retention_ms":RETENTION.as_millis() as u64,
        })
    }
}

impl Hub {
    fn locked(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn ensure_owner<'a>(state: &'a mut State, owner: &str) -> Result<&'a mut Journal, String> {
        if !state.owners.contains_key(owner) {
            if state.owners.len() >= MAX_OWNERS {
                return Err("events_capacity: account journal limit reached".into());
            }
            let journal = Journal::new()?;
            state.owners.insert(owner.to_owned(), journal);
        }
        Ok(state.owners.get_mut(owner).expect("account journal exists"))
    }

    fn existing<'a>(state: &'a mut State, owner: &str) -> Result<&'a mut Journal, String> {
        state
            .owners
            .get_mut(owner)
            .ok_or_else(|| "events_subscription_unavailable: account journal is absent".into())
    }

    pub(crate) fn publish_system(
        &self,
        owner: &str,
        client_id: &str,
        actor_id: Option<&str>,
        kind: &str,
        data: Value,
        audience: Option<Vec<String>>,
        dedupe_key: Option<&str>,
    ) -> Result<Value, String> {
        owner_identity(owner)?;
        identity(client_id, "client_id", 256, false)?;
        if let Some(actor_id) = actor_id {
            identity(actor_id, "actor_id", 256, false)?;
        }
        kind_name(kind, false)?;
        if kind.starts_with("custom.") {
            return Err(
                "events_kind: custom events require an authenticated agent publisher".into(),
            );
        }
        if let Some(key) = dedupe_key {
            identity(key, "dedupe_key", 128, true)?;
        }
        let audience = normalize_audience(audience)?;
        self.publish(
            owner,
            Publication {
                client_id: client_id.into(),
                actor_id: actor_id.map(str::to_owned),
                kind: kind.into(),
                origin: "system",
                data,
                audience,
            },
            dedupe_key.map(|key| PublishKey {
                client_id: client_id.into(),
                agent_id: None,
                request_id: key.into(),
            }),
        )
    }

    pub(crate) fn publish_custom(
        &self,
        actor: &Actor,
        request_id: &str,
        name: &str,
        data: Value,
    ) -> Result<Value, String> {
        actor.validate()?;
        identity(request_id, "request_id", 128, true)?;
        kind_name(name, false)?;
        if !name.starts_with("custom.") {
            return Err("events_kind: an agent may publish only custom.* events".into());
        }
        self.publish(
            &actor.owner,
            Publication {
                client_id: actor.client_id.clone(),
                actor_id: Some(actor.agent_id.clone()),
                kind: name.into(),
                origin: "agent",
                data,
                audience: None,
            },
            Some(PublishKey {
                client_id: actor.client_id.clone(),
                agent_id: Some(actor.agent_id.clone()),
                request_id: request_id.into(),
            }),
        )
    }

    fn publish(
        &self,
        owner: &str,
        publication: Publication,
        key: Option<PublishKey>,
    ) -> Result<Value, String> {
        let mut state = self.locked();
        let now = Instant::now();
        let journal = Self::ensure_owner(&mut state, owner)?;
        journal.prune(now);
        if let Some(key) = key.as_ref()
            && let Some(receipt) = journal
                .published
                .iter_mut()
                .find(|receipt| &receipt.key == key)
        {
            if receipt.publication != publication {
                return Err("events_request_conflict: publish request_id was already used with different content".into());
            }
            receipt.expires = now + RETENTION;
            let mut response = receipt.response.clone();
            response["duplicate"] = json!(true);
            return Ok(response);
        }
        let seq = journal
            .sequence
            .checked_add(1)
            .filter(|seq| *seq <= MAX_SEQUENCE)
            .ok_or("events_sequence_exhausted: account journal cannot accept another event")?;
        let event_id = format!("event_{}_{seq}", journal.generation);
        let value = json!({
            "id":event_id,"seq":seq,"client_id":publication.client_id,
            "kind":publication.kind,"origin":publication.origin,
            "actor_id":publication.actor_id,"time_ms":now_ms(),"data":publication.data,
        });
        let value_bytes = encoded_len(&value)?;
        if value_bytes > MAX_EVENT_BYTES {
            if publication.origin == "system" {
                journal.system_gap(seq);
                drop(state);
                self.changed.notify_waiters();
            }
            return Err("events_event_too_large: serialized event exceeds 4096 bytes".into());
        }
        let audience_bytes = encoded_len(&json!(publication.audience))?;
        let retained_bytes = value_bytes + audience_bytes;
        let response = json!({
            "status":"published","event_id":event_id,"seq":seq,"cursor":seq,
            "client_id":publication.client_id,"kind":publication.kind,
            "origin":publication.origin,"duplicate":false,
            "retention_ms":RETENTION.as_millis() as u64,
        });
        let receipt_bytes = value_bytes
            + audience_bytes
            + encoded_len(&response)?
            + key.as_ref().map_or(0, |key| {
                key.client_id.len()
                    + key.agent_id.as_ref().map_or(0, String::len)
                    + key.request_id.len()
            });
        if key.is_some()
            && (journal.published.len() >= MAX_PUBLISH_RECEIPTS
                || journal
                    .published
                    .iter()
                    .map(|receipt| receipt.bytes)
                    .sum::<usize>()
                    + receipt_bytes
                    > MAX_RECEIPT_BYTES)
        {
            if publication.origin == "system" {
                journal.system_gap(seq);
                drop(state);
                self.changed.notify_waiters();
            }
            return Err("events_dedupe_capacity: recent publish receipts are retained; retry after their retention window".into());
        }
        while journal.events.len() >= MAX_EVENTS
            || journal.event_bytes + retained_bytes > MAX_JOURNAL_BYTES
        {
            journal.evict_front();
        }
        journal.sequence = seq;
        journal.event_bytes += retained_bytes;
        journal.events.push_back(Event {
            seq,
            client_id: publication.client_id.clone(),
            actor_id: publication.actor_id.clone(),
            kind: publication.kind.clone(),
            audience: publication.audience.clone(),
            value,
            value_bytes,
            retained_bytes,
            at: now,
        });
        if let Some(key) = key {
            journal.published.push_back(PublishReceipt {
                key,
                publication,
                response: response.clone(),
                bytes: receipt_bytes,
                expires: now + RETENTION,
            });
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(response)
    }

    pub(crate) fn subscribe(
        &self,
        actor: &Actor,
        request_id: &str,
        filter: Filter,
        after: Option<u64>,
    ) -> Result<Value, String> {
        actor.validate()?;
        identity(request_id, "request_id", 128, true)?;
        let filter = filter.normalized(actor)?;
        let mut state = self.locked();
        let now = Instant::now();
        let journal = Self::ensure_owner(&mut state, &actor.owner)?;
        journal.prune(now);
        if let Some(index) = journal
            .subscribed
            .iter()
            .position(|receipt| &receipt.actor == actor && receipt.request_id == request_id)
        {
            let receipt = &journal.subscribed[index];
            if receipt.filter != filter || receipt.after != after {
                return Err("events_request_conflict: subscribe request_id was already used with a different filter or cursor".into());
            }
            let id = receipt.subscription_id.clone();
            let initial = receipt.initial_cursor;
            journal.subscribed[index].expires = now + RETENTION;
            journal.touch(&id, now);
            let mut response = match journal.subscriptions.get(&id) {
                Some(subscription) => journal.descriptor(&id, subscription),
                None => {
                    json!({"status":"unsubscribed","subscription_id":id,"filter":filter,"cursor":initial,"next_cursor":initial})
                }
            };
            response["duplicate"] = json!(true);
            return Ok(response);
        }
        let cursor = after.unwrap_or(journal.sequence);
        journal.check_cursor(cursor)?;
        let bytes = encoded_len(
            &json!({"actor":actor,"request_id":request_id,"filter":filter,"after":after}),
        )? + 256;
        if journal.subscriptions.len() >= MAX_SUBSCRIPTIONS
            || journal.subscribed.len() >= MAX_SUBSCRIBE_RECEIPTS
            || journal
                .subscribed
                .iter()
                .map(|receipt| receipt.bytes)
                .sum::<usize>()
                + bytes
                > MAX_SUBSCRIBE_BYTES
        {
            return Err(
                "events_subscription_capacity: subscription or retained request limit reached"
                    .into(),
            );
        }
        let id = format!("subscription_{}", nonce()?);
        let subscription = Subscription {
            actor: actor.clone(),
            filter: filter.clone(),
            cursor,
            delivered: cursor,
            expires: now + RETENTION,
            closed: false,
        };
        let mut response = journal.descriptor(&id, &subscription);
        response["duplicate"] = json!(false);
        journal.subscriptions.insert(id.clone(), subscription);
        journal.subscribed.push_back(SubscribeReceipt {
            actor: actor.clone(),
            request_id: request_id.into(),
            filter,
            after,
            subscription_id: id,
            initial_cursor: cursor,
            bytes,
            expires: now + RETENTION,
        });
        Ok(response)
    }

    pub(crate) fn read(
        &self,
        actor: &Actor,
        subscription_id: &str,
        limit: usize,
    ) -> Result<Value, String> {
        self.read_excluding(actor, subscription_id, limit, None)
    }

    fn read_excluding(
        &self,
        actor: &Actor,
        subscription_id: &str,
        limit: usize,
        exclude_origin_wait_id: Option<&str>,
    ) -> Result<Value, String> {
        actor.validate()?;
        valid_limit(limit)?;
        let mut state = self.locked();
        let now = Instant::now();
        let journal = Self::existing(&mut state, &actor.owner)?;
        journal.prune(now);
        let subscription = journal.subscription(actor, subscription_id)?;
        if subscription.closed {
            return Err("events_client_closed: a selected client closed; create a subscription for authorized live clients".into());
        }
        let cursor = subscription.cursor;
        journal.check_cursor(cursor)?;
        let filter = &subscription.filter;
        let recipient = format!("{}:{}", actor.client_id, actor.agent_id);
        let mut events = Vec::new();
        let mut next_cursor = cursor;
        let mut bytes = 0;
        let mut has_more = false;
        for event in journal.events.iter().filter(|event| event.seq > cursor) {
            if event.visible_to(&recipient)
                && filter.matches(event)
                && !event.excluded_wait(actor, filter.include_own_waits, exclude_origin_wait_id)
            {
                if events.len() >= limit || bytes + event.value_bytes + 1 > MAX_READ_BYTES - 2048 {
                    has_more = true;
                    break;
                }
                bytes += event.value_bytes + 1;
                events.push(event.value.clone());
            }
            next_cursor = event.seq;
        }
        let subscription = journal
            .subscriptions
            .get_mut(subscription_id)
            .expect("validated subscription");
        subscription.delivered = subscription.delivered.max(next_cursor);
        journal.touch(subscription_id, now);
        Ok(json!({
            "status":if events.is_empty() {"pending"} else {"ready"},
            "subscription_id":subscription_id,"events":events,"cursor":cursor,
            "next_cursor":next_cursor,"has_more":has_more,
            "oldest_cursor":journal.oldest_cursor,"latest_cursor":journal.sequence,
            "retention_ms":RETENTION.as_millis() as u64,
        }))
    }

    pub(crate) fn ack(
        &self,
        actor: &Actor,
        subscription_id: &str,
        through: u64,
    ) -> Result<Value, String> {
        actor.validate()?;
        let mut state = self.locked();
        let now = Instant::now();
        let journal = Self::existing(&mut state, &actor.owner)?;
        journal.prune(now);
        let subscription = journal.subscription(actor, subscription_id)?;
        if subscription.closed {
            return Err("events_client_closed: subscription contains a closed client".into());
        }
        if through > subscription.delivered {
            return Err("events_ack_ahead: cursor has not been delivered to this authenticated subscription".into());
        }
        let duplicate = through <= subscription.cursor;
        let subscription = journal
            .subscriptions
            .get_mut(subscription_id)
            .expect("validated subscription");
        subscription.cursor = subscription.cursor.max(through);
        let cursor = subscription.cursor;
        journal.touch(subscription_id, now);
        drop(state);
        self.changed.notify_waiters();
        Ok(
            json!({"status":"acknowledged","subscription_id":subscription_id,"cursor":cursor,"duplicate":duplicate}),
        )
    }

    pub(crate) fn unsubscribe(
        &self,
        actor: &Actor,
        subscription_id: &str,
    ) -> Result<Value, String> {
        actor.validate()?;
        let mut state = self.locked();
        let now = Instant::now();
        let journal = Self::existing(&mut state, &actor.owner)?;
        journal.prune(now);
        let index = journal.subscribed.iter().position(|receipt| &receipt.actor == actor && receipt.subscription_id == subscription_id)
            .ok_or("events_subscription_unavailable: subscription is absent, expired, or belongs to another authenticated actor")?;
        journal.subscribed[index].expires = now + RETENTION;
        let duplicate = journal.subscriptions.remove(subscription_id).is_none();
        drop(state);
        self.changed.notify_waiters();
        Ok(json!({"status":"unsubscribed","subscription_id":subscription_id,"duplicate":duplicate}))
    }

    pub(crate) fn subscriptions(&self, actor: &Actor) -> Result<Value, String> {
        actor.validate()?;
        let mut state = self.locked();
        let Some(journal) = state.owners.get_mut(&actor.owner) else {
            return Ok(json!({"subscriptions":[]}));
        };
        journal.prune(Instant::now());
        let subscriptions: Vec<_> = journal
            .subscriptions
            .iter()
            .filter(|(_, subscription)| &subscription.actor == actor)
            .map(|(id, subscription)| journal.descriptor(id, subscription))
            .collect();
        Ok(json!({"subscriptions":subscriptions}))
    }

    pub(crate) fn subscription_clients(
        &self,
        actor: &Actor,
        subscription_id: &str,
    ) -> Result<Vec<String>, String> {
        actor.validate()?;
        let mut state = self.locked();
        let journal = Self::existing(&mut state, &actor.owner)?;
        journal.prune(Instant::now());
        Ok(journal
            .subscription(actor, subscription_id)?
            .filter
            .client_ids
            .clone())
    }

    pub(crate) async fn wait(
        &self,
        actor: &Actor,
        subscription_id: &str,
        limit: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        self.wait_excluding(actor, subscription_id, limit, wait_ms, None)
            .await
    }

    pub(crate) async fn wait_excluding(
        &self,
        actor: &Actor,
        subscription_id: &str,
        limit: usize,
        wait_ms: u64,
        exclude_origin_wait_id: Option<&str>,
    ) -> Result<Value, String> {
        actor.validate()?;
        valid_limit(limit)?;
        if wait_ms > MAX_WAIT_MS {
            return Err("events_wait_limit: wait_ms must be at most 55000".into());
        }
        let _guard = self.begin_wait(actor, subscription_id)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            // Register before reading the retained state. Publication in either
            // half of this interval is observed by the read or by the notifier.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let page =
                self.read_excluding(actor, subscription_id, limit, exclude_origin_wait_id)?;
            if page["status"] == "ready" || wait_ms == 0 || tokio::time::Instant::now() >= deadline
            {
                return Ok(page);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.read_excluding(actor, subscription_id, limit, exclude_origin_wait_id);
            }
        }
    }

    fn begin_wait(&self, actor: &Actor, subscription_id: &str) -> Result<WaitGuard<'_>, String> {
        let mut state = self.locked();
        let journal = Self::existing(&mut state, &actor.owner)?;
        journal.prune(Instant::now());
        journal.subscription(actor, subscription_id)?;
        if state.waiters >= MAX_WAITERS {
            return Err("events_wait_capacity: too many active observation calls".into());
        }
        state.waiters += 1;
        Ok(WaitGuard { hub: self })
    }

    pub(crate) fn close_client(&self, client_id: &str) {
        let mut state = self.locked();
        for journal in state.owners.values_mut() {
            journal.prune(Instant::now());
            for subscription in journal.subscriptions.values_mut() {
                if subscription.actor.client_id == client_id
                    || subscription
                        .filter
                        .client_ids
                        .iter()
                        .any(|selected| selected == client_id)
                {
                    subscription.closed = true;
                }
            }
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

struct WaitGuard<'a> {
    hub: &'a Hub,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        self.hub.locked().waiters -= 1;
    }
}

impl Actor {
    fn validate(&self) -> Result<(), String> {
        owner_identity(&self.owner)?;
        identity(&self.client_id, "client_id", 256, false)?;
        identity(&self.agent_id, "agent_id", 256, false)
    }
}

impl Filter {
    fn normalized(mut self, actor: &Actor) -> Result<Self, String> {
        if self.kinds.len() > 32 || self.client_ids.len() > 32 {
            return Err(
                "events_filter_limit: at most 32 kinds and 32 clients may be selected".into(),
            );
        }
        for kind in &self.kinds {
            kind_name(kind, true)?;
        }
        if self.client_ids.is_empty() {
            self.client_ids.push(actor.client_id.clone());
        }
        for client_id in &self.client_ids {
            identity(client_id, "filter.client_id", 256, false)?;
        }
        if let Some(actor_id) = self.actor_id.as_deref() {
            identity(actor_id, "filter.actor_id", 256, false)?;
        }
        if let Some(task_id) = self.task_id.as_deref() {
            identity(task_id, "filter.task_id", 256, true)?;
        }
        self.kinds.sort();
        self.kinds.dedup();
        self.client_ids.sort();
        self.client_ids.dedup();
        Ok(self)
    }

    fn matches(&self, event: &Event) -> bool {
        self.client_ids.contains(&event.client_id)
            && (self.kinds.is_empty()
                || self.kinds.iter().any(|kind| {
                    kind == "*"
                        || kind == &event.kind
                        || kind.strip_suffix(".*").is_some_and(|prefix| {
                            event.kind.starts_with(prefix)
                                && event.kind.as_bytes().get(prefix.len()) == Some(&b'.')
                        })
                }))
            && self
                .actor_id
                .as_ref()
                .is_none_or(|actor| event.actor_id.as_ref() == Some(actor))
            && self
                .task_id
                .as_ref()
                .is_none_or(|task| event.value["data"]["task_id"].as_str() == Some(task.as_str()))
    }
}

impl Event {
    fn visible_to(&self, recipient: &str) -> bool {
        self.audience
            .as_ref()
            .is_none_or(|audience| audience.iter().any(|allowed| allowed == recipient))
    }

    fn excluded_wait(
        &self,
        actor: &Actor,
        include_own_waits: bool,
        exclude_origin_wait_id: Option<&str>,
    ) -> bool {
        self.kind.starts_with("wait.")
            && ((!include_own_waits
                && self.client_id == actor.client_id
                && self.actor_id.as_ref() == Some(&actor.agent_id))
                || exclude_origin_wait_id
                    .is_some_and(|id| self.value["data"]["origin_wait_id"].as_str() == Some(id)))
    }
}

fn identity(value: &str, field: &str, maximum: usize, colon: bool) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > maximum
        || value.chars().any(char::is_control)
        || (!colon && value.contains(':'))
    {
        return Err(format!(
            "events_invalid_identity: {field} must contain 1..{maximum} bytes without control characters{}",
            if colon { "" } else { " or colons" }
        ));
    }
    Ok(())
}

fn owner_identity(owner: &str) -> Result<(), String> {
    // bridge::owner is an authenticated opaque key with NUL-delimited fields.
    // It is compared exactly and never rendered as an event identifier.
    if owner.is_empty() || owner.len() > 512 {
        return Err("events_invalid_identity: owner must contain 1..512 opaque bytes".into());
    }
    Ok(())
}

fn kind_name(kind: &str, pattern: bool) -> Result<(), String> {
    if pattern && kind == "*" {
        return Ok(());
    }
    let plain = if pattern {
        kind.strip_suffix(".*").unwrap_or(kind)
    } else {
        kind
    };
    if kind.len() > 128
        || plain.is_empty()
        || plain.split('.').any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err("events_kind: use nonempty dot-separated names; filters may end in .*".into());
    }
    Ok(())
}

fn normalize_audience(audience: Option<Vec<String>>) -> Result<Option<Vec<String>>, String> {
    let Some(mut audience) = audience else {
        return Ok(None);
    };
    if audience.is_empty() || audience.len() > 64 {
        return Err("events_audience_limit: a private event requires 1..64 exact client_id:agent_id recipients".into());
    }
    for recipient in &audience {
        let (client, agent) = recipient
            .split_once(':')
            .ok_or("events_audience: use exact client_id:agent_id recipients")?;
        identity(client, "audience.client_id", 256, false)?;
        identity(agent, "audience.agent_id", 256, false)?;
    }
    audience.sort();
    audience.dedup();
    Ok(Some(audience))
}

fn valid_limit(limit: usize) -> Result<(), String> {
    if limit == 0 || limit > MAX_READ_EVENTS {
        return Err("events_read_limit: limit must be from 1 through 64".into());
    }
    Ok(())
}

fn encoded_len(value: &Value) -> Result<usize, String> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|error| error.to_string())
}

fn nonce() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| format!("events_randomness: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
#[path = "events/tests.rs"]
mod tests;
