//! Client-owned requests. Broker revisions decide whether an edit won against delivery.
use crate::wire::{
    MAX_USER_MESSAGE_BYTES, MAX_USER_MESSAGES, SteeringAgent, SteeringCommand, SteeringMessage,
    SteeringSnapshot, SteeringState,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::mpsc;

const MAX_RECORDS: usize = 128;
const MAX_TEXT_BYTES: usize = 512 * 1024;
const MAX_HANDOFF_BYTES: u64 = 4 * 1024 * 1024;
const HANDOFF_ENV: &str = "SSMCP_CLIENT_OUTBOX";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
static STATE: OnceLock<Mutex<Manager>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum UiEvent {
    Accepted { id: String, text: String },
    Editable { id: String, text: String },
    Failed { id: String, message: String },
}

pub(super) enum Transport {
    Wire { id: u64 },
    Reload,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum MutationKind {
    Publish,
    Takeback,
    Cancel,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    command_id: String,
    kind: MutationKind,
    expected_revision: u64,
    for_reload: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    message: SteeringMessage,
    room_id: Option<String>,
    /// A new local request or a successful takeback is known not to be delivered.
    safe_to_publish: bool,
    ever_sent: bool,
    uncertain: bool,
    retarget_required: bool,
    resume_after_reload: bool,
    pending: Option<Mutation>,
    error: Option<String>,
}

impl Record {
    fn terminal(&self) -> bool {
        matches!(
            self.message.state,
            SteeringState::Delivered | SteeringState::Cancelled
        )
    }
    /// Resume only a takeback confirmed for this reload in the still-current room.
    /// An uncertain outcome must remain available for explicit operator review.
    fn resume_reclaimed(&mut self, room_id: Option<&str>) {
        if self.resume_after_reload
            && room_id.is_some()
            && self.room_id.as_deref() == room_id
            && self.message.state == SteeringState::Editing
            && self.safe_to_publish
            && self.pending.is_none()
            && !self.uncertain
            && !self.retarget_required
        {
            self.message.state = SteeringState::Queued;
            self.error = None;
        }
    }

    fn status(&self, connected: bool) -> &'static str {
        if self.message.state == SteeringState::Delivered {
            return "delivered";
        }
        if self.message.state == SteeringState::Cancelled {
            return "cancelled";
        }
        if self.retarget_required {
            return "retarget-required";
        }
        if self.uncertain {
            return "uncertain";
        }
        if let Some(pending) = &self.pending {
            return match pending.kind {
                MutationKind::Publish => "sending",
                MutationKind::Takeback => "taking-back",
                MutationKind::Cancel => "cancelling",
            };
        }
        if self.message.state == SteeringState::Editing {
            return "editing";
        }
        if self.safe_to_publish {
            if connected { "sending" } else { "offline" }
        } else {
            "queued"
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Draft {
    text: String,
    cursor: usize,
}

/// A displaced input is retained separately while a queued request is edited.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedDraft {
    pub text: String,
    pub cursor: usize,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(super) struct DraftContext {
    pub editing: Option<String>,
    pub target: Option<String>,
    pub parked: Option<SavedDraft>,
}

impl DraftContext {
    fn validate(&self) -> Result<(), String> {
        for id in [
            self.editing.as_deref(),
            self.target.as_deref(),
            self.parked
                .as_ref()
                .and_then(|draft| draft.target.as_deref()),
        ]
        .into_iter()
        .flatten()
        {
            if !valid_id(id) {
                return Err("invalid draft context identifier".into());
            }
        }
        if self.parked.as_ref().is_some_and(|draft| {
            draft.text.len() > MAX_USER_MESSAGE_BYTES || !draft.text.is_char_boundary(draft.cursor)
        }) {
            return Err("invalid or oversized parked draft".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Outbox {
    schema_version: u32,
    room_id: Option<String>,
    draft: Draft,
    #[serde(default)]
    context: DraftContext,
    records: Vec<Record>,
}

impl Default for Outbox {
    fn default() -> Self {
        Self {
            schema_version: 1,
            room_id: None,
            draft: Draft::default(),
            context: DraftContext::default(),
            records: Vec::new(),
        }
    }
}

impl Outbox {
    fn validate(&self) -> Result<(), String> {
        self.context.validate()?;
        if self.schema_version != 1
            || self.records.len() > MAX_RECORDS
            || self
                .records
                .iter()
                .map(|record| record.message.text.len())
                .sum::<usize>()
                > MAX_TEXT_BYTES
            || self.draft.text.len() > MAX_USER_MESSAGE_BYTES
            || !self.draft.text.is_char_boundary(self.draft.cursor)
            || self.room_id.as_ref().is_some_and(|room| !valid_id(room))
        {
            return Err("invalid or oversized client outbox".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for record in &self.records {
            let message = &record.message;
            if !valid_id(&message.id)
                || !valid_id(&message.target)
                || !ids.insert(&message.id)
                || record.room_id.as_ref().is_some_and(|room| !valid_id(room))
                || record
                    .error
                    .as_ref()
                    .is_some_and(|error| error.len() > 1024)
                || record
                    .pending
                    .as_ref()
                    .is_some_and(|pending| !valid_id(&pending.command_id))
            {
                return Err("invalid client outbox record".into());
            }
            validate_text(&message.text)?;
        }
        Ok(())
    }
}

struct Request {
    command: SteeringCommand,
    queued_at: Instant,
    sent: bool,
    prepared: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct AcceptedHistory {
    id: String,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
struct DeliveredMemory {
    id: String,
    text: String,
    revision: u64,
    target: String,
}

#[derive(Default)]
struct Manager {
    outbox: Outbox,
    connected: bool,
    supported: bool,
    ready: bool,
    revision: u64,
    agents: Vec<SteeringAgent>,
    sender: Option<mpsc::Sender<Transport>>,
    requests: BTreeMap<u64, Request>,
    next_request: u64,
    events: VecDeque<UiEvent>,
    // Drained by transport replies even when no terminal frontend is running.
    // Pure Manager operations never perform history I/O.
    accepted_history: Vec<AcceptedHistory>,
    delivered_memory: Vec<DeliveredMemory>,
    frozen: bool,
    reload_requested: bool,
    error: Option<String>,
    restored: bool,
}

fn state() -> &'static Mutex<Manager> {
    STATE.get_or_init(|| Mutex::new(Manager::default()))
}

fn with<T>(f: impl FnOnce(&mut Manager) -> T) -> T {
    f(&mut state().lock().unwrap_or_else(|error| error.into_inner()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes)
        .map_err(|_| "cannot create local request identity")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
pub(super) fn validate_text(text: &str) -> Result<(), String> {
    if text.trim().is_empty()
        || text.len() > MAX_USER_MESSAGE_BYTES
        || text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Err("request must contain 1..=16384 UTF-8 bytes of text; only newline and tab controls are allowed".into());
    }
    Ok(())
}

impl Manager {
    fn event(&mut self, event: UiEvent) {
        if self.events.len() == 256 {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }

    fn snapshot(&self) -> Value {
        json!({
            "schema_version":1,"connected":self.connected,"supported":self.supported,
            "room_id":self.outbox.room_id,"revision":self.revision,"agents":self.agents,
            "draft":{"text":self.outbox.draft.text,"cursor":self.outbox.draft.cursor},
            "draft_context":self.outbox.context,
            "queue":self.outbox.records.iter().map(|record| json!({
                "id":record.message.id,"text":record.message.text,"target":record.message.target,
                "revision":record.message.revision,"status":record.status(self.connected),
                "created_at_ms":record.message.created_at_ms,"updated_at_ms":record.message.updated_at_ms,
                "delivery":record.message.delivery,"error":record.error,"room_id":record.room_id,
                "retarget_required":record.retarget_required,"uncertain":record.uncertain
            })).collect::<Vec<_>>(),
            "error":self.error,"reloading":self.frozen || self.reload_requested
        })
    }

    fn mutable(&self) -> Result<(), String> {
        if self.frozen {
            return Err("client reload is preparing; the draft is retained".into());
        }
        if self.connected && !self.supported {
            return Err(
                "this gateway does not support the request queue; update ssmcpd to wire version 7"
                    .into(),
            );
        }
        Ok(())
    }

    fn target(&self, target: Option<&str>) -> Result<String, String> {
        let target = target.unwrap_or("next");
        if target == "next" {
            return Ok(target.into());
        }
        if !valid_id(target)
            || !self.connected
            || !self.ready
            || !self.agents.iter().any(|agent| agent.agent_id == target)
        {
            return Err("choose next or an agent from the current connected room".into());
        }
        Ok(target.into())
    }

    fn index(&self, id: &str) -> Result<usize, String> {
        self.outbox
            .records
            .iter()
            .position(|record| record.message.id == id)
            .ok_or_else(|| "queued request was not found".into())
    }

    fn prune(&mut self) {
        while self.outbox.records.len() >= MAX_RECORDS
            || self
                .outbox
                .records
                .iter()
                .map(|record| record.message.text.len())
                .sum::<usize>()
                > MAX_TEXT_BYTES - MAX_USER_MESSAGE_BYTES
        {
            let Some(index) = self
                .outbox
                .records
                .iter()
                .position(|record| record.terminal() && record.pending.is_none())
            else {
                break;
            };
            self.outbox.records.remove(index);
        }
    }

    fn submit(&mut self, text: &str, target: Option<&str>) -> Result<String, String> {
        self.mutable()?;
        validate_text(text)?;
        let target = self.target(target)?;
        self.prune();
        if self
            .outbox
            .records
            .iter()
            .filter(|record| !record.terminal())
            .count()
            >= MAX_USER_MESSAGES
            || self.outbox.records.len() >= MAX_RECORDS
            || self
                .outbox
                .records
                .iter()
                .map(|record| record.message.text.len())
                .sum::<usize>()
                + text.len()
                > MAX_TEXT_BYTES
        {
            return Err(
                "local request queue is full; finish or cancel a queued request first".into(),
            );
        }
        let id = random_id()?;
        self.outbox.records.push(Record {
            message: SteeringMessage {
                id: id.clone(),
                revision: 0,
                text: text.into(),
                target,
                state: SteeringState::Queued,
                created_at_ms: now_ms(),
                updated_at_ms: now_ms(),
                delivery: None,
            },
            room_id: self.outbox.room_id.clone(),
            safe_to_publish: true,
            ever_sent: false,
            uncertain: false,
            retarget_required: false,
            resume_after_reload: false,
            pending: None,
            error: None,
        });
        let index = self.outbox.records.len() - 1;
        if self.connected
            && self.supported
            && self.ready
            && let Err(error) = self.mutate(index, MutationKind::Publish, false)
        {
            self.outbox.records.pop();
            return Err(error);
        }
        Ok(id)
    }

    fn publish_edit(&mut self, id: &str, text: &str, target: Option<&str>) -> Result<(), String> {
        self.mutable()?;
        validate_text(text)?;
        let index = self.index(id)?;
        let target =
            self.target(target.or(Some(self.outbox.records[index].message.target.as_str())))?;
        let original = self.outbox.records[index].clone();
        if original.message.state != SteeringState::Editing
            || original.pending.is_some()
            || original.uncertain
            || original.retarget_required
        {
            return Err("take back the queued request before editing; stale targets need explicit retargeting".into());
        }
        if self
            .outbox
            .records
            .iter()
            .map(|record| record.message.text.len())
            .sum::<usize>()
            - original.message.text.len()
            + text.len()
            > MAX_TEXT_BYTES
        {
            return Err("local request queue text limit reached".into());
        }
        let record = &mut self.outbox.records[index];
        record.message.text = text.into();
        record.message.target = target;
        record.message.state = SteeringState::Queued;
        record.message.updated_at_ms = now_ms();
        record.error = None;
        if self.connected
            && self.supported
            && self.ready
            && let Err(error) = self.mutate(index, MutationKind::Publish, false)
        {
            self.outbox.records[index] = original;
            return Err(error);
        }
        Ok(())
    }

    fn take_back(&mut self, id: &str) -> Result<(), String> {
        self.mutable()?;
        let index = self.index(id)?;
        let record = &self.outbox.records[index];
        if record.terminal()
            || record.pending.is_some()
            || record.uncertain
            || record.retarget_required
        {
            return Err("request is delivered, changing, or needs explicit retargeting; refresh its state before editing".into());
        }
        if record.safe_to_publish {
            self.outbox.records[index].message.state = SteeringState::Editing;
            self.outbox.records[index].resume_after_reload = false;
            self.event(UiEvent::Editable {
                id: id.into(),
                text: super::references::source(&self.outbox.records[index].message.text)
                    .to_owned(),
            });
            return Ok(());
        }
        self.mutate(index, MutationKind::Takeback, false)
    }

    fn cancel(&mut self, id: &str) -> Result<(), String> {
        self.mutable()?;
        let index = self.index(id)?;
        let record = &self.outbox.records[index];
        if record.terminal() || record.pending.is_some() || record.uncertain {
            return Err(
                "request is delivered, changing, or its previous delivery is uncertain".into(),
            );
        }
        if record.safe_to_publish
            && (!record.ever_sent || !self.connected || record.room_id != self.outbox.room_id)
        {
            self.outbox.records[index].message.state = SteeringState::Cancelled;
            self.outbox.records[index].resume_after_reload = false;
            return Ok(());
        }
        self.mutate(index, MutationKind::Cancel, false)
    }

    fn retarget(&mut self, id: &str, target: Option<&str>) -> Result<(), String> {
        self.mutable()?;
        let target = self.target(target)?;
        let index = self.index(id)?;
        let record = &mut self.outbox.records[index];
        let offline_unsent = !self.connected
            && record.safe_to_publish
            && !record.uncertain
            && record.message.state == SteeringState::Queued;
        if record.terminal()
            || record.pending.is_some()
            || (!record.retarget_required
                && !record.uncertain
                && record.message.state != SteeringState::Editing
                && !offline_unsent)
        {
            return Err("take back this request before changing its recipient".into());
        }
        let changed_room = record.room_id != self.outbox.room_id;
        if changed_room || record.uncertain {
            record.message.revision = 0;
            record.ever_sent = false;
        }
        record.message.target = target;
        record.message.state = SteeringState::Editing;
        record.room_id = self.outbox.room_id.clone();
        record.safe_to_publish = true;
        record.uncertain = false;
        record.retarget_required = false;
        record.resume_after_reload = false;
        record.error = None;
        let text = super::references::source(&record.message.text).to_owned();
        self.event(UiEvent::Editable {
            id: id.into(),
            text,
        });
        Ok(())
    }

    fn enqueue(&mut self, command: SteeringCommand) -> Result<u64, String> {
        if !self.connected || !self.supported {
            return Err("request queue is not connected".into());
        }
        let id = self
            .next_request
            .checked_add(1)
            .ok_or("request identity space exhausted")?;
        self.sender
            .as_ref()
            .ok_or("request queue is not connected")?
            .try_send(Transport::Wire { id })
            .map_err(|_| "request transport is busy; the draft has been retained")?;
        self.next_request = id;
        self.requests.insert(
            id,
            Request {
                command,
                queued_at: Instant::now(),
                sent: false,
                prepared: false,
            },
        );
        Ok(id)
    }

    fn mutate(&mut self, index: usize, kind: MutationKind, for_reload: bool) -> Result<(), String> {
        let record = &self.outbox.records[index];
        if record.pending.is_some() {
            return Err("request change is already waiting for acknowledgement".into());
        }
        if record.room_id != self.outbox.room_id || record.uncertain || record.retarget_required {
            return Err("this request needs explicit retargeting before it can be changed".into());
        }
        let command_id = random_id()?;
        let message_id = record.message.id.clone();
        let expected_revision = record.message.revision;
        let command = match kind {
            MutationKind::Publish => SteeringCommand::Publish {
                command_id: command_id.clone(),
                message_id,
                expected_revision,
                text: record.message.text.clone(),
                target: record.message.target.clone(),
            },
            MutationKind::Takeback => SteeringCommand::Takeback {
                command_id: command_id.clone(),
                message_id,
                expected_revision,
            },
            MutationKind::Cancel => SteeringCommand::Cancel {
                command_id: command_id.clone(),
                message_id,
                expected_revision,
            },
        };
        self.enqueue(command)?;
        self.outbox.records[index].pending = Some(Mutation {
            command_id,
            kind,
            expected_revision,
            for_reload,
        });
        self.outbox.records[index].resume_after_reload =
            for_reload && kind == MutationKind::Takeback;
        self.outbox.records[index].error = None;
        Ok(())
    }

    fn request_snapshot(&mut self) -> Result<(), String> {
        if !self
            .requests
            .values()
            .any(|request| matches!(request.command, SteeringCommand::Snapshot {}))
        {
            self.enqueue(SteeringCommand::Snapshot {})?;
        }
        Ok(())
    }

    fn connect(&mut self, room_id: &str, protocol: u32) -> mpsc::Receiver<Transport> {
        self.disconnect();
        let (sender, receiver) = mpsc::channel(MAX_RECORDS + 8);
        let changed = self.outbox.room_id.as_deref() != Some(room_id);
        self.outbox.room_id = Some(room_id.into());
        self.connected = true;
        self.supported = protocol >= 7;
        self.ready = false;
        self.revision = 0;
        self.sender = Some(sender);
        self.error = None;
        self.reload_requested = false;
        self.frozen = false;
        if changed {
            for record in &mut self.outbox.records {
                if record.terminal() {
                    continue;
                }
                record.pending = None;
                if record.message.target != "next" {
                    record.retarget_required = true;
                }
                if record.safe_to_publish {
                    record.room_id = Some(room_id.into());
                    record.message.revision = 0;
                    record.ever_sent = false;
                    if record.resume_after_reload {
                        record.message.state = SteeringState::Queued;
                    }
                    record.resume_after_reload = false;
                } else {
                    record.uncertain = true;
                    record.error = Some("Previous room ended before delivery could be confirmed. This request will not be replayed automatically.".into());
                }
            }
        }
        if self.supported {
            let _ = self.request_snapshot();
        }
        receiver
    }

    fn disconnect(&mut self) {
        self.connected = false;
        self.ready = false;
        self.sender = None;
        self.agents.clear();
        for request in self.requests.values() {
            if let Some(id) = command_message(&request.command)
                && let Some(record) = self
                    .outbox
                    .records
                    .iter_mut()
                    .find(|record| record.message.id == id)
            {
                if request.sent && !record.terminal() {
                    record.uncertain = true;
                }
                record.pending = None;
            }
        }
        self.requests.clear();
    }

    fn pending_command(&self, id: u64) -> Option<SteeringCommand> {
        self.requests
            .get(&id)
            .filter(|request| !request.sent)
            .map(|request| request.command.clone())
    }

    fn prepare_publish(
        &mut self,
        id: u64,
        expanded: String,
    ) -> Result<Option<SteeringCommand>, String> {
        let Some(request) = self.requests.get(&id).filter(|request| !request.sent) else {
            return Ok(None);
        };
        if request.prepared {
            return Ok(Some(request.command.clone()));
        }
        let SteeringCommand::Publish {
            command_id,
            message_id,
            expected_revision,
            ..
        } = &request.command
        else {
            return Ok(Some(request.command.clone()));
        };
        validate_text(&expanded)?;
        let index = self.index(message_id)?;
        let record = &self.outbox.records[index];
        if record.room_id != self.outbox.room_id
            || record.terminal()
            || record.message.revision != *expected_revision
            || !record
                .pending
                .as_ref()
                .is_some_and(|pending| pending.command_id == *command_id)
        {
            return Err(
                "The request changed while its references were prepared; review it before sending."
                    .into(),
            );
        }
        let used = self
            .outbox
            .records
            .iter()
            .map(|record| record.message.text.len())
            .sum::<usize>();
        if used - record.message.text.len() + expanded.len() > MAX_TEXT_BYTES {
            return Err("The expanded references exceed the local queue's text limit; cancel a queued request or use fewer references.".into());
        }
        let request = self.requests.get_mut(&id).expect("validated request");
        let SteeringCommand::Publish { text, .. } = &mut request.command else {
            unreachable!();
        };
        text.clone_from(&expanded);
        request.prepared = true;
        self.outbox.records[index].message.text = expanded;
        Ok(Some(request.command.clone()))
    }

    /// A local preparation failure proves that this revision was never written.
    /// Keep prior server identity/revision and the exact edit for another attempt.
    fn fail_unsent(&mut self, id: u64, error: &str) {
        if !self.requests.get(&id).is_some_and(|request| !request.sent) {
            return;
        }
        let request = self.requests.remove(&id).expect("validated request");
        let SteeringCommand::Publish {
            command_id,
            message_id,
            expected_revision,
            text,
            target,
        } = request.command
        else {
            return;
        };
        let Ok(index) = self.index(&message_id) else {
            return;
        };
        let record = &mut self.outbox.records[index];
        if !record
            .pending
            .as_ref()
            .is_some_and(|pending| pending.command_id == command_id)
        {
            return;
        }
        record.pending = None;
        let editable = record.room_id == self.outbox.room_id
            && record.message.revision == expected_revision
            && !record.terminal();
        let text = super::references::source(&text).to_owned();
        let error = bounded_error(error);
        if editable {
            record.message.text.clone_from(&text);
            record.message.target = target;
            record.message.state = SteeringState::Editing;
            record.safe_to_publish = true;
            record.uncertain = false;
            record.retarget_required = false;
            record.resume_after_reload = false;
            record.error = Some(error.clone());
        }
        self.event(UiEvent::Failed {
            id: message_id.clone(),
            message: error,
        });
        if editable {
            self.event(UiEvent::Editable {
                id: message_id,
                text,
            });
        }
    }

    fn sent(&mut self, id: u64) -> bool {
        let Some(request) = self.requests.get_mut(&id) else {
            return false;
        };
        if request.sent {
            return false;
        }
        request.sent = true;
        request.queued_at = Instant::now();
        if let Some(message_id) = command_message(&request.command)
            && let Some(record) = self
                .outbox
                .records
                .iter_mut()
                .find(|record| record.message.id == message_id)
        {
            record.ever_sent = true;
            if matches!(request.command, SteeringCommand::Publish { .. }) {
                record.safe_to_publish = false;
            }
        }
        true
    }

    fn merge(&mut self, snapshot: &SteeringSnapshot) -> Result<(), String> {
        if snapshot.schema_version != 1
            || Some(snapshot.client_id.as_str()) != self.outbox.room_id.as_deref()
            || snapshot.agents.len() > 64
            || snapshot.messages.len() > MAX_RECORDS
            || snapshot
                .messages
                .iter()
                .map(|message| message.text.len())
                .sum::<usize>()
                > MAX_TEXT_BYTES
        {
            return Err("invalid request queue snapshot".into());
        }
        for agent in &snapshot.agents {
            if !valid_id(&agent.agent_id)
                || agent.name.len() > 128
                || agent.role.as_ref().is_some_and(|role| role.len() > 128)
            {
                return Err("invalid agent roster".into());
            }
        }
        for message in &snapshot.messages {
            validate_text(&message.text)?;
            if !valid_id(&message.id) || !valid_id(&message.target) {
                return Err("invalid request identity".into());
            }
        }
        if self.ready && snapshot.revision < self.revision {
            return Ok(());
        }
        self.ready = true;
        self.revision = snapshot.revision;
        self.agents.clone_from(&snapshot.agents);
        self.error = None;
        for message in &snapshot.messages {
            if let Some(record) = self.outbox.records.iter_mut().find(|record| {
                record.message.id == message.id && record.room_id == self.outbox.room_id
            }) {
                if message.revision >= record.message.revision {
                    if !record.terminal()
                        && message.revision > record.message.revision
                        && message.state == SteeringState::Delivered
                        && message.delivery.is_some()
                        && message.target == "next"
                    {
                        self.delivered_memory.push(DeliveredMemory {
                            id: message.id.clone(),
                            text: super::references::source(&message.text).to_owned(),
                            revision: message.revision,
                            target: message.target.clone(),
                        });
                    }
                    let keep_local_edit = message.state == SteeringState::Editing
                        && message.revision == record.message.revision
                        && !record.terminal()
                        && (record.safe_to_publish
                            || record
                                .pending
                                .as_ref()
                                .is_some_and(|pending| pending.kind == MutationKind::Publish));
                    if keep_local_edit {
                        // A failed or still-preparing local edit has the same
                        // server revision as the original taken-back contents.
                        record.message.state = message.state;
                        record.message.updated_at_ms = message.updated_at_ms;
                        record.message.delivery.clone_from(&message.delivery);
                    } else {
                        record.message.clone_from(message);
                    }
                    record.ever_sent = true;
                    record.uncertain = false;
                    record.safe_to_publish = message.state == SteeringState::Editing;
                    if record.terminal() {
                        record.resume_after_reload = false;
                        record.retarget_required = false;
                    }
                }
            } else if !self
                .outbox
                .records
                .iter()
                .any(|record| record.message.id == message.id)
            {
                self.prune();
                if self.outbox.records.len() < MAX_RECORDS
                    && self
                        .outbox
                        .records
                        .iter()
                        .map(|record| record.message.text.len())
                        .sum::<usize>()
                        + message.text.len()
                        <= MAX_TEXT_BYTES
                {
                    self.outbox.records.push(Record {
                        message: message.clone(),
                        room_id: self.outbox.room_id.clone(),
                        safe_to_publish: message.state == SteeringState::Editing,
                        ever_sent: true,
                        uncertain: false,
                        retarget_required: false,
                        resume_after_reload: false,
                        pending: None,
                        error: None,
                    });
                }
            }
        }
        // A complete, current snapshot also tells us which acknowledged
        // records are no longer retained. The server can trim delivered history
        // before the client observes delivery; absence must never masquerade as
        // an editable queued request or cause an automatic resend.
        for record in &mut self.outbox.records {
            if record.room_id == self.outbox.room_id
                && record.ever_sent
                && !record.terminal()
                && !record.safe_to_publish
                && !record.uncertain
                && record.pending.is_none()
                && !snapshot
                    .messages
                    .iter()
                    .any(|message| message.id == record.message.id)
            {
                record.uncertain = true;
                record.resume_after_reload = false;
                record.error = Some("Server no longer retains this request; previous delivery cannot be confirmed. Review it before resending.".into());
            }
            if !self.frozen {
                // A prior snapshot may arrive after the reclaim ACK. Retain the
                // resume intent until publish is queued, rather than reverting
                // the operator's request to an unattended editing state.
                record.resume_reclaimed(self.outbox.room_id.as_deref());
            }
        }
        Ok(())
    }

    fn reply(&mut self, id: u64, result: Result<SteeringSnapshot, String>) -> Result<(), String> {
        let Some(request) = self.requests.remove(&id) else {
            return Ok(());
        };
        let message_id = command_message(&request.command).map(str::to_owned);
        let response = result.and_then(|snapshot| {
            self.merge(&snapshot)?;
            Ok(snapshot)
        });
        if let Some(message_id) = message_id {
            let Ok(index) = self.index(&message_id) else {
                return Ok(());
            };
            let Some(pending) = self.outbox.records[index].pending.take() else {
                return Ok(());
            };
            let matching = command_id(&request.command) == Some(pending.command_id.as_str());
            if !matching {
                self.outbox.records[index].pending = Some(pending);
                return Ok(());
            }
            match response {
                Ok(snapshot) => {
                    let acknowledged = snapshot.messages.iter().find(|message| {
                        message.id == message_id && message.revision > pending.expected_revision
                    });
                    match (pending.kind, acknowledged) {
                        (MutationKind::Publish, Some(message))
                            if matches!(
                                message.state,
                                SteeringState::Queued | SteeringState::Delivered
                            ) =>
                        {
                            let SteeringCommand::Publish { text, .. } = request.command else {
                                unreachable!()
                            };
                            let text = super::references::source(&text).to_owned();
                            self.accepted_history.push(AcceptedHistory {
                                id: message_id.clone(),
                                text: text.clone(),
                            });
                            self.event(UiEvent::Accepted {
                                id: message_id,
                                text,
                            });
                        }
                        (MutationKind::Takeback, Some(message))
                            if message.state == SteeringState::Editing =>
                        {
                            self.outbox.records[index].safe_to_publish = true;
                            if pending.for_reload {
                                self.outbox.records[index].resume_after_reload = true;
                                if !self.frozen {
                                    // The reload may have timed out while this takeback
                                    // was in flight. Its successful ACK now makes retry safe.
                                    self.outbox.records[index]
                                        .resume_reclaimed(self.outbox.room_id.as_deref());
                                }
                            } else {
                                self.event(UiEvent::Editable {
                                    id: message_id,
                                    text: super::references::source(&message.text).to_owned(),
                                });
                            }
                        }
                        (MutationKind::Cancel, Some(message))
                            if message.state == SteeringState::Cancelled => {}
                        _ => {
                            let message = "Request state changed before the update could be confirmed; refresh before editing".to_owned();
                            self.outbox.records[index].error = Some(message.clone());
                            self.event(UiEvent::Failed {
                                id: message_id,
                                message,
                            });
                        }
                    }
                }
                Err(message) => {
                    let message = bounded_error(&message);
                    let record = &mut self.outbox.records[index];
                    record.error = Some(message.clone());
                    if !record.terminal() {
                        // An error can represent a revision conflict after the delivery boundary.
                        record.uncertain = record.ever_sent;
                        if !record.ever_sent {
                            record.message.state = SteeringState::Editing;
                        }
                    }
                    self.event(UiEvent::Failed {
                        id: message_id,
                        message,
                    });
                    let _ = self.request_snapshot();
                }
            }
        } else if let Err(error) = response {
            self.error = Some(bounded_error(&error));
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<(), String> {
        if !self.connected || !self.supported {
            return Ok(());
        }
        let expired = self
            .requests
            .iter()
            .filter(|(_, request)| request.sent && request.queued_at.elapsed() >= REQUEST_TIMEOUT)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in expired {
            self.reply(
                id,
                Err(
                    "Queue acknowledgement timed out; delivery will be checked before any retry"
                        .into(),
                ),
            )?;
        }
        if !self.frozen && self.ready {
            let indices = self
                .outbox
                .records
                .iter()
                .enumerate()
                .filter(|(_, record)| {
                    record.message.state == SteeringState::Queued
                        && record.safe_to_publish
                        && record.pending.is_none()
                        && !record.uncertain
                        && !record.retarget_required
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            for index in indices {
                self.mutate(index, MutationKind::Publish, false)?;
            }
        }
        self.request_snapshot()
    }

    fn begin_reload(&mut self) -> Result<(), String> {
        self.frozen = true;
        if self.connected && self.supported {
            self.request_snapshot()?;
        }
        Ok(())
    }

    fn abort_reload(&mut self, error: &str) {
        self.frozen = false;
        self.reload_requested = false;
        for record in &mut self.outbox.records {
            record.resume_reclaimed(self.outbox.room_id.as_deref());
        }
        self.event(UiEvent::Failed {
            id: String::new(),
            message: bounded_error(error),
        });
    }

    fn reload_ready(&mut self) -> Result<bool, String> {
        if !self.supported {
            return Ok(true);
        }
        if !self.requests.is_empty() || !self.ready {
            return Ok(false);
        }
        let indices = self
            .outbox
            .records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.room_id == self.outbox.room_id
                    && record.message.state == SteeringState::Queued
                    && !record.safe_to_publish
                    && !record.uncertain
                    && !record.retarget_required
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if indices.is_empty() {
            return Ok(true);
        }
        for index in indices {
            self.mutate(index, MutationKind::Takeback, true)?;
        }
        Ok(false)
    }
}

fn command_message(command: &SteeringCommand) -> Option<&str> {
    match command {
        SteeringCommand::Snapshot {} => None,
        SteeringCommand::Publish { message_id, .. }
        | SteeringCommand::Takeback { message_id, .. }
        | SteeringCommand::Cancel { message_id, .. } => Some(message_id),
    }
}
fn command_id(command: &SteeringCommand) -> Option<&str> {
    match command {
        SteeringCommand::Snapshot {} => None,
        SteeringCommand::Publish { command_id, .. }
        | SteeringCommand::Takeback { command_id, .. }
        | SteeringCommand::Cancel { command_id, .. } => Some(command_id),
    }
}
fn bounded_error(error: &str) -> String {
    error
        .chars()
        .filter(|ch| !ch.is_control())
        .take(256)
        .collect()
}

pub(super) fn snapshot() -> Value {
    with(|manager| manager.snapshot())
}
pub(super) fn submit(text: &str, target: Option<&str>) -> Result<String, String> {
    with(|manager| manager.submit(text, target))
}
pub(super) fn publish_edit(id: &str, text: &str, target: Option<&str>) -> Result<(), String> {
    with(|manager| manager.publish_edit(id, text, target))
}
pub(super) fn take_back(id: &str) -> Result<(), String> {
    with(|manager| manager.take_back(id))
}
pub(super) fn cancel(id: &str) -> Result<(), String> {
    with(|manager| manager.cancel(id))
}
pub(super) fn retarget(id: &str, target: Option<&str>) -> Result<(), String> {
    with(|manager| manager.retarget(id, target))
}
pub(super) fn draft() -> (String, usize) {
    with(|manager| {
        (
            manager.outbox.draft.text.clone(),
            manager.outbox.draft.cursor,
        )
    })
}
pub(super) fn draft_context() -> DraftContext {
    with(|manager| manager.outbox.context.clone())
}
pub(super) fn set_draft_context(context: DraftContext) -> Result<(), String> {
    context.validate()?;
    with(|manager| manager.outbox.context = context);
    Ok(())
}
pub(super) fn set_draft(text: &str, cursor: usize) -> Result<(), String> {
    if text.len() > MAX_USER_MESSAGE_BYTES || !text.is_char_boundary(cursor) {
        return Err("invalid or oversized composer draft".into());
    }
    with(|manager| {
        manager.outbox.draft = Draft {
            text: text.into(),
            cursor,
        }
    });
    Ok(())
}
pub(super) fn take_events() -> Vec<UiEvent> {
    with(|manager| manager.events.drain(..).collect())
}
pub(super) fn connect(room_id: &str, protocol: u32) -> mpsc::Receiver<Transport> {
    let transport = with(|manager| manager.connect(room_id, protocol));
    super::interface::listener::connect(room_id);
    super::interface::user_wait::connect(room_id);
    transport
}
pub(super) fn disconnect() {
    with(Manager::disconnect);
    super::interface::listener::disconnect();
    super::interface::user_wait::disconnect();
    // Display invalidation happens only after releasing the request manager lock.
    super::terminal::callers_disconnected();
}
pub(super) fn pending_command(id: u64) -> Option<SteeringCommand> {
    with(|manager| manager.pending_command(id))
}
pub(super) fn prepare_publish(id: u64, text: String) -> Result<Option<SteeringCommand>, String> {
    with(|manager| manager.prepare_publish(id, text))
}
pub(super) fn fail_unsent(id: u64, error: &str) {
    with(|manager| manager.fail_unsent(id, error));
}
pub(super) fn sent(id: u64) -> bool {
    with(|manager| manager.sent(id))
}
pub(super) fn reply(id: u64, result: Result<SteeringSnapshot, String>) -> Result<(), String> {
    let (outcome, accepted, delivered) = with(|manager| {
        let outcome = manager.reply(id, result);
        (
            outcome,
            std::mem::take(&mut manager.accepted_history),
            std::mem::take(&mut manager.delivered_memory),
        )
    });
    // Disk and display work happen after releasing the request manager lock.
    // A history error cannot change an acknowledged publication to failed.
    for message in accepted {
        if let Err(error) = super::interface::history::record_request(&message.id, &message.text) {
            super::render::status(
                "history",
                "Warning",
                &format!("Request was accepted, but its history could not be saved: {error}"),
            );
        }
    }
    // Queue publication is not delivery. Only a newly confirmed public request
    // can become shared workspace memory, after its original history exists.
    for message in delivered {
        if let Err(error) = super::interface::history::remember_delivered_request(
            &message.id,
            &message.text,
            message.revision,
            &message.target,
        ) {
            super::render::status(
                "memory",
                "Warning",
                &format!(
                    "Request delivery was confirmed, but its memory could not be saved: {error}"
                ),
            );
        }
    }
    outcome
}
pub(super) fn poll() -> Result<(), String> {
    with(Manager::poll)
}
pub(super) fn begin_reload() -> Result<(), String> {
    with(Manager::begin_reload)
}
pub(super) fn reload_ready() -> Result<bool, String> {
    with(Manager::reload_ready)
}
pub(super) fn abort_reload(error: &str) {
    with(|manager| manager.abort_reload(error));
}
pub(super) fn request_reload() -> Result<(), String> {
    with(|manager| {
        if !manager.connected {
            return Err("client is not connected; draft and queue are retained".into());
        }
        if manager.reload_requested || manager.frozen {
            return Err("client reload is already pending".into());
        }
        manager
            .sender
            .as_ref()
            .ok_or("client is not connected")?
            .try_send(Transport::Reload)
            .map_err(|_| "client control queue is busy")?;
        manager.reload_requested = true;
        Ok(())
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handoff {
    created_at_ms: u64,
    outbox: Outbox,
}

pub(super) struct ReloadHandoff {
    path: PathBuf,
}
impl Drop for ReloadHandoff {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = fs::remove_dir(parent);
        }
    }
}

fn write_handoff(outbox: Outbox, directory: &Path) -> Result<ReloadHandoff, String> {
    outbox.validate()?;
    let bytes = serde_json::to_vec(&Handoff {
        created_at_ms: now_ms(),
        outbox,
    })
    .map_err(|_| "cannot serialize client outbox")?;
    if bytes.len() as u64 > MAX_HANDOFF_BYTES {
        return Err("client outbox exceeds reload handoff limit".into());
    }
    let folder = directory.join(format!("ssmcp-outbox-{}", random_id()?));
    let builder = &mut fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&folder)
        .map_err(|_| "cannot create private outbox directory")?;
    let handoff = ReloadHandoff {
        path: folder.join("state.json"),
    };
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(&handoff.path)
        .map_err(|_| "cannot create private client outbox")?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| "cannot save client outbox")?;
    Ok(handoff)
}

fn read_handoff(path: &Path) -> Result<Option<Outbox>, String> {
    let folder = path.parent().ok_or("invalid client outbox handoff path")?;
    let name = folder
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("ssmcp-outbox-"));
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some("state.json")
        || !name.is_some_and(|name| {
            name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err("invalid client outbox handoff path".into());
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect client outbox handoff".into()),
    };
    let directory =
        fs::symlink_metadata(folder).map_err(|_| "cannot inspect private outbox directory")?;
    if !metadata.is_file() || !directory.is_dir() || metadata.len() > MAX_HANDOFF_BYTES {
        return Err("invalid client outbox handoff file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid only reads the process identity.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid
            || directory.uid() != uid
            || metadata.permissions().mode() & 0o077 != 0
            || directory.permissions().mode() & 0o077 != 0
        {
            return Err("client outbox handoff must be private to the current user".into());
        }
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "cannot read client outbox handoff")?;
    let mut bytes = Vec::new();
    file.take(MAX_HANDOFF_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read client outbox handoff")?;
    if bytes.len() as u64 > MAX_HANDOFF_BYTES {
        return Err("client outbox handoff is oversized".into());
    }
    let handoff: Handoff =
        serde_json::from_slice(&bytes).map_err(|_| "invalid client outbox handoff")?;
    if now_ms().saturating_sub(handoff.created_at_ms) > 15 * 60 * 1000 {
        return Err("client outbox handoff expired".into());
    }
    handoff.outbox.validate()?;
    fs::remove_file(path).map_err(|_| "cannot consume client outbox handoff")?;
    let _ = fs::remove_dir(folder);
    Ok(Some(handoff.outbox))
}

pub(super) fn restore_handoff() -> Result<(), String> {
    with(|manager| {
        if manager.restored {
            return Ok(());
        }
        manager.restored = true;
        if let Some(path) = std::env::var_os(HANDOFF_ENV)
            && let Some(outbox) = read_handoff(Path::new(&path))?
        {
            manager.outbox = outbox;
        }
        Ok(())
    })
}

pub(super) fn prepare_reload(command: &mut std::process::Command) -> Result<ReloadHandoff, String> {
    let outbox = with(|manager| manager.outbox.clone());
    let handoff = write_handoff(outbox, &std::env::temp_dir())?;
    command.env(HANDOFF_ENV, &handoff.path);
    Ok(handoff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_for(room: &str, revision: u64, messages: Vec<SteeringMessage>) -> SteeringSnapshot {
        SteeringSnapshot {
            schema_version: 1,
            client_id: room.into(),
            revision,
            agents: vec![SteeringAgent {
                agent_id: "agent_one".into(),
                name: "Actual member".into(),
                status: "working".into(),
                role: None,
            }],
            messages,
        }
    }

    fn command(
        manager: &mut Manager,
        receiver: &mut mpsc::Receiver<Transport>,
    ) -> (u64, SteeringCommand) {
        match receiver.try_recv().unwrap() {
            Transport::Wire { id } => {
                let command = manager.requests[&id].command.clone();
                assert!(manager.sent(id));
                (id, command)
            }
            Transport::Reload => panic!("unexpected reload"),
        }
    }

    fn ready(manager: &mut Manager, receiver: &mut mpsc::Receiver<Transport>, room: &str) {
        let (id, request) = command(manager, receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        manager
            .reply(id, Ok(snapshot_for(room, 0, vec![])))
            .unwrap();
    }

    fn publish(
        manager: &mut Manager,
        receiver: &mut mpsc::Receiver<Transport>,
        text: &str,
        target: Option<&str>,
    ) -> SteeringMessage {
        let message_id = manager.submit(text, target).unwrap();
        assert!(manager.events.is_empty());
        let (id, request) = command(manager, receiver);
        let SteeringCommand::Publish {
            message_id: sent_id,
            expected_revision,
            text: sent_text,
            target,
            ..
        } = request
        else {
            panic!("publish required");
        };
        assert_eq!(sent_id, message_id);
        assert_eq!(expected_revision, 0);
        assert_eq!(sent_text, text);
        let message = SteeringMessage {
            id: message_id,
            revision: 1,
            text: text.into(),
            target,
            state: SteeringState::Queued,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            delivery: None,
        };
        manager
            .reply(id, Ok(snapshot_for("room_one", 1, vec![message.clone()])))
            .unwrap();
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Accepted {
                id: message.id.clone(),
                text: text.into()
            })
        );
        message
    }

    #[test]
    fn only_new_public_delivery_receipts_enter_shared_memory_once() {
        for target in ["next", "agent_one"] {
            let mut manager = Manager::default();
            let mut receiver = manager.connect("room_one", crate::wire::VERSION);
            ready(&mut manager, &mut receiver, "room_one");
            let source = "原始请求 @notes.txt";
            let mut message = publish(&mut manager, &mut receiver, source, Some(target));
            assert!(
                manager.delivered_memory.is_empty(),
                "publication is not delivery"
            );
            message.text = expanded_reference_fixture(source, "expanded file contents");
            message.state = SteeringState::Delivered;
            message.revision = 2;
            message.delivery = Some(crate::wire::SteeringDelivery {
                run_id: "delivery_run".into(),
                agent_id: Some("agent_one".into()),
                included_at_ms: 42,
            });
            let snapshot = snapshot_for("room_one", 2, vec![message.clone()]);
            manager.merge(&snapshot).unwrap();
            if target == "next" {
                assert_eq!(
                    manager.delivered_memory,
                    vec![DeliveredMemory {
                        id: message.id.clone(),
                        text: source.into(),
                        revision: 2,
                        target: "next".into(),
                    }]
                );
            } else {
                assert!(
                    manager.delivered_memory.is_empty(),
                    "exact-agent requests remain private"
                );
            }
            manager.delivered_memory.clear();
            manager.merge(&snapshot).unwrap();
            assert!(
                manager.delivered_memory.is_empty(),
                "a repeated terminal snapshot is not new memory"
            );

            let mut restored = Manager::default();
            restored.connect("room_one", crate::wire::VERSION);
            restored.merge(&snapshot).unwrap();
            assert!(
                restored.delivered_memory.is_empty(),
                "imported history must not enter the active branch"
            );
        }
    }

    #[test]
    fn unreceipted_or_already_terminal_requests_do_not_enter_memory() {
        for cancelled in [false, true] {
            let mut manager = Manager::default();
            let mut receiver = manager.connect("room_one", crate::wire::VERSION);
            ready(&mut manager, &mut receiver, "room_one");
            let mut message = publish(&mut manager, &mut receiver, "public request", None);
            if cancelled {
                manager.outbox.records[0].message.state = SteeringState::Cancelled;
            }
            message.state = SteeringState::Delivered;
            message.revision = 2;
            if cancelled {
                message.delivery = Some(crate::wire::SteeringDelivery {
                    run_id: "late_receipt".into(),
                    agent_id: None,
                    included_at_ms: 42,
                });
            }
            manager
                .merge(&snapshot_for("room_one", 2, vec![message]))
                .unwrap();
            assert!(manager.delivered_memory.is_empty());
        }
    }

    #[test]
    fn headless_publish_ack_produces_one_history_record_without_consuming_ui_events() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let text = "Native UI request with no terminal";
        let message_id = manager.submit(text, None).unwrap();
        assert!(manager.accepted_history.is_empty());
        let (request_id, _) = command(&mut manager, &mut receiver);
        assert!(manager.accepted_history.is_empty());
        let mut accepted = manager.outbox.records[0].message.clone();
        accepted.revision = 1;
        let snapshot = snapshot_for("room_one", 1, vec![accepted]);
        manager.reply(request_id, Ok(snapshot.clone())).unwrap();
        assert_eq!(manager.events.len(), 1, "no frontend consumed the UI event");
        assert_eq!(
            std::mem::take(&mut manager.accepted_history),
            vec![AcceptedHistory {
                id: message_id,
                text: text.into(),
            }]
        );
        manager.reply(request_id, Ok(snapshot)).unwrap();
        assert!(
            manager.accepted_history.is_empty(),
            "duplicate ACK does not create history"
        );
        assert_eq!(manager.events.len(), 1, "history does not drain UI events");
    }

    #[tokio::test]
    async fn reference_parse_failure_never_becomes_sendable_and_preserves_chinese_draft() {
        let directory = std::env::temp_dir().join(format!(
            "ssmcp-steering-reference-test-{}",
            random_id().unwrap()
        ));
        fs::create_dir(&directory).unwrap();
        let workspace = crate::workspace::Workspace::new(directory.clone()).unwrap();
        let context = crate::context::Context::new(directory.clone(), vec![]).unwrap();
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let draft = "保留这份中文原稿 @\"未闭合";
        let message_id = manager.submit(draft, Some("agent_one")).unwrap();
        let Transport::Wire { id, .. } = receiver.try_recv().unwrap() else {
            panic!("publish required");
        };
        let error = super::super::references::expand(draft, &workspace, &context)
            .await
            .unwrap_err();
        drop(workspace);
        drop(context);
        fs::remove_dir_all(directory).unwrap();
        manager.fail_unsent(id, &error);
        assert!(manager.pending_command(id).is_none());
        assert!(
            !manager.sent(id),
            "the sender cannot write a failed request"
        );
        let record = &manager.outbox.records[manager.index(&message_id).unwrap()];
        assert_eq!(record.message.text, draft);
        assert_eq!(record.message.target, "agent_one");
        assert_eq!(record.message.revision, 0);
        assert_eq!(record.message.state, SteeringState::Editing);
        assert!(!record.ever_sent);
        assert!(!record.uncertain);
        assert!(record.safe_to_publish);
        assert!(record.pending.is_none());
        assert!(
            matches!(manager.events.pop_front(), Some(UiEvent::Failed { id, .. }) if id == message_id)
        );
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Editable {
                id: message_id,
                text: draft.into(),
            })
        );
        manager.poll().unwrap();
        while let Ok(transport) = receiver.try_recv() {
            assert!(
                matches!(
                    transport,
                    Transport::Wire { id } if matches!(manager.requests[&id].command, SteeringCommand::Snapshot {})
                ),
                "local validation failure must not automatically retry"
            );
        }
    }

    #[test]
    fn failed_reference_edit_keeps_revision_target_and_draft_through_old_snapshots() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut old = publish(&mut manager, &mut receiver, "earlier contents", None);
        manager.take_back(&old.id).unwrap();
        let (takeback_id, _) = command(&mut manager, &mut receiver);
        old.state = SteeringState::Editing;
        old.revision = 2;
        manager
            .reply(
                takeback_id,
                Ok(snapshot_for("room_one", 2, vec![old.clone()])),
            )
            .unwrap();
        manager.events.clear();
        let draft = "新的中文要求 @missing.txt";
        manager
            .publish_edit(&old.id, draft, Some("agent_one"))
            .unwrap();
        let Transport::Wire { id, .. } = receiver.try_recv().unwrap() else {
            panic!("publish required");
        };
        // An unrelated snapshot can arrive while asynchronous reference reads run.
        manager
            .merge(&snapshot_for("room_one", 2, vec![old.clone()]))
            .unwrap();
        assert_eq!(manager.outbox.records[0].message.text, draft);
        manager.fail_unsent(id, "Referenced file was not found.");
        manager
            .merge(&snapshot_for("room_one", 2, vec![old.clone()]))
            .unwrap();
        let record = &manager.outbox.records[0];
        assert!(
            record.ever_sent,
            "prior accepted revisions retain their identity"
        );
        assert!(!record.uncertain);
        assert!(record.safe_to_publish);
        assert_eq!(record.message.revision, 2);
        assert_eq!(record.message.target, "agent_one");
        assert_eq!(record.message.text, draft);
        assert_eq!(record.message.state, SteeringState::Editing);
        assert_eq!(record.room_id.as_deref(), Some("room_one"));
        assert!(!manager.sent(id));
        old.state = SteeringState::Delivered;
        old.revision = 3;
        manager
            .merge(&snapshot_for("room_one", 3, vec![old.clone()]))
            .unwrap();
        assert_eq!(
            manager.outbox.records[0].message, old,
            "new authoritative revisions still win over local edits"
        );
    }

    fn expanded_reference_fixture(text: &str, preview: &str) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{text}\n\n<ssmcp-references-v1>\n{}\n</ssmcp-references-v1>",
            json!({
                "schema_version": 1,
                "source_sha256": format!("{:x}", Sha256::digest(text.as_bytes())),
                "references": [{"kind": "text", "path": "notes.txt", "preview": preview}],
            })
        )
    }

    #[test]
    fn prepared_publish_freezes_command_payload_and_ui_events_keep_the_source() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let draft = "阅读这份笔记 @notes.txt";
        let message_id = manager.submit(draft, None).unwrap();
        let Transport::Wire { id, .. } = receiver.try_recv().unwrap() else {
            panic!("publish required");
        };
        manager.requests.get_mut(&id).unwrap().queued_at = Instant::now() - REQUEST_TIMEOUT * 2;
        manager.poll().unwrap();
        assert!(
            manager.requests.contains_key(&id),
            "preparation time cannot consume the server acknowledgement deadline"
        );
        assert!(manager.events.is_empty());
        let (snapshot_id, snapshot) = command(&mut manager, &mut receiver);
        assert_eq!(snapshot, SteeringCommand::Snapshot {});
        manager
            .reply(snapshot_id, Ok(snapshot_for("room_one", 0, vec![])))
            .unwrap();

        let expanded = expanded_reference_fixture(draft, "first file contents");
        let prepared_command = manager
            .prepare_publish(id, expanded.clone())
            .unwrap()
            .unwrap();
        let second = expanded_reference_fixture(draft, "changed file contents");
        assert_eq!(
            manager.prepare_publish(id, second).unwrap(),
            Some(prepared_command.clone())
        );
        assert_eq!(manager.pending_command(id), Some(prepared_command));
        assert_eq!(manager.outbox.records[0].message.text, expanded);
        let started = Instant::now();
        assert!(manager.sent(id));
        assert!(manager.requests[&id].queued_at >= started);
        assert!(!manager.sent(id), "one transport frame per queued request");
        let mut accepted = manager.outbox.records[0].message.clone();
        accepted.revision = 1;
        manager
            .reply(id, Ok(snapshot_for("room_one", 1, vec![accepted.clone()])))
            .unwrap();
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Accepted {
                id: message_id.clone(),
                text: draft.into()
            })
        );
        manager.take_back(&message_id).unwrap();
        let (takeback_id, _) = command(&mut manager, &mut receiver);
        accepted.state = SteeringState::Editing;
        accepted.revision = 2;
        manager
            .reply(takeback_id, Ok(snapshot_for("room_one", 2, vec![accepted])))
            .unwrap();
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Editable {
                id: message_id,
                text: draft.into()
            })
        );
    }

    #[test]
    fn prepared_publish_survives_handoff_and_cancelled_transport_cannot_send() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let draft = "重启后仍然是同一份 @notes.txt";
        manager.submit(draft, None).unwrap();
        let Transport::Wire { id, .. } = receiver.try_recv().unwrap() else {
            panic!("publish required");
        };
        let frozen = expanded_reference_fixture(draft, "contents when first prepared");
        manager
            .prepare_publish(id, frozen.clone())
            .unwrap()
            .unwrap();
        manager.disconnect();
        assert!(manager.pending_command(id).is_none());
        assert!(
            manager
                .prepare_publish(id, "late completion".into())
                .unwrap()
                .is_none()
        );
        assert!(!manager.sent(id));
        let outbox = serde_json::from_slice(&serde_json::to_vec(&manager.outbox).unwrap()).unwrap();
        let mut restored = Manager {
            outbox,
            ..Manager::default()
        };
        let mut receiver = restored.connect("room_two", 7);
        ready(&mut restored, &mut receiver, "room_two");
        restored.poll().unwrap();
        let (_, request) = command(&mut restored, &mut receiver);
        assert!(matches!(request, SteeringCommand::Publish { text, .. } if text == frozen));
    }

    #[test]
    fn enqueue_ack_and_edit_use_the_same_message_and_expected_revision() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        manager.outbox.draft = Draft {
            text: "original request".into(),
            cursor: 16,
        };
        let mut message = publish(&mut manager, &mut receiver, "original request", None);
        assert_eq!(
            manager.outbox.draft.text, "original request",
            "only the composer may clear its matching draft"
        );
        manager.take_back(&message.id).unwrap();
        assert!(manager.events.is_empty());
        let (id, request) = command(&mut manager, &mut receiver);
        assert!(matches!(
            request,
            SteeringCommand::Takeback {
                expected_revision: 1,
                ..
            }
        ));
        message.state = SteeringState::Editing;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message.clone()])))
            .unwrap();
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Editable {
                id: message.id.clone(),
                text: message.text.clone()
            })
        );
        manager
            .publish_edit(&message.id, "改进后的 request", None)
            .unwrap();
        let (id, request) = command(&mut manager, &mut receiver);
        assert!(
            matches!(request, SteeringCommand::Publish { message_id, expected_revision: 2, .. } if message_id == message.id)
        );
        message.state = SteeringState::Queued;
        message.revision = 3;
        message.text = "改进后的 request".into();
        manager
            .reply(id, Ok(snapshot_for("room_one", 3, vec![message.clone()])))
            .unwrap();
        assert_eq!(manager.outbox.records.len(), 1);
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Accepted {
                id: message.id,
                text: message.text
            })
        );
    }

    #[test]
    fn delivery_winning_takeback_cannot_restore_or_republish_the_request() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut message = publish(&mut manager, &mut receiver, "one request", None);
        manager.take_back(&message.id).unwrap();
        let (id, _) = command(&mut manager, &mut receiver);
        manager
            .reply(
                id,
                Err("already_delivered: request was included in the next response".into()),
            )
            .unwrap();
        assert!(
            manager
                .events
                .iter()
                .all(|event| !matches!(event, UiEvent::Editable { .. }))
        );
        let (id, request) = command(&mut manager, &mut receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        message.state = SteeringState::Delivered;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message.clone()])))
            .unwrap();
        assert!(manager.publish_edit(&message.id, "retry", None).is_err());
        assert!(manager.cancel(&message.id).is_err());
        assert!(manager.retarget(&message.id, None).is_err());
        manager.disconnect();
        let mut receiver = manager.connect("room_two", 7);
        ready(&mut manager, &mut receiver, "room_two");
        manager.poll().unwrap();
        let (_, request) = command(&mut manager, &mut receiver);
        assert_eq!(
            request,
            SteeringCommand::Snapshot {},
            "delivered requests must never replay"
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn abrupt_reconnect_preserves_uncertain_requests_and_replays_only_never_sent_ones() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let message = publish(&mut manager, &mut receiver, "possibly delivered", None);
        manager.disconnect();
        let local = manager.submit("never sent", None).unwrap();
        let mut receiver = manager.connect("room_two", 7);
        ready(&mut manager, &mut receiver, "room_two");
        assert!(manager.outbox.records[manager.index(&message.id).unwrap()].uncertain);
        manager.poll().unwrap();
        let (_, request) = command(&mut manager, &mut receiver);
        assert!(
            matches!(request, SteeringCommand::Publish { message_id, expected_revision: 0, .. } if message_id == local)
        );
        let (_, request) = command(&mut manager, &mut receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn planned_reload_reclaims_pending_delivery_and_round_trips_a_private_outbox() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut message = publish(&mut manager, &mut receiver, "known undelivered", None);
        manager.outbox.draft = Draft {
            text: "未发送草稿".into(),
            cursor: "未发送".len(),
        };
        manager.outbox.context = DraftContext {
            editing: Some(message.id.clone()),
            target: Some("agent_one".into()),
            parked: Some(SavedDraft {
                text: "parked request".into(),
                cursor: 6,
                target: Some("another_agent".into()),
            }),
        };
        manager.begin_reload().unwrap();
        assert!(manager.submit("blocked during reload", None).is_err());
        let (id, _) = command(&mut manager, &mut receiver);
        manager
            .reply(id, Ok(snapshot_for("room_one", 1, vec![message.clone()])))
            .unwrap();
        assert!(!manager.reload_ready().unwrap());
        let (id, request) = command(&mut manager, &mut receiver);
        assert!(matches!(
            request,
            SteeringCommand::Takeback {
                expected_revision: 1,
                ..
            }
        ));
        message.state = SteeringState::Editing;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message])))
            .unwrap();
        assert!(manager.reload_ready().unwrap());
        assert!(
            manager.events.is_empty(),
            "reload reclamation must not overwrite the active draft"
        );
        let ticket = write_handoff(manager.outbox.clone(), &std::env::temp_dir()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&ticket.path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(ticket.path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        let outbox = read_handoff(&ticket.path).unwrap().unwrap();
        assert!(!ticket.path.exists());
        assert!(
            read_handoff(&ticket.path).unwrap().is_none(),
            "handoff is consumed exactly once"
        );
        assert_eq!(outbox.draft.text, "未发送草稿");
        assert_eq!(outbox.draft.cursor, "未发送".len());
        assert_eq!(
            outbox.context, manager.outbox.context,
            "editing identity and displaced draft recipient must survive reload"
        );
        let mut restored = Manager {
            outbox,
            ..Manager::default()
        };
        let mut receiver = restored.connect("room_two", 7);
        assert_eq!(
            restored.snapshot()["queue"][0]["status"],
            "sending",
            "a reclaimed record is not server-queued in its replacement room yet"
        );
        ready(&mut restored, &mut receiver, "room_two");
        restored.poll().unwrap();
        let (id, request) = command(&mut restored, &mut receiver);
        assert!(
            matches!(request, SteeringCommand::Publish { expected_revision: 0, text, target, .. } if text == "known undelivered" && target == "next")
        );
        assert_eq!(restored.snapshot()["queue"][0]["status"], "sending");
        let mut accepted = restored.outbox.records[0].message.clone();
        accepted.revision = 1;
        restored
            .reply(id, Ok(snapshot_for("room_two", 1, vec![accepted])))
            .unwrap();
        assert_eq!(restored.snapshot()["queue"][0]["status"], "queued");
    }

    fn pending_reload_takeback(
        manager: &mut Manager,
        receiver: &mut mpsc::Receiver<Transport>,
        message: &SteeringMessage,
    ) -> u64 {
        manager.begin_reload().unwrap();
        let (id, request) = command(manager, receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        manager
            .reply(id, Ok(snapshot_for("room_one", 1, vec![message.clone()])))
            .unwrap();
        assert!(!manager.reload_ready().unwrap());
        let (id, request) = command(manager, receiver);
        assert!(matches!(
            request,
            SteeringCommand::Takeback {
                message_id,
                expected_revision: 1,
                ..
            } if message_id == message.id
        ));
        id
    }

    #[test]
    fn aborted_reload_requeues_confirmed_reclaims_without_replacing_the_draft() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut message = publish(&mut manager, &mut receiver, "keep this request", None);
        manager.outbox.draft = Draft {
            text: "正在编写的新要求".into(),
            cursor: "正在编写".len(),
        };
        manager.outbox.context = DraftContext {
            target: Some("agent_one".into()),
            ..DraftContext::default()
        };
        let original_draft = manager.outbox.draft.clone();
        let original_context = manager.outbox.context.clone();
        let id = pending_reload_takeback(&mut manager, &mut receiver, &message);
        message.state = SteeringState::Editing;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message.clone()])))
            .unwrap();
        assert!(manager.reload_ready().unwrap());

        manager.abort_reload("server did not confirm reload");
        assert!(!manager.frozen);
        assert!(!manager.reload_requested);
        assert_eq!(manager.snapshot()["queue"][0]["status"], "sending");
        assert_eq!(manager.outbox.draft.text, original_draft.text);
        assert_eq!(manager.outbox.draft.cursor, original_draft.cursor);
        assert_eq!(manager.outbox.context, original_context);
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Failed {
                id: String::new(),
                message: "server did not confirm reload".into()
            })
        );
        assert!(manager.events.is_empty());

        manager.poll().unwrap();
        let (id, request) = command(&mut manager, &mut receiver);
        assert!(matches!(
            request,
            SteeringCommand::Publish {
                message_id,
                expected_revision: 2,
                text,
                target,
                ..
            } if message_id == message.id && text == message.text && target == "next"
        ));
        message.state = SteeringState::Queued;
        message.revision = 3;
        manager
            .reply(id, Ok(snapshot_for("room_one", 3, vec![message])))
            .unwrap();
        assert_eq!(manager.snapshot()["queue"][0]["status"], "queued");
        assert!(!manager.outbox.records[0].resume_after_reload);
        assert!(matches!(
            command(&mut manager, &mut receiver).1,
            SteeringCommand::Snapshot {}
        ));
        manager.poll().unwrap();
        assert!(
            receiver.try_recv().is_err(),
            "the request must be republished once"
        );
    }

    #[test]
    fn aborted_reload_waits_for_a_late_takeback_ack_before_republishing() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut message = publish(&mut manager, &mut receiver, "wait for the receipt", None);
        let id = pending_reload_takeback(&mut manager, &mut receiver, &message);

        manager.abort_reload("reclamation timed out");
        manager.poll().unwrap();
        let (snapshot_id, request) = command(&mut manager, &mut receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        assert!(
            receiver.try_recv().is_err(),
            "an unconfirmed takeback must not resend"
        );
        assert!(!manager.outbox.records[0].safe_to_publish);
        assert!(manager.outbox.records[0].pending.is_some());

        message.state = SteeringState::Editing;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message.clone()])))
            .unwrap();
        manager
            .reply(
                snapshot_id,
                Ok(snapshot_for("room_one", 2, vec![message.clone()])),
            )
            .unwrap();
        manager.poll().unwrap();
        assert!(matches!(
            command(&mut manager, &mut receiver).1,
            SteeringCommand::Publish {
                message_id,
                expected_revision: 2,
                ..
            } if message_id == message.id
        ));
        assert!(!manager.outbox.records[0].resume_after_reload);
    }

    #[test]
    fn aborted_reload_preserves_manual_edits_and_uncertain_takebacks() {
        for uncertain in [false, true] {
            let mut manager = Manager::default();
            let mut receiver = manager.connect("room_one", 7);
            ready(&mut manager, &mut receiver, "room_one");
            let mut message = publish(&mut manager, &mut receiver, "review explicitly", None);
            if uncertain {
                let id = pending_reload_takeback(&mut manager, &mut receiver, &message);
                manager.reply(id, Err("receipt lost".into())).unwrap();
            } else {
                manager.take_back(&message.id).unwrap();
                let (id, _) = command(&mut manager, &mut receiver);
                message.state = SteeringState::Editing;
                message.revision = 2;
                manager
                    .reply(id, Ok(snapshot_for("room_one", 2, vec![message])))
                    .unwrap();
                manager.frozen = true;
            }

            manager.abort_reload("reload cancelled");
            manager.poll().unwrap();
            assert_eq!(
                manager.snapshot()["queue"][0]["status"],
                if uncertain { "uncertain" } else { "editing" }
            );
            assert_eq!(manager.outbox.records[0].resume_after_reload, uncertain);
            let (id, request) = command(&mut manager, &mut receiver);
            assert_eq!(request, SteeringCommand::Snapshot {});
            assert!(
                receiver.try_recv().is_err(),
                "review-required content must not be resent"
            );
            if uncertain {
                // A lost takeback ACK is recoverable once a fresh authoritative
                // snapshot proves that the same-room request was reclaimed.
                let mut confirmed = manager.outbox.records[0].message.clone();
                confirmed.state = SteeringState::Editing;
                confirmed.revision = 2;
                manager
                    .reply(id, Ok(snapshot_for("room_one", 2, vec![confirmed])))
                    .unwrap();
                manager.poll().unwrap();
                assert!(matches!(
                    command(&mut manager, &mut receiver).1,
                    SteeringCommand::Publish {
                        expected_revision: 2,
                        ..
                    }
                ));
            }
        }
    }
    #[test]
    fn exact_agent_targets_need_explicit_retarget_after_a_room_change() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let mut message = publish(
            &mut manager,
            &mut receiver,
            "agent specific",
            Some("agent_one"),
        );
        manager.take_back(&message.id).unwrap();
        let (id, _) = command(&mut manager, &mut receiver);
        message.state = SteeringState::Editing;
        message.revision = 2;
        manager
            .reply(id, Ok(snapshot_for("room_one", 2, vec![message.clone()])))
            .unwrap();
        manager.events.clear();
        manager.disconnect();
        let mut receiver = manager.connect("room_two", 7);
        ready(&mut manager, &mut receiver, "room_two");
        assert!(manager.outbox.records[0].retarget_required);
        assert!(
            manager
                .publish_edit(&message.id, "agent specific", None)
                .is_err()
        );
        manager.retarget(&message.id, None).unwrap();
        assert_eq!(manager.outbox.records[0].message.target, "next");
        assert_eq!(
            manager.outbox.records[0].message.state,
            SteeringState::Editing
        );
        assert!(
            receiver.try_recv().is_err(),
            "retargeting alone must not send a request"
        );
        manager
            .publish_edit(&message.id, "explicitly resubmitted", None)
            .unwrap();
        let (_, request) = command(&mut manager, &mut receiver);
        assert!(
            matches!(request, SteeringCommand::Publish { expected_revision: 0, target, .. } if target == "next")
        );
    }

    #[test]
    fn old_gateways_never_receive_new_frames_and_invalid_input_cannot_change_the_outbox() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("old_room", 6);
        manager.poll().unwrap();
        assert!(receiver.try_recv().is_err());
        assert!(manager.submit("local draft", None).is_err());
        manager.disconnect();
        for text in [
            "".to_owned(),
            " \n\t".to_owned(),
            "\x1b[2J".to_owned(),
            "x".repeat(MAX_USER_MESSAGE_BYTES + 1),
        ] {
            assert!(manager.submit(&text, None).is_err());
        }
        assert!(manager.outbox.records.is_empty());
        assert!(manager.submit("valid\n多行\trequest", None).is_ok());
    }

    #[test]
    fn offline_unsent_request_can_be_explicitly_retargeted_into_editing() {
        let mut manager = Manager::default();
        let id = manager.submit("review this offline draft", None).unwrap();
        assert_eq!(manager.outbox.records[0].status(false), "offline");
        manager.retarget(&id, None).unwrap();
        let record = &manager.outbox.records[0];
        assert_eq!(record.message.state, SteeringState::Editing);
        assert!(record.safe_to_publish);
        assert!(!record.ever_sent);
        assert!(manager.requests.is_empty());
        assert_eq!(
            manager.events.pop_front(),
            Some(UiEvent::Editable {
                id,
                text: "review this offline draft".into(),
            })
        );
    }

    #[test]
    fn draft_context_is_bounded_and_older_handoffs_remain_readable() {
        let mut previous = serde_json::to_value(Outbox::default()).unwrap();
        previous.as_object_mut().unwrap().remove("context");
        let previous: Outbox = serde_json::from_value(previous).unwrap();
        assert_eq!(previous.context, DraftContext::default());
        previous.validate().unwrap();
        for context in [
            DraftContext {
                editing: Some("private-fixture\n".into()),
                ..DraftContext::default()
            },
            DraftContext {
                target: Some("x".repeat(129)),
                ..DraftContext::default()
            },
            DraftContext {
                parked: Some(SavedDraft {
                    text: "界".into(),
                    cursor: 1,
                    target: None,
                }),
                ..DraftContext::default()
            },
            DraftContext {
                parked: Some(SavedDraft {
                    text: "x".repeat(MAX_USER_MESSAGE_BYTES + 1),
                    cursor: 0,
                    target: None,
                }),
                ..DraftContext::default()
            },
            DraftContext {
                parked: Some(SavedDraft {
                    text: String::new(),
                    cursor: 0,
                    target: Some("bad target".into()),
                }),
                ..DraftContext::default()
            },
        ] {
            let error = context.validate().unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn handoff_rejects_shared_permissions_without_consuming_private_data() {
        use std::os::unix::fs::PermissionsExt;
        let ticket = write_handoff(Outbox::default(), &std::env::temp_dir()).unwrap();
        fs::set_permissions(&ticket.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_handoff(&ticket.path).is_err());
        assert!(ticket.path.exists());
        fs::set_permissions(&ticket.path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_handoff(&ticket.path).unwrap().is_some());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn trimmed_server_history_marks_requests_uncertain_without_replaying() {
        let queue = crate::steering::Queue::default();
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        let (id, request) = command(&mut manager, &mut receiver);
        manager
            .reply(id, queue.command("room_one", vec![], request))
            .unwrap();
        let text = "x".repeat(MAX_USER_MESSAGE_BYTES);
        let mut first_id = String::new();
        for index in 0..30 {
            let message_id = manager.submit(&text, None).unwrap();
            if index == 0 {
                first_id = message_id;
            }
            let (id, request) = command(&mut manager, &mut receiver);
            manager
                .reply(id, queue.command("room_one", vec![], request))
                .unwrap();
            manager.events.clear();
        }
        assert_eq!(manager.outbox.records[0].status(true), "queued");
        let claimed = queue.claim("room_one", "run_one", None, 1, 64 * 1024);
        assert_eq!(claimed[0]["message_id"], first_id);

        // The next publish crosses the server's retained-byte limit. Its ACK
        // omits the first delivered record before this client has observed it.
        manager.submit(&text, None).unwrap();
        let (id, request) = command(&mut manager, &mut receiver);
        let snapshot = queue.command("room_one", vec![], request).unwrap();
        assert!(
            !snapshot
                .messages
                .iter()
                .any(|message| message.id == first_id)
        );
        manager.reply(id, Ok(snapshot)).unwrap();
        let record = &manager.outbox.records[manager.index(&first_id).unwrap()];
        assert_eq!(record.status(true), "uncertain");
        assert!(!record.safe_to_publish);
        assert!(
            record
                .error
                .as_deref()
                .unwrap()
                .contains("no longer retains")
        );
        assert!(
            record.message.delivery.is_none(),
            "absence is not a delivery receipt"
        );

        manager.poll().unwrap();
        let (_, request) = command(&mut manager, &mut receiver);
        assert_eq!(request, SteeringCommand::Snapshot {});
        assert!(
            receiver.try_recv().is_err(),
            "expired history never replays automatically"
        );
    }

    #[test]
    fn missing_history_reconciliation_ignores_stale_snapshots_and_pending_mutations() {
        let mut manager = Manager::default();
        let mut receiver = manager.connect("room_one", 7);
        ready(&mut manager, &mut receiver, "room_one");
        let message = publish(&mut manager, &mut receiver, "confirmed queued", None);
        manager.merge(&snapshot_for("room_one", 0, vec![])).unwrap();
        assert_eq!(manager.outbox.records[0].status(true), "queued");

        let pending_id = manager.submit("still being published", None).unwrap();
        let _ = command(&mut manager, &mut receiver);
        manager
            .merge(&snapshot_for("room_one", 1, vec![message]))
            .unwrap();
        let pending = &manager.outbox.records[manager.index(&pending_id).unwrap()];
        assert!(!pending.uncertain);
        assert_eq!(pending.status(true), "sending");
    }
}
