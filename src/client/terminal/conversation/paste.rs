//! A failed private paste remains recoverable without replacing the user's draft.
use super::*;

impl State {
    pub(super) fn chat_paste_recovery_hint(&self) -> Option<String> {
        self.conversation.paste_recovery.as_ref().map(|text| {
            format!(
                "Original paste retained · {} UTF-8 bytes · /paste-retry",
                text.len()
            )
        })
    }

    pub(super) fn chat_retry_paste(&mut self, argument: &str) -> Result<(), String> {
        if self.conversation.input_task.is_some() {
            return Err("A local input job is still running; retained paste is unchanged".into());
        }
        if argument == "discard" {
            if self.conversation.paste_recovery.take().is_none() {
                return Err("No failed paste is retained".into());
            }
            self.notice = Some("Retained paste discarded · existing draft is unchanged".into());
            return Ok(());
        }
        if !argument.is_empty() {
            return Err(
                "Use /paste-retry, or /paste-retry discard to explicitly clear the retained text"
                    .into(),
            );
        }
        let text = self
            .conversation
            .paste_recovery
            .clone()
            .ok_or("No failed paste is retained")?;
        self.conversation.retrying_paste = true;
        if let Err(error) = self.chat_start_input(attachments::Source::Text(text), true) {
            self.conversation.retrying_paste = false;
            return Err(error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_paste_retains_raw_text_draft_cursor_and_recipient_until_retry() {
        let mut state = super::super::super::preview_state(24, 100, &Ui::default());
        state.conversation.composer.set_text("保留草稿 tail");
        state.conversation.composer.left();
        state.conversation.target = Some("chosen-agent".into());
        let stamp = state.chat_input_stamp().unwrap();
        let original = "中文\r\n  indent\rkept\n".repeat(400);
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::RetainedText {
                text: original.clone(),
                error: "Private directory is unavailable".into(),
            }),
        );
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
        assert_eq!(
            state.conversation.paste_recovery.as_deref(),
            Some(original.as_str())
        );
        assert!(state.notice.as_deref().unwrap().contains("/paste-retry"));
        assert!(state.chat_retry_paste("unexpected").is_err());
        assert_eq!(
            state.conversation.paste_recovery.as_deref(),
            Some(original.as_str())
        );

        state.preview = false;
        assert!(
            state
                .chat_start_input(attachments::Source::Text("new input".into()), false)
                .unwrap_err()
                .contains("previous paste")
        );
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
        assert_eq!(
            state.conversation.paste_recovery.as_deref(),
            Some(original.as_str())
        );

        state.conversation.retrying_paste = true;
        state.chat_finish_input(Some(&stamp), None, Err("Retry still blocked".into()));
        assert!(!state.conversation.retrying_paste);
        assert_eq!(
            state.conversation.paste_recovery.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));

        let reference = "@\".ssmcp/inputs/paste-00000000000000000000000000000000.txt\"";
        state.conversation.retrying_paste = true;
        state.chat_finish_input(
            Some(&stamp),
            None,
            Ok(attachments::Loaded::Reference {
                reference: reference.into(),
                description: "Saved original text".into(),
            }),
        );
        assert!(state.conversation.paste_recovery.is_none());
        assert!(state.conversation.composer.text().contains(reference));
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        assert!(state.conversation.submitting.is_none());
    }

    #[test]
    fn retained_paste_requires_explicit_discard_and_never_enters_snapshot() {
        let mut state = super::super::super::preview_state(24, 48, &Ui::default());
        state.conversation.composer.set_text("existing draft");
        let before = state.chat_input_stamp();
        state.conversation.paste_recovery = Some("private pasted content".into());
        let snapshot = state.conversation.snapshot();
        assert_eq!(
            snapshot["retained_paste_bytes"],
            "private pasted content".len()
        );
        assert!(!snapshot.to_string().contains("private pasted content"));
        assert!(
            state.chat_retry_paste("").is_err(),
            "preview cannot write a private file"
        );
        assert!(state.conversation.paste_recovery.is_some());
        assert!(!state.conversation.retrying_paste);
        state.chat_retry_paste("discard").unwrap();
        assert!(state.conversation.paste_recovery.is_none());
        assert_eq!(state.chat_input_stamp(), before);
    }
}
