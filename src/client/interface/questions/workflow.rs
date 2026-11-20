//! Question change notifications and authenticated message deduplication scope.
use super::*;
use tokio::sync::watch;

static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();

pub(super) fn notify() {
    CHANGES
        .get_or_init(|| watch::channel(0).0)
        .send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
}

pub(super) fn message_scope() -> Option<String> {
    RequestScope::current().ok().map(|scope| {
        format!(
            "{}:{}:{}:{}:{}",
            scope.connection,
            scope.session_id,
            scope.requester.provider,
            scope.requester.subject,
            scope.requester.thread_id
        )
    })
}
