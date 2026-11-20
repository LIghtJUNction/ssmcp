//! Versioned JSONL conversation history. It never restores tools or delivery state.
pub(crate) mod archive;
pub(crate) mod descriptions;
pub(crate) mod goals;
mod listener;
mod ownership;
pub(crate) use listener::{
    ensure_current_id, record_listener_result, record_request_in, with_listener_conversation,
};
#[cfg(test)]
pub(crate) use ownership::with_test_history;
pub(crate) use ownership::{
    HistoryId, HistoryOwner, capture, current_owner, record_owned_agent, record_owned_answer,
    record_owned_request, remember_visible_reply,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Read, Write},
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
const MAX_PAGE_BYTES: usize = 64 * 1024;

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchFilter {
    query: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    case_sensitive: bool,
}

impl SearchFilter {
    fn validate(&self) -> Result<(), String> {
        if self.query.trim().is_empty()
            || self.query.len() > 256
            || self
                .query
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Err("history query must contain 1..=256 UTF-8 bytes of literal text".into());
        }
        if let Some(id) = &self.id {
            session_name(id)?;
        }
        if self
            .role
            .as_deref()
            .is_some_and(|role| !matches!(role, "system" | "user" | "assistant" | "tool"))
        {
            return Err("history search role must be system, user, assistant or tool".into());
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct SearchHit {
    conversation_id: String,
    message_id: String,
    message_index: usize,
    role: String,
    kind: String,
    created_at_ms: u64,
    content_bytes: usize,
    source_truncated: bool,
    snippet: String,
    snippet_truncated: bool,
}

struct SearchResult {
    matches: Vec<SearchHit>,
    scanned_sessions: usize,
    scan_truncated: bool,
}

// Lowercasing can change UTF-8 length (for example İ → i + combining dot).
// Map the match back to the original character boundary before slicing.
fn match_index(content: &str, query: &str, case_sensitive: bool) -> Option<usize> {
    if case_sensitive {
        return content.find(query);
    }
    let folded_offset = content.to_lowercase().find(&query.to_lowercase())?;
    let mut folded_bytes = 0;
    for (offset, character) in content.char_indices() {
        folded_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        if folded_offset < folded_bytes {
            return Some(offset);
        }
    }
    None
}

fn search_hit(conversation_id: &str, index: usize, message: &Message, at: usize) -> SearchHit {
    let start = message.content[..at]
        .char_indices()
        .rev()
        .nth(80)
        .map_or(0, |(offset, _)| offset);
    let end = message.content[start..]
        .char_indices()
        .nth(384)
        .map_or(message.content.len(), |(offset, _)| start + offset);
    SearchHit {
        conversation_id: conversation_id.into(),
        message_id: message.id.clone(),
        message_index: index,
        role: message.role.clone(),
        kind: message.kind.clone(),
        created_at_ms: message.created_at_ms,
        content_bytes: message.content.len(),
        source_truncated: message.truncated,
        snippet: format!(
            "{}{}{}",
            if start > 0 { "…" } else { "" },
            &message.content[start..end],
            if end < message.content.len() {
                "…"
            } else {
                ""
            }
        ),
        snippet_truncated: start > 0 || end < message.content.len(),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Entry {
    Session(SessionMetadata),
    Message(Message),
}

struct Active {
    metadata: SessionMetadata,
    // A lifetime lease prevents another client from editing or evicting this session.
    _lease: File,
}

struct History {
    directory: SafeDirectory,
    workspace: String,
    active: Option<Active>,
    ownership: ownership::Ownership,
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
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
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

fn session_name(id: &str) -> Result<String, String> {
    if !valid_id(id, "conversation") {
        return Err("invalid conversation identifier".into());
    }
    Ok(format!("{id}.jsonl"))
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
    let suffix: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
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
    let path = std::env::current_dir()
        .and_then(fs::canonicalize)
        .map_err(|_| "cannot determine conversation workspace")?;
    let value = path
        .to_str()
        .ok_or("conversation workspace must be UTF-8")?;
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("conversation workspace path is invalid or oversized".into());
    }
    Ok(value.to_owned())
}

fn with<T>(operation: impl FnOnce(&mut History) -> Result<T, String>) -> Result<T, String> {
    #[cfg(test)]
    if let Ok(store) = ownership::TEST_HISTORY.try_with(std::sync::Arc::clone) {
        let mut state = store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        return operation(state.as_mut().expect("fixture history was initialized"));
    }
    let store = HISTORY.get_or_init(HistoryStore::default);
    let mut state = store
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.is_none() {
        let mut history = History::at(default_directory()?)?;
        history.ownership.current = store.current.clone();
        *state = Some(history);
    }
    operation(state.as_mut().expect("history was initialized"))
}

impl History {
    fn at(path: PathBuf) -> Result<Self, String> {
        Self::with_workspace(path, workspace()?)
    }

    fn with_workspace(path: PathBuf, workspace: String) -> Result<Self, String> {
        Ok(Self {
            directory: SafeDirectory::at(path)?,
            workspace,
            active: None,
            ownership: ownership::Ownership::default(),
        })
    }

    fn lease(&self, id: &str) -> Result<File, String> {
        let lease = self
            .directory
            .open(&format!("{id}.lease"), true, true, false)?;
        crate::platform::lock_file(&lease, true)
            .map_err(|_| "conversation is open in another client; choose a different history")?;
        Ok(lease)
    }

    fn load(&self, id: &str) -> Result<Conversation, String> {
        let name = session_name(id)?;
        let file = self.directory.open(&name, false, false, false)?;
        let metadata = file
            .metadata()
            .map_err(|_| "cannot inspect conversation history")?;
        if metadata.len() > MAX_SESSION_BYTES as u64 {
            return Err("conversation history file exceeds 512 KiB".into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_SESSION_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read conversation history")?;
        if bytes.len() > MAX_SESSION_BYTES || !bytes.ends_with(b"\n") {
            return Err("conversation history is oversized or incomplete".into());
        }
        let mut lines = bytes.split(|byte| *byte == b'\n');
        let first = lines.next().ok_or("conversation history is empty")?;
        let Entry::Session(metadata) =
            serde_json::from_slice(first).map_err(|_| "invalid conversation session metadata")?
        else {
            return Err("conversation history must begin with session metadata".into());
        };
        if metadata.schema != SCHEMA
            || metadata.schema_version != 1
            || metadata.id != id
            || metadata.workspace.is_empty()
            || metadata.workspace.len() > 4096
            || metadata.workspace.chars().any(char::is_control)
        {
            return Err("unsupported or inconsistent conversation metadata".into());
        }
        let mut messages = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Entry::Message(mut message) =
                serde_json::from_slice(line).map_err(|_| "invalid conversation message")?
            else {
                return Err("session metadata must occur only once".into());
            };
            validate_content(&message.content)?;
            if !valid_id(&message.id, "message")
                || !ids.insert(message.id.clone())
                || !matches!(
                    message.role.as_str(),
                    "system" | "user" | "assistant" | "tool"
                )
                || !matches!(
                    message.kind.as_str(),
                    "requirement" | "answer" | "agent_message" | "system_message" | "tool_message"
                )
            {
                return Err("invalid conversation message identity or role".into());
            }
            // Imported or manually edited history passes through the same audit boundary.
            message.content = safe_text(&message.content);
            if message.content.len() > MAX_CONTENT {
                let mut end = MAX_CONTENT;
                while !message.content.is_char_boundary(end) {
                    end -= 1;
                }
                message.content.truncate(end);
                message.truncated = true;
            }
            messages.push(message);
            if messages.len() > MAX_MESSAGES {
                return Err("conversation history exceeds 500 retained messages".into());
            }
        }
        Ok(Conversation { metadata, messages })
    }

    fn encode(conversation: &Conversation) -> Result<Vec<u8>, String> {
        let mut bytes = serde_json::to_vec(&Entry::Session(conversation.metadata.clone()))
            .map_err(|_| "cannot encode conversation metadata")?;
        bytes.push(b'\n');
        for message in &conversation.messages {
            serde_json::to_writer(&mut bytes, &Entry::Message(message.clone()))
                .map_err(|_| "cannot encode conversation message")?;
            bytes.push(b'\n');
        }
        Ok(bytes)
    }

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
            .unwrap_or_else(|| "Empty conversation".into());
        SessionSummary {
            id: conversation.metadata.id.clone(),
            workspace: conversation.metadata.workspace.clone(),
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
            archived: false,
            current: self
                .active
                .as_ref()
                .is_some_and(|active| active.metadata.id == conversation.metadata.id),
        }
    }

    fn histories(&self, names: &[String]) -> Result<Vec<(SessionSummary, usize)>, String> {
        let archived = self.archive_index()?;
        let mut output = Vec::new();
        for name in names {
            let Some(id) = name
                .strip_suffix(".jsonl")
                .filter(|id| valid_id(id, "conversation"))
            else {
                continue;
            };
            // Corrupt, linked, or nonprivate files are never displayed or pruned.
            let Ok(conversation) = self.load(id) else {
                continue;
            };
            let file = self.directory.open(name, false, false, false)?;
            let bytes = usize::try_from(
                file.metadata()
                    .map_err(|_| "cannot inspect history size")?
                    .len(),
            )
            .map_err(|_| "conversation file size is unsupported")?;
            let mut summary = self.summary(&conversation);
            summary.archived = archived.contains(id);
            output.push((summary, bytes));
        }
        output.sort_by(|left, right| {
            right
                .0
                .updated_at_ms
                .cmp(&left.0.updated_at_ms)
                .then_with(|| right.0.id.cmp(&left.0.id))
        });
        Ok(output)
    }

    fn retain(&self, id: &str, bytes: usize) -> Result<(), String> {
        let names = self.directory.names()?;
        let histories = self.histories(&names)?;
        let mut count = 1 + histories
            .iter()
            .filter(|(history, _)| history.id != id)
            .count();
        let mut total = bytes
            + histories
                .iter()
                .filter(|(history, _)| history.id != id)
                .map(|(_, bytes)| *bytes)
                .sum::<usize>();
        let mut reclaim = Vec::new();
        for (history, size) in histories.iter().rev() {
            if count <= MAX_SESSIONS && total <= MAX_TOTAL_BYTES {
                break;
            }
            if history.id == id || history.archived {
                continue;
            }
            if let Ok(lease) = self.lease(&history.id) {
                reclaim.push((history.id.clone(), lease));
                count -= 1;
                total -= size;
            }
        }
        if count > MAX_SESSIONS || total > MAX_TOTAL_BYTES {
            return Err("conversation retention is full of active or archived sessions; restore an archive or close an older client and retry".into());
        }
        for (id, _lease) in reclaim {
            self.remove_description(&id, &names)?;
            self.remove_goal(&id, &names)?;
            self.directory.remove(&session_name(&id)?)?;
            self.directory.remove(&format!("{id}.lease"))?;
        }
        Ok(())
    }

    fn request_source(
        &self,
        request_id: &str,
        text: &str,
    ) -> Result<Option<(String, Message)>, String> {
        let identity = request_identity(request_id, text)?;
        let mut expected_text = safe_text(text);
        if expected_text.len() > MAX_CONTENT {
            let mut end = MAX_CONTENT;
            while !expected_text.is_char_boundary(end) {
                end -= 1;
            }
            expected_text.truncate(end);
        }
        let mut ids = Vec::new();
        if let Some(active) = &self.active {
            ids.push(active.metadata.id.clone());
        }
        for name in self.directory.names()? {
            if let Some(id) = name.strip_suffix(".jsonl")
                && valid_id(id, "conversation")
                && !ids.iter().any(|known| known == id)
            {
                ids.push(id.into());
            }
        }
        if ids.len() > MAX_SESSIONS {
            return Err("too many retained histories to resolve a delivered request safely".into());
        }
        for id in ids {
            let conversation = self.load(&id)?;
            if conversation.metadata.workspace != self.workspace {
                continue;
            }
            if let Some(message) = conversation.messages.iter().find(|m| m.id == identity) {
                if message.role != "user" || message.content != expected_text {
                    return Err(
                        "delivered request conflicts with its original history source".into(),
                    );
                }
                return Ok(Some((id, message.clone())));
            }
        }
        Ok(None)
    }

    fn delivered_source(&self, request_id: &str, text: &str) -> Result<(String, Message), String> {
        let _lock = self.directory.lock()?;
        self.request_source(request_id, text)?.ok_or_else(|| {
            "delivered request source is absent from retained JSONL; memory was not changed".into()
        })
    }

    /// Called only after a confirmed takeback, never for an uncertain edit or retry.
    /// Resolve the original exact request source even after switching histories.
    fn recall_request(&mut self, request_id: &str, text: &str) -> Result<bool, String> {
        let _lock = self.directory.lock()?;
        let Some((id, message)) = self.request_source(request_id, text)? else {
            return Ok(false);
        };
        let _lease = if self
            .active
            .as_ref()
            .is_some_and(|active| active.metadata.id == id)
        {
            None
        } else {
            Some(self.lease(&id)?)
        };
        let mut conversation = self.load(&id)?;
        conversation.messages.retain(|entry| entry.id != message.id);
        self.directory
            .write_atomic(&session_name(&id)?, &Self::encode(&conversation)?)?;
        Ok(true)
    }

    fn record(&mut self, role: &str, text: &str) -> Result<Option<Committed>, String> {
        self.record_identified(role, text, None)
    }

    fn record_agent(&mut self, id: &str, text: &str) -> Result<Option<Committed>, String> {
        if !valid_id(id, "message") {
            return Err("invalid agent message identity for conversation history".into());
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
        let (role, kind) = role_and_kind(role)?;
        validate_content(text)?;
        let _lock = self.directory.lock()?;
        let pending = self.ownership.pending(owner)?;
        let current = self.ownership.current.get().as_ref() == Some(owner);
        let id = owner.id();
        if self.archive_index()?.contains(id) {
            return Err("history_owner_archived: the original conversation is archived".into());
        }
        let mut conversation = if pending {
            let name = session_name(id)?;
            if self.directory.names()?.contains(&name) {
                return Err("history_owner_conflict: reserved conversation already exists".into());
            }
            Conversation {
                metadata: SessionMetadata {
                    schema: SCHEMA.into(),
                    schema_version: 1,
                    id: id.to_owned(),
                    workspace: self.workspace.clone(),
                    created_at_ms: now_ms(),
                    truncated_messages: 0,
                },
                messages: Vec::new(),
            }
        } else {
            self.load(id)
                .map_err(|error| format!("history_owner_unavailable: {error}"))?
        };
        // A switch releases the active lease as before. An inactive append
        // must take its own lease under the directory lock and cannot use it
        // to replace the selected conversation.
        let lease = if current && !pending {
            None
        } else {
            Some(
                self.lease(id)
                    .map_err(|error| format!("history_owner_unavailable: {error}"))?,
            )
        };
        let mut content = safe_text(text);
        let truncated = content.len() > MAX_CONTENT;
        if truncated {
            let mut end = MAX_CONTENT;
            while !content.is_char_boundary(end) {
                end -= 1;
            }
            content.truncate(end);
        }
        if conversation.metadata.workspace != self.workspace {
            return Err("active conversation workspace changed".into());
        }
        let id = match identity {
            Some(id) => {
                if let Some(previous) = conversation
                    .messages
                    .iter()
                    .find(|message| message.id == id)
                {
                    if previous.role == role && previous.content == content {
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
        conversation.messages.push(Message {
            id,
            role: role.into(),
            content,
            kind: kind.into(),
            created_at_ms: now_ms(),
            truncated,
        });
        let mut bytes = Self::encode(&conversation)?;
        while conversation.messages.len() > MAX_MESSAGES || bytes.len() > MAX_SESSION_BYTES {
            conversation.messages.remove(0);
            conversation.metadata.truncated_messages =
                conversation.metadata.truncated_messages.saturating_add(1);
            bytes = Self::encode(&conversation)?;
        }
        self.retain(&conversation.metadata.id, bytes.len())?;
        let committed = conversation
            .messages
            .last()
            .cloned()
            .map(|message| Committed {
                workspace: self.workspace.clone(),
                conversation_id: conversation.metadata.id.clone(),
                message,
            });
        self.directory
            .write_atomic(&session_name(&conversation.metadata.id)?, &bytes)?;
        self.ownership.materialized(owner);
        if current {
            if let Some(lease) = lease {
                self.active = Some(Active {
                    metadata: conversation.metadata,
                    _lease: lease,
                });
            } else if let Some(active) = &mut self.active {
                active.metadata = conversation.metadata;
            }
        }
        Ok(committed)
    }

    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        self.sessions_filtered(archive::Filter::Active)
    }

    fn sessions_filtered(&self, filter: archive::Filter) -> Result<Vec<SessionSummary>, String> {
        let _lock = self.directory.lock()?;
        let names = self.directory.names()?;
        Ok(self
            .histories(&names)?
            .into_iter()
            .map(|(summary, _)| summary)
            .filter(|summary| {
                summary.workspace == self.workspace && filter.includes(summary.archived)
            })
            .map(|mut summary| {
                self.describe_summary(&mut summary, &names);
                summary
            })
            .collect())
    }

    fn current(&self) -> Result<Option<Conversation>, String> {
        let Some(active) = &self.active else {
            return Ok(None);
        };
        self.read(&active.metadata.id).map(Some)
    }

    fn read(&self, id: &str) -> Result<Conversation, String> {
        let _lock = self.directory.lock()?;
        let conversation = self.load(id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
        }
        Ok(conversation)
    }

    fn metadata(&self, id: &str) -> Result<Value, String> {
        let _lock = self.directory.lock()?;
        let conversation = self.load(id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
        }
        let mut summary = self.summary(&conversation);
        self.describe_summary(&mut summary, &self.directory.names()?);
        summary.archived = self.archive_index()?.contains(id);
        Ok(json!({"schema_version":1,"metadata":conversation.metadata,"summary":summary}))
    }

    fn search(&self, filter: &SearchFilter) -> Result<SearchResult, String> {
        filter.validate()?;
        let _lock = self.directory.lock()?;
        let mut conversations = Vec::new();
        let mut scan_truncated = false;
        if let Some(id) = &filter.id {
            let conversation = self.load(id)?;
            if conversation.metadata.workspace != self.workspace {
                return Err("conversation belongs to a different workspace".into());
            }
            conversations.push(conversation);
        } else {
            // Inspect file metadata before reading bodies. Manual imports must
            // not turn a bounded search into an unbounded scan of old files.
            let mut candidates = Vec::new();
            for name in self.directory.names()? {
                let Some(id) = name
                    .strip_suffix(".jsonl")
                    .filter(|id| valid_id(id, "conversation"))
                else {
                    continue;
                };
                let Ok(file) = self.directory.open(&name, false, false, false) else {
                    continue;
                };
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                let Ok(bytes) = usize::try_from(metadata.len()) else {
                    continue;
                };
                if bytes > MAX_SESSION_BYTES {
                    continue;
                }
                candidates.push((
                    id.to_owned(),
                    bytes,
                    metadata.modified().unwrap_or(UNIX_EPOCH),
                ));
            }
            candidates
                .sort_by(|left, right| right.2.cmp(&left.2).then_with(|| right.0.cmp(&left.0)));
            let mut scanned_bytes = 0usize;
            for (scanned_files, (id, bytes, _)) in candidates.into_iter().enumerate() {
                if scanned_files >= MAX_SESSIONS
                    || scanned_bytes.saturating_add(bytes) > MAX_TOTAL_BYTES
                {
                    scan_truncated = true;
                    break;
                }
                scanned_bytes += bytes;
                let Ok(conversation) = self.load(&id) else {
                    continue;
                };
                if conversation.metadata.workspace == self.workspace {
                    conversations.push(conversation);
                }
            }
        }
        conversations.sort_by(|left, right| {
            let left = self.summary(left);
            let right = self.summary(right);
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| right.id.cmp(&left.id))
        });
        let mut result = SearchResult {
            matches: Vec::new(),
            scanned_sessions: conversations.len(),
            scan_truncated,
        };
        for conversation in conversations {
            for (index, message) in conversation.messages.iter().enumerate() {
                if filter
                    .role
                    .as_ref()
                    .is_some_and(|role| role != &message.role)
                {
                    continue;
                }
                if let Some(at) =
                    match_index(&message.content, &filter.query, filter.case_sensitive)
                {
                    result
                        .matches
                        .push(search_hit(&conversation.metadata.id, index, message, at));
                }
            }
        }
        Ok(result)
    }

    fn resume(&mut self, id: &str) -> Result<Conversation, String> {
        let _lock = self.directory.lock()?;
        let conversation = self.load(id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
        }
        if self.archive_index()?.contains(id) {
            return Err("conversation is archived; restore it before resuming".into());
        }
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.metadata.id != id)
        {
            let lease = self.lease(id)?;
            self.active = Some(Active {
                metadata: conversation.metadata.clone(),
                _lease: lease,
            });
        }
        self.ownership.select(id, false);
        Ok(conversation)
    }
}

#[derive(Debug)]
struct Committed {
    workspace: String,
    conversation_id: String,
    message: Message,
}

/// Source JSONL is committed first. Derived memory I/O and display never run
/// under the history mutex or its transactional file lock.
fn remember_committed(committed: Option<Committed>) {
    if let Some(saved) = committed
        && let Err(error) = crate::client::context_memory::record_history(
            &saved.workspace,
            &saved.conversation_id,
            &saved.message,
        )
    {
        crate::client::terminal::notice(&format!(
            "Conversation saved; local memory could not be updated: {error}"
        ));
    }
}

pub(crate) fn record(role: &str, text: &str) -> Result<(), String> {
    let committed = with(|history| history.record(role, text))?;
    remember_committed(committed);
    Ok(())
}
pub(crate) fn record_request(request_id: &str, text: &str) -> Result<(), String> {
    with(|history| history.record_request(request_id, text)).map(|_| ())
}
/// Retract only the exact queued source whose takeback was confirmed by the broker.
pub(crate) fn recall_request(request_id: &str, text: &str) -> Result<(), String> {
    with(|history| history.recall_request(request_id, text)).map(|_| ())
}
/// Resolve the saved original text only after a new delivery receipt arrives.
/// Targeted requests remain outside shared automatic memory even after delivery.
pub(crate) fn remember_delivered_request(
    request_id: &str,
    text: &str,
    revision: u64,
    target: &str,
) -> Result<(), String> {
    if target != "next" {
        return Ok(());
    }
    let (workspace, conversation_id, message) = with(|history| {
        let (conversation_id, message) = history.delivered_source(request_id, text)?;
        Ok((history.workspace.clone(), conversation_id, message))
    })?;
    crate::client::context_memory::record_delivered_history(
        &workspace,
        &conversation_id,
        &message,
        request_id,
        revision,
        target,
    )
}

/// Read only the active local JSONL conversation; never select or resume another.
pub(crate) fn current() -> Result<Option<Conversation>, String> {
    with(|history| history.current())
}

impl History {
    fn release_current(&mut self) {
        self.active = None;
        self.ownership.release();
    }
}

/// Release the parent process lease before its replacement resumes this file.
/// This is also needed when the platform spawns and waits instead of exec.
pub(crate) fn release_current() -> Result<(), String> {
    with(|history| {
        history.release_current();
        super::messages::restore_copy(&[]);
        Ok(())
    })
}
pub(crate) fn sessions() -> Result<Vec<SessionSummary>, String> {
    with(|history| history.sessions())
}
pub(crate) fn read(id: &str) -> Result<Conversation, String> {
    with(|history| history.read(id))
}
pub(crate) fn resume(id: &str) -> Result<Conversation, String> {
    with(|history| {
        let conversation = history.resume(id)?;
        // The directory lock has been released. Publish the copy cache in
        // selection order, without acquiring the terminal or running display hooks.
        super::messages::restore_copy(&conversation.messages);
        Ok(conversation)
    })
}

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

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List {
        #[serde(default)]
        state: archive::Filter,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Read {
        id: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Metadata {
        id: String,
    },
    Search {
        query: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        case_sensitive: bool,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
}

fn search_page(
    history: &History,
    filter: &SearchFilter,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    if !(1..=64).contains(&limit) {
        return Err("history page limit must be in 1..=64".into());
    }
    let result = history.search(filter)?;
    let total = result.matches.len();
    if offset > total {
        return Err("history search offset exceeds match count".into());
    }
    let mut page = Vec::new();
    // Account for the query, envelope, and JSON escaping, as well as each hit.
    let mut bytes = 4096;
    for entry in result.matches.into_iter().skip(offset).take(limit) {
        let size = serde_json::to_vec(&entry)
            .map_err(|_| "cannot encode history match")?
            .len();
        if bytes + size > MAX_PAGE_BYTES {
            break;
        }
        bytes += size;
        page.push(entry);
    }
    let next = offset + page.len();
    Ok(
        json!({"schema_version":1,"schema":SCHEMA,"query":filter.query,
        "case_sensitive":filter.case_sensitive,"id":filter.id,"role":filter.role,
        "matches":page,"total":total,"offset":offset,"next_offset":(next < total).then_some(next),
        "scanned_sessions":result.scanned_sessions,"scan_truncated":result.scan_truncated,
        "scope":"current_workspace","order":"session_updated_desc_then_message_asc"}),
    )
}

pub(super) fn call(arguments: Value) -> Result<Value, String> {
    if arguments
        .as_object()
        .is_none_or(|object| object.values().any(Value::is_null))
    {
        return Err("history expects an object; omit optional fields instead of null".into());
    }
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "expected a history list, metadata, read or search action")?;
    let (offset, limit) = match &request {
        Request::List { offset, limit, .. }
        | Request::Read { offset, limit, .. }
        | Request::Search { offset, limit, .. } => (offset.unwrap_or(0), limit.unwrap_or(16)),
        Request::Metadata { .. } => (0, 1),
    };
    if !(1..=64).contains(&limit) {
        return Err("history page limit must be in 1..=64".into());
    }
    match request {
        Request::Metadata { id } => with(|history| history.metadata(&id)),
        Request::Search {
            query,
            id,
            role,
            case_sensitive,
            ..
        } => {
            let filter = SearchFilter {
                query,
                id,
                role,
                case_sensitive,
            };
            with(|history| search_page(history, &filter, offset, limit))
        }
        Request::List { state, .. } => {
            let sessions = archive::sessions(state)?;
            let total = sessions.len();
            if offset > total {
                return Err("history page offset exceeds session count".into());
            }
            let mut page = Vec::new();
            let mut bytes = 512;
            for summary in sessions.into_iter().skip(offset).take(limit) {
                let size = serde_json::to_vec(&summary)
                    .map_err(|_| "cannot encode history summary")?
                    .len();
                if bytes + size > MAX_PAGE_BYTES {
                    break;
                }
                bytes += size;
                page.push(summary);
            }
            let next = offset + page.len();
            Ok(
                json!({"schema_version":1,"schema":SCHEMA,"state":state,"sessions":page,"total":total,"offset":offset,
                "next_offset":(next < total).then_some(next)}),
            )
        }
        Request::Read { id, .. } => {
            let conversation = read(&id)?;
            let total = conversation.messages.len();
            if offset > total {
                return Err("history page offset exceeds message count".into());
            }
            let mut page = Vec::new();
            let mut bytes = serde_json::to_vec(&conversation.metadata)
                .map_err(|_| "cannot encode history metadata")?
                .len()
                + 512;
            for message in conversation.messages.into_iter().skip(offset).take(limit) {
                let size = serde_json::to_vec(&message)
                    .map_err(|_| "cannot encode history message")?
                    .len();
                if bytes + size > MAX_PAGE_BYTES {
                    break;
                }
                bytes += size;
                page.push(message);
            }
            let next = offset + page.len();
            Ok(
                json!({"schema_version":1,"metadata":conversation.metadata,"messages":page,"total":total,"offset":offset,
                "next_offset":(next < total).then_some(next)}),
            )
        }
    }
}

pub(super) fn tool() -> Value {
    let page = json!({"type":"integer","minimum":1,"maximum":64,"default":16});
    let offset = json!({"type":"integer","minimum":0,"default":0});
    json!({
        "name":"ssmcp.ui.history",
        "description":"Read and search this workspace's local, redacted conversation history. list returns recent session summaries (state: active by default, archived, or all); archived conversations remain readable/searchable and protected from automatic retention cleanup. Use ssmcp.ui.history_archive for reversible archive changes and ssmcp.ui.history_update to edit saved titles/summaries without changing messages. metadata returns one session's metadata and summary without messages; read returns metadata and message pages; search matches literal keywords (case-insensitive by default) with optional session ID and role filters. Search returns source message IDs, message indexes and bounded snippets; use read for full text. Pages are at most 64 KiB: follow next_offset. Search scans at most 64 sessions / 16 MiB and reports scan_truncated. The open JSONL schema is ssmcp.conversation.v1. These read-only actions never resume a harness, invoke a model, resend requirements or expose saved agent credentials. Only current-workspace histories are readable. Each history keeps at most 500 messages / 512 KiB and reports truncated_messages.",
        "inputSchema":{"type":"object","oneOf":[
            {"type":"object","required":["action"],"additionalProperties":false,"properties":{
                "action":{"const":"list"},"state":{"type":"string","enum":["active","archived","all"],"default":"active"},"offset":offset,"limit":page
            }},
            {"type":"object","required":["action","id"],"additionalProperties":false,"properties":{
                "action":{"const":"read"},
                "id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"},
                "offset":offset,"limit":page
            }},
            {"type":"object","required":["action","id"],"additionalProperties":false,"properties":{
                "action":{"const":"metadata"},"id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"}
            }},
            {"type":"object","required":["action","query"],"additionalProperties":false,"properties":{
                "action":{"const":"search"},"query":{"type":"string","minLength":1,"maxLength":256,"description":"Literal keyword or phrase; at most 256 UTF-8 bytes, not a regular expression."},
                "id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"},
                "role":{"type":"string","enum":["system","user","assistant","tool"]},
                "case_sensitive":{"type":"boolean","default":false},"offset":offset,"limit":page
            }}
        ]},
        "annotations":{"readOnlyHint":true,"destructiveHint":false}
    })
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
                .join(random_id("history_test").unwrap());
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

    fn query(text: &str) -> SearchFilter {
        SearchFilter {
            query: text.into(),
            id: None,
            role: None,
            case_sensitive: false,
        }
    }

    #[test]
    fn metadata_and_search_are_read_only_scoped_and_keep_source_positions() {
        let temp = Temporary::new();
        let mut history = temp.history();
        history
            .record("user", "请修复登录，History search uses literal [A.*]")
            .unwrap();
        history
            .record("assistant", "登录已修复，history preserved")
            .unwrap();
        history
            .record("tool", "API_TOKEN=private_fixture_secret")
            .unwrap();
        let current = history.current().unwrap().unwrap();
        let id = current.metadata.id.clone();
        let path = temp
            .0
            .join("conversations")
            .join(session_name(&id).unwrap());
        let before = fs::read(&path).unwrap();
        let metadata = history.metadata(&id).unwrap();
        assert_eq!(metadata["metadata"]["id"], id);
        assert_eq!(metadata["summary"]["message_count"], 3);
        assert_eq!(metadata["summary"]["current"], true);
        assert!(metadata.get("messages").is_none());

        let result = history.search(&query("HISTORY")).unwrap();
        assert_eq!(result.matches.len(), 2);
        assert_eq!(result.matches[1].message_index, 1);
        assert_eq!(result.matches[1].message_id, current.messages[1].id);
        assert_eq!(result.matches[0].conversation_id, id);
        assert_eq!(history.search(&query("[A.*]")).unwrap().matches.len(), 1);
        assert!(history.search(&query("H.*ry")).unwrap().matches.is_empty());
        assert_eq!(history.search(&query("登录")).unwrap().matches.len(), 2);
        assert!(
            history
                .search(&query("private_fixture_secret"))
                .unwrap()
                .matches
                .is_empty()
        );

        let mut filter = query("history");
        filter.id = Some(id.clone());
        filter.role = Some("assistant".into());
        filter.case_sensitive = true;
        assert_eq!(history.search(&filter).unwrap().matches.len(), 1);
        filter.role = Some("user".into());
        assert!(history.search(&filter).unwrap().matches.is_empty());

        let other =
            History::with_workspace(temp.0.join("conversations"), "/fixture/other".into()).unwrap();
        assert!(other.search(&query("登录")).unwrap().matches.is_empty());
        assert!(other.metadata(&id).is_err());
        assert!(other.search(&filter).is_err());
        assert_eq!(history.current().unwrap().unwrap(), current);
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn search_snippets_find_late_unicode_matches_and_paginate_within_budget() {
        let temp = Temporary::new();
        let mut history = temp.history();
        for n in 0..70 {
            history
                .record(
                    "user",
                    &format!(
                        "{n} {}İSTANBUL 登录{}",
                        "前文".repeat(250),
                        "后文".repeat(250)
                    ),
                )
                .unwrap();
        }
        let filter = query("i\u{307}stanbul 登录");
        let first = search_page(&history, &filter, 0, 64).unwrap();
        assert_eq!(first["total"], 70);
        let hits = first["matches"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert!(
            hits.iter()
                .all(|hit| hit["snippet"].as_str().unwrap().contains("İSTANBUL 登录"))
        );
        assert!(hits.iter().all(|hit| hit["snippet_truncated"] == true));
        assert!(serde_json::to_vec(&first).unwrap().len() <= MAX_PAGE_BYTES);
        let next = first["next_offset"].as_u64().unwrap() as usize;
        assert_eq!(next, hits.len());
        let second = search_page(&history, &filter, next, 64).unwrap();
        assert_eq!(second["matches"][0]["message_index"], next);
        assert!(serde_json::to_vec(&second).unwrap().len() <= MAX_PAGE_BYTES);
        assert!(search_page(&history, &filter, 71, 1).is_err());
        assert!(search_page(&history, &filter, 0, 0).is_err());
        assert!(search_page(&history, &filter, 0, 65).is_err());
    }

    #[test]
    fn search_bounds_manually_imported_files_and_reports_incomplete_scope() {
        let temp = Temporary::new();
        let mut history = temp.history();
        history.record("user", "bounded keyword").unwrap();
        let mut conversation = history.current().unwrap().unwrap();
        for _ in 0..MAX_SESSIONS {
            conversation.metadata.id = random_id("conversation").unwrap();
            history
                .directory
                .write_atomic(
                    &session_name(&conversation.metadata.id).unwrap(),
                    &History::encode(&conversation).unwrap(),
                )
                .unwrap();
        }
        let result = history.search(&query("keyword")).unwrap();
        assert!(result.scan_truncated);
        assert_eq!(result.scanned_sessions, MAX_SESSIONS);
        assert_eq!(result.matches.len(), MAX_SESSIONS);
    }

    #[test]
    fn search_rejects_bad_queries_paths_roles_and_schema_fields() {
        for text in ["", "  \n", "bad\u{1b}query", &"x".repeat(257)] {
            assert!(query(text).validate().is_err());
        }
        let mut filter = query("valid");
        filter.id = Some("../outside".into());
        assert!(filter.validate().is_err());
        filter.id = None;
        filter.role = Some("agent".into());
        assert!(filter.validate().is_err());
        assert!(call(json!({"action":"metadata","id":"conversation_00000000000000000000000000000000","limit":1})).is_err());
        assert!(call(json!({"action":"search","query":"valid","id":null})).is_err());
        let schema = tool();
        assert_eq!(schema["annotations"]["readOnlyHint"], true);
        assert_eq!(schema["inputSchema"]["oneOf"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn delivered_request_source_uses_original_identity_without_rewriting_history() {
        let temp = Temporary::new();
        let mut history = temp.history();
        history
            .record_request("request_fixture", "Original source @file.rs")
            .unwrap();
        let session = history.sessions().unwrap().remove(0).id;
        let path = temp
            .0
            .join("conversations")
            .join(session_name(&session).unwrap());
        let before = fs::read(&path).unwrap();
        let (id, message) = history
            .delivered_source("request_fixture", "Original source @file.rs")
            .unwrap();
        assert_eq!(id, session);
        assert_eq!(message.role, "user");
        assert_eq!(message.content, "Original source @file.rs");
        assert!(
            history
                .delivered_source("request_fixture", "Different edited source")
                .is_err()
        );
        let reopened = temp.history();
        assert_eq!(
            reopened
                .delivered_source("request_fixture", "Original source @file.rs")
                .unwrap()
                .0,
            session
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let other =
            History::with_workspace(temp.0.join("conversations"), "/fixture/different".into())
                .unwrap();
        assert!(
            other
                .delivered_source("request_fixture", "Original source @file.rs")
                .is_err()
        );
    }

    #[test]
    fn records_versioned_jsonl_with_standard_roles_and_redacted_content() {
        let temp = Temporary::new();
        let mut history = temp.history();
        history.record("user", "A user request").unwrap();
        history
            .record("agent", "# Reply\n\nAPI_TOKEN=private_fixture_secret")
            .unwrap();
        history
            .record("answer", "Continue with option one.")
            .unwrap();
        let summary = history.sessions().unwrap().remove(0);
        let bytes = fs::read(
            temp.0
                .join("conversations")
                .join(session_name(&summary.id).unwrap()),
        )
        .unwrap();
        let lines: Vec<Value> = String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["schema"], SCHEMA);
        assert_eq!(lines[0]["schema_version"], 1);
        assert_eq!(lines[1]["role"], "user");
        assert_eq!(lines[2]["role"], "assistant");
        assert_eq!(lines[3]["role"], "user");
        assert_eq!(lines[3]["kind"], "answer");
        assert!(
            !lines[2]["content"]
                .as_str()
                .unwrap()
                .contains("private_fixture_secret")
        );
        assert!(lines[2]["content"].as_str().unwrap().contains("[redacted]"));
        assert!(
            lines
                .iter()
                .skip(1)
                .all(|line| line["type"] == "message" && line["content"].is_string())
        );
    }

    #[test]
    fn identified_agent_replies_share_jsonl_without_duplicate_records() {
        let temp = Temporary::new();
        let mut history = temp.history();
        let id = "message_00000000000000000000000000000001";
        let text = "## 结果\n保留中文正文";
        history.record_agent(id, text).unwrap();
        history.record_agent(id, text).unwrap();
        assert!(history.record_agent(id, "changed reply").is_err());
        assert!(history.record_agent("../unsafe", text).is_err());
        let sessions = history.sessions().unwrap();
        let conversation = history.read(&sessions[0].id).unwrap();
        assert_eq!(conversation.messages.len(), 1);
        let message = &conversation.messages[0];
        assert_eq!(message.id, id);
        assert_eq!(message.role, "assistant");
        assert_eq!(message.kind, "agent_message");
        assert_eq!(message.content, text);
    }

    #[test]
    fn current_export_reads_only_the_active_conversation_without_selecting_old_history() {
        let temp = Temporary::new();
        {
            let mut previous = temp.history();
            previous.record("user", "Other conversation").unwrap();
        }
        let mut history = temp.history();
        assert!(history.current().unwrap().is_none());
        history.record("assistant", "Current context").unwrap();
        let current = history.current().unwrap().unwrap();
        assert_eq!(current.messages.len(), 1);
        assert_eq!(current.messages[0].content, "Current context");
        assert_eq!(history.sessions().unwrap().len(), 2);
        assert_eq!(history.current().unwrap().unwrap(), current);
    }

    #[test]
    fn reload_releases_the_live_parent_lease_and_continues_without_replaying_records() {
        let temp = Temporary::new();
        let mut parent = temp.history();
        parent.record("user", "Retained request 界").unwrap();
        parent.record("assistant", "Retained reply").unwrap();
        let saved = parent.current().unwrap().unwrap();
        let mut replacement = temp.history();
        assert!(replacement.resume(&saved.metadata.id).is_err());
        parent.release_current();
        assert!(parent.current().unwrap().is_none());
        assert_eq!(replacement.resume(&saved.metadata.id).unwrap(), saved);
        assert_eq!(replacement.current().unwrap().unwrap(), saved);
        replacement.record("user", "Continue after reload").unwrap();
        let continued = replacement.current().unwrap().unwrap();
        assert_eq!(continued.metadata.id, saved.metadata.id);
        assert_eq!(&continued.messages[..2], &saved.messages);
        assert_eq!(continued.messages.len(), 3);
        assert_eq!(replacement.sessions().unwrap().len(), 1);
        // Keep the parent object alive: spawn-and-wait platforms must not depend
        // on process exit or dropping the old History object to unlock the file.
        assert!(parent.current().unwrap().is_none());
    }

    #[test]
    fn resume_restores_text_only_and_continues_the_selected_file() {
        let temp = Temporary::new();
        let id = {
            let mut previous = temp.history();
            previous.record("user", "First session").unwrap();
            previous.sessions().unwrap()[0].id.clone()
        };
        let mut current = temp.history();
        assert!(
            current
                .sessions()
                .unwrap()
                .iter()
                .all(|session| !session.current)
        );
        let restored = current.resume(&id).unwrap();
        assert_eq!(restored.messages.len(), 1);
        assert_eq!(restored.messages[0].content, "First session");
        current.record("agent", "Continued response").unwrap();
        let resumed = current.read(&id).unwrap();
        assert_eq!(resumed.messages.len(), 2);
        assert_eq!(resumed.metadata.id, id);
        assert_eq!(current.sessions().unwrap().len(), 1);
        assert!(current.sessions().unwrap()[0].current);
        assert!(
            !serde_json::to_string(&resumed)
                .unwrap()
                .contains("agent_id")
        );
    }

    #[test]
    fn accepted_request_identity_deduplicates_retries_and_keeps_real_edits() {
        let temp = Temporary::new();
        let session_id = {
            let mut history = temp.history();
            history
                .record_request("request_one", "Original request")
                .unwrap();
            history
                .record_request("request_one", "Original request")
                .unwrap();
            history
                .record_request("request_one", "Edited request")
                .unwrap();
            history
                .record_request("request_two", "Original request")
                .unwrap();
            let summary = history.sessions().unwrap().remove(0);
            let conversation = history.read(&summary.id).unwrap();
            assert_eq!(conversation.messages.len(), 3);
            assert_ne!(conversation.messages[0].id, conversation.messages[1].id);
            assert_ne!(conversation.messages[0].id, conversation.messages[2].id);
            assert!(
                conversation
                    .messages
                    .iter()
                    .all(|message| valid_id(&message.id, "message"))
            );
            summary.id
        };
        let mut resumed = temp.history();
        resumed.resume(&session_id).unwrap();
        resumed
            .record_request("request_one", "Original request")
            .unwrap();
        assert_eq!(resumed.read(&session_id).unwrap().messages.len(), 3);
    }

    #[test]
    fn recalled_request_removes_only_its_original_source_and_stays_removed_after_resume() {
        let temp = Temporary::new();
        let mut history = temp.history();
        let original = "SAME_REQUEST_BODY 界";
        history
            .record_request("request_delivered", original)
            .unwrap();
        history
            .record_request("request_recalled", original)
            .unwrap();
        history.record("assistant", "KEEP_ASSISTANT_REPLY").unwrap();
        let first = history.current().unwrap().unwrap();
        let retained = vec![first.messages[0].clone(), first.messages[2].clone()];
        let first_id = first.metadata.id;
        // Switching the active history must not make recall edit the wrong file.
        history.release_current();
        history.record("assistant", "SECOND_CONVERSATION").unwrap();
        let second = history.current().unwrap().unwrap();
        assert_ne!(second.metadata.id, first_id);
        assert!(
            !history
                .recall_request("request_recalled", "different source")
                .unwrap()
        );
        assert!(
            history
                .recall_request("request_recalled", original)
                .unwrap()
        );
        assert!(
            !history
                .recall_request("request_recalled", original)
                .unwrap()
        );
        assert_eq!(history.read(&first_id).unwrap().messages, retained);
        assert_eq!(history.current().unwrap().unwrap(), second);
        assert!(
            history
                .delivered_source("request_delivered", original)
                .is_ok()
        );
        assert!(
            history
                .delivered_source("request_recalled", original)
                .is_err()
        );
        drop(history);
        let mut resumed = temp.history();
        assert_eq!(resumed.resume(&first_id).unwrap().messages, retained);
        resumed
            .record_request("request_recalled", "EDITED_REQUEST_BODY")
            .unwrap();
        resumed
            .record_request("request_recalled", "EDITED_REQUEST_BODY")
            .unwrap();
        let conversation = resumed.read(&first_id).unwrap();
        assert_eq!(&conversation.messages[..2], retained.as_slice());
        assert_eq!(conversation.messages.len(), 3);
        assert_eq!(conversation.messages[2].content, "EDITED_REQUEST_BODY");
    }

    #[test]
    fn active_history_cannot_be_resumed_by_another_client() {
        let temp = Temporary::new();
        let mut first = temp.history();
        first.record("user", "Still active").unwrap();
        let id = first.sessions().unwrap()[0].id.clone();
        let mut second = temp.history();
        assert!(second.resume(&id).is_err());
        assert_eq!(
            second.read(&id).unwrap().messages[0].content,
            "Still active"
        );
        drop(first);
        assert!(second.resume(&id).is_ok());
    }

    #[test]
    fn history_is_scoped_to_workspace_and_rejects_path_traversal() {
        let temp = Temporary::new();
        let mut first = temp.history();
        first.record("user", "Private workspace history").unwrap();
        let id = first.sessions().unwrap()[0].id.clone();
        let mut other =
            History::with_workspace(temp.0.join("conversations"), "/different".into()).unwrap();
        assert!(other.sessions().unwrap().is_empty());
        assert!(other.read(&id).is_err());
        assert!(other.resume(&id).is_err());
        for id in [
            "../outside",
            "/etc/passwd",
            "conversation_../escape",
            "conversation_bad",
            "",
        ] {
            assert!(first.read(id).is_err());
        }
        assert!(first.record("admin", "Unrecognized role").is_err());
        assert!(first.record("user", "bad\u{001b}[2J").is_err());
    }

    #[test]
    fn per_session_budget_discards_old_messages_with_an_explicit_count() {
        let temp = Temporary::new();
        let mut history = temp.history();
        for _ in 0..40 {
            history.record("user", &"x".repeat(MAX_CONTENT)).unwrap();
        }
        let summary = history.sessions().unwrap().remove(0);
        let conversation = history.read(&summary.id).unwrap();
        assert!(conversation.messages.len() < 40);
        assert_eq!(
            conversation.metadata.truncated_messages + conversation.messages.len() as u64,
            40
        );
        let path = temp
            .0
            .join("conversations")
            .join(session_name(&summary.id).unwrap());
        assert!(fs::metadata(path).unwrap().len() <= MAX_SESSION_BYTES as u64);
    }

    #[cfg(unix)]
    #[test]
    fn transaction_lock_releases_while_an_inherited_descriptor_stays_open() {
        let temp = Temporary::new();
        let history = temp.history();
        let lock = history.directory.lock().unwrap();
        let inherited = lock._file.try_clone().unwrap();
        let busy = Some("conversation storage is busy; retry shortly");
        assert_eq!(history.directory.lock().err().as_deref(), busy);
        drop(lock);
        let next = history.directory.lock().unwrap();
        drop(inherited);
        assert_eq!(history.directory.lock().err().as_deref(), busy);
        drop(next);
        assert!(history.directory.lock().is_ok());
    }

    #[test]
    fn retention_keeps_active_history_and_bounds_closed_sessions() {
        let temp = Temporary::new();
        let mut active = temp.history();
        active
            .record("user", "Preserve this active conversation")
            .unwrap();
        let id = active.sessions().unwrap()[0].id.clone();
        for index in 0..MAX_SESSIONS + 2 {
            let mut history = temp.history();
            history
                .record("user", &format!("Finished conversation {index}"))
                .unwrap();
        }
        let sessions = active.sessions().unwrap();
        assert_eq!(sessions.len(), MAX_SESSIONS);
        assert!(sessions.iter().any(|session| session.id == id));
        assert!(active.read(&id).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn private_storage_rejects_symlinks_hardlinks_fifo_and_shared_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = Temporary::new();
        let outside = temp.0.join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, temp.0.join("linked")).unwrap();
        assert!(History::with_workspace(temp.0.join("linked"), "/fixture".into()).is_err());
        let mut history = temp.history();
        history.record("user", "Owned").unwrap();
        let id = history.sessions().unwrap()[0].id.clone();
        let path = temp
            .0
            .join("conversations")
            .join(session_name(&id).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(history.read(&id).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let linked = random_id("conversation").unwrap();
        fs::hard_link(
            &path,
            temp.0
                .join("conversations")
                .join(session_name(&linked).unwrap()),
        )
        .unwrap();
        assert!(history.read(&id).is_err());
        fs::remove_file(
            temp.0
                .join("conversations")
                .join(session_name(&linked).unwrap()),
        )
        .unwrap();
        let link = random_id("conversation").unwrap();
        symlink(
            &path,
            temp.0
                .join("conversations")
                .join(session_name(&link).unwrap()),
        )
        .unwrap();
        assert!(history.read(&link).is_err());
        let fifo_id = random_id("conversation").unwrap();
        let fifo = temp
            .0
            .join("conversations")
            .join(session_name(&fifo_id).unwrap());
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(history.read(&fifo_id).is_err());
        assert_eq!(history.read(&id).unwrap().messages.len(), 1);
    }

    #[test]
    fn incomplete_or_newer_schema_files_are_not_resumed() {
        let temp = Temporary::new();
        let mut history = temp.history();
        history.record("user", "Keep valid history").unwrap();
        let id = history.sessions().unwrap()[0].id.clone();
        let path = temp
            .0
            .join("conversations")
            .join(session_name(&id).unwrap());
        let original = fs::read(&path).unwrap();
        fs::write(&path, &original[..original.len() - 1]).unwrap();
        assert!(history.resume(&id).is_err());
        let mut text = String::from_utf8(original).unwrap();
        text = text.replace(SCHEMA, "ssmcp.conversation.v99");
        fs::write(&path, text).unwrap();
        assert!(history.read(&id).is_err());
    }
}
