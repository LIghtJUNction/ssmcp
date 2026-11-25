//! Immutable metadata accepted at the relay's native-call boundary.
//!
//! This is an in-process capability, never deserialized from tool arguments.
//! Run IDs correlate presentation; they do not grant access to any resource.
use crate::{
    client::interface::history::{self, HistoryId, HistoryOwner},
    wire::{CodeContext, ControlAttribution, openai::OpenAiContext},
};
use std::{
    future::Future,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

static NEXT_CONNECTION: AtomicU64 = AtomicU64::new(1);

tokio::task_local! {
    static CURRENT: Source;
}

#[derive(Debug)]
pub(crate) struct CallScope {
    connection: u64,
    client_id: Option<String>,
    native_tool: OnceLock<String>,
    local_task_participant: OnceLock<ThreadScope>,
    pub(super) clock: OnceLock<std::sync::Weak<super::time::Clock>>,
    request_id: u64,
    run_id: Option<String>,
    attribution: Option<ControlAttribution>,
    thread: Option<ThreadScope>,
    history: Option<Result<HistoryOwner, String>>,
}

/// Local sidecar write bound to the already captured history at call admission.
pub(crate) struct OpenAiObservation {
    owner: HistoryOwner,
    metadata: OpenAiContext,
    observed_ms: u64,
}

impl OpenAiObservation {
    /// Returns false only if the verified history has not been saved yet.
    pub(crate) fn record(&self) -> Result<bool, String> {
        history::openai::observe(&self.owner, &self.metadata, self.observed_ms)
    }
}

/// Accepted only from the relay's verified participant binding, never tool JSON.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadScope {
    session_id: String,
    thread_id: String,
}

impl ThreadScope {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn thread_id(&self) -> &str {
        &self.thread_id
    }
}

pub(super) fn connection() -> u64 {
    NEXT_CONNECTION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("native connection generations exhausted")
}

impl CallScope {
    #[cfg(test)]
    pub(super) fn from_relay(
        connection: u64,
        request_id: u64,
        attribution: Option<&ControlAttribution>,
        context: Option<&CodeContext>,
    ) -> Result<Arc<Self>, String> {
        Self::from_relay_for_client(connection, None, request_id, attribution, context)
    }

    pub(crate) fn from_relay_for_client(
        connection: u64,
        client_id: Option<&str>,
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
        let thread = context
            .filter(|context| context.thread_id.is_some())
            .map(|context| {
                let thread_id = context.thread_id.as_deref().expect("validated pair");
                if attribution
                    .and_then(|value| value.agent.as_ref())
                    .is_none_or(|agent| agent.id != thread_id)
                {
                    return Err("thread scope lacks a matching authenticated participant".into());
                }
                Ok::<_, String>(ThreadScope {
                    session_id: context.session_id.clone().expect("validated pair"),
                    thread_id: thread_id.into(),
                })
            })
            .transpose()?;
        if let (Some(client_id), Some(thread)) = (client_id, &thread)
            && client_id != thread.session_id()
        {
            return Err("thread scope belongs to a different native connection".into());
        }
        let mut scope = Self {
            connection,
            client_id: client_id
                .map(str::to_owned)
                .or_else(|| thread.as_ref().map(|thread| thread.session_id().to_owned())),
            native_tool: OnceLock::new(),
            local_task_participant: OnceLock::new(),
            clock: OnceLock::new(),
            request_id,
            run_id: context.map(|context| context.run_id.clone()),
            attribution: attribution.cloned(),
            thread,
            history: None,
        };
        scope.history = Some(history::capture(Some(&scope)));
        Ok(Arc::new(scope))
    }

    pub(crate) fn connection(&self) -> u64 {
        self.connection
    }

    /// Frozen from the relay registration, never a display name or tool argument.
    pub(crate) fn client_id(&self) -> Option<&str> {
        self.client_id.as_deref()
    }

    pub(crate) fn set_native_tool(&self, name: &str) {
        let _ = self.native_tool.set(name.to_owned());
    }

    pub(crate) fn native_tool(&self) -> Option<&str> {
        self.native_tool.get().map(String::as_str)
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

    /// Called only at private local frame admission, before the provider is run.
    /// Never turns a local login into an account-backed thread or grants history access.
    pub(crate) fn bind_local_task_participant(&self, agent_id: String) -> Result<(), String> {
        let client = self.client_id().ok_or("missing registered local client")?;
        if self.attribution.is_some()
            || self.thread.is_some()
            || !agent_id
                .strip_prefix("agent_")
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("invalid local task participant".into());
        }
        self.local_task_participant
            .set(ThreadScope {
                session_id: client.into(),
                thread_id: agent_id,
            })
            .map_err(|_| "local task participant is already bound".into())
    }

    pub(super) fn local_task_participant(&self) -> Option<&ThreadScope> {
        self.local_task_participant.get()
    }

    pub(crate) fn thread(&self) -> Option<&ThreadScope> {
        self.thread.as_ref()
    }

    pub(crate) fn openai_observation(
        &self,
        metadata: OpenAiContext,
        observed_ms: u64,
    ) -> Option<OpenAiObservation> {
        // Ordinary unbound calls may have a display owner. That alone cannot
        // associate an external conversation or subject with the history.
        self.thread()?;
        let owner = self.history.as_ref()?.as_ref().ok()?.clone();
        Some(OpenAiObservation {
            owner,
            metadata,
            observed_ms,
        })
    }

    pub(crate) fn same_call(&self, other: &Self) -> bool {
        self.connection() == other.connection()
            && self.client_id() == other.client_id()
            && self.request_id() == other.request_id()
            && self.run_id() == other.run_id()
            && self.attribution() == other.attribution()
            && self.thread() == other.thread()
    }

    /// Tokio does not inherit this scope across spawn/spawn_blocking. A native
    /// owner must explicitly retain this Arc for work that outlives its call.
    pub(crate) async fn scope<F: Future>(self: &Arc<Self>, future: F) -> F::Output {
        Source {
            call: Some(self.clone()),
            history: self.history.clone(),
            local_artifact_tracking: false,
        }
        .scope(future)
        .await
    }
}

/// Retained event provenance. Local ownership and authenticated creator are independent.
#[derive(Clone, Debug, Default)]
pub(crate) struct Source {
    pub(crate) call: Option<Arc<CallScope>>,
    local_artifact_tracking: bool,
    history: Option<Result<HistoryOwner, String>>,
}

impl Source {
    /// New work inherits any explicit context, including an unowned context.
    pub(crate) fn capture() -> Self {
        CURRENT.try_with(Clone::clone).unwrap_or_else(|_| Self {
            call: None,
            history: Some(history::capture(None)),
            local_artifact_tracking: false,
        })
    }

    pub(crate) fn with_local_artifact_tracking(mut self) -> Self {
        self.local_artifact_tracking = self.call.is_none();
        self
    }

    pub(crate) fn allows_local_artifact_tracking(&self) -> bool {
        self.call.is_none() && self.local_artifact_tracking
    }

    pub(crate) fn owner(&self) -> Option<&HistoryOwner> {
        self.history.as_ref()?.as_ref().ok()
    }

    /// Called only by the native pipes/PTY completion paths with their original job.
    pub(crate) fn report_task_failure(&self, snapshot: &serde_json::Value) {
        let Some(call) = &self.call else {
            return;
        };
        if let Some(notice) = super::time::memory_hook::Notice::from_snapshot(snapshot)
            && let Some(clock) = call.clock.get().and_then(std::sync::Weak::upgrade)
        {
            clock.record_task_failure(self, notice);
        }
    }

    pub(crate) fn display(&self) -> DisplaySource {
        DisplaySource {
            call: self.call.clone(),
            history: self.owner().map_or(DisplayHistory::Global, |owner| {
                DisplayHistory::Local(owner.display_id())
            }),
        }
    }

    pub(crate) fn require_owner(&self) -> Result<&HistoryOwner, String> {
        self.history
            .as_ref()
            .ok_or_else(|| "history_owner_missing: no local history was captured".to_owned())?
            .as_ref()
            .map_err(Clone::clone)
    }

    pub(crate) async fn scope<F: Future>(self, future: F) -> F::Output {
        CURRENT.scope(self, future).await
    }

    pub(crate) fn blocking<F: FnOnce() -> R, R>(self, work: F) -> R {
        CURRENT.sync_scope(self, work)
    }
}

/// Display routing for a local conversation, global activity or legacy attach replay.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum DisplayHistory {
    #[default]
    NoSelection,
    Local(HistoryId),
    LegacyAttach,
    Global,
}

impl DisplayHistory {
    pub(crate) fn local_id(&self) -> Option<&HistoryId> {
        match self {
            Self::Local(id) => Some(id),
            _ => None,
        }
    }
}

/// Local display provenance. Attach replay does not recreate a call scope.
#[derive(Clone)]
pub(crate) struct DisplaySource {
    pub(crate) call: Option<Arc<CallScope>>,
    pub(crate) history: DisplayHistory,
}

impl Default for DisplaySource {
    fn default() -> Self {
        Self {
            call: None,
            history: DisplayHistory::Global,
        }
    }
}

pub(crate) fn current_source() -> Source {
    CURRENT.try_with(Clone::clone).unwrap_or_default()
}

pub(crate) fn current() -> Option<Arc<CallScope>> {
    CURRENT
        .try_with(|source| source.call.clone())
        .ok()
        .flatten()
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
                Source::default().blocking(|| assert!(current().is_none()));
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

    #[test]
    fn call_scope_thread_requires_the_same_verified_agent_and_preserves_session() {
        let agent_id = format!("agent_{}", "a".repeat(32));
        let mut attribution = source("123", 1).attribution().unwrap().clone();
        let context = CodeContext {
            run_id: "a".repeat(32),
            code: "return 1".into(),
            timestamp_ms: 1,
            session_id: Some("session_fixture".into()),
            thread_id: Some(agent_id.clone()),
        };
        assert!(CallScope::from_relay(7, 1, None, Some(&context)).is_err());
        assert!(CallScope::from_relay(7, 1, Some(&attribution), Some(&context)).is_err());
        attribution.agent = Some(crate::wire::ControlAgent {
            id: agent_id.clone(),
            name: "display only".into(),
        });
        let scope = CallScope::from_relay(7, 1, Some(&attribution), Some(&context)).unwrap();
        assert_eq!(scope.thread().unwrap().session_id(), "session_fixture");
        assert_eq!(scope.thread().unwrap().thread_id(), agent_id);
        assert_eq!(scope.client_id(), Some("session_fixture"));
        assert!(
            CallScope::from_relay_for_client(
                7,
                Some("other_connection"),
                1,
                Some(&attribution),
                Some(&context)
            )
            .is_err()
        );
        attribution.agent.as_mut().unwrap().id = format!("agent_{}", "b".repeat(32));
        assert!(CallScope::from_relay(7, 1, Some(&attribution), Some(&context)).is_err());
    }

    #[test]
    fn main_scope_freezes_registered_client_and_native_tool_without_display_inference() {
        let attribution = source("123", 1).attribution().unwrap().clone();
        let client = "a".repeat(64);
        let scope = CallScope::from_relay_for_client(9, Some(&client), 4, Some(&attribution), None)
            .unwrap();
        assert_eq!(scope.client_id(), Some(client.as_str()));
        assert!(scope.thread().is_none());
        scope.set_native_tool("ssmcp.fs.stat");
        scope.set_native_tool("forged later label");
        assert_eq!(scope.native_tool(), Some("ssmcp.fs.stat"));
        assert_eq!(scope.attribution().unwrap().account.subject, "123");
    }
}
