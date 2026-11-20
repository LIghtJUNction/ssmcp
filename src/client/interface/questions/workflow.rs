//! Native endpoints use an accepted call scope, never identity fields in JSON.
use super::*;
use std::time::Duration;
use tokio::sync::watch;

static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();

pub(super) fn notify() {
    CHANGES
        .get_or_init(|| watch::channel(0).0)
        .send_modify(|revision| {
            *revision = revision.wrapping_add(1);
        });
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum FinishRequest {
    Request {
        reason: String,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        idempotency_key: Option<String>,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
    Cancel {
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    Wait {
        request_id: String,
        #[serde(default = "default_timeout")]
        timeout_ms: u64,
    },
}

fn default_timeout() -> u64 {
    20_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerRequest {
    id: String,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    decision: Option<FinishChoice>,
    #[serde(default)]
    comment: Option<String>,
}

fn finish_view(queue: &Queue, id: &str, scope: &RequestScope) -> Result<QuestionView, String> {
    queue.validate_scope(scope)?;
    queue
        .records
        .iter()
        .find(|record| {
            record.question.id == id
                && record.routing.as_ref().is_some_and(|routing| {
                    routing.kind == QuestionKind::Finish && routing.scope.same_thread(scope)
                })
        })
        .map(Record::view)
        .ok_or_else(|| "finish_request_not_found: no retained request for this thread".into())
}

async fn wait_for_decision(
    queue: &Mutex<Queue>,
    id: &str,
    scope: &RequestScope,
    changes: &mut watch::Receiver<u64>,
    deadline: tokio::time::Instant,
) -> Result<bool, String> {
    loop {
        let view = finish_view(
            &queue.lock().unwrap_or_else(|error| error.into_inner()),
            id,
            scope,
        )?;
        if view
            .finish
            .as_ref()
            .is_none_or(|finish| finish.status != FinishStatus::Pending)
        {
            return Ok(false);
        }
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return Ok(true),
            change = changes.changed() => { if change.is_err() { return Err("finish_wait_unavailable".into()); } }
        }
    }
}

fn cancel(id: &str, scope: &RequestScope) -> Result<QuestionView, String> {
    let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
    finish_view(&queue, id, scope)?;
    let record = queue
        .records
        .iter_mut()
        .find(|record| record.question.id == id)
        .expect("checked request");
    let routing = record.routing.as_mut().expect("checked finish");
    if matches!(
        routing.finish_status,
        Some(FinishStatus::Pending | FinishStatus::Approved)
    ) {
        routing.finish_status = Some(FinishStatus::Cancelled);
        routing.invalidation_reason = Some("request_cancelled".into());
        if record.question.state == QuestionState::Pending {
            record.question.state = QuestionState::Skipped;
        }
        queue.revision = queue.revision.saturating_add(1);
    }
    let view = finish_view(&queue, id, scope)?;
    drop(queue);
    changed();
    Ok(view)
}

struct WaitGuard {
    id: String,
    scope: RequestScope,
    finished: bool,
    cancel_on_drop: bool,
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        if !self.finished && self.cancel_on_drop {
            let _ = cancel(&self.id, &self.scope);
        }
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        if let Some(count) = queue.finish_waiting.get_mut(&self.id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                queue.finish_waiting.remove(&self.id);
            }
        }
        drop(queue);
        changed();
    }
}

pub(super) async fn finish(arguments: Value) -> Result<Value, String> {
    object_without_nulls(&arguments)?;
    let request: FinishRequest = serde_json::from_value(arguments).map_err(
        |_| "finish request accepts op request/wait/cancel and no actor or approval fields",
    )?;
    let scope = RequestScope::current().map_err(|error| format!(
        "{error}; join the existing client with collab.join (no model is started), then bind its returned agent_id/token"))?;
    if let FinishRequest::Wait {
        request_id,
        timeout_ms,
    } = request
    {
        identifier(&request_id)?;
        if !(1..=25_000).contains(&timeout_ms) {
            return Err("timeout_ms must be 1..=25000".into());
        }
        let mut changes = CHANGES.get_or_init(|| watch::channel(0).0).subscribe();
        {
            let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
            finish_view(&queue, &request_id, &scope)?;
            *queue.finish_waiting.entry(request_id.clone()).or_default() += 1;
        }
        // A passive observer must never cancel the approval transaction on drop.
        // The original request/cancel API retains its explicit cancellation semantics.
        let _guard = WaitGuard {
            id: request_id.clone(),
            scope: scope.clone(),
            finished: false,
            cancel_on_drop: false,
        };
        changed();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let timed_out =
            wait_for_decision(state(), &request_id, &scope, &mut changes, deadline).await?;
        let view = finish_view(
            &state().lock().unwrap_or_else(|error| error.into_inner()),
            &request_id,
            &scope,
        )?;
        return Ok(finish_response(&request_id, false, timed_out, view));
    }
    if let FinishRequest::Cancel {
        request_id,
        idempotency_key,
    } = request
    {
        let request_id = match (request_id, idempotency_key) {
            (Some(id), None) => {
                identifier(&id)?;
                id
            }
            (None, Some(key)) => {
                identifier(&key)?;
                let queue = state().lock().unwrap_or_else(|error| error.into_inner());
                queue.validate_scope(&scope)?;
                let Some(record) = queue.records.iter().find(|record| {
                    record.idempotency_key.as_deref() == Some(key.as_str())
                        && record.routing.as_ref().is_some_and(|routing| {
                            routing.kind == QuestionKind::Finish
                                && routing.scope.same_thread(&scope)
                        })
                }) else {
                    return Ok(
                        json!({"schema_version":1,"cancelled":false,"reason":"request_not_created"}),
                    );
                };
                record.question.id.clone()
            }
            _ => return Err("cancel requires exactly one request_id or idempotency_key".into()),
        };
        return Ok(json!({"schema_version":1,"request_id":request_id,
            "request":cancel(&request_id, &scope)?,"timed_out":false}));
    }
    let FinishRequest::Request {
        reason,
        request_id,
        idempotency_key,
        timeout_ms,
    } = request
    else {
        unreachable!()
    };
    validate_text(&reason, MAX_QUESTION_BYTES, "reason")?;
    if !(1..=25_000).contains(&timeout_ms) {
        return Err("timeout_ms must be 1..=25000".into());
    }
    if let Some(id) = &request_id {
        identifier(id)?;
    }
    if let Some(key) = &idempotency_key {
        identifier(key)?;
    }
    if request_id.is_some() && idempotency_key.is_some() {
        return Err("resume with request_id or create with idempotency_key, not both".into());
    }
    // Subscribe before examining state so an immediate local answer is retained.
    let mut changes = CHANGES.get_or_init(|| watch::channel(0).0).subscribe();
    let (id, created) = {
        let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
        if let Some(id) = request_id {
            let view = finish_view(&queue, &id, &scope)?;
            if view.question.question != reason {
                return Err("finish_reason_mismatch: resume the original reason".into());
            }
            (id, false)
        } else {
            let request = parse_ask(
                json!({"question":reason,"title":"Request to finish","options":["No","Yes"]}),
            )?;
            let request = AskRequest {
                idempotency_key,
                ..request
            };
            let (question, created) =
                queue.ask_for(request, Some(scope.clone()), QuestionKind::Finish)?;
            (question.id, created)
        }
    };
    if created {
        if let Some(question) = get(&id) {
            let _ = super::super::history::record("assistant", &history_text(&question));
        }
        changed();
    }
    let mut guard = WaitGuard {
        id: id.clone(),
        scope: scope.clone(),
        finished: false,
        cancel_on_drop: true,
    };
    *state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .finish_waiting
        .entry(id.clone())
        .or_default() += 1;
    changed();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let timed_out = wait_for_decision(state(), &id, &scope, &mut changes, deadline).await?;
    let view = {
        let queue = state().lock().unwrap_or_else(|error| error.into_inner());
        finish_view(&queue, &id, &scope)?
    };
    guard.finished = true;
    Ok(finish_response(&id, created, timed_out, view))
}

fn finish_response(id: &str, created: bool, timed_out: bool, view: QuestionView) -> Value {
    json!({"schema_version":1,"request_id":id,"created":created,
        "request":view,"timed_out":timed_out,
        "default_decision":"no","decision":view.finish.as_ref().and_then(|finish|finish.decision.as_ref()),
        "approved":view.finish.as_ref().is_some_and(|finish|finish.approved),
        "note":"Only the local user's explicit Yes permits completion. A timeout, an agent's completion or an agent's answer is not approval. Resume this request ID after a pending timeout; after No, continue the work or wait for user input. External assistant final output cannot be intercepted."})
}

pub(super) fn answer(arguments: Value) -> Result<Value, String> {
    object_without_nulls(&arguments)?;
    let request: AnswerRequest = serde_json::from_value(arguments)
        .map_err(|_| "answer accepts id with answer, or decision yes/no and optional comment; actor fields are forbidden")?;
    identifier(&request.id)?;
    let scope = RequestScope::current()?;
    let view =
        match (
            request.answer.as_deref(),
            request.decision,
            request.comment.as_deref(),
        ) {
            (Some(answer), None, None) => {
                let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
                queue.finish_for(&request.id, Some(answer), Some(&scope))?;
                queue
                    .records
                    .iter()
                    .find(|record| record.question.id == request.id)
                    .expect("answered record")
                    .view()
            }
            (None, Some(decision), comment) => state()
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .decide_finish_for(&request.id, decision, comment, Some(&scope))?,
            _ => return Err(
                "provide one ordinary answer, or an explicit yes/no decision and optional comment"
                    .into(),
            ),
        };
    changed();
    Ok(json!({"schema_version":1,"question":view}))
}

pub(super) fn authorize_completion() -> Result<String, String> {
    let scope = RequestScope::current()?;
    let id = state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .consume_finish(&scope)?;
    changed();
    Ok(id)
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

pub(super) fn tools() -> Vec<Value> {
    let id = json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"});
    vec![
        json!({
            "name":"ssmcp.ui.answer",
            "description":"Answer a retained question as the currently verified bound agent in the same client and account. Ordinary answers retain their agent origin. Agents may decline a finish request with decision no, but only the local user may approve completion; decision yes from any agent is rejected. Text yes is ordinary text and never approval. Reads do not consume answers.",
            "inputSchema":{"type":"object","additionalProperties":false,"required":["id"],"properties":{
                "id":id,"answer":{"type":"string","minLength":1,"maxLength":MAX_ANSWER_BYTES},
                "decision":{"enum":["yes","no"]},"comment":{"type":"string","minLength":1,"maxLength":MAX_ANSWER_BYTES}},
                "oneOf":[{"required":["answer"],"not":{"anyOf":[{"required":["decision"]},{"required":["comment"]}]}},
                    {"required":["decision"],"not":{"required":["answer"]}}]},
            "annotations":{"readOnlyHint":false,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.ui.finish",
            "description":"Request/resume the local user's reasoned finish decision using the current verified thread. This native entry also works when public request_finish is not exposed. collab.join registers the current participant without starting a model; bind its returned agent_id/token first. Request creates or resumes a reasoned request; wait only observes an existing request_id and its cancellation never invalidates that request. Bounded waits of 1..25000ms leave pending unchanged on timeout: resume the same ID. Only an explicit local human Yes permits completion; agent approval and timeout never do. Default No does not invent a human decision. Prefer public request_finish when exposed to avoid occupying a JavaScript worker. Cancel explicitly invalidates only this thread's exact request.",
            "inputSchema":{"type":"object","additionalProperties":false,"required":["op"],"properties":{
                "op":{"enum":["request","wait","cancel"]},"reason":{"type":"string","minLength":1,"maxLength":MAX_QUESTION_BYTES},
                "request_id":id,"idempotency_key":id,"timeout_ms":{"type":"integer","minimum":1,"maximum":25000,"default":20000}},
                "oneOf":[{"properties":{"op":{"const":"request"}},"required":["reason"]},
                    {"properties":{"op":{"const":"wait"}},"required":["request_id"],"not":{"anyOf":[{"required":["reason"]},{"required":["idempotency_key"]}]}},
                    {"properties":{"op":{"const":"cancel"}},"required":["request_id"],"not":{"anyOf":[{"required":["reason"]},{"required":["idempotency_key"]},{"required":["timeout_ms"]}]}}]},
            "annotations":{"readOnlyHint":false,"destructiveHint":false}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_json_cannot_supply_an_actor_or_decision() {
        for extra in ["actor", "approved", "decision", "thread_id", "subject"] {
            let mut value = json!({"op":"request","reason":"Ready after checks"});
            value[extra] = json!("yes");
            assert!(serde_json::from_value::<FinishRequest>(value).is_err());
        }
        assert!(
            serde_json::from_value::<AnswerRequest>(
                json!({"id":"q","decision":"yes","actor":"human"})
            )
            .is_err()
        );
    }

    fn fixture() -> (Mutex<Queue>, RequestScope, String) {
        let scope = RequestScope {
            connection: 7,
            session_id: "room".into(),
            requester: AgentActor {
                provider: "github".into(),
                subject: "42".into(),
                thread_id: "requester".into(),
                name: "display".into(),
            },
        };
        let mut queue = Queue {
            connection: Some(("room".into(), 7)),
            ..Queue::default()
        };
        let id = queue
            .ask_for(
                parse_ask(json!({"question":"Checks complete"})).unwrap(),
                Some(scope.clone()),
                QuestionKind::Finish,
            )
            .unwrap()
            .0
            .id;
        (Mutex::new(queue), scope, id)
    }

    #[tokio::test]
    async fn bounded_timeout_retains_pending_id_and_does_not_invent_no_or_yes() {
        let (queue, scope, id) = fixture();
        let (_sender, mut changes) = watch::channel(0);
        assert!(
            wait_for_decision(
                &queue,
                &id,
                &scope,
                &mut changes,
                tokio::time::Instant::now() + Duration::from_millis(1)
            )
            .await
            .unwrap()
        );
        let view = finish_view(&queue.lock().unwrap(), &id, &scope).unwrap();
        let finish = view.finish.unwrap();
        assert_eq!(finish.status, FinishStatus::Pending);
        assert_eq!(finish.default_decision, FinishChoice::No);
        assert!(finish.decision.is_none());
        assert!(!finish.approved);
        assert_eq!(view.question.id, id);
    }

    #[tokio::test]
    async fn a_real_decision_wakes_the_wait_and_keeps_its_actor_and_comment() {
        let (queue, scope, id) = fixture();
        let (sender, mut changes) = watch::channel(0);
        let waiter = wait_for_decision(
            &queue,
            &id,
            &scope,
            &mut changes,
            tokio::time::Instant::now() + Duration::from_secs(2),
        );
        let decision = async {
            tokio::task::yield_now().await;
            queue
                .lock()
                .unwrap()
                .decide_finish_for(
                    &id,
                    FinishChoice::No,
                    Some("Please finish the remaining check"),
                    None,
                )
                .unwrap();
            sender.send_modify(|revision| *revision += 1);
        };
        let (timed_out, ()) = tokio::join!(waiter, decision);
        assert!(!timed_out.unwrap());
        let view = finish_view(&queue.lock().unwrap(), &id, &scope).unwrap();
        let decision = view.finish.unwrap().decision.unwrap();
        assert_eq!(decision.actor, DecisionActor::Human);
        assert_eq!(decision.choice, FinishChoice::No);
        assert_eq!(
            decision.comment.as_deref(),
            Some("Please finish the remaining check")
        );
    }
}
