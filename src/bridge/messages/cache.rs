//! Bounded idempotency receipts. A retained key is never silently evicted.
use crate::messaging::{Source, Target, now_ms, random_id, valid_id};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

pub(super) const KEY_WINDOW_MS: u64 = 5 * 60 * 1000;
const MAX_BATCHES: usize = 256;
const MAX_OWNER_BATCHES: usize = 64;
const MAX_ACTIVE_BATCHES: usize = 8;
const MAX_RECEIPT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
struct Budget(AtomicUsize);
impl Budget {
    fn charge(&self, bytes: usize) -> Result<(), String> {
        let mut used = self.0.load(Ordering::Relaxed);
        loop {
            let next = used
                .checked_add(bytes)
                .filter(|n| *n <= MAX_RECEIPT_BYTES)
                .ok_or("Message receipt byte budget reached; active keys are never evicted")?;
            match self
                .0
                .compare_exchange_weak(used, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(actual) => used = actual,
            }
        }
    }
}

#[derive(Clone, Serialize)]
pub(super) struct Delivery {
    pub client_id: String,
    pub target: Option<Target>,
    pub delivery_id: Option<String>,
    pub state: String,
    pub detail: Option<String>,
    pub receipt: Option<Value>,
}

impl Delivery {
    pub fn pending(client_id: String, target: Option<Target>) -> Self {
        Self {
            client_id,
            target,
            delivery_id: None,
            state: "pending".into(),
            detail: None,
            receipt: None,
        }
    }
    pub fn fail(&mut self, state: &str, detail: impl Into<String>) {
        self.state = state.into();
        let mut detail = detail.into();
        if detail.len() > 512 {
            let mut end = 512;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        self.detail = Some(detail);
    }
}

pub(super) struct BatchState {
    pub active: bool,
    pub resolved: bool,
    pub recipients: Vec<Delivery>,
    pub error: Option<String>,
}

pub(super) struct Batch {
    pub owner: String,
    pub key: String,
    pub message_id: String,
    pub source: Source,
    pub sent_at_ms: u64,
    pub expires_at_ms: u64,
    fingerprint: [u8; 32],
    budget: Arc<Budget>,
    reserved: AtomicUsize,
    pub state: Mutex<BatchState>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.budget
            .0
            .fetch_sub(self.reserved.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

impl Batch {
    pub fn reserve_recipients(&self, count: usize) -> Result<(), String> {
        // Each retained delivery has bounded IDs, <=512 error bytes and a fixed small ACK.
        let bytes = count
            .checked_mul(1536)
            .ok_or("Message receipt budget overflow")?;
        self.budget.charge(bytes)?;
        self.reserved.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }
    pub fn snapshot(&self, duplicate: bool) -> Value {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        json!({"message_id":self.message_id,"idempotency_key":self.key,
            "source":self.source,"sent_at_ms":self.sent_at_ms,"receipt_expires_at_ms":self.expires_at_ms,
            "duplicate":duplicate,"in_progress":state.active,"recipients_resolved":state.resolved,
            "recipients":state.recipients,"error":state.error,
            "replay":false,"note":"Delivery ACK means candidate storage, not reading or execution. Unknown outcomes are never resent."})
    }
}

pub(super) struct Guard(pub Arc<Batch>);
impl Guard {
    pub fn finish(&self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .active = false;
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.active {
            return;
        }
        state.active = false;
        state.error = Some(
            "Sending was interrupted; inspect delivery status, do not replay the batch".into(),
        );
        for recipient in &mut state.recipients {
            if matches!(recipient.state.as_str(), "pending" | "dispatching") {
                recipient.fail(
                    "unknown",
                    "Request ended before a delivery acknowledgement was available",
                );
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Router {
    epoch: OnceLock<Result<String, String>>,
    clock: AtomicU64,
    budget: Arc<Budget>,
    batches: Mutex<BTreeMap<(String, String), Arc<Batch>>>,
}

impl Router {
    fn epoch(&self) -> Result<&str, String> {
        self.epoch
            .get_or_init(|| random_id("relay"))
            .as_ref()
            .map(String::as_str)
            .map_err(Clone::clone)
    }
    fn now(&self) -> u64 {
        // A wall-clock correction cannot make an expired, forgotten key valid again.
        let now = now_ms();
        self.clock.fetch_max(now, Ordering::Relaxed).max(now)
    }
    pub fn key(&self) -> Result<Value, String> {
        let now = self.now();
        let key = format!("{}.{now}.{}", self.epoch()?, random_id("key")?);
        Ok(
            json!({"idempotency_key":key,"expires_at_ms":now+KEY_WINDOW_MS,"valid_for_ms":KEY_WINDOW_MS}),
        )
    }
    fn expiry(&self, key: &str) -> Result<u64, String> {
        if key.len() > 160 {
            return Err("Message key exceeds its byte bound".into());
        }
        let mut parts = key.split('.');
        let epoch = parts.next().unwrap_or_default();
        let stamp = parts
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or("Use an idempotency key returned by messages.key")?;
        let nonce = parts.next().unwrap_or_default();
        if parts.next().is_some() || epoch != self.epoch()? || !valid_id(nonce, "key", 16) {
            return Err("Message key belongs to another relay generation or is invalid; obtain a new key for an intentional new send".into());
        }
        let now = self.now();
        let expires = stamp
            .checked_add(KEY_WINDOW_MS)
            .ok_or("Invalid message key timestamp")?;
        if stamp > now || expires <= now {
            return Err(
                "Message key expired or is from the future; old batches are never replayed".into(),
            );
        }
        Ok(expires)
    }
    pub(super) fn reserve(
        &self,
        owner: String,
        key: String,
        fingerprint: [u8; 32],
        source: Source,
    ) -> Result<(Arc<Batch>, bool), String> {
        let expires = self.expiry(&key)?;
        let now = self.now();
        let mut batches = self.batches.lock().unwrap_or_else(|e| e.into_inner());
        batches.retain(|_, batch| {
            batch.expires_at_ms > now
                || batch.state.lock().unwrap_or_else(|e| e.into_inner()).active
        });
        if let Some(batch) = batches.get(&(owner.clone(), key.clone())) {
            if batch.fingerprint != fingerprint {
                return Err("Message key already identifies different content, source participant, or recipient selector".into());
            }
            return Ok((batch.clone(), true));
        }
        if batches.len() >= MAX_BATCHES
            || batches.values().filter(|b| b.owner == owner).count() >= MAX_OWNER_BATCHES
        {
            return Err(
                "Message receipt capacity reached; valid keys are retained until expiry".into(),
            );
        }
        if batches
            .values()
            .filter(|b| b.state.lock().unwrap_or_else(|e| e.into_inner()).active)
            .count()
            >= MAX_ACTIVE_BATCHES
        {
            return Err("Too many active message batches".into());
        }
        let message_id = random_id("peer")?;
        self.budget.charge(2048)?;
        let batch = Arc::new(Batch {
            budget: self.budget.clone(),
            reserved: AtomicUsize::new(2048),
            owner: owner.clone(),
            key: key.clone(),
            message_id,
            source,
            sent_at_ms: now,
            expires_at_ms: expires,
            fingerprint,
            state: Mutex::new(BatchState {
                active: true,
                resolved: false,
                recipients: Vec::new(),
                error: None,
            }),
        });
        batches.insert((owner, key), batch.clone());
        Ok((batch, false))
    }
    pub(super) fn get(&self, owner: &str, message_id: &str) -> Result<Arc<Batch>, String> {
        if !valid_id(message_id, "peer", 16) {
            return Err("Invalid peer message ID".into());
        }
        let now = self.now();
        self.batches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|batch| {
                batch.owner == owner && batch.message_id == message_id && batch.expires_at_ms > now
            })
            .cloned()
            .ok_or_else(|| {
                "Message receipt is unknown or expired; this does not authorize replay".into()
            })
    }
    pub fn limits() -> Value {
        json!({"idempotency_window_ms":KEY_WINDOW_MS,"max_retained_batches":MAX_BATCHES,
            "max_batches_per_principal":MAX_OWNER_BATCHES,"max_reserved_receipt_bytes":MAX_RECEIPT_BYTES,"max_active_batches":MAX_ACTIVE_BATCHES,
            "automatic_retry":false,"expires_across_relay_restart":true})
    }
}
