//! Bounded process-local conversation records for live display and request ownership.
//! No history, archive or checkpoint is read from or written to disk.
#[cfg(test)]
pub(crate) mod archive;
#[cfg(test)]
pub(crate) mod descriptions;
pub(crate) mod goals;
mod listener;
pub(crate) mod openai;
mod ownership;
pub(crate) use listener::{
    ensure_current_id, record_listener_result, record_request_in, with_listener_conversation,
};
#[cfg(test)]
pub(crate) use ownership::with_test_history;
pub(crate) use ownership::{
    HistoryId, HistoryOwner, capture, current_owner, record_owned_agent, record_owned_answer,
    record_owned_input, record_owned_program_reply, record_owned_request, remember_visible_reply,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA: &str = "ssmcp.conversation.v1";
const MAX_CONTENT: usize = 16 * 1024;
const MAX_MESSAGES: usize = 500;
const MAX_SESSION_BYTES: usize = 512 * 1024;
const MAX_SESSIONS: usize = 64;
const MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_SCAN_ENTRIES: usize = 4096;
const HISTORY_RETIRED: &str = "tool_retired: conversation history, archives and recovery have been removed; use ssmcp.ui.events for live process logs";

#[derive(Default)]
struct HistoryStore {
    state: Mutex<Option<History>>,
    current: ownership::CurrentOwner,
}
static HISTORY: OnceLock<HistoryStore> = OnceLock::new();

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Message {
    pub id: String,
    pub role: String,
    pub content: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_origin: Option<super::messages::InputOrigin>,
    pub created_at_ms: u64,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionMetadata {
    pub schema: String,
    pub schema_version: u32,
    pub id: String,
    pub workspace: String,
    pub created_at_ms: u64,
    #[serde(default)]
    pub truncated_messages: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct Conversation {
    pub metadata: SessionMetadata,
    pub messages: Vec<Message>,
}

#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SessionSummary {
    pub id: String,
    pub workspace: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: usize,
    pub truncated_messages: u64,
    pub current: bool,
    pub archived: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn random_id(prefix: &str) -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|_| "cannot create history identity")?;
    let suffix: String = crate::hex::encode(bytes);
    Ok(format!("{prefix}_{suffix}"))
}

fn valid_id(id: &str, prefix: &str) -> bool {
    id.strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('_'))
        .is_some_and(|suffix| {
            suffix.len() == 32
                && suffix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
}

pub(crate) fn valid_message_id(id: &str) -> bool {
    valid_id(id, "message")
}

fn safe_text(text: &str) -> String {
    crate::audit::event(json!(text))
        .as_str()
        .unwrap_or("[redacted]")
        .to_owned()
}

pub(super) fn validate_content(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_CONTENT {
        return Err("history text must contain 1..=16384 UTF-8 bytes".into());
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err("history text contains unsupported control characters".into());
    }
    Ok(())
}

pub(crate) fn request_identity(request_id: &str, text: &str) -> Result<String, String> {
    if request_id.is_empty()
        || request_id.len() > 128
        || !request_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err("invalid request identity for conversation history".into());
    }
    let mut digest = Sha256::new();
    digest.update(b"ssmcp.history.request.v1\0");
    digest.update(request_id.as_bytes());
    digest.update(b"\0");
    digest.update(text.as_bytes());
    let digest = digest.finalize();
    let suffix: String = crate::hex::encode(&digest[..16]);
    Ok(format!("message_{suffix}"))
}

fn role_and_kind(role: &str) -> Result<(&'static str, &'static str), String> {
    match role {
        "user" => Ok(("user", "requirement")),
        "agent" | "assistant" => Ok(("assistant", "agent_message")),
        "answer" => Ok(("user", "answer")),
        "system" => Ok(("system", "system_message")),
        "tool" => Ok(("tool", "tool_message")),
        _ => Err("history role must be system, user, assistant, tool, agent or answer".into()),
    }
}

pub(crate) fn default_directory() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("cannot determine local conversation state directory")?;
    #[cfg(not(windows))]
    let root =
        if let Some(path) = std::env::var_os("XDG_STATE_HOME").filter(|path| !path.is_empty()) {
            let path = PathBuf::from(path);
            if !path.is_absolute() {
                return Err("XDG_STATE_HOME must be an absolute directory".into());
            }
            path
        } else {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or("cannot determine local conversation state directory")?
                .join(".local/state")
        };
    Ok(root.join("ssmcp/conversations"))
}

pub(crate) fn workspace() -> Result<String, String> {
    let cwd = std::env::current_dir().map_err(|error| {
        format!("cannot read current directory for conversation workspace: {error}")
    })?;
    canonical_workspace(&cwd)
}

fn canonical_workspace(cwd: &Path) -> Result<String, String> {
    let path = fs::canonicalize(cwd).map_err(|error| {
        format!(
            "cannot resolve conversation workspace {}: {error}",
            cwd.display()
        )
    })?;
    let value = path
        .to_str()
        .ok_or("conversation workspace must be UTF-8")?;
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("conversation workspace path is invalid or oversized".into());
    }
    Ok(value.to_owned())
}

struct Active {
    metadata: SessionMetadata,
}

struct History {
    workspace: String,
    active: Option<Active>,
    conversations: std::collections::BTreeMap<String, Conversation>,
    goals: std::collections::BTreeMap<String, String>,
    openai: std::collections::BTreeMap<String, openai::Saved>,
    ownership: ownership::Ownership,
}

fn with<T>(operation: impl FnOnce(&mut History) -> Result<T, String>) -> Result<T, String> {
    #[cfg(test)]
    if let Ok(store) = ownership::TEST_HISTORY.try_with(std::sync::Arc::clone) {
        let mut state = store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        return operation(state.as_mut().expect("fixture log was initialized"));
    }
    let store = HISTORY.get_or_init(HistoryStore::default);
    let mut state = store
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.is_none() {
        let mut history = History::new(workspace()?);
        history.ownership.current = store.current.clone();
        *state = Some(history);
    }
    operation(state.as_mut().expect("live log was initialized"))
}

impl History {
    fn new(workspace: String) -> Self {
        Self {
            workspace,
            active: None,
            conversations: Default::default(),
            goals: Default::default(),
            openai: Default::default(),
            ownership: Default::default(),
        }
    }

    #[cfg(test)]
    fn with_workspace(_path: PathBuf, workspace: String) -> Result<Self, String> {
        Ok(Self::new(workspace))
    }

    fn load(&self, id: &str) -> Result<Conversation, String> {
        if !valid_id(id, "conversation") {
            return Err("invalid live conversation identifier".into());
        }
        self.conversations.get(id).cloned().ok_or_else(|| {
            "live_log_unavailable: the original process log is no longer retained".into()
        })
    }

    fn encoded_bytes(conversation: &Conversation) -> Result<usize, String> {
        serde_json::to_vec(conversation)
            .map(|bytes| bytes.len())
            .map_err(|_| "cannot measure live log size".into())
    }

    fn retain(&mut self, id: &str, bytes: usize) -> Result<(), String> {
        let mut count = 1;
        let mut total = bytes;
        let mut candidates = Vec::new();
        for (other_id, conversation) in &self.conversations {
            if other_id == id {
                continue;
            }
            let size = Self::encoded_bytes(conversation)?;
            count += 1;
            total = total.saturating_add(size);
            if !self.ownership.is_live(other_id) {
                let updated = conversation
                    .messages
                    .last()
                    .map_or(conversation.metadata.created_at_ms, |message| {
                        message.created_at_ms
                    });
                candidates.push((updated, other_id.clone(), size));
            }
        }
        candidates.sort();
        let mut reclaim = Vec::new();
        for (_, other_id, size) in candidates {
            if count <= MAX_SESSIONS && total <= MAX_TOTAL_BYTES {
                break;
            }
            reclaim.push(other_id);
            count -= 1;
            total = total.saturating_sub(size);
        }
        if count > MAX_SESSIONS || total > MAX_TOTAL_BYTES {
            return Err(
                "live_log_capacity: active request owners fill the bounded process log".into(),
            );
        }
        for other_id in reclaim {
            self.conversations.remove(&other_id);
            self.goals.remove(&other_id);
            self.openai.remove(&other_id);
        }
        Ok(())
    }

    fn materialize(&mut self, owner: &HistoryOwner) -> Result<(), String> {
        if !self.ownership.pending(owner)? {
            self.load(owner.id())?;
            return Ok(());
        }
        if self.conversations.contains_key(owner.id()) {
            return Err("history_owner_conflict: reserved live conversation already exists".into());
        }
        let metadata = SessionMetadata {
            schema: SCHEMA.into(),
            schema_version: 1,
            id: owner.id().to_owned(),
            workspace: self.workspace.clone(),
            created_at_ms: now_ms(),
            truncated_messages: 0,
        };
        let conversation = Conversation {
            metadata: metadata.clone(),
            messages: Vec::new(),
        };
        self.retain(owner.id(), Self::encoded_bytes(&conversation)?)?;
        self.conversations
            .insert(owner.id().to_owned(), conversation);
        self.ownership.materialized(owner);
        if self.ownership.current.get().as_ref() == Some(owner) {
            self.active = Some(Active { metadata });
        }
        Ok(())
    }

    fn record(&mut self, role: &str, text: &str) -> Result<Option<Committed>, String> {
        self.record_identified(role, text, None)
    }

    fn record_agent(&mut self, id: &str, text: &str) -> Result<Option<Committed>, String> {
        if !valid_message_id(id) {
            return Err("invalid agent message identity".into());
        }
        self.record_identified("assistant", text, Some(id.to_owned()))
    }

    fn record_request(
        &mut self,
        request_id: &str,
        text: &str,
    ) -> Result<Option<Committed>, String> {
        self.record_identified("user", text, Some(request_identity(request_id, text)?))
    }

    fn record_identified(
        &mut self,
        role: &str,
        text: &str,
        identity: Option<String>,
    ) -> Result<Option<Committed>, String> {
        let owner = self.capture_owner()?;
        self.record_owned(&owner, role, text, identity)
    }

    fn record_owned(
        &mut self,
        owner: &HistoryOwner,
        role: &str,
        text: &str,
        identity: Option<String>,
    ) -> Result<Option<Committed>, String> {
        self.record_owned_with_origin(owner, role, text, identity, None)
    }

    fn record_owned_with_origin(
        &mut self,
        owner: &HistoryOwner,
        role: &str,
        text: &str,
        identity: Option<String>,
        input_origin: Option<super::messages::InputOrigin>,
    ) -> Result<Option<Committed>, String> {
        if input_origin.is_some() && role != "tool" {
            return Err("generated input must retain its non-human conversation role".into());
        }
        let (role, kind) = role_and_kind(role)?;
        validate_content(text)?;
        let pending = self.ownership.pending(owner)?;
        let mut conversation = if pending {
            if self.conversations.contains_key(owner.id()) {
                return Err(
                    "history_owner_conflict: reserved live conversation already exists".into(),
                );
            }
            Conversation {
                metadata: SessionMetadata {
                    schema: SCHEMA.into(),
                    schema_version: 1,
                    id: owner.id().to_owned(),
                    workspace: self.workspace.clone(),
                    created_at_ms: now_ms(),
                    truncated_messages: 0,
                },
                messages: Vec::new(),
            }
        } else {
            self.load(owner.id())
                .map_err(|error| format!("history_owner_unavailable: {error}"))?
        };
        let mut content = safe_text(text);
        let truncated = content.len() > MAX_CONTENT;
        if truncated {
            content.truncate(content.floor_char_boundary(MAX_CONTENT));
        }
        let id = match identity {
            Some(id) => {
                if let Some(previous) = conversation
                    .messages
                    .iter()
                    .find(|message| message.id == id)
                {
                    if previous.role == role
                        && previous.content == content
                        && previous.input_origin == input_origin
                    {
                        return Ok(None);
                    }
                    return Err(
                        "conversation request identity conflicts with a retained message".into(),
                    );
                }
                id
            }
            None => random_id("message")?,
        };
        let message = Message {
            id,
            role: role.into(),
            content,
            kind: kind.into(),
            input_origin,
            created_at_ms: now_ms(),
            truncated,
        };
        conversation.messages.push(message.clone());
        let mut bytes = Self::encoded_bytes(&conversation)?;
        while conversation.messages.len() > MAX_MESSAGES || bytes > MAX_SESSION_BYTES {
            conversation.messages.remove(0);
            conversation.metadata.truncated_messages =
                conversation.metadata.truncated_messages.saturating_add(1);
            bytes = Self::encoded_bytes(&conversation)?;
        }
        self.retain(owner.id(), bytes)?;
        if self.ownership.current.get().as_ref() == Some(owner) {
            self.active = Some(Active {
                metadata: conversation.metadata.clone(),
            });
        }
        self.conversations
            .insert(owner.id().to_owned(), conversation);
        self.ownership.materialized(owner);
        Ok(Some(Committed { message }))
    }

    fn recall_request(&mut self, request_id: &str, text: &str) -> Result<bool, String> {
        let identity = request_identity(request_id, text)?;
        let mut expected = safe_text(text);
        expected.truncate(expected.floor_char_boundary(MAX_CONTENT));
        for conversation in self.conversations.values_mut() {
            if let Some(index) = conversation
                .messages
                .iter()
                .position(|message| message.id == identity)
            {
                let message = &conversation.messages[index];
                if message.role != "user" || message.content != expected {
                    return Err("delivered request conflicts with its original live source".into());
                }
                conversation.messages.remove(index);
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(test)]
    fn summary(&self, conversation: &Conversation) -> SessionSummary {
        let title = conversation
            .messages
            .iter()
            .find(|message| message.role == "user")
            .or_else(|| conversation.messages.first())
            .map(|message| {
                message
                    .content
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect()
            })
            .unwrap_or_else(|| "Live session".into());
        SessionSummary {
            id: conversation.metadata.id.clone(),
            workspace: self.workspace.clone(),
            title,
            summary: None,
            created_at_ms: conversation.metadata.created_at_ms,
            updated_at_ms: conversation
                .messages
                .last()
                .map_or(conversation.metadata.created_at_ms, |message| {
                    message.created_at_ms
                }),
            message_count: conversation.messages.len(),
            truncated_messages: conversation.metadata.truncated_messages,
            current: self
                .active
                .as_ref()
                .is_some_and(|active| active.metadata.id == conversation.metadata.id),
            archived: false,
        }
    }

    #[cfg(test)]
    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        Ok(self
            .conversations
            .values()
            .map(|conversation| self.summary(conversation))
            .collect())
    }

    #[cfg(test)]
    fn read(&self, id: &str) -> Result<Conversation, String> {
        self.load(id)
    }

    fn current(&self) -> Result<Option<Conversation>, String> {
        self.active
            .as_ref()
            .map(|active| self.load(&active.metadata.id))
            .transpose()
    }

    #[cfg(test)]
    fn resume(&mut self, id: &str) -> Result<Conversation, String> {
        let conversation = self.load(id)?;
        self.active = Some(Active {
            metadata: conversation.metadata.clone(),
        });
        self.ownership.select(id, false);
        Ok(conversation)
    }

    #[cfg(test)]
    fn release_current(&mut self) {
        self.active = None;
        self.ownership.release();
    }
}

#[derive(Debug)]
struct Committed {
    message: Message,
}

fn remember_committed(_committed: Option<Committed>) {}

pub(crate) fn record(role: &str, text: &str) -> Result<(), String> {
    with(|history| history.record(role, text)).map(|_| ())
}

pub(crate) fn record_request(request_id: &str, text: &str) -> Result<(), String> {
    with(|history| history.record_request(request_id, text)).map(|_| ())
}

pub(crate) fn recall_request(request_id: &str, text: &str) -> Result<(), String> {
    with(|history| history.recall_request(request_id, text)).map(|_| ())
}

pub(crate) fn remember_delivered_request(
    _request_id: &str,
    _text: &str,
    _revision: u64,
    _target: &str,
) -> Result<(), String> {
    Ok(())
}

pub(crate) fn current() -> Result<Option<Conversation>, String> {
    with(|history| history.current())
}

#[cfg(test)]
pub(crate) fn release_current() -> Result<(), String> {
    with(|history| {
        history.release_current();
        super::messages::restore_copy(&[]);
        Ok(())
    })
}

#[cfg(all(test, unix))]
pub(crate) fn sessions() -> Result<Vec<SessionSummary>, String> {
    with(|history| history.sessions())
}
#[cfg(test)]
pub(crate) fn read(id: &str) -> Result<Conversation, String> {
    with(|history| history.read(id))
}

#[cfg(test)]
pub(crate) fn resume_test_history(id: &str) -> Result<Conversation, String> {
    ownership::TEST_HISTORY
        .try_with(|_| ())
        .map_err(|_| "no isolated live-log fixture")?;
    with(|history| history.resume(id))
}

#[cfg(test)]
pub(crate) fn resume(_id: &str) -> Result<Conversation, String> {
    Err(HISTORY_RETIRED.into())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsRequest {
    action: String,
    client_id: String,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    main: bool,
    #[serde(default)]
    cursor: Option<u64>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(super) fn call(arguments: Value) -> Result<Value, String> {
    if arguments.get("action").and_then(Value::as_str) != Some("thread") {
        return Err(HISTORY_RETIRED.into());
    }
    if arguments
        .as_object()
        .is_none_or(|object| object.values().any(Value::is_null))
    {
        return Err("live events expect an object; omit optional fields instead of null".into());
    }
    let request: EventsRequest =
        serde_json::from_value(arguments).map_err(|_| "expected a live thread event request")?;
    let limit = request.limit.unwrap_or(16);
    if request.action != "thread" || !(1..=64).contains(&limit) {
        return Err("live event page limit must be in 1..=64".into());
    }
    let source = crate::runtime::call_scope::Source::capture();
    if source.call.as_ref().and_then(|call| call.client_id()) != Some(request.client_id.as_str()) {
        return Err("live events require this native connection's real client_id".into());
    }
    if request.generation.as_ref().is_some_and(|value| {
        value.is_empty() || value.len() > 128 || value.chars().any(char::is_control)
    }) {
        return Err("invalid live event generation".into());
    }
    crate::session::attach::thread_history::page(
        &crate::session::attach::thread_history::Selector {
            client_id: request.client_id,
            agent_id: request.agent_id,
            main: request.main,
        },
        request.cursor,
        request.generation.as_deref(),
        limit,
    )
}

pub(super) fn tool() -> Value {
    json!({
        "name":"ssmcp.ui.events",
        "description":"Read the bounded live process event log. No conversation history, archive, checkpoint or replay is stored. Use this connection's real client_id; agent_id selects a verified thread and main selects the unbound main caller. Omit both for the process aggregate. Follow generation and next_cursor. The first page’s end_cursor is the exclusive upper bound for a finite read; later entries may arrive while paging. retention_gap_possible reports bounded log pruning.",
        "inputSchema":{
            "type":"object","required":["action","client_id"],"additionalProperties":false,
            "properties":{
                "action":{"const":"thread"},"client_id":{"type":"string","pattern":"^[0-9a-fA-F]{64}$"},
                "agent_id":{"type":"string","pattern":"^agent_[0-9a-fA-F]{32}$"},
                "main":{"type":"boolean","default":false},
                "cursor":{"type":"integer","minimum":0},
                "generation":{"type":"string","minLength":1,"maxLength":128},
                "limit":{"type":"integer","minimum":1,"maximum":64,"default":16}
            },
            "not":{"required":["agent_id","main"],"properties":{"main":{"const":true}}}
        },
        "annotations":{"readOnlyHint":true,"destructiveHint":false}
    })
}

// Private filesystem helpers remain for the client updater, which owns its own state.
pub(crate) struct SafeDirectory {
    path: PathBuf,
    #[cfg(unix)]
    directory: File,
    #[cfg(windows)]
    directory: cap_std::fs::Dir,
}

pub(crate) struct HistoryLock {
    _file: File,
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        // flock is owned by the open file description. A concurrent fork can
        // retain a duplicate until exec, so closing only our fd can leave a
        // completed transaction locked. Explicitly release our own lock first.
        #[cfg(unix)]
        while let Err(error) = self._file.unlock() {
            if error.kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
    }
}

fn private_tree(path: &Path) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err("history state directory must be absolute without parent traversal".into());
    }
    if let Some(parent) = path.parent() {
        private_tree(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("history directory must not contain symlinks or non-directory entries".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match crate::platform::private_directory(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let metadata = fs::symlink_metadata(path)
                        .map_err(|_| "cannot inspect new history directory")?;
                    if metadata.is_dir() && !metadata.file_type().is_symlink() {
                        Ok(())
                    } else {
                        Err("history directory was replaced while being created".into())
                    }
                }
                Err(_) => Err("cannot create private conversation directory".into()),
            }
        }
        Err(_) => Err("cannot inspect conversation directory".into()),
    }
}

fn private_file(file: &File) -> Result<(), String> {
    let metadata = file
        .metadata()
        .map_err(|_| "cannot inspect conversation file")?;
    if !metadata.is_file() {
        return Err("conversation storage requires a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
        {
            return Err(
                "conversation files must be privately owned with permissions 0600 and one link"
                    .into(),
            );
        }
    }
    #[cfg(windows)]
    crate::platform::windows::verify_file(file).map_err(|_| "conversation file is not private")?;
    Ok(())
}

impl SafeDirectory {
    pub(crate) fn at(path: PathBuf) -> Result<Self, String> {
        private_tree(&path)?;
        #[cfg(unix)]
        let directory = {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            let directory = fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
                .map_err(|_| "cannot open conversation directory")?;
            let metadata = directory
                .metadata()
                .map_err(|_| "cannot inspect conversation directory")?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(
                    "conversation directory must be privately owned with permissions 0700".into(),
                );
            }
            directory
        };
        #[cfg(windows)]
        let directory = crate::platform::windows::private_directory(&path)
            .map_err(|_| "conversation directory must have private permissions")?;
        Ok(Self { path, directory })
    }

    fn verify(&self) -> Result<(), String> {
        let metadata = fs::symlink_metadata(&self.path)
            .map_err(|_| "conversation directory is unavailable")?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err("conversation directory was replaced".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let original = self
                .directory
                .metadata()
                .map_err(|_| "conversation directory is unavailable")?;
            if metadata.dev() != original.dev()
                || metadata.ino() != original.ino()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.mode() & 0o077 != 0
            {
                return Err("conversation directory identity or permissions changed".into());
            }
        }
        Ok(())
    }

    pub(crate) fn open(
        &self,
        name: &str,
        write: bool,
        create: bool,
        exclusive: bool,
    ) -> Result<File, String> {
        self.verify()?;
        if name.is_empty() || name.contains(['/', '\\']) || name.contains('\0') {
            return Err("invalid conversation storage filename".into());
        }
        #[cfg(unix)]
        let file = {
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = std::ffi::CString::new(name)
                .map_err(|_| "invalid conversation storage filename")?;
            let flags = if write { libc::O_RDWR } else { libc::O_RDONLY }
                | if create { libc::O_CREAT } else { 0 }
                | if exclusive { libc::O_EXCL } else { 0 }
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC;
            let fd =
                unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err("cannot open private conversation file".into());
            }
            unsafe { File::from_raw_fd(fd) }
        };
        #[cfg(windows)]
        let file = {
            use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
            let mut options = cap_std::fs::OpenOptions::new();
            options
                .read(true)
                .write(write)
                .create(create && !exclusive)
                .create_new(exclusive)
                .follow(FollowSymlinks::No);
            self.directory
                .open_with(name, &options)
                .map_err(|_| "cannot open private conversation file")?
                .into_std()
        };
        private_file(&file)?;
        Ok(file)
    }

    pub(crate) fn lock(&self) -> Result<HistoryLock, String> {
        let file = self.open(".history.lock", true, true, false)?;
        crate::platform::lock_file(&file, true)
            .map_err(|_| "conversation storage is busy; retry shortly")?;
        Ok(HistoryLock { _file: file })
    }

    pub(crate) fn names(&self) -> Result<Vec<String>, String> {
        self.verify()?;
        #[cfg(unix)]
        let entries = fs::read_dir(&self.path).map_err(|_| "cannot list conversation history")?;
        #[cfg(windows)]
        let entries = self
            .directory
            .entries()
            .map_err(|_| "cannot list conversation history")?;
        let mut names = Vec::new();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_SCAN_ENTRIES {
                return Err("conversation directory has too many entries".into());
            }
            let entry = entry.map_err(|_| "cannot inspect conversation directory entry")?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_owned());
            }
        }
        self.verify()?;
        Ok(names)
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name).map_err(|_| "invalid history filename")?;
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err("cannot reclaim old conversation history".into());
            }
        }
        #[cfg(windows)]
        self.directory
            .remove_file(name)
            .map_err(|_| "cannot reclaim old conversation history")?;
        Ok(())
    }

    pub(crate) fn write_atomic(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > MAX_SESSION_BYTES {
            return Err("conversation exceeds its storage budget".into());
        }
        let temporary = format!(".{}.tmp", random_id("conversation")?);
        let mut file = self.open(&temporary, true, true, true)?;
        let outcome = (|| {
            file.write_all(bytes)
                .and_then(|_| file.sync_all())
                .map_err(|_| "cannot save conversation history")?;
            self.verify()?;
            // Any existing destination must be a private ordinary file. Rename
            // itself stays relative to the pinned directory and never follows it.
            let existing = self.names()?.iter().any(|entry| entry == name);
            if existing {
                self.open(name, false, false, false)?;
            }
            #[cfg(unix)]
            {
                use std::os::fd::AsRawFd;
                let from = std::ffi::CString::new(temporary.as_str())
                    .map_err(|_| "invalid temporary history name")?;
                let to = std::ffi::CString::new(name).map_err(|_| "invalid history name")?;
                if unsafe {
                    libc::renameat(
                        self.directory.as_raw_fd(),
                        from.as_ptr(),
                        self.directory.as_raw_fd(),
                        to.as_ptr(),
                    )
                } != 0
                {
                    return Err("cannot atomically replace conversation history".into());
                }
                self.directory
                    .sync_all()
                    .map_err(|_| "cannot flush conversation directory")?;
            }
            #[cfg(windows)]
            self.directory
                .rename(&temporary, &self.directory, name)
                .map_err(|_| "cannot atomically replace conversation history")?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = self.remove(&temporary);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) struct Temporary(PathBuf);
    impl Temporary {
        pub(super) fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(random_id("live_log_test").unwrap());
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
        pub(super) fn history(&self) -> History {
            History::with_workspace(self.0.join("conversations"), "/fixture/workspace".into())
                .unwrap()
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn recording_uses_only_bounded_memory_and_creates_no_state_directory() {
        let temporary = Temporary::new();
        let mut history = temporary.history();
        history
            .record_request("fixture", "Current request")
            .unwrap();
        history.record("assistant", "Current response").unwrap();
        assert_eq!(history.current().unwrap().unwrap().messages.len(), 2);
        assert!(fs::read_dir(&temporary.0).unwrap().next().is_none());
    }

    #[test]
    fn old_files_are_ignored_without_modification_or_recovery() {
        let temporary = Temporary::new();
        let directory = temporary.0.join("conversations");
        fs::create_dir(&directory).unwrap();
        let original = b"private old conversation data";
        let old = directory.join("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.jsonl");
        fs::write(&old, original).unwrap();
        let mut history = temporary.history();
        assert!(history.current().unwrap().is_none());
        assert!(history.sessions().unwrap().is_empty());
        assert!(
            history
                .read("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .is_err()
        );
        history.record("user", "New process only").unwrap();
        assert_eq!(fs::read(&old).unwrap(), original);
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        assert!(temporary.history().sessions().unwrap().is_empty());
    }

    #[test]
    fn live_record_bytes_and_message_count_are_bounded() {
        let temporary = Temporary::new();
        let mut history = temporary.history();
        let body = "x".repeat(MAX_CONTENT);
        for _ in 0..MAX_MESSAGES + 4 {
            history.record("user", &body).unwrap();
        }
        let current = history.current().unwrap().unwrap();
        assert!(current.messages.len() <= MAX_MESSAGES);
        assert!(History::encoded_bytes(&current).unwrap() <= MAX_SESSION_BYTES);
        assert!(current.metadata.truncated_messages > 0);
        assert!(fs::read_dir(&temporary.0).unwrap().next().is_none());
    }

    #[test]
    fn retained_message_identity_still_deduplicates_and_rejects_changed_results() {
        let temporary = Temporary::new();
        let mut history = temporary.history();
        let id = "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        history.record_agent(id, "Result").unwrap();
        assert!(history.record_agent(id, "Result").unwrap().is_none());
        assert!(history.record_agent(id, "Different result").is_err());
        assert_eq!(history.current().unwrap().unwrap().messages.len(), 1);
    }

    #[test]
    fn released_owner_cannot_write_into_the_next_live_session() {
        let temporary = Temporary::new();
        let mut history = temporary.history();
        history.record("user", "Original request").unwrap();
        let original = history.capture_owner().unwrap();
        history.release_current();
        history.record("user", "Later request").unwrap();
        assert!(
            history
                .record_owned(&original, "assistant", "Late response", None)
                .unwrap_err()
                .contains("history_owner_released")
        );
        assert_eq!(history.current().unwrap().unwrap().messages.len(), 1);
    }

    #[test]
    fn closed_process_contexts_are_bounded_and_never_written_to_disk() {
        let temporary = Temporary::new();
        let mut history = temporary.history();
        for _ in 0..MAX_SESSIONS + 4 {
            history.record("user", "Transient request").unwrap();
            history.release_current();
        }
        assert!(history.conversations.len() <= MAX_SESSIONS);
        assert!(fs::read_dir(&temporary.0).unwrap().next().is_none());
    }

    #[test]
    fn legacy_history_and_resume_requests_are_retired() {
        for action in ["list", "read", "metadata", "search", "archive", "restore"] {
            assert!(
                call(json!({"action":action}))
                    .unwrap_err()
                    .starts_with("tool_retired:")
            );
        }
        assert!(
            resume("conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .unwrap_err()
                .starts_with("tool_retired:")
        );
        let descriptor = tool();
        assert_eq!(descriptor["name"], "ssmcp.ui.events");
        assert_eq!(
            descriptor["inputSchema"]["properties"]["action"]["const"],
            "thread"
        );
    }
}
