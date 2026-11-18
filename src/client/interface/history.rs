//! Versioned JSONL conversation history. It never restores tools or delivery state.
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

static HISTORY: OnceLock<Mutex<Option<History>>> = OnceLock::new();

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
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub message_count: usize,
    pub truncated_messages: u64,
    pub current: bool,
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

fn validate_content(text: &str) -> Result<(), String> {
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

fn request_identity(request_id: &str, text: &str) -> Result<String, String> {
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
    let mut state = HISTORY
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.is_none() {
        *state = Some(History::at(default_directory()?)?);
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
        }
    }

    fn histories(&self) -> Result<Vec<(SessionSummary, usize)>, String> {
        let mut output = Vec::new();
        for name in self.directory.names()? {
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
            let file = self.directory.open(&name, false, false, false)?;
            let bytes = usize::try_from(
                file.metadata()
                    .map_err(|_| "cannot inspect history size")?
                    .len(),
            )
            .map_err(|_| "conversation file size is unsupported")?;
            output.push((self.summary(&conversation), bytes));
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
        let histories = self.histories()?;
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
            if history.id == id {
                continue;
            }
            if let Ok(lease) = self.lease(&history.id) {
                reclaim.push((history.id.clone(), lease));
                count -= 1;
                total -= size;
            }
        }
        if count > MAX_SESSIONS || total > MAX_TOTAL_BYTES {
            return Err("conversation retention is full of active sessions; close an older client and retry".into());
        }
        for (id, _lease) in reclaim {
            self.directory.remove(&session_name(&id)?)?;
            self.directory.remove(&format!("{id}.lease"))?;
        }
        Ok(())
    }

    fn delivered_source(&self, request_id: &str, text: &str) -> Result<(String, Message), String> {
        let identity = request_identity(request_id, text)?;
        let mut expected_text = safe_text(text);
        if expected_text.len() > MAX_CONTENT {
            let mut end = MAX_CONTENT;
            while !expected_text.is_char_boundary(end) {
                end -= 1;
            }
            expected_text.truncate(end);
        }
        let _lock = self.directory.lock()?;
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
                return Ok((id, message.clone()));
            }
        }
        Err("delivered request source is absent from retained JSONL; memory was not changed".into())
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
        let (role, kind) = role_and_kind(role)?;
        validate_content(text)?;
        let _lock = self.directory.lock()?;
        let mut new_active = None;
        let mut conversation = if let Some(active) = &self.active {
            self.load(&active.metadata.id)?
        } else {
            let metadata = SessionMetadata {
                schema: SCHEMA.into(),
                schema_version: 1,
                id: random_id("conversation")?,
                workspace: self.workspace.clone(),
                created_at_ms: now_ms(),
                truncated_messages: 0,
            };
            let lease = self.lease(&metadata.id)?;
            new_active = Some(Active {
                metadata: metadata.clone(),
                _lease: lease,
            });
            Conversation {
                metadata,
                messages: Vec::new(),
            }
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
        if let Some(active) = new_active {
            self.active = Some(active);
        }
        if let Some(active) = &mut self.active {
            active.metadata = conversation.metadata;
        }
        Ok(committed)
    }

    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        let _lock = self.directory.lock()?;
        Ok(self
            .histories()?
            .into_iter()
            .map(|(summary, _)| summary)
            .filter(|summary| summary.workspace == self.workspace)
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

    fn resume(&mut self, id: &str) -> Result<Conversation, String> {
        let _lock = self.directory.lock()?;
        let conversation = self.load(id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
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
pub(crate) fn record_agent(id: &str, text: &str) -> Result<(), String> {
    let committed = with(|history| history.record_agent(id, text))?;
    remember_committed(committed);
    Ok(())
}
pub(crate) fn record_request(request_id: &str, text: &str) -> Result<(), String> {
    with(|history| history.record_request(request_id, text)).map(|_| ())
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
pub(crate) fn sessions() -> Result<Vec<SessionSummary>, String> {
    with(|history| history.sessions())
}
pub(crate) fn read(id: &str) -> Result<Conversation, String> {
    with(|history| history.read(id))
}
pub(crate) fn resume(id: &str) -> Result<Conversation, String> {
    let conversation = with(|history| history.resume(id))?;
    // Update copy state after releasing the history lock, without display or record hooks.
    super::messages::restore_copy(&conversation.messages);
    Ok(conversation)
}

pub(crate) struct SafeDirectory {
    path: PathBuf,
    #[cfg(unix)]
    directory: File,
    #[cfg(windows)]
    directory: cap_std::fs::Dir,
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

    pub(crate) fn lock(&self) -> Result<File, String> {
        let file = self.open(".history.lock", true, true, false)?;
        crate::platform::lock_file(&file, true)
            .map_err(|_| "conversation storage is busy; retry shortly")?;
        Ok(file)
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
}

pub(super) fn call(arguments: Value) -> Result<Value, String> {
    if arguments
        .as_object()
        .is_none_or(|object| object.values().any(Value::is_null))
    {
        return Err("history expects an object; omit optional fields instead of null".into());
    }
    let request: Request =
        serde_json::from_value(arguments).map_err(|_| "expected a history list or read action")?;
    let (offset, limit) = match &request {
        Request::List { offset, limit } | Request::Read { offset, limit, .. } => {
            (offset.unwrap_or(0), limit.unwrap_or(16))
        }
    };
    if !(1..=64).contains(&limit) {
        return Err("history page limit must be in 1..=64".into());
    }
    match request {
        Request::List { .. } => {
            let sessions = sessions()?;
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
                json!({"schema_version":1,"schema":SCHEMA,"sessions":page,"total":total,"offset":offset,
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
        "description":"Read this workspace's local, redacted conversation history. list returns recent session summaries; read returns versioned metadata and message pages. Pages are at most 64 KiB: follow next_offset. The open JSONL schema is ssmcp.conversation.v1 with system/user/assistant/tool roles and string content. This does not restore any external harness session, invoke a model, resend old user requirements, or expose saved agent credentials. The user chooses /resume locally to display and continue a past conversation. Only current-workspace histories are readable. At most 64 histories / 16 MiB are retained; each history keeps at most 500 messages / 512 KiB and reports truncated_messages.",
        "inputSchema":{"type":"object","oneOf":[
            {"type":"object","required":["action"],"additionalProperties":false,"properties":{
                "action":{"const":"list"},"offset":offset,"limit":page
            }},
            {"type":"object","required":["action","id"],"additionalProperties":false,"properties":{
                "action":{"const":"read"},
                "id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"},
                "offset":offset,"limit":page
            }}
        ]},
        "annotations":{"readOnlyHint":true,"destructiveHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(random_id("history_test").unwrap());
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
        fn history(&self) -> History {
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
