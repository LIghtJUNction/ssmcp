//! Question answers and explicit finish decisions preserve the user's main draft.
use super::*;
use crate::client::interface::questions::{
    self as queue, AnswerDelivery, DecisionActor, FinishChoice, FinishStatus, QuestionKind,
    QuestionState, QuestionView,
};

fn editable(view: &QuestionView) -> bool {
    view.question.state == QuestionState::Pending
        && view
            .finish
            .as_ref()
            .is_none_or(|finish| finish.status == FinishStatus::Pending)
}

fn choice_label(choice: FinishChoice) -> &'static str {
    match choice {
        FinishChoice::No => "No",
        FinishChoice::Yes => "Yes",
    }
}

fn actor_label(actor: &DecisionActor) -> String {
    match actor {
        DecisionActor::Human => "You".into(),
        DecisionActor::Agent { agent } => format!(
            "Agent {} · {}:{}",
            panel::safe_text(&agent.name),
            panel::safe_text(&agent.provider),
            panel::safe_text(&agent.subject)
        ),
    }
}

fn delivery_label(view: &QuestionView) -> &'static str {
    match view.delivery {
        AnswerDelivery::NotAnswered => "Waiting for an answer",
        AnswerDelivery::Pending => "Answer queued · waiting for requester call",
        AnswerDelivery::Delivered => "Included in requester tool response",
        AnswerDelivery::ManualRead => "Saved · requester must read the answer",
        AnswerDelivery::ScopeExpired => "Saved · original request scope expired",
    }
}

fn finish_label(status: FinishStatus) -> &'static str {
    match status {
        FinishStatus::Pending => "Waiting for your decision",
        FinishStatus::Approved => "Approved · waiting for agent receipt",
        FinishStatus::Denied => "No · keep working",
        FinishStatus::Cancelled => "Request cancelled",
        FinishStatus::Invalidated => "Approval invalidated",
        FinishStatus::Completed => "Finish receipt committed",
    }
}

impl State {
    fn question_views(&self) -> Vec<QuestionView> {
        #[cfg(test)]
        if let Some(views) = &self.conversation.question_fixture {
            return ordered(views.clone());
        }
        ordered(queue::retained_views())
    }

    fn question_view(&self, id: &str) -> Option<QuestionView> {
        #[cfg(test)]
        if let Some(views) = &self.conversation.question_fixture {
            return views.iter().find(|view| view.question.id == id).cloned();
        }
        queue::get_view(id)
    }

    fn question_current(&self) -> Option<QuestionView> {
        self.conversation
            .question_id
            .as_deref()
            .and_then(|id| self.question_view(id))
    }

    fn question_open_selected(&mut self) {
        let views = self.question_views();
        self.conversation
            .answers
            .retain(|id, _| views.iter().any(|view| view.question.id == *id));
        self.conversation
            .finish_choices
            .retain(|id, _| views.iter().any(|view| view.question.id == *id));
        self.conversation.selected = self
            .conversation
            .selected
            .min(views.len().saturating_sub(1));
        if let Some(view) = views.get(self.conversation.selected) {
            let id = &view.question.id;
            self.conversation.question_id = Some(id.clone());
            if editable(view) {
                self.conversation.answers.entry(id.clone()).or_default();
                if view.kind == QuestionKind::Finish {
                    self.conversation
                        .finish_choices
                        .entry(id.clone())
                        .or_insert(FinishChoice::No);
                }
            }
            self.conversation.detail = true;
            self.conversation.detail_scroll = 0;
            self.conversation.content_focus = view.kind == QuestionKind::Finish || !editable(view);
        } else {
            self.conversation.question_id = None;
            self.conversation.detail = false;
            self.conversation.content_focus = true;
        }
        self.dirty = true;
    }

    pub(super) fn chat_questions(&mut self) {
        let views = self.question_views();
        // Shift+Up cycles unanswered requests first, and opens retained receipts when none wait.
        let waiting: Vec<_> = views
            .iter()
            .enumerate()
            .filter(|(_, view)| editable(view))
            .map(|(index, _)| index)
            .collect();
        let candidates: Vec<_> = if waiting.is_empty() {
            (0..views.len()).collect()
        } else {
            waiting
        };
        let next = if self.conversation.view == View::Questions {
            self.conversation
                .question_id
                .as_ref()
                .and_then(|id| {
                    candidates
                        .iter()
                        .position(|index| views[*index].question.id == *id)
                })
                .and_then(|position| candidates.get((position + 1) % candidates.len().max(1)))
                .copied()
                .unwrap_or_else(|| candidates.first().copied().unwrap_or(0))
        } else {
            candidates.first().copied().unwrap_or(0)
        };
        self.chat_change_view(View::Questions);
        self.conversation.selected = next;
        self.conversation
            .answers
            .retain(|id, _| views.iter().any(|view| view.question.id == *id));
        self.conversation
            .finish_choices
            .retain(|id, _| views.iter().any(|view| view.question.id == *id));
        self.question_open_selected();
    }

    pub(super) fn question_editor(&self) -> Option<&Composer> {
        if self
            .question_current()
            .as_ref()
            .is_some_and(|view| !editable(view))
        {
            return None;
        }
        self.conversation
            .question_id
            .as_ref()
            .and_then(|id| self.conversation.answers.get(id))
    }

    fn question_editor_mut(&mut self) -> Option<&mut Composer> {
        if !self.conversation.detail {
            self.question_open_selected();
        }
        if self
            .question_current()
            .as_ref()
            .is_some_and(|view| !editable(view))
        {
            self.notice = Some("This request already has a recorded answer or decision".into());
            self.conversation.content_focus = true;
            return None;
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
        if self
            .question_current()
            .as_ref()
            .is_some_and(|view| !editable(view))
        {
            return Err(
                "This request is already answered; its recorded response cannot be edited".into(),
            );
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
                .min(self.question_views().len().saturating_sub(1));
        }
    }

    fn question_choice(&self, id: &str) -> FinishChoice {
        self.conversation
            .finish_choices
            .get(id)
            .copied()
            .unwrap_or(FinishChoice::No)
    }

    fn question_choose(&mut self, choice: FinishChoice) {
        if let Some(view) = self.question_current()
            && view.kind == QuestionKind::Finish
            && editable(&view)
        {
            self.conversation
                .finish_choices
                .insert(view.question.id, choice);
            self.conversation.content_focus = true;
            self.notice = Some(format!(
                "{} selected · Enter confirms · Tab edits the optional comment",
                choice_label(choice)
            ));
        }
    }

    fn question_finish_input(&self, id: &str) -> (FinishChoice, Option<String>) {
        let comment = self
            .conversation
            .answers
            .get(id)
            .map(|editor| editor.text().trim())
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        (self.question_choice(id), comment)
    }

    fn question_finish(&mut self, skip: bool) {
        if !skip && self.chat_wait_for_input() {
            return;
        }
        let views = self.question_views();
        let selected = if self.conversation.detail {
            self.question_current()
        } else {
            views.get(self.conversation.selected).cloned()
        };
        let Some(view) = selected else {
            return;
        };
        if !editable(&view) {
            self.notice = Some("Already recorded · Shift+Up opens the next pending request".into());
            return;
        }
        let id = view.question.id.clone();
        if view.kind == QuestionKind::Finish {
            if skip {
                self.notice = Some("Choose No or Yes, then press Enter · Delete never approves or skips a finish request".into());
                return;
            }
            let (choice, comment) = self.question_finish_input(&id);
            match queue::decide_finish(&id, choice, comment.as_deref()) {
                Ok(recorded) => self.question_recorded(recorded),
                Err(error) => self.notice = Some(error),
            }
        } else {
            let result = if skip {
                queue::skip(&id)
            } else {
                let raw = self
                    .question_editor()
                    .map(|editor| editor.text().trim().to_owned())
                    .unwrap_or_default();
                let answer = raw
                    .parse::<usize>()
                    .ok()
                    .and_then(|index| {
                        index
                            .checked_sub(1)
                            .and_then(|index| view.question.options.get(index))
                    })
                    .cloned()
                    .unwrap_or(raw);
                queue::answer(&id, &answer)
            };
            match result {
                Ok(_) => {
                    if let Some(recorded) = queue::get_view(&id) {
                        self.question_recorded(recorded);
                    }
                }
                Err(error) => self.notice = Some(error),
            }
        }
        self.dirty = true;
    }

    fn question_recorded(&mut self, view: QuestionView) {
        let id = &view.question.id;
        self.conversation.answers.remove(id);
        self.conversation.finish_choices.remove(id);
        let text = if let Some(decision) = view
            .finish
            .as_ref()
            .and_then(|finish| finish.decision.as_ref())
        {
            format!(
                "{} · {}{}\n",
                choice_label(decision.choice),
                actor_label(&decision.actor),
                decision
                    .comment
                    .as_ref()
                    .map(|text| format!("\n{text}"))
                    .unwrap_or_default()
            )
        } else {
            view.question
                .answer
                .clone()
                .unwrap_or_else(|| "Skipped by user".into())
        };
        let history_error = if !self.preview {
            crate::client::interface::history::record("answer", &text).err()
        } else {
            None
        };
        let mut record = format!("\n[ANSWER] #{id}\n");
        for row in markdown::rows(&text, self.geometry.left, &self.ui) {
            let _ = writeln!(record, "{row}");
        }
        record.push('\n');
        self.log(record.as_bytes());
        self.conversation.question_id = Some(id.clone());
        self.conversation.detail = true;
        self.conversation.content_focus = true;
        self.conversation.detail_scroll = 0;
        self.notice = Some(if let Some(error) = history_error {
            format!("Answer retained; history could not be saved: {error}")
        } else if let Some(finish) = &view.finish {
            format!("{} · original draft preserved", finish_label(finish.status))
        } else {
            format!("{} · original draft preserved", delivery_label(&view))
        });
    }

    pub(super) fn question_character(&mut self, character: char) {
        match character {
            '\u{1b}' => self.question_escape(),
            '\t' => {
                if !self.conversation.detail {
                    self.question_open_selected();
                } else if self.question_current().as_ref().is_none_or(editable) {
                    self.conversation.content_focus = !self.conversation.content_focus;
                }
            }
            '\r' if !self.conversation.detail => self.question_open_selected(),
            '\r' if self.conversation.content_focus => {
                if self
                    .question_current()
                    .is_some_and(|view| view.kind == QuestionKind::Finish)
                {
                    self.question_finish(false);
                } else {
                    self.question_open_selected();
                }
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
        let finish = self.conversation.detail
            && self
                .question_current()
                .is_some_and(|view| view.kind == QuestionKind::Finish && editable(&view));
        match key {
            Key::Questions => self.chat_questions(),
            Key::Escape => self.question_escape(),
            Key::Help => self.help = true,
            Key::Layout => self.chat_change_view(View::Activity),
            Key::Sidebar => self.actions.push("sidebar.toggle"),
            Key::Theme => self.actions.push("theme.next"),
            Key::Reload => self.actions.push("reload"),
            Key::Save => self.actions.push("save"),
            Key::FocusBack => {
                if self.conversation.detail && self.question_current().as_ref().is_none_or(editable)
                {
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
            Key::Back if content && finish => self.question_choose(FinishChoice::No),
            Key::Open if content && finish => self.question_choose(FinishChoice::Yes),
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
                } else if button & 32 == 0
                    && button & 3 == 0
                    && let Some((_, id, choice)) = self
                        .conversation
                        .finish_button_rows
                        .iter()
                        .find(|(row, _, _)| *row == y)
                        .cloned()
                {
                    // Bind the click to the ID actually drawn; a reordered queue cannot retarget it.
                    if self.conversation.question_id.as_ref() == Some(&id) {
                        self.question_choose(choice);
                    }
                } else if y > prompt_top(self.geometry) && self.conversation.detail {
                    self.conversation.content_focus = false;
                } else if !self.conversation.detail {
                    if let Some(index) = self.chat_list_hit(y)
                        && let Some(id) = self.conversation.drawn_questions.get(index)
                    {
                        let id = id.clone();
                        if let Some(index) = self
                            .question_views()
                            .iter()
                            .position(|view| view.question.id == id)
                        {
                            self.conversation.selected = index;
                            if button == 2 {
                                self.question_open_selected();
                            }
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

    pub(super) fn question_input_hint(&self) -> &'static str {
        match self.question_current() {
            Some(view) if !editable(&view) => "Recorded · Shift+Up opens pending requests",
            Some(view) if view.kind == QuestionKind::Finish => {
                "Optional comment · Enter confirms selected No / Yes"
            }
            _ => "Type an answer · number selects an option",
        }
    }

    pub(super) fn question_navigation_hint(&self) -> &'static str {
        if !self.conversation.detail {
            "↑↓ select · Enter details · Shift+↑ pending · Esc back"
        } else if self
            .question_current()
            .is_some_and(|view| view.kind == QuestionKind::Finish && editable(&view))
        {
            "← No · → Yes · Enter confirms · Tab comment · Esc list"
        } else if self
            .question_current()
            .as_ref()
            .is_some_and(|view| !editable(view))
        {
            "Recorded response · Shift+↑ pending · Esc list"
        } else {
            "Enter answer · Shift+↑ next · Esc list · Tab browse"
        }
    }

    pub(super) fn question_rows(&mut self, height: u16) -> Vec<String> {
        let width = self.geometry.left;
        let views = self.question_views();
        let waiting = views.iter().filter(|view| editable(view)).count();
        self.conversation.finish_button_rows.clear();
        self.conversation.drawn_questions =
            views.iter().map(|view| view.question.id.clone()).collect();
        let mut rows = vec![self.chat_line(
            &format!("  Questions · {waiting} waiting · Shift+↑"),
            PanelTone::Heading,
            width,
        )];
        if views.is_empty() {
            rows.push(self.chat_line(
                "  No requests yet. Esc returns to your draft.",
                PanelTone::Muted,
                width,
            ));
            return rows;
        }
        if self.conversation.detail
            && let Some(view) = self.question_current()
        {
            let question = &view.question;
            rows.push(
                self.chat_line(
                    question
                        .title
                        .as_deref()
                        .unwrap_or(if view.kind == QuestionKind::Finish {
                            "Finish request"
                        } else {
                            "Reply when ready"
                        }),
                    PanelTone::Accent,
                    width,
                ),
            );
            if view.kind == QuestionKind::Finish && editable(&view) {
                for choice in [FinishChoice::No, FinishChoice::Yes] {
                    let selected = self.question_choice(&question.id) == choice;
                    let row = body_top(self.geometry.rows) + rows.len() as u16;
                    if rows.len() < usize::from(height) {
                        self.conversation.finish_button_rows.push((
                            row,
                            question.id.clone(),
                            choice,
                        ));
                    }
                    rows.push(self.chat_line(
                        &format!(
                            "  {} {} · {}",
                            if selected { "●" } else { "○" },
                            choice_label(choice),
                            if choice == FinishChoice::No {
                                "keep working (default)"
                            } else {
                                "allow the agent to finish"
                            }
                        ),
                        if selected {
                            PanelTone::Accent
                        } else {
                            PanelTone::Muted
                        },
                        width,
                    ));
                }
            }
            let mut body = Vec::new();
            if let Some(requester) = &view.requester {
                body.push(self.chat_line(
                    &format!(
                        "From {} · {}:{}",
                        panel::safe_text(&requester.name),
                        panel::safe_text(&requester.provider),
                        panel::safe_text(&requester.subject)
                    ),
                    PanelTone::Muted,
                    width,
                ));
            }
            body.extend(markdown::rows(&question.question, width, &self.ui));
            if let Some(finish) = &view.finish {
                body.push(self.chat_line("", PanelTone::Normal, width));
                body.push(self.chat_line(finish_label(finish.status), PanelTone::Accent, width));
                if let Some(decision) = &finish.decision {
                    body.push(self.chat_line(
                        &format!(
                            "{} by {} · revision {}",
                            choice_label(decision.choice),
                            actor_label(&decision.actor),
                            decision.revision
                        ),
                        PanelTone::Muted,
                        width,
                    ));
                    if let Some(comment) = &decision.comment {
                        body.extend(markdown::rows(comment, width, &self.ui));
                    }
                }
                if let Some(reason) = &finish.invalidation_reason {
                    body.extend(markdown::rows(reason, width, &self.ui));
                }
            } else {
                if !question.options.is_empty() && editable(&view) {
                    body.push(self.chat_line("", PanelTone::Normal, width));
                    for (index, option) in question.options.iter().enumerate() {
                        body.extend(markdown::rows(
                            &format!("{}. {option}", index + 1),
                            width,
                            &self.ui,
                        ));
                    }
                    body.push(self.chat_line(
                        "Enter a number or write your own answer.",
                        PanelTone::Muted,
                        width,
                    ));
                }
                if let Some(answer) = &question.answer {
                    body.push(self.chat_line("", PanelTone::Normal, width));
                    body.push(self.chat_line("Answer", PanelTone::Heading, width));
                    body.extend(markdown::rows(answer, width, &self.ui));
                    if let Some(actor) = &view.answer_actor {
                        body.push(self.chat_line(&actor_label(actor), PanelTone::Muted, width));
                    }
                }
                body.push(self.chat_line(delivery_label(&view), PanelTone::Muted, width));
            }
            if !editable(&view) {
                self.conversation.content_focus = true;
                if let Some(local) = self
                    .conversation
                    .answers
                    .get(&question.id)
                    .filter(|editor| !editor.is_empty())
                {
                    body.push(self.chat_line("", PanelTone::Normal, width));
                    body.push(self.chat_line(
                        "Your unsent draft · not delivered",
                        PanelTone::Muted,
                        width,
                    ));
                    body.extend(markdown::rows(local.text(), width, &self.ui));
                }
            }
            let available = usize::from(height).saturating_sub(rows.len());
            self.conversation.detail_scroll = self
                .conversation
                .detail_scroll
                .min(body.len().saturating_sub(available));
            rows.extend(body.into_iter().skip(self.conversation.detail_scroll));
        } else {
            for index in self.chat_list_window(views.len(), height) {
                let view = &views[index];
                rows.push(self.chat_line(
                    &format!(
                        "{} {}",
                        if index == self.conversation.selected {
                            "›"
                        } else {
                            " "
                        },
                        view.question.title.as_deref().unwrap_or(
                            if view.kind == QuestionKind::Finish {
                                "Finish request"
                            } else {
                                "Question"
                            }
                        )
                    ),
                    if index == self.conversation.selected {
                        PanelTone::Accent
                    } else {
                        PanelTone::Normal
                    },
                    width,
                ));
                let status = view
                    .finish
                    .as_ref()
                    .map(|finish| finish_label(finish.status))
                    .unwrap_or_else(|| delivery_label(view));
                rows.push(self.chat_line(&format!("  {status}"), PanelTone::Muted, width));
            }
        }
        rows
    }
}

fn ordered(mut views: Vec<QuestionView>) -> Vec<QuestionView> {
    // Stable ordering keeps live pending questions first while retaining completed delivery receipts.
    views.sort_by_key(|view| !editable(view));
    views
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::interface::questions::{AgentActor, FinishDecision, FinishView, Question};
    use crate::client::terminal::{Layout, Ui, preview_state};
    use std::time::Instant;

    fn fixture(id: &str, finish: bool) -> QuestionView {
        QuestionView {
            question: Question {
                id: id.into(), title: Some(if finish { "Ready to finish?" } else { "Which format?" }.into()),
                question: if finish { "The implementation and local checks are complete.\nMay this agent finish this conversation?" } else { "Please choose an output format. Work continues while this question is pending." }.into(),
                options: if finish { vec![] } else { vec!["Markdown".into(), "PDF".into()] },
                state: QuestionState::Pending, answer: None, created_at_ms: 1, answered_at_ms: None,
            },
            kind: if finish { QuestionKind::Finish } else { QuestionKind::Question },
            requester: Some(AgentActor { provider: "github".into(), subject: "123".into(), thread_id: "requesting-thread".into(), name: "Builder".into() }),
            finish: finish.then(|| FinishView { reason: "Checks complete".into(), default_decision: FinishChoice::No, decision: None, status: FinishStatus::Pending, approved: false, invalidation_reason: None }),
            answer_actor: None, answer_revision: 0, automatic_delivery: true, delivery: AnswerDelivery::NotAnswered,
        }
    }

    fn state(columns: u16, views: Vec<QuestionView>) -> State {
        let mut state = preview_state(
            28,
            columns,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                color: true,
                ..Ui::default()
            },
        );
        state
            .conversation
            .composer
            .set_text("保留原始草稿\n  indentation");
        state.conversation.target = Some("chosen-agent".into());
        state.conversation.question_fixture = Some(views);
        state.chat_questions();
        state
    }

    fn screen(state: &mut State, name: &str) -> String {
        let frame = state.chat_frame(Instant::now());
        let mut parser = vt100::Parser::new(frame.geometry.rows, frame.geometry.cols, 0);
        let bytes = frame.diff(None);
        parser.process(&bytes);
        if let Some(directory) = std::env::var_os("SSMCP_INTERACTION_PREVIEW_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            let name = format!("{name}-{}", frame.geometry.cols);
            std::fs::write(directory.join(format!("{name}.ansi")), &bytes).unwrap();
            std::fs::write(directory.join(format!("{name}.json")), serde_json::to_vec(&json!({
                "rows":frame.geometry.rows,"cols":frame.geometry.cols,"source":"production conversation Frame"
            })).unwrap()).unwrap();
        }
        parser.screen().contents()
    }

    #[test]
    fn finish_defaults_to_no_and_comment_text_never_selects_yes() {
        let mut state = state(120, vec![fixture("finish-a", true)]);
        let original = state.conversation.composer.text().to_owned();
        assert!(state.conversation.content_focus);
        assert_eq!(
            state.question_finish_input("finish-a"),
            (FinishChoice::No, None)
        );
        for character in "yes".chars() {
            state.question_character(character);
        }
        assert_eq!(
            state.question_finish_input("finish-a"),
            (FinishChoice::No, Some("yes".into()))
        );
        state.question_key(Key::FocusBack);
        state.question_key(Key::Open);
        assert_eq!(
            state.question_finish_input("finish-a"),
            (FinishChoice::Yes, Some("yes".into()))
        );
        state.question_key(Key::Back);
        assert_eq!(state.question_finish_input("finish-a").0, FinishChoice::No);
        state.question_key(Key::Delete);
        assert!(state.notice.as_deref().unwrap().contains("Delete never"));
        assert_eq!(state.conversation.composer.text(), original);
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
    }

    #[test]
    fn finish_buttons_are_bound_to_drawn_request_and_do_not_submit() {
        for columns in [120, 48] {
            let mut state = state(columns, vec![fixture("finish-a", true)]);
            let contents = screen(&mut state, "finish-default");
            assert!(contents.contains("No"));
            assert!(contents.contains("Yes"));
            let (row, _, _) = state
                .conversation
                .finish_button_rows
                .iter()
                .find(|(_, _, choice)| *choice == FinishChoice::Yes)
                .unwrap()
                .clone();
            state.question_key(Key::Mouse {
                button: 0,
                x: 2,
                y: row,
                release: false,
            });
            assert_eq!(state.question_choice("finish-a"), FinishChoice::Yes);
            assert_eq!(
                state.question_current().unwrap().question.state,
                QuestionState::Pending
            );
            state.conversation.question_id = Some("replacement".into());
            state.question_key(Key::Mouse {
                button: 0,
                x: 2,
                y: row,
                release: false,
            });
            assert_eq!(state.question_choice("replacement"), FinishChoice::No);
        }
    }

    #[test]
    fn trusted_agent_decisions_and_queued_answers_remain_visible() {
        for columns in [120, 48] {
            let mut finish = fixture("finish-a", true);
            finish.question.state = QuestionState::Answered;
            let details = finish.finish.as_mut().unwrap();
            details.status = FinishStatus::Approved;
            details.approved = true;
            details.decision = Some(FinishDecision {
                choice: FinishChoice::Yes,
                comment: Some("Independent review passed.".into()),
                actor: DecisionActor::Agent {
                    agent: AgentActor {
                        provider: "github".into(),
                        subject: "123".into(),
                        thread_id: "reviewer-thread".into(),
                        name: "Reviewer".into(),
                    },
                },
                revision: 1,
                decided_at_ms: 2,
            });
            let mut state = state(columns, vec![finish]);
            let original = state.conversation.composer.text().to_owned();
            let contents = screen(&mut state, "finish-agent");
            assert!(contents.contains("Approved"));
            assert!(contents.contains("Reviewer"));
            state.question_character('\r');
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("Already recorded")
            );

            let mut answered = fixture("answer-a", false);
            answered.question.state = QuestionState::Answered;
            answered.question.answer = Some("Use Markdown.".into());
            answered.answer_actor = Some(DecisionActor::Human);
            answered.answer_revision = 1;
            answered.delivery = AnswerDelivery::Pending;
            state.conversation.question_fixture = Some(vec![answered]);
            state.chat_questions();
            let contents = screen(&mut state, "question-pending");
            assert!(contents.contains("Answer queued"));
            state.conversation.question_fixture.as_mut().unwrap()[0].delivery =
                AnswerDelivery::Delivered;
            let contents = screen(&mut state, "question-delivered");
            assert!(contents.contains("Included in requester"));
            assert_eq!(state.conversation.composer.text(), original);
            assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        }
    }

    #[test]
    fn another_agents_decision_preserves_but_does_not_submit_the_local_comment() {
        let mut state = state(120, vec![fixture("finish-a", true)]);
        for ch in "my unfinished comment".chars() {
            state.question_character(ch);
        }
        let view = &mut state.conversation.question_fixture.as_mut().unwrap()[0];
        view.question.state = QuestionState::Answered;
        view.finish.as_mut().unwrap().status = FinishStatus::Denied;
        assert!(state.question_editor().is_none());
        let contents = screen(&mut state, "finish-external");
        assert!(contents.contains("Your unsent draft"));
        assert!(contents.contains("my unfinished comment"));
        assert!(state.conversation.content_focus);
        state.chat_questions();
        assert_eq!(
            state.conversation.answers["finish-a"].text(),
            "my unfinished comment"
        );
        assert_eq!(
            state.conversation.composer.text(),
            "保留原始草稿\n  indentation"
        );
    }

    #[test]
    fn shift_up_prefers_pending_requests_without_discarding_other_answer_drafts() {
        let mut done = fixture("completed", false);
        done.question.state = QuestionState::Answered;
        done.delivery = AnswerDelivery::Pending;
        let mut state = state(
            120,
            vec![done, fixture("first", false), fixture("second", false)],
        );
        assert_eq!(state.conversation.question_id.as_deref(), Some("first"));
        state.question_character('甲');
        state.chat_questions();
        assert_eq!(state.conversation.question_id.as_deref(), Some("second"));
        state.question_character('乙');
        state.chat_questions();
        assert_eq!(state.conversation.question_id.as_deref(), Some("first"));
        assert_eq!(state.conversation.answers["first"].text(), "甲");
        assert_eq!(state.conversation.answers["second"].text(), "乙");
        assert_eq!(state.question_views().len(), 3);
    }
}
