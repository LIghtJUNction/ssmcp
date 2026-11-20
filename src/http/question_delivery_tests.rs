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
use tokio::{net::TcpListener, sync::Notify, task::JoinHandle, time::timeout};
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
    summary_calls: u64,
    summary_reminded: bool,
    exec_running: bool,
    summary_probe_pause: Option<(Arc<Notify>, Arc<Notify>)>,
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
            summary_calls: 0,
            summary_reminded: false,
            exec_running: false,
            summary_probe_pause: None,
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
                    code_context,
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
                        if matches!(name.as_str(), "ssmcp.fs.stat" | "ssmcp.exec.wait" | "ssmcp.ui.tool_summary") {
                            assert_eq!(
                                code_context.as_ref().and_then(|context| context.thread_id.as_deref()),
                                Some(expected_agent.as_str()),
                            );
                        }
                        let mut output = match name.as_str() {
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
                            "ssmcp.fs.stat" => json!({"structuredContent":{"exists":true}}),
                            "ssmcp.exec.wait" => {
                                assert_eq!(arguments["job_id"], "summary-job");
                                json!({"structuredContent":{"job_id":"summary-job","running":state.exec_running}})
                            }
                            "ssmcp.ui.tool_summary" => {
                                assert_eq!(arguments, json!({"action":"list"}));
                                json!({"structuredContent":{"schema_version":1,"threshold":10,
                                    "groups":[{"group_id":"await-fixture-group","revision":0,
                                        "call_count":state.summary_calls,"covered_call_count":0}],
                                    "has_more":false,"truncated":false}})
                            }
                            _ => panic!("unexpected fixture native tool {name}"),
                        };
                        if matches!(
                            name.as_str(),
                            "fixture.actual" | "ssmcp.ui.question_delivery" | "ssmcp.exec.wait"
                        ) {
                            output["_meta"] = json!({"ssmcp.question_answers":{
                                "schema_version":1,"session_id":client_id,"thread_id":expected_agent,
                                "provider":"github","subject":"42",
                                "answers":if state.pending {vec![state.view.clone()]} else {vec![]}
                            }});
                        }
                        if matches!(name.as_str(), "ssmcp.fs.stat" | "ssmcp.exec.wait") {
                            state.summary_calls += 1;
                            if state.summary_calls > 10 && !state.summary_reminded {
                                state.summary_reminded = true;
                                output["_meta"]["ssmcp.tool_summary"] = json!({
                                    "schema_version":1,"group_id":"await-fixture-group","revision":0,
                                    "call_count":state.summary_calls,"covered_call_count":0,
                                    "threshold":10,"tool":"ssmcp.ui.tool_summary"
                                });
                            }
                        }
                        if name == "fixture.error" {
                            Err("synthetic native failure".into())
                        } else {
                            Ok(output)
                        }
                    }
                };
                let pause = if name == "ssmcp.ui.tool_summary" {
                    answers.lock().unwrap().summary_probe_pause.take()
                } else {
                    None
                };
                if let Some((arrived, resume)) = pause {
                    arrived.notify_one();
                    resume.notified().await;
                }
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
            } else if name == "ssmcp.fs.stat" {
                json!({"path":"summary-fixture"})
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


// Passive await shares the authenticated native wire, but must not opt into
// the other delivery channels merely because its eleventh call earns a summary.
impl Fixture {
    async fn ten_summary_calls(&self) {
        let run = self.run();
        for _ in 0..10 {
            self.native_call(run.clone(), "ssmcp.fs.stat").await.unwrap();
        }
        assert_eq!(self.native.lock().unwrap().summary_calls, 10);
        assert!(run.tool_summary_notices(self.session.client_id(), Some(&self.agent_id)).is_empty());
        assert!(!self.session.tool_summary_advertised());
    }

    fn await_arguments(&self, mut arguments: Value) -> await_tool::Arguments {
        arguments["client_id"] = json!(self.session.client_id());
        arguments["agent"] = json!({"id":self.agent_id,"token":self.agent_token});
        await_tool::Arguments::parse(arguments).unwrap()
    }

    async fn public_await(&self, arguments: Value) -> Value {
        timeout(
            Duration::from_secs(5),
            await_tool::execute(
                self.broker.clone(),
                "mcp".into(),
                self.token.clone(),
                self.await_arguments(arguments),
            ),
        )
        .await
        .expect("bounded passive observation must finish")
        .unwrap()
    }

    fn publish_summary_requirement(&self) {
        self.session.steering.command(
            self.session.client_id(),
            self.session.collaboration.presence().unwrap(),
            wire::SteeringCommand::Publish {
                command_id: "publish-summary-requirement".into(),
                message_id: "summary-requirement".into(),
                expected_revision: 0,
                text: "Keep this user requirement queued during passive observation.".into(),
                target: self.agent_id.clone(),
            },
        ).unwrap();
    }

    fn summary_requirement_state(&self) -> wire::SteeringState {
        self.session.steering.command(
            self.session.client_id(),
            vec![],
            wire::SteeringCommand::Snapshot {},
        ).unwrap().messages.into_iter()
            .find(|message| message.id == "summary-requirement").unwrap().state
    }

    fn assert_summary_observation_did_not_consume_questions(&self) {
        let state = self.native.lock().unwrap();
        assert!(state.pending);
        assert!(state.acknowledgements.is_empty());
        assert!(!state.calls.iter().any(|name| matches!(
            name.as_str(), "ssmcp.ui.question_delivery" | "ssmcp.ui.question_ack"
        )));
        assert_eq!(self.summary_requirement_state(), wire::SteeringState::Queued);
    }
}

fn assert_await_summary(response: &Value, client_id: &str) {
    let notices = response["structuredContent"]["tool_summary_reminders"].as_array().unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["client_id"], client_id);
    assert_eq!(notices[0]["group_id"], "await-fixture-group");
    assert_eq!(notices[0]["call_count"], 11);
    assert_eq!(notices[0]["covered_call_count"], 0);
    assert_eq!(notices[0]["revision"], 0);
    assert_eq!(notices[0]["threshold"], 10);
    assert!(response["content"].as_array().unwrap().iter().any(|part|
        part["text"].as_str().is_some_and(|text| text.starts_with("TOOL SUMMARY REMINDER"))));
    assert!(response.to_string().len() < wire::MAX_FRAME);
}

#[tokio::test]
async fn tool_summary_public_await_delivers_the_eleventh_call_without_consuming_user_input() {
    let fixture = Fixture::new().await;
    fixture.ten_summary_calls().await;
    fixture.publish_summary_requirement();
    let inbox_id = fixture.send_message(0).await;

    let response = fixture.public_await(json!({
        "targets":[{"type":"exec","job_id":"summary-job"}],"timeout_ms":1000
    })).await;
    assert_eq!(response["structuredContent"]["status"], "ready");
    assert_eq!(response["structuredContent"]["results"][0]["data"]["job_id"], "summary-job");
    assert_await_summary(&response, fixture.session.client_id());
    assert!(fixture.session.tool_summary_advertised());
    for field in ["user_messages", "question_answers", "native_messages", "conversation_reminders"] {
        assert!(response["structuredContent"].get(field).is_none(), "{field}");
    }
    fixture.assert_summary_observation_did_not_consume_questions();
    assert_eq!(fixture.message_status(&inbox_id).await, "queued");
    let state = fixture.native.lock().unwrap();
    assert_eq!(state.summary_calls, 11);
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.ui.tool_summary").count(), 1);
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.exec.wait").count(), 1);
}

#[tokio::test]
async fn tool_summary_code_mode_wait_for_keeps_the_native_delivery_boundary() {
    let fixture = Fixture::new().await;
    fixture.ten_summary_calls().await;
    let authorization = fixture.broker.authenticate("mcp", &fixture.token).await.unwrap();
    let arguments: CodeArguments = serde_json::from_value(json!({
        "code":format!("return await waitFor({},{{targets:[{{type:'exec',job_id:'summary-job'}}],timeout_ms:1000}});",
            serde_json::to_string(fixture.session.client_id()).unwrap()),
        "timeout_ms":10000,
        "agent":{"client_id":fixture.session.client_id(),"id":fixture.agent_id,"token":fixture.agent_token}
    })).unwrap();
    let response = execute(
        fixture.broker.clone(), "mcp".into(), authorization, fixture.token.clone(), arguments,
    ).await.unwrap();
    assert_eq!(response["structuredContent"]["result"]["status"], "ready");
    assert_await_summary(&response, fixture.session.client_id());
    fixture.wait_for_ack().await;
    let state = fixture.native.lock().unwrap();
    assert_eq!(state.summary_calls, 11);
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.exec.wait").count(), 1);
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.ui.tool_summary").count(), 1);
}

#[tokio::test]
async fn tool_summary_passive_await_rechecks_oauth_and_binding_after_probe() {
    for revoked in ["oauth", "agent"] {
        let fixture = Fixture::new().await;
        fixture.ten_summary_calls().await;
        fixture.publish_summary_requirement();
        let arrived = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        fixture.native.lock().unwrap().summary_probe_pause = Some((arrived.clone(), resume.clone()));
        let broker = fixture.broker.clone();
        let token = fixture.token.clone();
        let arguments = fixture.await_arguments(json!({
            "targets":[{"type":"exec","job_id":"summary-job"}],"timeout_ms":1000
        }));
        let observing = tokio::spawn(async move {
            await_tool::execute(broker, "mcp".into(), token, arguments).await.unwrap()
        });
        timeout(Duration::from_secs(3), arrived.notified()).await
            .expect("the final native summary probe must reach the authenticated peer");
        match revoked {
            "oauth" => fixture.broker.oauth.revoke_subject("42").unwrap(),
            "agent" => {
                fixture.session.collaboration.dispatch(
                    &fixture.owner, "leave",
                    json!({"agent_id":fixture.agent_id,"token":fixture.agent_token}),
                ).await.unwrap();
            }
            _ => unreachable!(),
        }
        resume.notify_one();
        let response = timeout(Duration::from_secs(3), observing).await.unwrap().unwrap();
        assert_eq!(response["structuredContent"]["status"], "unauthorized", "{revoked}");
        assert!(response["structuredContent"]["results"][0].get("data").is_none(), "{revoked}");
        assert!(response["structuredContent"].get("tool_summary_reminders").is_none(), "{revoked}");
        fixture.assert_summary_observation_did_not_consume_questions();
        assert_eq!(fixture.native.lock().unwrap().summary_calls, 11);
    }
}

#[tokio::test]
async fn tool_summary_passive_timeout_resume_and_cancel_do_not_redeliver_or_consume_input() {
    let fixture = Fixture::new().await;
    fixture.ten_summary_calls().await;
    fixture.publish_summary_requirement();
    let inbox_id = fixture.send_message(0).await;
    fixture.native.lock().unwrap().exec_running = true;

    let first = fixture.public_await(json!({
        "targets":[{"type":"exec","job_id":"summary-job"}],"timeout_ms":1000
    })).await;
    assert_eq!(first["structuredContent"]["status"], "timed_out");
    assert_eq!(first["structuredContent"]["continue_wait"], true);
    assert_await_summary(&first, fixture.session.client_id());
    let wait_id = first["structuredContent"]["wait_id"].clone();

    let resumed = fixture.public_await(json!({"wait_id":wait_id,"timeout_ms":1000})).await;
    assert_eq!(resumed["structuredContent"]["status"], "timed_out");
    assert_eq!(resumed["structuredContent"]["wait_id"], wait_id);
    assert!(resumed["structuredContent"].get("tool_summary_reminders").is_none());
    for _ in 0..2 {
        let cancelled = fixture.public_await(json!({"wait_id":wait_id,"op":"cancel"})).await;
        assert_eq!(cancelled["structuredContent"]["status"], "cancelled");
        assert_eq!(cancelled["structuredContent"]["wait_id"], wait_id);
        assert!(cancelled["structuredContent"].get("tool_summary_reminders").is_none());
    }
    fixture.assert_summary_observation_did_not_consume_questions();
    assert_eq!(fixture.message_status(&inbox_id).await, "queued");
    let state = fixture.native.lock().unwrap();
    assert_eq!(state.summary_calls, 12, "only the resumed native wait is a new logical call");
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.ui.tool_summary").count(), 1);
    assert_eq!(state.calls.iter().filter(|name| name.as_str() == "ssmcp.exec.wait").count(), 2);
}
