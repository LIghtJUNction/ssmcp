//! Bounded questions, authenticated answers, and explicit finish decisions.
mod delivery;
mod requests;
mod workflow;
pub(crate) use delivery::attach as attach_answers;
pub(crate) use requests::{
    AgentActor, AnswerDelivery, DecisionActor, FinishChoice, FinishDecision, FinishStatus,
    FinishView, QuestionKind, QuestionView,
};
use requests::{RequestScope, Routing};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_PENDING: usize = 32;
const MAX_RETAINED: usize = 64;
const MAX_TOTAL_BYTES: usize = 256 * 1024;
const MAX_QUESTION_BYTES: usize = 8 * 1024;
const MAX_ANSWER_BYTES: usize = 16 * 1024;
const MAX_PAGE_BYTES: usize = 64 * 1024;

static QUESTIONS: OnceLock<Mutex<Queue>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuestionState {
    Pending,
    Answered,
    Skipped,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Question {
    pub id: String,
    pub title: Option<String>,
    pub question: String,
    pub options: Vec<String>,
    pub state: QuestionState,
    pub answer: Option<String>,
    pub created_at_ms: u64,
    pub answered_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct AskRequest {
    question: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    state: Option<QuestionState>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Clone)]
struct Record {
    question: Question,
    idempotency_key: Option<String>,
    routing: Option<Routing>,
}

impl Record {
    fn bytes(&self) -> usize {
        let question = &self.question;
        question.id.len()
            + question.question.len()
            + question.title.as_ref().map_or(0, String::len)
            + question.options.iter().map(String::len).sum::<usize>()
            + question.answer.as_ref().map_or(0, String::len)
            + self.idempotency_key.as_ref().map_or(0, String::len)
            + self.routing.as_ref().map_or(0, Routing::bytes)
            + 256
    }

    fn view(&self) -> QuestionView {
        self.routing.as_ref().map_or_else(
            || requests::legacy_view(self.question.clone()),
            |routing| routing.view(self.question.clone()),
        )
    }

    fn visible_to(&self, caller: Option<&RequestScope>, local: bool) -> bool {
        local
            || self.routing.as_ref().map_or(caller.is_none(), |routing| {
                caller.is_some_and(|caller| routing.scope.same_account(caller))
            })
    }

    fn reclaimable(&self) -> bool {
        self.question.state != QuestionState::Pending
            && self.routing.as_ref().is_none_or(|routing| {
                !routing.scope.has_thread()
                    || routing.answer_revision == 0
                    || routing.answer_delivered
            })
    }
}

#[derive(Default)]
struct Queue {
    records: VecDeque<Record>,
    revision: u64,
    connection: Option<(String, u64)>,
    finish_waiting: std::collections::HashMap<String, usize>,
}

fn state() -> &'static Mutex<Queue> {
    QUESTIONS.get_or_init(|| Mutex::new(Queue::default()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|_| "cannot create a question identifier")?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("question_{suffix}"))
}

fn validate_text(text: &str, limit: usize, name: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err(format!("{name} must contain text"));
    }
    if text.len() > limit {
        return Err(format!("{name} exceeds {limit} UTF-8 bytes"));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(format!("{name} contains unsupported control characters"));
    }
    Ok(())
}

fn identifier(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err("question identifiers and idempotency keys must be 1..=128 ASCII letters, digits, underscores or hyphens".into());
    }
    Ok(())
}

fn object_without_nulls(arguments: &Value) -> Result<(), String> {
    let object = arguments.as_object().ok_or("expected an argument object")?;
    if object.values().any(Value::is_null) {
        return Err("omit optional question fields instead of passing null".into());
    }
    Ok(())
}

fn parse_ask(arguments: Value) -> Result<AskRequest, String> {
    object_without_nulls(&arguments)?;
    let request: AskRequest =
        serde_json::from_value(arguments).map_err(|_| "invalid question arguments")?;
    validate_text(&request.question, MAX_QUESTION_BYTES, "question")?;
    if let Some(title) = &request.title {
        validate_text(title, 128, "question title")?;
        if title.contains(['\n', '\t']) {
            return Err("question title must be one line".into());
        }
    }
    if request.options.len() > 8 {
        return Err("a question accepts at most 8 suggested answers".into());
    }
    let mut labels = std::collections::BTreeSet::new();
    for option in &request.options {
        validate_text(option, 256, "suggested answer")?;
        if option.contains(['\n', '\t']) {
            return Err("suggested answers must each be one line".into());
        }
        if !labels.insert(option.trim()) {
            return Err("suggested answers must be distinct".into());
        }
    }
    if let Some(key) = &request.idempotency_key {
        identifier(key)?;
    }
    Ok(request)
}

fn parse_read(arguments: Value) -> Result<ReadRequest, String> {
    object_without_nulls(&arguments)?;
    let request: ReadRequest =
        serde_json::from_value(arguments).map_err(|_| "invalid question read arguments")?;
    if let Some(id) = &request.id {
        identifier(id)?;
        if request.offset.is_some() || request.limit.is_some() || request.state.is_some() {
            return Err("read one question by id, or filter and page the question list".into());
        }
    }
    if request
        .limit
        .is_some_and(|limit| !(1..=32).contains(&limit))
    {
        return Err("question page limit must be in 1..=32".into());
    }
    Ok(request)
}

impl Queue {
    fn pending_count(&self) -> usize {
        self.records
            .iter()
            .filter(|record| record.question.state == QuestionState::Pending)
            .count()
    }

    fn bytes(&self) -> usize {
        self.records.iter().map(Record::bytes).sum()
    }

    // Reclaim completed history only after proving pending questions still fit.
    fn reserve(&mut self, extra: usize, slots: usize, protect: Option<&str>) -> Result<(), String> {
        let protected = self
            .records
            .iter()
            .filter(|record| !record.reclaimable() || protect == Some(record.question.id.as_str()))
            .map(Record::bytes)
            .sum::<usize>();
        if protected.saturating_add(extra) > MAX_TOTAL_BYTES {
            return Err(
                "question queue byte limit reached; answer or skip an existing question".into(),
            );
        }
        while self.bytes().saturating_add(extra) > MAX_TOTAL_BYTES
            || self.records.len().saturating_add(slots) > MAX_RETAINED
        {
            let index = self
                .records
                .iter()
                .position(|record| {
                    record.reclaimable() && protect != Some(record.question.id.as_str())
                })
                .ok_or("question queue capacity reached")?;
            self.records.remove(index);
        }
        Ok(())
    }

    fn ask(&mut self, request: AskRequest) -> Result<(Question, bool), String> {
        self.ask_for(request, None, QuestionKind::Question)
    }

    fn ask_for(
        &mut self,
        request: AskRequest,
        scope: Option<RequestScope>,
        kind: QuestionKind,
    ) -> Result<(Question, bool), String> {
        if let Some(scope) = &scope {
            self.validate_scope(scope)?;
            if kind == QuestionKind::Finish && !scope.has_thread() {
                return Err(
                    "finish_thread_required: an account alone cannot select an asking agent".into(),
                );
            }
        } else if kind == QuestionKind::Finish {
            return Err(
                "finish_thread_required: finish requests need a verified agent thread".into(),
            );
        }
        if let Some(key) = &request.idempotency_key
            && let Some(record) = self.records.iter().find(|record| {
                record.idempotency_key.as_ref() == Some(key)
                    && match (&record.routing, &scope) {
                        (None, None) => true,
                        (Some(routing), Some(scope)) => routing.scope.same_thread(scope),
                        _ => false,
                    }
            })
        {
            let question = &record.question;
            if question.question != request.question
                || question.title != request.title
                || question.options != request.options
                || record
                    .routing
                    .as_ref()
                    .map_or(QuestionKind::Question, |r| r.kind)
                    != kind
            {
                return Err("idempotency key already belongs to a different question".into());
            }
            return Ok((question.clone(), false));
        }
        if kind == QuestionKind::Finish
            && let Some(scope) = &scope
            && let Some(record) = self.records.iter().find(|record| {
                record.routing.as_ref().is_some_and(|routing| {
                    routing.scope.same_thread(scope)
                        && matches!(
                            routing.finish_status,
                            Some(FinishStatus::Pending | FinishStatus::Approved)
                        )
                })
            })
        {
            if record.question.question == request.question {
                return Ok((record.question.clone(), false));
            }
            return Err("finish_already_pending: resume or cancel the existing request".into());
        }
        if self.pending_count() >= MAX_PENDING {
            return Err(
                "32 questions are already pending; answer or skip one before adding more".into(),
            );
        }
        let mut id = random_id()?;
        while self.records.iter().any(|record| record.question.id == id) {
            id = random_id()?;
        }
        let record = Record {
            question: Question {
                id,
                title: request.title,
                question: request.question,
                options: request.options,
                state: QuestionState::Pending,
                answer: None,
                created_at_ms: now_ms(),
                answered_at_ms: None,
            },
            idempotency_key: request.idempotency_key,
            routing: scope.map(|scope| Routing::new(scope, kind)),
        };
        self.reserve(record.bytes(), 1, None)?;
        let question = record.question.clone();
        self.records.push_back(record);
        self.revision = self.revision.saturating_add(1);
        Ok((question, true))
    }

    fn finish(&mut self, id: &str, answer: Option<&str>) -> Result<Question, String> {
        self.finish_for(id, answer, None)
    }

    fn finish_for(
        &mut self,
        id: &str,
        answer: Option<&str>,
        caller: Option<&RequestScope>,
    ) -> Result<Question, String> {
        identifier(id)?;
        if let Some(caller) = caller {
            self.validate_scope(caller)?;
        }
        if let Some(text) = answer {
            validate_text(text, MAX_ANSWER_BYTES, "answer")?;
        }
        let record = self
            .records
            .iter()
            .find(|record| record.question.id == id)
            .ok_or("question is no longer retained")?;
        if record
            .routing
            .as_ref()
            .is_some_and(|routing| routing.kind == QuestionKind::Finish)
        {
            return Err(
                "finish_decision_required: use an explicit yes/no decision, not answer text".into(),
            );
        }
        if let Some(caller) = caller
            && record
                .routing
                .as_ref()
                .is_none_or(|routing| !routing.scope.same_account(caller))
        {
            return Err(
                "question_access_denied: another account, connection, or unowned question".into(),
            );
        }
        let expected_state = if answer.is_some() {
            QuestionState::Answered
        } else {
            QuestionState::Skipped
        };
        if record.question.state != QuestionState::Pending {
            if record.question.state == expected_state
                && record.question.answer.as_deref() == answer
            {
                return Ok(record.question.clone());
            }
            return Err("question has already been answered or skipped".into());
        }
        self.reserve(
            answer.map_or(0, str::len) + usize::from(record.routing.is_some()) * 1024,
            0,
            Some(id),
        )?;
        let record = self
            .records
            .iter_mut()
            .find(|record| record.question.id == id)
            .expect("capacity reclamation preserves the question being answered");
        record.question.state = expected_state;
        record.question.answer = answer.map(str::to_owned);
        record.question.answered_at_ms = Some(now_ms());
        if let Some(routing) = &mut record.routing {
            routing.answer_actor = Some(caller.map_or(DecisionActor::Human, RequestScope::actor));
            routing.answer_revision = routing.answer_revision.saturating_add(1);
            routing.answer_delivered = false;
        }
        let question = record.question.clone();
        let target = record
            .routing
            .as_ref()
            .map(|routing| routing.scope.requester.thread_id.clone())
            .filter(|id| !id.is_empty());
        self.revision = self.revision.saturating_add(1);
        if caller.is_none() {
            // A human answer supplies new information to its asking thread.
            // It is not silently covered by an older completion approval.
            self.invalidate_finishes(target.as_deref(), "new_human_question_answer");
        }
        Ok(question)
    }

    #[cfg(test)]
    fn snapshot(&self) -> Value {
        json!({
            "schema_version": 1,
            "revision": self.revision,
            "pending_count": self.pending_count(),
            "questions": self.records.iter().map(Record::view).collect::<Vec<_>>(),
        })
    }

    #[cfg(test)]
    fn read(&self, request: ReadRequest) -> Result<Value, String> {
        self.read_for(request, None, true)
    }

    fn read_for(
        &self,
        request: ReadRequest,
        caller: Option<&RequestScope>,
        local: bool,
    ) -> Result<Value, String> {
        if let Some(id) = request.id {
            let question = &self
                .records
                .iter()
                .find(|record| record.question.id == id && record.visible_to(caller, local))
                .ok_or("question is no longer retained")?
                .view();
            return Ok(json!({"schema_version":1,"revision":self.revision,"question":question}));
        }
        let records = self
            .records
            .iter()
            .filter(|record| record.visible_to(caller, local))
            .filter(|record| {
                request
                    .state
                    .is_none_or(|state| state == record.question.state)
            })
            .collect::<Vec<_>>();
        let pending_count = records
            .iter()
            .filter(|record| record.question.state == QuestionState::Pending)
            .count();
        let offset = request.offset.unwrap_or(0);
        if offset > records.len() {
            return Err("question page offset exceeds the filtered list length".into());
        }
        let mut page = Vec::new();
        let mut bytes: usize = 512;
        for record in records.iter().skip(offset).take(request.limit.unwrap_or(8)) {
            let view = record.view();
            let size = serde_json::to_vec(&view)
                .map_err(|_| "cannot serialize question")?
                .len();
            if bytes.saturating_add(size) > MAX_PAGE_BYTES {
                break;
            }
            bytes += size;
            page.push(view);
        }
        let next_offset = offset + page.len();
        Ok(json!({
            "schema_version":1,
            "revision":self.revision,
            "pending_count":pending_count,
            "questions":page,
            "total":records.len(),
            "offset":offset,
            "next_offset":(next_offset < records.len()).then_some(next_offset),
        }))
    }
}

fn caller_scope() -> Result<Option<RequestScope>, String> {
    let Some(scope) = crate::runtime::call_scope::current() else {
        return Ok(None);
    };
    if scope.attribution().is_none() {
        return Ok(None);
    }
    let queue = state().lock().unwrap_or_else(|error| error.into_inner());
    let (id, _) = queue
        .connection
        .as_ref()
        .ok_or("question_scope_expired: no current connection")?;
    let caller = if scope.thread().is_some() {
        RequestScope::from_call(&scope)?
    } else {
        RequestScope::account_only(&scope, id)?
    };
    queue.validate_scope(&caller)?;
    Ok(Some(caller))
}

pub(crate) fn snapshot() -> Value {
    let local = crate::runtime::call_scope::current().is_none();
    let caller = match caller_scope() {
        Ok(caller) => caller,
        Err(error) => {
            return json!({"schema_version":1,"questions":[],"pending_count":0,"error":error});
        }
    };
    let queue = state().lock().unwrap_or_else(|error| error.into_inner());
    let visible: Vec<_> = queue
        .records
        .iter()
        .filter(|record| record.visible_to(caller.as_ref(), local))
        .collect();
    json!({"schema_version":1,"revision":queue.revision,
        "pending_count":visible.iter().filter(|r| r.question.state == QuestionState::Pending).count(),
        "questions":visible.iter().map(|r|r.view()).collect::<Vec<_>>()})
}

pub(crate) fn pending_count() -> usize {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .pending_count()
}

pub(crate) fn pending() -> Vec<Question> {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .records
        .iter()
        .filter(|record| record.question.state == QuestionState::Pending)
        .map(|record| record.question.clone())
        .collect()
}

pub(crate) fn get(id: &str) -> Option<Question> {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .records
        .iter()
        .find(|record| record.question.id == id)
        .map(|record| record.question.clone())
}

pub(crate) fn pending_views() -> Vec<QuestionView> {
    retained_views()
        .into_iter()
        .filter(|view| view.question.state == QuestionState::Pending)
        .collect()
}

/// Local UI projection; native reads apply the verified caller filter separately.
pub(crate) fn retained_views() -> Vec<QuestionView> {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .records
        .iter()
        .map(Record::view)
        .collect()
}

pub(crate) fn get_view(id: &str) -> Option<QuestionView> {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .records
        .iter()
        .find(|record| record.question.id == id)
        .map(Record::view)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FinishWaitSnapshot {
    pub connection_id: Option<String>,
    pub active: Vec<FinishWaitEntry>,
}

#[derive(Clone, Debug)]
pub(crate) struct FinishWaitEntry {
    pub request_id: String,
}

pub(crate) fn finish_wait_snapshot() -> FinishWaitSnapshot {
    let queue = state().lock().unwrap_or_else(|error| error.into_inner());
    FinishWaitSnapshot {
        connection_id: queue
            .connection
            .as_ref()
            .map(|(id, generation)| format!("{id}:{generation}")),
        active: queue
            .records
            .iter()
            .filter(|record| {
                queue.finish_waiting.contains_key(&record.question.id)
                    && record.routing.as_ref().is_some_and(|routing| {
                        !routing.scope_expired
                            && routing.finish_status == Some(FinishStatus::Pending)
                    })
            })
            .map(|record| FinishWaitEntry {
                request_id: record.question.id.clone(),
            })
            .collect(),
    }
}

pub(crate) fn decide_finish(
    id: &str,
    choice: FinishChoice,
    comment: Option<&str>,
) -> Result<QuestionView, String> {
    let result = state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .decide_finish_for(id, choice, comment, None)?;
    // The terminal handler already owns its State lock and sets its own dirty
    // flag. Only wake remote waiters here; never re-enter the terminal mutex.
    workflow::notify();
    Ok(result)
}

pub(crate) fn connect(connection_id: &str) {
    let generation = super::harness::local_runtime()
        .ok()
        .map_or(0, |(runtime, _)| runtime.connection_generation());
    let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
    queue.invalidate_finishes(None, "connection_changed");
    for record in &mut queue.records {
        if let Some(routing) = &mut record.routing {
            routing.scope_expired = true;
        }
    }
    queue.connection = Some((connection_id.into(), generation));
    drop(queue);
    changed();
}

pub(crate) fn disconnect() {
    let mut queue = state().lock().unwrap_or_else(|error| error.into_inner());
    queue.invalidate_finishes(None, "disconnected");
    for record in &mut queue.records {
        if let Some(routing) = &mut record.routing {
            routing.scope_expired = true;
        }
    }
    queue.connection = None;
    drop(queue);
    changed();
}

pub(crate) fn user_input(target: Option<&str>) {
    if state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .invalidate_finishes(target, "new_user_input")
    {
        workflow::notify();
    }
}

fn changed() {
    workflow::notify();
    crate::client::terminal::invalidate();
}

pub(crate) fn answer(id: &str, text: &str) -> Result<Question, String> {
    let answer = state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .finish(id, Some(text))?;
    workflow::notify();
    Ok(answer)
}

pub(crate) fn skip(id: &str) -> Result<Question, String> {
    let question = state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .finish(id, None)?;
    workflow::notify();
    Ok(question)
}

pub(super) async fn finish_request(arguments: Value) -> Result<Value, String> {
    workflow::finish(arguments).await
}

pub(super) fn agent_answer(arguments: Value) -> Result<Value, String> {
    workflow::answer(arguments)
}

pub(super) fn acknowledge_answers(arguments: Value) -> Result<Value, String> {
    delivery::acknowledge(arguments)
}

pub(super) fn delivery_peek(arguments: Value) -> Result<Value, String> {
    delivery::peek(arguments)
}

pub(super) fn authorize_completion() -> Result<String, String> {
    workflow::authorize_completion()
}

pub(super) fn message_scope() -> Option<String> {
    workflow::message_scope()
}

fn history_text(question: &Question) -> String {
    let mut text = match &question.title {
        Some(title) => format!("## Question · {title}\n\n"),
        None => "## Question\n\n".into(),
    };
    text.push_str(&question.question);
    if !question.options.is_empty() {
        text.push_str("\n\nSuggested answers:\n");
        for (index, option) in question.options.iter().enumerate() {
            text.push_str(&format!("{}. {option}\n", index + 1));
        }
    }
    text
}

// The recorder runs after releasing the queue lock. Persistence failure cannot
// undo a successfully queued question or turn it into a wait for an answer.
fn enqueue(
    state: &Mutex<Queue>,
    request: AskRequest,
    on_created: impl FnOnce(&Question),
) -> Result<Value, String> {
    let (question, created, pending_count) = {
        let mut queue = state.lock().unwrap_or_else(|error| error.into_inner());
        let (question, created) = queue.ask(request)?;
        (question, created, queue.pending_count())
    };
    if created {
        on_created(&question);
    }
    Ok(json!({
        "schema_version":1,
        "id":question.id,
        "state":question.state,
        "created":created,
        "blocking":false,
            "automatic_delivery":false,
            "delivery_note":"No verified asking thread; read this legacy question explicitly with ssmcp.ui.questions",
        "pending_count":pending_count,
        "question":question,
        "answer_with":"Shift+Up opens questions in the local client",
        "read_with":"ssmcp.ui.questions",
    }))
}

pub(super) fn ask(arguments: Value) -> Result<Value, String> {
    let request = parse_ask(arguments)?;
    if let Some(scope) = caller_scope()? {
        let automatic_delivery = scope.has_thread();
        let (question, created) = state()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .ask_for(request, Some(scope), QuestionKind::Question)?;
        if created {
            let _ = super::history::record("assistant", &history_text(&question));
            changed();
        }
        return Ok(
            json!({"schema_version":1,"id":question.id,"created":created,
            "state":question.state,"blocking":false,"automatic_delivery":automatic_delivery,
            "delivery_note":if automatic_delivery { "Replies are delivered to the verified requesting thread" } else { "No verified asking thread; read manually with ssmcp.ui.questions, or bind a collab member for future automatic replies" },
            "question":get_view(&question.id),"answer_with":"Shift+Up opens questions in the local client",
            "read_with":"ssmcp.ui.questions"}),
        );
    }
    enqueue(state(), request, |question| {
        if let Err(error) = super::history::record("assistant", &history_text(question)) {
            super::super::terminal::notice(&format!(
                "Question queued; history could not be saved: {error}"
            ));
        }
    })
}

pub(super) fn read(arguments: Value) -> Result<Value, String> {
    let request = parse_read(arguments)?;
    let local = crate::runtime::call_scope::current().is_none();
    let caller = caller_scope()?;
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .read_for(request, caller.as_ref(), local)
}

pub(super) fn parse_message(arguments: Value) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Message {
        text: String,
    }
    let request: Message =
        serde_json::from_value(arguments).map_err(|_| "expected a message text object")?;
    validate_text(&request.text, MAX_ANSWER_BYTES, "message")?;
    Ok(request.text)
}

pub(super) fn tools() -> Vec<Value> {
    let identifier =
        json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"});
    let mut tools = vec![
        json!({
            "name":"ssmcp.ui.ask",
            "description":"Queue a question for this client's local user and return immediately with an ID. Multiple questions can remain pending while you continue working. The user opens questions with Shift+Up, answers in the TUI, or skips them. Read the result later with ssmcp.ui.questions; do not hold a tool call open waiting for an answer. Markdown question text and up to eight suggested answers are supported; the user can always write a different answer. An idempotency_key reuses a retained question with exactly matching content. Verified threads receive typed question_answers on their next eligible native call, separate from user requirements. Reads do not consume answers. Unbound callers receive an explicit manual-read response, never next-caller routing. Records survive reconnect for local review, but old delivery scopes expire. State ends on process restart. At most 32 pending and 64 retained questions share a 256 KiB budget; oldest completed history is reclaimed first.",
            "inputSchema":{
                "type":"object","additionalProperties":false,"required":["question"],
                "properties":{
                    "question":{"type":"string","minLength":1,"maxLength":MAX_QUESTION_BYTES,"description":"Markdown question, at most 8192 UTF-8 bytes; newline and tab are the only permitted control characters."},
                    "title":{"type":"string","minLength":1,"maxLength":128,"description":"Optional one-line title, at most 128 UTF-8 bytes."},
                    "options":{"type":"array","maxItems":8,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":256},"description":"Distinct one-line suggested answers, each at most 256 UTF-8 bytes. Free-form answers remain available."},
                    "idempotency_key":identifier
                }
            },
            "annotations":{"readOnlyHint":false,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.ui.questions",
            "description":"Read queued questions or completed answers without waiting or removing them. Pass id to read one retained question, or page the list with optional state, offset and limit. States are pending, answered and skipped. The local user or another verified agent in the same client/account can answer; actor origin is retained. Pages contain at most 64 KiB; follow next_offset. Pending questions are never evicted. Unacknowledged routed answers are never reclaimed; completed acknowledged history may be reclaimed within the 64-record / 256 KiB budget. All state ends on client process restart.",
            "inputSchema":{
                "type":"object","additionalProperties":false,
                "properties":{
                    "id":identifier,
                    "state":{"type":"string","enum":["pending","answered","skipped"]},
                    "offset":{"type":"integer","minimum":0,"default":0},
                    "limit":{"type":"integer","minimum":1,"maximum":32,"default":8}
                },
                "oneOf":[
                    {"required":["id"],"not":{"anyOf":[{"required":["state"]},{"required":["offset"]},{"required":["limit"]}]}},
                    {"not":{"required":["id"]}}
                ]
            },
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }),
        super::messages::tool(),
    ];
    tools.extend(workflow::tools());
    tools.push(delivery::tool());
    tools.push(delivery::peek_tool());
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(question: &str) -> AskRequest {
        parse_ask(json!({"question":question})).unwrap()
    }

    #[test]
    fn questions_accumulate_and_can_be_answered_out_of_order() {
        let mut queue = Queue::default();
        let first = queue.ask(request("Which branch?")).unwrap().0;
        let second = queue
            .ask(
                parse_ask(json!({"question":"**Choose a format**","options":["HTML","Markdown"]}))
                    .unwrap(),
            )
            .unwrap()
            .0;
        assert_eq!(queue.pending_count(), 2);
        assert_eq!(queue.records[0].question, first);
        let answered = queue.finish(&second.id, Some("Use both, please.")).unwrap();
        assert_eq!(answered.state, QuestionState::Answered);
        assert_eq!(answered.answer.as_deref(), Some("Use both, please."));
        assert_eq!(queue.pending_count(), 1);
        assert_eq!(queue.records[0].question.state, QuestionState::Pending);
        assert_eq!(
            queue.finish(&first.id, None).unwrap().state,
            QuestionState::Skipped
        );
        assert_eq!(queue.pending_count(), 0);
        let revision = queue.revision;
        queue.finish(&second.id, Some("Use both, please.")).unwrap();
        assert_eq!(queue.revision, revision);
        assert!(queue.finish(&second.id, Some("No, PDF")).is_err());
        assert!(queue.finish(&first.id, Some("main")).is_err());
    }

    #[test]
    fn created_question_records_its_context_once_after_releasing_the_queue_lock() {
        let queue = Mutex::new(Queue::default());
        let request = parse_ask(json!({
            "question":"**Choose** a destination.\nKeep the details.",
            "title":"Destination",
            "options":["GitHub","Google Drive"],
            "idempotency_key":"question_history_context",
        }))
        .unwrap();
        let mut records = Vec::new();
        let first = enqueue(&queue, request.clone(), |question| {
            assert!(
                queue.try_lock().is_ok(),
                "history must run outside the queue lock"
            );
            records.push(history_text(question));
        })
        .unwrap();
        assert_eq!(first["created"], true);
        assert_eq!(first["blocking"], false);
        assert_eq!(records.len(), 1);
        assert!(records[0].contains("Destination"));
        assert!(records[0].contains("**Choose** a destination.\nKeep the details."));
        assert!(records[0].contains("1. GitHub\n2. Google Drive"));
        let repeated = enqueue(&queue, request, |_| panic!("retry must not record again")).unwrap();
        assert_eq!(repeated["id"], first["id"]);
        assert_eq!(repeated["created"], false);
        assert_eq!(queue.lock().unwrap().pending_count(), 1);
    }

    #[test]
    fn retry_keys_reuse_questions_without_reopening_completed_answers() {
        let mut queue = Queue::default();
        let request =
            parse_ask(json!({"question":"Continue?","idempotency_key":"checkpoint_1"})).unwrap();
        let (first, created) = queue.ask(request.clone()).unwrap();
        assert!(created);
        assert!(!queue.ask(request.clone()).unwrap().1);
        queue.finish(&first.id, Some("Yes")).unwrap();
        let (again, created) = queue.ask(request).unwrap();
        assert!(!created);
        assert_eq!(again.answer.as_deref(), Some("Yes"));
        assert_eq!(queue.records.len(), 1);
        assert!(
            queue
                .ask(
                    parse_ask(json!({"question":"Delete?","idempotency_key":"checkpoint_1"}))
                        .unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn pending_limit_never_discards_an_unanswered_question() {
        let mut queue = Queue::default();
        for index in 0..MAX_PENDING {
            queue.ask(request(&format!("Question {index}"))).unwrap();
        }
        let before = queue.snapshot();
        assert!(queue.ask(request("One too many")).is_err());
        assert_eq!(queue.snapshot(), before);
        let first = queue.records[0].question.id.clone();
        queue.finish(&first, None).unwrap();
        queue.ask(request("Now there is room")).unwrap();
        assert_eq!(queue.pending_count(), MAX_PENDING);
    }

    #[test]
    fn retained_history_reclaims_completed_records_and_keeps_pending_ones() {
        let mut queue = Queue::default();
        let pending = queue.ask(request("Keep this unanswered")).unwrap().0;
        for index in 0..MAX_RETAINED + 3 {
            let question = queue.ask(request(&format!("Resolved {index}"))).unwrap().0;
            queue.finish(&question.id, Some("done")).unwrap();
        }
        assert_eq!(queue.records.len(), MAX_RETAINED);
        assert_eq!(queue.pending_count(), 1);
        assert_eq!(queue.records[0].question, pending);
        assert!(queue.bytes() <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn answer_budget_failure_preserves_the_question_and_other_pending_text() {
        let mut queue = Queue::default();
        for _ in 0..30 {
            queue.ask(request(&"x".repeat(MAX_QUESTION_BYTES))).unwrap();
        }
        let first = queue.records[0].question.id.clone();
        let before = queue.snapshot();
        assert!(
            queue
                .finish(&first, Some(&"y".repeat(MAX_ANSWER_BYTES)))
                .is_err()
        );
        assert_eq!(queue.snapshot(), before);
        queue.finish(&first, Some("yes")).unwrap();
        assert!(queue.bytes() <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn reads_page_without_consuming_and_filter_by_state() {
        let mut queue = Queue::default();
        let first = queue.ask(request("First")).unwrap().0;
        queue.ask(request("Second")).unwrap();
        queue.finish(&first.id, Some("是")).unwrap();
        let page = queue.read(parse_read(json!({"limit":1})).unwrap()).unwrap();
        assert_eq!(page["total"], 2);
        assert_eq!(page["next_offset"], 1);
        let answered = queue
            .read(parse_read(json!({"state":"answered"})).unwrap())
            .unwrap();
        assert_eq!(answered["questions"][0]["answer"], "是");
        assert_eq!(
            queue
                .read(parse_read(json!({"id":first.id})).unwrap())
                .unwrap()["question"]["state"],
            "answered"
        );
        assert_eq!(queue.records.len(), 2);
        for arguments in [
            json!({"id":"known","limit":1}),
            json!({"id":"known","state":"pending"}),
            json!({"offset":-1}),
            json!({"limit":0}),
            json!({"limit":33}),
            json!({"extra":true}),
            json!({"state":null}),
        ] {
            assert!(parse_read(arguments).is_err());
        }
        assert!(
            queue
                .read(parse_read(json!({"offset":3})).unwrap())
                .is_err()
        );
    }

    #[test]
    fn question_and_message_validation_reject_controls_and_byte_overflows() {
        for arguments in [
            json!({"question":" \n\t"}),
            json!({"question":"bad\u{001b}[2J"}),
            json!({"question":"界".repeat(MAX_QUESTION_BYTES / 3 + 1)}),
            json!({"question":"Fine","title":"two\nlines"}),
            json!({"question":"Fine","options":["yes"," yes "]}),
            json!({"question":"Fine","options":["x".repeat(257)]}),
            json!({"question":"Fine","idempotency_key":"has space"}),
            json!({"question":"Fine","title":null}),
            json!({"question":"Fine","answer":"spoof"}),
        ] {
            assert!(parse_ask(arguments).is_err());
        }
        assert!(parse_message(json!({"text":"# Heading\n\n**Hello**"})).is_ok());
        assert!(parse_message(json!({"text":"hi","role":"user"})).is_err());
        assert!(parse_message(json!({"text":"hi\u{0007}"})).is_err());
        assert!(parse_message(json!({"text":"x".repeat(MAX_ANSWER_BYTES + 1)})).is_err());
        let mut queue = Queue::default();
        let question = queue.ask(request("Choose?")).unwrap().0;
        assert!(queue.finish(&question.id, Some("")).is_err());
        assert!(
            queue
                .finish(&question.id, Some("\u{001b}]0;title"))
                .is_err()
        );
        assert_eq!(queue.pending_count(), 1);
    }

    #[test]
    fn tools_do_not_offer_a_way_to_impersonate_the_users_answer() {
        let catalog = tools();
        assert_eq!(catalog.len(), 3);
        let ask = &catalog[0]["inputSchema"];
        assert!(ask["properties"].get("answer").is_none());
        let read = &catalog[1];
        assert_eq!(read["annotations"]["readOnlyHint"], true);
        assert!(read["inputSchema"]["properties"].get("answer").is_none());
        let message = &catalog[2]["inputSchema"];
        assert!(message["properties"].get("role").is_none());
        assert!(message["properties"].get("agent").is_none());
    }
}
