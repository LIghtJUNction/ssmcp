//! Session-owned update discovery, kept outside the compositor lock.
use super::{Arc, Ordering, Shared, State};
use crate::client::update::ConnectionVersions;
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Notice {
    Release(String),
    Connection(ConnectionVersions),
}

impl Notice {
    pub(super) fn text(&self) -> String {
        match self {
            Self::Release(message) => message.clone(),
            Self::Connection(versions) => versions
                .update_message()
                .unwrap_or_else(|| versions.summary()),
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Self::Release(_) => 2,
            Self::Connection(versions) => u8::from(versions.update_message().is_some()),
        }
    }
}

#[derive(Default)]
struct Seen(VecDeque<Notice>);

impl Seen {
    fn contains(&self, notice: &Notice) -> bool {
        self.0.contains(notice)
    }

    fn record(&mut self, notice: Notice) {
        if self.contains(&notice) {
            return;
        }
        if self.0.len() == 16 {
            self.0.pop_front();
        }
        self.0.push_back(notice);
    }
}

fn with_seen<T>(operation: impl FnOnce(&mut Seen) -> T) -> T {
    static SHOWN: std::sync::OnceLock<std::sync::Mutex<Seen>> = std::sync::OnceLock::new();
    operation(
        &mut SHOWN
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    )
}

fn was_shown(notice: &Notice) -> bool {
    with_seen(|seen| seen.contains(notice))
}

// A generated frame is not delivery: only a completely drained terminal
// presentation consumes deduplication. Hidden or partially written notices retry.
pub(super) fn complete(pending: &mut Option<Notice>, drained: bool) {
    if drained && let Some(notice) = pending.take() {
        with_seen(|seen| seen.record(notice));
    }
}

fn offer(state: &mut State, notice: Notice, shown: impl FnOnce(&Notice) -> bool) -> bool {
    if shown(&notice)
        || state
            .update_notice
            .as_ref()
            .is_some_and(|current| current.priority() > notice.priority())
    {
        return false;
    }
    state.update_notice = Some(notice);
    state.dirty = true;
    true
}

pub(super) fn connection(state: &mut State, versions: &ConnectionVersions) {
    offer(state, Notice::Connection(versions.clone()), was_shown);
}

pub(super) fn spawn(
    shared: &Arc<Shared>,
    connected: bool,
    stdout_is_terminal: bool,
    check: impl std::future::Future<Output = Option<String>> + Send + 'static,
) -> Option<tokio::task::JoinHandle<()>> {
    // Session already requires an interactive dashboard and stdin/stderr.
    // A machine-readable stdout pipe and a preview never start this request.
    if !connected || !stdout_is_terminal {
        return None;
    }
    if shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .preview
    {
        return None;
    }
    let runtime = tokio::runtime::Handle::try_current().ok()?;
    let display = Arc::downgrade(shared);
    Some(runtime.spawn(async move {
        let Some(message) = check.await else {
            return;
        };
        publish(&display, message, was_shown);
    }))
}

fn publish(
    display: &std::sync::Weak<Shared>,
    message: String,
    shown: impl FnOnce(&Notice) -> bool,
) -> bool {
    let Some(shared) = display.upgrade() else {
        return false;
    };
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if shared.stop.load(Ordering::Acquire) || !shared.alive.load(Ordering::Acquire) || state.preview
    {
        return false;
    }
    // This lower-priority slot cannot replace errors, drafts, or workflow hints.
    offer(&mut state, Notice::Release(message), shown)
}

#[cfg(test)]
mod tests;
