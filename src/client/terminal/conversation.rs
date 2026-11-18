//! Conversation-style local controls. No input is ever forwarded to a hidden PTY.
use super::*;
use crate::client::{attachments, composer::Composer, references, steering};
use crate::wire::UserImage;
use serde_json::{Value, json};
mod history;
mod questions;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum View {
    #[default]
    Activity,
    Audit,
    Agents,
    Queue,
    Questions,
    History,
}
impl View {
    fn name(self) -> &'static str {
        match self {
            Self::Activity => "activity",
            Self::Audit => "code audit",
            Self::Agents => "agents",
            Self::Queue => "queue",
            Self::Questions => "questions",
            Self::History => "history",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Activity => Self::Audit,
            Self::Audit => Self::Agents,
            Self::Agents => Self::Queue,
            Self::Queue => Self::Questions,
            Self::Questions => Self::History,
            Self::History => Self::Activity,
        }
    }
}

#[derive(Default)]
pub(super) struct Conversation {
    pub(super) composer: Composer,
    pub(super) view: View,
    pub(super) content_focus: bool,
    selected: usize,
    selected_agent: Option<String>,
    list_start: usize,
    list_count: usize,
    detail: bool,
    detail_scroll: usize,
    target: Option<String>,
    editing: Option<String>,
    submitting: Option<(String, String)>,
    parked: Option<steering::SavedDraft>,
    text_pending: Vec<u8>,
    paste: Option<Vec<u8>>,
    paste_overflow: bool,
    restored: bool,
    images: Vec<UserImage>,
    images_changed: bool,
    image_task: Option<attachments::Task>,
    persisted_context: Option<Value>,
    activity_header: ActivityHeader,
    question_id: Option<String>,
    answers: BTreeMap<String, Composer>,
    help_scroll: usize,
    sessions: Vec<crate::client::interface::history::SessionSummary>,
    history: Option<crate::client::interface::history::Conversation>,
    history_rows: Vec<String>,
    pub(super) history_width: u16,
}

#[derive(Default)]
struct ActivityHeader {
    // A run ID belongs to its first observed connection. The display never
    // promotes an unfinished historical run after the client reconnects.
    runs: std::collections::VecDeque<(String, String)>,
    stale: std::collections::BTreeSet<String>,
}

#[derive(Default, Debug)]
struct ActivitySummary {
    active: Vec<String>,
    idle: Vec<String>,
    unknown: Vec<String>,
    reported: Vec<String>,
    offline: bool,
}

impl ActivityHeader {
    fn observe(&mut self, event: &Value, data: &Value) {
        let run = field(event, "run_id");
        if field(event, "phase") != "started"
            || run.is_empty()
            || run.len() > 256
            || !data["connected"].as_bool().unwrap_or(false)
            || self.runs.iter().any(|(id, _)| id == run)
        {
            return;
        }
        self.runs
            .push_back((run.into(), field(data, "room_id").into()));
        while self.runs.len() > 256 {
            if let Some((id, _)) = self.runs.pop_front() {
                self.stale.remove(&id);
            }
        }
    }

    fn summarize(&mut self, observed: &[Value], data: &Value) -> ActivitySummary {
        let mut summary = ActivitySummary::default();
        let preview = data["preview"].as_bool().unwrap_or(false);
        if !preview && !data["connected"].as_bool().unwrap_or(false) {
            self.stale
                .extend(self.runs.iter().map(|(id, _)| id.clone()));
            summary.offline = true;
            return summary;
        }

        // Names/model labels are display groups only, never collaboration IDs.
        // Missing names stay separate by run; repeated calls with known labels
        // contribute one group, and any current active call keeps it active.
        let mut groups =
            std::collections::BTreeMap::<(String, String, String), (String, bool)>::new();
        let mut unknown = std::collections::BTreeSet::new();
        for entry in observed {
            let run = field(entry, "run_id");
            let name = field(entry, "name");
            let model = field(entry, "model");
            let source = field(entry, "source");
            let known =
                !name.is_empty() && name != "Unknown controller" && source != "metadata missing";
            let label = if known {
                panel::safe_text(name)
            } else {
                format!("Unknown #{}", run.chars().take(6).collect::<String>())
            };
            let current = preview
                || (!self.stale.contains(run)
                    && self
                        .runs
                        .iter()
                        .any(|(id, room)| id == run && room == field(data, "room_id")));
            if !current {
                if entry["active"].as_bool().unwrap_or(false) {
                    unknown.insert(label);
                }
                continue;
            }
            let key = if known {
                (source.into(), name.into(), model.into())
            } else {
                (String::new(), run.into(), String::new())
            };
            let group = groups.entry(key).or_insert((label, false));
            group.1 |= entry["active"].as_bool().unwrap_or(false);
        }
        let mut name_counts = std::collections::BTreeMap::<String, usize>::new();
        for (label, _) in groups.values() {
            *name_counts.entry(label.clone()).or_default() += 1;
        }
        for ((_, _, model), (mut label, active)) in groups {
            if name_counts[&label] > 1 && !model.is_empty() {
                label.push_str(&format!(" ({})", panel::safe_text(&model)));
            }
            if active {
                summary.active.push(label);
            } else {
                summary.idle.push(label);
            }
        }
        summary.unknown = unknown.into_iter().collect();
        let mut members = std::collections::BTreeSet::new();
        for member in items(data, "agents") {
            if members.insert(field(&member, "agent_id").to_owned()) {
                let name = field(&member, "name");
                summary.reported.push(panel::safe_text(if name.is_empty() {
                    field(&member, "agent_id")
                } else {
                    name
                }));
            }
        }
        summary
    }
}

fn activity_segments(
    summary: &ActivitySummary,
    title: &str,
    width: usize,
) -> Vec<(PanelTone, String)> {
    if width == 0 {
        return Vec::new();
    }
    if summary.offline {
        return vec![(
            PanelTone::Warning,
            panel::fit(
                &if width >= 64 {
                    format!(
                        "{} · Offline · activity unknown",
                        panel::fit(title, (width / 2).min(64))
                    )
                } else {
                    "Offline · activity unknown".into()
                },
                width,
            ),
        )];
    }
    let mut groups = vec![
        (
            PanelTone::Success,
            format!("● Active {}", summary.active.len()),
            &summary.active,
        ),
        (
            PanelTone::Muted,
            format!("○ Idle {}", summary.idle.len()),
            &summary.idle,
        ),
    ];
    if !summary.unknown.is_empty() {
        groups.push((
            PanelTone::Warning,
            format!("? Unknown {}", summary.unknown.len()),
            &summary.unknown,
        ));
    }
    if !summary.reported.is_empty() {
        groups.push((
            PanelTone::Accent,
            format!("◇ Reported {}", summary.reported.len()),
            &summary.reported,
        ));
    }
    let separator = " · ";
    let mut result = Vec::new();
    let mut available = width;
    if width >= 64 {
        let title = panel::fit(title, (width / 2).min(64));
        available = available.saturating_sub(title.width() + separator.width());
        result.push((PanelTone::Heading, title));
        result.push((PanelTone::Muted, separator.into()));
    }
    let required = |groups: &[(PanelTone, String, &Vec<String>)]| {
        groups
            .iter()
            .map(|(_, label, _)| label.width())
            .sum::<usize>()
            + groups.len().saturating_sub(1) * separator.width()
    };
    while groups.len() > 2 && required(&groups) + 8 > available {
        groups.pop();
    }
    if required(&groups) > available {
        groups[0].1 = format!("● {}", summary.active.len());
        groups[1].1 = format!("○ {}", summary.idle.len());
    }
    let mut remaining = available.saturating_sub(required(&groups));
    let mut budgets = groups
        .iter()
        .map(|(_, label, _)| label.width())
        .collect::<Vec<_>>();
    let wanted = groups
        .iter()
        .map(|(_, label, names)| {
            label.width()
                + if names.is_empty() {
                    0
                } else {
                    1 + names.join(", ").width()
                }
        })
        .collect::<Vec<_>>();
    while remaining > 0 {
        let mut changed = false;
        for (budget, desired) in budgets.iter_mut().zip(&wanted) {
            if remaining > 0 && *budget < *desired {
                *budget += 1;
                remaining -= 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut used = 0;
    for (index, ((tone, label, names), budget)) in groups.into_iter().zip(budgets).enumerate() {
        if index > 0 {
            let part = fit(separator, available.saturating_sub(used) as u16);
            used += part.width();
            result.push((PanelTone::Muted, part));
        }
        let text = if names.is_empty() {
            label
        } else {
            format!("{label} {}", names.join(", "))
        };
        let text = panel::fit(&text, budget.min(available.saturating_sub(used)));
        used += text.width();
        result.push((tone, text));
    }
    result
}

impl Conversation {
    pub(super) fn snapshot(&self) -> Value {
        json!({"view":match self.view { View::Activity=>"activity",View::Audit=>"audit",View::Agents=>"agents",View::Queue=>"queue",View::Questions=>"questions",View::History=>"history" },
            "focus":if self.content_focus {"content"} else {"input"},
            "answering":self.question_id,"pending_questions":crate::client::interface::questions::pending_count(),
            "selected":self.selected,"selected_agent":self.selected_agent,"target":self.target,"editing":self.editing,
            "draft":{"text":self.composer.text(),"cursor":self.composer.cursor(),
                "images":attachments::metadata(&self.images)}, "loading_image":self.image_task.is_some()})
    }
}

pub(super) fn prompt_rows(rows: u16) -> u16 {
    if rows >= 10 {
        3
    } else if rows >= 6 {
        1
    } else {
        0
    }
}
pub(super) fn body_rows(rows: u16) -> u16 {
    if rows >= 6 {
        rows.saturating_sub(prompt_rows(rows) + 3).max(1)
    } else {
        rows.saturating_sub(u16::from(rows >= 3)).max(1)
    }
}
pub(super) fn body_top(rows: u16) -> u16 {
    u16::from(rows >= 6)
}
pub(super) fn prompt_top(rows: u16) -> u16 {
    body_top(rows) + body_rows(rows)
}

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn message_id(value: &Value) -> &str {
    value
        .get("id")
        .or_else(|| value.get("message_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
}
fn items(value: &Value, key: &str) -> Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn latest_takeback_request(data: &Value) -> Result<&Value, &'static str> {
    let queue = data["queue"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let latest = queue
        .iter()
        .rev()
        .find(|entry| !matches!(field(entry, "status"), "delivered" | "cancelled"));
    match latest {
        Some(entry) if matches!(field(entry, "status"), "queued" | "editing" | "offline") => {
            Ok(entry)
        }
        Some(entry)
            if matches!(
                field(entry, "status"),
                "sending" | "taking-back" | "cancelling"
            ) =>
        {
            Err(
                "The latest request is changing · wait for queue confirmation, then press Shift+↑ again",
            )
        }
        Some(_) => {
            Err("Previous delivery is uncertain · review this request in /queue before editing")
        }
        None if queue
            .iter()
            .any(|entry| field(entry, "status") == "delivered") =>
        {
            Err("No pending request · delivered requests cannot be recalled")
        }
        None => Err("No pending request to take back"),
    }
}

fn recipients(value: &Value, observed: Vec<Value>) -> Vec<Value> {
    let mut entries = vec![
        json!({"agent_id":"next","name":"Next caller","role":"default recipient","status":"first eligible tool response"}),
    ];
    entries.extend(items(value, "agents"));
    entries.extend(observed);
    entries
}

fn agent_key(value: &Value) -> String {
    if field(value, "kind") == "observed" {
        format!("run:{}", field(value, "run_id"))
    } else {
        format!("member:{}", field(value, "agent_id"))
    }
}

impl State {
    pub(super) fn chat_observe_run(&mut self, event: &Value) {
        self.conversation
            .activity_header
            .observe(event, &steering::snapshot());
    }

    /// Main's context label contains the startup project and optional session
    /// title. Keep the project in the persistent footer, leaving header space
    /// for the distinct session title and observed caller activity.
    fn chat_context_labels(&self) -> (String, Option<String>) {
        let label = self.context_label.strip_prefix("  /  ").unwrap_or("");
        let (project, title) = label.split_once("  /  ").unwrap_or((label, ""));
        let project = if !project.is_empty() {
            project.to_owned()
        } else if self.preview {
            "ssmcp".into()
        } else {
            panel::safe_text(&self.project_name)
        };
        let title = (!title.is_empty() && title != project).then(|| title.to_owned());
        (project, title)
    }

    fn chat_activity_header(&mut self, data: &Value, observed: &[Value], width: u16) -> String {
        let summary = self.conversation.activity_header.summarize(observed, data);
        let suffix = if !self.log_query.is_empty() || self.log_level != 0 {
            format!(
                " · {} /{}",
                ["all", "commands", "errors"][usize::from(self.log_level)],
                self.log_query
            )
        } else {
            String::new()
        };
        let (_, session_title) = self.chat_context_labels();
        let context = session_title
            .map(|title| format!("  /  {title}"))
            .unwrap_or_default();
        let title = format!(
            "  ssmcp  /  {}{context}{suffix}",
            self.conversation.view.name()
        );
        let segments = activity_segments(&summary, &title, usize::from(width));
        let occupied = segments.iter().map(|(_, text)| text.width()).sum::<usize>();
        let mut line = segments
            .into_iter()
            .map(|(tone, text)| self.tone(tone, &text))
            .collect::<String>();
        line.push_str(&self.tone(
            PanelTone::Normal,
            &" ".repeat(usize::from(width).saturating_sub(occupied)),
        ));
        line
    }

    fn chat_saved_draft(&self) -> steering::SavedDraft {
        steering::SavedDraft {
            text: self.conversation.composer.text().into(),
            cursor: self.conversation.composer.cursor(),
            target: self.conversation.target.clone(),
            images: self.conversation.images.clone(),
        }
    }

    fn chat_restore_draft(&mut self, draft: steering::SavedDraft) {
        self.conversation.composer.set_text(&draft.text);
        while self.conversation.composer.cursor() > draft.cursor
            && self.conversation.composer.left()
        {}
        self.conversation.target = draft.target;
        self.conversation.images = draft.images;
        self.conversation.images_changed = true;
    }

    fn chat_data(&self) -> Value {
        if self.preview {
            json!({"connected":false,"supported":true,"preview":true,
                "agents":[
                    {"agent_id":"example-builder","name":"Builder","role":"implementation","status":"working"},
                    {"agent_id":"example-reviewer","name":"Reviewer","role":"review","status":"available"}],
                "queue":[{"id":"example-request","text":"Keep the interface simple; verify command output and source audit.","status":"queued","target":"next","revision":1}]})
        } else {
            steering::snapshot()
        }
    }

    fn chat_block_replacement(&mut self) {
        self.notice = Some(
            (if self.conversation.image_task.is_some() {
                "Clipboard operation in progress · continue typing, or press Esc to cancel"
            } else if self.conversation.submitting.is_some() {
                "Waiting for queue confirmation · your current draft is preserved"
            } else if self.conversation.editing.is_some() {
                "Finish the current edit before opening another request"
            } else {
                "Previous draft is parked · finish this draft or press Esc to restore it"
            })
            .into(),
        );
        self.conversation.content_focus = false;
        self.dirty = true;
    }

    /// Local enqueue has already retained the submitted text in the outbox.
    /// Free the editor immediately so subsequent input belongs to the next draft.
    fn chat_enqueued(&mut self, id: String, text: String) {
        self.conversation.submitting = Some((id, text));
        self.conversation.composer.take_text();
        self.conversation.images.clear();
        self.conversation.images_changed = true;
        self.conversation.editing = None;
        if let Some(parked) = self.conversation.parked.take() {
            self.chat_restore_draft(parked);
        }
        self.notice = Some("Sending to queue… · the next draft stays editable".into());
        self.dirty = true;
    }

    fn chat_accepted(&mut self, id: String, text: String) {
        let text = references::source(&text).to_owned();
        self.conversation.composer.remember(&text);
        if self.conversation.view == View::History {
            self.chat_change_view(View::Activity);
        }
        let mut record = format!("\n{} #{id}\n", self.tone(PanelTone::Accent, "[USER]"));
        for line in markdown::rows(&text, self.geometry.left, &self.ui) {
            let _ = writeln!(record, "{line}");
        }
        for (index, image) in steering::message_images(&id).iter().enumerate() {
            let _ = writeln!(
                record,
                "  [Image {}] {}",
                index + 1,
                panel::safe_text(&attachments::description(image))
            );
        }
        record.push_str(&self.tone(
            PanelTone::Warning,
            "  queued · waiting for the next eligible tool response",
        ));
        record.push_str("\n\n");
        self.log(record.as_bytes());
        if self
            .conversation
            .submitting
            .as_ref()
            .is_some_and(|(pending, submitted)| pending == &id && submitted == &text)
        {
            self.conversation.submitting = None;
        }
        // Even identical text may be a deliberately typed next request.
        self.notice = Some("Queued · delivered with the next eligible tool response".into());
        self.dirty = true;
    }

    fn chat_editable(
        &mut self,
        id: String,
        text: String,
        target: Option<String>,
        images: Vec<UserImage>,
    ) {
        let text = references::source(&text).to_owned();
        let same_edit = self.conversation.editing.as_ref() == Some(&id);
        if self.conversation.image_task.is_some()
            || self.conversation.submitting.is_some()
            || (!same_edit
                && (self.conversation.editing.is_some() || self.conversation.parked.is_some()))
        {
            self.chat_block_replacement();
            return;
        }
        if !same_edit {
            if !self.conversation.composer.is_empty() || !self.conversation.images.is_empty() {
                self.conversation.parked = Some(self.chat_saved_draft());
            }
            self.conversation.composer.set_text(&text);
            self.conversation.images = images;
            self.conversation.images_changed = true;
        }
        self.conversation.target = target;
        self.conversation.editing = Some(id);
        self.conversation.content_focus = false;
        self.notice = Some("Editing · this request is held until you press Enter".into());
        self.dirty = true;
    }

    fn chat_updates(&mut self) {
        if !self.conversation.restored && !self.preview {
            let (text, cursor) = steering::draft();
            let context = steering::draft_context();
            self.chat_restore_draft(steering::SavedDraft {
                text,
                cursor,
                target: context.target,
                images: steering::draft_images(),
            });
            self.conversation.editing = context.editing;
            self.conversation.parked = context.parked;
            self.conversation.restored = true;
        }
        self.chat_poll_image();
        if self.preview {
            return;
        }
        let mut failed_notice = None;
        for event in steering::take_events() {
            match event {
                steering::UiEvent::Accepted { id, text } => {
                    self.chat_accepted(id, text);
                }
                steering::UiEvent::Editable { id, text, images } => {
                    let data = steering::snapshot();
                    let target = items(&data, "queue")
                        .iter()
                        .find(|item| message_id(item) == id)
                        .and_then(|record| record.get("target").and_then(Value::as_str))
                        .filter(|target| *target != "next")
                        .map(str::to_owned);
                    self.chat_editable(id, text, target, images);
                }
                steering::UiEvent::Failed { id, message } => {
                    failed_notice = Some(message.clone());
                    if self
                        .conversation
                        .submitting
                        .as_ref()
                        .is_some_and(|(pending, _)| pending == &id)
                    {
                        self.conversation.submitting = None;
                    }
                    self.notice = Some(panel::safe_text(&message));
                }
            }
            self.dirty = true;
        }
        if let Some(id) = self.conversation.editing.clone() {
            let data = steering::snapshot();
            if let Some(record) = items(&data, "queue")
                .iter()
                .find(|record| message_id(record) == id)
            {
                if field(record, "status") == "cancelled" {
                    self.conversation.editing = None;
                    self.chat_cancel_image();
                    self.conversation.composer.take_text();
                    self.conversation.images.clear();
                    self.conversation.images_changed = true;
                    if let Some(parked) = self.conversation.parked.take() {
                        self.chat_restore_draft(parked);
                    }
                    self.notice = Some("Request cancelled · previous draft restored".into());
                } else if field(record, "status") == "delivered"
                    && self.conversation.submitting.is_none()
                {
                    self.conversation.editing = None;
                    self.notice = Some(
                        "This request was already delivered · current text is kept as a new draft"
                            .into(),
                    );
                }
            }
        }
        if let Some(message) = failed_notice {
            self.notice = Some(message);
        }
        self.chat_persist_draft();
    }

    fn chat_persist_draft(&mut self) {
        if self.preview {
            return;
        }
        let result = if self.conversation.images_changed {
            steering::set_draft_with_images(
                self.conversation.composer.text(),
                self.conversation.composer.cursor(),
                self.conversation.images.clone(),
            )
        } else {
            steering::set_draft(
                self.conversation.composer.text(),
                self.conversation.composer.cursor(),
            )
        };
        match result {
            Ok(()) => self.conversation.images_changed = false,
            Err(error) => self.notice = Some(error),
        }
        // Parked images can contain megabytes. Compare small metadata before
        // cloning a retained draft, rather than copying payloads on each redraw.
        let context_key = json!({
            "editing": self.conversation.editing, "target": self.conversation.target,
            "parked": self.conversation.parked.as_ref().map(|draft| json!({
                "text":draft.text, "cursor":draft.cursor, "target":draft.target,
                "images":attachments::metadata(&draft.images)
            }))
        });
        if self.conversation.persisted_context.as_ref() != Some(&context_key) {
            match steering::set_draft_context(steering::DraftContext {
                editing: self.conversation.editing.clone(),
                target: self.conversation.target.clone(),
                parked: self.conversation.parked.clone(),
            }) {
                Ok(()) => self.conversation.persisted_context = Some(context_key),
                Err(error) => self.notice = Some(error),
            }
        }
    }

    fn chat_replace_images(&mut self, images: Vec<UserImage>) -> Result<(), String> {
        crate::wire::validate_user_images(&images)?;
        if !self.preview {
            steering::set_draft_with_images(
                self.conversation.composer.text(),
                self.conversation.composer.cursor(),
                images.clone(),
            )?;
        }
        self.conversation.images = images;
        self.conversation.images_changed = false;
        self.dirty = true;
        Ok(())
    }

    fn chat_start_image(
        &mut self,
        source: attachments::Source,
        fallback: Option<String>,
    ) -> Result<(), String> {
        if self.conversation.image_task.is_some() {
            return Err("A clipboard or image load is already in progress · Esc cancels it".into());
        }
        let task = attachments::Task::start(source, &self.conversation.images, fallback)?;
        self.conversation.composer.begin_external_paste();
        self.conversation.image_task = Some(task);
        self.conversation.content_focus = false;
        self.notice = Some("Loading clipboard or image… · keep typing · Esc cancels".into());
        self.dirty = true;
        Ok(())
    }

    fn chat_cancel_image(&mut self) {
        self.conversation.image_task = None;
        self.conversation.composer.cancel_external_paste();
    }

    fn chat_poll_image(&mut self) {
        let Some(result) = self
            .conversation
            .image_task
            .as_mut()
            .and_then(attachments::Task::poll)
        else {
            return;
        };
        let fallback = self
            .conversation
            .image_task
            .take()
            .and_then(|mut task| task.fallback.take());
        self.chat_loaded_image(result, fallback);
    }

    fn chat_loaded_image(
        &mut self,
        result: Result<attachments::Loaded, String>,
        fallback: Option<String>,
    ) {
        match result {
            Ok(attachments::Loaded::Copied) => {
                self.conversation.composer.cancel_external_paste();
                self.notice = Some("Latest agent message copied to the clipboard".into());
            }
            Ok(attachments::Loaded::Text(text)) => {
                let stored;
                let inserted = if text.len() > 8 * 1024 {
                    let space = crate::client::composer::MAX_DRAFT_BYTES
                        .saturating_sub(self.conversation.composer.text().len());
                    let result = if references::paste::REFERENCE_BYTES + 2 > space {
                        Err(
                            "Not enough draft space for this paste; your draft was preserved"
                                .into(),
                        )
                    } else {
                        references::paste::store(&text)
                    };
                    match result {
                        Ok(reference) => stored = format!("\n{reference}\n"),
                        Err(error) => {
                            self.conversation.composer.cancel_external_paste();
                            self.notice = Some(error);
                            self.dirty = true;
                            return;
                        }
                    }
                    stored.as_str()
                } else {
                    &text
                };
                let pasted = self.conversation.composer.finish_external_paste(inserted);
                self.notice = Some(if !pasted {
                    "Paste exceeds the 16 KiB draft limit; the draft was preserved".into()
                } else if text.len() > 8 * 1024 {
                    "Long paste saved as @file · add your instructions after the reference".into()
                } else if text.is_empty() {
                    "The clipboard is empty · use /attach PATH for an image file".into()
                } else if attachments::pasted_paths(&text).is_some() {
                    "Path pasted as text · image was not attached; /attach PATH shows the error"
                        .into()
                } else {
                    "Clipboard text pasted".into()
                });
            }
            Ok(attachments::Loaded::Images(images)) => {
                self.conversation.composer.cancel_external_paste();
                let mut combined = self.conversation.images.clone();
                let mut added = Vec::new();
                for image in images {
                    if !combined.iter().any(|old| old.id == image.id) {
                        added.push(attachments::description(&image));
                        combined.push(image);
                    }
                }
                match self.chat_replace_images(combined) {
                    Ok(()) if added.is_empty() => {
                        self.notice = Some("This image is already attached".into())
                    }
                    Ok(()) => {
                        self.notice = Some(format!(
                            "Attached {} · /images details · /detach N removes",
                            added.join(" · ")
                        ))
                    }
                    Err(error) => self.notice = Some(error),
                }
            }
            Err(error) => {
                let restored = fallback
                    .as_deref()
                    .is_some_and(|text| self.conversation.composer.finish_external_paste(text));
                self.conversation.composer.cancel_external_paste();
                self.notice = Some(if restored {
                    format!("{error} · pasted path preserved as text; no image attached")
                } else {
                    error
                });
            }
        }
        self.dirty = true;
    }

    fn chat_change_view(&mut self, view: View) {
        self.conversation.view = view;
        self.conversation.content_focus = view != View::Activity;
        self.conversation.selected = 0;
        self.conversation.selected_agent = None;
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        self.conversation.detail = false;
        self.conversation.detail_scroll = 0;
        self.help = false;
        self.notice = None;
        self.dirty = true;
    }

    fn chat_agents(&mut self) {
        self.chat_change_view(View::Agents);
        let data = self.chat_data();
        let entries = recipients(&data, self.audit.observed_controllers());
        let target = self.conversation.target.as_deref().unwrap_or("next");
        self.conversation.selected = entries
            .iter()
            .position(|entry| field(entry, "agent_id") == target)
            .unwrap_or(0);
        self.conversation.selected_agent = entries.get(self.conversation.selected).map(agent_key);
    }

    fn chat_command(&mut self, text: &str) -> Result<bool, String> {
        if let Some(command) = text.strip_prefix('!') {
            if self.preview {
                return Err("Preview does not execute shell commands".into());
            }
            crate::client::interface::harness::submit_shell(command)?;
            self.notice = Some("Shell command started · results appear here".into());
            return Ok(true);
        }
        if !text.starts_with('/') {
            return Ok(false);
        }
        let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        match command {
            "/help" => {
                self.conversation.help_scroll = 0;
                self.help = true;
                self.conversation.content_focus = true;
            }
            "/copy" => {
                if !rest.is_empty() {
                    return Err(
                        "Use /copy without arguments to copy the latest agent message".into(),
                    );
                }
                let text = crate::client::messages::last_text()
                    .ok_or("No agent message is available to copy yet")?;
                if self.conversation.image_task.is_some() {
                    return Err(
                        "Another clipboard operation is in progress · Esc cancels it".into(),
                    );
                }
                let task = attachments::Task::start(attachments::Source::Copy(text), &[], None)?;
                self.conversation.image_task = Some(task);
                self.notice = Some("Copying the latest agent message…".into());
            }
            "/paste" | "/attach" => {
                let source = if command == "/paste" {
                    if !rest.is_empty() {
                        return Err("Use /paste without arguments, or /attach PATH".into());
                    }
                    attachments::Source::Clipboard
                } else {
                    attachments::Source::Paths(attachments::pasted_paths(rest).ok_or(
                        "Use /attach PATH for PNG, JPEG, GIF or WebP; quote paths with spaces",
                    )?)
                };
                let draft = self.chat_saved_draft();
                self.conversation.composer.take_text();
                if let Err(error) = self.chat_start_image(source, None) {
                    self.chat_restore_draft(draft);
                    return Err(error);
                }
            }
            "/images" => {
                if !rest.is_empty() {
                    return Err("Use /images without arguments".into());
                }
                let mut text = String::from("\n[IMAGES] Draft attachments\n");
                if self.conversation.images.is_empty() {
                    text.push_str("  No images attached · Ctrl+V, /paste or /attach PATH\n");
                }
                for (index, image) in self.conversation.images.iter().enumerate() {
                    let _ = writeln!(
                        text,
                        "  [Image {}] {}",
                        index + 1,
                        panel::safe_text(&attachments::description(image))
                    );
                }
                text.push_str("  /detach N removes an image · /detach all clears attachments\n\n");
                self.log(text.as_bytes());
                self.chat_change_view(View::Activity);
            }
            "/detach" => {
                let mut images = self.conversation.images.clone();
                if images.is_empty() {
                    return Err("The draft has no attached images".into());
                }
                if rest == "all" {
                    images.clear();
                } else {
                    let index = if rest.is_empty() {
                        images.len()
                    } else {
                        rest.parse::<usize>()
                            .map_err(|_| "Use /detach N or /detach all")?
                    };
                    if index == 0 || index > images.len() {
                        return Err(format!("Choose an image from 1 to {}", images.len()));
                    }
                    images.remove(index - 1);
                }
                self.chat_replace_images(images)?;
                self.notice = Some("Image removed from the draft".into());
            }
            "/agents" => self.chat_agents(),
            "/questions" => {
                if !rest.is_empty() {
                    return Err("Use /questions to answer pending agent questions".into());
                }
                self.chat_questions();
            }
            "/resume" => {
                if !rest.is_empty() {
                    return Err("Use /resume to choose a saved conversation".into());
                }
                self.chat_resume();
            }
            "/pi" | "/cc" | "/codex" => {
                if self.preview {
                    return Err("Preview does not invoke installed harnesses".into());
                }
                crate::client::interface::harness::submit(&command[1..], rest)?;
                self.notice = Some("Harness request queued · results appear here".into());
            }
            "/tasks" => {
                if self.preview {
                    return Err("Preview has no live command jobs".into());
                }
                crate::client::interface::harness::submit_tasks(rest)?;
                self.notice = Some("Task status requested · results appear here".into());
            }
            "/queue" => self.chat_change_view(View::Queue),
            "/audit" => self.chat_change_view(View::Audit),
            "/logs" => self.chat_change_view(View::Activity),
            "/sidebar" => self.actions.push("sidebar.toggle"),
            "/theme" => {
                if rest.is_empty() {
                    self.actions.push("theme.next");
                } else {
                    let theme = crate::client::ui::Theme::parse(rest)?;
                    // Apply after releasing the terminal lock, just like function keys.
                    crate::client::interface::queue_theme(theme);
                }
            }
            "/filter" => {
                if let Some(level) = rest.strip_prefix("--") {
                    self.log_level = match level {
                        "all" => 0,
                        "commands" => 1,
                        "errors" => 2,
                        _ => {
                            return Err("Use /filter text, or --all / --commands / --errors".into());
                        }
                    };
                } else {
                    if rest.len() > 256 || rest.chars().any(char::is_control) {
                        return Err("A log filter accepts up to 256 bytes of plain text".into());
                    }
                    self.log_query = rest.into();
                }
                self.filter_offset = 0;
                self.chat_change_view(View::Activity);
            }
            "/clear" => {
                self.events.clear();
                self.logs = vt100::Parser::new(
                    nonzero(self.geometry.log_rows),
                    nonzero(self.geometry.left),
                    SCROLLBACK_ROWS,
                );
                self.log_query.clear();
                self.log_level = 0;
                self.filter_offset = 0;
                self.chat_change_view(View::Activity);
            }
            "/reload" | "/restart" => {
                if !rest.is_empty() {
                    return Err(format!(
                        "Use {command} without arguments to restart this session"
                    ));
                }
                if self.preview {
                    return Err("Preview has no connected session to restart".into());
                }
                // Preserve the parked request before the restart worker can run.
                // The command itself should never reappear as a user requirement.
                let command = self.chat_saved_draft();
                let parked = self.conversation.parked.take();
                self.conversation.composer.take_text();
                if let Some(draft) = parked.clone() {
                    self.chat_restore_draft(draft);
                }
                self.chat_persist_draft();
                if let Err(error) = steering::request_reload() {
                    self.chat_restore_draft(command);
                    self.conversation.parked = parked;
                    self.chat_persist_draft();
                    return Err(error);
                }
                self.notice =
                    Some("Preparing restart · preserving confirmed pending requests".into());
            }
            _ => return Err("Unknown command · /help lists local commands".into()),
        }
        Ok(true)
    }

    fn chat_refresh_completions(&mut self) {
        let enabled = !self.preview
            && !self.selection
            && !self.help
            && self.conversation.view != View::Questions
            && !self.conversation.content_focus;
        self.conversation.composer.refresh_completions(enabled);
    }

    fn chat_menu_visible(&self) -> bool {
        !self.conversation.content_focus
            && self.conversation.image_task.is_none()
            && (self.conversation.editing.is_none()
                || self.conversation.composer.reference_active())
            && self
                .conversation
                .composer
                .menu_visible(self.geometry.left, prompt_rows(self.geometry.rows))
    }

    fn chat_submit(&mut self) {
        if self.conversation.image_task.is_some() {
            self.chat_block_replacement();
            return;
        }
        self.chat_refresh_completions();
        if self.conversation.composer.completion_loading() {
            self.notice = Some("Loading reference suggestions · your draft is preserved".into());
            return;
        }
        if self.chat_menu_visible()
            && self
                .conversation
                .composer
                .selected_suggestion()
                .is_some_and(|item| item.reference)
        {
            if let Err(error) = self.conversation.composer.accept_suggestion() {
                self.notice = Some(error);
            } else {
                self.notice = None;
            }
            return;
        }
        if self.conversation.submitting.is_some() {
            self.notice = Some("Waiting for queue confirmation · your draft is preserved".into());
            return;
        }
        let text = if self.chat_menu_visible() {
            self.conversation
                .composer
                .selected_suggestion()
                .expect("a visible command menu has a selection")
                .name
                .to_owned()
        } else {
            self.conversation.composer.text().to_owned()
        };
        if text.trim().is_empty() && self.conversation.images.is_empty() {
            return;
        }
        if self.conversation.editing.is_none()
            || matches!(
                text.split_whitespace().next(),
                Some("/paste" | "/attach" | "/images" | "/detach" | "/copy")
            )
        {
            match self.chat_command(&text) {
                Ok(true) => {
                    if !matches!(
                        text.split_whitespace().next(),
                        Some("/reload" | "/restart" | "/paste" | "/attach")
                    ) {
                        self.conversation.composer.take_text();
                    }
                    return;
                }
                Err(error) => {
                    self.notice = Some(error);
                    return;
                }
                Ok(false) => {}
            }
        }
        if self.preview {
            self.notice = Some("Preview only · connect a session to queue a requirement".into());
            return;
        }
        // The native edit API treats omitted target as "keep the previous one".
        // The composer always shows a deliberate choice, including Next caller.
        let target = Some(self.conversation.target.as_deref().unwrap_or("next"));
        let result = if let Some(id) = self.conversation.editing.clone() {
            steering::publish_edit_with_images(&id, &text, target, self.conversation.images.clone())
                .map(|()| id)
        } else {
            steering::submit_with_images(&text, target, self.conversation.images.clone())
        };
        match result {
            Ok(id) => self.chat_enqueued(id, text),
            Err(error) => self.notice = Some(error),
        }
    }

    fn chat_selected(&self) -> Option<Value> {
        let data = self.chat_data();
        let entries = if self.conversation.view == View::Agents {
            recipients(&data, self.audit.observed_controllers())
        } else {
            items(&data, "queue")
        };
        if self.conversation.view == View::Agents
            && let Some(selected) = &self.conversation.selected_agent
            && let Some(entry) = entries.iter().find(|entry| agent_key(entry) == *selected)
        {
            return Some(entry.clone());
        }
        entries.get(self.conversation.selected).cloned()
    }

    fn chat_prepare_takeback(&mut self, data: &Value) -> Option<String> {
        if self.conversation.image_task.is_some()
            || self.conversation.submitting.is_some()
            || self.conversation.editing.is_some()
            || self.conversation.parked.is_some()
        {
            self.chat_block_replacement();
            return None;
        }
        match latest_takeback_request(data) {
            Ok(entry) => {
                self.conversation.content_focus = false;
                self.conversation.detail = false;
                self.notice = Some(
                    "Taking back the latest request… · draft and images stay here until confirmed"
                        .into(),
                );
                self.dirty = true;
                Some(message_id(entry).into())
            }
            Err(message) => {
                self.notice = Some(message.into());
                self.dirty = true;
                None
            }
        }
    }

    fn chat_takeback_latest(&mut self) {
        let data = self.chat_data();
        if let Some(id) = self.chat_prepare_takeback(&data) {
            if self.preview {
                self.notice = Some("Preview only · Shift+↑ takes back the latest pending request in a connected session".into());
            } else if let Err(error) = steering::take_back(&id) {
                self.notice = Some(error);
            }
        }
    }

    /// Leaving an unchanged held request is safe: its original content remains
    /// in the queue as Editing. Changed text/images stay here until submitted or
    /// cancelled; restoring another draft must not discard those changes.
    fn chat_leave_unmodified_edit(&mut self, data: &Value) -> bool {
        let Some(id) = self.conversation.editing.as_deref() else {
            return false;
        };
        if self.conversation.content_focus
            || self.conversation.detail
            || self.conversation.submitting.is_some()
            || self.conversation.parked.is_none()
        {
            return false;
        }
        let queue = items(data, "queue");
        let unchanged = queue
            .iter()
            .find(|entry| message_id(entry) == id)
            .is_some_and(|entry| {
                let target = field(entry, "target");
                field(entry, "status") == "editing"
                    && references::source(field(entry, "text")) == self.conversation.composer.text()
                    && (if target.is_empty() { "next" } else { target })
                        == self.conversation.target.as_deref().unwrap_or("next")
                    && items(entry, "images") == attachments::metadata(&self.conversation.images)
            });
        if unchanged {
            self.conversation.editing = None;
            if let Some(parked) = self.conversation.parked.take() {
                self.chat_restore_draft(parked);
            }
            self.notice =
                Some("Previous draft restored · taken-back request stays held in /queue".into());
        } else {
            self.notice = Some("This edit has changes · finish or cancel it first; both drafts and images are preserved".into());
        }
        true
    }

    fn chat_open(&mut self, edit: bool) {
        if edit
            && self.conversation.view == View::Queue
            && (self.conversation.image_task.is_some()
                || self.conversation.editing.is_some()
                || self.conversation.submitting.is_some()
                || self.conversation.parked.is_some())
        {
            self.chat_block_replacement();
            return;
        }
        match self.conversation.view {
            View::Audit => self.audit.open(),
            View::Agents => {
                if let Some(agent) = self.chat_selected() {
                    if field(&agent, "kind") == "observed" {
                        if self.audit.open_run(field(&agent, "run_id")) {
                            self.conversation.detail = true;
                            self.conversation.content_focus = true;
                            self.notice = Some(
                                "Observed call · ← / Esc / left click returns to agents".into(),
                            );
                        } else {
                            self.notice =
                                Some("This call is no longer in the retained audit history".into());
                        }
                        return;
                    }
                    self.conversation.target = (field(&agent, "agent_id") != "next")
                        .then(|| field(&agent, "agent_id").into());
                    self.chat_change_view(View::Activity);
                    self.notice = Some(format!(
                        "To {} · Enter sends · ← at draft start switches agent",
                        panel::safe_text(field(&agent, "name"))
                    ));
                }
            }
            View::Queue => {
                if let Some(message) = self.chat_selected() {
                    if edit
                        && matches!(field(&message, "status"), "queued" | "editing")
                        && !self.preview
                    {
                        if let Err(error) = steering::take_back(message_id(&message)) {
                            self.notice = Some(error);
                        }
                    } else {
                        self.conversation.detail = true;
                        self.conversation.detail_scroll = 0;
                    }
                }
            }
            View::History => self.chat_resume_selected(),
            View::Activity => {
                if !self.log_query.is_empty() || self.log_level != 0 {
                    self.notice = Some("Clear the log filter to expand individual events".into());
                    return;
                }
                if edit {
                    self.events.toggle();
                } else {
                    self.events.open();
                }
            }
            View::Questions => {}
        }
    }

    fn chat_move(&mut self, delta: isize) {
        match self.conversation.view {
            View::Activity => {
                self.scroll_logs(-delta);
                self.events.scroll_by(delta);
            }
            View::Audit => self.audit.scroll_by(delta),
            View::Agents if self.conversation.detail => self.audit.scroll_by(delta),
            View::Queue | View::History if self.conversation.detail => {
                self.conversation.detail_scroll =
                    self.conversation.detail_scroll.saturating_add_signed(delta);
            }
            View::History => {
                self.conversation.selected = self
                    .conversation
                    .selected
                    .saturating_add_signed(delta)
                    .min(self.conversation.sessions.len().saturating_sub(1));
            }
            view => {
                let data = self.chat_data();
                let entries = if view == View::Agents {
                    recipients(&data, self.audit.observed_controllers())
                } else {
                    items(&data, "queue")
                };
                if view == View::Agents
                    && let Some(selected) = &self.conversation.selected_agent
                    && let Some(index) = entries
                        .iter()
                        .position(|entry| agent_key(entry) == *selected)
                {
                    self.conversation.selected = index;
                }
                self.conversation.selected = self
                    .conversation
                    .selected
                    .saturating_add_signed(delta)
                    .min(entries.len().saturating_sub(1));
                if view == View::Agents {
                    self.conversation.selected_agent =
                        entries.get(self.conversation.selected).map(agent_key);
                }
            }
        }
    }

    fn chat_key(&mut self, key: Key) {
        self.chat_refresh_completions();
        if matches!(key, Key::Select) || (self.selection && matches!(key, Key::Escape)) {
            self.selection = !self.selection;
            self.notice = self.selection.then(|| {
                "Copy mode · drag to select · use your terminal Copy · F7/Esc back".into()
            });
            self.dirty = true;
            return;
        }
        if self.selection {
            return;
        }
        if self.help {
            let page = usize::from(body_rows(self.geometry.rows).saturating_sub(1).max(1));
            match key {
                Key::Help | Key::Escape => self.chat_escape(),
                Key::AuditUp => {
                    self.conversation.help_scroll = self.conversation.help_scroll.saturating_sub(1)
                }
                Key::AuditDown => {
                    self.conversation.help_scroll = self.conversation.help_scroll.saturating_add(1)
                }
                Key::Up => {
                    self.conversation.help_scroll =
                        self.conversation.help_scroll.saturating_sub(page)
                }
                Key::Down => {
                    self.conversation.help_scroll =
                        self.conversation.help_scroll.saturating_add(page)
                }
                Key::Home | Key::HistoryStart => self.conversation.help_scroll = 0,
                Key::End | Key::HistoryEnd => self.conversation.help_scroll = usize::MAX,
                Key::Mouse {
                    button,
                    release: false,
                    ..
                } if button & 64 != 0 => {
                    self.conversation.help_scroll = if button & 1 == 0 {
                        self.conversation.help_scroll.saturating_sub(3)
                    } else {
                        self.conversation.help_scroll.saturating_add(3)
                    };
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        if matches!(key, Key::Takeback) {
            if self.conversation.view == View::Questions {
                self.chat_change_view(View::Activity);
            }
            self.chat_takeback_latest();
            return;
        }
        if self.conversation.view == View::Questions {
            self.question_key(key);
            return;
        }
        if self.conversation.view == View::Agents
            && self.conversation.content_focus
            && matches!(key, Key::Delete | Key::Newline)
        {
            self.chat_change_view(View::Activity);
        }
        let content = self.conversation.content_focus;
        match key {
            Key::Help => {
                self.conversation.help_scroll = 0;
                self.help = !self.help;
                self.conversation.content_focus = self.help;
            }
            Key::Layout => match self.conversation.view.next() {
                View::History => self.chat_resume(),
                View::Questions => self.chat_questions(),
                view => self.chat_change_view(view),
            },
            Key::Sidebar => self.actions.push("sidebar.toggle"),
            Key::Theme => self.actions.push("theme.next"),
            Key::Reload => self.actions.push("reload"),
            Key::Save => self.actions.push("save"),
            Key::Filter => {
                self.log_level = (self.log_level + 1) % 3;
                self.filter_offset = 0;
            }
            Key::FocusBack => self.conversation.content_focus = !content,
            Key::Up => self.chat_move(
                -(isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::Down => self.chat_move(
                (isize::try_from(self.geometry.log_rows).unwrap_or(1).max(1) - 1).max(1),
            ),
            Key::HistoryStart => {
                self.scroll_logs(isize::MAX);
                self.events.scroll_by(isize::MIN.saturating_add(1));
                self.chat_change_view(View::Activity);
            }
            Key::HistoryEnd => {
                self.scroll_logs(isize::MIN);
                self.events.latest();
                self.chat_change_view(View::Activity);
            }
            Key::AuditUp
                if content
                    && self.conversation.view == View::Activity
                    && self.log_query.is_empty()
                    && self.log_level == 0 =>
            {
                self.events.select(-1)
            }
            Key::AuditDown
                if content
                    && self.conversation.view == View::Activity
                    && self.log_query.is_empty()
                    && self.log_level == 0 =>
            {
                self.events.select(1)
            }
            Key::AuditUp if content => self.chat_move(-1),
            Key::AuditDown if content => self.chat_move(1),
            Key::AuditUp => {
                if self.chat_menu_visible() {
                    self.conversation.composer.move_suggestion(-1);
                    self.dirty = true;
                    return;
                }
                if self.conversation.submitting.is_some() {
                    self.notice =
                        Some("Waiting for queue confirmation · your draft is preserved".into());
                    return;
                }
                if !self.conversation.composer.up(self.geometry.left) {
                    if self.conversation.image_task.is_some() {
                        self.chat_block_replacement();
                        return;
                    }
                    if self.conversation.editing.is_none() && self.conversation.parked.is_some() {
                        self.chat_block_replacement();
                        return;
                    }
                    let data = self.chat_data();
                    let queued = items(&data, "queue")
                        .into_iter()
                        .rev()
                        .find(|entry| field(entry, "status") == "queued");
                    if self.conversation.editing.is_none()
                        && !self.preview
                        && let Some(queued) = queued
                    {
                        if let Err(error) = steering::take_back(message_id(&queued)) {
                            self.notice = Some(error);
                        }
                    } else if self.conversation.editing.is_none()
                        && self.conversation.images.is_empty()
                    {
                        self.conversation.composer.history_previous();
                    }
                }
            }
            Key::AuditDown => {
                if self.chat_menu_visible() {
                    self.conversation.composer.move_suggestion(1);
                    self.dirty = true;
                    return;
                }
                if !self.conversation.composer.down(self.geometry.left)
                    && self.conversation.editing.is_none()
                {
                    if self.conversation.parked.is_some() || self.conversation.image_task.is_some()
                    {
                        self.chat_block_replacement();
                    } else if self.conversation.images.is_empty() {
                        self.conversation.composer.history_next();
                    }
                }
            }
            Key::Back if self.conversation.view == View::Agents && content => {
                self.chat_escape();
            }
            Key::Back if self.conversation.view == View::Activity && content => {
                if !self.log_query.is_empty() || self.log_level != 0 {
                    self.notice = Some("Clear the log filter to fold individual events".into());
                } else if !self.events.back() {
                    self.chat_agents();
                }
            }
            Key::Back if content => {
                self.audit.back();
                self.conversation.detail = false;
                self.conversation.detail_scroll = 0;
            }
            Key::Back if self.conversation.composer.cursor() == 0 => self.chat_agents(),
            Key::Back => {
                self.conversation.composer.left();
            }
            Key::Open if content => self.chat_open(false),
            Key::Open => {
                self.conversation.composer.right();
            }
            Key::Home if content => self.chat_move(isize::MIN.saturating_add(1)),
            Key::End if content => self.chat_move(isize::MAX),
            Key::Home => {
                self.conversation.composer.home();
            }
            Key::End => {
                self.conversation.composer.end();
            }
            Key::Delete if content && self.conversation.view == View::Queue => {
                if let Some(message) = self.chat_selected()
                    && let Err(error) = steering::cancel(message_id(&message))
                {
                    self.notice = Some(error);
                }
            }
            Key::Delete => {
                self.conversation.composer.delete();
            }
            Key::Newline => {
                self.conversation.composer.insert("\n");
            }
            Key::Escape => self.chat_escape(),
            Key::Mouse {
                button,
                x,
                y,
                release,
            } if !release && x < self.geometry.left => {
                if button & 64 != 0 {
                    self.chat_move(if button & 1 == 0 { -3 } else { 3 });
                } else if y > prompt_top(self.geometry.rows) {
                    self.conversation.content_focus = false;
                } else {
                    self.conversation.content_focus = true;
                    if self.conversation.view == View::Activity
                        && self.log_query.is_empty()
                        && self.log_level == 0
                    {
                        if let Some(row) = y.checked_sub(body_top(self.geometry.rows)) {
                            if self.events.mouse_select(row) {
                                if button == 0 {
                                    self.events.toggle();
                                } else if button == 2 {
                                    self.events.open();
                                }
                            }
                        }
                        self.dirty = true;
                        return;
                    }
                    if matches!(
                        self.conversation.view,
                        View::Agents | View::Queue | View::History
                    ) && !self.conversation.detail
                    {
                        if self.conversation.view == View::Agents
                            && x >= self.chat_agent_list_width()
                        {
                            return;
                        }
                        let Some(index) = self.chat_list_hit(y) else {
                            self.dirty = true;
                            return;
                        };
                        self.conversation.selected = index;
                        self.conversation.selected_agent = None;
                    }
                    if button == 2 {
                        self.chat_open(false);
                    } else if button == 0
                        && (self.conversation.view == View::Audit
                            || (self.conversation.view == View::Agents && self.conversation.detail))
                    {
                        self.audit.back();
                        self.conversation.detail = false;
                    }
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_escape(&mut self) {
        self.dirty = true;
        if self.conversation.image_task.is_some() {
            self.chat_cancel_image();
            self.notice = Some("Clipboard operation cancelled · draft preserved".into());
            return;
        }
        if self.conversation.view == View::Questions && !self.help {
            self.question_key(Key::Escape);
            return;
        }
        if !self.help && (self.chat_menu_visible() || self.conversation.composer.reference_active())
        {
            self.conversation.composer.dismiss_suggestions();
            self.notice = None;
            return;
        }
        if self.help {
            self.help = false;
            self.conversation.content_focus = false;
            self.notice = None;
            return;
        }
        if self.conversation.view == View::History {
            if self.conversation.detail {
                self.conversation.detail = false;
                self.conversation.content_focus = true;
                self.notice = None;
            } else {
                self.chat_change_view(View::Activity);
            }
            return;
        }
        if self.conversation.view == View::Agents {
            if self.conversation.detail {
                self.audit.back();
                self.conversation.detail = false;
                self.conversation.content_focus = true;
                self.notice = None;
            } else {
                self.chat_change_view(View::Activity);
            }
            return;
        }
        if self.chat_leave_unmodified_edit(&self.chat_data()) {
            return;
        }
        if !self.conversation.content_focus
            && !self.conversation.detail
            && self.conversation.editing.is_none()
            && self.conversation.submitting.is_none()
            && let Some(parked) = self.conversation.parked.take()
        {
            self.chat_restore_draft(parked);
            self.notice = Some("Previous draft restored · recipient and cursor preserved".into());
            return;
        }
        self.help = false;
        if self.conversation.detail {
            self.conversation.detail = false;
            if self.conversation.view == View::Agents {
                self.audit.back();
            }
        } else {
            self.audit.back();
            self.conversation.content_focus = false;
        }
        self.notice = None;
    }

    fn chat_character(&mut self, character: char) {
        self.chat_refresh_completions();
        if self.selection {
            if character == '\u{1b}' {
                self.chat_key(Key::Select);
            }
            return;
        }
        if self.help {
            if character == '\u{1b}' {
                self.chat_escape();
            } else if !character.is_control() {
                self.help = false;
                self.conversation.content_focus = false;
                self.notice = None;
                if !self.conversation.composer.insert(&character.to_string()) {
                    self.notice = Some("Draft limit reached · 16 KiB".into());
                }
                self.dirty = true;
            }
            return;
        }
        if self.conversation.view == View::Questions {
            self.question_character(character);
            return;
        }
        if self.conversation.view == View::Agents
            && self.conversation.content_focus
            && matches!(
                character,
                '\n' | '\u{1}' | '\u{5}' | '\u{15}' | '\u{b}' | '\u{17}' | '\u{7f}' | '\u{8}'
            )
        {
            self.chat_change_view(View::Activity);
        }
        match character {
            'r' | 'R'
                if self.conversation.content_focus && self.conversation.view == View::Queue =>
            {
                if let Some(message) = self.chat_selected() {
                    let recoverable = matches!(
                        field(&message, "status"),
                        "uncertain" | "retarget-required" | "offline"
                    );
                    let same_retarget = recoverable
                        && self.conversation.editing.as_deref() == Some(message_id(&message));
                    if self.conversation.image_task.is_some()
                        || self.conversation.submitting.is_some()
                        || (self.conversation.parked.is_some() && !same_retarget)
                        || self
                            .conversation
                            .editing
                            .as_ref()
                            .is_some_and(|editing| !recoverable || editing != message_id(&message))
                    {
                        self.chat_block_replacement();
                        return;
                    }
                    if recoverable && !self.preview {
                        if let Err(error) = steering::retarget(message_id(&message), None) {
                            self.notice = Some(error);
                        }
                    } else {
                        if !self.conversation.composer.is_empty()
                            || !self.conversation.images.is_empty()
                        {
                            self.conversation.parked = Some(self.chat_saved_draft());
                        }
                        self.conversation
                            .composer
                            .set_text(references::source(field(&message, "text")));
                        self.conversation.images = steering::message_images(message_id(&message));
                        self.conversation.images_changed = true;
                        self.conversation.editing = None;
                        self.conversation.target = None;
                        self.conversation.content_focus = false;
                        self.notice = Some(
                            (if self.conversation.parked.is_some() {
                                "Copied to a new draft · Enter sends · Esc restores previous draft"
                            } else {
                                "Copied to a new draft · Enter sends a new request"
                            })
                            .into(),
                        );
                    }
                }
            }
            '\u{1b}' => self.chat_escape(),
            '\u{16}' => {
                if let Err(error) = self.chat_start_image(attachments::Source::Clipboard, None) {
                    self.notice = Some(error);
                }
            }
            '\t' => {
                if self.chat_menu_visible() {
                    if let Err(error) = self.conversation.composer.accept_suggestion() {
                        self.notice = Some(error);
                    }
                } else if self.conversation.composer.completion_loading() {
                    self.notice =
                        Some("Loading reference suggestions · your draft is preserved".into());
                } else {
                    self.conversation.content_focus = !self.conversation.content_focus;
                }
            }
            '\u{f}' => {
                self.chat_change_view(if self.conversation.view == View::Audit {
                    View::Activity
                } else {
                    View::Audit
                });
            }
            '\r' if self.conversation.content_focus => self.chat_open(true),
            '\r' => {
                if self.conversation.composer.text()[..self.conversation.composer.cursor()]
                    .ends_with('\\')
                {
                    self.conversation.composer.backspace();
                    self.conversation.composer.insert("\n");
                } else {
                    self.chat_submit();
                }
            }
            '\n' => {
                self.conversation.composer.insert("\n");
            }
            '\u{1}' => {
                self.conversation.composer.home();
            }
            '\u{5}' => {
                self.conversation.composer.end();
            }
            '\u{15}' => {
                self.conversation.composer.clear_before();
            }
            '\u{b}' => {
                self.conversation.composer.clear_after();
            }
            '\u{17}' => {
                self.conversation.composer.delete_word();
            }
            '\u{7f}' | '\u{8}' => {
                self.conversation.composer.backspace();
            }
            '?' if self.conversation.composer.is_empty() && !self.conversation.content_focus => {
                self.help = !self.help;
            }
            ch if !ch.is_control() => {
                if self.conversation.view == View::Agents {
                    self.chat_change_view(View::Activity);
                }
                self.conversation.content_focus = false;
                if !self.conversation.composer.insert(&ch.to_string()) {
                    self.notice = Some("Draft limit reached · 16 KiB".into());
                }
            }
            _ => {}
        }
        self.dirty = true;
    }

    fn chat_paste_text(&mut self, text: &str) -> Result<(), String> {
        if self.conversation.view != View::Questions
            && self.conversation.image_task.is_none()
            && let Some(paths) = attachments::pasted_paths(text)
        {
            match self.chat_start_image(attachments::Source::Paths(paths), Some(text.to_owned())) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    // A failed image load must not eat an ordinary path paste.
                    if !self.conversation.composer.paste(text) {
                        return Err(format!("{error}; the draft was preserved"));
                    }
                    self.notice = Some(format!("{error}; path pasted as text"));
                    return Ok(());
                }
            }
        }
        let large = text.len() > 8 * 1024;
        let question = self.conversation.view == View::Questions;
        let used = if question {
            self.question_paste_len()?
        } else {
            self.conversation.composer.text().len()
        };
        let required = if large {
            references::paste::REFERENCE_BYTES + 2
        } else {
            text.len()
        };
        if required > crate::client::composer::MAX_DRAFT_BYTES.saturating_sub(used) {
            return Err("Not enough draft space for this paste; your draft was preserved".into());
        }
        let stored;
        let inserted = if large {
            stored = format!("\n{}\n", references::paste::store(text)?);
            stored.as_str()
        } else {
            text
        };
        let accepted = if question {
            self.question_paste(inserted)
        } else {
            self.conversation.composer.paste(inserted)
        };
        if !accepted {
            return Err("Paste could not be inserted; your draft was preserved".into());
        }
        if large {
            self.notice = Some(
                "Long paste saved as @file · add your instructions after the reference".into(),
            );
        }
        Ok(())
    }

    fn chat_output(&mut self, output: &[u8]) {
        if output == b"\x1b[200~" {
            self.conversation.paste = Some(Vec::new());
            self.conversation.paste_overflow = false;
            return;
        }
        if output == b"\x1b[201~" {
            if let Some(paste) = self.conversation.paste.take() {
                if !paste.is_empty() {
                    if self.conversation.view == View::Agents {
                        self.chat_change_view(View::Activity);
                    }
                    if self.help {
                        self.help = false;
                        self.conversation.content_focus = false;
                    }
                }
                let result = if self.selection {
                    Err("Leave copy mode before pasting".into())
                } else if self.conversation.paste_overflow {
                    Err("Paste exceeds 4 MiB; save it as a file and use @path".into())
                } else {
                    match std::str::from_utf8(&paste) {
                        Ok(text) => self.chat_paste_text(text),
                        Err(_) => {
                            Err("Paste is not UTF-8 text; save it as a file and use @path".into())
                        }
                    }
                };
                if let Err(error) = result {
                    self.notice = Some(error);
                }
            }
            self.dirty = true;
            return;
        }
        if let Some(paste) = &mut self.conversation.paste {
            if paste.len().saturating_add(output.len()) <= references::paste::MAX_PASTE_BYTES {
                paste.extend_from_slice(output);
            } else {
                self.conversation.paste_overflow = true;
            }
            return;
        }
        self.conversation.text_pending.extend_from_slice(output);
        match std::str::from_utf8(&self.conversation.text_pending) {
            Ok(text) => {
                let chars: Vec<char> = text.chars().collect();
                self.conversation.text_pending.clear();
                for ch in chars {
                    self.chat_character(ch);
                }
            }
            Err(error) if error.error_len().is_some() => {
                self.conversation.text_pending.clear();
            }
            Err(_) => {}
        }
    }

    pub(super) fn chat_keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        self.chat_updates();
        // Keep text and navigation in their original order, including coalesced reads.
        for byte in bytes {
            let (output, keys) = self.keyboard.feed(&[*byte], now, false, true, false);
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        if flush {
            let (output, keys) = self.keyboard.feed(&[], now, true, true, false);
            self.chat_output(&output);
            for key in keys {
                self.chat_key(key);
            }
        }
        self.chat_refresh_completions();
        self.chat_persist_draft();
        Vec::new()
    }

    fn chat_line(&self, text: &str, tone: PanelTone, width: u16) -> String {
        let plain = fit(text, width);
        self.tone(
            tone,
            &format!(
                "{plain}{}",
                " ".repeat(usize::from(width).saturating_sub(plain.width()))
            ),
        )
    }

    fn chat_list_window(&mut self, count: usize, height: u16) -> std::ops::Range<usize> {
        self.conversation.selected = self.conversation.selected.min(count.saturating_sub(1));
        let capacity = usize::from(height.saturating_sub(1) / 2).max(1);
        let start = self.conversation.selected.saturating_sub(capacity - 1);
        let count = if height > 1 {
            count.saturating_sub(start).min(capacity)
        } else {
            0
        };
        self.conversation.list_start = start;
        self.conversation.list_count = count;
        start..start + count
    }

    fn chat_agent_list_width(&self) -> u16 {
        let width = self.geometry.left;
        if width >= 76 {
            (width / 2).min(36)
        } else {
            width
        }
    }

    fn chat_agent_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        let entries = recipients(data, self.audit.observed_controllers());
        if let Some(selected) = &self.conversation.selected_agent
            && let Some(index) = entries
                .iter()
                .position(|entry| agent_key(entry) == *selected)
        {
            self.conversation.selected = index;
        }
        let window = self.chat_list_window(entries.len(), height);
        let selected = &entries[self.conversation.selected];
        self.conversation.selected_agent = Some(agent_key(selected));
        let width = self.geometry.left;
        let list_width = self.chat_agent_list_width();
        let target = self.conversation.target.as_deref().unwrap_or("next");
        let mut rows = vec![self.chat_line(
            &format!("  Agents  ·  {} connected", items(data, "agents").len()),
            PanelTone::Heading,
            list_width,
        )];
        for index in window {
            let entry = &entries[index];
            let observed = field(entry, "kind") == "observed";
            let marker = if index == self.conversation.selected {
                "›"
            } else {
                " "
            };
            let current = !observed && field(entry, "agent_id") == target;
            let name = if observed {
                format!("Call · {}", field(entry, "name"))
            } else {
                field(entry, "name").to_owned()
            };
            let title = format!(" {marker} {}{name}", if current { "● " } else { "" });
            let subtitle = if observed {
                format!(
                    "     {} · view only",
                    if entry["active"] == true {
                        "active"
                    } else {
                        "recent"
                    }
                )
            } else if field(entry, "agent_id") == "next" {
                "     Automatic · next response".into()
            } else {
                let duplicate_name = entries
                    .iter()
                    .filter(|other| {
                        field(other, "kind") != "observed"
                            && field(other, "name") == field(entry, "name")
                    })
                    .count()
                    > 1;
                format!(
                    "     {}{}",
                    field(entry, "status"),
                    if duplicate_name {
                        format!(" · {}", panel::fit(field(entry, "agent_id"), 12))
                    } else {
                        String::new()
                    }
                )
            };
            rows.push(self.chat_line(
                &title,
                if index == self.conversation.selected {
                    PanelTone::Accent
                } else {
                    PanelTone::Normal
                },
                list_width,
            ));
            rows.push(self.chat_line(&subtitle, PanelTone::Muted, list_width));
        }
        if list_width < width {
            let observed = field(selected, "kind") == "observed";
            let next = field(selected, "agent_id") == "next";
            let details = [
                (
                    if observed {
                        "Call details"
                    } else {
                        "Message target"
                    },
                    PanelTone::Heading,
                ),
                ("", PanelTone::Normal),
                (field(selected, "name"), PanelTone::Accent),
                (
                    if observed {
                        field(selected, "model")
                    } else {
                        field(selected, "role")
                    },
                    PanelTone::Muted,
                ),
                (
                    if observed {
                        field(selected, "tool")
                    } else {
                        field(selected, "status")
                    },
                    PanelTone::Muted,
                ),
                ("", PanelTone::Normal),
                (
                    if observed {
                        "Enter / →  inspect"
                    } else {
                        "Enter / →  start chatting"
                    },
                    PanelTone::Normal,
                ),
                ("Esc        back to conversation", PanelTone::Muted),
                ("", PanelTone::Normal),
                (
                    if observed {
                        "Activity record · view only"
                    } else if next {
                        "The next eligible tool response"
                    } else {
                        "Messages go only to this agent"
                    },
                    PanelTone::Muted,
                ),
                (
                    if observed || next {
                        ""
                    } else {
                        field(selected, "agent_id")
                    },
                    PanelTone::Muted,
                ),
            ];
            rows.resize_with(usize::from(height), || {
                self.chat_line("", PanelTone::Normal, list_width)
            });
            for (index, row) in rows.iter_mut().enumerate() {
                let (detail, tone) = details
                    .get(index)
                    .copied()
                    .unwrap_or(("", PanelTone::Normal));
                row.push_str(&self.tone(PanelTone::Muted, " │ "));
                row.push_str(&self.chat_line(detail, tone, width - list_width - 3));
            }
        }
        rows
    }

    fn chat_list_rows(&mut self, data: &Value, height: u16) -> Vec<String> {
        self.conversation.list_start = 0;
        self.conversation.list_count = 0;
        if self.conversation.view == View::Agents {
            return self.chat_agent_rows(data, height);
        }
        let width = self.geometry.left;
        let entries = items(data, "queue");
        self.conversation.selected = self
            .conversation
            .selected
            .min(entries.len().saturating_sub(1));
        let mut rows = vec![self.chat_line(
            "  Requests · Enter edit · → details · Delete cancel · R resend",
            PanelTone::Muted,
            width,
        )];
        if entries.is_empty() {
            rows.push(self.chat_line(
                "  No queued requests. Write a requirement below.",
                PanelTone::Normal,
                width,
            ));
            return rows;
        }
        if self.conversation.detail {
            let entry = &entries[self.conversation.selected];
            rows.push(self.chat_line(
                &format!(
                    "  {} · {} · ← back",
                    field(entry, "status"),
                    message_id(entry)
                ),
                PanelTone::Accent,
                width,
            ));
            let mut wrapped = items(entry, "images")
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    self.chat_line(
                        &format!(
                            "  [Image {}] {} · {}",
                            index + 1,
                            field(image, "name"),
                            field(image, "mime_type")
                        ),
                        PanelTone::Accent,
                        width,
                    )
                })
                .collect::<Vec<_>>();
            wrapped.extend(markdown::rows(
                references::source(field(entry, "text")),
                width,
                &self.ui,
            ));
            self.conversation.detail_scroll = self.conversation.detail_scroll.min(
                wrapped
                    .len()
                    .saturating_sub(usize::from(height).saturating_sub(2)),
            );
            rows.extend(wrapped.into_iter().skip(self.conversation.detail_scroll));
            return rows;
        }
        for index in self.chat_list_window(entries.len(), height) {
            let entry = &entries[index];
            let marker = if index == self.conversation.selected {
                "›"
            } else {
                " "
            };
            let count = items(entry, "images").len();
            rows.push(self.chat_line(
                &format!(
                    "{marker} {}{}  ·  {}",
                    field(entry, "status"),
                    if count == 0 {
                        String::new()
                    } else {
                        format!(" · {count} image(s)")
                    },
                    message_id(entry)
                ),
                if index == self.conversation.selected {
                    PanelTone::Accent
                } else {
                    match field(entry, "status") {
                        "queued" | "sending" | "editing" => PanelTone::Warning,
                        "delivered" => PanelTone::Success,
                        "uncertain" | "retarget-required" => PanelTone::Error,
                        _ => PanelTone::Muted,
                    }
                },
                width,
            ));
            rows.push(self.chat_line(
                &format!(
                        "  {}",
                        references::source(field(entry, "text"))
                            .lines()
                            .next()
                            .unwrap_or("")
                    ),
                PanelTone::Muted,
                width,
            ));
        }
        rows
    }

    fn chat_list_hit(&self, row: u16) -> Option<usize> {
        let relative = row.checked_sub(body_top(self.geometry.rows) + 1)?;
        if relative >= body_rows(self.geometry.rows).saturating_sub(1) {
            return None;
        }
        let index = usize::from(relative / 2);
        (index < self.conversation.list_count).then_some(self.conversation.list_start + index)
    }

    /// Keep project context at the lower right, independently of the sidebar.
    /// Widths are terminal cells; untrusted directory/branch controls are escaped.
    fn chat_footer(&self, hint: &str, width: u16) -> String {
        if width < 28 {
            return self.chat_line(&format!(" {hint}"), PanelTone::Muted, width);
        }
        let (project, _) = self.chat_context_labels();
        let budget = width.saturating_sub(24).min((width / 2).max(16));
        let mut context = project;
        if budget >= 24
            && let Some(git) = self.panel.git.as_ref()
        {
            let branch =
                git.branch
                    .as_deref()
                    .unwrap_or(if git.detached { "detached" } else { "" });
            if !branch.is_empty() {
                context.push_str(" · ");
                context.push_str(&panel::safe_text(branch));
                if git.stale {
                    context.push_str(" ?");
                } else if git.staged + git.unstaged + git.untracked + git.conflicts > 0 {
                    context.push_str(" *");
                }
            }
        }
        let right = panel::fit(&context, usize::from(budget));
        let left_width = usize::from(width).saturating_sub(right.width() + 2);
        let left = fit(&format!(" {hint}"), left_width as u16);
        let padding =
            " ".repeat(usize::from(width).saturating_sub(left.width() + right.width() + 1));
        let left = self.tone(PanelTone::Muted, &format!("{left}{padding}"));
        let right = if self.ui.color {
            format!(
                "\x1b[{};{}m{right}\x1b[0m",
                self.ui.background(),
                self.ui.color("blue")
            )
        } else {
            right
        };
        format!("{left}{right}{}", self.tone(PanelTone::Normal, " "))
    }

    pub(super) fn chat_frame(&mut self, now: Instant) -> Frame {
        self.chat_updates();
        self.chat_refresh_completions();
        let g = self.geometry;
        let data = self.chat_data();
        let agents = items(&data, "agents");
        let observed = self.audit.observed_controllers();
        let queue = items(&data, "queue");
        let pending = queue
            .iter()
            .filter(|item| matches!(field(item, "status"), "queued" | "sending" | "editing"))
            .count();
        let top = body_top(g.rows);
        let height = body_rows(g.rows);
        let mut body = match self.conversation.view {
            View::Activity => self.filtered_logs().unwrap_or_else(|| {
                if self.events.is_empty() {
                    (0..height)
                        .map(|row| screen_row(self.logs.screen(), row, g.left, &self.ui))
                        .collect()
                } else {
                    self.events
                        .render(g.left, height, &self.ui, self.conversation.content_focus)
                }
            }),
            View::History => self.chat_history_rows(height),
            View::Questions => self.question_rows(height),
            View::Audit => self.audit.render(g.left, height, &self.ui),
            View::Agents if self.conversation.detail => self.audit.render(g.left, height, &self.ui),
            _ => self.chat_list_rows(&data, height),
        };
        if self.help {
            body = self.chat_help_rows(height);
        }
        body.resize_with(usize::from(height), || {
            self.chat_line("", PanelTone::Normal, g.left)
        });
        body.truncate(usize::from(height));
        let empty = Composer::default();
        let input = if self.conversation.view == View::Questions {
            self.question_editor().unwrap_or(&empty)
        } else {
            &self.conversation.composer
        };
        let mut prompt = input.rows(
            g.left,
            prompt_rows(g.rows).max(1),
            &self.ui,
            self.conversation.view != View::Questions
                && !self.conversation.content_focus
                && self.conversation.image_task.is_none()
                && (self.conversation.editing.is_none() || input.reference_active()),
        );
        if self.conversation.view == View::Questions && input.is_empty() && !prompt.lines.is_empty()
        {
            prompt.lines[0] = self.chat_line(
                "❯ Type an answer · number selects an option",
                PanelTone::Muted,
                g.left,
            );
        }
        let available = usize::from(prompt_rows(g.rows)).saturating_sub(prompt.lines.len());
        if self.conversation.view != View::Questions
            && available > 0
            && !self.conversation.images.is_empty()
        {
            // Attachment descriptions use only spare input rows; the cursor's
            // viewport and the last activity line remain visible.
            let count = self.conversation.images.len();
            for (index, image) in self.conversation.images.iter().take(available).enumerate() {
                let extra = if index + 1 == available && count > available {
                    format!(" · +{} more · /images", count - available)
                } else {
                    String::new()
                };
                prompt.lines.push(self.chat_line(
                    &format!(
                        "  [Image {}] {}{extra}",
                        index + 1,
                        attachments::description(image)
                    ),
                    PanelTone::Accent,
                    g.left,
                ));
            }
        }
        let mut label = self
            .conversation
            .target
            .as_ref()
            .map(|target| {
                agents
                    .iter()
                    .find(|agent| field(agent, "agent_id") == target)
                    .map(|agent| field(agent, "name").to_owned())
                    .unwrap_or_else(|| "offline recipient".into())
            })
            .unwrap_or_else(|| "next caller".into());
        if self.conversation.view == View::Questions {
            label = self
                .conversation
                .question_id
                .as_ref()
                .and_then(|id| crate::client::interface::questions::get(id))
                .and_then(|q| q.title)
                .unwrap_or_else(|| "pending question".into());
        }
        let mut rows = Vec::new();
        for index in 0..g.rows {
            let line = if g.rows >= 3 && index == g.rows - 1 {
                let status = if self.preview {
                    "preview · sample data"
                } else if !data["connected"].as_bool().unwrap_or(false) {
                    "disconnected"
                } else if !data["supported"].as_bool().unwrap_or(false) {
                    "server update required for queue"
                } else {
                    "connected"
                };
                let hint = self.notice.clone().unwrap_or_else(|| {
                    if self.help { return "Help · ↑↓ / PgUp/PgDn scroll · Home/End · Esc back".into(); }
                    if self.conversation.view == View::History {
                        return if self.conversation.detail { "Saved conversation · PgUp/PgDn · type to continue · Esc list".into() }
                            else { "↑↓ select · Enter resume · Esc back".into() };
                    }
                    if self.conversation.view == View::Questions {
                        return if self.conversation.detail {
                            "Enter answer · Esc list · Tab browse".into()
                        } else {
                            "↑↓ select · Enter answer · Delete skip · Esc back".into()
                        };
                    }
                    if self.conversation.view == View::Agents {
                        return if self.conversation.detail {
                            "← / Esc  back to agents".into()
                        } else {
                            "↑↓ select · Enter/→ choose · Esc back · ● current recipient".into()
                        };
                    }
                    format!(
                        "{} · {status} · {} members / {} calls · {pending} queued · ← agents · /help",
                        if self.conversation.content_focus {
                            "browse"
                        } else {
                            "input"
                        },
                        agents.len(),
                        observed.len()
                    )
                });
                rows.push(self.chat_footer(&hint, g.cols));
                continue;
            } else if top > 0 && index == 0 {
                self.chat_activity_header(&data, &observed, g.left)
            } else if index >= top && index < top + height {
                body[usize::from(index - top)].clone()
            } else if index == prompt_top(g.rows) && prompt_rows(g.rows) > 0 {
                let title = format!(
                    "─ {}{} · {} ",
                    if self.conversation.view == View::Questions {
                        "Answer"
                    } else if self.conversation.editing.is_some() {
                        "Editing"
                    } else {
                        "To"
                    },
                    if self.conversation.view == View::Questions {
                        String::new()
                    } else if let Some(task) = &self.conversation.image_task {
                        if task.copying {
                            " · copying…".into()
                        } else {
                            " · loading image…".into()
                        }
                    } else if self.conversation.images.is_empty() {
                        String::new()
                    } else {
                        format!(" · {} image(s) · /images", self.conversation.images.len())
                    },
                    label
                );
                let title_width = fit(&title, g.left).width();
                let title = fit(&title, g.left);
                format!(
                    "{}{}",
                    self.tone(PanelTone::Accent, &title),
                    self.tone(
                        PanelTone::Muted,
                        &"─".repeat(usize::from(g.left).saturating_sub(title_width))
                    )
                )
            } else {
                prompt
                    .lines
                    .get(usize::from(index.saturating_sub(prompt_top(g.rows) + 1)))
                    .cloned()
                    .unwrap_or_else(|| self.chat_line("", PanelTone::Normal, g.left))
            };
            rows.push(line);
        }
        let layout = PanelLayout::configured(
            g.cols,
            g.rows,
            self.ui.sidebar && !self.ui.panels.is_empty(),
            self.ui.sidebar_width,
        );
        if let Some(rect) = layout.sidebar {
            let limit = usize::from(g.rows.saturating_sub(1));
            let mut sidebar = Vec::new();
            for kind in &self.ui.panels {
                let mut section = match kind {
                    crate::client::ui::Panel::Agents => {
                        let mut section =
                            vec![(format!(" Members · {}", agents.len()), PanelTone::Heading)];
                        section.push((" ← choose agent".into(), PanelTone::Muted));
                        for agent in agents.iter().take(4) {
                            section.push((format!(" {}", field(agent, "name")), PanelTone::Agent));
                            section.push((
                                format!("   reported {}", field(agent, "status")),
                                PanelTone::Muted,
                            ));
                        }
                        if !observed.is_empty() {
                            section.push((
                                format!(" Observed calls · {}", observed.len()),
                                PanelTone::Heading,
                            ));
                            for controller in observed.iter().take(3) {
                                section.push((
                                    format!(" ↳ {}", field(controller, "name")),
                                    PanelTone::Agent,
                                ));
                                section.push((
                                    format!(
                                        "   {} · {}",
                                        field(controller, "model"),
                                        field(controller, "status")
                                    ),
                                    PanelTone::Muted,
                                ));
                            }
                        }
                        if agents.is_empty() && observed.is_empty() {
                            section.push((" Waiting for agent activity".into(), PanelTone::Muted));
                        }
                        section
                    }
                    crate::client::ui::Panel::Queue => vec![
                        (
                            format!(" Queue · {pending} pending"),
                            if pending > 0 {
                                PanelTone::Warning
                            } else {
                                PanelTone::Muted
                            },
                        ),
                        (" /queue to review and edit".into(), PanelTone::Muted),
                    ],
                    _ => {
                        let mut ui = self.ui.clone();
                        ui.panels = vec![*kind];
                        let mut section = panel::render_configured(
                            &self.panel,
                            rect.width.saturating_sub(1),
                            g.rows,
                            now,
                            &ui,
                        );
                        if section.first().is_some_and(|row| row.text == "Ready") {
                            section.clear();
                        }
                        while section.last().is_some_and(|row| row.text.trim().is_empty()) {
                            section.pop();
                        }
                        section
                            .into_iter()
                            .map(|row| (format!(" {}", row.text), row.tone))
                            .collect()
                    }
                };
                if !section.is_empty() {
                    if !sidebar.is_empty() {
                        sidebar.push((String::new(), PanelTone::Normal));
                    }
                    sidebar.append(&mut section);
                }
            }
            for (index, line) in rows.iter_mut().enumerate().take(limit) {
                let (text, tone) = sidebar
                    .get(index)
                    .cloned()
                    .unwrap_or((String::new(), PanelTone::Normal));
                line.push_str(&self.tone(PanelTone::Muted, "│"));
                line.push_str(&self.chat_line(&text, tone, rect.width));
            }
        }
        let cursor = (
            (prompt_top(g.rows) + 1 + prompt.cursor_row).min(g.rows - 1),
            prompt.cursor_col.min(g.left - 1),
        );
        Frame {
            geometry: g,
            rows,
            cursor,
            hidden: self.help || self.conversation.content_focus || prompt_rows(g.rows) == 0,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: true,
            mouse_capture: !self.selection,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(rows: u16, cols: u16) -> State {
        super::super::preview_state(rows, cols, &Ui::default())
    }
    fn input(state: &mut State, bytes: &[u8]) {
        assert!(state.chat_keyboard(bytes, Instant::now(), false).is_empty());
    }
    fn display(state: &mut State) -> vt100::Parser {
        let frame = state.chat_frame(Instant::now());
        let mut parser = vt100::Parser::new(
            nonzero(frame.geometry.rows),
            nonzero(frame.geometry.cols),
            0,
        );
        parser.process(&frame.diff(None));
        parser
    }

    fn activity_event(run: &str, phase: &str, name: &str) -> Value {
        json!({"type":"ssmcp.code_mode","run_id":run,"phase":phase,"success":phase=="finished",
            "controller":{"name":name,"model":"model-x","source":"caller-reported"}})
    }

    #[test]
    fn activity_groups_remain_active_until_every_observed_call_finishes() {
        let mut audit = AuditView::default();
        let mut header = ActivityHeader::default();
        let data = json!({"connected":true,"room_id":"room-a",
            "agents":[{"agent_id":"member","name":"Reported worker","status":"working"}]});
        for run in ["one", "two"] {
            let event = activity_event(run, "started", "Builder");
            audit.ingest(&event);
            header.observe(&event, &data);
        }
        let summary = header.summarize(&audit.observed_controllers(), &data);
        assert_eq!(summary.active, ["Builder"]);
        assert!(summary.idle.is_empty());
        assert_eq!(summary.reported, ["Reported worker"]);
        audit.ingest(&activity_event("one", "finished", "Builder"));
        assert_eq!(
            header
                .summarize(&audit.observed_controllers(), &data)
                .active,
            ["Builder"]
        );
        audit.ingest(&activity_event("two", "finished", "Builder"));
        let summary = header.summarize(&audit.observed_controllers(), &data);
        assert!(summary.active.is_empty());
        assert_eq!(summary.idle, ["Builder"]);
    }

    #[test]
    fn disconnected_and_historical_runs_do_not_become_idle_or_active_after_reconnect() {
        let mut header = ActivityHeader::default();
        let mut audit = AuditView::default();
        let old = json!({"connected":true,"room_id":"old"});
        let event = activity_event("old-run", "started", "Old caller");
        audit.ingest(&event);
        header.observe(&event, &old);
        let offline = header.summarize(&audit.observed_controllers(), &json!({"connected":false}));
        assert!(offline.offline);
        assert!(offline.active.is_empty() && offline.idle.is_empty());
        // Same-room recovery is conservative if a disconnect was observed.
        let recovered = header.summarize(&audit.observed_controllers(), &old);
        assert!(recovered.active.is_empty());
        assert_eq!(recovered.unknown, ["Old caller"]);
        let new = json!({"connected":true,"room_id":"new"});
        let event = activity_event("new-run", "started", "New caller");
        audit.ingest(&event);
        header.observe(&event, &new);
        let summary = header.summarize(&audit.observed_controllers(), &new);
        assert_eq!(summary.active, ["New caller"]);
        assert!(summary.idle.is_empty());
        assert_eq!(summary.unknown, ["Old caller"]);
        // A duplicated historical start cannot acquire the new room.
        header.observe(&activity_event("old-run", "started", "Old caller"), &new);
        assert_eq!(
            header.summarize(&audit.observed_controllers(), &new).active,
            ["New caller"]
        );
    }

    #[test]
    fn activity_header_preserves_counts_and_clips_long_unicode_names_at_all_widths() {
        let summary = ActivitySummary {
            active: vec!["正在修复终端界面和处理大量任务".repeat(4)],
            idle: vec!["空闲审查者".repeat(4)],
            reported: vec!["未实测成员".repeat(4)],
            ..ActivitySummary::default()
        };
        for width in 0..=160 {
            let segments = activity_segments(&summary, "  ssmcp  /  activity", width);
            let text = segments
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<String>();
            assert!(text.width() <= width, "width {width}: {text:?}");
            assert!(!text.chars().any(char::is_control));
            if width >= 32 {
                assert!(text.contains("● Active 1"));
                assert!(text.contains("○ Idle 1"));
            }
        }
        let segments = activity_segments(&summary, "ssmcp", 160);
        assert!(
            segments
                .iter()
                .any(|(tone, text)| matches!(tone, PanelTone::Success)
                    && text.starts_with("● Active"))
        );
        assert!(
            segments
                .iter()
                .any(|(tone, text)| matches!(tone, PanelTone::Muted) && text.starts_with("○ Idle"))
        );
        for (rows, cols) in [(6, 20), (12, 40), (24, 100), (36, 160)] {
            let mut state = preview(rows, cols);
            input(&mut state, "输入\n第二行".as_bytes());
            let before = (state.geometry.pty_rows, prompt_top(rows));
            let parser = display(&mut state);
            let (actual_rows, actual_cols) = parser.screen().size();
            assert_eq!((actual_rows.get(), actual_cols.get()), (rows, cols));
            assert_eq!((state.geometry.pty_rows, prompt_top(rows)), before);
            assert!(!parser.screen().hide_cursor());
        }
    }

    #[test]
    fn header_shows_startup_project_and_optional_custom_title() {
        let ui = Ui {
            sidebar: false,
            ..Ui::default()
        };
        let mut state = super::super::preview_state(24, 100, &ui);
        for (cwd, title, project, heading_title) in [
            ("/workspace/alpha-project/", None, "alpha-project", None),
            (
                "/workspace/终端",
                Some("修复输入框"),
                "终端",
                Some("修复输入框"),
            ),
            (
                "/workspace/alpha-project",
                Some("alpha-project"),
                "alpha-project",
                None,
            ),
            ("", None, "ssmcp", None),
        ] {
            state.set_context(cwd, title);
            let screen = display(&mut state).screen().contents();
            let header = screen.lines().next().unwrap();
            let footer = screen.lines().last().unwrap().trim_end();
            assert!(header.starts_with("  ssmcp  /  activity"));
            assert!(header.contains("● Active 0") && header.contains("○ Idle 1"));
            assert!(
                footer.ends_with(project),
                "startup project stays at the lower right"
            );
            if let Some(title) = heading_title {
                assert!(
                    header.contains(title),
                    "custom session title remains visible"
                );
            }
            if project != "ssmcp" {
                assert!(
                    !header.contains(project),
                    "do not repeat the footer project in the header"
                );
            }
        }
        state.set_context("/workspace/alpha-project", Some("Review uploads"));
        state.conversation.view = View::Queue;
        let screen = display(&mut state).screen().contents();
        assert!(
            screen
                .lines()
                .next()
                .unwrap()
                .starts_with("  ssmcp  /  queue  /  Review uploads")
        );
        assert!(
            screen
                .lines()
                .last()
                .unwrap()
                .trim_end()
                .ends_with("alpha-project")
        );
    }

    #[test]
    fn header_is_one_safe_row_with_narrow_and_unicode_content() {
        let ui = Ui {
            sidebar: false,
            ..Ui::default()
        };
        for cols in [20, 40, 80, 100] {
            let mut state = super::super::preview_state(24, cols, &ui);
            let before = display(&mut state).screen().contents();
            state.set_context(
                "/workspace/项目\nname",
                Some("修改标题\r\u{1b}[2J\u{202e}extra-long-title"),
            );
            let after = display(&mut state).screen().contents();
            assert_eq!(
                after.lines().skip(1).take(22).collect::<Vec<_>>(),
                before.lines().skip(1).take(22).collect::<Vec<_>>(),
                "header and footer must not wrap or inject controls at {cols} columns",
            );
            let header = after.lines().next().unwrap();
            let footer = after.lines().last().unwrap();
            assert!(header.width() <= usize::from(cols));
            assert!(footer.width() <= usize::from(cols));
            if cols == 100 {
                assert!(footer.contains(r"项目\nname"));
                assert!(header.contains(r"修改标题\r\x1b"));
                assert!(
                    state
                        .context_label
                        .contains(r"修改标题\r\x1b[2J\u{202e}extra-long-title")
                );
                assert!(header.contains("● Active") && header.contains("○ Idle"));
            }
        }
    }

    #[test]
    fn left_opens_agents_only_at_draft_start_and_escape_keeps_the_draft() {
        let mut state = preview(28, 120);
        input(&mut state, "中文 draft".as_bytes());
        input(&mut state, b"\x1b[D");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.cursor(), "中文 draf".len());
        input(&mut state, b"\x01\x1b[D");
        assert_eq!(state.conversation.view, View::Agents);
        assert!(state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "中文 draft");
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "中文 draft");
        assert_eq!(state.conversation.composer.cursor(), 0);
        input(&mut state, b"\x1b[D");
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:next")
        );
    }

    #[test]
    fn choosing_an_agent_returns_to_chat_and_preserves_the_current_edit() {
        let mut state = preview(28, 120);
        state.conversation.composer.set_text("draft under review");
        state.conversation.editing = Some("existing-request".into());
        state.conversation.composer.home();
        input(&mut state, b"\x1b[D\x1b[B\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
        assert_eq!(state.conversation.composer.text(), "draft under review");
        assert_eq!(state.conversation.composer.cursor(), 0);
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("existing-request")
        );
        assert!(state.conversation.submitting.is_none());
        input(&mut state, b"\x1b[D");
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:example-builder")
        );
        input(&mut state, b"\x1b[B\x1b[C");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
        assert_eq!(state.conversation.composer.cursor(), 0);
    }

    #[test]
    fn agent_panel_closes_before_typing_or_pasting_without_switching_recipient() {
        let mut state = preview(28, 120);
        state.conversation.target = Some("example-reviewer".into());
        input(&mut state, b"\x1b[D");
        input(&mut state, "继续".as_bytes());
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "继续");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
        input(&mut state, b"\x01\x1b[D");
        input(&mut state, b"\x1b[200~pasted \x1b[201~");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "pasted 继续");
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-reviewer")
        );
    }

    #[test]
    fn agents_command_opens_the_same_picker_as_the_draft_shortcut() {
        let mut state = preview(28, 120);
        input(&mut state, b"/agents\r");
        assert_eq!(state.conversation.view, View::Agents);
        assert!(state.conversation.composer.is_empty());
        assert!(state.conversation.submitting.is_none());
        assert!(
            crate::client::composer::SLASH_COMMANDS
                .iter()
                .any(|command| command.name == "/agents")
        );
    }

    #[test]
    fn question_entry_and_shift_up_preserve_independent_answer_and_image_drafts() {
        let mut state = preview(28, 120);
        state.conversation.composer.set_text("original requirement");
        let image = test_image("question-draft.png", 64);
        state.conversation.images = vec![image.clone()];
        assert!(state.chat_command("/questions").unwrap());
        assert_eq!(state.conversation.view, View::Questions);
        assert_eq!(state.conversation.composer.text(), "original requirement");
        assert_eq!(state.conversation.images, vec![image.clone()]);
        let mut answer = Composer::default();
        answer.set_text("unfinished answer");
        state
            .conversation
            .answers
            .insert("question-one".into(), answer);
        input(&mut state, b"\x1b[1;2A");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "original requirement");
        assert_eq!(state.conversation.images, vec![image]);
        assert_eq!(
            state.conversation.answers["question-one"].text(),
            "unfinished answer"
        );
        assert!(state.notice.as_deref().unwrap().contains("Shift+↑"));
    }

    #[test]
    fn agent_panel_adapts_to_small_screens_and_shows_how_to_return() {
        for rows in [1, 3, 6, 10, 18, 36] {
            for cols in [1, 4, 20, 60, 80, 120, 160] {
                let mut state = preview(rows, cols);
                state.chat_agents();
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), usize::from(rows));
                assert!(frame.cursor.0 < rows && frame.cursor.1 < cols);
                let shot = display(&mut state);
                assert_eq!(shot.screen().size(), (nonzero(rows), nonzero(cols)));
                if rows >= 10 && cols >= 80 {
                    let visible = shot.screen().contents();
                    assert!(visible.contains("Agents"));
                    assert!(visible.contains("Esc back"));
                    assert!(!visible.contains("collab.join"));
                }
            }
        }
    }

    #[test]
    fn agent_details_are_not_mouse_targets_and_editing_keys_reveal_the_draft() {
        let mut state = preview(28, 160);
        state.conversation.composer.set_text("draft");
        state.conversation.composer.home();
        state.chat_agents();
        display(&mut state);
        state.chat_key(Key::Mouse {
            button: 2,
            x: state.chat_agent_list_width() + 5,
            y: body_top(state.geometry.rows) + 5,
            release: false,
        });
        assert_eq!(state.conversation.view, View::Agents);
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:next")
        );
        assert!(state.conversation.target.is_none());
        state.chat_key(Key::Delete);
        assert_eq!(state.conversation.view, View::Activity);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "raft");
        state.chat_agents();
        state.chat_character('\u{b}');
        assert_eq!(state.conversation.view, View::Activity);
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn mouse_selects_visible_items_after_a_list_has_scrolled() {
        let data = json!({
            "agents":(0..24).map(|index| json!({"agent_id":format!("agent_{index}"),"name":format!("Agent {index}"),"role":"worker","status":"working"})).collect::<Vec<_>>(),
            "queue":(0..24).map(|index| json!({"id":format!("request_{index}"),"text":format!("Request {index}"),"status":"queued"})).collect::<Vec<_>>()
        });
        for view in [View::Agents, View::Queue] {
            let mut state = preview(18, 120);
            state.conversation.view = view;
            state.conversation.content_focus = true;
            state.conversation.selected = 15;
            let rows = state.chat_list_rows(&data, body_rows(state.geometry.rows));
            let start = state.conversation.list_start;
            assert!(start > 0);
            assert_eq!(state.conversation.list_count, 5);
            assert!(!rows[1].contains("Agent 0  ·"));
            let first = body_top(state.geometry.rows) + 1;
            state.chat_key(Key::Mouse {
                button: 0,
                x: 1,
                y: first,
                release: false,
            });
            assert_eq!(state.conversation.selected, start);
            state.chat_key(Key::Mouse {
                button: 0,
                x: 1,
                y: first + 3,
                release: false,
            });
            assert_eq!(
                state.conversation.selected,
                start + 1,
                "subtitle selects its visible item"
            );
            state.chat_key(Key::Mouse {
                button: 2,
                x: 1,
                y: body_top(state.geometry.rows),
                release: false,
            });
            assert_eq!(
                state.conversation.selected,
                start + 1,
                "list header is not an entry"
            );
            assert!(!state.conversation.detail);
            state.chat_key(Key::Mouse {
                button: 2,
                x: 1,
                y: prompt_top(state.geometry.rows),
                release: false,
            });
            assert_eq!(
                state.conversation.selected,
                start + 1,
                "prompt border is not an entry"
            );
        }
    }

    #[test]
    fn continuation_only_removes_a_backslash_immediately_before_the_cursor() {
        let mut state = preview(36, 120);
        state.chat_restore_draft(steering::SavedDraft {
            text: "界x\\".into(),
            cursor: "界".len(),
            target: None,
            images: Vec::new(),
        });
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "界x\\");
        assert_eq!(state.conversation.composer.cursor(), "界".len());
        state.chat_restore_draft(steering::SavedDraft {
            text: "界\\tail".into(),
            cursor: "界\\".len(),
            target: None,
            images: Vec::new(),
        });
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "界\ntail");
        assert_eq!(state.conversation.composer.cursor(), "界\n".len());
        state.conversation.composer.set_text("last\\");
        input(&mut state, b"\r");
        assert_eq!(state.conversation.composer.text(), "last\n");
    }

    #[test]
    fn help_blocks_hidden_queue_actions_and_printable_input_returns_to_the_editor() {
        let mut state = preview(36, 120);
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        state.conversation.composer.set_text("draft A");
        state.chat_key(Key::Help);
        state.chat_key(Key::Delete);
        state.chat_character('\r');
        state.chat_key(Key::Open);
        state.chat_key(Key::Mouse {
            button: 2,
            x: 1,
            y: 2,
            release: false,
        });
        assert!(state.help);
        assert!(!state.conversation.detail);
        assert!(
            state.notice.is_none(),
            "hidden cancel/takeback must not be attempted"
        );
        assert_eq!(state.conversation.composer.text(), "draft A");
        assert!(state.conversation.parked.is_none());
        state.chat_character('R');
        assert!(!state.help);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "draft AR");
        assert!(
            state.conversation.parked.is_none(),
            "R must type rather than copy the hidden queue entry"
        );
    }

    #[test]
    fn a_parked_draft_survives_all_second_replacement_paths_and_escape_restores_it() {
        let mut state = preview(36, 120);
        let original = steering::SavedDraft {
            text: "继续review".into(),
            cursor: "继续re".len(),
            target: Some("example-reviewer".into()),
            images: Vec::new(),
        };
        state.chat_restore_draft(original.clone());
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        let copied = state.chat_saved_draft();
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        input(&mut state, b"\tR");
        assert_eq!(state.chat_saved_draft(), copied);
        input(&mut state, b"\t\r");
        assert_eq!(state.chat_saved_draft(), copied);
        state.conversation.composer.remember("unrelated history");
        input(&mut state, b"\x1b[A\x1b[B");
        assert_eq!(state.chat_saved_draft(), copied);
        state.chat_editable(
            "another_request".into(),
            "external request".into(),
            Some("example-builder".into()),
            Vec::new(),
        );
        assert_eq!(state.chat_saved_draft(), copied);
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        assert!(state.conversation.editing.is_none());
        state.chat_key(Key::Help);
        state.chat_key(Key::Escape);
        assert_eq!(
            state.chat_saved_draft(),
            copied,
            "closing help must not abandon the copied draft"
        );
        state.chat_key(Key::Escape);
        assert_eq!(state.chat_saved_draft(), original);
        assert!(state.conversation.parked.is_none());
    }

    #[test]
    fn same_request_retarget_keeps_revisions_in_the_editor_and_the_parked_draft() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("original draft A");
        let original = state.chat_saved_draft();
        state.chat_editable(
            "request_b".into(),
            "old B".into(),
            Some("example-builder".into()),
            Vec::new(),
        );
        state.conversation.composer.set_text("revised B");
        state.conversation.composer.left();
        let cursor = state.conversation.composer.cursor();
        state.chat_editable("request_b".into(), "old B".into(), None, Vec::new());
        assert_eq!(state.conversation.composer.text(), "revised B");
        assert_eq!(state.conversation.composer.cursor(), cursor);
        assert!(state.conversation.target.is_none());
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        assert_eq!(state.conversation.editing.as_deref(), Some("request_b"));
    }

    #[test]
    fn successful_local_enqueue_keeps_different_and_identical_following_drafts() {
        for next in ["next", "first"] {
            let mut state = preview(36, 120);
            state.conversation.composer.set_text("first");
            state.chat_enqueued("request_one".into(), "first".into());
            assert!(state.conversation.composer.is_empty());
            input(&mut state, next.as_bytes());
            assert_eq!(state.conversation.composer.text(), next);
            state.chat_accepted("request_one".into(), "first".into());
            assert_eq!(
                state.conversation.composer.text(),
                next,
                "a later ACK cannot erase a new draft"
            );
            assert!(state.conversation.submitting.is_none());
        }
    }

    #[test]
    fn local_enqueue_restores_the_parked_recipient_and_cursor_before_acknowledgement() {
        let mut state = preview(36, 120);
        let parked = steering::SavedDraft {
            text: "原稿 A".into(),
            cursor: "原".len(),
            target: Some("example-reviewer".into()),
            images: Vec::new(),
        };
        state.conversation.parked = Some(parked.clone());
        state.conversation.editing = Some("request_b".into());
        state.conversation.composer.set_text("edited B");
        state.chat_enqueued("request_b".into(), "edited B".into());
        assert_eq!(state.chat_saved_draft(), parked);
        assert!(state.conversation.editing.is_none());
        assert!(state.conversation.parked.is_none());
        input(&mut state, b"next");
        let following = state.chat_saved_draft();
        state.chat_accepted("request_b".into(), "edited B".into());
        assert_eq!(state.chat_saved_draft(), following);
    }

    #[test]
    fn editing_navigation_cannot_replace_a_taken_back_request_with_history() {
        let mut state = preview(36, 120);
        state.conversation.editing = Some("request-being-edited".into());
        state.conversation.composer.set_text("revised requirement");
        state
            .conversation
            .composer
            .remember("unrelated accepted request");
        input(&mut state, b"\x1b[A\x1b[B");
        assert_eq!(state.conversation.composer.text(), "revised requirement");
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        assert_eq!(state.conversation.composer.text(), "revised requirement");
        assert_eq!(
            state.conversation.editing.as_deref(),
            Some("request-being-edited")
        );
        assert!(!state.conversation.content_focus);
    }

    #[test]
    fn temporarily_parked_draft_restores_its_recipient_and_utf8_cursor() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("继续review");
        state.conversation.composer.left();
        state.conversation.target = Some("example-reviewer".into());
        let before = state.chat_saved_draft();
        state.conversation.view = View::Queue;
        state.conversation.content_focus = true;
        input(&mut state, b"R");
        assert!(state.conversation.parked.is_some());
        assert!(state.conversation.target.is_none());
        let parked = state.conversation.parked.take().unwrap();
        state.chat_restore_draft(parked);
        assert_eq!(state.conversation.composer.text(), before.text);
        assert_eq!(state.conversation.composer.cursor(), before.cursor);
        assert_eq!(state.conversation.target, before.target);
    }

    #[test]
    fn coalesced_utf8_navigation_and_multiline_paste_keep_input_local() {
        let mut state = preview(36, 120);
        input(&mut state, "修复界面".as_bytes());
        input(&mut state, b"\x1b[D\x1b[D");
        input(&mut state, "这个".as_bytes());
        assert_eq!(state.conversation.composer.text(), "修复这个界面");
        input(&mut state, b"\x1b[F\x1b[200~\r\n/reload\r\nq\x1b[201~");
        assert_eq!(
            state.conversation.composer.text(),
            "修复这个界面\n/reload\nq"
        );
        assert!(
            state.notice.is_none(),
            "pasted commands must not run or submit"
        );
        assert!(!display(&mut state).screen().hide_cursor());
        input(&mut state, b"\r");
        assert!(state.notice.as_ref().unwrap().contains("Preview only"));
        assert!(state.conversation.composer.text().ends_with("/reload\nq"));
    }

    #[test]
    fn keyboard_views_and_arrow_selection_preserve_unsubmitted_text() {
        let mut state = preview(36, 120);
        input(&mut state, b"\x1b[D\x1b[B\r");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("example-builder")
        );
        assert!(!state.conversation.content_focus);
        input(&mut state, b"new requirement\x0f");
        assert_eq!(state.conversation.view, View::Audit);
        assert_eq!(state.conversation.composer.text(), "new requirement");
        input(&mut state, b"\x1b[D\x1b[C\x0f");
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.conversation.composer.text(), "new requirement");
    }

    #[test]
    fn wrapped_input_arrows_move_the_cursor_before_recalling_history() {
        let mut state = preview(24, 30);
        state.conversation.composer.remember("previous request");
        let text = "界".repeat(40);
        input(&mut state, text.as_bytes());
        input(&mut state, b"\x1b[A");
        assert_eq!(state.conversation.composer.text(), text);
        assert!(state.conversation.composer.cursor() < text.len());
        assert!(state.conversation.composer.cursor() > 0);
        input(&mut state, b"\x1b[H\x1b[A");
        assert_eq!(state.conversation.composer.text(), "previous request");
    }

    #[test]
    fn slash_menu_arrows_do_not_replace_drafts_and_tab_uses_the_selected_item() {
        let mut state = preview(36, 120);
        state.conversation.composer.remember("previous request");
        input(&mut state, b"/a\x1b[A");
        assert_eq!(state.conversation.composer.text(), "/a");
        assert_eq!(
            state
                .conversation
                .composer
                .selected_suggestion()
                .unwrap()
                .name,
            "/attach"
        );
        assert!(state.conversation.editing.is_none());
        assert!(state.conversation.target.is_none());
        assert!(
            display(&mut state)
                .screen()
                .contents()
                .contains("› /attach")
        );
        input(&mut state, b"\t");
        assert_eq!(state.conversation.composer.text(), "/attach ");
        assert!(!state.conversation.content_focus);
        assert!(!state.chat_menu_visible());

        state.conversation.composer.set_text("/a");
        input(&mut state, b"\x1b[B\r");
        assert_eq!(state.conversation.view, View::Audit);
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn escape_closes_command_menu_without_restoring_or_overwriting_a_draft() {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("/re");
        state.conversation.parked = Some(steering::SavedDraft {
            text: "parked request".into(),
            cursor: 6,
            target: None,
            images: Vec::new(),
        });
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.composer.text(), "/re");
        assert!(state.conversation.parked.is_some());
        assert!(!state.chat_menu_visible());
        state.chat_key(Key::Escape);
        assert_eq!(state.conversation.composer.text(), "parked request");

        // Re-editing a user requirement beginning with '/' is plain text,
        // including Tab: local command selection must not rewrite its body.
        state.conversation.composer.set_text("/re");
        state.conversation.editing = Some("request-being-edited".into());
        assert!(!state.chat_menu_visible());
        input(&mut state, b"\t");
        assert_eq!(state.conversation.composer.text(), "/re");
    }

    #[test]
    fn restart_alias_uses_the_same_validation_and_preview_guard_as_reload() {
        let mut state = preview(36, 120);
        assert_eq!(
            state.chat_command("/restart"),
            state.chat_command("/reload")
        );
        assert!(
            state
                .chat_command("/restart extra")
                .unwrap_err()
                .contains("without arguments")
        );
        input(&mut state, b"/re\x1b[B\r");
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .contains("no connected session")
        );
        assert_eq!(state.conversation.composer.text(), "/re");
    }

    #[test]
    fn mouse_wheel_scrolls_without_focus_switch_or_draft_loss() {
        let mut state = preview(24, 100);
        for index in 0..80 {
            state.log(format!("event {index}\n").as_bytes());
        }
        input(&mut state, b"unfinished");
        assert_eq!(state.logs.screen().scrollback(), 0);
        input(&mut state, b"\x1b[<64;10;5M");
        assert!(state.logs.screen().scrollback() > 0);
        assert!(!state.conversation.content_focus);
        assert_eq!(state.conversation.composer.text(), "unfinished");
        input(&mut state, b"\x1b[1;5F");
        assert_eq!(state.logs.screen().scrollback(), 0);
    }

    #[test]
    fn every_terminal_size_keeps_frames_and_cursor_inside_the_screen() {
        for rows in [1, 2, 3, 5, 6, 9, 10, 18, 36] {
            for cols in [1, 2, 4, 20, 95, 96, 120] {
                let mut state = preview(rows, cols);
                input(&mut state, "界面\nLong input line".as_bytes());
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), usize::from(rows));
                assert!(frame.cursor.0 < rows && frame.cursor.1 < cols);
                let screen = display(&mut state);
                assert!(screen.screen().cursor_position().0 < rows);
                assert_eq!(screen.screen().size(), (nonzero(rows), nonzero(cols)));
            }
        }
    }

    #[test]
    fn sidebar_sections_obey_configuration_and_log_filters_keep_results() {
        let mut state = preview(36, 120);
        state.ui.panels = vec![crate::client::ui::Panel::Queue];
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("Queue · 1 pending"));
        assert!(!screen.contains("Members · 2"));
        // The READY fixture also mentions `/filter cargo`; use the full command
        // so this assertion tests block association rather than that second match.
        input(&mut state, b"/filter cargo test --lib\r");
        let screen = display(&mut state).screen().contents();
        assert!(screen.contains("cargo test --lib"));
        assert!(screen.contains("exit=0"));
        assert!(screen.contains("custom_colors ... ok"));
        assert!(!screen.contains("A quieter workspace"));
        assert!(
            !screen.contains("[RUN]"),
            "conversation presentation uses quiet headings"
        );
    }
    #[test]
    fn observed_agent_calls_open_audit_without_changing_the_message_recipient() {
        let mut state = preview(28, 120);
        state.audit = Default::default();
        for id in ["alpha", "beta"] {
            state.audit.ingest(&json!({
                "type":"ssmcp.code_mode","run_id":id,"phase":"started",
                "code":format!("return '{id}';"),
                "controller":{"name":"ChatGPT","model":"Declared model","source":"caller-reported"}
            }));
        }
        state.conversation.target = Some("chosen-recipient".into());
        state.conversation.composer.insert("draft stays here");
        state.chat_change_view(View::Agents);
        let data = state.chat_data();
        let first_observed = recipients(&data, state.audit.observed_controllers())
            .iter()
            .position(|entry| field(entry, "kind") == "observed")
            .unwrap();
        state.chat_move(first_observed as isize);
        let selected = field(&state.chat_selected().unwrap(), "run_id").to_owned();
        state.chat_open(true);
        assert!(state.conversation.detail);
        assert_eq!(state.audit.snapshot()["selected"]["run_id"], selected);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("chosen-recipient")
        );
        assert_eq!(state.conversation.composer.text(), "draft stays here");
        assert!(
            display(&mut state)
                .screen()
                .contents()
                .contains("Declared model")
        );
        state.chat_key(Key::Back);
        assert_eq!(state.conversation.view, View::Agents);
        assert!(!state.conversation.detail);
        state.chat_open(false);
        state.chat_key(Key::Mouse {
            button: 0,
            x: 1,
            y: 2,
            release: false,
        });
        assert!(!state.conversation.detail);
        assert_eq!(
            state.conversation.target.as_deref(),
            Some("chosen-recipient")
        );

        state.audit.ingest(&json!({"type":"ssmcp.code_mode","run_id":"new-call","phase":"started","code":"return 3;"}));
        state.chat_list_rows(&data, body_rows(state.geometry.rows));
        assert_eq!(field(&state.chat_selected().unwrap(), "run_id"), selected);
        state.chat_move(1);
        assert_ne!(field(&state.chat_selected().unwrap(), "run_id"), selected);
    }

    #[test]
    fn an_escape_waiting_in_the_second_decoder_does_not_swallow_shift_up() {
        use crate::client::input::{InputEvent, SessionKeys};

        fn feed(
            state: &mut State,
            session: &mut SessionKeys,
            text: &str,
            now: Instant,
            flush: bool,
        ) {
            for event in session.feed(text, now, flush) {
                match event {
                    InputEvent::Text(text) => {
                        assert!(state.chat_keyboard(text.as_bytes(), now, false).is_empty());
                    }
                    InputEvent::NextTerminal => panic!("Shift+Up is not terminal switching"),
                }
            }
            if flush {
                assert!(state.chat_keyboard(&[], now, true).is_empty());
            }
        }

        for split in [false, true] {
            let mut state = preview(24, 80);
            state.chat_restore_draft(steering::SavedDraft {
                text: "未提交草稿界".into(),
                cursor: "未提交草稿".len(),
                images: vec![test_image("draft.png", 20)],
                target: None,
            });
            let before = state.chat_saved_draft();
            let mut session = SessionKeys::default();
            let now = Instant::now();
            feed(&mut state, &mut session, "\u{1b}", now, false);
            // The outer decoder releases Escape after 80 ms. The display
            // decoder has its own timer and still holds it at t=120 ms.
            feed(
                &mut state,
                &mut session,
                "",
                now + Duration::from_millis(100),
                true,
            );
            assert_eq!(state.keyboard.pending, b"\x1b");
            if split {
                feed(
                    &mut state,
                    &mut session,
                    "\u{1b}[1;",
                    now + Duration::from_millis(120),
                    false,
                );
                feed(
                    &mut state,
                    &mut session,
                    "2A",
                    now + Duration::from_millis(125),
                    false,
                );
            } else {
                feed(
                    &mut state,
                    &mut session,
                    "\u{1b}[1;2A",
                    now + Duration::from_millis(120),
                    false,
                );
            }
            assert_eq!(
                state.chat_saved_draft(),
                before,
                "no CSI bytes may enter the draft"
            );
            assert!(state.keyboard.pending.is_empty());
            assert!(state.notice.as_ref().unwrap().contains("Shift+↑"));
        }
    }

    #[test]
    fn shift_up_decodes_across_reads_without_recalling_plain_arrow_history() {
        let mut state = preview(24, 40);
        state.conversation.composer.remember("old history");
        state.conversation.composer.set_text("正在编辑的草稿");
        let before = state.chat_saved_draft();
        input(&mut state, b"\x1b[1;");
        input(&mut state, b"2A");
        assert_eq!(state.chat_saved_draft(), before);
        assert!(state.notice.as_ref().unwrap().contains("Shift+↑"));
        assert!(state.conversation.editing.is_none());
        input(&mut state, b"\x1b[H\x1b[A");
        assert_eq!(state.conversation.composer.text(), "old history");
    }

    #[test]
    fn shift_up_chooses_latest_pending_and_never_replays_delivered_or_changing_requests() {
        let data = json!({"queue":[
            {"id":"first","text":"first","status":"queued"},
            {"id":"second","text":"second","status":"queued"},
            {"id":"receipt","text":"delivered","status":"delivered"}]});
        let mut state = preview(24, 100);
        state.conversation.composer.set_text("new draft");
        let before = state.chat_saved_draft();
        assert_eq!(
            state.chat_prepare_takeback(&data).as_deref(),
            Some("second")
        );
        assert_eq!(
            state.chat_saved_draft(),
            before,
            "wait for CAS confirmation before replacing input"
        );
        assert!(state.conversation.editing.is_none());
        for status in ["sending", "taking-back", "uncertain", "retarget-required"] {
            let data =
                json!({"queue":[{"id":"first","status":"queued"},{"id":"latest","status":status}]});
            assert!(state.chat_prepare_takeback(&data).is_none());
            assert_eq!(state.chat_saved_draft(), before);
        }
        let delivered = json!({"queue":[{"id":"receipt","status":"delivered"}]});
        assert!(state.chat_prepare_takeback(&delivered).is_none());
        assert!(
            state
                .notice
                .as_ref()
                .unwrap()
                .contains("cannot be recalled")
        );
        assert_eq!(state.chat_saved_draft(), before);
        assert!(state.conversation.parked.is_none());
    }

    #[test]
    fn confirmed_shift_up_preserves_both_image_drafts_and_escape_only_leaves_unchanged_edits() {
        let mut state = preview(24, 100);
        let original = steering::SavedDraft {
            text: "当前需求".into(),
            cursor: "当前".len(),
            target: Some("reviewer".into()),
            images: vec![test_image("draft.png", 20)],
        };
        state.chat_restore_draft(original.clone());
        let taken = test_image("queued.png", 40);
        let data = json!({"queue":[{"id":"latest","status":"queued","text":"待发送",
            "target":"next","images":attachments::metadata(std::slice::from_ref(&taken))}]});
        assert_eq!(
            state.chat_prepare_takeback(&data).as_deref(),
            Some("latest")
        );
        assert_eq!(state.chat_saved_draft(), original);
        // This is the same Editable event emitted only after successful CAS.
        state.chat_editable("latest".into(), "待发送".into(), None, vec![taken.clone()]);
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        assert_eq!(
            state.conversation.images.as_slice(),
            std::slice::from_ref(&taken)
        );
        let held = json!({"queue":[{"id":"latest","status":"editing","text":"待发送",
            "target":"next","images":attachments::metadata(std::slice::from_ref(&taken))}]});
        state.conversation.composer.set_text("修改后的需求");
        assert!(state.chat_leave_unmodified_edit(&held));
        assert_eq!(state.conversation.composer.text(), "修改后的需求");
        assert_eq!(state.conversation.parked.as_ref(), Some(&original));
        state.conversation.composer.set_text("待发送");
        state.conversation.images.clear();
        assert!(state.chat_leave_unmodified_edit(&held));
        assert!(
            state.conversation.editing.is_some(),
            "removing an image is an edit"
        );
        state.conversation.images = vec![taken];
        assert!(state.chat_leave_unmodified_edit(&held));
        assert_eq!(state.chat_saved_draft(), original);
        assert!(state.conversation.editing.is_none());
        assert!(state.conversation.parked.is_none());
        assert!(state.notice.as_ref().unwrap().contains("stays held"));
    }

    fn test_image(name: &str, color: u8) -> UserImage {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            1,
            1,
            image::Rgba([color, 0, 0, 255]),
        ))
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
        UserImage::from_bytes(name, "image/png", &bytes.into_inner()).unwrap()
    }

    #[test]
    fn image_drafts_park_restore_and_render_without_exposing_base64() {
        let mut state = preview(36, 120);
        let original = test_image("original.png [1×1]", 20);
        state.chat_replace_images(vec![original.clone()]).unwrap();
        let saved = state.chat_saved_draft();
        state.chat_editable(
            "image_b".into(),
            "edit B".into(),
            None,
            vec![test_image("queue.png [1×1]", 40)],
        );
        assert_eq!(state.conversation.parked.as_ref(), Some(&saved));
        assert!(state.conversation.images[0].name.starts_with("queue"));
        state.chat_enqueued("image_b".into(), "edited B".into());
        assert_eq!(state.chat_saved_draft(), saved);
        assert_eq!(state.conversation.images, vec![original]);
        assert!(
            display(&mut state)
                .screen()
                .contents()
                .contains("[Image 1]")
        );
        let snapshot = state.conversation.snapshot();
        assert!(snapshot["draft"]["images"][0].get("data").is_none());
        assert_eq!(snapshot["draft"]["images"][0]["mime_type"], "image/png");
    }

    #[test]
    fn attachment_only_submit_detach_and_failed_paste_preserve_the_draft() {
        let mut state = preview(36, 120);
        state
            .chat_replace_images(vec![test_image("screen.png [1×1]", 90)])
            .unwrap();
        input(&mut state, b"\r");
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("connect a session")
        );
        assert_eq!(state.conversation.images.len(), 1);
        state.conversation.composer.set_text("draft ");
        state.conversation.composer.begin_external_paste();
        state.conversation.composer.insert("later");
        state.chat_loaded_image(
            Err("Cannot decode image".into()),
            Some("/tmp/image.png ".into()),
        );
        assert_eq!(
            state.conversation.composer.text(),
            "draft /tmp/image.png later"
        );
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("no image attached")
        );
        let before = state.conversation.composer.text().to_owned();
        state.chat_command("/detach all").unwrap();
        assert!(state.conversation.images.is_empty());
        assert_eq!(state.conversation.composer.text(), before);
        state.chat_loaded_image(Ok(attachments::Loaded::Copied), None);
        assert_eq!(state.conversation.composer.text(), before);
        assert!(state.notice.as_deref().unwrap().contains("copied"));
    }
}
