use super::*;
use crate::client::interface::history::{MAX_SESSIONS, archive::Filter, tests::Temporary};
use std::fs;

fn update(history: &mut History, arguments: Value) -> Result<Value, String> {
    history.change_description(Request::parse(arguments)?)
}

#[test]
fn descriptions_reopen_and_partial_edits_preserve_jsonl_and_archive() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history.record("user", "Original automatic title").unwrap();
    history.record("assistant", "Original reply").unwrap();
    let conversation = history.current().unwrap().unwrap();
    let id = &conversation.metadata.id;
    let path = history.directory.path.join(session_name(id).unwrap());
    let jsonl = fs::read(&path).unwrap();
    history.archive_change(Some(id), true).unwrap();
    let archive_path = history.directory.path.join("archives.json");
    let archive_bytes = fs::read(&archive_path).unwrap();

    let result = update(
        &mut history,
        json!({"id":id,"title":"自定义标题","summary":"Saved summary"}),
    )
    .unwrap();
    assert_eq!(result["id"], id.as_str());
    assert_eq!(result["changed"], true);
    assert_eq!(result["contents_changed"], false);
    assert_eq!(result["summary"]["archived"], true);
    assert_eq!(result["summary"]["current"], false);

    let mut reopened = directory.history();
    assert!(reopened.current().unwrap().is_none());
    assert!(reopened.sessions().unwrap().is_empty());
    let listed = reopened.sessions_filtered(Filter::Archived).unwrap();
    assert_eq!(listed[0].id, *id);
    assert_eq!(listed[0].title, "自定义标题");
    assert_eq!(listed[0].summary.as_deref(), Some("Saved summary"));
    assert_eq!(listed[0].message_count, 2);
    assert_eq!(reopened.read(id).unwrap(), conversation);
    let metadata = reopened.metadata(id).unwrap();
    assert_eq!(metadata["metadata"], json!(conversation.metadata));
    assert_eq!(metadata["summary"], json!(listed[0]));

    let edit = json!({"id":id,"summary":"Partial change"});
    let result = update(&mut reopened, edit.clone()).unwrap();
    assert_eq!(result["summary"]["title"], "自定义标题");
    assert_eq!(result["summary"]["summary"], "Partial change");
    let sidecar = history.directory.path.join(name(id).unwrap());
    let changed_bytes = fs::read(&sidecar).unwrap();
    assert_eq!(update(&mut reopened, edit).unwrap()["changed"], false);
    assert_eq!(fs::read(&sidecar).unwrap(), changed_bytes);

    let result = update(&mut reopened, json!({"id":id,"title":""})).unwrap();
    assert_eq!(result["summary"]["title"], "Original automatic title");
    assert_eq!(result["summary"]["summary"], "Partial change");
    let result = update(&mut reopened, json!({"id":id,"summary":""})).unwrap();
    assert!(result["summary"].get("summary").is_none());
    assert_eq!(
        update(&mut reopened, json!({"id":id,"summary":""})).unwrap()["changed"],
        false
    );
    assert_eq!(fs::read(&path).unwrap(), jsonl);
    assert_eq!(fs::read(&archive_path).unwrap(), archive_bytes);
    assert!(reopened.current().unwrap().is_none());
}

#[test]
fn description_updates_require_workspace_and_active_lease_and_foreign_sidecars_are_ignored() {
    let directory = Temporary::new();
    let mut owner = directory.history();
    owner.record("user", "Visible own history").unwrap();
    let id = owner.current().unwrap().unwrap().metadata.id;
    let request = json!({"id":id,"title":"Own description"});
    let mut other = directory.history();
    assert!(
        update(&mut other, request.clone())
            .unwrap_err()
            .contains("open in another client")
    );

    let mut foreign =
        History::with_workspace(owner.directory.path.clone(), "/foreign".into()).unwrap();
    foreign.record("user", "Foreign history").unwrap();
    let foreign_id = foreign.current().unwrap().unwrap().metadata.id;
    update(
        &mut foreign,
        json!({"id":foreign_id,"summary":"Foreign summary"}),
    )
    .unwrap();
    assert!(
        update(&mut foreign, request.clone())
            .unwrap_err()
            .contains("different workspace")
    );
    assert!(
        update(&mut owner, json!({"id":foreign_id,"title":"Rejected"}))
            .unwrap_err()
            .contains("different workspace")
    );
    update(&mut owner, request).unwrap();
    let listed = owner.sessions().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].title, "Own description");

    let foreign_description = json!({"schema_version":1,"id":id,"workspace":"/foreign",
        "title":"Foreign title","summary":"Foreign summary"});
    owner
        .directory
        .write_atomic(
            &name(&id).unwrap(),
            &serde_json::to_vec(&foreign_description).unwrap(),
        )
        .unwrap();
    let listed = owner.sessions().unwrap();
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].title, "Visible own history");
    assert!(listed[0].summary.is_none());
    assert_eq!(
        owner.metadata(&id).unwrap()["summary"]["title"],
        "Visible own history"
    );
    assert!(
        update(&mut owner, json!({"id":id,"summary":"Rejected"}))
            .unwrap_err()
            .contains("workspace does not match")
    );
}

#[test]
fn descriptions_validate_exact_ids_utf8_bytes_and_redact_before_storage() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history.record("user", "Original").unwrap();
    let id = history.current().unwrap().unwrap().metadata.id;
    for args in [
        json!({"title":"Missing id"}),
        json!({"id":id,"title":null,"summary":"Ambiguous null"}),
        json!({"id":id,"summary":null}),
        json!({"id":id}),
        json!({"id":id,"summary":"Unknown field","text":"invalid"}),
        json!({"id":id,"title":true}),
        json!({"id":"../outside","title":"Wrong ID"}),
        json!({"id":&id[..id.len() - 1],"title":"Prefix"}),
        json!({"id":"conversation_ffffffffffffffffffffffffffffffff","title":"Unknown ID"}),
        json!({"id":id,"title":"bad\u{001b}[2J"}),
        json!({"id":id,"summary":"bad\u{0000}"}),
        json!({"id":id,"summary":"bad\nnewline"}),
        json!({"id":id,"title":"界".repeat(86)}),
        json!({"id":id,"summary":"界".repeat(683)}),
    ] {
        assert!(
            update(&mut history, args.clone()).is_err(),
            "accepted {args}"
        );
    }
    let sidecar = history.directory.path.join(name(&id).unwrap());
    assert!(!sidecar.exists());
    let title = format!("{}a", "界".repeat(85));
    let summary = format!("{}ab", "界".repeat(682));
    update(
        &mut history,
        json!({"id":id,"title":title,"summary":summary}),
    )
    .unwrap();
    let reopened = directory.history().metadata(&id).unwrap();
    assert_eq!(reopened["summary"]["title"], title);
    assert_eq!(reopened["summary"]["summary"], summary);

    let expands = format!(
        "{} password=x",
        "a".repeat(MAX_TITLE_BYTES - " password=x".len())
    );
    let edit = json!({"id":id,"title":expands,"summary":"API_TOKEN=private_fixture_secret"});
    assert_eq!(update(&mut history, edit.clone()).unwrap()["changed"], true);
    let bytes = fs::read_to_string(&sidecar).unwrap();
    assert!(!bytes.contains("private_fixture_secret"));
    assert!(!bytes.contains("password=x"));
    assert!(bytes.len() <= MAX_DESCRIPTION_BYTES);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let reopened = directory.history().metadata(&id).unwrap();
    assert_eq!(reopened["summary"]["summary"], "API_TOKEN=[redacted]");
    assert!(reopened["summary"]["title"].as_str().unwrap().len() <= MAX_TITLE_BYTES);
    assert_eq!(update(&mut history, edit).unwrap()["changed"], false);
    assert_eq!(fs::read_to_string(&sidecar).unwrap(), bytes);
}

#[test]
fn normal_retention_reclaims_the_description_with_its_jsonl() {
    let directory = Temporary::new();
    let mut history = directory.history();
    history.record("user", "Described oldest history").unwrap();
    let mut newer = history.current().unwrap().unwrap();
    let id = newer.metadata.id.clone();
    update(
        &mut history,
        json!({"id":id,"summary":"Reclaim with this history"}),
    )
    .unwrap();
    let sidecar = history.directory.path.join(name(&id).unwrap());
    assert!(sidecar.is_file());
    drop(history);

    let mut history = directory.history();
    newer.metadata.created_at_ms = u64::MAX;
    newer.messages[0].created_at_ms = u64::MAX;
    for index in 0..MAX_SESSIONS - 1 {
        newer.metadata.id = format!("conversation_{index:032x}");
        history
            .directory
            .write_atomic(
                &session_name(&newer.metadata.id).unwrap(),
                &History::encode(&newer).unwrap(),
            )
            .unwrap();
    }
    history.record("user", "Triggers normal retention").unwrap();
    assert_eq!(history.sessions().unwrap().len(), MAX_SESSIONS);
    assert!(history.read(&id).is_err());
    assert!(!sidecar.exists());
}
