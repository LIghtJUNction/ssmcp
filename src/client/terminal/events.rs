//! Bounded local event previews. Expanding reveals retained display text, not a
//! reread of tools or an unbounded replacement for their original output archive.
use super::{super::ui::Ui, images};
use serde_json::{Value, json};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod activity;

const MAX_TOOL_BYTES: usize = 8 * 1024;
const MAX_EVENTS: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const LONG_LINES: usize = 6;
const LONG_BYTES: usize = 640;

struct Event {
    id: u64,
    text: String,
    summary: String,
    lines: usize,
    expanded: bool,
    truncated: bool,
    tone: &'static str,
    cache: Option<(usize, bool, Vec<String>)>,
    image: Option<images::Preview>,
    activity: Option<activity::Activity>,
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
        if bytes.is_empty() {
            return;
        }
        let (text, truncated) = retained_text(bytes);
        let plain = unstyled(&text);
        let Some(first) = plain.lines().find(|line| !line.trim().is_empty()) else {
            return;
        };
        let first = first.trim();
        let conversation = ["[USER]", "[AGENT]", "[ANSWER]", "[IMAGE]"]
            .iter()
            .any(|prefix| first.starts_with(prefix));
        let tone = if first.starts_with("[ERR") {
            "error"
        } else if first.starts_with("[WARN") || first.starts_with("[STDERR") {
            "warning"
        } else if first.starts_with("[OK") {
            "success"
        } else if conversation || first.starts_with("[RUN") || first.starts_with("[REQ") {
            "accent"
        } else {
            "foreground"
        };
        let summary = super::super::panel::fit(first, 200);
        let lines = plain.lines().count().max(1);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let at_end = self.events.is_empty() || self.follow;
        self.bytes += text.len();
        self.events.push_back(Event {
            id,
            text,
            summary,
            lines,
            expanded: conversation || (lines <= LONG_LINES && plain.len() <= LONG_BYTES),
            truncated,
            tone,
            cache: None,
            image: None,
            activity: None,
        });
        self.prune();
        if at_end {
            self.selected = Some(id);
            self.follow = true;
        } else if !self
            .events
            .iter()
            .any(|event| Some(event.id) == self.selected)
        {
            self.selected = self.events.front().map(|event| event.id);
            self.reveal = true;
        }
    }

    /// Fold tool activity in this same bounded store; notices/images still append.
    pub(crate) fn ingest_event(&mut self, label: &str, value: &Value, bytes: &[u8]) {
        if activity::duplicate_correlation(value) {
            return;
        }
        let (text, truncated) = retained_text_limit(bytes, MAX_TOOL_BYTES);
        let plain = unstyled(&text);
        let first = plain
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        if ["[USER]", "[AGENT]", "[ANSWER]", "[IMAGE]"]
            .iter()
            .any(|p| first.starts_with(p))
        {
            self.ingest(bytes);
            return;
        }
        let next = activity::metadata(label, value);
        let old = self.events.iter().rposition(|event| {
            event.activity.as_ref().is_some_and(|old| {
                old.matches(&next) && (!next.started || old.status == activity::Status::Running)
            })
        });
        if let Some(index) = old {
            let event = &mut self.events[index];
            self.bytes -= event.text.len();
            let mut combined = std::mem::take(&mut event.text);
            combined.push_str(&text);
            let (text, trimmed) = retained_text_limit(combined.as_bytes(), MAX_TOOL_BYTES);
            event.text = text;
            event.lines = unstyled(&event.text).lines().count().max(1);
            event.truncated |= truncated || trimmed;
            event.cache = None;
            let metadata = event.activity.as_mut().unwrap();
            metadata.update(next);
            event.expanded |= metadata.diff;
            event.tone = activity_tone(metadata, event.tone);
            self.bytes += event.text.len();
            self.prune();
        } else if !first.is_empty() {
            let previous = self.next_id;
            self.ingest(text.as_bytes());
            if self.next_id == previous {
                return;
            }
            if let Some(event) = self.events.back_mut() {
                event.expanded = next.diff;
                event.truncated |= truncated;
                event.tone = activity_tone(&next, event.tone);
                event.activity = Some(next);
            }
        }
    }

    fn prune(&mut self) {
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some(event) = self.events.pop_front() {
                self.bytes -= event.text.len();
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        if self.selected.is_some()
            && !self
                .events
                .iter()
                .any(|event| Some(event.id) == self.selected)
        {
            self.selected = self.events.front().map(|event| event.id);
            self.reveal = !self.follow;
        }
    }

    /// Attach only a decoded image from the native result path. Text containing
    /// "[IMAGE]" cannot manufacture a protocol placement.
    pub(crate) fn attach_image(&mut self, preview: images::Preview) {
        if let Some(event) = self.events.back_mut() {
            event.image = Some(preview);
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
        if self.events.is_empty() {
            return;
        }
        let current = self
            .events
            .iter()
            .position(|event| Some(event.id) == self.selected)
            .unwrap_or(self.events.len() - 1);
        let next = current
            .saturating_add_signed(delta)
            .min(self.events.len() - 1);
        self.selected = Some(self.events[next].id);
        self.follow = false;
        self.reveal = true;
    }

    /// Scroll within expanded events without changing the selected event.
    pub(crate) fn scroll_by(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        self.follow = false;
        self.reveal = false;
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.total_rows.saturating_sub(self.page_rows));
        if delta > 0 && self.scroll == self.total_rows.saturating_sub(self.page_rows) {
            self.latest();
        }
    }

    pub(crate) fn latest(&mut self) {
        self.follow = true;
        self.reveal = false;
        self.selected = self.events.back().map(|event| event.id);
    }

    pub(crate) fn toggle(&mut self) -> bool {
        if let Some(event) = self
            .events
            .iter_mut()
            .find(|event| Some(event.id) == self.selected)
        {
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
        let Some(event) = self
            .events
            .iter_mut()
            .find(|event| Some(event.id) == self.selected)
        else {
            return false;
        };
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
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "schema_version":1, "events":self.events.iter().map(|event| json!({
                "id":event.id,"summary":event.summary,"lines":event.lines,
                "bytes":event.text.len(),"expanded":event.expanded,"truncated":event.truncated,
                "selected":Some(event.id)==self.selected,
                "tool_status":event.activity.as_ref().map(|value| value.status.label())
            })).collect::<Vec<_>>(),
            "retained_events":self.events.len(),"retained_bytes":self.bytes,
            "max_events":MAX_EVENTS,"max_bytes":MAX_BYTES,"dropped_events":self.dropped,
            "selected":self.selected,"scroll":self.scroll,"follow":self.follow,
            "max_tool_bytes":MAX_TOOL_BYTES,
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
        for event in &mut self.events {
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
                event.tone
            };
            if event.expanded {
                if let Some(image) = &event.image {
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(&clip_plain(&event.summary, body_width), tone, ui, false)
                        ),
                    ));
                    let inset = 2.min(body_width.saturating_sub(1));
                    let image_width = body_width.saturating_sub(inset).max(1) as u16;
                    let image_height = page.saturating_sub(2).clamp(1, 18) as u16;
                    let native = self.image_cells.filter(|_| ui.color).and_then(|cell| {
                        let native = image.native.as_ref()?;
                        Some((
                            native.clone(),
                            native.layout(image_width, image_height, cell)?,
                        ))
                    });
                    if let Some((native, area)) = native {
                        image_spans.push((rows.len(), (indent + inset) as u16, native, area));
                        rows.extend((0..area.rows).map(|_| (event.id, String::new())));
                    } else {
                        for row in
                            image.render(usize::from(image_width), usize::from(image_height), ui)
                        {
                            rows.push((event.id, format!("{}{row}", " ".repeat(indent + inset))));
                        }
                    }
                    continue;
                }
                if event.cache.as_ref().is_none_or(|(cached_width, color, _)| {
                    *cached_width != body_width || *color != ui.color
                }) {
                    event.cache = Some((
                        body_width,
                        ui.color,
                        styled_rows(&event.text, body_width, ui.color),
                    ));
                }
                let content = &event.cache.as_ref().unwrap().2;
                if event.activity.is_some() {
                    rows.push((
                        event.id,
                        format!(
                            "{}{}",
                            paint(&prefix, tone, ui, selected && focused),
                            paint(
                                &clip_plain(&event_heading(event), body_width),
                                tone,
                                ui,
                                false
                            )
                        ),
                    ));
                }
                for (index, row) in content.iter().enumerate() {
                    let prefix = if index == 0 && event.activity.is_none() {
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
                    event_heading(event),
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
        if self.follow {
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

fn activity_tone(value: &activity::Activity, fallback: &'static str) -> &'static str {
    if value.failed {
        "error"
    } else {
        match value.status {
            activity::Status::Running => "accent",
            activity::Status::Done | activity::Status::Exit(0) => "success",
            _ => fallback,
        }
    }
}

fn event_heading(event: &Event) -> String {
    let status = event
        .activity
        .as_ref()
        .map(|value| value.status.label())
        .unwrap_or_default();
    if status.is_empty() {
        event.summary.clone()
    } else {
        format!("{} · {status}", event.summary)
    }
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
    retained_text_limit(bytes, MAX_BYTES)
}

fn retained_text_limit(bytes: &[u8], limit: usize) -> (String, bool) {
    let raw = String::from_utf8_lossy(bytes);
    let clean = sanitize(&raw, true);
    if clean.len() <= limit {
        return (clean, false);
    }
    let note = "\n… preview truncated; inspect the original output archive …\n";
    let budget = limit - note.len();
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

    fn plain_ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
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
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use std::io::Cursor;

    fn image_event(view: &mut EventView) {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            600,
            300,
            image::Rgba([30, 120, 210, 255]),
        ));
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let block = json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(encoded.into_inner())});
        let preview = images::decode(&block, true).unwrap();
        view.ingest(b"[IMAGE] 600 x 300\n");
        view.attach_image(preview);
        view.image_cells(Some(images::CellSize {
            width: 8,
            height: 16,
        }));
    }

    #[test]
    fn native_images_follow_visible_event_crops_and_clear_on_fold() {
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        let mut view = EventView::new();
        image_event(&mut view);
        for index in 0..3 {
            view.ingest(format!("after {index}\n").as_bytes());
        }
        view.render(28, 8, &ui, false);
        let placements = view.image_placements(3);
        assert_eq!(placements.len(), 1);
        let mut renderer = images::Renderer::default();
        let (_, wire) = renderer.update(placements, (20, 0), false);
        let placement = wire
            .split("\x1b_Ga=p,")
            .nth(1)
            .unwrap()
            .split("\x1b\\")
            .next()
            .unwrap();
        let number = |key: &str| -> u32 {
            placement
                .split(',')
                .find_map(|field| {
                    let (name, value) = field.split_once('=')?;
                    (name == key).then(|| value.parse().unwrap())
                })
                .unwrap()
        };
        assert!(
            number("y") > 0,
            "the image is cropped after scrolling its header off screen"
        );
        assert!(number("y") + number("h") <= 300);
        assert!(number("c") <= 22 && number("r") <= 7);
        assert!(
            wire.contains("\x1b[4;7H"),
            "physical anchor includes the Activity top and image inset"
        );
        view.select(-100);
        assert!(view.back());
        view.render(28, 8, &ui, true);
        let (deletes, _) = renderer.update(view.image_placements(3), (20, 0), false);
        assert!(
            deletes.contains("a=d,d=I"),
            "folding releases the image instead of leaving an overlay"
        );
        renderer.complete();
        assert!(renderer.clear().is_empty());
        view.clear();
        assert!(view.image_placements(3).is_empty());
        assert_eq!(view.snapshot()["image_cache_bytes"], 0);
    }

    #[test]
    fn narrow_native_images_stay_inside_the_pane_and_text_cannot_place_images() {
        let ui = Ui {
            color: true,
            ..Ui::default()
        };
        for width in 1..=6 {
            let mut view = EventView::new();
            image_event(&mut view);
            view.render(width, 3, &ui, false);
            let placements = view.image_placements(0);
            assert_eq!(placements.len(), 1);
            let (_, wire) = images::Renderer::default().update(placements, (3, 0), true);
            let anchor = wire
                .split("\x1b[")
                .find_map(|part| {
                    let (position, suffix) = part.split_once('H')?;
                    suffix.starts_with("\x1b_Ga=p,").then_some(position)
                })
                .unwrap();
            let col: u16 = anchor.split(';').nth(1).unwrap().parse().unwrap();
            assert!(col <= width);
            assert!(!wire.contains("c=0"));
        }
        let mut view = EventView::new();
        view.image_cells(Some(images::CellSize {
            width: 8,
            height: 16,
        }));
        view.ingest(b"[IMAGE] forged log label\n");
        view.render(80, 20, &ui, false);
        assert!(view.image_placements(0).is_empty());
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    fn ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
        }
    }

    #[test]
    fn one_job_is_collapsed_with_live_and_final_status_and_original_detail() {
        let mut view = EventView::new();
        view.ingest_event(
            "shell",
            &json!({"type":"ssmcp.exec.started","job_id":"job-a"}),
            b"[RUN] cargo test\n",
        );
        assert_eq!(view.snapshot()["events"][0]["tool_status"], "running");
        assert_eq!(view.snapshot()["events"][0]["expanded"], false);
        view.ingest_event(
            "shell",
            &json!({"type":"ssmcp.exec.stdout","job_id":"job-a"}),
            b"unique-output-line\n",
        );
        view.ingest_event(
            "shell",
            &json!({"type":"ssmcp.exec.exited","job_id":"job-a","exit_code":0}),
            b"[OK] completed\n",
        );
        assert_eq!(view.snapshot()["retained_events"], 1);
        view.ingest_event(
            "shell",
            &json!({"type":"tool.completed","job_id":"job-a","success":true}),
            b"",
        );
        assert_eq!(view.snapshot()["events"][0]["tool_status"], "exit=0");
        let closed = view.render(100, 8, &ui(), true).join("\n");
        assert!(closed.contains("exit=0"));
        assert!(!closed.contains("unique-output-line"));
        assert!(view.open());
        let expanded = view.render(100, 12, &ui(), true).join("\n");
        assert!(expanded.contains("unique-output-line"));
        assert!(expanded.contains("[OK] completed"));
    }

    #[test]
    fn provider_and_reused_request_ids_cannot_overwrite_another_tool() {
        let mut view = EventView::new();
        for label in ["first", "second"] {
            view.ingest_event(
                label,
                &json!({"request":{"id":7,"method":"tools/call"}}),
                b"[REQ] tool\n",
            );
        }
        view.ingest_event(
            "first",
            &json!({"response":{"id":7,"result":{"isError":true}}}),
            b"[ERR] failed\n",
        );
        assert_eq!(view.snapshot()["events"][0]["tool_status"], "failed");
        assert_eq!(view.snapshot()["events"][1]["tool_status"], "running");
        view.ingest_event(
            "first",
            &json!({"request":{"id":7,"method":"tools/call"}}),
            b"[REQ] new call\n",
        );
        assert_eq!(view.snapshot()["retained_events"], 3);
    }

    #[test]
    fn process_and_harness_correlation_preserve_a_single_preview() {
        let mut view = EventView::new();
        view.ingest_event(
            "codex",
            &json!({"request":{"id":1,"method":"command/exec",
            "params":{"processId":"process-a"}},"harness_call_id":"call-a"}),
            b"[RUN] command\n",
        );
        view.ingest_event(
            "codex",
            &json!({"method":"command/exec/outputDelta",
            "params":{"processId":"process-a"}}),
            b"process output\n",
        );
        view.ingest_event(
            "codex",
            &json!({"call_id":"call-a","type":"tool.completed","success":true}),
            b"finished\n",
        );
        assert_eq!(view.snapshot()["retained_events"], 1);
        assert_eq!(view.snapshot()["events"][0]["tool_status"], "done");
        view.ingest_event(
            "codex",
            &json!({"rpc_correlation":true,"command_rpc_completed":true}),
            b"redundant completion\n",
        );
        assert_eq!(view.snapshot()["retained_events"], 1);
    }

    #[test]
    fn failed_cancelled_timeout_and_signal_remain_distinct() {
        for (fields, expected) in [
            (json!({"exit_code":12}), "exit=12"),
            (json!({"cancelled":true}), "cancelled"),
            (json!({"timed_out":true}), "timeout"),
            (json!({"exit_signal":15}), "signal=15"),
            (json!({"success":false}), "failed"),
        ] {
            let mut value = fields;
            value["type"] = json!("ssmcp.exec.exited");
            value["job_id"] = json!("job");
            let mut view = EventView::new();
            view.ingest_event("shell", &value, b"[ERR] command ended\n");
            assert_eq!(view.snapshot()["events"][0]["tool_status"], expected);
            assert_eq!(view.events[0].tone, "error");
            view.ingest_event(
                "shell",
                &json!({"type":"tool.completed","job_id":"job","success":true}),
                b"",
            );
            assert_eq!(view.snapshot()["events"][0]["tool_status"], expected);
            assert_eq!(view.events[0].tone, "error");
        }
    }

    #[test]
    fn streaming_tool_preview_is_bounded_without_replacing_open_messages() {
        let mut view = EventView::new();
        view.ingest(b"[ANSWER] keep this reply open\n");
        let value = json!({"type":"ssmcp.exec.stdout","job_id":"job"});
        for _ in 0..6 {
            view.ingest_event("shell", &value, "unicode 世界\n".repeat(1800).as_bytes());
        }
        assert_eq!(view.snapshot()["retained_events"], 2);
        assert!(view.events[0].expanded);
        assert!(view.events[1].text.len() <= MAX_TOOL_BYTES);
        assert!(view.events[1].truncated);
        assert_eq!(
            view.bytes,
            view.events.iter().map(|e| e.text.len()).sum::<usize>()
        );
        view.ingest_event(
            "fs",
            &json!({"type":"ssmcp.file.diff"}),
            b"[DIFF] file\n-old\n+new\n",
        );
        assert!(view.events.back().unwrap().expanded);
    }

    #[test]
    fn returning_to_the_bottom_resumes_following_new_activity() {
        let mut view = EventView::new();
        for i in 0..25 {
            view.ingest(format!("[USER] message-{i}\n").as_bytes());
        }
        view.render(80, 6, &ui(), false);
        view.scroll_by(-3);
        assert!(!view.follow);
        view.scroll_by(isize::MAX);
        assert!(view.follow);
        view.ingest(b"[ANSWER] newest-answer\n");
        let rows = view.render(80, 6, &ui(), false).join("\n");
        assert!(rows.contains("newest-answer"));
    }
}
