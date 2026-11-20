//! Private, bounded display handoff. These DTOs contain no native call identity
//! and never enter the outbox, dispatcher, history JSONL or command executor.
use super::*;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    collections::HashSet,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum DisplayKind {
    User,
    Assistant,
    Tool,
    Log,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum History {
    NoSelection,
    Local(String),
    LegacyAttach,
    #[default]
    Global,
}

impl History {
    fn capture(history: &DisplayHistory) -> Self {
        match history {
            DisplayHistory::NoSelection => Self::NoSelection,
            DisplayHistory::Local(id) => Self::Local(id.as_str().to_owned()),
            DisplayHistory::LegacyAttach => Self::LegacyAttach,
            DisplayHistory::Global => Self::Global,
        }
    }

    fn restore(&self) -> DisplayHistory {
        match self {
            Self::NoSelection => DisplayHistory::NoSelection,
            Self::Local(id) => {
                HistoryId::parse(id).map_or(DisplayHistory::Global, DisplayHistory::Local)
            }
            Self::LegacyAttach => DisplayHistory::LegacyAttach,
            Self::Global => DisplayHistory::Global,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    Done,
    Exit(i64),
    Cancelled,
    Timeout,
    Signal(i64),
    Failed,
}
impl Outcome {
    fn capture(status: Status) -> Option<Self> {
        Some(match status {
            Status::Running => return None,
            Status::Done => Self::Done,
            Status::Exit(code) => Self::Exit(code),
            Status::Cancelled => Self::Cancelled,
            Status::Timeout => Self::Timeout,
            Status::Signal(signal) => Self::Signal(signal),
            Status::Failed => Self::Failed,
        })
    }
    fn status(self) -> Status {
        match self {
            Self::Done => Status::Done,
            Self::Exit(code) => Status::Exit(code),
            Self::Cancelled => Status::Cancelled,
            Self::Timeout => Status::Timeout,
            Self::Signal(signal) => Status::Signal(signal),
            Self::Failed => Status::Failed,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    text: String,
    truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    /// Ordinal within this bounded display, never an old or native event ID.
    index: usize,
    kind: DisplayKind,
    #[serde(default)]
    history: History,
    /// Exact JSONL identity for display reconciliation and confirmed recall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_id: Option<String>,
    text: String,
    summary: String,
    source: Option<Source>,
    command: bool,
    diff: bool,
    raw_message: bool,
    #[serde(default)]
    plain_message: bool,
    outcome: Option<Outcome>,
    interrupted: bool,
    boundary: bool,
    #[serde(default)]
    transparent_log: bool,
    language: String,
    tone: String,
    truncated: bool,
    expanded: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    version: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    selected_history_id: Option<String>,
    entries: Vec<Entry>,
    omitted: bool,
}
impl Default for Snapshot {
    fn default() -> Self {
        Self {
            version: 1,
            selected_history_id: None,
            entries: Vec::new(),
            omitted: false,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireSnapshot {
    version: u8,
    #[serde(default)]
    selected_history_id: Option<String>,
    entries: Vec<Entry>,
    omitted: bool,
}

impl<'de> Deserialize<'de> for Snapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A bad optional preview cannot make an otherwise valid draft unreadable.
        let value = Value::deserialize(deserializer)?;
        let Ok(raw) = serde_json::from_value::<WireSnapshot>(value) else {
            return Ok(Self::omitted());
        };
        if raw.version != 1 {
            return Ok(Self::omitted());
        }
        let mut snapshot = Self {
            version: 1,
            selected_history_id: raw.selected_history_id,
            entries: raw.entries,
            omitted: raw.omitted,
        };
        snapshot.bound();
        Ok(snapshot)
    }
}

fn safe_text(text: &str) -> (String, bool) {
    let safe = crate::audit::event(Value::String(unstyled(text)));
    let safe = safe.as_str().unwrap_or_default();
    let mut plain = String::with_capacity(safe.len());
    for ch in safe.chars() {
        match ch {
            '\n' => plain.push('\n'),
            '\t' => plain.push_str("    "),
            ch if ch.is_control() => {}
            ch => plain.push(ch),
        }
    }
    retained_text_with_note(plain.as_bytes(), "\n… reload display shortened …\n")
}

impl Snapshot {
    fn omitted() -> Self {
        Self {
            omitted: true,
            ..Self::default()
        }
    }

    pub(in crate::client::terminal) fn discard(&mut self) {
        *self = Self::omitted();
    }

    pub(in crate::client::terminal) fn selected_history(&self) -> Option<HistoryId> {
        HistoryId::parse(self.selected_history_id.as_deref()?)
    }

    fn bound(&mut self) {
        if self.selected_history().is_none() {
            self.selected_history_id = None;
        }
        if self.entries.len() > MAX_EVENTS {
            self.entries.drain(..self.entries.len() - MAX_EVENTS);
            self.omitted = true;
        }
        let mut indices = HashSet::new();
        self.entries.retain(|entry| {
            let valid = entry.index < MAX_EVENTS && indices.insert(entry.index);
            self.omitted |= !valid;
            valid
        });
        for entry in &mut self.entries {
            let history = entry.history.restore();
            entry.history = History::capture(&history);
            if history.local_id().is_none()
                || !matches!(entry.kind, DisplayKind::User | DisplayKind::Assistant)
            {
                entry.history_id = None;
            }
            entry.history_id = entry
                .history_id
                .take()
                .filter(|id| crate::client::interface::history::valid_message_id(id));
            let (text, shortened) = safe_text(&entry.text);
            entry.text = text;
            entry.truncated |= shortened;
            self.omitted |= shortened;
            let (summary, _) = safe_text(&entry.summary);
            entry.summary = clip_plain(&summary.replace('\n', " "), 512);
            entry.language = language(&entry.language).to_owned();
            entry.tone = tone(&entry.tone).to_owned();
            if let Some(source) = &mut entry.source {
                let (text, shortened) = safe_text(&source.text);
                source.text = text;
                source.truncated |= shortened;
                self.omitted |= shortened;
            }
        }
        // Use serialized size: quotes and newlines also consume handoff bytes.
        let mut bytes = serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len());
        while bytes > MAX_BYTES && !self.entries.is_empty() {
            self.entries.remove(0);
            self.omitted = true;
            bytes = serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len());
        }
    }
}

fn language(value: &str) -> &'static str {
    match value {
        "python" => "python",
        "rust" => "rust",
        "json" => "json",
        "bash" => "bash",
        "shell" => "shell",
        "javascript" => "javascript",
        "typescript" => "typescript",
        "toml" => "toml",
        "yaml" => "yaml",
        "diff" => "diff",
        "markdown" => "markdown",
        _ => "text",
    }
}
fn tone(value: &str) -> &'static str {
    match value {
        "error" => "error",
        "warning" => "warning",
        "success" => "success",
        "accent" => "accent",
        _ => "muted",
    }
}

fn new_display_base() -> u64 {
    static NEXT: OnceLock<AtomicU64> = OnceLock::new();
    // Local display IDs stay exactly representable in JSON consumers. The new
    // process allocates a fresh generation; no prior correlation key is reused.
    NEXT.get_or_init(|| {
        AtomicU64::new(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64
                & ((1u64 << 53) - 1),
        )
    })
    .fetch_add(MAX_EVENTS as u64 + 1, Ordering::Relaxed)
}

impl EventView {
    pub(in crate::client::terminal) fn reload_display(&self) -> Snapshot {
        let entries = self
            .events
            .iter()
            .enumerate()
            .map(|(index, event)| {
                let kind = match event.meta.kind {
                    Kind::Tool => DisplayKind::Tool,
                    Kind::Log => DisplayKind::Log,
                    Kind::Conversation if event.meta.user => DisplayKind::User,
                    Kind::Conversation => DisplayKind::Assistant,
                };
                let mut text = if let Some(read) = &event.meta.read {
                    format!("[READ]\n{}", read.details(&event.text))
                } else {
                    event.text.clone()
                };
                if event.image.is_some() {
                    text.push_str(
                        "\nImage preview is unavailable after reload; retained text only.\n",
                    );
                }
                Entry {
                    index,
                    kind,
                    history: History::capture(&event.meta.history),
                    history_id: event
                        .history_id
                        .clone()
                        .or_else(|| event.meta.message_id.clone()),
                    text,
                    summary: event.chat_heading(),
                    source: event.command_source.as_ref().map(|source| Source {
                        text: source.text.clone(),
                        truncated: source.truncated,
                    }),
                    command: event.meta.command,
                    diff: event.meta.diff,
                    raw_message: event.meta.raw_message,
                    plain_message: event.meta.plain_message,
                    outcome: event.meta.status.and_then(Outcome::capture),
                    interrupted: event.interrupted || event.meta.status == Some(Status::Running),
                    boundary: event.reload_boundary || event.image.is_some(),
                    transparent_log: event.meta.kind == Kind::Log && event.meta.read_bookkeeping,
                    language: event.meta.language.to_owned(),
                    tone: event.tone.to_owned(),
                    truncated: event.truncated,
                    expanded: event.expanded,
                }
            })
            .collect();
        let mut snapshot = Snapshot {
            selected_history_id: self
                .history_selection
                .local_id()
                .map(|id| id.as_str().to_owned()),
            entries,
            ..Snapshot::default()
        };
        snapshot.bound();
        snapshot
    }

    pub(in crate::client::terminal) fn restore_reload_display(
        &mut self,
        mut saved: Snapshot,
    ) -> Option<String> {
        saved.bound();
        if saved.entries.is_empty() {
            return saved.omitted.then(|| "Some tool display history was omitted during reload; your draft and conversation are preserved.".into());
        }
        self.clear();
        // Positions belong to the Chat display; establish its grouping before
        // Canvas restores a header selection on a terminal with no first frame.
        self.scope = EventScope::Conversation;
        self.next_id = new_display_base();
        for entry in saved.entries {
            let kind = match entry.kind {
                DisplayKind::Tool => Kind::Tool,
                DisplayKind::Log => Kind::Log,
                DisplayKind::User | DisplayKind::Assistant => Kind::Conversation,
            };
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let source = entry.source.map(|source| CommandSource {
                text: source.text,
                truncated: source.truncated,
            });
            let meta = Meta {
                kind,
                user: matches!(entry.kind, DisplayKind::User),
                author: if matches!(entry.kind, DisplayKind::Assistant) {
                    entry.summary.clone()
                } else {
                    String::new()
                },
                raw_message: entry.raw_message,
                plain_message: entry.plain_message,
                history: entry.history.restore(),
                history_message: entry.history_id.is_some(),
                read_bookkeeping: kind == Kind::Log && entry.transparent_log,
                command: entry.command,
                diff: entry.diff,
                status: entry.outcome.map(Outcome::status),
                language: language(&entry.language),
                // Native keys, call scope and request boundaries stay empty.
                ..Meta::default()
            };
            self.events.push_back(Event {
                id,
                lines: entry.text.lines().count().max(1),
                text: entry.text,
                summary: entry.summary,
                command_source: source,
                expanded: entry.expanded,
                chat_expanded: false,
                panel: CommandPanel::default(),
                group_panel: CommandPanel::default(),
                group_expanded: false,
                truncated: entry.truncated,
                tone: tone(&entry.tone),
                cache: None,
                meta,
                image: None,
                diagnostic: None,
                history_id: entry.history_id,
                historical: kind == Kind::Tool,
                interrupted: entry.interrupted,
                reload_index: Some(entry.index),
                reload_boundary: entry.boundary,
            });
        }
        self.prune();
        self.selected = self.visible_ids().last().copied();
        (saved.omitted || self.dropped > 0).then(|| "Some tool display history was shortened during reload; your draft and conversation are preserved.".into())
    }

    pub(in crate::client::terminal) fn reload_log_text(&self) -> String {
        let mut text = String::new();
        for event in self
            .events
            .iter()
            .filter(|event| event.reload_index.is_some())
        {
            event.append_reload_log_text(&mut text, &event.text);
        }
        text
    }

    pub(in crate::client::terminal) fn append_reconciled_reload_logs(&self, text: &mut String) {
        for event in self
            .events
            .iter()
            .filter(|event| event.reload_index.is_none() && event.history_id.is_some())
        {
            let (safe, _) = safe_text(&event.text);
            event.append_reload_log_text(text, &safe);
        }
    }
}

impl Event {
    fn append_reload_log_text(&self, text: &mut String, body: &str) {
        if self.meta.raw_message {
            text.push_str(if self.meta.user {
                "[USER] "
            } else {
                "[AGENT] "
            });
            text.push_str(&self.summary);
            text.push('\n');
        }
        text.push_str(body);
        if !body.ends_with('\n') {
            text.push('\n');
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Reading {
    selected: Option<usize>,
    selected_group: Option<usize>,
    expanded: Vec<usize>,
    panels: Vec<(usize, CommandPosition)>,
}

fn reading<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Reading>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let Ok(mut reading) = serde_json::from_value::<Reading>(value) else {
        return Ok(None);
    };
    reading.selected = reading.selected.filter(|index| *index < MAX_EVENTS);
    reading.selected_group = reading.selected_group.filter(|index| *index < MAX_EVENTS);
    reading.expanded.retain(|index| *index < MAX_EVENTS);
    reading.expanded.truncate(MAX_EVENTS);
    reading.panels.retain(|(index, _)| *index < MAX_EVENTS);
    reading.panels.truncate(MAX_EVENTS);
    Ok(Some(reading))
}

/// Old checkpoints still contain just global scroll/follow. New positions use
/// bounded ordinals resolved to the fresh display IDs after rehydration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReloadPosition {
    scroll: usize,
    follow: bool,
    #[serde(default, deserialize_with = "reading")]
    reading: Option<Reading>,
}
impl ReloadPosition {
    pub(in crate::client::terminal) fn discard_display(&mut self) {
        self.reading = None;
    }
    pub(in crate::client::terminal) fn position(&self, view: &EventView) -> EventPosition {
        let mut position = EventPosition {
            scroll: self.scroll,
            follow: self.follow,
            reveal: false,
            ..EventPosition::default()
        };
        if let Some(reading) = &self.reading {
            let id = |index: usize| {
                view.events
                    .iter()
                    .find(|event| event.reload_index == Some(index))
                    .map(|event| event.id)
            };
            position.selected = reading.selected.and_then(id);
            position.selected_group = reading.selected_group.and_then(id);
            position.chat_expanded = reading
                .expanded
                .iter()
                .filter_map(|index| id(*index))
                .collect();
            position.panel_scroll = reading
                .panels
                .iter()
                .filter_map(|(index, panel)| id(*index).map(|id| (id, *panel)))
                .collect();
        }
        position
    }
}
impl EventPosition {
    pub(in crate::client::terminal) fn for_reload(&self, view: &EventView) -> ReloadPosition {
        let index = |id: u64| view.events.iter().position(|event| event.id == id);
        ReloadPosition {
            scroll: self.scroll,
            follow: self.follow,
            reading: Some(Reading {
                selected: self.selected.and_then(index),
                selected_group: self.selected_group.and_then(index),
                expanded: self
                    .chat_expanded
                    .iter()
                    .filter_map(|id| index(*id))
                    .collect(),
                panels: self
                    .panel_scroll
                    .iter()
                    .filter_map(|(id, panel)| index(*id).map(|index| (index, *panel)))
                    .collect(),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
