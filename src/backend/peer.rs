//! Explicit JSON-response MCP over HTTP/Unix, without spawning another ssmcp.
//! Reuses this application's HTTP trust configuration and catalog descriptors.
use crate::catalog::ToolDescriptor;
use reqwest::{
    Client, Url,
    header::{self, HeaderValue},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, time::timeout};

const VERSION: &str = "2025-11-25";
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_CATALOG: usize = 8 * 1024 * 1024;
const MAX_PAGES: usize = 64;
const MAX_TOOLS: usize = 4096;
const CALL_TIMEOUT: Duration = Duration::from_secs(900);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Settings {
    pub endpoint: Url,
    pub token_env: String,
    pub socket: Option<PathBuf>,
}

impl Settings {
    pub fn parse(arguments: &[OsString]) -> Result<Self, String> {
        let mut endpoint = None;
        let mut token_env = None;
        let mut socket = None;
        let mut arguments = arguments.iter();
        while let Some(argument) = arguments.next() {
            let argument = argument.to_str().ok_or("peer arguments must be UTF-8")?;
            let (flag, inline) = argument
                .split_once('=')
                .map_or((argument, None), |(flag, value)| (flag, Some(value)));
            if !matches!(flag, "--endpoint" | "--token-env" | "--socket") {
                return Err("ssmcp peer accepts only --endpoint URL --token-env NAME [--socket PATH] after --; no child command or plaintext token is accepted".into());
            }
            let value = inline
                .or_else(|| arguments.next().and_then(|value| value.to_str()))
                .filter(|value| !value.is_empty() && !value.starts_with('-'))
                .ok_or("peer option requires a nonempty value")?;
            let slot = match flag {
                "--endpoint" => &mut endpoint,
                "--token-env" => &mut token_env,
                _ => &mut socket,
            };
            if slot.replace(value.to_owned()).is_some() {
                return Err("peer options may be supplied only once".into());
            }
        }
        let endpoint =
            endpoint.ok_or("--agent ssmcp requires -- --endpoint URL --token-env NAME")?;
        let endpoint = endpoint_url(&endpoint)?;
        let token_env = token_env.ok_or("--agent ssmcp requires --token-env NAME after --; supply its OAuth bearer in that environment variable")?;
        if token_env.len() > 128
            || !token_env.bytes().enumerate().all(|(i, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (i > 0 && byte.is_ascii_digit())
            })
        {
            return Err(
                "--token-env requires an environment variable name, not a token value".into(),
            );
        }
        let socket = socket.map(PathBuf::from);
        if let Some(path) = &socket {
            if !path.is_absolute() || path.as_os_str().len() > 4096 {
                return Err("peer --socket requires an absolute path".into());
            }
            if endpoint.scheme() != "http" {
                return Err("peer --socket requires the local listener's displayed literal-loopback HTTP URL; HTTPS is not downgraded to plaintext".into());
            }
            #[cfg(not(unix))]
            return Err("peer Unix socket transport is unavailable on this platform".into());
        }
        Ok(Self {
            endpoint,
            token_env,
            socket,
        })
    }

    /// A gateway URL does not identify a device. Reject only the exact endpoint
    /// this connection itself advertises, never every user on the same host.
    pub fn reject_self(&self, own_endpoint: &str) -> Result<(), String> {
        if Url::parse(own_endpoint).is_ok_and(|own| own == self.endpoint) {
            return Err("ssmcp peer is this connection's own MCP endpoint; select a different explicit peer endpoint".into());
        }
        Ok(())
    }

    fn validate_socket(&self) -> Result<(), String> {
        let Some(path) = &self.socket else {
            return Ok(());
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let parent = path.parent().ok_or("peer socket has no parent directory")?;
            let directory = std::fs::symlink_metadata(parent)
                .map_err(|_| "peer socket directory is unavailable")?;
            let metadata =
                std::fs::symlink_metadata(path).map_err(|_| "peer socket is unavailable")?;
            // SAFETY: geteuid has no pointer arguments or process side effects.
            let uid = unsafe { libc::geteuid() };
            if !directory.is_dir()
                || directory.uid() != uid
                || directory.mode() & 0o077 != 0
                || parent.canonicalize().ok().as_deref() != Some(parent)
                || !metadata.file_type().is_socket()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err("peer socket and its directory must be private, owned by this user, and not symlinks".into());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err("peer Unix socket transport is unavailable on this platform".into())
        }
    }
}

fn endpoint_url(raw: &str) -> Result<Url, String> {
    if raw.len() > 8192 || raw.chars().any(char::is_control) {
        return Err("peer endpoint must be a bounded MCP URL without control characters".into());
    }
    let url = Url::parse(raw).map_err(|_| "peer endpoint must be an absolute MCP URL")?;
    let loopback = url
        .host_str()
        .and_then(|host| {
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    let user = url.path().strip_prefix('/').unwrap_or("");
    if !matches!(url.scheme(), "https" | "http")
        || (url.scheme() == "http" && !loopback)
        || url.host_str().is_none()
        || url.port_or_known_default().is_none_or(|port| port == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !crate::broker::valid_username(user)
    {
        return Err("peer endpoint must be the exact HTTPS or literal-loopback HTTP MCP URL ending in /USER, without credentials, query or fragment".into());
    }
    Ok(url)
}

pub(crate) struct Peer {
    settings: Settings,
    client: Client,
    authorization: HeaderValue,
    session: Mutex<Option<HeaderValue>>,
    version: Mutex<String>,
    routes: Mutex<BTreeMap<String, String>>,
    next_id: AtomicU64,
    closed: watch::Sender<bool>,
}

impl Peer {
    pub async fn start(settings: Settings) -> Result<Arc<Self>, String> {
        let token = std::env::var(&settings.token_env)
            .map_err(|_| "peer OAuth bearer environment variable is missing or not UTF-8")?;
        Self::connect(settings, &token).await
    }

    async fn connect(settings: Settings, token: &str) -> Result<Arc<Self>, String> {
        settings.validate_socket()?;
        if token.is_empty()
            || token.len() > 8192
            || !token.bytes().all(|byte| byte.is_ascii_graphic())
        {
            return Err("peer OAuth bearer must be nonempty, bounded and contain no whitespace/control characters".into());
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| "peer OAuth bearer is not a valid header")?;
        authorization.set_sensitive(true);
        let mut builder = crate::http_client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5));
        #[cfg(unix)]
        if let Some(path) = &settings.socket {
            builder = builder.unix_socket(path.as_path());
        }
        let client = builder
            .build()
            .map_err(|_| "could not construct peer HTTP client")?;
        let (closed, _) = watch::channel(false);
        let peer = Arc::new(Self {
            settings,
            client,
            authorization,
            session: Mutex::new(None),
            version: Mutex::new(VERSION.into()),
            routes: Mutex::new(BTreeMap::new()),
            next_id: AtomicU64::new(1),
            closed,
        });
        let initialized = peer
            .request(
                "initialize",
                json!({
                    "protocolVersion":VERSION,"capabilities":{},
                    "clientInfo":{"name":"ssmcp-peer","version":env!("CARGO_PKG_VERSION")}
                }),
                Duration::from_secs(10),
            )
            .await?;
        if !matches!(
            initialized["protocolVersion"].as_str(),
            Some("2025-11-25" | "2025-06-18" | "2025-03-26")
        ) || !initialized["capabilities"]["tools"].is_object()
        {
            return Err(
                "peer did not initialize a compatible JSON-response MCP tools server".into(),
            );
        }
        *peer.version.lock().unwrap() = initialized["protocolVersion"].as_str().unwrap().to_owned();
        peer.exchange(
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
            None,
            Duration::from_secs(10),
        )
        .await?;
        Ok(peer)
    }

    pub fn details(&self) -> Value {
        json!({"scope":"explicit MCP gateway, not a device/session binding",
            "endpoint":self.settings.endpoint.as_str(),"transport":if self.settings.socket.is_some(){"unix_http"}else{"http"},
            "automatic_model_turn":false,"owns_peer_process":false})
    }

    fn error_text(&self, text: &str) -> String {
        let secret = self
            .authorization
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "));
        let text = secret.map_or_else(
            || text.to_owned(),
            |secret| text.replace(secret, "[redacted]"),
        );
        text.chars().filter(|c| !c.is_control()).take(512).collect()
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.exchange(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            Some(id),
            deadline,
        )
        .await
    }

    async fn exchange(
        &self,
        body: Value,
        id: Option<u64>,
        deadline: Duration,
    ) -> Result<Value, String> {
        let mut closed = self.closed.subscribe();
        if *closed.borrow() {
            return Err("MCP peer connection is closed".into());
        }
        tokio::select! {
            biased;
            _ = closed.changed() => Err("MCP peer connection is closed".into()),
            result = timeout(deadline, self.exchange_inner(body, id)) => {
                result.map_err(|_| "MCP peer response timed out".to_owned())?
            }
        }
    }

    async fn exchange_inner(&self, body: Value, id: Option<u64>) -> Result<Value, String> {
        self.settings.validate_socket()?;
        let body = serde_json::to_vec(&body).map_err(|_| "could not encode peer request")?;
        if body.len() > MAX_REQUEST {
            return Err("MCP peer request exceeds 1 MiB".into());
        }
        let mut request = self
            .client
            .post(self.settings.endpoint.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(
                header::ORIGIN,
                self.settings.endpoint.origin().ascii_serialization(),
            )
            .header("MCP-Protocol-Version", self.version.lock().unwrap().clone())
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        if let Some(session) = self.session.lock().unwrap().clone() {
            request = request.header("MCP-Session-Id", session);
        }
        let mut response = request
            .send()
            .await
            .map_err(|_| "MCP peer HTTP transport failed")?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "MCP peer rejected the request (HTTP {}); check the explicit endpoint and its OAuth bearer",
                status.as_u16()
            ));
        }
        if let Some(session) = response.headers().get("MCP-Session-Id") {
            if session.is_empty()
                || session.len() > 256
                || !session.as_bytes().iter().all(u8::is_ascii_graphic)
            {
                return Err("MCP peer returned an invalid session header".into());
            }
            let mut stored = self.session.lock().unwrap();
            if stored.as_ref().is_some_and(|old| old != session) {
                return Err("MCP peer changed its session identity".into());
            }
            let mut session = session.clone();
            session.set_sensitive(true);
            *stored = Some(session);
        }
        if id.is_none() && matches!(status.as_u16(), 202 | 204) {
            return Ok(Value::Null);
        }
        let json_response = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            });
        if !json_response {
            return Err("MCP peer must return bounded application/json responses".into());
        }
        if response
            .content_length()
            .is_some_and(|bytes| bytes > MAX_RESPONSE as u64)
        {
            return Err("MCP peer response exceeds 2 MiB".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "MCP peer response body failed")?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err("MCP peer response exceeds 2 MiB".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "MCP peer returned invalid JSON")?;
        if value["jsonrpc"] != "2.0"
            || value.get("id").and_then(Value::as_u64) != id
            || id.is_none()
            || value.get("result").is_some() == value.get("error").is_some()
        {
            return Err("MCP peer response does not match its JSON-RPC request".into());
        }
        if let Some(error) = value.get("error") {
            let code = error["code"]
                .as_i64()
                .ok_or("MCP peer returned an invalid JSON-RPC error")?;
            let message = error["message"]
                .as_str()
                .ok_or("MCP peer returned an invalid JSON-RPC error")?;
            return Err(format!(
                "MCP peer error {code}: {}",
                self.error_text(message)
            ));
        }
        Ok(value["result"].clone())
    }

    pub async fn compact_tools(&self) -> Result<Vec<ToolDescriptor>, String> {
        timeout(Duration::from_secs(75), self.enumerate())
            .await
            .map_err(|_| "MCP peer catalog enumeration timed out".to_owned())?
    }

    async fn enumerate(&self) -> Result<Vec<ToolDescriptor>, String> {
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        let mut routes = BTreeMap::new();
        let mut tools = Vec::new();
        let mut bytes = 0usize;
        for _ in 0..MAX_PAGES {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
            let mut page = self
                .request("tools/list", params, Duration::from_secs(15))
                .await?;
            let entries = page["tools"]
                .as_array_mut()
                .ok_or("MCP peer tools/list has no tools array")?;
            for mut entry in std::mem::take(entries) {
                let native = entry["name"]
                    .as_str()
                    .filter(|name| !name.is_empty() && name.len() <= 1024)
                    .ok_or("MCP peer returned a missing or oversized tool name")?
                    .to_owned();
                if !entry["inputSchema"].is_object() {
                    return Err("MCP peer returned a tool without an input schema".into());
                }
                let route = format!("peer.{}", crate::registry::component(&native));
                if routes.insert(route.clone(), native).is_some() {
                    return Err("MCP peer repeated a tool name or route".into());
                }
                entry["name"] = json!(route);
                let tool = ToolDescriptor::from_value(entry)?;
                bytes += tool.len();
                if routes.len() > MAX_TOOLS || bytes > MAX_CATALOG {
                    return Err("MCP peer catalog exceeds its 4096-tool or 8 MiB limit".into());
                }
                tools.push(tool);
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => {
                    if *self.closed.borrow() {
                        return Err("MCP peer connection is closed".into());
                    }
                    *self.routes.lock().unwrap() = routes;
                    return Ok(tools);
                }
                Some(Value::String(next)) if !next.is_empty() && next.len() <= 1024 => {
                    if !cursors.insert(next.clone()) {
                        return Err("MCP peer repeated a tools/list cursor".into());
                    }
                    cursor = Some(next.clone());
                }
                _ => return Err("MCP peer returned an invalid tools/list cursor".into()),
            }
        }
        Err("MCP peer exceeded its tools/list page limit".into())
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let native = self
            .routes
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or("MCP peer tool is not in the negotiated catalog")?;
        let result = self
            .request(
                "tools/call",
                json!({"name":native,"arguments":arguments}),
                CALL_TIMEOUT,
            )
            .await?;
        if !result.is_object()
            || !result["content"].is_array()
            || result
                .get("isError")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err("MCP peer returned an invalid tool result".into());
        }
        Ok(result)
    }

    pub async fn shutdown(&self) {
        self.closed.send_replace(true);
        self.routes.lock().unwrap().clear();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.closed.send_replace(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Body,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
    };
    use tokio::{net::TcpListener, sync::Notify, task::JoinHandle};

    const TOKEN: &str = "private-fixture-oauth-bearer";
    struct Fixture {
        origin: String,
        mode: &'static str,
        requests: Mutex<Vec<Value>>,
        entered: Notify,
    }
    struct Server {
        state: Arc<Fixture>,
        task: JoinHandle<()>,
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Server {
        async fn start(mode: &'static str) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let state = Arc::new(Fixture {
                origin: format!("http://{}", listener.local_addr().unwrap()),
                mode,
                requests: Mutex::new(Vec::new()),
                entered: Notify::new(),
            });
            let router = Router::new()
                .route("/alice", post(handle))
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Self { state, task }
        }
        fn settings(&self) -> Settings {
            Settings::parse(
                &[
                    "--endpoint",
                    &format!("{}/alice", self.state.origin),
                    "--token-env",
                    "FIXTURE_PEER_TOKEN",
                ]
                .map(OsString::from),
            )
            .unwrap()
        }
        async fn peer(&self) -> Arc<Peer> {
            Peer::connect(self.settings(), TOKEN).await.unwrap()
        }
    }

    async fn handle(
        State(state): State<Arc<Fixture>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        if headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {TOKEN}"))
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(&state.origin)
            || headers.get(header::HOST).and_then(|v| v.to_str().ok())
                != state.origin.strip_prefix("http://")
        {
            return StatusCode::FORBIDDEN.into_response();
        }
        state.requests.lock().unwrap().push(body.clone());
        if state.mode == "redirect" {
            return (
                StatusCode::TEMPORARY_REDIRECT,
                [(
                    header::LOCATION,
                    format!("{}/must-not-follow", state.origin),
                )],
            )
                .into_response();
        }
        let method = body["method"].as_str().unwrap();
        let id = body["id"].clone();
        let result = match method {
            "initialize" => json!({"protocolVersion":VERSION,"capabilities":{"tools":{}},
                "serverInfo":{"name":"ssmcp-peer-fixture","version":"1"}}),
            "notifications/initialized" => return StatusCode::ACCEPTED.into_response(),
            "tools/list" => {
                if body["params"].get("cursor").is_none() {
                    json!({"tools":[{"name":"Echo","description":"actual fixture echo", "inputSchema":{"type":"object"},
                        "outputSchema":{"type":"object"},"x-peer-extension":{"kept":true}}],"nextCursor":"second"})
                } else {
                    assert_eq!(body["params"]["cursor"], "second");
                    if state.mode == "repeated_cursor" {
                        json!({"tools":[],"nextCursor":"second"})
                    } else {
                        json!({"tools":[{"name":"Fail","inputSchema":{"type":"object"}},
                        {"name":"RpcError","inputSchema":{"type":"object"}}]})
                    }
                }
            }
            "tools/call" => match body["params"]["name"].as_str().unwrap() {
                "Echo" => json!({"content":[{"type":"text","text":"actual peer reply"}],
                        "structuredContent":{"received":body["params"]["arguments"]},"isError":false}),
                "Fail" => {
                    json!({"content":[{"type":"text","text":"actual peer tool failure"}],"isError":true})
                }
                "RpcError" => {
                    return Json(json!({"jsonrpc":"2.0","id":id,
                        "error":{"code":-32017,"message":format!("fixture denied {TOKEN}")}}))
                    .into_response();
                }
                _ => unreachable!(),
            },
            "oversized" => {
                return (
                    [(header::CONTENT_TYPE, "application/json")],
                    vec![b'x'; MAX_RESPONSE + 1],
                )
                    .into_response();
            }
            "chunked" => {
                let chunks = futures_util::stream::iter([
                    Ok::<_, std::io::Error>(vec![b'x'; MAX_RESPONSE / 2 + 1]),
                    Ok(vec![b'y'; MAX_RESPONSE / 2 + 1]),
                ]);
                return Response::builder()
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from_stream(chunks))
                    .unwrap();
            }
            "slow" => {
                state.entered.notify_one();
                tokio::time::sleep(Duration::from_millis(200)).await;
                json!({})
            }
            "mismatched" => {
                return Json(json!({"jsonrpc":"2.0","id":999999,"result":{}})).into_response();
            }
            _ => unreachable!(),
        };
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
    }

    #[test]
    fn peer_settings_require_explicit_auth_target_and_distinguish_gateway_from_session() {
        let parse =
            |args: &[&str]| Settings::parse(&args.iter().map(OsString::from).collect::<Vec<_>>());
        for args in [
            vec![],
            vec!["--endpoint", "https://relay.example/alice"],
            vec![
                "--endpoint",
                "http://relay.example/alice",
                "--token-env",
                "TOKEN",
            ],
            vec![
                "--endpoint",
                "https://u:p@relay.example/alice",
                "--token-env",
                "TOKEN",
            ],
            vec![
                "--endpoint",
                "https://relay.example/alice?token=secret",
                "--token-env",
                "TOKEN",
            ],
            vec![
                "--endpoint",
                "https://relay.example/",
                "--token-env",
                "TOKEN",
            ],
            vec![
                "--endpoint",
                "https://relay.example/alice",
                "--token",
                "secret",
            ],
            vec![
                "--endpoint",
                "https://relay.example/alice",
                "--token-env",
                "TOKEN",
                "--socket",
                "/tmp/socket",
            ],
            vec![
                "--endpoint",
                "http://127.0.0.1:9000/alice",
                "--token-env",
                "TOKEN",
                "--socket",
                "relative",
            ],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        let settings = parse(&[
            "--endpoint",
            "https://relay.example/alice",
            "--token-env",
            "PEER_TOKEN",
        ])
        .unwrap();
        assert!(settings.reject_self("https://relay.example/alice").is_err());
        assert!(settings.reject_self("https://relay.example/bob").is_ok());
        assert!(settings.reject_self("https://other.example/alice").is_ok());
        assert_eq!(settings.token_env, "PEER_TOKEN");
    }

    #[tokio::test]
    async fn peer_real_http_handshake_pages_and_calls_preserve_native_results_and_errors() {
        let server = Server::start("healthy").await;
        let peer = server.peer().await;
        let catalog = peer.compact_tools().await.unwrap();
        assert_eq!(catalog.len(), 3);
        let echo = catalog[0].value().unwrap();
        assert_eq!(echo["name"], "peer.Echo");
        assert_eq!(echo["x-peer-extension"]["kept"], true);
        assert_eq!(echo["outputSchema"]["type"], "object");
        let arguments =
            json!({"text":"中文\nsecond line", "fake_result":"never substitutes native output"});
        let result = peer.call("peer.Echo", arguments.clone()).await.unwrap();
        assert_eq!(result["structuredContent"]["received"], arguments);
        assert_eq!(result["content"][0]["text"], "actual peer reply");
        assert_eq!(
            peer.call("peer.Fail", json!({})).await.unwrap()["isError"],
            true
        );
        let error = peer.call("peer.RpcError", json!({})).await.unwrap_err();
        assert!(error.contains("-32017") && error.contains("[redacted]") && !error.contains(TOKEN));
        let before = server.state.requests.lock().unwrap().len();
        assert!(peer.call("peer.Unknown", json!({})).await.is_err());
        assert_eq!(server.state.requests.lock().unwrap().len(), before);
        let requests = server.state.requests.lock().unwrap().clone();
        assert_eq!(requests[0]["method"], "initialize");
        assert_eq!(requests[1]["method"], "notifications/initialized");
        assert_eq!(requests[3]["params"]["cursor"], "second");
        assert!(
            !requests
                .iter()
                .any(|row| row["method"] == "turn/start" || row["method"] == "prompt")
        );
        peer.shutdown().await;
        assert!(peer.call("peer.Echo", json!({})).await.is_err());
        // Closing the connection never stops the peer gateway.
        assert_eq!(server.peer().await.compact_tools().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn peer_auth_paging_redirect_and_jsonrpc_failures_do_not_publish_partial_tools() {
        let server = Server::start("repeated_cursor").await;
        assert!(
            Peer::connect(server.settings(), "wrong-token")
                .await
                .is_err()
        );
        assert!(server.state.requests.lock().unwrap().is_empty());
        let peer = server.peer().await;
        assert!(peer.compact_tools().await.err().unwrap().contains("cursor"));
        assert!(peer.call("peer.Echo", json!({})).await.is_err());
        assert!(
            peer.request("mismatched", json!({}), Duration::from_secs(1))
                .await
                .unwrap_err()
                .contains("JSON-RPC")
        );
        let redirect = Server::start("redirect").await;
        let error = match Peer::connect(redirect.settings(), TOKEN).await {
            Ok(_) => panic!("followed a redirect"),
            Err(error) => error,
        };
        assert!(error.contains("307") && !error.contains(TOKEN));
        assert_eq!(redirect.state.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn peer_limits_response_bytes_time_and_cancels_only_owned_requests() {
        let server = Server::start("healthy").await;
        let peer = server.peer().await;
        for method in ["oversized", "chunked"] {
            assert!(
                peer.request(method, json!({}), Duration::from_secs(2))
                    .await
                    .unwrap_err()
                    .contains("2 MiB")
            );
        }
        assert!(
            peer.request("slow", json!({}), Duration::from_millis(20))
                .await
                .unwrap_err()
                .contains("timed out")
        );
        let before = server.state.requests.lock().unwrap().len();
        assert!(
            peer.request(
                "oversized",
                json!({"text":"x".repeat(MAX_REQUEST)}),
                Duration::from_secs(1)
            )
            .await
            .unwrap_err()
            .contains("1 MiB")
        );
        assert_eq!(server.state.requests.lock().unwrap().len(), before);
        // Consume the earlier slow request's notification before waiting on this one.
        server.state.entered.notified().await;
        let pending = tokio::spawn({
            let peer = peer.clone();
            async move { peer.request("slow", json!({}), CALL_TIMEOUT).await }
        });
        server.state.entered.notified().await;
        peer.shutdown().await;
        assert!(
            timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .contains("closed")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn peer_unix_http_keeps_url_host_origin_and_oauth_without_tcp_fallback() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut random = [0u8; 8];
        crate::platform::random_bytes(&mut random).unwrap();
        let directory =
            std::env::temp_dir().join(format!("ssmcp-peer-test-{}", crate::hex::encode(random)));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .unwrap();
        let socket = directory.join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let state = Arc::new(Fixture {
            origin: "http://127.0.0.1:9".into(),
            mode: "healthy",
            requests: Mutex::new(Vec::new()),
            entered: Notify::new(),
        });
        let router = Router::new()
            .route("/alice", post(handle))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let server = Server { state, task };
        let mut settings = server.settings();
        settings.socket = Some(socket.clone());
        let peer = Peer::connect(settings.clone(), TOKEN).await.unwrap();
        assert_eq!(peer.compact_tools().await.unwrap().len(), 3);
        assert_eq!(
            peer.call("peer.Echo", json!({"via":"unix"})).await.unwrap()["structuredContent"]["received"]
                ["via"],
            "unix"
        );
        peer.shutdown().await;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Peer::connect(settings, TOKEN).await.is_err());
        drop(server);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
