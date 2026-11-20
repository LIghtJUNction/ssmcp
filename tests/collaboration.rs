//! Collaboration runs in the broker without code_mode workers.
//! Authenticated response finalization may make a read-only native question probe.
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

struct Client {
    id: String,
    token: String,
    task: JoinHandle<()>,
    attachment: JoinHandle<ssmcp::Result<()>>,
}
struct Server {
    broker: Arc<Broker>,
    http: reqwest::Client,
    url: String,
    owner: String,
    task: JoinHandle<()>,
    clients: Vec<Client>,
    question_probes: Arc<Mutex<Vec<String>>>,
    unexpected_native_calls: Arc<Mutex<Vec<Operation>>>,
    rpc_id: AtomicUsize,
}
impl Server {
    async fn start() -> Self {
        let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let router = ssmcp::http::router(broker.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            broker,
            http: {
                let _ = rustls::crypto::ring::default_provider().install_default();
                reqwest::Client::builder()
            }
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap(),
            url,
            owner: String::new(),
            task,
            clients: Vec::new(),
            question_probes: Arc::new(Mutex::new(Vec::new())),
            unexpected_native_calls: Arc::new(Mutex::new(Vec::new())),
            rpc_id: AtomicUsize::new(1),
        }
    }
    async fn attach(&mut self) -> String {
        let (server, client) = UnixStream::pair().unwrap();
        let attachment = tokio::spawn(self.broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: format!("collaboration-fixture-{}", self.clients.len()),
                    title: None,
                    cwd: "/workspace/collaboration".into(),
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
            panic!("missing client registration");
        };
        self.owner = url.rsplit('/').next().unwrap().into();
        let token = self.oauth_login(&url, &login_code).await;
        let question_probes = self.question_probes.clone();
        let unexpected_native_calls = self.unexpected_native_calls.clone();
        let native_client_id = client_id.clone();
        let task = tokio::spawn(async move {
            while let Ok(Some(frame)) = wire::read_frame(&mut reader).await {
                if let Frame::Call { id, operation, .. } = frame {
                    let result = match operation {
                        Operation::CallTool { name, arguments }
                            if name == "ssmcp.ui.question_delivery" && arguments == json!({}) =>
                        {
                            question_probes
                                .lock()
                                .unwrap()
                                .push(native_client_id.clone());
                            // This legacy socket fixture has no authenticated native
                            // question store. A read-only probe must not block collab.
                            Err("typed question delivery is unavailable in this fixture".into())
                        }
                        operation => {
                            unexpected_native_calls.lock().unwrap().push(operation);
                            Err("collaboration must remain in the broker".into())
                        }
                    };
                    if wire::write_frame(&mut writer, &Frame::Reply { id, result })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        });
        self.clients.push(Client {
            id: client_id.clone(),
            token,
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
            .json(&json!({"client_name":"collaboration fixture","redirect_uris":[CALLBACK],
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
                ("state", "collaboration"),
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

    async fn rpc(&self, issuer: usize, method: &str, mut params: Value) -> (u16, Value) {
        params["_meta"] = json!({
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{}
        });
        let mut request = self
            .http
            .post(format!("{}/{}", self.url, self.owner))
            .bearer_auth(&self.clients[issuer].token)
            .header("Accept", "application/json")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", method);
        if method == "tools/call" {
            request = request.header("Mcp-Name", params["name"].as_str().unwrap());
        }
        let response = request
            .json(&json!({
                "jsonrpc":"2.0", "id":self.rpc_id.fetch_add(1, Ordering::Relaxed),
                "method":method, "params":params
            }))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }
    async fn raw(&self, issuer: usize, action: &str, mut arguments: Value) -> (u16, Value) {
        arguments["action"] = json!(action);
        self.rpc(
            issuer,
            "tools/call",
            json!({"name":"collab","arguments":arguments}),
        )
        .await
    }
    async fn call(&self, action: &str, arguments: Value) -> Value {
        let (status, body) = self.raw(0, action, arguments).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.get("error").is_none(), "{body}");
        assert_ne!(body["result"]["isError"], true, "{body}");
        body["result"]["structuredContent"].clone()
    }
    async fn rejected(&self, issuer: usize, action: &str, arguments: Value) {
        let (status, body) = self.raw(issuer, action, arguments).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["result"]["isError"], true, "{body}");
        assert!(body["result"]["structuredContent"]["messages"].is_null());
    }
    async fn rejected_without_native_calls(&self, issuer: usize, action: &str, arguments: Value) {
        let probes_before = self.question_probes.lock().unwrap().len();
        self.rejected(issuer, action, arguments).await;
        assert_eq!(
            self.question_probes.lock().unwrap().len(),
            probes_before,
            "invalid credentials must not trigger even a read-only native probe"
        );
        self.assert_no_unexpected_native_calls();
    }
    fn assert_no_unexpected_native_calls(&self) {
        let calls = self.unexpected_native_calls.lock().unwrap();
        assert!(calls.is_empty(), "unexpected native operations: {calls:?}");
    }
    fn assert_native_calls(&self, client_id: &str, expected_probes: usize) {
        self.assert_no_unexpected_native_calls();
        let probes = self.question_probes.lock().unwrap();
        assert_eq!(probes.len(), expected_probes, "question probes: {probes:?}");
        assert!(
            probes.iter().all(|target| target == client_id),
            "question probe reached another client: {probes:?}"
        );
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
fn credentials(client_id: &str, agent: &Value) -> Value {
    json!({"client_id":client_id,"agent_id":agent["agent_id"],"token":agent["token"]})
}

#[tokio::test]
async fn members_exchange_private_and_broadcast_messages_with_ack_and_retry() {
    let mut server = Server::start().await;
    let room = server.attach().await;
    let other_room = server.attach().await;
    assert_ne!(server.clients[0].id, server.clients[1].id);
    let (status, catalog) = server.rpc(0, "tools/list", json!({})).await;
    assert_eq!(status, 200, "{catalog}");
    assert!(
        catalog["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "collab")
    );

    // Display names are deliberately identical: identities must remain independent.
    let a = server
        .call(
            "join",
            json!({"client_id":room,"name":"worker","role":"author"}),
        )
        .await;
    let b = server
        .call(
            "join",
            json!({"client_id":room,"name":"worker","role":"reviewer"}),
        )
        .await;
    let c = server
        .call(
            "join",
            json!({"client_id":room,"name":"worker","role":"tester"}),
        )
        .await;
    assert_ne!(a["agent_id"], b["agent_id"]);
    assert_ne!(b["agent_id"], c["agent_id"]);
    assert_ne!(a["token"], b["token"]);
    let listed = server.call("list", json!({"client_id":room})).await;
    let agents = listed["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3);
    for agent in [&a, &b, &c] {
        assert!(
            agents
                .iter()
                .any(|peer| peer["agent_id"] == agent["agent_id"])
        );
        assert!(
            !listed
                .to_string()
                .contains(agent["token"].as_str().unwrap())
        );
    }
    assert_eq!(
        server.call("list", json!({"client_id":other_room})).await["agents"],
        json!([])
    );
    let mut update = credentials(&room, &a);
    update["status"] = json!("implementing message transport");
    server.call("update", update).await;
    let peers = server.call("list", json!({"client_id":room})).await;
    assert!(
        peers["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|peer| peer["agent_id"] == a["agent_id"]
                && peer["status"] == "implementing message transport")
    );

    let mut message = credentials(&room, &a);
    message["to"] = b["agent_id"].clone();
    message["payload"] = json!({"kind":"review","files":["src/collab.rs"]});
    message["topic"] = json!("transport");
    message["idempotency_key"] = json!("review-request-1");
    let receipt = server.call("send", message.clone()).await;
    let duplicate = server.call("send", message.clone()).await;
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(duplicate["message_id"], receipt["message_id"]);
    let inbox = server.call("receive", credentials(&room, &b)).await;
    assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["messages"][0]["message_id"], receipt["message_id"]);
    assert_eq!(inbox["messages"][0]["from"], a["agent_id"]);
    assert_eq!(inbox["messages"][0]["payload"], message["payload"]);
    assert_eq!(
        server.call("receive", credentials(&room, &b)).await["messages"],
        inbox["messages"]
    );
    for agent in [&a, &c] {
        assert_eq!(
            server.call("receive", credentials(&room, agent)).await["messages"],
            json!([])
        );
    }
    message["payload"] = json!({"kind":"different"});
    server.rejected(0, "send", message).await;

    let mut invalid = credentials(&room, &b);
    invalid["token"] = json!("0".repeat(64));
    server
        .rejected_without_native_calls(0, "receive", invalid)
        .await;
    // The second OAuth grant has the same Linux endpoint but a different owner.
    server
        .rejected_without_native_calls(1, "receive", credentials(&room, &b))
        .await;
    server
        .rejected_without_native_calls(0, "receive", credentials(&other_room, &b))
        .await;

    let mut ack = credentials(&room, &b);
    ack["through"] = inbox["next_cursor"].clone();
    server.call("ack", ack.clone()).await;
    server.call("ack", ack).await;
    assert_eq!(
        server.call("receive", credentials(&room, &b)).await["messages"],
        json!([])
    );

    // Null is a valid JSON payload and must survive public tool parsing.
    let mut null_message = credentials(&room, &a);
    null_message["to"] = b["agent_id"].clone();
    null_message["payload"] = Value::Null;
    let null_receipt = server.call("send", null_message).await;
    let null_inbox = server.call("receive", credentials(&room, &b)).await;
    assert_eq!(null_inbox["messages"].as_array().unwrap().len(), 1);
    assert_eq!(
        null_inbox["messages"][0]["message_id"],
        null_receipt["message_id"]
    );
    assert!(null_inbox["messages"][0]["payload"].is_null());
    let mut ack = credentials(&room, &b);
    ack["through"] = null_inbox["next_cursor"].clone();
    server.call("ack", ack).await;

    let mut broadcast = credentials(&room, &a);
    broadcast["to"] = json!("*");
    broadcast["payload"] = json!({"kind":"ready"});
    let receipt = server.call("send", broadcast).await;
    assert_eq!(receipt["recipients"].as_array().unwrap().len(), 2);
    for agent in [&b, &c] {
        let received = server.call("receive", credentials(&room, agent)).await;
        assert_eq!(received["messages"].as_array().unwrap().len(), 1);
        assert_eq!(received["messages"][0]["message_id"], receipt["message_id"]);
    }
    assert_eq!(
        server.call("receive", credentials(&room, &a)).await["messages"],
        json!([])
    );
    server.call("leave", credentials(&room, &c)).await;
    server
        .rejected_without_native_calls(0, "receive", credentials(&room, &c))
        .await;
    assert_eq!(
        server.call("list", json!({"client_id":room})).await["agents"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Seventeen successful bound actions and the authenticated idempotency
    // conflict may probe. Leaving retires the binding before finalization.
    server.assert_native_calls(&room, 18);
}

#[tokio::test]
async fn five_waiting_agents_do_not_block_a_sender() {
    let mut server = Server::start().await;
    let room = server.attach().await;
    let sender = server
        .call("join", json!({"client_id":room,"name":"sender"}))
        .await;
    let mut receivers = Vec::new();
    for index in 0..5 {
        receivers.push(
            server
                .call(
                    "join",
                    json!({
                        "client_id":room,"name":format!("receiver-{index}")
                    }),
                )
                .await,
        );
    }
    let waits = receivers.iter().map(|agent| {
        let mut args = credentials(&room, agent);
        args["wait_ms"] = json!(5000);
        server.call("receive", args)
    });
    let send = async {
        // Let all requests enter their waiting phase before testing sender
        // admission. This exposes a four-worker code_mode implementation.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let mut args = credentials(&room, &sender);
        args["to"] = json!("*");
        args["payload"] = json!("wake every receiver");
        tokio::time::timeout(Duration::from_secs(2), server.call("send", args))
            .await
            .expect("waiting receivers blocked the sender")
    };
    let (received, sent) = tokio::join!(futures_util::future::join_all(waits), send);
    assert_eq!(sent["recipients"].as_array().unwrap().len(), 5);
    for inbox in received {
        assert_eq!(inbox["messages"].as_array().unwrap().len(), 1);
        assert_eq!(inbox["messages"][0]["message_id"], sent["message_id"]);
        assert_eq!(inbox["messages"][0]["payload"], "wake every receiver");
    }
    // One sender and five receivers finalize independently of code_mode workers.
    server.assert_native_calls(&room, 6);
}

#[tokio::test]
async fn disconnect_all_ends_waiting_receivers_and_revokes_old_room_access() {
    let mut server = Server::start().await;
    let room = server.attach().await;
    let agent = server
        .call("join", json!({"client_id":room,"name":"waiting"}))
        .await;
    let mut args = credentials(&room, &agent);
    args["wait_ms"] = json!(30_000);
    let receive = server.raw(0, "receive", args);
    let disconnect = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        server.broker.disconnect_all().await;
    };
    let ((status, body), ()) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(receive, disconnect)
    })
    .await
    .expect("client disconnect did not wake the collaboration wait");
    assert!(
        status == 401 || body["result"]["isError"] == true || body.get("error").is_some(),
        "{status}: {body}"
    );
    assert!(body["result"]["structuredContent"]["messages"].is_null());
    let (status, _) = server.raw(0, "list", json!({"client_id":room})).await;
    assert_eq!(status, 401);
    server.assert_native_calls(&room, 0);
}
