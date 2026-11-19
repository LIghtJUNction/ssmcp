//! Real CLI checks against private HTTP/WebSocket account fixtures.
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
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const TOKEN: &str = "cl_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-allow-account-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(path.join("auth")).unwrap();
        fs::set_permissions(path.join("auth"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join("config.toml"), "[ui]\ncolor = false\n").unwrap();
        Self(path)
    }
    fn account(&self, server: &str) {
        let path = self
            .0
            .join("auth")
            .join(format!("{:x}.json", Sha256::digest(server.as_bytes())));
        let expires = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "server":server, "token":TOKEN, "principal":"owner@github",
                "subject":"123", "username":"owner-home", "expires_at":expires
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ssmcp"));
        command
            .args(args)
            .env("SSMCP_AUTH_HOME", self.0.join("auth"))
            .env("SSMCP_CLIENT_CONFIG", self.0.join("config.toml"))
            .env_remove("SSMCP_UI_CONFIG")
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
}
fn authorized(headers: &HeaderMap) {
    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
}
async fn whoami(headers: HeaderMap) -> Json<serde_json::Value> {
    authorized(&headers);
    Json(
        json!({"provider":"github","subject":"123","login":"owner","principal":"owner@github","username":"owner-home"}),
    )
}
async fn connect(
    State(peer): State<Peer>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    authorized(&headers);
    assert_eq!(
        headers[wire::CAPABILITIES_HEADER],
        wire::SESSION_ACCESS_CAPABILITY
    );
    upgrade.on_upgrade(move |mut socket| async move {
        let bytes = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match socket.next().await.unwrap().unwrap() {
                    Message::Text(value) => break value.as_bytes().to_vec(),
                    Message::Binary(value) => break value.to_vec(),
                    Message::Ping(_) | Message::Pong(_) => continue,
                    other => panic!("expected Hello, received {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        let Frame::Hello {
            version, metadata, ..
        } = serde_json::from_slice(&bytes).unwrap()
        else {
            panic!("expected Hello")
        };
        assert_eq!(version, wire::VERSION);
        peer.hellos.lock().unwrap().push(metadata);
        let response = if peer.legacy {
            Frame::Ready {
                url: "https://fixture.invalid/mcp".into(),
                login_code: String::new(),
                login_expires_in: 0,
                client_id: "fixture-client".into(),
                capabilities: Vec::new(),
            }
        } else {
            Frame::Reject {
                message: "allow fixture completed without starting tools".into(),
            }
        };
        socket
            .send(Message::Binary(
                serde_json::to_vec(&response).unwrap().into(),
            ))
            .await
            .unwrap();
        // The candidate must terminate its transport before returning an error.
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
async fn allow_help_explains_registered_and_github_targets_on_current_commands() {
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
        for expected in ["--allow", "USER@github", "server username"] {
            assert!(text.contains(expected), "{args:?}: missing {expected}");
        }
        assert!(!text.contains("SSH sharing"));
        assert!(!text.contains("SSH options"));
    }
}

#[tokio::test]
async fn allow_cli_keeps_mixed_targets_through_authenticated_https_registration() {
    for legacy in [false, true] {
        let home = Home::new();
        let hellos = Arc::new(Mutex::new(Vec::new()));
        let peer = Peer {
            legacy,
            hellos: hellos.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/api/client/whoami", get(whoami))
            .route("/api/client/connect", get(connect))
            .with_state(peer);
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        home.account(&base);
        let output = home
            .run(&[
                "--server",
                &base,
                "--no-watchdog",
                "--no-browser",
                "--allow",
                "Alice, ALICE@GitHub, Alice_Dev",
                "--allow",
                "alice",
                "--name",
                "allow-cli-fixture",
            ])
            .await;
        server.abort();
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "failed policy acknowledgement must not publish credentials"
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(if legacy {
                "per-session sharing"
            } else {
                "allow fixture completed"
            }),
            "{stderr}"
        );
        assert!(!stderr.contains("Starting native tools"), "{stderr}");
        let captured = hellos.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].allow, ["alice", "alice@github", "alice_dev"]);
        assert_eq!(captured[0].name, "allow-cli-fixture");
    }
}
