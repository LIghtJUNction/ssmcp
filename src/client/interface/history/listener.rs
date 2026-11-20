//! Pin program results to the conversation that supplied the accepted input.
use super::*;

impl History {
    fn listener_conversation(&mut self) -> Result<String, String> {
        if let Some(active) = &self.active {
            self.check_listener_conversation(&active.metadata.id)?;
            return Ok(active.metadata.id.clone());
        }
        let _lock = self.directory.lock()?;
        let metadata = SessionMetadata {
            schema: SCHEMA.into(),
            schema_version: 1,
            id: random_id("conversation")?,
            workspace: self.workspace.clone(),
            created_at_ms: now_ms(),
            truncated_messages: 0,
        };
        let lease = self.lease(&metadata.id)?;
        let conversation = Conversation {
            metadata: metadata.clone(),
            messages: Vec::new(),
        };
        let bytes = Self::encode(&conversation)?;
        self.retain(&metadata.id, bytes.len())?;
        self.directory
            .write_atomic(&session_name(&metadata.id)?, &bytes)?;
        let id = metadata.id.clone();
        self.active = Some(Active {
            metadata,
            _lease: lease,
        });
        Ok(id)
    }

    fn check_listener_conversation(&self, id: &str) -> Result<(), String> {
        if self
            .active
            .as_ref()
            .is_none_or(|active| active.metadata.id != id)
            || self.archive_index()?.contains(id)
        {
            return Err(
                "listener_conversation_changed: the original conversation is no longer active"
                    .into(),
            );
        }
        Ok(())
    }

    fn record_listener_result(
        &mut self,
        conversation_id: &str,
        delivery_id: &str,
        source: &str,
        message_id: &str,
        text: &str,
    ) -> Result<(), String> {
        self.check_listener_conversation(conversation_id)?;
        validate_content(source)?;
        validate_content(text)?;
        if !valid_id(message_id, "message") {
            return Err("invalid listener result message identity".into());
        }
        // A very fast provider can return before the sender processes its accepted ACK.
        // Both paths share the existing deterministic request identity.
        self.record_request(delivery_id, source)?;
        self.record_agent(message_id, text)?;
        Ok(())
    }
}

pub(crate) fn ensure_current_id() -> Result<String, String> {
    with(History::listener_conversation)
}

pub(crate) fn record_request_in(
    conversation_id: &str,
    request_id: &str,
    text: &str,
) -> Result<(), String> {
    with(|history| {
        history.check_listener_conversation(conversation_id)?;
        history.record_request(request_id, text).map(|_| ())
    })
}

/// Used only by the small publication append. The History mutex prevents /resume
/// or archive from changing its destination until that append completes.
pub(crate) fn with_listener_conversation<T>(
    id: &str,
    publish: impl FnOnce() -> T,
) -> Result<T, String> {
    with(|history| {
        history.check_listener_conversation(id)?;
        Ok(publish())
    })
}

pub(crate) fn record_listener_result(
    conversation_id: &str,
    delivery_id: &str,
    source: &str,
    message_id: &str,
    text: &str,
) -> Result<(), String> {
    with(|history| {
        history.record_listener_result(conversation_id, delivery_id, source, message_id, text)
    })
    // Explicit program input/results stay out of shared automatic context memory.
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(random_id("listener_history_test").unwrap());
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }
        fn history(&self) -> History {
            History::with_workspace(self.0.join("conversations"), "/fixture/workspace".into())
                .unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn listener_history_pins_empty_conversation_and_records_input_before_result_once() {
        let fixture = Fixture::new();
        let mut history = fixture.history();
        let conversation = history.listener_conversation().unwrap();
        assert_eq!(history.listener_conversation().unwrap(), conversation);
        assert!(history.current().unwrap().unwrap().messages.is_empty());
        let message_id = "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        history
            .record_listener_result(
                &conversation,
                "delivery_1",
                "  original\n输入",
                message_id,
                "模型结果",
            )
            .unwrap();
        history
            .record_request("delivery_1", "  original\n输入")
            .unwrap();
        history
            .record_listener_result(
                &conversation,
                "delivery_1",
                "  original\n输入",
                message_id,
                "模型结果",
            )
            .unwrap();
        let saved = history.current().unwrap().unwrap();
        assert_eq!(saved.metadata.id, conversation);
        assert_eq!(saved.messages.len(), 2);
        assert_eq!(saved.messages[0].role, "user");
        assert_eq!(saved.messages[0].content, "  original\n输入");
        assert_eq!(saved.messages[1].role, "assistant");
        assert_eq!(saved.messages[1].id, message_id);
        assert_eq!(saved.messages[1].content, "模型结果");
    }

    #[test]
    fn listener_history_rejects_late_result_after_release_or_resume() {
        let fixture = Fixture::new();
        let mut history = fixture.history();
        let original = history.listener_conversation().unwrap();
        let before = history.read(&original).unwrap();
        history.release_current();
        assert!(
            history
                .record_listener_result(
                    &original,
                    "delivery_1",
                    "source",
                    "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "late"
                )
                .is_err()
        );
        let next = history.listener_conversation().unwrap();
        assert_ne!(next, original);
        assert!(
            history
                .record_listener_result(
                    &original,
                    "delivery_1",
                    "source",
                    "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "late"
                )
                .is_err()
        );
        assert_eq!(history.read(&original).unwrap(), before);
        assert!(history.current().unwrap().unwrap().messages.is_empty());
    }

    #[test]
    fn listener_history_rejects_invalid_result_before_recording_source() {
        let fixture = Fixture::new();
        let mut history = fixture.history();
        let original = history.listener_conversation().unwrap();
        assert!(
            history
                .record_listener_result(
                    &original,
                    "delivery_1",
                    "source",
                    "message_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "\u{1b}[2J"
                )
                .is_err()
        );
        assert!(history.current().unwrap().unwrap().messages.is_empty());
    }
}
