//! A bounded native candidate inbox. Reads and cancelled waits never consume it.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::{
    sync::{Notify, Semaphore},
    time::Instant,
};

const MAX_SEQUENCE: u64 = (1 << 53) - 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub sequence: u64,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
    pub message: PeerMessage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub session_id: String,
    pub revision: u64,
    pub closed: bool,
    pub retained_bytes: usize,
    pub candidates: Vec<Candidate>,
}

struct Record {
    candidate: Candidate,
    bytes: usize,
    expires: Instant,
}

struct Receipt {
    id: String,
    target: Target,
    fingerprint: [u8; 32],
    sequence: u64,
    state: &'static str,
    expires: Instant,
}

#[derive(Default)]
struct State {
    closed: bool,
    revision: u64,
    sequence: u64,
    retired_through: u64,
    bytes: usize,
    candidates: VecDeque<Record>,
    receipts: VecDeque<Receipt>,
    expiry_running: bool,
}

impl State {
    fn purge(&mut self) -> bool {
        let now = Instant::now();
        let mut removed = Vec::new();
        self.candidates.retain(|record| {
            if record.expires > now {
                return true;
            }
            self.bytes -= record.bytes;
            self.retired_through = self.retired_through.max(record.candidate.sequence);
            removed.push(record.candidate.message.delivery_id.clone());
            false
        });
        for receipt in &mut self.receipts {
            if removed.contains(&receipt.id) {
                receipt.state = "expired";
            }
        }
        // An unacknowledged candidate always retains its deduplication proof.
        self.receipts
            .retain(|receipt| receipt.state == "candidate" || receipt.expires > now);
        if !removed.is_empty() {
            self.revision += 1;
        }
        !removed.is_empty()
    }
}

pub struct Inbox {
    process: ProcessIdentity,
    session_id: String,
    state: Mutex<State>,
    changed: Arc<Notify>,
    waiters: Semaphore,
}

impl Inbox {
    pub fn new() -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            process: process_identity()?,
            session_id: random_id("session")?,
            state: Mutex::new(State::default()),
            changed: Arc::new(Notify::new()),
            waiters: Semaphore::new(4),
        }))
    }

    pub fn capability(&self) -> Value {
        json!({"version":VERSION,"process_id":self.process.process_id,"pid":self.process.pid,
            "session_id":self.session_id,"state":if self.state.lock().unwrap_or_else(|e| e.into_inner()).closed {"closed"} else {"live"},
            "thread_kind":"explicit registered collaboration participant, not an OS or model thread",
            "limits":limits()})
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Cheap UI invalidation: payload snapshots need only be cloned on changes.
    pub fn revision(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revision
    }

    fn target(&self, target: &Target) -> Result<(), String> {
        target.validate()?;
        if target.session_id != self.session_id {
            return Err("peer session generation changed; refresh messages.list".into());
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.purge() {
            self.changed.notify_waiters();
        }
        Snapshot {
            session_id: self.session_id.clone(),
            revision: state.revision,
            closed: state.closed,
            retained_bytes: state.bytes,
            candidates: state
                .candidates
                .iter()
                .map(|r| r.candidate.clone())
                .collect(),
        }
    }

    pub fn deliver(self: &Arc<Self>, message: PeerMessage) -> Result<Value, String> {
        message.validate()?;
        self.target(&message.target)?;
        let encoded = serde_json::to_vec(&message).map_err(|e| e.to_string())?;
        let fingerprint: [u8; 32] = Sha256::digest(&encoded).into();
        let bytes = encoded.len() + 64;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.purge();
        if state.closed {
            return Err("peer session is closed".into());
        }
        if let Some(receipt) = state.receipts.iter().find(|r| r.id == message.delivery_id) {
            if receipt.fingerprint != fingerprint {
                return Err(
                    "delivery ID already identifies different content or recipients".into(),
                );
            }
            return Ok(
                json!({"delivery_id":receipt.id,"state":receipt.state,"sequence":receipt.sequence,"duplicate":true}),
            );
        }
        if state.candidates.len() >= MAX_MESSAGES
            || state.bytes.saturating_add(bytes) > MAX_INBOX_BYTES
        {
            return Err(
                "peer candidate inbox is full; acknowledge candidates before sending more".into(),
            );
        }
        if state.sequence >= MAX_SEQUENCE {
            return Err("peer inbox cursor exhausted".into());
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "peer inbox needs its native async runtime")?;
        state.sequence += 1;
        state.revision += 1;
        let sequence = state.sequence;
        let received_at_ms = now_ms();
        let expires_at_ms = received_at_ms.saturating_add(message.ttl_ms);
        let receipt = Receipt {
            id: message.delivery_id.clone(),
            target: message.target.clone(),
            fingerprint,
            sequence,
            state: "candidate",
            expires: Instant::now() + Duration::from_millis(MAX_TTL_MS),
        };
        let result = json!({"delivery_id":receipt.id,"state":"candidate","sequence":sequence,
            "duplicate":false,"accepted":true,"note":"Accepted into the candidate inbox; not read or executed"});
        let expires = Instant::now() + Duration::from_millis(message.ttl_ms);
        state.candidates.push_back(Record {
            candidate: Candidate {
                sequence,
                received_at_ms,
                expires_at_ms,
                message,
            },
            bytes,
            expires,
        });
        state.bytes += bytes;
        while state.receipts.len() >= MAX_RECEIPTS {
            let retired = state
                .receipts
                .iter()
                .position(|r| r.state != "candidate")
                .expect("candidate count was bounded before receipt insertion");
            state.receipts.remove(retired);
        }
        state.receipts.push_back(receipt);
        if !state.expiry_running {
            state.expiry_running = true;
            handle.spawn(expire(Arc::downgrade(self), self.changed.clone()));
        }
        drop(state);
        self.changed.notify_waiters();
        Ok(result)
    }

    pub fn read(&self, target: &Target, after: u64, limit: usize) -> Result<Value, String> {
        self.target(target)?;
        if !(1..=MAX_READ_MESSAGES).contains(&limit) {
            return Err("peer read limit is out of range".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.purge() {
            self.changed.notify_waiters();
        }
        if state.closed {
            return Err("peer session is closed".into());
        }
        if after > state.sequence {
            return Err("peer cursor is ahead of this session".into());
        }
        let mut messages = Vec::new();
        let mut bytes = 0usize;
        let mut next = after;
        let mut has_more = false;
        for record in &state.candidates {
            if record.candidate.sequence <= after || record.candidate.message.target != *target {
                continue;
            }
            if messages.len() >= limit || bytes.saturating_add(record.bytes) > MAX_READ_BYTES {
                has_more = true;
                break;
            }
            bytes += record.bytes;
            next = record.candidate.sequence;
            messages.push(&record.candidate);
        }
        Ok(
            json!({"session_id":self.session_id,"messages":messages,"next_cursor":next,
            "latest_cursor":state.sequence,"revision":state.revision,"has_more":has_more,
            "truncated":after<state.retired_through,"timed_out":false,"consumed":false}),
        )
    }

    pub async fn wait(
        &self,
        target: &Target,
        after: u64,
        limit: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        if wait_ms > MAX_WAIT_MS {
            return Err("peer wait exceeds 30 seconds".into());
        }
        let _permit = self
            .waiters
            .try_acquire()
            .map_err(|_| "peer wait limit reached")?;
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let mut result = self.read(target, after, limit)?;
            if !result["messages"].as_array().unwrap().is_empty() {
                return Ok(result);
            }
            if Instant::now() >= deadline {
                result["timed_out"] = json!(true);
                return Ok(result);
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                let mut result = self.read(target, after, limit)?;
                result["timed_out"] = json!(result["messages"].as_array().unwrap().is_empty());
                return Ok(result);
            }
        }
    }

    pub fn ack(
        &self,
        target: &Target,
        ids: &[String],
        through: Option<u64>,
    ) -> Result<Value, String> {
        NativeRequest::Ack {
            target: target.clone(),
            ids: ids.to_vec(),
            through,
        }
        .validate()?;
        self.target(target)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.purge();
        if state.closed {
            return Err("peer session is closed".into());
        }
        if through.is_some_and(|cursor| cursor > state.sequence) {
            return Err("ack cursor is ahead of this session".into());
        }
        let ids: BTreeSet<_> = ids.iter().cloned().collect();
        for id in &ids {
            if !state
                .receipts
                .iter()
                .any(|r| r.id == *id && r.target == *target)
            {
                return Err(
                    "delivery is unavailable or belongs to a different peer mailbox".into(),
                );
            }
        }
        let mut removed = Vec::new();
        let mut released = 0;
        let mut retired = state.retired_through;
        state.candidates.retain(|record| {
            let candidate = &record.candidate;
            if candidate.message.target != *target
                || !(ids.contains(&candidate.message.delivery_id)
                    || through.is_some_and(|cursor| candidate.sequence <= cursor))
            {
                return true;
            }
            released += record.bytes;
            retired = retired.max(candidate.sequence);
            removed.push(candidate.message.delivery_id.clone());
            false
        });
        for receipt in &mut state.receipts {
            if removed.contains(&receipt.id) {
                receipt.state = "acknowledged";
            }
        }
        state.bytes -= released;
        state.retired_through = retired;
        if !removed.is_empty() {
            state.revision += 1;
        }
        let revision = state.revision;
        drop(state);
        self.changed.notify_waiters();
        Ok(json!({"acknowledged":removed,"revision":revision,
            "note":"Explicitly handled; not proof that a human read or understood the message"}))
    }

    pub fn status(&self, target: &Target, ids: &[String]) -> Result<Value, String> {
        NativeRequest::Status {
            target: target.clone(),
            ids: ids.to_vec(),
        }
        .validate()?;
        self.target(target)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.purge();
        if state.closed {
            return Err("peer session is closed".into());
        }
        let statuses:Vec<_>=ids.iter().map(|id| {
            let receipt=state.receipts.iter().find(|r|r.id==*id && r.target==*target);
            json!({"delivery_id":id,"state":receipt.map(|r|r.state).unwrap_or("unknown"),"sequence":receipt.map(|r|r.sequence)})
        }).collect();
        Ok(json!({"deliveries":statuses}))
    }

    pub fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.candidates.clear();
        state.receipts.clear();
        state.bytes = 0;
        state.revision += 1;
        drop(state);
        self.changed.notify_waiters();
    }

    pub async fn perform(self: &Arc<Self>, request: NativeRequest) -> Result<Value, String> {
        request.validate()?;
        match request {
            NativeRequest::Deliver { message } => self.deliver(*message),
            NativeRequest::Read {
                target,
                after,
                limit,
            } => self.read(&target, after, limit),
            NativeRequest::Wait {
                target,
                after,
                limit,
                wait_ms,
            } => self.wait(&target, after, limit, wait_ms).await,
            NativeRequest::Ack {
                target,
                ids,
                through,
            } => self.ack(&target, &ids, through),
            NativeRequest::Status { target, ids } => self.status(&target, &ids),
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.changed.notify_waiters();
    }
}

async fn expire(inbox: Weak<Inbox>, changed: Arc<Notify>) {
    loop {
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let deadline = {
            let Some(inbox) = inbox.upgrade() else { return };
            let mut state = inbox.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.purge() {
                inbox.changed.notify_waiters();
            }
            if state.closed || state.candidates.is_empty() {
                state.expiry_running = false;
                return;
            }
            state.candidates.iter().map(|r| r.expires).min().unwrap()
        };
        tokio::select! { _=notified=>{}, _=tokio::time::sleep_until(deadline)=>{} }
    }
}
