//! Local draft editing and rendering; submission and queue ownership live outside this module.
use super::{completion, ui::Ui};
#[cfg(test)]
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
mod slash;
#[cfg(test)]
mod slash_tests;

pub(super) const MAX_DRAFT_BYTES: usize = 16 * 1024;
const MAX_HISTORY_ENTRIES: usize = 64;
const MAX_HISTORY_BYTES: usize = 256 * 1024;

/// Local input navigation only; accepted requests are never resubmitted from this state.
#[cfg(test)]
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct InputHistory {
    entries: VecDeque<String>,
    index: Option<usize>,
    saved_draft: Option<(String, usize)>,
}

#[cfg(test)]
impl InputHistory {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.entries.len() > MAX_HISTORY_ENTRIES
            || self
                .entries
                .iter()
                .any(|text| text.trim().is_empty() || text.len() > MAX_DRAFT_BYTES)
            || self.entries.iter().map(String::len).sum::<usize>() > MAX_HISTORY_BYTES
            || self.index.is_some_and(|index| index >= self.entries.len())
            || self.index.is_some() != self.saved_draft.is_some()
            || self.saved_draft.as_ref().is_some_and(|(text, cursor)| {
                text.len() > MAX_DRAFT_BYTES || !text.is_char_boundary(*cursor)
            })
        {
            return Err("invalid reload input history".into());
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Suggestion<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub reference: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

pub(super) const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        description: "Show keyboard shortcuts and commands",
    },
    SlashCommand {
        name: "/announcements",
        description: "Read announcements full screen or dismiss a banner",
    },
    SlashCommand {
        name: "/version",
        description: "Show this client's version",
    },
    SlashCommand {
        name: "/update",
        description: "Update the installed client; /update --check checks for updates",
    },
    SlashCommand {
        name: "/about",
        description: "Show application information, license and project links",
    },
    SlashCommand {
        name: "/exit",
        description: "Close this frontend and restore the terminal",
    },
    SlashCommand {
        name: "/quit",
        description: "Close this frontend (alias of /exit)",
    },
    SlashCommand {
        name: "/reload",
        description: "Restart this client with a fresh live session",
    },
    SlashCommand {
        name: "/restart",
        description: "Restart this client session (alias of /reload)",
    },
    SlashCommand {
        name: "/command",
        description: "View complete command and script: /command [JOB_ID]",
    },
    SlashCommand {
        name: "/chsh",
        description: "Show or choose the shell for new terminals and ! commands",
    },
    SlashCommand {
        name: "/tasks",
        description: "Open Tasks beside Logs; browse jobs or cancel an exact ID",
    },
    SlashCommand {
        name: "/goal",
        description: "Current conversation goal: /goal [set <text> | clear]",
    },
    SlashCommand {
        name: "/openai",
        description: "Show the saved conversation's observed OpenAI session and subject",
    },
    SlashCommand {
        name: "/openai-session",
        description: "Show only the observed openai/session value",
    },
    SlashCommand {
        name: "/openai-subject",
        description: "Show only the observed openai/subject value",
    },
    SlashCommand {
        name: "/chatgpt",
        description: "Saved ChatGPT conversation URL: set <URL>, open, copy, clear",
    },
    SlashCommand {
        name: "/plugin",
        description: "Manage local native plugins: list, load, unload or help",
    },
    SlashCommand {
        name: "/plugin-docs",
        description: "Open the official OpenAI Plugins reference in your browser",
    },
    SlashCommand {
        name: "/queue",
        description: "Show queued requests",
    },
    SlashCommand {
        name: "/send",
        description: "发送给当前选定的监听程序、原生收件人或群组：/send <正文>",
    },
    SlashCommand {
        name: "/wait",
        description: "查看计时和用户等待面板；/wait status 读取快照；/wait cancel <编号> 只取消对应等待，会话继续",
    },
    SlashCommand {
        name: "/audit",
        description: "Open code audit",
    },
    SlashCommand {
        name: "/diff",
        description: "Choose diff layout: single or split; narrow screens use single",
    },
    SlashCommand {
        name: "/allow",
        description: "List session access; /allow add USER grants access to this session",
    },
    SlashCommand {
        name: "/logs",
        description: "Open the Logs canvas panel; Esc returns to the previous panel",
    },
    SlashCommand {
        name: "/raw",
        description: "Toggle the diagnostic log panel",
    },
    SlashCommand {
        name: "/new",
        description: "Open a new session in the current directory",
    },
    SlashCommand {
        name: "/cd",
        description: "Change this session’s current directory",
    },
    SlashCommand {
        name: "/close",
        description: "Close this canvas panel; keep the session and running work",
    },
    SlashCommand {
        name: "/questions",
        description: "Review pending questions",
    },
    SlashCommand {
        name: "/filter",
        description: "Filter logs by text",
    },
    SlashCommand {
        name: "/theme",
        description: "Choose a display theme",
    },
    SlashCommand {
        name: "/sidebar",
        description: "Toggle sidebar",
    },
    SlashCommand {
        name: "/clear",
        description: "Clear visible logs",
    },
    SlashCommand {
        name: "/sessions",
        description: "Browse session processes and threads, full IDs and group selection",
    },
    SlashCommand {
        name: "/group",
        description: "Select any exact recipients; send, save, inspect or retry a group",
    },
    SlashCommand {
        name: "/message",
        description: "Choose a verified agent or account inbox for a native text message",
    },
    SlashCommand {
        name: "/paste",
        description: "Import clipboard text, image or desktop file selection",
    },
    SlashCommand {
        name: "/paste-retry",
        description: "Retry the retained paste; /paste-retry discard explicitly clears it",
    },
    SlashCommand {
        name: "/attach",
        description: "Reference a workspace file or import selected local files",
    },
    SlashCommand {
        name: "/select",
        description: "Freeze display for terminal mouse selection; F7/Esc returns",
    },
    SlashCommand {
        name: "/copy",
        description: "Copy the latest agent reply",
    },
    SlashCommand {
        name: "/invite",
        description: "Copy a coordinator invitation; /invite worker recruits an external worker",
    },
    SlashCommand {
        name: "/worker",
        description: "Worker stations: ls | +N | -N | retire #NN",
    },
    SlashCommand {
        name: "/track-command",
        description: "Run a local command with Linux task artifact accounting",
    },
    SlashCommand {
        name: "/handoff",
        description: "Ask the current agent for a handoff prompt and copy it",
    },
    SlashCommand {
        name: "/handoff-export",
        description: "Prepare a context file and editable handoff draft",
    },
];

/// Cursor coordinates are zero based and relative to `lines`.
#[derive(Clone, Debug, Default)]
pub(super) struct ComposerRows {
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Exact reference names for visible menu rows, before display clipping.
    pub references: Vec<(u16, String)>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Composer {
    text: String,
    cursor: usize,
    revision: u64,
    preferred_column: Option<usize>,
    history: VecDeque<String>,
    history_bytes: usize,
    history_index: Option<usize>,
    saved_draft: Option<(String, usize)>,
    suggestion_index: usize,
    suggestions_dismissed: bool,
    reference_query: Option<completion::Query>,
    reference_input: Option<(u64, usize, bool)>,
    reference_version: Option<completion::Version>,
    references: completion::Matches,
}

impl Composer {
    pub(super) fn text(&self) -> &str {
        &self.text
    }
    pub(super) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
    /// A byte offset that always falls on a UTF-8 boundary.
    pub(super) fn cursor(&self) -> usize {
        self.cursor
    }
    pub(super) fn revision(&self) -> u64 {
        self.revision
    }

    /// Reject oversized edits as a whole so a submitted request is never silently truncated.
    pub(super) fn insert(&mut self, text: &str) -> bool {
        if text.len() > MAX_DRAFT_BYTES.saturating_sub(self.text.len()) {
            return false;
        }
        if !text.is_empty() {
            self.detach_history();
            self.text.insert_str(self.cursor, text);
            self.cursor += text.len();
        }
        true
    }

    /// Normalize pasted line endings while preserving other request content.
    pub(super) fn paste(&mut self, text: &str) -> bool {
        if !text.contains('\r') {
            return self.insert(text);
        }
        let remaining = MAX_DRAFT_BYTES.saturating_sub(self.text.len());
        let mut normalized = String::with_capacity(text.len().min(remaining));
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            let ch = if ch == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                '\n'
            } else {
                ch
            };
            if normalized.len() + ch.len_utf8() > remaining {
                return false;
            }
            normalized.push(ch);
        }
        self.insert(&normalized)
    }

    pub(super) fn set_text(&mut self, text: &str) -> bool {
        if text.len() > MAX_DRAFT_BYTES {
            return false;
        }
        self.detach_history();
        self.text.clear();
        self.text.push_str(text);
        self.cursor = self.text.len();
        true
    }

    /// Draining a draft does not mark it as submitted; call `remember` after queue acceptance.
    pub(super) fn take_text(&mut self) -> String {
        self.detach_history();
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    pub(super) fn backspace(&mut self) -> bool {
        let Some((start, _)) = self.text[..self.cursor].char_indices().next_back() else {
            return false;
        };
        self.remove(start, self.cursor)
    }

    pub(super) fn delete(&mut self) -> bool {
        let Some(ch) = self.text[self.cursor..].chars().next() else {
            return false;
        };
        self.remove(self.cursor, self.cursor + ch.len_utf8())
    }

    pub(super) fn left(&mut self) -> bool {
        self.preferred_column = None;
        let Some((cursor, _)) = self.text[..self.cursor].char_indices().next_back() else {
            return false;
        };
        self.cursor = cursor;
        true
    }

    pub(super) fn right(&mut self) -> bool {
        self.preferred_column = None;
        let Some(ch) = self.text[self.cursor..].chars().next() else {
            return false;
        };
        self.cursor += ch.len_utf8();
        true
    }

    /// Home / Ctrl+A move to the beginning of the current logical line.
    pub(super) fn home(&mut self) -> bool {
        self.preferred_column = None;
        let cursor = self.line_start();
        let changed = cursor != self.cursor;
        self.cursor = cursor;
        changed
    }

    /// End / Ctrl+E move to the end of the current logical line.
    pub(super) fn end(&mut self) -> bool {
        self.preferred_column = None;
        let cursor = self.line_end();
        let changed = cursor != self.cursor;
        self.cursor = cursor;
        changed
    }

    /// Move by rendered rows, returning false only at the first navigable row.
    pub(super) fn up(&mut self, width: u16) -> bool {
        self.vertical(width, true)
    }

    /// Move by rendered rows, returning false only at the last navigable row.
    pub(super) fn down(&mut self, width: u16) -> bool {
        self.vertical(width, false)
    }

    fn vertical(&mut self, width: u16, up: bool) -> bool {
        if width == 0 {
            return false;
        }
        let (_, content_width) = editor_columns(usize::from(width));
        let mut positions = Vec::new();
        let _ = self.wrapped(content_width, |offset, row, line| {
            positions.push((offset, row, line.width()));
        });
        let Some(&(_, row, column)) = positions
            .iter()
            .find(|(offset, _, _)| *offset == self.cursor)
        else {
            return false;
        };
        let target_row = if up {
            positions
                .iter()
                .rev()
                .find(|(_, candidate, _)| *candidate < row)
        } else {
            positions.iter().find(|(_, candidate, _)| *candidate > row)
        };
        let Some(&(_, target_row, _)) = target_row else {
            return false;
        };
        let column = self.preferred_column.unwrap_or(column);
        // Tabs may occupy multiple rows but remain one input character. Only
        // real UTF-8 boundaries are cursor stops, so skip tab-only visual rows.
        let offset = positions
            .iter()
            .rev()
            .find(|(_, row, candidate)| *row == target_row && *candidate <= column)
            .or_else(|| positions.iter().find(|(_, row, _)| *row == target_row))
            .map(|(offset, _, _)| *offset)
            .expect("the target row contains a cursor stop");
        self.cursor = offset;
        self.preferred_column = Some(column);
        true
    }

    /// Ctrl+U removes the part of this logical line before the cursor.
    pub(super) fn clear_before(&mut self) -> bool {
        self.remove(self.line_start(), self.cursor)
    }

    /// Ctrl+K removes to the end of the line, or joins the next line at its end.
    pub(super) fn clear_after(&mut self) -> bool {
        let end = self.line_end();
        let end = if end == self.cursor && end < self.text.len() {
            end + 1
        } else {
            end
        };
        self.remove(self.cursor, end)
    }

    /// Ctrl+W removes preceding whitespace and then the preceding word.
    pub(super) fn delete_word(&mut self) -> bool {
        let mut start = self.cursor;
        let mut word = false;
        for (offset, ch) in self.text[..self.cursor].char_indices().rev() {
            if word && ch.is_whitespace() {
                break;
            }
            word |= !ch.is_whitespace();
            start = offset;
        }
        self.remove(start, self.cursor)
    }

    /// Keep accepted requests locally, without changing the current draft.
    pub(super) fn remember(&mut self, text: &str) {
        if text.trim().is_empty() || text.len() > MAX_DRAFT_BYTES {
            return;
        }
        self.history_index = None;
        self.saved_draft = None;
        if self.history.back().is_some_and(|previous| previous == text) {
            return;
        }
        self.history.push_back(text.to_owned());
        self.history_bytes += text.len();
        while self.history.len() > MAX_HISTORY_ENTRIES || self.history_bytes > MAX_HISTORY_BYTES {
            if let Some(removed) = self.history.pop_front() {
                self.history_bytes -= removed.len();
            }
        }
    }

    #[cfg(test)]
    pub(super) fn reload_history(&self) -> InputHistory {
        InputHistory {
            entries: self.history.clone(),
            index: self.history_index,
            saved_draft: self.saved_draft.clone(),
        }
    }

    #[cfg(test)]
    pub(super) fn restore_reload_history(&mut self, history: InputHistory) -> Result<(), String> {
        history.validate()?;
        self.history_bytes = history.entries.iter().map(String::len).sum();
        self.history = history.entries;
        // The outbox owns the current draft. Resume browsing only when that draft
        // still matches the saved entry; a newer draft must win.
        self.history_index = history
            .index
            .filter(|index| self.history[*index] == self.text);
        self.saved_draft = self.history_index.and(history.saved_draft);
        Ok(())
    }

    pub(super) fn history_previous(&mut self) -> bool {
        let index = match self.history_index {
            Some(0) => return false,
            Some(index) => index - 1,
            None => {
                let Some(index) = self.history.len().checked_sub(1) else {
                    return false;
                };
                self.saved_draft = Some((self.text.clone(), self.cursor));
                index
            }
        };
        self.text.clone_from(&self.history[index]);
        self.cursor = self.text.len();
        self.preferred_column = None;
        self.history_index = Some(index);
        self.reset_suggestions();
        true
    }

    pub(super) fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            self.text.clone_from(&self.history[index + 1]);
            self.cursor = self.text.len();
        } else {
            self.history_index = None;
            let (text, cursor) = self.saved_draft.take().unwrap_or_default();
            self.text = text;
            self.cursor = cursor;
        }
        self.preferred_column = None;
        self.reset_suggestions();
        true
    }

    pub(super) fn suggestions(&self) -> Vec<Suggestion<'_>> {
        if self.suggestions_dismissed {
            return Vec::new();
        }
        if self.reference_active() {
            return self
                .references
                .items
                .iter()
                .map(|item| Suggestion {
                    name: &item.name,
                    description: &item.description,
                    reference: true,
                })
                .collect();
        }
        let Some(query) = slash::query(&self.text, self.cursor) else {
            return Vec::new();
        };
        query
            .commands()
            .iter()
            .filter(|command| query.matches(command))
            .map(|command| Suggestion {
                name: command.name,
                description: command.description,
                reference: false,
            })
            .collect()
    }

    pub(super) fn selected_suggestion(&self) -> Option<Suggestion<'_>> {
        let suggestions = self.suggestions();
        suggestions
            .get(
                self.suggestion_index
                    .min(suggestions.len().saturating_sub(1)),
            )
            .copied()
    }

    pub(super) fn refresh_completions(&mut self, enabled: bool) {
        self.refresh_completions_with(enabled, completion::lookup);
    }

    /// Enter keeps the existing first-word shortcut. Merely showing a secondary
    /// menu must not select a verb or discard arguments from the actual draft.
    pub(super) fn primary_command_suggestion(&self) -> bool {
        let text = self.text.trim_start();
        text.starts_with('/') && !text.chars().any(char::is_whitespace)
    }

    fn refresh_completions_with(
        &mut self,
        enabled: bool,
        lookup: impl FnOnce(
            &completion::Query,
            Option<completion::Version>,
        ) -> Option<(completion::Version, completion::Matches)>,
    ) {
        let input = (self.revision, self.cursor, enabled);
        if self.reference_input != Some(input) {
            self.reference_input = Some(input);
            let query = enabled
                .then(|| completion::query(&self.text, self.cursor))
                .flatten();
            if query != self.reference_query {
                self.reference_query = query;
                self.reference_version = None;
                self.references = completion::Matches::default();
                self.suggestion_index = 0;
                self.suggestions_dismissed = false;
            }
        }
        if !self.suggestions_dismissed
            && let Some(query) = &self.reference_query
            && let Some((version, references)) = lookup(query, self.reference_version)
        {
            let selected = self
                .references
                .items
                .get(self.suggestion_index)
                .map(|item| item.name.clone());
            self.reference_version = Some(version);
            self.references = references;
            self.suggestion_index = selected
                .and_then(|name| {
                    self.references
                        .items
                        .iter()
                        .position(|item| item.name == name)
                })
                .unwrap_or_else(|| {
                    self.suggestion_index
                        .min(self.references.items.len().saturating_sub(1))
                });
        }
    }

    pub(super) fn reference_active(&self) -> bool {
        !self.suggestions_dismissed
            && self.reference_query.is_some()
            && (self.reference_input == Some((self.revision, self.cursor, true))
                || completion::query(&self.text, self.cursor) == self.reference_query)
    }

    pub(super) fn completion_loading(&self) -> bool {
        self.reference_active() && self.references.loading
    }

    pub(super) fn accept_suggestion(&mut self) -> Result<bool, String> {
        let Some(selected) = self.selected_suggestion() else {
            return Ok(false);
        };
        if selected.reference {
            let query = self.reference_query.as_ref().unwrap();
            let index = self
                .suggestion_index
                .min(self.references.items.len().saturating_sub(1));
            let (text, cursor) = completion::apply(
                &self.text,
                self.cursor,
                query,
                &self.references.items[index].replacement,
                MAX_DRAFT_BYTES,
            )?;
            self.detach_history();
            self.text = text;
            self.cursor = cursor;
        } else {
            let (text, cursor) = slash::apply(&self.text, self.cursor, selected.name)?;
            self.detach_history();
            self.text = text;
            self.cursor = cursor;
        }
        Ok(true)
    }

    pub(super) fn accept_reference(&mut self, name: &str) -> Result<bool, String> {
        if !self.reference_active() {
            return Err("The input changed; choose a current suggestion".into());
        }
        let index = self
            .references
            .items
            .iter()
            .position(|item| item.name == name)
            .ok_or("The reference suggestions changed; choose a current file or skill")?;
        self.suggestion_index = index;
        self.accept_suggestion()
    }

    pub(super) fn move_suggestion(&mut self, delta: isize) -> bool {
        let count = self.suggestions().len();
        if count == 0 {
            return false;
        }
        self.suggestion_index =
            (self.suggestion_index as isize + delta).rem_euclid(count as isize) as usize;
        true
    }

    pub(super) fn dismiss_suggestions(&mut self) {
        self.suggestions_dismissed = true;
    }

    pub(super) fn menu_visible(&self, width: u16, max_height: u16) -> bool {
        if width == 0 || max_height == 0 || self.suggestions().is_empty() {
            return false;
        }
        if self.reference_active() {
            return max_height >= 2;
        }
        let (_, content_width) = editor_columns(usize::from(width));
        self.wrapped(content_width, |_, _, _| {}).len() < usize::from(max_height)
    }

    /// Render only safe text and validated UI colors. The viewport always contains the cursor.
    pub(super) fn rows(
        &self,
        width: u16,
        max_height: u16,
        ui: &Ui,
        show_suggestions: bool,
    ) -> ComposerRows {
        if width == 0 || max_height == 0 {
            return ComposerRows::default();
        }
        let width = usize::from(width);
        let height = usize::from(max_height);
        let (prefix_width, content_width) = editor_columns(width);
        let mut cursor = (0, 0);
        let wrapped = self.wrapped(content_width, |offset, row, line| {
            if offset == self.cursor {
                cursor = (row, line.width());
            }
        });
        let (cursor_row, cursor_col) = cursor;
        let suggestions = if show_suggestions {
            self.suggestions()
        } else {
            Vec::new()
        };
        let available = if self.reference_active() {
            height.saturating_sub(1)
        } else {
            height.saturating_sub(wrapped.len().min(height))
        };
        let notice = (show_suggestions && self.reference_active())
            .then_some(self.references.notice.as_deref())
            .flatten();
        let notice_rows =
            usize::from(notice.is_some() && (suggestions.is_empty() || available > 1))
                .min(available);
        let hint_count = suggestions
            .len()
            .min(3)
            .min(available.saturating_sub(notice_rows));
        let visible_height = height - hint_count - notice_rows;
        let first = cursor_row
            .saturating_sub(visible_height / 2)
            .min(wrapped.len().saturating_sub(visible_height));
        let last = (first + visible_height).min(wrapped.len());
        let mut lines = Vec::with_capacity(last - first + hint_count);
        for (index, body) in wrapped[first..last].iter().enumerate() {
            let prompt = match (prefix_width, index) {
                (2, 0) => "❯ ",
                (1, 0) => "❯",
                (2, _) => "  ",
                (1, _) => " ",
                _ => "",
            };
            let (body, tone) = if self.text.is_empty() {
                ("Message the agent", "muted")
            } else {
                (body.as_str(), "foreground")
            };
            lines.push(paint_row(&[(prompt, "accent"), (body, tone)], width, ui));
        }
        let selected = self
            .suggestion_index
            .min(suggestions.len().saturating_sub(1));
        let suggestion_start = selected.saturating_sub(hint_count.saturating_sub(1));
        let mut reference_rows = Vec::new();
        for (index, command) in suggestions
            .iter()
            .enumerate()
            .skip(suggestion_start)
            .take(hint_count)
        {
            if command.reference {
                reference_rows.push((lines.len() as u16, command.name.to_owned()));
            }
            lines.push(paint_row(
                &[
                    (if index == selected { "› " } else { "  " }, "accent"),
                    (command.name, "accent"),
                    ("  ", "muted"),
                    (command.description, "muted"),
                ],
                width,
                ui,
            ));
        }
        if notice_rows > 0 {
            lines.push(paint_row(&[(notice.unwrap_or(""), "muted")], width, ui));
        }
        ComposerRows {
            lines,
            cursor_row: (cursor_row - first) as u16,
            cursor_col: (prefix_width + cursor_col).min(width - 1) as u16,
            references: reference_rows,
        }
    }

    fn remove(&mut self, start: usize, end: usize) -> bool {
        if start == end {
            return false;
        }
        self.detach_history();
        self.text.replace_range(start..end, "");
        self.cursor = start;
        true
    }

    fn detach_history(&mut self) {
        self.history_index = None;
        self.saved_draft = None;
        self.preferred_column = None;
        self.reset_suggestions();
    }

    fn reset_suggestions(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.suggestion_index = 0;
        self.suggestions_dismissed = false;
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1)
    }
    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |offset| self.cursor + offset)
    }

    /// Rendering and movement share wrapping, including replacement glyphs,
    /// expanded tabs, leading combining marks and the reserved final cursor cell.
    fn wrapped(&self, width: usize, mut cursor_at: impl FnMut(usize, usize, &str)) -> Vec<String> {
        let mut lines = vec![String::new()];
        let mut used = 0;
        for (offset, ch) in self.text.char_indices() {
            if ch == '\n' {
                cursor_at(offset, lines.len() - 1, lines.last().unwrap());
                lines.push(String::new());
                used = 0;
                continue;
            }
            let visible = if ch == '\t' {
                " ".repeat(4 - used % 4)
            } else {
                safe_char(ch).to_string()
            };
            for (index, mut ch) in visible.chars().enumerate() {
                let mut cells = ch.width().unwrap_or(0);
                if cells > width {
                    ch = '�';
                    cells = 1;
                }
                if used + cells > width {
                    lines.push(String::new());
                    used = 0;
                }
                if index == 0 {
                    cursor_at(offset, lines.len() - 1, lines.last().unwrap());
                }
                if cells == 0 && used == 0 {
                    lines.last_mut().unwrap().push('◌');
                    used = 1;
                }
                lines.last_mut().unwrap().push(ch);
                used += cells;
            }
        }
        cursor_at(self.text.len(), lines.len() - 1, lines.last().unwrap());
        lines
    }
}

fn safe_char(ch: char) -> char {
    if ch.is_control()
        || matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    {
        '�'
    } else {
        ch
    }
}

fn editor_columns(width: usize) -> (usize, usize) {
    let prefix_width = if width >= 4 {
        2
    } else if width == 3 {
        1
    } else {
        0
    };
    // Leave a trailing cursor cell instead of depending on terminal autowrap.
    (prefix_width, width.saturating_sub(prefix_width + 1).max(1))
}

fn paint_row(parts: &[(&str, &str)], width: usize, ui: &Ui) -> String {
    let mut result = String::new();
    let mut used = 0;
    if ui.color {
        result.push_str(&format!(
            "\x1b[{};{}m",
            ui.input_background(),
            ui.color("foreground")
        ));
    }
    for (part, tone) in parts {
        if ui.color {
            result.push_str(&format!("\x1b[{}m", ui.color(tone)));
        }
        let mut text = String::new();
        let mut part_width = 0;
        for ch in part.chars().map(safe_char) {
            let cells = ch.width().unwrap_or(0);
            if used + part_width + cells > width {
                break;
            }
            text.push(ch);
            part_width += cells;
        }
        used += text.width();
        result.push_str(&text);
        if used >= width {
            break;
        }
    }
    result.push_str(&" ".repeat(width.saturating_sub(used)));
    if ui.color {
        result.push_str("\x1b[0m");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_preserves_utf8_and_never_silently_truncates_a_request() {
        let mut draft = Composer::default();
        assert!(draft.insert("修复 café"));
        assert!(draft.left());
        assert!(draft.backspace());
        assert_eq!(draft.text(), "修复 caé");
        assert!(draft.delete());
        assert_eq!(draft.text(), "修复 ca");
        assert!(draft.home());
        assert!(draft.right());
        assert_eq!(draft.cursor(), "修".len());
        assert!(draft.insert("好"));
        assert_eq!(draft.text(), "修好复 ca");
        assert!(draft.set_text(&"界".repeat(MAX_DRAFT_BYTES / 3)));
        let before = draft.text().to_owned();
        assert!(!draft.insert("界"));
        assert!(!draft.set_text(&"x".repeat(MAX_DRAFT_BYTES + 1)));
        assert_eq!(draft.text(), before);
    }

    #[test]
    fn multiline_movement_retains_display_column_and_line_editing_is_local() {
        let mut draft = Composer::default();
        assert!(draft.paste("abcde\r\nx\r界界z"));
        assert_eq!(draft.text(), "abcde\nx\n界界z");
        assert!(draft.up(120));
        assert_eq!(&draft.text()[..draft.cursor()], "abcde\nx");
        assert!(draft.up(120));
        assert_eq!(draft.cursor(), 5);
        assert!(!draft.up(120));
        assert!(draft.down(120));
        assert!(draft.down(120));
        assert_eq!(draft.cursor(), draft.text().len());
        assert!(draft.clear_before());
        assert_eq!(draft.text(), "abcde\nx\n");
        assert!(draft.up(120));
        assert!(draft.clear_after());
        assert_eq!(draft.text(), "abcde\n\n");
        assert!(draft.clear_after());
        assert_eq!(draft.text(), "abcde\n");
        assert!(draft.set_text("keep 前一个词   "));
        assert!(draft.delete_word());
        assert_eq!(draft.text(), "keep ");
    }

    #[test]
    fn vertical_movement_follows_soft_wrapping_and_preserves_the_cursor_column() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let mut draft = Composer::default();
        draft.set_text("abcdefghi");
        // Seven physical columns leave four text cells and a final cursor cell.
        assert_eq!(draft.rows(7, 10, &ui, true).cursor_row, 2);
        assert!(draft.up(7));
        assert_eq!(draft.cursor(), 5);
        assert_eq!(draft.rows(7, 10, &ui, true).cursor_row, 1);
        assert!(draft.up(7));
        assert_eq!(draft.cursor(), 1);
        assert!(!draft.up(7));
        assert!(draft.down(7));
        assert_eq!(draft.cursor(), 5);
        assert!(draft.down(7));
        assert_eq!(draft.cursor(), 9);
        assert!(!draft.down(7));

        // EOF occupies the reserved cell; an interior soft-wrap boundary is
        // instead the first cursor stop of the following row.
        draft.set_text("abcdefgh");
        assert_eq!(draft.rows(7, 10, &ui, true).cursor_col, 6);
        assert!(draft.up(7));
        assert_eq!(draft.cursor(), 3);
        assert!(draft.down(7));
        assert_eq!(draft.cursor(), 8);
        draft.set_text("abcd\nefgh");
        assert!(draft.up(7));
        assert_eq!(
            draft.cursor(),
            4,
            "a hard newline is a real end-of-row stop"
        );
        assert!(draft.down(7));
        assert_eq!(draft.cursor(), 9);
    }

    #[test]
    fn vertical_movement_matches_rendered_unicode_tabs_and_narrow_rows() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let mut draft = Composer::default();
        draft.set_text("\tx");
        assert_eq!(draft.rows(1, 20, &ui, true).cursor_row, 4);
        assert!(draft.up(1));
        assert_eq!(
            draft.cursor(),
            0,
            "expanded tab cells are not input offsets"
        );
        assert!(draft.down(1));
        assert_eq!(draft.cursor(), 2);
        draft.set_text("\u{301}ab");
        assert!(draft.up(5));
        assert_eq!(draft.cursor(), "\u{301}".len());
        assert!(draft.down(5));
        assert_eq!(draft.cursor(), draft.text().len());

        let text = "\t界\u{301}x\n\u{301}ab";
        for width in [1, 2, 3, 4, 7, 12] {
            draft.set_text(text);
            let mut row = draft.rows(width, 100, &ui, true).cursor_row;
            while draft.up(width) {
                assert!(text.is_char_boundary(draft.cursor()));
                let next = draft.rows(width, 100, &ui, true).cursor_row;
                assert!(
                    next < row,
                    "Up must move to an earlier rendered row at width {width}"
                );
                row = next;
            }
            assert_eq!(row, 0);
            while draft.down(width) {
                assert!(text.is_char_boundary(draft.cursor()));
                let next = draft.rows(width, 100, &ui, true).cursor_row;
                assert!(
                    next > row,
                    "Down must move to a later rendered row at width {width}"
                );
                row = next;
            }
            assert_eq!(draft.cursor(), text.len());
            assert_eq!(draft.text(), text);
        }
        assert!(!draft.up(0));
        assert!(!draft.down(0));
    }

    #[test]
    fn command_menu_tracks_selection_and_can_be_dismissed_without_editing() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let mut draft = Composer::default();
        draft.set_text("/re");
        assert!(draft.menu_visible(60, 3));
        assert!(!draft.menu_visible(60, 1));
        assert_eq!(draft.selected_suggestion().unwrap().name, "/reload");
        assert!(draft.move_suggestion(1));
        assert_eq!(draft.selected_suggestion().unwrap().name, "/restart");
        assert!(
            draft
                .rows(60, 3, &ui, true)
                .lines
                .iter()
                .any(|line| line.contains("› /restart"))
        );
        assert_eq!(draft.rows(60, 3, &ui, false).lines.len(), 1);
        draft.dismiss_suggestions();
        assert!(!draft.menu_visible(60, 3));
        assert!(!draft.move_suggestion(1));
        assert_eq!(draft.text(), "/re");
        draft.set_text("/");
        assert!(draft.move_suggestion(-1));
        assert_eq!(draft.selected_suggestion().unwrap().name, "/handoff-export");
        assert!(
            draft
                .rows(60, 3, &ui, true)
                .lines
                .iter()
                .any(|line| line.contains("› /handoff-export"))
        );
        draft.set_text("/handoff");
        assert_eq!(draft.selected_suggestion().unwrap().name, "/handoff");
        draft.set_text("/");
        draft.insert("au");
        assert_eq!(draft.selected_suggestion().unwrap().name, "/audit");
    }

    #[test]
    fn reload_history_validates_utf8_and_keeps_a_newer_outbox_draft() {
        let mut composer = Composer::default();
        composer.remember("accepted");
        composer.set_text("草稿");
        composer.history_previous();
        let saved = composer.reload_history();
        let mut restored = Composer::default();
        restored.set_text("newer unsent draft");
        restored.restore_reload_history(saved.clone()).unwrap();
        assert_eq!(restored.text(), "newer unsent draft");
        assert!(!restored.history_next());
        assert!(restored.history_previous());
        assert_eq!(restored.text(), "accepted");
        assert!(restored.history_next());
        assert_eq!(restored.text(), "newer unsent draft");

        let mut invalid = saved;
        invalid.saved_draft = Some(("界".into(), 1));
        assert!(
            invalid.validate().is_err(),
            "a UTF-8 cursor cannot split a code point"
        );
    }

    #[test]
    fn history_restores_draft_and_cursor_without_mutating_accepted_requests() {
        let mut draft = Composer::default();
        draft.remember("first");
        draft.remember("second");
        draft.remember("second");
        assert_eq!(draft.history.len(), 2);
        draft.set_text("unfinished");
        draft.left();
        let cursor = draft.cursor();
        assert!(draft.history_previous());
        assert_eq!(draft.text(), "second");
        assert!(draft.history_previous());
        assert_eq!(draft.text(), "first");
        assert!(!draft.history_previous());
        assert!(draft.history_next());
        assert!(draft.history_next());
        assert_eq!(draft.text(), "unfinished");
        assert_eq!(draft.cursor(), cursor);
        assert!(draft.history_previous());
        draft.insert(" revised");
        assert!(draft.history_previous());
        assert_eq!(draft.text(), "second");
        assert!(draft.history_next());
        assert_eq!(draft.text(), "second revised");
        assert_eq!(draft.take_text(), "second revised");
        assert!(draft.is_empty());
        assert_eq!(draft.history.len(), 2);
    }

    #[test]
    fn history_is_bounded_by_both_entries_and_bytes() {
        let mut draft = Composer::default();
        for index in 0..100 {
            draft.remember(&format!("request {index}"));
        }
        assert_eq!(draft.history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(draft.history.front().unwrap(), "request 36");
        for index in 0..100 {
            draft.remember(&format!("{index}{}", "界".repeat(5000)));
        }
        assert!(draft.history_bytes <= MAX_HISTORY_BYTES);
        assert!(draft.history.len() < MAX_HISTORY_ENTRIES);
        assert_eq!(
            draft.history_bytes,
            draft.history.iter().map(String::len).sum::<usize>()
        );
    }

    #[test]
    fn rendering_keeps_cursor_visible_and_never_emits_pasted_controls() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let mut draft = Composer::default();
        draft.set_text("界e\u{301}\t\x1b]0;injected\x07\u{202e}\nnext\nlast");
        for width in [1, 2, 3, 4, 12, 40] {
            let rendered = draft.rows(width, 2, &ui, true);
            assert!(!rendered.lines.is_empty());
            assert!(rendered.lines.len() <= 2);
            assert!(usize::from(rendered.cursor_row) < rendered.lines.len());
            assert!(rendered.cursor_col < width);
            for row in rendered.lines {
                assert_eq!(row.width(), usize::from(width), "{row:?}");
                assert!(!row.chars().any(char::is_control));
                assert!(!row.contains('\u{202e}'));
            }
        }
        draft.home();
        draft.up(120);
        draft.up(120);
        let rendered = draft.rows(12, 2, &ui, true);
        assert!(rendered.lines[0].starts_with("❯ "));
        assert_eq!(rendered.cursor_row, 0);
        assert!(draft.rows(0, 2, &ui, true).lines.is_empty());
        assert!(draft.rows(12, 0, &ui, true).lines.is_empty());
    }

    #[test]
    fn slash_suggestions_are_local_and_stop_when_arguments_begin() {
        let ui = Ui {
            color: false,
            ..Ui::default()
        };
        let mut draft = Composer::default();
        draft.set_text("/");
        assert_eq!(draft.suggestions().len(), SLASH_COMMANDS.len());
        draft.set_text("/re");
        assert_eq!(
            draft
                .suggestions()
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            vec!["/reload", "/restart"]
        );
        let rendered = draft.rows(60, 4, &ui, true);
        assert_eq!(rendered.lines.len(), 3);
        assert!(rendered.lines[1].contains("/reload"));
        assert_eq!(rendered.cursor_col, 5);
        draft.set_text("/plugin");
        assert_eq!(draft.selected_suggestion().unwrap().name, "/plugin");
        for (prefix, command) in [("/ver", "/version"), ("/abo", "/about")] {
            draft.set_text(prefix);
            let suggestions = draft.suggestions();
            assert_eq!(suggestions.len(), 1);
            assert_eq!(suggestions[0].name, command);
        }
        for value in [
            "/filter cargo",
            "/version extra",
            "/about extra",
            "/unknown",
            "message /reload",
            "/\n",
        ] {
            draft.set_text(value);
            assert!(draft.suggestions().is_empty());
        }
    }
    fn completion_fixture(names: &[&str]) -> completion::Matches {
        completion::Matches {
            items: names
                .iter()
                .map(|name| completion::Candidate {
                    name: (*name).into(),
                    description: "File".into(),
                    replacement: format!("{name} "),
                })
                .collect(),
            ..completion::Matches::default()
        }
    }

    #[test]
    fn unchanged_completion_keeps_query_storage_menu_and_selection() {
        let mut draft = Composer::default();
        draft.set_text("@docs");
        draft.refresh_completions_with(true, |query, previous| {
            assert_eq!(query.prefix, "docs");
            assert!(previous.is_none());
            Some((
                completion::Version::fixture_for_test(1),
                completion_fixture(&["@docs-a", "@docs-b"]),
            ))
        });
        draft.suggestion_index = 1;
        let query_storage = draft.reference_query.as_ref().unwrap().prefix.as_ptr();
        let menu_storage = draft.references.items.as_ptr();
        for _ in 0..100 {
            draft.refresh_completions_with(true, |query, previous| {
                assert_eq!(query.prefix.as_ptr(), query_storage);
                assert!(previous.is_some());
                None
            });
            assert!(draft.reference_active());
            assert_eq!(draft.references.items.as_ptr(), menu_storage);
            assert_eq!(draft.selected_suggestion().unwrap().name, "@docs-b");
        }
        draft.refresh_completions_with(true, |query, previous| {
            assert_eq!(query.prefix.as_ptr(), query_storage);
            assert!(previous.is_some());
            Some((
                completion::Version::fixture_for_test(2),
                completion_fixture(&["@docs-b", "@docs-a", "@docs-c"]),
            ))
        });
        assert_eq!(draft.suggestion_index, 0);
        assert_eq!(draft.selected_suggestion().unwrap().name, "@docs-b");
        assert_eq!(draft.references.items.len(), 3);
        draft.dismiss_suggestions();
        draft.refresh_completions_with(true, |_, _| {
            panic!("a late discovery must not reopen a dismissed menu")
        });
        assert!(!draft.reference_active());
    }

    #[test]
    fn completion_refresh_tracks_loading_cursor_edits_and_disabled_menus() {
        let mut draft = Composer::default();
        draft.set_text("@docs");
        draft.refresh_completions_with(true, |_, _| {
            Some((
                completion::Version::fixture_for_test(1),
                completion::Matches {
                    notice: Some("Loading".into()),
                    loading: true,
                    ..completion::Matches::default()
                },
            ))
        });
        assert!(draft.completion_loading());
        draft.refresh_completions_with(true, |_, previous| {
            assert!(previous.is_some());
            Some((
                completion::Version::fixture_for_test(2),
                completion_fixture(&["@docs"]),
            ))
        });
        assert!(!draft.completion_loading());
        assert!(draft.left());
        draft.refresh_completions_with(true, |query, previous| {
            assert_eq!(query.prefix, "doc");
            assert!(previous.is_none());
            Some((
                completion::Version::fixture_for_test(2),
                completion_fixture(&["@docs"]),
            ))
        });
        draft.refresh_completions_with(false, |_, _| panic!("disabled menu lookup"));
        assert!(!draft.reference_active());
        draft.refresh_completions_with(true, |query, previous| {
            assert_eq!(query.prefix, "doc");
            assert!(previous.is_none());
            Some((
                completion::Version::fixture_for_test(2),
                completion_fixture(&["@docs"]),
            ))
        });
        assert!(draft.insert("x"));
        draft.refresh_completions_with(true, |query, previous| {
            assert_eq!(query.prefix, "docx");
            assert!(previous.is_none());
            Some((
                completion::Version::fixture_for_test(2),
                completion_fixture(&[]),
            ))
        });
        assert_eq!(draft.text(), "@docxs");
        assert!(draft.references.items.is_empty());
    }

    fn reference_fixture(text: &str, cursor: usize, replacement: &str) -> Composer {
        let mut draft = Composer::default();
        draft.set_text(text);
        draft.cursor = cursor;
        draft.reference_query = completion::query(text, cursor);
        draft.references = completion::Matches {
            items: vec![completion::Candidate {
                name: "@设计 文稿.md".into(),
                description: "File".into(),
                replacement: replacement.into(),
            }],
            ..completion::Matches::default()
        };
        draft
    }

    #[test]
    fn reference_completion_preserves_surrounding_text_and_utf8_cursor() {
        let text = "检查 @\"设计 文\"，然后测试";
        let cursor = text.find("文\"").unwrap() + "文".len();
        let mut draft = reference_fixture(text, cursor, "@\"设计 文稿.md\" ");
        assert!(draft.menu_visible(40, 3));
        assert!(draft.selected_suggestion().unwrap().reference);
        assert!(draft.accept_suggestion().unwrap());
        assert_eq!(draft.text(), "检查 @\"设计 文稿.md\" ，然后测试");
        assert!(draft.text().is_char_boundary(draft.cursor()));
        assert!(draft.suggestions().is_empty());
    }

    #[test]
    fn stale_or_oversized_reference_completion_keeps_the_draft() {
        let mut draft = reference_fixture("@文", "@文".len(), "@\"设计 文稿.md\" ");
        draft.left();
        let original = draft.text().to_owned();
        assert!(!draft.accept_suggestion().unwrap());
        assert_eq!(draft.text(), original);
        let mut draft = reference_fixture("@文", "@文".len(), &"x".repeat(MAX_DRAFT_BYTES + 1));
        let original = (draft.text().to_owned(), draft.cursor());
        assert!(draft.accept_suggestion().is_err());
        assert_eq!((draft.text().to_owned(), draft.cursor()), original);
    }

    #[test]
    fn reference_menu_survives_wrapped_drafts_and_escape_only_dismisses_it() {
        let text = "第一行\n第二行\n第三行 @文";
        let mut draft = reference_fixture(text, text.len(), "@\"设计 文稿.md\" ");
        let rows = draft.rows(20, 3, &Ui::default(), true);
        assert!(draft.menu_visible(20, 3));
        assert_eq!(rows.lines.len(), 3);
        assert!(rows.lines.iter().any(|line| line.contains("@设计")));
        draft.dismiss_suggestions();
        assert!(draft.suggestions().is_empty());
        assert_eq!(draft.text(), text);
    }

    #[test]
    fn reference_menu_rows_accept_the_drawn_name_after_results_reorder() {
        let text = "检查 @设";
        let mut draft = reference_fixture(text, text.len(), "@\"设计 甲.md\" ");
        draft.references.items[0].name = "@设计 甲.md".into();
        draft.references.items.push(completion::Candidate {
            name: "@设计 乙.md".into(),
            description: "File".into(),
            replacement: "@\"设计 乙.md\" ".into(),
        });
        for width in [8, 20, 48, 120] {
            let rows = draft.rows(width, 3, &Ui::default(), true);
            assert_eq!(
                rows.references,
                vec![(1, "@设计 甲.md".into()), (2, "@设计 乙.md".into()),]
            );
            assert!(
                draft
                    .rows(width, 3, &Ui::default(), false)
                    .references
                    .is_empty()
            );
        }
        draft.references.items.swap(0, 1);
        assert!(draft.accept_reference("@设计 乙.md").unwrap());
        assert_eq!(draft.text(), "检查 @\"设计 乙.md\" ");
        assert_eq!(draft.cursor(), draft.text().len());
        assert!(
            draft.suggestions().is_empty(),
            "the next Enter can submit the completed draft"
        );
    }

    #[test]
    fn vanished_or_invalid_reference_click_preserves_the_draft() {
        let text = "检查 @文";
        let mut draft = reference_fixture(text, text.len(), "@\"设计 文稿.md\" ");
        let original = (draft.text().to_owned(), draft.cursor());
        draft.references.items.clear();
        assert!(draft.accept_reference("@设计 文稿.md").is_err());
        assert_eq!((draft.text().to_owned(), draft.cursor()), original);

        let mut draft = reference_fixture(text, text.len(), &"x".repeat(MAX_DRAFT_BYTES + 1));
        assert!(draft.accept_reference("@设计 文稿.md").is_err());
        assert_eq!((draft.text().to_owned(), draft.cursor()), original);
        draft.dismiss_suggestions();
        assert!(draft.accept_reference("@设计 文稿.md").is_err());
        assert_eq!((draft.text().to_owned(), draft.cursor()), original);
    }
}
