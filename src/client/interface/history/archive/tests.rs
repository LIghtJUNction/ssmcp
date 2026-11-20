use super::*;

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(random_id("history_archive_test").unwrap());
        crate::platform::private_directory(&path).unwrap();
        Self(path)
    }
    fn history(&self) -> History {
        History::with_workspace(self.0.join("conversations"), "/fixture/workspace".into()).unwrap()
    }
    fn bytes(&self, id: &str) -> Vec<u8> {
        fs::read(self.0.join("conversations").join(session_name(id).unwrap())).unwrap()
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn history_archive_roundtrip_keeps_original_bytes_and_never_selects_or_replays() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history
        .record("user", "Archive this original 中文 message")
        .unwrap();
    history.record("assistant", "Retained reply").unwrap();
    let original = history.current().unwrap().unwrap();
    let id = original.metadata.id.clone();
    let bytes = directory.bytes(&id);
    let archived = history.archive_change(None, true).unwrap();
    assert_eq!(archived["id"], id);
    assert_eq!(archived["archived"], true);
    assert_eq!(archived["changed"], true);
    assert_eq!(archived["contents_changed"], false);
    assert!(history.current().unwrap().is_none());
    assert!(history.sessions().unwrap().is_empty());
    assert_eq!(
        history.sessions_filtered(Filter::Archived).unwrap()[0].id,
        id
    );
    assert_eq!(history.metadata(&id).unwrap()["summary"]["archived"], true);
    assert_eq!(history.read(&id).unwrap(), original);
    assert_eq!(directory.bytes(&id), bytes);
    assert_eq!(
        history.archive_change(Some(&id), true).unwrap()["changed"],
        false
    );
    assert!(history.resume(&id).unwrap_err().contains("archived"));

    let mut reopened = directory.history();
    assert_eq!(
        reopened.sessions_filtered(Filter::Archived).unwrap()[0].id,
        id
    );
    assert!(reopened.resume(&id).unwrap_err().contains("archived"));
    let filter = SearchFilter {
        query: "original".into(),
        id: None,
        role: None,
        case_sensitive: false,
    };
    assert_eq!(
        reopened.search(&filter).unwrap().matches[0].conversation_id,
        id
    );

    history
        .record("user", "A new conversation after archive")
        .unwrap();
    let next = history.current().unwrap().unwrap().metadata.id;
    assert_ne!(next, id);
    assert_eq!(directory.bytes(&id), bytes);
    let restored = history.archive_change(Some(&id), false).unwrap();
    assert_eq!(restored["changed"], true);
    assert_eq!(restored["archived"], false);
    assert_eq!(history.current().unwrap().unwrap().metadata.id, next);
    assert_eq!(
        history.archive_change(Some(&id), false).unwrap()["changed"],
        false
    );
    assert_eq!(directory.bytes(&id), bytes);
    assert_eq!(history.resume(&id).unwrap(), original);
    assert_eq!(history.sessions_filtered(Filter::All).unwrap().len(), 2);
    assert!(
        history
            .sessions_filtered(Filter::Archived)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn history_archive_respects_workspace_and_other_clients_active_lease() {
    let directory = Temporary::new();
    let mut owner = directory.history();
    owner
        .record("user", "Only this client writes this history")
        .unwrap();
    let id = owner.current().unwrap().unwrap().metadata.id;
    let before = directory.bytes(&id);
    let mut other = directory.history();
    assert!(
        other
            .archive_change(Some(&id), true)
            .unwrap_err()
            .contains("open in another client")
    );
    let mut foreign = History::with_workspace(
        directory.0.join("conversations"),
        "/another/workspace".into(),
    )
    .unwrap();
    assert!(
        foreign
            .archive_change(Some(&id), true)
            .unwrap_err()
            .contains("different workspace")
    );
    assert!(other.archive_change(Some("../escape"), true).is_err());
    assert_eq!(directory.bytes(&id), before);
    assert!(owner.archive_index().unwrap().conversations.is_empty());
    owner.archive_change(None, true).unwrap();
    assert_eq!(
        other.archive_change(Some(&id), false).unwrap()["archived"],
        false
    );
    assert_eq!(directory.bytes(&id), before);
}

#[test]
fn history_archive_retention_never_deletes_protected_logs_when_capacity_is_full() {
    let directory = Temporary::new();
    let mut history = directory.history();
    let mut protected = Vec::new();
    for index in 0..MAX_SESSIONS {
        history
            .record("user", &format!("Archived original {index}"))
            .unwrap();
        let id = history.current().unwrap().unwrap().metadata.id;
        let bytes = directory.bytes(&id);
        history.archive_change(None, true).unwrap();
        protected.push((id, bytes));
    }
    assert_eq!(
        history.sessions_filtered(Filter::Archived).unwrap().len(),
        MAX_SESSIONS
    );
    assert!(
        history
            .record("user", "Must not evict an archive")
            .err()
            .unwrap()
            .contains("archived sessions")
    );
    assert!(history.current().unwrap().is_none());
    for (id, bytes) in &protected {
        assert_eq!(&directory.bytes(id), bytes);
    }
    let restored = &protected[0].0;
    history.archive_change(Some(restored), false).unwrap();
    history
        .record("user", "Only unprotected history may now be reclaimed")
        .unwrap();
    for (id, bytes) in protected.iter().skip(1) {
        assert_eq!(&directory.bytes(id), bytes);
    }
    assert_eq!(
        history.sessions_filtered(Filter::Archived).unwrap().len(),
        MAX_SESSIONS - 1
    );
}

#[test]
fn history_archive_invalid_index_fails_without_releasing_or_modifying_current_history() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history.record("user", "Keep this exact history").unwrap();
    let id = history.current().unwrap().unwrap().metadata.id;
    let before = directory.bytes(&id);
    history
        .directory
        .write_atomic(INDEX_NAME, b"{invalid-json")
        .unwrap();
    assert!(history.archive_change(None, true).is_err());
    assert!(history.sessions().is_err());
    assert!(
        history
            .record("user", "Blocked by invalid archive state")
            .is_err()
    );
    assert_eq!(history.current().unwrap().unwrap().metadata.id, id);
    assert_eq!(directory.bytes(&id), before);
}

#[cfg(unix)]
#[test]
fn history_archive_rejects_linked_index_without_touching_the_target() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history.record("user", "Private history").unwrap();
    let id = history.current().unwrap().unwrap().metadata.id;
    let before = directory.bytes(&id);
    let external = directory.0.join("outside-index");
    fs::write(&external, b"must stay unchanged").unwrap();
    std::os::unix::fs::symlink(
        &external,
        directory.0.join("conversations").join(INDEX_NAME),
    )
    .unwrap();
    assert!(history.archive_change(None, true).is_err());
    assert_eq!(fs::read(&external).unwrap(), b"must stay unchanged");
    assert_eq!(directory.bytes(&id), before);
    assert_eq!(history.current().unwrap().unwrap().metadata.id, id);
}

#[test]
fn history_archive_tools_separate_read_only_discovery_from_explicit_mutation() {
    assert_eq!(super::super::tool()["annotations"]["readOnlyHint"], true);
    assert_eq!(tool()["annotations"]["readOnlyHint"], false);
    assert_eq!(tool()["annotations"]["destructiveHint"], false);
    assert!(serde_json::from_value::<Request>(json!({"action":"archive"})).is_err());
    assert!(
        serde_json::from_value::<Request>(
            json!({"action":"archive","id":"conversation_00000000000000000000000000000000","delete":true})
        ).is_err()
    );
    let request: super::super::Request = serde_json::from_value(json!({"action":"list"})).unwrap();
    assert!(matches!(
        request,
        super::super::Request::List {
            state: Filter::Active,
            ..
        }
    ));
    for state in ["active", "archived", "all"] {
        assert!(
            serde_json::from_value::<super::super::Request>(json!({"action":"list","state":state}))
                .is_ok()
        );
    }
    assert!(
        serde_json::from_value::<super::super::Request>(json!({"action":"list","state":"delete"}))
            .is_err()
    );
}
