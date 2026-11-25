//! Actual CLI checks use private loopback peers; no tool backend or account is created.
#![cfg(unix)]
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade, ws::Message},
    http::HeaderMap,
    response::Response,
    routing::get,
};
use futures_util::StreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use ssmcp::wire::{self, ClientMetadata, Frame};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TOKEN: &str = "cl_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-allow-account-{}-{timestamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("auth")).unwrap();
        fs::set_permissions(path.join("auth"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("config.toml"), "[ui]\ncolor = false\n").unwrap();
        Self(path)
    }
    fn account(&self, server: &str) {
        let path = self.0.join("auth").join(format!(
            "{}.json",
            Sha256::digest(server.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ));
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        fs::write(&path,serde_json::to_vec(&json!({"server":server,"token":TOKEN,"principal":"owner@github","subject":"123","username":"owner-home","expires_at":expires})).unwrap()).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args(args)
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("SSMCP_SESSION_HOME", self.0.join("sessions"))
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env_remove("SSMCP_UI_CONFIG")
            .env_remove("SSMCP_RELAY_TOKEN")
            .env("NO_COLOR", "1")
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .unwrap()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone)]
struct Peer {
    legacy: bool,
    hellos: Arc<Mutex<Vec<ClientMetadata>>>,
    whoamis: Arc<AtomicUsize>,
}
async fn whoami(State(peer): State<Peer>, headers: HeaderMap) -> Json<serde_json::Value> {
    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
    peer.whoamis.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"provider":"github","subject":"123","login":"owner","principal":"owner@github","username":"owner-home"}),
    )
}
async fn connect(
    State(peer): State<Peer>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    assert!(!headers.contains_key("authorization"));
    let offer = headers[wire::CAPABILITIES_HEADER].to_str().unwrap();
    assert!(
        offer
            .split(',')
            .any(|v| v == wire::SESSION_ACCESS_CAPABILITY)
    );
    assert!(
        offer
            .split(',')
            .any(|v| v == wire::CONTROL_ATTRIBUTION_CAPABILITY)
    );
    upgrade.on_upgrade(move |mut socket| async move {
        async fn next_frame(socket: &mut axum::extract::ws::WebSocket) -> Frame {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    match socket.next().await.unwrap().unwrap() {
                        Message::Text(text) => break serde_json::from_str(&text).unwrap(),
                        Message::Binary(bytes) => break serde_json::from_slice(&bytes).unwrap(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        other => panic!("unexpected registration message: {other:?}"),
                    }
                }
            })
            .await
            .unwrap()
        }
        let Frame::ResolveController { id: 0, principal } = next_frame(&mut socket).await else {
            panic!("expected controller resolution before Hello");
        };
        if peer.legacy {
            socket
                .send(Message::Binary(
                    serde_json::to_vec(&Frame::Reject {
                        message: "unsupported admission".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            return;
        }
        let proposal = Frame::ResolvedController {
            id: 0,
            principal,
            provider: "github".into(),
            subject: "123".into(),
        };
        socket
            .send(Message::Binary(
                serde_json::to_vec(&proposal).unwrap().into(),
            ))
            .await
            .unwrap();
        let Frame::Hello {
            version, metadata, ..
        } = next_frame(&mut socket).await
        else {
            panic!("expected Hello")
        };
        assert_eq!(version, wire::VERSION);
        peer.hellos.lock().unwrap().push(metadata);
        let response = Frame::Reject {
            message: "allow fixture completed without starting tools".into(),
        };
        socket
            .send(Message::Binary(
                serde_json::to_vec(&response).unwrap().into(),
            ))
            .await
            .unwrap();
        while let Ok(Some(Ok(message))) =
            tokio::time::timeout(Duration::from_secs(3), socket.next()).await
        {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
}

#[tokio::test]
async fn allow_help_uses_only_server_qualified_accounts() {
    let home = Home::new();
    for args in [
        vec!["--help"],
        vec!["--help-full"],
        vec!["sessions", "create", "--help"],
    ] {
        let output = home.run(&args).await;
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("--allow"), "{args:?}: {text}");
        assert!(!text.contains("ssmcp invite"));
        assert!(!text.contains("ssmcp join"));
        assert!(!text.contains("--allow USER@github"));
    }
}

#[tokio::test]
async fn device_cli_connects_without_a_token_with_explicit_or_remembered_target() {
    for (legacy, explicit, saved) in [
        (false, true, false),
        (true, true, true),
        (false, false, true),
    ] {
        let home = Home::new();
        let hellos = Arc::new(Mutex::new(Vec::new()));
        let whoamis = Arc::new(AtomicUsize::new(0));
        let peer = Peer {
            legacy,
            hellos: hellos.clone(),
            whoamis: whoamis.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/api/client/whoami", get(whoami))
            .route("/api/client/connect", get(connect))
            .with_state(peer);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        if saved {
            home.account(&base);
        }
        let mut args = vec![
            "--server".to_owned(),
            base.clone(),
            "--no-watchdog".into(),
            "--no-browser".into(),
            "--name".into(),
            "allow-cli-fixture".into(),
        ];
        if explicit {
            args.push(format!("Alice@{base}"));
        } else {
            let path = home.0.join("auth/last-connection.json");
            let target = json!({"mode":"relay", "username":"saved-target", "server":base});
            fs::write(&path, serde_json::to_vec(&target).unwrap()).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let output = home
            .run(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .await;
        server.abort();
        let _ = server.await;
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "rejected registration must not publish credentials"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(if legacy {
                "unsupported admission"
            } else {
                "allow fixture completed"
            }),
            "{stderr}"
        );
        assert!(!stderr.contains("Starting native tools"), "{stderr}");
        assert_eq!(whoamis.load(Ordering::SeqCst), 0);
        let captured = hellos.lock().unwrap();
        if legacy {
            assert!(
                captured.is_empty(),
                "an old relay cannot publish the device metadata"
            );
            continue;
        }
        assert_eq!(captured.len(), 1);
        let expected = if explicit {
            vec![format!("alice@{base}")]
        } else {
            vec![format!("saved-target@{base}")]
        };
        assert_eq!(captured[0].allow, expected);
    }
}

#[tokio::test]
async fn allow_cli_rejects_wrong_servers_and_retired_syntax_before_network() {
    let home = Home::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    for target in ["alice", "alice@github", "alice@another.example"] {
        let output = home
            .run(&["--server", &base, "--no-watchdog", "--allow", target])
            .await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("allow"));
    }
    for command in ["invite", "join", "login"] {
        let output = home.run(&[command, "--help"]).await;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("removed") || error.contains("unrecognized command"));
    }
    // Old credentials must not select a device owner or cause a login probe.
    home.account(&base);
    let output = home.run(&["--server", &base, "--no-watchdog"]).await;
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("choose an account"));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}
