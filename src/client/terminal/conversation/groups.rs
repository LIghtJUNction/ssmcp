//! Local group selection is explicit. Plans retain exact identities and child receipts.
use super::*;
use crate::{client::messaging as transport, wire::messages::Recipient};
use std::collections::BTreeMap;

mod reload;
use steering::SavedRecipient;

#[derive(Default)]
pub(super) struct Selection {
    pub(super) selected: BTreeMap<String, Recipient>,
    saved: BTreeMap<String, Vec<SavedRecipient>>,
    restoring: Option<Vec<SavedRecipient>>,
    restore_error: Option<String>,
    pending: Option<Pending>,
    retry: BTreeMap<String, Pending>,
    last: Option<String>,
    observed: Option<Value>,
}

struct Pending {
    request_id: String,
    recipients: Vec<Recipient>,
    body: String,
    stamp: InputStamp,
}

pub(super) fn arguments(text: &str) -> Option<&str> {
    let rest = text.trim_start().strip_prefix("/group")?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

pub(super) fn retains_draft(text: &str) -> bool {
    arguments(text).is_some_and(|arguments| {
        matches!(arguments.split_whitespace().next(), Some("send" | "retry"))
    })
}

fn same_selection<'a>(
    left: &[Recipient],
    right: impl ExactSizeIterator<Item = &'a Recipient>,
) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            a.id == b.id
                && a.account.provider == b.account.provider
                && a.account.subject == b.account.subject
        })
}

impl State {
    fn group_recipient(snapshot: &Value, id: &str) -> Result<Recipient, String> {
        if snapshot["available"] != true || snapshot["connected"] != true {
            return Err(
                "Group messaging is unavailable on this connection; selection and draft retained"
                    .into(),
            );
        }
        let value = items(snapshot, "recipients")
            .into_iter()
            .find(|entry| field(entry, "id") == id)
            .ok_or("Choose an exact recipient from the current Sessions page")?;
        let recipient: Recipient = serde_json::from_value(value)
            .map_err(|_| "The recipient descriptor is inconsistent")?;
        recipient.validate()?;
        if !recipient.online || !recipient.sendable {
            return Err(
                "This recipient is unavailable; it was not replaced by another target".into(),
            );
        }
        Ok(recipient)
    }

    pub(super) fn chat_group_toggle(&mut self) {
        let Some(entry) = self.chat_selected() else {
            return;
        };
        let Some(id) = sessions::group_id(&entry) else {
            self.notice = Some("This row has no verified native message address".into());
            return;
        };
        if self.conversation.groups.selected.remove(id).is_none() {
            if self.conversation.groups.selected.len() >= transport::MAX_GROUP_RECIPIENTS {
                self.notice = Some("A group contains at most 32 exact recipients".into());
                return;
            }
            match Self::group_recipient(&self.native_snapshot(), id) {
                Ok(recipient) => {
                    self.conversation
                        .groups
                        .selected
                        .insert(id.into(), recipient);
                }
                Err(error) => {
                    self.notice = Some(error);
                    return;
                }
            }
        }
        self.conversation.groups.cancel_restoration();
        self.chat_recipient_changed(true);
        self.notice = Some(format!(
            "{} selected · Space toggles · g composes · /group clear resets",
            self.conversation.groups.selected.len()
        ));
        self.dirty = true;
    }

    pub(super) fn chat_group_compose(&mut self) {
        if self.conversation.groups.restoring.is_some() {
            self.notice =
                Some("Saved selection is awaiting verification · /group list reviews it".into());
            return;
        }
        if self.conversation.groups.selected.is_empty() {
            self.notice = Some("Select process inboxes or threads with Space first".into());
            return;
        }
        if self.conversation.editing.is_some()
            || self.conversation.submitting.is_some()
            || self.conversation.recalling.is_some()
            || self.conversation.parked.is_some()
            || self.conversation.native_pending.is_some()
            || self.conversation.groups.pending.is_some()
        {
            self.notice =
                Some("Finish the pending draft or receipt before composing a group message".into());
            return;
        }
        let current = self.conversation.composer.text();
        if !arguments(current).is_some_and(|args| args.starts_with("send "))
            && listener::body(current).is_none()
        {
            let text = format!("/group send {current}");
            if !self.conversation.composer.set_text(&text) {
                self.notice = Some(
                    "The draft is too long for the group command prefix; your text is unchanged"
                        .into(),
                );
                return;
            }
        }
        self.chat_recipient_changed(true);
        self.chat_change_view(View::Activity);
        self.conversation.content_focus = false;
        self.notice = Some(format!(
            "To {} selected recipients · Enter sends · /group list reviews IDs",
            self.conversation.groups.selected.len()
        ));
        self.chat_persist_draft();
    }

    pub(super) fn chat_group_report(&mut self, text: &str) {
        let mut record = String::new();
        for row in markdown::rows(text, self.geometry.left, &self.ui) {
            let _ = writeln!(record, "{row}");
        }
        self.log_message("system", text, record.as_bytes());
        self.chat_change_view(View::Activity);
        self.conversation.content_focus = false;
        self.dirty = true;
    }

    fn group_current_selection(&self) -> Result<Vec<Recipient>, String> {
        if self.conversation.groups.restoring.is_some() {
            return Err("Saved selection is awaiting current-directory verification · /group list reviews it".into());
        }
        if self.conversation.groups.selected.is_empty() {
            return Err(
                "The group is empty; open /sessions and use Space to select exact recipients"
                    .into(),
            );
        }
        let snapshot = self.native_snapshot();
        self.conversation.groups.selected.values().map(|expected| {
            let current = Self::group_recipient(&snapshot, &expected.id)?;
            if current.account.provider != expected.account.provider
                || current.account.subject != expected.account.subject
            {
                return Err("A selected recipient changed account; deselect it and select the intended account again".into());
            }
            Ok(current)
        }).collect()
    }

    fn group_status(&mut self, id: Option<&str>) -> Result<(), String> {
        let snapshot = self.native_snapshot();
        let groups = items(&snapshot, "groups");
        let wanted = id.or(self.conversation.groups.last.as_deref());
        let group = groups
            .iter()
            .find(|group| Some(field(group, "request_id")) == wanted)
            .ok_or("No retained group receipt matches that ID")?;
        let mut text = format!("Group {}\n\n", field(group, "request_id"));
        for target in items(group, "targets") {
            let _ = writeln!(
                text,
                "{} · {}\n{}\nMessage ID: {}",
                panel::safe_text(field(&target, "name")),
                field(&target, "status"),
                field(&target, "recipient_id"),
                if field(&target, "message_id").is_empty() {
                    "unconfirmed"
                } else {
                    field(&target, "message_id")
                }
            );
            if !field(&target, "error").is_empty() {
                let _ = writeln!(text, "{}", panel::safe_text(field(&target, "error")));
            }
            text.push('\n');
        }
        self.chat_group_report(&text);
        Ok(())
    }

    pub(super) fn chat_group_pending(&self) -> bool {
        self.conversation.groups.pending.is_some()
    }

    pub(super) fn group_send(&mut self, body: &str) -> Result<(), String> {
        if self.conversation.groups.pending.is_some() {
            return Err("Waiting for this group's service receipts; your draft is retained".into());
        }
        if self.conversation.native_pending.is_some() {
            return Err(
                "Wait for the current single-message receipt before sending a group".into(),
            );
        }
        if self.conversation.force_task.is_some() {
            return Err("Waiting for the listener's receipt · draft retained".into());
        }
        crate::wire::messages::validate_text(body)?;
        if !self.chat_native_sending_enabled() {
            return Err("Preview does not send group messages".into());
        }
        let selected = &self.conversation.groups.selected;
        let retry = self
            .conversation
            .groups
            .retry
            .values()
            .find(|pending| {
                self.conversation.groups.restoring.is_none()
                    && !pending.recipients.is_empty()
                    && pending.body == body
                    && same_selection(&pending.recipients, selected.values())
            })
            .map(|pending| (pending.request_id.clone(), pending.recipients.clone()));
        let (request_id, recipients) = if let Some((id, recipients)) = retry {
            // The group transport revalidates each failed child before dispatch.
            // An accepted child does not have to stay online for another child to retry.
            self.chat_transport_retry_group(&id)?;
            (id, recipients)
        } else {
            let recipients = self.group_current_selection()?;
            let mut bytes = [0u8; 16];
            crate::platform::random_bytes(&mut bytes)
                .map_err(|error| format!("Could not create a group request ID: {error}"))?;
            let id = format!(
                "ui-group-{}",
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            self.chat_transport_send_group(&recipients, body, &id)?;
            (id, recipients)
        };
        self.conversation.groups.pending = Some(Pending {
            request_id: request_id.clone(),
            recipients,
            body: body.into(),
            stamp: self.chat_composer_stamp(),
        });
        self.conversation.groups.retry.remove(&request_id);
        self.conversation.groups.last = Some(request_id);
        self.notice = Some(
            "Group accepted locally · awaiting individual service receipts · draft retained".into(),
        );
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_group_command(&mut self, arguments: &str) -> Result<(), String> {
        let (action, rest) = arguments
            .split_once(char::is_whitespace)
            .unwrap_or((arguments, ""));
        match action {
            "" | "pick" => {
                if !rest.trim().is_empty() { return Err("Use /group to choose recipients".into()); }
                self.chat_change_view(View::Agents);
                self.chat_select_target();
                self.conversation.content_focus = true;
                self.notice = Some("Space selects exact recipients · g composes · i shows full IDs".into());
            }
            "send" => self.group_send(rest)?,
            "retry" => {
                if self.conversation.groups.pending.is_some()
                    || self.conversation.native_pending.is_some()
                    || self.conversation.force_task.is_some()
                {
                    return Err("A message is still waiting for receipts".into());
                }
                if !self.chat_native_sending_enabled() { return Err("Preview does not retry group messages".into()); }
                let id = if rest.trim().is_empty() {
                    self.conversation.groups.last.clone().ok_or("No group is available to retry")?
                } else {
                    rest.trim().to_owned()
                };
                let snapshot = self.native_snapshot();
                let groups = items(&snapshot, "groups");
                let group = groups.iter().find(|group| field(group, "request_id") == id)
                    .ok_or("Group receipt is no longer retained")?;
                self.chat_transport_retry_group(&id)?;
                let recipients = self.conversation.groups.retry.remove(&id)
                    .map(|pending| pending.recipients).unwrap_or_default();
                self.conversation.groups.pending = Some(Pending {
                    request_id:id.clone(), recipients, body:field(group,"text").into(),
                    stamp:self.chat_composer_stamp(),
                });
                self.conversation.groups.last = Some(id);
                self.notice = Some("Retrying only unconfirmed failed targets with their original request IDs".into());
            }
            "status" => self.group_status((!rest.trim().is_empty()).then_some(rest.trim()))?,
            "list" => {
                if !rest.trim().is_empty() { return Err("Use /group list without arguments".into()); }
                let mut text = format!("Selected recipients: {}\n\n", self.conversation.groups.selected.len());
                for recipient in self.conversation.groups.selected.values() {
                    let _ = writeln!(text, "{}\n{}\n", panel::safe_text(&recipient.display_name), recipient.id);
                }
                if let Some(restoring) = &self.conversation.groups.restoring {
                    text.push_str("Saved selection waiting for current-directory verification:\n");
                    for recipient in restoring {
                        let _ = writeln!(text, "{}", recipient.id);
                    }
                    if let Some(error) = &self.conversation.groups.restore_error {
                        let _ = writeln!(text, "\n{}", panel::safe_text(error));
                    }
                    text.push('\n');
                }
                if !self.conversation.groups.saved.is_empty() {
                    text.push_str("Saved groups (retained across reload):\n");
                    for (name, recipients) in &self.conversation.groups.saved {
                        let _ = writeln!(text, "{name} · {} recipients", recipients.len());
                    }
                }
                self.chat_group_report(&text);
            }
            "clear" => {
                if !rest.trim().is_empty() { return Err("Use /group clear without arguments".into()); }
                self.conversation.groups.selected.clear();
                self.conversation.groups.cancel_restoration();
                self.chat_recipient_changed(self.conversation.send_to_group);
                self.notice = Some("Group selection cleared · submitted groups keep their original recipients".into());
            }
            "add" => {
                let ids: Vec<_> = rest.split_whitespace().collect();
                if ids.is_empty() { return Err("Use /group add EXACT_ID [EXACT_ID ...]".into()); }
                let mut selected = self.conversation.groups.selected.clone();
                let snapshot = self.native_snapshot();
                for id in ids {
                    let recipient = Self::group_recipient(&snapshot, id)?;
                    selected.insert(id.into(), recipient);
                }
                if selected.len() > transport::MAX_GROUP_RECIPIENTS {
                    return Err("A group contains at most 32 exact recipients".into());
                }
                self.conversation.groups.selected = selected;
                self.conversation.groups.cancel_restoration();
                self.chat_recipient_changed(true);
                self.notice = Some(format!("{} exact recipients selected", self.conversation.groups.selected.len()));
            }
            "remove" => {
                let ids: Vec<_> = rest.split_whitespace().collect();
                if ids.is_empty() { return Err("Use /group remove EXACT_ID [EXACT_ID ...]".into()); }
                for id in &ids { crate::wire::messages::parse_recipient(id)?; }
                if let Some(restoring) = &mut self.conversation.groups.restoring {
                    restoring.retain(|recipient| !ids.contains(&recipient.id.as_str()));
                    self.conversation.groups.restore_error = None;
                    let snapshot = self.native_snapshot();
                    self.chat_restore_group_selection(&snapshot);
                } else {
                    for id in ids { self.conversation.groups.selected.remove(id); }
                }
                self.chat_recipient_changed(self.conversation.send_to_group);
                self.notice = Some(format!("{} exact recipients selected", self.conversation.groups.selected.len()));
            }
            "save" | "use" => {
                let name = rest.trim();
                if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                    return Err("Choose a group name of 1–64 bytes without control characters".into());
                }
                if action == "save" {
                    let recipients = self.group_current_selection()?;
                    if !self.conversation.groups.saved.contains_key(name) && self.conversation.groups.saved.len() >= 16 {
                        return Err("At most 16 named groups are retained".into());
                    }
                    self.conversation.groups.saved.insert(name.into(), recipients.iter().map(SavedRecipient::from).collect());
                    self.notice = Some(format!("Saved group {name} · retained across reload; identities rechecked before use"));
                } else {
                    let recipients = self.conversation.groups.saved.get(name)
                        .ok_or("No saved group has that name")?.clone();
                    let snapshot = self.native_snapshot();
                    let selected = recipients.iter()
                        .map(|expected| expected.resolve(&snapshot).map(|current| (current.id.clone(), current)))
                        .collect::<Result<BTreeMap<_, _>, _>>()?;
                    self.conversation.groups.selected = selected;
                    self.conversation.groups.cancel_restoration();
                    self.chat_recipient_changed(true);
                    self.notice = Some(format!("Selected group {name} · {} exact recipients", self.conversation.groups.selected.len()));
                }
            }
            _ => return Err("Use /group [list|clear|add IDs|remove IDs|save NAME|use NAME|send TEXT|status [ID]|retry [ID]]".into()),
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_poll_groups(&mut self, snapshot: &Value) {
        self.chat_restore_group_selection(snapshot);
        let Some(pending) = &self.conversation.groups.pending else {
            return;
        };
        let Some(group) = items(snapshot, "groups")
            .into_iter()
            .find(|group| field(group, "request_id") == pending.request_id)
        else {
            return;
        };
        if self.conversation.groups.observed.as_ref() != Some(&group) {
            self.conversation.groups.observed = Some(group.clone());
            self.dirty = true;
        }
        let targets = items(&group, "targets");
        if targets.is_empty()
            || targets
                .iter()
                .any(|target| matches!(field(target, "status"), "waiting" | "submitting"))
        {
            return;
        }
        let accepted = targets
            .iter()
            .filter(|target| {
                matches!(
                    field(target, "status"),
                    "queued" | "included" | "acknowledged"
                )
            })
            .count();
        let pending = self.conversation.groups.pending.take().unwrap();
        let complete = accepted == targets.len();
        let same = self.chat_composer_stamp() == pending.stamp;
        if complete && same {
            self.conversation.composer.take_text();
        }
        let summary = format!(
            "Group {} · {accepted}/{} queued or received · {} unconfirmed/expired",
            pending.request_id,
            targets.len(),
            targets.len() - accepted
        );
        let mut record = format!("\n[GROUP] {summary}\n\n");
        for row in markdown::rows(&pending.body, self.geometry.left, &self.ui) {
            let _ = writeln!(record, "{row}");
        }
        self.log_message("user", &pending.body, record.as_bytes());
        self.notice = Some(format!(
            "{summary} · /group status shows each target{}",
            if complete && same {
                ""
            } else {
                " · draft retained"
            }
        ));
        if !complete {
            self.conversation
                .groups
                .retry
                .insert(pending.request_id.clone(), pending);
        }
        self.chat_persist_draft();
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests;
