//! JSON-response Streamable HTTP for MCP 2026-07-28 and the 2025 revisions.
//!
//! Protocol: <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http>
use crate::{
    broker::{Authorization, Broker, Session},
    code_mode::{self, DEFAULT_TIMEOUT_MS, Host, HostRequest, MAX_CODE_BYTES, MAX_TIMEOUT_MS},
    resources::{self, ResourceError},
    wire::{MAX_FRAME, Operation},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{any, get},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};

const MODERN: &str = "2026-07-28";
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const VERSIONS: &[&str] = &[MODERN, "2025-11-25", "2025-06-18", "2025-03-26"];
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";

pub fn router(broker: Arc<Broker>) -> Router {
    Router::new()
        .merge(crate::oauth::routes())
        .merge(crate::portal::routes())
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/{user}", any(endpoint))
        .with_state(broker)
        .layer(middleware::map_response(no_store))
        .layer(middleware::from_fn(crate::observability::http))
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn endpoint(
    State(broker): State<Arc<Broker>>,
    Path(user): Path<String>,
    request: Request,
) -> Response {
    // Path extraction percent-decodes input. Only validated Linux usernames may
    // appear inside an OAuth challenge URL or select a resource.
    if !crate::broker::valid_username(&user) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let headers = request.headers();
    if !valid_origin(headers, &broker.public_url) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(secret) = bearer(headers) else {
        return unauthorized(&broker.public_url, &user, has_bearer_authorization(headers));
    };
    if broker.authenticate(&user, secret).await.is_none() {
        return unauthorized(&broker.public_url, &user, true);
    }
    let access_token = secret.to_owned();
    if request.method() != Method::POST {
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response();
    }
    if !single_header(headers, "content-type")
        .ok()
        .flatten()
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if !accepts_json(headers) {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    let headers = headers.clone();
    let bytes = match tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), MAX_FRAME),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Fault::new(StatusCode::BAD_REQUEST, -32700, "Parse error").response(None),
    };
    let request = match RpcRequest::parse(&value) {
        Ok(request) => request,
        Err(fault) => return fault.response(valid_id(value.get("id"))),
    };
    let era = match validate_protocol(&headers, &request) {
        Ok(era) => era,
        Err(fault) => return fault.response(request.id),
    };
    // A notification never invokes a terminal operation, even if named tools/call.
    let Some(id) = request.id else {
        return StatusCode::ACCEPTED.into_response();
    };
    // The body may arrive slowly. Check the grant again before dispatching work.
    let Some(authorization) = broker.authenticate(&user, &access_token).await else {
        return unauthorized(&broker.public_url, &user, true);
    };
    let subject = match &authorization {
        Authorization::Identity(identity) => vec![("github_subject_id", identity.subject.clone())],
        Authorization::Legacy { user, .. } => vec![("user", user.clone())],
    };
    let mut fields = vec![
        ("event", "mcp_call".into()),
        ("rpc_id", id.to_string()),
        ("method", request.method.to_owned()),
    ];
    fields.extend(subject);
    crate::observability::log("info", "auth", &fields);
    let result = match action(&request, era) {
        Ok(Action::Reply(result)) => result,
        Ok(Action::List(arguments)) => {
            tool_result(list(&broker, &user, &authorization, arguments).await)
        }
        Ok(Action::Code(arguments)) => crate::code_mode::tool_result(
            execute(broker, user, authorization, access_token, arguments).await,
        ),
        Ok(Action::ResourceList(cursor)) => {
            let result = resources::list(&broker, &user, &authorization, cursor.as_deref()).await;
            if broker.authenticate(&user, &access_token).await.is_none() {
                return unauthorized(&broker.public_url, &user, true);
            }
            match result {
                Ok(value) => value,
                Err(error) => return resource_fault(error, era).response(Some(id)),
            }
        }
        Ok(Action::ResourceRead(uri)) => {
            let result = resources::read(&broker, &user, &authorization, &uri).await;
            if broker.authenticate(&user, &access_token).await.is_none() {
                return unauthorized(&broker.public_url, &user, true);
            }
            match result {
                Ok(value) => value,
                Err(error) => return resource_fault(error, era).response(Some(id)),
            }
        }
        Err(fault) => return fault.response(Some(id)),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":decorate_result(result, era)})).into_response()
}

fn unauthorized(public_url: &str, user: &str, invalid_token: bool) -> Response {
    let mut challenge = format!(
        "Bearer resource_metadata=\"{public_url}/.well-known/oauth-protected-resource/{user}\", scope=\"mcp\""
    );
    // RFC 6750: an invalid/expired supplied token tells the client to refresh.
    // Initial discovery without Bearer credentials carries no token error.
    if invalid_token {
        challenge.push_str(", error=\"invalid_token\"");
    }
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
    )
        .into_response()
}

fn has_bearer_authorization(headers: &HeaderMap) -> bool {
    headers.get_all(header::AUTHORIZATION).iter().any(|value| {
        value.to_str().ok().is_some_and(|value| {
            value
                .split_ascii_whitespace()
                .next()
                .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Bearer"))
        })
    })
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, Fault> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(Fault::header());
    }
    value.to_str().map(Some).map_err(|_| Fault::header())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = single_header(headers, "authorization").ok()??;
    let (scheme, secret) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Bearer")
        || secret.is_empty()
        || !secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
    {
        return None;
    }
    Some(secret)
}

fn valid_origin(headers: &HeaderMap, public_url: &str) -> bool {
    match single_header(headers, "origin") {
        Ok(None) => true,
        Ok(Some(origin)) => origin == public_url.trim_end_matches('/'),
        Err(_) => false,
    }
}

fn accepts_json(headers: &HeaderMap) -> bool {
    if !headers.contains_key(header::ACCEPT) {
        return true;
    }
    let mut best = None::<(u8, f32)>;
    for header in headers.get_all(header::ACCEPT) {
        let Ok(header) = header.to_str() else {
            return false;
        };
        for item in header.split(',') {
            let mut parts = item.split(';');
            let specificity = match parts
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "application/json" => 2,
                "application/*" => 1,
                "*/*" => 0,
                _ => continue,
            };
            let mut quality = 1.0;
            for parameter in parts {
                if let Some((key, value)) = parameter.trim().split_once('=')
                    && key.eq_ignore_ascii_case("q")
                {
                    quality = value
                        .parse::<f32>()
                        .ok()
                        .filter(|q| (0.0..=1.0).contains(q))
                        .unwrap_or(0.0);
                }
            }
            if best.is_none_or(|(prior, q)| {
                specificity > prior || (specificity == prior && quality > q)
            }) {
                best = Some((specificity, quality));
            }
        }
    }
    best.is_some_and(|(_, quality)| quality > 0.0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Era {
    Legacy,
    Modern,
}

struct RpcRequest<'a> {
    id: Option<&'a Value>,
    method: &'a str,
    params: Option<&'a Map<String, Value>>,
}

fn valid_id(id: Option<&Value>) -> Option<&Value> {
    id.filter(|value| value.is_string() || value.is_i64() || value.is_u64())
}

impl<'a> RpcRequest<'a> {
    fn parse(value: &'a Value) -> Result<Self, Fault> {
        let object = value.as_object().ok_or_else(Fault::invalid_request)?;
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || object.contains_key("result")
            || object.contains_key("error")
        {
            return Err(Fault::invalid_request());
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(Fault::invalid_request)?;
        let id = object.get("id");
        if id.is_some() && valid_id(id).is_none() {
            return Err(Fault::invalid_request());
        }
        let params = match object.get("params") {
            None => None,
            Some(Value::Object(params)) => Some(params),
            _ => return Err(Fault::params("params must be an object")),
        };
        Ok(Self { id, method, params })
    }

    fn param(&self, key: &str) -> Option<&'a Value> {
        self.params.and_then(|params| params.get(key))
    }
}

fn validate_protocol(headers: &HeaderMap, request: &RpcRequest<'_>) -> Result<Era, Fault> {
    let header_version = single_header(headers, "mcp-protocol-version")?;
    if let Some(version) = header_version
        && !VERSIONS.contains(&version)
    {
        return Err(Fault::unsupported_version(version));
    }
    // The current revision defines no metadata-header requirements for notifications.
    if request.id.is_none() {
        return Ok(if header_version == Some(MODERN) {
            Era::Modern
        } else {
            Era::Legacy
        });
    }
    let metadata = match request.param("_meta") {
        None => None,
        Some(Value::Object(metadata)) => Some(metadata),
        _ => return Err(Fault::params("_meta must be an object")),
    };
    let modern = header_version == Some(MODERN)
        || metadata.is_some_and(|meta| meta.contains_key(VERSION_META));
    if !modern {
        return Ok(Era::Legacy);
    }
    let metadata = metadata.ok_or_else(|| Fault::params("required request metadata is missing"))?;
    let body_version = metadata
        .get(VERSION_META)
        .and_then(Value::as_str)
        .ok_or_else(|| Fault::params("protocolVersion metadata must be a string"))?;
    if !metadata
        .get(CAPABILITIES_META)
        .is_some_and(Value::is_object)
    {
        return Err(Fault::params(
            "clientCapabilities metadata must be an object",
        ));
    }
    if metadata
        .get("io.modelcontextprotocol/clientInfo")
        .is_some_and(|info| !valid_implementation(info))
    {
        return Err(Fault::params("clientInfo must include a name and version"));
    }
    if header_version != Some(body_version) {
        return Err(Fault::header());
    }
    if body_version != MODERN {
        return Err(Fault::unsupported_version(body_version));
    }
    if single_header(headers, "mcp-method")? != Some(request.method) {
        return Err(Fault::header());
    }
    let name_key = match request.method {
        "tools/call" | "prompts/get" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    };
    if let Some(key) = name_key {
        let name = request
            .param(key)
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::params("name or uri must be a string"))?;
        let header_name = single_header(headers, "mcp-name")?.ok_or_else(Fault::header)?;
        if decode_header_name(header_name)?.as_str() != name {
            return Err(Fault::header());
        }
    }
    Ok(Era::Modern)
}

fn decode_header_name(value: &str) -> Result<String, Fault> {
    if let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        let bytes = STANDARD.decode(encoded).map_err(|_| Fault::header())?;
        return String::from_utf8(bytes).map_err(|_| Fault::header());
    }
    if value.is_empty()
        || value.trim() != value
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ' || byte == b'\t')
    {
        return Err(Fault::header());
    }
    Ok(value.to_owned())
}

enum Action {
    Reply(Value),
    List(ListArguments),
    Code(CodeArguments),
    ResourceList(Option<String>),
    ResourceRead(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeArguments {
    code: String,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

const INSTRUCTIONS: &str = "Use list to discover clients allowed for your authenticated identity and this endpoint, then list({client_id}) for a client's native tool names and schemas. Use code_mode to run JavaScript with clients, await call(clientId, toolName, arguments), await tools(clientId, cursor), print/console.log and an explicit return. Each client provides shell tools and discovers installed Codex, Claude Code, Pi and OpenCode harnesses. Use exec.run or exec.parallel for isolated commands, and exec.start/read/wait/cancel for managed background work. Native policies and approvals remain in effect. Each client's access policy is enforced for discovery and native calls. Login-code grants end when their issuing SSH connection ends. Read ssmcp://guide and ssmcp://skill for usage, ssmcp://limits for budgets, and ssmcp://clients for a paged live inventory. Client metadata and tool schemas are exposed through resources/list and resources/read; resource reads are snapshots without subscriptions.";

fn action(request: &RpcRequest<'_>, era: Era) -> Result<Action, Fault> {
    match request.method {
        "initialize" if era == Era::Legacy => {
            let version = request
                .param("protocolVersion")
                .and_then(Value::as_str)
                .ok_or_else(|| Fault::params("protocolVersion must be a string"))?;
            if !request.param("capabilities").is_some_and(Value::is_object)
                || !request
                    .param("clientInfo")
                    .is_some_and(valid_implementation)
            {
                return Err(Fault::params(
                    "initialize requires capabilities and clientInfo",
                ));
            }
            Ok(Action::Reply(json!({
                "protocolVersion":if LEGACY.contains(&version) { version } else { LEGACY[0] },
                "capabilities":{"tools":{},"resources":{"subscribe":false,"listChanged":false}},
                "serverInfo":server_info(),
                "instructions":INSTRUCTIONS
            })))
        }
        "server/discover" if era == Era::Modern => Ok(Action::Reply(json!({
            "supportedVersions":VERSIONS,"capabilities":{"tools":{},"resources":{"subscribe":false,"listChanged":false}},
            "instructions":INSTRUCTIONS
        }))),
        "ping" => Ok(Action::Reply(json!({}))),
        "tools/list" => {
            if request.param("cursor").is_some() {
                return Err(Fault::params(
                    "the public tool catalog has no further pages",
                ));
            }
            Ok(Action::Reply(json!({"tools":tool_definitions()})))
        }
        "resources/list" => {
            resource_params(request, &["cursor"])?;
            Ok(Action::ResourceList(resource_cursor(request)?))
        }
        "resources/templates/list" => {
            resource_params(request, &["cursor"])?;
            if resource_cursor(request)?.is_some() {
                return Err(Fault::params(
                    "the resource template catalog has no further pages",
                ));
            }
            Ok(Action::Reply(resources::templates()))
        }
        "resources/read" => {
            resource_params(request, &["uri"])?;
            let uri = request
                .param("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| Fault::params("resource uri must be a string"))?;
            if uri.is_empty() || uri.len() > 1024 || uri.chars().any(char::is_control) {
                return Err(Fault::params(
                    "resource uri must be 1..=1024 bytes without control characters",
                ));
            }
            Ok(Action::ResourceRead(uri.into()))
        }
        "tools/call" => operation(request),
        _ => Err(Fault::new(
            if era == Era::Modern {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            },
            -32601,
            "Method not found",
        )),
    }
}

fn resource_params(request: &RpcRequest<'_>, allowed: &[&str]) -> Result<(), Fault> {
    if request.params.is_some_and(|params| {
        params
            .keys()
            .any(|key| key != "_meta" && !allowed.contains(&key.as_str()))
    }) {
        return Err(Fault::params("unknown resource parameter"));
    }
    Ok(())
}

fn resource_cursor(request: &RpcRequest<'_>) -> Result<Option<String>, Fault> {
    match request.param("cursor") {
        None => Ok(None),
        Some(Value::String(cursor)) if !cursor.is_empty() && cursor.len() <= 128 => {
            Ok(Some(cursor.clone()))
        }
        _ => Err(Fault::params(
            "resource cursor must be a nonempty string of at most 128 bytes",
        )),
    }
}

fn resource_fault(error: ResourceError, era: Era) -> Fault {
    let (status, code) = match error {
        ResourceError::InvalidParams(_) => (StatusCode::BAD_REQUEST, -32602),
        ResourceError::NotFound => (
            if era == Era::Modern {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::OK
            },
            -32002,
        ),
        ResourceError::Unavailable | ResourceError::TooLarge => (StatusCode::OK, -32603),
    };
    Fault::new(status, code, error.message())
}

fn valid_implementation(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.get("name").is_some_and(Value::is_string)
            && object.get("version").is_some_and(Value::is_string)
    })
}

fn operation(request: &RpcRequest<'_>) -> Result<Action, Fault> {
    let name = request
        .param("name")
        .and_then(Value::as_str)
        .ok_or_else(|| Fault::params("tool name must be a string"))?;
    let arguments = request
        .param("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "list" => {
            let arguments: ListArguments = serde_json::from_value(arguments)
                .map_err(|_| Fault::params("list accepts only client_id and cursor strings"))?;
            if arguments
                .client_id
                .as_ref()
                .is_some_and(|id| id.is_empty() || id.len() > 64)
                || arguments
                    .cursor
                    .as_ref()
                    .is_some_and(|cursor| cursor.len() > 128)
                || (arguments.cursor.is_some() && arguments.client_id.is_none())
            {
                return Err(Fault::params(
                    "client_id must be 1..=64 bytes; cursor is at most 128 bytes and requires client_id",
                ));
            }
            Ok(Action::List(arguments))
        }
        "code_mode" => {
            let arguments: CodeArguments = serde_json::from_value(arguments).map_err(|_| {
                Fault::params("code_mode requires code and accepts an optional timeout_ms integer")
            })?;
            if arguments.code.trim().is_empty() || arguments.code.len() > MAX_CODE_BYTES {
                return Err(Fault::params(&format!(
                    "code must be nonempty and at most {MAX_CODE_BYTES} UTF-8 bytes"
                )));
            }
            if arguments.timeout_ms == 0 || arguments.timeout_ms > MAX_TIMEOUT_MS {
                return Err(Fault::params(&format!(
                    "timeout_ms must be 1..={MAX_TIMEOUT_MS}"
                )));
            }
            Ok(Action::Code(arguments))
        }
        _ => Err(Fault::params("unknown public tool; use list or code_mode")),
    }
}

fn client_info(session: &Session) -> Value {
    let mut info = json!(session.info());
    info["cwd_kind"] = json!("startup");
    info
}

fn usage() -> Value {
    json!({
        "language":"javascript",
        "clients":"Online client metadata snapshot; select a client by id from list.",
        "call":"await call(clientId, toolName, arguments = {}) returns short native MCP results unchanged. Long backend output returns artifact metadata in structuredContent; inspect isError, spilled and completeness. Use outputs.read for raw PTY bytes or the saved MCP JSON of other oversized backend results.",
        "tools":"await tools(clientId, cursor?) returns a page of native tool schemas; pass nextCursor to get the next page.",
        "exec":"exec.run(clientId,{command:[...],cwd,wait_ms}) runs a command and collects output; exec.parallel(clientId,[{command:[...]},...],{concurrency:4}) settles bounded parallel runs. exec.start/list/status/read/wait/cancel/forget manage independent jobs. wait_ms only limits waiting; timeout_ms limits execution. Reuse job_id; commands survive code_mode completion.",
        "reload":"await reload(clientId) acknowledges a client restart from the updated installed executable. Stops background work; call list again for its new ID. GitHub --allow access persists; login-code connections require new OAuth login.",
        "outputs":"Long command output spills to private files under /tmp/ssmcp on the client. outputs.read(clientId,{id,offset,max_bytes}), outputs.stat(clientId,{id}), outputs.cleanup(clientId) inspect artifacts by returned ID. Metadata includes SHA-256, bytes, lines, completeness and expiry; automatic retention is 24 hours.",
        "output":"Use print(value) or console.log(value) for logs; return a JSON-serializable value. Use image(nativeScreenshotResult) to emit actual MCP image content without copying base64 into logs.",
        "computer":"Native desktop control: computer.status/monitors/windows/focus/screenshot/move/click/drag/key/type/scroll(clientId, options). Read status and tool schemas for session-specific capabilities; screenshot coordinates describe the current display mapping.",
        "browser":"Native Chromium control: browser.status/launch/connect/tabs/open/navigate/snapshot/click/fill/key/scroll/evaluate/screenshot/close/disconnect(clientId, options). Uses client-owned CDP connections and installed system browsers; no model is invoked.",
        "fs":"fs.list/stat/read/write/mkdir and fs.readText/writeText/editText(clientId, options) provide rooted binary and UTF-8 file primitives.",
        "skills":"skills.list/read/view(clientId, options) discover metadata summaries or read full SKILL.md documents without invoking a model.",
        "agents":"agents.list/read/resolve(clientId, options) inspect instruction files and root-to-target precedence with explicit provenance.",
        "context":"context.roots(clientId) lists workspace and operator-configured read-only context roots. Pass a returned root ID to skills/agents.",
        "rsync":"rsync({source:{client_id,path},destination:{client_id,path},dry_run:true,delete:false}) starts a managed job; sync is an alias. Both paths must be existing directories; copies contents. Reverse by swapping endpoints.",
        "jobs":"jobs.status(id), jobs.wait(id,{wait_ms:5000}), jobs.cancel(id), jobs.list(). Jobs survive code_mode completion; inspect the final state and exit codes.",
        "ports":"ports.open({listen:{client_id,port},target:{client_id,port}}), ports.list(), ports.close(id). Loopback only; port 0 selects an unused listener port.",
        "resources":"resources.list(cursor?), resources.templates(), resources.read(uri). Start with ssmcp://guide.",
        "workspace":"The caller sandbox needs its own registered client; server-side JavaScript cannot see its filesystem or loopback.",
        "example":"const c = clients.find(c => c.type === 'shell'); if (!c) throw new Error('No shell client'); return await call(c.id, 'terminal.status', {});"
    })
}

async fn list(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
    arguments: ListArguments,
) -> Result<Value, String> {
    if let Some(id) = arguments.client_id {
        let session = broker.client(user, authorization, &id).await?;
        let mut page = session
            .call(Operation::ListTools {
                cursor: arguments.cursor,
            })
            .await?;
        if !page.is_object() {
            return Err("client returned an invalid tool catalog".into());
        }
        page["client"] = client_info(&session);
        return Ok(page);
    }
    let sessions = broker.clients(user, authorization).await?;
    let clients: Vec<_> = sessions
        .iter()
        .map(|session| client_info(session))
        .collect();
    Ok(json!({"clients":clients,"code_mode":usage()}))
}

async fn execute(
    broker: Arc<Broker>,
    user: String,
    authorization: Authorization,
    access_token: String,
    arguments: CodeArguments,
) -> Result<Value, String> {
    let sessions = broker.clients(&user, &authorization).await?;
    let clients = json!(
        sessions
            .iter()
            .map(|session| client_info(session))
            .collect::<Vec<_>>()
    );
    let request_id = crate::observability::REQUEST_ID
        .try_with(Clone::clone)
        .unwrap_or_default();
    let host: Host = Arc::new(move |request| {
        let request_id = request_id.clone();
        let broker = broker.clone();
        let user = user.clone();
        let access_token = access_token.clone();
        Box::pin(async move {
            let (client_id, operation) = match request {
                HostRequest::Builtin { name, arguments } => {
                    return crate::bridge::dispatch(broker, user, access_token, name, arguments)
                        .await;
                }
                HostRequest::Call {
                    client_id,
                    name,
                    arguments,
                } => (client_id, Operation::CallTool { name, arguments }),
                HostRequest::Tools { client_id, cursor } => {
                    (client_id, Operation::ListTools { cursor })
                }
            };
            // A snapshot grants no authority. Recheck token state, resource scope,
            // the live connection and its ACL for every call, including after await.
            let authorization = broker
                .authenticate(&user, &access_token)
                .await
                .ok_or("authorization expired or was revoked; refresh the MCP connection")?;
            let session = broker.client(&user, &authorization, &client_id).await?;
            crate::observability::log(
                "debug",
                "proxy",
                &[
                    ("event", "native_dispatch".into()),
                    ("request_id", request_id.clone()),
                    ("client_id", client_id.clone()),
                ],
            );
            let is_reload = crate::client::reload::is_reload(&operation);
            let result = crate::observability::REQUEST_ID
                .scope(request_id, session.call(operation))
                .await?;
            // The authorized lifecycle action deliberately invalidates its old
            // connection. Deliver its ACK even if EOF arrives before this task
            // resumes; it contains no post-disconnect data or reusable authority.
            if is_reload && crate::client::reload::accepted(&result) {
                // Only fixed acknowledgement fields survive this exception;
                // arbitrary native content still requires the normal live check.
                return Ok(crate::client::reload::acknowledgement(
                    &client_id,
                    result["structuredContent"]["auth"] == "github",
                ));
            }
            let authorization = broker
                .authenticate(&user, &access_token)
                .await
                .ok_or("authorization expired or was revoked; refresh the MCP connection")?;
            broker.client(&user, &authorization, &client_id).await?;
            Ok(result)
        })
    });
    code_mode::execute(arguments.code, arguments.timeout_ms, clients, host).await
}

fn tool_result(result: Result<Value, String>) -> Value {
    match result {
        Ok(value) => crate::backend::result(value),
        Err(message) => json!({"content":[{"type":"text","text":message}],"isError":true}),
    }
}

pub(crate) fn tool_definitions() -> Value {
    json!([
        {
            "name":"list",
            "description":"List online clients allowed for your authenticated identity and this endpoint, with unique id, name, optional task title, type, hostname and startup cwd. Use title to identify the intended task, then dispatch by id. With client_id, return that client's native tool names, descriptions and input schemas for code_mode; follow nextCursor with cursor to page. Client access policies are checked on every lookup.",
            "inputSchema":{"type":"object","properties":{
                "client_id":{"type":"string","minLength":1,"maxLength":64},
                "cursor":{"type":"string","maxLength":128}
            },"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"code_mode",
            "description":"Run JavaScript that orchestrates the clients returned by list. Globals: clients (metadata array), await call(clientId, nativeToolName, arguments = {}), await tools(clientId, cursor?) for native tool schemas, print/console.log, and top-level return. Short native results are returned unchanged; long backend output returns artifact metadata. Inspect isError, structuredContent.spilled and capture completeness. Use outputs.read for raw PTY bytes or the saved MCP JSON of other oversized backend results. Includes native computer and browser controls with image(screenshotResult) for visible screenshots; exec.run and exec.parallel for bounded parallel commands; exec.start/list/status/read/wait/cancel/forget for background job management; reload(clientId), outputs.read/stat/cleanup, fs, context, skills, agents, rsync/sync managed jobs, jobs.status/wait/cancel/list, ports.open/list/close and resources.list/read/templates. Reload requires updated client and server, stops background work and reconnects with a new ID; call list again. Supports variables, loops, conditions, await and Promise.all. The caller sandbox must be connected as its own client for cross-workspace file or port access. All shell/codex/cc/pi/opencode actions run through call over SSH. Native permissions/approvals apply. System Node.js executes the code in an isolated process; only the orchestration APIs above are exposed. Return JSON-serializable data; results and logs are bounded. Timeout stops orchestration; already-started native actions may continue, so inspect their state before retrying.",
            "inputSchema":{"type":"object","properties":{
                "code":{"type":"string","minLength":1,"maxLength":MAX_CODE_BYTES},
                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT_MS,"default":DEFAULT_TIMEOUT_MS}
            },"required":["code"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}
        }
    ])
}

fn server_info() -> Value {
    json!({"name":"ssmcpd","version":env!("CARGO_PKG_VERSION")})
}

fn decorate_result(mut result: Value, era: Era) -> Value {
    if era == Era::Modern {
        result["resultType"] = json!("complete");
        if !result.get("_meta").is_some_and(Value::is_object) {
            result["_meta"] = json!({});
        }
        result["_meta"]["io.modelcontextprotocol/serverInfo"] = server_info();
    }
    result
}

struct Fault {
    status: StatusCode,
    code: i32,
    message: String,
    data: Option<Value>,
}

impl Fault {
    fn new(status: StatusCode, code: i32, message: &str) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            data: None,
        }
    }
    fn invalid_request() -> Self {
        Self::new(StatusCode::BAD_REQUEST, -32600, "Invalid request")
    }
    fn params(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, -32602, message)
    }
    fn header() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            -32020,
            "Required MCP header is missing, malformed, or does not match the request",
        )
    }
    fn unsupported_version(version: &str) -> Self {
        let mut fault = Self::new(
            StatusCode::BAD_REQUEST,
            -32022,
            "Unsupported protocol version",
        );
        fault.data = Some(json!({"supported":VERSIONS,"requested":version}));
        fault
    }
    fn response(self, id: Option<&Value>) -> Response {
        let mut value = json!({"jsonrpc":"2.0","error":{"code":self.code,"message":self.message}});
        if let Some(id) = id {
            value["id"] = id.clone();
        }
        if let Some(data) = self.data {
            value["error"]["data"] = data;
        }
        (self.status, Json(value)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modern_request(method: &str) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":{"_meta":{
            VERSION_META:MODERN,CAPABILITIES_META:{}
        }}})
    }
    fn modern_headers(method: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-protocol-version", HeaderValue::from_static(MODERN));
        headers.insert("mcp-method", HeaderValue::from_str(method).unwrap());
        headers
    }
    fn parse(value: &Value) -> RpcRequest<'_> {
        RpcRequest::parse(value).unwrap_or_else(|f| panic!("{}", f.message))
    }

    #[test]
    fn bearer_is_single_and_never_accepts_ambiguous_values() {
        let mut headers = HeaderMap::new();
        for bad in [
            "",
            "secret",
            "Basic secret",
            "Bearer ",
            "Bearer  secret",
            "Bearer secret ",
            "Bearer one,two",
        ] {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(bad).unwrap());
            assert!(bearer(&headers).is_none(), "{bad}");
        }
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc-_123"),
        );
        assert_eq!(bearer(&headers), Some("abc-_123"));
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer other"),
        );
        assert!(bearer(&headers).is_none());
    }

    #[test]
    fn bearer_challenge_signals_invalid_tokens_without_mislabeling_initial_discovery() {
        let mut headers = HeaderMap::new();
        assert!(!has_bearer_authorization(&headers));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Basic secret"),
        );
        assert!(!has_bearer_authorization(&headers));
        for value in ["Bearer", "Bearer ", "bEaReR unknown", "Bearer one,two"] {
            headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert!(has_bearer_authorization(&headers), "{value}");
        }
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer other"),
        );
        assert!(has_bearer_authorization(&headers));

        for invalid_token in [false, true] {
            let response = unauthorized("https://mcp.example.test", "alice", invalid_token);
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            let challenge = response.headers()[header::WWW_AUTHENTICATE]
                .to_str()
                .unwrap();
            assert!(challenge.contains("resource_metadata=\"https://mcp.example.test/.well-known/oauth-protected-resource/alice\""));
            assert_eq!(challenge.contains("error=\"invalid_token\""), invalid_token);
        }
    }

    #[test]
    fn origin_must_be_the_configured_origin() {
        let mut headers = HeaderMap::new();
        assert!(valid_origin(&headers, "https://example.com"));
        for origin in [
            "null",
            "https://evil.example",
            "https://example.com.evil",
            "https://example.com/",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
            assert!(!valid_origin(&headers, "https://example.com"));
        }
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://example.com"),
        );
        assert!(valid_origin(&headers, "https://example.com/"));
        headers.append(
            header::ORIGIN,
            HeaderValue::from_static("https://example.com"),
        );
        assert!(!valid_origin(&headers, "https://example.com"));
    }

    #[test]
    fn malformed_rpc_and_batches_are_rejected() {
        for value in [
            json!([]),
            json!(null),
            json!({}),
            json!({"jsonrpc":"1.0","id":1,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":1.5,"method":"ping"}),
            json!({"jsonrpc":"2.0","id":1,"result":{}}),
            json!({"jsonrpc":"2.0","id":1,"method":"ping","params":[]}),
        ] {
            assert!(RpcRequest::parse(&value).is_err(), "{value}");
        }
        assert!(RpcRequest::parse(&json!({"jsonrpc":"2.0","id":"x","method":"ping"})).is_ok());
    }

    #[test]
    fn notifications_have_no_execution_id() {
        for method in ["notifications/initialized", "tools/call", "unknown"] {
            let value = json!({"jsonrpc":"2.0","method":method});
            let request = parse(&value);
            assert!(request.id.is_none());
            assert_eq!(
                validate_protocol(&HeaderMap::new(), &request).ok(),
                Some(Era::Legacy)
            );
        }
    }

    #[test]
    fn modern_metadata_and_headers_are_checked() {
        let value = modern_request("tools/list");
        let headers = modern_headers("tools/list");
        assert_eq!(
            validate_protocol(&headers, &parse(&value)).ok(),
            Some(Era::Modern)
        );
        let mut missing_caps = value.clone();
        missing_caps["params"]["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(CAPABILITIES_META);
        assert_eq!(
            validate_protocol(&headers, &parse(&missing_caps))
                .err()
                .unwrap()
                .code,
            -32602
        );
        let mut malformed_info = value.clone();
        malformed_info["params"]["_meta"]["io.modelcontextprotocol/clientInfo"] =
            json!({"name": "test"});
        assert_eq!(
            validate_protocol(&headers, &parse(&malformed_info))
                .err()
                .unwrap()
                .code,
            -32602
        );
        for name in ["mcp-method", "mcp-protocol-version"] {
            let mut missing = headers.clone();
            missing.remove(name);
            assert_eq!(
                validate_protocol(&missing, &parse(&value))
                    .err()
                    .unwrap()
                    .code,
                -32020
            );
        }
        let mut mismatch = headers.clone();
        mismatch.insert("mcp-method", HeaderValue::from_static("tools/call"));
        assert_eq!(
            validate_protocol(&mismatch, &parse(&value))
                .err()
                .unwrap()
                .code,
            -32020
        );
        mismatch = headers.clone();
        mismatch.append("mcp-method", HeaderValue::from_static("tools/list"));
        assert_eq!(
            validate_protocol(&mismatch, &parse(&value))
                .err()
                .unwrap()
                .code,
            -32020
        );
    }

    #[test]
    fn modern_name_headers_support_base64_and_reject_mismatches() {
        let mut value = modern_request("tools/call");
        value["params"]["name"] = json!("terminal.status");
        let mut headers = modern_headers("tools/call");
        assert_eq!(
            validate_protocol(&headers, &parse(&value))
                .err()
                .unwrap()
                .code,
            -32020
        );
        headers.insert("mcp-name", HeaderValue::from_static("terminal.status"));
        assert!(validate_protocol(&headers, &parse(&value)).is_ok());
        headers.insert(
            "mcp-name",
            HeaderValue::from_str(&format!(
                "=?base64?{}?=",
                STANDARD.encode("terminal.status")
            ))
            .unwrap(),
        );
        assert!(validate_protocol(&headers, &parse(&value)).is_ok());
        for bad in ["terminal.write", "=?base64?%%%?=", " terminal.status"] {
            headers.insert("mcp-name", HeaderValue::from_str(bad).unwrap());
            assert_eq!(
                validate_protocol(&headers, &parse(&value))
                    .err()
                    .unwrap()
                    .code,
                -32020
            );
        }
    }

    #[test]
    fn unknown_version_lists_supported_versions() {
        let value = modern_request("tools/list");
        let mut headers = modern_headers("tools/list");
        headers.insert(
            "mcp-protocol-version",
            HeaderValue::from_static("2099-01-01"),
        );
        let fault = validate_protocol(&headers, &parse(&value)).err().unwrap();
        assert_eq!(fault.code, -32022);
        assert_eq!(fault.data.unwrap()["supported"], json!(VERSIONS));
    }

    #[test]
    fn legacy_initialization_never_negotiates_a_modern_version() {
        for version in [MODERN, "2025-11-25", "2025-06-18", "2025-03-26", "unknown"] {
            let value = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":version,"capabilities":{},"clientInfo":{"name":"test","version":"1"}
            }});
            let request = parse(&value);
            assert_eq!(
                validate_protocol(&HeaderMap::new(), &request).ok(),
                Some(Era::Legacy)
            );
            let Action::Reply(result) =
                action(&request, Era::Legacy).unwrap_or_else(|f| panic!("{}", f.message))
            else {
                panic!()
            };
            assert!(LEGACY.contains(&result["protocolVersion"].as_str().unwrap()));
            if LEGACY.contains(&version) {
                assert_eq!(result["protocolVersion"], version);
            }
        }
    }

    #[test]
    fn discovery_and_unknown_method_use_correct_era() {
        let value = modern_request("server/discover");
        let request = parse(&value);
        let Action::Reply(result) =
            action(&request, Era::Modern).unwrap_or_else(|f| panic!("{}", f.message))
        else {
            panic!()
        };
        let result = decorate_result(result, Era::Modern);
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"], json!(VERSIONS));
        assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());
        assert_eq!(action(&request, Era::Legacy).err().unwrap().code, -32601);
        let value = modern_request("missing");
        assert_eq!(
            action(&parse(&value), Era::Modern).err().unwrap().status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            action(&parse(&value), Era::Legacy).err().unwrap().status,
            StatusCode::OK
        );
    }

    #[test]
    fn public_catalog_and_arguments_are_exactly_two_tools() {
        let names: Vec<_> = tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["list", "code_mode"]);
        let mut value =
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list"}});
        assert!(matches!(operation(&parse(&value)), Ok(Action::List(_))));
        for arguments in [
            json!({"extra":true}),
            json!({"cursor":"abc"}),
            json!({"client_id":""}),
            json!({"client_id":123}),
        ] {
            value["params"]["arguments"] = arguments;
            assert!(operation(&parse(&value)).is_err());
        }
        value["params"]["name"] = json!("code_mode");
        for arguments in [
            json!({}),
            json!({"code":""}),
            json!({"code":"return 1", "timeout_ms":0}),
            json!({"code":"return 1", "timeout_ms":MAX_TIMEOUT_MS+1}),
            json!({"code":"return 1", "extra":true}),
            json!({"code":"界".repeat(MAX_CODE_BYTES/3+1)}),
        ] {
            value["params"]["arguments"] = arguments;
            assert!(operation(&parse(&value)).is_err());
        }
        value["params"]["arguments"] = json!({"code":"return clients.length"});
        assert!(matches!(operation(&parse(&value)), Ok(Action::Code(_))));
        for name in [
            "terminal.read",
            "terminal.write",
            "codex.thread_start",
            "cc.Read",
            "pi.prompt",
            "opencode.request",
        ] {
            value["params"]["name"] = json!(name);
            value["params"]["arguments"] = json!({});
            assert!(operation(&parse(&value)).is_err());
        }
    }

    #[test]
    fn modern_result_preserves_native_metadata() {
        let native = json!({"content":[],"_meta":{"vendor/result":[1,2,3],"ui":{"resourceUri":"ui://native"}}});
        let result = decorate_result(native.clone(), Era::Modern);
        assert_eq!(
            result["_meta"]["vendor/result"],
            native["_meta"]["vendor/result"]
        );
        assert_eq!(result["_meta"]["ui"], native["_meta"]["ui"]);
        assert_eq!(result["resultType"], "complete");
        assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());
    }

    #[test]
    fn accept_negotiation_honors_explicit_json_exclusion() {
        let mut headers = HeaderMap::new();
        assert!(accepts_json(&headers));
        for value in [
            "application/json, text/event-stream",
            "*/*",
            "application/*",
        ] {
            headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
            assert!(accepts_json(&headers));
        }
        for value in [
            "text/event-stream",
            "application/json;q=0, */*",
            "application/json;q=garbage",
        ] {
            headers.insert(header::ACCEPT, HeaderValue::from_str(value).unwrap());
            assert!(!accepts_json(&headers));
        }
    }
}
