//! Verify exact recipients for current-process groups and parked drafts.
use super::*;

impl From<&Recipient> for SavedRecipient {
    fn from(recipient: &Recipient) -> Self {
        Self {
            id: recipient.id.clone(),
            provider: recipient.account.provider.clone(),
            subject: recipient.account.subject.clone(),
        }
    }
}

impl SavedRecipient {
    pub(super) fn resolve(&self, snapshot: &Value) -> Result<Recipient, String> {
        if snapshot["available"] != true || snapshot["connected"] != true {
            return Err(
                "The new connection has not loaded an authorized recipient directory".into(),
            );
        }
        let value = items(snapshot, "recipients")
            .into_iter()
            .find(|entry| field(entry, "id") == self.id)
            .ok_or_else(|| {
                format!(
                    "Saved recipient {} is not in the current directory",
                    self.id
                )
            })?;
        let current: Recipient = serde_json::from_value(value)
            .map_err(|_| "The current recipient descriptor is inconsistent")?;
        current.validate()?;
        if !current.online || !current.sendable {
            return Err(format!(
                "Saved recipient {} is not currently available",
                self.id
            ));
        }
        if current.account.provider != self.provider || current.account.subject != self.subject {
            return Err(format!(
                "Saved recipient {} changed account; choose the intended identity explicitly",
                self.id
            ));
        }
        Ok(current)
    }
}

impl Selection {
    pub(in crate::client::terminal::conversation) fn saved_selection(&self) -> Vec<SavedRecipient> {
        self.restoring
            .clone()
            .unwrap_or_else(|| self.selected.values().map(SavedRecipient::from).collect())
    }

    pub(in crate::client::terminal::conversation) fn saved_selection_matches(
        &self,
        saved: &[SavedRecipient],
    ) -> bool {
        if let Some(restoring) = &self.restoring {
            return restoring == saved;
        }
        self.selected.len() == saved.len()
            && self.selected.values().zip(saved).all(|(current, saved)| {
                current.id == saved.id
                    && current.account.provider == saved.provider
                    && current.account.subject == saved.subject
            })
    }

    pub(in crate::client::terminal::conversation) fn restore_selection(
        &mut self,
        recipients: Vec<SavedRecipient>,
    ) {
        if !self.saved_selection_matches(&recipients) {
            self.selected.clear();
            self.restoring = (!recipients.is_empty()).then_some(recipients);
            self.restore_error = None;
        }
    }

    pub(in crate::client::terminal::conversation) fn len(&self) -> usize {
        self.restoring
            .as_ref()
            .map_or(self.selected.len(), Vec::len)
    }

    pub(super) fn cancel_restoration(&mut self) {
        self.restoring = None;
        self.restore_error = None;
    }
}

impl State {
    pub(super) fn chat_restore_group_selection(&mut self, snapshot: &Value) {
        let Some(expected) = &self.conversation.groups.restoring else {
            return;
        };
        let resolved = expected
            .iter()
            .map(|expected| {
                expected
                    .resolve(snapshot)
                    .map(|current| (current.id.clone(), current))
            })
            .collect::<Result<BTreeMap<_, _>, _>>();
        match resolved {
            Ok(current) => {
                self.conversation.groups.selected = current;
                self.conversation.groups.cancel_restoration();
                self.notice = Some(format!(
                    "Restored {} exact group recipients after current-directory verification · g composes",
                    self.conversation.groups.selected.len()
                ));
                self.dirty = true;
            }
            Err(error) => {
                if self.conversation.groups.restore_error.as_ref() != Some(&error) {
                    self.conversation.groups.restore_error = Some(error);
                    self.notice = Some(
                        "Saved group selection awaits current-directory verification · /group list reviews it · no message sent".into(),
                    );
                    self.dirty = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::terminal::conversation::groups::tests::{recipient, state};

    fn prepare_group_selection(target: &mut State) {
        let mut chosen = state();
        chosen
            .chat_group_command(&format!(
                "add {} {} {}",
                recipient(1, false).id,
                recipient(1, true).id,
                recipient(2, false).id
            ))
            .unwrap();
        chosen.chat_group_command("save review team").unwrap();
        let selected = chosen.conversation.groups.saved_selection();
        target.conversation.groups.saved = chosen.conversation.groups.saved;
        target.conversation.groups.restore_selection(selected);
    }

    #[test]
    fn saved_selection_comparison_checks_accounts_before_and_after_restore() {
        let mut selection = Selection::default();
        assert!(selection.saved_selection_matches(&[]));
        let item = recipient(1, false);
        selection.selected.insert(item.id.clone(), item);
        let saved = selection.saved_selection();
        assert!(selection.saved_selection_matches(&saved));
        assert!(!selection.saved_selection_matches(&[]));
        let mut changed = saved.clone();
        changed[0].subject = "99".into();
        assert!(!selection.saved_selection_matches(&changed));
        changed[0].subject.clone_from(&saved[0].subject);
        changed[0].provider = "changed".into();
        assert!(!selection.saved_selection_matches(&changed));
        selection.selected.clear();
        selection.restoring = Some(saved.clone());
        assert!(selection.saved_selection_matches(&saved));
        assert!(!selection.saved_selection_matches(&changed));
        selection.restore_selection(saved.clone());
        assert_eq!(selection.restoring.as_ref(), Some(&saved));
    }

    #[test]
    fn restored_group_waits_for_every_current_recipient_and_never_sends_automatically() {
        let mut state = state();
        prepare_group_selection(&mut state);
        let ready = state.native_snapshot();
        let mut incomplete = ready.clone();
        incomplete["recipients"].as_array_mut().unwrap().pop();
        state.chat_restore_group_selection(&incomplete);
        assert!(state.conversation.groups.selected.is_empty());
        assert!(state.group_current_selection().is_err());
        let mut disconnected = ready.clone();
        disconnected["connected"] = json!(false);
        state.chat_restore_group_selection(&disconnected);
        assert!(state.conversation.groups.selected.is_empty());
        state.chat_restore_group_selection(&ready);
        assert_eq!(state.conversation.groups.selected.len(), 3);
        assert!(state.conversation.groups.restoring.is_none());
        assert!(state.conversation.groups.pending.is_none());
        assert!(state.conversation.groups.retry.is_empty());
        assert_eq!(state.native_snapshot()["groups"], json!([]));
        assert_eq!(state.native_snapshot()["outgoing"], json!([]));
    }

    #[test]
    fn send_group_intent_survives_canvas_and_draft_restoration_and_rechecks_accounts() {
        let mut state = state();
        let thread = recipient(1, true);
        state
            .chat_group_command(&format!("add {}", thread.id))
            .unwrap();
        state.conversation.composer.set_text("/send  original 中文");
        state.conversation.composer.left();
        let draft = state.chat_saved_draft();
        assert_eq!(draft.send_group.as_ref().unwrap()[0].id, thread.id);
        state.canvas_focus(View::Audit);
        state.canvas_focus(View::Activity);
        assert_eq!(state.chat_saved_draft(), draft);

        // A parked draft keeps its own exact group even if the passive roster
        // was changed while that draft was out of the composer.
        state.chat_group_command("clear").unwrap();
        state
            .chat_group_command(&format!("add {}", recipient(2, false).id))
            .unwrap();
        state.chat_restore_draft(draft.clone());
        assert_eq!(state.chat_saved_draft(), draft);
        assert!(state.group_current_selection().is_err());

        let mut wrong_account = state.native_snapshot();
        wrong_account["recipients"][1]["account"]["subject"] = json!("99");
        state.chat_restore_group_selection(&wrong_account);
        assert!(state.group_current_selection().is_err());
        assert_eq!(state.chat_saved_draft(), draft);
        let verified = state.native_snapshot();
        state.chat_restore_group_selection(&verified);
        let selection = state.group_current_selection().unwrap();
        assert_eq!(selection.len(), 1);
        assert_eq!(selection[0].id, thread.id);
        assert_eq!(state.chat_saved_draft(), draft);
        assert!(!state.chat_group_pending());
    }

    #[test]
    fn removing_an_unavailable_saved_recipient_requires_an_explicit_user_change() {
        let mut state = state();
        prepare_group_selection(&mut state);
        state.conversation.native_fixture.as_mut().unwrap()["recipients"]
            .as_array_mut()
            .unwrap()
            .pop();
        let incomplete = state.native_snapshot();
        state.chat_restore_group_selection(&incomplete);
        assert!(state.conversation.groups.selected.is_empty());
        state
            .chat_group_command(&format!("remove {}", recipient(2, false).id))
            .unwrap();
        assert_eq!(state.conversation.groups.selected.len(), 2);
        assert!(state.conversation.groups.restoring.is_none());
        assert!(state.conversation.groups.pending.is_none());
    }

    #[test]
    fn restored_group_rejects_changed_account_and_manual_choice_cancels_late_restore() {
        let mut state = state();
        prepare_group_selection(&mut state);
        let mut changed = state.native_snapshot();
        changed["recipients"][0]["account"]["subject"] = json!("99");
        state.chat_restore_group_selection(&changed);
        assert!(state.conversation.groups.selected.is_empty());
        assert!(
            state
                .conversation
                .groups
                .restore_error
                .as_ref()
                .unwrap()
                .contains("changed account")
        );
        state
            .chat_group_command(&format!("add {}", recipient(2, false).id))
            .unwrap();
        let ready = state.native_snapshot();
        state.chat_restore_group_selection(&ready);
        assert_eq!(state.conversation.groups.selected.len(), 1);
        assert!(
            state
                .conversation
                .groups
                .selected
                .contains_key(&recipient(2, false).id)
        );
        assert!(state.conversation.groups.restoring.is_none());
    }

    #[test]
    fn restored_named_group_uses_fresh_descriptors_and_requires_account_revalidation() {
        let mut state = state();
        prepare_group_selection(&mut state);
        state.chat_group_command("clear").unwrap();
        state.conversation.native_fixture.as_mut().unwrap()["recipients"][0]["account"]["subject"] =
            json!("99");
        assert!(state.chat_group_command("use review team").is_err());
        assert!(state.conversation.groups.selected.is_empty());
        state.conversation.native_fixture.as_mut().unwrap()["recipients"][0]["account"]["subject"] =
            json!("42");
        state.conversation.native_fixture.as_mut().unwrap()["recipients"][0]["display_name"] =
            json!("Fresh verified name");
        state.chat_group_command("use review team").unwrap();
        assert_eq!(state.conversation.groups.selected.len(), 3);
        assert_eq!(
            state.conversation.groups.selected[&recipient(1, false).id].display_name,
            "Fresh verified name"
        );
        assert!(state.conversation.groups.pending.is_none());
    }
}
