//! Typed question ownership and finish decisions. Tool JSON never supplies an actor.
use super::{Question, QuestionState};
use crate::runtime::call_scope::CallScope;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuestionKind {
    #[default]
    Question,
    Finish,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishChoice {
    Yes,
    No,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct AgentActor {
    pub provider: String,
    pub subject: String,
    pub thread_id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "origin", rename_all = "snake_case")]
pub(crate) enum DecisionActor {
    Human,
    Agent { agent: AgentActor },
}

impl DecisionActor {
    fn same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Human, Self::Human) => true,
            (Self::Agent { agent: left }, Self::Agent { agent: right }) => {
                left.provider == right.provider
                    && left.subject == right.subject
                    && left.thread_id == right.thread_id
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FinishDecision {
    pub choice: FinishChoice,
    pub comment: Option<String>,
    pub actor: DecisionActor,
    pub revision: u64,
    pub decided_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FinishStatus {
    Pending,
    Approved,
    Denied,
    Cancelled,
    Invalidated,
    Completed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AnswerDelivery {
    NotAnswered,
    Pending,
    Delivered,
    ManualRead,
    ScopeExpired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FinishView {
    pub reason: String,
    pub default_decision: FinishChoice,
    pub decision: Option<FinishDecision>,
    pub status: FinishStatus,
    pub approved: bool,
    pub invalidation_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct QuestionView {
    #[serde(flatten)]
    pub question: Question,
    pub kind: QuestionKind,
    pub requester: Option<AgentActor>,
    pub finish: Option<FinishView>,
    pub answer_actor: Option<DecisionActor>,
    pub answer_revision: u64,
    pub automatic_delivery: bool,
    pub delivery: AnswerDelivery,
}

/// No Deserialize implementation: construct only from the accepted relay scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RequestScope {
    pub(super) connection: u64,
    pub(super) session_id: String,
    pub(super) requester: AgentActor,
}

impl RequestScope {
    pub(super) fn account_only(scope: &CallScope, session_id: &str) -> Result<Self, String> {
        let attribution = scope
            .attribution()
            .ok_or("question_auth_required: no authenticated account")?;
        Ok(Self {
            connection: scope.connection(),
            session_id: session_id.into(),
            requester: AgentActor {
                provider: attribution.account.provider.clone(),
                subject: attribution.account.subject.clone(),
                thread_id: String::new(),
                name: String::new(),
            },
        })
    }

    pub(super) fn has_thread(&self) -> bool {
        !self.requester.thread_id.is_empty()
    }

    pub(super) fn from_call(scope: &CallScope) -> Result<Self, String> {
        let attribution = scope
            .attribution()
            .ok_or("question_auth_required: authenticated controller metadata is unavailable")?;
        let thread = scope
            .thread()
            .ok_or("question_thread_required: automatic replies require a verified agent thread")?;
        let agent = attribution
            .agent
            .as_ref()
            .ok_or("question_thread_required: authenticated participant metadata is unavailable")?;
        if agent.id != thread.thread_id() {
            return Err("question_thread_required: participant and thread differ".into());
        }
        Ok(Self {
            connection: scope.connection(),
            session_id: thread.session_id().into(),
            requester: AgentActor {
                provider: attribution.account.provider.clone(),
                subject: attribution.account.subject.clone(),
                thread_id: thread.thread_id().into(),
                name: agent.name.clone(),
            },
        })
    }

    pub(super) fn current() -> Result<Self, String> {
        let scope = crate::runtime::call_scope::current()
            .ok_or("question_auth_required: no accepted relay call scope")?;
        Self::from_call(&scope)
    }

    pub(super) fn same_account(&self, other: &Self) -> bool {
        self.connection == other.connection
            && self.session_id == other.session_id
            && self.requester.provider == other.requester.provider
            && self.requester.subject == other.requester.subject
    }

    pub(super) fn same_thread(&self, other: &Self) -> bool {
        self.same_account(other) && self.requester.thread_id == other.requester.thread_id
    }

    pub(super) fn actor(&self) -> DecisionActor {
        DecisionActor::Agent {
            agent: self.requester.clone(),
        }
    }

    pub(super) fn valid_connection(&self, connection: Option<(&str, u64)>) -> bool {
        connection
            .is_some_and(|(id, generation)| self.session_id == id && self.connection == generation)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Routing {
    pub(super) scope: RequestScope,
    pub(super) kind: QuestionKind,
    pub(super) finish_status: Option<FinishStatus>,
    pub(super) decision: Option<FinishDecision>,
    pub(super) invalidation_reason: Option<String>,
    pub(super) answer_actor: Option<DecisionActor>,
    pub(super) answer_revision: u64,
    pub(super) answer_delivered: bool,
    pub(super) scope_expired: bool,
}

impl Routing {
    pub(super) fn bytes(&self) -> usize {
        self.scope.session_id.len()
            + self.scope.requester.provider.len()
            + self.scope.requester.subject.len()
            + self.scope.requester.thread_id.len()
            + self.scope.requester.name.len()
            + self.invalidation_reason.as_ref().map_or(0, String::len)
            + self.decision.as_ref().map_or(0, |decision| {
                decision.comment.as_ref().map_or(0, String::len)
                    + serde_json::to_vec(&decision.actor).map_or(0, |value| value.len())
            })
            + self.answer_actor.as_ref().map_or(0, |actor| {
                serde_json::to_vec(actor).map_or(0, |value| value.len())
            })
            + 512
    }

    pub(super) fn new(scope: RequestScope, kind: QuestionKind) -> Self {
        Self {
            scope,
            kind,
            finish_status: (kind == QuestionKind::Finish).then_some(FinishStatus::Pending),
            decision: None,
            invalidation_reason: None,
            answer_actor: None,
            answer_revision: 0,
            answer_delivered: false,
            scope_expired: false,
        }
    }

    pub(super) fn can_decide(
        &self,
        caller: Option<&RequestScope>,
        choice: FinishChoice,
    ) -> Result<(), String> {
        if self.kind != QuestionKind::Finish {
            return Err("question_kind_mismatch: this is not a finish request".into());
        }
        if let Some(caller) = caller {
            if !self.scope.same_account(caller) {
                return Err("question_access_denied: another account or connection".into());
            }
            if choice == FinishChoice::Yes && self.scope.same_thread(caller) {
                return Err(
                    "finish_self_approval_forbidden: the requesting agent cannot approve itself"
                        .into(),
                );
            }
        }
        Ok(())
    }

    pub(super) fn view(&self, question: Question) -> QuestionView {
        let finish = self.finish_status.map(|status| FinishView {
            reason: question.question.clone(),
            default_decision: FinishChoice::No,
            decision: self.decision.clone(),
            status,
            approved: status == FinishStatus::Approved,
            invalidation_reason: self.invalidation_reason.clone(),
        });
        QuestionView {
            question,
            kind: self.kind,
            requester: self
                .scope
                .has_thread()
                .then(|| self.scope.requester.clone()),
            finish,
            answer_actor: self.answer_actor.clone(),
            answer_revision: self.answer_revision,
            automatic_delivery: self.scope.has_thread(),
            delivery: if self.answer_revision == 0 {
                AnswerDelivery::NotAnswered
            } else if self.scope_expired {
                AnswerDelivery::ScopeExpired
            } else if !self.scope.has_thread() {
                AnswerDelivery::ManualRead
            } else if self.answer_delivered {
                AnswerDelivery::Delivered
            } else {
                AnswerDelivery::Pending
            },
        }
    }

    pub(super) fn invalidate(&mut self, question: &mut Question, reason: &str) -> bool {
        if !matches!(
            self.finish_status,
            Some(FinishStatus::Pending | FinishStatus::Approved)
        ) {
            return false;
        }
        self.finish_status = Some(FinishStatus::Invalidated);
        self.invalidation_reason = Some(reason.into());
        if question.state == QuestionState::Pending {
            question.state = QuestionState::Skipped;
        }
        true
    }
}

impl super::Queue {
    pub(super) fn validate_scope(&self, scope: &RequestScope) -> Result<(), String> {
        if scope.valid_connection(
            self.connection
                .as_ref()
                .map(|(id, generation)| (id.as_str(), *generation)),
        ) {
            Ok(())
        } else {
            Err("question_scope_expired: request belongs to another connection".into())
        }
    }

    pub(super) fn decide_finish_for(
        &mut self,
        id: &str,
        choice: FinishChoice,
        comment: Option<&str>,
        caller: Option<&RequestScope>,
    ) -> Result<QuestionView, String> {
        super::identifier(id)?;
        if let Some(comment) = comment {
            super::validate_text(comment, super::MAX_ANSWER_BYTES, "comment")?;
        }
        if let Some(caller) = caller {
            self.validate_scope(caller)?;
        }
        let record = self
            .records
            .iter()
            .find(|r| r.question.id == id)
            .ok_or("question is no longer retained")?;
        let routing = record
            .routing
            .as_ref()
            .ok_or("finish_decision_required: not a finish request")?;
        self.validate_scope(&routing.scope)?;
        routing.can_decide(caller, choice)?;
        let actor = caller.map_or(DecisionActor::Human, RequestScope::actor);
        if let Some(previous) = &routing.decision {
            if previous.choice == choice
                && previous.comment.as_deref() == comment
                && previous.actor.same_identity(&actor)
            {
                return Ok(record.view());
            }
            return Err("finish_already_decided: the recorded decision is immutable".into());
        }
        if routing.finish_status != Some(FinishStatus::Pending) {
            return Err("finish_not_pending: this request can no longer be decided".into());
        }
        self.reserve(
            comment.map_or(0, str::len).saturating_mul(2) + 2048,
            0,
            Some(id),
        )?;
        let record = self
            .records
            .iter_mut()
            .find(|r| r.question.id == id)
            .expect("reservation preserves its protected request");
        let routing = record.routing.as_mut().expect("validated finish request");
        let revision = routing.answer_revision.saturating_add(1);
        routing.decision = Some(FinishDecision {
            choice,
            comment: comment.map(str::to_owned),
            actor: actor.clone(),
            revision,
            decided_at_ms: super::now_ms(),
        });
        routing.finish_status = Some(match choice {
            FinishChoice::Yes => FinishStatus::Approved,
            FinishChoice::No => FinishStatus::Denied,
        });
        routing.answer_actor = Some(actor);
        routing.answer_revision = revision;
        routing.answer_delivered = false;
        record.question.state = QuestionState::Answered;
        record.question.answer = comment.map(str::to_owned);
        record.question.answered_at_ms = Some(super::now_ms());
        self.revision = self.revision.saturating_add(1);
        Ok(record.view())
    }

    pub(super) fn invalidate_finishes(&mut self, target: Option<&str>, reason: &str) -> bool {
        let mut changed = false;
        for record in &mut self.records {
            if let Some(routing) = &mut record.routing
                && target.is_none_or(|target| {
                    target == "next" || target == routing.scope.requester.thread_id
                })
            {
                changed |= routing.invalidate(&mut record.question, reason);
            }
        }
        if changed {
            self.revision = self.revision.saturating_add(1);
        }
        changed
    }

    pub(super) fn consume_finish(&mut self, scope: &RequestScope) -> Result<String, String> {
        self.validate_scope(scope)?;
        let record = self.records.iter_mut().find(|record| {
            record.routing.as_ref().is_some_and(|routing| {
                routing.scope.same_thread(scope)
                    && routing.finish_status == Some(FinishStatus::Approved)
            })
        }).ok_or("finish_approval_required: request_finish must be approved before an explicit final answer")?;
        record
            .routing
            .as_mut()
            .expect("matched scope")
            .finish_status = Some(FinishStatus::Completed);
        self.revision = self.revision.saturating_add(1);
        Ok(record.question.id.clone())
    }
}

pub(super) fn legacy_view(question: Question) -> QuestionView {
    let delivery = if question.state == QuestionState::Pending {
        AnswerDelivery::NotAnswered
    } else {
        AnswerDelivery::ManualRead
    };
    QuestionView {
        question,
        kind: QuestionKind::Question,
        requester: None,
        finish: None,
        answer_actor: None,
        answer_revision: 0,
        automatic_delivery: false,
        delivery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::interface::questions::{Queue, parse_ask};
    use serde_json::json;

    fn scope(subject: &str, thread: &str) -> RequestScope {
        RequestScope {
            connection: 7,
            session_id: "room".into(),
            requester: AgentActor {
                provider: "github".into(),
                subject: subject.into(),
                thread_id: thread.into(),
                name: "display only".into(),
            },
        }
    }

    fn queue() -> Queue {
        Queue {
            connection: Some(("room".into(), 7)),
            ..Queue::default()
        }
    }

    fn finish(queue: &mut Queue, owner: RequestScope) -> String {
        queue
            .ask_for(
                parse_ask(
                    json!({"question":"The requested change is verified.","options":["No","Yes"]}),
                )
                .unwrap(),
                Some(owner),
                QuestionKind::Finish,
            )
            .unwrap()
            .0
            .id
    }

    #[test]
    fn finish_defaults_to_no_without_claiming_a_user_decision() {
        let mut queue = queue();
        let id = finish(&mut queue, scope("42", "requester"));
        let view = queue
            .records
            .iter()
            .find(|record| record.question.id == id)
            .unwrap()
            .view();
        let finish = view.finish.unwrap();
        assert_eq!(finish.default_decision, FinishChoice::No);
        assert_eq!(finish.status, FinishStatus::Pending);
        assert!(finish.decision.is_none());
        assert!(!finish.approved);
        assert_eq!(view.delivery, AnswerDelivery::NotAnswered);
        assert!(queue.consume_finish(&scope("42", "requester")).is_err());
        assert!(queue.finish(&id, Some("yes")).is_err());
    }

    #[test]
    fn an_unbound_account_keeps_manual_answers_private_without_next_caller_delivery() {
        let mut queue = queue();
        let mut manual = scope("42", "");
        manual.requester.name.clear();
        let question = queue
            .ask_for(
                parse_ask(json!({"question":"Private account choice"})).unwrap(),
                Some(manual.clone()),
                QuestionKind::Question,
            )
            .unwrap()
            .0;
        let record = queue
            .records
            .iter()
            .find(|record| record.question.id == question.id)
            .unwrap();
        assert!(record.visible_to(Some(&scope("42", "reader")), false));
        assert!(!record.visible_to(Some(&scope("43", "reader")), false));
        assert!(!record.visible_to(None, false));
        assert!(!record.view().automatic_delivery);
        assert!(record.view().requester.is_none());
        queue.finish(&question.id, Some("Keep it local")).unwrap();
        assert_eq!(queue.records[0].view().delivery, AnswerDelivery::ManualRead);
        assert!(
            queue
                .ask_for(
                    parse_ask(json!({"question":"Can I finish?"})).unwrap(),
                    Some(manual),
                    QuestionKind::Finish
                )
                .is_err()
        );
    }

    #[test]
    fn a_new_human_question_answer_invalidates_the_asking_threads_old_approval() {
        let mut queue = queue();
        let owner = scope("42", "requester");
        let question = queue
            .ask_for(
                parse_ask(json!({"question":"One remaining detail?"})).unwrap(),
                Some(owner.clone()),
                QuestionKind::Question,
            )
            .unwrap()
            .0;
        let finish_id = finish(&mut queue, owner.clone());
        queue
            .decide_finish_for(&finish_id, FinishChoice::Yes, None, None)
            .unwrap();
        queue
            .finish(&question.id, Some("There is one more required change."))
            .unwrap();
        assert!(queue.consume_finish(&owner).is_err());
        let view = queue
            .records
            .iter()
            .find(|record| record.question.id == finish_id)
            .unwrap()
            .view();
        assert_eq!(view.finish.unwrap().status, FinishStatus::Invalidated);
    }

    #[test]
    fn only_another_verified_agent_in_the_same_account_can_approve() {
        let mut queue = queue();
        let requester = scope("42", "requester");
        let id = finish(&mut queue, requester.clone());
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, None, Some(&requester))
                .unwrap_err()
                .contains("self_approval")
        );
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, None, Some(&scope("7", "reviewer")))
                .is_err()
        );
        let mut foreign_connection = scope("42", "reviewer");
        foreign_connection.connection = 8;
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, None, Some(&foreign_connection))
                .is_err()
        );

        let reviewer = scope("42", "reviewer");
        let result = queue
            .decide_finish_for(
                &id,
                FinishChoice::Yes,
                Some("Verified the regression."),
                Some(&reviewer),
            )
            .unwrap();
        let decision = result.finish.unwrap().decision.unwrap();
        assert_eq!(decision.actor, reviewer.actor());
        assert_eq!(decision.choice, FinishChoice::Yes);
        assert_eq!(result.delivery, AnswerDelivery::Pending);
        assert!(queue.consume_finish(&reviewer).is_err());
        assert_eq!(queue.consume_finish(&requester).unwrap(), id);
        assert!(queue.consume_finish(&requester).is_err());
    }

    #[test]
    fn human_decision_and_agent_denial_keep_their_real_actors() {
        let mut queue = queue();
        let id = finish(&mut queue, scope("42", "first"));
        let yes = queue
            .decide_finish_for(&id, FinishChoice::Yes, None, None)
            .unwrap();
        assert_eq!(
            yes.finish.unwrap().decision.unwrap().actor,
            DecisionActor::Human
        );

        let requester = scope("42", "second");
        let id = finish(&mut queue, requester.clone());
        let no = queue
            .decide_finish_for(
                &id,
                FinishChoice::No,
                Some("More work remains."),
                Some(&requester),
            )
            .unwrap();
        let finish = no.finish.unwrap();
        assert_eq!(finish.status, FinishStatus::Denied);
        assert!(!finish.approved);
        assert_eq!(finish.decision.unwrap().actor, requester.actor());
    }

    #[test]
    fn retries_keep_the_same_decision_and_cannot_rewrite_an_actor() {
        let mut queue = queue();
        let owner = scope("42", "requester");
        let id = finish(&mut queue, owner.clone());
        assert_eq!(finish(&mut queue, owner), id);
        let reviewer = scope("42", "reviewer");
        let first = queue
            .decide_finish_for(&id, FinishChoice::Yes, Some("checked"), Some(&reviewer))
            .unwrap();
        let revision = queue.revision;
        assert_eq!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, Some("checked"), Some(&reviewer))
                .unwrap(),
            first
        );
        assert_eq!(queue.revision, revision);
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, Some("checked"), None)
                .is_err()
        );
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::No, Some("changed"), Some(&reviewer))
                .is_err()
        );
    }

    #[test]
    fn new_user_input_invalidates_approval_only_for_the_relevant_thread() {
        let mut queue = queue();
        let first = scope("42", "first");
        let second = scope("42", "second");
        let one = finish(&mut queue, first.clone());
        let two = finish(&mut queue, second.clone());
        queue
            .decide_finish_for(&one, FinishChoice::Yes, None, None)
            .unwrap();
        queue
            .decide_finish_for(&two, FinishChoice::Yes, None, None)
            .unwrap();
        assert!(queue.invalidate_finishes(Some("first"), "new_user_input"));
        assert!(queue.consume_finish(&first).is_err());
        assert_eq!(queue.consume_finish(&second).unwrap(), two);
        let view = queue
            .records
            .iter()
            .find(|r| r.question.id == one)
            .unwrap()
            .view();
        let invalidated = view.finish.unwrap();
        assert_eq!(invalidated.status, FinishStatus::Invalidated);
        assert_eq!(invalidated.decision.unwrap().actor, DecisionActor::Human);
        assert!(!invalidated.approved);
    }

    #[test]
    fn next_caller_input_and_new_connections_cannot_reuse_old_yes() {
        let mut queue = queue();
        let owner = scope("42", "requester");
        let id = finish(&mut queue, owner.clone());
        queue
            .decide_finish_for(&id, FinishChoice::Yes, None, None)
            .unwrap();
        queue.invalidate_finishes(Some("next"), "new_user_input");
        assert!(queue.consume_finish(&owner).is_err());
        queue.connection = Some(("replacement-room".into(), 8));
        assert!(
            queue
                .decide_finish_for(&id, FinishChoice::Yes, None, None)
                .is_err()
        );
        assert!(queue.consume_finish(&owner).is_err());
    }

    #[test]
    fn scoped_answers_are_not_reclaimed_before_delivery_or_visible_to_another_account() {
        let mut queue = queue();
        let owner = scope("42", "requester");
        let question = queue
            .ask_for(
                parse_ask(json!({"question":"Which file?"})).unwrap(),
                Some(owner.clone()),
                QuestionKind::Question,
            )
            .unwrap()
            .0;
        let reviewer = scope("42", "reviewer");
        queue
            .finish_for(&question.id, Some("src/main.rs"), Some(&reviewer))
            .unwrap();
        let record = queue.records.front().unwrap();
        assert!(!record.reclaimable());
        assert!(record.visible_to(Some(&reviewer), false));
        assert!(!record.visible_to(Some(&scope("7", "reviewer")), false));
        assert!(!record.visible_to(None, false));
        assert_eq!(record.view().answer_actor, Some(reviewer.actor()));
        assert_eq!(record.view().delivery, AnswerDelivery::Pending);
        queue
            .records
            .front_mut()
            .unwrap()
            .routing
            .as_mut()
            .unwrap()
            .answer_delivered = true;
        assert!(queue.records.front().unwrap().reclaimable());
    }

    #[test]
    fn idempotency_keys_are_scoped_to_the_verified_requester() {
        let mut queue = queue();
        let input =
            parse_ask(json!({"question":"Which branch?","idempotency_key":"same_key"})).unwrap();
        let one = queue
            .ask_for(
                input.clone(),
                Some(scope("42", "first")),
                QuestionKind::Question,
            )
            .unwrap()
            .0
            .id;
        let two = queue
            .ask_for(
                input.clone(),
                Some(scope("42", "second")),
                QuestionKind::Question,
            )
            .unwrap()
            .0
            .id;
        let other = queue
            .ask_for(
                input.clone(),
                Some(scope("7", "first")),
                QuestionKind::Question,
            )
            .unwrap()
            .0
            .id;
        assert_ne!(one, two);
        assert_ne!(one, other);
        assert_eq!(
            queue
                .ask_for(input, Some(scope("42", "first")), QuestionKind::Question)
                .unwrap()
                .0
                .id,
            one
        );
    }
}
