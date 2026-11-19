//! Immutable metadata accepted at the relay's native-call boundary.
//!
//! This is an in-process capability, never deserialized from tool arguments.
//! Run IDs correlate presentation; they do not grant access to any resource.
use crate::wire::{CodeContext, ControlAttribution};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    static CURRENT: Option<Arc<CallScope>>;
}

#[derive(Debug)]
pub(crate) struct CallScope {
    connection: u64,
    request_id: u64,
    run_id: Option<String>,
    attribution: Option<ControlAttribution>,
}

pub(super) fn connection() -> u64 {
    NEXT_CONNECTION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("native connection generations exhausted")
}

impl CallScope {
    pub(super) fn from_relay(
        connection: u64,
        request_id: u64,
        attribution: Option<&ControlAttribution>,
        context: Option<&CodeContext>,
    ) -> Result<Arc<Self>, String> {
        if let Some(attribution) = attribution {
            attribution.validate()?;
        }
        if let Some(context) = context {
            context.validate()?;
        }
        Ok(Arc::new(Self {
            connection,
            request_id,
            run_id: context.map(|context| context.run_id.clone()),
            attribution: attribution.cloned(),
        }))
    }

    pub(crate) fn connection(&self) -> u64 {
        self.connection
    }

    pub(crate) fn request_id(&self) -> u64 {
        self.request_id
    }

    pub(crate) fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }

    pub(crate) fn attribution(&self) -> Option<&ControlAttribution> {
        self.attribution.as_ref()
    }

    pub(crate) fn same_call(&self, other: &Self) -> bool {
        self.connection() == other.connection()
            && self.request_id() == other.request_id()
            && self.run_id() == other.run_id()
            && self.attribution() == other.attribution()
    }

    /// Tokio does not inherit this scope across spawn/spawn_blocking. A native
    /// owner must explicitly retain this Arc for work that outlives its call.
    pub(crate) async fn scope<F: Future>(self: &Arc<Self>, future: F) -> F::Output {
        CURRENT.scope(Some(self.clone()), future).await
    }
}

/// Preserve a captured native call across a known blocking worker boundary.
/// An absent capture deliberately does not inherit any ambient source.
pub(crate) fn blocking<F: FnOnce() -> R, R>(scope: Option<Arc<CallScope>>, work: F) -> R {
    CURRENT.sync_scope(scope, work)
}

pub(crate) fn current() -> Option<Arc<CallScope>> {
    CURRENT.try_with(Clone::clone).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::ControlAccount;

    fn source(subject: &str, request_id: u64) -> Arc<CallScope> {
        CallScope::from_relay(
            1,
            request_id,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: "same-label".into(),
                },
                owner: None,
                agent: None,
            }),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn call_scope_is_immutable_and_isolated_across_interleaved_requests() {
        let first = source("123", 7);
        let second = source("456", 8);
        let barrier = tokio::sync::Barrier::new(2);
        let check = |source: Arc<CallScope>| {
            let barrier = &barrier;
            async move {
                source
                    .scope(async {
                        barrier.wait().await;
                        tokio::task::yield_now().await;
                        let seen = current().unwrap();
                        assert!(Arc::ptr_eq(&seen, &source));
                        assert_eq!(seen.connection(), 1);
                        assert_eq!(seen.request_id(), source.request_id());
                        assert_eq!(seen.run_id(), None);
                        assert_eq!(seen.attribution(), source.attribution());
                        // A provider payload remains ordinary data.
                        let _forged = serde_json::json!({"source":{"subject":"999"}});
                        assert_eq!(current().unwrap().attribution(), source.attribution());
                    })
                    .await;
            }
        };
        tokio::join!(check(first), check(second));
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn call_scope_background_work_requires_explicit_propagation() {
        let source = source("123", 1);
        source
            .scope(async {
                assert!(tokio::spawn(async { current().is_none() }).await.unwrap());
                assert!(
                    tokio::task::spawn_blocking(|| current().is_none())
                        .await
                        .unwrap()
                );
                blocking(None, || assert!(current().is_none()));
                assert!(current().is_some());
                let retained = current().unwrap();
                let inherited = tokio::spawn(async move {
                    retained
                        .scope(async { current().unwrap().request_id() })
                        .await
                })
                .await
                .unwrap();
                assert_eq!(inherited, 1);
            })
            .await;
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn call_scope_cancellation_drops_only_the_active_scope() {
        let source = source("123", 1);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(1),
            source.scope(std::future::pending::<()>()),
        )
        .await;
        assert!(result.is_err());
        assert!(current().is_none());
    }

    #[test]
    fn call_scope_validates_wire_metadata_and_connection_generations() {
        let mut invalid = source("123", 1).attribution().unwrap().clone();
        invalid.account.subject = "0".into();
        assert!(CallScope::from_relay(1, 1, Some(&invalid), None).is_err());
        assert_ne!(connection(), connection());
    }
}
