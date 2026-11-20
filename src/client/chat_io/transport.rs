use super::{Input, Outcome, Sender};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Semaphore, watch},
};

#[cfg(target_os = "linux")]
mod socket;
#[cfg(unix)]
mod stdio;

const FRAME_LIMIT: usize = 6 * crate::wire::MAX_USER_MESSAGE_BYTES + 4096;
const READ_LIMIT: Duration = Duration::from_secs(10);
const MCP_VERSION: &str = "2025-11-25";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Message {
    text: String,
}

pub(super) enum Endpoint {
    #[cfg(unix)]
    Stdio(bool),
    Http(TcpListener, HttpState),
    #[cfg(target_os = "linux")]
    Socket(crate::local_socket::Bound),
}

impl Endpoint {
    pub(super) async fn open(input: &Input) -> Result<Option<Self>, String> {
        Ok(Some(match input {
            Input::Terminal | Input::None => return Ok(None),
            #[cfg(unix)]
            Input::Pipe | Input::McpStdio => {
                use std::io::IsTerminal;
                if std::io::stdin().is_terminal() {
                    return Err("pipe and MCP stdio input require redirected stdin".into());
                }
                Self::Stdio(matches!(input, Input::McpStdio))
            }
            #[cfg(not(unix))]
            Input::Pipe | Input::McpStdio => {
                return Err("pipe and MCP stdio chat input require Unix".into());
            }
            Input::Web(config) | Input::McpHttp(config) => {
                let state = HttpState::new(
                    config.address,
                    config.token_file.clone(),
                    matches!(input, Input::McpHttp(_)),
                )?;
                let listener = TcpListener::bind(config.address)
                    .await
                    .map_err(|error| format!("cannot bind chat input: {error}"))?;
                Self::Http(listener, state)
            }
            #[cfg(target_os = "linux")]
            Input::Socket(path) => Self::Socket(socket::bind(path.as_deref()).await?),
            #[cfg(not(target_os = "linux"))]
            Input::Socket(_) => return Err("socket chat input requires Linux".into()),
        }))
    }

    pub(super) async fn serve(
        self,
        sender: Sender,
        mut stop: watch::Receiver<bool>,
    ) -> Result<(), String> {
        match self {
            #[cfg(unix)]
            Self::Stdio(mcp) => stdio::serve(sender, mcp).await,
            #[cfg(target_os = "linux")]
            Self::Socket(bound) => socket::serve(bound, sender).await,
            Self::Http(listener, state) => {
                let path = if state.mcp { "/mcp" } else { "/" };
                crate::client::render::status(
                    "chat input",
                    "Listening",
                    &format!("{}{path}", state.origin),
                );
                let app = if state.mcp {
                    Router::new().route("/mcp", get(http_get).post(http_post))
                } else {
                    Router::new()
                        .route("/", get(http_get))
                        .route("/message", post(http_post))
                };
                axum::serve(listener, app.with_state((state, sender)))
                    .with_graceful_shutdown(async move {
                        let _ = stop.changed().await;
                    })
                    .await
                    .map_err(|error| error.to_string())
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct HttpState {
    authority: String,
    origin: String,
    token_file: PathBuf,
    mcp: bool,
    readers: Arc<Semaphore>,
}

impl HttpState {
    fn new(address: SocketAddr, token_file: PathBuf, mcp: bool) -> Result<Self, String> {
        token(&token_file)?;
        Ok(Self {
            authority: address.to_string(),
            origin: format!("http://{address}"),
            token_file,
            mcp,
            readers: Arc::new(Semaphore::new(8)),
        })
    }

    fn check_origin(&self, headers: &HeaderMap) -> bool {
        // HTTP permits omission of its default port. Since both expected values
        // come from our bound IP address, exact matching of these two forms also
        // rejects paths, aliases, userinfo and arbitrary loopback origins.
        let local = |value: &axum::http::HeaderValue, expected: &str| {
            value.as_bytes() == expected.as_bytes()
                || expected
                    .strip_suffix(":80")
                    .is_some_and(|default| value.as_bytes() == default.as_bytes())
        };
        headers.get_all(header::HOST).iter().count() == 1
            && headers
                .get(header::HOST)
                .is_some_and(|host| local(host, &self.authority))
            && headers.get_all(header::ORIGIN).iter().count() <= 1
            && headers
                .get(header::ORIGIN)
                .is_none_or(|origin| local(origin, &self.origin))
    }

    fn authenticate(&self, headers: &HeaderMap) -> Option<Authorization> {
        if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
            return None;
        }
        let provided = headers
            .get(header::AUTHORIZATION)?
            .as_bytes()
            .strip_prefix(b"Bearer ")?;
        let expected = token(&self.token_file).ok()?;
        bool::from(expected.as_slice().ct_eq(provided)).then(|| Authorization {
            token_file: self.token_file.clone(),
            fingerprint: Sha256::digest(expected).into(),
        })
    }
}

pub(super) struct Authorization {
    token_file: PathBuf,
    fingerprint: [u8; 32],
}

impl Authorization {
    pub(super) fn current(&self) -> bool {
        token(&self.token_file).is_ok_and(|token| {
            let fingerprint: [u8; 32] = Sha256::digest(token).into();
            bool::from(fingerprint.as_slice().ct_eq(&self.fingerprint))
        })
    }
}

pub(super) async fn revoked(authorization: Option<&Authorization>) {
    let Some(authorization) = authorization else {
        return std::future::pending().await;
    };
    while authorization.current() {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn token(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let bytes = super::private_file(path)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| "input token must be ASCII")?
        .trim();
    if !(32..=256).contains(&text.len())
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err(
            "input token requires 32–256 ASCII letters, digits, underscores or hyphens".into(),
        );
    }
    Ok(text.as_bytes().to_vec())
}

async fn http_get(State((state, _)): State<(HttpState, Sender)>, headers: HeaderMap) -> Response {
    if !state.check_origin(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if state.mcp {
        return if state.authenticate(&headers).is_some() {
            StatusCode::METHOD_NOT_ALLOWED
        } else {
            StatusCode::UNAUTHORIZED
        }
        .into_response();
    }
    ([
        (header::CACHE_CONTROL, "no-store"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'; script-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"),
    ], Html(PAGE)).into_response()
}

async fn http_post(
    State((state, sender)): State<(HttpState, Sender)>,
    request: Request,
) -> Response {
    if !state.check_origin(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(authorization) = state.authenticate(request.headers()) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if state.mcp
        && request
            .headers()
            .get("mcp-protocol-version")
            .is_some_and(|v| v.as_bytes() != MCP_VERSION.as_bytes())
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Ok(_permit) = state.readers.try_acquire() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    let result = tokio::time::timeout(READ_LIMIT, to_bytes(request.into_body(), FRAME_LIMIT)).await;
    let bytes = match result {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    if state.mcp {
        match serde_json::from_slice(&bytes) {
            Ok(value) => match rpc(&sender, value, true, Some(authorization)).await {
                Some(value) => Json(value).into_response(),
                None => StatusCode::ACCEPTED.into_response(),
            },
            Err(_) => Json(rpc_error(Value::Null, -32700, "invalid JSON")).into_response(),
        }
    } else {
        match serde_json::from_slice::<Message>(&bytes) {
            Ok(message) => match sender.dispatch(&message.text, Some(authorization)).await {
                Ok(receipt) if receipt.status == "failed" => {
                    (StatusCode::UNPROCESSABLE_ENTITY, Json(json!(receipt))).into_response()
                }
                Ok(receipt) => Json(json!(receipt)).into_response(),
                Err(error) => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error":error})),
                )
                    .into_response(),
            },
            Err(_) => StatusCode::BAD_REQUEST.into_response(),
        }
    }
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn envelope(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

async fn rpc(
    sender: &Sender,
    request: Value,
    initialized: bool,
    authorization: Option<Authorization>,
) -> Option<Value> {
    let id = request.get("id").cloned();
    if request["jsonrpc"] != "2.0"
        || !request.is_object()
        || id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
    {
        return Some(rpc_error(Value::Null, -32600, "invalid JSON-RPC request"));
    }
    let method = request["method"].as_str().unwrap_or("");
    let id = id?;
    let response = match method {
        "initialize" if request["params"]["protocolVersion"].is_string() => envelope(
            id,
            json!({
                "protocolVersion":MCP_VERSION, "serverInfo":{"name":"ssmcp-chat-input","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"tools":{}}
            }),
        ),
        "initialize" => rpc_error(id, -32602, "initialize requires a protocol version"),
        "ping" => envelope(id, json!({})),
        "tools/list" if initialized => envelope(
            id,
            json!({"tools":[{
                "name":"chat_submit", "description":"Send one message to the configured SSMCP chat recipient.",
                "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}
            }]}),
        ),
        "tools/call" if initialized => {
            if request["params"]["name"] != "chat_submit" {
                return Some(rpc_error(id, -32602, "unknown input tool"));
            }
            let Ok(message) =
                serde_json::from_value::<Message>(request["params"]["arguments"].clone())
            else {
                return Some(rpc_error(
                    id,
                    -32602,
                    "chat_submit requires only a text string",
                ));
            };
            envelope(
                id,
                tool_result(sender.dispatch(&message.text, authorization).await),
            )
        }
        "tools/list" | "tools/call" => rpc_error(
            id,
            -32002,
            "initialize and send notifications/initialized first",
        ),
        _ => rpc_error(id, -32601, "method not found"),
    };
    Some(response)
}

fn tool_result(outcome: Outcome) -> Value {
    let (value, is_error) = match outcome {
        Ok(receipt) => (json!(receipt), receipt.status == "failed"),
        Err(error) => (json!({"error":error}), true),
    };
    json!({"isError":is_error,"content":[{"type":"text","text":value.to_string()}],"structuredContent":value})
}

const PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>SSMCP chat input</title>
<h1>SSMCP chat input</h1>
<form id="composer">
<label>Input token <input type="password" id="token" autocomplete="off" required></label><br>
<label>Message <textarea id="message" required></textarea></label><br>
<button id="send">Send</button>
</form>
<pre id="result" aria-live="polite"></pre>
<script>
const form = document.getElementById('composer');
const token = document.getElementById('token');
const message = document.getElementById('message');
const send = document.getElementById('send');
const result = document.getElementById('result');
form.addEventListener('submit', async event => {
  event.preventDefault();
  const text = message.value;
  send.disabled = true;
  try {
    const res = await fetch('/message', {
      method: 'POST',
      headers: {'Content-Type': 'application/json', Authorization: 'Bearer ' + token.value},
      body: JSON.stringify({text})
    });
    if (!res.headers.get('content-type')?.startsWith('application/json')) {
      result.textContent = 'Input rejected or unavailable (HTTP ' + res.status + ').';
      return;
    }
    const reply = await res.json();
    result.textContent = JSON.stringify(reply, null, 2);
    if (res.ok && reply.status !== 'unconfirmed' && message.value === text) message.value = '';
  } catch {
    result.textContent = 'No confirmed result. Check the session before retrying.';
  } finally {
    send.disabled = false;
  }
});
</script>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::chat_io::{Receipt, Request as Incoming};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn mcp_passes_literal_framed_text_to_the_live_route_and_reports_a_queue_receipt() {
        let (tx, mut rx) = mpsc::channel::<Incoming>(1);
        let sender = Sender {
            queue: tx,
            save_exchange: false,
        };
        let original = "Inspect @not-created.rs and keep $100\nNext line";
        let call = json!({"jsonrpc":"2.0","id":"fixture","method":"tools/call","params":{
            "name":"chat_submit","arguments":{"text":original}
        }});
        let blocked = rpc(&sender, call.clone(), false, None).await.unwrap();
        assert_eq!(blocked["error"]["code"], -32002);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        let receiver = async {
            let request = rx.recv().await.unwrap();
            assert_eq!(request.text, original);
            let mut receipt = Receipt::new("queued", Some("request-one".into()));
            receipt.warnings.push("@not-created.rs: unavailable".into());
            request.reply.send(Ok(receipt)).unwrap();
        };
        let (reply, ()) = tokio::join!(rpc(&sender, call, true, None), receiver);
        let value = reply.unwrap();
        assert_eq!(value["id"], "fixture");
        assert_eq!(value["result"]["structuredContent"]["status"], "queued");
        assert_eq!(
            value["result"]["structuredContent"]["warnings"][0],
            "@not-created.rs: unavailable"
        );
    }

    #[tokio::test]
    async fn cancelling_an_input_ticket_leaves_no_receiver_for_dispatch() {
        let (tx, mut rx) = mpsc::channel::<Incoming>(1);
        let sender = Sender {
            queue: tx,
            save_exchange: false,
        };
        let ticket = sender.start("no replay", None).unwrap();
        assert!(sender.start("over capacity", None).is_err());
        drop(ticket);
        assert!(rx.recv().await.unwrap().reply.is_closed());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn loopback_origin_check_does_not_authorize_rebound_hosts() {
        let state = HttpState {
            authority: "127.0.0.1:8181".into(),
            origin: "http://127.0.0.1:8181".into(),
            token_file: PathBuf::new(),
            mcp: false,
            readers: Arc::new(Semaphore::new(1)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8181".parse().unwrap());
        assert!(state.check_origin(&headers));
        assert!(state.authenticate(&headers).is_none());
        headers.insert(header::ORIGIN, "https://unrelated.example".parse().unwrap());
        assert!(!state.check_origin(&headers));
        headers.remove(header::ORIGIN);
        headers.insert(header::HOST, "unrelated.example".parse().unwrap());
        assert!(!state.check_origin(&headers));
    }

    #[test]
    fn loopback_origin_accepts_only_the_bound_host_and_http_default_port() {
        let state = HttpState {
            authority: "[::1]:80".into(),
            origin: "http://[::1]:80".into(),
            token_file: PathBuf::new(),
            mcp: false,
            readers: Arc::new(Semaphore::new(1)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "[::1]".parse().unwrap());
        for origin in ["http://[::1]", "http://[::1]:80"] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(state.check_origin(&headers));
        }
        for origin in [
            "https://[::1]",
            "http://[::1]/",
            "http://user@[::1]",
            "http://127.0.0.1",
            "http://[::1]:81",
        ] {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert!(!state.check_origin(&headers));
        }
    }

    #[tokio::test]
    async fn unauthenticated_web_post_cannot_submit_to_the_route() {
        let (tx, mut rx) = mpsc::channel::<Incoming>(1);
        let state = HttpState {
            authority: "127.0.0.1:8181".into(),
            origin: "http://127.0.0.1:8181".into(),
            token_file: PathBuf::new(),
            mcp: false,
            readers: Arc::new(Semaphore::new(1)),
        };
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/message")
            .header(header::HOST, state.authority.as_str())
            .header(header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(r#"{"text":"must not dispatch"}"#))
            .unwrap();
        assert_eq!(
            http_post(
                State((
                    state,
                    Sender {
                        queue: tx.clone(),
                        save_exchange: false
                    }
                )),
                request
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED,
        );
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
