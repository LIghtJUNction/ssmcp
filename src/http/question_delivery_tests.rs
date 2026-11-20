//! Response-boundary tests use authenticated websocket peers and private stores.
//! The peer below models a retained native answer, not the process-global UI.
use super::*;
use crate::{
    identity::Identity,
    portal::Portal,
    wire::{self, Frame},
};
use futures_util::{SinkExt, StreamExt};
use std::{path::PathBuf, sync::Mutex};
use tokio::{net::TcpListener, task::JoinHandle, time::timeout};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NativeAnswers {
    view: Value,
    pending: bool,
    calls: Vec<String>,
    acknowledgements: Vec<Value>,
    finish_operations: Vec<Value>,
}

struct Fixture {
    broker: Arc<Broker>,
    token: String,
    session: Arc<Session>,
    owner: String,
    agent_id: String,
    agent_token: String,
    native: Arc<Mutex<NativeAnswers>>,
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

fn original() -> Value {
    json!({"content":[{"type":"text","text":"original result"}],
        "structuredContent":{"result":null},"isError":false})
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

impl Fixture {
    async fn new() -> Self {
        let mut nonce = [0_u8; 16];
        crate::platform::random_bytes(&mut nonce).unwrap();
        let nonce: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory =
            Directory(std::env::temp_dir().join(format!("ssmcp-question-delivery-{nonce}")));
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
                "{}, {}, {}",
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
                wire::messages::CAPABILITY
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
                    name: "delivery fixture".into(),
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
                .any(|c| c == wire::CONTROL_ATTRIBUTION_CAPABILITY)
        );
        assert!(capabilities.iter().any(|c| c == wire::messages::CAPABILITY));
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
            .dispatch(&owner, "join", json!({"name":"asking agent"}))
            .await
            .unwrap();
        let agent_id = joined["agent_id"].as_str().unwrap().to_owned();
        let agent_token = joined["token"].as_str().unwrap().to_owned();
        let native = Arc::new(Mutex::new(NativeAnswers {
            view: json!({"id":"retained_question","title":null,"question":"Which check should run?",
                "options":[],"state":"answered","answer":"Run the bounded native check.",
                "created_at_ms":1,"answered_at_ms":2,"kind":"question",
                "requester":{"provider":"github","subject":"42","thread_id":agent_id,"name":"asking agent"},
                "finish":null,"answer_actor":{"origin":"agent","agent":{"provider":"github","subject":"42",
                    "thread_id":"agent_11111111111111111111111111111111","name":"reviewer"}},
                "answer_revision":1,"automatic_delivery":true,"delivery":"pending"}),
            pending: true,
            calls: vec![],
            acknowledgements: vec![],
            finish_operations: vec![],
        }));
        let answers = native.clone();
        let expected_agent = agent_id.clone();
        let peer = tokio::spawn(async move {
            while let Some(frame) = receive(&mut socket).await {
                if matches!(frame, Frame::Reject { .. }) {
                    break;
                }
                let Frame::Call {
                    id,
                    operation: Operation::CallTool { name, arguments },
                    attribution,
                    ..
                } = frame
                else {
                    // Completion audit frames do not represent native operations.
                    assert!(matches!(frame, Frame::CodeModeFinished { .. }));
                    continue;
                };
                let value = {
                    let mut state = answers.lock().unwrap();
                    state.calls.push(name.clone());
                    if name == "ssmcp.client.capabilities" {
                        Ok(json!({"thread_scope":1}))
                    } else {
                        let attribution = attribution.expect("authenticated native attribution");
                        assert_eq!(attribution.account.subject, "42");
                        assert_eq!(attribution.agent.as_ref().unwrap().id, expected_agent);
                        let mut output = match name.as_str() {
                            "ssmcp.ui.finish" => {
                                state.finish_operations.push(arguments.clone());
                                assert_eq!(arguments["request_id"], state.view["id"]);
                                match arguments["op"].as_str() {
                                    Some("request") => {
                                        assert_eq!(arguments["reason"], state.view["question"]);
                                        json!({"structuredContent":{
                                            "schema_version":1,"request_id":state.view["id"],
                                            "created":false,"request":state.view,
                                            "timed_out":state.view["finish"]["status"]=="pending",
                                            "default_decision":"no",
                                            "decision":state.view["finish"]["decision"],
                                            "approved":state.view["finish"]["approved"]
                                        }})
                                    }
                                    Some("cancel") => {
                                        state.view["finish"]["status"] = json!("cancelled");
                                        state.view["finish"]["approved"] = json!(false);
                                        json!({"structuredContent":{
                                            "schema_version":1,"request_id":state.view["id"],
                                            "request":state.view,"timed_out":false
                                        }})
                                    }
                                    _ => panic!("unexpected fixture finish operation"),
                                }
                            }
                            "ssmcp.ui.questions" => {
                                json!({"structuredContent":{"schema_version":1,"question":state.view}})
                            }
                            "ssmcp.ui.question_delivery" => {
                                json!({"structuredContent":{"available":true}})
                            }
                            "ssmcp.ui.question_ack" => {
                                assert_eq!(
                                    arguments,
                                    json!({"answers":[{"id":"retained_question","revision":1}]})
                                );
                                state.acknowledgements.push(arguments.clone());
                                state.pending = false;
                                state.view["delivery"] = json!("delivered");
                                json!({"structuredContent":{"acknowledged":true}})
                            }
                            "fixture.actual" | "fixture.error" => {
                                json!({"structuredContent":{"native":"kept"}})
                            }
                            _ => panic!("unexpected fixture native tool {name}"),
                        };
                        if matches!(
                            name.as_str(),
                            "fixture.actual" | "ssmcp.ui.question_delivery"
                        ) {
                            output["_meta"] = json!({"ssmcp.question_answers":{
                                "schema_version":1,"session_id":client_id,"thread_id":expected_agent,
                                "provider":"github","subject":"42",
                                "answers":if state.pending {vec![state.view.clone()]} else {vec![]}
                            }});
                        }
                        if name == "fixture.error" {
                            Err("synthetic native failure".into())
                        } else {
                            Ok(output)
                        }
                    }
                };
                send(&mut socket, Frame::Reply { id, result: value }).await;
            }
        });
        Self {
            broker,
            token,
            session,
            owner,
            agent_id,
            agent_token,
            native,
            peer,
            server,
            _directory: directory,
        }
    }

    fn run(&self) -> code_mode::AuditRun {
        let run = code_mode::AuditRun::new("return null;", None).unwrap();
        run.record_connection(&self.session).unwrap();
        run.bind(
            self.session.client_id(),
            &self.owner,
            &self.agent_id,
            &self.agent_token,
        )
        .unwrap();
        run
    }

    fn report(&self, run: code_mode::AuditRun) -> CodeRunReport {
        CodeRunReport {
            broker: self.broker.clone(),
            user: "mcp".into(),
            access_token: self.token.clone(),
            run,
            delivered: true,
        }
    }

    async fn native_call(&self, run: code_mode::AuditRun, name: &str) -> Result<Value, String> {
        let host = code_host(
            self.broker.clone(),
            "mcp".into(),
            self.token.clone(),
            run,
            None,
            "delivery fixture".into(),
        );
        host(HostRequest::Call {
            client_id: self.session.client_id().into(),
            name: name.into(),
            arguments: if name == "ssmcp.ui.questions" {
                json!({"id":"retained_question"})
            } else {
                json!({})
            },
        })
        .await
    }

    async fn wait_for_ack(&self) {
        timeout(Duration::from_secs(3), async {
            loop {
                if !self.native.lock().unwrap().pending {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("exact native acknowledgement should follow included answer");
        assert!(!self.peer.is_finished(), "fixture peer must remain healthy");
    }

    async fn send_message(&self, index: usize) -> String {
        let response = self.broker.message_request("mcp",&self.token,json!({
            "client_id":self.session.client_id(),"action":"send",
            "recipient_id":format!("agent:{}:{}",self.session.client_id(),self.agent_id),
            "text":format!("native mailbox item {index}"),"request_id":format!("fixture_message_{index}")
        })).await.unwrap();
        response["receipts"][0]["message_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    async fn message_status(&self, id: &str) -> Value {
        let response = self
            .broker
            .message_request(
                "mcp",
                &self.token,
                json!({
                    "client_id":self.session.client_id(),"action":"status","message_id":id
                }),
            )
            .await
            .unwrap();
        response["receipts"][0]["status"].clone()
    }
}

#[tokio::test]
async fn explicit_question_readback_keeps_the_answer_until_a_bound_next_response() {
    let fixture = Fixture::new().await;
    let read = fixture.run();
    let native = fixture
        .native_call(read.clone(), "ssmcp.ui.questions")
        .await
        .unwrap();
    assert_eq!(
        native["structuredContent"]["question"]["answer_revision"],
        1
    );
    let response = finish_user_response(&fixture.report(read), original()).await;
    assert!(
        response["structuredContent"]
            .get("question_answers")
            .is_none()
    );
    {
        let state = fixture.native.lock().unwrap();
        assert!(state.pending);
        assert!(state.acknowledgements.is_empty());
        assert!(
            !state
                .calls
                .iter()
                .any(|name| name == "ssmcp.ui.question_delivery")
        );
    }
    let next = fixture.report(fixture.run());
    let response = finish_user_response(&next, original()).await;
    let answer = &response["structuredContent"]["question_answers"][0];
    assert_eq!(response["structuredContent"]["result"], Value::Null);
    assert_eq!(answer["id"], "retained_question");
    assert_eq!(answer["answer_revision"], 1);
    assert_eq!(answer["answer_actor"]["origin"], "agent");
    assert_eq!(answer["delivery"], "delivered");
    assert_eq!(answer["client_id"], fixture.session.client_id());
    assert!(response["structuredContent"].get("user_messages").is_none());
    fixture.wait_for_ack().await;
    let later = finish_user_response(&fixture.report(fixture.run()), original()).await;
    assert!(later["structuredContent"].get("question_answers").is_none());
    assert_eq!(fixture.native.lock().unwrap().acknowledgements.len(), 1);
}

#[tokio::test]
async fn native_errors_still_deliver_verified_answers_without_rewriting_the_error() {
    let fixture = Fixture::new().await;
    let run = fixture.run();
    assert!(
        fixture
            .native_call(run.clone(), "fixture.error")
            .await
            .is_err()
    );
    let original = code_mode::tool_result(Err("synthetic native failure".into()));
    let response = finish_user_response(&fixture.report(run), original.clone()).await;
    assert_eq!(response["isError"], true);
    assert_eq!(response["content"][0], original["content"][0]);
    assert_eq!(
        response["structuredContent"]["question_answers"][0]["id"],
        "retained_question"
    );
    fixture.wait_for_ack().await;
}

#[tokio::test]
async fn question_finalization_rechecks_oauth_policy_binding_and_connection_after_eligibility() {
    for failure in ["oauth", "ban", "agent", "connection"] {
        let fixture = Fixture::new().await;
        let run = fixture.run();
        let native = fixture
            .native_call(run.clone(), "fixture.actual")
            .await
            .unwrap();
        assert!(
            native
                .get("_meta")
                .is_none_or(|meta| meta.get("ssmcp.question_answers").is_none()),
            "trusted native metadata must be stripped before returning to JavaScript"
        );
        let report = fixture.report(run);
        let eligible = eligible_user_response(&report).await.unwrap();
        assert_eq!(eligible.len(), 1);
        match failure {
            "oauth" => fixture.broker.oauth.revoke_subject("42").unwrap(),
            "ban" => {
                fixture.broker.user_policies.set("42", true, None);
            }
            "agent" => {
                fixture
                    .session
                    .collaboration
                    .dispatch(
                        &fixture.owner,
                        "leave",
                        json!({"agent_id":fixture.agent_id,"token":fixture.agent_token}),
                    )
                    .await
                    .unwrap();
            }
            "connection" => fixture.broker.disconnect_all().await,
            _ => unreachable!(),
        }
        let response = commit_user_response(&report, original(), eligible);
        assert!(
            response["structuredContent"]
                .get("question_answers")
                .is_none(),
            "{failure}"
        );
        assert!(
            response["structuredContent"]
                .get("native_messages")
                .is_none(),
            "{failure}"
        );
        let state = fixture.native.lock().unwrap();
        assert!(state.pending, "{failure}");
        assert!(state.acknowledgements.is_empty(), "{failure}");
    }
}

#[tokio::test]
async fn finalizer_validates_the_envelope_and_budget_before_claiming_exact_typed_batches() {
    let fixture = Fixture::new().await;
    let mut message_ids = Vec::new();
    for index in 0..33 {
        message_ids.push(fixture.send_message(index).await);
    }
    let run = fixture.run();
    fixture
        .native_call(run.clone(), "fixture.actual")
        .await
        .unwrap();
    let report = fixture.report(run);
    for response in [
        json!({"content":"invalid content shape","structuredContent":{}}),
        json!({"content":[],"structuredContent":"invalid structured shape"}),
        json!({"content":[{"type":"text","text":"x".repeat(crate::wire::MAX_FRAME-8192)}],
            "structuredContent":{"result":null}}),
    ] {
        let eligible = eligible_user_response(&report).await.unwrap();
        let result = commit_user_response(&report, response.clone(), eligible);
        assert_eq!(
            result, response,
            "ineligible envelope must remain unchanged"
        );
        assert!(fixture.native.lock().unwrap().pending);
        assert!(fixture.native.lock().unwrap().acknowledgements.is_empty());
        assert_eq!(fixture.message_status(&message_ids[0]).await, "queued");
    }
    let eligible = eligible_user_response(&report).await.unwrap();
    let response = commit_user_response(&report, original(), eligible);
    assert_eq!(
        response["structuredContent"]["question_answers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        response["structuredContent"]["native_messages"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
    let content = response["content"].as_array().unwrap();
    let question_position = content
        .iter()
        .position(|item| {
            item["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("QUESTION ANSWERS"))
        })
        .unwrap();
    let native_position = content
        .iter()
        .position(|item| {
            item["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("NATIVE MESSAGES"))
        })
        .unwrap();
    assert!(question_position < native_position);
    assert!(response.to_string().len() < crate::wire::MAX_FRAME);
    fixture.wait_for_ack().await;
    let included: Vec<_> = response["structuredContent"]["native_messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            message["receipt"]["message_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        included
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        32
    );
    let remaining = message_ids
        .iter()
        .find(|id| !included.contains(id))
        .unwrap();
    assert_eq!(fixture.message_status(remaining).await, "queued");
    let next = finish_user_response(&fixture.report(fixture.run()), original()).await;
    assert!(next["structuredContent"].get("question_answers").is_none());
    assert_eq!(
        next["structuredContent"]["native_messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        next["structuredContent"]["native_messages"][0]["receipt"]["message_id"],
        *remaining
    );
}

mod finish_receipts;
