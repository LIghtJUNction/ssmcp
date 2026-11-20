//! The public coordinator talks to an authenticated websocket peer. This fixture
//! models native pending replies; real terminal decisions remain a PTY check.
use super::*;
use crate::{
    identity::Identity,
    portal::Portal,
    wire::{self, Frame},
};
use futures_util::{SinkExt, StreamExt};
use std::{path::PathBuf, sync::Mutex};
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle, time::timeout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

const REQUEST_ID: &str = "native_retained_finish";
const REASON: &str = "Checks complete";

#[derive(Clone, Copy)]
enum ReplyMode {
    Delayed(Duration),
    Never,
}

#[derive(Default)]
struct NativeState {
    requests: Vec<Value>,
    cancellations: Vec<Value>,
    replies: usize,
}

struct Directory(PathBuf);

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    broker: Arc<Broker>,
    token: String,
    client_id: String,
    agent_id: String,
    agent_token: String,
    native: Arc<Mutex<NativeState>>,
    changed: Arc<Notify>,
    peer: JoinHandle<()>,
    server: JoinHandle<()>,
    _directory: Directory,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.peer.abort();
        self.server.abort();
    }
}

async fn send(socket: &mut Socket, frame: Frame) {
    socket
        .send(Message::Binary(serde_json::to_vec(&frame).unwrap().into()))
        .await
        .unwrap();
}

async fn receive(socket: &mut Socket) -> Option<Frame> {
    loop {
        match socket.next().await?.unwrap() {
            Message::Text(data) => return Some(serde_json::from_slice(data.as_bytes()).unwrap()),
            Message::Binary(data) => return Some(serde_json::from_slice(&data).unwrap()),
            Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
            Message::Pong(_) => {}
            Message::Close(_) => return None,
            _ => panic!("unexpected fixture websocket message"),
        }
    }
}

fn pending(agent_id: &str, created: bool) -> Value {
    json!({"schema_version":1,"request_id":REQUEST_ID,"created":created,
        "timed_out":true,"default_decision":"no","decision":null,"approved":false,
        "request":{
            "id":REQUEST_ID,"title":"Request to finish","question":REASON,
            "options":["No","Yes"],"state":"pending","answer":null,
            "created_at_ms":1,"answered_at_ms":null,"kind":"finish",
            "requester":{"provider":"github","subject":"42","thread_id":agent_id,
                "name":"finish requester"},
            "finish":{"reason":REASON,"default_decision":"no","decision":null,
                "status":"pending","approved":false,"invalidation_reason":null},
            "answer_actor":null,"answer_revision":0,"automatic_delivery":true,
            "delivery":"not_answered"}})
}

impl Fixture {
    async fn new(mode: ReplyMode) -> Self {
        let mut nonce = [0_u8; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = Directory(std::env::temp_dir().join(format!("ssmcp-finish-rpc-{nonce}")));
        let mut broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture-secret".into(),
            }),
        )
        .unwrap();
        broker.portal = Portal::open(&directory.0).unwrap();
        broker
            .portal
            .storage(|store| {
                store.login(&Identity {
                    provider: "github".into(),
                    subject: "42".into(),
                    login: "octocat".into(),
                })?;
                store.claim_handle("42", "alice")?;
                Ok(())
            })
            .await
            .unwrap();
        let device_token = broker
            .portal
            .client_storage(|store| store.login("42".into()))
            .await
            .unwrap()
            .0;
        let broker = Arc::new(broker);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/api/client/connect", listener.local_addr().unwrap());
        let routes = router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
        let mut request = url.into_client_request().unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {device_token}").parse().unwrap(),
        );
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
        send(
            &mut socket,
            Frame::Hello {
                version: wire::VERSION,
                mode: wire::Mode::Shell,
                metadata: wire::ClientMetadata {
                    name: "finish transport fixture".into(),
                    title: None,
                    cwd: "/fixture".into(),
                    hostname: "localhost".into(),
                    allow: vec![],
                },
            },
        )
        .await;
        let Some(Frame::Ready {
            client_id,
            capabilities,
            ..
        }) = timeout(Duration::from_secs(3), receive(&mut socket))
            .await
            .unwrap()
        else {
            panic!("authenticated Ready required");
        };
        assert!(
            capabilities
                .iter()
                .any(|capability| capability == wire::CONTROL_ATTRIBUTION_CAPABILITY)
        );
        let token = broker.oauth.test_identity_tokens("mcp", "42")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let authorization = broker.authenticate("mcp", &token).await.unwrap();
        let session = broker
            .client("mcp", &authorization, &client_id)
            .await
            .unwrap();
        let owner = crate::bridge::owner("mcp", &authorization).unwrap();
        let joined = session
            .collaboration
            .dispatch(&owner, "join", json!({"name":"finish requester"}))
            .await
            .unwrap();
        let agent_id = joined["agent_id"].as_str().unwrap().to_owned();
        let agent_token = joined["token"].as_str().unwrap().to_owned();
        let native = Arc::new(Mutex::new(NativeState::default()));
        let changed = Arc::new(Notify::new());
        let peer_state = native.clone();
        let peer_changed = changed.clone();
        let expected_agent = agent_id.clone();
        let expected_client = client_id.clone();
        let peer = tokio::spawn(async move {
            while let Some(frame) = receive(&mut socket).await {
                let Frame::Call {
                    id,
                    operation: Operation::CallTool { name, arguments },
                    attribution,
                    code_context,
                    ..
                } = frame
                else {
                    assert!(matches!(frame, Frame::CodeModeFinished { .. }));
                    continue;
                };
                if name == "ssmcp.client.capabilities" {
                    send(
                        &mut socket,
                        Frame::Reply {
                            id,
                            result: Ok(json!({"thread_scope":1})),
                        },
                    )
                    .await;
                    continue;
                }
                let attribution = attribution.expect("verified account attribution");
                assert_eq!(attribution.account.subject, "42");
                assert_eq!(attribution.agent.as_ref().unwrap().id, expected_agent);
                let context = code_context.expect("verified thread scope");
                assert_eq!(
                    context.session_id.as_deref(),
                    Some(expected_client.as_str())
                );
                assert_eq!(context.thread_id.as_deref(), Some(expected_agent.as_str()));
                let output = match name.as_str() {
                    "ssmcp.ui.finish" if arguments["op"] == "request" => {
                        let wait = arguments["timeout_ms"].as_u64().unwrap();
                        let created = {
                            let mut state = peer_state.lock().unwrap();
                            if let Some(id) = arguments["request_id"].as_str() {
                                assert_eq!(id, REQUEST_ID);
                            }
                            assert_eq!(arguments["reason"], REASON);
                            state.requests.push(arguments.clone());
                            state.requests.len() == 1
                        };
                        peer_changed.notify_one();
                        match mode {
                            ReplyMode::Delayed(delay) => {
                                // Native decision time plus controlled transport overhead
                                // greater than the old fixed 250 ms allowance.
                                tokio::time::sleep(Duration::from_millis(wait) + delay).await;
                                peer_state.lock().unwrap().replies += 1;
                                json!({"structuredContent":pending(&expected_agent, created)})
                            }
                            ReplyMode::Never => continue,
                        }
                    }
                    "ssmcp.ui.finish" if arguments["op"] == "cancel" => {
                        peer_state.lock().unwrap().cancellations.push(arguments);
                        peer_changed.notify_one();
                        json!({"structuredContent":{"cancelled":true,"request_id":REQUEST_ID}})
                    }
                    "ssmcp.ui.question_delivery" => json!({
                        "structuredContent":{"available":true},
                        "_meta":{"ssmcp.question_answers":{"schema_version":1,
                            "session_id":expected_client,"thread_id":expected_agent,
                            "provider":"github","subject":"42","answers":[]}}
                    }),
                    _ => panic!("unexpected native tool {name}"),
                };
                send(
                    &mut socket,
                    Frame::Reply {
                        id,
                        result: Ok(output),
                    },
                )
                .await;
            }
        });
        Self {
            broker,
            token,
            client_id,
            agent_id,
            agent_token,
            native,
            changed,
            peer,
            server,
            _directory: directory,
        }
    }

    fn start(
        &self,
        timeout_ms: u64,
        idempotency_key: Option<&str>,
        request_id: Option<&str>,
    ) -> JoinHandle<Result<Value, String>> {
        let mut value = json!({"client_id":self.client_id,"reason":REASON,
            "timeout_ms":timeout_ms,"agent":{"id":self.agent_id,"token":self.agent_token}});
        if let Some(key) = idempotency_key {
            value["idempotency_key"] = json!(key);
        }
        if let Some(id) = request_id {
            value["request_id"] = json!(id);
        }
        let arguments = Arguments::parse(value).unwrap();
        tokio::spawn(execute(
            self.broker.clone(),
            "mcp".into(),
            self.token.clone(),
            arguments,
        ))
    }

    async fn wait_for_native(&self, cancelled: bool) {
        timeout(Duration::from_secs(3), async {
            loop {
                let notified = self.changed.notified();
                let ready = {
                    let state = self.native.lock().unwrap();
                    if cancelled {
                        !state.cancellations.is_empty()
                    } else {
                        !state.requests.is_empty()
                    }
                };
                if ready {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("native request or exact cancellation should arrive");
        assert!(
            !self.peer.is_finished(),
            "native websocket must remain healthy"
        );
    }
}

#[tokio::test]
async fn native_decision_budget_survives_extra_transport_delay_and_retains_pending_id() {
    for decision_ms in [1, 1000] {
        let fixture = Fixture::new(ReplyMode::Delayed(Duration::from_millis(350))).await;
        let response = timeout(
            Duration::from_secs(8),
            fixture.start(decision_ms, Some("delayed_finish"), None),
        )
        .await
        .expect("native RPC and response processing remain bounded")
        .unwrap()
        .unwrap();
        assert_ne!(response["isError"], true);
        let data = &response["structuredContent"];
        assert_eq!(data["request_id"], REQUEST_ID);
        assert_eq!(data["request"]["id"], REQUEST_ID);
        assert_eq!(data["created"], true);
        assert_eq!(data["timed_out"], true);
        assert_eq!(data["default_decision"], "no");
        assert!(data["decision"].is_null());
        assert_eq!(data["approved"], false);
        assert_eq!(data["request"]["finish"]["status"], "pending");
        assert!(data["request"]["finish"]["decision"].is_null());
        assert_eq!(data["request"]["finish"]["approved"], false);
        let resumed = timeout(
            Duration::from_secs(8),
            fixture.start(1, None, Some(REQUEST_ID)),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(resumed["structuredContent"]["request_id"], REQUEST_ID);
        assert_eq!(resumed["structuredContent"]["created"], false);
        assert!(resumed["structuredContent"]["decision"].is_null());
        assert_eq!(resumed["structuredContent"]["approved"], false);
        let state = fixture.native.lock().unwrap();
        assert_eq!(state.requests.len(), 2);
        assert_eq!(state.requests[0]["timeout_ms"], decision_ms);
        assert_eq!(state.requests[1]["timeout_ms"], 1);
        assert_eq!(state.replies, 2);
        assert!(state.cancellations.is_empty());
    }
}

#[tokio::test]
async fn missing_native_reply_exhausts_transport_grace_and_cancels_only_that_request() {
    let fixture = Fixture::new(ReplyMode::Never).await;
    let started = Instant::now();
    let result = timeout(
        Duration::from_secs(9),
        fixture.start(1, Some("hung_finish"), None),
    )
    .await
    .expect("a missing native reply must not wait for the generic tool deadline")
    .unwrap();
    let error = result.unwrap_err();
    assert!(
        error.starts_with("finish_native_wait_timed_out:"),
        "{error}"
    );
    assert!(started.elapsed() >= NATIVE_RPC_GRACE);
    fixture.wait_for_native(true).await;
    let state = fixture.native.lock().unwrap();
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.requests[0]["timeout_ms"], 1);
    assert_eq!(
        state.replies, 0,
        "no synthetic pending or approval was returned"
    );
    assert_eq!(
        state.cancellations,
        [json!({"op":"cancel","idempotency_key":"hung_finish"})]
    );
}

#[tokio::test]
async fn external_cancellation_still_sends_the_exact_native_cancel() {
    let fixture = Fixture::new(ReplyMode::Never).await;
    let request = fixture.start(25_000, Some("aborted_finish"), None);
    fixture.wait_for_native(false).await;
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    fixture.wait_for_native(true).await;
    let state = fixture.native.lock().unwrap();
    assert_eq!(state.requests.len(), 1);
    assert_eq!(state.replies, 0);
    assert_eq!(
        state.cancellations,
        [json!({"op":"cancel","idempotency_key":"aborted_finish"})]
    );
}
