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

fn subscribe() -> watch::Receiver<u64> {
    CHANGES.get_or_init(|| watch::channel(0).0).subscribe()
}

/// Prepare or resume one real, verified-thread finish request. No tool JSON can
/// supply a Human actor or a decision.
pub(crate) fn prepare_finish(
    reason: Option<&str>,
    request_id: Option<&str>,
    recipient: &str,
) -> Result<FinishReceipt, String> {
    let scope = RequestScope::current()?;
    if scope.requester.thread_id != recipient {
        return Err(
            "finish_scope_mismatch: recipient must be the authenticated asking thread".into(),
        );
    }
    if let Some(id) = request_id {
        let queue = state().lock().unwrap_or_else(|error| error.into_inner());
        return FinishReceipt::from_view(queue.finish_view_for(&scope, id)?, false);
    }
    let request = parse_ask(json!({
        "title":"结束对话前请你决定",
        "question":reason.unwrap_or("Agent 已准备结束本轮对话。是否允许现在结束？选择 No 继续；只有你明确选择 Yes 才会批准。"),
        "options":["No","Yes"]
    }))?;
    let owner = history::capture();
    let (question, created, view) = {
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        let (question, created) = queue.ask_owned(
            request,
            Some(scope.clone()),
            QuestionKind::Finish,
            Some(owner),
        )?;
        let view = queue.finish_view_for(&scope, &question.id)?;
        (question, created, view)
    };
    if created {
        let _ = history::record_created(&question);
        changed();
    }
    FinishReceipt::from_view(view, false)
}

/// Read-only authorization before the wait hub changes any terminal state.
pub(crate) fn validate_finish(id: Option<&str>, recipient: Option<&str>) -> Result<(), String> {
    let scope = RequestScope::current()?;
    if recipient.is_some_and(|recipient| scope.requester.thread_id != recipient) {
        return Err("finish_scope_mismatch: this wait belongs to another asking thread".into());
    }
    let queue = state().lock().unwrap_or_else(|error| error.into_inner());
    queue.validate_scope(&scope)?;
    if let Some(id) = id {
        queue.finish_view_for(&scope, id)?;
    }
    Ok(())
}

pub(crate) fn validate_finish_owner(
    original: &crate::runtime::call_scope::CallScope,
    request_id: Option<&str>,
) -> Result<(), String> {
    let original = RequestScope::from_call(original)?;
    let current = RequestScope::current()?;
    if !original.same_thread(&current) {
        return Err(
            "finish_scope_mismatch: this wait belongs to another thread, account or connection"
                .into(),
        );
    }
    let queue = state().lock().unwrap_or_else(|error| error.into_inner());
    queue.validate_scope(&current)?;
    if let Some(id) = request_id {
        queue.finish_view_for(&current, id)?;
    }
    Ok(())
}

pub(crate) fn cancel_finish(id: &str, reason: &str) -> Result<FinishReceipt, String> {
    let scope = RequestScope::current()?;
    let view = state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .cancel_finish_for(&scope, id, reason)?;
    notify();
    FinishReceipt::from_view(view, false)
}

pub(crate) struct FinishWait {
    request_id: String,
    scope: RequestScope,
    changes: watch::Receiver<u64>,
    keep: bool,
}

impl FinishWait {
    pub(crate) fn begin(id: &str, recipient: &str) -> Result<Self, String> {
        let scope = RequestScope::current()?;
        if scope.requester.thread_id != recipient {
            return Err(
                "finish_scope_mismatch: recipient must be the authenticated asking thread".into(),
            );
        }
        // Subscribe before the first observation; changes cannot be lost between
        // examining a Pending decision and polling changed().
        let changes = subscribe();
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        queue.finish_view_for(&scope, id)?;
        if queue.finish_waiting.contains_key(id) {
            return Err("finish_busy: this exact request already has an active waiter".into());
        }
        queue.finish_waiting.insert(id.into(), 1);
        Ok(Self {
            request_id: id.into(),
            scope,
            changes,
            keep: false,
        })
    }

    pub(crate) fn receipt(&self) -> Result<FinishReceipt, String> {
        let queue = state().lock().unwrap_or_else(|error| error.into_inner());
        FinishReceipt::from_view(queue.finish_view_for(&self.scope, &self.request_id)?, false)
    }

    pub(crate) fn poll(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<Option<FinishReceipt>, String> {
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        let view = queue.finish_view_for(&self.scope, &self.request_id)?;
        let finish = view.finish.as_ref().ok_or("finish_request_required")?;
        if finish.status == FinishStatus::Pending {
            return Ok(None);
        }
        if finish.status == FinishStatus::Approved {
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            let revision = finish
                .decision
                .as_ref()
                .ok_or("finish_decision_missing")?
                .revision;
            let consumed =
                queue.consume_finish_revision(&self.scope, &self.request_id, revision)?;
            self.keep = true;
            return FinishReceipt::from_view(consumed, true).map(Some);
        }
        self.keep = true;
        FinishReceipt::from_view(view, false).map(Some)
    }

    pub(crate) async fn changed(&mut self) -> Result<(), String> {
        self.changes
            .changed()
            .await
            .map_err(|_| "finish decision channel closed".into())
    }

    pub(crate) fn end(&mut self, timed_out: bool) -> Result<FinishReceipt, String> {
        self.keep = true;
        if timed_out {
            return self.receipt();
        }
        let view = state()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancel_finish_for(&self.scope, &self.request_id, "wait_cancelled")?;
        notify();
        FinishReceipt::from_view(view, false)
    }
}

impl Drop for FinishWait {
    fn drop(&mut self) {
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        queue.finish_waiting.remove(&self.request_id);
        if !self.keep {
            let _ = queue.cancel_finish_for(&self.scope, &self.request_id, "wait_cancelled");
        }
        drop(queue);
        notify();
    }
}
