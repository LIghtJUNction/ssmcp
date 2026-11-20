//! Explicit native recipients and receipts; message text never enters file or shell expansion.
use super::*;
use crate::client::messaging as transport;

pub(super) struct Pending {
    request_id: String,
    recipient_id: String,
    body: String,
    original: steering::SavedDraft,
    revision: u64,
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
    matches!(
        text.split_whitespace().next(),
        Some(
            "/message"
                | "/recipients"
                | "/help"
                | "/questions"
                | "/paste"
                | "/paste-retry"
                | "/group"
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

    pub(super) fn chat_native_picker(&mut self, query: &str) -> Result<(), String> {
        if self.conversation.editing.is_some()
            || self.conversation.submitting.is_some()
            || self.conversation.recalling.is_some()
            || self.conversation.parked.is_some()
        {
            return Err("Finish the current queue edit before choosing a native recipient".into());
        }
        self.chat_native_refresh();
        let snapshot = self.native_snapshot();
        if snapshot["available"] != true || snapshot["connected"] != true {
            return Err(
                "Native messaging is unavailable on this connection · draft retained".into(),
            );
        }
        let entries = self.native_entries();
        let query = query.trim().trim_start_matches('@');
        let selected = entries
            .iter()
            .find(|entry| query.is_empty() || names(entry, query))
            .ok_or(if query.is_empty() {
                "No verified native recipients are loaded · retry /message after refresh"
            } else {
                "No verified native recipient matches this name · draft retained"
            })?;
        let id = field(selected, "id").to_owned();
        let data = self.chat_data();
        let selected = self
            .chat_recipients(&data)
            .iter()
            .find(|entry| sessions::group_id(entry) == Some(id.as_str()))
            .map(agent_key)
            .unwrap_or_else(|| format!("native:{id}"));
        self.conversation.native_picker = true;
        self.chat_change_view(View::Agents);
        self.conversation.content_focus = true;
        self.conversation.detail = false;
        self.conversation.selected_agent = Some(selected);
        self.notice = Some("Choose an inbox or verified agent · only message text is sent".into());
        self.dirty = true;
        Ok(())
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
        if self.conversation.native_pending.is_some() {
            self.notice = Some("Waiting for the native message receipt · draft retained".into());
            return;
        }
        let Some(id) = self.conversation.native_recipient.clone() else {
            return;
        };
        let recipient = match self.native_descriptor(&id) {
            Ok(recipient) => recipient,
            Err(error) => {
                self.notice = Some(error);
                return;
            }
        };
        let body = message_body(text, &recipient).to_owned();
        if body.trim().is_empty() {
            self.notice = Some("Write a message after choosing the recipient".into());
            return;
        }
        if self.preview {
            self.notice = Some("Preview does not send native messages · draft retained".into());
            return;
        }
        let request_id = if let Some(retry) = &self.conversation.native_retry
            && retry.recipient_id == id
            && retry.body == body
        {
            retry.request_id.clone()
        } else {
            let mut bytes = [0u8; 16];
            if let Err(error) = crate::platform::random_bytes(&mut bytes) {
                self.notice = Some(format!("Could not create a message request ID: {error}"));
                return;
            }
            format!(
                "ui-{}",
                bytes
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )
        };
        let pending = Pending {
            request_id: request_id.clone(),
            recipient_id: id.clone(),
            body: body.clone(),
            original: self.chat_saved_draft(),
            revision: self.conversation.composer.revision(),
        };
        match transport::send(&id, &body, &request_id) {
            Ok(()) => {
                self.conversation.native_pending = Some(pending);
                self.conversation.native_retry = None;
                self.notice =
                    Some("Submitting native message · draft retained until service receipt".into());
            }
            Err(error) => {
                self.conversation.native_retry = Some(pending);
                self.notice = Some(error);
            }
        }
        self.dirty = true;
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
                    let same = self.conversation.composer.text() == pending.original.text
                        && self.conversation.composer.cursor() == pending.original.cursor
                        && self.conversation.composer.revision() == pending.revision
                        && self.conversation.target == pending.original.target
                        && self.conversation.native_recipient == pending.original.native_recipient
                        && self.conversation.editing.is_none();
                    if same {
                        self.conversation.composer.take_text();
                    }
                    self.conversation.composer.remember(&pending.original.text);
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

    pub(super) fn chat_native_hint(&self) -> Option<String> {
        if !matches!(self.conversation.view, View::Activity | View::Agents) {
            return None;
        }
        if self.conversation.native_pending.is_some() {
            return Some("Submitting native message · draft retained".into());
        }
        let selected = self.conversation.native_recipient.as_deref()?;
        if let Some(receipt) = &self.conversation.native_last
            && field(receipt, "recipient_id") == selected
        {
            return Some(receipt_hint(receipt));
        }
        Some("Native message · text only · Enter sends · /message chooses recipient".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::terminal::{Layout, Ui, preview_state};
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
            original: state.chat_saved_draft(),
            revision: state.conversation.composer.revision(),
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

    #[test]
    fn native_recipient_survives_draft_restore_and_blocks_stale_accounts() {
        let mut state = state(120);
        let entry = state.chat_native_entries().remove(0);
        state.chat_native_select(&entry);
        let saved = state.chat_saved_draft();
        state.conversation.native_recipient = None;
        state.chat_restore_draft(saved.clone());
        assert_eq!(state.conversation.native_recipient, saved.native_recipient);
        assert_eq!(state.conversation.composer.text(), saved.text);
        state.conversation.native_account = Some(("github".into(), "123".into()));
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
