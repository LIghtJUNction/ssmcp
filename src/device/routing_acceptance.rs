// Real loopback HTTP/WebSocket acceptance. This fixture never uses production
// credentials and does not claim to validate TLS, public NATs, or P2P transport.
use ring::signature::{Ed25519KeyPair, KeyPair};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

#[derive(Default)]
struct RelayTcpCounters {
    received: AtomicU64,
    sent: AtomicU64,
}
async fn count_copy<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    counter: &AtomicU64,
) -> std::io::Result<()> {
    let mut bytes = [0u8; 16384];
    loop {
        let count = reader.read(&mut bytes).await?;
        if count == 0 {
            return writer.shutdown().await;
        }
        writer.write_all(&bytes[..count]).await?;
        counter.fetch_add(count as u64, Ordering::Relaxed);
    }
}

struct AddressFixture {
    _directory: Temporary,
    broker: Arc<Broker>,
    base: String,
    owner: String,
    stranger: String,
    owner_mcp: String,
    stranger_mcp: String,
    server: tokio::task::JoinHandle<()>,
    proxy: tokio::task::JoinHandle<()>,
}

impl Drop for AddressFixture {
    fn drop(&mut self) {
        self.proxy.abort();
        self.server.abort();
    }
}

impl AddressFixture {
    async fn new() -> Self {
        let directory = Temporary::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", public.local_addr().unwrap());
        let counters = Arc::new(RelayTcpCounters::default());
        let counted = counters.clone();
        let proxy = tokio::spawn(async move {
            while let Ok((mut client, _)) = public.accept().await {
                let counted = counted.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = tokio::net::TcpStream::connect(upstream).await else {
                        return;
                    };
                    let (client_read, client_write) = client.split();
                    let (server_read, server_write) = server.split();
                    let _ = tokio::try_join!(
                        count_copy(client_read, server_write, &counted.received),
                        count_copy(server_read, client_write, &counted.sent)
                    );
                });
            }
        });
        let mut broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture".into(),
            }),
        )
        .unwrap();
        // Only the test listener is HTTP. Production clients require their
        // configured trusted HTTPS origin; these proofs bind this loopback URL.
        broker.public_url = base.clone();
        broker.portal = crate::portal::Portal::open(&directory.0).unwrap();
        broker
            .portal
            .storage(|store| {
                for (subject, login) in [("123", "alice"), ("456", "bob")] {
                    store.login(&Identity {
                        provider: "github".into(),
                        subject: subject.into(),
                        login: login.into(),
                    })?;
                    store.claim_handle(subject, login)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        // The routing fixture keeps a fleet of verified devices online under this account.
        broker.record_recharge("123").await.unwrap();
        let (owner, stranger) = broker
            .portal
            .client_storage(|store| {
                Ok((store.login("123".into())?.0, store.login("456".into())?.0))
            })
            .await
            .unwrap();
        let owner_mcp = broker.oauth.test_identity_tokens("mcp", "123")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let stranger_mcp = broker.oauth.test_identity_tokens("mcp", "456")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let broker = Arc::new(broker);
        let router = crate::http::router(broker.clone()).route("/__routing_test_metrics",get(move || {
            let counters=counters.clone();
            async move { Json(json!({"tcp_received_bytes":counters.received.load(Ordering::Relaxed),"tcp_sent_bytes":counters.sent.load(Ordering::Relaxed)})) }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            _directory: directory,
            broker,
            base,
            owner,
            stranger,
            owner_mcp,
            stranger_mcp,
            server,
            proxy,
        }
    }
}

struct SimulatedIdentity {
    key: Ed25519KeyPair,
    session: String,
}
impl SimulatedIdentity {
    fn new(index: usize) -> Self {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        Self {
            key: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
            session: format!("{index:064x}"),
        }
    }
    async fn proof(&self, http: &reqwest::Client, base: &str, credential: &str) -> String {
        let challenge: Value = http
            .post(format!("{base}{}", wire::device_address::CHALLENGE_PATH))
            .bearer_auth(credential)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        wire::device_address::Proof::sign(
            &self.key,
            base,
            &wire::device_address::credential_key(credential),
            &self.session,
            challenge["nonce"].as_str().unwrap(),
        )
        .unwrap()
        .header()
        .unwrap()
    }
    fn identity(&self, base: &str, subject: &str) -> wire::device_address::Identity {
        wire::device_address::Proof {
            version: 1,
            public_key: wire::device_address::encode(self.key.public_key().as_ref()),
            session_key: self.session.clone(),
            nonce: String::new(),
            signature: String::new(),
        }
        .identity(base, subject)
    }
}

async fn proven_socket(base: &str, token: &str, proof: &str, hostname: &str) -> Socket {
    let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
        .headers_mut()
        .insert(wire::device_address::PROOF_HEADER, proof.parse().unwrap());
    request.headers_mut().insert(
        wire::CAPABILITIES_HEADER,
        wire::SESSION_ACCESS_CAPABILITY.parse().unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Binary(
            serde_json::to_vec(&wire::Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: wire::ClientMetadata {
                    name: "work".into(),
                    title: None,
                    cwd: "/fixture".into(),
                    hostname: hostname.into(),
                    allow: vec![],
                },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    socket
}

#[derive(Default)]
struct DeviceCounters {
    calls: AtomicU64,
    catalog_reads: AtomicU64,
    receive_payload_bytes: AtomicU64,
    send_payload_bytes: AtomicU64,
    alive: AtomicBool,
}
struct SimulatedDevice {
    client_id: String,
    counters: Arc<DeviceCounters>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    runner: tokio::task::JoinHandle<()>,
}
impl Drop for SimulatedDevice {
    fn drop(&mut self) {
        self.runner.abort();
    }
}
impl SimulatedDevice {
    async fn connect(
        http: &reqwest::Client,
        base: &str,
        credential: &str,
        identity: &SimulatedIdentity,
        index: usize,
        hostname: &str,
    ) -> Self {
        let proof = identity.proof(http, base, credential).await;
        let mut socket = proven_socket(base, credential, &proof, hostname).await;
        let client_id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Ready {
                client_id,
                capabilities,
                ..
            } => {
                assert!(
                    capabilities
                        .iter()
                        .any(|c| c == wire::device_address::CAPABILITY)
                );
                client_id
            }
            other => panic!(
                "expected verified device Ready: {}",
                serde_json::to_string(&other).unwrap()
            ),
        };
        let counters = Arc::new(DeviceCounters::default());
        counters.alive.store(true, Ordering::SeqCst);
        let observed = counters.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let runner = tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
            loop {
                let message = tokio::select! {
                    _=&mut stopped => { let _=socket.close(None).await; break; },
                    _=heartbeat.tick()=> { if socket.send(Message::Ping(Vec::new().into())).await.is_err(){break;} continue; },
                    message=socket.next()=> match message { Some(Ok(value))=>value, _=>break },
                };
                let bytes = match message {
                    Message::Binary(bytes) => bytes.to_vec(),
                    Message::Text(text) => text.as_bytes().to_vec(),
                    Message::Ping(bytes) => {
                        if socket.send(Message::Pong(bytes)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                observed
                    .receive_payload_bytes
                    .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                let Ok(frame) = serde_json::from_slice::<wire::Frame>(&bytes) else {
                    break;
                };
                if let wire::Frame::Call { id, operation, .. } = frame {
                    let result = match operation {
                        Operation::ListTools { .. } => {
                            observed.catalog_reads.fetch_add(1, Ordering::SeqCst);
                            Ok(
                                json!({"tools":[{"name":"fixture.echo","description":"Isolated routing acceptance echo","inputSchema":{"type":"object"}}]}),
                            )
                        }
                        Operation::CallTool { name, .. } if name == "fixture.lose_receipt" => {
                            observed.calls.fetch_add(1, Ordering::SeqCst);
                            let _ = socket.close(None).await;
                            break;
                        }
                        Operation::CallTool { name, arguments } if name == "fixture.echo" => {
                            observed.calls.fetch_add(1, Ordering::SeqCst);
                            Ok(json!({"structuredContent":{"device_index":index,"echo":arguments}}))
                        }
                        Operation::CallTool { name, .. } if name == "ssmcp.client.capabilities" => {
                            Ok(json!({"thread_scope":1}))
                        }
                        Operation::Status {} => Ok(json!({"device_index":index})),
                        _ => Err("unsupported isolated fixture operation".into()),
                    };
                    let reply = serde_json::to_vec(&wire::Frame::Reply { id, result }).unwrap();
                    observed
                        .send_payload_bytes
                        .fetch_add(reply.len() as u64, Ordering::Relaxed);
                    if socket.send(Message::Binary(reply.into())).await.is_err() {
                        break;
                    }
                }
            }
            observed.alive.store(false, Ordering::SeqCst);
        });
        Self {
            client_id,
            counters,
            stop: Some(stop),
            runner,
        }
    }
    async fn disconnect(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        tokio::time::timeout(Duration::from_secs(5), &mut self.runner)
            .await
            .unwrap()
            .unwrap();
    }
}

async fn mcp_request(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let response=http.post(format!("{base}/mcp")).bearer_auth(token)
        .header("accept","application/json").json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        .send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

fn mcp_value(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "{response}");
    assert_ne!(response["result"]["isError"], true, "{response}");
    &response["result"]["structuredContent"]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_verified_devices_route_through_one_mcp_without_cross_account_or_reconnect_confusion() {
    let fixture = AddressFixture::new().await;
    let http = crate::http_client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let identities: Vec<_> = (0..10).map(SimulatedIdentity::new).collect();
    let mut devices = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        devices.push(
            SimulatedDevice::connect(
                &http,
                &fixture.base,
                &fixture.owner,
                identity,
                index,
                "same-host",
            )
            .await,
        );
    }
    let listed = mcp_request(&http, &fixture.base, &fixture.owner_mcp, "list", json!({})).await;
    let listed = mcp_value(&listed);
    assert_eq!(listed["clients"].as_array().unwrap().len(), 10);
    assert_eq!(listed["device_sessions"].as_array().unwrap().len(), 10);
    let ids: std::collections::HashSet<_> = listed["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["routing"]["device_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 10);
    for client in listed["clients"].as_array().unwrap() {
        assert_eq!(client["routing"]["alias"], "work@same-host.local");
        assert_eq!(client["routing"]["alias_kind"], "display_only");
    }
    for (index, device) in devices.iter().enumerate() {
        let stable = identities[index].identity(&fixture.base, "123");
        let found = mcp_request(
            &http,
            &fixture.base,
            &fixture.owner_mcp,
            "list",
            json!({"device_id":stable.device_id,"session_id":stable.session_id}),
        )
        .await;
        assert_eq!(mcp_value(&found)["clients"].as_array().unwrap().len(), 1);
        assert_eq!(mcp_value(&found)["clients"][0]["id"], device.client_id);
        let code = format!(
            "return await call({},'fixture.echo',{{sequence:{index}}});",
            serde_json::to_string(&device.client_id).unwrap()
        );
        let result = mcp_request(
            &http,
            &fixture.base,
            &fixture.owner_mcp,
            "code_mode",
            json!({"code":code}),
        )
        .await;
        let value = &mcp_value(&result)["result"]["structuredContent"];
        assert_eq!(value["device_index"], index, "{result}");
        assert_eq!(value["echo"]["sequence"], index);
    }
    let foreign = mcp_request(
        &http,
        &fixture.base,
        &fixture.stranger_mcp,
        "list",
        json!({}),
    )
    .await;
    assert_eq!(mcp_value(&foreign)["clients"], json!([]));
    assert_eq!(mcp_value(&foreign)["device_sessions"], json!([]));
    let code = format!(
        "return await call({},'fixture.echo',{{sequence:999}});",
        serde_json::to_string(&devices[0].client_id).unwrap()
    );
    let denied = mcp_request(
        &http,
        &fixture.base,
        &fixture.stranger_mcp,
        "code_mode",
        json!({"code":code}),
    )
    .await;
    assert!(
        denied.get("error").is_some() || denied["result"]["isError"] == true,
        "{denied}"
    );
    for device in &devices {
        assert_eq!(device.counters.calls.load(Ordering::SeqCst), 1);
    }

    // A signed proof is valid once; replay cannot create another connection.
    let proof = identities[0]
        .proof(&http, &fixture.base, &fixture.owner)
        .await;
    let mut duplicate = proven_socket(&fixture.base, &fixture.owner, &proof, "same-host").await;
    assert!(matches!(
        receive(&mut duplicate).await,
        Some(wire::Frame::Reject { .. })
    ));
    let mut replay = format!(
        "{}/api/client/connect",
        fixture.base.replace("http://", "ws://")
    )
    .into_client_request()
    .unwrap();
    replay.headers_mut().insert(
        "authorization",
        format!("Bearer {}", fixture.owner).parse().unwrap(),
    );
    replay
        .headers_mut()
        .insert(wire::device_address::PROOF_HEADER, proof.parse().unwrap());
    assert!(
        matches!(tokio_tungstenite::connect_async(replay).await,Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status()==StatusCode::UNAUTHORIZED)
    );

    let old = devices[0].client_id.clone();
    devices[0].disconnect().await;
    for attempt in 0..100 {
        let inventory =
            mcp_request(&http, &fixture.base, &fixture.owner_mcp, "list", json!({})).await;
        if mcp_value(&inventory)["device_sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["last_connection_id"] == old && entry["status"] == "offline")
        {
            break;
        }
        assert!(attempt < 99, "closed connection must become offline");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    devices[0] = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &identities[0],
        0,
        "renamed-host",
    )
    .await;
    assert_ne!(old, devices[0].client_id);
    let stable = identities[0].identity(&fixture.base, "123");
    let current = mcp_request(
        &http,
        &fixture.base,
        &fixture.owner_mcp,
        "list",
        json!({"device_id":stable.device_id,"session_id":stable.session_id}),
    )
    .await;
    assert_eq!(
        mcp_value(&current)["clients"][0]["routing"]["alias"],
        "work@renamed-host.local"
    );
    assert_eq!(
        mcp_value(&current)["clients"][0]["id"],
        devices[0].client_id
    );
    let stale = http
        .post(format!("{}/api/client/call", fixture.base))
        .bearer_auth(&fixture.owner)
        .json(&json!({"client_id":old,"name":"fixture.echo","arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::NOT_FOUND);
    assert_eq!(devices[0].counters.calls.load(Ordering::SeqCst), 0);
    let foreign = http
        .post(format!("{}/api/client/call", fixture.base))
        .bearer_auth(&fixture.stranger)
        .json(&json!({"client_id":devices[0].client_id,"name":"fixture.echo","arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    for device in &mut devices {
        device.disconnect().await;
    }
    fixture.broker.disconnect_all().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_reconnect_handoff_lists_the_same_session_without_replaying_lost_work() {
    let fixture = AddressFixture::new().await;
    let http = crate::http_client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let identity = SimulatedIdentity::new(81);
    let stable = identity.identity(&fixture.base, "123");
    let mut old =
        SimulatedDevice::connect(&http, &fixture.base, &fixture.owner, &identity, 1, "host").await;
    let uncertain = http
        .post(format!("{}/api/client/call", fixture.base))
        .bearer_auth(&fixture.owner)
        .json(&json!({"client_id":old.client_id,"name":"fixture.lose_receipt","arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(uncertain.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(old.counters.calls.load(Ordering::SeqCst), 1);
    old.disconnect().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page =
                mcp_request(&http, &fixture.base, &fixture.owner_mcp, "list", json!({})).await;
            if mcp_value(&page)["device_sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| {
                    item["last_connection_id"] == old.client_id && item["status"] == "offline"
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("old connection did not leave the live registry");

    let mut current = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &identity,
        2,
        "renamed-host",
    )
    .await;
    assert_ne!(old.client_id, current.client_id);
    let page = mcp_request(&http, &fixture.base, &fixture.owner_mcp, "list", json!({})).await;
    let entries = mcp_value(&page)["handoffs"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{page}");
    let handoff = &entries[0];
    assert_eq!(handoff["status"], "same_session");
    assert_eq!(handoff["previous_client_id"], old.client_id);
    assert_eq!(handoff["client_id"], current.client_id);
    assert_eq!(handoff["device_id"], stable.device_id);
    assert_eq!(handoff["session_id"], stable.session_id);

    let catalogs = current.counters.catalog_reads.load(Ordering::SeqCst);
    let stale_catalog = mcp_request(
        &http,
        &fixture.base,
        &fixture.owner_mcp,
        "list",
        json!({"client_id":old.client_id}),
    )
    .await;
    let stale_page = mcp_value(&stale_catalog);
    assert_eq!(&stale_page["handoff"], handoff);
    assert_eq!(stale_page["tools"], json!([]));
    assert_eq!(
        current.counters.catalog_reads.load(Ordering::SeqCst),
        catalogs
    );

    let live_catalog = mcp_request(
        &http,
        &fixture.base,
        &fixture.owner_mcp,
        "list",
        json!({"client_id":current.client_id}),
    )
    .await;
    let live_page = mcp_value(&live_catalog);
    assert_eq!(live_page["client"]["id"], current.client_id);
    assert!(
        live_page["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "fixture.echo")
    );
    assert_eq!(
        current.counters.catalog_reads.load(Ordering::SeqCst),
        catalogs + 1
    );

    let hidden = mcp_request(
        &http,
        &fixture.base,
        &fixture.stranger_mcp,
        "list",
        json!({}),
    )
    .await;
    assert_eq!(mcp_value(&hidden)["handoffs"], json!([]));
    let hidden = mcp_request(
        &http,
        &fixture.base,
        &fixture.stranger_mcp,
        "list",
        json!({"client_id":old.client_id}),
    )
    .await;
    assert!(
        hidden.get("error").is_some() || hidden["result"]["isError"] == true,
        "{hidden}"
    );
    assert!(!hidden.to_string().contains(&current.client_id), "{hidden}");
    assert!(!hidden.to_string().contains(&stable.session_id), "{hidden}");

    let denied = http
        .post(format!("{}/api/client/call", fixture.base))
        .bearer_auth(&fixture.owner)
        .json(&json!({"client_id":old.client_id,"name":"fixture.echo","arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::NOT_FOUND);
    assert_eq!(current.counters.calls.load(Ordering::SeqCst), 0);
    current.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_restart_handoff_discovers_candidates_without_transferring_work() {
    let fixture = AddressFixture::new().await;
    let http = crate::http_client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let mut identity = SimulatedIdentity::new(1);
    let original = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &identity,
        1,
        "same-host",
    )
    .await;
    let route = identity.identity(&fixture.base, "123");
    let lookup = json!({"device_id":route.device_id,"session_id":route.session_id});
    let discover = |token, args| mcp_request(&http, &fixture.base, token, "list", args);
    let found = discover(&fixture.owner_mcp, lookup.clone()).await;
    assert_eq!(
        mcp_value(&found)["handoff"]["client_id"],
        original.client_id
    );
    let joined = mcp_request(&http, &fixture.base, &fixture.owner_mcp, "collab",
        json!({"action":"join","client_id":original.client_id,"name":"fixture-worker","role":"worker"})).await;
    let member = mcp_value(&joined);

    // The fixture records one effect and closes its socket before sending a
    // receipt. Looking up a successor must never dispatch that work again.
    let lost = http
        .post(format!("{}/api/client/call", fixture.base))
        .bearer_auth(&fixture.owner)
        .json(&json!({"client_id":original.client_id,"name":"fixture.lose_receipt","arguments":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(lost.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(original.counters.calls.load(Ordering::SeqCst), 1);
    let unrelated = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &SimulatedIdentity::new(1),
        0,
        "same-host",
    )
    .await;
    let old_catalog = discover(&fixture.owner_mcp, json!({"client_id":original.client_id})).await;
    assert_eq!(old_catalog["result"]["isError"], true);
    assert!(
        old_catalog["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("/invite")
    );
    let before = discover(&fixture.owner_mcp, lookup.clone()).await;
    assert_eq!(mcp_value(&before)["handoff"]["status"], "unavailable");

    // A foreground restart has the same installation signing key and cwd but
    // deliberately generates a fresh process session key.
    identity.session = format!("{:064x}", 2);
    let replacement = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &identity,
        2,
        "renamed-host",
    )
    .await;
    let one = discover(&fixture.owner_mcp, lookup.clone()).await;
    let handoff = &mcp_value(&one)["handoff"];
    assert_eq!(handoff["status"], "confirmation_required");
    assert_eq!(
        handoff["candidate_client_ids"],
        json!([replacement.client_id])
    );
    assert!(handoff.get("client_id").is_none());
    assert_eq!(mcp_value(&one)["clients"], json!([]));
    let plain = discover(&fixture.owner_mcp, json!({})).await;
    assert_eq!(mcp_value(&plain)["handoffs"], json!([]));
    let catalogs = replacement.counters.catalog_reads.load(Ordering::SeqCst);
    let old_catalog = discover(&fixture.owner_mcp, json!({"client_id":original.client_id})).await;
    assert_eq!(old_catalog["result"]["isError"], true);
    assert_eq!(
        replacement.counters.catalog_reads.load(Ordering::SeqCst),
        catalogs
    );
    let stale = mcp_request(
        &http,
        &fixture.base,
        &fixture.owner_mcp,
        "collab",
        json!({"action":"receive","client_id":replacement.client_id,
        "agent_id":member["agent_id"],"token":member["token"]}),
    )
    .await;
    assert!(stale["result"]["isError"] == true || stale.get("error").is_some());
    let fresh = mcp_request(&http, &fixture.base, &fixture.owner_mcp, "collab",
        json!({"action":"join","client_id":replacement.client_id,"name":"fixture-worker","role":"worker"})).await;
    assert_ne!(mcp_value(&fresh)["agent_id"], member["agent_id"]);

    identity.session = format!("{:064x}", 3);
    let parallel = SimulatedDevice::connect(
        &http,
        &fixture.base,
        &fixture.owner,
        &identity,
        3,
        "same-host",
    )
    .await;
    let ambiguous = discover(&fixture.owner_mcp, lookup.clone()).await;
    let handoff = &mcp_value(&ambiguous)["handoff"];
    assert_eq!(handoff["status"], "ambiguous");
    assert_eq!(handoff["candidate_count"], 2);
    assert!(handoff.get("client_id").is_none());
    let foreign = discover(&fixture.stranger_mcp, lookup.clone()).await;
    assert_eq!(mcp_value(&foreign)["handoff"]["candidate_count"], 0);
    assert_eq!(mcp_value(&foreign)["device_sessions"], json!([]));
    assert_eq!(replacement.counters.calls.load(Ordering::SeqCst), 0);
    assert_eq!(parallel.counters.calls.load(Ordering::SeqCst), 0);
    assert_eq!(unrelated.counters.calls.load(Ordering::SeqCst), 0);

    http.post(format!("{}/api/client/logout", fixture.base))
        .bearer_auth(&fixture.owner)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let revoked = http
        .post(format!(
            "{}{}",
            fixture.base,
            wire::device_address::CHALLENGE_PATH
        ))
        .bearer_auth(&fixture.owner)
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
    fixture.broker.oauth.revoke_subject("123").unwrap();
    let denied = http.post(format!("{}/mcp",fixture.base)).bearer_auth(&fixture.owner_mcp)
        .header("accept","application/json")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":lookup}}))
        .send().await.unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
}

// Separate process: CPU/RSS below belong to the relay fixture, never to the
// simulated devices and workload generator. The proxy counts TCP payload bytes
// on this fixture alone, including HTTP, WebSocket framing, and heartbeats.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "private child of the explicit device soak, not an independent unit test"]
async fn device_soak_server_process() {
    let context = std::env::var_os("SSMCP_DEVICE_SOAK_CONTEXT").expect("soak child context path");
    let context = PathBuf::from(context);
    let fixture = AddressFixture::new().await;
    use std::os::unix::fs::OpenOptionsExt;
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(context.with_extension("pending"))
        .unwrap();
    std::io::Write::write_all(&mut output,serde_json::to_string(&json!({
        "base":fixture.base,"credential":fixture.owner,"mcp":fixture.owner_mcp,"pid":std::process::id()
    })).unwrap().as_bytes()).unwrap();
    drop(output);
    std::fs::rename(context.with_extension("pending"), &context).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(7200);
    while !context.with_extension("stop").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "soak child lifetime bound"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    fixture.broker.disconnect_all().await;
}

#[cfg(target_os = "linux")]
struct SoakChild(std::process::Child);
#[cfg(target_os = "linux")]
impl Drop for SoakChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(target_os = "linux")]
fn process_sample(pid: u32) -> (u64, u64) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let fields: Vec<_> = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect();
    let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    (ticks, fields[21].parse::<u64>().unwrap() * page_size)
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "explicit 100-device real-time soak; defaults to 3600 measured seconds"]
async fn hundred_devices_real_time_soak() {
    let duration = std::env::var("SSMCP_DEVICE_SOAK_SECONDS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap())
        .unwrap_or(3600);
    assert!((1..=3600).contains(&duration));
    let report_path = PathBuf::from(
        std::env::var_os("SSMCP_DEVICE_SOAK_REPORT").expect("fresh report output path"),
    );
    assert!(
        !report_path.exists(),
        "never overwrite an earlier acceptance result"
    );
    let temporary = Temporary::new();
    let context = temporary.0.join("context.json");
    let log = std::fs::File::create(temporary.0.join("relay.log")).unwrap();
    let mut child = SoakChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "device::tests::device_soak_server_process",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("SSMCP_DEVICE_SOAK_CONTEXT", &context)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let ready = std::time::Instant::now() + Duration::from_secs(30);
    while !context.exists() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "relay fixture exited during startup"
        );
        assert!(
            std::time::Instant::now() < ready,
            "relay fixture startup timeout"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let configuration: Value = serde_json::from_slice(&std::fs::read(&context).unwrap()).unwrap();
    let base = configuration["base"].as_str().unwrap();
    let credential = configuration["credential"].as_str().unwrap();
    let http = crate::http_client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let identities: Vec<_> = (0..100).map(SimulatedIdentity::new).collect();
    let mut devices = Vec::new();
    for (index, identity) in identities.iter().enumerate() {
        devices.push(
            SimulatedDevice::connect(&http, base, credential, identity, index, "same-host").await,
        );
    }
    let before: Value = http
        .get(format!("{base}/__routing_test_metrics"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (cpu_start, _) = process_sample(child.0.id());
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(duration);
    let mut pacing = tokio::time::interval(Duration::from_millis(100));
    pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut latencies = Vec::new();
    let mut attempted = 0u64;
    let mut successes = 0u64;
    let mut failures = 0u64;
    let mut unknown = 0u64;
    let mut samples = Vec::new();
    let mut next_sample = started;
    let mut last_cpu = cpu_start;
    let mut last_sample = started;
    let mut connection_loss = false;
    while std::time::Instant::now() < deadline {
        pacing.tick().await;
        if std::time::Instant::now() >= deadline {
            break;
        }
        let index = (attempted % 100) as usize;
        let sequence = attempted;
        attempted += 1;
        let call_started = std::time::Instant::now();
        // The HTTP account call and MCP call share Broker::client and the same
        // per-call authorization/dispatch. The separate ten-device test above
        // exercises the real /mcp JS selection path; this measures relay load.
        let result=http.post(format!("{base}/api/client/call")).bearer_auth(credential)
            .json(&json!({"client_id":devices[index].client_id,"name":"fixture.echo","arguments":{"sequence":sequence,"device_index":index}}))
            .send().await;
        match result {
            Ok(response) if response.status().is_success() => {
                match response.json::<Value>().await {
                    Ok(value)
                        if value["structuredContent"]["device_index"] == index
                            && value["structuredContent"]["echo"]["sequence"] == sequence =>
                    {
                        successes += 1;
                        latencies.push(call_started.elapsed().as_secs_f64() * 1000.0);
                    }
                    _ => failures += 1,
                }
            }
            Ok(_) => failures += 1,
            Err(_) => {
                unknown += 1;
                failures += 1;
            }
        }
        // Never replay an uncertain request: every next iteration has a new
        // sequence and does independent synthetic work on its selected device.
        let now = std::time::Instant::now();
        if now >= next_sample {
            let live = devices
                .iter()
                .filter(|v| v.counters.alive.load(Ordering::SeqCst))
                .count();
            connection_loss |= live != 100;
            let (cpu, rss) = process_sample(child.0.id());
            let elapsed = now.duration_since(last_sample).as_secs_f64();
            let cpu_percent = if elapsed > 0.0 {
                (cpu - last_cpu) as f64 / ticks_per_second / elapsed * 100.0
            } else {
                0.0
            };
            samples.push(json!({"elapsed_seconds":started.elapsed().as_secs_f64(),"online_devices":live,"relay_cpu_one_core_percent":cpu_percent,"relay_rss_bytes":rss,"attempted":attempted,"successes":successes,"failures":failures}));
            last_cpu = cpu;
            last_sample = now;
            next_sample = now + Duration::from_secs(30);
        }
    }
    let elapsed = started.elapsed().as_secs_f64();
    let (cpu_end, rss_end) = process_sample(child.0.id());
    let after: Value = http
        .get(format!("{base}/__routing_test_metrics"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let inventory: Value = http
        .get(format!("{base}/api/client/sessions"))
        .bearer_auth(credential)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let online = inventory["clients"].as_array().unwrap().len();
    connection_loss |= online != 100;
    let distinct: std::collections::HashSet<_> = inventory["device_sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["routing"]["device_id"].as_str().unwrap())
        .collect();
    connection_loss |= distinct.len() != 100;
    latencies.sort_by(|a, b| a.total_cmp(b));
    let quantile = |q: f64| {
        latencies
            .get(((latencies.len().saturating_sub(1)) as f64 * q).round() as usize)
            .copied()
    };
    let received = after["tcp_received_bytes"].as_u64().unwrap()
        - before["tcp_received_bytes"].as_u64().unwrap();
    let sent =
        after["tcp_sent_bytes"].as_u64().unwrap() - before["tcp_sent_bytes"].as_u64().unwrap();
    let report = json!({
        "schema":"ssmcp-device-soak-v1","source_commit":std::env::var("SSMCP_DEVICE_SOAK_SOURCE").ok(),
        "test_binary_sha256":std::env::var("SSMCP_DEVICE_SOAK_BINARY_SHA256").ok(),
        "profile":if duration==3600{"one_hour"}else{"smoke"},"requested_seconds":duration,"measured_seconds":elapsed,
        "one_hour_complete":duration==3600 && elapsed>=3600.0,"simulated_devices":100,"online_at_end":online,"distinct_devices":distinct.len(),"connection_loss_observed":connection_loss,
        "attempted":attempted,"successful":successes,"failed":failures,"unknown_not_replayed":unknown,
        "failure_rate":failures as f64/attempted.max(1) as f64,"successful_requests_per_second":successes as f64/elapsed,
        "latency_ms":{"p50":quantile(0.50),"p95":quantile(0.95),"p99":quantile(0.99),"max":latencies.last()},
        "relay":{"pid":child.0.id(),"cpu_seconds":(cpu_end-cpu_start) as f64/ticks_per_second,"mean_cpu_one_core_percent":(cpu_end-cpu_start) as f64/ticks_per_second/elapsed*100.0,"rss_bytes_at_end":rss_end,"received_tcp_payload_bytes":received,"sent_tcp_payload_bytes":sent,"received_bytes_per_second":received as f64/elapsed,"sent_bytes_per_second":sent as f64/elapsed},
        "measurement_boundary":"Separate real relay process plus lightweight counting TCP proxy; loopback only. Byte counts include HTTP, WebSocket framing and heartbeat payloads, exclude IP/TCP headers. Device simulator CPU is excluded. Production TLS, NAT and P2P are not exercised.",
        "workload":"10 scheduled exact-client account API echo calls per second; no unknown call is replayed; MCP selection is verified separately by ten-device acceptance.",
        "samples":samples,
    });
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    println!(
        "device soak report: {} ({} seconds, {} successes, {} failures)",
        report_path.display(),
        elapsed,
        successes,
        failures
    );
    for device in &mut devices {
        device.disconnect().await;
    }
    std::fs::write(context.with_extension("stop"), b"stop").unwrap();
    let stop_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < stop_deadline,
            "relay child cleanup bound"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        failures, 0,
        "inspect retained report; failed/unknown calls were not replayed"
    );
    assert!(
        !connection_loss,
        "inspect retained report; an online device disappeared"
    );
    assert!(
        successes >= duration * 5,
        "fixture sustained at least half the scheduled 10 req/s load"
    );
}
