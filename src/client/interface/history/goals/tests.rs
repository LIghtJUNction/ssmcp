use super::*;
use crate::client::interface::history::tests::Temporary;

#[test]
fn goal_round_trip_and_archive_preserve_original_message_bytes() {
    let fixture = Temporary::new();
    let mut history = fixture.history();
    let owner = history.capture_owner().unwrap();
    assert_eq!(history.read_goal(&owner).unwrap(), None);
    assert_eq!(history.change_goal(&owner, None).unwrap(), None);
    assert!(history.current().unwrap().is_none());
    history
        .change_goal(&owner, Some("  Ship the local fixture  "))
        .unwrap();
    assert!(history.current().unwrap().unwrap().messages.is_empty());
    history.record("user", "original input").unwrap();
    let original = History::encode(&history.current().unwrap().unwrap()).unwrap();
    history
        .change_goal(&owner, Some("Verify the local fixture"))
        .unwrap();
    assert_eq!(
        History::encode(&history.current().unwrap().unwrap()).unwrap(),
        original
    );
    let id = owner.id().to_owned();
    history.archive_change(Some(&id), true).unwrap();
    assert!(history.read_goal(&owner).is_err());
    assert!(
        history
            .directory
            .names()
            .unwrap()
            .contains(&name(&id).unwrap())
    );
    history.archive_change(Some(&id), false).unwrap();
    history.resume(&id).unwrap();
    let resumed = history.capture_owner().unwrap();
    assert_eq!(
        history.read_goal(&resumed).unwrap().as_deref(),
        Some("Verify the local fixture")
    );
    history.change_goal(&resumed, None).unwrap();
    assert_eq!(history.read_goal(&resumed).unwrap(), None);
    assert!(
        !history
            .directory
            .names()
            .unwrap()
            .contains(&name(&id).unwrap())
    );
    assert_eq!(
        History::encode(&history.current().unwrap().unwrap()).unwrap(),
        original
    );
}

#[test]
fn goal_rejects_old_owner_even_after_same_history_is_resumed() {
    let fixture = Temporary::new();
    let mut history = fixture.history();
    let first = history.capture_owner().unwrap();
    history.change_goal(&first, Some("First goal")).unwrap();
    history.release_current();
    let second = history.capture_owner().unwrap();
    history.change_goal(&second, Some("Second goal")).unwrap();
    assert!(history.change_goal(&first, Some("late write")).is_err());
    assert_eq!(
        history.read_goal(&second).unwrap().as_deref(),
        Some("Second goal")
    );
    history.resume(first.id()).unwrap();
    let resumed = history.capture_owner().unwrap();
    assert_ne!(resumed, first);
    assert!(history.change_goal(&first, Some("stale lease")).is_err());
    assert!(history.change_goal(&second, Some("wrong current")).is_err());
    assert_eq!(
        history.read_goal(&resumed).unwrap().as_deref(),
        Some("First goal")
    );
}

#[test]
fn goal_text_is_bounded_single_line_redacted_and_identity_checked() {
    assert_eq!(
        normalize(&"😀".repeat(MAX_GOAL_CHARS)).unwrap().len(),
        MAX_GOAL_BYTES
    );
    for invalid in [
        "".into(),
        "   ".into(),
        "x".repeat(MAX_GOAL_CHARS + 1),
        "a\nb".into(),
        "a\rb".into(),
        "a\u{1b}[2J".into(),
        "a\u{2028}b".into(),
    ] {
        assert!(normalize(&invalid).is_err(), "{invalid:?}");
    }
    let fixture = Temporary::new();
    let mut history = fixture.history();
    let owner = history.capture_owner().unwrap();
    history
        .change_goal(&owner, Some("API_TOKEN=private_fixture_goal_secret"))
        .unwrap();
    assert!(
        !history
            .read_goal(&owner)
            .unwrap()
            .unwrap()
            .contains("private_fixture_goal_secret")
    );
    let valid = Goal {
        schema_version: 1,
        id: owner.id().into(),
        workspace: history.workspace.clone(),
        text: "safe".into(),
    };
    let mut value = serde_json::to_value(valid).unwrap();
    for key in ["id", "workspace"] {
        let saved = value[key].clone();
        value[key] = json!("different-owner");
        history
            .directory
            .write_atomic(
                &name(owner.id()).unwrap(),
                &serde_json::to_vec(&value).unwrap(),
            )
            .unwrap();
        assert!(
            history
                .read_goal(&owner)
                .unwrap_err()
                .contains("identity or workspace")
        );
        value[key] = saved;
    }
    value["text"] = json!("unsafe\u{1b}[2J");
    history
        .directory
        .write_atomic(
            &name(owner.id()).unwrap(),
            &serde_json::to_vec(&value).unwrap(),
        )
        .unwrap();
    assert!(history.read_goal(&owner).is_err());
    // Explicit clear can recover a malformed private sidecar without changing history.
    history.change_goal(&owner, None).unwrap();
    assert_eq!(history.read_goal(&owner).unwrap(), None);
}

#[test]
fn conversation_retention_removes_its_goal_sidecar() {
    let fixture = Temporary::new();
    let mut history = fixture.history();
    let owner = history.capture_owner().unwrap();
    history
        .change_goal(&owner, Some("Reclaim with its conversation"))
        .unwrap();
    let id = owner.id().to_owned();
    history.release_current();
    history
        .retain(
            "conversation_ffffffffffffffffffffffffffffffff",
            MAX_TOTAL_BYTES,
        )
        .unwrap();
    let names = history.directory.names().unwrap();
    assert!(!names.contains(&session_name(&id).unwrap()));
    assert!(!names.contains(&name(&id).unwrap()));
}
