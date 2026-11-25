//! A bounded multiline composer. Editing never dispatches a native command.
use std::ops::Range;
use zgpui::native::{ButtonStyle, Canvas, Event, Rect, Ui};

const MAX_BYTES: usize = 4096;
const LINE_HEIGHT: f32 = 22.0;
const PAD: f32 = 11.0;

#[derive(Debug)]
struct Line {
    start: usize,
    end: usize,
    carets: Vec<(usize, f32)>,
}
impl Line {
    fn x(&self, offset: usize) -> f32 {
        self.carets
            .iter()
            .rev()
            .find(|(at, _)| *at <= offset)
            .map_or(0.0, |(_, x)| *x)
    }
    fn nearest(&self, x: f32) -> usize {
        self.carets
            .iter()
            .min_by(|a, b| (a.1 - x).abs().total_cmp(&(b.1 - x).abs()))
            .map_or(self.start, |(at, _)| *at)
    }
}

fn wrap_lines(text: &str, width: f32, mut measure: impl FnMut(char) -> f32) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut line = Line {
        start: 0,
        end: 0,
        carets: vec![(0, 0.0)],
    };
    let mut x = 0.0;
    for (at, ch) in text.char_indices() {
        if ch == '\n' {
            line.end = at;
            lines.push(line);
            line = Line {
                start: at + 1,
                end: at + 1,
                carets: vec![(at + 1, 0.0)],
            };
            x = 0.0;
            continue;
        }
        let advance = measure(ch).max(0.0);
        if x + advance > width.max(1.0) && at > line.start {
            line.end = at;
            lines.push(line);
            line = Line {
                start: at,
                end: at,
                carets: vec![(at, 0.0)],
            };
            x = 0.0;
        }
        x += advance;
        line.end = at + ch.len_utf8();
        line.carets.push((line.end, x));
    }
    lines.push(line);
    lines
}

fn boundary(text: &str, offset: usize) -> usize {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

#[derive(Default)]
pub struct Composer {
    cursor: usize,
    anchor: Option<usize>,
    focused: bool,
    bounds: Option<Rect>,
    pointer: (f32, f32),
    lines: Vec<Line>,
    first_line: usize,
    visible_lines: usize,
    reveal: bool,
    preferred_x: Option<f32>,
    clipboard: Option<String>,
    submit: bool,
}
impl Composer {
    pub fn focus_end(&mut self, text: &str) {
        self.cursor = text.len();
        self.anchor = None;
        self.focused = true;
        self.preferred_x = None;
        self.reveal = true;
    }
    pub fn focused(&self) -> bool {
        self.focused
    }
    pub fn focus_at(&mut self, text: &str, index: usize) {
        self.focus_end(text);
        self.move_to(text, index, false);
    }
    pub fn blur(&mut self) {
        self.focused = false;
        self.bounds = None;
        self.submit = false;
    }
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }
    pub fn take_submit(&mut self) -> bool {
        std::mem::take(&mut self.submit)
    }
    fn selection(&self) -> Range<usize> {
        let anchor = self.anchor.unwrap_or(self.cursor);
        anchor.min(self.cursor)..anchor.max(self.cursor)
    }
    fn line_index(&self) -> usize {
        self.lines
            .iter()
            .rposition(|line| line.start <= self.cursor)
            .unwrap_or(0)
    }
    fn move_to(&mut self, text: &str, offset: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = boundary(text, offset);
        self.reveal = true;
    }
    fn insert(&mut self, text: &mut String, input: &str) {
        let value: String = input
            .replace("\r\n", "\n")
            .replace('\r', "\n")
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect();
        let range = self.selection();
        if value.is_empty() || text.len() - range.len() + value.len() > MAX_BYTES {
            return;
        }
        text.replace_range(range.clone(), &value);
        self.cursor = range.start + value.len();
        self.anchor = None;
        self.preferred_x = None;
        self.reveal = true;
    }
    fn delete(&mut self, text: &mut String, backward: bool) {
        let mut range = self.selection();
        if range.is_empty() {
            if backward {
                range.start = text[..self.cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(at, _)| at);
            } else if let Some(ch) = text[self.cursor..].chars().next() {
                range.end += ch.len_utf8();
            }
        }
        text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = None;
        self.preferred_x = None;
        self.reveal = true;
    }
    fn vertical(&mut self, text: &str, down: bool, extend: bool) {
        if self.lines.is_empty() {
            return;
        }
        let index = self.line_index();
        let x = self
            .preferred_x
            .unwrap_or_else(|| self.lines[index].x(self.cursor));
        let next = if down {
            (index + 1).min(self.lines.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.move_to(text, self.lines[next].nearest(x), extend);
        self.preferred_x = Some(x);
    }
    /// True means this editor consumed a keyboard/scroll event. Pointer events
    /// still reach Ui so its focus order and release bookkeeping stay intact.
    pub fn event(&mut self, event: &Event, text: &mut String, enabled: bool) -> bool {
        self.cursor = boundary(text, self.cursor);
        self.anchor = self.anchor.map(|at| boundary(text, at));
        match event {
            Event::Move(x, y) | Event::Up(x, y) => self.pointer = (*x, *y),
            Event::Down(x, y) => {
                self.pointer = (*x, *y);
                self.focused = enabled && self.bounds.is_some_and(|rect| rect.contains(*x, *y));
                if self.focused {
                    let rect = self.bounds.unwrap();
                    let row =
                        self.first_line + ((*y - rect.y - PAD).max(0.0) / LINE_HEIGHT) as usize;
                    let at = self
                        .lines
                        .get(row)
                        .map_or(text.len(), |line| line.nearest(*x - rect.x - PAD));
                    self.move_to(text, at, false);
                    self.preferred_x = None;
                }
            }
            Event::Scroll(delta)
                if enabled
                    && self
                        .bounds
                        .is_some_and(|r| r.contains(self.pointer.0, self.pointer.1)) =>
            {
                self.first_line = if *delta > 0 {
                    self.first_line.saturating_sub(2)
                } else {
                    self.first_line
                        .saturating_add(2)
                        .min(self.lines.len().saturating_sub(self.visible_lines))
                };
                self.reveal = false;
                return true;
            }
            Event::Text(value) if self.focused && enabled => {
                self.insert(text, value);
                return true;
            }
            Event::Key {
                code,
                control,
                shift,
                text: value,
            } if self.focused && enabled => {
                if *code == 9 {
                    self.focused = false;
                    return false;
                }
                if *code == 27 {
                    self.focused = false;
                    return true;
                }
                if *control {
                    match *code {
                        13 => self.submit = true,
                        65 | 97 => {
                            self.anchor = Some(0);
                            self.cursor = text.len();
                            self.reveal = true;
                        }
                        67 | 99 | 88 | 120 => {
                            let selection = self.selection();
                            if !selection.is_empty() {
                                self.clipboard = Some(text[selection].to_owned());
                                if matches!(*code, 88 | 120) {
                                    self.delete(text, false);
                                }
                            }
                        }
                        1005 => self.move_to(text, 0, *shift),
                        1006 => self.move_to(text, text.len(), *shift),
                        _ => {}
                    }
                    return true;
                }
                match *code {
                    8 => self.delete(text, true),
                    127 => self.delete(text, false),
                    13 => self.insert(text, "\n"),
                    1001 => {
                        let range = self.selection();
                        let at = if !*shift && !range.is_empty() {
                            range.start
                        } else {
                            text[..self.cursor]
                                .char_indices()
                                .last()
                                .map_or(0, |(at, _)| at)
                        };
                        self.move_to(text, at, *shift);
                        self.preferred_x = None;
                    }
                    1002 => {
                        let range = self.selection();
                        let at = if !*shift && !range.is_empty() {
                            range.end
                        } else {
                            self.cursor
                                + text[self.cursor..].chars().next().map_or(0, char::len_utf8)
                        };
                        self.move_to(text, at, *shift);
                        self.preferred_x = None;
                    }
                    1003 => self.vertical(text, false, *shift),
                    1004 => self.vertical(text, true, *shift),
                    1005 => {
                        let at = self
                            .lines
                            .get(self.line_index())
                            .map_or(0, |line| line.start);
                        self.move_to(text, at, *shift);
                        self.preferred_x = None;
                    }
                    1006 => {
                        let at = self
                            .lines
                            .get(self.line_index())
                            .map_or(text.len(), |line| line.end);
                        self.move_to(text, at, *shift);
                        self.preferred_x = None;
                    }
                    _ => self.insert(text, value),
                }
                return true;
            }
            _ => {}
        }
        false
    }
    pub fn draw(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
        enabled: bool,
    ) {
        self.bounds = Some(rect);
        self.focused &= enabled;
        self.cursor = boundary(text, self.cursor);
        self.anchor = self.anchor.map(|at| boundary(text, at));
        // Register an explicit focus stop. Enter activates it after Tab; clicking
        // or quoting activates it directly without a synthetic keyboard event.
        if ui.button(canvas, 302, rect, "", ButtonStyle::Secondary, enabled) && !self.focused {
            self.focus_end(text);
        }
        if self.focused {
            canvas.rect(rect, 8.0, 0x1a73e8ff);
        }
        canvas.rect(
            rect.inset(2.0),
            6.0,
            if enabled { 0xffffffff } else { 0xf2f4f7ff },
        );
        self.lines = wrap_lines(text, rect.w - PAD * 2.0 - 4.0, |ch| {
            canvas.measure(
                if ch == '\t' {
                    "    ".into()
                } else {
                    ch.to_string()
                }
                .as_str(),
                15,
                false,
            )
        });
        self.visible_lines = ((rect.h - PAD * 2.0) / LINE_HEIGHT).floor().max(1.0) as usize;
        let cursor_line = self.line_index();
        if self.reveal {
            if cursor_line < self.first_line {
                self.first_line = cursor_line;
            }
            if cursor_line >= self.first_line + self.visible_lines {
                self.first_line = cursor_line + 1 - self.visible_lines;
            }
            self.reveal = false;
        }
        self.first_line = self
            .first_line
            .min(self.lines.len().saturating_sub(self.visible_lines));
        let selection = self.selection();
        canvas.clip(rect.inset(8.0));
        for (index, line) in self
            .lines
            .iter()
            .enumerate()
            .skip(self.first_line)
            .take(self.visible_lines)
        {
            let y = rect.y + PAD + (index - self.first_line) as f32 * LINE_HEIGHT;
            if self.focused && selection.start < line.end && selection.end > line.start {
                let x1 = line.x(selection.start.max(line.start));
                let x2 = line.x(selection.end.min(line.end));
                canvas.rect(
                    Rect::new(rect.x + PAD + x1, y - 1.0, (x2 - x1).max(3.0), LINE_HEIGHT),
                    1.0,
                    0xe8f0feff,
                );
            }
            let display = text[line.start..line.end].replace('\t', "    ");
            canvas.text(&display, rect.x + PAD, y, 15, false, 0x253142ff);
            if self.focused && index == cursor_line {
                canvas.rect(
                    Rect::new(
                        rect.x + PAD + line.x(self.cursor),
                        y - 1.0,
                        1.4,
                        LINE_HEIGHT,
                    ),
                    0.0,
                    0x1a73e8ff,
                );
            }
        }
        if text.is_empty() {
            canvas.text(
                "写下新的输入…",
                rect.x + PAD,
                rect.y + PAD,
                15,
                false,
                0x667085ff,
            );
        }
        canvas.unclip();
        if self.lines.len() > self.visible_lines {
            let track = rect.h - 16.0;
            let thumb = (track * self.visible_lines as f32 / self.lines.len() as f32).max(12.0);
            let y = rect.y
                + 8.0
                + (track - thumb) * self.first_line as f32
                    / self.lines.len().saturating_sub(self.visible_lines).max(1) as f32;
            canvas.rect(
                Rect::new(rect.x + rect.w - 6.0, y, 2.0, thumb),
                1.0,
                0xaab4c4ff,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(code: i32, control: bool, shift: bool) -> Event {
        Event::Key {
            code,
            control,
            shift,
            text: String::new(),
        }
    }
    #[test]
    fn multiline_quote_keeps_original_identity_and_draft_through_edit_and_submit() {
        let mut text = ssmcp::client::replies::quote_reply(
            "message_original",
            "中文\n@literal $skill",
            "已有草稿 🦀",
            MAX_BYTES,
        )
        .unwrap();
        let original = text.clone();
        let mut editor = Composer::default();
        editor.focus_end(&text);
        assert!(editor.event(&Event::Text("！".into()), &mut text, true));
        editor.event(&key(13, false, false), &mut text, true);
        editor.event(&Event::Text("下一行".into()), &mut text, true);
        assert_eq!(text, format!("{original}！\n下一行"));
        assert!(!editor.take_submit());
        editor.event(&key(13, true, false), &mut text, true);
        assert!(editor.take_submit());
        assert!(!editor.take_submit());
    }
    #[test]
    fn replacing_selection_uses_remaining_byte_budget_and_keeps_unicode_whole() {
        let mut text = "x".repeat(MAX_BYTES);
        let mut editor = Composer::default();
        editor.focus_end(&text);
        editor.event(&key(65, true, false), &mut text, true);
        editor.event(&Event::Text("中文\r\n🦀".into()), &mut text, true);
        assert_eq!(text, "中文\n🦀");
        let before = text.clone();
        editor.event(&Event::Text("y".repeat(MAX_BYTES)), &mut text, true);
        assert_eq!(text, before);
        editor.event(&key(65, true, false), &mut text, true);
        editor.event(&key(67, true, false), &mut text, true);
        assert_eq!(editor.take_clipboard().as_deref(), Some(before.as_str()));
    }
    #[test]
    fn cursor_editing_preserves_utf8_boundaries() {
        let mut text = "中🦀文".to_owned();
        let mut editor = Composer::default();
        editor.focus_end(&text);
        editor.event(&key(1001, false, false), &mut text, true);
        editor.event(&key(8, false, false), &mut text, true);
        assert_eq!(text, "中文");
        assert_eq!(editor.cursor, "中".len());
        editor.event(&key(1001, false, true), &mut text, true);
        editor.event(&Event::Text("首".into()), &mut text, true);
        assert_eq!(text, "首文");
    }
    #[test]
    fn visual_lines_keep_empty_lines_and_wide_character_boundaries() {
        let text = "a界b\n\nc";
        let lines = wrap_lines(text, 3.0, |ch| if ch == '界' { 2.0 } else { 1.0 });
        assert_eq!(
            lines
                .iter()
                .map(|line| &text[line.start..line.end])
                .collect::<Vec<_>>(),
            ["a界", "b", "", "c"]
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.carets)
                .all(|(at, _)| text.is_char_boundary(*at))
        );
        assert_eq!(lines[0].nearest(2.8), "a界".len());
    }
    #[test]
    fn disabled_editor_does_not_change_or_submit_a_retained_draft() {
        let mut text = "kept".to_owned();
        let mut editor = Composer::default();
        editor.focus_end(&text);
        assert!(!editor.event(&Event::Text("change".into()), &mut text, false));
        assert!(!editor.event(&key(13, true, false), &mut text, false));
        assert_eq!(text, "kept");
        assert!(!editor.take_submit());
    }
}
