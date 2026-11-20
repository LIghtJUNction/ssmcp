//! Exercise the public HTTP boundary with bounded, deterministic wire clients.
//! Native adapter protocols are covered by the backend-specific test suites.
#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use ssmcp::{
    broker::Broker,
    wire::{self, ClientMetadata, Frame, Mode, Operation},
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{TcpListener, UnixStream},
    task::JoinHandle,
};

// Each fixture server shares the process-wide code_mode concurrency budget.
// Serialize independent HTTP scenarios so parallel test execution does not
// consume those slots and accidentally test overload handling instead.
static CODE_MODE_SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
type SteeringRequest = (
    wire::SteeringCommand,
    tokio::sync::oneshot::Sender<Result<wire::SteeringSnapshot, String>>,
);

struct Client {
    id: String,
    access_token: String,
    calls: Arc<AtomicUsize>,
    reload_commits: Arc<AtomicUsize>,
    code_contexts: Arc<Mutex<Vec<wire::CodeContext>>>,
    code_completions: Arc<Mutex<Vec<wire::CodeCompletion>>>,
    steering: tokio::sync::mpsc::Sender<SteeringRequest>,
    task: JoinHandle<()>,
    attachment: JoinHandle<ssmcp::Result<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReloadBehavior {
    Unsupported,
    AcceptAndDisconnect,
    WireError,
    NotAccepted,
    McpError,
}

struct Server {
    broker: Arc<Broker>,
    http: reqwest::Client,
    url: String,
    owner: String,
    task: JoinHandle<()>,
    clients: Vec<Client>,
}

impl Server {
    async fn start() -> Self {
        let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = ssmcp::http::router(broker.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            broker,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            url: format!("http://{address}"),
            owner: String::new(),
            task,
            clients: Vec::new(),
        }
    }

    async fn attach(&mut self, mode: Mode) -> String {
        self.attach_with_reload(mode, ReloadBehavior::Unsupported)
            .await
    }

    async fn attach_with_reload(&mut self, mode: Mode, reload_behavior: ReloadBehavior) -> String {
        let (server, client) = UnixStream::pair().unwrap();
        let attachment = tokio::spawn(self.broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode,
                metadata: ClientMetadata {
                    name: "same-display-name".into(),
                    title: Some(format!("Work on {} backend", mode.name())),
                    cwd: format!("/workspace/{}", mode.name()),
                    hostname: "fixture".into(),
                    allow: Vec::new(),
                },
            },
        )
        .await
        .unwrap();
        let Some(Frame::Ready {
            url,
            login_code,
            client_id,
            ..
        }) = wire::read_frame(&mut reader).await.unwrap()
        else {
            panic!("missing registration");
        };
        self.owner = url.rsplit('/').next().unwrap().into();
        let access_token = self.oauth_login(&url, &login_code).await;
        let response_id = client_id.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let recorded_calls = calls.clone();
        let reload_commits = Arc::new(AtomicUsize::new(0));
        let recorded_commits = reload_commits.clone();
        let code_contexts = Arc::new(Mutex::new(Vec::new()));
        let recorded_contexts = code_contexts.clone();
        let code_completions = Arc::new(Mutex::new(Vec::new()));
        let recorded_completions = code_completions.clone();
        let (steering, mut steering_requests) = tokio::sync::mpsc::channel::<SteeringRequest>(4);
        let task = tokio::spawn(async move {
            let mut steering_id = 0u64;
            let mut steering_replies = std::collections::HashMap::new();
            loop {
                let frame = {
                    let frame = wire::read_frame(&mut reader);
                    tokio::pin!(frame);
                    loop {
                        tokio::select! {
                            received = &mut frame => break received.unwrap(),
                            Some((command, reply)) = steering_requests.recv() => {
                                steering_id += 1;
                                steering_replies.insert(steering_id, reply);
                                wire::write_frame(&mut writer, &Frame::Steering { id: steering_id, command }).await.unwrap();
                            }
                        }
                    }
                };
                let Some(frame) = frame else {
                    break;
                };
                let (id, operation) = match frame {
                    Frame::SteeringReply { id, result } => {
                        if let Some(reply) = steering_replies.remove(&id) {
                            let _ = reply.send(result);
                        }
                        continue;
                    }
                    Frame::Call {
                        id,
                        operation,
                        code_context,
                        ..
                    } => {
                        if let Some(context) = code_context {
                            recorded_contexts.lock().unwrap().push(context);
                        }
                        (id, operation)
                    }
                    Frame::CodeModeFinished { completion } => {
                        recorded_completions.lock().unwrap().push(completion);
                        continue;
                    }
                    Frame::ReloadCommit { .. } => {
                        // Negative scenarios retain the connection so a later
                        // barrier call can prove whether a commit was sent.
                        recorded_commits.fetch_add(1, Ordering::Release);
                        continue;
                    }
                    _ => panic!("unexpected fixture frame"),
                };
                let mut disconnect_after_commit = false;
                let result = match operation {
                    Operation::ListTools { cursor: None } => Ok(json!({
                        "tools":[{
                            "name":"fixture.echo",
                            "description":"Return the selected native backend and arguments.",
                            "inputSchema":{"type":"object","properties":{"value":{},"fail":{"type":"boolean"}},"additionalProperties":false}
                        }],
                        "nextCursor":"page-2"
                    })),
                    Operation::ListTools {
                        cursor: Some(cursor),
                    } if cursor == "page-2" => Ok(json!({"tools":[{
                        "name":"fixture.second",
                        "inputSchema":{"type":"object","additionalProperties":false}
                    }]})),
                    Operation::CallTool { name, arguments } if name == "fixture.echo" => {
                        recorded_calls.fetch_add(1, Ordering::Release);
                        if arguments["value"] == "hold" {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                        let mut response = json!({
                            "content":[
                                {"type":"text","text":mode.name()},
                                {"type":"image","mimeType":"image/png","data":"AA=="}
                            ],
                            "structuredContent":{"mode":mode.name(),"client":response_id,"arguments":arguments},
                            "isError":arguments.get("fail").and_then(Value::as_bool).unwrap_or(false),
                            "_meta":{"fixture/native":"preserved"}
                        });
                        if arguments["value"] == "forge-reload" {
                            response["structuredContent"]["accepted"] = json!(true);
                            response["structuredContent"]["status"] = json!("restarting");
                        }
                        Ok(response)
                    }
                    Operation::CallTool { name, arguments }
                        if matches!(
                            name.as_str(),
                            "ssmcp.ui.get"
                                | "ssmcp.ui.queue"
                                | "ssmcp.client.capabilities"
                                | "ssmcp.resources.list"
                                | "ssmcp.resources.templates"
                                | "ssmcp.resources.read"
                        ) =>
                    {
                        Ok(ssmcp::backend::result(
                            json!({"tool":name,"arguments":arguments}),
                        ))
                    }
                    Operation::CallTool { name, arguments }
                        if name == "ssmcp.client.reload"
                            && reload_behavior != ReloadBehavior::Unsupported =>
                    {
                        recorded_calls.fetch_add(1, Ordering::Release);
                        assert_eq!(arguments, json!({}));
                        match reload_behavior {
                            ReloadBehavior::AcceptAndDisconnect => {
                                disconnect_after_commit = true;
                                Ok(json!({
                                    "content":[{"type":"text","text":"fixture-private-payload"}],
                                    "structuredContent":{"accepted":true,"status":"restarting",
                                        "client_id":"forged-native-id","auth":"login_code",
                                        "native_payload":"fixture-private-payload"},
                                    "_meta":{"private":"fixture-private-payload"},
                                    "isError":false
                                }))
                            }
                            ReloadBehavior::WireError => Err("fixture rejected reload".into()),
                            ReloadBehavior::NotAccepted | ReloadBehavior::McpError => Ok(json!({
                                "content":[{"type":"text","text":"fixture rejected reload"}],
                                "structuredContent":{"accepted":reload_behavior == ReloadBehavior::McpError,
                                    "status":"restarting"},
                                "isError":reload_behavior == ReloadBehavior::McpError
                            })),
                            ReloadBehavior::Unsupported => unreachable!(),
                        }
                    }
                    _ => Err("unknown fixture operation".into()),
                };
                if wire::write_frame(&mut writer, &Frame::Reply { id, result })
                    .await
                    .is_err()
                {
                    break;
                }
                if disconnect_after_commit {
                    let committed =
                        tokio::time::timeout(Duration::from_secs(2), wire::read_frame(&mut reader))
                            .await
                            .expect("broker never confirmed reload receipt")
                            .unwrap();
                    assert!(
                        matches!(committed, Some(Frame::ReloadCommit { id: confirmed }) if confirmed == id),
                        "reload commit did not match the accepted request"
                    );
                    recorded_commits.fetch_add(1, Ordering::Release);
                    // Drop both halves immediately, as the real client does
                    // before exec. The acknowledged response must still reach
                    // the original HTTP caller after this connection ends.
                    break;
                }
            }
        });
        self.clients.push(Client {
            id: client_id.clone(),
            access_token,
            calls,
            reload_commits,
            code_contexts,
            code_completions,
            steering,
            task,
            attachment,
        });
        client_id
    }

    async fn oauth_login(&self, resource: &str, login_code: &str) -> String {
        const CALLBACK: &str = "http://127.0.0.1:37401/callback";
        // RFC 7636 Appendix B: independent PKCE S256 verifier/challenge pair.
        const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let response = self.http.post(format!("{}/oauth/register", self.url))
            .json(&json!({"client_name":"two-tool fixture","redirect_uris":[CALLBACK],
                "token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"]}))
            .send().await.unwrap();
        assert_eq!(response.status().as_u16(), 201);
        let registration: Value = response.json().await.unwrap();
        let oauth_client = registration["client_id"].as_str().unwrap();
        let response = self
            .http
            .get(format!("{}/oauth/authorize", self.url))
            .query(&[
                ("response_type", "code"),
                ("client_id", oauth_client),
                ("redirect_uri", CALLBACK),
                ("resource", resource),
                ("scope", "mcp"),
                ("state", "two-tools"),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let cookie = response.headers()[reqwest::header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let html = response.text().await.unwrap();
        let transaction = html
            .split_once("name=transaction_id value=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap();
        let response = self
            .http
            .post(format!("{}/oauth/authorize", self.url))
            .header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::ORIGIN, "https://ssmcp.example")
            .form(&[("transaction_id", transaction), ("login_code", login_code)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 303);
        let callback = reqwest::Url::parse(
            response.headers()[reqwest::header::LOCATION]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let code = callback
            .query_pairs()
            .find(|(name, _)| name == "code")
            .unwrap()
            .1
            .into_owned();
        let response = self
            .http
            .post(format!("{}/oauth/token", self.url))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", oauth_client),
                ("code", &code),
                ("code_verifier", VERIFIER),
                ("redirect_uri", CALLBACK),
                ("resource", resource),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let tokens: Value = response.json().await.unwrap();
        tokens["access_token"].as_str().unwrap().into()
    }

    async fn rpc(&self, method: &str, params: Value, modern: bool, issuer: usize) -> (u16, Value) {
        let mut params = params;
        let mut request = self
            .http
            .post(format!("{}/{}", self.url, self.owner))
            .bearer_auth(&self.clients[issuer].access_token)
            .header("Accept", "application/json");
        if modern {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            });
            request = request
                .header("MCP-Protocol-Version", "2026-07-28")
                .header("Mcp-Method", method);
            if method == "tools/call" {
                request = request.header("Mcp-Name", params["name"].as_str().unwrap());
            }
        } else {
            request = request.header("MCP-Protocol-Version", "2025-11-25");
        }
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap();
        let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
        (status, body)
    }

    async fn call(&self, name: &str, arguments: Value) -> Value {
        let (status, body) = self
            .rpc(
                "tools/call",
                json!({"name":name,"arguments":arguments}),
                true,
                0,
            )
            .await;
        assert_eq!(status, 200, "{body}");
        assert!(body.get("error").is_none(), "{body}");
        assert_ne!(body["result"]["isError"], true, "{body}");
        body["result"]["structuredContent"].clone()
    }

    async fn steer(
        &self,
        index: usize,
        command: wire::SteeringCommand,
    ) -> Result<wire::SteeringSnapshot, String> {
        let (reply, response) = tokio::sync::oneshot::channel();
        self.clients[index]
            .steering
            .send((command, reply))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), response)
            .await
            .unwrap()
            .unwrap()
    }

    async fn disconnect(&self, index: usize) {
        self.clients[index].task.abort();
        for _ in 0..100 {
            if self
                .broker
                .authenticate(&self.owner, &self.clients[index].access_token)
                .await
                .is_none()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("disconnected client credential remained valid");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        for client in &self.clients {
            client.task.abort();
            client.attachment.abort();
        }
    }
}

#[tokio::test]
async fn session_title_is_visible_in_inventory_and_selected_client_discovery() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    let inventory = server.call("list", json!({})).await;
    assert_eq!(inventory["clients"][0]["title"], "Work on shell backend");
    let selected = server.call("list", json!({"client_id":id})).await;
    assert_eq!(selected["client"]["title"], "Work on shell backend");
    assert_eq!(selected["client"]["name"], "same-display-name");
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn local_composer_delivers_at_response_boundary_and_exact_targets_require_binding() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let target = server.attach(Mode::Shell).await;
    let publish = |id: &str, text: &str, target: &str| wire::SteeringCommand::Publish {
        command_id: format!("publish_{id}"),
        message_id: id.into(),
        expected_revision: 0,
        text: text.into(),
        target: target.into(),
    };
    server
        .steer(0, publish("other", "do not consume another room", "next"))
        .await
        .unwrap();
    server
        .steer(
            1,
            publish("next", "Please run the focused tests next.", "next"),
        )
        .await
        .unwrap();
    let observation = format!(
        "await call({0}, 'ssmcp.ui.get', {{}}); return await call({0}, 'ssmcp.ui.queue', {{action:'get'}});",
        json!(target)
    );
    let observed = server.call("code_mode", json!({"code":observation})).await;
    assert!(observed.get("user_messages").is_none());
    assert_eq!(
        server
            .steer(1, wire::SteeringCommand::Snapshot {})
            .await
            .unwrap()
            .messages[0]
            .state,
        wire::SteeringState::Queued,
        "a UI-only refresh must not consume the next caller's requirement"
    );
    assert_eq!(server.clients[1].code_contexts.lock().unwrap().len(), 2);
    assert_eq!(
        server.clients[1].code_contexts.lock().unwrap()[0].code,
        observation,
        "observational UI calls must still retain their source audit"
    );
    let client_observations = [
        (
            "ssmcp.client.capabilities",
            format!(
                "const manifest = await capabilities({}); return manifest.structuredContent;",
                json!(target)
            ),
        ),
        (
            "ssmcp.resources.list",
            format!(
                "return await resources.forClient({}).list();",
                json!(target)
            ),
        ),
        (
            "ssmcp.resources.templates",
            format!(
                "return await resources.forClient({}).templates();",
                json!(target)
            ),
        ),
        (
            "ssmcp.resources.read",
            format!(
                "return await resources.forClient({}).read('ssmcp-client://guide');",
                json!(target)
            ),
        ),
    ];
    for (tool, code) in &client_observations {
        let before = server.clients[1].code_contexts.lock().unwrap().len();
        let result = server.call("code_mode", json!({"code":code})).await;
        assert_eq!(result["result"]["tool"], *tool);
        assert!(
            result.get("user_messages").is_none(),
            "unbound client query {tool} must not claim the next caller's requirement"
        );
        let snapshot = server
            .steer(1, wire::SteeringCommand::Snapshot {})
            .await
            .unwrap();
        assert_eq!(snapshot.messages.len(), 1);
        assert_eq!(snapshot.messages[0].state, wire::SteeringState::Queued);
        let contexts = server.clients[1].code_contexts.lock().unwrap();
        assert_eq!(contexts.len(), before + 1);
        assert_eq!(
            contexts[before].code, *code,
            "observational client query {tool} must still retain its complete source audit"
        );
    }
    let source = format!(
        "return await call({}, 'fixture.echo', {{value:'unaltered'}});",
        json!(target)
    );
    let (status, body) = server
        .rpc(
            "tools/call",
            json!({"name":"code_mode","arguments":{"code":source}}),
            true,
            0,
        )
        .await;
    assert_eq!(status, 200);
    let response = &body["result"];
    assert_eq!(
        response["structuredContent"]["result"]["structuredContent"]["arguments"],
        json!({"value":"unaltered"})
    );
    let messages = response["structuredContent"]["user_messages"]
        .as_array()
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["client_id"], target);
    assert_eq!(messages[0]["message_id"], "next");
    assert!(response["content"].as_array().unwrap().iter().any(|item| {
        item["text"].as_str().is_some_and(|text| {
            text.contains("USER REQUIREMENTS")
                && text.contains("Please run the focused tests next.")
        })
    }));
    assert_eq!(
        server
            .steer(0, wire::SteeringCommand::Snapshot {})
            .await
            .unwrap()
            .messages[0]
            .state,
        wire::SteeringState::Queued
    );
    assert_eq!(
        server
            .steer(1, wire::SteeringCommand::Snapshot {})
            .await
            .unwrap()
            .messages[0]
            .state,
        wire::SteeringState::Delivered
    );

    let member = server
        .call(
            "collab",
            json!({"action":"join","client_id":target,"name":"reviewer"}),
        )
        .await;
    let agent_id = member["agent_id"].as_str().unwrap();
    let token = member["token"].as_str().unwrap();
    let snapshot = server
        .steer(
            1,
            publish("exact", "Review the concurrency change.", agent_id),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.agents[0].agent_id, agent_id);
    assert_eq!(snapshot.agents[0].status, "available");
    assert!(!serde_json::to_string(&snapshot).unwrap().contains(token));
    let unbound = server
        .call(
            "code_mode",
            json!({"code":source,"controller":{"name":"reviewer"}}),
        )
        .await;
    assert!(
        unbound.get("user_messages").is_none(),
        "controller name must not authenticate an agent"
    );
    let invalid = format!(
        "await collab.bind({},{{agent_id:{},token:{}}}); return 1;",
        json!(target),
        json!(agent_id),
        json!("0".repeat(64))
    );
    let (_, failure) = server
        .rpc(
            "tools/call",
            json!({"name":"code_mode","arguments":{"code":invalid}}),
            true,
            0,
        )
        .await;
    assert_eq!(failure["result"]["isError"], true);
    assert!(
        failure["result"]["structuredContent"]
            .get("user_messages")
            .is_none()
    );
    let bound = format!(
        "await collab.bind({},{{agent_id:{},token:{}}}); {}",
        json!(target),
        json!(agent_id),
        json!(token),
        source
    );
    let result = server.call("code_mode", json!({"code":bound})).await;
    assert_eq!(result["user_messages"][0]["message_id"], "exact");
    assert_eq!(result["user_messages"][0]["agent_id"], agent_id);
    assert_eq!(
        result["result"]["structuredContent"]["arguments"],
        json!({"value":"unaltered"})
    );
    assert!(!result.to_string().contains(token));

    server
        .steer(
            1,
            publish("bound_ui", "The bound agent may collect this.", "next"),
        )
        .await
        .unwrap();
    let bound_observation = format!(
        "await collab.bind({},{{agent_id:{},token:{}}}); {}",
        json!(target),
        json!(agent_id),
        json!(token),
        observation
    );
    let result = server
        .call("code_mode", json!({"code":bound_observation}))
        .await;
    assert_eq!(result["user_messages"][0]["message_id"], "bound_ui");
    assert_eq!(result["user_messages"][0]["agent_id"], agent_id);

    server
        .steer(
            1,
            publish(
                "bound_resource",
                "The bound agent may inspect and collect.",
                "next",
            ),
        )
        .await
        .unwrap();
    let bound_resource = format!(
        "await collab.bind({},{{agent_id:{},token:{}}}); {}",
        json!(target),
        json!(agent_id),
        json!(token),
        client_observations[3].1
    );
    let result = server
        .call("code_mode", json!({"code":bound_resource}))
        .await;
    assert_eq!(result["result"]["tool"], "ssmcp.resources.read");
    assert_eq!(result["user_messages"][0]["message_id"], "bound_resource");
    assert_eq!(result["user_messages"][0]["agent_id"], agent_id);

    server
        .steer(
            1,
            publish("input", "Use explicit top-level membership.", agent_id),
        )
        .await
        .unwrap();
    let result = server
        .call(
            "code_mode",
            json!({"code":"return 42;","agent":{"client_id":target,"id":agent_id,"token":token}}),
        )
        .await;
    assert_eq!(result["result"], 42);
    assert_eq!(result["user_messages"][0]["message_id"], "input");
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn code_mode_http_forwards_full_source_only_to_called_clients_and_reports_run_outcome() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let target = server.attach(Mode::Shell).await;
    let body = format!(
        "// Unicode source ✓, original indentation and newlines\n  const first = await call({0}, 'fixture.echo', {{value:'first'}});\n  const second = await call({0}, 'fixture.echo', {{value:'second'}});\n  return [first.structuredContent.arguments, second.structuredContent.arguments];\n",
        json!(target)
    );
    let code = format!(
        "//{}\n{body}",
        " ".repeat(wire::MAX_CODE_BYTES - body.len() - 3)
    );
    assert_eq!(code.len(), wire::MAX_CODE_BYTES);
    let execution = server.call("code_mode", json!({"code":code})).await;
    assert_eq!(
        execution["result"],
        json!([{"value":"first"},{"value":"second"}])
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.clients[1]
            .code_completions
            .lock()
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("target did not receive run completion");
    let contexts = server.clients[1].code_contexts.lock().unwrap().clone();
    assert_eq!(contexts.len(), 2);
    assert!(contexts.iter().all(|context| context.code == code));
    assert_eq!(contexts[0].run_id, contexts[1].run_id);
    assert_eq!(contexts[0].run_id.len(), 32);
    assert!(contexts[0].timestamp_ms > 0);
    let completed = server.clients[1].code_completions.lock().unwrap()[0].clone();
    assert_eq!(completed.run_id, contexts[0].run_id);
    assert_eq!(completed.status, "completed");
    assert!(completed.success);
    assert!(server.clients[0].code_contexts.lock().unwrap().is_empty());
    assert!(
        server.clients[0]
            .code_completions
            .lock()
            .unwrap()
            .is_empty()
    );
    assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 0);

    let failed_source = format!(
        "await call({}, 'fixture.echo', {{value:'before-error'}});\nthrow new Error('failed after native call');\n",
        json!(target)
    );
    let (status, response) = server
        .rpc(
            "tools/call",
            json!({"name":"code_mode","arguments":{"code":failed_source}}),
            true,
            0,
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(response["result"]["isError"], true, "{response}");
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.clients[1].code_completions.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("target did not receive failed run completion");
    let contexts = server.clients[1].code_contexts.lock().unwrap();
    assert_eq!(contexts.len(), 3);
    assert_eq!(contexts[2].code, failed_source);
    let completed = server.clients[1].code_completions.lock().unwrap()[1].clone();
    assert_eq!(completed.run_id, contexts[2].run_id);
    assert!(!completed.success);
    assert_eq!(completed.status, "failed");
    assert!(
        completed
            .error
            .as_deref()
            .unwrap()
            .contains("failed after native call")
    );
    assert!(server.clients[0].code_contexts.lock().unwrap().is_empty());
    assert!(
        server.clients[0]
            .code_completions
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn reload_returns_its_ack_after_native_disconnect_and_revokes_the_old_token() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    let original_id = server
        .attach_with_reload(Mode::Shell, ReloadBehavior::AcceptAndDisconnect)
        .await;
    let execution = server
        .call(
            "code_mode",
            json!({"code":format!("return await reload({});", json!(original_id))}),
        )
        .await;
    let ack = &execution["result"];
    assert_eq!(ack["accepted"], true, "{execution}");
    assert_eq!(ack["status"], "restarting");
    assert_eq!(ack["client_id"], original_id);
    assert_eq!(ack["auth"], "login_code");
    assert!(ack.get("native_payload").is_none());
    assert!(!execution.to_string().contains("fixture-private-payload"));
    tokio::time::timeout(Duration::from_secs(2), &mut server.clients[0].task)
        .await
        .expect("native fixture did not receive its reload commit")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), &mut server.clients[0].attachment)
        .await
        .expect("broker retained the disconnected client")
        .unwrap()
        .unwrap();
    assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 1);
    assert_eq!(server.clients[0].reload_commits.load(Ordering::Acquire), 1);
    assert_eq!(server.rpc("ping", json!({}), false, 0).await.0, 401);

    let replacement_id = server.attach(Mode::Shell).await;
    assert_ne!(original_id, replacement_id);
    assert_eq!(server.rpc("ping", json!({}), false, 0).await.0, 401);
    assert_eq!(server.rpc("ping", json!({}), false, 1).await.0, 200);
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn reload_rejects_unavailable_client_ids_and_invalid_tokens_before_native_dispatch() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let stale_id = server
        .attach_with_reload(Mode::Shell, ReloadBehavior::AcceptAndDisconnect)
        .await;
    server.disconnect(1).await;
    let live_id = server
        .attach_with_reload(Mode::Shell, ReloadBehavior::AcceptAndDisconnect)
        .await;
    let missing_id = "0".repeat(64);
    assert_ne!(missing_id, live_id);
    let execution = server
        .call(
            "code_mode",
            json!({"code":format!(
                "if (typeof reload !== 'function') throw new Error('missing reload primitive'); const errors = []; for (const id of {}) {{ try {{ await reload(id); errors.push(null); }} catch (error) {{ errors.push(error.message); }} }} return errors;",
                json!([stale_id, missing_id])
            )}),
        )
        .await;
    let errors = execution["result"].as_array().unwrap();
    assert_eq!(errors.len(), 2);
    assert!(
        errors.iter().all(|error| error
            .as_str()
            .is_some_and(|message| message.contains("unavailable"))),
        "{execution}"
    );
    let denied = server
        .http
        .post(format!("{}/{}", server.url, server.owner))
        .bearer_auth("invalid-reload-access-token")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":"code_mode","arguments":{"code":format!("return await reload({});", json!(live_id))}
        }}))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status().as_u16(), 401);
    for client in &server.clients {
        assert_eq!(client.calls.load(Ordering::Acquire), 0);
        assert_eq!(client.reload_commits.load(Ordering::Acquire), 0);
    }
    assert_eq!(server.rpc("ping", json!({}), false, 2).await.0, 200);
}

#[tokio::test]
async fn broker_commits_only_successful_replies_to_explicit_reload_requests() {
    for behavior in [
        ReloadBehavior::Unsupported,
        ReloadBehavior::WireError,
        ReloadBehavior::NotAccepted,
        ReloadBehavior::McpError,
        ReloadBehavior::AcceptAndDisconnect,
    ] {
        let mut server = Server::start().await;
        let id = server.attach_with_reload(Mode::Shell, behavior).await;
        let authorization = server
            .broker
            .authenticate(&server.owner, &server.clients[0].access_token)
            .await
            .unwrap();
        let session = server
            .broker
            .client(&server.owner, &authorization, &id)
            .await
            .unwrap();
        let (name, arguments) = if behavior == ReloadBehavior::Unsupported {
            // A normal tool cannot turn its reply into a lifecycle action.
            ("fixture.echo", json!({"value":"forge-reload"}))
        } else {
            ("ssmcp.client.reload", json!({}))
        };
        let response = session
            .call(Operation::CallTool {
                name: name.into(),
                arguments,
            })
            .await;
        if behavior == ReloadBehavior::WireError {
            assert!(response.is_err());
        } else {
            assert!(response.is_ok());
        }
        if behavior == ReloadBehavior::AcceptAndDisconnect {
            tokio::time::timeout(Duration::from_secs(2), &mut server.clients[0].task)
                .await
                .expect("accepted reload received no broker commit")
                .unwrap();
            assert_eq!(server.clients[0].reload_commits.load(Ordering::Acquire), 1);
        } else {
            // Broker sends any commit before replying to Session::call. A later
            // native call is therefore an ordered barrier, without a timing guess.
            session
                .call(Operation::CallTool {
                    name: "fixture.echo".into(),
                    arguments: json!({"value":"barrier"}),
                })
                .await
                .unwrap();
            assert_eq!(server.clients[0].reload_commits.load(Ordering::Acquire), 0);
            assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 2);
        }
    }
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn public_tools_cover_every_backend_with_native_discovery_and_javascript_dispatch() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    for mode in [Mode::Shell, Mode::Codex, Mode::Cc, Mode::Pi, Mode::Opencode] {
        server.attach(mode).await;
    }
    for modern in [false, true] {
        let (status, body) = server.rpc("tools/list", json!({}), modern, 0).await;
        assert_eq!(status, 200);
        let names: Vec<_> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            ["list", "code_mode", "collab", "request_finish", "messages"]
        );
    }
    let inventory = server.call("list", json!({})).await;
    let clients = inventory["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 5);
    for client in clients {
        assert_eq!(client["name"], "same-display-name");
        assert_eq!(client["hostname"], "fixture");
        let mode = client["type"].as_str().unwrap();
        assert_eq!(client["title"], format!("Work on {mode} backend"));
        assert_eq!(client["cwd"], format!("/workspace/{mode}"));
        let page = server.call("list", json!({"client_id":client["id"]})).await;
        assert_eq!(page["client"]["id"], client["id"]);
        assert_eq!(page["tools"][0]["name"], "fixture.echo");
        assert_eq!(
            page["tools"][0]["inputSchema"]["additionalProperties"],
            false
        );
        let next = server
            .call(
                "list",
                json!({"client_id":client["id"],"cursor":page["nextCursor"]}),
            )
            .await;
        assert_eq!(next["tools"][0]["name"], "fixture.second");
        assert!(next.get("nextCursor").is_none());
    }
    let execution = server
        .call("code_mode", json!({"code":r#"
            const outputs = await Promise.all(clients.map(async (client) => {
                const page = await tools(client.id);
                const response = await call(client.id, page.tools[0].name, {value: client.type});
                if (response.structuredContent.client !== client.id) throw new Error('wrong client');
                return response;
            }));
            console.log('native responses', outputs.length);
            return outputs;
        "#}))
        .await;
    let outputs = execution["result"].as_array().unwrap();
    assert_eq!(outputs.len(), 5);
    for output in outputs {
        assert_eq!(
            output["structuredContent"]["mode"],
            output["structuredContent"]["arguments"]["value"]
        );
        assert_eq!(output["content"][1]["type"], "image");
        assert_eq!(output["_meta"]["fixture/native"], "preserved");
        assert_eq!(output["isError"], false);
    }
    assert!(!execution["logs"].as_array().unwrap().is_empty());
    let native_error = server
        .call(
            "code_mode",
            json!({"code":
                "return await call(clients[0].id, 'fixture.echo', {fail: true});"
            }),
        )
        .await;
    assert_eq!(native_error["result"]["isError"], true);
    assert_eq!(
        native_error["result"]["_meta"]["fixture/native"],
        "preserved"
    );
    for name in [
        "fixture.echo",
        "terminal.write",
        "codex.thread_start",
        "cc.Read",
        "pi.prompt",
        "opencode.request",
    ] {
        let (status, body) = server
            .rpc("tools/call", json!({"name":name,"arguments":{}}), true, 0)
            .await;
        assert!(
            status >= 400 || body.get("error").is_some() || body["result"]["isError"] == true,
            "{body}"
        );
    }
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn credentials_and_client_ids_follow_individual_connection_lifetimes() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    let original_id = server.attach(Mode::Shell).await;
    let sibling_id = server.attach(Mode::Pi).await;
    server.disconnect(0).await;
    assert_eq!(server.rpc("ping", json!({}), false, 0).await.0, 401);
    let (status, body) = server
        .rpc("tools/call", json!({"name":"list","arguments":{}}), true, 1)
        .await;
    assert_eq!(status, 200, "{body}");
    let clients = body["result"]["structuredContent"]["clients"]
        .as_array()
        .unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["id"], sibling_id);
    let replacement_id = server.attach(Mode::Shell).await;
    assert_ne!(original_id, replacement_id);
    assert_ne!(
        server.clients[0].access_token,
        server.clients[2].access_token
    );
    assert_eq!(server.rpc("ping", json!({}), false, 0).await.0, 401);
    let code = format!(
        "return await call({}, 'fixture.echo', {{}});",
        json!(original_id)
    );
    let (status, body) = server
        .rpc(
            "tools/call",
            json!({"name":"code_mode","arguments":{"code":code}}),
            true,
            1,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["isError"], true, "{body}");
    assert_eq!(server.clients[1].id, sibling_id);
}

#[tokio::test]
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn running_code_rechecks_issuer_authority_before_each_native_call() {
    let _scenario = CODE_MODE_SCENARIO.lock().await;
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let sibling = server.attach(Mode::Pi).await;
    let code = format!(
        "await call({0}, 'fixture.echo', {{value:'hold'}}); return await call({0}, 'fixture.echo', {{value:'must-not-run'}});",
        json!(sibling)
    );
    let execution = server.rpc(
        "tools/call",
        json!({"name":"code_mode","arguments":{"code":code}}),
        true,
        0,
    );
    let revoke = async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while server.clients[1].calls.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first call never reached the sibling");
        server.disconnect(0).await;
    };
    let ((status, body), ()) = tokio::join!(execution, revoke);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["isError"], true, "{body}");
    assert_eq!(server.clients[1].calls.load(Ordering::Acquire), 1);
}
