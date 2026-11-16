//! Resource discovery, schema pagination and authorization at the public HTTP boundary.
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
                        "nextCursor":"page /+=?&:"
                    })),
                    Operation::ListTools {
                        cursor: Some(cursor),
                    } if cursor == "page /+=?&:" => Ok(json!({"tools":[{
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
            } else if method == "resources/read" {
                request = request.header("Mcp-Name", params["uri"].as_str().unwrap());
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

fn data(body: &Value) -> Value {
    serde_json::from_str(body["result"]["contents"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn resources_discover_and_page_native_schemas_in_both_protocol_eras() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    for modern in [false, true] {
        let (status, body) = server.rpc("tools/list", json!({}), modern, 0).await;
        assert_eq!(status, 200, "{body}");
        let names: Vec<_> = body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list", "code_mode"]);
        let method = if modern {
            "server/discover"
        } else {
            "initialize"
        };
        let (_, body) = server
            .rpc(
                method,
                json!({
                    "protocolVersion":"2025-11-25","capabilities":{},
                    "clientInfo":{"name":"resources fixture","version":"1"}
                }),
                modern,
                0,
            )
            .await;
        assert_eq!(
            body["result"]["capabilities"]["resources"]["subscribe"],
            false
        );
        assert_eq!(
            body["result"]["capabilities"]["resources"]["listChanged"],
            false
        );

        let (status, body) = server.rpc("resources/list", json!({}), modern, 0).await;
        assert_eq!(status, 200, "{body}");
        let resources = body["result"]["resources"].as_array().unwrap();
        assert_eq!(resources.len(), 6);
        assert!(resources.iter().any(|r| r["uri"] == "ssmcp://skill"));
        let (_, body) = server
            .rpc("resources/templates/list", json!({}), modern, 0)
            .await;
        assert_eq!(
            body["result"]["resourceTemplates"]
                .as_array()
                .unwrap()
                .len(),
            3
        );

        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":"ssmcp://clients"}),
                modern,
                0,
            )
            .await;
        let inventory = data(&body);
        assert_eq!(inventory["clients"][0]["id"], id);
        assert_eq!(inventory["clients"][0]["cwd_kind"], "startup");
        let uri = format!("ssmcp://clients/{id}/tools");
        let (_, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        let page = data(&body);
        assert_eq!(page["tools"][0]["name"], "fixture.echo");
        assert_eq!(page["client"]["id"], id);
        let next_uri = page["nextUri"].as_str().unwrap();
        assert!(next_uri.contains("%26"), "{next_uri}");
        let (_, body) = server
            .rpc("resources/read", json!({"uri":next_uri}), modern, 0)
            .await;
        let page = data(&body);
        assert_eq!(page["tools"][0]["name"], "fixture.second");
        assert!(page.get("nextUri").is_none());
        for uri in ["ssmcp://guide", "ssmcp://skill"] {
            let (status, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["result"]["contents"][0]["mimeType"], "text/markdown");
            assert!(
                !body["result"]["contents"][0]["text"]
                    .as_str()
                    .unwrap()
                    .is_empty()
            );
        }
        let (_, body) = server
            .rpc("resources/read", json!({"uri":"ssmcp://limits"}), modern, 0)
            .await;
        assert_eq!(data(&body)["public_tools"], json!(["list", "code_mode"]));
        assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 0);
    }
}

#[tokio::test]
async fn snapshots_change_with_connections_and_offline_resource_ids_are_denied() {
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let (_, before) = server.rpc("resources/list", json!({}), false, 0).await;
    let sibling = server.attach(Mode::Pi).await;
    let (_, after) = server.rpc("resources/list", json!({}), false, 0).await;
    assert_eq!(
        after["result"]["resources"].as_array().unwrap().len(),
        before["result"]["resources"].as_array().unwrap().len() + 2
    );
    let uri = format!("ssmcp://clients/{sibling}/info");
    let (_, info) = server
        .rpc("resources/read", json!({"uri":uri}), false, 0)
        .await;
    assert_eq!(data(&info)["type"], "pi");
    server.disconnect(1).await;
    for modern in [false, true] {
        let (status, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        assert_eq!(body["error"]["code"], -32002, "{body}");
        assert_eq!(status, if modern { 404 } else { 200 });
    }
    let (_, inventory) = server
        .rpc("resources/read", json!({"uri":"ssmcp://clients"}), false, 0)
        .await;
    assert_eq!(data(&inventory)["clients"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn resources_require_a_live_token_scoped_to_the_requested_endpoint() {
    let mut server = Server::start().await;
    server.attach(Mode::Shell).await;
    let unknown = if server.owner == "nobody" {
        "someoneelse"
    } else {
        "nobody"
    };
    let response = server
        .http
        .post(format!("{}/{unknown}", server.url))
        .bearer_auth(&server.clients[0].access_token)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"resources/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let response = server.http.post(format!("{}/{}",server.url,server.owner))
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"ssmcp://guide"}}))
        .send().await.unwrap();
    assert_eq!(response.status().as_u16(), 401);
    server.disconnect(0).await;
    for method in [
        "resources/list",
        "resources/templates/list",
        "resources/read",
    ] {
        let (status, _) = server
            .rpc(method, json!({"uri":"ssmcp://guide"}), false, 0)
            .await;
        assert_eq!(status, 401);
    }
}

#[tokio::test]
async fn malformed_uris_cursors_and_resource_parameters_do_not_execute_native_work() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    for (method, params) in [
        ("resources/list", json!({"cursor":1})),
        ("resources/list", json!({"cursor":"stale"})),
        ("resources/list", json!({"surprise":true})),
        ("resources/templates/list", json!({"cursor":"extra-page"})),
        ("resources/read", json!({"uri":3})),
        (
            "resources/read",
            json!({"uri":"ssmcp://guide","path":"/etc/passwd"}),
        ),
        (
            "resources/read",
            json!({"uri":"ssmcp://clients?cursor=a&cursor=b"}),
        ),
    ] {
        let (status, body) = server.rpc(method, params, false, 0).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["code"], -32602);
    }
    for uri in [
        "file:///etc/passwd".to_owned(),
        "https://example.com".to_owned(),
        "ssmcp://guide?cursor=anything".to_owned(),
        format!("ssmcp://clients/{id}/../../etc/passwd"),
        format!("ssmcp://clients/{}/info", "0".repeat(64)),
    ] {
        let (_, body) = server
            .rpc("resources/read", json!({"uri":uri}), false, 0)
            .await;
        assert_eq!(body["error"]["code"], -32002, "{body}");
    }
    let (_, body) = server
        .rpc(
            "resources/subscribe",
            json!({"uri":"ssmcp://clients"}),
            false,
            0,
        )
        .await;
    assert_eq!(body["error"]["code"], -32601);
    assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 0);
}
