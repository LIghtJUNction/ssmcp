use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

struct Step {
    path: &'static str,
    status: u16,
    body: Value,
    authenticated: bool,
}

async fn server(steps: impl FnOnce(&str) -> Vec<Step>) -> (String, JoinHandle<Vec<Value>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let steps = steps(&origin);
    let job = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for step in steps {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            let (header_end, content_length) = loop {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&bytes[..count]);
                assert!(request.len() < 32 * 1024);
                if let Some(offset) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = std::str::from_utf8(&request[..offset]).unwrap();
                    assert_eq!(
                        headers.lines().next().unwrap().split_whitespace().nth(1),
                        Some(step.path)
                    );
                    assert_eq!(
                        headers.lines().any(|line| line
                            .to_ascii_lowercase()
                            .starts_with("authorization: bearer ")),
                        step.authenticated
                    );
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (offset + 4, length);
                }
            };
            while request.len() < header_end + content_length {
                let mut bytes = [0; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&bytes[..count]);
                assert!(request.len() < 32 * 1024);
            }
            bodies.push(if content_length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap()
            });
            let payload = serde_json::to_vec(&step.body).unwrap();
            let headers = format!(
                "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                step.status,
                payload.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(&payload).await.unwrap();
        }
        bodies
    });
    (origin, job)
}

fn root() -> PathBuf {
    let mut bytes = [0; 16];
    platform::random_bytes(&mut bytes).unwrap();
    let root = std::env::temp_dir().join(format!(
        "ssmcp-named-account-{}",
        crate::hex::encode(Sha256::digest(bytes))
    ));
    create_tree(&root).unwrap();
    root
}

fn identity(username: Option<&str>) -> Value {
    json!({"identity":{
        "provider":"github","subject":"123","login":"Octocat",
        "principal":"octocat@github","username":username
    }})
}

fn saved(server: &str) -> Credential {
    Credential {
        server: server.into(),
        token: "a".repeat(64),
        principal: "octocat@github".into(),
        subject: Some("123".into()),
        username: None,
        expires_at: now() + 3600,
    }
}

fn authorization_steps(server: &str, username: &str) -> Vec<Step> {
    let mut ready = identity(Some(username));
    ready["access_token"] = json!("b".repeat(64));
    ready["expires_in"] = json!(3600);
    ready["status"] = json!("authorized");
    vec![
        Step {
            path: "/api/client/login/start",
            status: 200,
            authenticated: false,
            body: json!({
                "device_code":"c".repeat(64), "verification_uri_complete":format!("{server}/setup?code=ABCD"),
                "expires_in":600,"interval":1,"user_code":"ABCD"
            }),
        },
        Step {
            path: "/api/client/login/poll",
            status: 200,
            authenticated: false,
            body: ready,
        },
    ]
}

#[test]
fn named_destinations_normalize_accounts_ports_and_ipv6() {
    for (target, username, origin) in [
        ("Alice@EXAMPLE.com:443", "alice", "https://example.com"),
        (
            "alice_dev@EXAMPLE.com:8443",
            "alice_dev",
            "https://example.com:8443",
        ),
        ("alice@[::1]:8443", "alice", "https://[::1]:8443"),
        (
            "alice@http://127.0.0.1:3000",
            "alice",
            "http://127.0.0.1:3000",
        ),
    ] {
        assert_eq!(
            parse_target(target).unwrap(),
            (username.into(), origin.into())
        );
    }
    for target in [
        "alice",
        "alice@",
        "@example.com",
        "root@example.com",
        "ab@example.com",
        "älïce@example.com",
        "alice@@example.com",
        "alice@host/path",
        "alice@host?query",
        "alice@host#fragment",
        "alice@host\\other",
        "alice@http://example.com",
        "alice@host:0",
        "alice@host:65536",
        "alice@ https://example.com",
        "alice@https://user:pass@example.com",
    ] {
        assert!(parse_target(target).is_err(), "{target}");
    }
}

#[tokio::test]
async fn existing_account_claims_a_name_without_another_login() {
    let root = root();
    let (origin, job) = server(|_| {
        vec![
            Step {
                path: "/api/client/whoami",
                status: 200,
                body: identity(None),
                authenticated: true,
            },
            Step {
                path: "/api/client/username",
                status: 200,
                body: identity(Some("alice")),
                authenticated: true,
            },
        ]
    })
    .await;
    let credential = saved(&origin);
    save_at(&root, &credential).unwrap();
    let current = connect_at(&root, Some(&origin), Some("Alice"), true, true)
        .await
        .unwrap();
    assert_eq!(current.username.as_deref(), Some("alice"));
    assert_eq!(current.token, credential.token);
    assert_eq!(current.principal, "octocat@github");
    let requests = job.await.unwrap();
    assert_eq!(requests[1], json!({"username":"alice"}));
    assert_eq!(
        load_at(&root, Some(&origin), false)
            .unwrap()
            .unwrap()
            .username
            .as_deref(),
        Some("alice")
    );
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn wrong_username_or_subject_never_replaces_saved_credentials() {
    for wrong_subject in [false, true] {
        let root = root();
        let (origin, job) = server(|_| {
            let mut value = identity(Some(if wrong_subject {
                "alice"
            } else {
                "someone-else"
            }));
            if wrong_subject {
                value["identity"]["subject"] = json!("456");
            }
            vec![Step {
                path: "/api/client/whoami",
                status: 200,
                body: value,
                authenticated: true,
            }]
        })
        .await;
        let credential = saved(&origin);
        save_at(&root, &credential).unwrap();
        let before = fs::read(credential_path(&root, &origin)).unwrap();
        assert!(
            connect_at(&root, Some(&origin), Some("alice"), true, true)
                .await
                .is_err()
        );
        job.await.unwrap();
        assert_eq!(fs::read(credential_path(&root, &origin)).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn expired_and_rejected_tokens_automatically_authorize_the_named_account() {
    for rejected in [false, true] {
        let root = root();
        let (origin, job) = server(|origin| {
            let mut steps = if rejected {
                vec![Step {
                    path: "/api/client/whoami",
                    status: 401,
                    body: json!({"error":"invalid_token"}),
                    authenticated: true,
                }]
            } else {
                Vec::new()
            };
            steps.extend(authorization_steps(origin, "alice"));
            steps
        })
        .await;
        let mut credential = saved(&origin);
        credential.username = Some("alice".into());
        if !rejected {
            credential.expires_at = now() - 1;
        }
        save_at(&root, &credential).unwrap();
        let current = connect_at(&root, Some(&origin), Some("alice"), true, true)
            .await
            .unwrap();
        assert_eq!(current.token, "b".repeat(64));
        assert_eq!(current.username.as_deref(), Some("alice"));
        assert_eq!(current.subject.as_deref(), Some("123"));
        let bodies = job.await.unwrap();
        assert_eq!(bodies[usize::from(rejected)]["username"], "alice");
        assert_eq!(
            load_at(&root, Some(&origin), false).unwrap().unwrap().token,
            current.token
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn first_connection_authorizes_and_binds_the_requested_username() {
    let root = root();
    let (origin, job) = server(|origin| authorization_steps(origin, "alice")).await;
    let current = connect_at(&root, Some(&origin), Some("ALICE"), true, true)
        .await
        .unwrap();
    assert_eq!(current.username.as_deref(), Some("alice"));
    assert_eq!(job.await.unwrap()[0]["username"], "alice");
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn outages_and_bans_do_not_restart_login_or_remove_credentials() {
    for status in [403, 429, 503] {
        let root = root();
        let (origin, job) = server(|_| {
            vec![Step {
                path: "/api/client/whoami",
                status,
                body: json!({"error":if status==403{"account_banned"}else{"unavailable"}}),
                authenticated: true,
            }]
        })
        .await;
        let credential = saved(&origin);
        save_at(&root, &credential).unwrap();
        let before = fs::read(credential_path(&root, &origin)).unwrap();
        assert!(
            connect_at(&root, Some(&origin), Some("alice"), true, true)
                .await
                .is_err()
        );
        job.await.unwrap();
        assert_eq!(fs::read(credential_path(&root, &origin)).unwrap(), before);
        fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn oauth_cannot_save_a_different_username_than_requested() {
    let root = root();
    let (origin, job) = server(|origin| authorization_steps(origin, "bob")).await;
    assert!(
        connect_at(&root, Some(&origin), Some("alice"), true, true)
            .await
            .is_err()
    );
    job.await.unwrap();
    assert!(load_at(&root, Some(&origin), true).unwrap().is_none());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn background_sessions_report_required_authorization_without_opening_a_browser() {
    let root = root();
    let origin = "https://example.com";
    let mut credential = saved(origin);
    credential.username = Some("alice".into());
    credential.expires_at = now() - 1;
    save_at(&root, &credential).unwrap();
    let error = connect_at(&root, Some(origin), None, true, false)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("background session needs authorization")
    );
    assert!(load_at(&root, Some(origin), true).unwrap().is_some());
    fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn account_compat_verifies_registered_handle_without_changing_saved_login() {
    let directory = root();
    let (origin, requests) = server(|_| {
        vec![Step {
            path: "/api/client/whoami",
            status: 200,
            body: identity(Some("alice")),
            authenticated: true,
        }]
    })
    .await;
    let credential = saved(&origin);
    save_at(&directory, &credential).unwrap();
    let before = fs::read(credential_path(&directory, &origin)).unwrap();
    let current = compat_at(&directory, &origin, &[format!("alice@{origin}")])
        .await
        .unwrap();
    // The registered handle deliberately differs from the GitHub login.
    assert_eq!(current.username.as_deref(), Some("alice"));
    assert_eq!(current.subject.as_deref(), Some("123"));
    assert_eq!(current.principal, "octocat@github");
    assert_eq!(current.token, credential.token);
    assert_eq!(requests.await.unwrap(), vec![Value::Null]);
    assert_eq!(
        fs::read(credential_path(&directory, &origin)).unwrap(),
        before
    );
    fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn account_compat_rejects_unproven_binding_and_never_authorizes_or_claims() {
    let mut wrong_subject = identity(Some("alice"));
    wrong_subject["identity"]["subject"] = json!("456");
    let mut wrong_principal = identity(Some("alice"));
    wrong_principal["identity"]["login"] = json!("other");
    wrong_principal["identity"]["principal"] = json!("other@github");
    let mut missing_subject = identity(Some("alice"));
    missing_subject["identity"]
        .as_object_mut()
        .unwrap()
        .remove("subject");
    for (status, body) in [
        (200, identity(None)),
        (200, identity(Some("different"))),
        (200, wrong_subject),
        (200, wrong_principal),
        (200, missing_subject),
        (401, json!({"error":"invalid_token"})),
        (403, json!({"error":"account_banned"})),
        (503, json!({"error":"unavailable"})),
    ] {
        let directory = root();
        let (origin, requests) = server(|_| {
            vec![Step {
                path: "/api/client/whoami",
                status,
                body,
                authenticated: true,
            }]
        })
        .await;
        let credential = saved(&origin);
        save_at(&directory, &credential).unwrap();
        let path = credential_path(&directory, &origin);
        let before = fs::read(&path).unwrap();
        let error = compat_at(&directory, &origin, &[format!("alice@{origin}")])
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains(&credential.token));
        assert_eq!(requests.await.unwrap(), vec![Value::Null]);
        assert_eq!(fs::read(path).unwrap(), before);
        fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn account_compat_rejects_missing_expired_multi_and_cross_origin_before_network() {
    let directory = root();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let self_allow = vec![format!("alice@{origin}")];
    assert!(compat_at(&directory, &origin, &self_allow).await.is_err());
    let mut credential = saved(&origin);
    credential.expires_at = now() - 1;
    save_at(&directory, &credential).unwrap();
    assert!(compat_at(&directory, &origin, &self_allow).await.is_err());
    credential.expires_at = now() + 3600;
    save_at(&directory, &credential).unwrap();
    for allow in [
        Vec::new(),
        vec![format!("alice@{origin}"), format!("bob@{origin}")],
        vec!["alice@other.example".into()],
    ] {
        assert!(compat_at(&directory, &origin, &allow).await.is_err());
    }
    let second_origin = "http://127.0.0.1:1";
    assert!(
        compat_at(
            &directory,
            second_origin,
            &[format!("alice@{second_origin}")]
        )
        .await
        .is_err()
    );
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    fs::remove_dir_all(directory).unwrap();
}
