//! Workspace-local memory checkpoints. Source JSONL and request delivery are never mutated.
use super::interface::history::{self, SafeDirectory};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const SCHEMA: &str = "ssmcp.context-memory.v1";
const FILE: &str = "state.json";
const MAX_BRANCHES: usize = 8;
const MAX_BRANCH_BYTES: usize = 24 * 1024;
const MAX_STATE_BYTES: usize = 256 * 1024;
const MAX_SUMMARY: usize = 8 * 1024;
const MAX_RECENT: usize = 8;
const KEEP_RECENT: usize = 4;
const MAX_RECENT_TEXT: usize = 1536;
const MAX_RECORD_TEXT: usize = 16 * 1024;
const MAX_SOURCES: usize = 16;
// Leave room for the server's client_id/context envelope inside its 12 KiB cap.
const MAX_SNAPSHOT: usize = 12 * 1024 - 256;
const MAX_REVISION: u64 = (1 << 53) - 1;

static MEMORY: OnceLock<Mutex<Local>> = OnceLock::new();

#[derive(Default)]
struct Local {
    store: Option<Arc<Store>>,
    error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Source {
    conversation_id: String,
    message_id: String,
    sha256: String,
}
impl Source {
    fn same_message(&self, other: &Self) -> bool {
        self.conversation_id == other.conversation_id && self.message_id == other.message_id
    }
    fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.conversation_id, "conversation")
            || !valid_id(&self.message_id, "message")
            || !hex(&self.sha256, 64)
        {
            return Err("invalid context history source identity".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Delivery {
    request_id: String,
    revision: u64,
    target: String,
}
impl Delivery {
    fn validate(&self) -> Result<(), String> {
        if self.target != "next"
            || self.revision == 0
            || self.request_id.is_empty()
            || self.request_id.len() > 128
            || !self
                .request_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        {
            return Err("only a confirmed next-caller delivery may enter shared memory".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Recent {
    source: Source,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivery: Option<Delivery>,
    role: String,
    text: String,
    created_at_ms: u64,
    source_text_bytes: usize,
    truncated: bool,
}
impl Recent {
    fn validate(&self) -> Result<(), String> {
        self.source.validate()?;
        match (self.role.as_str(), self.delivery.as_ref()) {
            ("assistant", None) => {}
            ("user", Some(delivery)) => delivery.validate()?,
            _ => {
                return Err(
                    "unconfirmed or targeted user text cannot enter shared context memory".into(),
                );
            }
        }
        // A valid source may start with a long whitespace prefix. An excerpt
        // still represents that source and need not itself contain a word.
        if self.text.is_empty()
            || self.text.len() > MAX_RECENT_TEXT
            || self
                .text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
        {
            return Err("invalid recent context excerpt".into());
        }
        if !matches!(self.role.as_str(), "user" | "assistant")
            || self.source_text_bytes < self.text.len()
            || self.source_text_bytes > MAX_RECORD_TEXT
            || (!self.truncated
                && (self.source_text_bytes != self.text.len()
                    || self.source.sha256 != digest(&self.text)))
        {
            return Err("invalid recent context record".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    LocalExtractive,
    ProvidedSummary,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    mode: Mode,
    text: String,
    created_at_ms: u64,
    sources: Vec<Source>,
    omitted_sources: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Branch {
    id: String,
    name: String,
    parent_id: Option<String>,
    revision: u64,
    created_at_ms: u64,
    updated_at_ms: u64,
    checkpoint: Option<Checkpoint>,
    recent: Vec<Recent>,
    omitted_recent: u64,
}
impl Branch {
    fn mode(&self) -> &str {
        match self.checkpoint.as_ref().map(|c| c.mode) {
            None => "recent",
            Some(Mode::LocalExtractive) => "local_extractive",
            Some(Mode::ProvidedSummary) => "provided_summary",
        }
    }
    fn trim_recent(&mut self) -> Result<(), String> {
        while self.recent.len() > MAX_RECENT
            || serde_json::to_vec(self)
                .map_err(|_| "cannot encode context branch")?
                .len()
                > MAX_BRANCH_BYTES
        {
            if self.recent.len() <= 1 {
                return Err("context branch exceeds its 24 KiB budget; shorten the summary".into());
            }
            self.recent.remove(0);
            self.omitted_recent = self.omitted_recent.saturating_add(1);
        }
        Ok(())
    }
    fn validate(&self, revision: u64) -> Result<(), String> {
        if !valid_id(&self.id, "context")
            || self.revision == 0
            || self.revision > revision
            || self.recent.len() > MAX_RECENT
        {
            return Err("invalid context branch identity, revision or recent count".into());
        }
        one_line(&self.name, 80, "context branch name")?;
        if let Some(checkpoint) = &self.checkpoint {
            text(&checkpoint.text, MAX_SUMMARY, "context summary")?;
            if checkpoint.sources.len() > MAX_SOURCES {
                return Err("too many context checkpoint sources".into());
            }
            for (index, source) in checkpoint.sources.iter().enumerate() {
                source.validate()?;
                if checkpoint.sources[..index]
                    .iter()
                    .any(|prior| prior.same_message(source))
                {
                    return Err("duplicate context checkpoint source identity".into());
                }
            }
        }
        for (index, record) in self.recent.iter().enumerate() {
            record.validate()?;
            if self.checkpoint.as_ref().is_some_and(|checkpoint| {
                checkpoint.sources.iter().any(|source| {
                    source.same_message(&record.source) && source.sha256 != record.source.sha256
                })
            }) {
                return Err("conflicting context source hashes for one history identity".into());
            }
            if self.recent[..index]
                .iter()
                .any(|previous| previous.source.same_message(&record.source))
            {
                return Err("duplicate context recent source identity".into());
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "cannot encode context branch")?
            .len()
            > MAX_BRANCH_BYTES
        {
            return Err("context branch exceeds its 24 KiB budget".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Document {
    schema: String,
    schema_version: u32,
    workspace: String,
    workspace_id: String,
    revision: u64,
    active_id: String,
    branches: Vec<Branch>,
}
impl Document {
    fn new(workspace: &str) -> Result<Self, String> {
        let id = random_id()?;
        let now = now_ms();
        Ok(Self {
            schema: SCHEMA.into(),
            schema_version: 1,
            workspace: workspace.into(),
            workspace_id: digest(workspace),
            revision: 1,
            active_id: id.clone(),
            branches: vec![Branch {
                id,
                name: "main".into(),
                parent_id: None,
                revision: 1,
                created_at_ms: now,
                updated_at_ms: now,
                checkpoint: None,
                recent: Vec::new(),
                omitted_recent: 0,
            }],
        })
    }
    fn validate(&self, workspace: &str) -> Result<(), String> {
        if self.schema != SCHEMA || self.schema_version != 1 {
            return Err(
                "unsupported context memory schema; the existing file was preserved".into(),
            );
        }
        if self.workspace != workspace || self.workspace_id != digest(workspace) {
            return Err("context memory belongs to a different workspace".into());
        }
        if self.revision == 0 || self.revision > MAX_REVISION {
            return Err("invalid context memory revision".into());
        }
        if self.branches.is_empty() || self.branches.len() > MAX_BRANCHES {
            return Err("context memory must contain 1–8 branches".into());
        }
        self.branch(&self.active_id)?;
        for (index, branch) in self.branches.iter().enumerate() {
            branch.validate(self.revision)?;
            if self.branches[..index]
                .iter()
                .any(|prior| prior.id == branch.id)
            {
                return Err("duplicate context branch identity".into());
            }
            if let Some(parent) = &branch.parent_id {
                if parent == &branch.id {
                    return Err("context branch cannot be its own parent".into());
                }
                self.branch(parent)?;
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "cannot encode context memory")?
            .len()
            > MAX_STATE_BYTES
        {
            return Err("context memory exceeds its 256 KiB workspace budget".into());
        }
        Ok(())
    }
    fn branch(&self, id: &str) -> Result<&Branch, String> {
        if !valid_id(id, "context") {
            return Err(
                "branch id must be context_ followed by 32 lowercase hexadecimal digits".into(),
            );
        }
        self.branches
            .iter()
            .find(|branch| branch.id == id)
            .ok_or_else(|| {
                "context branch does not exist in this workspace; use ssmcp.context.list".into()
            })
    }
    fn active_mut(&mut self) -> &mut Branch {
        self.branches
            .iter_mut()
            .find(|b| b.id == self.active_id)
            .expect("validated active branch")
    }
}

struct Store {
    directory: SafeDirectory,
    workspace: String,
    restored: bool,
}
impl Store {
    fn at(base: PathBuf, workspace: String) -> Result<Self, String> {
        if workspace.len() > 4096
            || !PathBuf::from(&workspace).is_absolute()
            || workspace.chars().any(char::is_control)
        {
            return Err("context workspace must be a bounded absolute path".into());
        }
        let directory = SafeDirectory::at(base.join(digest(&workspace)))?;
        let lock = directory.lock()?;
        let restored = directory.names()?.iter().any(|name| name == FILE);
        let store = Self {
            directory,
            workspace,
            restored,
        };
        if restored {
            store.load()?;
        } else {
            let document = Document::new(&store.workspace)?;
            store.save(&document)?;
        }
        drop(lock);
        Ok(store)
    }
    fn load(&self) -> Result<Document, String> {
        let file = self.directory.open(FILE, false, false, false)?;
        let mut bytes = Vec::new();
        file.take(MAX_STATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read context memory")?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err("context memory file exceeds 256 KiB; it was preserved".into());
        }
        let document: Document = serde_json::from_slice(&bytes).map_err(
            |_| "context memory is invalid JSON or contains unsupported fields; it was preserved",
        )?;
        document.validate(&self.workspace)?;
        Ok(document)
    }
    fn save(&self, document: &Document) -> Result<(), String> {
        document.validate(&self.workspace)?;
        let bytes = serde_json::to_vec(document).map_err(|_| "cannot encode context memory")?;
        self.directory.write_atomic(FILE, &bytes)
    }
    fn read(&self) -> Result<Document, String> {
        let _lock = self.directory.lock()?;
        self.load()
    }
    fn update(
        &self,
        expected_revision: Option<u64>,
        update: impl FnOnce(&mut Document, u64) -> Result<bool, String>,
    ) -> Result<Document, String> {
        let _lock = self.directory.lock()?;
        let mut document = self.load()?;
        if expected_revision.is_some_and(|revision| revision != document.revision) {
            return Err(format!(
                "context revision changed (current {}); read again before retrying",
                document.revision
            ));
        }
        let revision = document
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_REVISION)
            .ok_or("context revision exhausted; existing memory was preserved")?;
        if update(&mut document, revision)? {
            document.revision = revision;
            self.save(&document)?;
        }
        Ok(document)
    }
    #[cfg(test)]
    fn record(&self, conversation_id: &str, message: &history::Message) -> Result<(), String> {
        let delivery = (message.role == "user").then(|| Delivery {
            request_id: message.id.clone(),
            revision: 1,
            target: "next".into(),
        });
        self.record_confirmed(conversation_id, message, delivery)
    }

    fn record_confirmed(
        &self,
        conversation_id: &str,
        message: &history::Message,
        delivery: Option<Delivery>,
    ) -> Result<(), String> {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            return Ok(());
        }
        text(&message.content, MAX_RECORD_TEXT, "history context text")?;
        let source = Source {
            conversation_id: conversation_id.into(),
            message_id: message.id.clone(),
            sha256: digest(&message.content),
        };
        source.validate()?;
        let retained = clipped(&message.content, MAX_RECENT_TEXT);
        let record = Recent {
            source,
            delivery,
            role: message.role.clone(),
            truncated: message.truncated || retained.len() < message.content.len(),
            text: retained,
            source_text_bytes: message.content.len(),
            created_at_ms: message.created_at_ms,
        };
        record.validate()?;
        self.update(None, |document, revision| {
            let branch = document.active_mut();
            let prior = branch
                .recent
                .iter()
                .map(|r| &r.source)
                .chain(branch.checkpoint.iter().flat_map(|c| c.sources.iter()))
                .find(|prior| prior.same_message(&record.source));
            if let Some(prior) = prior {
                if prior.sha256 != record.source.sha256 {
                    return Err(
                        "context source identity conflicts with different history text".into(),
                    );
                }
                return Ok(false);
            }
            branch.recent.push(record);
            branch.revision = revision;
            branch.updated_at_ms = now_ms().max(branch.updated_at_ms);
            branch.trim_recent()?;
            Ok(true)
        })?;
        Ok(())
    }
    fn compact(&self, summary: Option<String>, expected: Option<u64>) -> Result<Document, String> {
        if let Some(summary) = &summary {
            text(summary, MAX_SUMMARY, "context summary")?;
        }
        self.update(expected, |document, revision| {
            let branch = document.active_mut();
            let mut sources = Vec::new();
            let mut omitted_sources = branch.checkpoint.as_ref().map_or(0, |c| c.omitted_sources);
            for source in branch
                .checkpoint
                .iter()
                .flat_map(|c| c.sources.iter())
                .chain(branch.recent.iter().map(|r| &r.source))
            {
                if !sources
                    .iter()
                    .any(|prior: &Source| prior.same_message(source))
                {
                    sources.push(source.clone());
                }
            }
            if sources.len() > MAX_SOURCES {
                let removed = sources.len() - MAX_SOURCES;
                sources.drain(..removed);
                omitted_sources = omitted_sources.saturating_add(removed as u64);
            }
            let (mode, summary) = match summary {
                Some(summary) => (Mode::ProvidedSummary, summary),
                None => (Mode::LocalExtractive, extractive(branch)),
            };
            let now = now_ms().max(branch.updated_at_ms);
            branch.checkpoint = Some(Checkpoint {
                mode,
                text: summary,
                created_at_ms: now,
                sources,
                omitted_sources,
            });
            if branch.recent.len() > KEEP_RECENT {
                let removed = branch.recent.len() - KEEP_RECENT;
                branch.recent.drain(..removed);
                branch.omitted_recent = branch.omitted_recent.saturating_add(removed as u64);
            }
            branch.revision = revision;
            branch.updated_at_ms = now;
            branch.trim_recent()?;
            Ok(true)
        })
    }
    fn fork(
        &self,
        name: Option<String>,
        activate: bool,
        expected: Option<u64>,
    ) -> Result<(Document, String), String> {
        if let Some(name) = &name {
            one_line(name, 80, "context branch name")?;
        }
        let id = random_id()?;
        let document = self.update(expected, |document, revision| {
            if document.branches.len() >= MAX_BRANCHES {
                return Err(
                    "context branch limit reached (8); switch to an existing branch".into(),
                );
            }
            let mut branch = document.branch(&document.active_id)?.clone();
            branch.parent_id = Some(branch.id.clone());
            branch.id = id.clone();
            branch.name = name.unwrap_or_else(|| format!("fork-{}", &id[8..16]));
            branch.revision = revision;
            branch.created_at_ms = now_ms();
            branch.updated_at_ms = branch.created_at_ms;
            if activate {
                document.active_id = id.clone();
            }
            document.branches.push(branch);
            Ok(true)
        })?;
        Ok((document, id))
    }
    fn switch(&self, id: &str, expected: Option<u64>) -> Result<Document, String> {
        self.update(expected, |document, _| {
            document.branch(id)?;
            if document.active_id == id {
                return Ok(false);
            }
            document.active_id = id.into();
            Ok(true)
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}
fn digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn hex(value: &str, bytes: usize) -> bool {
    value.len() == bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn valid_id(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|v| v.strip_prefix('_'))
        .is_some_and(|v| hex(v, 32))
}
fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes)
        .map_err(|_| "cannot create context branch identity")?;
    Ok(format!(
        "context_{}",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}
fn text(value: &str, limit: usize, label: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > limit {
        return Err(format!("{label} must contain 1–{limit} UTF-8 bytes"));
    }
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\t' | '\r'))
    {
        return Err(format!(
            "{label} must not contain terminal control characters"
        ));
    }
    Ok(())
}
fn one_line(value: &str, limit: usize, label: &str) -> Result<(), String> {
    text(value, limit, label)?;
    if value.chars().any(char::is_control) {
        return Err(format!(
            "{label} must be one line without control characters"
        ));
    }
    Ok(())
}
fn clipped(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}
fn extractive(branch: &Branch) -> String {
    // Repeating an extractive checkpoint with no older records to fold must
    // not keep nesting its own headings until they displace the saved text.
    if branch.recent.len() <= KEEP_RECENT
        && let Some(checkpoint) = &branch.checkpoint
        && checkpoint.mode == Mode::LocalExtractive
    {
        return checkpoint.text.clone();
    }
    let mut summary =
        String::from("Local extractive checkpoint; no model or semantic summarization was used.\n");
    if let Some(previous) = &branch.checkpoint {
        summary.push_str("\nPrevious checkpoint excerpt:\n");
        summary.push_str(&clipped(&previous.text, 3 * 1024));
        if previous.text.len() > 3 * 1024 {
            summary.push_str("\n[previous checkpoint excerpt truncated]");
        }
        summary.push('\n');
    }
    let older = branch.recent.len().saturating_sub(KEEP_RECENT);
    let count = if older == 0 {
        branch.recent.len()
    } else {
        older
    };
    for recent in branch.recent.iter().take(count) {
        summary.push_str(&format!(
            "\n[{} · {}]\n",
            recent.role, recent.source.message_id
        ));
        summary.push_str(&clipped(&recent.text, 512));
        if recent.text.len() > 512 || recent.truncated {
            summary.push_str("\n[excerpt truncated; read source JSONL]");
        }
        summary.push('\n');
    }
    if branch.recent.is_empty() && branch.checkpoint.is_none() {
        summary.push_str("No confirmed conversation records are available yet.\n");
    }
    if branch.omitted_recent > 0 {
        summary.push_str(&format!("\n{} earlier records were omitted from bounded recent memory; source history follows its existing retention.\n", branch.omitted_recent));
    }
    clipped(&summary, MAX_SUMMARY)
}

pub(crate) fn initialize() -> Result<(), String> {
    let local = MEMORY.get_or_init(Default::default);
    if local
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .store
        .is_some()
    {
        return Ok(());
    }
    // Directory validation and disk reads never run while holding the global
    // memory handle. A racing successful initialization must not be overwritten.
    let outcome = (|| {
        let workspace = history::workspace()?;
        let history_directory = history::default_directory()?;
        let base = history_directory
            .parent()
            .ok_or("cannot determine context state directory")?
            .join("context-memory");
        Store::at(base, workspace).map(Arc::new)
    })();
    let mut state = local.lock().unwrap_or_else(|e| e.into_inner());
    if state.store.is_some() {
        return Ok(());
    }
    match outcome {
        Ok(store) => {
            state.store = Some(store);
            state.error = None;
            Ok(())
        }
        Err(error) => {
            state.error = Some(error.clone());
            Err(error)
        }
    }
}
fn store_handle() -> Result<Option<Arc<Store>>, String> {
    let state = MEMORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match (&state.store, &state.error) {
        (Some(store), _) => Ok(Some(Arc::clone(store))),
        (None, Some(error)) => Err(format!("context memory unavailable: {error}")),
        (None, None) => Ok(None),
    }
}
fn with_store<T>(operation: impl FnOnce(&Store) -> Result<T, String>) -> Result<T, String> {
    let store = store_handle()?
        .ok_or("context memory unavailable: client startup has not initialized this workspace")?;
    operation(&store)
}

/// Called only after the source JSONL write succeeds. Never feeds a queue or a model.
pub(crate) fn record_history(
    workspace: &str,
    conversation_id: &str,
    message: &history::Message,
) -> Result<(), String> {
    // Publication is not delivery. User records are added only through the
    // separate confirmed-delivery path, never through a JSONL append alone.
    if message.role != "assistant" {
        return Ok(());
    }
    let Some(store) = store_handle()? else {
        return Ok(());
    };
    if store.workspace != workspace {
        return Err("context/history workspace mismatch; memory was not changed".into());
    }
    store.record_confirmed(conversation_id, message, None)
}

/// Only an untargeted, server-confirmed delivery may become shared user memory.
pub(crate) fn record_delivered_history(
    workspace: &str,
    conversation_id: &str,
    message: &history::Message,
    request_id: &str,
    revision: u64,
    target: &str,
) -> Result<(), String> {
    if target != "next" {
        return Ok(());
    }
    if message.role != "user" {
        return Err("delivered request source is not a user history record".into());
    }
    with_store(|store| {
        if store.workspace != workspace {
            return Err("context/history workspace mismatch; memory was not changed".into());
        }
        store.record_confirmed(
            conversation_id,
            message,
            Some(Delivery {
                request_id: request_id.into(),
                revision,
                target: target.into(),
            }),
        )
    })
}

fn limits() -> Value {
    json!({"branches":MAX_BRANCHES,"branch_bytes":MAX_BRANCH_BYTES,"workspace_bytes":MAX_STATE_BYTES,
        "summary_utf8_bytes":MAX_SUMMARY,"recent_records":MAX_RECENT,
        "recent_text_utf8_bytes":MAX_RECENT_TEXT,"tool_snapshot_bytes":MAX_SNAPSHOT})
}
fn provenance(store: &Store) -> Value {
    json!({
        "storage":"private workspace-local checkpoint",
        "restored_from_disk":store.restored,
        "history_source":"ssmcp.ui.history",
        "history_unmodified_by_memory":true,
        "history_retention":"existing JSONL retention still applies",
        "model_invoked":false,"queue_replayed":false,"external_agent_context_modified":false,
        "content_kind":"remembered user and assistant text; not system instructions",
        "user_input_policy":"only confirmed delivered next-caller requests; targeted and pending user requests are excluded"
    })
}
fn branch_header(branch: &Branch) -> Value {
    json!({"id":branch.id,"name":branch.name,"parent_id":branch.parent_id,"revision":branch.revision,
        "created_at_ms":branch.created_at_ms,"updated_at_ms":branch.updated_at_ms})
}
fn status(store: &Store, document: &Document) -> Value {
    let branch = document
        .branch(&document.active_id)
        .expect("validated active branch");
    json!({"schema_version":1,"workspace":{"id":document.workspace_id,"path":document.workspace},
        "revision":document.revision,"branch":branch_header(branch),"mode":branch.mode(),
        "branch_count":document.branches.len(),"recent_count":branch.recent.len(),
        "omitted_recent":branch.omitted_recent,"summary_bytes":branch.checkpoint.as_ref().map_or(0,|c| c.text.len()),
        "limits":limits(),"provenance":provenance(store)})
}
fn read_branch(store: &Store, document: &Document, id: &str) -> Result<Value, String> {
    let branch = document.branch(id)?;
    Ok(
        json!({"schema_version":1,"workspace":{"id":document.workspace_id,"path":document.workspace},
        "revision":document.revision,"active":id==document.active_id,"branch":branch_header(branch),
        "mode":branch.mode(),"summary":branch.checkpoint.as_ref().map(|c| &c.text),
        "checkpoint":branch.checkpoint,"recent":branch.recent,"omitted_recent":branch.omitted_recent,
        "provenance":provenance(store)}),
    )
}
fn snapshot(store: &Store, document: &Document) -> Result<Value, String> {
    let branch = document.branch(&document.active_id)?;
    let mut value = read_branch(store, document, &document.active_id)?;
    // Full sources are retained by context.read. Avoid copying summary text twice.
    value.as_object_mut().expect("object").remove("checkpoint");
    value["context_sources"] = json!(branch.checkpoint.as_ref().map(|c| &c.sources));
    value["workspace"]["path"] = json!(clipped(&document.workspace, 512));
    value["snapshot"] = json!({"summary_truncated":false,"recent_omitted":0,
        "workspace_path_truncated":document.workspace.len()>512,"full_read":"ssmcp.context.read"});
    loop {
        if serde_json::to_vec(&value)
            .map_err(|_| "cannot encode context snapshot")?
            .len()
            <= MAX_SNAPSHOT
        {
            return Ok(value);
        }
        let recent = value["recent"].as_array_mut().expect("recent array");
        if !recent.is_empty() {
            recent.remove(0);
            let omitted = value["snapshot"]["recent_omitted"].as_u64().unwrap_or(0) + 1;
            value["snapshot"]["recent_omitted"] = json!(omitted);
            continue;
        }
        if let Some(summary) = value["summary"].as_str().filter(|s| s.len() > 128) {
            value["summary"] = json!(clipped(summary, summary.len() / 2));
            value["snapshot"]["summary_truncated"] = json!(true);
            continue;
        }
        return Err("context snapshot cannot fit its 12 KiB budget".into());
    }
}

/// Optional side metadata; never routed through user_messages or interpreted as a command.
pub(crate) fn snapshot_for_tool() -> Option<Value> {
    with_store(|store| snapshot(store, &store.read()?)).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadRequest {
    id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactRequest {
    summary: Option<String>,
    expected_revision: Option<u64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkRequest {
    name: Option<String>,
    #[serde(default = "yes")]
    activate: bool,
    expected_revision: Option<u64>,
}
fn yes() -> bool {
    true
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SwitchRequest {
    id: String,
    expected_revision: Option<u64>,
}
fn parse<T: serde::de::DeserializeOwned>(arguments: Value) -> Result<T, String> {
    serde_json::from_value(arguments)
        .map_err(|_| "invalid context arguments; use the tool's declared schema".into())
}

/// UI projections share the same on-disk document and revision as native tools.
pub(crate) fn ui_snapshot() -> Result<Value, String> {
    with_store(|store| {
        let document = store.read()?;
        Ok(json!({
            "status":status(store, &document),
            "read":read_branch(store, &document, &document.active_id)?,
            "list":listing(&document)
        }))
    })
}
fn listing(document: &Document) -> Value {
    json!({"schema_version":1,"workspace":{"id":document.workspace_id,"path":document.workspace},
        "revision":document.revision,"active_id":document.active_id,
        "branches":document.branches.iter().map(|branch| json!({
            "branch":branch_header(branch),"mode":branch.mode(),"recent_count":branch.recent.len(),
            "summary_bytes":branch.checkpoint.as_ref().map_or(0, |c| c.text.len()),
            "active":branch.id==document.active_id})).collect::<Vec<_>>(),"limits":limits()})
}

pub(crate) fn call(name: &str, arguments: Value) -> Result<Value, String> {
    with_store(|store| match name {
        "ssmcp.context.status" => {
            let _: Empty = parse(arguments)?;
            Ok(status(store, &store.read()?))
        }
        "ssmcp.context.list" => {
            let _: Empty = parse(arguments)?;
            Ok(listing(&store.read()?))
        }
        "ssmcp.context.read" => {
            let request: ReadRequest = parse(arguments)?;
            let document = store.read()?;
            read_branch(
                store,
                &document,
                request.id.as_deref().unwrap_or(&document.active_id),
            )
        }
        "ssmcp.context.compact" => {
            let request: CompactRequest = parse(arguments)?;
            let document = store.compact(request.summary, request.expected_revision)?;
            read_branch(store, &document, &document.active_id)
        }
        "ssmcp.context.fork" => {
            let request: ForkRequest = parse(arguments)?;
            let (document, id) =
                store.fork(request.name, request.activate, request.expected_revision)?;
            let mut value = read_branch(store, &document, &id)?;
            value["active_id"] = json!(document.active_id);
            Ok(value)
        }
        "ssmcp.context.switch" => {
            let request: SwitchRequest = parse(arguments)?;
            let document = store.switch(&request.id, request.expected_revision)?;
            read_branch(store, &document, &document.active_id)
        }
        _ => Err("unknown context memory operation".into()),
    })
}

pub(crate) fn command(name: &str, arguments: &str) -> Result<String, String> {
    let arguments = arguments.trim();
    match name {
        "context" => {
            if !arguments.is_empty() {
                return Err("usage: /context".into());
            }
            let value = call("ssmcp.context.list", json!({}))?;
            let mut output = format!("Local context · revision {}\n\n", value["revision"]);
            for entry in value["branches"].as_array().expect("branch list") {
                output.push_str(&format!(
                    "{} {} · {} · {}\n",
                    if entry["active"] == true {
                        "●"
                    } else {
                        "○"
                    },
                    entry["branch"]["name"].as_str().unwrap_or(""),
                    entry["branch"]["id"].as_str().unwrap_or(""),
                    entry["mode"].as_str().unwrap_or("")
                ));
            }
            output.push_str("\nUse /memory to inspect, /compact to checkpoint, /fork [name], /switch <id>.\nLocal memory does not change an external model's token window or replay queued work.");
            Ok(output)
        }
        "memory" => {
            if !arguments.is_empty() {
                return Err("usage: /memory".into());
            }
            let value = call("ssmcp.context.read", json!({}))?;
            let mut output = format!(
                "Memory · {} · revision {} · {}\n\n",
                value["branch"]["name"].as_str().unwrap_or(""),
                value["revision"],
                value["mode"].as_str().unwrap_or("")
            );
            output.push_str(
                value["summary"]
                    .as_str()
                    .unwrap_or("No checkpoint summary yet."),
            );
            for entry in value["recent"].as_array().expect("recent array") {
                output.push_str(&format!(
                    "\n\n[{} · {}]\n{}{}",
                    entry["role"].as_str().unwrap_or(""),
                    entry["source"]["message_id"].as_str().unwrap_or(""),
                    entry["text"].as_str().unwrap_or(""),
                    if entry["truncated"] == true {
                        "\n[excerpt; full source remains in conversation history]"
                    } else {
                        ""
                    }
                ));
            }
            Ok(output)
        }
        "compact" | "fork" | "switch" => {
            let before = call("ssmcp.context.status", json!({}))?;
            let expected = before["revision"].as_u64().expect("revision");
            let value = match name {
                "compact" => call(
                    "ssmcp.context.compact",
                    json!({
                        "summary":if arguments.is_empty() {None} else {Some(arguments)},
                        "expected_revision":expected,
                    }),
                )?,
                "fork" => call(
                    "ssmcp.context.fork",
                    json!({
                        "name":if arguments.is_empty() {None} else {Some(arguments)},
                        "activate":true,"expected_revision":expected,
                    }),
                )?,
                _ => {
                    if arguments.is_empty() {
                        return Err("usage: /switch <context_branch_id>".into());
                    }
                    call(
                        "ssmcp.context.switch",
                        json!({"id":arguments,"expected_revision":expected}),
                    )?
                }
            };
            Ok(format!(
                "Local context {} · {} · revision {} · {}\nSource JSONL was not modified; no model was started and no queued request was replayed.",
                match name {
                    "compact" => "checkpoint saved",
                    "fork" => "branch created",
                    _ => "branch restored",
                },
                value["branch"]["id"].as_str().unwrap_or(""),
                value["revision"],
                value["mode"].as_str().unwrap_or("")
            ))
        }
        _ => Err("unknown local context command".into()),
    }
}

pub(crate) fn tools() -> Vec<Value> {
    let revision = json!({"type":"integer","minimum":1,"maximum":MAX_REVISION,
        "description":"Compare with the workspace revision from status/read before changing memory. A stale revision rejects the entire mutation."});
    let id = json!({"type":"string","pattern":"^context_[0-9a-f]{32}$",
        "description":"Opaque branch ID from this workspace's context.list; never a file path."});
    let common = "Workspace-local memory only. Automatic recent memory includes assistant records and confirmed delivered next-caller user requests; pending and targeted requests are excluded. Original conversation JSONL follows its existing retention and is not rewritten. No model is launched, external model context is not changed, and no queued requirement is replayed.";
    [
        ("status", "Read the active local memory branch, revision, bounds and recovery provenance.", json!({}), Vec::<&str>::new(), true),
        ("list", "List at most eight independent memory branches in this workspace.", json!({}), vec![], true),
        ("read", "Read the full bounded summary, recent excerpts and source conversation/message IDs for a branch; defaults to active.", json!({"id":id}), vec![], true),
        ("compact", "Save a checkpoint and retain the latest four records. Supply summary to save caller-authored text (not a verified model summary); omit it for explicitly marked local extractive excerpts. Use expected_revision to prevent a stale overwrite.", json!({"summary":{"type":"string","minLength":1,"maxLength":MAX_SUMMARY,"description":"1–8192 UTF-8 bytes; caller-authored summary, not automatically verified against its source."},"expected_revision":revision}), vec![], false),
        ("fork", "Copy the current memory/checkpoint into a new independent branch; activate defaults to true. Copies no queue or process state. Maximum eight branches.", json!({"name":{"type":"string","minLength":1,"maxLength":80},"activate":{"type":"boolean","default":true},"expected_revision":revision}), vec![], false),
        ("switch", "Restore an existing branch as active; future confirmed history records enter it. Takes an opaque local branch ID, not a path.", json!({"id":id,"expected_revision":revision}), vec!["id"], false),
    ].into_iter().map(|(name, description, properties, required, read_only)| json!({
        "name":format!("ssmcp.context.{name}"),
        "description":format!("{description} {common}"),
        "inputSchema":{"type":"object","additionalProperties":false,"required":required,"properties":properties},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":false},
    })).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("ssmcp-memory-test-{}", random_id().unwrap()));
            crate::platform::private_directory(&root).unwrap();
            Self(root)
        }
        fn store(&self, workspace: &str) -> Store {
            Store::at(self.0.join("memory"), workspace.into()).unwrap()
        }
        fn state_path(&self, workspace: &str) -> PathBuf {
            self.0.join("memory").join(digest(workspace)).join(FILE)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn message(n: usize, role: &str, value: &str) -> history::Message {
        history::Message {
            id: format!("message_{n:032x}"),
            role: role.into(),
            content: value.into(),
            kind: if role == "user" {
                "requirement"
            } else {
                "agent_message"
            }
            .into(),
            created_at_ms: n as u64,
            truncated: false,
        }
    }
    fn conversation() -> &'static str {
        "conversation_00000000000000000000000000000001"
    }

    #[test]
    fn fork_switch_and_restart_restore_isolated_memory_without_source_replay() {
        let temporary = Temporary::new();
        let workspace = "/fixture/workspace-a";
        let store = temporary.store(workspace);
        assert!(!store.restored);
        let first = message(1, "user", "Keep the user goal intact.");
        store.record(conversation(), &first).unwrap();
        let state = store.read().unwrap();
        let main = state.active_id.clone();
        let checkpoint = store
            .compact(
                Some("Goal: preserve user intent.".into()),
                Some(state.revision),
            )
            .unwrap();
        let (forked, branch) = store
            .fork(Some("experiment".into()), true, Some(checkpoint.revision))
            .unwrap();
        assert_eq!(forked.active_id, branch);
        assert_eq!(
            forked.branch(&branch).unwrap().parent_id.as_ref(),
            Some(&main)
        );
        assert_eq!(
            forked.branch(&branch).unwrap().checkpoint,
            forked.branch(&main).unwrap().checkpoint
        );
        store
            .record(conversation(), &message(2, "assistant", "Experiment only"))
            .unwrap();
        let state = store.read().unwrap();
        assert_eq!(state.branch(&main).unwrap().recent.len(), 1);
        assert_eq!(state.branch(&branch).unwrap().recent.len(), 2);
        let restored = temporary.store(workspace);
        assert!(restored.restored);
        assert_eq!(restored.read().unwrap(), state);
        let switched = restored.switch(&main, Some(state.revision)).unwrap();
        assert_eq!(switched.active_id, main);
        assert_eq!(
            switched.branch(&main).unwrap().recent[0].source.message_id,
            first.id
        );
        assert_eq!(switched.branch(&main).unwrap().recent.len(), 1);
        let before_retry = switched.revision;
        restored.record(conversation(), &first).unwrap();
        assert_eq!(restored.read().unwrap().revision, before_retry);
        let snapshot = snapshot(&restored, &restored.read().unwrap()).unwrap();
        assert_eq!(snapshot["provenance"]["queue_replayed"], false);
        assert_eq!(snapshot["provenance"]["model_invoked"], false);
        assert_eq!(
            snapshot["provenance"]["external_agent_context_modified"],
            false
        );
    }

    #[test]
    fn compare_and_swap_rejects_lost_updates_and_bad_ids_without_replacing_disk() {
        let temporary = Temporary::new();
        let store = temporary.store("/fixture/cas");
        let second = temporary.store("/fixture/cas");
        let stale = second.read().unwrap().revision;
        store
            .record(conversation(), &message(1, "user", "New requirement"))
            .unwrap();
        let before = fs::read(temporary.state_path("/fixture/cas")).unwrap();
        assert!(
            second
                .compact(Some("stale overwrite".into()), Some(stale))
                .unwrap_err()
                .contains("revision changed")
        );
        assert!(second.fork(None, true, Some(stale)).is_err());
        assert!(second.switch("../../outside", None).is_err());
        assert!(second.compact(Some("\x1b[2J".into()), None).is_err());
        assert!(
            second
                .compact(Some("界".repeat(MAX_SUMMARY / 3 + 1)), None)
                .is_err()
        );
        assert_eq!(
            fs::read(temporary.state_path("/fixture/cas")).unwrap(),
            before
        );
    }

    #[test]
    fn bounded_unicode_excerpts_and_extractive_checkpoints_keep_provenance() {
        let temporary = Temporary::new();
        let store = temporary.store("/fixture/extractive");
        for n in 1..=20 {
            store
                .record(
                    conversation(),
                    &message(
                        n,
                        if n % 2 == 0 { "assistant" } else { "user" },
                        &format!("Record {n} 界 {}", "界".repeat(1000)),
                    ),
                )
                .unwrap();
        }
        let before = store.read().unwrap();
        assert_eq!(
            before.branch(&before.active_id).unwrap().recent.len(),
            MAX_RECENT
        );
        assert!(before.branch(&before.active_id).unwrap().omitted_recent >= 12);
        let compacted = store.compact(None, Some(before.revision)).unwrap();
        let branch = compacted.branch(&compacted.active_id).unwrap();
        assert_eq!(branch.mode(), "local_extractive");
        assert_eq!(branch.recent.len(), KEEP_RECENT);
        let checkpoint = branch.checkpoint.as_ref().unwrap();
        assert!(
            checkpoint
                .text
                .contains("no model or semantic summarization")
        );
        assert!(checkpoint.text.contains("excerpt truncated"));
        assert!(!checkpoint.sources.is_empty());
        for source in &checkpoint.sources {
            source.validate().unwrap();
            assert_eq!(source.conversation_id, conversation());
        }
        assert!(
            branch
                .recent
                .iter()
                .all(|r| r.truncated && r.text.len() <= MAX_RECENT_TEXT)
        );
        let recent_ids: Vec<_> = branch
            .recent
            .iter()
            .map(|r| r.source.message_id.clone())
            .collect();
        let repeated = store.compact(None, Some(compacted.revision)).unwrap();
        assert_eq!(
            repeated
                .branch(&repeated.active_id)
                .unwrap()
                .checkpoint
                .as_ref()
                .unwrap()
                .text,
            checkpoint.text
        );
        let provided = store
            .compact(
                Some("Caller supplied goal and next step.".into()),
                Some(repeated.revision),
            )
            .unwrap();
        let active = provided.branch(&provided.active_id).unwrap();
        assert_eq!(active.mode(), "provided_summary");
        assert_eq!(
            active
                .recent
                .iter()
                .map(|r| r.source.message_id.clone())
                .collect::<Vec<_>>(),
            recent_ids
        );
    }

    #[test]
    fn snapshot_budget_counts_json_escaping_and_reserves_the_server_envelope() {
        let temporary = Temporary::new();
        let store = temporary.store("/fixture/escaped");
        let summary = format!("S{}", "\n".repeat(MAX_SUMMARY - 1));
        store.compact(Some(summary), None).unwrap();
        for n in 1..=MAX_RECENT {
            let value = format!("{}\tword {n}", " ".repeat(MAX_RECENT_TEXT + 32));
            store
                .record(conversation(), &message(n, "user", &value))
                .unwrap();
        }
        let document = store.read().unwrap();
        let snapshot = snapshot(&store, &document).unwrap();
        assert!(serde_json::to_vec(&snapshot).unwrap().len() <= MAX_SNAPSHOT);
        assert!(
            serde_json::to_vec(&json!({"client_id":"c".repeat(64),"context":snapshot}))
                .unwrap()
                .len()
                <= 12 * 1024
        );
        assert!(
            snapshot["snapshot"]["summary_truncated"] == true
                || snapshot["snapshot"]["recent_omitted"].as_u64().unwrap() > 0
        );
        let branch = document.branch(&document.active_id).unwrap();
        assert_eq!(branch.checkpoint.as_ref().unwrap().text.len(), MAX_SUMMARY);
        assert!(!branch.recent.is_empty());
    }

    #[test]
    fn branch_count_and_full_workspace_storage_are_bounded() {
        let temporary = Temporary::new();
        let store = temporary.store("/fixture/bounds");
        store.compact(Some("x".repeat(MAX_SUMMARY)), None).unwrap();
        for n in 1..=MAX_RECENT {
            store
                .record(conversation(), &message(n, "user", &"界".repeat(4000)))
                .unwrap();
        }
        for n in 1..MAX_BRANCHES {
            store
                .fork(Some(format!("branch-{n}")), false, None)
                .unwrap();
        }
        let before = fs::read(temporary.state_path("/fixture/bounds")).unwrap();
        assert!(before.len() <= MAX_STATE_BYTES);
        assert_eq!(store.read().unwrap().branches.len(), MAX_BRANCHES);
        assert!(store.fork(None, true, None).is_err());
        assert_eq!(
            fs::read(temporary.state_path("/fixture/bounds")).unwrap(),
            before
        );
    }

    #[test]
    fn workspace_and_schema_mismatch_fail_closed_without_deleting_memory() {
        let temporary = Temporary::new();
        let one = temporary.store("/fixture/one");
        let two = temporary.store("/fixture/two");
        assert_ne!(one.read().unwrap().active_id, two.read().unwrap().active_id);
        one.record(
            conversation(),
            &message(1, "assistant", "Only workspace one"),
        )
        .unwrap();
        assert!(two.read().unwrap().branches[0].recent.is_empty());
        let target = temporary.state_path("/fixture/two");
        let copied = fs::read(temporary.state_path("/fixture/one")).unwrap();
        fs::write(&target, &copied).unwrap();
        assert!(two.read().unwrap_err().contains("different workspace"));
        assert_eq!(fs::read(&target).unwrap(), copied);
        let mut invalid = one.read().unwrap();
        invalid.schema_version = 99;
        let path = temporary.state_path("/fixture/one");
        let bytes = serde_json::to_vec(&invalid).unwrap();
        fs::write(&path, &bytes).unwrap();
        assert!(one.read().unwrap_err().contains("unsupported"));
        assert!(one.compact(None, None).is_err());
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[cfg(unix)]
    #[test]
    fn private_atomic_storage_rejects_symlinks_and_hardlinks() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let temporary = Temporary::new();
        let workspace = "/fixture/private";
        let store = temporary.store(workspace);
        let path = temporary.state_path(workspace);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o777,
            0o700
        );
        let linked = temporary.0.join("linked.json");
        fs::hard_link(&path, &linked).unwrap();
        let original = fs::read(&path).unwrap();
        assert!(store.compact(None, None).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        fs::remove_file(&path).unwrap();
        symlink(&linked, &path).unwrap();
        assert!(store.read().is_err());
        assert!(Store::at(temporary.0.join("memory"), workspace.into()).is_err());
        assert_eq!(fs::read(&linked).unwrap(), original);
    }

    #[test]
    fn pending_and_targeted_user_text_cannot_enter_snapshots_or_extractive_checkpoints() {
        let temporary = Temporary::new();
        let store = temporary.store("/fixture/delivery-boundary");
        let pending = message(1, "user", "PRIVATE_PENDING_REQUIREMENT");
        assert!(
            store
                .record_confirmed(conversation(), &pending, None)
                .is_err()
        );
        assert!(
            store
                .record_confirmed(
                    conversation(),
                    &pending,
                    Some(Delivery {
                        request_id: "request_one".into(),
                        revision: 3,
                        target: "agent_private".into(),
                    })
                )
                .is_err()
        );
        let before = store.read().unwrap();
        assert_eq!(before.revision, 1);
        let compacted = store.compact(None, Some(before.revision)).unwrap();
        assert!(
            !serde_json::to_string(&snapshot(&store, &compacted).unwrap())
                .unwrap()
                .contains("PRIVATE_PENDING_REQUIREMENT")
        );
        store
            .record_confirmed(
                conversation(),
                &pending,
                Some(Delivery {
                    request_id: "request_one".into(),
                    revision: 4,
                    target: "next".into(),
                }),
            )
            .unwrap();
        let delivered = store.read().unwrap();
        let context = snapshot(&store, &delivered).unwrap();
        assert_eq!(context["recent"][0]["text"], "PRIVATE_PENDING_REQUIREMENT");
        assert_eq!(context["recent"][0]["delivery"]["target"], "next");
        let mut invalid = delivered;
        invalid.active_mut().recent[0].delivery = None;
        assert!(invalid.validate("/fixture/delivery-boundary").is_err());
    }

    #[test]
    fn recovery_rejects_conflicting_or_duplicate_checkpoint_source_identities() {
        let temporary = Temporary::new();
        let workspace = "/fixture/source-integrity";
        let store = temporary.store(workspace);
        store
            .record(
                conversation(),
                &message(1, "assistant", "Verified source identity"),
            )
            .unwrap();
        let mut document = store.compact(None, None).unwrap();
        assert!(
            document.validate(workspace).is_ok(),
            "same source may appear in checkpoint and recent"
        );
        document.active_mut().checkpoint.as_mut().unwrap().sources[0].sha256 = "0".repeat(64);
        let bytes = serde_json::to_vec(&document).unwrap();
        let path = temporary.state_path(workspace);
        fs::write(&path, &bytes).unwrap();
        assert!(
            store
                .read()
                .unwrap_err()
                .contains("conflicting context source hashes")
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "a rejected recovery preserves the source file"
        );
        let branch = document.active_mut();
        branch.checkpoint.as_mut().unwrap().sources[0].sha256 =
            branch.recent[0].source.sha256.clone();
        let source = branch.checkpoint.as_ref().unwrap().sources[0].clone();
        branch.checkpoint.as_mut().unwrap().sources.push(source);
        assert!(
            document
                .validate(workspace)
                .unwrap_err()
                .contains("duplicate context checkpoint source")
        );
    }

    #[test]
    fn native_schemas_separate_observation_from_memory_mutations() {
        let catalog = tools();
        assert_eq!(catalog.len(), 6);
        for tool in catalog {
            let name = tool["name"].as_str().unwrap();
            assert_ne!(name, "ssmcp.context.roots");
            assert_eq!(tool["inputSchema"]["additionalProperties"], false);
            let read_only = matches!(
                name,
                "ssmcp.context.status" | "ssmcp.context.read" | "ssmcp.context.list"
            );
            assert_eq!(tool["annotations"]["readOnlyHint"], read_only);
            if !read_only {
                assert_eq!(
                    tool["inputSchema"]["properties"]["expected_revision"]["maximum"],
                    MAX_REVISION
                );
            }
        }
        assert!(parse::<SwitchRequest>(json!({"id":"x","path":"/tmp/escape"})).is_err());
        assert!(parse::<CompactRequest>(json!({"expected_revision":-1})).is_err());
        assert!(parse::<ForkRequest>(json!({"activate":"true"})).is_err());
    }
}
