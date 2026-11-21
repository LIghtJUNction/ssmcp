//! Completion permission requires a human decision, not an agent result or stale status.
use super::*;

fn scope(subject: &str, thread: &str) -> RequestScope {
    RequestScope {
        connection: 7,
        session_id: "room".into(),
        requester: AgentActor {
            provider: "github".into(),
            subject: subject.into(),
            thread_id: thread.into(),
            name: "synthetic participant".into(),
        },
    }
}

fn queue() -> Queue {
    Queue {
        connection: Some(("room".into(), 7)),
        ..Queue::default()
    }
}

fn finish(queue: &mut Queue, requester: RequestScope) -> String {
    queue
        .ask_for(
            parse_ask(json!({
                "question": "The requested change is ready for review.",
                "options": ["No", "Yes"]
            }))
            .unwrap(),
            Some(requester),
            QuestionKind::Finish,
        )
        .unwrap()
        .0
        .id
}

fn view(queue: &Queue, id: &str) -> QuestionView {
    queue
        .records
        .iter()
        .find(|record| record.question.id == id)
        .unwrap()
        .view()
}

fn snapshot(queue: &Queue) -> (u64, Vec<QuestionView>) {
    (
        queue.revision,
        queue.records.iter().map(Record::view).collect(),
    )
}

#[test]
fn only_human_yes_in_approved_state_exposes_and_consumes_permission() {
    use FinishChoice::{No, Yes};
    use FinishStatus::{Approved, Cancelled, Completed, Denied, Invalidated, Pending};

    let agent = scope("42", "reviewer").actor();
    for (status, decision, permitted) in [
        (Approved, Some((Yes, DecisionActor::Human)), true),
        (Approved, Some((Yes, agent.clone())), false),
        (Approved, None, false),
        (Approved, Some((No, DecisionActor::Human)), false),
        (Approved, Some((No, agent)), false),
        (Pending, Some((Yes, DecisionActor::Human)), false),
        (Denied, Some((Yes, DecisionActor::Human)), false),
        (Cancelled, Some((Yes, DecisionActor::Human)), false),
        (Invalidated, Some((Yes, DecisionActor::Human)), false),
        (Completed, Some((Yes, DecisionActor::Human)), false),
    ] {
        let mut queue = queue();
        let requester = scope("42", "requester");
        let id = finish(&mut queue, requester.clone());
        // Model legacy or inconsistent records directly: neither the public view
        // nor the final-answer gate may trust an Approved status on its own.
        let record = queue.records.back_mut().unwrap();
        let routing = record.routing.as_mut().unwrap();
        routing.finish_status = Some(status);
        routing.decision = decision.map(|(choice, actor)| FinishDecision {
            choice,
            comment: None,
            actor,
            revision: 1,
            decided_at_ms: 1,
        });
        routing.answer_actor = routing.decision.as_ref().map(|value| value.actor.clone());
        routing.answer_revision = u64::from(routing.decision.is_some());
        if routing.decision.is_some() {
            record.question.state = QuestionState::Answered;
            record.question.answered_at_ms = Some(1);
        }
        let before = snapshot(&queue);
        assert_eq!(view(&queue, &id).finish.unwrap().approved, permitted);
        if permitted {
            assert_eq!(queue.consume_finish(&requester).unwrap(), id);
            let consumed = view(&queue, &id).finish.unwrap();
            assert_eq!(consumed.status, Completed);
            assert!(!consumed.approved);
        } else {
            assert!(
                queue
                    .consume_finish(&requester)
                    .unwrap_err()
                    .starts_with("finish_approval_required:"),
                "status {status:?} must not grant completion permission"
            );
            assert_eq!(snapshot(&queue), before);
        }
    }
}

#[test]
fn every_agent_yes_is_rejected_without_changing_the_request_or_revision() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let id = finish(&mut queue, requester.clone());
    let mut other_provider = scope("42", "reviewer");
    other_provider.requester.provider = "another-provider".into();
    let mut other_connection = requester.clone();
    other_connection.connection += 1;
    let mut other_session = requester.clone();
    other_session.session_id = "another-room".into();

    for caller in [
        requester.clone(),
        scope("42", "reviewer"),
        scope("42", ""),
        scope("99", "reviewer"),
        other_provider,
        other_connection,
        other_session,
    ] {
        let before = snapshot(&queue);
        let error = queue
            .decide_finish_for(&id, FinishChoice::Yes, Some("ready"), Some(&caller))
            .unwrap_err();
        if requester.same_account(&caller) {
            assert!(error.starts_with("finish_human_approval_required:"));
        }
        assert_eq!(snapshot(&queue), before, "rejected caller: {caller:?}");
    }
}

#[test]
fn human_approval_retries_are_idempotent_and_only_the_requester_consumes_once() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let id = finish(&mut queue, requester.clone());
    let approved = queue
        .decide_finish_for(&id, FinishChoice::Yes, Some("Reviewed"), None)
        .unwrap();
    let before = snapshot(&queue);
    assert!(approved.finish.as_ref().unwrap().approved);
    assert_eq!(approved.answer_actor, Some(DecisionActor::Human));
    assert_eq!(
        queue
            .decide_finish_for(&id, FinishChoice::Yes, Some("Reviewed"), None)
            .unwrap(),
        approved
    );
    assert_eq!(snapshot(&queue), before);

    for caller in [scope("42", "reviewer"), scope("99", "requester")] {
        assert!(queue.consume_finish(&caller).is_err());
        assert_eq!(snapshot(&queue), before);
    }
    assert!(
        queue
            .decide_finish_for(&id, FinishChoice::Yes, Some("Reviewed"), Some(&requester))
            .unwrap_err()
            .starts_with("finish_human_approval_required:")
    );
    assert!(
        queue
            .decide_finish_for(&id, FinishChoice::No, Some("Changed"), None)
            .unwrap_err()
            .starts_with("finish_already_decided:")
    );
    assert_eq!(snapshot(&queue), before);
    assert_eq!(queue.consume_finish(&requester).unwrap(), id);
    let completed = snapshot(&queue);
    assert_eq!(completed.0, before.0 + 1);
    assert_eq!(
        view(&queue, &id).finish.unwrap().status,
        FinishStatus::Completed
    );
    assert!(queue.consume_finish(&requester).is_err());
    assert_eq!(snapshot(&queue), completed);
}

#[test]
fn human_and_agent_no_keep_their_real_actor_and_never_grant_permission() {
    let requester = scope("42", "requester");
    for caller in [None, Some(requester.clone()), Some(scope("42", "reviewer"))] {
        let mut queue = queue();
        let id = finish(&mut queue, requester.clone());
        let actor = caller
            .as_ref()
            .map_or(DecisionActor::Human, RequestScope::actor);
        let denied = queue
            .decide_finish_for(&id, FinishChoice::No, Some("Continue"), caller.as_ref())
            .unwrap();
        let decision = denied.finish.as_ref().unwrap();
        assert_eq!(decision.status, FinishStatus::Denied);
        assert!(!decision.approved);
        assert_eq!(decision.decision.as_ref().unwrap().actor, actor);
        assert_eq!(denied.answer_actor, Some(actor));
        let before = snapshot(&queue);
        assert_eq!(
            queue
                .decide_finish_for(&id, FinishChoice::No, Some("Continue"), caller.as_ref())
                .unwrap(),
            denied
        );
        assert!(queue.consume_finish(&requester).is_err());
        assert_eq!(snapshot(&queue), before);
    }
}

#[test]
fn ordinary_agent_answers_keep_their_source_without_approving_finish() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let reviewer = scope("42", "reviewer");
    let finish_id = finish(&mut queue, requester.clone());
    let pending_finish = view(&queue, &finish_id);
    let question_id = queue
        .ask_for(
            parse_ask(json!({"question": "Did the check complete?"})).unwrap(),
            Some(requester.clone()),
            QuestionKind::Question,
        )
        .unwrap()
        .0
        .id;
    queue
        .finish_for(
            &question_id,
            Some("Yes, the check completed."),
            Some(&reviewer),
        )
        .unwrap();
    let answered = view(&queue, &question_id);
    assert_eq!(answered.kind, QuestionKind::Question);
    assert_eq!(answered.question.state, QuestionState::Answered);
    assert_eq!(answered.answer_actor, Some(reviewer.actor()));
    assert_eq!(answered.answer_revision, 1);
    assert_eq!(answered.delivery, AnswerDelivery::Pending);
    assert!(answered.finish.is_none());
    assert_eq!(
        serde_json::to_value(&answered).unwrap()["answer_actor"]["origin"],
        "agent"
    );
    assert_eq!(view(&queue, &finish_id), pending_finish);
    let before = snapshot(&queue);
    assert!(queue.consume_finish(&requester).is_err());
    assert_eq!(snapshot(&queue), before);
}

#[test]
fn pending_observation_and_text_answers_cannot_authorize_completion() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let id = finish(&mut queue, requester.clone());
    let before = snapshot(&queue);
    // A waiter observing the same pending record again receives no implicit Yes.
    // Asynchronous timeout/drop behavior belongs in workflow-level tests.
    for _ in 0..3 {
        let pending = view(&queue, &id).finish.unwrap();
        assert_eq!(pending.status, FinishStatus::Pending);
        assert_eq!(pending.default_decision, FinishChoice::No);
        assert!(pending.decision.is_none());
        assert!(!pending.approved);
        assert!(queue.consume_finish(&requester).is_err());
        assert_eq!(snapshot(&queue), before);
    }
    for caller in [None, Some(&requester)] {
        assert!(
            queue
                .finish_for(&id, Some("Yes"), caller)
                .unwrap_err()
                .starts_with("finish_decision_required:")
        );
        assert_eq!(snapshot(&queue), before);
    }
}

#[test]
fn another_agents_completed_request_does_not_approve_a_pending_request() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let other = scope("42", "other-agent");
    let pending_id = finish(&mut queue, requester.clone());
    let other_id = finish(&mut queue, other.clone());
    queue
        .decide_finish_for(&other_id, FinishChoice::Yes, None, None)
        .unwrap();
    assert_eq!(queue.consume_finish(&other).unwrap(), other_id);
    let before = snapshot(&queue);
    assert!(queue.consume_finish(&requester).is_err());
    assert_eq!(snapshot(&queue), before);
    let pending = view(&queue, &pending_id).finish.unwrap();
    assert_eq!(pending.status, FinishStatus::Pending);
    assert!(pending.decision.is_none());
    assert!(!pending.approved);
}

#[test]
fn next_input_invalidates_all_remaining_pending_and_approved_threads() {
    let mut queue = queue();
    let first = scope("42", "first");
    let pending = scope("42", "pending");
    let second = scope("42", "second");
    let first_id = finish(&mut queue, first.clone());
    let pending_id = finish(&mut queue, pending.clone());
    let second_id = finish(&mut queue, second.clone());
    for id in [&first_id, &second_id] {
        queue
            .decide_finish_for(id, FinishChoice::Yes, None, None)
            .unwrap();
    }
    let untouched_pending = view(&queue, &pending_id);
    let untouched_second = view(&queue, &second_id);
    assert!(queue.invalidate_finishes(Some("first"), "new_user_input"));
    assert_eq!(view(&queue, &pending_id), untouched_pending);
    assert_eq!(view(&queue, &second_id), untouched_second);
    assert!(queue.consume_finish(&first).is_err());
    assert!(queue.invalidate_finishes(Some("next"), "new_user_input"));
    for (id, requester) in [
        (&first_id, &first),
        (&pending_id, &pending),
        (&second_id, &second),
    ] {
        let invalidated = view(&queue, id).finish.unwrap();
        assert_eq!(invalidated.status, FinishStatus::Invalidated);
        assert_eq!(
            invalidated.invalidation_reason.as_deref(),
            Some("new_user_input")
        );
        assert!(!invalidated.approved);
        assert!(queue.consume_finish(requester).is_err());
    }
    assert_eq!(
        view(&queue, &pending_id).question.state,
        QuestionState::Skipped
    );
    let before = snapshot(&queue);
    assert!(!queue.invalidate_finishes(Some("next"), "new_user_input"));
    assert_eq!(snapshot(&queue), before);
}

#[test]
fn connection_invalidation_prevents_reusing_a_human_approval() {
    for (reason, connection) in [
        ("disconnected", None),
        ("connection_changed", Some(("room".to_owned(), 8))),
        ("connection_changed", Some(("new-room".to_owned(), 7))),
    ] {
        let mut queue = queue();
        let requester = scope("42", "requester");
        let id = finish(&mut queue, requester.clone());
        queue
            .decide_finish_for(&id, FinishChoice::Yes, None, None)
            .unwrap();
        // Apply the connection lifecycle to a private Queue, without changing
        // the process-global connection shared by other unit tests.
        assert!(queue.invalidate_finishes(None, reason));
        for record in &mut queue.records {
            record.routing.as_mut().unwrap().scope_expired = true;
        }
        queue.connection = connection;
        let before = snapshot(&queue);
        assert!(
            queue
                .consume_finish(&requester)
                .unwrap_err()
                .starts_with("question_scope_expired:")
        );
        let invalidated = view(&queue, &id);
        assert_eq!(invalidated.delivery, AnswerDelivery::ScopeExpired);
        let finish = invalidated.finish.unwrap();
        assert_eq!(finish.status, FinishStatus::Invalidated);
        assert_eq!(finish.invalidation_reason.as_deref(), Some(reason));
        assert_eq!(finish.decision.unwrap().actor, DecisionActor::Human);
        assert!(!finish.approved);
        if let Some((session, generation)) = &queue.connection {
            let mut reconnected = requester.clone();
            reconnected.session_id = session.clone();
            reconnected.connection = *generation;
            assert!(queue.consume_finish(&reconnected).is_err());
        }
        assert_eq!(snapshot(&queue), before);
    }
}

#[test]
fn an_unbound_next_caller_cannot_create_or_consume_a_finish_request() {
    let mut queue = queue();
    let unbound = scope("42", "");
    let requester = scope("42", "requester");
    let id = finish(&mut queue, requester.clone());
    queue
        .decide_finish_for(&id, FinishChoice::Yes, None, None)
        .unwrap();
    let before = snapshot(&queue);
    for owner in [None, Some(unbound.clone())] {
        assert!(
            queue
                .ask_for(
                    parse_ask(json!({"question": "Finish without a verified thread?"})).unwrap(),
                    owner,
                    QuestionKind::Finish,
                )
                .unwrap_err()
                .starts_with("finish_thread_required:")
        );
        assert_eq!(snapshot(&queue), before);
    }
    assert!(queue.consume_finish(&unbound).is_err());
    assert_eq!(snapshot(&queue), before);
    assert_eq!(queue.consume_finish(&requester).unwrap(), id);
}

#[test]
fn finish_wait_exact_request_revision_and_scope_are_consumed_once() {
    let mut queue = queue();
    let requester = scope("42", "requester");
    let id = finish(&mut queue, requester.clone());
    let pending =
        FinishReceipt::from_view(queue.finish_view_for(&requester, &id).unwrap(), false).unwrap();
    assert_eq!(pending.request_id, id);
    assert_eq!(pending.status, FinishStatus::Pending);
    assert_eq!(pending.actor, None);
    assert!(!pending.human_permission());
    let approved = queue
        .decide_finish_for(&id, FinishChoice::Yes, Some("Reviewed"), None)
        .unwrap();
    let revision = approved.answer_revision;
    let before = snapshot(&queue);
    for (caller, request_id, expected_revision) in [
        (scope("42", "other-thread"), id.as_str(), revision),
        (requester.clone(), "question_unknown", revision),
        (requester.clone(), id.as_str(), revision + 1),
        (requester.clone(), id.as_str(), 0),
    ] {
        assert!(
            queue
                .consume_finish_revision(&caller, request_id, expected_revision)
                .is_err()
        );
        assert_eq!(snapshot(&queue), before);
    }
    let observed =
        FinishReceipt::from_view(queue.finish_view_for(&requester, &id).unwrap(), false).unwrap();
    assert_eq!(observed.choice, Some(FinishChoice::Yes));
    assert_eq!(observed.actor, Some(DecisionActor::Human));
    assert!(
        !observed.human_permission(),
        "reading Human Yes does not consume permission"
    );
    let consumed = queue
        .consume_finish_revision(&requester, &id, revision)
        .unwrap();
    let receipt = FinishReceipt::from_view(consumed, true).unwrap();
    assert_eq!(receipt.revision, Some(revision));
    assert!(receipt.human_permission());
    assert!(
        queue
            .consume_finish_revision(&requester, &id, revision)
            .is_err()
    );
    let reread =
        FinishReceipt::from_view(queue.finish_view_for(&requester, &id).unwrap(), false).unwrap();
    assert_eq!(reread.status, FinishStatus::Completed);
    assert!(
        !reread.human_permission(),
        "resuming or reading a completed request cannot replay Yes"
    );
}

#[test]
fn finish_wait_cancellation_preserves_the_real_actor_and_invalidates_permission() {
    for choice in [None, Some(FinishChoice::Yes), Some(FinishChoice::No)] {
        let mut queue = queue();
        let requester = scope("42", "requester");
        let id = finish(&mut queue, requester.clone());
        if let Some(choice) = choice {
            queue.decide_finish_for(&id, choice, None, None).unwrap();
        }
        let before = snapshot(&queue);
        assert!(
            queue
                .cancel_finish_for(&scope("42", "other-thread"), &id, "wait_cancelled")
                .is_err()
        );
        assert_eq!(snapshot(&queue), before);
        let cancelled = queue
            .cancel_finish_for(&requester, &id, "wait_cancelled")
            .unwrap();
        let receipt = FinishReceipt::from_view(cancelled, false).unwrap();
        assert!(!receipt.human_permission());
        assert_eq!(receipt.actor, choice.map(|_| DecisionActor::Human));
        assert_eq!(receipt.choice, choice);
        assert!(
            queue
                .consume_finish_revision(&requester, &id, receipt.revision.unwrap_or(0))
                .is_err()
        );
        assert_eq!(
            receipt.status,
            if choice == Some(FinishChoice::No) {
                FinishStatus::Denied
            } else {
                FinishStatus::Cancelled
            }
        );
    }
}
