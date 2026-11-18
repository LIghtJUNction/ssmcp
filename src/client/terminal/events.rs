//! Bounded local event previews. Expanding reveals retained display text, not a
//! reread of tools or an unbounded replacement for their original output archive.
use super::super::ui::Ui;
use serde_json::{Value, json};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_EVENTS: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;

struct Event {
    id: u64,
    text: String,
    summary: String,
    lines: usize,
    expanded: bool,
    truncated: bool,
    tone: &'static str,
    cache: Option<(usize, bool, Vec<String>)>,
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
        self.ingest_with_expansion(bytes, false);
    }

    pub(crate) fn ingest_expanded(&mut self, bytes: &[u8]) {
        self.ingest_with_expansion(bytes, true);
    }

    fn ingest_with_expansion(&mut self, bytes: &[u8], expand_all: bool) {
        if bytes.is_empty() {
            return;
        }
        let (text, truncated) = retained_text(bytes);
        let plain = unstyled(&text);
        let Some(first) = plain.lines().find(|line| !line.trim().is_empty()) else {
            return;
        };
        let first = heading(first);
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
        // A batch can begin with stdout and contain the failure later. Keep
        // that failure visible rather than judging only its first heading.
        let error = plain
            .lines()
            .map(heading)
            .find(|line| line.starts_with("[ERR"));
        let warning = plain
            .lines()
            .map(heading)
            .find(|line| line.starts_with("[WARN") || line.starts_with("[STDERR"));
        let attention = error.or(warning);
        let tone = if conversation {
            "accent"
        } else if error.is_some() {
            "error"
        } else if warning.is_some() {
            "warning"
        } else if first.starts_with("[OK") {
            "success"
        } else if first.starts_with("[RUN") || first.starts_with("[REQ") {
            "accent"
        } else {
            "foreground"
        };
        let summary = event_summary(
            &plain,
            if conversation {
                first
            } else {
                attention.unwrap_or(first)
            },
        );
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
            expanded: expand_all || conversation || attention.is_some(),
            truncated,
            tone,
            cache: None,
        });
        while self.events.len() > MAX_EVENTS || self.bytes > MAX_BYTES {
            if let Some(event) = self.events.pop_front() {
                self.bytes -= event.text.len();
                self.dropped = self.dropped.saturating_add(1);
            }
        }
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
        self.follow = false;
        self.reveal = false;
        self.scroll = self
            .scroll
            .saturating_add_signed(delta)
            .min(self.total_rows.saturating_sub(self.page_rows));
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
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "schema_version":1, "events":self.events.iter().map(|event| json!({
                "id":event.id,"summary":event.summary,"lines":event.lines,
                "bytes":event.text.len(),"expanded":event.expanded,"truncated":event.truncated,
                "selected":Some(event.id)==self.selected
            })).collect::<Vec<_>>(),
            "retained_events":self.events.len(),"retained_bytes":self.bytes,
            "max_events":MAX_EVENTS,"max_bytes":MAX_BYTES,"dropped_events":self.dropped,
            "selected":self.selected,"scroll":self.scroll,
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
                for (index, row) in content.iter().enumerate() {
                    let prefix = if index == 0 {
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
                let mut label = event.summary.clone();
                if event.lines > 1 {
                    label.push_str(&format!(" · {} lines", event.lines));
                }
                if event.truncated {
                    label.push_str(" · partial");
                }
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
            if let Some(selected_row) = selected_row {
                if selected_row < self.scroll || selected_row >= self.scroll.saturating_add(page) {
                    self.scroll = selected_row;
                }
            }
            self.reveal = false;
        }
        self.scroll = self.scroll.min(max_scroll);
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
    // Generic audit response headings put the actual native name on the next
    // line. Include that name without copying JSON or source into the summary.
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
    if clean.len() <= MAX_BYTES {
        return (clean, false);
    }
    let note = "\n… preview truncated; inspect the original output archive …\n";
    let budget = MAX_BYTES - note.len();
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
        for tag in ["USER", "AGENT", "UPDATE", "ANSWER", "IMAGE", "CONTEXT"] {
            view.ingest(format!("[{tag}]\n{}", "visible content\n".repeat(20)).as_bytes());
            assert!(view.events.back().unwrap().expanded, "{tag}");
        }
        assert_eq!(view.snapshot()["retained_events"], 7);
    }

    #[test]
    fn short_success_is_compact_but_keeps_unicode_detail_for_existing_expand() {
        let mut view = EventView::new();
        let source = "[RES ] audit\n  ssmcp.fs.read_text\n  中文界面 e\u{301}\n";
        view.ingest(source.as_bytes());
        let closed = view.render(80, 8, &plain_ui(), true).join("\n");
        assert!(closed.contains("ssmcp.fs.read_text"));
        assert!(!closed.contains("中文界面"));
        assert!(!view.events[0].expanded);
        assert_eq!(view.events[0].text, source);
        assert!(view.open());
        assert!(
            view.render(80, 8, &plain_ui(), true)
                .join("\n")
                .contains("中文界面 e\u{301}")
        );
    }

    #[test]
    fn errors_warnings_and_timestamped_progress_stay_visible() {
        let mut view = EventView::new();
        for header in [
            "[ERR ] failed",
            "[WARN] diagnostic",
            "[STDERR] failure",
            "12:34:56.789Z [UPDATE] progress",
            "[ANSWER] result",
        ] {
            view.ingest(format!("{header}\n{}", "details 界\n".repeat(20)).as_bytes());
            assert!(view.events.back().unwrap().expanded, "{header}");
        }
        view.ingest(b"[STDOUT] command\npartial\n[ERR ] exit=7\n");
        let failed = view.events.back().unwrap();
        assert!(failed.expanded);
        assert_eq!(failed.tone, "error");
        assert_eq!(failed.summary, "[ERR ] exit=7");
    }

    #[test]
    fn explicit_verbose_display_keeps_long_tool_output_open() {
        let mut view = EventView::new();
        view.ingest_expanded(format!("[READ] source\n{}", "retained body\n".repeat(30)).as_bytes());
        assert!(view.events[0].expanded);
        assert_eq!(view.events[0].lines, 31);
        assert!(view.back());
        assert!(view.open());
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
