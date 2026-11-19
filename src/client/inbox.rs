//! Local operator facade for peer candidates; never publishes user steering.
pub use crate::messaging::queue::{Candidate, Snapshot};
use crate::messaging::{MAX_BATCH, Target, queue::Inbox};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, Weak},
};

#[derive(Default)]
struct Binding {
    generation: u64,
    inbox: Weak<Inbox>,
}

fn binding() -> &'static Mutex<Binding> {
    static CURRENT: OnceLock<Mutex<Binding>> = OnceLock::new();
    CURRENT.get_or_init(|| Mutex::new(Binding::default()))
}

pub(crate) fn bind(inbox: &Arc<Inbox>) {
    let mut current = binding().lock().unwrap_or_else(|e| e.into_inner());
    current.generation = current.generation.wrapping_add(1);
    current.inbox = Arc::downgrade(inbox);
}

fn current() -> Result<Arc<Inbox>, String> {
    binding()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .inbox
        .upgrade()
        .ok_or_else(|| "No live peer candidate inbox".into())
}

/// Cheap invalidation token. Does not clone candidates or mark anything read.
pub fn version() -> Option<(u64, u64)> {
    let current = binding().lock().unwrap_or_else(|e| e.into_inner());
    Some((current.generation, current.inbox.upgrade()?.revision()))
}

/// A local operator may inspect all mailbox targets on this physical connection.
/// Remote participant authorization is separately enforced by the relay.
pub fn snapshot() -> Result<Snapshot, String> {
    Ok(current()?.snapshot())
}

pub fn read(delivery_id: &str) -> Result<Candidate, String> {
    let snapshot = snapshot()?;
    if snapshot.closed {
        return Err("Peer candidate session is closed".into());
    }
    snapshot
        .candidates
        .into_iter()
        .find(|candidate| candidate.message.delivery_id == delivery_id)
        .ok_or_else(|| "Peer candidate expired or was already handled".into())
}

/// Explicit operator action only. Never claims human understanding or starts a tool/model.
pub fn ack(ids: &[String]) -> Result<(), String> {
    if ids.is_empty() || ids.len() > MAX_BATCH {
        return Err("Select 1..=128 peer candidates".into());
    }
    let inbox = current()?;
    let snapshot = inbox.snapshot();
    if snapshot.closed {
        return Err("Peer candidate session is closed".into());
    }
    let mut groups: BTreeMap<Target, Vec<String>> = BTreeMap::new();
    for id in ids {
        let candidate = snapshot
            .candidates
            .iter()
            .find(|candidate| &candidate.message.delivery_id == id)
            .ok_or_else(|| {
                "A peer candidate expired or was already handled; refresh the inbox".to_string()
            })?;
        groups
            .entry(candidate.message.target.clone())
            .or_default()
            .push(id.clone());
    }
    for (target, ids) in groups {
        inbox.ack(&target, &ids, None)?;
    }
    Ok(())
}
