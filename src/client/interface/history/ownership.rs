//! Process-local capabilities for writing to an explicitly captured history.
use super::{Committed, HISTORY, History, HistoryStore, random_id, with};
use crate::runtime::call_scope::CallScope;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};

const MAX_BINDINGS: usize = 1024;
const RELEASED: &str = "history_owner_released: the original conversation is no longer writable";

/// An identifier used by display projection. It conveys no writer lease or relay identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryId(Arc<str>);

impl HistoryId {
    pub(crate) fn parse(id: &str) -> Option<Self> {
        super::valid_id(id, "conversation").then(|| Self(Arc::from(id)))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A reserved or existing local conversation. Copies cannot change its destination.
#[derive(Clone, Debug)]
pub(crate) struct HistoryOwner {
    id: Arc<str>,
    #[cfg(test)]
    fixture: Option<Weak<HistoryStore>>,
}

impl HistoryOwner {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn display_id(&self) -> HistoryId {
        HistoryId(self.id.clone())
    }
}

impl PartialEq for HistoryOwner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.id, &other.id)
    }
}

impl Eq for HistoryOwner {}

/// Share the selected identity without sharing the history storage lock.
/// Writers hold the history transaction lock; this short lock never covers
/// disk I/O or calls back into history storage or the terminal.
#[derive(Clone, Default)]
pub(super) struct CurrentOwner(Arc<Mutex<Option<HistoryOwner>>>);

impl CurrentOwner {
    pub(super) fn get(&self) -> Option<HistoryOwner> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn replace(&self, owner: Option<HistoryOwner>) {
        *self.0.lock().unwrap_or_else(|error| error.into_inner()) = owner;
    }
}

#[derive(Debug, Eq, Hash, PartialEq)]
struct VerifiedThread {
    provider: String,
    subject: String,
    client: String,
    thread: String,
}

impl VerifiedThread {
    fn from_call(call: &CallScope) -> Option<Self> {
        let account = &call.attribution()?.account;
        let thread = call.thread()?;
        Some(Self {
            provider: account.provider.clone(),
            subject: account.subject.clone(),
            client: thread.session_id().to_owned(),
            thread: thread.thread_id().to_owned(),
        })
    }
}

struct IssuedOwner {
    id: Weak<str>,
    pending: bool,
}

#[derive(Default)]
pub(super) struct Ownership {
    pub(super) current: CurrentOwner,
    issued: HashMap<String, IssuedOwner>,
    threads: HashMap<VerifiedThread, HistoryOwner>,
}

impl Ownership {
    pub(super) fn select(&mut self, id: &str, pending: bool) -> HistoryOwner {
        self.issued.retain(|_, entry| entry.id.strong_count() != 0);
        let entry = self.issued.get_mut(id);
        let identity = if let Some((entry, token)) =
            entry.and_then(|entry| entry.id.upgrade().map(|token| (entry, token)))
        {
            entry.pending = pending;
            token
        } else {
            let token: Arc<str> = Arc::from(id);
            self.issued.insert(
                id.to_owned(),
                IssuedOwner {
                    id: Arc::downgrade(&token),
                    pending,
                },
            );
            token
        };
        let owner = HistoryOwner {
            id: identity,
            #[cfg(test)]
            fixture: TEST_HISTORY.try_with(Arc::downgrade).ok(),
        };
        self.current.replace(Some(owner.clone()));
        owner
    }

    pub(super) fn pending(&self, owner: &HistoryOwner) -> Result<bool, String> {
        self.issued
            .get(owner.id())
            .filter(|entry| std::ptr::addr_eq(entry.id.as_ptr(), Arc::as_ptr(&owner.id)))
            .map(|entry| entry.pending)
            .ok_or_else(|| RELEASED.to_owned())
    }

    pub(super) fn materialized(&mut self, owner: &HistoryOwner) {
        if let Some(entry) = self.issued.get_mut(owner.id()) {
            entry.pending = false;
        }
    }

    pub(super) fn revoke(&mut self, id: &str) {
        self.issued.remove(id);
        if self
            .current
            .get()
            .as_ref()
            .is_some_and(|owner| owner.id() == id)
        {
            self.current.replace(None);
        }
    }

    pub(super) fn release(&mut self) {
        self.issued.clear();
        self.current.replace(None);
        // Keep the bound identities as tombstones. A retry must never bind to a
        // later conversation, even after release or when the table is full.
    }
}

impl History {
    pub(super) fn capture_owner(&mut self) -> Result<HistoryOwner, String> {
        if let Some(owner) = self.ownership.current.get() {
            return Ok(owner);
        }
        if let Some(active) = &self.active {
            return Ok(self.ownership.select(&active.metadata.id, false));
        }
        Ok(self.ownership.select(&random_id("conversation")?, true))
    }

    fn bind_owner(&mut self, identity: VerifiedThread) -> Result<HistoryOwner, String> {
        if let Some(owner) = self.ownership.threads.get(&identity) {
            return Ok(owner.clone());
        }
        if self.ownership.threads.len() >= MAX_BINDINGS {
            return Err("history_owner_capacity: verified history bindings are full".into());
        }
        let owner = self.capture_owner()?;
        self.ownership.threads.insert(identity, owner.clone());
        Ok(owner)
    }
}

/// Capture at admission. A first verified call binds its stable account/client/thread.
pub(crate) fn capture(call: Option<&CallScope>) -> Result<HistoryOwner, String> {
    #[cfg(test)]
    if TEST_HISTORY.try_with(|_| ()).is_err() {
        return Err("no isolated history fixture for this call".into());
    }
    with(|history| match call.and_then(VerifiedThread::from_call) {
        Some(identity) => history.bind_owner(identity),
        None => history.capture_owner(),
    })
}

/// Observe the selected owner without reserving a conversation or waiting for disk I/O.
pub(crate) fn current_owner() -> Option<HistoryOwner> {
    #[cfg(test)]
    if let Ok(owner) = TEST_HISTORY.try_with(|store| store.current.get()) {
        return owner;
    }
    HISTORY.get().and_then(|store| store.current.get())
}

fn with_owner<T>(
    owner: &HistoryOwner,
    operation: impl FnOnce(&mut History, &HistoryOwner) -> Result<T, String>,
) -> Result<T, String> {
    #[cfg(test)]
    if let Some(fixture) = &owner.fixture {
        let store = fixture.upgrade().ok_or(RELEASED)?;
        let mut state = store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        return operation(state.as_mut().ok_or(RELEASED)?, owner);
    }
    with(|history| operation(history, owner))
}

/// Append using the captured capability and return the persisted message ID.
pub(crate) fn record_owned_agent(
    owner: &HistoryOwner,
    id: Option<&str>,
    text: &str,
) -> Result<String, String> {
    let committed: Option<Committed> = with_owner(owner, |history, owner| {
        if id.is_some_and(|id| !super::valid_id(id, "message")) {
            return Err("invalid agent message identity for conversation history".into());
        }
        history.record_owned(owner, "assistant", text, id.map(str::to_owned))
    })?;
    let persisted_id = match &committed {
        Some(saved) => saved.message.id.clone(),
        // A matching explicit ID was already stored.
        None => id
            .ok_or("agent message was not committed to conversation history")?
            .to_owned(),
    };
    #[cfg(test)]
    if owner.fixture.is_some() {
        // Fixture history cannot write into a separately configured memory store.
        return Ok(persisted_id);
    }
    super::remember_committed(committed);
    Ok(persisted_id)
}

pub(crate) fn record_owned_request(
    owner: &HistoryOwner,
    request_id: &str,
    text: &str,
) -> Result<(), String> {
    let id = super::request_identity(request_id, text)?;
    with_owner(owner, |history, owner| {
        history
            .record_owned(owner, "user", text, Some(id))
            .map(|_| ())
    })
}

/// Check and update the leaf copy cache while selection remains stable.
pub(crate) fn remember_visible_reply(owner: &HistoryOwner, text: &str) -> bool {
    with_owner(owner, |history, owner| {
        let current = history.ownership.current.get().as_ref() == Some(owner);
        if current {
            super::super::messages::remember_last(text);
        }
        Ok(current)
    })
    .unwrap_or(false)
}

#[cfg(test)]
tokio::task_local! {
    pub(super) static TEST_HISTORY: Arc<HistoryStore>;
}

#[cfg(test)]
pub(crate) async fn with_test_history<F: std::future::Future>(
    path: std::path::PathBuf,
    future: F,
) -> F::Output {
    let history = History::with_workspace(path, "/owner-fixture".into()).unwrap();
    let store = HistoryStore {
        current: history.ownership.current.clone(),
        state: Mutex::new(Some(history)),
    };
    TEST_HISTORY.scope(Arc::new(store), future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_owner_is_available_while_history_storage_is_locked() {
        use std::{sync::mpsc, time::Duration};

        let temp = super::super::tests::Temporary::new();
        let history = temp.history();
        let store = Arc::new(HistoryStore {
            current: history.ownership.current.clone(),
            state: Mutex::new(Some(history)),
        });
        TEST_HISTORY.sync_scope(store.clone(), || {
            let expected = capture(None).unwrap();
            let locked = store.state.lock().unwrap();
            let reader_store = store.clone();
            let (sent, received) = mpsc::channel();
            let reader = std::thread::spawn(move || {
                TEST_HISTORY.sync_scope(reader_store, || {
                    sent.send(current_owner()).unwrap();
                });
            });

            // The reader must finish while storage is still locked. The timeout
            // is only a deadlock watchdog; no sleep or storage speed is assumed.
            let observed = received.recv_timeout(Duration::from_secs(1));
            drop(locked);
            reader.join().unwrap();
            assert_eq!(
                observed.expect("display ownership must not wait for history storage"),
                Some(expected)
            );
        });
    }

    #[test]
    fn shared_current_owner_tracks_switch_revocation_and_release() {
        let mut ownership = Ownership::default();
        let display = ownership.current.clone();
        let a = "conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let b = "conversation_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(display.get().is_none());

        let first = ownership.select(a, true);
        assert_eq!(display.get(), Some(first.clone()));
        assert_eq!(ownership.select(a, false), first);
        let second = ownership.select(b, false);
        assert_eq!(display.get(), Some(second.clone()));
        ownership.revoke(a);
        assert_eq!(display.get(), Some(second));
        ownership.revoke(b);
        assert!(display.get().is_none());

        let restored = ownership.select(a, false);
        assert_ne!(restored, first);
        assert_eq!(display.get(), Some(restored));
        ownership.release();
        assert!(display.get().is_none());
    }

    #[test]
    fn a_reserved_inactive_owner_can_materialize_only_its_own_file() {
        let temp = super::super::tests::Temporary::new();
        let mut history = temp.history();
        history.record_request("b", "Saved B").unwrap();
        let b = history.current().unwrap().unwrap();
        history.release_current();

        let pending_a = history.capture_owner().unwrap();
        assert_eq!(history.sessions().unwrap().len(), 1);
        assert!(history.current().unwrap().is_none());
        history.resume(&b.metadata.id).unwrap();
        history
            .record_owned(&pending_a, "assistant", "First A", None)
            .unwrap();
        history
            .record_owned(&pending_a, "assistant", "Second A", None)
            .unwrap();

        let a = history.read(pending_a.id()).unwrap();
        assert_eq!(a.messages.len(), 2);
        assert_eq!(a.messages[0].content, "First A");
        assert_eq!(a.messages[1].content, "Second A");
        assert_eq!(history.current().unwrap().unwrap(), b);
    }

    #[test]
    fn an_inactive_owner_cannot_write_through_another_clients_lease() {
        let temp = super::super::tests::Temporary::new();
        let mut history = temp.history();
        history.record_request("b", "Saved B").unwrap();
        let b = history.current().unwrap().unwrap();
        history.release_current();
        history.record_request("a", "Saved A").unwrap();
        let a = history.capture_owner().unwrap();
        history.resume(&b.metadata.id).unwrap();

        // Separate History instances open independent file handles, as another client does.
        let mut other = temp.history();
        let original_a = other.resume(a.id()).unwrap();
        let error = history
            .record_owned(&a, "assistant", "Blocked reply", None)
            .unwrap_err();
        assert!(error.contains("history_owner_unavailable"), "{error}");
        assert_eq!(other.current().unwrap().unwrap(), original_a);
        assert_eq!(history.current().unwrap().unwrap(), b);
    }

    #[test]
    fn capacity_failure_and_closed_owner_never_rebind_a_thread_to_b() {
        let temp = super::super::tests::Temporary::new();
        let mut history = temp.history();
        history.record_request("b", "Saved B").unwrap();
        let b = history.current().unwrap().unwrap();
        history.release_current();
        history.record_request("a", "Saved A").unwrap();

        let identity = |index: usize| VerifiedThread {
            provider: "github".into(),
            subject: "123".into(),
            client: "client_fixture".into(),
            thread: format!("agent_{index:032x}"),
        };
        let first = history.bind_owner(identity(0)).unwrap();
        for index in 1..MAX_BINDINGS {
            history.bind_owner(identity(index)).unwrap();
        }
        history.resume(&b.metadata.id).unwrap();
        let error = history.bind_owner(identity(MAX_BINDINGS)).unwrap_err();
        assert!(error.contains("history_owner_capacity"), "{error}");
        let late = history.bind_owner(identity(0)).unwrap();
        assert_eq!(late, first);
        history
            .record_owned(&late, "assistant", "Late A", None)
            .unwrap();
        assert_eq!(
            history.read(first.id()).unwrap().messages[1].content,
            "Late A"
        );
        assert_eq!(history.current().unwrap().unwrap(), b);

        // The thread survives as a tombstone, including across archive restore.
        history.archive_change(Some(first.id()), true).unwrap();
        history.archive_change(Some(first.id()), false).unwrap();
        let old = history.bind_owner(identity(0)).unwrap();
        assert_eq!(old, first);
        let error = history
            .record_owned(&old, "assistant", "Stale reply", None)
            .unwrap_err();
        assert!(error.contains("history_owner_released"), "{error}");
        assert_eq!(history.read(first.id()).unwrap().messages.len(), 2);
        assert_eq!(history.current().unwrap().unwrap(), b);
    }
}
