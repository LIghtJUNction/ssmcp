use super::*;
use std::path::PathBuf;

fn memory() -> (PathBuf, History) {
    let path = std::env::temp_dir().join(random_id("ssmcp-goal-memory").unwrap());
    let history = History::with_workspace(path.clone(), "/goal-fixture".into()).unwrap();
    (path, history)
}

#[test]
fn goal_validation_keeps_existing_text_bounds() {
    assert_eq!(
        normalize("  Finish the current task  ").unwrap(),
        "Finish the current task"
    );
    assert_eq!(normalize("完成当前任务").unwrap(), "完成当前任务");
    for invalid in [
        "",
        "   ",
        "two\nlines",
        "tab\there",
        "line\u{2028}break",
        "line\u{2029}break",
    ] {
        assert!(normalize(invalid).is_err(), "{invalid:?}");
    }
    assert!(normalize(&"x".repeat(MAX_GOAL_CHARS + 1)).is_err());
    assert!(normalize(&"🙂".repeat(MAX_GOAL_CHARS)).is_ok());
    assert!(normalize(&"🙂".repeat(MAX_GOAL_CHARS + 1)).is_err());
}

#[test]
fn clearing_a_pending_goal_does_not_create_a_context_or_files() {
    let (path, mut history) = memory();
    let owner = history.capture_owner().unwrap();
    assert_eq!(history.read_goal(&owner).unwrap(), None);
    assert_eq!(history.change_goal(&owner, None).unwrap(), None);
    assert!(history.active.is_none());
    assert!(history.conversations.is_empty());
    assert!(history.goals.is_empty());
    assert!(!path.exists());
}

#[test]
fn setting_a_pending_goal_materializes_only_the_original_ram_context() {
    let (path, mut history) = memory();
    let owner = history.capture_owner().unwrap();
    assert_eq!(
        history
            .change_goal(&owner, Some("  current task  "))
            .unwrap(),
        Some("current task".into())
    );
    assert_eq!(history.active.as_ref().unwrap().metadata.id, owner.id());
    assert!(history.load(owner.id()).unwrap().messages.is_empty());
    assert_eq!(
        history.read_goal(&owner).unwrap(),
        Some("current task".into())
    );
    assert!(!path.exists());

    let replacement = History::with_workspace(path.clone(), "/goal-fixture".into()).unwrap();
    assert!(replacement.goals.is_empty());
    assert!(replacement.read_goal(&owner).is_err());
    assert!(!path.exists());
}

#[test]
fn goal_changes_require_the_selected_unrevoked_owner() {
    let (path, mut history) = memory();
    let first = history.capture_owner().unwrap();
    history.change_goal(&first, Some("first goal")).unwrap();
    let second = history
        .ownership
        .select(&random_id("conversation").unwrap(), true);
    assert!(history.read_goal(&first).is_err());
    assert!(
        history
            .change_goal(&first, Some("wrong destination"))
            .is_err()
    );
    history.change_goal(&second, Some("second goal")).unwrap();
    assert_eq!(history.goals.get(first.id()).unwrap(), "first goal");
    assert_eq!(
        history.read_goal(&second).unwrap(),
        Some("second goal".into())
    );

    history.ownership.revoke(second.id());
    assert!(history.read_goal(&second).is_err());
    assert!(history.change_goal(&second, None).is_err());
    assert_eq!(history.goals.get(second.id()).unwrap(), "second goal");
    assert!(!path.exists());
}

#[test]
fn invalid_goal_leaves_current_ram_value_unchanged() {
    let (path, mut history) = memory();
    let owner = history.capture_owner().unwrap();
    history.change_goal(&owner, Some("keep this goal")).unwrap();
    assert!(
        history
            .change_goal(&owner, Some("invalid\nchange"))
            .is_err()
    );
    assert_eq!(
        history.read_goal(&owner).unwrap(),
        Some("keep this goal".into())
    );
    assert_eq!(history.change_goal(&owner, None).unwrap(), None);
    assert!(history.read_goal(&owner).unwrap().is_none());
    assert!(!path.exists());
}
