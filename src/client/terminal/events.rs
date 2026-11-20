//! Bounded local event previews. Expanding reveals retained display text, not a
//! reread of tools or an unbounded replacement for their original output archive.
use super::{super::ui::Ui, images};
use crate::{
    client::interface::history::{HistoryId, HistoryOwner},
    runtime::call_scope::{DisplayHistory, DisplaySource},
};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
mod command_rows;
#[cfg(test)]
mod command_visibility_tests;
mod correlation;
mod diff;
mod read_activity;
mod reload_display;
pub(super) use reload_display::{ReloadPosition, Snapshot as ReloadDisplay};
#[cfg(all(test, unix))]
mod source_tests;
mod tool_activity;
mod tool_summaries;
use correlation::{Kind, Meta, Status};
pub(crate) use diff::DiffMode;

/// Both panes share retained events; this changes only which rows are visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum EventScope {
    Conversation,
    #[default]
    All,
}

impl EventScope {
    fn label(self) -> &'static str {
        match self {
            Self::Conversation => "conversation",
            Self::All => "all",
        }
    }
}

const MAX_EVENTS: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_DETAIL: usize = 128 * 1024;

struct Event {
    id: u64,
    text: String,
    command_source: Option<CommandSource>,
    summary: String,
    lines: usize,
    expanded: bool,
    chat_expanded: bool,
    panel: CommandPanel,
    group_panel: CommandPanel,
    group_expanded: bool,
    truncated: bool,
    tone: &'static str,
    cache: Option<(usize, String, Vec<String>)>,
    meta: Meta,
    image: Option<images::Preview>,
    diagnostic: Option<(u64, u64)>,
    history_id: Option<String>,
    historical: bool,
    interrupted: bool,
    reload_index: Option<usize>,
    reload_boundary: bool,
    tool_group_id: Option<String>,
}

/// Independently bounded display metadata never becomes native output or log text.
struct CommandSource {
    text: String,
    truncated: bool,
}

impl CommandSource {
    fn new(text: String) -> Self {
        let (text, truncated) = retained_text_with_note(
            text.as_bytes(),
            "\n… command preview truncated; use /command JOB_ID for retained argv …\n",
        );
        Self { text, truncated }
    }
}

/// Each command keeps its own viewport. None follows newly arriving output.
#[derive(Default)]
struct CommandPanel {
    scroll: Option<usize>,
    folded_scroll: Option<Option<usize>>,
    rows: usize,
    page: usize,
}

#[derive(Clone, Copy, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct CommandPosition {
    scroll: Option<usize>,
    group_scroll: Option<usize>,
    group_expanded: bool,
    folded_scroll: Option<Option<usize>>,
}

impl CommandPanel {
    fn offset(&self) -> usize {
        self.scroll
            .unwrap_or_else(|| self.rows.saturating_sub(self.page))
            .min(self.rows.saturating_sub(self.page))
    }

    fn scroll_by(&mut self, delta: isize) {
        let end = self.rows.saturating_sub(self.page);
        let offset = self.offset().saturating_add_signed(delta).min(end);
        self.scroll = (offset != end).then_some(offset);
    }
}

impl Event {
    fn set_chat_expanded(&mut self, expanded: bool) -> bool {
        if self.chat_expanded == expanded {
            return false;
        }
        if self.meta.command && !self.meta.diff && self.image.is_none() {
            if expanded {
                // Details start with the command, while folding returns to the
                // exact log position (including following live output).
                self.panel.folded_scroll = Some(self.panel.scroll);
                self.panel.scroll = Some(0);
            } else if let Some(scroll) = self.panel.folded_scroll.take() {
                self.panel.scroll = scroll;
            }
        }
        self.chat_expanded = expanded;
        true
    }

    fn expanded(&self, scope: EventScope) -> bool {
        match scope {
            EventScope::Conversation => self.meta.kind == Kind::Conversation || self.chat_expanded,
            EventScope::All => self.expanded,
        }
    }

    fn chat_heading(&self) -> String {
        if self.meta.kind == Kind::Conversation {
            return if self.meta.user {
                "You".into()
            } else if self.meta.author.is_empty() {
                "Assistant".into()
            } else {
                self.meta.author.clone()
            };
        }
        // Work only on the already-redacted renderer summary. Native command
        // metadata can contain secrets that the human transcript has removed.
        let plain = unstyled(&self.summary);
        let heading = heading(&plain);
        let (tag, mut title) = heading
            .strip_prefix('[')
            .and_then(|text| text.split_once(']'))
            .map_or(("", heading), |(tag, text)| (tag.trim(), text.trim()));
        if self.meta.command && matches!(tag, "RUN" | "OK" | "ERR" | "WARN") {
            for key in &self.meta.keys {
                let mut parts = key.split('\0');
                let (Some(provider), Some("job"), Some(id)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                let source = if provider.is_empty() {
                    None
                } else {
                    title
                        .strip_prefix(provider)
                        .and_then(|text| text.strip_prefix(' '))
                };
                let provider_prefix = source.is_some();
                let Some((shown_id, command)) = source.unwrap_or(title).split_once(' ') else {
                    continue;
                };
                let compact_id = shown_id.strip_prefix('#').is_some_and(|shown| {
                    shown == id
                        || id.rsplit_once('-').is_some_and(|(nonce, suffix)| {
                            nonce.len() == 32
                                && nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
                                && suffix == shown
                                && !shown.is_empty()
                                && shown.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                });
                // Without a provider, strip only the exact known job ID or
                // its display abbreviation, never an arbitrary command #arg.
                if shown_id == id
                    || compact_id
                    || (provider_prefix
                        && shown_id.strip_prefix('#').is_some_and(|id| !id.is_empty()))
                    || shown_id.strip_prefix('…').is_some_and(|tail| {
                        tail.chars().count() == 12 && id.chars().count() > 18 && id.ends_with(tail)
                    })
                {
                    title = command.trim_start();
                    if tag == "RUN" {
                        title = title.strip_suffix(" started").unwrap_or(title);
                    } else if let Some((_, command)) = title.split_once(" · ") {
                        title = command;
                    }
                    break;
                }
            }
        } else if !self.meta.command
            && tag == "REQ"
            && let Some((tool, call)) = title.rsplit_once(" call=")
            && call.ends_with(" started")
        {
            title = tool;
        }
        title.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn chat_title(&self) -> String {
        let heading = self.chat_heading();
        let mut title = match self.meta.status {
            Some(Status::Done | Status::Exit(0)) => format!("{heading} · done"),
            Some(status) => format!("{heading} · {}", status.label()),
            None => heading,
        };
        if self.historical {
            title.push_str(" · history");
            if self.interrupted {
                title.push_str(" · connection ended");
            }
        }
        title
    }

    fn chat_tone(&self, selected: bool) -> &'static str {
        match self.meta.status {
            Some(status) if status.failed() => status.tone(),
            _ if matches!(self.tone, "error" | "warning") => self.tone,
            Some(Status::Running) => "accent",
            _ if selected => "accent",
            _ => "muted",
        }
    }

    /// At most two result lines, drawn from the retained redacted transcript.
    /// The existing render cache keeps scanning out of idle repaint frames.
    fn chat_preview(&self, width: usize) -> Vec<String> {
        let plain = unstyled(&self.text);
        let summary = unstyled(&self.summary);
        let mut first = Vec::new();
        let mut last = VecDeque::new();
        let mut errors = VecDeque::new();
        let mut error_section = false;
        let mut source = false;
        let mut result = false;
        let mut count = 0;
        for line in plain.lines() {
            let mut text = heading(line);
            if self.meta.diff && (text.starts_with("[DIFF]") || text.starts_with("@@ ")) {
                continue;
            }
            if let Some((tag, detail)) =
                text.strip_prefix('[').and_then(|text| text.split_once(']'))
            {
                match tag.trim() {
                    "STDOUT" | "STDERR" | "RES" | "OUT" | "RESULT" => {
                        error_section = tag.trim() == "STDERR";
                        source = false;
                        result = true;
                        continue;
                    }
                    "RUN" | "REQ" | "OK" | "SAVE" | "READ" | "WRITE" | "EDIT" => continue,
                    "ERR" | "WARN" => {
                        error_section = true;
                        source = false;
                        result = true;
                        let detail = detail.trim();
                        if text == heading(&summary)
                            || self
                                .meta
                                .keys
                                .iter()
                                .any(|key| key.split('\0').next() == Some(detail))
                            || (self.meta.command
                                && detail.split_whitespace().any(|word| {
                                    word.starts_with("exit=")
                                        || word.starts_with("signal=")
                                        || matches!(word, "completed" | "cancelled" | "timed_out")
                                }))
                        {
                            continue;
                        }
                        text = detail;
                    }
                    _ => {}
                }
            }
            if text.is_empty() {
                continue;
            }
            if self.meta.command && !result {
                source |= text.starts_with("script · ");
                if source || text.starts_with("cwd ") || text.starts_with("cmd=") {
                    continue;
                }
            }
            result = true;
            count += 1;
            if first.len() < 2 {
                first.push(text);
            }
            if last.len() == 2 {
                last.pop_front();
            }
            last.push_back(text);
            if error_section {
                if errors.len() == 2 {
                    errors.pop_front();
                }
                errors.push_back(text);
            }
        }
        let selected = if self.meta.status.is_some_and(Status::failed) {
            if errors.is_empty() {
                last.into_iter().collect()
            } else {
                errors.into_iter().collect()
            }
        } else {
            first
        };
        let more = self.truncated
            || count > selected.len()
            || selected.iter().any(|line| line.width() > width);
        let mut rows = selected
            .into_iter()
            .map(|line| clip_plain(line, width))
            .collect::<Vec<_>>();
        if !rows.is_empty() {
            rows.push(clip_plain(
                if more {
                    "… Click expand"
                } else {
                    "Click expand"
                },
                width,
            ));
        }
        rows
    }

    /// Keep command output intact while removing the renderer's lifecycle labels.
    /// The original redacted transcript remains available in Logs and search.
    fn command_output(&self) -> String {
        let mut rows = Vec::new();
        let mut result = false;
        let mut script = false;
        for raw in self.text.lines() {
            let plain = unstyled(raw);
            let text = heading(&plain);
            if let Some((tag, detail)) =
                text.strip_prefix('[').and_then(|text| text.split_once(']'))
            {
                match tag.trim() {
                    "STDOUT" | "STDERR" | "RES" | "OUT" | "RESULT" => {
                        result = true;
                        script = false;
                        continue;
                    }
                    "RUN" | "REQ" | "OK" | "SAVE" | "READ" | "WRITE" | "EDIT" => continue,
                    "ERR" | "WARN"
                        if detail.split_whitespace().any(|word| {
                            word.starts_with("exit=")
                                || word.starts_with("signal=")
                                || matches!(word, "completed" | "cancelled" | "timed_out")
                        }) =>
                    {
                        continue;
                    }
                    _ => {}
                }
            }
            if !result {
                script |= text.starts_with("script · ");
                if script || text.is_empty() || text.starts_with("cwd ") || text.starts_with("cmd=")
                {
                    continue;
                }
            }
            result = true;
            rows.push(raw);
        }
        while rows.last().is_some_and(|row| row.trim().is_empty()) {
            rows.pop();
        }
        rows.join("\n")
    }

    fn command_detail_text(&self) -> String {
        let detail = if let Some(source) = &self.command_source {
            let (header, rest) = self.text.split_once('\n').unwrap_or((&self.text, ""));
            format!("{header}\n{}{rest}", source.text)
        } else {
            self.text.trim_end().to_owned()
        };
        if self.historical {
            let notice = if self.interrupted {
                "connection ended · read-only display"
            } else {
                "read-only display"
            };
            format!("[HISTORY] {notice}\n{detail}")
        } else {
            detail
        }
    }

    fn append(&mut self, text: String, truncated: bool) {
        if !text.is_empty() {
            self.text.push_str(&text);
            let (retained, trimmed) = retained_text(self.text.as_bytes());
            self.text = retained;
            self.truncated |= trimmed;
            self.lines = unstyled(&self.text).lines().count().max(1);
        }
        self.truncated |= truncated;
    }

    fn retained_bytes(&self) -> usize {
        self.text.len()
            + self.summary.len()
            + self.meta.bytes()
            + self
                .command_source
                .as_ref()
                .map_or(0, |source| source.text.len())
            + self.history_id.as_ref().map_or(0, String::len)
            + self.tool_group_id.as_ref().map_or(0, String::len)
    }

    fn owned_by(&self, selection: &DisplayHistory) -> bool {
        self.meta.history != DisplayHistory::Global && &self.meta.history == selection
    }

    fn matches(
        &self,
        query: &str,
        level: u8,
        scope: EventScope,
        selection: &DisplayHistory,
    ) -> bool {
        (scope == EventScope::All || (self.meta.kind != Kind::Log && self.owned_by(selection)))
            && (level != 1 || self.meta.command || self.summary.starts_with("[RUN"))
            && (level != 2 || self.meta.status.is_some_and(Status::failed) || self.tone == "error")
            && (query.is_empty()
                || self.summary.to_lowercase().contains(query)
                || self.text.to_lowercase().contains(query)
                || self
                    .meta
                    .read
                    .as_ref()
                    .is_some_and(|read| read.path.to_lowercase().contains(query)))
    }

    fn summary_with_status(&self) -> String {
        match self.meta.status {
            Some(status) => format!("{} [{}]", self.summary, status.label()),
            None => self.summary.clone(),
        }
    }
}

/// A canvas column keeps only its reading position, never another event store.
#[derive(Clone, Debug)]
pub(super) struct EventPosition {
    selected: Option<u64>,
    selected_group: Option<u64>,
    scroll: usize,
    follow: bool,
    reveal: bool,
    new_image: Option<u64>,
    chat_expanded: Vec<u64>,
    read_expanded: Vec<u64>,
    panel_scroll: Vec<(u64, CommandPosition)>,
}

impl Default for EventPosition {
    fn default() -> Self {
        Self {
            selected: None,
            selected_group: None,
            scroll: 0,
            follow: true,
            reveal: false,
            new_image: None,
            chat_expanded: Vec::new(),
            read_expanded: Vec::new(),
            panel_scroll: Vec::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct EventView {
    events: VecDeque<Event>,
    bytes: usize,
    next_id: u64,
    selected: Option<u64>,
    selected_group: Option<u64>,
    scroll: usize,
    follow: bool,
    reveal: bool,
    dropped: usize,
    hit_rows: Vec<Option<u64>>,
    panel_hit_rows: Vec<Option<u64>>,
    group_hit_rows: Vec<Option<u64>>,
    group_header_hit_rows: Vec<Option<u64>>,
    total_rows: usize,
    page_rows: usize,
    image_cells: Option<images::CellSize>,
    placements: Vec<images::Placement>,
    new_image: Option<u64>,
    query: String,
    level: u8,
    scope: EventScope,
    history_selection: DisplayHistory,
    summary_groups: VecDeque<tool_summaries::Group>,
    summary_calls: VecDeque<tool_summaries::Call>,
    summary_observers: VecDeque<(u64, u64)>,
    active_summary_group: Option<String>,
    reload_history_preview: bool,
    diff_mode: DiffMode,
    read_boundary: read_activity::Boundary,
    read_expanded: Vec<u64>,
}

impl EventView {
    pub(crate) fn new() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    fn read_groups(&self) -> Vec<read_activity::Group> {
        if self.scope != EventScope::Conversation || !self.query.is_empty() || self.level != 0 {
            return Vec::new();
        }
        let mut groups = read_activity::groups(&self.events, |event| {
            event.owned_by(&self.history_selection)
        });
        for group in &mut groups {
            group.expanded = self.read_expanded.contains(&group.head);
        }
        groups
    }

    fn tool_groups(&self) -> Vec<tool_activity::Group> {
        if self.scope != EventScope::Conversation || !self.query.is_empty() || self.level != 0 {
            return Vec::new();
        }
        let mut groups = tool_activity::groups(&self.events, |event| {
            event.owned_by(&self.history_selection)
        });
        for group in &mut groups {
            if let Some(id) = &self.events[group.start].tool_group_id {
                group.info = self
                    .summary_groups
                    .iter()
                    .find(|saved| &saved.display.group_id == id)
                    .map(|saved| saved.display.clone());
            }
        }
        // A single call keeps its existing card until it has a summary.
        groups.retain(|group| {
            group.members.len() > 1
                || group
                    .info
                    .as_ref()
                    .is_some_and(|info| info.summary.is_some())
        });
        groups
    }

    fn tool_group_for(&self, id: u64) -> Option<tool_activity::Group> {
        let index = self.events.iter().position(|event| event.id == id)?;
        self.tool_groups()
            .into_iter()
            .find(|group| group.contains(index))
    }

    fn selection_targets(&self) -> Vec<(u64, bool)> {
        let groups = self.tool_groups();
        let reads = self.read_groups();
        let mut targets = Vec::new();
        for (index, event) in self.events.iter().enumerate() {
            if !event.matches(&self.query, self.level, self.scope, &self.history_selection) {
                continue;
            }
            if let Some(group) = groups.iter().find(|group| group.contains(index)) {
                if index == group.start {
                    targets.push((group.head, true));
                }
                if group.expanded {
                    targets.push((event.id, false));
                }
            } else if reads
                .iter()
                .find(|group| group.contains(index))
                .is_none_or(|group| group.expanded || index == group.start)
            {
                targets.push((event.id, false));
            }
        }
        targets
    }

    fn visible_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for (id, _) in self.selection_targets() {
            if ids.last() != Some(&id) {
                ids.push(id);
            }
        }
        ids
    }

    fn preserve_tool_selection(&mut self) {
        let Some(group) = self.selected.and_then(|id| self.tool_group_for(id)) else {
            self.selected_group = None;
            return;
        };
        if !group.expanded {
            if !self.follow && self.selected_group.is_none() {
                // A new adjacent call must not swallow an item already being read.
                self.events[group.start].group_expanded = true;
                self.reveal = true;
            } else {
                self.selected = Some(group.head);
                self.selected_group = Some(group.head);
            }
        }
    }

    fn set_tool_expanded(&mut self, group: &tool_activity::Group, expanded: bool) -> bool {
        if group.expanded == expanded {
            return false;
        }
        let head = &mut self.events[group.start];
        head.group_expanded = expanded;
        if expanded && head.group_panel.rows == 0 {
            head.group_panel.scroll = Some(0);
        }
        self.selected = Some(group.head);
        self.selected_group = Some(group.head);
        self.follow = false;
        self.reveal = true;
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        true
    }

    fn selected_tool_header(&self) -> Option<tool_activity::Group> {
        let group = self.selected.and_then(|id| self.tool_group_for(id))?;
        (self.selected_group == Some(group.head) || !group.expanded).then_some(group)
    }

    fn read_group_for(&self, id: u64) -> Option<read_activity::Group> {
        if self.tool_group_for(id).is_some() {
            return None;
        }
        let index = self.events.iter().position(|event| event.id == id)?;
        self.read_groups()
            .into_iter()
            .find(|group| group.contains(index))
    }

    fn preserve_read_selection(&mut self) {
        self.preserve_tool_selection();
        if !self.follow
            && let Some(group) = self.selected.and_then(|id| self.read_group_for(id))
            && !group.expanded
            && self.selected != Some(group.head)
        {
            // A newly formed run must not swallow the item the user selected.
            self.read_expanded.push(group.head);
            self.reveal = true;
        }
    }

    fn set_read_expanded(&mut self, group: &read_activity::Group, expanded: bool) -> bool {
        if group.expanded == expanded {
            return false;
        }
        if expanded {
            self.read_expanded.push(group.head);
        } else {
            self.read_expanded.retain(|id| *id != group.head);
            self.selected = Some(group.head);
        }
        self.follow = false;
        self.reveal = true;
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.placements.clear();
        true
    }

    pub(crate) fn diff_mode(&self) -> DiffMode {
        self.diff_mode
    }

    pub(crate) fn set_diff_mode(&mut self, mode: DiffMode) {
        if self.diff_mode == mode {
            return;
        }
        self.diff_mode = mode;
        for event in &mut self.events {
            if event.meta.diff {
                event.cache = None;
            }
        }
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.placements.clear();
        self.reveal = !self.follow;
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.events.iter().any(|event| {
            event.matches(&self.query, self.level, self.scope, &self.history_selection)
        })
    }

    pub(crate) fn set_scope(&mut self, scope: EventScope) {
        if self.scope == scope {
            return;
        }
        self.scope = scope;
        self.refresh_visibility();
    }

    pub(super) fn set_history_owner(&mut self, owner: Option<HistoryId>) {
        if owner.is_none() && self.reload_history_preview {
            return;
        }
        self.set_history_selection(
            owner.map_or(DisplayHistory::NoSelection, DisplayHistory::Local),
        );
    }

    pub(super) fn set_history_selection(&mut self, selection: DisplayHistory) {
        // The first legacy page may arrive after the frontend selected a saved history.
        if selection == DisplayHistory::LegacyAttach
            && self.history_selection.local_id().is_some()
            && !self.reload_history_preview
        {
            return;
        }
        self.reload_history_preview = false;
        if self.history_selection != selection {
            self.history_selection = selection;
            self.read_boundary.interrupt();
            self.refresh_visibility();
        }
    }

    pub(super) fn set_reload_history_preview(&mut self, history: HistoryId) {
        self.set_history_selection(DisplayHistory::Local(history));
        self.reload_history_preview = true;
    }

    fn local_display_history(&self) -> DisplayHistory {
        if self.reload_history_preview {
            // A restored preview can filter old cards, but cannot route new events.
            return DisplayHistory::Global;
        }
        match &self.history_selection {
            DisplayHistory::Local(owner) => DisplayHistory::Local(owner.clone()),
            _ => DisplayHistory::NoSelection,
        }
    }

    /// Replace saved prose while retaining native activity and its correlation.
    pub(super) fn rehydrate_history(
        &mut self,
        owner: HistoryOwner,
        messages: &[crate::client::interface::history::Message],
    ) {
        let selection = DisplayHistory::Local(owner.display_id());
        self.set_history_selection(selection.clone());
        self.events
            .retain(|event| !(event.meta.history_message && event.owned_by(&selection)));
        self.prune();
        self.refresh_visibility();
        for message in messages {
            self.ingest_identified_message(&message.id, &message.role, &message.content, b"");
        }
    }

    pub(super) fn reconcile_reload_history(
        &mut self,
        history: &crate::client::interface::history::Conversation,
    ) {
        let selection = self.history_selection.clone();
        if selection.local_id().map(HistoryId::as_str) != Some(history.metadata.id.as_str()) {
            return;
        }
        let saved_ids: HashSet<_> = history
            .messages
            .iter()
            .map(|message| message.id.as_str())
            .collect();
        let mut displayed_ids = HashSet::new();
        self.events.retain(|event| {
            if !event.owned_by(&selection) {
                return true;
            }
            event.history_id.as_ref().is_none_or(|id| {
                saved_ids.contains(id.as_str()) && displayed_ids.insert(id.clone())
            })
        });
        self.prune();
        for message in &history.messages {
            if !displayed_ids.contains(&message.id) {
                self.ingest_identified_message(&message.id, &message.role, &message.content, b"");
            }
        }
        self.refresh_visibility();
    }

    fn refresh_visibility(&mut self) {
        self.new_image = None;
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.placements.clear();
        self.total_rows = 0;
        self.page_rows = 0;
        self.scroll = 0;
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if !self.selected.is_some_and(|id| visible.contains(&id)) {
            self.selected = if self.follow {
                visible.last()
            } else {
                visible.first()
            }
            .copied();
        }
        self.reveal = !self.follow;
    }

    /// Only explicit local conversation producers may use this entry point.
    /// Plain log text, including a literal "[AGENT]", stays in the log pane.
    #[cfg(test)]
    pub(crate) fn ingest_conversation(&mut self, bytes: &[u8]) {
        self.read_boundary.interrupt();
        let text = String::from_utf8_lossy(bytes);
        let user = text
            .lines()
            .map(unstyled)
            .find(|line| !line.trim().is_empty())
            .is_some_and(|line| heading(&line).starts_with("[USER]"));
        self.ingest_inner(
            bytes,
            Meta {
                kind: Kind::Conversation,
                user,
                ..Meta::default()
            },
        );
    }

    pub(super) fn position(&self) -> EventPosition {
        EventPosition {
            selected: self.selected,
            selected_group: self.selected_group,
            scroll: self.scroll,
            follow: self.follow,
            reveal: self.reveal,
            new_image: self.new_image,
            read_expanded: self.read_expanded.clone(),
            panel_scroll: self
                .events
                .iter()
                .map(|event| {
                    (
                        event.id,
                        CommandPosition {
                            scroll: event.panel.scroll,
                            group_scroll: event.group_panel.scroll,
                            group_expanded: event.group_expanded,
                            folded_scroll: event.panel.folded_scroll,
                        },
                    )
                })
                .collect(),
            chat_expanded: self
                .events
                .iter()
                .filter(|event| event.chat_expanded)
                .map(|event| event.id)
                .collect(),
        }
    }

    pub(super) fn restore_position(&mut self, position: &EventPosition) {
        self.selected = position.selected;
        self.selected_group = position.selected_group;
        self.scroll = position.scroll;
        self.follow = position.follow;
        self.reveal = position.reveal;
        for event in &mut self.events {
            event.chat_expanded = position.chat_expanded.contains(&event.id);
            let panel = position
                .panel_scroll
                .iter()
                .find(|(id, _)| *id == event.id)
                .map(|(_, panel)| *panel)
                .unwrap_or_default();
            event.panel.scroll = panel.scroll;
            event.group_panel.scroll = panel.group_scroll;
            event.group_expanded = panel.group_expanded;
            event.panel.folded_scroll = panel.folded_scroll;
        }
        self.read_expanded = position
            .read_expanded
            .iter()
            .copied()
            .filter(|id| self.events.iter().any(|event| event.id == *id))
            .collect();
        // The deferred first-image anchor belongs to this column's reading
        // position. Switching columns is not an explicit request for latest.
        self.new_image = position.new_image.filter(|id| {
            self.follow
                && self.events.iter().any(|event| {
                    event.id == *id
                        && event.matches(
                            &self.query,
                            self.level,
                            self.scope,
                            &self.history_selection,
                        )
                        && event
                            .image
                            .as_ref()
                            .is_some_and(|image| image.native.is_some())
                })
        });
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.placements.clear();
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if !self.selected.is_some_and(|id| visible.contains(&id)) {
            self.selected = if self.follow {
                visible.last()
            } else {
                visible.first()
            }
            .copied();
            self.reveal = !self.follow;
        }
    }

    pub(crate) fn ingest(&mut self, bytes: &[u8]) {
        self.read_boundary.interrupt();
        self.ingest_inner(
            bytes,
            Meta {
                history: self.local_display_history(),
                ..Meta::default()
            },
        );
    }

    /// Keep one bounded warning card per renderer diagnostic. Repeated updates
    /// preserve its position, expansion and the reader's current viewport.
    pub(crate) fn ingest_diagnostic(
        &mut self,
        update: &super::super::render::DiagnosticUpdate,
    ) -> bool {
        self.read_boundary.interrupt();
        let (text, truncated) = retained_text(update.text().as_bytes());
        let inserted = if let Some(event) = self
            .events
            .iter_mut()
            .find(|event| event.diagnostic.is_some_and(|(id, _)| id == update.id))
        {
            event.text = text;
            event.summary = super::super::panel::fit(&update.heading(), 200);
            event.lines = unstyled(&event.text).lines().count().max(1);
            event.truncated = truncated;
            event.cache = None;
            event.diagnostic = Some((update.id, update.occurrences));
            event.meta.kind = Kind::Log;
            false
        } else {
            self.ingest(text.as_bytes());
            if let Some(event) = self.events.back_mut() {
                event.summary = super::super::panel::fit(&update.heading(), 200);
                event.expanded = false;
                event.truncated |= truncated;
                event.tone = "warning";
                event.diagnostic = Some((update.id, update.occurrences));
                event.meta.kind = Kind::Log;
            }
            true
        };
        self.prune();
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if !self.selected.is_some_and(|id| visible.contains(&id)) {
            self.selected = visible.first().copied();
            self.reveal = true;
        }
        inserted
    }

    /// Attach an internal JSONL identity; display text and external event fields
    /// never grant the ability to retract a retained conversation message.
    pub(crate) fn ingest_identified_message(
        &mut self,
        id: &str,
        role: &str,
        text: &str,
        bytes: &[u8],
    ) {
        let next = self.next_id;
        self.ingest_event(
            "message",
            &json!({"type":"ssmcp.message", "role":role, "text":text,"message_id":id}),
            bytes,
        );
        if let Some(event) = self.events.back_mut().filter(|event| event.id == next) {
            event.history_id = Some(id.to_owned());
            self.prune();
        }
    }

    pub(crate) fn recall_message(&mut self, id: &str) -> bool {
        let before = self.events.len();
        self.events.retain(|event| {
            !(event.meta.user
                && event.meta.kind == Kind::Conversation
                && event.history_id.as_deref() == Some(id))
        });
        if self.events.len() == before {
            return false;
        }
        self.prune();
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if !self
            .selected
            .is_some_and(|selected| visible.contains(&selected))
        {
            self.selected = if self.follow {
                visible.last()
            } else {
                visible.first()
            }
            .copied();
            self.reveal = true;
        }
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        true
    }

    /// Correlate only registered event identities. Text is never an identity.
    pub(crate) fn ingest_event(&mut self, label: &str, event: &Value, bytes: &[u8]) {
        let mut meta = Meta::from_event(label, event);
        meta.history = self.local_display_history();
        self.ingest_event_with_meta(label, event, bytes, meta);
    }

    pub(crate) fn ingest_scoped_event(
        &mut self,
        label: &str,
        event: &Value,
        bytes: &[u8],
        source: DisplaySource,
    ) {
        let mut meta = Meta::from_event(label, event);
        meta.set_source(source);
        self.ingest_event_with_meta(label, event, bytes, meta);
    }

    fn ingest_event_with_meta(&mut self, label: &str, event: &Value, bytes: &[u8], mut meta: Meta) {
        // Read runs retain their request barriers even when orchestration is
        // transparent in the outer tool window.
        let read_bookkeeping = meta.read_bookkeeping;
        // Code-mode audit records describe the envelope, not another native call.
        // Keep them in Logs without adding to or splitting the tool window.
        if matches!(
            event["type"].as_str(),
            Some("ssmcp.code_mode" | "ssmcp.controller.started" | "ssmcp.controller.finished")
        ) {
            meta.kind = Kind::Log;
            meta.read_bookkeeping = true;
        }
        if event["type"] == "ssmcp.message" && matches!(label, "message" | "agent" | "listener") {
            meta.history_message = true;
            meta.message_id = event["message_id"].as_str().map(str::to_owned);
            if let Some(id) = &meta.message_id
                && self.events.iter().any(|previous| {
                    previous.owned_by(&meta.history)
                        && (previous.meta.message_id.as_ref() == Some(id)
                            || previous.history_id.as_ref() == Some(id))
                })
            {
                // A committed message can be read by /resume before its live
                // projection reaches the terminal. The raw log still gets it.
                return;
            }
        }
        let read = if meta.command
            || meta.diff
            || meta
                .status
                .is_some_and(|status| status.failed() || status == Status::Running)
        {
            None
        } else {
            meta.read.as_mut()
        };
        self.read_boundary.observe(event, read, read_bookkeeping);
        // Balance controller boundaries before hiding observer diagnostics.
        if self.summary_observer_event(&meta) {
            return;
        }
        if meta.kind == Kind::Conversation
            && event["presentation"].is_null()
            && let Some(text) = event["text"].as_str()
        {
            // Render ordinary prose from its source at the pane's current width.
            // Structured presentations keep their already-rendered cards and tables;
            // their text field is the plain fallback used for copying and history.
            meta.raw_message = true;
            // Redact only the display copy; the native message stays intact.
            let display = crate::audit::event(Value::String(text.to_owned()));
            self.ingest_inner(display.as_str().unwrap_or_default().as_bytes(), meta);
        } else if meta.command
            && let Some(source) = crate::client::render::interactive_command_source(event)
        {
            // Source is display metadata, not command output. Keep it separate
            // so transcript bytes, summary parsing and lifecycle correlation
            // retain their existing meaning.
            self.ingest_inner_with_source(bytes, meta, Some(CommandSource::new(source)));
        } else {
            self.ingest_inner(bytes, meta);
        }
    }

    fn ingest_inner(&mut self, bytes: &[u8], meta: Meta) {
        self.ingest_inner_with_source(bytes, meta, None);
    }

    fn ingest_inner_with_source(
        &mut self,
        bytes: &[u8],
        mut meta: Meta,
        command_source: Option<CommandSource>,
    ) {
        if meta.skip {
            return;
        }
        let mut matches = self
            .summary_event_for(&meta)
            .into_iter()
            .collect::<Vec<_>>();
        for key in &meta.keys {
            if let Some((index, _)) = self.events.iter().enumerate().rev().find(|(_, event)| {
                event.meta.same_source(&meta)
                    && event.meta.keys.contains(key)
                    && (!meta.started
                        || event
                            .meta
                            .status
                            .is_none_or(|status| status == Status::Running))
            }) && !matches.contains(&index)
            {
                matches.push(index);
            }
        }
        matches.sort_unstable();
        let (text, truncated) = retained_text(bytes);
        let plain = unstyled(&text);
        let first = plain.lines().map(heading).find(|line| !line.is_empty());
        if first.is_none() && matches.is_empty() {
            return; // Empty stream chunks must not become separate UI cards.
        }
        if meta.kind == Kind::Conversation
            || (meta.kind == Kind::Log && !meta.read_bookkeeping && matches.is_empty())
        {
            self.active_summary_group = None;
        }
        let first = first.unwrap_or("");
        let conversation = meta.kind == Kind::Conversation
            || [
                "[USER]",
                "[AGENT]",
                "[UPDATE]",
                "[ANSWER]",
                "[IMAGE]",
                "[CONTEXT]",
            ]
            .iter()
            .any(|prefix| first.starts_with(prefix));
        let error = plain
            .lines()
            .map(heading)
            .find(|line| line.starts_with("[ERR"));
        let warning = plain
            .lines()
            .map(heading)
            .find(|line| line.starts_with("[WARN") || line.starts_with("[STDERR"));
        let attention = error.or(warning);
        meta.diff |= first.starts_with("[DIFF]") || first.starts_with("diff --git ");
        if meta.language.is_empty() {
            meta.language = first
                .split_whitespace()
                .find_map(|part| {
                    let language = correlation::language_for_path(part);
                    (!language.is_empty()).then_some(language)
                })
                .unwrap_or("");
        }
        let tone = if error.is_some() {
            "error"
        } else if attention.is_some() {
            "warning"
        } else if first.starts_with("[OK") {
            "success"
        } else if first.starts_with("[READ") || first.starts_with("[WRITE") {
            "blue"
        } else if first.starts_with("[REQ") || first.starts_with("[RES") {
            "purple"
        } else if conversation || first.starts_with("[RUN") {
            "accent"
        } else {
            "foreground"
        };
        // Ordinary short log lines keep their original styled body visible.
        // Compact defaults apply to tool events, whose heading carries meaning.
        let short_log = meta.keys.is_empty()
            && !meta.command
            && !first.starts_with('[')
            && plain.lines().count() <= 6
            && text.len() <= 640;
        let expanded = conversation
            || short_log
            || attention.is_some()
            || meta.diff
            || meta.status.is_some_and(Status::failed);
        let follow = self.follow || self.events.is_empty();
        if let Some(&index) = matches.first() {
            // A response can join a request row and its native job row. Keep
            // their earliest position and both retained bodies, once.
            for &other in matches.iter().skip(1).rev() {
                if let Some(previous) = self.events.remove(other) {
                    if self.selected == Some(previous.id) {
                        self.selected = Some(self.events[index].id);
                    }
                    if previous.summary.starts_with("[RUN")
                        && !self.events[index].summary.starts_with("[RUN")
                    {
                        self.events[index].summary = previous.summary;
                    }
                    self.events[index].expanded |= previous.expanded;
                    self.events[index].chat_expanded |= previous.chat_expanded;
                    if self.events[index].panel.scroll.is_none() {
                        self.events[index].panel.scroll = previous.panel.scroll;
                    }
                    if self.events[index].command_source.is_none() {
                        self.events[index].command_source = previous.command_source;
                    }
                    self.events[index].append(previous.text, previous.truncated);
                    self.events[index].meta.merge(previous.meta);
                }
            }
            let event = &mut self.events[index];
            // Replace the dispatch placeholder with the first native display
            // body; later stdout/lifecycle updates append to the same logical row.
            if event.tool_group_id.is_some()
                && event.meta.keys.is_empty()
                && event.text.starts_with("[RUN] ssmcp.")
                && !plain.trim().is_empty()
            {
                event.text.clear();
                event.lines = 0;
                event.summary = event_summary(&plain, attention.unwrap_or(first));
                event.meta.read = meta.read.clone();
                event.tone = tone;
            }
            event.append(text, truncated);
            if let Some(source) = command_source {
                event.command_source = Some(source);
            }
            event.meta.merge(meta);
            event.expanded |= expanded;
            event.cache = None;
        } else {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let summary = if meta.raw_message {
                if meta.user { "[USER]" } else { "[AGENT]" }.to_owned()
            } else {
                event_summary(
                    &plain,
                    if conversation {
                        first
                    } else {
                        attention.unwrap_or(first)
                    },
                )
            };
            let lines = plain.lines().count().max(1);
            self.events.push_back(Event {
                id,
                text,
                command_source,
                summary,
                lines,
                expanded,
                chat_expanded: false,
                panel: CommandPanel::default(),
                group_panel: CommandPanel::default(),
                group_expanded: false,
                truncated,
                tone,
                meta,
                cache: None,
                image: None,
                diagnostic: None,
                history_id: None,
                historical: false,
                interrupted: false,
                reload_index: None,
                reload_boundary: false,
                tool_group_id: None,
            });
        }
        self.prune();
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if follow {
            self.selected = visible.last().copied();
            self.follow = true;
        } else if !self.selected.is_some_and(|id| visible.contains(&id)) {
            self.selected = visible.first().copied();
            self.reveal = true;
        }
        // Updating a correlated row does not move a reader away from their viewport.
    }

    fn prune(&mut self) {
        self.prune_summary_groups();
        self.bytes = self.events.iter().map(Event::retained_bytes).sum::<usize>()
            + self
                .summary_groups
                .iter()
                .map(tool_summaries::Group::retained_bytes)
                .sum::<usize>();
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            let Some(event) = self.events.pop_front() else {
                break;
            };
            if let Some(group_id) = &event.tool_group_id
                && let Some(next) = self
                    .events
                    .iter_mut()
                    .find(|next| next.tool_group_id.as_ref() == Some(group_id))
            {
                next.group_expanded = event.group_expanded;
                next.group_panel = event.group_panel;
                if self.selected_group == Some(event.id) {
                    self.selected_group = Some(next.id);
                    self.selected = Some(next.id);
                }
            }
            self.dropped = self.dropped.saturating_add(1);
            self.prune_summary_groups();
            self.bytes = self.events.iter().map(Event::retained_bytes).sum::<usize>()
                + self
                    .summary_groups
                    .iter()
                    .map(tool_summaries::Group::retained_bytes)
                    .sum::<usize>();
        }
        let events = &self.events;
        self.read_expanded
            .retain(|id| events.iter().any(|event| event.id == *id));
        self.prune_summary_groups();
    }

    pub(crate) fn filter(&mut self, query: &str, level: u8) {
        let query = query.to_lowercase();
        if self.query == query && self.level == level {
            return;
        }
        self.query = query;
        self.level = level;
        self.new_image = None;
        self.preserve_read_selection();
        let visible = self.visible_ids();
        if !self.selected.is_some_and(|id| visible.contains(&id)) {
            self.selected = visible.first().copied();
        }
        self.reveal = true;
    }

    pub(crate) fn rows_below(&self) -> usize {
        self.total_rows
            .saturating_sub(self.page_rows)
            .saturating_sub(self.scroll)
    }

    pub(crate) fn is_following(&self) -> bool {
        self.follow
    }

    pub(super) fn ingest_image(
        &mut self,
        bytes: &[u8],
        preview: images::Preview,
        source: Option<DisplaySource>,
    ) {
        self.read_boundary.interrupt();
        let mut meta = Meta {
            history: self.local_display_history(),
            ..Meta::default()
        };
        if let Some(source) = source {
            meta.set_source(source);
        }
        // The placeholder and image are one update. A skipped insertion must
        // never attach the preview to a previously selected or retained card.
        let id = self.next_id;
        self.ingest_inner(bytes, meta);
        if self.events.back().is_some_and(|event| event.id == id) {
            self.attach_image(preview);
        }
    }

    /// Attach only a decoded image from the native result path. Text containing
    /// "[IMAGE]" cannot manufacture a protocol placement.
    pub(crate) fn attach_image(&mut self, preview: images::Preview) {
        self.active_summary_group = None;
        if let Some(event) = self.events.back_mut() {
            event.image = Some(preview);
            event.meta.kind = Kind::Tool;
            event.chat_expanded = false;
            if self.follow
                && event.matches(&self.query, self.level, self.scope, &self.history_selection)
            {
                self.selected = Some(event.id);
                if self.scope != EventScope::Conversation {
                    self.new_image.get_or_insert(event.id);
                }
            }
        }
        let mut bytes = self.image_bytes();
        for event in &mut self.events {
            if bytes <= images::CACHE_BYTES {
                break;
            }
            if let Some(image) = event.image.as_mut()
                && let Some(native) = image.native.take()
            {
                bytes -= native.bytes();
            }
        }
    }

    fn image_bytes(&self) -> usize {
        self.events
            .iter()
            .filter_map(|event| event.image.as_ref()?.native.as_ref())
            .map(|native| native.bytes())
            .sum()
    }

    /// A terminal can reject one image while still supporting other images.
    /// Keep that event's cell preview and release only its native payload.
    pub(crate) fn reject_image(&mut self, id: u32) {
        for event in &mut self.events {
            if let Some(image) = event.image.as_mut()
                && image
                    .native
                    .as_ref()
                    .is_some_and(|native| native.id() == id)
            {
                image.native = None;
                if self.new_image == Some(event.id) {
                    self.new_image = None;
                }
                if self.selected == Some(event.id) && !self.follow {
                    self.reveal = true;
                }
            }
        }
    }

    pub(crate) fn image_cells(&mut self, cells: Option<images::CellSize>) {
        self.image_cells = cells;
    }

    pub(crate) fn image_placements(&self, top: u16) -> Vec<images::Placement> {
        self.placements
            .iter()
            .cloned()
            .map(|mut placement| {
                placement.offset_row(top);
                placement
            })
            .collect()
    }

    /// Negative selects an earlier event; positive selects a later event.
    pub(crate) fn select(&mut self, delta: isize) {
        self.new_image = None;
        let visible = self.selection_targets();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|(id, group)| {
                Some(*id) == self.selected && *group == self.selected_group.is_some()
            })
            .unwrap_or(visible.len() - 1);
        let next = current.saturating_add_signed(delta).min(visible.len() - 1);
        let (id, group) = visible[next];
        self.selected = Some(id);
        self.selected_group = group.then_some(id);
        self.follow = false;
        self.reveal = true;
    }

    /// Scroll within expanded events without changing the selected event.
    pub(crate) fn scroll_by(&mut self, delta: isize) {
        self.new_image = None;
        self.follow = false;
        self.reveal = false;
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.total_rows.saturating_sub(self.page_rows));
        self.follow = self.scroll == self.total_rows.saturating_sub(self.page_rows);
    }

    /// Expanded details own their inner viewport; folded items and group chrome
    /// own the shared tool window. An edge never scrolls the conversation below.
    pub(crate) fn scroll_panel_at(&mut self, row: u16, delta: isize) -> bool {
        let row = usize::from(row);
        if let Some(id) = self.panel_hit_rows.get(row).copied().flatten() {
            return self.scroll_panel(id, delta);
        }
        self.group_hit_rows
            .get(row)
            .copied()
            .flatten()
            .is_some_and(|id| self.scroll_tool_group(id, delta))
    }

    pub(crate) fn scroll_selected_panel(&mut self, delta: isize) -> bool {
        if let Some(id) = self.selected_group {
            return self.scroll_tool_group(id, delta);
        }
        let Some(id) = self.selected else {
            return false;
        };
        if self.tool_group_for(id).is_some() {
            if self
                .events
                .iter()
                .any(|event| event.id == id && event.chat_expanded)
                && self.scroll_panel(id, delta)
            {
                return true;
            }
            return self.scroll_tool_group(id, delta);
        }
        self.scroll_panel(id, delta)
    }

    fn scroll_tool_group(&mut self, id: u64, delta: isize) -> bool {
        let Some(group) = self.tool_group_for(id) else {
            return false;
        };
        if !group.expanded {
            return false;
        }
        self.events[group.start].group_panel.scroll_by(delta);
        self.new_image = None;
        self.follow = false;
        self.reveal = false;
        true
    }

    fn scroll_panel(&mut self, id: u64, delta: isize) -> bool {
        if self.scope != EventScope::Conversation || !self.query.is_empty() {
            return false;
        }
        let Some(event) = self.events.iter_mut().find(|event| {
            event.id == id
                && event.meta.command
                && !event.meta.diff
                && event.panel.rows > 0
                && event.matches(&self.query, self.level, self.scope, &self.history_selection)
        }) else {
            return false;
        };
        event.panel.scroll_by(delta);
        self.new_image = None;
        self.follow = false;
        self.reveal = false;
        true
    }

    pub(crate) fn latest(&mut self) {
        self.new_image = None;
        self.follow = true;
        self.reveal = false;
        self.selected = self.visible_ids().last().copied();
        self.selected_group = self
            .selected
            .and_then(|id| self.tool_group_for(id))
            .filter(|group| !group.expanded)
            .map(|group| group.head);
        if let Some(group) = self.selected.and_then(|id| self.tool_group_for(id)) {
            self.events[group.start].group_panel.scroll = None;
        }
    }

    pub(crate) fn toggle(&mut self) -> bool {
        self.new_image = None;
        if let Some(group) = self.selected_tool_header() {
            return self.set_tool_expanded(&group, !group.expanded);
        }
        if let Some(group) = self.selected.and_then(|id| self.read_group_for(id)) {
            return self.set_read_expanded(&group, !group.expanded);
        }
        if let Some(event) = self.events.iter_mut().find(|event| {
            Some(event.id) == self.selected
                && event.matches(&self.query, self.level, self.scope, &self.history_selection)
        }) {
            if self.scope == EventScope::Conversation {
                if event.meta.kind == Kind::Conversation {
                    return false;
                }
                event.set_chat_expanded(!event.chat_expanded);
            } else {
                if event.meta.diff {
                    return false;
                }
                event.expanded = !event.expanded;
            }
            self.follow = false;
            self.reveal = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn open(&mut self) -> bool {
        self.set_expanded(true)
    }

    pub(crate) fn back(&mut self) -> bool {
        self.set_expanded(false)
    }

    fn set_expanded(&mut self, expanded: bool) -> bool {
        self.new_image = None;
        if let Some(group) = self.selected_tool_header() {
            return self.set_tool_expanded(&group, expanded);
        }
        if let Some(group) = self.selected.and_then(|id| self.read_group_for(id)) {
            return self.set_read_expanded(&group, expanded);
        }
        let Some(event) = self.events.iter_mut().find(|event| {
            Some(event.id) == self.selected
                && event.matches(&self.query, self.level, self.scope, &self.history_selection)
        }) else {
            return false;
        };
        let changed = if self.scope == EventScope::Conversation {
            if event.meta.kind == Kind::Conversation {
                return false;
            }
            event.set_chat_expanded(expanded)
        } else {
            if event.meta.diff && !expanded {
                return false;
            }
            let changed = event.expanded != expanded;
            event.expanded = expanded;
            changed
        };
        self.follow = false;
        self.reveal = true;
        changed
    }

    /// Row is relative to the event pane from the most recent render.
    pub(crate) fn mouse_select(&mut self, row: u16) -> bool {
        let Some(id) = self.hit_rows.get(usize::from(row)).copied().flatten() else {
            return false;
        };
        if !self.events.iter().any(|event| {
            event.id == id
                && event.matches(&self.query, self.level, self.scope, &self.history_selection)
        }) {
            return false;
        }
        self.selected = Some(id);
        self.selected_group = self
            .group_header_hit_rows
            .get(usize::from(row))
            .copied()
            .flatten();
        self.new_image = None;
        self.follow = false;
        // Preserve the clicked row, even when it belongs to a long body.
        self.reveal = false;
        true
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.summary_groups.clear();
        self.summary_calls.clear();
        self.summary_observers.clear();
        self.active_summary_group = None;
        self.read_boundary.interrupt();
        self.read_expanded.clear();
        self.bytes = 0;
        self.selected = None;
        self.selected_group = None;
        self.scroll = 0;
        self.follow = true;
        self.reveal = false;
        self.dropped = 0;
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.total_rows = 0;
        self.page_rows = 0;
        self.placements.clear();
        self.new_image = None;
        self.query.clear();
        self.level = 0;
    }

    pub(crate) fn snapshot(&self) -> Value {
        let groups = self.read_groups();
        json!({
            "schema_version":1, "events":self.events.iter().enumerate().map(|(index,event)| json!({
                "id":event.id,"summary":event.summary,"lines":event.lines,
                "bytes":event.text.len(),"expanded":if self.tool_groups().iter().any(|group| group.contains(index)) { event.expanded(self.scope) } else { groups.iter().find(|group|group.contains(index)).map_or_else(||event.expanded(self.scope),|group|group.expanded) },"truncated":event.truncated,
                "selected":Some(event.id)==self.selected,
                "status":event.meta.status.map(Status::label),"command":event.meta.command,
                "kind":event.meta.kind.label(),
                "panel":event.meta.command.then(|| json!({"scroll":event.panel.offset(),"rows":event.panel.rows,"visible_rows":event.panel.page,"following":event.panel.scroll.is_none()})),
                "diagnostic_occurrences":event.diagnostic.map(|(_, count)| count),
                "historical":event.historical,"connection_ended":event.interrupted
            })).collect::<Vec<_>>(),
            "retained_events":self.events.len(),"retained_bytes":self.bytes,
            "event_scope":self.scope.label(),"diff_mode":self.diff_mode().name(),
            "visible_events":self.events.iter().filter(|event| event.matches(&self.query,self.level,self.scope,&self.history_selection)).count(),
            "visible_cards":self.visible_ids().len(),
            "read_groups":groups.iter().map(|group|json!({
                "id":group.head,"count":group.members.len(),"expanded":group.expanded,
                "members":group.members.iter().map(|index|self.events[*index].id).collect::<Vec<_>>()
            })).collect::<Vec<_>>(),
            "tool_groups":self.tool_groups().iter().map(|group| {
                let panel = &self.events[group.start].group_panel;
                json!({
                    "id":group.head,"count":group.members.len(),"expanded":group.expanded,
                    "group_id":group.info.as_ref().map(|info|&info.group_id),
                    "summary":group.info.as_ref().and_then(|info|info.summary.as_deref()),
                    "revision":group.info.as_ref().map_or(0, |info|info.revision),
                    "call_count":group.info.as_ref().map_or(group.members.len() as u64, |info|info.call_count),
                    "covered_call_count":group.info.as_ref().map_or(0, |info|info.covered_call_count),
                    "needs_summary":group.info.as_ref().is_some_and(|info|info.needs_summary()),
                    "summary_editable":group.info.as_ref().is_some_and(|info|
                        self.summary_groups.iter().any(|saved|
                            saved.display.group_id == info.group_id && saved.editable())),
                    "stats":tool_activity::stats(&self.events, group),
                    "members":group.members.iter().map(|index|self.events[*index].id).collect::<Vec<_>>(),
                    "scroll":panel.offset(),"rows":panel.rows,"visible_rows":if group.expanded {panel.page} else {0},
                    "following":panel.scroll.is_none()
                })
            }).collect::<Vec<_>>(),
            "max_events":MAX_EVENTS,"max_bytes":MAX_BYTES,"dropped_events":self.dropped,
            "selected":self.selected,"selected_group":self.selected_group,"scroll":self.scroll,
            "rows_below":self.rows_below(),"following":self.follow,
            "image_cache_bytes":self.image_bytes(),"max_image_cache_bytes":images::CACHE_BYTES,
            "scope":"retained display preview; original tool output uses its archive"
        })
    }

    pub(crate) fn render(
        &mut self,
        width: u16,
        height: u16,
        ui: &Ui,
        focused: bool,
    ) -> Vec<String> {
        self.preserve_read_selection();
        if let Some(group) = self.selected.and_then(|id| self.read_group_for(id))
            && !group.expanded
        {
            self.selected = Some(group.head);
        }
        let groups = self.read_groups();
        let tool_groups = self.tool_groups();
        self.hit_rows.clear();
        self.panel_hit_rows.clear();
        self.group_hit_rows.clear();
        self.group_header_hit_rows.clear();
        self.placements.clear();
        if width == 0 || height == 0 {
            self.page_rows = 0;
            return vec![String::new(); usize::from(height)];
        }
        let width = usize::from(width);
        let height = usize::from(height);
        let footer = usize::from(height > 1 && self.scope == EventScope::All);
        let page = height - footer;
        let indent = 4.min(width.saturating_sub(1));
        let body_width = width.saturating_sub(indent).max(1);
        let mut rows: Vec<(u64, String)> = Vec::new();
        let mut visible_events = 0;
        let mut selected_row = None;
        let mut image_spans = Vec::new();
        let mut panel_spans = Vec::new();
        let mut group_spans = Vec::new();
        let mut group_header_spans = Vec::new();
        let new_image = self.new_image.take();
        let mut new_images = false;
        let mut image_anchor = None;
        let mut previous_compact_tool = false;
        let palette = format!(
            "{}:{}:{}",
            palette_key(ui),
            self.scope.label(),
            self.diff_mode.name()
        );
        for index in 0..self.events.len() {
            if let Some(group) = tool_groups.iter().find(|group| group.contains(index)) {
                if index != group.start {
                    continue;
                }
                if !rows.is_empty() {
                    rows.push((group.head, String::new()));
                }
                let start = rows.len();
                let block = tool_activity::render(
                    &mut self.events,
                    group,
                    tool_activity::Options {
                        width,
                        page,
                        palette: &palette,
                        ui,
                        selected: self.selected,
                        selected_group: self.selected_group,
                        focused,
                        reveal: self.reveal,
                    },
                );
                if let Some(row) = block.selected_row {
                    selected_row = Some(start + row);
                }
                panel_spans.extend(
                    block
                        .panels
                        .into_iter()
                        .map(|(id, from, to)| (id, start + from, start + to)),
                );
                group_spans.push((group.head, start, start + block.rows.len()));
                group_header_spans.push((group.head, start, start + block.header_rows));
                if group.expanded {
                    group_header_spans.push((
                        group.head,
                        start + block.rows.len() - 1,
                        start + block.rows.len(),
                    ));
                }
                rows.extend(block.rows);
                previous_compact_tool = false;
                visible_events += group.members.len();
                continue;
            }
            let event = &mut self.events[index];
            if !event.matches(&self.query, self.level, self.scope, &self.history_selection) {
                continue;
            }
            if let Some(group) = groups.iter().find(|group| group.contains(index)) {
                if index == group.start {
                    visible_events += 1;
                    if !rows.is_empty() && !(previous_compact_tool && !group.expanded) {
                        rows.push((group.head, String::new()));
                    }
                    previous_compact_tool = !group.expanded;
                    let selected = self.selected == Some(group.head);
                    if selected {
                        selected_row = Some(rows.len());
                    }
                    let marker = if group.expanded { "▾" } else { "▸" };
                    let prefix = clip_plain(
                        &format!("{} {marker} ", if selected && focused { "›" } else { " " }),
                        indent,
                    );
                    rows.push((
                        group.head,
                        format!(
                            "{}{}",
                            paint(&prefix, "muted", ui, selected && focused),
                            paint(
                                &clip_plain(&group.title(), body_width),
                                "muted",
                                ui,
                                selected && focused
                            )
                        ),
                    ));
                }
                if group.expanded {
                    let selected = self.selected == Some(event.id);
                    if selected && event.id != group.head {
                        selected_row = Some(rows.len());
                    }
                    let detail_palette = format!("{palette}:read-activity");
                    let inset = 2.min(body_width.saturating_sub(1));
                    let detail_width = body_width.saturating_sub(inset).max(1);
                    if event
                        .cache
                        .as_ref()
                        .is_none_or(|(cached_width, cached_palette, _)| {
                            *cached_width != detail_width || *cached_palette != detail_palette
                        })
                    {
                        let detail = event.meta.read.as_ref().unwrap().details(&event.text);
                        event.cache = Some((
                            detail_width,
                            detail_palette,
                            styled_rows(&detail, detail_width, ui.color),
                        ));
                    }
                    for (row, text) in event.cache.as_ref().unwrap().2.iter().enumerate() {
                        let marker = if row == 0 && selected && focused {
                            "› "
                        } else {
                            "  "
                        };
                        let marker = paint(
                            &clip_plain(marker, inset),
                            "accent",
                            ui,
                            selected && focused,
                        );
                        rows.push((event.id, format!("{}{marker}{text}", " ".repeat(indent))));
                    }
                }
                continue;
            }
            visible_events += 1;
            new_images |= new_image == Some(event.id);
            let selected = Some(event.id) == self.selected;
            let chat = self.scope == EventScope::Conversation;
            let compact_tool = chat
                && event.meta.kind == Kind::Tool
                && event.image.is_none()
                && !event.expanded(self.scope)
                && self.query.is_empty();
            if chat && !rows.is_empty() && !(previous_compact_tool && compact_tool) {
                rows.push((event.id, String::new()));
            }
            previous_compact_tool = compact_tool;
            if selected {
                selected_row = Some(rows.len());
            }
            let conversation = chat && event.meta.kind == Kind::Conversation;
            if conversation && event.image.is_none() {
                let tone = if event.meta.user { "accent" } else { "heading" };
                let marker = if event.meta.user { "❯" } else { "●" };
                let named_author = !event.meta.user && !event.meta.author.is_empty();
                if named_author {
                    rows.push((
                        event.id,
                        paint(
                            &clip_plain(&format!("  {marker} {}", event.meta.author), width),
                            tone,
                            ui,
                            true,
                        ),
                    ));
                }
                if event
                    .cache
                    .as_ref()
                    .is_none_or(|(cached_width, cached_palette, _)| {
                        *cached_width != body_width || *cached_palette != palette
                    })
                {
                    let body = if event.meta.raw_message {
                        &event.text
                    } else {
                        conversation_body(&event.text)
                    };
                    let lines = if event.meta.raw_message
                        && !event.meta.user
                        && !event.meta.plain_message
                    {
                        super::markdown::rows(body, body_width as u16, ui)
                    } else {
                        styled_rows(body, body_width, ui.color)
                    };
                    event.cache = Some((body_width, palette.clone(), lines));
                }
                for (index, row) in event.cache.as_ref().unwrap().2.iter().enumerate() {
                    let prefix = if index == 0 && !named_author {
                        paint(
                            &clip_plain(&format!("  {marker} "), indent),
                            tone,
                            ui,
                            false,
                        )
                    } else {
                        " ".repeat(indent)
                    };
                    rows.push((event.id, format!("{prefix}{row}")));
                }
                continue;
            }
            let expanded = event.expanded(self.scope);
            let title = if chat {
                if conversation {
                    event.chat_heading()
                } else {
                    event.chat_title()
                }
            } else {
                event.summary_with_status()
            };
            let marker = if expanded { "▾" } else { "▸" };
            let prefix = format!("{} {marker} ", if selected && focused { "›" } else { " " });
            let prefix = clip_plain(&prefix, indent);
            let tone = if chat {
                event.chat_tone(selected && focused)
            } else if selected && focused {
                "accent"
            } else {
                event.meta.status.map_or(event.tone, Status::tone)
            };
            if chat
                && event.meta.command
                && !event.meta.diff
                && event.image.is_none()
                && self.query.is_empty()
            {
                let start = rows.len();
                let prefix = clip_plain(
                    &format!("{} • ", if selected && focused { "›" } else { " " }),
                    indent,
                );
                rows.push((
                    event.id,
                    format!(
                        "{}{}",
                        paint(&prefix, tone, ui, selected && focused),
                        event.command_heading_row(body_width, ui)
                    ),
                ));
                let inset = 2.min(body_width.saturating_sub(1));
                let panel_width = body_width.saturating_sub(inset).max(1);
                let panel_palette = format!("{palette}:command:{expanded}");
                if event
                    .cache
                    .as_ref()
                    .is_none_or(|(cached_width, cached_palette, _)| {
                        *cached_width != panel_width || *cached_palette != panel_palette
                    })
                {
                    let body = if expanded {
                        // Add the already-redacted source only to this expanded
                        // display copy. Logs and folded output retain event.text.
                        format!("Command\n{}", event.command_detail_text().trim_end())
                    } else {
                        event.command_output()
                    };
                    let lines = if body.is_empty() {
                        Vec::new()
                    } else {
                        styled_rows(
                            &semantic_text(&body, ui, event.meta.language),
                            panel_width,
                            ui.color,
                        )
                    };
                    event.cache = Some((panel_width, panel_palette, lines));
                }
                let content = &event.cache.as_ref().unwrap().2;
                event.panel.rows = content.len();
                let limit = if expanded { (page / 2).clamp(3, 12) } else { 3 };
                event.panel.page = limit.min(content.len());
                let offset = event.panel.offset();
                if event.panel.scroll.is_some() {
                    event.panel.scroll = Some(offset);
                }
                if content.is_empty() {
                    let label = if event.meta.status == Some(Status::Running) {
                        "(no output yet)"
                    } else {
                        "(no output)"
                    };
                    rows.push((
                        event.id,
                        paint(
                            &clip_plain(&format!("{}└ {label}", " ".repeat(indent)), width),
                            "muted",
                            ui,
                            false,
                        ),
                    ));
                }
                for (line, row) in content
                    .iter()
                    .skip(offset)
                    .take(event.panel.page)
                    .enumerate()
                {
                    let gutter = paint(
                        &clip_plain(if line == 0 { "└ " } else { "  " }, inset),
                        "muted",
                        ui,
                        false,
                    );
                    rows.push((event.id, format!("{}{gutter}{row}", " ".repeat(indent))));
                }
                {
                    previous_compact_tool = false;
                    let range = format!(
                        "{}–{}/{}",
                        offset + 1,
                        offset + event.panel.page,
                        content.len()
                    );
                    let partial = if event.truncated
                        || (expanded
                            && event
                                .command_source
                                .as_ref()
                                .is_some_and(|source| source.truncated))
                    {
                        " · partial"
                    } else {
                        ""
                    };
                    let detail = if expanded && width >= 70 {
                        format!("{range} · wheel/PgUp/PgDn · Home command · Enter fold{partial}")
                    } else if expanded && width >= 48 {
                        format!("{range} · Home cmd · Enter fold{partial}")
                    } else if expanded {
                        format!("{range} · Enter fold{partial}")
                    } else {
                        event.command_preview_hint(content.len(), event.panel.page)
                    };
                    rows.push((
                        event.id,
                        paint(
                            &clip_plain(&format!("{}  {detail}", " ".repeat(indent)), width),
                            "muted",
                            ui,
                            false,
                        ),
                    ));
                    panel_spans.push((event.id, start, rows.len()));
                }
                continue;
            }
            if expanded || !self.query.is_empty() || (chat && event.image.is_some()) {
                if let Some(image) = &event.image {
                    let thumbnail = chat && !expanded;
                    let image_start = rows.len();
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(&clip_plain(&title, body_width), tone, ui, false)
                        ),
                    ));
                    let inset = 2.min(body_width.saturating_sub(1));
                    let image_width = body_width.saturating_sub(inset).max(1) as u16;
                    let native = self.image_cells.filter(|_| ui.color).and_then(|cell| {
                        let native = image.native.as_ref()?;
                        let area = if thumbnail {
                            native.layout(
                                image_width.min(32),
                                page.saturating_sub(3).clamp(1, 6) as u16,
                                cell,
                            )?
                        } else {
                            native.layout_width(image_width, cell)?
                        };
                        Some((native.clone(), area))
                    });
                    let detail = match &native {
                        Some((native, area)) => {
                            let dimensions =
                                if (native.width, native.height) == (image.width, image.height) {
                                    format!("{} × {}", image.width, image.height)
                                } else {
                                    format!(
                                        "{} × {} preview · {} × {} source",
                                        native.width, native.height, image.width, image.height
                                    )
                                };
                            if thumbnail {
                                format!("Click/Enter expands · {dimensions}")
                            } else if usize::from(area.rows) > page.saturating_sub(2) {
                                format!("{dimensions} · scroll to inspect")
                            } else {
                                dimensions
                            }
                        }
                        None => format!("{} × {} · Image attachment", image.width, image.height),
                    };
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            " ".repeat(indent + inset),
                            paint(
                                &clip_plain(&detail, usize::from(image_width)),
                                "muted",
                                ui,
                                false
                            )
                        ),
                    ));
                    if let Some((native, area)) = native {
                        if new_images
                            && !thumbnail
                            && image_anchor.is_none()
                            && usize::from(area.rows) > page.saturating_sub(2)
                        {
                            image_anchor = Some((event.id, image_start));
                        }
                        image_spans.push((rows.len(), (indent + inset) as u16, native, area));
                        rows.extend((0..area.rows).map(|_| (event.id, String::new())));
                    } else {
                        if new_images
                            && !thumbnail
                            && self.follow
                            && ui.color
                            && self.image_cells.is_none()
                            && image.native.is_some()
                        {
                            // Resize briefly removes cell measurements. Keep
                            // the first unseen native image pending until its
                            // real height is known, without pausing text output.
                            self.new_image.get_or_insert(event.id);
                        }
                        rows.push((
                            event.id,
                            format!(
                                "{}{}",
                                " ".repeat(indent + inset),
                                paint(
                                    &clip_plain(
                                        "Inline preview unavailable. Use a terminal with image support.",
                                        usize::from(image_width),
                                    ),
                                    "muted",
                                    ui,
                                    false,
                                )
                            ),
                        ));
                    }
                    continue;
                }
                let correlated = chat || event.meta.raw_message || !event.meta.keys.is_empty();
                if correlated {
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(&clip_plain(&title, body_width), tone, ui, false)
                        ),
                    ));
                }
                if event
                    .cache
                    .as_ref()
                    .is_none_or(|(cached_width, cached_palette, _)| {
                        *cached_width != body_width || *cached_palette != palette
                    })
                {
                    let lines = if event.meta.raw_message {
                        if event.meta.user || event.meta.plain_message {
                            styled_rows(&event.text, body_width, ui.color)
                        } else {
                            super::markdown::rows(&event.text, body_width as u16, ui)
                        }
                    } else {
                        let read_detail = (chat || !self.query.is_empty())
                            .then(|| {
                                event
                                    .meta
                                    .read
                                    .as_ref()
                                    .map(|read| read.details(&event.text))
                            })
                            .flatten();
                        let body = if let Some(detail) = &read_detail {
                            detail.as_str()
                        } else if chat {
                            conversation_body(&event.text)
                        } else if correlated {
                            event.text.split_once('\n').map_or("", |(_, body)| body)
                        } else {
                            &event.text
                        };
                        if event.meta.diff {
                            diff::rows(body, body_width, ui, event.meta.language, self.diff_mode)
                        } else {
                            styled_rows(
                                &semantic_text(body, ui, event.meta.language),
                                body_width,
                                ui.color,
                            )
                        }
                    };
                    event.cache = Some((body_width, palette.clone(), lines));
                }
                let content = &event.cache.as_ref().unwrap().2;
                for (index, row) in content.iter().enumerate() {
                    let prefix = if index == 0 && !correlated {
                        paint(&prefix, tone, ui, selected && focused)
                    } else {
                        " ".repeat(indent)
                    };
                    rows.push((event.id, format!("{prefix}{row}")));
                }
                if event.truncated {
                    rows.push((
                        event.id,
                        paint(
                            &clip_plain(
                                if chat {
                                    "    More output is available in Logs."
                                } else {
                                    "    Preview truncated; inspect the original output archive."
                                },
                                width,
                            ),
                            "warning",
                            ui,
                            false,
                        ),
                    ));
                }
            } else {
                let label = if chat {
                    title
                } else {
                    format!(
                        "{} · {} lines{}",
                        title,
                        event.lines,
                        if event.truncated { " · partial" } else { "" }
                    )
                };
                let label = clip_plain(&label, body_width);
                rows.push((
                    event.id,
                    format!(
                        "{}{}",
                        paint(&prefix, tone, ui, selected && focused),
                        paint(&label, tone, ui, selected && focused)
                    ),
                ));
                if chat && event.meta.kind == Kind::Tool && event.image.is_none() {
                    let preview_width = body_width.saturating_sub(2).max(1);
                    let preview_palette = format!("{palette}:preview");
                    if event
                        .cache
                        .as_ref()
                        .is_none_or(|(cached_width, cached_palette, _)| {
                            *cached_width != preview_width || *cached_palette != preview_palette
                        })
                    {
                        event.cache = Some((
                            preview_width,
                            preview_palette,
                            event.chat_preview(preview_width),
                        ));
                    }
                    let preview = &event.cache.as_ref().unwrap().2;
                    if !preview.is_empty() {
                        previous_compact_tool = false;
                    }
                    for (index, line) in preview.iter().enumerate() {
                        let hint = index + 1 == preview.len();
                        let tone = if !hint && event.meta.status.is_some_and(Status::failed) {
                            tone
                        } else {
                            "muted"
                        };
                        let prefix = if index == 0 { "└ " } else { "  " };
                        let line = format!("{}{prefix}{line}", " ".repeat(indent));
                        rows.push((event.id, paint(&clip_plain(&line, width), tone, ui, false)));
                    }
                }
            }
        }
        self.total_rows = rows.len();
        self.page_rows = page;
        let max_scroll = rows.len().saturating_sub(page);
        if self.follow
            && let Some((id, start)) = image_anchor
        {
            // A new tall image starts at its heading, rather than silently
            // showing only its bottom. The existing Ctrl+End action resumes
            // following; later tool output cannot pull the reader away.
            self.selected = Some(id);
            self.scroll = start;
            self.follow = false;
            self.reveal = false;
        } else if self.follow {
            self.scroll = max_scroll;
        } else if self.reveal {
            if let Some(selected_row) = selected_row
                && (selected_row < self.scroll || selected_row >= self.scroll.saturating_add(page))
            {
                self.scroll = selected_row;
            }
            self.reveal = false;
        }
        self.scroll = self.scroll.min(max_scroll);
        for (start, col, image, area) in image_spans {
            let visible_start = start.max(self.scroll);
            let visible_end = (start + usize::from(area.rows)).min(self.scroll + page);
            if visible_start < visible_end
                && let Some(placement) = area.placement(
                    image,
                    (visible_start - self.scroll) as u16,
                    col,
                    (visible_start - start) as u16,
                    (visible_end - visible_start) as u16,
                )
            {
                self.placements.push(placement);
            }
        }
        self.group_header_hit_rows.resize(page, None);
        for (id, start, end) in group_header_spans {
            for row in start.max(self.scroll)..end.min(self.scroll + page) {
                self.group_header_hit_rows[row - self.scroll] = Some(id);
            }
        }
        self.group_hit_rows.resize(page, None);
        for (id, start, end) in group_spans {
            for row in start.max(self.scroll)..end.min(self.scroll + page) {
                self.group_hit_rows[row - self.scroll] = Some(id);
            }
        }
        self.panel_hit_rows.resize(page, None);
        for (id, start, end) in panel_spans {
            for row in start.max(self.scroll)..end.min(self.scroll + page) {
                self.panel_hit_rows[row - self.scroll] = Some(id);
            }
        }
        let mut result = rows
            .into_iter()
            .skip(self.scroll)
            .take(page)
            .map(|(id, row)| {
                self.hit_rows.push(Some(id));
                row
            })
            .collect::<Vec<_>>();
        while result.len() < page {
            result.push(String::new());
            self.hit_rows.push(None);
        }
        if footer != 0 {
            let hint = if visible_events == 0 {
                if self.events.is_empty() {
                    "No events yet."
                } else {
                    "No matching events."
                }
                .to_owned()
            } else if width < 60 {
                "Enter fold/open · retained preview only".to_owned()
            } else if focused {
                format!(
                    "{} events · ↑↓ select · Enter fold/open · full output: archive",
                    visible_events
                )
            } else {
                format!(
                    "{} events · focus Activity to fold/open · full output: archive",
                    visible_events
                )
            };
            result.push(paint(&clip_plain(&hint, width), "muted", ui, false));
            self.hit_rows.push(None);
        }
        result
    }
}

/// Remove the renderer's first label only. Message text and tool output below
/// that boundary are retained verbatim in both the event and the original log.
fn conversation_body(text: &str) -> &str {
    let text = text.trim_matches(['\r', '\n']);
    let Some((first, body)) = text.split_once('\n') else {
        return text;
    };
    let plain = unstyled(first);
    let first = heading(&plain);
    if first
        .strip_prefix('[')
        .and_then(|text| text.split_once(']'))
        .is_some_and(|(tag, _)| {
            matches!(
                tag.trim(),
                "USER"
                    | "AGENT"
                    | "UPDATE"
                    | "ANSWER"
                    | "CONTEXT"
                    | "HISTORY"
                    | "REQ"
                    | "RES"
                    | "RUN"
                    | "OUT"
                    | "STDOUT"
                    | "STDERR"
                    | "READ"
                    | "EDIT"
                    | "DIFF"
                    | "ERR"
                    | "WARN"
                    | "OK"
                    | "IMG"
                    | "IMAGE"
                    | "SKIL"
                    | "PROG"
                    | "SAVE"
            )
        })
    {
        body.trim_matches(['\r', '\n'])
    } else {
        text
    }
}

fn heading(line: &str) -> &str {
    let line = line.trim();
    if let Some((timestamp, rest)) = line.split_once(' ')
        && timestamp.len() == 13
        && timestamp.ends_with('Z')
        && timestamp
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b":.Z".contains(&byte))
    {
        return rest.trim_start();
    }
    line
}

fn event_summary(plain: &str, first: &str) -> String {
    let mut summary = first.to_owned();
    if (first.starts_with("[RES") || first.starts_with("[REQ"))
        && first.ends_with("audit")
        && let Some(detail) = plain.lines().map(heading).find(|line| {
            line.contains('.')
                && line.len() <= 128
                && line
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        })
    {
        summary.push_str(" · ");
        summary.push_str(detail);
    }
    super::super::panel::fit(&summary, 200)
}

/// Reuse the shared lexer, preserving existing renderer SGR and exact text.
fn semantic_text(text: &str, ui: &Ui, language: &str) -> String {
    if !ui.color {
        return sanitize(text, false);
    }
    let mut output = String::new();
    let mut lexer = crate::client::highlight::Lexer::for_language(language);
    let mut code = !language.is_empty();
    let mut fence: Option<char> = None;
    for line in text.split_inclusive('\n') {
        let plain = unstyled(line);
        let trimmed = plain.trim_start();
        let marker = trimmed.chars().next();
        let is_fence = matches!(marker, Some('\u{60}' | '~'))
            && trimmed.chars().take_while(|ch| Some(*ch) == marker).count() >= 3;
        if is_fence {
            if fence == marker {
                fence = None;
                code = !language.is_empty();
                lexer = crate::client::highlight::Lexer::for_language(language);
            } else if fence.is_none() {
                fence = marker;
                code = true;
                lexer = crate::client::highlight::Lexer::for_language(
                    trimmed.trim_start_matches(marker.unwrap()).trim(),
                );
            }
            output.push_str(&paint(line, "muted", ui, false));
            continue;
        }
        if line.contains('\x1b') {
            // Advance language state, but the renderer's established colors win.
            if code {
                let _ = lexer.line(&plain);
            }
            output.push_str(line);
        } else if trimmed.starts_with('[') {
            let tone = if trimmed.starts_with("[ERR") {
                "error"
            } else if trimmed.starts_with("[WARN") || trimmed.starts_with("[STDERR") {
                "warning"
            } else if trimmed.starts_with("[OK") {
                "success"
            } else if trimmed.starts_with("[READ") || trimmed.starts_with("[WRITE") {
                "blue"
            } else if trimmed.starts_with("[RES") || trimmed.starts_with("[REQ") {
                "purple"
            } else {
                "accent"
            };
            output.push_str(&paint(line, tone, ui, false));
        } else if code {
            for token in lexer.line(line) {
                output.push_str(&paint(token.text, token.tone, ui, token.bold));
            }
        } else if trimmed.starts_with('+') && !trimmed.starts_with("+++") {
            output.push_str(&paint(line, "success", ui, false));
        } else if trimmed.starts_with('-') && !trimmed.starts_with("---") {
            output.push_str(&paint(line, "error", ui, false));
        } else if trimmed.starts_with('#') || trimmed.starts_with("//") {
            output.push_str(&paint(line, "muted", ui, false));
        } else {
            for word in line.split_inclusive(char::is_whitespace) {
                let token = word.trim_end();
                let path = token.contains('/')
                    || token.contains('\\')
                    || !correlation::language_for_path(token).is_empty();
                if path {
                    output.push_str(&paint(word, "blue", ui, false));
                } else {
                    output.push_str(word);
                }
            }
        }
    }
    output
}

fn palette_key(ui: &Ui) -> String {
    if !ui.color {
        return String::new();
    }
    [
        "foreground",
        "muted",
        "accent",
        "success",
        "warning",
        "error",
        "blue",
        "purple",
    ]
    .iter()
    .map(|tone| ui.color(tone))
    .collect::<Vec<_>>()
    .join(";")
}

fn paint(text: &str, tone: &str, ui: &Ui, bold: bool) -> String {
    if ui.color {
        format!(
            "\x1b[{}{}m{text}\x1b[0m",
            if bold { "1;" } else { "" },
            ui.color(tone)
        )
    } else {
        text.to_owned()
    }
}

fn clip_plain(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width.saturating_sub(1) {
            break;
        }
        result.push(ch);
        used += cells;
    }
    if width != 0 {
        result.push('…');
    }
    result
}

/// Retain SGR colors only. Cursor movement, OSC clipboard/hyperlink commands and
/// all other control sequences are absent from the independently rendered view.
fn sanitize(text: &str, keep_color: bool) -> String {
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.next() {
                Some('[') => {
                    let mut sequence = String::new();
                    for next in chars.by_ref() {
                        sequence.push(next);
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                        if sequence.len() > 128 {
                            break;
                        }
                    }
                    if keep_color
                        && sequence.ends_with('m')
                        && sequence[..sequence.len() - 1]
                            .chars()
                            .all(|c| c.is_ascii_digit() || c == ';')
                    {
                        result.push_str("\x1b[");
                        result.push_str(&sequence);
                    }
                }
                Some(']') => {
                    while let Some(next) = chars.next() {
                        if next == '\x07' || (next == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if ch == '\r' {
            if chars.peek() != Some(&'\n') {
                result.push('\n');
            }
        } else if !ch.is_control() || matches!(ch, '\n' | '\t') {
            result.push(ch);
        }
    }
    result
}

fn unstyled(text: &str) -> String {
    sanitize(text, false)
}

fn retained_text(bytes: &[u8]) -> (String, bool) {
    retained_text_with_note(
        bytes,
        "\n… preview truncated; inspect the original output archive …\n",
    )
}

fn retained_text_with_note(bytes: &[u8], note: &str) -> (String, bool) {
    let raw = String::from_utf8_lossy(bytes);
    let clean = sanitize(&raw, true);
    if clean.len() <= MAX_DETAIL {
        return (clean, false);
    }
    let budget = MAX_DETAIL - note.len();
    let mut head = budget / 2;
    while !clean.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = clean.len() - (budget - head);
    while !clean.is_char_boundary(tail) {
        tail += 1;
    }
    (
        format!(
            "{}{note}{}",
            sanitize(&clean[..head], true),
            sanitize(&clean[tail..], true)
        ),
        true,
    )
}

/// Reflow only the retained display text, maintaining SGR across soft wraps.
pub(super) fn styled_rows(text: &str, width: usize, color: bool) -> Vec<String> {
    let text = sanitize(text, color);
    let text = text.trim_matches('\n');
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut active = String::new();
    let mut used = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.next() == Some('[') {
            let mut sequence = String::from("\x1b[");
            for next in chars.by_ref() {
                sequence.push(next);
                if next == 'm' {
                    break;
                }
            }
            row.push_str(&sequence);
            if sequence == "\x1b[0m" || sequence == "\x1b[m" {
                active.clear();
            } else {
                if active.len() > 256 {
                    active.clear();
                }
                active.push_str(&sequence);
            }
            continue;
        }
        if ch == '\n' {
            if color {
                row.push_str("\x1b[0m");
            }
            rows.push(row);
            row = active.clone();
            used = 0;
            continue;
        }
        let expanded = if ch == '\t' {
            " ".repeat(4 - used % 4)
        } else {
            ch.to_string()
        };
        for mut ch in expanded.chars() {
            let mut cells = ch.width().unwrap_or(0);
            if cells > width {
                ch = '…';
                cells = 1;
            }
            if used + cells > width && used != 0 {
                if color {
                    row.push_str("\x1b[0m");
                }
                rows.push(row);
                row = active.clone();
                used = 0;
            }
            row.push(ch);
            used += cells;
        }
    }
    if color {
        row.push_str("\x1b[0m");
    }
    rows.push(row);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recalled_message_uses_internal_identity_and_preserves_identical_other_messages() {
        let mut view = EventView::new();
        view.ingest_identified_message(
            "message_pending",
            "user",
            "SAME_BODY",
            b"[USER] SAME_BODY\n",
        );
        let removed = view.events[0].id;
        view.ingest_identified_message(
            "message_delivered",
            "user",
            "SAME_BODY",
            b"[USER] SAME_BODY\n",
        );
        view.ingest_event(
            "message",
            &json!({
                "type":"ssmcp.message", "role":"user", "text":"SAME_BODY",
                "history_id":"message_pending",
            }),
            b"[USER] SAME_BODY\n",
        );
        view.selected = Some(removed);
        assert!(view.recall_message("message_pending"));
        assert_eq!(view.events.len(), 2);
        assert!(view.events.iter().all(|event| event.text == "SAME_BODY"));
        assert!(
            view.selected
                .is_some_and(|id| view.events.iter().any(|event| event.id == id))
        );
        assert_eq!(
            view.bytes,
            view.events.iter().map(Event::retained_bytes).sum::<usize>()
        );
        assert!(!view.recall_message("message_pending"));
        assert_eq!(view.events.len(), 2);
        // Resumed JSONL messages keep their internal identity as well.
        view.clear();
        view.ingest_identified_message(
            "message_pending",
            "user",
            "SAME_BODY",
            b"[USER] SAME_BODY\n",
        );
        assert!(view.recall_message("message_pending"));
        assert!(view.events.is_empty());
        assert_eq!(view.bytes, 0);
    }

    #[test]
    fn diff_modes_reflow_existing_events_without_changing_retained_content() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.ingest_event(
            "files",
            &json!({"type":"ssmcp.file.diff","path":"example.rs"}),
            b"[DIFF] example.rs\n@@ -1 +1 @@\n-old value\n+new value\n",
        );
        assert_eq!(view.diff_mode(), DiffMode::Single);
        let preview = view.render(120, 30, &plain_ui(), false).join("\n");
        assert!(preview.contains("-old value") && preview.contains("+new value"));
        assert!(!preview.contains("[DIFF]") && !preview.contains("@@ -1"));
        assert!(view.open());
        let retained = (view.events.len(), view.bytes);
        let single = view.render(120, 30, &plain_ui(), true);
        view.set_diff_mode(DiffMode::Split);
        assert!(
            !view.mouse_select(0),
            "old row hit targets must be invalidated"
        );
        let split = view.render(120, 30, &plain_ui(), true).join("\n");
        assert!(split.contains("Old") && split.contains("New"));
        assert!(
            split
                .lines()
                .any(|row| row.contains("- old value") && row.contains("+ new value"))
        );
        assert_eq!(view.snapshot()["diff_mode"], "split");
        assert_eq!((view.events.len(), view.bytes), retained);
        view.set_diff_mode(DiffMode::Single);
        assert_eq!(view.render(120, 30, &plain_ui(), true), single);
        let narrow_single = view.render(40, 30, &plain_ui(), true);
        view.set_diff_mode(DiffMode::Split);
        assert_eq!(view.render(40, 30, &plain_ui(), true), narrow_single);
        assert_eq!((view.events.len(), view.bytes), retained);
    }

    #[test]
    fn native_prose_uses_original_markdown_instead_of_pre_wrapped_log_rows() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        let source = "连续中文正文保持自然换行，段落不被旧终端列宽切碎。";
        view.ingest_event(
            "agent",
            &json!({"type":"ssmcp.message","text":source}),
            "[AGENT] Progress\n连\n续\n中\n文\n".as_bytes(),
        );
        assert_eq!(view.events.back().unwrap().text, source);
        view.render(16, 20, &plain_ui(), false);
        let wide = view.render(100, 20, &plain_ui(), false).join("\n");
        assert!(wide.contains(source), "{wide}");
        assert!(!wide.contains("Progress"));
        view.ingest_event("user", &json!({"type":"ssmcp.message","role":"user","text":"[AGENT] literal\nKeep this user text."}),
            b"[USER] #request queued\nold rows\n");
        let rows = view.render(100, 30, &plain_ui(), false).join("\n");
        assert!(
            rows.contains("[AGENT] literal"),
            "literal message content is preserved"
        );
        assert!(rows.contains("Keep this user text."));
        assert!(!rows.contains("#request") && !rows.contains("old rows"));

        let message = json!({
            "type":"ssmcp.message",
            "role":"assistant",
            "text":"Authorization: Bearer ui-fixture-secret\nA public explanation remains visible."
        });
        let original = message.clone();
        view.ingest_event(
            "agent",
            &message,
            b"[AGENT] Progress\nAuthorization: [redacted]\nA public explanation remains visible.\n",
        );
        let rows = view.render(100, 30, &plain_ui(), false).join("\n");
        assert!(!rows.contains("ui-fixture-secret"), "{rows}");
        assert!(rows.contains("Authorization:[redacted]"), "{rows}");
        assert!(
            rows.contains("A public explanation remains visible."),
            "{rows}"
        );
        assert_eq!(message, original);
    }

    #[test]
    fn structured_presentations_keep_the_rendered_document_in_the_conversation() {
        let ui = plain_ui();
        let presentation = json!({
            "title": "Release summary",
            "blocks": [
                {"type": "card", "title": "Ready card", "body": "Verified structured content", "tone": "success"},
                {"type": "table", "columns": ["Check", "Result"], "rows": [["unit", "passed"]]}
            ]
        });
        let document: crate::client::presentation::Presentation =
            serde_json::from_value(presentation.clone()).unwrap();
        let rendered = format!("\n[AGENT] Answer\n{}\n", document.render(80, &ui, false));
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.ingest_event(
            "agent",
            &json!({
                "type": "ssmcp.message",
                "format": "intelligence_ui.v1",
                "text": "plain fallback only",
                "presentation": presentation
            }),
            rendered.as_bytes(),
        );
        let rows = view.render(100, 40, &ui, false).join("\n");
        for expected in [
            "Release summary",
            "Ready card",
            "Verified structured content",
            "Check",
            "Result",
            "unit",
            "passed",
        ] {
            assert!(rows.contains(expected), "missing {expected}: {rows}");
        }
        assert!(!rows.contains("plain fallback only"));
    }

    #[test]
    fn conversation_command_updates_preserve_command_source_and_output() {
        let mut view = EventView::new();
        let job = "1234567890abcdef1234567890abcdef-1a";
        let invocation = "bash -lc 'printf first'";
        let start_header = "[RUN ] shell #1a bash -lc · 2 lines started";
        let cwd = "       cwd /example\n";
        let started = format!("{start_header}\n{cwd}");
        let output = "[STDOUT] shell #1a\nfirst-result\nsecond-result\nthird-result\n";
        let finished = "[OK ] shell #1a completed exit=0 0.04s · bash -lc · 2 lines\n";
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":job,"command":["bash","-lc","printf first"]}),
            started.as_bytes(),
        );
        assert_eq!(view.events[0].text, started);
        view.set_scope(EventScope::Conversation);
        let running = view.render(100, 12, &plain_ui(), false).join("\n");
        assert!(
            running.contains("Running bash -lc 'printf first'"),
            "{running}"
        );
        assert!(!running.contains("shell #1a") && !running.contains("started"));
        view.ingest_event(
            "shell",
            &json!({"type":"exec.stdout","job_id":job}),
            output.as_bytes(),
        );
        let streamed = format!("{started}{output}");
        assert_eq!(view.events[0].text, streamed);
        view.ingest_event(
            "shell",
            &json!({"type":"exec.exited","job_id":job,"exit_code":0}),
            finished.as_bytes(),
        );
        let done = view.render(100, 12, &plain_ui(), false).join("\n");
        assert!(done.contains("Ran bash -lc 'printf first'"), "{done}");
        assert!(done.contains("first-result") && done.contains("second-result"));
        assert!(done.contains("Show details · Enter"));
        for hidden in ["exit=0", "cwd /example", "[RUN", "[STDOUT", "[OK"] {
            assert!(!done.contains(hidden), "unexpected {hidden}: {done}");
        }
        let retained = format!("{streamed}{finished}");
        assert_eq!(view.events.len(), 1);
        let event = &view.events[0];
        assert_eq!(event.text, retained);
        assert!(!event.text.contains(invocation), "{}", event.text);
        assert_eq!(event.meta.status, Some(Status::Exit(0)));
        let source = event
            .command_source
            .as_ref()
            .expect("lifecycle updates preserve the audited invocation");
        assert_eq!(source.text, format!("cmd={invocation}\n"));
        assert!(!source.truncated, "the fixture invocation fits in full");

        let expected_detail = format!("{start_header}\ncmd={invocation}\n{cwd}{output}{finished}");
        let detail = event.command_detail_text();
        assert_eq!(detail, expected_detail);
        assert_eq!(detail.matches(invocation).count(), 1, "{detail}");

        let bytes = view.bytes;
        assert!(view.open(), "the selected command expands in conversation");
        let expanded = view.render(100, 20, &plain_ui(), false).join("\n");
        assert_eq!(expanded.matches(invocation).count(), 2, "{expanded}");
        assert_eq!(
            view.render(100, 20, &plain_ui(), false).join("\n"),
            expanded
        );
        view.set_scope(EventScope::All);
        view.open();
        let logs = view.render(100, 20, &plain_ui(), false).join("\n");
        for detail in [
            "shell #1a",
            "started",
            "exit=0",
            "first-result",
            "second-result",
            "third-result",
            "[STDOUT]",
        ] {
            assert!(logs.contains(detail), "missing {detail}: {logs}");
        }
        assert!(!logs.contains(invocation), "{logs}");
        assert_eq!((view.events.len(), view.bytes), (1, bytes));
        assert_eq!(view.events[0].text, retained);
        assert_eq!(view.events[0].command_detail_text(), expected_detail);

        for (job, summary, expected) in [
            ("17", "[RUN] #17 cargo test --lib", "Ran cargo test --lib"),
            (
                "1234567890abcdef1234567890abcdef-1a",
                "[RUN] #1a cargo check",
                "Ran cargo check",
            ),
            ("17", "[RUN] printf #17 started", "Ran printf #17 started"),
            ("17", "[RUN] #other cargo check", "Ran #other cargo check"),
        ] {
            let mut preview = EventView::new();
            preview.ingest_event(
                "preview",
                &json!({"type":"exec.exited","job_id":job,"exit_code":0}),
                summary.as_bytes(),
            );
            preview.set_scope(EventScope::Conversation);
            let rendered = preview.render(100, 8, &plain_ui(), false).join("\n");
            assert!(rendered.contains(expected), "{rendered}");
            assert!(
                rendered.contains("(no output)") && rendered.contains("Show details · Enter"),
                "empty output still exposes retained command details: {rendered}"
            );
            assert_eq!(preview.events[0].text, summary);
        }
    }

    #[test]
    fn conversation_command_arguments_and_failure_states_remain_visible_when_selected() {
        for (mut outcome, status, tone) in [
            (json!({"exit_code":7}), "exit=7", "error"),
            (json!({"cancelled":true}), "cancelled", "warning"),
            (json!({"timed_out":true}), "timeout", "warning"),
            (json!({"exit_signal":9}), "signal=9", "error"),
        ] {
            let mut view = EventView::new();
            let job = "1234567890abcdef1234567890abcdef-abc";
            view.ingest_event(
                "shell",
                &json!({"type":"exec.started","job_id":job,"command":["printf","#123","started"]}),
                b"[RUN ] shell #abc printf '#123' started started\n",
            );
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":job}),
                b"[STDOUT] shell #abc\nwarming up\nloading inputs\n",
            );
            outcome["type"] = json!("exec.exited");
            outcome["job_id"] = json!(job);
            view.ingest_event(
                "shell",
                &outcome,
                b"[ERR ] shell exit=7\n[STDERR] shell #abc\nwarning: cache unavailable\nwarning: retry exhausted\nfatal: failed to load configuration\n\n",
            );
            view.set_scope(EventScope::Conversation);
            let ui = Ui::default();
            let rendered = view.render(100, 8, &ui, true).join("\n");
            let plain = unstyled(&rendered);
            assert!(
                plain.contains("printf '#123' started") && plain.to_lowercase().contains(status),
                "{plain}"
            );
            assert!(!plain.contains("shell #abc") && !plain.contains(" · done"));
            assert!(
                plain.contains("warning: retry exhausted")
                    && plain.contains("fatal: failed to load configuration"),
                "{plain}"
            );
            for hidden in ["warming up", "[STDERR]", "[ERR"] {
                assert!(!plain.contains(hidden), "unexpected {hidden}: {plain}");
            }
            assert!(rendered.contains(&ui.color(tone)), "{rendered:?}");
            view.open();
            let head = view.render(100, 12, &plain_ui(), true).join("\n");
            assert!(
                head.contains("Command") && head.contains("warming up"),
                "{head}"
            );
            assert_eq!(view.events[0].panel.offset(), 0);
            assert!(view.scroll_selected_panel(isize::MAX));
            let expanded = view.render(100, 12, &plain_ui(), true).join("\n");
            assert!(
                expanded.contains("warning: cache unavailable")
                    && expanded.contains("warning: retry exhausted")
                    && expanded.contains("fatal: failed to load configuration"),
                "{expanded}"
            );
        }
    }

    #[test]
    fn compact_tool_runs_keep_message_spacing_and_exact_mouse_targets() {
        let mut view = EventView::new();
        view.ingest_conversation(b"[USER] queued\nPlease check the project.\n");
        for (id, tool) in [("a", "inspect symbols"), ("b", "inspect diagnostics")] {
            view.ingest_event(
                "native",
                &json!({"type":"harness.tool.started","call_id":id}),
                format!("[REQ] {tool} call={id} started\n").as_bytes(),
            );
        }
        view.ingest_event(
            "agent",
            &json!({"type":"ssmcp.message","author":"Ada","kind":"answer","text":"Both checks are running."}),
            b"",
        );
        view.set_scope(EventScope::Conversation);
        let folded = view.render(100, 20, &plain_ui(), false).join("\n");
        assert!(folded.contains("Tools · 2 calls") && !folded.contains("inspect symbols"));
        view.select(-1);
        assert!(view.open());
        let rows = view.render(100, 20, &plain_ui(), false);
        let first = rows
            .iter()
            .position(|row| row.contains("inspect symbols"))
            .unwrap();
        let second = rows
            .iter()
            .position(|row| row.contains("inspect diagnostics"))
            .unwrap();
        let answer = rows.iter().position(|row| row.contains("Ada")).unwrap();
        assert_eq!(second, first + 1, "folded tools share a compact group");
        assert!(rows[first - 1].contains("Tools · 2 calls"), "{rows:?}");
        assert!(rows[first - 2].is_empty(), "{rows:?}");
        assert_eq!(
            answer,
            second + 3,
            "assistant prose starts after the shared footer and a paragraph gap"
        );
        assert_eq!(view.hit_rows[first], Some(view.events[1].id));
        assert_eq!(view.hit_rows[second], Some(view.events[2].id));
        for (id, detail) in [
            ("a", "first-result\nfirst-context\nfirst-tail\n"),
            ("b", "second-result\nsecond-context\nsecond-tail\n"),
        ] {
            view.ingest_event(
                "native",
                &json!({"type":"tool_result","call_id":id}),
                detail.as_bytes(),
            );
        }
        let rows = view.render(100, 24, &plain_ui(), false);
        let result_row = rows
            .iter()
            .position(|row| row.contains("second-result"))
            .unwrap();
        assert_eq!(view.hit_rows[result_row], Some(view.events[2].id));
        assert_eq!(view.hit_rows[result_row + 1], Some(view.events[2].id));
        assert!(rows[result_row + 2].contains("1–6/6"), "{rows:?}");
        assert_eq!(
            rows.iter()
                .filter(|row| row.contains("scroll tools"))
                .count(),
            1
        );
        let collapsed = rows.join("\n");
        assert!(collapsed.contains("first-result") && collapsed.contains("second-context"));
        assert!(!collapsed.contains("first-tail") && !collapsed.contains("second-tail"));
        assert!(view.mouse_select((result_row + 1) as u16));
        assert_eq!(view.selected, Some(view.events[2].id));
        assert!(view.open());
        let expanded = view.render(100, 20, &plain_ui(), false).join("\n");
        assert!(
            expanded.contains("second-result") && expanded.contains("second-tail"),
            "{expanded}"
        );
        assert!(expanded.contains("first-context"), "{expanded}");
        assert_eq!(view.events.len(), 4);
    }

    #[test]
    fn command_panels_keep_independent_positions_while_streams_append() {
        let mut view = EventView::new();
        for id in ["build", "test"] {
            view.ingest_event(
                "shell",
                &json!({"type":"exec.started","job_id":id}),
                format!("[RUN] shell {id} cargo {id} started\n").as_bytes(),
            );
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":id}),
                (0..30)
                    .map(|n| format!("{id}-{n:02}\n"))
                    .collect::<String>()
                    .as_bytes(),
            );
            if id == "build" {
                view.ingest(b"separating diagnostic\n");
            }
        }
        view.set_scope(EventScope::Conversation);
        let rows = view.render(100, 24, &plain_ui(), false);
        assert_eq!(view.events.len(), 3);
        assert_eq!(
            view.total_rows, 11,
            "two bounded windows and one paragraph gap"
        );
        let first = rows
            .iter()
            .position(|row| row.contains("build-27"))
            .unwrap();
        let second = rows.iter().position(|row| row.contains("test-27")).unwrap();
        let before = view.scroll;
        assert!(view.scroll_panel_at(first as u16, -9));
        assert_eq!(view.events[0].panel.offset(), 18);
        assert_eq!(view.events[2].panel.offset(), 27);
        assert_eq!(view.scroll, before);
        assert!(view.scroll_panel_at(second as u16, -3));
        assert_eq!(view.events[0].panel.offset(), 18);
        assert_eq!(view.events[2].panel.offset(), 24);
        for id in ["build", "test"] {
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":id}),
                format!("{id}-30\n").as_bytes(),
            );
        }
        let rows = view.render(100, 24, &plain_ui(), false);
        assert_eq!(
            view.events.len(),
            3,
            "each stream remains in its original command"
        );
        assert_eq!(view.events[0].panel.offset(), 18);
        assert_eq!(view.events[2].panel.offset(), 24);
        assert!(rows.iter().any(|row| row.contains("build-18")));
        assert!(!rows.iter().any(|row| row.contains("build-30")));
        assert!(view.scroll_panel_at(first as u16, isize::MAX));
        assert!(view.events[0].panel.scroll.is_none());
        view.ingest_event(
            "shell",
            &json!({"type":"exec.stdout","job_id":"build"}),
            b"build-31\n",
        );
        let rows = view.render(100, 24, &plain_ui(), false);
        assert!(rows.iter().any(|row| row.contains("build-31")));
        assert_eq!(view.events[2].panel.offset(), 24);
        assert!(
            !view.scroll_panel_at(5, -3),
            "the paragraph gap is outside either window"
        );
    }

    #[test]
    fn expanded_command_panels_reveal_retained_content_without_growing_unbounded() {
        let mut view = EventView::new();
        let text = format!(
            "[RUN] cargo test started\n{}",
            (0..80)
                .map(|n| format!("case-{n:02}\n"))
                .collect::<String>()
        );
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"test"}),
            text.as_bytes(),
        );
        view.set_scope(EventScope::Conversation);
        view.render(80, 24, &plain_ui(), true);
        assert!(view.open());
        view.render(80, 24, &plain_ui(), true);
        assert_eq!(view.events[0].panel.page, 12);
        assert_eq!(view.total_rows, 14);
        assert_eq!(view.events[0].panel.offset(), 0);
        let head = view.render(80, 24, &plain_ui(), true).join("\n");
        assert!(head.contains("Command") && head.contains("[RUN] cargo test started"));
        assert!(head.contains("case-00") && head.contains("case-09"));
        let position = view.position();
        assert!(view.scroll_selected_panel(isize::MAX));
        let tail = view.render(80, 24, &plain_ui(), true).join("\n");
        assert!(tail.contains("case-79"));
        view.restore_position(&position);
        assert_eq!(view.events[0].panel.offset(), 0);
        assert!(view.back());
        for width in [1, 24, 48, 100] {
            for row in view.render(width, 24, &plain_ui(), true) {
                assert!(
                    unstyled(&row).width() <= usize::from(width),
                    "{width}: {row}"
                );
            }
        }
        view.filter("case-40", 0);
        assert!(
            !view.scroll_selected_panel(3),
            "search retains its existing navigation"
        );
        let found = view.render(80, 90, &plain_ui(), false).join("\n");
        assert!(found.contains("case-40"));
        assert_eq!(
            view.events[0].text, text,
            "folding and scrolling never discard the retained source"
        );
    }

    #[test]
    fn command_details_open_at_the_script_and_restore_each_columns_folded_log_position() {
        for manual_scroll in [false, true] {
            let mut view = EventView::new();
            let text = format!(
                "[RUN] shell script python3 -c · 3 lines started\n\
                 cwd /workspace/demo\n\
                 script · python3 · 3 lines\n\
                 1 import pathlib\n\
                 2 token = '<redacted>'\n\
                 3 print(pathlib.Path.cwd())\n\
                 [STDOUT] shell script\n{}",
                (0..40)
                    .map(|n| format!("output-{n:02}\n"))
                    .collect::<String>()
            );
            view.ingest_event(
                "shell",
                &json!({"type":"exec.started","job_id":"script",
                    "command":["python3","-c","API_TOKEN=raw-metadata-secret"]}),
                text.as_bytes(),
            );
            view.set_scope(EventScope::Conversation);
            view.render(100, 24, &plain_ui(), true);
            if manual_scroll {
                assert!(view.scroll_selected_panel(-9));
            }
            let folded = view.events[0].panel.scroll;
            assert!(view.open());
            let details = view.render(100, 24, &plain_ui(), true).join("\n");
            assert!(details.contains("Command") && details.contains("1 import pathlib"));
            assert!(details.contains("token = '<redacted>'"));
            assert!(!details.contains("raw-metadata-secret"));
            assert_eq!(view.events[0].panel.offset(), 0);
            assert!(view.scroll_selected_panel(3));
            let scrolled = view.events[0].panel.scroll;
            assert!(
                !view.open(),
                "opening an already-open panel keeps its position"
            );
            assert_eq!(view.events[0].panel.scroll, scrolled);
            let column = view.position();
            assert!(view.scroll_selected_panel(isize::MAX));
            assert!(view.back());
            view.render(100, 24, &plain_ui(), true);
            assert_eq!(view.events[0].panel.scroll, folded);

            view.restore_position(&column);
            view.render(100, 24, &plain_ui(), true);
            assert_eq!(view.events[0].panel.scroll, scrolled);
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":"script"}),
                b"output-40\n",
            );
            assert!(view.back());
            let folded_rows = view.render(100, 24, &plain_ui(), true).join("\n");
            assert_eq!(view.events.len(), 1, "reads remain in the same command");
            assert_eq!(view.events[0].panel.scroll, folded);
            assert_eq!(folded_rows.contains("output-40"), !manual_scroll);
            assert!(view.open());
            let reopened = view.render(48, 24, &plain_ui(), true).join("\n");
            assert!(reopened.contains("1 import pathlib") && reopened.contains("Home cmd"));
            assert_eq!(view.events[0].text, format!("{text}output-40\n"));
        }
    }

    #[test]
    fn conversation_messages_flow_inline_without_repeated_role_labels() {
        let mut view = EventView::new();
        view.ingest_conversation(b"[USER] queued\nKeep the conversation readable.\n");
        view.ingest_event(
            "agent",
            &json!({"type":"ssmcp.message","kind":"answer"}),
            b"[AGENT] Answer\nThe result stays beside its marker.\n",
        );
        view.set_scope(EventScope::Conversation);
        let rows = view.render(80, 20, &plain_ui(), false).join("\n");
        assert!(rows.contains("❯ Keep the conversation readable."), "{rows}");
        assert!(
            rows.contains("● The result stays beside its marker."),
            "{rows}"
        );
        assert!(
            !rows
                .lines()
                .any(|row| matches!(row.trim(), "❯ You" | "● Assistant"))
        );
    }

    #[test]
    fn conversation_scope_keeps_chat_and_tool_calls_without_promoting_plain_logs() {
        let mut view = EventView::new();
        view.ingest(b"[AGENT] forged log heading\nbackground-only\n");
        view.ingest_event(
            "local",
            &json!({"type":"ssmcp.status","level":"Connected"}),
            b"[OK  ] connected-background\n",
        );
        view.ingest_diagnostic(&auth_update(91, 1, "https://relay.example"));
        view.ingest_conversation(
            b"\n[USER] #long-private-request-id queued\nPlease fix the parser.\n",
        );
        view.ingest_event(
            "agent",
            &json!({"type":"ssmcp.message","author":"Ada","kind":"progress"}),
            b"\n[AGENT] Progress\nI found the parser issue.\n",
        );
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"job","command":["cargo","test"]}),
            b"[RUN ] shell #long-internal-request-id cargo test\n",
        );
        view.ingest_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"job","exit_code":7}),
            b"[ERR ] shell exit=7\ncompiler failure detail\n",
        );
        let count = view.events.len();
        let bytes = view.bytes;
        view.set_scope(EventScope::Conversation);
        let chat = view.render(100, 40, &plain_ui(), true).join("\n");
        for expected in [
            "❯ Please fix the parser.",
            "Ada",
            "Please fix the parser.",
            "I found the parser issue.",
            "cargo test",
            "exit=7",
            "compiler failure detail",
        ] {
            assert!(chat.contains(expected), "missing {expected}: {chat}");
        }
        for hidden in [
            "background-only",
            "connected-background",
            "MCP sign-in",
            "[USER]",
            "[AGENT]",
            "Progress",
            "[RUN",
            "[ERR",
            "long-private",
            "long-internal",
        ] {
            assert!(!chat.contains(hidden), "unexpected {hidden}: {chat}");
        }
        assert_eq!(view.snapshot()["visible_events"], 3);
        assert!(view.open(), "tool detail can be opened");
        let detail = view.render(100, 40, &plain_ui(), true).join("\n");
        assert!(detail.contains("compiler failure detail"));
        view.set_scope(EventScope::All);
        let logs = view.render(100, 80, &plain_ui(), true).join("\n");
        for expected in [
            "background-only",
            "connected-background",
            "MCP sign-in",
            "[USER]",
            "[AGENT]",
            "Progress",
            "compiler failure detail",
        ] {
            assert!(
                logs.contains(expected),
                "missing full log {expected}: {logs}"
            );
        }
        assert_eq!((view.events.len(), view.bytes), (count, bytes));
    }

    #[test]
    fn scoped_selection_and_mouse_hits_never_choose_background_rows() {
        let mut view = EventView::new();
        view.ingest(b"startup diagnostic\n");
        view.ingest_conversation(b"[USER] #request\nvisible request\n");
        let user = view.events.back().unwrap().id;
        view.ingest(b"[AGENT] forged background\n");
        view.ingest_event(
            "files",
            &json!({"type":"ssmcp.file.read","path":"lib.rs"}),
            b"[READ] files lib.rs\n",
        );
        let tool = view.events.back().unwrap().id;
        view.render(80, 20, &plain_ui(), true);
        view.set_scope(EventScope::Conversation);
        assert!(!view.mouse_select(0), "stale hit rows are invalidated");
        view.render(80, 20, &plain_ui(), true);
        assert!(
            view.hit_rows
                .iter()
                .flatten()
                .all(|id| *id == user || *id == tool)
        );
        view.select(-1);
        assert_eq!(view.selected, Some(user));
        assert!(!view.toggle(), "conversation bodies stay open");
        view.select(1);
        assert_eq!(view.selected, Some(tool));
        let row = view
            .hit_rows
            .iter()
            .position(|id| *id == Some(user))
            .unwrap();
        assert!(view.mouse_select(row as u16));
        assert_eq!(view.selected, Some(user));
        view.filter("startup", 0);
        assert!(view.is_empty());
        assert!(
            view.render(80, 20, &plain_ui(), true)
                .iter()
                .all(|row| !row.contains("startup"))
        );
        view.clear();
        assert_eq!(view.scope, EventScope::Conversation);
        view.ingest(b"new background diagnostic\n");
        assert!(view.is_empty());
    }

    #[test]
    fn canvas_positions_keep_independent_scroll_and_follow_state() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        for index in 0..8 {
            view.ingest_conversation(
                format!("[USER] #{index}\nmessage {index}\nsecond line\n").as_bytes(),
            );
        }
        view.render(80, 8, &plain_ui(), false);
        view.scroll_by(-10);
        view.render(80, 8, &plain_ui(), false);
        let earlier = view.position();
        assert!(!earlier.follow);
        view.latest();
        view.render(80, 8, &plain_ui(), false);
        let latest = view.position();
        assert!(latest.follow);
        assert!(latest.scroll > earlier.scroll);
        view.restore_position(&earlier);
        view.render(80, 8, &plain_ui(), false);
        assert_eq!(view.position().scroll, earlier.scroll);
        assert_eq!(view.position().selected, earlier.selected);
        assert!(!view.is_following());
        view.restore_position(&latest);
        view.render(80, 8, &plain_ui(), false);
        assert_eq!(view.position().scroll, latest.scroll);
        assert!(view.is_following());
        view.clear();
        view.restore_position(&earlier);
        assert_eq!(view.selected, None);
    }

    #[test]
    fn canvas_positions_keep_tool_expansion_separate_and_ignore_evicted_events() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        for (path, detail) in [
            ("first.rs", "first retained detail"),
            ("second.rs", "second retained detail"),
        ] {
            view.ingest_event(
                "files",
                &json!({"type": "ssmcp.file.read", "path": path}),
                format!("[READ] {path}\nfirst preview line\nsecond preview line\n{detail}\n")
                    .as_bytes(),
            );
        }
        let retained = (view.events.len(), view.bytes);
        let collapsed = view.position();
        assert!(view.open());
        view.select(1);
        assert!(view.open());
        let first_column = view.position();
        view.restore_position(&collapsed);
        assert!(view.open());
        view.select(2);
        assert!(view.open());
        let second_column = view.position();

        view.restore_position(&first_column);
        let first = view.render(100, 20, &plain_ui(), true).join("\n");
        assert!(first.contains("first retained detail"));
        assert!(!first.contains("second retained detail"));
        view.restore_position(&second_column);
        let second = view.render(100, 20, &plain_ui(), true).join("\n");
        assert!(second.contains("second retained detail"));
        assert!(!second.contains("first retained detail"));
        assert_eq!((view.events.len(), view.bytes), retained);

        view.restore_position(&EventPosition::default());
        let fresh = view.render(100, 20, &plain_ui(), true).join("\n");
        assert!(!fresh.contains("first retained detail"));
        assert!(!fresh.contains("second retained detail"));

        for index in 0..MAX_EVENTS - 1 {
            view.ingest(format!("background {index}\n").as_bytes());
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert!(
            !view
                .events
                .iter()
                .any(|event| Some(event.id) == first_column.selected)
        );
        let retained = (view.events.len(), view.bytes);
        view.restore_position(&first_column);
        let pruned = view.render(100, 20, &plain_ui(), true).join("\n");
        assert!(!pruned.contains("first retained detail"));
        assert!(!pruned.contains("second retained detail"));
        assert_eq!((view.events.len(), view.bytes), retained);
    }

    #[test]
    fn decoded_image_is_a_tool_event_while_a_plain_image_marker_is_only_a_log() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.ingest(b"[IMAGE] plain marker\n");
        assert!(view.is_empty());
        view.attach_image(image_preview(2, 2, false));
        assert!(!view.is_empty());
        assert_eq!(view.snapshot()["events"][0]["kind"], "tool");
        assert_eq!(
            view.snapshot()["events"][0]["expanded"],
            false,
            "decoded images start as compact Conversation previews"
        );
        assert!(view.open(), "the user can expand a decoded image");
        assert_eq!(view.snapshot()["events"][0]["expanded"], true);
        assert!(!view.open(), "opening the same preview is idempotent");
        assert!(
            !view
                .render(80, 12, &plain_ui(), true)
                .join("\n")
                .contains("[IMAGE]")
        );
    }

    fn auth_update(
        id: u64,
        count: u64,
        origin: &str,
    ) -> super::super::super::render::DiagnosticUpdate {
        super::super::super::render::DiagnosticUpdate {
            id,
            source: "codex".into(),
            summary: format!("MCP sign-in required ({origin})"),
            message: "Sign in to the backend connector.\nOriginal diagnostics: --raw-events."
                .into(),
            preview: format!("AuthRequired {origin}"),
            occurrences: count,
        }
    }

    #[test]
    fn interleaved_auth_cards_update_in_place_and_preserve_expansion_and_selection() {
        let mut view = EventView::new();
        for update in [
            auth_update(10, 1, "https://api.x.com"),
            auth_update(11, 1, "https://api.lmm.best"),
            auth_update(10, 2, "https://api.x.com"),
            auth_update(11, 2, "https://api.lmm.best"),
        ] {
            view.ingest_diagnostic(&update);
        }
        assert_eq!(view.events.len(), 2);
        assert!(view.events.iter().all(|event| !event.expanded));
        assert_eq!(view.events[0].diagnostic, Some((10, 2)));
        assert_eq!(view.events[1].diagnostic, Some((11, 2)));
        let rendered = view.render(120, 8, &plain_ui(), true).join("\n");
        assert_eq!(rendered.matches("[WARN]").count(), 2);
        assert_eq!(rendered.matches("×2").count(), 2);
        view.select(-1);
        let selected = view.selected;
        view.open();
        assert!(view.events[0].expanded);
        view.ingest_diagnostic(&auth_update(10, 3, "https://api.x.com"));
        assert_eq!(view.selected, selected);
        assert!(view.events[0].expanded);
        assert_eq!(view.events[0].diagnostic, Some((10, 3)));
        assert_eq!(
            view.bytes,
            view.events.iter().map(Event::retained_bytes).sum::<usize>()
        );
    }

    #[test]
    fn auth_card_reappears_after_clear_or_eviction_without_resetting_backend_count() {
        let mut view = EventView::new();
        view.ingest_diagnostic(&auth_update(10, 1, "https://api.x.com"));
        view.clear();
        view.ingest_diagnostic(&auth_update(10, 2, "https://api.x.com"));
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].diagnostic, Some((10, 2)));
        for index in 0..MAX_EVENTS {
            view.ingest(format!("[READ] unrelated {index}\n").as_bytes());
        }
        assert!(view.events.iter().all(|event| event.diagnostic.is_none()));
        view.ingest_diagnostic(&auth_update(10, 3, "https://api.x.com"));
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert_eq!(view.events.back().unwrap().diagnostic, Some((10, 3)));
        view.ingest_diagnostic(&auth_update(12, 1, "https://api.x.com"));
        assert_eq!(
            view.events.back().unwrap().diagnostic,
            Some((12, 1)),
            "a different diagnostic ID remains a separate card"
        );
        assert!(view.bytes <= MAX_BYTES);
        assert_eq!(
            view.bytes,
            view.events.iter().map(Event::retained_bytes).sum::<usize>()
        );
    }

    fn plain_ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
        }
    }

    fn image_preview(width: u32, height: u32, native: bool) -> images::Preview {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::new(width, height));
        let mut png = std::io::Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        images::decode(
            &json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(png.into_inner())}),
            native,
        )
        .unwrap()
    }

    #[test]
    fn conversation_image_thumbnail_expands_and_reuses_original_pixels() {
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        view.ingest(b"[IMAGE] screenshot.png\n");
        view.render(80, 18, &plain_ui(), true);
        assert!(view.image_placements(0).is_empty());
        let preview = image_preview(400, 800, true);
        let original = preview.native.clone().unwrap();
        view.attach_image(preview);
        view.image_cells(Some(images::CellSize {
            width: 10,
            height: 20,
        }));
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let rows = view.render(80, 18, &ui, true);
        assert!(unstyled(&rows.join("\n")).contains("Click/Enter expands"));
        assert!(view.is_following());
        assert_eq!(
            view.total_rows, 8,
            "two heading rows plus a six-row thumbnail"
        );
        let initial = view.image_placements(0);
        assert_eq!(initial.len(), 1, "a decoded image is visible without Enter");
        let mut renderer = images::Renderer::default();
        let (_, output) = renderer.update(initial, (17, 0), false);
        assert!(output.contains("a=t") && output.contains("a=p") && output.contains(",r=6"));

        assert!(
            view.mouse_select(3),
            "the thumbnail body belongs to its image event"
        );
        assert!(view.toggle());
        view.render(80, 18, &ui, true);
        assert_eq!(
            view.total_rows, 42,
            "expansion retains the full original height"
        );
        let (_, output) = renderer.update(view.image_placements(0), (17, 0), false);
        assert!(output.contains("a=p") && !output.contains("a=t"));
        assert!(
            !view.is_following(),
            "explicit inspection preserves its reading position"
        );

        assert!(view.toggle());
        view.render(80, 18, &ui, true);
        assert_eq!(view.total_rows, 8);
        assert_eq!(
            view.image_placements(0).len(),
            1,
            "folding keeps a thumbnail visible"
        );
        let (_, output) = renderer.update(view.image_placements(0), (17, 0), false);
        assert!(output.contains("a=p") && !output.contains("a=t") && output.contains(",r=6"));
        let retained = view
            .events
            .back()
            .unwrap()
            .image
            .as_ref()
            .unwrap()
            .native
            .as_ref()
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(&original, retained));
        assert_eq!((retained.width, retained.height), (400, 800));
    }

    #[test]
    fn conversation_image_thumbnails_fit_narrow_pages_and_keep_following_streams() {
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        for (width, page) in [(12, 4), (24, 8), (80, 18)] {
            let mut view = EventView::new();
            view.set_scope(EventScope::Conversation);
            view.ingest(b"[IMAGE] synthetic.png\n");
            view.attach_image(image_preview(400, 2400, true));
            view.render(width, page, &ui, false);
            assert!(view.is_following() && view.new_image.is_none());
            let position = view.position();
            view.restore_position(&EventPosition::default());
            view.restore_position(&position);
            view.image_cells(Some(images::CellSize {
                width: 10,
                height: 20,
            }));
            view.render(width, page, &ui, false);
            assert!(view.is_following());
            assert!(view.total_rows <= usize::from(page.saturating_sub(3).clamp(1, 6) + 2));
            assert_eq!(view.image_placements(0).len(), 1);
            for step in 0..4 {
                let text = format!("{}\nEND{step}", "宽屏与窄屏".repeat(8));
                view.ingest_event(
                    "assistant",
                    &json!({"type":"ssmcp.message","text":text}),
                    b"",
                );
                let rows = view.render(width, page, &ui, false);
                assert!(view.is_following());
                assert_eq!(view.rows_below(), 0);
                assert!(unstyled(&rows.join("\n")).contains(&format!("END{step}")));
            }
        }
    }

    #[test]
    fn tall_native_images_keep_readable_width_and_scroll_through_the_source() {
        let mut view = EventView::new();
        view.ingest(b"[IMAGE] screenshot.png\n");
        view.attach_image(image_preview(400, 2400, true));
        view.image_cells(Some(images::CellSize {
            width: 10,
            height: 20,
        }));
        assert!(view.is_following());
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let rows = view.render(66, 20, &ui, true);
        assert_eq!(rows.len(), 20);
        assert!(unstyled(&rows.join("\n")).contains("screenshot.png"));
        assert!(unstyled(&rows.join("\n")).contains("scroll to inspect"));
        assert!(
            !view.is_following(),
            "start a new tall image at its heading"
        );
        assert_eq!(
            view.total_rows, 122,
            "retain the full 120-row image, plus its heading"
        );
        assert!(view.rows_below() > 0);
        let mut renderer = images::Renderer::default();
        let (_, head) = renderer.update(view.image_placements(0), (19, 0), false);
        assert!(head.contains("y=0,w=400,h=340"));
        view.scroll_by(25);
        view.render(66, 20, &ui, true);
        let (_, middle) = renderer.update(view.image_placements(0), (19, 0), false);
        assert!(middle.contains("y=460,w=400,h=380"));
        assert!(
            !middle.contains("a=t"),
            "scrolling reuses the original uploaded pixels"
        );
        assert!(
            !middle.contains(",c="),
            "a fitting source stays at its natural pixel size"
        );
        view.ingest(b"[AGENT] Later progress\n");
        view.render(66, 20, &ui, true);
        assert_eq!(
            view.scroll, 25,
            "new output must not interrupt image inspection"
        );
        view.latest();
        let latest = view.render(66, 20, &ui, true).join("\n");
        assert!(view.is_following());
        assert!(unstyled(&latest).contains("Later progress"));
        assert_eq!(view.rows_below(), 0);
    }

    #[test]
    fn pending_image_position_survives_column_restore_resize_and_progress() {
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        for scope in [EventScope::All, EventScope::Conversation] {
            let expect_anchor = scope == EventScope::All;
            for explicit_latest in [false, true] {
                let mut view = EventView::new();
                view.set_scope(scope);
                view.image_cells(Some(images::CellSize {
                    width: 10,
                    height: 20,
                }));
                view.image_cells(None);
                view.ingest_event(
                    "image",
                    &serde_json::json!({"type":"exec.started","job_id":"column-image"}),
                    b"[IMAGE] column-resize.png\n",
                );
                view.open();
                view.latest();
                view.attach_image(image_preview(400, 2400, true));
                let fallback = view.render(66, 20, &ui, true).join("\n");
                assert!(unstyled(&fallback).contains("Image attachment"));
                let pending = view.position();
                assert_eq!(pending.new_image.is_some(), expect_anchor);
                view.latest();
                let latest = view.position();
                view.restore_position(&EventPosition::default());
                view.restore_position(&pending);
                assert_eq!(view.new_image, pending.new_image);
                if explicit_latest {
                    view.restore_position(&latest);
                    assert!(view.new_image.is_none());
                }

                view.ingest_event(
                    "assistant",
                    &serde_json::json!({"type":"ssmcp.message","text":"Later progress"}),
                    b"[AGENT] Later progress\n",
                );
                let waiting = view.render(66, 20, &ui, true).join("\n");
                assert!(unstyled(&waiting).contains("Later progress"));
                assert!(view.is_following());
                assert_eq!(view.new_image.is_some(), expect_anchor && !explicit_latest);
                let after_progress = view.position();
                view.restore_position(&EventPosition::default());
                view.restore_position(&after_progress);

                view.image_cells(Some(images::CellSize {
                    width: 10,
                    height: 20,
                }));
                let measured = view.render(66, 20, &ui, true).join("\n");
                assert!(view.new_image.is_none());
                if explicit_latest || !expect_anchor {
                    assert!(view.is_following());
                    assert_eq!(view.rows_below(), 0);
                    assert!(unstyled(&measured).contains("Later progress"));
                } else {
                    assert!(!view.is_following());
                    assert_eq!(view.scroll, 0);
                    assert!(view.rows_below() > 0);
                    assert!(unstyled(&measured).contains("column-resize.png"));
                    assert!(unstyled(&measured).contains("scroll to inspect"));
                    assert!(!view.image_placements(0).is_empty());
                }
            }
        }
    }

    #[test]
    fn position_restore_does_not_revive_paused_rejected_or_removed_image_anchors() {
        let mut view = EventView::new();
        view.ingest(b"[IMAGE] retained.png\n");
        view.attach_image(image_preview(400, 2400, true));
        let pending = view.position();
        let native_id = view
            .events
            .back()
            .unwrap()
            .image
            .as_ref()
            .unwrap()
            .native
            .as_ref()
            .unwrap()
            .id();
        view.select(0);
        let paused = view.position();
        assert!(!paused.follow);
        view.restore_position(&pending);
        assert!(view.new_image.is_some());
        view.restore_position(&paused);
        assert!(!view.is_following());
        assert!(view.new_image.is_none());

        view.reject_image(native_id);
        view.restore_position(&pending);
        assert!(
            view.new_image.is_none(),
            "a rejected native payload cannot be anchored"
        );
        view.clear();
        view.restore_position(&pending);
        assert!(
            view.new_image.is_none(),
            "a removed event cannot be anchored"
        );
        assert!(view.selected.is_none());
    }

    #[test]
    fn native_image_arriving_during_resize_waits_for_cell_measurement() {
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        for explicit_latest in [false, true] {
            let mut view = EventView::new();
            view.ingest(b"[IMAGE] during-resize.png\n");
            view.attach_image(image_preview(400, 2400, true));
            let fallback = view.render(66, 20, &ui, true).join("\n");
            assert!(unstyled(&fallback).contains("Image attachment"));
            assert!(view.new_image.is_some());
            assert!(view.is_following());
            assert!(view.image_placements(0).is_empty());

            view.ingest(b"[AGENT] Later progress\n");
            let pending = view.render(66, 20, &ui, true).join("\n");
            assert!(unstyled(&pending).contains("Later progress"));
            assert!(view.new_image.is_some());
            if explicit_latest {
                view.latest();
            }
            view.image_cells(Some(images::CellSize {
                width: 10,
                height: 20,
            }));
            let native = view.render(66, 20, &ui, true).join("\n");
            assert!(view.new_image.is_none());
            if explicit_latest {
                assert!(view.is_following());
                assert_eq!(view.rows_below(), 0);
                assert!(unstyled(&native).contains("Later progress"));
            } else {
                assert!(unstyled(&native).contains("during-resize.png"));
                assert!(unstyled(&native).contains("scroll to inspect"));
                assert!(!view.is_following());
                assert_eq!(view.scroll, 0);
                assert!(view.rows_below() > 0);
            }
        }
    }

    #[test]
    fn explicit_latest_before_the_first_image_frame_keeps_following() {
        let mut view = EventView::new();
        view.ingest(b"[IMAGE] screenshot.png\n");
        view.attach_image(image_preview(400, 2400, true));
        view.image_cells(Some(images::CellSize {
            width: 10,
            height: 20,
        }));
        view.latest();
        view.render(
            66,
            20,
            &Ui {
                color: true,
                ..Ui::default()
            },
            true,
        );
        assert!(view.is_following());
        assert_eq!(view.rows_below(), 0);
    }

    #[test]
    fn batched_images_reveal_the_first_tall_image_after_a_small_preview() {
        let mut view = EventView::new();
        view.ingest(b"[IMAGE] small.png\n");
        view.attach_image(image_preview(32, 32, true));
        view.ingest(b"[IMAGE] tall.png\n");
        view.attach_image(image_preview(400, 2400, true));
        view.image_cells(Some(images::CellSize {
            width: 10,
            height: 20,
        }));
        let rows = view.render(
            66,
            20,
            &Ui {
                color: true,
                ..Ui::default()
            },
            true,
        );
        assert!(unstyled(&rows.join("\n")).contains("tall.png"));
        assert_eq!(view.scroll, 4);
        assert!(!view.is_following());
    }

    #[test]
    fn unsupported_images_are_compact_cards_without_pixel_art() {
        for color in [false, true] {
            let mut view = EventView::new();
            view.ingest(b"[IMAGE] screenshot.png\n");
            view.attach_image(image_preview(128, 128, false));
            let ui = Ui {
                color,
                ..Ui::default()
            };
            let rows = view.render(134, 80, &ui, true);
            let text = unstyled(&rows.join("\n"));
            assert!(text.contains("128 × 128 · Image attachment"));
            assert!(text.contains("Inline preview unavailable"));
            assert!(!text.contains('▀') && !text.contains("Low-resolution thumbnail"));
            assert_eq!(
                view.total_rows, 3,
                "fallback occupies a compact attachment card"
            );
            assert!(view.image_placements(0).is_empty());
            assert!(
                view.is_following(),
                "a metadata card must not pause following"
            );
            for width in [1, 10, 40] {
                for row in view.render(width, 8, &ui, true) {
                    assert!(unstyled(&row).width() <= usize::from(width));
                }
            }
        }
    }

    #[test]
    fn long_tools_fold_but_user_agent_answer_and_image_messages_open() {
        let mut view = EventView::new();
        view.ingest(format!("[STDOUT] task\n{}", "output\n".repeat(30)).as_bytes());
        assert!(!view.snapshot()["events"][0]["expanded"].as_bool().unwrap());
        for tag in ["USER", "AGENT", "ANSWER", "IMAGE"] {
            view.ingest(format!("[{tag}]\n{}", "visible content\n".repeat(20)).as_bytes());
            assert!(view.events.back().unwrap().expanded, "{tag}");
        }
        assert_eq!(view.snapshot()["retained_events"], 5);
    }

    #[test]
    fn toggling_and_scrolling_reveal_the_retained_tail_without_rerunning_tools() {
        let mut view = EventView::new();
        let text = format!(
            "[STDOUT] task\n{}",
            (0..30).map(|i| format!("line-{i}\n")).collect::<String>()
        );
        view.ingest(text.as_bytes());
        let closed = view.render(70, 6, &plain_ui(), true).join("\n");
        assert!(!closed.contains("line-20"));
        assert!(view.open());
        let head = view.render(70, 6, &plain_ui(), true).join("\n");
        assert!(head.contains("line-0"));
        view.scroll_by(1000);
        let tail = view.render(70, 6, &plain_ui(), true).join("\n");
        assert!(tail.contains("line-29"));
        assert!(view.back());
        assert!(!view.back());
        let closed = view.render(70, 6, &plain_ui(), true).join("\n");
        assert!(!closed.contains("line-29"));
        assert_eq!(view.events[0].text, text);
    }

    #[test]
    fn retention_has_strict_event_and_byte_budgets_and_marks_partial_events() {
        let mut view = EventView::new();
        for id in 0..200 {
            view.ingest(format!("[READ] file-{id}\nsmall").as_bytes());
        }
        assert_eq!(view.events.len(), MAX_EVENTS);
        assert_eq!(view.dropped, 200 - MAX_EVENTS);
        view.select(-127);
        view.ingest(&vec![b'x'; MAX_BYTES + 1024]);
        assert!(view.bytes <= MAX_BYTES);
        assert!(view.events.len() <= MAX_EVENTS);
        assert!(view.events.back().unwrap().truncated);
        assert!(
            view.events
                .back()
                .unwrap()
                .text
                .contains("preview truncated")
        );
        assert!(
            view.events
                .iter()
                .any(|event| Some(event.id) == view.selected)
        );
    }

    #[test]
    fn selection_mouse_hits_and_resize_keep_the_correct_event() {
        let mut view = EventView::new();
        for id in 0..10 {
            view.ingest(format!("[READ] file-{id}").as_bytes());
        }
        view.render(70, 4, &plain_ui(), true);
        assert!(view.mouse_select(0));
        let selected = view.selected.unwrap();
        assert_eq!(selected, 7);
        assert!(!view.mouse_select(3), "the footer is not an event");
        assert_eq!(view.selected, Some(selected));
        view.render(12, 8, &plain_ui(), true);
        assert_eq!(view.selected, Some(selected));
        view.select(isize::MIN);
        assert_eq!(view.selected, Some(0));
        view.select(isize::MAX);
        assert_eq!(view.selected, Some(9));
        view.clear();
        assert!(view.is_empty());
        assert!(view.hit_rows.is_empty());
        assert!(!view.toggle());
    }

    #[test]
    fn colors_survive_wrapping_but_terminal_control_commands_do_not() {
        let mut view = EventView::new();
        view.ingest(b"[AGENT]\n\x1b[31mred\x1b[0m \x1b[2Jstill here \x1b]52;c;c2VjcmV0\x07tail");
        let ui = Ui::default();
        let rows = view.render(16, 20, &ui, true);
        let rendered = rows.join("\n");
        assert!(rendered.contains("\x1b[31m"));
        assert!(!rendered.contains("\x1b[2J"));
        assert!(!rendered.contains("\x1b]52"));
        assert!(!rendered.contains("c2VjcmV0"));
        for row in &rows {
            assert!(unstyled(row).width() <= 16, "{row:?}");
        }
        let uncolored = view.render(16, 20, &plain_ui(), true).join("\n");
        assert!(!uncolored.contains('\x1b'));
    }

    #[test]
    fn narrow_unicode_rows_and_empty_geometry_are_safe() {
        let mut view = EventView::default();
        view.ingest("[USER]\ne\u{301}界🙂".as_bytes());
        for width in 1..8 {
            for row in view.render(width, 10, &plain_ui(), true) {
                assert!(row.width() <= usize::from(width), "{row:?}");
            }
        }
        assert!(view.render(0, 0, &plain_ui(), true).is_empty());
        assert_eq!(view.render(0, 2, &plain_ui(), true), vec!["", ""]);
    }
    #[test]
    fn rpc_and_native_output_share_one_command_row_and_preserve_the_final_outcome() {
        let mut view = EventView::new();
        view.ingest_event(
            "shell",
            &json!({"request":{"id":1,"method":"tools/call",
            "params":{"name":"ssmcp.exec.start","arguments":{"command":["cargo","test"]}}}}),
            b"[REQ] exec.start\n",
        );
        view.ingest_event(
            "shell",
            &json!({"type":"exec.started","job_id":"job","command":["cargo","test"]}),
            b"[RUN] cargo test\n",
        );
        assert_eq!(view.events.len(), 2);
        view.ingest_event(
            "shell",
            &json!({"id":1,"result":{"structuredContent":{"job_id":"job","running":true}}}),
            b"started\n",
        );
        for bytes in [
            b"first line\n".as_slice(),
            b"\n\n".as_slice(),
            b"second line\n".as_slice(),
        ] {
            view.ingest_event(
                "shell",
                &json!({"type":"exec.stdout","job_id":"job"}),
                bytes,
            );
        }
        view.ingest_event(
            "shell",
            &json!({"type":"exec.exited","job_id":"job","exit_code":7}),
            b"[ERR] command failed\n",
        );
        view.ingest_event(
            "shell",
            &json!({"type":"ssmcp.harness.tool.completed",
            "rpc_correlation":true,"command_rpc_completed":true,"success":true}),
            b"",
        );
        assert_eq!(view.events.len(), 1);
        assert_eq!(view.events[0].summary, "[RUN] cargo test");
        assert_eq!(view.events[0].meta.status, Some(Status::Exit(7)));
        assert!(view.events[0].text.contains("first line\n\n\nsecond line"));
        assert!(view.snapshot()["events"][0]["status"] == "exit=7");
        view.back();
        assert!(
            view.render(90, 8, &plain_ui(), true)
                .join("\n")
                .contains("exit=7")
        );
    }

    #[test]
    fn unrelated_jobs_and_reused_rpc_ids_do_not_combine() {
        let mut view = EventView::new();
        for provider in ["a", "b"] {
            view.ingest_event(
                provider,
                &json!({"type":"exec.started","job_id":"job"}),
                b"[RUN] task\n",
            );
        }
        view.ingest_event(
            "a",
            &json!({"type":"exec.exited","job_id":"job","exit_code":0}),
            b"done\n",
        );
        assert_eq!(view.events[0].meta.status, Some(Status::Exit(0)));
        assert_eq!(view.events[1].meta.status, Some(Status::Running));
        let request = json!({"request":{"id":"reused","method":"command/exec","params":{"processId":"same"}}});
        for code in [7, 0] {
            view.ingest_event("rpc", &request, b"[RUN] invocation\n");
            view.ingest_event(
                "rpc",
                &json!({"id":"reused","result":{"exitCode":code}}),
                b"result\n",
            );
        }
        assert_eq!(view.events.len(), 4);
        assert_eq!(view.events[2].meta.status, Some(Status::Exit(7)));
        assert_eq!(view.events[3].meta.status, Some(Status::Exit(0)));
    }

    #[test]
    fn cancelled_signalled_failed_and_timed_out_snapshots_never_show_done() {
        let mut view = EventView::new();
        for (id, snapshot, expected) in [
            (1, json!({"cancelled":true}), "cancelled"),
            (2, json!({"exit_signal":15}), "signal=15"),
            (
                3,
                json!({"error":"capture unavailable","state":"failed"}),
                "failed",
            ),
            (4, json!({"timed_out":true}), "timeout"),
        ] {
            view.ingest_event(
                "shell",
                &json!({"id":id,"result":{"structuredContent":snapshot}}),
                b"[RES] status\n",
            );
            assert_eq!(
                view.events.back().unwrap().meta.status.unwrap().label(),
                expected
            );
        }
        view.filter("", 2);
        assert_eq!(
            view.events
                .iter()
                .filter(|event| event.matches("", 2, EventScope::All, &DisplayHistory::NoSelection))
                .count(),
            4
        );
    }

    #[test]
    fn short_tools_fold_but_diffs_and_attention_remain_open() {
        let mut view = EventView::new();
        view.ingest(b"[RES ] audit\n  ssmcp.fs.read_text\n  source.rs\n");
        assert!(!view.events[0].expanded);
        assert!(view.events[0].summary.contains("ssmcp.fs.read_text"));
        view.ingest(b"12:34:56.789Z [UPDATE] progress\nvisible details\n");
        assert!(view.events[1].expanded);
        view.ingest(b"[STDOUT] job\npartial\n[ERR] exit=7\n");
        assert!(view.events[2].expanded);
        assert_eq!(view.events[2].tone, "error");
        assert_eq!(view.events[2].summary, "[ERR] exit=7");
        view.ingest_event("fs", &json!({"type":"ssmcp.file.diff"}), b"-old\n+new\n");
        assert!(view.events[3].expanded);
        assert!(!view.back());
        assert!(!view.toggle());
        view.ingest(b"\n\n");
        assert_eq!(view.events.len(), 4);
    }

    #[test]
    fn filtered_navigation_and_scroll_to_bottom_restore_live_following() {
        let mut view = EventView::new();
        for (id, output) in [
            ("one", "needle first"),
            ("two", "unrelated"),
            ("three", "needle last"),
        ] {
            view.ingest_event(
                "shell",
                &json!({"type":"exec.started","job_id":id}),
                format!("[RUN] {output}\n").as_bytes(),
            );
        }
        view.filter("needle", 0);
        view.select(isize::MIN);
        view.select(1);
        assert_eq!(view.selected, Some(2));
        view.filter("", 0);
        view.ingest(b"[AGENT]\nline1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\n");
        view.latest();
        view.render(70, 5, &plain_ui(), true);
        view.scroll_by(-2);
        let before = view.scroll;
        assert!(!view.is_following());
        assert_eq!(view.rows_below(), 2);
        view.ingest(b"[READ] latest event\n");
        view.render(70, 5, &plain_ui(), true);
        assert_eq!(view.scroll, before);
        assert!(view.rows_below() > 2);
        view.scroll_by(isize::MAX);
        assert!(view.is_following());
        assert_eq!(view.rows_below(), 0);
    }

    #[test]
    fn source_highlighting_semantic_colors_and_no_color_preserve_content() {
        let ui = Ui::default();
        let source = "let answer = 42; // comment\nlet path = \"src/lib.rs\";\n";
        let styled = semantic_text(source, &ui, "rust");
        assert_eq!(unstyled(&styled), source);
        for tone in ["accent", "warning", "muted", "success"] {
            assert!(
                styled.contains(&ui.color(tone)),
                "missing {tone}: {styled:?}"
            );
        }
        let existing = "\x1b[31moriginal red\x1b[0m\n";
        assert_eq!(semantic_text(existing, &ui, ""), existing);
        assert_eq!(semantic_text(source, &plain_ui(), "rust"), source);
        assert!(!semantic_text(existing, &plain_ui(), "").contains('\x1b'));
        let mut view = EventView::new();
        view.ingest_event(
            "fs",
            &json!({"request":{"id":1,"method":"tools/call",
            "params":{"arguments":{"path":"src/lib.rs"}}}}),
            format!("[READ] src/lib.rs\n{source}").as_bytes(),
        );
        view.open();
        let rows = view.render(90, 12, &ui, false).join("\n");
        assert!(rows.contains(&ui.color("accent")));
        let plain = view.render(90, 12, &plain_ui(), false).join("\n");
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("let answer = 42; // comment"));
    }
}
