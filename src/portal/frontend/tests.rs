use super::*;
use crate::{broker::Broker, identity::Identity, wire};
use axum::http::{StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    process::{Child, Command, Stdio},
};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let suffix = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("ssmcp-public-{suffix}"));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn socket(&self) -> PathBuf {
        self.0.join("render.sock")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
async fn http(broker: Arc<Broker>) -> (String, JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, crate::http::router(broker))
            .await
            .unwrap();
    });
    (base, task)
}
fn configured(directory: &Directory) -> Broker {
    let mut broker = Broker::new("https://ssmcp.test".into());
    broker
        .portal
        .configure_public_frontend(directory.socket())
        .unwrap();
    broker
}
async fn stop(task: JoinHandle<()>) {
    task.abort();
    let _ = task.await;
}

#[test]
fn schema_accepts_only_public_rendering_and_known_assets() {
    for request in [
        json!({"operation":"account","subject":"123"}),
        json!({"operation":"guide_css","token":"secret"}),
        json!({"operation":"guide","language":"english","public_url":"https://ssmcp.test","cookie":"private"}),
        json!({"operation":"asset","name":"../../client-credentials.json","subject":"123"}),
    ] {
        assert!(serde_json::from_value::<Request>(request).is_err());
    }
    for name in ["../client-credentials.json", "/etc/passwd", "unknown.js"] {
        assert!(Page::resolve(Request::Asset { name: name.into() }).is_err());
    }
    assert!(Page::resolve(Request::GuideCss {}).is_ok());
}

#[tokio::test]
async fn socket_rejects_unsafe_parent_symlink_and_second_writer() {
    let directory = Directory::new();
    let path = directory.socket();
    let bound = socket::Bound::bind(&path).await.unwrap();
    assert_eq!(
        fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(socket::Bound::bind(&path).await.is_err());
    drop(bound);
    assert!(!path.exists());
    let link = directory.0.join("link");
    symlink(&directory.0, &link).unwrap();
    assert!(Endpoint::new(link.join("render.sock")).is_err());
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Endpoint::new(path.clone()).is_err());
    assert!(socket::Bound::bind(&path).await.is_err());
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(&path, b"not a socket").unwrap();
    assert!(Endpoint::new(path).is_err());
}

#[tokio::test]
async fn private_renderer_round_trip_and_owned_task_shutdown() {
    let directory = Directory::new();
    let bound = socket::Bound::bind(&directory.socket()).await.unwrap();
    let task = tokio::spawn(async move {
        run(bound).await.unwrap();
    });
    let endpoint = Endpoint::new(directory.socket()).unwrap();
    let rendered = endpoint
        .render(
            Request::Guide {
                language: guide::Language::Chinese,
                public_url: "https://ssmcp.test:8443".into(),
            },
            "text/html; charset=utf-8",
            None,
        )
        .await
        .unwrap();
    let body = String::from_utf8(rendered.body).unwrap();
    assert!(body.contains("lang=\"zh-CN\""));
    assert!(body.contains("https://ssmcp.test:8443/mcp"));
    let (content_type, bytes) = assets::public_bytes("brand-mark.png").unwrap();
    let hash = format!("{:x}", Sha256::digest(bytes));
    let rendered = endpoint
        .render(
            Request::Asset {
                name: "brand-mark.png".into(),
            },
            content_type,
            Some(&hash),
        )
        .await
        .unwrap();
    assert_eq!(rendered.body, bytes);
    stop(task).await;
    assert!(!directory.socket().exists());
    assert!(
        endpoint
            .render(Request::GuideCss {}, "text/css; charset=utf-8", None)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn timeout_oversized_and_mismatched_worker_responses_fall_back() {
    for variant in [
        "timeout",
        "oversized",
        "wrong-hash",
        "wrong-type",
        "wrong-protocol",
    ] {
        let directory = Directory::new();
        let bound = socket::Bound::bind(&directory.socket()).await.unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = bound.listener.accept().await.unwrap();
            let _ = read_frame(&mut stream, MAX_REQUEST).await.unwrap();
            if variant == "timeout" {
                tokio::time::sleep(Duration::from_secs(2)).await;
                return;
            }
            let bytes = b"not the bundled stylesheet";
            let response = Header {
                protocol: if variant == "wrong-protocol" {
                    99
                } else {
                    PROTOCOL
                },
                content_type: if variant == "wrong-type" {
                    "application/x-test"
                } else {
                    "text/css; charset=utf-8"
                }
                .into(),
                sha256: if variant == "wrong-hash" {
                    "0".repeat(64)
                } else {
                    format!("{:x}", Sha256::digest(bytes))
                },
            };
            write_frame(
                &mut stream,
                &serde_json::to_vec(&response).unwrap(),
                MAX_HEADER,
            )
            .await
            .unwrap();
            if variant == "oversized" {
                let _ = stream.write_u32((MAX_BODY + 1) as u32).await;
            } else {
                let _ = write_frame(&mut stream, bytes, MAX_BODY).await;
            }
        });
        let endpoint = Endpoint::new(directory.socket()).unwrap();
        let start = tokio::time::Instant::now();
        assert!(
            endpoint
                .render(Request::GuideCss {}, "text/css; charset=utf-8", None)
                .await
                .is_none(),
            "{variant}"
        );
        assert!(start.elapsed() < Duration::from_secs(1), "{variant}");
        stop(task).await;
    }
}

async fn changed_css_worker(bound: socket::Bound, changed: &'static [u8]) {
    loop {
        let (mut stream, _) = bound.listener.accept().await.unwrap();
        let bytes = read_frame(&mut stream, MAX_REQUEST).await.unwrap();
        let request: Request = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(request, Request::Asset { ref name } if name == "portal.css"));
        let header = Header {
            protocol: PROTOCOL,
            content_type: "text/css; charset=utf-8".into(),
            sha256: format!("{:x}", Sha256::digest(changed)),
        };
        write_frame(
            &mut stream,
            &serde_json::to_vec(&header).unwrap(),
            MAX_HEADER,
        )
        .await
        .unwrap();
        let _ = write_frame(&mut stream, changed, MAX_BODY).await;
    }
}
#[tokio::test]
async fn new_worker_asset_hashes_work_while_old_core_urls_keep_exact_bytes() {
    let directory = Directory::new();
    let bound = socket::Bound::bind(&directory.socket()).await.unwrap();
    let changed = b"body { color: rgb(3, 4, 5); }";
    let worker = tokio::spawn(changed_css_worker(bound, changed));
    let (base, server) = http(Arc::new(configured(&directory))).await;
    let client = reqwest::Client::new();
    let new_hash = format!("{:x}", Sha256::digest(changed));
    let new_path = format!("/account/assets/{new_hash}/portal.css");
    let response = client
        .get(format!("{base}{new_path}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-ssmcp-asset-sha256"], new_hash);
    assert_eq!(response.bytes().await.unwrap().as_ref(), changed);
    let response = client
        .get(format!("{base}{}", assets::url("portal.css")))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        assets::public_bytes("portal.css").unwrap().1
    );
    let response = client
        .get(format!("{base}/portal.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.bytes().await.unwrap().as_ref(), changed);
    let response = client
        .get(format!("{base}/account/assets/{new_hash}/unknown.css"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    stop(worker).await;
    assert_eq!(
        client
            .get(format!("{base}{new_path}"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client
            .get(format!("{base}{}", assets::url("portal.css")))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    stop(server).await;
}

#[tokio::test]
async fn guide_proxy_never_forwards_authorization_cookies_or_untrusted_host() {
    let directory = Directory::new();
    let bound = socket::Bound::bind(&directory.socket()).await.unwrap();
    let worker = tokio::spawn(async move {
        let (mut stream, _) = bound.listener.accept().await.unwrap();
        let bytes = read_frame(&mut stream, MAX_REQUEST).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value,
            json!({"operation":"guide","language":"english","public_url":"https://ssmcp.test"})
        );
        let body = b"<!doctype html><p>Public worker fixture</p>";
        let header = Header {
            protocol: PROTOCOL,
            content_type: "text/html; charset=utf-8".into(),
            sha256: format!("{:x}", Sha256::digest(body)),
        };
        write_frame(
            &mut stream,
            &serde_json::to_vec(&header).unwrap(),
            MAX_HEADER,
        )
        .await
        .unwrap();
        write_frame(&mut stream, body, MAX_BODY).await.unwrap();
    });
    let (base, server) = http(Arc::new(configured(&directory))).await;
    let response = reqwest::Client::new()
        .get(format!("{base}/guide/en"))
        .header(header::AUTHORIZATION, "Bearer private-test-token")
        .header(header::COOKIE, "private-test-cookie")
        .header(header::HOST, "untrusted.invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("Public worker fixture")
    );
    worker.await.unwrap();
    stop(server).await;
}

struct Process(Child);
impl Process {
    fn graceful_stop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            // SAFETY: this is our live, unreaped child; its PID cannot be reused.
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            while self.0.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
async fn worker_process(directory: &Directory, generation: usize) -> Process {
    let mut command = if let Some(binary) = std::env::var_os("SSMCP_FRONTEND_TEST_BINARY") {
        let mut command = Command::new(binary);
        command.arg("--socket").arg(directory.socket());
        command
    } else {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "portal::frontend::tests::worker_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("SSMCP_PRIVATE_FRONTEND_SOCKET", directory.socket());
        command
    };
    let log = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(directory.0.join(format!("worker-{generation}.log")))
        .unwrap();
    let mut process = Process(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let endpoint = Endpoint::new(directory.socket()).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if endpoint
            .render(Request::GuideCss {}, "text/css; charset=utf-8", None)
            .await
            .is_some()
        {
            return process;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "owned public worker exited"
        );
        assert!(
            tokio::time::Instant::now() < deadline,
            "owned public worker did not listen"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn ready(socket: &mut Ws) -> String {
    loop {
        let message = timeout(Duration::from_secs(4), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let bytes = match message {
            Message::Binary(b) => b.to_vec(),
            Message::Text(s) => s.as_bytes().to_vec(),
            _ => continue,
        };
        if let wire::Frame::Ready { client_id, .. } = serde_json::from_slice(&bytes).unwrap() {
            return client_id;
        }
        panic!("expected ready");
    }
}
async fn ping(socket: &mut Ws) {
    socket
        .send(Message::Ping(b"same-device-connection".to_vec().into()))
        .await
        .unwrap();
    loop {
        let message = timeout(Duration::from_secs(4), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let Message::Pong(bytes) = message {
            assert_eq!(bytes.as_ref(), b"same-device-connection");
            return;
        }
        assert!(!matches!(message, Message::Close(_)));
    }
}
async fn authorized_initialize(client: &reqwest::Client, base: &str, token: &str) -> StatusCode {
    client.post(format!("{base}/mcp")).bearer_auth(token).header(header::ACCEPT,"application/json")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2026-07-28","capabilities":{},"clientInfo":{"name":"public-frontend-fixture","version":"1"}
        }})).send().await.unwrap().status()
}

#[tokio::test]
async fn replacing_real_worker_preserves_core_oauth_and_same_device_websocket() {
    let directory = Directory::new();
    let mut broker = Broker::with_identity_config(
        "https://ssmcp.test".into(),
        crate::oauth::OAuthConfig::default(),
        Some(crate::identity::GitHubConfig {
            client_id: "private-fixture".into(),
            client_secret: "private-fixture".into(),
        }),
    )
    .unwrap();
    broker
        .oauth
        .enable_client_registry(&directory.0.join("oauth.toml"))
        .unwrap();
    broker.portal = crate::portal::Portal::open(&directory.0.join("accounts")).unwrap();
    broker
        .portal
        .configure_public_frontend(directory.socket())
        .unwrap();
    broker
        .portal
        .storage(|store| {
            store.login(&Identity {
                provider: "github".into(),
                subject: "123".into(),
                login: "alice".into(),
            })?;
            store.claim_handle("123", "alice")?;
            Ok(())
        })
        .await
        .unwrap();
    broker
        .configure_user_policies(crate::user_policy::DEFAULT_RATE_LIMIT_BPS)
        .await
        .unwrap();
    let token = broker.oauth.test_identity_tokens("mcp", "123")["access_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let unchanged_grants = fs::read(directory.0.join("oauth.toml.grants.json")).unwrap();
    let broker = Arc::new(broker);
    let authorization = broker.authenticate("mcp", &token).await.unwrap();
    let (base, server) = http(broker.clone()).await;
    let core_pid = std::process::id();
    let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        wire::CAPABILITIES_HEADER,
        format!(
            "{}, {}",
            wire::SESSION_ACCESS_CAPABILITY,
            wire::CONTROL_ATTRIBUTION_CAPABILITY
        )
        .parse()
        .unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let hello = wire::Frame::Hello {
        version: wire::VERSION,
        mode: wire::Mode::Shell,
        metadata: wire::ClientMetadata {
            name: "private-frontend-device".into(),
            title: None,
            cwd: "/tmp".into(),
            hostname: "fixture".into(),
            allow: vec!["alice@ssmcp.test".into()],
        },
    };
    socket
        .send(Message::Binary(serde_json::to_vec(&hello).unwrap().into()))
        .await
        .unwrap();
    let id = ready(&mut socket).await;
    let original = broker.client("mcp", &authorization, &id).await.unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(4))
        .build()
        .unwrap();
    let mut first = worker_process(&directory, 1).await;
    let first_pid = first.0.id();
    assert_eq!(
        authorized_initialize(&client, &base, &token).await,
        StatusCode::OK
    );
    assert!(
        client
            .get(format!("{base}/guide/zh"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("lang=\"zh-CN\"")
    );
    ping(&mut socket).await;
    first.graceful_stop();
    assert!(
        !directory.socket().exists(),
        "graceful worker stop cleans only its endpoint"
    );
    assert_eq!(
        client
            .get(format!("{base}/guide/en"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        authorized_initialize(&client, &base, &token).await,
        StatusCode::OK
    );
    let mut second = worker_process(&directory, 2).await;
    assert_ne!(first_pid, second.0.id());
    ping(&mut socket).await;
    assert_eq!(core_pid, std::process::id());
    assert!(Arc::ptr_eq(
        &original,
        &broker.client("mcp", &authorization, &id).await.unwrap()
    ));
    assert_eq!(
        authorized_initialize(&client, &base, &token).await,
        StatusCode::OK
    );
    assert_eq!(
        unchanged_grants,
        fs::read(directory.0.join("oauth.toml.grants.json")).unwrap()
    );
    second.graceful_stop();
    socket.close(None).await.unwrap();
    broker.disconnect_all().await;
    stop(server).await;
}

#[test]
#[ignore = "owned worker process entry used only by the replacement regression"]
fn worker_process_fixture() {
    let path = PathBuf::from(
        std::env::var_os("SSMCP_PRIVATE_FRONTEND_SOCKET").expect("private socket path"),
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(serve(&path))
        .unwrap();
}
