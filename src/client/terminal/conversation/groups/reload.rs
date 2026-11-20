//! Reload keeps user-chosen identities, never send plans or connection credentials.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedRecipient {
    id: String,
    provider: String,
    subject: String,
}

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
    fn validate(&self) -> Result<(), String> {
        crate::wire::messages::parse_recipient(&self.id)?;
        if self.provider != "github"
            || self.subject.len() > 20
            || !self.subject.bytes().all(|byte| byte.is_ascii_digit())
            || !self.subject.parse::<u64>().is_ok_and(|id| id > 0)
        {
            return Err("Invalid saved group account identity".into());
        }
        Ok(())
    }

    pub(super) fn id(&self) -> &str {
        &self.id
    }

    pub(super) fn resolve(&self, snapshot: &Value) -> Result<Recipient, String> {
        self.validate()?;
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::client::terminal::conversation) struct ReloadGroups {
    selected: Vec<SavedRecipient>,
    saved: BTreeMap<String, Vec<SavedRecipient>>,
}

impl ReloadGroups {
    pub(in crate::client::terminal::conversation) fn validate(&self) -> Result<(), String> {
        fn recipients(items: &[SavedRecipient]) -> Result<(), String> {
            if items.len() > transport::MAX_GROUP_RECIPIENTS {
                return Err("A saved group contains more than 32 recipients".into());
            }
            let mut ids = std::collections::BTreeSet::new();
            for item in items {
                item.validate()?;
                if !ids.insert(item.id.as_str()) {
                    return Err("A saved group contains duplicate recipient identities".into());
                }
            }
            Ok(())
        }
        recipients(&self.selected)?;
        if self.saved.len() > 16 {
            return Err("A reload checkpoint contains more than 16 named groups".into());
        }
        for (name, members) in &self.saved {
            if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                return Err("A saved group has an invalid name".into());
            }
            if members.is_empty() {
                return Err("A named group has no selected recipients".into());
            }
            recipients(members)?;
        }
        Ok(())
    }
}

impl Selection {
    pub(in crate::client::terminal::conversation) fn reload_snapshot(&self) -> ReloadGroups {
        ReloadGroups {
            selected: self
                .restoring
                .clone()
                .unwrap_or_else(|| self.selected.values().map(SavedRecipient::from).collect()),
            saved: self.saved.clone(),
        }
    }

    pub(in crate::client::terminal::conversation) fn restore(
        &mut self,
        saved: ReloadGroups,
    ) -> Result<(), String> {
        saved.validate()?;
        *self = Self {
            saved: saved.saved,
            restoring: (!saved.selected.is_empty()).then_some(saved.selected),
            ..Self::default()
        };
        Ok(())
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
                    "Restored {} exact group recipients after current-directory verification · Enter still required to send",
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

    fn saved() -> ReloadGroups {
        let mut state = state();
        state
            .chat_group_command(&format!(
                "add {} {} {}",
                recipient(1, false).id,
                recipient(1, true).id,
                recipient(2, false).id
            ))
            .unwrap();
        state.chat_group_command("save review team").unwrap();
        state.conversation.groups.reload_snapshot()
    }

    #[test]
    fn reload_group_configuration_contains_only_exact_identities_and_no_send_plan() {
        let mut state = state();
        state
            .chat_group_command(&format!("add {}", recipient(1, false).id))
            .unwrap();
        state.chat_group_command("save one").unwrap();
        state.conversation.groups.last = Some("old-request-must-not-restore".into());
        state.conversation.groups.retry = Some(Pending {
            request_id: "old-request-must-not-restore".into(),
            recipients: vec![recipient(1, false)],
            body: "old-body-must-not-restore".into(),
            original: state.chat_saved_draft(),
            revision: state.conversation.composer.revision(),
        });
        let saved = state.conversation.groups.reload_snapshot();
        let encoded = serde_json::to_value(&saved).unwrap();
        let identity = &encoded["selected"][0];
        assert_eq!(identity.as_object().unwrap().len(), 3);
        assert_eq!(identity["id"], recipient(1, false).id);
        assert_eq!(identity["provider"], "github");
        assert_eq!(identity["subject"], "42");
        assert!(!encoded.to_string().contains("old-request"));
        assert!(!encoded.to_string().contains("old-body"));
        assert!(!encoded.to_string().contains("token"));
        state.conversation.groups.restore(saved).unwrap();
        assert!(state.conversation.groups.selected.is_empty());
        assert!(state.conversation.groups.pending.is_none());
        assert!(state.conversation.groups.retry.is_none());
        assert!(state.conversation.groups.last.is_none());
        assert!(state.conversation.groups.observed.is_none());
    }

    #[test]
    fn restored_group_waits_for_every_current_recipient_and_never_sends_automatically() {
        let mut state = state();
        state.conversation.groups.restore(saved()).unwrap();
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
        assert!(state.conversation.groups.retry.is_none());
        assert_eq!(state.native_snapshot()["groups"], json!([]));
        assert_eq!(state.native_snapshot()["outgoing"], json!([]));
    }

    #[test]
    fn removing_an_unavailable_saved_recipient_requires_an_explicit_user_change() {
        let mut state = state();
        state.conversation.groups.restore(saved()).unwrap();
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
        state.conversation.groups.restore(saved()).unwrap();
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
        state.conversation.groups.restore(saved()).unwrap();
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

    #[test]
    fn reload_group_configuration_rejects_duplicates_wildcards_and_credential_fields() {
        let original = serde_json::to_value(saved()).unwrap();
        let mut duplicate = original.clone();
        let repeated = duplicate["selected"][0].clone();
        duplicate["selected"].as_array_mut().unwrap().push(repeated);
        assert!(
            serde_json::from_value::<ReloadGroups>(duplicate)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut wildcard = original.clone();
        wildcard["selected"][0]["id"] = json!("*");
        assert!(
            serde_json::from_value::<ReloadGroups>(wildcard)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut credential = original;
        credential["selected"][0]["token"] = json!("must-not-restore");
        assert!(serde_json::from_value::<ReloadGroups>(credential).is_err());
    }
}
