//! A bounded multiline composer. Editing never dispatches a native command.
use std::ops::Range;
use zgpui::native::{ButtonStyle, Canvas, Event, Rect, Ui};

const MAX_BYTES: usize = 4096;
const LINE_HEIGHT: f32 = 22.0;
const PAD: f32 = 11.0;
pub const SUGGESTION_HEIGHT: f32 = 36.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub verb: &'static str,
    pub capability: &'static str,
    insertion: &'static str,
    hint: &'static str,
}

pub const SLASH_COMMANDS: [SlashCommand; 2] = [
    SlashCommand {
        verb: "/new",
        capability: "session.new",
        insertion: "/new",
        hint: "新建会话",
    },
    SlashCommand {
        verb: "/cd",
        capability: "session.cd",
        insertion: "/cd ",
        hint: "切换目录",
    },
];

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
    available_commands: [bool; SLASH_COMMANDS.len()],
    suggestion_index: usize,
    suggestions_dismissed: bool,
    suggestion_boxes: Vec<(Rect, SlashCommand)>,
    pressed_suggestion: Option<SlashCommand>,
}
impl Composer {
    pub fn set_available_commands(&mut self, supports: impl Fn(&str) -> bool) {
        self.available_commands = SLASH_COMMANDS.map(|command| supports(command.capability));
    }
    fn suggestions<'a>(&'a self, text: &'a str) -> impl Iterator<Item = SlashCommand> + 'a {
        let active = self.focused
            && !self.suggestions_dismissed
            && self.cursor == text.len()
            && self.selection().is_empty()
            && text.starts_with('/');
        SLASH_COMMANDS
            .into_iter()
            .zip(self.available_commands)
            .filter_map(move |(command, enabled)| {
                (active && enabled && command.verb.starts_with(text) && command.insertion != text)
                    .then_some(command)
            })
    }
    pub fn has_suggestions(&self, text: &str) -> bool {
        self.suggestions(text).next().is_some()
    }
    fn fill_suggestion(&mut self, text: &mut String, command: SlashCommand) {
        text.clear();
        text.push_str(command.insertion);
        self.focus_end(text);
        self.suggestions_dismissed = true;
        self.suggestion_boxes.clear();
    }
    fn suggestion_at(&self, x: f32, y: f32) -> Option<SlashCommand> {
        self.suggestion_boxes
            .iter()
            .find(|(rect, _)| rect.contains(x, y))
            .map(|(_, command)| *command)
    }
    fn suggestion_key(&mut self, code: i32, text: &mut String) -> bool {
        let count = self.suggestions(text).count();
        if count == 0 {
            return false;
        }
        self.suggestion_index = self.suggestion_index.min(count - 1);
        match code {
            1003 => self.suggestion_index = (self.suggestion_index + count - 1) % count,
            1004 => self.suggestion_index = (self.suggestion_index + 1) % count,
            9 | 13 => {
                let command = self
                    .suggestions(text)
                    .nth(self.suggestion_index)
                    .expect("visible suggestion");
                self.fill_suggestion(text, command);
            }
            27 => {
                self.suggestions_dismissed = true;
                self.suggestion_boxes.clear();
            }
            _ => return false,
        }
        true
    }
    pub fn draw_suggestions(
        &mut self,
        canvas: &mut Canvas<'_>,
        ui: &mut Ui,
        rect: Rect,
        text: &str,
    ) {
        self.suggestion_boxes.clear();
        let count = self.suggestions(text).count();
        if count == 0 {
            return;
        }
        self.suggestion_index = self.suggestion_index.min(count - 1);
        let width = (rect.w / count as f32).min(190.);
        for i in 0..count {
            let command = self.suggestions(text).nth(i).expect("visible suggestion");
            let item = Rect::new(
                rect.x + i as f32 * width,
                rect.y,
                (width - 4.).max(1.),
                rect.h,
            );
            self.suggestion_boxes.push((item, command));
            if self.suggestion_index == i {
                canvas.rect(item, 7., 0xe8f0feff);
            }
            ui.button(
                canvas,
                52_000 + i as u64,
                item,
                "",
                ButtonStyle::Quiet,
                true,
            );
            canvas.ellipsis(
                command.verb,
                item.x + 10.,
                item.y + 7.,
                item.w - 16.,
                14,
                true,
                0x202124ff,
            );
            if item.w > 112. {
                canvas.ellipsis(
                    command.hint,
                    item.x + 59.,
                    item.y + 9.,
                    item.w - 67.,
                    12,
                    false,
                    0x5f6368ff,
                );
            }
        }
    }
    pub fn focus_end(&mut self, text: &str) {
        self.suggestions_dismissed = false;
        self.suggestion_index = 0;
        self.cursor = text.len();
        self.anchor = None;
        self.focused = true;
        self.preferred_x = None;
        self.reveal = true;
    }
    pub fn focus_at(&mut self, text: &str, index: usize) {
        self.focus_end(text);
        self.move_to(text, index, false);
    }
    pub fn blur(&mut self) {
        self.focused = false;
        self.bounds = None;
        self.submit = false;
        self.suggestion_boxes.clear();
        self.pressed_suggestion = None;
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
        self.suggestions_dismissed = false;
        self.suggestion_index = 0;
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
        self.suggestions_dismissed = false;
        self.suggestion_index = 0;
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
            Event::Up(x, y) if self.pressed_suggestion.is_some() => {
                let pressed = self.pressed_suggestion.take();
                if enabled
                    && pressed == self.suggestion_at(*x, *y)
                    && let Some(command) = pressed
                    && self.suggestions(text).any(|candidate| candidate == command)
                {
                    self.fill_suggestion(text, command);
                }
                return true;
            }
            Event::Move(x, y) | Event::Up(x, y) => self.pointer = (*x, *y),
            Event::Down(x, y) => {
                self.pointer = (*x, *y);
                if enabled
                    && let Some(command) = self.suggestion_at(*x, *y)
                    && self.suggestions(text).any(|candidate| candidate == command)
                {
                    self.pressed_suggestion = Some(command);
                    return true;
                }
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
                if !*control && !*shift && self.suggestion_key(*code, text) {
                    return true;
                }
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
        placeholder: &str,
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
            canvas.text(&display, rect.x + PAD, y, 15, false, 0x202124ff);
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
                placeholder,
                rect.x + PAD,
                rect.y + PAD,
                15,
                false,
                0x5f6368ff,
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
    fn slash_suggestions_filter_by_capability_and_complete_without_submitting() {
        let mut editor = Composer::default();
        editor.set_available_commands(|_| true);
        let mut draft = "/".to_owned();
        editor.focus_end(&draft);
        assert_eq!(
            editor
                .suggestions(&draft)
                .map(|c| c.verb)
                .collect::<Vec<_>>(),
            ["/new", "/cd"]
        );
        assert!(editor.event(&key(1004, false, false), &mut draft, true));
        assert!(editor.event(&key(13, false, false), &mut draft, true));
        assert_eq!(draft, "/cd ");
        assert!(!editor.take_submit());
        assert!(!editor.has_suggestions(&draft));
        editor.event(&Event::Text("目录 with spaces".into()), &mut draft, true);
        assert_eq!(draft, "/cd 目录 with spaces");
        assert!(!editor.has_suggestions(&draft));

        draft = "/n".to_owned();
        editor.focus_end(&draft);
        editor.set_available_commands(|id| id == "session.cd");
        assert!(!editor.has_suggestions(&draft));
        editor.set_available_commands(|id| id == "session.new");
        assert!(editor.event(&key(9, false, false), &mut draft, true));
        assert_eq!(draft, "/new");
        assert!(!editor.take_submit());
        editor.event(&key(13, true, false), &mut draft, true);
        assert!(editor.take_submit());
    }

    #[test]
    fn escape_and_noncommand_text_preserve_normal_multiline_editing() {
        let mut editor = Composer::default();
        editor.set_available_commands(|_| true);
        let mut draft = "/".to_owned();
        editor.focus_end(&draft);
        assert!(editor.event(&key(27, false, false), &mut draft, true));
        assert!(editor.focused);
        assert!(!editor.has_suggestions(&draft));
        editor.event(&Event::Text("n".into()), &mut draft, true);
        assert!(editor.has_suggestions(&draft));
        editor.event(&key(13, false, true), &mut draft, true);
        assert_eq!(draft, "/n\n");
        for input in ["hello /", "/cd path", "/unknown", "> /new", "/new", " /c"] {
            draft = input.into();
            editor.focus_end(&draft);
            assert!(!editor.has_suggestions(&draft));
        }
        draft = "/cd".into();
        editor.focus_at(&draft, 1);
        assert!(!editor.has_suggestions(&draft));
    }

    #[test]
    fn click_completes_only_the_same_still_available_suggestion() {
        let mut editor = Composer::default();
        let mut draft = "/".to_owned();
        let rect = Rect::new(5., 10., 50., 32.);
        editor.focus_end(&draft);
        editor.set_available_commands(|_| true);
        editor.suggestion_boxes.push((rect, SLASH_COMMANDS[1]));
        assert!(editor.event(&Event::Down(10., 16.), &mut draft, true));
        assert!(editor.event(&Event::Up(100., 96.), &mut draft, true));
        assert_eq!(draft, "/");
        editor.event(&Event::Down(10., 16.), &mut draft, true);
        editor.set_available_commands(|id| id == "session.new");
        editor.event(&Event::Up(10., 16.), &mut draft, true);
        assert_eq!(draft, "/");
        editor.set_available_commands(|_| true);
        editor.event(&Event::Down(10., 16.), &mut draft, true);
        editor.event(&Event::Up(10., 16.), &mut draft, true);
        assert_eq!(draft, "/cd ");
        assert!(!editor.take_submit());
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
