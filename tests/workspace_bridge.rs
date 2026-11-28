//! Real system rsync and loopback TCP through authenticated HTTP code_mode calls.
#![cfg(target_os = "linux")]

#[path = "support/http_client.rs"]
mod http_client;
#[path = "support/verified_broker.rs"]
mod verified_broker;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use ssmcp::{
    backend,
    broker::Broker,
    rsync::Rsync,
    wire::{self, ClientMetadata, Frame, Mode, Operation},
    workspace::Workspace,
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream, UnixStream},
    sync::Mutex,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "ssmcp-bridge-e2e-{}",
            format_args!(
                "{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Peer {
    id: String,
    token: String,
    root: PathBuf,
    workspace: Arc<Workspace>,
    rsync: Arc<Rsync>,
    calls: Arc<StdMutex<HashMap<String, usize>>>,
    task: JoinHandle<()>,
    attachment: JoinHandle<ssmcp::Result<()>>,
}

struct Server {
    broker: Arc<Broker>,
    http: reqwest::Client,
    url: String,
    owner: String,
    task: JoinHandle<()>,
    peers: Vec<Peer>,
    scratch: Scratch,
    _accounts: verified_broker::Accounts,
}

impl Server {
    async fn start() -> Self {
        let mut broker = Broker::new("https://ssmcp.example".into());
        let accounts = verified_broker::Accounts::configure(&mut broker);
        let broker = Arc::new(broker);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = ssmcp::http::router(broker.clone());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            broker,
            http: http_client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap(),
            url: format!("http://{address}"),
            owner: String::new(),
            task,
            peers: Vec::new(),
            scratch: Scratch::new(),
            _accounts: accounts,
        }
    }

    async fn attach(&mut self, name: &str) -> String {
        let root = self.scratch.0.join(name);
        std::fs::create_dir(&root).unwrap();
        let workspace = Arc::new(Workspace::new(root.clone()).unwrap());
        let rsync = Arc::new(
            Rsync::with_helper(root.clone(), PathBuf::from(env!("CARGO_BIN_EXE_ssmcp"))).unwrap(),
        );
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
                    name: name.into(),
                    title: None,
                    cwd: root.to_str().unwrap().into(),
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
            panic!("missing registration")
        };
        self.owner = url.rsplit('/').next().unwrap().into();
        let token = self.oauth_login(&url, &login_code).await;
        let writer = Arc::new(Mutex::new(writer));
        let workspace_copy = workspace.clone();
        let rsync_copy = rsync.clone();
        let calls = Arc::new(StdMutex::new(HashMap::<String, usize>::new()));
        let calls_copy = calls.clone();
        let task = tokio::spawn(async move {
            let mut operations = JoinSet::new();
            loop {
                tokio::select! {
                    frame = wire::read_frame(&mut reader) => {
                        let (id, operation) = match frame {
                            Ok(Some(Frame::Call { id, operation, .. })) => (id, operation),
                            Ok(Some(Frame::CodeModeFinished { .. })) => continue,
                            _ => break,
                        };
                        let workspace = workspace_copy.clone();
                        let rsync = rsync_copy.clone();
                        let writer = writer.clone();
                        let calls = calls_copy.clone();
                        operations.spawn(async move {
                            let result = match operation {
                                Operation::ListTools { cursor: None } => {
                                    let mut catalog = Workspace::tools();
                                    catalog.extend(Rsync::tools());
                                    Ok(json!({"tools":catalog}))
                                }
                                Operation::CallTool { name, arguments } => {
                                    *calls.lock().unwrap().entry(name.clone()).or_default() += 1;
                                    let output = if name.starts_with("ssmcp.rsync.") {
                                        rsync.perform(&name, arguments).await
                                    } else {
                                        workspace.perform(&name, arguments).await
                                    };
                                    output.map(backend::result)
                                }
                                _ => Err("unsupported fixture operation".into()),
                            };
                            let _ = wire::write_frame(&mut *writer.lock().await, &Frame::Reply { id, result }).await;
                        });
                    }
                    _ = operations.join_next(), if !operations.is_empty() => {}
                }
            }
            operations.abort_all();
            workspace_copy.shutdown().await;
            rsync_copy.shutdown().await;
        });
        self.peers.push(Peer {
            id: client_id.clone(),
            token,
            root,
            workspace,
            rsync,
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

    async fn code_raw(&self, issuer: usize, code: &str) -> Value {
        let response = self
            .http
            .post(format!("{}/{}", self.url, self.owner))
            .bearer_auth(&self.peers[issuer].token)
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name":"code_mode","arguments":{"code":code,"timeout_ms":25000}
                }}),
            )
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status.as_u16(), 200, "{body}");
        body
    }

    async fn code_as(&self, issuer: usize, code: &str) -> Value {
        let body = self.code_raw(issuer, code).await;
        assert!(body.get("error").is_none(), "{body}");
        assert_ne!(body["result"]["isError"], true, "{body}");
        body["result"]["structuredContent"]["result"].clone()
    }

    async fn code(&self, code: &str) -> Value {
        self.code_as(0, code).await
    }

    async fn sync(
        &self,
        source: usize,
        source_path: &str,
        destination: usize,
        destination_path: &str,
        options: Value,
    ) -> Value {
        let mut args = options;
        args["source"] = json!({"client_id":self.peers[source].id,"path":source_path});
        args["destination"] =
            json!({"client_id":self.peers[destination].id,"path":destination_path});
        let job = self.code(&format!("return await rsync({args});")).await;
        assert_eq!(job["kind"], "sync", "{job}");
        let id = job["id"].as_str().unwrap();
        let end = Instant::now() + Duration::from_secs(25);
        loop {
            let status = self
                .code(&format!(
                    "return await jobs.wait({}, {{wait_ms:1000}});",
                    json!(id)
                ))
                .await;
            match status["state"].as_str().unwrap() {
                "completed" => {
                    assert_eq!(status["result"]["sender"]["exit_code"], 0, "{status}");
                    assert_eq!(status["result"]["receiver"]["exit_code"], 0, "{status}");
                    return status;
                }
                "failed" | "cancelled" => panic!("sync did not complete: {status}"),
                "running" => assert!(
                    Instant::now() < end,
                    "sync exceeded test deadline: {status}"
                ),
                value => panic!("unknown job state: {value}"),
            }
        }
    }

    async fn shutdown(&self) {
        for peer in &self.peers {
            peer.task.abort();
            peer.attachment.abort();
            peer.workspace.shutdown().await;
            peer.rsync.shutdown().await;
        }
        self.task.abort();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        for peer in &self.peers {
            peer.task.abort();
            peer.attachment.abort();
        }
    }
}

fn bytes(size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| (i.wrapping_mul(37) & 255) as u8)
        .collect()
}

fn count(peer: &Peer, name: &str) -> usize {
    peer.calls.lock().unwrap().get(name).copied().unwrap_or(0)
}

#[tokio::test]
#[ignore = "requires system rsync, Node.js, bubblewrap and permitted Linux namespaces"]
async fn real_rsync_dry_run_forward_reverse_delete_and_rooted_fs() {
    let mut server = Server::start().await;
    let left = server.attach("left").await;
    let right = server.attach("right").await;
    let source = server.peers[0].root.join("project");
    let destination = server.peers[1].root.join("mirror");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    let binary = bytes(512 * 1024 + 19);
    std::fs::write(source.join("nested/binary.dat"), &binary).unwrap();
    std::fs::write(source.join(".hidden"), b"hidden file\n").unwrap();
    std::fs::write(
        destination.join("keep.txt"),
        b"preserve until explicit delete",
    )
    .unwrap();

    // dry_run defaults to true; even a successful job must not create destination files.
    let dry = server.sync(0, "project", 1, "mirror", json!({})).await;
    assert!(!destination.join("nested").exists());
    assert!(destination.join("keep.txt").exists());
    let denied = server
        .code_raw(1, &format!("return await jobs.status({});", dry["id"]))
        .await;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert!(
        server.code_as(1, "return await jobs.list();").await["jobs"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    server
        .sync(0, "project", 1, "mirror", json!({"dry_run":false}))
        .await;
    assert_eq!(
        std::fs::read(destination.join("nested/binary.dat")).unwrap(),
        binary
    );
    assert_eq!(
        std::fs::read(destination.join(".hidden")).unwrap(),
        b"hidden file\n"
    );
    assert!(destination.join("keep.txt").exists());

    std::fs::write(destination.join("return.bin"), bytes(1025)).unwrap();
    server
        .sync(1, "mirror", 0, "project", json!({"dry_run":false}))
        .await;
    assert_eq!(
        std::fs::read(source.join("return.bin")).unwrap(),
        bytes(1025)
    );
    std::fs::remove_file(source.join("return.bin")).unwrap();
    server
        .sync(
            0,
            "project",
            1,
            "mirror",
            json!({"dry_run":false,"delete":true}),
        )
        .await;
    assert!(!destination.join("return.bin").exists());

    let fs_result = server
        .code(&format!(
            r#"
        const unpack=r=>r.structuredContent??r;
        const created=unpack(await fs.mkdir({left},{{path:"small",parents:true}}));
        await fs.write({left},{{path:"small/data",data_base64:"AAECA/8="}});
        const value=unpack(await fs.read({left},{{path:"small/data"}}));
        const meta=unpack(await fs.stat({left},{{path:"."}}));
        const denied=[];
        for(const path of ["../escape","/etc/passwd","nested/../../escape"]) {{
            try {{
                const r=await fs.read({left},{{path}});
                denied.push(r.isError===true);
            }} catch(e) {{ denied.push(true); }}
        }}
        return {{value,meta,denied}};
    "#,
            left = json!(left)
        ))
        .await;
    assert_eq!(
        STANDARD
            .decode(fs_result["value"]["data_base64"].as_str().unwrap())
            .unwrap(),
        [0, 1, 2, 3, 255]
    );
    assert_eq!(fs_result["denied"], json!([true, true, true]));
    assert!(count(&server.peers[0], "ssmcp.rsync.start") > 0);
    assert!(count(&server.peers[1], "ssmcp.rsync.start") > 0);
    assert_ne!(left, right);
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires system rsync, Node.js, bubblewrap and permitted Linux namespaces"]
async fn tcp_mappings_work_in_both_directions_preserve_half_close_and_cleanup() {
    let mut server = Server::start().await;
    server.attach("left").await;
    server.attach("right").await;
    for (listen_peer, target_peer) in [(0, 1), (1, 0)] {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let target_task = tokio::spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let mut request = Vec::new();
            socket.read_to_end(&mut request).await.unwrap();
            request.reverse();
            socket.write_all(&request).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mapping = server
            .code(&format!(
                "return await ports.open({});",
                json!({
                    "listen":{"client_id":server.peers[listen_peer].id,"port":0},
                    "target":{"client_id":server.peers[target_peer].id,"port":target_port},
                    "ttl_ms":30000
                })
            ))
            .await;
        assert_eq!(mapping["state"], "running", "{mapping}");
        let address = mapping["listener"]["address"].as_str().unwrap();
        let id = mapping["id"].as_str().unwrap();
        let denied = server
            .code_raw(1, &format!("return await ports.close({});", json!(id)))
            .await;
        assert_eq!(denied["result"]["isError"], true, "{denied}");
        let list = server.code("return await ports.list();").await;
        assert!(
            list["ports"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == id)
        );

        let mut socket = TcpStream::connect(address).await.unwrap();
        let payload = bytes(128 * 1024 + 17);
        timeout(Duration::from_secs(15), async {
            socket.write_all(&payload).await.unwrap();
            socket.shutdown().await.unwrap();
            let mut reply = Vec::new();
            socket.read_to_end(&mut reply).await.unwrap();
            let expected: Vec<_> = payload.into_iter().rev().collect();
            assert_eq!(reply, expected);
        })
        .await
        .expect("mapped stream stalled after write half-close");
        timeout(Duration::from_secs(1), target_task)
            .await
            .unwrap()
            .unwrap();
        assert!(count(&server.peers[listen_peer], "ssmcp.net.listen") > 0);
        assert!(count(&server.peers[target_peer], "ssmcp.net.connect") > 0);
        let closed = server
            .code(&format!("return await ports.close({});", json!(id)))
            .await;
        assert_eq!(closed["state"], "closed", "{closed}");
        assert!(TcpStream::connect(address).await.is_err());
        for peer in &server.peers {
            let handles = peer
                .workspace
                .perform("ssmcp.net.list", json!({}))
                .await
                .unwrap();
            assert!(
                handles["handles"].as_array().unwrap().is_empty(),
                "{handles}"
            );
        }
    }
    server.shutdown().await;
}

#[tokio::test]
#[ignore = "requires system rsync, Node.js, bubblewrap and permitted Linux namespaces"]
async fn cancelling_an_active_sync_closes_every_owned_rsync_endpoint() {
    let mut server = Server::start().await;
    server.attach("left").await;
    server.attach("right").await;
    let source = server.peers[0].root.join("source");
    let destination = server.peers[1].root.join("destination");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    // A sparse fixture makes a transfer long enough to cancel without allocating a large buffer.
    std::fs::File::create(source.join("large.bin"))
        .unwrap()
        .set_len(64 * 1024 * 1024)
        .unwrap();
    let args = json!({
        "source":{"client_id":server.peers[0].id,"path":"source"},
        "destination":{"client_id":server.peers[1].id,"path":"destination"},
        "dry_run":false
    });
    let job = server.code(&format!("return await rsync({args});")).await;
    let id = job["id"].as_str().unwrap();
    timeout(Duration::from_secs(5), async {
        while server
            .peers
            .iter()
            .any(|peer| count(peer, "ssmcp.rsync.write") == 0)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("rsync did not enter the bidirectional data pump");
    let stopped = server
        .code(&format!("return await jobs.cancel({});", json!(id)))
        .await;
    assert_eq!(stopped["state"], "cancelled", "{stopped}");
    let status = server
        .code(&format!("return await jobs.status({});", json!(id)))
        .await;
    assert_eq!(status["state"], "cancelled", "{status}");
    for peer in &server.peers {
        assert_eq!(
            count(peer, "ssmcp.rsync.start"),
            count(peer, "ssmcp.rsync.close"),
            "cancel returned before acquired endpoints were closed"
        );
    }
    server.shutdown().await;
}
