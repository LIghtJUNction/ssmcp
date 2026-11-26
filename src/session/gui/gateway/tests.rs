use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use tokio::{net::TcpListener, task::JoinHandle};
use zbridge::{
    Capabilities, Command, CommandResult, Effect, Event, EventPage, Mode, Resource,
    State as BridgeState,
};

const TOKEN: &str = "1111111111111111111111111111111111111111111111111111111111111111";
fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[derive(Default)]
struct Counts {
    opens: AtomicUsize,
    closes: AtomicUsize,
    commands: AtomicUsize,
    reads: AtomicUsize,
    close_failures: AtomicUsize,
    online_limit: AtomicBool,
}
struct FixtureHost {
    counts: Arc<Counts>,
}
struct FixtureCore {
    counts: Arc<Counts>,
    read_only: bool,
    closed: AtomicBool,
}
impl Host for FixtureHost {
    fn relay_refusal(&self, session: &str, _settle: bool) -> CoreFuture<'_, Option<RelayRefusal>> {
        let refused = session == "fixture" && self.counts.online_limit.load(Ordering::Acquire);
        Box::pin(async move {
            Ok(refused.then(|| {
                RelayRefusal::from_destination(
                    RelayAdmissionDenial::OnlineSessionLimit,
                    "alice@relay.example",
                    Effect::MayHaveOccurred,
                )
                .unwrap()
            }))
        })
    }

    fn sessions(&self) -> CoreFuture<'_, Vec<Value>> {
        Box::pin(async {
            Ok(vec![
                json!({"name":"fixture","cwd":"/fixture","manager_state":"active","manager_active":true,"connection_state":"disconnected","phase":"idle","generation":"fixture-manager","stop_requested":false,"state_directory":"/private/descriptor-directory","token":"not-for-http","args":["private"]}),
            ])
        })
    }
    fn connect(&self, session: String, read_only: bool) -> CoreFuture<'_, Arc<dyn Connection>> {
        Box::pin(async move {
            if session != "fixture" {
                return Err(unavailable("unknown fixture session"));
            }
            self.counts.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Arc::new(FixtureCore {
                counts: self.counts.clone(),
                read_only,
                closed: AtomicBool::new(false),
            }) as Arc<dyn Connection>)
        })
    }
}
fn id(value: &str) -> Id {
    Id::new(value).unwrap()
}
impl Connection for FixtureCore {
    fn request(&self, request: BridgeRequest) -> CoreFuture<'_, BridgeResponse> {
        Box::pin(async move {
            self.counts.reads.fetch_add(1, Ordering::Relaxed);
            if self.closed.load(Ordering::Acquire) {
                return Err(BridgeError::new(ErrorCode::Closed, "fixture detached"));
            }
            let generation = id("fixture-core");
            let body = if request
                .operation
                .generation()
                .is_some_and(|g| g != &generation)
            {
                ResponseBody::Error(BridgeError::new(
                    ErrorCode::StaleGeneration,
                    "fixture generation changed",
                ))
            } else {
                match request.operation {
                    Operation::Capabilities {} => ResponseBody::Capabilities(Capabilities {
                        application: id("ssmcp"),
                        mode: if self.read_only {
                            Mode::Observe
                        } else {
                            Mode::Control
                        },
                        resources: vec![Resource {
                            id: id("runtime"),
                            schema_version: 1,
                            description: "Actual fixture state".into(),
                        }],
                        commands: if self.read_only {
                            vec![]
                        } else {
                            vec![Command {
                                id: id("terminal.write"),
                                description: "Fixture single-send command".into(),
                                input_schema: json!({"type":"object"}),
                            }]
                        },
                        events: true,
                        max_frame_bytes: zbridge::MAX_FRAME_BYTES as u32,
                        max_event_page_bytes: zbridge::MAX_PAGE_BYTES as u32,
                    }),
                    Operation::State { resource, .. } => ResponseBody::State(BridgeState {
                        resource,
                        schema_version: 1,
                        revision: None,
                        value: json!({"backend_active":false,"live_cursor":"9007199254740993"}),
                    }),
                    Operation::Events { cursor, .. } => {
                        let start = cursor.unwrap_or(Counter(9_007_199_254_740_993));
                        ResponseBody::Events(EventPage {
                            events: vec![Event {
                                sequence: start,
                                topic: id("ssmcp.fixture"),
                                value: json!({"fixture":true}),
                            }],
                            start_cursor: start,
                            next_cursor: Counter(start.0 + 1),
                            live_cursor: Counter(start.0 + 1),
                            retention_gap: false,
                            has_more: false,
                        })
                    }
                    Operation::Command { command, .. } => {
                        if self.read_only {
                            ResponseBody::Error(BridgeError::new(
                                ErrorCode::Denied,
                                "fixture observer cannot write",
                            ))
                        } else {
                            self.counts.commands.fetch_add(1, Ordering::Relaxed);
                            if command.as_str() == "terminal.write" {
                                return Err(unavailable(
                                    "fixture lost its native receipt after execution",
                                ));
                            }
                            ResponseBody::Command(CommandResult {
                                command,
                                value: json!({"accepted":true}),
                            })
                        }
                    }
                }
            };
            Ok(BridgeResponse {
                version: zbridge::VERSION,
                id: request.id,
                generation: Some(generation),
                body,
            })
        })
    }
    fn close(&self) -> CoreFuture<'_, ()> {
        Box::pin(async move {
            if self
                .counts
                .close_failures
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Err(unavailable("fixture lost its detach acknowledgement"));
            }
            if !self.closed.swap(true, Ordering::AcqRel) {
                self.counts.closes.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        })
    }
}

struct Server {
    url: String,
    task: JoinHandle<()>,
    counts: Arc<Counts>,
}
impl Server {
    async fn new(assets: Option<PathBuf>) -> Self {
        Self::with_launch_refusal(assets, None).await
    }

    async fn with_launch_refusal(assets: Option<PathBuf>, reason: Option<RelayRefusal>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let counts = Arc::new(Counts::default());
        let app = router(
            Config {
                token: TOKEN.into(),
                origin: url.clone(),
                assets,
            },
            Arc::new(FixtureHost {
                counts: counts.clone(),
            }),
        )
        .unwrap();
        let app = if let Some(reason) = reason {
            app.layer(axum::Extension(reason))
        } else {
            app
        };
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, task, counts }
    }
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        http()
            .request(method, format!("{}{PREFIX}{path}", self.url))
            .bearer_auth(TOKEN)
    }
    async fn open(&self, key: &str, read_only: bool) -> Value {
        let response = self
            .request(reqwest::Method::POST, "/connections")
            .json(&json!({"request_id":key,"session":"fixture","read_only":read_only}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response.json().await.unwrap()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn default_gateway_serves_the_bundled_browser_app() {
    let server = Server::new(None).await;
    for (path, mime) in [
        ("/", "text/html"),
        ("/styles.css", "text/css"),
        ("/app.mjs", "text/javascript"),
        ("/src/gateway.mjs", "text/javascript"),
    ] {
        let page = http()
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK, "{path}");
        assert!(
            page.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with(mime)
        );
        assert!(!page.bytes().await.unwrap().is_empty());
    }
    for path in ["/dev-server.mjs", "/pairing.json", "/test/fixtures.mjs"] {
        let page = http()
            .get(format!("{}{path}", server.url))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn http_requires_pairing_and_exact_origin_before_accessing_core() {
    let server = Server::new(None).await;
    let url = format!("{}{PREFIX}/sessions", server.url);
    assert_eq!(
        http().get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http()
            .get(&url)
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .request(reqwest::Method::GET, "/sessions")
            .header(header::ORIGIN, "https://unrelated.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .request(reqwest::Method::GET, "/sessions")
            .header(header::HOST, "unrelated.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = server
        .request(reqwest::Method::GET, "/sessions")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    let page: Value = response.json().await.unwrap();
    assert_eq!(page["sessions"][0]["connection_state"], "disconnected");
    for private in ["state_directory", "token", "args"] {
        assert!(page["sessions"][0].get(private).is_none());
    }
    assert_eq!(server.counts.opens.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn paired_startup_refusal_opens_setup_and_reads_the_original_result_without_launch() {
    let mut url = reqwest::Url::parse("http://127.0.0.1:1234").unwrap();
    let reason = RelayRefusal::from_destination(
        RelayAdmissionDenial::TrafficQuotaExceeded,
        "alice@relay.example",
        Effect::MayHaveOccurred,
    )
    .unwrap();
    let reason =
        super::super::launch_parameters(&mut url, Err(CreationError::Relay(reason))).unwrap();
    assert_eq!(url.query(), Some("setup=1&launch_refused=1"));
    let server = Server::with_launch_refusal(None, reason).await;
    let unpaired = http()
        .get(format!("{}{PREFIX}/launch", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(unpaired.status(), StatusCode::UNAUTHORIZED);
    for _ in 0..2 {
        let result = server
            .request(reqwest::Method::GET, "/launch")
            .send()
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::PAYMENT_REQUIRED);
        let body: Value = result.json().await.unwrap();
        assert_eq!(body["error"]["effect"], "may_have_occurred");
        assert_eq!(body["error"]["code"], "traffic_quota_exceeded");
        assert_eq!(body["error"]["relay_origin"], "https://relay.example");
    }
    assert_eq!(server.counts.opens.load(Ordering::Relaxed), 0);
    let ready = Server::new(None).await;
    let result: Value = ready
        .request(reqwest::Method::GET, "/launch")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["status"], "ready");
}

#[tokio::test]
async fn registered_online_limit_reaches_open_events_and_bridge_without_replaying() {
    let server = Server::new(None).await;
    let opened = server.open("original", false).await;
    let id = opened["connection_id"].as_str().unwrap();
    server.counts.online_limit.store(true, Ordering::Release);
    for (path, input) in [
        (
            "/connections".into(),
            json!({"request_id":"original","session":"fixture","read_only":false}),
        ),
        (
            format!("/connections/{id}/events"),
            json!({"request_id":"observe","generation":"fixture-core","wait_ms":0}),
        ),
        (
            format!("/connections/{id}/bridge"),
            json!({"version":1,"id":"caps","operation":{"op":"capabilities"}}),
        ),
    ] {
        let response = server
            .request(reqwest::Method::POST, &path)
            .json(&input)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], "online_session_limit");
        assert_eq!(body["error"]["effect"], "may_have_occurred");
        assert_eq!(body["error"]["relay_origin"], "https://relay.example");
    }
    assert_eq!(server.counts.opens.load(Ordering::Relaxed), 1);
    assert_eq!(server.counts.commands.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn open_retries_use_one_lease_and_closed_receipts_do_not_resurrect_it() {
    let server = Server::new(None).await;
    let opened = server.open("open-once", true).await;
    assert_eq!(opened["capabilities"]["body"]["value"]["mode"], "observe");
    assert_eq!(opened["capabilities"]["id"], "open-once");
    assert_eq!(
        opened["capabilities"]["body"]["value"]["commands"],
        json!([])
    );
    let request = json!({"request_id":"open-once","session":"fixture","read_only":true});
    let duplicate: Value = server
        .request(reqwest::Method::POST, "/connections")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(duplicate["connection_id"], opened["connection_id"]);
    assert_eq!(duplicate["duplicate"], true);
    assert_eq!(server.counts.opens.load(Ordering::Relaxed), 1);
    let conflict = server
        .request(reqwest::Method::POST, "/connections")
        .json(&json!({"request_id":"open-once","session":"fixture","read_only":false}))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    let path = format!("/connections/{}", opened["connection_id"].as_str().unwrap());
    let closed: Value = server
        .request(reqwest::Method::DELETE, &path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(closed["session_stopped"], false);
    let retry: Value = server
        .request(reqwest::Method::POST, "/connections")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(retry["status"], "closed");
    assert_eq!(server.counts.opens.load(Ordering::Relaxed), 1);
    assert_eq!(server.counts.closes.load(Ordering::Relaxed), 1);
    assert_eq!(
        server
            .request(reqwest::Method::DELETE, &path)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        server
            .request(reqwest::Method::DELETE, "/connections/not-observed")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::GONE
    );
}

#[tokio::test]
async fn failed_detach_stays_closed_to_work_until_an_explicit_retry_is_acknowledged() {
    let server = Server::new(None).await;
    let opened = server.open("detach-once", true).await;
    let path = format!("/connections/{}", opened["connection_id"].as_str().unwrap());
    server.counts.close_failures.store(1, Ordering::Relaxed);
    let first = server
        .request(reqwest::Method::DELETE, &path)
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::SERVICE_UNAVAILABLE);
    let error: Value = first.json().await.unwrap();
    assert_eq!(error["error"]["effect"], "may_have_occurred");
    let list: Value = server
        .request(reqwest::Method::GET, "/connections")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["connections"][0]["status"], "closing");
    let blocked = server
        .request(reqwest::Method::POST, &format!("{path}/bridge"))
        .json(&json!({"version":1,"id":"blocked","operation":{"op":"capabilities"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::GONE);
    let second = server
        .request(reqwest::Method::DELETE, &path)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(server.counts.closes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn closing_a_long_lived_connection_keeps_its_open_retry_receipt() {
    let counts = Arc::new(Counts::default());
    let request = OpenRequest {
        request_id: id("long-open"),
        session: "fixture".into(),
        read_only: true,
    };
    let connection_id = "gui_long".to_owned();
    let core = Arc::new(FixtureCore {
        counts: counts.clone(),
        read_only: true,
        closed: AtomicBool::new(false),
    });
    let state = Arc::new(Gateway {
        token: TOKEN.into(),
        origin: "http://127.0.0.1:1".into(),
        authority: "127.0.0.1:1".into(),
        assets: None,
        host: Arc::new(FixtureHost {
            counts: counts.clone(),
        }),
        creations: Mutex::new(Vec::new()),
        next_connection: AtomicU64::new(2),
        requests: Semaphore::new(32),
        data: Mutex::new(Data {
            connections: BTreeMap::from([(
                connection_id.clone(),
                Connected {
                    session: "fixture".into(),
                    read_only: true,
                    request_id: request.request_id.clone(),
                    core,
                    used: Instant::now(),
                    closing: false,
                },
            )]),
            opens: VecDeque::from([OpenReceipt {
                request,
                connection_id: connection_id.clone(),
                retained_at: Instant::now() - RETENTION - Duration::from_secs(1),
                outcome: OpenOutcome::Connected,
            }]),
        }),
    });
    let _ = close(State(state.clone()), Path(connection_id))
        .await
        .unwrap();
    state.prune();
    assert_eq!(state.data.lock().unwrap().opens.len(), 1);
    let request = Request::builder()
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"request_id":"long-open","session":"fixture","read_only":true}).to_string(),
        ))
        .unwrap();
    let response = open(State(state), request).await.unwrap();
    let body = to_bytes(response.into_body(), 4096).await.unwrap();
    let reply: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(reply["status"], "closed");
    assert_eq!(counts.opens.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn idle_expiry_preserves_an_unconfirmed_release_and_its_retry_receipt() {
    let counts = Arc::new(Counts::default());
    counts.close_failures.store(1, Ordering::Relaxed);
    let request = OpenRequest {
        request_id: id("idle-open"),
        session: "fixture".into(),
        read_only: true,
    };
    let connection_id = "gui_idle".to_owned();
    let expired = Instant::now() - RETENTION - Duration::from_secs(1);
    let core = Arc::new(FixtureCore {
        counts: counts.clone(),
        read_only: true,
        closed: AtomicBool::new(false),
    });
    let state = Arc::new(Gateway {
        token: TOKEN.into(),
        origin: "http://127.0.0.1:1".into(),
        authority: "127.0.0.1:1".into(),
        assets: None,
        host: Arc::new(FixtureHost {
            counts: counts.clone(),
        }),
        creations: Mutex::new(Vec::new()),
        next_connection: AtomicU64::new(2),
        requests: Semaphore::new(32),
        data: Mutex::new(Data {
            connections: BTreeMap::from([(
                connection_id.clone(),
                Connected {
                    session: "fixture".into(),
                    read_only: true,
                    request_id: request.request_id.clone(),
                    core,
                    used: expired,
                    closing: false,
                },
            )]),
            opens: VecDeque::from([OpenReceipt {
                request,
                connection_id: connection_id.clone(),
                retained_at: expired,
                outcome: OpenOutcome::Connected,
            }]),
        }),
    });
    state.prune();
    assert!(state.connection(&connection_id).is_err());
    timeout(Duration::from_secs(1), async {
        while counts.close_failures.load(Ordering::Relaxed) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.prune();
    {
        let data = state.data.lock().unwrap();
        assert!(data.connections[&connection_id].closing);
        assert_eq!(data.opens.len(), 1);
    }
    assert_eq!(counts.closes.load(Ordering::Relaxed), 0);
    let _ = close(State(state.clone()), Path(connection_id))
        .await
        .unwrap();
    state.prune();
    assert!(state.data.lock().unwrap().connections.is_empty());
    assert_eq!(state.data.lock().unwrap().opens.len(), 1);
    assert_eq!(counts.closes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn real_http_event_envelopes_preserve_large_cursors_and_generation() {
    let server = Server::new(None).await;
    let opened = server.open("events-open", true).await;
    let path = format!(
        "/connections/{}/events",
        opened["connection_id"].as_str().unwrap()
    );
    let fixture: Value = serde_json::from_str(include_str!("contract-v1.json")).unwrap();
    let reply: Value = server
        .request(reqwest::Method::POST, &path)
        .json(&fixture["observe_request"])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reply, fixture["observe_response"]);
    assert_eq!(reply["schema"], "ssmcp-gui-events-v1");
    assert_eq!(reply["request_id"], "observe");
    assert_eq!(reply["page"]["next_cursor"], "9007199254740994");
    let stale=server.request(reqwest::Method::POST,&path).json(&json!({"request_id":"stale","generation":"replaced","cursor":"9007199254740993","wait_ms":0})).send().await.unwrap();
    assert_eq!(stale.status(), StatusCode::CONFLICT);
    let invalid=server.request(reqwest::Method::POST,&path).json(&json!({"request_id":"bad","generation":"fixture-core","cursor":9007199254740993_u64,"wait_ms":0})).send().await.unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn readonly_commands_are_denied_without_entering_native_transport() {
    let server = Server::new(None).await;
    let opened = server.open("readonly-open", true).await;
    let path = format!(
        "/connections/{}/bridge",
        opened["connection_id"].as_str().unwrap()
    );
    let reads_before = server.counts.reads.load(Ordering::Relaxed);
    let request = BridgeRequest {
        version: zbridge::VERSION,
        id: id("readonly-command"),
        operation: Operation::Command {
            generation: id("fixture-core"),
            command: id("terminal.write"),
            arguments: json!({"terminal_id":"fixture","data":""}),
        },
    };
    let response = server
        .request(reqwest::Method::POST, &path)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: BridgeResponse = response.json().await.unwrap();
    response.validate_for(&request).unwrap();
    let ResponseBody::Error(error) = response.body else {
        panic!("readonly command was not denied")
    };
    assert_eq!(error.code, ErrorCode::Denied);
    assert_eq!(error.effect, Effect::None);
    // Count entry to Connection::request, not only successful commands: the
    // permission check must happen before even the native transport is called.
    assert_eq!(server.counts.reads.load(Ordering::Relaxed), reads_before);
    assert_eq!(server.counts.commands.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn a_lost_command_receipt_is_unknown_and_is_never_automatically_replayed() {
    let server = Server::new(None).await;
    let opened = server.open("control-open", false).await;
    let path = format!(
        "/connections/{}/bridge",
        opened["connection_id"].as_str().unwrap()
    );
    let request = json!({"version":1,"id":"write-once","operation":{"op":"command","generation":"fixture-core","command":"terminal.write","arguments":{"terminal_id":"fixture","data":"hello"}}});
    let response = server
        .request(reqwest::Method::POST, &path)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let response: BridgeResponse = response.json().await.unwrap();
    let ResponseBody::Error(error) = response.body else {
        panic!("lost receipt was presented as success")
    };
    assert_eq!(error.effect, Effect::MayHaveOccurred);
    assert_eq!(response.id.as_str(), "write-once");
    assert_eq!(response.generation, Some(id("fixture-core")));
    assert_eq!(server.counts.commands.load(Ordering::Relaxed), 1);
    let _ = server
        .request(reqwest::Method::GET, "/connections")
        .send()
        .await
        .unwrap();
    assert_eq!(server.counts.commands.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn invalid_or_oversized_requests_never_reach_the_native_transport() {
    let server = Server::new(None).await;
    let opened = server.open("bounded-open", true).await;
    let path = format!(
        "/connections/{}/bridge",
        opened["connection_id"].as_str().unwrap()
    );
    let before = server.counts.reads.load(Ordering::Relaxed);
    let oversized = server
        .request(reqwest::Method::POST, &path)
        .header(header::CONTENT_TYPE, "application/json")
        .body("x".repeat(zbridge::MAX_FRAME_BYTES + 1))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let forged = server
        .request(reqwest::Method::POST, &path)
        .json(
            &json!({"version":1,"id":"forged","owner":"someone","operation":{"op":"capabilities"}}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status(), StatusCode::BAD_REQUEST);
    let invalid=server.request(reqwest::Method::POST,&path).json(&json!({"version":1,"id":"long","operation":{"op":"events","generation":"fixture-core","wait_ms":55000,"limit":16}})).send().await.unwrap();
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    assert_eq!(server.counts.reads.load(Ordering::Relaxed), before);
}

#[test]
fn remote_cleartext_origins_and_invalid_pairing_secrets_are_rejected() {
    let host = Arc::new(FixtureHost {
        counts: Arc::new(Counts::default()),
    });
    for origin in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com/path",
        "https://example.com/?token=x",
    ] {
        assert!(
            router(
                Config {
                    token: TOKEN.into(),
                    origin: origin.into(),
                    assets: None
                },
                host.clone()
            )
            .is_err()
        );
    }
    assert!(
        router(
            Config {
                token: "short".into(),
                origin: "http://127.0.0.1:1".into(),
                assets: None
            },
            host
        )
        .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn public_assets_cannot_escape_their_directory_or_expose_pairing_json() {
    use std::os::unix::fs::symlink;
    let directory = std::env::temp_dir().join(format!(
        "ssmcp-gui-assets-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let assets = directory.join("assets");
    std::fs::create_dir(&assets).unwrap();
    std::fs::write(
        assets.join("index.html"),
        "<!doctype html><title>Transport fixture</title>",
    )
    .unwrap();
    std::fs::write(directory.join("secret.js"), "private content").unwrap();
    std::fs::write(assets.join("pairing.json"), "private pairing fixture").unwrap();
    symlink(directory.join("secret.js"), assets.join("escape.js")).unwrap();
    let server = Server::new(Some(assets)).await;
    let client = http();
    let response = client.get(&server.url).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
    for path in ["escape.js", "pairing.json", ".secret.js"] {
        assert_eq!(
            client
                .get(format!("{}/{path}", server.url))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    drop(server);
    std::fs::remove_dir_all(directory).unwrap();
}

struct CreationHost {
    count: AtomicUsize,
    dir: PathBuf,
}
impl Host for CreationHost {
    fn receipt_directory(&self) -> Option<&std::path::Path> {
        Some(&self.dir)
    }
    fn authorize_creation(&self, _: &str) -> Result<(), BridgeError> {
        Ok(())
    }
    fn create(&self, destination: String) -> CoreFuture<'_, String, CreationError> {
        Box::pin(async move {
            let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
            if destination == "quota@relay.test" {
                return Err(CreationError::Relay(
                    RelayRefusal::from_destination(
                        RelayAdmissionDenial::TrafficQuotaExceeded,
                        &destination,
                        zbridge::Effect::None,
                    )
                    .unwrap(),
                ));
            }
            if n == 1 {
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Ok(format!("live:{n:032x}"))
        })
    }
    fn sessions(&self) -> CoreFuture<'_, Vec<Value>> {
        Box::pin(async { Ok(vec![]) })
    }
    fn connect(&self, _: String, _: bool) -> CoreFuture<'_, Arc<dyn Connection>> {
        Box::pin(async { Err(unavailable("fixture has no native endpoint")) })
    }
}
#[tokio::test]
async fn creation_requires_pairing_and_consent_and_deduplicates_concurrent_and_old_requests() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(crate::session::random_suffix().unwrap());
    crate::platform::private_directory(&dir).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let host = Arc::new(CreationHost {
        count: AtomicUsize::new(0),
        dir: dir.clone(),
    });
    let app = router(
        Config {
            token: TOKEN.into(),
            origin: url.clone(),
            assets: None,
        },
        host.clone(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = http();
    let uri = format!("{url}{PREFIX}/owned-sessions");
    let create = |id: &str, d: &str| {
        client
            .post(&uri)
            .bearer_auth(TOKEN)
            .json(&json!({"request_id":id,"destination":d,"terms":false,"privacy":false}))
            .send()
    };
    assert_eq!(
        client
            .post(&uri)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .post(&uri)
            .bearer_auth(TOKEN)
            .header("Origin", "https://wrong.test")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        create("no-consent", "alice@ssmcp.lmm.best")
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    let (a, b) = tokio::join!(
        create("same", "alice@relay.test"),
        create("same", "alice@relay.test")
    );
    let a: Value = a.unwrap().error_for_status().unwrap().json().await.unwrap();
    let b: Value = b.unwrap().error_for_status().unwrap().json().await.unwrap();
    assert_eq!(a, b);
    assert_eq!(
        a,
        json!({"request_id":"same","session":"live:00000000000000000000000000000001"})
    );
    assert_eq!(
        create("same", "other@relay.test").await.unwrap().status(),
        StatusCode::CONFLICT
    );
    // More than a complete in-memory receipt window can pass without replaying the old request.
    for n in 0..MAX_OPEN_RECEIPTS + 2 {
        let row: Value = create(&format!("req-{n}"), "alice@relay.test")
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_ne!(a["session"], row["session"]);
    }
    let replay: Value = create("same", "alice@relay.test")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, a);
    assert_eq!(host.count.load(Ordering::SeqCst), MAX_OPEN_RECEIPTS + 3);
    for _ in 0..2 {
        let rejected = create("quota", "quota@relay.test").await.unwrap();
        assert_eq!(rejected.status(), StatusCode::PAYMENT_REQUIRED);
        let body: Value = rejected.json().await.unwrap();
        assert_eq!(body["error"]["code"], "traffic_quota_exceeded");
        assert_eq!(body["error"]["relay_origin"], "https://relay.test");
        assert_eq!(body["error"]["effect"], "none");
    }
    // Same request reads the original persisted refusal without another launch.
    assert_eq!(host.count.load(Ordering::SeqCst), MAX_OPEN_RECEIPTS + 4);
    let legacy = BridgeError::new(ErrorCode::Unavailable, "old receipt").uncertain();
    let decoded: CreationError = serde_json::from_value(json!(&legacy)).unwrap();
    assert!(matches!(decoded, CreationError::Bridge(error) if error == legacy));
    server.abort();
    std::fs::remove_dir_all(dir).unwrap();
}
#[tokio::test]
async fn attach_gateway_rejects_owned_creation() {
    let server = Server::new(None).await;
    let r = server
        .request(reqwest::Method::POST, "/owned-sessions")
        .json(&json!({"request_id":"owned","destination":"alice@relay.test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}
