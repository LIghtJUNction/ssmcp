//! Explicit native recipients and receipts; message text never enters file or shell expansion.
use super::*;
use crate::{client::messaging as transport, wire::messages::Recipient};

pub(super) struct Pending {
    request_id: String,
    recipient_id: String,
    body: String,
    stamp: InputStamp,
}

const LOOKUP_PENDING: &str = "Looking up native recipients · draft retained";
const NO_MATCH: &str = "No verified native recipient matches this name · draft retained";

pub(super) struct PendingPicker {
    query: String,
    directory: transport::DirectoryLookup,
    stamp: InputStamp,
    owner: Option<crate::client::interface::history::HistoryOwner>,
    content_focus: bool,
    pub(super) clear_command: bool,
}

enum Body<'a> {
    Literal(&'a str),
    WithOptionalMention(&'a str),
}

fn account(entry: &Value) -> Option<(String, String)> {
    let provider = field(&entry["account"], "provider");
    let subject = field(&entry["account"], "subject");
    (!provider.is_empty() && !subject.is_empty()).then(|| (provider.into(), subject.into()))
}

fn valid_entry(entry: &Value) -> bool {
    !field(entry, "id").is_empty()
        && !field(entry, "client_id").is_empty()
        && account(entry).is_some()
        && match field(entry, "kind") {
            "account_inbox" => entry["agent_id"].is_null(),
            "agent" => !field(entry, "agent_id").is_empty(),
            _ => false,
        }
}

fn leading_mention(text: &str) -> Option<(&str, usize)> {
    let rest = text.strip_prefix('@')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let token = &rest[..end];
    (!token.is_empty() && !token.starts_with('.') && !token.contains(['/', '\\', '"', '\'']))
        .then_some((token, end + 1))
}

fn names(entry: &Value, token: &str) -> bool {
    ["id", "client_id", "agent_id", "display_name"]
        .iter()
        .any(|key| !field(entry, key).is_empty() && field(entry, key) == token)
}

fn picker_entry(entries: impl IntoIterator<Item = Value>, query: &str) -> Option<Value> {
    let exact_id = crate::wire::messages::parse_recipient(query).is_ok();
    entries.into_iter().filter(valid_entry).find(|entry| {
        if exact_id {
            field(entry, "id") == query
        } else {
            query.is_empty() || names(entry, query)
        }
    })
}

fn message_body<'a>(text: &'a str, recipient: &Value) -> &'a str {
    let Some((token, end)) = leading_mention(text) else {
        return text;
    };
    if !names(recipient, token) {
        return text;
    }
    let rest = &text[end..];
    // Remove the address and one separator only; the body keeps its own indentation/newlines.
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix([' ', '\n', '\t']))
        .unwrap_or(rest)
}

fn local_command(text: &str) -> bool {
    if matches!(text.split_whitespace().next(), Some("/invite" | "/worker")) {
        return true;
    }
    matches!(
        text.split_whitespace().next(),
        Some(
            "/message"
                | "/recipients"
                | "/help"
                | "/version"
                | "/about"
                | "/plugin"
                | "/questions"
                | "/paste"
                | "/paste-retry"
                | "/group"
                | "/goal"
                | "/chsh"
                | "/openai"
                | "/openai-session"
                | "/openai-subject"
                | "/chatgpt"
                | "/plugin-docs"
                | "/sessions"
                | "/attach"
                | "/select"
                | "/copy"
                | "/command"
                | "/tasks"
                | "/logs"
                | "/raw"
                | "/clear"
                | "/theme"
                | "/sidebar"
                | "/exit"
                | "/quit"
                | "/logout"
                | "/login"
        )
    )
}

fn receipt_label(status: &str) -> &'static str {
    match status {
        "submitting" => "Submitting · awaiting service receipt",
        "queued" => "Queued by service · waiting for recipient call",
        "included" => "Included in recipient tool response",
        "acknowledged" => "Acknowledged by recipient",
        "expired" => "Message expired · no automatic reroute",
        "failed" => "Delivery failed or unconfirmed",
        _ => "Awaiting message receipt",
    }
}

fn receipt_hint(receipt: &Value) -> String {
    if field(receipt, "status") == "included" && receipt["connection_closed"] == true {
        "Included in recipient tool response · connection closed · acknowledgement unavailable"
            .into()
    } else {
        receipt_label(field(receipt, "status")).into()
    }
}

impl State {
    pub(super) fn native_snapshot(&self) -> Value {
        #[cfg(test)]
        if let Some(snapshot) = &self.conversation.native_fixture {
            return snapshot.clone();
        }
        #[cfg(test)]
        if let Some(connection) = &self.conversation.native_connection {
            return connection.snapshot();
        }
        if self.preview {
            json!({"available":false,"connected":false,"recipients":[],"outgoing":[]})
        } else {
            transport::snapshot()
        }
    }

    fn native_entries(&self) -> Vec<Value> {
        let snapshot = self.native_snapshot();
        if snapshot["available"] != true {
            return Vec::new();
        }
        items(&snapshot, "recipients")
            .into_iter()
            .filter(valid_entry)
            .collect()
    }

    pub(super) fn chat_native_entries(&self) -> Vec<Value> {
        self.native_entries()
            .into_iter()
            .map(|entry| {
                json!({
                    "kind":"native","native_kind":entry["kind"],"id":entry["id"],
                    "client_id":entry["client_id"],"agent_id":entry["agent_id"],
                    "name":entry["display_name"],"account":entry["account"],
                    "online":entry["online"],"sendable":entry["sendable"]
                })
            })
            .collect()
    }

    fn native_descriptor(&self, id: &str) -> Result<Value, String> {
        let snapshot = self.native_snapshot();
        if snapshot["available"] != true || snapshot["connected"] != true {
            return Err(
                "Native messaging is not connected · your draft and recipient are retained".into(),
            );
        }
        let entry = items(&snapshot, "recipients")
            .into_iter()
            .find(|entry| valid_entry(entry) && field(entry, "id") == id)
            .ok_or(
                "Recipient is not currently verified · /message refreshes the list; draft retained",
            )?;
        if entry["online"] != true || entry["sendable"] != true {
            return Err(
                "This native recipient is unavailable · select an available inbox or agent".into(),
            );
        }
        if self.conversation.native_recipient.as_deref() == Some(id)
            && let Some(expected) = &self.conversation.native_account
            && account(&entry).as_ref() != Some(expected)
        {
            return Err(
                "Recipient account changed · select the recipient again; draft retained".into(),
            );
        }
        Ok(entry)
    }

    pub(super) fn chat_native_refresh(&mut self) {
        self.dirty = true;
        if !self.preview
            && transport::available()
            && let Err(error) = transport::refresh()
        {
            self.notice = Some(error);
        }
    }

    fn chat_native_picker_blocked(&self) -> bool {
        self.conversation.editing.is_some()
            || self.conversation.submitting.is_some()
            || self.conversation.recalling.is_some()
            || self.conversation.parked.is_some()
    }

    pub(super) fn chat_native_picker(&mut self, query: &str) -> Result<(), String> {
        self.chat_cancel_native_lookup();
        if self.chat_native_picker_blocked() {
            return Err("Finish the current queue edit before choosing a native recipient".into());
        }
        let snapshot = self.native_snapshot();
        if snapshot["available"] != true || snapshot["connected"] != true {
            return Err(
                "Native messaging is unavailable on this connection · draft retained".into(),
            );
        }
        let query = query.trim().trim_start_matches('@');
        if let Some(entry) = picker_entry(items(&snapshot, "recipients"), query) {
            self.chat_native_refresh();
            return self.chat_open_native_picker(field(&entry, "id"));
        }
        if query.is_empty() {
            self.chat_native_refresh();
            return Err(
                "No verified native recipients are loaded · retry /message after refresh".into(),
            );
        }
        if !self.chat_native_sending_enabled() {
            return Err(NO_MATCH.into());
        }
        let stamp = self
            .chat_input_stamp()
            .ok_or("No active draft for native lookup")?;
        #[cfg(test)]
        let directory = if let Some(connection) = &mut self.conversation.native_connection {
            connection.lookup_directory()
        } else {
            transport::lookup_directory()
        };
        #[cfg(not(test))]
        let directory = transport::lookup_directory();
        self.conversation.native_lookup = Some(PendingPicker {
            query: query.into(),
            directory: directory.map_err(|error| format!("{error} · draft retained"))?,
            stamp,
            owner: (!self.preview)
                .then(crate::client::interface::history::current_owner)
                .flatten(),
            content_focus: self.conversation.content_focus,
            clear_command: false,
        });
        self.notice = Some(LOOKUP_PENDING.into());
        self.dirty = true;
        Ok(())
    }

    fn chat_open_native_picker(&mut self, id: &str) -> Result<(), String> {
        let data = self.chat_data();
        let selected = self
            .chat_recipients(&data)
            .iter()
            .find(|entry| sessions::group_id(entry) == Some(id))
            .map(agent_key)
            .ok_or("Verified native recipient left the directory · draft retained")?;
        self.chat_change_view(View::Agents);
        // Restoring the Agents column also restores its local queue selection.
        // Apply the explicit native-picker intent after that view transition.
        self.conversation.native_picker = true;
        self.conversation.content_focus = true;
        self.conversation.detail = false;
        self.conversation.selected_agent = Some(selected);
        self.notice = Some("Choose an inbox or verified agent · only message text is sent".into());
        self.dirty = true;
        Ok(())
    }

    pub(in crate::client::terminal) fn chat_cancel_native_lookup(&mut self) {
        if self.conversation.native_lookup.is_some() {
            self.conversation.native_lookup = None;
            if self.notice.as_deref() == Some(LOOKUP_PENDING) {
                self.notice = None;
                self.dirty = true;
            }
        }
    }

    pub(super) fn chat_poll_native_lookup(&mut self) {
        let Some(pending) = self.conversation.native_lookup.as_ref() else {
            return;
        };
        if self.ui.layout != Layout::Chat
            || self.help
            || self.selection
            || self.conversation.content_focus != pending.content_focus
            || self.chat_native_picker_blocked()
            || self.chat_input_stamp().as_ref() != Some(&pending.stamp)
            || (!self.preview
                && pending.owner != crate::client::interface::history::current_owner())
        {
            self.chat_cancel_native_lookup();
            return;
        }
        let mut pending = self
            .conversation
            .native_lookup
            .take()
            .expect("active native lookup");
        #[cfg(test)]
        let result = if let Some(connection) = &self.conversation.native_connection {
            connection.directory_result(&mut pending.directory)
        } else {
            transport::directory_result(&mut pending.directory)
        };
        #[cfg(not(test))]
        let result = transport::directory_result(&mut pending.directory);
        let Some(result) = result else {
            self.conversation.native_lookup = Some(pending);
            return;
        };
        let lookup_failure = |error: String| {
            format!(
                "Native recipient lookup failed · {} · draft retained",
                panel::safe_text(&error)
            )
        };
        let resolved = result.map_err(lookup_failure).and_then(|recipients| {
            if let Some(entry) =
                picker_entry(recipients.iter().map(|entry| json!(entry)), &pending.query)
            {
                self.chat_open_native_picker(field(&entry, "id"))?;
                return Ok(None);
            }
            #[cfg(test)]
            let follow_up = if let Some(connection) = &mut self.conversation.native_connection {
                connection.confirm_directory_miss(&pending.directory)
            } else {
                transport::confirm_directory_miss(&pending.directory)
            };
            #[cfg(not(test))]
            let follow_up = transport::confirm_directory_miss(&pending.directory);
            match follow_up.map_err(lookup_failure)? {
                Some(directory) => Ok(Some(directory)),
                None => Err(NO_MATCH.into()),
            }
        });
        match resolved {
            Ok(Some(directory)) => {
                pending.directory = directory;
                self.conversation.native_lookup = Some(pending);
                self.notice = Some(LOOKUP_PENDING.into());
            }
            Ok(None) if pending.clear_command => {
                self.conversation.composer.take_text();
            }
            Ok(None) => {}
            Err(error) => self.notice = Some(error),
        }
        self.dirty = true;
    }

    pub(super) fn chat_native_has_mention(&self) -> bool {
        leading_mention(self.conversation.composer.text()).is_some_and(|(token, _)| {
            self.native_entries()
                .iter()
                .any(|entry| names(entry, token))
        })
    }

    pub(super) fn chat_native_pick_mention(&mut self, text: &str) -> bool {
        let Some((token, _)) = leading_mention(text) else {
            return false;
        };
        let matches: Vec<_> = self
            .native_entries()
            .into_iter()
            .filter(|entry| names(entry, token))
            .collect();
        if matches.is_empty()
            || self
                .conversation
                .native_recipient
                .as_ref()
                .is_some_and(|selected| matches.iter().any(|entry| field(entry, "id") == selected))
        {
            return false;
        }
        self.notice = self.chat_native_picker(token).err().or_else(|| {
            Some(
                "Select the intended native recipient · use @./path or @\"path\" for a local file"
                    .into(),
            )
        });
        true
    }

    pub(super) fn chat_native_select(&mut self, entry: &Value) {
        if self.conversation.editing.is_some()
            || self.conversation.parked.is_some()
            || self.conversation.submitting.is_some()
            || self.conversation.recalling.is_some()
        {
            self.chat_block_replacement();
            return;
        }
        let id = field(entry, "id");
        // Selection itself is allowed to rebind a stale display to its current authenticated account.
        let old = self.conversation.native_account.take();
        let checked = self.native_descriptor(id);
        self.conversation.native_account = old;
        match checked {
            Ok(recipient) => {
                self.conversation.native_recipient = Some(id.into());
                self.conversation.native_account = account(&recipient);
                self.chat_recipient_changed(false);
                self.chat_change_view(View::Activity);
                self.conversation.content_focus = false;
                self.notice = Some(format!(
                    "To {} · plain text only · Enter queues a message",
                    panel::safe_text(field(&recipient, "display_name"))
                ));
                self.chat_persist_draft();
            }
            Err(error) => self.notice = Some(error),
        }
    }

    pub(super) fn chat_native_label(&self) -> Option<String> {
        let id = self.conversation.native_recipient.as_deref()?;
        Some(
            self.native_entries()
                .iter()
                .find(|entry| field(entry, "id") == id)
                .map(|entry| {
                    format!(
                        "message · {}",
                        panel::safe_text(field(entry, "display_name"))
                    )
                })
                .unwrap_or_else(|| "native recipient · verification pending".into()),
        )
    }

    pub(super) fn chat_native_details(&self, entry: &Value) -> Vec<(String, PanelTone)> {
        let inbox = field(entry, "native_kind") == "account_inbox";
        let id = field(entry, "id");
        let mut rows = vec![
            (
                (if inbox {
                    "Account inbox"
                } else {
                    "Verified agent"
                })
                .into(),
                PanelTone::Heading,
            ),
            ("".into(), PanelTone::Normal),
            (panel::safe_text(field(entry, "name")), PanelTone::Accent),
            (
                format!(
                    "{}:{}",
                    field(&entry["account"], "provider"),
                    field(&entry["account"], "subject")
                ),
                PanelTone::Muted,
            ),
            (
                (if entry["online"] == true && entry["sendable"] == true {
                    "Available"
                } else {
                    "Unavailable"
                })
                .into(),
                PanelTone::Normal,
            ),
            ("".into(), PanelTone::Normal),
            ("Enter / → choose recipient".into(), PanelTone::Normal),
            ("Your draft stays in place".into(), PanelTone::Muted),
            ("".into(), PanelTone::Normal),
            (
                (if inbox {
                    "This account's next call receives it"
                } else {
                    "Only this verified agent receives it"
                })
                .into(),
                PanelTone::Normal,
            ),
            ("Message text only".into(), PanelTone::Muted),
            (
                "Paths are not opened or transferred".into(),
                PanelTone::Muted,
            ),
        ];
        if self.conversation.native_recipient.as_deref() == Some(id) {
            rows.push(("● Current native recipient".into(), PanelTone::Accent));
        }
        if let Some(receipt) = &self.conversation.native_last
            && field(receipt, "recipient_id") == id
        {
            rows.push(("".into(), PanelTone::Normal));
            rows.push((receipt_hint(receipt), PanelTone::Muted));
            if !field(receipt, "error").is_empty() {
                rows.push((panel::safe_text(field(receipt, "error")), PanelTone::Muted));
            }
        }
        rows
    }

    pub(super) fn chat_native_is_command(&self, text: &str) -> bool {
        self.conversation.native_recipient.is_none() || local_command(text)
    }

    pub(super) fn chat_native_submit(&mut self, text: &str) {
        self.dirty = true;
        if let Err(error) = self.chat_send_native(Body::WithOptionalMention(text)) {
            self.notice = Some(error);
        }
    }

    pub(super) fn chat_native_force_send(&mut self, text: &str) -> Result<(), String> {
        self.chat_send_native(Body::Literal(text))
    }

    fn chat_send_native(&mut self, text: Body<'_>) -> Result<(), String> {
        if self.conversation.native_pending.is_some() {
            return Err("Waiting for the native message receipt · draft retained".into());
        }
        if self.chat_group_pending() || self.conversation.force_task.is_some() {
            return Err("Waiting for the current message receipt · draft retained".into());
        }
        let id = self
            .conversation
            .native_recipient
            .clone()
            .ok_or("Select an exact native recipient first")?;
        let recipient = self.native_descriptor(&id)?;
        let body = match text {
            Body::Literal(body) => {
                if self.chat_native_account().is_none() {
                    return Err(
                        "Select this native recipient again to verify its account · draft retained"
                            .into(),
                    );
                }
                body
            }
            Body::WithOptionalMention(text) => message_body(text, &recipient),
        };
        if body.trim().is_empty() {
            return Err("Write a message after choosing the recipient".into());
        }
        if !self.chat_native_sending_enabled() {
            return Err("Preview does not send native messages · draft retained".into());
        }
        let expected = account(&recipient).ok_or("Native recipient account is unavailable")?;
        self.conversation.native_account = Some(expected.clone());
        let request_id = if let Some(retry) = &self.conversation.native_retry
            && retry.recipient_id == id
            && retry.body == body
            && retry.stamp.native_account.as_ref() == Some(&expected)
        {
            retry.request_id.clone()
        } else {
            let mut bytes = [0u8; 16];
            crate::platform::random_bytes(&mut bytes)
                .map_err(|error| format!("Could not create a message request ID: {error}"))?;
            format!("ui-{}", crate::hex::encode(bytes))
        };
        let pending = Pending {
            request_id: request_id.clone(),
            recipient_id: id.clone(),
            body: body.into(),
            stamp: self.chat_composer_stamp(),
        };
        match self.chat_transport_send(&id, &expected, body, &request_id) {
            Ok(()) => {
                self.conversation.native_pending = Some(pending);
                self.conversation.native_retry = None;
                self.notice =
                    Some("Submitting native message · draft retained until service receipt".into());
            }
            Err(error) => {
                self.conversation.native_retry = Some(pending);
                return Err(error);
            }
        }
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_native_sending_enabled(&self) -> bool {
        #[cfg(test)]
        if self.conversation.native_connection.is_some() {
            return true;
        }
        !self.preview
    }

    fn chat_transport_send(
        &mut self,
        id: &str,
        account: &(String, String),
        body: &str,
        request_id: &str,
    ) -> Result<(), String> {
        #[cfg(test)]
        if let Some(connection) = &mut self.conversation.native_connection {
            return connection.send(id, account, body, request_id);
        }
        transport::send(id, account, body, request_id)
    }

    pub(super) fn chat_transport_send_group(
        &mut self,
        recipients: &[Recipient],
        body: &str,
        request_id: &str,
    ) -> Result<(), String> {
        #[cfg(test)]
        if let Some(connection) = &mut self.conversation.native_connection {
            return connection.send_group(recipients, body, request_id);
        }
        transport::send_group(recipients, body, request_id)
    }

    pub(super) fn chat_transport_retry_group(&mut self, request_id: &str) -> Result<(), String> {
        #[cfg(test)]
        if let Some(connection) = &mut self.conversation.native_connection {
            return connection.retry_group(request_id);
        }
        transport::retry_group(request_id)
    }

    pub(super) fn chat_poll_native(&mut self) {
        let snapshot = self.native_snapshot();
        self.chat_poll_groups(&snapshot);
        if snapshot["connected"] != true {
            self.conversation.native_refreshed = false;
        } else if !self.preview && !self.conversation.native_refreshed {
            self.conversation.native_refreshed = true;
            self.chat_native_refresh();
        }
        let outgoing = items(&snapshot, "outgoing");
        if let Some(pending) = &self.conversation.native_pending
            && let Some(receipt) = outgoing.iter().find(|entry| {
                field(entry, "request_id") == pending.request_id
                    && field(entry, "recipient_id") == pending.recipient_id
                    && field(entry, "text") == pending.body
            })
        {
            let status = field(receipt, "status");
            if matches!(
                status,
                "queued" | "included" | "acknowledged" | "expired" | "failed"
            ) {
                let pending = self.conversation.native_pending.take().unwrap();
                self.conversation.native_last = Some(receipt.clone());
                self.conversation.native_receipt_background = Some(self.chat_background_ids());
                if matches!(status, "failed" | "expired") {
                    self.conversation.native_retry = if status == "failed" {
                        Some(pending)
                    } else {
                        // Expiry is a terminal broker receipt. Keep the draft,
                        // but do not retry that completed request ID.
                        None
                    };
                    let state = if status == "expired" {
                        "expired"
                    } else {
                        "unconfirmed"
                    };
                    self.notice = Some(format!(
                        "Message {state} · {} · draft retained",
                        panel::safe_text(field(receipt, "error"))
                    ));
                } else {
                    let same = self.chat_composer_stamp() == pending.stamp;
                    if same {
                        self.conversation.composer.take_text();
                    }
                    self.conversation.composer.remember(&pending.stamp.text);
                    let mut record = format!("\n[MESSAGE] #{}\n", pending.request_id);
                    for row in markdown::rows(&pending.body, self.geometry.left, &self.ui) {
                        let _ = writeln!(record, "{row}");
                    }
                    let _ = writeln!(record, "  {}\n", receipt_hint(receipt));
                    self.log_message("user", &pending.body, record.as_bytes());
                    self.notice = if same {
                        None
                    } else {
                        Some("Message queued · your newer draft is preserved".into())
                    };
                    self.chat_persist_draft();
                }
                self.dirty = true;
            }
        }
        if let Some(previous) = &self.conversation.native_last
            && let Some(receipt) = outgoing.iter().find(|entry| {
                field(entry, "request_id") == field(previous, "request_id")
                    && field(entry, "recipient_id") == field(previous, "recipient_id")
                    && field(entry, "text") == field(previous, "text")
            })
            && receipt != previous
        {
            self.conversation.native_last = Some(receipt.clone());
            self.dirty = true;
        }
    }

    pub(super) fn chat_background_ids(&self) -> std::collections::BTreeSet<String> {
        self.chat_user_wait_snapshot()
            .active
            .iter()
            .map(|entry| format!("wait:{}", entry.wait_id))
            .chain(
                self.chat_timing_snapshot()
                    .active
                    .iter()
                    .map(|entry| format!("task:{}", entry.id)),
            )
            .collect()
    }

    pub(super) fn chat_native_priority_hint(&self) -> Option<String> {
        // Pending sends are current actions. A retained receipt only outranks
        // background work that already existed when this send was confirmed.
        // A later independent wait/task gets its usual priority immediately;
        // status refreshes of this receipt do not reset that boundary.
        if self.conversation.native_pending.is_some()
            || self
                .conversation
                .native_receipt_background
                .as_ref()
                .is_some_and(|known| self.chat_background_ids().is_subset(known))
        {
            self.chat_native_receipt_hint()
        } else {
            None
        }
    }

    pub(super) fn chat_native_receipt_hint(&self) -> Option<String> {
        if !matches!(self.conversation.view, View::Activity | View::Agents) {
            return None;
        }
        if self.conversation.native_pending.is_some() {
            return Some("Submitting native message · draft retained".into());
        }
        if self.conversation.send_to_group {
            return None;
        }
        let selected = self.conversation.native_recipient.as_deref()?;
        let receipt = self.conversation.native_last.as_ref()?;
        (field(receipt, "recipient_id") == selected).then(|| receipt_hint(receipt))
    }

    pub(super) fn chat_native_hint(&self) -> Option<String> {
        if !matches!(self.conversation.view, View::Activity | View::Agents) {
            return None;
        }
        if self.conversation.native_pending.is_some() {
            return self.chat_native_receipt_hint();
        }
        if self.conversation.send_to_group {
            return Some(
                "Group destination · /send <text> · /group list reviews recipients".into(),
            );
        }
        self.conversation.native_recipient.as_ref()?;
        self.chat_native_receipt_hint().or_else(|| {
            Some("Native message · text only · Enter sends · /message chooses recipient".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::terminal::{Layout, Ui, preview_state},
        wire::messages::{Command, Reply},
    };
    use std::time::Instant;

    fn descriptor(id: &str, name: &str, subject: &str) -> Value {
        json!({"id":id,"client_id":id.strip_prefix("client:").unwrap(),"agent_id":null,"display_name":name,
            "kind":"account_inbox","account":{"provider":"github","subject":subject,"login":"owner"},
            "online":true,"sendable":true})
    }

    fn state(columns: u16) -> State {
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
        state.conversation.native_fixture = Some(json!({"available":true,"connected":true,
            "recipients":[descriptor("client:1111111111111111111111111111111111111111111111111111111111111111","project-z","123"),descriptor("client:2222222222222222222222222222222222222222222222222222222222222222","review","123")],"outgoing":[]}));
        state.conversation.composer.set_text("保留草稿");
        state
    }

    fn pending(state: &State, body: &str) -> Pending {
        Pending {
            request_id: "ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            recipient_id: "client:1111111111111111111111111111111111111111111111111111111111111111"
                .into(),
            body: body.into(),
            stamp: state.chat_composer_stamp(),
        }
    }

    #[test]
    fn native_mentions_require_selection_and_file_syntax_remains_literal() {
        let mut state = state(120);
        state
            .conversation
            .composer
            .set_text("@project-z hello\n  retained");
        assert!(state.chat_native_pick_mention("@project-z hello\n  retained"));
        assert!(state.conversation.native_recipient.is_none());
        assert_eq!(state.conversation.view, View::Agents);
        let selected = state.chat_selected().unwrap();
        state.chat_native_select(&selected);
        assert_eq!(
            state.conversation.native_recipient.as_deref(),
            Some("client:1111111111111111111111111111111111111111111111111111111111111111")
        );
        let recipient = state
            .native_descriptor(
                "client:1111111111111111111111111111111111111111111111111111111111111111",
            )
            .unwrap();
        assert_eq!(
            message_body(state.conversation.composer.text(), &recipient),
            "hello\n  retained"
        );
        assert_eq!(
            message_body("@./source.txt literal", &recipient),
            "@./source.txt literal"
        );
        assert_eq!(
            message_body("!printf never-execute", &recipient),
            "!printf never-execute"
        );
        assert!(!state.chat_native_pick_mention("@./source.txt"));
        assert!(!state.chat_native_is_command("!printf never-execute"));
        assert!(!state.chat_native_is_command("/unknown path"));
        assert!(state.chat_native_is_command("/message"));
        state
            .conversation
            .composer
            .set_text("!printf never-execute");
        state.chat_submit();
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("Preview does not send native")
        );
        assert_eq!(state.conversation.composer.text(), "!printf never-execute");
        assert!(state.actions.is_empty());
    }

    #[test]
    fn information_commands_stay_local_with_a_native_recipient_and_keep_invalid_input() {
        let mut state = state(120);
        let id = "client:1111111111111111111111111111111111111111111111111111111111111111";
        let recipient = state.native_descriptor(id).unwrap();
        state.chat_native_select(&recipient);

        state.conversation.composer.set_text("/version");
        state.chat_submit();
        let expected = format!("ssmcp {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(state.notice.as_deref(), Some(expected.as_str()));
        assert!(state.conversation.composer.is_empty());
        assert_eq!(state.conversation.native_recipient.as_deref(), Some(id));

        state.conversation.composer.set_text("/about");
        state.chat_submit();
        assert!(state.help);
        assert!(state.conversation.about);
        assert!(state.conversation.composer.is_empty());
        assert_eq!(state.conversation.native_recipient.as_deref(), Some(id));
        state.chat_key(Key::Escape);

        for command in ["/version", "/about"] {
            state
                .conversation
                .composer
                .set_text(&format!("{command} extra"));
            state.conversation.composer.left();
            let draft = state.chat_composer_stamp();
            state.chat_submit();
            let expected = format!("Use {command} without arguments");
            assert_eq!(state.notice.as_deref(), Some(expected.as_str()));
            assert_eq!(state.chat_composer_stamp(), draft);
            assert!(!state.help);
            assert!(!state.conversation.about);
        }
        assert!(state.conversation.native_pending.is_none());
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        assert!(!state.chat_native_is_command("/versioned"));
        assert!(!state.chat_native_is_command("/about-face"));
        assert!(!state.chat_native_is_command("/unknown path"));
    }

    #[test]
    fn plugin_management_stays_local_and_preview_keeps_the_draft() {
        let mut state = state(120);
        state.preview = true;
        let id = "client:1111111111111111111111111111111111111111111111111111111111111111";
        let recipient = state.native_descriptor(id).unwrap();
        state.chat_native_select(&recipient);
        state.conversation.composer.set_text("/plugin help");
        state.chat_submit();
        assert!(state.help);
        assert!(state.conversation.plugin_help);
        assert!(!state.conversation.about);
        assert!(state.conversation.composer.is_empty());
        assert_eq!(state.conversation.native_recipient.as_deref(), Some(id));
        let help = state.chat_help_rows(1000).join("\n");
        assert!(help.contains("Native plugins"));
        assert!(help.contains("/plugin load NAME PATH"));
        state.chat_key(Key::Escape);
        assert!(!state.help);
        assert!(!state.conversation.plugin_help);
        for command in [
            "/plugin",
            "/plugin list",
            "/plugin load demo ./demo.so",
            "/plugin unload demo",
        ] {
            state.conversation.composer.set_text(command);
            state.conversation.composer.left();
            let draft = state.chat_composer_stamp();
            state.chat_submit();
            assert_eq!(state.chat_composer_stamp(), draft);
            assert!(
                state
                    .notice
                    .as_deref()
                    .unwrap()
                    .contains("Preview does not manage native plugins")
            );
        }
        assert!(state.conversation.native_pending.is_none());
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        assert!(!state.chat_native_is_command("/plugins"));
        assert!(state.chat_command("/plugin help").unwrap());
        assert!(state.chat_command("/about").unwrap());
        assert!(!state.conversation.plugin_help);
        assert!(state.conversation.about);
        assert!(state.chat_command("/plugin help").unwrap());
        assert!(state.chat_command("/help").unwrap());
        assert!(!state.conversation.plugin_help);
        assert!(!state.conversation.about);
    }

    #[test]
    fn missing_native_recipients_never_fall_back_to_next_caller() {
        let mut state = state(120);
        let draft = state.conversation.composer.text().to_owned();
        assert!(state.chat_native_picker("missing-recipient").is_err());
        assert_ne!(state.conversation.view, View::Agents);
        assert!(state.conversation.native_recipient.is_none());
        state.conversation.native_fixture.as_mut().unwrap()["recipients"] = json!([]);
        assert!(state.chat_native_picker("").is_err());
        assert_ne!(state.conversation.view, View::Agents);
        assert!(state.conversation.native_recipient.is_none());
        assert_eq!(state.conversation.composer.text(), draft);
    }

    fn cold_state() -> State {
        let mut state = state(120);
        state.conversation.native_fixture = None;
        state.conversation.native_connection =
            Some(transport::test_connection::Connection::new(vec![]));
        state
    }

    fn fixture_transport(state: &mut State) -> &mut transport::test_connection::Connection {
        state.conversation.native_connection.as_mut().unwrap()
    }

    fn recipient(digit: &str) -> Recipient {
        let id = format!("client:{}", digit.repeat(64));
        serde_json::from_value(descriptor(&id, "verified inbox", "123")).unwrap()
    }

    #[test]
    fn cold_exact_lookup_highlights_merged_process_row_without_selecting_or_sending() {
        let mut state = cold_state();
        let own = recipient("1");
        state.conversation.process_id.clone_from(&own.client_id);
        state.conversation.target = Some("existing-route".into());
        let mut other = recipient("2");
        other.display_name.clone_from(&own.id);
        state
            .conversation
            .groups
            .selected
            .insert(other.id.clone(), other.clone());
        state.conversation.send_to_group = true;
        state.conversation.composer.left();
        let original = state.chat_saved_draft();

        let connection = fixture_transport(&mut state);
        connection.quiet_refresh();
        let old_list = connection.next_request().unwrap();
        assert!(matches!(old_list.command, Command::List {}));
        let sampled_before_query = Reply {
            recipients: vec![other.clone()],
            ..Default::default()
        };
        state.chat_native_picker(&own.id).unwrap();
        assert!(fixture_transport(&mut state).next_request().is_none());
        state.chat_updates();
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.chat_saved_draft(), original);
        assert_eq!(state.notice.as_deref(), Some(LOOKUP_PENDING));

        fixture_transport(&mut state).reply(old_list.id, Ok(sampled_before_query));
        state.chat_updates();
        assert_eq!(state.conversation.view, View::Activity);
        assert_eq!(state.chat_saved_draft(), original);
        assert_eq!(state.notice.as_deref(), Some(LOOKUP_PENDING));
        let connection = fixture_transport(&mut state);
        let fresh_list = connection.next_request().unwrap();
        assert!(fresh_list.id > old_list.id);
        assert!(matches!(fresh_list.command, Command::List {}));
        assert!(connection.next_request().is_none());

        connection.reply(
            fresh_list.id,
            Ok(Reply {
                recipients: vec![other.clone(), own.clone()],
                ..Default::default()
            }),
        );
        state.chat_updates();
        assert_eq!(state.conversation.view, View::Agents);
        assert!(state.conversation.native_picker);
        assert_eq!(
            state.conversation.selected_agent.as_deref(),
            Some("member:next")
        );
        assert_eq!(
            sessions::group_id(&state.chat_selected().unwrap()),
            Some(own.id.as_str())
        );
        assert_eq!(state.chat_saved_draft(), original);
        assert!(state.conversation.native_lookup.is_none());
        assert_eq!(state.conversation.groups.selected.len(), 1);
        assert_eq!(
            state.conversation.groups.selected.get(&other.id),
            Some(&other)
        );
        // Opening the Agents canvas may request a refresh; it must not send.
        while let Some(request) = fixture_transport(&mut state).next_request() {
            assert!(matches!(request.command, Command::List {}));
        }
    }

    #[test]
    fn explicit_lookup_reports_fresh_miss_or_failure_without_clearing_the_command() {
        for (reply, notice) in [
            (Ok(Reply::default()), NO_MATCH),
            (
                Err("directory denied".into()),
                "Native recipient lookup failed · directory denied · draft retained",
            ),
        ] {
            let mut state = cold_state();
            state
                .conversation
                .composer
                .set_text("/message missing-recipient");
            state.conversation.composer.left();
            let original = state.chat_saved_draft();
            state.chat_submit();
            assert_eq!(state.notice.as_deref(), Some(LOOKUP_PENDING));
            let connection = fixture_transport(&mut state);
            let list = connection.next_request().unwrap();
            connection.reply(list.id, reply);
            state.chat_updates();
            assert_eq!(state.conversation.view, View::Activity);
            assert_eq!(state.chat_saved_draft(), original);
            assert!(state.conversation.native_lookup.is_none());
            assert_eq!(state.notice.as_deref(), Some(notice));
            assert!(fixture_transport(&mut state).next_request().is_none());
        }
    }

    #[test]
    fn coalesced_miss_joins_a_query_after_list_and_finishes_on_its_empty_reply() {
        let mut state = cold_state();
        let connection = fixture_transport(&mut state);
        connection.quiet_refresh();
        let old_list = connection.next_request().unwrap();
        let command = format!("/message {}", recipient("1").id);
        state.conversation.composer.set_text(&command);
        state.conversation.composer.left();
        let original = state.chat_saved_draft();
        state.chat_submit();

        let connection = fixture_transport(&mut state);
        assert!(connection.next_request().is_none());
        connection.reply(old_list.id, Ok(Reply::default()));
        // A background refresh can start after the old reply but before the
        // terminal consumes it. It already satisfies the query's freshness.
        connection.quiet_refresh();
        let fresh_list = connection.next_request().unwrap();
        assert!(fresh_list.id > old_list.id);
        assert!(matches!(fresh_list.command, Command::List {}));
        state.chat_updates();
        assert_eq!(state.notice.as_deref(), Some(LOOKUP_PENDING));
        assert_eq!(state.chat_saved_draft(), original);
        assert_eq!(state.conversation.view, View::Activity);

        let connection = fixture_transport(&mut state);
        assert!(connection.next_request().is_none());
        connection.reply(fresh_list.id, Ok(Reply::default()));
        state.chat_updates();
        state.chat_updates();
        assert_eq!(state.notice.as_deref(), Some(NO_MATCH));
        assert!(state.conversation.native_lookup.is_none());
        assert_eq!(state.chat_saved_draft(), original);
        assert_eq!(state.conversation.view, View::Activity);
        assert!(fixture_transport(&mut state).next_request().is_none());
    }

    #[test]
    fn queued_lookup_reply_cannot_override_escape_navigation_or_a_newer_draft() {
        let actions: [fn(&mut State); 4] = [
            |state| {
                // A ready reply must lose to Escape, even before its prefix is decoded.
                state.chat_keyboard(b"\x1b", Instant::now(), false);
                state.chat_keyboard(b"", Instant::now(), true);
            },
            |state| {
                state.canvas_focus(View::Audit);
                state.canvas_focus(View::Activity);
            },
            |state| {
                // An edit can also arrive without a keyboard event.
                state.conversation.composer.set_text("newer draft");
                state.conversation.composer.left();
            },
            |state| {
                state.conversation.target = Some("next".into());
                state.chat_recipient_changed(false);
            },
        ];
        for change in actions {
            let mut state = cold_state();
            let own = recipient("1");
            let connection = fixture_transport(&mut state);
            connection.quiet_refresh();
            let old = connection.next_request().unwrap();
            state.chat_native_picker(&own.id).unwrap();
            fixture_transport(&mut state).reply(old.id, Ok(Reply::default()));
            state.chat_updates();
            let connection = fixture_transport(&mut state);
            let list = connection.next_request().unwrap();
            assert!(list.id > old.id);
            connection.reply(
                list.id,
                Ok(Reply {
                    recipients: vec![own],
                    ..Default::default()
                }),
            );

            change(&mut state);
            let expected = state.chat_saved_draft();
            state.chat_updates();
            assert_eq!(state.conversation.view, View::Activity);
            assert_eq!(state.chat_saved_draft(), expected);
            assert!(state.conversation.native_lookup.is_none());
            assert!(state.conversation.native_recipient.is_none());
            assert!(fixture_transport(&mut state).next_request().is_none());
        }
    }

    #[test]
    fn newer_query_coalesces_the_list_and_consumes_only_its_own_command() {
        let mut state = cold_state();
        let old = recipient("1");
        let new = recipient("2");
        let connection = fixture_transport(&mut state);
        connection.quiet_refresh();
        let old_list = connection.next_request().unwrap();
        state.chat_native_picker(&old.id).unwrap();
        fixture_transport(&mut state).reply(old_list.id, Ok(Reply::default()));
        state.chat_updates();

        let list = fixture_transport(&mut state).next_request().unwrap();
        assert!(list.id > old_list.id);
        let command = format!("/message {}", new.id);
        state.conversation.composer.set_text(&command);
        state.chat_submit();
        assert_eq!(state.conversation.composer.text(), command);
        let connection = fixture_transport(&mut state);
        assert!(connection.next_request().is_none());
        connection.reply(
            list.id,
            Ok(Reply {
                recipients: vec![old, new.clone()],
                ..Default::default()
            }),
        );
        state.chat_updates();
        assert_eq!(state.conversation.view, View::Agents);
        assert_eq!(
            sessions::group_id(&state.chat_selected().unwrap()),
            Some(new.id.as_str())
        );
        assert!(state.conversation.native_recipient.is_none());
        assert!(state.conversation.composer.is_empty());
    }

    #[test]
    fn native_recipient_survives_draft_restore_and_blocks_stale_accounts() {
        let mut state = state(120);
        let entry = state.chat_native_entries().remove(0);
        state.chat_native_select(&entry);
        let saved = state.chat_saved_draft();
        state.conversation.native_recipient = None;
        state.conversation.native_account = None;
        state.chat_restore_draft(saved.clone());
        assert_eq!(state.conversation.native_recipient, saved.native_recipient);
        assert_eq!(state.conversation.native_account, saved.native_account);
        assert_eq!(state.conversation.composer.text(), saved.text);
        state.conversation.native_fixture.as_mut().unwrap()["recipients"][0]["account"]["subject"] =
            json!("456");
        assert!(
            state
                .native_descriptor(
                    "client:1111111111111111111111111111111111111111111111111111111111111111"
                )
                .unwrap_err()
                .contains("account changed")
        );
        state.conversation.native_fixture.as_mut().unwrap()["recipients"] = json!([]);
        assert!(
            state
                .native_descriptor(
                    "client:1111111111111111111111111111111111111111111111111111111111111111"
                )
                .unwrap_err()
                .contains("not currently verified")
        );
        assert_eq!(state.conversation.composer.text(), saved.text);
        assert_eq!(state.conversation.native_recipient, saved.native_recipient);
    }

    #[test]
    fn matching_native_receipts_render_over_background_wait_without_another_key() {
        use crate::client::interface::user_wait::{Entry, Intent, Snapshot};

        let mut state = state(120);
        state.notice = None;
        state.conversation.content_focus = false;
        state.conversation.wait_fixture = Some(Snapshot {
            connected: true,
            active: vec![Entry {
                wait_id: "background-wait".into(),
                episode_id: "background-episode".into(),
                intent: Intent::NeedUser,
                recipient: "next".into(),
                reason: None,
                remaining_ms: 12_500,
                finish_request_id: None,
                notification_requested: false,
                notification: None,
            }],
            ..Snapshot::default()
        });
        assert!(state.chat_user_wait_hint().is_some());
        let recipient = "client:1111111111111111111111111111111111111111111111111111111111111111";
        let body = "!touch .native-must-not-execute\n@./missing-native-file\n  消息原文";
        state.conversation.native_recipient = Some(recipient.into());
        state.conversation.composer.set_text(body);
        // Selecting a recipient alone must not hide the background wait.
        assert!(state.chat_native_receipt_hint().is_none());
        state.conversation.native_pending = Some(pending(&state, body));
        let frame = state.chat_frame(Instant::now());
        assert!(
            frame
                .rows
                .last()
                .unwrap()
                .contains("Submitting native message")
        );
        let receipt = json!({
            "request_id":"ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "message_id":"native_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "recipient_id":recipient,"text":body,"status":"queued"
        });
        // Even a queued receipt must match all three binding fields before
        // clearing this draft. No input events drive these transitions.
        for (field, wrong) in [
            ("request_id", "ui-wrong"),
            ("recipient_id", "client:wrong"),
            ("text", "wrong payload"),
        ] {
            let mut unrelated = receipt.clone();
            unrelated[field] = json!(wrong);
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"] = json!([unrelated]);
            state.chat_poll_native();
            assert!(state.conversation.native_pending.is_some());
            assert_eq!(state.conversation.composer.text(), body);
        }
        for (status, label) in [
            ("queued", "Queued by service"),
            ("included", "Included in recipient tool response"),
            ("acknowledged", "Acknowledged by recipient"),
        ] {
            let mut current = receipt.clone();
            current["status"] = json!(status);
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"] = json!([current]);
            state.chat_poll_native();
            assert!(state.conversation.native_pending.is_none());
            assert_eq!(state.conversation.composer.text(), "");
            assert_eq!(
                state.conversation.native_recipient.as_deref(),
                Some(recipient)
            );
            assert!(state.chat_user_wait_hint().is_some());
            let frame = state.chat_frame(Instant::now());
            assert!(frame.rows.last().unwrap().contains(label), "{status}");
        }
        // A new independent wait must outrank the retained acknowledged receipt.
        // Elapsed-time changes and receipt refreshes must not reset this boundary.
        let mut later = state.conversation.wait_fixture.as_ref().unwrap().active[0].clone();
        later.wait_id = "later-wait".into();
        later.episode_id = "later-episode".into();
        state
            .conversation
            .wait_fixture
            .as_mut()
            .unwrap()
            .active
            .push(later);
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["updated_at_ms"] =
            json!(42);
        state.chat_poll_native();
        assert!(state.chat_native_priority_hint().is_none());
        assert!(
            state
                .chat_native_receipt_hint()
                .unwrap()
                .contains("Acknowledged")
        );
        let frame = state.chat_frame(Instant::now());
        assert!(frame.rows.last().unwrap().contains("等待你的输入"));

        state
            .conversation
            .wait_fixture
            .as_mut()
            .unwrap()
            .active
            .clear();
        state.conversation.timing_fixture = Some(crate::runtime::time::Snapshot {
            active: vec![crate::runtime::time::Entry {
                id: "later-task".into(),
                kind: "task",
                label: "Independent task".into(),
                status: crate::runtime::time::Status::Running,
                elapsed_ms: 1_000,
                remaining_ms: None,
                deadline_unix_ms: None,
                finished_age_ms: None,
                cancellable: false,
                episode: None,
                attribution: None,
                wait: None,
                durable: None,
            }],
            ..Default::default()
        });
        assert!(state.chat_native_priority_hint().is_none());
        let frame = state.chat_frame(Instant::now());
        assert!(frame.rows.last().unwrap().contains("任务"));
        assert_eq!(state.conversation.composer.text(), "");
        assert_eq!(
            state.conversation.native_recipient.as_deref(),
            Some(recipient)
        );
    }

    #[test]
    fn service_receipt_clears_only_the_exact_submitted_native_draft() {
        for edit_after_send in [false, true] {
            let mut state = state(120);
            state.conversation.native_recipient = Some(
                "client:1111111111111111111111111111111111111111111111111111111111111111".into(),
            );
            state.conversation.composer.set_text("message body");
            state.conversation.native_pending = Some(pending(&state, "message body"));
            if edit_after_send {
                state.conversation.composer.insert(" plus newer edit");
            }
            let expected = state.conversation.composer.text().to_owned();
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"] = json!([{
                "request_id":"ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","message_id":"native_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","recipient_id":"client:1111111111111111111111111111111111111111111111111111111111111111","text":"message body","status":"submitting"
            }]);
            state.chat_poll_native();
            assert!(state.conversation.native_pending.is_some());
            assert_eq!(state.conversation.composer.text(), expected);
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["status"] =
                json!("queued");
            state.chat_poll_native();
            assert!(state.conversation.native_pending.is_none());
            assert_eq!(
                state.conversation.composer.text(),
                if edit_after_send {
                    expected.as_str()
                } else {
                    ""
                }
            );
            assert!(
                state
                    .chat_native_hint()
                    .unwrap()
                    .contains("Queued by service")
            );
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["status"] =
                json!("included");
            state.chat_poll_native();
            assert!(state.chat_native_hint().unwrap().contains("Included"));
            state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["status"] =
                json!("acknowledged");
            state.chat_poll_native();
            assert!(state.chat_native_hint().unwrap().contains("Acknowledged"));
        }
    }

    #[test]
    fn a_mismatched_or_failed_native_receipt_never_clears_the_draft() {
        let mut state = state(120);
        state.conversation.native_recipient =
            Some("client:1111111111111111111111111111111111111111111111111111111111111111".into());
        state.conversation.composer.set_text("message body");
        state.conversation.native_pending = Some(pending(&state, "message body"));
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"] = json!([{
            "request_id":"ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","message_id":"native_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","recipient_id":"client:2222222222222222222222222222222222222222222222222222222222222222","text":"message body","status":"queued"
        }]);
        state.chat_poll_native();
        assert!(state.conversation.native_pending.is_some());
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["recipient_id"] =
            json!("client:1111111111111111111111111111111111111111111111111111111111111111");
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["text"] =
            json!("different body");
        state.chat_poll_native();
        assert!(state.conversation.native_pending.is_some());
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["text"] =
            json!("message body");
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["status"] =
            json!("failed");
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"][0]["error"] =
            json!("permission changed");
        state.chat_poll_native();
        assert!(state.conversation.native_pending.is_none());
        assert_eq!(
            state.conversation.native_retry.as_ref().unwrap().request_id,
            "ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert_eq!(state.conversation.composer.text(), "message body");
        assert_eq!(
            state.conversation.native_recipient.as_deref(),
            Some("client:1111111111111111111111111111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn native_picker_frame_identifies_account_inbox_without_claiming_an_agent() {
        for columns in [120, 48] {
            let mut state = state(columns);
            state.chat_native_picker("").unwrap();
            let frame = state.chat_frame(Instant::now());
            let mut parser = vt100::Parser::new(
                std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
                std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
                0,
            );
            let bytes = frame.diff(None);
            parser.process(&bytes);
            let contents = parser.screen().contents();
            assert!(contents.contains("project-z"));
            assert!(contents.contains("process"));
            if let Some(directory) = std::env::var_os("SSMCP_INTERACTION_PREVIEW_DIR") {
                let directory = std::path::PathBuf::from(directory);
                std::fs::create_dir_all(&directory).unwrap();
                let name = format!("native-picker-{columns}");
                std::fs::write(directory.join(format!("{name}.ansi")), bytes).unwrap();
                std::fs::write(directory.join(format!("{name}.json")), serde_json::to_vec(&json!({
                    "rows":frame.geometry.rows,"cols":frame.geometry.cols,"source":"production conversation Frame"
                })).unwrap()).unwrap();
            }
        }
    }

    #[test]
    fn closed_included_receipts_and_terminal_expiry_keep_delivery_facts_and_drafts() {
        let mut state = state(120);
        let recipient = state.chat_native_entries().remove(0);
        let recipient_id = field(&recipient, "id").to_owned();
        state.conversation.native_recipient = Some(recipient_id.clone());
        state.conversation.native_last = Some(json!({
            "request_id":"ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "recipient_id":recipient_id,
            "text":"message body",
            "status":"included",
            "connection_closed":true,
            "error":"消息已包含在收件方工具响应中；原连接已断开，未自动重发"
        }));
        let hint = state.chat_native_hint().unwrap();
        assert!(hint.contains("Included in recipient tool response"));
        assert!(hint.contains("connection closed"));
        assert!(hint.contains("acknowledgement unavailable"));
        assert!(!hint.contains("failed"));
        assert!(
            state
                .chat_native_details(&recipient)
                .iter()
                .any(|(text, _)| { text.contains("消息已包含在收件方工具响应中") })
        );

        state.conversation.composer.set_text("message body");
        state.conversation.native_pending = Some(pending(&state, "message body"));
        state.conversation.native_fixture.as_mut().unwrap()["outgoing"] = json!([{
            "request_id":"ui-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "message_id":"native_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "recipient_id":recipient_id,
            "text":"message body",
            "status":"expired",
            "error":"Connection ended after inclusion; acknowledgement is unavailable"
        }]);
        state.chat_poll_native();
        assert!(state.conversation.native_pending.is_none());
        assert!(state.conversation.native_retry.is_none());
        assert_eq!(state.conversation.composer.text(), "message body");
        assert!(state.notice.as_deref().unwrap().contains("Message expired"));
        assert!(
            state
                .chat_native_hint()
                .unwrap()
                .contains("Message expired")
        );
        assert_eq!(
            state.conversation.native_last.as_ref().unwrap()["status"],
            "expired"
        );
    }
}
