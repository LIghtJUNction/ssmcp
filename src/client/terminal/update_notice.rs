//! Session-owned update discovery, kept outside the compositor lock.
use super::{Arc, Ordering, Shared};

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
        let key = message.clone();
        publish(&display, message, || {
            crate::client::update::mark_notice_shown(&key)
        });
    }))
}

fn publish(
    display: &std::sync::Weak<Shared>,
    message: String,
    claim: impl FnOnce() -> bool,
) -> bool {
    let Some(shared) = display.upgrade() else {
        return false;
    };
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if shared.stop.load(Ordering::Acquire)
        || !shared.alive.load(Ordering::Acquire)
        || state.preview
        || !claim()
    {
        return false;
    }
    // This lower-priority slot cannot replace errors, drafts, or workflow hints.
    state.update_notice = Some(message);
    state.dirty = true;
    true
}

#[cfg(test)]
mod tests;
