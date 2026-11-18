//! Browser-to-provider-to-MCP regressions with real loopback HTTP and wire peers.
//! No external account, credential, backend process or model is used.
use super::{COOKIE, OAuthConfig};
use crate::{
    broker::Broker,
    identity::{GitHubConfig, IdentityProviders},
    wire::{self, ClientMetadata, Frame, Mode, Operation},
};
use axum::{
    Form, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{Client, Response, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
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

const PUBLIC: &str = "https://ssmcp.example.test";
const RESOURCE: &str = "https://ssmcp.example.test/mcp";
const CALLBACK: &str = "https://consumer.example.test/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const GITHUB_CLIENT: &str = "github-fixture-client";
const GITHUB_SECRET: &str = "github-fixture-secret";

#[derive(Default)]
struct GitHub {
    exchanges: AtomicUsize,
    profiles: AtomicUsize,
    lookups: AtomicUsize,
    verifiers: Mutex<Vec<String>>,
}

fn profile(login: &str) -> Value {
    match login {
        "alice" => json!({"id":42,"login":"Alice","type":"User"}),
        "bob" => json!({"id":7,"login":"bob","type":"User"}),
        _ => json!({"message":"Not Found"}),
    }
}

async fn github_token(
    State(github): State<Arc<GitHub>>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> (StatusCode, Json<Value>) {
    github.exchanges.fetch_add(1, Ordering::SeqCst);
    assert_eq!(headers[header::ACCEPT], "application/json");
    assert_eq!(form["client_id"], GITHUB_CLIENT);
    assert_eq!(form["client_secret"], GITHUB_SECRET);
    assert_eq!(
        form["redirect_uri"],
        format!("{PUBLIC}/oauth/callback/github")
    );
    github
        .verifiers
        .lock()
        .unwrap()
        .push(form["code_verifier"].clone());
    match form["code"].as_str() {
        "alice-code" => (
            StatusCode::OK,
            Json(json!({"access_token":"github-alice-token","token_type":"bearer","scope":""})),
        ),
        "bob-code" => (
            StatusCode::OK,
            Json(json!({"access_token":"github-bob-token","token_type":"bearer","scope":""})),
        ),
        _ => (
            StatusCode::OK,
            Json(json!({"error":"bad_verification_code"})),
        ),
    }
}

async fn github_user(
    State(github): State<Arc<GitHub>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    github.profiles.fetch_add(1, Ordering::SeqCst);
    match headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    {
        Some("Bearer github-alice-token") => (StatusCode::OK, Json(profile("alice"))),
        Some("Bearer github-bob-token") => (StatusCode::OK, Json(profile("bob"))),
        _ => (StatusCode::UNAUTHORIZED, Json(json!({}))),
    }
}

async fn github_lookup(
    State(github): State<Arc<GitHub>>,
    Path(login): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    github.lookups.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        headers[header::AUTHORIZATION],
        format!(
            "Basic {}",
            STANDARD.encode(format!("{GITHUB_CLIENT}:{GITHUB_SECRET}"))
        )
    );
    let value = profile(&login);
    (
        if value.get("id").is_some() {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        },
        Json(value),
    )
}

struct WireClient {
    id: String,
    resource: String,
    login_code: String,
    catalog_calls: Arc<AtomicUsize>,
    worker: JoinHandle<()>,
    attachment: JoinHandle<crate::Result<()>>,
}

struct BrowserFlow {
    client_id: String,
    transaction: String,
    cookie: String,
    resource: String,
}

struct Fixture {
    broker: Arc<Broker>,
    github: Arc<GitHub>,
    upstream: String,
    base: String,
    http: Client,
    servers: Vec<JoinHandle<()>>,
    clients: Vec<WireClient>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for client in &self.clients {
            client.worker.abort();
            client.attachment.abort();
        }
        for server in &self.servers {
            server.abort();
        }
    }
}

impl Fixture {
    async fn start() -> Self {
        let github = Arc::new(GitHub::default());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = format!("http://{}", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/login/oauth/access_token", post(github_token))
            .route("/user", get(github_user))
            .route("/users/{login}", get(github_lookup))
            .with_state(github.clone());
        let provider_task =
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let mut broker = Broker::with_oauth_config(PUBLIC.into(), OAuthConfig::default()).unwrap();
        broker.identities = IdentityProviders::with_github_test_endpoints(
            PUBLIC,
            GitHubConfig {
                client_id: GITHUB_CLIENT.into(),
                client_secret: GITHUB_SECRET.into(),
            },
            &upstream,
        )
        .unwrap();
        let broker = Arc::new(broker);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let http_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            broker,
            github,
            upstream,
            base,
            http: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            servers: vec![provider_task, http_task],
            clients: Vec::new(),
        }
    }

    async fn attach(&mut self, name: &str, allow: &[&str]) -> usize {
        let (server, client) = UnixStream::pair().unwrap();
        let attachment = tokio::spawn(self.broker.clone().attach(server));
        let (reader, mut writer) = client.into_split();
        let mut reader = BufReader::new(reader);
        wire::write_frame(
            &mut writer,
            &Frame::Hello {
                version: wire::VERSION,
                mode: Mode::Codex,
                metadata: ClientMetadata {
                    name: name.into(),
                    title: None,
                    cwd: format!("/workspace/{name}"),
                    hostname: "oauth-fixture".into(),
                    allow: allow.iter().map(|value| (*value).into()).collect(),
                },
            },
        )
        .await
        .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), wire::read_frame(&mut reader))
            .await
            .unwrap()
            .unwrap();
        let Some(Frame::Ready {
            url,
            login_code,
            login_expires_in,
            client_id,
        }) = frame
        else {
            panic!("wire registration did not return Ready");
        };
        if allow.is_empty() {
            assert_eq!(login_code.len(), 64);
            assert_eq!(login_expires_in, 1800);
            assert_ne!(url, RESOURCE);
        } else {
            assert!(login_code.is_empty());
            assert_eq!(login_expires_in, 0);
            assert_eq!(url, RESOURCE);
        }
        let catalog_calls = Arc::new(AtomicUsize::new(0));
        let recorded = catalog_calls.clone();
        let worker = tokio::spawn(async move {
            while let Ok(Some(frame)) = wire::read_frame(&mut reader).await {
                let (id, operation) = match frame {
                    Frame::Call { id, operation, .. } => (id, operation),
                    Frame::CodeModeFinished { .. } => continue,
                    _ => panic!("unexpected fixture frame"),
                };
                let result = match operation {
                    Operation::ListTools { cursor: None } => {
                        recorded.fetch_add(1, Ordering::SeqCst);
                        Ok(
                            json!({"tools":[{"name":"fixture.echo", "inputSchema":{"type":"object","additionalProperties":false}}]}),
                        )
                    }
                    _ => Err("unsupported fixture operation".into()),
                };
                if wire::write_frame(&mut writer, &Frame::Reply { id, result })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let index = self.clients.len();
        self.clients.push(WireClient {
            id: client_id,
            resource: url,
            login_code,
            catalog_calls,
            worker,
            attachment,
        });
        index
    }

    async fn begin(&self, resource: &str) -> BrowserFlow {
        // Omitting grant_types must still provision refresh support for this public MCP client.
        let response = self.http.post(format!("{}/oauth/register", self.base))
            .json(&json!({"client_name":"HTTP fixture", "redirect_uris":[CALLBACK], "token_endpoint_auth_method":"none"}))
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let registration: Value = response.json().await.unwrap();
        assert!(
            registration["grant_types"]
                .as_array()
                .unwrap()
                .contains(&json!("refresh_token"))
        );
        let client_id = registration["client_id"].as_str().unwrap().to_owned();
        let response = self
            .http
            .get(format!("{}/oauth/authorize", self.base))
            .query(&[
                ("response_type", "code"),
                ("client_id", client_id.as_str()),
                ("redirect_uri", CALLBACK),
                ("resource", resource),
                ("scope", "mcp"),
                ("state", "consumer-state"),
                ("code_challenge", CHALLENGE),
                ("code_challenge_method", "S256"),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        assert!(cookie.starts_with(&format!("{COOKIE}=")));
        let html = response.text().await.unwrap();
        if resource == RESOURCE {
            assert!(html.contains("name=provider value=\"github\""));
            assert!(!html.contains("name=login_code"));
        }
        let transaction = html
            .split_once("name=transaction_id value=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .to_owned();
        BrowserFlow {
            client_id,
            transaction,
            cookie,
            resource: resource.into(),
        }
    }

    async fn action(&self, flow: &BrowserFlow, key: &str, value: &str) -> Response {
        self.http
            .post(format!("{}/oauth/authorize", self.base))
            .header(header::COOKIE, &flow.cookie)
            .header(header::ORIGIN, PUBLIC)
            .form(&[("transaction_id", flow.transaction.as_str()), (key, value)])
            .send()
            .await
            .unwrap()
    }

    async fn start_provider(&self, flow: &BrowserFlow) -> Url {
        let response = self.action(flow, "provider", "github").await;
        // New-document navigation avoids inheriting the submitted form's CSP restriction.
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::LOCATION));
        let html = response.text().await.unwrap();
        assert!(html.contains("<meta http-equiv=refresh"));
        assert!(!html.contains(GITHUB_SECRET));
        let href = html
            .split_once("<a href=\"")
            .unwrap()
            .1
            .split('"')
            .next()
            .unwrap()
            .replace("&amp;", "&");
        let target = Url::parse(&href).unwrap();
        assert_eq!(target.origin().ascii_serialization(), self.upstream);
        assert_eq!(target.path(), "/login/oauth/authorize");
        assert_eq!(query(&target, "scope"), "");
        assert_eq!(query(&target, "code_challenge_method"), "S256");
        assert_eq!(
            query(&target, "redirect_uri"),
            format!("{PUBLIC}/oauth/callback/github")
        );
        assert_ne!(query(&target, "code_challenge"), CHALLENGE);
        target
    }

    async fn callback(&self, state: &str, cookie: Option<&str>, code: &str) -> Response {
        let mut request = self
            .http
            .get(format!("{}/oauth/callback/github", self.base))
            .query(&[("state", state), ("code", code)]);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        request.send().await.unwrap()
    }

    async fn token(&self, fields: &[(&str, &str)]) -> Response {
        self.http
            .post(format!("{}/oauth/token", self.base))
            .form(fields)
            .send()
            .await
            .unwrap()
    }

    async fn exchange_code(&self, flow: &BrowserFlow, response: Response) -> Value {
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let callback = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        assert_eq!(
            callback.origin().ascii_serialization(),
            "https://consumer.example.test"
        );
        assert_eq!(query(&callback, "state"), "consumer-state");
        assert_eq!(query(&callback, "iss"), PUBLIC);
        let code = query(&callback, "code");
        let response = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("client_id", &flow.client_id),
                ("redirect_uri", CALLBACK),
                ("resource", &flow.resource),
                ("code", &code),
                ("code_verifier", VERIFIER),
            ])
            .await;
        assert_eq!(response.status(), StatusCode::OK);
        let tokens: Value = response.json().await.unwrap();
        assert!(tokens["access_token"].as_str().unwrap().starts_with("at_"));
        assert!(tokens["refresh_token"].as_str().unwrap().starts_with("rt_"));
        tokens
    }

    async fn list(
        &self,
        resource_path: &str,
        token: &str,
        arguments: Value,
    ) -> (StatusCode, HeaderMap, Value) {
        let response = self.http.post(format!("{}{resource_path}", self.base))
            .bearer_auth(token).header("MCP-Protocol-Version", "2025-11-25")
            .header(header::ACCEPT, "application/json")
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{"name":"list", "arguments":arguments}}))
            .send().await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.bytes().await.unwrap();
        let value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        (status, headers, value)
    }
}

fn query(url: &Url, key: &str) -> String {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .unwrap()
        .1
        .into_owned()
}

fn visible_clients(value: &Value) -> Vec<String> {
    assert_ne!(value["result"]["isError"], true, "{value}");
    value["result"]["structuredContent"]["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|client| client["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn github_http_requires_bound_callback_and_explicit_consent_then_filters_acl_and_rotates_refresh()
 {
    let mut fixture = Fixture::start().await;
    let alice = fixture.attach("alice-project", &["alice@github"]).await;
    let bob = fixture.attach("bob-project", &["bob@github"]).await;
    let legacy = fixture.attach("legacy-project", &[]).await;
    assert_eq!(fixture.github.lookups.load(Ordering::SeqCst), 2);
    let flow = fixture.begin(RESOURCE).await;
    assert_eq!(
        fixture.action(&flow, "decision", "allow").await.status(),
        StatusCode::BAD_REQUEST
    );
    let target = fixture.start_provider(&flow).await;
    let state = query(&target, "state");

    // Neither a missing/wrong browser cookie nor a guessed state may spend the provider code.
    for cookie in [None, Some("__Host-ssmcp-oauth=wrong-browser")] {
        assert_eq!(
            fixture
                .callback(&state, cookie, "alice-code")
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fixture
            .callback("unknown-state", Some(&flow.cookie), "alice-code")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 0);
    let response = fixture
        .callback(&state, Some(&flow.cookie), "alice-code")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response.headers().contains_key(header::LOCATION));
    let consent = response.text().await.unwrap();
    assert!(consent.contains("Alice@github"));
    assert!(consent.contains("name=decision value=allow"));
    assert!(consent.contains("name=decision value=deny"));
    assert!(!consent.contains("github-alice-token"));
    assert!(!consent.contains(GITHUB_SECRET));
    assert!(fixture.broker.oauth.state.lock().unwrap().codes.is_empty());
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.github.profiles.load(Ordering::SeqCst), 1);
    let verifier = fixture.github.verifiers.lock().unwrap()[0].clone();
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        query(&target, "code_challenge")
    );
    assert_ne!(verifier, VERIFIER);
    assert_eq!(
        fixture
            .callback(&state, Some(&flow.cookie), "alice-code")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 1);
    // A stale consent page cannot have its already-verified identity replaced by another login.
    assert_eq!(
        fixture.action(&flow, "provider", "github").await.status(),
        StatusCode::BAD_REQUEST
    );

    let tokens = fixture
        .exchange_code(&flow, fixture.action(&flow, "decision", "allow").await)
        .await;
    let access = tokens["access_token"].as_str().unwrap();
    let refresh = tokens["refresh_token"].as_str().unwrap();
    let (status, _, listed) = fixture.list("/mcp", access, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        visible_clients(&listed),
        vec![fixture.clients[alice].id.clone()]
    );
    let (status, _, catalog) = fixture
        .list(
            "/mcp",
            access,
            json!({"client_id":fixture.clients[alice].id}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        catalog["result"]["structuredContent"]["tools"][0]["name"],
        "fixture.echo"
    );
    assert_eq!(
        fixture.clients[alice].catalog_calls.load(Ordering::SeqCst),
        1
    );
    for denied in [bob, legacy] {
        let (status, _, result) = fixture
            .list(
                "/mcp",
                access,
                json!({"client_id":fixture.clients[denied].id}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(
            fixture.clients[denied].catalog_calls.load(Ordering::SeqCst),
            0
        );
    }
    let owner_path = Url::parse(&fixture.clients[legacy].resource)
        .unwrap()
        .path()
        .to_owned();
    assert_eq!(
        fixture.list(&owner_path, access, json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, headers, _) = fixture.list("/mcp", "github-alice-token", json!({})).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(
        headers[header::WWW_AUTHENTICATE]
            .to_str()
            .unwrap()
            .contains("error=\"invalid_token\"")
    );

    let wrong_resource = fixture
        .token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &flow.client_id),
            ("refresh_token", refresh),
            ("resource", &fixture.clients[legacy].resource),
        ])
        .await;
    assert_eq!(wrong_resource.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        wrong_resource.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );
    // Identity grants remain renewable while the permitted SSH connection is offline.
    fixture.clients[alice].worker.abort();
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        &mut fixture.clients[alice].attachment,
    )
    .await
    .unwrap();
    let response = fixture
        .token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &flow.client_id),
            ("refresh_token", refresh),
            ("resource", RESOURCE),
        ])
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let renewed: Value = response.json().await.unwrap();
    assert_ne!(renewed["access_token"], tokens["access_token"]);
    assert_ne!(renewed["refresh_token"], tokens["refresh_token"]);
    let renewed_access = renewed["access_token"].as_str().unwrap();
    assert!(visible_clients(&fixture.list("/mcp", renewed_access, json!({})).await.2).is_empty());
    let reconnected = fixture.attach("alice-reconnected", &["alice@github"]).await;
    assert_eq!(
        visible_clients(&fixture.list("/mcp", renewed_access, json!({})).await.2),
        vec![fixture.clients[reconnected].id.clone()]
    );
    assert_ne!(fixture.clients[alice].id, fixture.clients[reconnected].id);
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 1);
    // Refresh reuse detection still revokes the whole family, including the latest AT.
    let replay = fixture
        .token(&[
            ("grant_type", "refresh_token"),
            ("client_id", &flow.client_id),
            ("refresh_token", refresh),
            ("resource", RESOURCE),
        ])
        .await;
    assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        replay.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );
    assert_eq!(
        fixture.list("/mcp", renewed_access, json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn github_http_denial_consumes_consent_without_issuing_an_mcp_grant() {
    let fixture = Fixture::start().await;
    let flow = fixture.begin(RESOURCE).await;
    let target = fixture.start_provider(&flow).await;
    let state = query(&target, "state");
    let response = fixture
        .callback(&state, Some(&flow.cookie), "bob-code")
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.text().await.unwrap().contains("bob@github"));
    let response = fixture.action(&flow, "decision", "deny").await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let callback = Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
    assert_eq!(query(&callback, "error"), "access_denied");
    assert_eq!(query(&callback, "state"), "consumer-state");
    assert!(!callback.query_pairs().any(|(name, _)| name == "code"));
    assert_eq!(
        fixture.action(&flow, "decision", "allow").await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        fixture
            .callback(&state, Some(&flow.cookie), "bob-code")
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 1);
    let store = fixture.broker.oauth.state.lock().unwrap();
    assert!(store.codes.is_empty() && store.access.is_empty() && store.refresh.is_empty());
}

#[tokio::test]
async fn legacy_http_login_remains_ssh_scoped_and_cannot_reach_identity_acl_sessions() {
    let mut fixture = Fixture::start().await;
    let alice = fixture.attach("alice-project", &["alice@github"]).await;
    let bob = fixture.attach("bob-project", &["bob@github"]).await;
    let legacy = fixture.attach("legacy-project", &[]).await;
    let client = &fixture.clients[legacy];
    let flow = fixture.begin(&client.resource).await;
    let response = fixture
        .action(&flow, "login_code", &client.login_code)
        .await;
    let tokens = fixture.exchange_code(&flow, response).await;
    let access = tokens["access_token"].as_str().unwrap();
    let path = Url::parse(&client.resource).unwrap().path().to_owned();
    let (status, _, result) = fixture.list(&path, access, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(visible_clients(&result), vec![client.id.clone()]);
    for denied in [alice, bob] {
        let (status, _, result) = fixture
            .list(
                &path,
                access,
                json!({"client_id":fixture.clients[denied].id}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(
            fixture.clients[denied].catalog_calls.load(Ordering::SeqCst),
            0
        );
    }
    assert_eq!(
        fixture.list("/mcp", access, json!({})).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(fixture.github.exchanges.load(Ordering::SeqCst), 0);
}
