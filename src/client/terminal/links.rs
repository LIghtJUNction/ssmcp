//! Bounded display links. Targets never execute while parsing or drawing.
//! Cached rows carry short local IDs, so wrapping a long URL cannot duplicate
//! that URL for every cell. Only the owner of the corresponding table resolves IDs.
use reqwest::Url;
use std::{
    ops::{Deref, Range},
    sync::Arc,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[cfg(all(test, unix))]
mod pty_fixture;
#[cfg(test)]
mod tests;

pub(super) const MAX_TARGETS: usize = 64;
pub(super) const MAX_TARGET_BYTES: usize = 2048;
pub(super) const MAX_TABLE_BYTES: usize = 32 * 1024;
const MAX_SPANS: usize = 128;
const MAX_SCAN_BYTES: usize = 256 * 1024;
const DOUBLE_CLICK: Duration = Duration::from_millis(250);
const CLICK_HOLD: Duration = Duration::from_secs(1);
pub(super) const CLOSE: &str = "\x1b]8;;\x1b\\";
const LOCAL: &str = "ssmcp-link:";

pub(super) fn target(raw: &str) -> Option<Arc<str>> {
    if raw.len() > MAX_TARGET_BYTES
        || raw
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.as_str().len() > MAX_TARGET_BYTES
    {
        return None;
    }
    Some(Arc::from(url.as_str()))
}

#[derive(Clone, Debug, Default)]
pub(super) struct Targets {
    values: Vec<Arc<str>>,
    bytes: usize,
}

impl Targets {
    pub(super) fn insert(&mut self, raw: &str) -> Option<u16> {
        let value = target(raw)?;
        self.insert_valid(value)
    }

    fn insert_valid(&mut self, value: Arc<str>) -> Option<u16> {
        if let Some(index) = self.values.iter().position(|existing| existing == &value) {
            return Some(index as u16);
        }
        if self.values.len() == MAX_TARGETS
            || self.bytes.saturating_add(value.len()) > MAX_TABLE_BYTES
        {
            return None;
        }
        let index = self.values.len() as u16;
        self.bytes += value.len();
        self.values.push(value);
        Some(index)
    }

    pub(super) fn get(&self, id: u16) -> Option<&Arc<str>> {
        self.values.get(usize::from(id))
    }
}

#[derive(Clone, Debug)]
pub(super) struct Span {
    pub(super) range: Range<usize>,
    pub(super) id: u16,
}

/// Safe text with all OSC records removed; span offsets count visible UTF-8
/// bytes, excluding SGR. Explicit links take precedence over bare URL detection.
pub(super) struct Source {
    pub(super) text: String,
    pub(super) targets: Targets,
    pub(super) spans: Vec<Span>,
}

pub(super) enum Token<'a> {
    Text(char),
    Sgr(&'a str),
    Osc(&'a str),
    Ignore,
}

/// One forward pass over UTF-8. Unterminated OSC records consume the remainder;
/// no control payload is recovered as clickable text.
pub(super) fn token(text: &str) -> (usize, Token<'_>) {
    let bytes = text.as_bytes();
    if bytes.starts_with(b"\x1b[") {
        let end = bytes[2..]
            .iter()
            .position(|byte| (0x40..=0x7e).contains(byte))
            .map_or(bytes.len(), |end| end + 3);
        let sgr = &text[..end];
        if end <= 130
            && sgr.ends_with('m')
            && sgr[2..sgr.len() - 1]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || byte == b';')
        {
            return (end, Token::Sgr(sgr));
        }
        return (end, Token::Ignore);
    }
    if bytes.starts_with(b"\x1b]")
        || bytes.starts_with(b"\x1bP")
        || bytes.starts_with(b"\x1b_")
        || bytes.starts_with(b"\x1b^")
        || bytes.starts_with(b"\x1bX")
    {
        let osc = bytes[1] == b']';
        let mut at = 2;
        while at < bytes.len() {
            if osc && bytes[at] == 7 {
                return (at + 1, Token::Osc(&text[2..at]));
            }
            if bytes[at..].starts_with(b"\x1b\\") {
                return (
                    at + 2,
                    if osc {
                        Token::Osc(&text[2..at])
                    } else {
                        Token::Ignore
                    },
                );
            }
            at += 1;
        }
        return (bytes.len(), Token::Ignore);
    }
    let c = text.chars().next().expect("nonempty token input");
    if matches!(c, '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}') {
        let end = text[c.len_utf8()..]
            .char_indices()
            .find(|(_, ch)| *ch == '\u{9c}' || (c == '\u{9d}' && *ch == '\x07'));
        return (
            end.map_or(text.len(), |(at, ch)| c.len_utf8() + at + ch.len_utf8()),
            Token::Ignore,
        );
    }
    if c == '\x1b' {
        return (
            1 + text[1..].chars().next().map_or(0, char::len_utf8),
            Token::Ignore,
        );
    }
    (
        c.len_utf8(),
        if c.is_control() && !matches!(c, '\n' | '\r' | '\t') {
            Token::Ignore
        } else {
            Token::Text(c)
        },
    )
}

pub(super) fn osc_uri(payload: &str) -> Option<&str> {
    let rest = payload.strip_prefix("8;")?;
    let (_, uri) = rest.split_once(';')?;
    Some(uri)
}

pub(super) fn marker(id: u16) -> String {
    format!("\x1b]8;;{LOCAL}{id}\x1b\\")
}

pub(super) fn local_id(payload: &str) -> Option<Option<u16>> {
    let uri = osc_uri(payload)?;
    if uri.is_empty() {
        return Some(None);
    }
    let id = uri.strip_prefix(LOCAL)?;
    if id.len() > 3 || !id.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(Some(id.parse().ok()?))
}

pub(super) fn sanitize(text: &str, keep_color: bool, keep_links: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut targets = Targets::default();
    while !rest.is_empty() {
        let (len, item) = token(rest);
        match item {
            Token::Sgr(sgr) if keep_color => out.push_str(sgr),
            Token::Osc(payload) if keep_links => {
                if let Some(uri) = osc_uri(payload) {
                    if uri.is_empty() {
                        out.push_str(CLOSE);
                    } else if let Some(id) = targets.insert(uri) {
                        out.push_str("\x1b]8;;");
                        out.push_str(targets.get(id).unwrap());
                        out.push_str("\x1b\\");
                    } else {
                        // Invalid replacement links close an old target too.
                        out.push_str(CLOSE);
                    }
                }
            }
            Token::Text('\r') => {
                if !rest[len..].starts_with('\n') {
                    out.push('\n');
                }
            }
            Token::Text(c) => out.push(c),
            _ => {}
        }
        rest = &rest[len..];
    }
    out
}

pub(super) fn bare(text: &str) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut next = 0;
    for (start, c) in text
        .char_indices()
        .take_while(|(at, _)| *at < MAX_SCAN_BYTES)
    {
        if start < next || !matches!(c, 'h' | 'H') || result.len() == MAX_SPANS {
            continue;
        }
        let tail = &text[start..];
        if !tail
            .get(..7)
            .is_some_and(|part| part.eq_ignore_ascii_case("http://"))
            && !tail
                .get(..8)
                .is_some_and(|part| part.eq_ignore_ascii_case("https://"))
        {
            continue;
        }
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(|before| before.is_alphanumeric() || matches!(before, '_' | '/' | '@'))
        {
            continue;
        }
        let end = tail
            .char_indices()
            .find(|(at, c)| {
                *at > MAX_TARGET_BYTES
                    || c.is_whitespace()
                    || c.is_control()
                    || "<>\"'`\\，。！？、；：）】》」』“”‘’".contains(*c)
            })
            .map_or(tail.len(), |(at, _)| at);
        let mut candidate = &tail[..end];
        while let Some(last) = candidate.chars().next_back() {
            let unbalanced = match last {
                ')' => candidate.matches(')').count() > candidate.matches('(').count(),
                ']' => candidate.matches(']').count() > candidate.matches('[').count(),
                '}' => candidate.matches('}').count() > candidate.matches('{').count(),
                _ => false,
            };
            if ".,;:!?".contains(last) || unbalanced {
                candidate = &candidate[..candidate.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        next = start + end;
        if target(candidate).is_some() {
            result.push(start..start + candidate.len());
        }
    }
    result
}

impl Source {
    pub(super) fn new(raw: &str, automatic: bool) -> Self {
        let mut source = Self {
            text: String::with_capacity(raw.len()),
            targets: Targets::default(),
            spans: Vec::new(),
        };
        let mut plain = String::with_capacity(raw.len());
        let mut active: Option<(usize, u16)> = None;
        let mut rest = raw;
        while !rest.is_empty() {
            let (len, item) = token(rest);
            match item {
                Token::Sgr(sgr) => source.text.push_str(sgr),
                Token::Text('\r') => {
                    if !rest[len..].starts_with('\n') {
                        source.text.push('\n');
                        plain.push('\n');
                    }
                }
                Token::Text(c) => {
                    source.text.push(c);
                    plain.push(c);
                }
                Token::Osc(payload) => {
                    if let Some(uri) = osc_uri(payload) {
                        if let Some((start, id)) = active.take()
                            && start < plain.len()
                            && source.spans.len() < MAX_SPANS
                        {
                            source.spans.push(Span {
                                range: start..plain.len(),
                                id,
                            });
                        }
                        if !uri.is_empty()
                            && source.spans.len() < MAX_SPANS
                            && let Some(id) = source.targets.insert(uri)
                        {
                            active = Some((plain.len(), id));
                        }
                    }
                }
                Token::Ignore => {}
            }
            rest = &rest[len..];
        }
        if let Some((start, id)) = active
            && start < plain.len()
            && source.spans.len() < MAX_SPANS
        {
            source.spans.push(Span {
                range: start..plain.len(),
                id,
            });
        }
        if automatic {
            for range in bare(&plain) {
                if source.spans.len() == MAX_SPANS {
                    break;
                }
                if !source
                    .spans
                    .iter()
                    .any(|span| span.range.start < range.end && range.start < span.range.end)
                    && let Some(id) = source.targets.insert(&plain[range.clone()])
                {
                    source.spans.push(Span { range, id });
                }
            }
            source.spans.sort_by_key(|span| span.range.start);
        }
        if !source.spans.is_empty() {
            let boundaries: Vec<_> = plain
                .grapheme_indices(true)
                .map(|(at, _)| at)
                .chain(std::iter::once(plain.len()))
                .collect();
            for span in &mut source.spans {
                span.range.start =
                    boundaries[boundaries.partition_point(|at| *at < span.range.start)];
                span.range.end = boundaries[boundaries
                    .partition_point(|at| *at <= span.range.end)
                    .saturating_sub(1)];
            }
            source.spans.retain(|span| !span.range.is_empty());
        }
        source
    }

    pub(super) fn marked(&self) -> String {
        self.mark(&self.text, true)
    }

    fn mark(&self, styled: &str, local: bool) -> String {
        let mut out = String::with_capacity(styled.len());
        let mut rest = styled;
        let mut visible = 0;
        let mut span = 0;
        let mut open = false;
        while !rest.is_empty() {
            while span < self.spans.len() && self.spans[span].range.end <= visible {
                if open {
                    out.push_str(CLOSE);
                    open = false;
                }
                span += 1;
            }
            if let Some(current) = self.spans.get(span)
                && current.range.start == visible
                && !open
            {
                if local {
                    out.push_str(&marker(current.id));
                } else {
                    out.push_str("\x1b]8;;");
                    out.push_str(self.targets.get(current.id).unwrap());
                    out.push_str("\x1b\\");
                }
                open = true;
            }
            let (len, item) = token(rest);
            match item {
                Token::Text(c) => {
                    out.push(c);
                    visible += c.len_utf8();
                }
                Token::Sgr(sgr) => out.push_str(sgr),
                _ => {}
            }
            rest = &rest[len..];
        }
        if open {
            out.push_str(CLOSE);
        }
        out
    }
}

/// Color/highlight transforms may add SGR but cannot alter the visible source.
/// Preserve OSC targets through those transforms without coloring inside OSC.
pub(super) fn through(text: &str, style: impl FnOnce(&str) -> String) -> String {
    if !text.contains("\x1b]8;") {
        return style(text);
    }
    let source = Source::new(text, false);
    source.mark(&style(&source.text), false)
}

struct Pieces<'a> {
    text: String,
    controls: Vec<(usize, &'a str)>,
}
impl<'a> Pieces<'a> {
    fn new(mut raw: &'a str) -> Self {
        let mut result = Self {
            text: String::with_capacity(raw.len()),
            controls: Vec::new(),
        };
        while !raw.is_empty() {
            let (len, item) = token(raw);
            match item {
                Token::Text(c) => result.text.push(c),
                Token::Sgr(_) | Token::Osc(_) => {
                    result.controls.push((result.text.len(), &raw[..len]))
                }
                Token::Ignore => {}
            }
            raw = &raw[len..];
        }
        result
    }
}

#[derive(Default)]
struct LinkStyle {
    underline: u8,
    link: Option<u16>,
    changed: bool,
}
impl LinkStyle {
    fn restore(&self) -> &'static str {
        match self.underline {
            4 => "\x1b[4m",
            21 => "\x1b[21m",
            _ => "\x1b[24m",
        }
    }
    fn apply(&mut self, raw: &str, targets: &Targets, out: &mut String) {
        match token(raw).1 {
            Token::Osc(payload) => {
                if let Some(id) = local_id(payload) {
                    self.link = id.filter(|id| targets.get(*id).is_some());
                }
            }
            Token::Sgr(sgr) => {
                let mut codes = sgr[2..sgr.len() - 1].split(';');
                while let Some(code) = codes.next() {
                    if matches!(code, "38" | "48" | "58") {
                        match codes.next() {
                            Some("2") => {
                                codes.next();
                                codes.next();
                                codes.next();
                            }
                            Some("5") => {
                                codes.next();
                            }
                            _ => {}
                        }
                    } else {
                        match code {
                            "" | "0" | "24" => self.underline = 0,
                            "4" => self.underline = 4,
                            "21" => self.underline = 21,
                            _ => {}
                        }
                    }
                }
                out.push_str(sgr);
                self.changed = true;
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct WrapStyle {
    sgr: String,
    link: Option<u16>,
}
impl WrapStyle {
    fn apply(&mut self, raw: &str, row: &mut String) {
        match token(raw).1 {
            Token::Sgr(sequence) => {
                row.push_str(sequence);
                if sequence == "\x1b[0m" || sequence == "\x1b[m" {
                    self.sgr.clear();
                } else {
                    if self.sgr.len() > 256 {
                        self.sgr.clear();
                    }
                    self.sgr.push_str(sequence);
                }
            }
            Token::Osc(payload) => {
                if let Some(id) = local_id(payload) {
                    self.link = id;
                    row.push_str(raw);
                }
            }
            _ => {}
        }
    }
    fn next(&self, row: &mut String, values: &mut Vec<String>, color: bool) {
        if self.link.is_some() {
            row.push_str(CLOSE);
        }
        if color {
            row.push_str("\x1b[0m");
        }
        values.push(std::mem::take(row));
        row.push_str(&self.sgr);
        if let Some(id) = self.link {
            row.push_str(&marker(id));
        }
    }
}

/// Safe UI rows already carry styles and fixed text. Re-parsing text with a
/// scalar VT engine would count a ZWJ emoji as several cells and scroll a narrow
/// row. Use the existing VT parser only for its SGR attribute semantics; glyphs
/// are emitted as complete clusters and all other controls remain discarded.
pub(super) fn fill_rows(rows: &mut [String], width: u16, ui: &crate::client::ui::Ui) {
    let mut attributes = vt100::Parser::new(super::nonzero(1), super::nonzero(1), 0);
    for row in rows {
        let pieces = Pieces::new(row);
        let mut out = String::from("\x1b[0m");
        if ui.color {
            out.push_str(&format!(
                "\x1b[{};{}m",
                ui.background(),
                ui.color("foreground")
            ));
        }
        attributes.process(b"\x1b[0m");
        let mut style = super::Style::default();
        let mut changed = true;
        let mut control = 0;
        let mut used = 0;
        for (at, cluster) in pieces.text.grapheme_indices(true) {
            while pieces
                .controls
                .get(control)
                .is_some_and(|(offset, _)| *offset <= at)
            {
                if let Token::Sgr(sgr) = token(pieces.controls[control].1).1 {
                    attributes.process(sgr.as_bytes());
                    changed = true;
                }
                control += 1;
            }
            if !cluster.chars().any(char::is_control) {
                let cells = cluster.width();
                if used + cells > usize::from(width) {
                    break;
                }
                if changed {
                    attributes.process(b"\r ");
                    let next = super::Style::of(attributes.screen().cell(0, 0).unwrap(), ui.color);
                    if next != style {
                        next.append(&mut out);
                        if ui.color {
                            if next.fg == vt100::Color::Default {
                                out.push_str(&format!("\x1b[{}m", ui.color("foreground")));
                            }
                            if next.bg == vt100::Color::Default {
                                out.push_str(&format!("\x1b[{}m", ui.background()));
                            }
                        }
                        style = next;
                    }
                    changed = false;
                }
                out.push_str(cluster);
                used += cells;
            }
            while pieces
                .controls
                .get(control)
                .is_some_and(|(offset, _)| *offset < at + cluster.len())
            {
                if let Token::Sgr(sgr) = token(pieces.controls[control].1).1 {
                    attributes.process(sgr.as_bytes());
                    changed = true;
                }
                control += 1;
            }
        }
        if used < usize::from(width) {
            out.push_str(&format!(
                "\x1b[0;{}m{}",
                if ui.color {
                    ui.background()
                } else {
                    "49".into()
                },
                " ".repeat(usize::from(width) - used)
            ));
        }
        out.push_str("\x1b[0m");
        *row = out;
    }
}

#[derive(Clone, Debug)]
pub(super) struct Hit {
    pub(super) start: u16,
    pub(super) end: u16,
    pub(super) target: Arc<str>,
    pub(super) occurrence: u16,
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        // Local table order can change after an anchored background append.
        // The visible cell range and URI, not an internal cache ID, identify it.
        self.start == other.start && self.end == other.end && self.target == other.target
    }
}
impl Eq for Hit {}

#[derive(Clone, Debug, Default)]
pub(super) struct Rows {
    values: Vec<String>,
    targets: Targets,
}

impl Rows {
    pub(super) fn new(values: Vec<String>, targets: Targets) -> Self {
        Self { values, targets }
    }

    pub(super) fn plain(self) -> Vec<String> {
        self.values
            .into_iter()
            .map(|row| sanitize(&row, true, false))
            .collect()
    }

    pub(super) fn resolve(&self, row: &str, width: usize) -> (String, Vec<Hit>) {
        if self.targets.values.is_empty() || !row.contains("\x1b]8;") {
            return (row.to_owned(), Vec::new());
        }
        let pieces = Pieces::new(row);
        let mut out = String::with_capacity(row.len());
        let mut hits: Vec<Hit> = Vec::new();
        let mut column = 0usize;
        let mut control = 0;
        let mut style = LinkStyle::default();
        let mut forced = false;
        for (at, cluster) in pieces.text.grapheme_indices(true) {
            while pieces
                .controls
                .get(control)
                .is_some_and(|(offset, _)| *offset <= at)
            {
                style.apply(pieces.controls[control].1, &self.targets, &mut out);
                control += 1;
            }
            let end = at + cluster.len();
            let changed_inside = pieces.controls[control..]
                .iter()
                .take_while(|(offset, _)| *offset < end)
                .any(|(_, raw)| matches!(token(raw).1, Token::Osc(_)));
            let link = style.link.filter(|_| !changed_inside);
            let cells = cluster.width();
            if column.saturating_add(cells) > width {
                break;
            }
            if link.is_some() {
                if !forced || style.changed {
                    out.push_str("\x1b[4m");
                }
            } else if forced {
                out.push_str(style.restore());
            }
            forced = link.is_some();
            style.changed = false;
            out.push_str(cluster);
            if let Some(id) = link
                && cells != 0
            {
                if let Some(last) = hits.last_mut()
                    && last.occurrence == id
                    && usize::from(last.end) == column
                {
                    last.end = (column + cells) as u16;
                } else {
                    hits.push(Hit {
                        start: column as u16,
                        end: (column + cells) as u16,
                        target: self.targets.get(id).unwrap().clone(),
                        occurrence: id,
                    });
                }
            }
            column += cells;
            // A style escape inside a cluster takes effect after the cluster;
            // it cannot split an emoji or make its byte suffix a separate cell.
            while pieces
                .controls
                .get(control)
                .is_some_and(|(offset, _)| *offset < end)
            {
                style.apply(pieces.controls[control].1, &self.targets, &mut out);
                control += 1;
            }
        }
        for (_, raw) in &pieces.controls[control..] {
            style.apply(raw, &self.targets, &mut out);
        }
        if forced {
            out.push_str(style.restore());
        }
        (out, hits)
    }
}

impl Deref for Rows {
    type Target = [String];
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}
impl From<Vec<String>> for Rows {
    fn from(values: Vec<String>) -> Self {
        Self::new(values, Targets::default())
    }
}
impl IntoIterator for Rows {
    type Item = String;
    type IntoIter = std::vec::IntoIter<String>;
    fn into_iter(self) -> Self::IntoIter {
        self.values.into_iter()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Location {
    pub(super) row: u16,
    pub(super) column: u16,
    pub(super) event: u64,
    pub(super) hit: Hit,
}

#[derive(Default)]
pub(super) struct Gesture {
    pressed: Option<(Location, Instant)>,
    pending: Option<(Location, Instant)>,
    suppressed: bool,
}

impl Gesture {
    pub(super) fn cancel(&mut self) {
        self.pressed = None;
        self.pending = None;
        self.suppressed = false;
    }
    pub(super) fn active(&self) -> bool {
        self.pressed.is_some() || self.suppressed
    }
    pub(super) fn press(&mut self, location: Location, now: Instant) {
        let duplicate = self.pending.as_ref().is_some_and(|(old, until)| {
            old.row == location.row
                && old.event == location.event
                && old.hit == location.hit
                && now < *until
        });
        self.pending = None;
        self.pressed = (!duplicate).then_some((location, now));
        self.suppressed = duplicate;
    }
    pub(super) fn release(&mut self, location: Option<Location>, now: Instant) {
        if self.suppressed {
            self.cancel();
            return;
        }
        if let Some((pressed, at)) = self.pressed.take()
            && location.as_ref() == Some(&pressed)
            && now.saturating_duration_since(at) < CLICK_HOLD
        {
            self.pending = Some((pressed, now + DOUBLE_CLICK));
        }
    }
    pub(super) fn take(
        &mut self,
        now: Instant,
        current: impl FnOnce(&Location) -> bool,
    ) -> Option<Arc<str>> {
        let (location, until) = self.pending.as_ref()?;
        if !current(location) {
            self.cancel();
            return None;
        }
        if now < *until {
            return None;
        }
        self.pending.take().map(|(location, _)| location.hit.target)
    }
}

/// Reopen only the short local ID at every hard/soft line boundary. The shared
/// target table owns each URI once, including at a one-cell viewport width.
pub(super) fn wrapped(text: &str, width: usize, color: bool) -> Rows {
    let clean = sanitize(text, color, true);
    let source = Source::new(clean.trim_matches('\n'), true);
    let marked = source.marked();
    let pieces = Pieces::new(&marked);
    let mut values = Vec::new();
    let mut row = String::new();
    let mut style = WrapStyle::default();
    let mut used = 0;
    let mut control = 0;
    let width = width.max(1);
    for (at, cluster) in pieces.text.grapheme_indices(true) {
        while pieces
            .controls
            .get(control)
            .is_some_and(|(offset, _)| *offset <= at)
        {
            style.apply(pieces.controls[control].1, &mut row);
            control += 1;
        }
        if cluster == "\n" {
            style.next(&mut row, &mut values, color);
            used = 0;
        } else {
            let expanded;
            let cluster = if cluster == "\t" {
                expanded = " ".repeat(4 - used % 4);
                expanded.as_str()
            } else {
                cluster
            };
            for piece in cluster.graphemes(true) {
                let piece = if piece.width() > width { "…" } else { piece };
                let cells = piece.width();
                if used + cells > width && used != 0 {
                    style.next(&mut row, &mut values, color);
                    used = 0;
                }
                row.push_str(piece);
                used += cells;
            }
        }
        while pieces
            .controls
            .get(control)
            .is_some_and(|(offset, _)| *offset < at + cluster.len())
        {
            style.apply(pieces.controls[control].1, &mut row);
            control += 1;
        }
    }
    for (_, raw) in &pieces.controls[control..] {
        style.apply(raw, &mut row);
    }
    if style.link.is_some() {
        row.push_str(CLOSE);
    }
    if color {
        row.push_str("\x1b[0m");
    }
    values.push(row);
    Rows::new(values, source.targets)
}

/// A retained preview must never begin halfway through an OSC payload. Drop
/// the intersected control record, rather than rendering its URI as text.
pub(super) fn safe_cut(text: &str, wanted: usize, after: bool) -> usize {
    let mut at = 0;
    while at < text.len() && at < wanted {
        let (len, _) = token(&text[at..]);
        if at + len > wanted {
            return if after { at + len } else { at };
        }
        at += len;
    }
    at
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ScreenHit {
    pub(super) row: u16,
    pub(super) event: u64,
    pub(super) hit: Hit,
}

/// The next frame is not clickable until its complete byte buffer was written.
/// Identical hit maps survive unrelated status/cursor redraws and anchored appends.
#[derive(Default)]
pub(super) struct Display {
    staged: Vec<ScreenHit>,
    shown: Vec<ScreenHit>,
    ready: bool,
    gesture: Gesture,
}

impl Display {
    pub(super) fn stage(&mut self, mut hits: Vec<ScreenHit>) {
        hits.truncate(1024);
        if hits != self.shown {
            self.ready = false;
            self.gesture.cancel();
        }
        self.staged = hits;
    }
    pub(super) fn presented(&mut self) {
        if self.ready {
            return;
        }
        self.shown.clone_from(&self.staged);
        self.ready = true;
    }
    pub(super) fn cancel(&mut self) {
        self.gesture.cancel();
    }
    pub(super) fn invalidate(&mut self) {
        self.ready = false;
        self.staged.clear();
        self.gesture.cancel();
    }
    fn locate(&self, column: u16, row: u16) -> Option<Location> {
        if !self.ready {
            return None;
        }
        self.shown
            .iter()
            .find(|item| item.row == row && (item.hit.start..item.hit.end).contains(&column))
            .map(|item| Location {
                row,
                column,
                event: item.event,
                hit: item.hit.clone(),
            })
    }
    pub(super) fn mouse(
        &mut self,
        button: u16,
        x: u16,
        y: u16,
        release: bool,
        now: Instant,
    ) -> bool {
        if button != 0 {
            let active = self.gesture.active();
            self.gesture.cancel();
            return active && button & 32 != 0;
        }
        let location = self.locate(x, y);
        if release {
            let active = self.gesture.active();
            self.gesture.release(location, now);
            active
        } else if let Some(location) = location {
            self.gesture.press(location, now);
            true
        } else {
            self.gesture.cancel();
            false
        }
    }
    pub(super) fn take(&mut self, now: Instant) -> Option<Arc<str>> {
        let shown = &self.shown;
        let ready = self.ready;
        self.gesture.take(now, |location| {
            ready
                && shown.iter().any(|item| {
                    item.row == location.row
                        && item.event == location.event
                        && item.hit == location.hit
                })
        })
    }
}

/// Opening happens outside the display mutex and never invokes a shell. Keep
/// launcher ownership until wait completes; a browser may legitimately outlive
/// the terminal, so neither its children nor a user's browser are terminated.
pub(super) fn open(raw: &str) -> Result<(), &'static str> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static RUNNING: AtomicUsize = AtomicUsize::new(0);
    struct Permit;
    impl Drop for Permit {
        fn drop(&mut self) {
            RUNNING.fetch_sub(1, Ordering::AcqRel);
        }
    }
    let value = target(raw).ok_or("invalid link")?;
    RUNNING
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < 4).then_some(count + 1)
        })
        .map_err(|_| "too many browser launchers")?;
    let permit = Permit;
    std::thread::Builder::new()
        .name("ssmcp-link-opener".into())
        .spawn(move || {
            let _permit = permit;
            let Ok(url) = Url::parse(&value) else {
                return;
            };
            let opened = crate::client::account::browser_command(&url)
                .spawn()
                .and_then(|mut child| child.wait())
                .is_ok_and(|status| status.success());
            if !opened {
                super::notice(crate::client::render::current_ui().language.text(
                    "Could not open link; check the default browser.",
                    "无法打开链接，请检查默认浏览器。",
                ));
            }
        })
        .map_err(|_| "cannot start browser launcher")?;
    Ok(())
}
