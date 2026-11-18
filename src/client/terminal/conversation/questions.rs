//! Independent answer drafts keep user requirements untouched.
use super::*;
use crate::client::interface::questions as queue;

impl State {
    fn question_open_selected(&mut self) {
        let pending = queue::pending();
        self.conversation.selected = self
            .conversation
            .selected
            .min(pending.len().saturating_sub(1));
        if let Some(question) = pending.get(self.conversation.selected) {
            self.conversation.question_id = Some(question.id.clone());
            self.conversation
                .answers
                .entry(question.id.clone())
                .or_default();
            self.conversation.detail = true;
            self.conversation.detail_scroll = 0;
            self.conversation.content_focus = false;
        } else {
            self.conversation.question_id = None;
            self.conversation.detail = false;
            self.conversation.content_focus = true;
        }
        self.dirty = true;
    }

    pub(super) fn chat_questions(&mut self) {
        let pending = queue::pending();
        let next = if self.conversation.view == View::Questions {
            self.conversation
                .question_id
                .as_ref()
                .and_then(|id| pending.iter().position(|q| &q.id == id))
                .map(|n| (n + 1) % pending.len().max(1))
                .unwrap_or(0)
        } else {
            0
        };
        self.chat_change_view(View::Questions);
        self.conversation.selected = next;
        self.conversation
            .answers
            .retain(|id, _| pending.iter().any(|q| &q.id == id));
        self.question_open_selected();
    }

    pub(super) fn question_editor(&self) -> Option<&Composer> {
        self.conversation
            .question_id
            .as_ref()
            .and_then(|id| self.conversation.answers.get(id))
    }

    fn question_editor_mut(&mut self) -> Option<&mut Composer> {
        if !self.conversation.detail {
            self.question_open_selected();
        }
        self.conversation
            .question_id
            .clone()
            .map(|id| self.conversation.answers.entry(id).or_default())
    }

    pub(super) fn question_paste_len(&mut self) -> Result<usize, String> {
        if !self.conversation.detail {
            self.question_open_selected();
        }
        self.question_editor()
            .map(|editor| editor.text().len())
            .ok_or_else(|| {
                "No pending question to answer; press Esc to return to your draft".into()
            })
    }

    pub(super) fn question_paste(&mut self, text: &str) -> bool {
        self.conversation.content_focus = false;
        self.question_editor_mut()
            .is_some_and(|editor| editor.paste(text))
    }

    fn question_escape(&mut self) {
        if self.conversation.detail {
            self.conversation.detail = false;
            self.conversation.content_focus = true;
            self.conversation.detail_scroll = 0;
            self.notice = None;
        } else {
            self.chat_change_view(View::Activity);
        }
        self.dirty = true;
    }

    fn question_move(&mut self, delta: isize) {
        if self.conversation.detail {
            self.conversation.detail_scroll =
                self.conversation.detail_scroll.saturating_add_signed(delta);
        } else {
            self.conversation.selected = self
                .conversation
                .selected
                .saturating_add_signed(delta)
                .min(queue::pending_count().saturating_sub(1));
        }
    }

    fn question_finish(&mut self, skip: bool) {
        if !skip && self.chat_wait_for_input() {
            return;
        }
        let pending = queue::pending();
        let id = if self.conversation.detail {
            self.conversation.question_id.clone()
        } else {
            pending
                .get(self.conversation.selected)
                .map(|q| q.id.clone())
        };
        let Some(id) = id else {
            return;
        };
        let result = if skip {
            queue::skip(&id)
        } else {
            let raw = self
                .question_editor()
                .map(|e| e.text().trim().to_owned())
                .unwrap_or_default();
            let answer = raw
                .parse::<usize>()
                .ok()
                .and_then(|index| {
                    pending
                        .iter()
                        .find(|q| q.id == id)
                        .and_then(|q| index.checked_sub(1).and_then(|i| q.options.get(i)))
                        .cloned()
                })
                .unwrap_or(raw);
            queue::answer(&id, &answer)
        };
        match result {
            Ok(question) => {
                self.conversation.answers.remove(&id);
                let mut record = format!("\n[ANSWER] #{id}\n");
                let text = question.answer.as_deref().unwrap_or("Skipped by user");
                let history_error = if !self.preview {
                    crate::client::interface::history::record("answer", text).err()
                } else {
                    None
                };
                for row in markdown::rows(text, self.geometry.left, &self.ui) {
                    let _ = writeln!(record, "{row}");
                }
                record.push('\n');
                self.log(record.as_bytes());
                if queue::pending_count() == 0 {
                    self.chat_change_view(View::Activity);
                } else {
                    self.question_open_selected();
                }
                self.notice = if let Some(error) = history_error {
                    Some(format!(
                        "Answer retained; history could not be saved: {error}"
                    ))
                } else {
                    Some(
                        if skip {
                            "Question skipped · your original draft is preserved"
                        } else {
                            "Answer saved · the agent can read it · original draft preserved"
                        }
                        .into(),
                    )
                };
            }
            Err(error) => self.notice = Some(error),
        }
        self.dirty = true;
    }

    pub(super) fn question_character(&mut self, character: char) {
        match character {
            '\u{1b}' => self.question_escape(),
            '\t' => {
                if !self.conversation.detail {
                    self.question_open_selected();
                } else {
                    self.conversation.content_focus = !self.conversation.content_focus;
                }
            }
            '\r' if !self.conversation.detail || self.conversation.content_focus => {
                self.question_open_selected()
            }
            '\r' => self.question_finish(false),
            ch => {
                self.conversation.content_focus = false;
                if let Some(editor) = self.question_editor_mut() {
                    match ch {
                        '\n' => {
                            editor.insert("\n");
                        }
                        '\u{1}' => {
                            editor.home();
                        }
                        '\u{5}' => {
                            editor.end();
                        }
                        '\u{15}' => {
                            editor.clear_before();
                        }
                        '\u{b}' => {
                            editor.clear_after();
                        }
                        '\u{17}' => {
                            editor.delete_word();
                        }
                        '\u{7f}' | '\u{8}' => {
                            editor.backspace();
                        }
                        ch if !ch.is_control() && !editor.insert(&ch.to_string()) => {
                            self.notice = Some("Answer limit reached · 16 KiB".into());
                        }
                        _ => {}
                    }
                }
            }
        }
        self.dirty = true;
    }

    pub(super) fn question_key(&mut self, key: Key) {
        let content = self.conversation.content_focus || !self.conversation.detail;
        match key {
            Key::Questions => self.chat_questions(),
            Key::Escape => self.question_escape(),
            Key::Help => {
                self.help = true;
            }
            Key::Layout => self.chat_change_view(View::Activity),
            Key::Sidebar => self.actions.push("sidebar.toggle"),
            Key::Theme => self.actions.push("theme.next"),
            Key::Reload => self.actions.push("reload"),
            Key::Save => self.actions.push("save"),
            Key::FocusBack => {
                if self.conversation.detail {
                    self.conversation.content_focus = !content;
                }
            }
            Key::Up => self.question_move(-5),
            Key::Down => self.question_move(5),
            Key::AuditUp if content => self.question_move(-1),
            Key::AuditDown if content => self.question_move(1),
            Key::AuditUp => {
                let width = self.geometry.left;
                if let Some(editor) = self.question_editor_mut() {
                    editor.up(width);
                }
            }
            Key::AuditDown => {
                let width = self.geometry.left;
                if let Some(editor) = self.question_editor_mut() {
                    editor.down(width);
                }
            }
            Key::Back if content => self.question_escape(),
            Key::Back => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.left();
                }
            }
            Key::Open if content => self.question_open_selected(),
            Key::Open => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.right();
                }
            }
            Key::Home if content => self.question_move(isize::MIN.saturating_add(1)),
            Key::End if content => self.question_move(isize::MAX),
            Key::Home => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.home();
                }
            }
            Key::End => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.end();
                }
            }
            Key::Delete if content => self.question_finish(true),
            Key::Delete => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.delete();
                }
            }
            Key::Newline => {
                if let Some(editor) = self.question_editor_mut() {
                    editor.insert("\n");
                }
            }
            Key::Mouse {
                button,
                x,
                y,
                release,
            } if !release && x < self.geometry.left => {
                if button & 64 != 0 {
                    self.question_move(if button & 1 == 0 { -3 } else { 3 });
                } else if y > prompt_top(self.geometry.rows) && self.conversation.detail {
                    self.conversation.content_focus = false;
                } else if !self.conversation.detail {
                    if let Some(index) = self.chat_list_hit(y) {
                        self.conversation.selected = index;
                        if button == 2 {
                            self.question_open_selected();
                        }
                    }
                } else {
                    self.conversation.content_focus = true;
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    pub(super) fn question_rows(&mut self, height: u16) -> Vec<String> {
        let width = self.geometry.left;
        let pending = queue::pending();
        let mut rows = vec![self.chat_line(
            &format!(
                "  Questions · {} waiting · agent work continues",
                pending.len()
            ),
            PanelTone::Heading,
            width,
        )];
        if pending.is_empty() {
            rows.push(self.chat_line(
                "  No questions waiting. Esc returns to your draft.",
                PanelTone::Muted,
                width,
            ));
            return rows;
        }
        if self.conversation.detail
            && let Some(question) = self
                .conversation
                .question_id
                .as_ref()
                .and_then(|id| pending.iter().find(|q| &q.id == id))
        {
            rows.push(
                self.chat_line(
                    question
                        .title
                        .as_deref()
                        .unwrap_or("  Reply below when ready"),
                    PanelTone::Accent,
                    width,
                ),
            );
            let mut body = markdown::rows(&question.question, width, &self.ui);
            if !question.options.is_empty() {
                body.push(self.chat_line("", PanelTone::Normal, width));
                for (index, option) in question.options.iter().enumerate() {
                    body.extend(markdown::rows(
                        &format!("{}. {option}", index + 1),
                        width,
                        &self.ui,
                    ));
                }
                body.push(self.chat_line(
                    "  Enter a number or write your own answer.",
                    PanelTone::Muted,
                    width,
                ));
            }
            self.conversation.detail_scroll = self.conversation.detail_scroll.min(
                body.len()
                    .saturating_sub(usize::from(height.saturating_sub(2))),
            );
            rows.extend(body.into_iter().skip(self.conversation.detail_scroll));
        } else {
            for index in self.chat_list_window(pending.len(), height) {
                let question = &pending[index];
                rows.push(self.chat_line(
                    &format!(
                        "{} {}",
                        if index == self.conversation.selected {
                            "›"
                        } else {
                            " "
                        },
                        question.title.as_deref().unwrap_or("Question")
                    ),
                    if index == self.conversation.selected {
                        PanelTone::Accent
                    } else {
                        PanelTone::Normal
                    },
                    width,
                ));
                rows.push(self.chat_line(
                    &format!("  {}", question.question.lines().next().unwrap_or("")),
                    PanelTone::Muted,
                    width,
                ));
            }
        }
        rows
    }
}
