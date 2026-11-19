use super::*;
use crate::{
    broker::{self, LOGIN_LIFETIME},
    history::AuditConfig,
    wire::{self, ClientMetadata, Frame, Operation},
};
use std::{
    collections::HashMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{io::BufReader, net::UnixStream, time::timeout};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-management-broker-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn identity(subject: &str) -> Identity {
    Identity {
        provider: "github".into(),
        subject: subject.into(),
        login: format!("user-{subject}"),
    }
}
fn configure(temp: &Temp, owners: HashMap<String, String>) -> Broker {
    let mut broker = Broker::new("https://example.test".into());
    broker.configure_management(
        ManagementPolicy::new(vec!["999".into()], owners).unwrap(),
        Arc::new(AuditLog::open(&temp.0.join("audit"), AuditConfig::default()).unwrap()),
    );
    broker
}

#[tokio::test]
async fn allowed_tool_access_cannot_list_or_disconnect_another_owners_connection() {
    let temp = Temp::new();
    let broker = configure(&temp, HashMap::new());
    let mut foreign = broker.test_register("gh-456", "secret").await;
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-456")
        .unwrap()
        .remove(foreign.client_id());
    Arc::get_mut(&mut foreign)
        .unwrap()
        .allowed()
        .push(identity("123"));
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-456")
        .unwrap()
        .insert(foreign.client_id().into(), foreign.clone());
    let own = broker.test_register("gh-123", "secret").await;
    let accessible = broker
        .clients(
            broker::IDENTITY_RESOURCE,
            &broker::Authorization::Identity(identity("123")),
        )
        .await
        .unwrap();
    assert_eq!(accessible.len(), 1);
    assert_eq!(accessible[0].client_id(), foreign.client_id());
    let personal = broker
        .management_snapshot(&identity("123"), Scope::Own)
        .await
        .unwrap();
    assert_eq!(personal.sessions.len(), 1);
    assert_eq!(personal.sessions[0].id, own.client_id());
    assert_eq!(
        broker
            .management_disconnect(&identity("123"), foreign.client_id())
            .await,
        Err(ManagementError::NotFound)
    );
    assert!(foreign.is_alive());
    assert_eq!(
        broker
            .management_snapshot(&identity("123"), Scope::All)
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    let admin = broker
        .management_snapshot(&identity("999"), Scope::All)
        .await
        .unwrap();
    assert_eq!(admin.sessions.len(), 2);
    assert_eq!(
        admin
            .sessions
            .iter()
            .find(|s| s.id == foreign.client_id())
            .unwrap()
            .allowed_github[0]
            .subject,
        "123"
    );
    let mut impostor = identity("999");
    impostor.provider = "other".into();
    assert_eq!(
        broker
            .management_snapshot(&impostor, Scope::All)
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
}

#[tokio::test]
async fn disconnect_closes_live_wire_and_records_metadata_without_arguments_or_results() {
    let temp = Temp::new();
    // SAFETY: geteuid has no memory preconditions.
    let ssh_user = broker::username(unsafe { libc::geteuid() }).unwrap();
    let broker = Arc::new(configure(
        &temp,
        HashMap::from([(ssh_user.clone(), "123".into())]),
    ));
    let (server, client) = UnixStream::pair().unwrap();
    let server_task = tokio::spawn(broker.clone().attach(server));
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    wire::write_frame(
        &mut writer,
        &Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: ClientMetadata {
                name: "real connection".into(),
                title: None,
                cwd: "/test".into(),
                hostname: "test-host".into(),
                allow: Vec::new(),
            },
        },
    )
    .await
    .unwrap();
    let Some(Frame::Ready {
        client_id,
        login_expires_in,
        ..
    }) = timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
        .await
        .unwrap()
        .unwrap()
    else {
        panic!("missing ready");
    };
    assert_eq!(login_expires_in, LOGIN_LIFETIME);
    let snapshot = broker
        .management_snapshot(&identity("123"), Scope::Own)
        .await
        .unwrap();
    assert_eq!(snapshot.sessions[0].id, client_id);
    assert_eq!(snapshot.sessions[0].owner_subject.as_deref(), Some("123"));
    assert!(snapshot.sessions[0].connected_at > 0);
    assert_eq!(
        broker
            .management_disconnect(&identity("456"), &client_id)
            .await,
        Err(ManagementError::NotFound)
    );
    let session = broker.online_session(&ssh_user).await.unwrap();
    let request = tokio::spawn(async move {
        session
            .call(Operation::CallTool {
                name: "terminal.exec".into(),
                arguments: serde_json::json!({"command":"private-argument-never-store"}),
            })
            .await
    });
    let Some(Frame::Call { id, .. }) =
        timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
    else {
        panic!("missing call");
    };
    wire::write_frame(
        &mut writer,
        &Frame::Reply {
            id,
            result: Ok(serde_json::json!({"isError":false,"output":"private-result-never-store"})),
        },
    )
    .await
    .unwrap();
    request.await.unwrap().unwrap();
    broker
        .management_disconnect(&identity("123"), &client_id)
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    timeout(Duration::from_secs(2), server_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        broker
            .management_snapshot(&identity("123"), Scope::Own)
            .await
            .unwrap()
            .sessions
            .is_empty()
    );
    assert!(broker.online_session(&ssh_user).await.is_none());
    let page = broker
        .management_audit(&identity("123"), Scope::Own, 200, None)
        .await
        .unwrap();
    for action in [
        AuditAction::SessionConnected,
        AuditAction::ToolCall,
        AuditAction::SessionDisconnectRequested,
        AuditAction::SessionDisconnected,
    ] {
        assert!(
            page.events.iter().any(|event| event.action == action),
            "missing {action:?}"
        );
    }
    let tool = page
        .events
        .iter()
        .find(|event| event.action == AuditAction::ToolCall)
        .unwrap();
    assert_eq!(tool.tool_name.as_deref(), Some("terminal.exec"));
    let persisted = fs::read_to_string(temp.0.join("audit/audit.jsonl")).unwrap();
    assert!(!persisted.contains("private-argument-never-store"));
    assert!(!persisted.contains("private-result-never-store"));
}

#[tokio::test]
async fn verified_callers_are_audited_separately_from_session_owners_and_controller_claims() {
    use crate::wire::{Controller, ControllerSource};
    let temp = Temp::new();
    let broker = configure(&temp, HashMap::new());
    let mut session = broker.test_register("gh-123", "secret").await;
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-123")
        .unwrap()
        .remove(session.client_id());
    let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
    let mutable = Arc::get_mut(&mut session).unwrap();
    mutable.allow = std::sync::Mutex::new(vec![identity("456"), identity("789")]);
    mutable.sender = sender;
    mutable.slots = tokio::sync::Semaphore::new(4);
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-123")
        .unwrap()
        .insert(session.client_id().into(), session.clone());
    let mut tasks = Vec::new();
    for (subject, name) in [("456", "tool.a"), ("789", "tool.b")] {
        let authorization = broker::Authorization::Identity(identity(subject));
        let permitted = broker
            .client(
                broker::IDENTITY_RESOURCE,
                &authorization,
                session.client_id(),
            )
            .await
            .unwrap();
        tasks.push(tokio::spawn(async move {
            permitted.call_with_authorization(
                Operation::CallTool {
                    name: name.into(),
                    arguments: serde_json::json!({"actor_subject":"999", "provider":"github", "command":"private-command"}),
                },
                Some(Controller { name: Some("GitHub administrator 999".into()), model: None, source: ControllerSource::CallerReported }),
                &authorization,
            ).await
        }));
    }
    for _ in 0..2 {
        let call = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        call.reply.send(Ok(serde_json::json!({"ok":true}))).unwrap();
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let owner = broker
        .management_audit(&identity("123"), Scope::Own, 50, None)
        .await
        .unwrap();
    assert_eq!(owner.events.len(), 4);
    for (subject, tool) in [("456", "tool.a"), ("789", "tool.b")] {
        let actor = broker
            .management_audit(&identity(subject), Scope::Own, 50, None)
            .await
            .unwrap();
        assert_eq!(actor.events.len(), 2);
        assert_eq!(actor.events[0].actor_subject.as_deref(), Some(subject));
        assert_eq!(actor.events[0].owner_subject.as_deref(), Some("123"));
        assert_eq!(actor.events[0].tool_name.as_deref(), Some(tool));
        assert!(
            broker
                .management_snapshot(&identity(subject), Scope::Own)
                .await
                .unwrap()
                .sessions
                .is_empty()
        );
    }
    assert!(
        broker
            .management_audit(&identity("321"), Scope::Own, 50, None)
            .await
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        broker
            .client(
                broker::IDENTITY_RESOURCE,
                &broker::Authorization::Identity(identity("321")),
                session.client_id()
            )
            .await
            .is_err()
    );
    assert_eq!(
        broker
            .management_record(
                Some(&identity("456")),
                AuditAction::ToolCall,
                AuditOutcome::Success
            )
            .await,
        Err(ManagementError::Forbidden)
    );
    assert_eq!(
        broker
            .management_disconnect(&identity("456"), session.client_id())
            .await,
        Err(ManagementError::NotFound)
    );
    assert!(session.is_alive());
    let persisted = fs::read_to_string(temp.0.join("audit/audit.jsonl")).unwrap();
    assert!(!persisted.contains("GitHub administrator 999"));
    assert!(!persisted.contains("private-command"));
}

async fn queued_session(
    broker: &Broker,
) -> (
    Arc<Session>,
    tokio::sync::mpsc::Receiver<crate::broker::Call>,
) {
    let mut session = broker.test_register("gh-123", "secret").await;
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-123")
        .unwrap()
        .remove(session.client_id());
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    Arc::get_mut(&mut session).unwrap().sender = sender;
    broker
        .sessions
        .write()
        .await
        .get_mut("gh-123")
        .unwrap()
        .insert(session.client_id().into(), session.clone());
    (session, receiver)
}

fn small_journal(temp: &Temp) -> Broker {
    let mut broker = Broker::new("https://example.test".into());
    broker.configure_management(
        ManagementPolicy::default(),
        Arc::new(
            AuditLog::open(
                &temp.0.join("audit"),
                AuditConfig {
                    retention_days: 7,
                    max_events: 1,
                },
            )
            .unwrap(),
        ),
    );
    broker
}

fn fail_next_compaction(temp: &Temp) {
    // Preserve the existing durable record while causing the next atomic
    // replacement to fail with a real filesystem error.
    fs::rename(
        temp.0.join("audit/audit.jsonl"),
        temp.0.join("audit/pending-saved.jsonl"),
    )
    .unwrap();
    fs::create_dir(temp.0.join("audit/audit.jsonl")).unwrap();
}

#[tokio::test]
async fn audit_failure_prevents_tool_dispatch_but_never_blocks_internal_cleanup() {
    let temp = Temp::new();
    let broker = small_journal(&temp);
    broker
        .management
        .as_ref()
        .unwrap()
        .audit
        .record(AuditEvent::new(AuditAction::Login, AuditOutcome::Success))
        .unwrap();
    fail_next_compaction(&temp);
    let (session, mut receiver) = queued_session(&broker).await;
    let error = session
        .call(Operation::CallTool {
            name: "terminal.exec".into(),
            arguments: serde_json::json!({"command":"must-not-execute"}),
        })
        .await
        .unwrap_err();
    assert!(error.contains("tool was not dispatched"));
    assert!(matches!(
        receiver.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let cleanup = tokio::spawn(async move {
        session
            .call_cleanup(Operation::CallTool {
                name: "ssmcp.net.close".into(),
                arguments: serde_json::json!({"id":"owned-handle"}),
            })
            .await
    });
    let call = timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    let Operation::CallTool { name, .. } = &call.operation else {
        panic!("wrong operation");
    };
    assert_eq!(name, "ssmcp.net.close");
    call.reply
        .send(Ok(serde_json::json!({"closed":true})))
        .unwrap();
    cleanup.await.unwrap().unwrap();
}

#[tokio::test]
async fn completion_audit_failure_reports_uncertain_execution_and_preserves_intent() {
    let temp = Temp::new();
    let broker = small_journal(&temp);
    let (session, mut receiver) = queued_session(&broker).await;
    session.allowed().push(identity("456"));
    let call = tokio::spawn(async move {
        session
            .call_with_authorization(
                Operation::CallTool {
                    name: "terminal.exec".into(),
                    arguments: serde_json::json!({"command":"private-command"}),
                },
                None,
                &broker::Authorization::Identity(identity("456")),
            )
            .await
    });
    let request = timeout(Duration::from_secs(2), receiver.recv())
        .await
        .unwrap()
        .unwrap();
    fail_next_compaction(&temp);
    request
        .reply
        .send(Ok(
            serde_json::json!({"isError":false,"output":"completed"}),
        ))
        .unwrap();
    let error = call.await.unwrap().unwrap_err();
    assert!(error.contains("tool may have executed"));
    assert!(error.contains("audit completion could not be saved"));
    let persisted = fs::read_to_string(temp.0.join("audit/pending-saved.jsonl")).unwrap();
    assert!(persisted.contains("\"outcome\":\"pending\""));
    assert!(!persisted.contains("\"outcome\":\"success\""));
    assert!(!persisted.contains("private-command"));
}

#[tokio::test]
async fn failed_connect_audit_never_sends_ready_or_leaves_a_registered_session() {
    let temp = Temp::new();
    let broker = small_journal(&temp);
    broker
        .management
        .as_ref()
        .unwrap()
        .audit
        .record(AuditEvent::new(AuditAction::Login, AuditOutcome::Success))
        .unwrap();
    fail_next_compaction(&temp);
    let broker = Arc::new(broker);
    let (server, client) = UnixStream::pair().unwrap();
    let task = tokio::spawn(broker.clone().attach(server));
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    wire::write_frame(
        &mut writer,
        &Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: ClientMetadata {
                name: "refused".into(),
                title: None,
                cwd: "/test".into(),
                hostname: "host".into(),
                allow: vec![],
            },
        },
    )
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert!(
        timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(broker.sessions.read().await.is_empty());
}

#[tokio::test]
async fn graceful_shutdown_drains_wire_disconnect_history_before_task_abort() {
    let temp = Temp::new();
    // SAFETY: geteuid has no memory preconditions.
    let user = broker::username(unsafe { libc::geteuid() }).unwrap();
    let broker = Arc::new(configure(&temp, HashMap::from([(user, "123".into())])));
    let (server, client) = UnixStream::pair().unwrap();
    let task = tokio::spawn(broker.clone().attach(server));
    let (reader, mut writer) = client.into_split();
    let mut reader = BufReader::new(reader);
    wire::write_frame(
        &mut writer,
        &Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: ClientMetadata {
                name: "shutdown".into(),
                title: None,
                cwd: "/test".into(),
                hostname: "host".into(),
                allow: vec![],
            },
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap(),
        Some(Frame::Ready { .. })
    ));
    broker.disconnect_all().await;
    let page = broker
        .management_audit(&identity("123"), Scope::Own, 20, None)
        .await
        .unwrap();
    assert!(
        page.events
            .iter()
            .any(|event| event.action == AuditAction::SessionDisconnected
                && event.outcome == AuditOutcome::Success)
    );
    assert!(
        timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
