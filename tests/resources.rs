//! Resource discovery, schema pagination and authorization at the public HTTP boundary.
#![cfg(target_os = "linux")]

#[path = "support/http_client.rs"]
mod http_client;

use serde_json::{Value, json};
use ssmcp::{
    broker::Broker,
    wire::{self, ClientMetadata, Frame, Mode, Operation},
};
use std::{
    collections::BTreeSet,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::BufReader,
    net::{TcpListener, UnixStream},
    sync::Notify,
    task::JoinHandle,
};

type NativeReply = Result<Value, String>;

const CLIENT_RESOURCE_KINDS: [&str; 9] = [
    "info",
    "tools",
    "capabilities",
    "resources",
    "resource-templates",
    "context",
    "skills",
    "skills/view",
    "agents",
];

fn client_resource_uris(id: &str) -> BTreeSet<String> {
    CLIENT_RESOURCE_KINDS
        .iter()
        .map(|kind| format!("ssmcp://clients/{id}/{kind}"))
        .collect()
}

fn resource_uris(body: &Value) -> BTreeSet<String> {
    body["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap().to_owned())
        .collect()
}

struct Client {
    access_token: String,
    calls: Arc<AtomicUsize>,
    native_calls: Arc<Mutex<Vec<(String, Value)>>>,
    native_reply: Arc<Mutex<Option<NativeReply>>>,
    native_paused: Arc<AtomicBool>,
    native_gate: Arc<Notify>,
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
            http: http_client::builder()
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
        self.attach_with_context(mode, None).await
    }

    async fn attach_with_context(
        &mut self,
        mode: Mode,
        context: Option<ssmcp::context::Context>,
    ) -> String {
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
                    title: None,
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
        let native_calls = Arc::new(Mutex::new(Vec::new()));
        let recorded_native_calls = native_calls.clone();
        let native_reply = Arc::new(Mutex::new(None));
        let recorded_native_reply = native_reply.clone();
        let native_paused = Arc::new(AtomicBool::new(false));
        let recorded_native_paused = native_paused.clone();
        let native_gate = Arc::new(Notify::new());
        let recorded_native_gate = native_gate.clone();
        let task = tokio::spawn(async move {
            while let Some(frame) = wire::read_frame(&mut reader).await.unwrap() {
                let (id, operation) = match frame {
                    Frame::Call { id, operation, .. } => (id, operation),
                    Frame::CodeModeFinished { .. } => continue,
                    _ => panic!("unexpected fixture frame"),
                };
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
                    Operation::CallTool { name, arguments }
                        if matches!(
                            name.as_str(),
                            "ssmcp.context.roots"
                                | "ssmcp.skills.list"
                                | "ssmcp.skills.view"
                                | "ssmcp.skills.read"
                                | "ssmcp.agents.resolve"
                                | "ssmcp.agents.read"
                        ) =>
                    {
                        recorded_calls.fetch_add(1, Ordering::Release);
                        recorded_native_calls
                            .lock()
                            .unwrap()
                            .push((name.clone(), arguments.clone()));
                        if recorded_native_paused.load(Ordering::Acquire) {
                            recorded_native_gate.notified().await;
                        }
                        let override_reply = recorded_native_reply.lock().unwrap().take();
                        if let Some(reply) = override_reply {
                            reply
                        } else if let Some(context) = &context {
                            context
                                .perform(&name, arguments)
                                .await
                                .map(|view| json!({"structuredContent":view,"isError":false}))
                        } else {
                            let root = arguments.get("root").cloned().unwrap_or(json!("workspace"));
                            let root_info =
                                json!({"id":root,"path":format!("/workspace/{}", mode.name())});
                            let view = match name.as_str() {
                                "ssmcp.context.roots" => json!({"roots":[
                                    {"id":"workspace","path":format!("/workspace/{}", mode.name()),"read_only":true}
                                ]}),
                                "ssmcp.skills.list" => {
                                    if arguments.get("cursor").is_none() {
                                        json!({
                                            "root":root_info,
                                            "skills":[{"name":"first skill","path":"skills/first/SKILL.md"}],
                                            "next_cursor":"skills /+=?&:界"
                                        })
                                    } else if arguments["cursor"] == "skills /+=?&:界" {
                                        json!({
                                            "root":root_info,
                                            "skills":[{"name":"second skill","path":"skills/second/SKILL.md"}]
                                        })
                                    } else {
                                        panic!("unexpected fixture skill cursor: {arguments}");
                                    }
                                }
                                "ssmcp.skills.view" => json!({
                                    "root":root_info,
                                    "scope":arguments.get("scope").cloned().unwrap_or(json!("effective")),
                                    "markdown":"# Skills\n\n- first skill: current description\n",
                                    "next_cursor":if arguments.get("cursor").is_none() { json!("skills /+=?&:界") } else { Value::Null }
                                }),
                                "ssmcp.skills.read" | "ssmcp.agents.read" => json!({
                                    "root":root_info,
                                    "path":arguments["path"],
                                    "content":if name == "ssmcp.skills.read" {
                                        "---\nname: first skill\ndescription: current description\n---\nExact skill body.\n"
                                    } else {
                                        "# Rules\nKeep the workspace scoped.\n"
                                    }
                                }),
                                "ssmcp.agents.resolve" => json!({
                                    "root":root_info,
                                    "target":arguments["path"],
                                    "files":[{"path":"AGENTS.md","content":"# Rules\nKeep the workspace scoped.\n"}],
                                    "truncated":false
                                }),
                                _ => unreachable!(),
                            };
                            Ok(json!({
                                "content":[{"type":"text","text":"native text fallback must not replace structured data"}],
                                "structuredContent":view,
                                "isError":false
                            }))
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
            }
        });
        self.clients.push(Client {
            access_token,
            calls,
            native_calls,
            native_reply,
            native_paused,
            native_gate,
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
        assert_eq!(
            names,
            [
                "link",
                "list",
                "code_mode",
                "collab",
                "await",
                "events",
                "messages"
            ]
        );
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
        let expected = [
            "ssmcp://guide",
            "ssmcp://limits",
            "ssmcp://recovery",
            "ssmcp://collaboration",
            "ssmcp://skill",
            "ssmcp://clients",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain(client_resource_uris(&id))
        .collect::<BTreeSet<_>>();
        assert_eq!(resources.len(), expected.len());
        assert_eq!(resource_uris(&body), expected);
        let (_, body) = server
            .rpc("resources/templates/list", json!({}), modern, 0)
            .await;
        let templates = body["result"]["resourceTemplates"].as_array().unwrap();
        assert_eq!(templates.len(), 13);
        let actual = templates
            .iter()
            .map(|template| template["uriTemplate"].as_str().unwrap())
            .collect::<BTreeSet<_>>();
        let expected = [
            "ssmcp://clients/{client_id}/capabilities",
            "ssmcp://clients/{client_id}/resources{?cursor}",
            "ssmcp://clients/{client_id}/resource-templates{?cursor}",
            "ssmcp://clients/{client_id}/resource{?uri}",
            "ssmcp://clients{?cursor}",
            "ssmcp://clients/{client_id}/info",
            "ssmcp://clients/{client_id}/tools{?cursor}",
            "ssmcp://clients/{client_id}/context",
            "ssmcp://clients/{client_id}/skills{?root,scope,cursor}",
            "ssmcp://clients/{client_id}/skills/view{?root,scope,cursor}",
            "ssmcp://clients/{client_id}/skills/read{?root,path}",
            "ssmcp://clients/{client_id}/agents/read{?root,path}",
            "ssmcp://clients/{client_id}/agents{?root,path}",
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);

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
        for uri in ["ssmcp://guide", "ssmcp://skill", "ssmcp://collaboration"] {
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
        assert_eq!(
            data(&body)["public_tools"],
            json!([
                "link",
                "list",
                "code_mode",
                "collab",
                "await",
                "events",
                "messages"
            ])
        );
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
        before["result"]["resources"].as_array().unwrap().len() + 9
    );
    let before_uris = resource_uris(&before);
    let after_uris = resource_uris(&after);
    assert!(before_uris.is_subset(&after_uris));
    assert_eq!(
        after_uris
            .difference(&before_uris)
            .cloned()
            .collect::<BTreeSet<_>>(),
        client_resource_uris(&sibling)
    );
    let uri = format!("ssmcp://clients/{sibling}/info");
    let (_, info) = server
        .rpc("resources/read", json!({"uri":uri}), false, 0)
        .await;
    assert_eq!(data(&info)["type"], "pi");
    server.disconnect(1).await;
    for modern in [false, true] {
        for kind in
            CLIENT_RESOURCE_KINDS
                .into_iter()
                .chain(["resource", "skills/read", "agents/read"])
        {
            let uri = format!("ssmcp://clients/{sibling}/{kind}");
            let (status, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert_eq!(body["error"]["code"], -32002, "{body}");
            assert_eq!(status, if modern { 404 } else { 200 });
        }
        let (_, remaining) = server.rpc("resources/list", json!({}), modern, 0).await;
        assert_eq!(
            remaining["result"]["resources"].as_array().unwrap().len(),
            before_uris.len()
        );
        assert_eq!(resource_uris(&remaining), before_uris);
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
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?root=one&root=two")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?root=one&ro%6Ft=two")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?path=.")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?scope=unknown")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills/view?scope=effective&scope=root")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills/read")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/agents/read?root=workspace")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/agents?cursor=next")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/context?root=workspace")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/agents?root=workspace&path=%FF")}),
        ),
        (
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?cursor={}", "x".repeat(129))}),
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

#[tokio::test]
async fn native_context_resources_dispatch_structured_views_and_preserve_encoded_paging() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    let root = "context +/=&?目录";
    let path = "nested/space +&?=/说明.md";
    let mut expected_calls = Vec::new();
    for modern in [false, true] {
        let uri = format!("ssmcp://clients/{id}/context");
        let (status, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        assert_eq!(status, 200, "{body}");
        let context = data(&body);
        assert_eq!(context["roots"][0]["id"], "workspace");
        assert_eq!(context["client"]["id"], id);
        assert!(context.get("content").is_none());
        assert!(context.get("structuredContent").is_none());
        expected_calls.push(("ssmcp.context.roots".to_owned(), json!({})));

        let query = serde_urlencoded::to_string([("root", root)]).unwrap();
        let uri = format!("ssmcp://clients/{id}/skills?{query}");
        let (status, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        assert_eq!(status, 200, "{body}");
        let first = data(&body);
        assert_eq!(first["skills"][0]["name"], "first skill");
        assert_eq!(first["root"]["id"], root);
        assert_eq!(first["client"]["id"], id);
        let next_uri = first["nextUri"].as_str().unwrap();
        assert!(next_uri.contains("%26"), "{next_uri}");
        let (_, body) = server
            .rpc("resources/read", json!({"uri":next_uri}), modern, 0)
            .await;
        let second = data(&body);
        assert_eq!(second["skills"][0]["name"], "second skill");
        assert_eq!(second["root"]["id"], root);
        assert!(second.get("nextUri").is_none());
        expected_calls.push(("ssmcp.skills.list".to_owned(), json!({"root":root})));
        expected_calls.push((
            "ssmcp.skills.list".to_owned(),
            json!({"root":root,"cursor":"skills /+=?&:界"}),
        ));

        let query = serde_urlencoded::to_string([("root", root), ("path", path)]).unwrap();
        let uri = format!("ssmcp://clients/{id}/agents?{query}");
        let (status, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        assert_eq!(status, 200, "{body}");
        let agents = data(&body);
        assert_eq!(agents["target"], path);
        assert_eq!(agents["root"]["id"], root);
        assert_eq!(
            agents["files"][0]["content"],
            "# Rules\nKeep the workspace scoped.\n"
        );
        assert_eq!(agents["truncated"], false);
        expected_calls.push((
            "ssmcp.agents.resolve".to_owned(),
            json!({"root":root,"path":path}),
        ));

        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/skills")}),
                modern,
                0,
            )
            .await;
        assert_eq!(data(&body)["root"]["id"], "workspace");
        expected_calls.push(("ssmcp.skills.list".to_owned(), json!({})));
        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/agents")}),
                modern,
                0,
            )
            .await;
        assert_eq!(data(&body)["target"], ".");
        assert_eq!(data(&body)["root"]["id"], "workspace");
        expected_calls.push(("ssmcp.agents.resolve".to_owned(), json!({"path":"."})));
    }
    assert_eq!(
        *server.clients[0].native_calls.lock().unwrap(),
        expected_calls
    );
    assert_eq!(server.clients[0].calls.load(Ordering::Acquire), 12);
}

#[tokio::test]
async fn unsupported_failed_and_malformed_native_views_are_unavailable() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    let uri = format!("ssmcp://clients/{id}/context");
    for modern in [false, true] {
        for reply in [
            Err("unknown native tool on an older client".into()),
            Ok(
                json!({"isError":true,"structuredContent":{"roots":[]},"content":[{"type":"text","text":"fixture private error"}]}),
            ),
            Ok(json!({"content":[{"type":"text","text":"{\"roots\":[]}"}]})),
            Ok(json!({"structuredContent":{}})),
            Ok(json!({"structuredContent":{"roots":"invalid"}})),
            Ok(json!({"isError":"false","structuredContent":{"roots":[]}})),
        ] {
            *server.clients[0].native_reply.lock().unwrap() = Some(reply);
            let (status, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert_eq!(status, 200, "{body}");
            assert_eq!(body["error"]["code"], -32603, "{body}");
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("unavailable")
            );
            assert!(body.get("result").is_none());
            assert!(!body.to_string().contains("fixture private error"));
        }
    }
    let uri = format!("ssmcp://clients/{id}/skills");
    for cursor in [json!(""), json!("\n"), json!("x".repeat(129)), json!(12)] {
        *server.clients[0].native_reply.lock().unwrap() = Some(Ok(json!({
            "structuredContent":{"skills":[],"next_cursor":cursor}
        })));
        let (_, body) = server
            .rpc("resources/read", json!({"uri":uri}), false, 0)
            .await;
        assert_eq!(body["error"]["code"], -32603, "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unavailable")
        );
    }
    *server.clients[0].native_reply.lock().unwrap() = Some(Ok(json!({
        "structuredContent":{"skills":[{"description":"x".repeat(ssmcp::resources::MAX_RESOURCE_BYTES)}]}
    })));
    let (_, body) = server
        .rpc("resources/read", json!({"uri":uri}), false, 0)
        .await;
    assert_eq!(body["error"]["code"], -32603, "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("1 MiB"));
}

#[tokio::test]
async fn native_views_recheck_authorization_and_target_after_await() {
    for disconnect_target in [false, true] {
        let mut server = Server::start().await;
        server.attach(Mode::Shell).await;
        let target = server.attach(Mode::Pi).await;
        server.clients[1]
            .native_paused
            .store(true, Ordering::Release);
        let uri = format!("ssmcp://clients/{target}/agents");
        let (response, ()) = tokio::join!(
            server.rpc("resources/read", json!({"uri":uri}), false, 0),
            async {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while server.clients[1].calls.load(Ordering::Acquire) == 0 {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("native resource dispatch never started");
                server
                    .disconnect(if disconnect_target { 1 } else { 0 })
                    .await;
                server.clients[1].native_gate.notify_one();
            }
        );
        if disconnect_target {
            assert_eq!(response.0, 200, "{}", response.1);
            assert_eq!(response.1["error"]["code"], -32002, "{}", response.1);
        } else {
            // The outer HTTP layer also invalidates a revoked connection-bound token.
            assert_eq!(response.0, 401, "{}", response.1);
            assert!(response.1.is_null());
        }
        assert!(response.1.get("result").is_none());
        if !disconnect_target {
            server.clients[1]
                .native_paused
                .store(false, Ordering::Release);
            let (_, body) = server
                .rpc("resources/read", json!({"uri":uri}), false, 1)
                .await;
            assert_eq!(data(&body)["files"][0]["path"], "AGENTS.md");
        }
    }
}

#[tokio::test]
async fn skill_views_preserve_scope_and_pagination_and_content_reads_are_exact() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    for modern in [false, true] {
        for scope in ["root", "effective"] {
            let uri = format!("ssmcp://clients/{id}/skills/view?scope={scope}");
            let (status, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert_eq!(status, 200, "{body}");
            let item = &body["result"]["contents"][0];
            assert_eq!(item["mimeType"], "text/markdown");
            let markdown = item["text"].as_str().unwrap();
            assert!(markdown.contains("# Skills"));
            let next_uri = markdown
                .split_once("[Next skill page](")
                .unwrap()
                .1
                .split_once(')')
                .unwrap()
                .0;
            assert!(next_uri.contains(&format!("scope={scope}")));
            assert!(next_uri.contains("%26"));
            let (_, next) = server
                .rpc("resources/read", json!({"uri":next_uri}), modern, 0)
                .await;
            assert!(
                !next["result"]["contents"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Next skill page")
            );
            let calls = server.clients[0].native_calls.lock().unwrap();
            assert_eq!(
                calls[calls.len() - 2],
                ("ssmcp.skills.view".to_owned(), json!({"scope":scope}))
            );
            assert_eq!(
                calls[calls.len() - 1],
                (
                    "ssmcp.skills.view".to_owned(),
                    json!({"scope":scope,"cursor":"skills /+=?&:界"})
                )
            );
        }
        for (kind, path, expected) in [
            (
                "skills",
                "skills/first/SKILL.md",
                "---\nname: first skill\ndescription: current description\n---\nExact skill body.\n",
            ),
            (
                "agents",
                "AGENTS.md",
                "# Rules\nKeep the workspace scoped.\n",
            ),
        ] {
            let query =
                serde_urlencoded::to_string([("root", "workspace"), ("path", path)]).unwrap();
            let uri = format!("ssmcp://clients/{id}/{kind}/read?{query}");
            let (_, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert_eq!(body["result"]["contents"][0]["mimeType"], "text/markdown");
            assert_eq!(body["result"]["contents"][0]["text"], expected);
        }
        for kind in ["skills", "agents"] {
            let (_, body) = server
                .rpc(
                    "resources/read",
                    json!({"uri":format!("ssmcp://clients/{id}/{kind}")}),
                    modern,
                    0,
                )
                .await;
            let value = data(&body);
            let entry = if kind == "skills" {
                &value["skills"][0]
            } else {
                &value["files"][0]
            };
            let read_uri = entry["readUri"].as_str().unwrap();
            let (_, read) = server
                .rpc("resources/read", json!({"uri":read_uri}), modern, 0)
                .await;
            assert!(read.get("result").is_some(), "{read}");
        }
    }
}

#[tokio::test]
async fn default_skill_views_require_effective_scope_and_never_fall_back_on_old_clients() {
    let mut server = Server::start().await;
    let id = server.attach(Mode::Shell).await;
    for modern in [false, true] {
        for (query, expected) in [
            ("", json!({"scope":"effective"})),
            ("?cursor=next", json!({"scope":"effective","cursor":"next"})),
            ("?root=workspace", json!({"root":"workspace"})),
            (
                "?root=context-1&cursor=next",
                json!({"root":"context-1","cursor":"next"}),
            ),
            ("?scope=root", json!({"scope":"root"})),
            ("?scope=effective", json!({"scope":"effective"})),
        ] {
            let uri = format!("ssmcp://clients/{id}/skills/view{query}");
            let before = server.clients[0].calls.load(Ordering::Acquire);
            let (_, body) = server
                .rpc("resources/read", json!({"uri":uri}), modern, 0)
                .await;
            assert!(body.get("result").is_some(), "{body}");
            assert_eq!(server.clients[0].calls.load(Ordering::Acquire), before + 1);
            assert_eq!(
                server.clients[0].native_calls.lock().unwrap().last(),
                Some(&("ssmcp.skills.view".to_owned(), expected)),
            );
        }
        for old_reply in [
            Err("invalid context arguments: unknown field scope".into()),
            Ok(json!({"isError":true,"content":[
                {"type":"text","text":"invalid context arguments: unknown field scope"}
            ]})),
        ] {
            *server.clients[0].native_reply.lock().unwrap() = Some(old_reply);
            let before = server.clients[0].calls.load(Ordering::Acquire);
            let (_, body) = server
                .rpc(
                    "resources/read",
                    json!({"uri":format!("ssmcp://clients/{id}/skills/view")}),
                    modern,
                    0,
                )
                .await;
            assert_eq!(body["error"]["code"], -32603, "{body}");
            assert!(body.get("result").is_none());
            assert_eq!(server.clients[0].calls.load(Ordering::Acquire), before + 1);
            assert_eq!(
                server.clients[0].native_calls.lock().unwrap().last(),
                Some(&("ssmcp.skills.view".to_owned(), json!({"scope":"effective"}))),
            );
        }
    }
}

#[tokio::test]
async fn resources_read_live_project_files_and_explicit_global_overlay_without_restart() {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let directory = Directory(std::env::temp_dir().join(format!(
        "ssmcp-live-resource-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
    )));
    let project = directory.0.join("project");
    let global = directory.0.join("global");
    for path in [
        project.join("skills/shared"),
        global.join("skills/shared"),
        global.join("skills/global-only"),
    ] {
        fs::create_dir_all(path).unwrap();
    }
    fs::write(
        project.join("AGENTS.md"),
        "# Project instructions\nfirst revision\n",
    )
    .unwrap();
    fs::write(
        project.join("skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Project description\n---\nProject body\n",
    )
    .unwrap();
    fs::write(
        global.join("skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Global duplicate\n---\nGlobal body\n",
    )
    .unwrap();
    let global_skill_path = format!(
        "skills/{}/{}/global +&?=/SKILL.md",
        "目录".repeat(30),
        "说明".repeat(30),
    );
    fs::create_dir_all(global.join(&global_skill_path).parent().unwrap()).unwrap();
    fs::write(
        global.join(&global_skill_path),
        "---\nname: global-only\ndescription: Available globally\n---\nGlobal-only body\n",
    )
    .unwrap();
    let context = ssmcp::context::Context::new(project.clone(), vec![global.clone()]).unwrap();
    let mut server = Server::start().await;
    let id = server.attach_with_context(Mode::Shell, Some(context)).await;
    for modern in [false, true] {
        let (_, instructions) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/agents")}),
                modern,
                0,
            )
            .await;
        assert_eq!(
            data(&instructions)["files"][0]["content"],
            "# Project instructions\nfirst revision\n"
        );
        let uri = format!("ssmcp://clients/{id}/skills?scope=effective");
        let (_, body) = server
            .rpc("resources/read", json!({"uri":uri}), modern, 0)
            .await;
        let value = data(&body);
        let skills = value["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        let shared = skills.iter().find(|item| item["name"] == "shared").unwrap();
        assert_eq!(shared["root"]["id"], "workspace");
        assert_eq!(shared["description"], "Project description");
        let global_skill = skills
            .iter()
            .find(|item| item["name"] == "global-only")
            .unwrap();
        assert_eq!(global_skill["root"]["id"], "context-1");
        assert!(global_skill["readUri"].as_str().unwrap().len() > 1024);
        assert!(global_skill["readUri"].as_str().unwrap().contains("%26"));
        for item in [shared, global_skill] {
            let (_, body) = server
                .rpc("resources/read", json!({"uri":item["readUri"]}), modern, 0)
                .await;
            let text = body["result"]["contents"][0]["text"].as_str().unwrap();
            assert_eq!(
                text,
                fs::read_to_string(
                    if item["root"]["id"] == "workspace" {
                        &project
                    } else {
                        &global
                    }
                    .join(item["path"].as_str().unwrap())
                )
                .unwrap()
            );
        }
        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/skills/view")}),
                modern,
                0,
            )
            .await;
        let markdown = body["result"]["contents"][0]["text"].as_str().unwrap();
        assert!(markdown.contains("Project description"), "{markdown}");
        assert!(markdown.contains("Available globally"), "{markdown}");
        assert!(!markdown.contains("Global duplicate"), "{markdown}");
    }

    // Editing the same files on a live client is visible through the same URIs.
    fs::write(
        project.join("AGENTS.md"),
        "# Project instructions\nsecond revision\n",
    )
    .unwrap();
    fs::write(
        project.join("skills/shared/SKILL.md"),
        "---\nname: shared\ndescription: Updated description\n---\nUpdated body\n",
    )
    .unwrap();
    for modern in [false, true] {
        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/agents")}),
                modern,
                0,
            )
            .await;
        let value = data(&body);
        assert_eq!(
            value["files"][0]["content"],
            "# Project instructions\nsecond revision\n"
        );
        let (_, raw) = server
            .rpc(
                "resources/read",
                json!({"uri":value["files"][0]["readUri"]}),
                modern,
                0,
            )
            .await;
        assert_eq!(
            raw["result"]["contents"][0]["text"],
            value["files"][0]["content"]
        );
        let (_, body) = server
            .rpc(
                "resources/read",
                json!({"uri":format!("ssmcp://clients/{id}/skills?scope=effective")}),
                modern,
                0,
            )
            .await;
        let value = data(&body);
        let skill = value["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "shared")
            .unwrap();
        assert_eq!(skill["description"], "Updated description");
        let (_, raw) = server
            .rpc("resources/read", json!({"uri":skill["readUri"]}), modern, 0)
            .await;
        assert!(
            raw["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .ends_with("Updated body\n")
        );
    }

    fs::remove_file(project.join("AGENTS.md")).unwrap();
    fs::remove_file(project.join("skills/shared/SKILL.md")).unwrap();
    let (_, body) = server
        .rpc(
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/agents")}),
            false,
            0,
        )
        .await;
    assert!(data(&body)["files"].as_array().unwrap().is_empty());
    let (_, body) = server
        .rpc(
            "resources/read",
            json!({"uri":format!("ssmcp://clients/{id}/skills?scope=effective")}),
            false,
            0,
        )
        .await;
    let value = data(&body);
    let shared = value["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["name"] == "shared")
        .unwrap();
    assert_eq!(shared["root"]["id"], "context-1");
    assert_eq!(shared["description"], "Global duplicate");
}
