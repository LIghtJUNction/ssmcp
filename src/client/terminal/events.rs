//! Bounded local event previews. Expanding reveals retained display text, not a
//! reread of tools or an unbounded replacement for their original output archive.
use super::{super::ui::Ui, images};
use serde_json::{Value, json};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
mod correlation;
use correlation::{Meta, Status};

const MAX_EVENTS: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_DETAIL: usize = 128 * 1024;

struct Event {
    id: u64,
    text: String,
    summary: String,
    lines: usize,
    expanded: bool,
    truncated: bool,
    tone: &'static str,
    cache: Option<(usize, String, Vec<String>)>,
    meta: Meta,
    image: Option<images::Preview>,
    diagnostic: Option<(u64, u64)>,
}

impl Event {
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
        self.text.len() + self.summary.len() + self.meta.bytes()
    }

    fn matches(&self, query: &str, level: u8) -> bool {
        (level != 1 || self.meta.command || self.summary.starts_with("[RUN"))
            && (level != 2 || self.meta.status.is_some_and(Status::failed) || self.tone == "error")
            && (query.is_empty()
                || self.summary.to_lowercase().contains(query)
                || self.text.to_lowercase().contains(query))
    }

    fn summary_with_status(&self) -> String {
        match self.meta.status {
            Some(status) => format!("{} [{}]", self.summary, status.label()),
            None => self.summary.clone(),
        }
    }
}

#[derive(Default)]
pub(crate) struct EventView {
    events: VecDeque<Event>,
    bytes: usize,
    next_id: u64,
    selected: Option<u64>,
    scroll: usize,
    follow: bool,
    reveal: bool,
    dropped: usize,
    hit_rows: Vec<Option<u64>>,
    total_rows: usize,
    page_rows: usize,
    image_cells: Option<images::CellSize>,
    placements: Vec<images::Placement>,
    new_image: Option<u64>,
    query: String,
    level: u8,
}

impl EventView {
    pub(crate) fn new() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub(crate) fn ingest(&mut self, bytes: &[u8]) {
        self.ingest_inner(bytes, Meta::default());
    }

    /// Keep one bounded warning card per renderer diagnostic. Repeated updates
    /// preserve its position, expansion and the reader's current viewport.
    pub(crate) fn ingest_diagnostic(
        &mut self,
        update: &super::super::render::DiagnosticUpdate,
    ) -> bool {
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
            false
        } else {
            self.ingest(text.as_bytes());
            if let Some(event) = self.events.back_mut() {
                event.summary = super::super::panel::fit(&update.heading(), 200);
                event.expanded = false;
                event.truncated |= truncated;
                event.tone = "warning";
                event.diagnostic = Some((update.id, update.occurrences));
            }
            true
        };
        self.prune();
        if !self
            .events
            .iter()
            .any(|event| Some(event.id) == self.selected)
        {
            self.selected = self
                .events
                .iter()
                .find(|event| event.matches(&self.query, self.level))
                .map(|event| event.id);
            self.reveal = true;
        }
        inserted
    }

    /// Correlate only registered event identities. Text is never an identity.
    pub(crate) fn ingest_event(&mut self, label: &str, event: &Value, bytes: &[u8]) {
        self.ingest_inner(bytes, Meta::from_event(label, event));
    }

    fn ingest_inner(&mut self, bytes: &[u8], mut meta: Meta) {
        if meta.skip {
            return;
        }
        let mut matches = Vec::new();
        for key in &meta.keys {
            if let Some((index, _)) = self.events.iter().enumerate().rev().find(|(_, event)| {
                event.meta.keys.contains(key)
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
        let first = first.unwrap_or("");
        let conversation = [
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
                    self.events[index].append(previous.text, previous.truncated);
                    self.events[index].meta.merge(previous.meta);
                }
            }
            let event = &mut self.events[index];
            event.append(text, truncated);
            event.meta.merge(meta);
            event.expanded |= expanded;
            event.cache = None;
        } else {
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let summary = event_summary(
                &plain,
                if conversation {
                    first
                } else {
                    attention.unwrap_or(first)
                },
            );
            let lines = plain.lines().count().max(1);
            self.events.push_back(Event {
                id,
                text,
                summary,
                lines,
                expanded,
                truncated,
                tone,
                meta,
                cache: None,
                image: None,
                diagnostic: None,
            });
        }
        self.prune();
        if follow {
            self.selected = self
                .events
                .iter()
                .rev()
                .find(|event| event.matches(&self.query, self.level))
                .map(|event| event.id);
            self.follow = true;
        } else if !self
            .events
            .iter()
            .any(|event| Some(event.id) == self.selected)
        {
            self.selected = self
                .events
                .iter()
                .find(|event| event.matches(&self.query, self.level))
                .map(|event| event.id);
            self.reveal = true;
        }
        // Updating a correlated row does not move a reader away from their viewport.
    }

    fn prune(&mut self) {
        self.bytes = self.events.iter().map(Event::retained_bytes).sum();
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some(event) = self.events.pop_front() {
                self.bytes -= event.retained_bytes();
                self.dropped = self.dropped.saturating_add(1);
            }
        }
    }

    pub(crate) fn filter(&mut self, query: &str, level: u8) {
        let query = query.to_lowercase();
        if self.query == query && self.level == level {
            return;
        }
        self.query = query;
        self.level = level;
        self.new_image = None;
        if !self
            .events
            .iter()
            .any(|event| Some(event.id) == self.selected && event.matches(&self.query, self.level))
        {
            self.selected = self
                .events
                .iter()
                .find(|event| event.matches(&self.query, self.level))
                .map(|event| event.id);
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

    /// Attach only a decoded image from the native result path. Text containing
    /// "[IMAGE]" cannot manufacture a protocol placement.
    pub(crate) fn attach_image(&mut self, preview: images::Preview) {
        if let Some(event) = self.events.back_mut() {
            event.image = Some(preview);
            if self.follow {
                self.new_image.get_or_insert(event.id);
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
        let visible = self
            .events
            .iter()
            .filter(|event| event.matches(&self.query, self.level))
            .map(|event| event.id)
            .collect::<Vec<_>>();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|id| Some(*id) == self.selected)
            .unwrap_or(visible.len() - 1);
        let next = current.saturating_add_signed(delta).min(visible.len() - 1);
        self.selected = Some(visible[next]);
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

    pub(crate) fn latest(&mut self) {
        self.new_image = None;
        self.follow = true;
        self.reveal = false;
        self.selected = self
            .events
            .iter()
            .rev()
            .find(|event| event.matches(&self.query, self.level))
            .map(|event| event.id);
    }

    pub(crate) fn toggle(&mut self) -> bool {
        self.new_image = None;
        if let Some(event) = self
            .events
            .iter_mut()
            .find(|event| Some(event.id) == self.selected)
        {
            if event.meta.diff {
                return false;
            }
            event.expanded = !event.expanded;
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
        let Some(event) = self
            .events
            .iter_mut()
            .find(|event| Some(event.id) == self.selected)
        else {
            return false;
        };
        if event.meta.diff && !expanded {
            return false;
        }
        let changed = event.expanded != expanded;
        event.expanded = expanded;
        self.follow = false;
        self.reveal = true;
        changed
    }

    /// Row is relative to the event pane from the most recent render.
    pub(crate) fn mouse_select(&mut self, row: u16) -> bool {
        let Some(id) = self.hit_rows.get(usize::from(row)).copied().flatten() else {
            return false;
        };
        if !self.events.iter().any(|event| event.id == id) {
            return false;
        }
        self.selected = Some(id);
        self.new_image = None;
        self.follow = false;
        // Preserve the clicked row, even when it belongs to a long body.
        self.reveal = false;
        true
    }

    pub(crate) fn clear(&mut self) {
        self.events.clear();
        self.bytes = 0;
        self.selected = None;
        self.scroll = 0;
        self.follow = true;
        self.reveal = false;
        self.dropped = 0;
        self.hit_rows.clear();
        self.total_rows = 0;
        self.page_rows = 0;
        self.placements.clear();
        self.new_image = None;
        self.query.clear();
        self.level = 0;
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "schema_version":1, "events":self.events.iter().map(|event| json!({
                "id":event.id,"summary":event.summary,"lines":event.lines,
                "bytes":event.text.len(),"expanded":event.expanded,"truncated":event.truncated,
                "selected":Some(event.id)==self.selected,
                "status":event.meta.status.map(Status::label),"command":event.meta.command,
                "diagnostic_occurrences":event.diagnostic.map(|(_, count)| count)
            })).collect::<Vec<_>>(),
            "retained_events":self.events.len(),"retained_bytes":self.bytes,
            "max_events":MAX_EVENTS,"max_bytes":MAX_BYTES,"dropped_events":self.dropped,
            "selected":self.selected,"scroll":self.scroll,
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
        self.hit_rows.clear();
        self.placements.clear();
        if width == 0 || height == 0 {
            self.page_rows = 0;
            return vec![String::new(); usize::from(height)];
        }
        let width = usize::from(width);
        let height = usize::from(height);
        let footer = usize::from(height > 1);
        let page = height - footer;
        let indent = 4.min(width.saturating_sub(1));
        let body_width = width.saturating_sub(indent).max(1);
        let mut rows: Vec<(u64, String)> = Vec::new();
        let mut selected_row = None;
        let mut image_spans = Vec::new();
        let new_image = self.new_image.take();
        let mut new_images = false;
        let mut image_anchor = None;
        let palette = palette_key(ui);
        for event in &mut self.events {
            new_images |= new_image == Some(event.id);
            if !event.matches(&self.query, self.level) {
                continue;
            }
            let selected = Some(event.id) == self.selected;
            if selected {
                selected_row = Some(rows.len());
            }
            let marker = if event.expanded { "▾" } else { "▸" };
            let prefix = format!("{} {marker} ", if selected && focused { "›" } else { " " });
            let prefix = clip_plain(&prefix, indent);
            let tone = if selected && focused {
                "accent"
            } else {
                event.meta.status.map_or(event.tone, Status::tone)
            };
            if event.expanded || !self.query.is_empty() {
                if let Some(image) = &event.image {
                    let image_start = rows.len();
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(
                                &clip_plain(&event.summary_with_status(), body_width),
                                tone,
                                ui,
                                false
                            )
                        ),
                    ));
                    let inset = 2.min(body_width.saturating_sub(1));
                    let image_width = body_width.saturating_sub(inset).max(1) as u16;
                    let native = self.image_cells.filter(|_| ui.color).and_then(|cell| {
                        let native = image.native.as_ref()?;
                        Some((native.clone(), native.layout_width(image_width, cell)?))
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
                            if usize::from(area.rows) > page.saturating_sub(2) {
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
                            && image_anchor.is_none()
                            && usize::from(area.rows) > page.saturating_sub(2)
                        {
                            image_anchor = Some((event.id, image_start));
                        }
                        image_spans.push((rows.len(), (indent + inset) as u16, native, area));
                        rows.extend((0..area.rows).map(|_| (event.id, String::new())));
                    } else {
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
                let correlated = !event.meta.keys.is_empty();
                if correlated {
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(
                                &clip_plain(&event.summary_with_status(), body_width),
                                tone,
                                ui,
                                false
                            )
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
                    event.cache = Some((
                        body_width,
                        palette.clone(),
                        styled_rows(
                            &semantic_text(
                                if correlated {
                                    event.text.split_once('\n').map_or("", |(_, body)| body)
                                } else {
                                    &event.text
                                },
                                ui,
                                event.meta.language,
                            ),
                            body_width,
                            ui.color,
                        ),
                    ));
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
                                "    Preview truncated; inspect the original output archive.",
                                width,
                            ),
                            "warning",
                            ui,
                            false,
                        ),
                    ));
                }
            } else {
                let label = format!(
                    "{} · {} lines{}",
                    event.summary_with_status(),
                    event.lines,
                    if event.truncated { " · partial" } else { "" }
                );
                let label = clip_plain(&label, body_width);
                rows.push((
                    event.id,
                    format!(
                        "{}{}",
                        paint(&prefix, tone, ui, selected && focused),
                        paint(&label, tone, ui, selected && focused)
                    ),
                ));
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
            let hint = if self.events.is_empty() {
                "No events yet.".to_owned()
            } else if width < 60 {
                "Enter fold/open · retained preview only".to_owned()
            } else if focused {
                format!(
                    "{} events · ↑↓ select · Enter fold/open · full output: archive",
                    self.events.len()
                )
            } else {
                format!(
                    "{} events · focus Activity to fold/open · full output: archive",
                    self.events.len()
                )
            };
            result.push(paint(&clip_plain(&hint, width), "muted", ui, false));
            self.hit_rows.push(None);
        }
        result
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
    let raw = String::from_utf8_lossy(bytes);
    let clean = sanitize(&raw, true);
    if clean.len() <= MAX_DETAIL {
        return (clean, false);
    }
    let note = "\n… preview truncated; inspect the original output archive …\n";
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
fn styled_rows(text: &str, width: usize, color: bool) -> Vec<String> {
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
                .filter(|event| event.matches("", 2))
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
