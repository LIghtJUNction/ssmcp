use super::*;
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::{AtomicU64, Ordering},
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ssmcp-history-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn event(actor: Option<&str>, owner: Option<&str>) -> AuditEvent {
    let mut event = AuditEvent::new(AuditAction::ToolCall, AuditOutcome::Success);
    event.actor_subject = actor.map(str::to_owned);
    event.owner_subject = owner.map(str::to_owned);
    event.session_id = Some("session-0123".into());
    event.tool_name = Some("ssmcp.exec.start".into());
    event
}

#[test]
fn configuration_rejects_unbounded_retention_and_storage() {
    AuditConfig::default().validate().unwrap();
    for config in [
        AuditConfig {
            retention_days: 0,
            max_events: 20_000,
        },
        AuditConfig {
            retention_days: 31,
            max_events: 20_000,
        },
        AuditConfig {
            retention_days: 7,
            max_events: 0,
        },
        AuditConfig {
            retention_days: 7,
            max_events: 100_001,
        },
    ] {
        assert!(config.validate().is_err());
    }
}

#[test]
fn persistence_overwrites_client_cursors_and_filters_actor_or_owner_exactly() {
    let temp = Temp::new();
    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    for input in [
        event(None, Some("11")),
        event(Some("22"), Some("11")),
        event(Some("33"), Some("44")),
        event(Some("11"), Some("33")),
    ] {
        log.record(input).unwrap();
    }
    let page = log.query(Some("11"), 2, None).unwrap();
    assert_eq!(page.events.iter().map(|e| e.id).collect::<Vec<_>>(), [4, 2]);
    assert_eq!(page.next_before, Some(2));
    let older = log.query(Some("11"), 2, page.next_before).unwrap();
    assert_eq!(older.events.iter().map(|e| e.id).collect::<Vec<_>>(), [1]);
    assert_eq!(older.next_before, None);
    assert!(log.query(Some("99"), 200, None).unwrap().events.is_empty());
    assert_eq!(log.query(None, 200, None).unwrap().events.len(), 4);
    assert!(log.query(Some("011"), 200, None).is_err());
    assert!(log.query(None, 0, None).is_err());
    assert!(log.query(None, 201, None).is_err());
    drop(log);

    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    let mut input = event(Some("11"), None);
    input.id = u64::MAX;
    input.at_unix = 1;
    let stored = log.record(input).unwrap();
    assert_eq!(stored.id, 5);
    assert!(stored.at_unix >= now().unwrap() - 1);
    assert_eq!(log.query(Some("11"), 1, None).unwrap().events[0].id, 5);
}

#[test]
fn startup_query_write_and_idle_prune_physically_remove_expired_events() {
    let config = AuditConfig {
        retention_days: 1,
        max_events: 100,
    };
    let timestamp = now().unwrap();
    for trigger in ["startup", "query", "write", "idle"] {
        let temp = Temp::new();
        let log = AuditLog::open(&temp.0, config).unwrap();
        log.record_at(event(Some("11"), None), timestamp - 86_401)
            .unwrap();
        assert!(
            fs::read_to_string(temp.0.join(JOURNAL))
                .unwrap()
                .contains("actor_subject")
        );
        let log = match trigger {
            "startup" => {
                drop(log);
                AuditLog::open(&temp.0, config).unwrap()
            }
            "query" => {
                assert!(log.query(Some("11"), 200, None).unwrap().events.is_empty());
                log
            }
            "write" => {
                log.record(event(Some("22"), None)).unwrap();
                log
            }
            "idle" => {
                assert_eq!(log.prune().unwrap(), 1);
                log
            }
            _ => unreachable!(),
        };
        let text = fs::read_to_string(temp.0.join(JOURNAL)).unwrap();
        assert!(
            !text.contains("\"actor_subject\":\"11\""),
            "{trigger} left old disk data"
        );
        assert!(log.query(Some("11"), 200, None).unwrap().events.is_empty());
        let saved = log.record(event(Some("33"), None)).unwrap();
        assert!(saved.id >= 2, "pruning must not reuse an old cursor");
    }
}

#[test]
fn retention_boundary_and_clock_order_do_not_resurrect_history() {
    let temp = Temp::new();
    let log = AuditLog::open(
        &temp.0,
        AuditConfig {
            retention_days: 1,
            max_events: 100,
        },
    )
    .unwrap();
    let timestamp = now().unwrap();
    log.record_at(event(Some("11"), None), timestamp).unwrap();
    log.record_at(event(Some("22"), None), timestamp - 100)
        .unwrap();
    assert_eq!(log.prune_at(timestamp + 86_300).unwrap(), 1);
    let state = log.lock().unwrap();
    assert_eq!(state.events.len(), 1);
    assert_eq!(state.events[0].actor_subject.as_deref(), Some("11"));
    drop(state);
    assert_eq!(log.prune_at(timestamp + 86_400).unwrap(), 1);
    assert_eq!(
        fs::read_to_string(temp.0.join(JOURNAL))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

#[test]
fn count_bound_compacts_and_survives_restart() {
    let temp = Temp::new();
    let config = AuditConfig {
        retention_days: 7,
        max_events: 3,
    };
    let log = AuditLog::open(&temp.0, config).unwrap();
    for _ in 0..6 {
        log.record(event(Some("11"), None)).unwrap();
    }
    assert_eq!(
        log.query(None, 200, None)
            .unwrap()
            .events
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        [6, 5, 4]
    );
    assert_eq!(
        fs::read_to_string(temp.0.join(JOURNAL))
            .unwrap()
            .lines()
            .count(),
        4
    );
    drop(log);
    let log = AuditLog::open(&temp.0, config).unwrap();
    assert_eq!(log.record(event(Some("11"), None)).unwrap().id, 7);
    assert_eq!(log.query(None, 200, None).unwrap().events.len(), 3);
}

#[test]
fn journal_byte_bound_reclaims_a_batch_before_appending() {
    let temp = Temp::new();
    let config = AuditConfig {
        retention_days: 7,
        max_events: MAX_EVENTS,
    };
    drop(AuditLog::open(&temp.0, config).unwrap());
    let mut raw = header_bytes(1).unwrap();
    let timestamp = now().unwrap();
    let mut payload = event(Some("11"), Some("22"));
    payload.client_name = Some("n".repeat(256));
    payload.hostname = Some("h".repeat(256));
    payload.tool_name = Some("t".repeat(256));
    payload.session_id = Some("s".repeat(128));
    payload.at_unix = timestamp;
    let mut final_id = 0;
    loop {
        payload.id = final_id + 1;
        let bytes = event_bytes(&payload).unwrap();
        if raw.len() as u64 + bytes.len() as u64 > MAX_JOURNAL_BYTES {
            break;
        }
        raw.extend_from_slice(&bytes);
        final_id += 1;
    }
    fs::write(temp.0.join(JOURNAL), raw).unwrap();
    let log = AuditLog::open(&temp.0, config).unwrap();
    let appended = log.record(payload).unwrap();
    assert_eq!(appended.id, final_id + 1);
    assert!(fs::metadata(temp.0.join(JOURNAL)).unwrap().len() <= MAX_JOURNAL_BYTES);
    assert!(log.lock().unwrap().events.front().unwrap().id > 1);
    drop(log);
    let reopened = AuditLog::open(&temp.0, config).unwrap();
    assert_eq!(
        reopened.query(None, 1, None).unwrap().events[0].id,
        appended.id
    );
}

#[test]
fn private_permissions_single_writer_and_links_are_enforced() {
    let temp = Temp::new();
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
    fs::set_permissions(&temp.0, fs::Permissions::from_mode(0o700)).unwrap();
    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    assert_eq!(
        fs::metadata(temp.0.join(JOURNAL)).unwrap().mode() & 0o777,
        0o600
    );
    assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
    drop(log);
    fs::set_permissions(temp.0.join(JOURNAL), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
    fs::set_permissions(temp.0.join(JOURNAL), fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(temp.0.join(JOURNAL), temp.0.join("second-link")).unwrap();
    assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
    fs::remove_file(temp.0.join("second-link")).unwrap();
    fs::remove_file(temp.0.join(JOURNAL)).unwrap();
    fs::write(temp.0.join("unrelated"), b"unchanged").unwrap();
    symlink(temp.0.join("unrelated"), temp.0.join(JOURNAL)).unwrap();
    assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
    assert_eq!(fs::read(temp.0.join("unrelated")).unwrap(), b"unchanged");
}

#[test]
fn schema_rejects_secrets_freeform_arguments_and_forged_identity_metadata() {
    let temp = Temp::new();
    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    let mut input = event(Some("01"), None);
    assert!(log.record(input.clone()).is_err());
    input.actor_subject = Some("11".into());
    input.tool_name = Some("shell --token secret".into());
    assert!(log.record(input.clone()).is_err());
    input.tool_name = Some("terminal.status".into());
    input.client_name = Some("name\nforged event".into());
    assert!(log.record(input.clone()).is_err());
    input.client_name = None;
    let mut serialized = serde_json::to_value(&input).unwrap();
    serialized["arguments"] = serde_json::json!({"token":"secret"});
    assert!(serde_json::from_value::<AuditEvent>(serialized).is_err());
    assert!(log.query(None, 200, None).unwrap().events.is_empty());
}

#[test]
fn corrupted_or_truncated_journal_is_not_silently_discarded() {
    for suffix in [b"invalid\n".as_slice(), b"{".as_slice()] {
        let temp = Temp::new();
        let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
        log.record(event(Some("11"), None)).unwrap();
        drop(log);
        let path = temp.0.join(JOURNAL);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(suffix)
            .unwrap();
        let before = fs::read(&path).unwrap();
        assert!(AuditLog::open(&temp.0, AuditConfig::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn restart_removes_owned_orphan_compactions_without_touching_unrelated_files() {
    let temp = Temp::new();
    drop(AuditLog::open(&temp.0, AuditConfig::default()).unwrap());
    let orphan = temp.0.join(".audit-0123456789abcdef0123456789abcdef.tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&orphan)
        .unwrap();
    file.write_all(b"expired audit metadata").unwrap();
    fs::write(temp.0.join("notes.txt"), b"unrelated").unwrap();
    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    assert!(!orphan.exists());
    assert_eq!(fs::read(temp.0.join("notes.txt")).unwrap(), b"unrelated");
    assert!(log.query(None, 200, None).unwrap().events.is_empty());
}

#[test]
fn pending_action_is_durable_before_completion() {
    let temp = Temp::new();
    let log = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    let mut requested = AuditEvent::new(AuditAction::KeyAdded, AuditOutcome::Pending);
    requested.actor_subject = Some("11".into());
    requested.owner_subject = Some("11".into());
    let saved = log.record(requested).unwrap();
    drop(log);
    let reopened = AuditLog::open(&temp.0, AuditConfig::default()).unwrap();
    let page = reopened.query(Some("11"), 200, None).unwrap();
    assert_eq!(page.events[0].id, saved.id);
    assert_eq!(page.events[0].outcome, AuditOutcome::Pending);
    let mut completed = AuditEvent::new(AuditAction::KeyAdded, AuditOutcome::Success);
    completed.actor_subject = Some("11".into());
    completed.owner_subject = Some("11".into());
    reopened.record(completed).unwrap();
    let page = reopened.query(Some("11"), 200, None).unwrap();
    assert_eq!(
        page.events
            .iter()
            .map(|event| event.outcome)
            .collect::<Vec<_>>(),
        [AuditOutcome::Success, AuditOutcome::Pending]
    );
}
