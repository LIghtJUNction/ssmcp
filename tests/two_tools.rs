//! Exercise the public HTTP boundary with bounded, deterministic wire clients.
//! Native adapter protocols are covered by the backend-specific test suites.
use serde_json::{Value, json};
use ssmcp::{
    broker::Broker,
    wire::{self, ClientMetadata, Frame, Mode, Operation},
};
use std::{
    sync::{
        Arc,
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
    access_token: String,
    calls: Arc<AtomicUsize>,
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
        let task = tokio::spawn(async move {
            while let Some(Frame::Call { id, operation }) =
                wire::read_frame(&mut reader).await.unwrap()
            {
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
                        Ok(json!({
                            "content":[
                                {"type":"text","text":mode.name()},
                                {"type":"image","mimeType":"image/png","data":"AA=="}
                            ],
                            "structuredContent":{"mode":mode.name(),"client":response_id,"arguments":arguments},
                            "isError":arguments.get("fail").and_then(Value::as_bool).unwrap_or(false),
                            "_meta":{"fixture/native":"preserved"}
                        }))
                    }
                    _ => Err("unknown fixture operation".into()),
                };
                if wire::write_frame(&mut writer, &Frame::Reply { id, result })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        self.clients.push(Client {
            id: client_id.clone(),
            access_token,
            calls,
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
#[ignore = "requires system Node.js, bubblewrap, and permitted user/network namespaces; CI runs explicitly with --include-ignored"]
async fn two_public_tools_cover_every_backend_with_native_discovery_and_javascript_dispatch() {
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
        assert_eq!(names, ["list", "code_mode"]);
    }
    let inventory = server.call("list", json!({})).await;
    let clients = inventory["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 5);
    for client in clients {
        assert_eq!(client["name"], "same-display-name");
        assert_eq!(client["hostname"], "fixture");
        let mode = client["type"].as_str().unwrap();
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
