use super::*;
use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-goal-ui-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!path.exists());
        crate::platform::private_directory(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state() -> State {
    let mut state = super::super::super::preview_state(28, 120, &Ui::default());
    state.preview = false;
    state.events.clear();
    state.conversation.restored = true;
    state
}

async fn settle(state: &mut State) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            state.chat_poll_goal();
            if state.conversation.goal.task.is_none() && state.conversation.goal.loaded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("goal worker completes");
}

fn screen(state: &mut State) -> String {
    // Render only this fixture's state; preview suppresses unrelated live connection polls.
    state.preview = true;
    let frame = state.chat_frame(Instant::now());
    state.preview = false;
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
        std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
        0,
    );
    parser.process(&frame.diff(None));
    parser.screen().contents()
}

#[test]
fn goal_commands_are_explicit_bounded_and_discoverable() {
    assert_eq!(Command::parse("").unwrap(), Command::Show);
    assert_eq!(Command::parse("show").unwrap(), Command::Show);
    assert_eq!(Command::parse("clear").unwrap(), Command::Clear);
    assert_eq!(
        Command::parse("set clear").unwrap(),
        Command::Set("clear".into())
    );
    assert_eq!(
        Command::parse("Build a fixture").unwrap(),
        Command::Set("Build a fixture".into())
    );
    for bad in ["set", "clear extra", "show extra", "set first\nsecond"] {
        assert!(Command::parse(bad).is_err(), "{bad}");
    }
    assert!(
        crate::client::composer::SLASH_COMMANDS
            .iter()
            .any(|command| command.name == "/goal")
    );
    let mut state = state();
    state.preview = true;
    state.conversation.composer.set_text("retained draft");
    for command in ["/goal", "/goal set example", "/goal clear"] {
        assert!(state.chat_command(command).unwrap_err().contains("Preview"));
    }
    assert_eq!(state.conversation.composer.text(), "retained draft");
    assert!(state.actions.is_empty());
}

#[tokio::test]
async fn goal_enter_saves_without_sending_and_projects_current_goal_to_ui() {
    let fixture = Fixture::new();
    history::with_test_history(fixture.0.join("history"), async {
        let mut state = state();
        // /goal remains a local command even with an exact native recipient selected.
        state.conversation.native_recipient = Some("exact-fixture-thread".into());
        state
            .conversation
            .composer
            .set_text("/goal set Ship the local fixture");
        state.chat_submit();
        assert!(state.conversation.goal.saving());
        assert!(state.conversation.composer.is_empty());
        assert!(state.conversation.native_pending.is_none());
        assert!(state.conversation.submitting.is_none());
        assert!(state.actions.is_empty());
        settle(&mut state).await;
        let owner = history::current_owner().unwrap();
        let goal = state.conversation.snapshot()["goal"].clone();
        assert_eq!(goal["conversation_id"], owner.id());
        assert_eq!(goal["text"], "Ship the local fixture");
        assert_eq!(goal["source"], "user");
        assert_eq!(goal["status"], "set");
        assert_eq!(goal["saving"], false);
        assert!(history::current().unwrap().unwrap().messages.is_empty());
        for width in [120, 48] {
            state.resize(28, width);
            let rendered = screen(&mut state);
            assert!(rendered.contains("Goal ·"), "{rendered}");
            assert!(rendered.contains("Ship the local fixture"), "{rendered}");
        }
        state.conversation.composer.set_text("keep the next draft");
        state.chat_command("/goal clear").unwrap();
        settle(&mut state).await;
        assert_eq!(state.conversation.snapshot()["goal"]["status"], "unset");
        assert_eq!(store::read(&owner).unwrap(), None);
        assert_eq!(state.conversation.composer.text(), "keep the next draft");
        assert!(history::current().unwrap().unwrap().messages.is_empty());
    })
    .await;
}

#[tokio::test]
async fn late_goal_completion_cannot_replace_or_display_in_another_conversation() {
    let fixture = Fixture::new();
    history::with_test_history(fixture.0.join("history"), async {
        let mut state = state();
        let first = history::capture(None).unwrap();
        store::change(&first, Some("First private goal")).unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        state.conversation.goal.owner = Some(first.clone());
        state.conversation.goal.task = Some(Task {
            owner: first.clone(),
            action: Action::Set,
            receiver,
        });
        assert!(state.chat_command("/reload").unwrap_err().contains("goal"));
        history::release_current().unwrap();
        let second = history::capture(None).unwrap();
        store::change(&second, Some("Second goal")).unwrap();
        sender.send(Ok(Some("First private goal".into()))).unwrap();
        assert_eq!(
            state.conversation.snapshot()["goal"]["conversation_id"],
            second.id()
        );
        assert!(state.conversation.snapshot()["goal"]["text"].is_null());
        state.chat_poll_goal();
        settle(&mut state).await;
        assert_eq!(state.conversation.snapshot()["goal"]["text"], "Second goal");
        assert!(!screen(&mut state).contains("First private goal"));
        assert!(
            !state
                .events
                .snapshot()
                .to_string()
                .contains("First private goal")
        );
        assert!(state.actions.is_empty());
    })
    .await;
}

#[tokio::test]
async fn goal_reload_reads_saved_value_without_replaying_a_change() {
    let fixture = Fixture::new();
    history::with_test_history(fixture.0.join("history"), async {
        let mut before = state();
        before
            .chat_command("/goal set Restore the saved goal")
            .unwrap();
        settle(&mut before).await;
        let conversation = history::current().unwrap().unwrap();
        let snapshot = before.chat_reload_snapshot();
        assert!(
            serde_json::to_value(&snapshot)
                .unwrap()
                .get("goal")
                .is_none()
        );
        let old_owner = history::current_owner().unwrap();
        history::release_current().unwrap();
        history::resume(&conversation.metadata.id).unwrap();
        let new_owner = history::current_owner().unwrap();
        assert_ne!(old_owner, new_owner);
        let mut after = state();
        after.chat_apply_reload_snapshot(
            snapshot,
            history::sessions().unwrap(),
            Some(&conversation),
        );
        assert!(after.conversation.goal.task.is_none());
        after.chat_poll_goal();
        assert_eq!(
            after.conversation.goal.task.as_ref().unwrap().action,
            Action::Load
        );
        settle(&mut after).await;
        assert_eq!(
            after.conversation.snapshot()["goal"]["text"],
            "Restore the saved goal"
        );
        assert_eq!(history::current().unwrap().unwrap(), conversation);
        assert!(after.actions.is_empty());
    })
    .await;
}
