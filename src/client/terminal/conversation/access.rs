//! Display only access lists acknowledged by the current connection's server.
use super::*;
use crate::client::access as session_access;

#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    List,
    Add(&'a str),
    Remove(&'a str),
}

fn parse(arguments: &str) -> Result<Action<'_>, String> {
    let words: Vec<_> = arguments.split_whitespace().collect();
    match words.as_slice() {
        [] => Ok(Action::List),
        ["add" | "remove"] => Err("Use /allow, /allow USER, or /allow remove USER".into()),
        [user] | ["add", user] => Ok(Action::Add(user)),
        ["remove", user] => Ok(Action::Remove(user)),
        _ => Err("Use /allow, /allow USER, or /allow remove USER".into()),
    }
}

fn message(snapshot: &session_access::Snapshot) -> String {
    if !snapshot.connected {
        return snapshot.last_error.as_deref().map(panel::safe_text)
            .unwrap_or_else(|| "Disconnected · session access is not confirmed".into());
    }
    if !snapshot.supported {
        return "Session access requires an updated server and an account HTTPS connection".into();
    }
    let mut text = String::new();
    if snapshot.pending {
        text.push_str("Waiting for the server to confirm session access.\n");
    } else if let Some(error) = &snapshot.last_error {
        text.push_str(&panel::safe_text(error));
        text.push('\n');
    } else if let Some(message) = &snapshot.last_message {
        text.push_str(&panel::safe_text(message));
        text.push('\n');
    }
    if snapshot.loaded {
        text.push_str(if snapshot.pending || snapshot.last_error.is_some() {
            "\nLast confirmed list:\n"
        } else {
            "\nAllowed users for this session:\n"
        });
        for entry in &snapshot.entries {
            text.push_str("- ");
            text.push_str(&panel::safe_text(&entry.principal));
            if entry.owner {
                text.push_str(" · owner");
            }
            text.push('\n');
        }
    } else if !snapshot.pending {
        text.push_str("\nThe current list is not confirmed. Use /allow to read it again.\n");
    }
    text.push_str("\n/allow USER grants access · /allow remove USER revokes access");
    text
}

impl State {
    pub(super) fn chat_allow(&mut self, arguments: &str) -> Result<(), String> {
        let action = parse(arguments)?;
        if self.preview {
            return Err("Preview cannot read or change a live session's access list".into());
        }
        self.chat_poll_access();
        let revision = session_access::snapshot().revision;
        match action {
            Action::List => session_access::refresh()?,
            Action::Add(user) => session_access::add(user)?,
            Action::Remove(user) => session_access::remove(user)?,
        }
        // Ok means queued. A later revision with pending=false is the result.
        self.conversation.access_request = Some(revision);
        self.notice = Some("Session access requested · waiting for the server".into());
        self.dirty = true;
        Ok(())
    }

    pub(super) fn chat_poll_access(&mut self) {
        if self.conversation.access_request.is_some() {
            self.chat_finish_access(&session_access::snapshot());
        }
    }

    fn chat_finish_access(&mut self, snapshot: &session_access::Snapshot) {
        let Some(revision) = self.conversation.access_request else { return; };
        if snapshot.pending || snapshot.revision == revision {
            return;
        }
        self.conversation.access_request = None;
        let text = message(snapshot);
        let raw = format!("\n[ACCESS]\n{text}\n");
        self.log_event("access", &json!({"type":"ssmcp.message", "author":"Session access", "text":text}), raw.as_bytes());
        self.notice = Some(if let Some(error) = &snapshot.last_error {
            panel::safe_text(error)
        } else if snapshot.connected && snapshot.supported && snapshot.loaded {
            snapshot.last_message.as_deref().map(panel::safe_text)
                .unwrap_or_else(|| "Session access list received · see Conversation".into())
        } else {
            "Session access is not confirmed · see Conversation".into()
        });
        self.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn confirmed() -> session_access::Snapshot {
        session_access::Snapshot {
            connected: true, supported: true, loaded: true, revision: 2,
            entries: vec![session_access::Entry {
                principal: "alice@github".into(), login: "alice".into(), subject: "42".into(), owner: true,
            }], ..Default::default()
        }
    }

    #[test]
    fn allow_syntax_keeps_user_resolution_in_the_authoritative_backend() {
        assert_eq!(parse("").unwrap(), Action::List);
        for user in ["Alice", "@Alice", "Alice@github"] {
            assert_eq!(parse(user).unwrap(), Action::Add(user));
            assert_eq!(parse(&format!("add {user}")).unwrap(), Action::Add(user));
            assert_eq!(parse(&format!("remove {user}")).unwrap(), Action::Remove(user));
        }
        for value in ["add", "remove", "alice bob", "remove alice extra"] {
            assert!(parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn access_results_wait_for_acknowledgement_and_preserve_the_editor() {
        let mut state = super::super::super::preview_state(28, 100, &Ui::default());
        state.conversation.composer.set_text("Keep my draft");
        state.conversation.target = Some("chosen-agent".into());
        let stamp = state.chat_input_stamp().unwrap();
        let before = state.events.snapshot()["retained_events"].as_u64().unwrap();
        state.conversation.access_request = Some(1);
        let mut snapshot = confirmed();
        snapshot.pending = true;
        snapshot.last_message = Some("not yet authoritative".into());
        state.chat_finish_access(&snapshot);
        assert_eq!(state.events.snapshot()["retained_events"], before);
        assert!(state.conversation.access_request.is_some());
        snapshot.pending = false;
        snapshot.last_message = None;
        state.chat_finish_access(&snapshot);
        assert_eq!(state.events.snapshot()["retained_events"], before + 1);
        assert!(state.conversation.access_request.is_none());
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&stamp));
        state.chat_finish_access(&snapshot);
        assert_eq!(state.events.snapshot()["retained_events"], before + 1, "one result per request");
        let frame = state.chat_frame(Instant::now());
        assert!(frame.rows.join("\n").contains("alice@github"));
        assert!(frame.rows.join("\n").contains("owner"));
    }

    #[test]
    fn access_errors_and_unknown_results_never_claim_a_new_grant() {
        let mut snapshot = confirmed();
        snapshot.last_error = Some("GitHub account not found".into());
        let shown = message(&snapshot);
        assert!(shown.contains("GitHub account not found") && shown.contains("Last confirmed list"));
        snapshot.loaded = false;
        let shown = message(&snapshot);
        assert!(shown.contains("not confirmed"));
        assert!(!shown.contains("alice@github"));
        snapshot.connected = false;
        assert!(!message(&snapshot).contains("Allowed users"));
    }

    #[test]
    fn allow_preview_and_invalid_commands_never_queue_access_or_replace_the_draft() {
        let mut state = super::super::super::preview_state(24, 48, &Ui::default());
        state.conversation.composer.set_text("保留草稿");
        let before = state.chat_input_stamp().unwrap();
        for command in ["/allow", "/allow @alice", "/allow add alice", "/allow remove alice", "/allow remove"] {
            assert!(state.chat_command(command).is_err());
            assert!(state.conversation.access_request.is_none());
            assert_eq!(state.chat_input_stamp().as_ref(), Some(&before));
        }
    }
}
