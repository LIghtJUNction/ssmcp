//! Bounded code_mode history and a terminal-independent source viewer.
use super::ui::Ui;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    time::{SystemTime, UNIX_EPOCH},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_ENTRIES: usize = 128;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_CALLS: usize = 128;
const MAX_SOURCE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
struct Call {
    tool: String,
    finished: bool,
    success: Option<bool>,
    error: Option<String>,
    duration_ms: u64,
    exit_code: Option<i64>,
    job_id: Option<String>,
    state: Option<String>,
}

#[derive(Clone, Debug)]
struct Entry {
    run_id: String,
    code: String,
    timestamp_ms: u64,
    running: bool,
    timed_out: bool,
    success: Option<bool>,
    error: Option<String>,
    duration_ms: u64,
    calls: BTreeMap<u64, Call>,
    truncated: bool,
}

impl Entry {
    fn status(&self) -> &'static str {
        if self.running {
            "running"
        } else if self.timed_out {
            "timeout"
        } else if self.success == Some(false) {
            "failed"
        } else if self.success == Some(true) {
            "done"
        } else {
            "received"
        }
    }
    fn bytes(&self) -> usize {
        self.run_id.len()
            + self.code.len()
            + self.error.as_ref().map_or(0, String::len)
            + self
                .calls
                .values()
                .map(|call| {
                    call.tool.len()
                        + call.error.as_ref().map_or(0, String::len)
                        + call.job_id.as_ref().map_or(0, String::len)
                        + call.state.as_ref().map_or(0, String::len)
                        + 64
                })
                .sum::<usize>()
            + 128
    }
    fn metadata(&self) -> Value {
        json!({
            "run_id":self.run_id,"timestamp_ms":self.timestamp_ms,"status":self.status(),
            "success":self.success,"error":self.error,"duration_ms":self.duration_ms,"timed_out":self.timed_out,
            "code_bytes":self.code.len(),"truncated":self.truncated,
            "calls":self.calls.iter().map(|(id,call)| json!({"request_id":id,"tool":call.tool,"finished":call.finished,"success":call.success,"error":call.error,"duration_ms":call.duration_ms,"exit_code":call.exit_code,"job_id":call.job_id,"state":call.state})).collect::<Vec<_>>()
        })
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct AuditView {
    entries: VecDeque<Entry>,
    selected: usize,
    detail: bool,
    scroll: usize,
    scroll_limit: Cell<usize>,
}

impl AuditView {
    /// Source and metadata are retained only after the existing audit redactor.
    /// Lifecycle repeats update the same record instead of duplicating source.
    pub(super) fn ingest(&mut self, value: &Value) -> bool {
        if value.get("type").and_then(Value::as_str) != Some("ssmcp.code_mode") {
            return false;
        }
        let phase = value
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("started");
        if !matches!(
            phase,
            "started" | "finished" | "call-started" | "call-finished"
        ) {
            return false;
        }
        let value = crate::audit::event(value.clone());
        let Some(id) = value
            .get("run_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return false;
        };
        let id = bounded(id, 256);
        let previous = self.selected_entry().map(|entry| entry.run_id.clone());
        let existing = self.entries.iter().position(|entry| entry.run_id == id);
        let index = if let Some(index) = existing {
            index
        } else {
            let timestamp_ms = value
                .get("timestamp_ms")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| {
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        .min(u128::from(u64::MAX)) as u64
                });
            self.entries.push_front(Entry {
                run_id: id,
                code: String::new(),
                timestamp_ms,
                running: true,
                timed_out: false,
                success: None,
                error: None,
                duration_ms: 0,
                calls: BTreeMap::new(),
                truncated: false,
            });
            0
        };
        let entry = &mut self.entries[index];
        if let Some(timestamp) = value.get("timestamp_ms").and_then(Value::as_u64) {
            entry.timestamp_ms = timestamp;
        }
        if let Some(code) = value.get("code").and_then(Value::as_str) {
            entry.truncated = code.len() > MAX_SOURCE_BYTES;
            entry.code = bounded(code, MAX_SOURCE_BYTES);
        }
        match phase {
            "started" => {
                // A late duplicate start must not undo an already observed result.
                if entry.success.is_none() {
                    entry.running = true;
                }
            }
            "finished" => {
                entry.running = false;
                entry.timed_out = value.get("status").and_then(Value::as_str) == Some("timeout");
                entry.success = value.get("success").and_then(Value::as_bool);
                entry.duration_ms = value
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(entry.duration_ms);
                entry.error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(|text| bounded(text, 4096));
            }
            "call-started" | "call-finished" => {
                if let Some(request_id) = value.get("request_id").and_then(Value::as_u64) {
                    if entry.calls.len() < MAX_CALLS || entry.calls.contains_key(&request_id) {
                        let call = entry.calls.entry(request_id).or_insert_with(|| Call {
                            tool: String::new(),
                            finished: false,
                            success: None,
                            error: None,
                            duration_ms: 0,
                            exit_code: None,
                            job_id: None,
                            state: None,
                        });
                        if let Some(tool) = value.get("tool").and_then(Value::as_str) {
                            call.tool = bounded(tool, 512);
                        }
                        if phase == "call-finished" {
                            call.finished = true;
                            call.success = value.get("success").and_then(Value::as_bool);
                            call.error = value
                                .get("error")
                                .and_then(Value::as_str)
                                .map(|text| bounded(text, 2048));
                            call.duration_ms = value
                                .get("duration_ms")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            call.exit_code = value.get("exit_code").and_then(Value::as_i64);
                            call.job_id = value
                                .get("job_id")
                                .and_then(Value::as_str)
                                .map(|text| bounded(text, 256));
                            call.state = value
                                .get("state")
                                .and_then(Value::as_str)
                                .map(|text| bounded(text, 64));
                        }
                    }
                }
            }
            _ => return false,
        }
        // Keep a single oversized record useful, then evict oldest runs to fit.
        let excess = entry.bytes().saturating_sub(MAX_BYTES);
        if excess > 0 {
            let keep = entry.code.len().saturating_sub(excess);
            truncate(&mut entry.code, keep);
            entry.truncated = true;
        }
        while self.entries.len() > MAX_ENTRIES || self.stored_bytes() > MAX_BYTES {
            self.entries.pop_back();
        }
        let visible = self.visible();
        self.selected = if existing.is_none() && !self.detail && self.selected == 0 {
            0
        } else {
            previous
                .as_ref()
                .and_then(|id| {
                    visible
                        .iter()
                        .position(|&index| self.entries[index].run_id == *id)
                })
                .unwrap_or(0)
        };
        if self.selected_entry().is_none() {
            self.detail = false;
            self.scroll = 0;
        }
        true
    }

    fn stored_bytes(&self) -> usize {
        self.entries.iter().map(Entry::bytes).sum()
    }
    fn visible(&self) -> Vec<usize> {
        (0..self.entries.len()).collect()
    }
    fn selected_entry(&self) -> Option<&Entry> {
        self.visible()
            .get(self.selected)
            .map(|&index| &self.entries[index])
    }

    pub(super) fn up(&mut self) {
        if self.detail {
            self.scroll = self.scroll.min(self.scroll_limit.get()).saturating_sub(1);
        } else {
            self.selected = self.selected.saturating_sub(1);
        }
    }
    pub(super) fn down(&mut self) {
        if self.detail {
            self.scroll = self
                .scroll
                .min(self.scroll_limit.get())
                .saturating_add(1)
                .min(self.scroll_limit.get());
        } else {
            self.selected = self
                .selected
                .saturating_add(1)
                .min(self.visible().len().saturating_sub(1));
        }
    }
    pub(super) fn page_up(&mut self, height: usize) {
        self.scroll_by(-(height.max(1).min(isize::MAX as usize) as isize));
    }
    pub(super) fn page_down(&mut self, height: usize) {
        self.scroll_by(height.max(1).min(isize::MAX as usize) as isize);
    }
    pub(super) fn scroll_by(&mut self, lines: isize) {
        let amount = lines.unsigned_abs();
        if self.detail {
            let current = self.scroll.min(self.scroll_limit.get());
            self.scroll = if lines < 0 {
                current.saturating_sub(amount)
            } else {
                current.saturating_add(amount).min(self.scroll_limit.get())
            };
        } else {
            self.selected = if lines < 0 {
                self.selected.saturating_sub(amount)
            } else {
                self.selected
                    .saturating_add(amount)
                    .min(self.visible().len().saturating_sub(1))
            };
        }
    }
    pub(super) fn open(&mut self) {
        if self.selected_entry().is_some() {
            self.detail = true;
            self.scroll = 0;
        }
    }
    pub(super) fn back(&mut self) {
        self.detail = false;
        self.scroll = 0;
    }
    pub(super) fn snapshot(&self) -> Value {
        let mut selected = self
            .selected_entry()
            .map(Entry::metadata)
            .unwrap_or(Value::Null);
        if let Some(entry) = self.selected_entry() {
            selected["code"] = json!(entry.code);
        }
        json!({"entries":self.entries.iter().map(Entry::metadata).collect::<Vec<_>>(),
            "visible":self.visible().len(),"selected_index":self.selected,"selected":selected,
            "detail":self.detail,"scroll":self.scroll,"stored_bytes":self.stored_bytes(),
            "limits":{"entries":MAX_ENTRIES,"bytes":MAX_BYTES,"source_bytes":MAX_SOURCE_BYTES}})
    }

    /// Rows contain only validated SGR sequences, and occupy exactly width cells.
    pub(super) fn render(&self, width: u16, height: u16, ui: &Ui) -> Vec<String> {
        let width = usize::from(width);
        let height = usize::from(height);
        let mut rows = if self.detail {
            self.detail_rows(width, height, ui)
        } else {
            self.list_rows(width, height, ui)
        };
        rows.truncate(height);
        rows.resize_with(height, || row(&[], width, ui));
        rows
    }

    fn list_rows(&self, width: usize, height: usize, ui: &Ui) -> Vec<String> {
        if height == 0 {
            return Vec::new();
        }
        let visible = self.visible();
        let title = format!(" Code mode · {} runs · Enter inspect", visible.len());
        let mut rows = vec![row(&[(title, "heading")], width, ui)];
        if visible.is_empty() {
            rows.push(row(
                &[(" Waiting for code_mode calls".into(), "muted")],
                width,
                ui,
            ));
            return rows;
        }
        let budget = height.saturating_sub(1);
        let per_entry = if width >= 45 && budget >= 2 { 2 } else { 1 };
        let capacity = (budget / per_entry).max(1);
        let first = self.selected.saturating_sub(capacity - 1);
        for (position, &index) in visible.iter().enumerate().skip(first).take(capacity) {
            let entry = &self.entries[index];
            let marker = if position == self.selected {
                "›"
            } else {
                " "
            };
            let status_tone = match entry.status() {
                "running" => "warning",
                "failed" | "timeout" => "error",
                "done" => "success",
                _ => "muted",
            };
            let time = clock(entry.timestamp_ms);
            let short_id = fit_plain(&entry.run_id, 18);
            let code = entry
                .code
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("(source unavailable)")
                .trim();
            if per_entry == 2 {
                rows.push(row(
                    &[
                        (format!("{marker} {time}  #{short_id}  "), "accent"),
                        (entry.status().into(), status_tone),
                        (format!("  {} calls", entry.calls.len()), "muted"),
                    ],
                    width,
                    ui,
                ));
                rows.push(row(
                    &[(
                        format!("  {code}"),
                        if position == self.selected {
                            "foreground"
                        } else {
                            "muted"
                        },
                    )],
                    width,
                    ui,
                ));
            } else {
                rows.push(row(
                    &[
                        (format!("{marker} {}  ", entry.status()), status_tone),
                        (code.into(), "foreground"),
                    ],
                    width,
                    ui,
                ));
            }
        }
        rows
    }

    fn detail_rows(&self, width: usize, height: usize, ui: &Ui) -> Vec<String> {
        if height == 0 {
            return Vec::new();
        }
        let Some(entry) = self.selected_entry() else {
            return Vec::new();
        };
        let mut rows = vec![row(
            &[(
                format!(" #{} · {} · Esc back", entry.run_id, entry.status()),
                "heading",
            )],
            width,
            ui,
        )];
        if height == 1 {
            return rows;
        }
        let source_lines = entry.code.split('\n').count();
        let mut metadata = format!(
            " {} · {} lines · {} calls · {} ms",
            clock(entry.timestamp_ms),
            source_lines,
            entry.calls.len(),
            entry.duration_ms
        );
        if entry.truncated {
            metadata.push_str(" · source truncated at display limit");
        }
        if let Some(error) = &entry.error {
            metadata.push_str(&format!(" · {error}"));
        }
        rows.push(row(
            &[(
                metadata,
                if entry.success == Some(false) {
                    "error"
                } else {
                    "muted"
                },
            )],
            width,
            ui,
        ));
        let lines = source_rows(&entry.code, width);
        let body = height.saturating_sub(rows.len());
        self.scroll_limit
            .set(lines.len().saturating_sub(body.max(1)));
        let offset = self.scroll.min(self.scroll_limit.get());
        rows.extend(
            lines
                .into_iter()
                .skip(offset)
                .take(body)
                .map(|line| row(&line, width, ui)),
        );
        rows
    }
}

fn bounded(text: &str, maximum: usize) -> String {
    let mut text = text.to_owned();
    truncate(&mut text, maximum);
    text
}
fn truncate(text: &mut String, maximum: usize) {
    if text.len() > maximum {
        let mut end = maximum;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
}
fn clock(timestamp_ms: u64) -> String {
    let seconds = timestamp_ms / 1000 % 86400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn display_chars(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().flat_map(|ch| {
        let chars = match ch {
            '\t' => [Some(' '); 4],
            '\r' => [None; 4],
            '\u{1b}' => [Some('␛'), None, None, None],
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => [Some('�'), None, None, None],
            value if value.is_control() => [Some(' '), None, None, None],
            value => [Some(value), None, None, None],
        };
        chars.into_iter().flatten()
    })
}
fn fit_plain(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut used = 0;
    for ch in display_chars(text) {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        output.push(ch);
        used += cells;
    }
    output
}
fn row(parts: &[(String, &str)], width: usize, ui: &Ui) -> String {
    let mut text = String::new();
    let mut used = 0;
    if ui.color {
        text.push_str(&format!(
            "\x1b[{};{}m",
            ui.background(),
            ui.color("foreground")
        ));
    }
    for (part, tone) in parts {
        let part = fit_plain(part, width.saturating_sub(used));
        if ui.color {
            text.push_str(&format!("\x1b[{}m", ui.color(tone)));
        }
        used += part.width();
        text.push_str(&part);
        if used >= width {
            break;
        }
    }
    text.push_str(&" ".repeat(width.saturating_sub(used)));
    if ui.color {
        text.push_str("\x1b[0m");
    }
    text
}

type Spans = Vec<(String, &'static str)>;
#[derive(Default)]
struct Lexer {
    block: bool,
    quote: Option<char>,
    escaped: bool,
}
impl Lexer {
    fn line(&mut self, line: &str) -> Spans {
        let mut spans = Vec::new();
        let mut index = 0;
        while index < line.len() {
            let tail = &line[index..];
            if self.block {
                let length = tail.find("*/").map_or(tail.len(), |end| {
                    self.block = false;
                    end + 2
                });
                spans.push((tail[..length].into(), "muted"));
                index += length;
                continue;
            }
            if let Some(quote) = self.quote {
                let mut length = 0;
                for ch in tail.chars() {
                    length += ch.len_utf8();
                    if self.escaped {
                        self.escaped = false;
                    } else if ch == '\\' {
                        self.escaped = true;
                    } else if ch == quote {
                        self.quote = None;
                        break;
                    }
                }
                spans.push((tail[..length].into(), "success"));
                index += length;
                continue;
            }
            if tail.starts_with("//") {
                spans.push((tail.into(), "muted"));
                break;
            }
            if tail.starts_with("/*") {
                self.block = true;
                spans.push(("/*".into(), "muted"));
                index += 2;
                continue;
            }
            let ch = tail.chars().next().unwrap();
            if matches!(ch, '\'' | '"' | '`') {
                self.quote = Some(ch);
                spans.push((ch.to_string(), "success"));
                index += ch.len_utf8();
                continue;
            }
            if ch.is_alphabetic() || matches!(ch, '_' | '$') {
                let length = tail
                    .char_indices()
                    .find(|(_, ch)| !(ch.is_alphanumeric() || matches!(ch, '_' | '$')))
                    .map_or(tail.len(), |(end, _)| end);
                let word = &tail[..length];
                let tone = if matches!(
                    word,
                    "const"
                        | "let"
                        | "var"
                        | "await"
                        | "async"
                        | "return"
                        | "if"
                        | "else"
                        | "for"
                        | "while"
                        | "of"
                        | "in"
                        | "new"
                        | "function"
                        | "try"
                        | "catch"
                        | "finally"
                        | "throw"
                        | "class"
                        | "extends"
                        | "import"
                        | "export"
                        | "from"
                        | "switch"
                        | "case"
                        | "break"
                        | "continue"
                        | "typeof"
                        | "instanceof"
                        | "delete"
                        | "yield"
                ) {
                    "accent"
                } else if matches!(word, "true" | "false" | "null" | "undefined") {
                    "warning"
                } else {
                    "foreground"
                };
                spans.push((word.into(), tone));
                index += length;
                continue;
            }
            if ch.is_ascii_digit() {
                let length = tail
                    .char_indices()
                    .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_')))
                    .map_or(tail.len(), |(end, _)| end);
                spans.push((tail[..length].into(), "warning"));
                index += length;
                continue;
            }
            spans.push((ch.to_string(), "foreground"));
            index += ch.len_utf8();
        }
        if self.quote != Some('`') {
            self.quote = None;
            self.escaped = false;
        }
        spans
    }
}

fn source_rows(code: &str, width: usize) -> Vec<Spans> {
    if width == 0 {
        return vec![Vec::new()];
    }
    let digits = code.split('\n').count().to_string().len();
    let prefix = if width > digits + 3 { digits + 2 } else { 0 };
    let available = width - prefix;
    let mut lexer = Lexer::default();
    let mut rows = Vec::new();
    for (number, line) in code.split('\n').enumerate() {
        let mut row = if prefix > 0 {
            vec![(format!("{:>digits$}  ", number + 1), "muted")]
        } else {
            Vec::new()
        };
        let mut used = 0;
        for (text, tone) in lexer.line(line) {
            for mut ch in display_chars(&text) {
                let mut cells = ch.width().unwrap_or(0);
                if cells > available {
                    ch = '�';
                    cells = 1;
                }
                if used + cells > available {
                    rows.push(row);
                    row = if prefix > 0 {
                        vec![(format!("{}↳ ", " ".repeat(digits)), "muted")]
                    } else {
                        Vec::new()
                    };
                    used = 0;
                }
                if let Some((part, _)) = row.last_mut().filter(|(_, last)| *last == tone) {
                    part.push(ch);
                } else {
                    row.push((ch.to_string(), tone));
                }
                used += cells;
            }
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn run_and_call_updates_keep_source_and_final_failure() {
        let mut view = AuditView::default();
        assert!(view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"one","phase":"started","code":"const x = 1;\nreturn x;","timestamp_ms":0})));
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"one","phase":"call-started","request_id":12,"tool":"ssmcp.exec.start"}));
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"one","phase":"call-finished","request_id":12,"success":true}));
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"one","phase":"finished","success":false,"error":"script failed","duration_ms":40}));
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"one","phase":"call-finished","request_id":12,"success":true}));
        let snapshot = view.snapshot();
        assert_eq!(snapshot["entries"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["selected"]["status"], "failed");
        assert_eq!(snapshot["selected"]["code"], "const x = 1;\nreturn x;");
        assert_eq!(snapshot["selected"]["calls"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["selected"]["calls"][0]["tool"], "ssmcp.exec.start");
    }
    #[test]
    fn unicode_source_wraps_and_display_controls_cannot_escape() {
        let mut view = AuditView::default();
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"a","phase":"started","code":"const 测试 = '你好';\n\u{1b}[2J// comment\nreturn 123;"}));
        view.open();
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        for width in [1, 7, 16, 40] {
            let rows = view.render(width, 12, &ui);
            assert_eq!(rows.len(), 12);
            assert!(
                rows.iter()
                    .all(|row| row.width() == usize::from(width) && !row.contains('\u{1b}'))
            );
        }
        let rows = view.render(40, 12, &Ui::default());
        assert!(
            rows.iter()
                .any(|row| row.contains(&Ui::default().color("accent")))
        );
    }
    #[test]
    fn history_is_bounded_without_splitting_utf8() {
        let mut view = AuditView::default();
        for id in 0..140 {
            view.ingest(&json!({"type":"ssmcp.code_mode","run_id":id.to_string(),"phase":"started","code":"x"}));
        }
        assert_eq!(view.entries.len(), MAX_ENTRIES);
        view.ingest(&json!({"type":"ssmcp.code_mode","run_id":"large","phase":"started","code":"测".repeat(MAX_BYTES/3+10)}));
        assert!(view.stored_bytes() <= MAX_BYTES);
        assert!(view.entries[0].truncated);
        assert!(
            view.entries[0]
                .code
                .is_char_boundary(view.entries[0].code.len())
        );
    }
}
