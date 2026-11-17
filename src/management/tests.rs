use super::*;
use crate::{history::AuditConfig, identity::Identity};
use std::{fs, os::unix::fs::PermissionsExt};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-management-{}-{}",
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
fn identity(subject: &str, login: &str) -> Identity {
    Identity {
        provider: "github".into(),
        subject: subject.into(),
        login: login.into(),
    }
}

#[test]
fn ownership_never_comes_from_access_allowlists_and_subjects_survive_renames() {
    let policy = ManagementPolicy::new(
        vec!["123".into()],
        HashMap::from([("legacy".into(), "456".into())]),
    )
    .unwrap();
    assert!(policy.is_admin(&identity("123", "renamed")));
    assert!(!policy.is_admin(&identity("789", "original-admin-login")));
    assert_eq!(policy.owner_subject("gh-123").as_deref(), Some("123"));
    assert_eq!(policy.owner_subject("legacy").as_deref(), Some("456"));
    assert_eq!(policy.owner_subject("unmapped"), None);
    assert_eq!(policy.owner_subject("gh-00123"), None);
    for bad in ["0", "01", "-1", "alice", "18446744073709551616"] {
        assert!(ManagementPolicy::new(vec![bad.into()], HashMap::new()).is_err());
    }
    assert!(
        ManagementPolicy::new(vec![], HashMap::from([("gh-123".into(), "456".into())])).is_err()
    );
}

#[tokio::test]
async fn private_socket_enforces_scope_record_types_and_file_permissions() {
    let temp = Temp::new();
    let audit = Arc::new(AuditLog::open(&temp.0.join("audit"), AuditConfig::default()).unwrap());
    let mut broker = Broker::new("https://example.test".into());
    broker.configure_management(
        ManagementPolicy::new(vec!["999".into()], HashMap::new()).unwrap(),
        audit,
    );
    broker.test_register("gh-123", "one").await;
    broker.test_register("gh-456", "two").await;
    let broker = Arc::new(broker);
    let socket = temp.0.join("admin.sock");
    let server = ManagementServer::bind(&socket).await.unwrap();
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(ManagementServer::bind(&socket).await.is_err());
    let task = tokio::spawn(server.run(broker));
    let client = ManagementClient::new(socket.clone());
    let own = client
        .snapshot(&identity("123", "alice"), Scope::Own)
        .await
        .unwrap();
    assert!(!own.is_admin);
    assert_eq!(own.sessions.len(), 1);
    assert_eq!(own.sessions[0].ssh_user, "gh-123");
    assert_eq!(
        client
            .snapshot(&identity("123", "alice"), Scope::All)
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    let all = client
        .snapshot(&identity("999", "renamed-admin"), Scope::All)
        .await
        .unwrap();
    assert!(all.is_admin);
    assert_eq!(all.sessions.len(), 2);
    client
        .record_audit(
            &identity("123", "alice"),
            AuditAction::Login,
            AuditOutcome::Success,
        )
        .await
        .unwrap();
    client
        .record_audit(
            &identity("456", "bob"),
            AuditAction::KeyAdded,
            AuditOutcome::Success,
        )
        .await
        .unwrap();
    client
        .record_anonymous_audit(AuditAction::AccessDenied, AuditOutcome::Denied)
        .await
        .unwrap();
    assert_eq!(
        client
            .record_anonymous_audit(AuditAction::Login, AuditOutcome::Success)
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    assert_eq!(
        client
            .record_audit(
                &identity("123", "alice"),
                AuditAction::ToolCall,
                AuditOutcome::Success
            )
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    assert_eq!(
        client
            .record_audit(
                &identity("123", "alice"),
                AuditAction::AdminViewed,
                AuditOutcome::Success
            )
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    let page = client
        .audit(&identity("123", "alice"), Scope::Own, 20, None)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].actor_subject.as_deref(), Some("123"));
    assert_eq!(
        client
            .audit(&identity("123", "alice"), Scope::All, 20, None)
            .await
            .unwrap_err(),
        ManagementError::Forbidden
    );
    assert_eq!(
        client
            .audit(&identity("999", "admin"), Scope::All, 201, None)
            .await
            .unwrap_err(),
        ManagementError::Invalid
    );
    let page = client
        .audit(&identity("999", "admin"), Scope::All, 1, None)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
    let next = client
        .audit(&identity("999", "admin"), Scope::All, 1, page.next_before)
        .await
        .unwrap();
    assert_ne!(page.events[0].id, next.events[0].id);
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660)).unwrap();
    assert_eq!(
        client
            .snapshot(&identity("123", "alice"), Scope::Own)
            .await
            .unwrap_err(),
        ManagementError::Unavailable
    );
    task.abort();
    let _ = task.await;
    assert!(!socket.exists());
}

#[tokio::test]
async fn oversized_frames_and_disabled_management_fail_closed() {
    let temp = Temp::new();
    let socket = temp.0.join("admin.sock");
    let server = ManagementServer::bind(&socket).await.unwrap();
    let task = tokio::spawn(server.run(Arc::new(Broker::new("https://example.test".into()))));
    let client = ManagementClient::new(socket.clone());
    assert_eq!(
        client
            .snapshot(&identity("1", "alice"), Scope::Own)
            .await
            .unwrap_err(),
        ManagementError::Disabled
    );
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    stream.write_u32((MAX_REQUEST + 1) as u32).await.unwrap();
    assert!(
        timeout(Duration::from_secs(2), stream.read_u8())
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(
        client
            .snapshot(&identity("1", "alice"), Scope::Own)
            .await
            .unwrap_err(),
        ManagementError::Disabled
    );
    task.abort();
    let _ = task.await;
}
