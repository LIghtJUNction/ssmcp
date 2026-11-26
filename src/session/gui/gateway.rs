//! HTTP transport for an explicitly paired CLI host; all native work stays in Host.
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Query, Request, State, rejection::QueryRejection},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    net::IpAddr,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use tokio::{io::AsyncReadExt, sync::Semaphore, time::timeout};
use zbridge::{
    BridgeError, Counter, ErrorCode, Id, Operation, Request as BridgeRequest,
    Response as BridgeResponse, ResponseBody,
};

use crate::session::attach::{EventObserver, MAX_OBSERVATION_MS};

pub(super) type CoreFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, BridgeError>> + Send + 'a>>;

pub(super) trait Connection: Send + Sync {
    fn request(&self, request: BridgeRequest) -> CoreFuture<'_, BridgeResponse>;
    fn close(&self) -> CoreFuture<'_, ()>;
}

pub(super) trait Host: Send + Sync {
    fn launch(&self, _destination: String) -> CoreFuture<'_, String> {
        Box::pin(async {
            Err(BridgeError::new(
                ErrorCode::Denied,
                "session launch is unavailable",
            ))
        })
    }
    fn sessions(&self) -> CoreFuture<'_, Vec<Value>>;
    fn connect(&self, session: String, read_only: bool) -> CoreFuture<'_, Arc<dyn Connection>>;
}

pub(super) struct Config {
    pub token: String,
    pub origin: String,
    pub assets: Option<PathBuf>,
}

const PREFIX: &str = "/api/gui/v1";
const MAX_CONNECTIONS: usize = 16;
const MAX_OPEN_RECEIPTS: usize = 128;
const RETENTION: Duration = Duration::from_secs(15 * 60);
const MAX_ASSET_BYTES: u64 = 2 * 1024 * 1024;
const NATIVE_DEADLINE: Duration = Duration::from_secs(55);

struct Gateway {
    token: String,
    origin: String,
    authority: String,
    assets: Option<PathBuf>,
    host: Arc<dyn Host>,
    data: Mutex<Data>,
    next_connection: AtomicU64,
    requests: Semaphore,
}

#[derive(Default)]
struct Data {
    connections: BTreeMap<String, Connected>,
    opens: VecDeque<OpenReceipt>,
}

struct Connected {
    session: String,
    read_only: bool,
    request_id: Id,
    core: Arc<dyn Connection>,
    used: Instant,
    closing: bool,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OpenRequest {
    request_id: Id,
    session: String,
    #[serde(default = "read_only")]
    read_only: bool,
}
fn read_only() -> bool {
    true
}

struct OpenReceipt {
    request: OpenRequest,
    connection_id: String,
    retained_at: Instant,
    outcome: OpenOutcome,
}
enum OpenOutcome {
    Opening,
    Connected,
    Failed(BridgeError),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "page_limit")]
    limit: usize,
}
fn page_limit() -> usize {
    16
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObserveRequest {
    request_id: Id,
    generation: Id,
    #[serde(default)]
    cursor: Option<Counter>,
    #[serde(default = "event_limit")]
    limit: u16,
    #[serde(default = "event_wait")]
    wait_ms: u16,
}
fn event_limit() -> u16 {
    16
}
fn event_wait() -> u16 {
    MAX_OBSERVATION_MS
}

#[derive(Debug)]
struct ApiError(StatusCode, BridgeError);
impl From<BridgeError> for ApiError {
    fn from(error: BridgeError) -> Self {
        let status = match error.code {
            ErrorCode::InvalidRequest | ErrorCode::UnsupportedVersion => StatusCode::BAD_REQUEST,
            ErrorCode::Denied => StatusCode::FORBIDDEN,
            ErrorCode::Closed => StatusCode::GONE,
            ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::StaleGeneration | ErrorCode::InvalidCursor | ErrorCode::Conflict => {
                StatusCode::CONFLICT
            }
            ErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self(status, error)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
fn invalid(message: &str) -> ApiError {
    BridgeError::invalid(message).into()
}
fn unavailable(message: &str) -> BridgeError {
    BridgeError::new(ErrorCode::Unavailable, message)
}

pub(super) fn router(config: Config, host: Arc<dyn Host>) -> Result<Router, BridgeError> {
    routes(config, host, false)
}

pub(super) fn routes(
    config: Config,
    host: Arc<dyn Host>,
    magisk_entry: bool,
) -> Result<Router, BridgeError> {
    if config.token.len() != 64 || !config.token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BridgeError::invalid(
            "gateway pairing requires a 32-byte hexadecimal secret",
        ));
    }
    let url = reqwest::Url::parse(&config.origin)
        .map_err(|_| BridgeError::invalid("invalid gateway origin"))?;
    let loopback = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if !matches!(url.scheme(), "http" | "https")
        || (url.scheme() == "http" && !loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || url.host_str().is_none()
    {
        return Err(BridgeError::invalid(
            "gateway origin must be loopback HTTP or an explicit HTTPS origin",
        ));
    }
    let origin = url.origin().ascii_serialization();
    let authority = origin.split_once("://").expect("HTTP origin").1.to_owned();
    let assets = config
        .assets
        .map(|path| {
            path.canonicalize()
                .map_err(|_| BridgeError::invalid("gateway assets directory is unavailable"))
        })
        .transpose()?;
    if assets.as_ref().is_some_and(|path| !path.is_dir()) {
        return Err(BridgeError::invalid("gateway assets must be a directory"));
    }
    let state = Arc::new(Gateway {
        token: config.token,
        origin,
        authority,
        assets,
        host,
        data: Mutex::new(Data::default()),
        next_connection: AtomicU64::new(1),
        requests: Semaphore::new(32),
    });
    let api = Router::new()
        .route("/api/gui/v1/launch", post(launch))
        .route("/api/gui/v1/register", post(register))
        .route("/api/gui/v1/sessions", get(sessions))
        .route("/api/gui/v1/connections", get(connections).post(open))
        .route("/api/gui/v1/connections/{id}", axum::routing::delete(close))
        .route("/api/gui/v1/connections/{id}/bridge", post(bridge))
        .route("/api/gui/v1/connections/{id}/events", post(events));
    let homepage = if magisk_entry {
        get(magisk_index)
    } else {
        get(index)
    };
    let app = api.route("/", homepage).route("/{*path}", get(asset));
    Ok(app
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state))
}

async fn guard(State(state): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let valid_origin = headers.get_all(header::HOST).iter().count() == 1
        && headers
            .get(header::HOST)
            .is_some_and(|value| value.as_bytes() == state.authority.as_bytes())
        && headers.get_all(header::ORIGIN).iter().count() <= 1
        && headers
            .get(header::ORIGIN)
            .is_none_or(|value| value.as_bytes() == state.origin.as_bytes());
    if !valid_origin {
        return ApiError::from(BridgeError::new(
            ErrorCode::Denied,
            "gateway origin does not match its configured listener or proxy",
        ))
        .into_response();
    }
    if request.uri().path().starts_with(PREFIX) {
        let token = (headers.get_all(header::AUTHORIZATION).iter().count() == 1)
            .then(|| headers.get(header::AUTHORIZATION))
            .flatten()
            .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "));
        if !token.is_some_and(|token| bool::from(token.ct_eq(state.token.as_bytes()))) {
            return ApiError(
                StatusCode::UNAUTHORIZED,
                BridgeError::new(ErrorCode::Denied, "gateway pairing is required"),
            )
            .into_response();
        }
    }
    let Ok(_permit) = state.requests.try_acquire() else {
        return ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            unavailable("gateway request capacity reached"),
        )
        .into_response();
    };
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; object-src 'none'".parse().unwrap());
    response
}

async fn input<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.split(';').next().map(str::trim) != Some("application/json"))
    {
        return Err(invalid("use application/json for gateway requests"));
    }
    let bytes = timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), zbridge::MAX_FRAME_BYTES),
    )
    .await
    .map_err(|_| invalid("gateway request body deadline elapsed"))?
    .map_err(|_| {
        ApiError::from(BridgeError::new(
            ErrorCode::TooLarge,
            "gateway request exceeds 256 KiB",
        ))
    })?;
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid gateway request fields"))
}

impl Gateway {
    fn prune(self: &Arc<Self>) {
        let expired = {
            let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let expired: Vec<_> = data
                .connections
                .iter_mut()
                .filter_map(|(id, connection)| {
                    if connection.closing || now.duration_since(connection.used) < RETENTION {
                        return None;
                    }
                    connection.closing = true;
                    connection.used = now;
                    Some((id.clone(), connection.core.clone()))
                })
                .collect();
            let Data { connections, opens } = &mut *data;
            opens.retain(|r| {
                now.duration_since(r.retained_at) < RETENTION
                    || connections.contains_key(&r.connection_id)
            });
            expired
        };
        for (id, connection) in expired {
            let state = self.clone();
            tokio::spawn(async move {
                if matches!(
                    timeout(NATIVE_DEADLINE, connection.close()).await,
                    Ok(Ok(()))
                ) {
                    state.acknowledge_close(&id);
                }
            });
        }
    }

    fn acknowledge_close(&self, id: &str) {
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        data.connections.remove(id);
        if let Some(receipt) = data.opens.iter_mut().find(|r| r.connection_id == id) {
            receipt.retained_at = Instant::now();
        }
    }

    fn connection(self: &Arc<Self>, id: &str) -> Result<(Arc<dyn Connection>, bool), BridgeError> {
        self.prune();
        let mut data = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let connection = data.connections.get_mut(id).ok_or_else(|| {
            BridgeError::new(
                ErrorCode::Closed,
                "gateway connection is closed or unavailable",
            )
        })?;
        if connection.closing {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "gateway connection is detaching; retry DELETE to confirm lease release",
            ));
        }
        connection.used = Instant::now();
        Ok((connection.core.clone(), connection.read_only))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchRequest {
    destination: String,
    #[serde(default)]
    terms: bool,
    #[serde(default)]
    privacy: bool,
}

impl LaunchRequest {
    fn checked_destination(self) -> Result<String, ApiError> {
        let (name, origin) = crate::server_endpoint::parse_target(&self.destination)
            .map_err(|_| invalid("enter a valid username@server"))?;
        let url = reqwest::Url::parse(&origin).map_err(|_| invalid("invalid relay"))?;
        if url
            .host_str()
            .is_some_and(|s| s.trim_end_matches('.') == "ssmcp.lmm.best")
            && !(self.terms && self.privacy)
        {
            return Err(invalid(
                "confirm the hosted relay's terms and privacy policy",
            ));
        }
        crate::server_endpoint::allow_principal(&name, &origin)
            .map_err(|_| invalid("invalid destination"))
    }
}

async fn register(
    State(_): State<Arc<Gateway>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let destination = input::<LaunchRequest>(request)
        .await?
        .checked_destination()?;
    let url = crate::desktop_terminal::registration_url(&destination)
        .await
        .map_err(|e| invalid(&e.to_string()))?;
    Ok(Json(json!({"url":url})))
}

async fn launch(
    State(state): State<Arc<Gateway>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let destination = input::<LaunchRequest>(request)
        .await?
        .checked_destination()?;
    let target = state.host.launch(destination.clone()).await?;
    crate::desktop_terminal::save_desktop_profile(&destination)
        .map_err(|e| unavailable(&e.to_string()))?;
    Ok(Json(json!({"session":target})))
}

async fn sessions(
    State(state): State<Arc<Gateway>>,
    query: Result<Query<PageQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| invalid("invalid session page query"))?;
    if query.limit == 0 || query.limit > 32 {
        return Err(invalid("session page limit must be 1..32"));
    }
    let list = timeout(Duration::from_secs(15), state.host.sessions())
        .await
        .map_err(|_| unavailable("session list deadline elapsed"))??;
    if list.len() > 1024 || query.offset > list.len() {
        return Err(invalid(
            "session page offset exceeds the current bounded list",
        ));
    }
    let end = (query.offset + query.limit).min(list.len());
    let fields = [
        "target",
        "kind",
        "state",
        "name",
        "cwd",
        "manager_state",
        "manager_active",
        "connection_state",
        "phase",
        "generation",
        "stop_requested",
    ];
    let sessions: Vec<Value> = list[query.offset..end]
        .iter()
        .map(|row| {
            Value::Object(
                fields
                    .iter()
                    .map(|key| ((*key).into(), row.get(*key).cloned().unwrap_or(Value::Null)))
                    .collect(),
            )
        })
        .collect();
    let reply = json!({"sessions":sessions,"next_offset":if end < list.len() {Some(end)} else {None},"has_more":end < list.len()});
    zbridge::encoded_len(&reply, zbridge::MAX_FRAME_BYTES)?;
    Ok(Json(reply))
}

async fn connections(State(state): State<Arc<Gateway>>) -> Json<Value> {
    state.prune();
    let data = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let entries: Vec<_> = data.connections.iter().map(|(id, c)| json!({"connection_id":id,"session":c.session,"read_only":c.read_only,"request_id":c.request_id,"status":if c.closing {"closing"} else {"connected"}})).collect();
    Json(json!({"connections":entries}))
}

struct Opening {
    state: Arc<Gateway>,
    request_id: Id,
    settled: bool,
}
impl Drop for Opening {
    fn drop(&mut self) {
        if !self.settled {
            let mut data = self.state.data.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(receipt) = data
                .opens
                .iter_mut()
                .find(|r| r.request.request_id == self.request_id)
            {
                receipt.outcome = OpenOutcome::Failed(
                    unavailable("connection opening was interrupted; its lease outcome is unknown")
                        .uncertain(),
                );
                receipt.retained_at = Instant::now();
            }
        }
    }
}

async fn open(State(state): State<Arc<Gateway>>, request: Request) -> Result<Response, ApiError> {
    let input: OpenRequest = input(request).await?;
    if input.session.is_empty()
        || input.session.len() > 128
        || input.session.trim() != input.session
        || input
            .session
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(invalid("invalid saved session name"));
    }
    state.prune();
    let (connection_id, duplicate, opening) = {
        let mut data = state.data.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = data
            .opens
            .iter()
            .position(|r| r.request.request_id == input.request_id)
        {
            if data.opens[index].request != input {
                return Err(BridgeError::new(
                    ErrorCode::Conflict,
                    "open request_id was already used with different fields",
                )
                .into());
            }
            data.opens[index].retained_at = Instant::now();
            let previous = &data.opens[index];
            match &previous.outcome {
                OpenOutcome::Failed(error) => return Err(error.clone().into()),
                OpenOutcome::Opening => return Ok((StatusCode::ACCEPTED, Json(json!({"status":"opening","connection_id":previous.connection_id,"session":input.session,"read_only":input.read_only,"duplicate":true}))).into_response()),
                OpenOutcome::Connected if !data.connections.contains_key(&previous.connection_id) => return Ok(Json(json!({"status":"closed","connection_id":previous.connection_id,"session":input.session,"read_only":input.read_only,"duplicate":true})).into_response()),
                OpenOutcome::Connected if data.connections.get(&previous.connection_id).is_some_and(|c| c.closing) => return Ok(Json(json!({"status":"closing","connection_id":previous.connection_id,"session":input.session,"read_only":input.read_only,"duplicate":true})).into_response()),
                _ => (previous.connection_id.clone(), true, false),
            }
        } else {
            let pending = data
                .opens
                .iter()
                .filter(|r| matches!(r.outcome, OpenOutcome::Opening))
                .count();
            if data.connections.len() + pending >= MAX_CONNECTIONS
                || data.opens.len() >= MAX_OPEN_RECEIPTS
            {
                return Err(unavailable(
                    "gateway connection or recent-open receipt capacity reached",
                )
                .into());
            }
            let sequence = state
                .next_connection
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
                .map_err(|_| unavailable("gateway connection sequence exhausted"))?;
            let connection_id = format!("gui_{sequence:032x}");
            data.opens.push_back(OpenReceipt {
                request: input.clone(),
                connection_id: connection_id.clone(),
                retained_at: Instant::now(),
                outcome: OpenOutcome::Opening,
            });
            (connection_id, false, true)
        }
    };
    if opening {
        let mut flight = Opening {
            state: state.clone(),
            request_id: input.request_id.clone(),
            settled: false,
        };
        let connected = timeout(
            Duration::from_secs(10),
            state.host.connect(input.session.clone(), input.read_only),
        )
        .await
        .map_err(|_| {
            unavailable("connection opening deadline elapsed; lease outcome is unknown").uncertain()
        })
        .and_then(|r| r);
        {
            let mut data = state.data.lock().unwrap_or_else(|e| e.into_inner());
            data.opens
                .iter_mut()
                .find(|r| r.request.request_id == input.request_id)
                .expect("bounded open receipt exists")
                .retained_at = Instant::now();
            match &connected {
                Ok(core) => {
                    data.connections.insert(
                        connection_id.clone(),
                        Connected {
                            session: input.session.clone(),
                            read_only: input.read_only,
                            request_id: input.request_id.clone(),
                            core: core.clone(),
                            used: Instant::now(),
                            closing: false,
                        },
                    );
                    data.opens
                        .iter_mut()
                        .find(|r| r.request.request_id == input.request_id)
                        .expect("open receipt retained during bounded connect")
                        .outcome = OpenOutcome::Connected;
                }
                Err(error) => {
                    data.opens
                        .iter_mut()
                        .find(|r| r.request.request_id == input.request_id)
                        .expect("open receipt retained during bounded connect")
                        .outcome = OpenOutcome::Failed(error.clone())
                }
            }
        }
        flight.settled = true;
        connected?;
    }
    let (core, _) = state.connection(&connection_id)?;
    let request = BridgeRequest {
        version: zbridge::VERSION,
        id: input.request_id,
        operation: Operation::Capabilities {},
    };
    let capabilities = timeout(NATIVE_DEADLINE, core.request(request.clone()))
        .await
        .map_err(|_| unavailable("capability discovery deadline elapsed"))??;
    capabilities.validate_for(&request)?;
    if let ResponseBody::Error(error) = &capabilities.body {
        return Err(error.clone().into());
    }
    Ok((if duplicate {StatusCode::OK} else {StatusCode::CREATED}, Json(json!({"status":"connected","connection_id":connection_id,"session":input.session,"read_only":input.read_only,"capabilities":capabilities,"duplicate":duplicate}))).into_response())
}

async fn bridge(
    State(state): State<Arc<Gateway>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<BridgeResponse>, ApiError> {
    let request: BridgeRequest = input(request).await?;
    request.validate()?;
    let (core, read_only) = state.connection(&id)?;
    let command = request.operation.is_command();
    if command && read_only {
        // This denial happens before native dispatch, so its effect is known.
        return Ok(Json(BridgeResponse {
            version: zbridge::VERSION,
            id: request.id,
            generation: request.operation.generation().cloned(),
            body: ResponseBody::Error(BridgeError::new(
                ErrorCode::Denied,
                "attach frontend is read-only",
            )),
        }));
    }
    let result = timeout(NATIVE_DEADLINE, core.request(request.clone()))
        .await
        .map_err(|_| unavailable("native request deadline elapsed"))
        .and_then(|r| r)
        .and_then(|response| {
            response.validate_for(&request)?;
            Ok(response)
        });
    let response = match result {
        Ok(response) => response,
        Err(error) => BridgeResponse {
            version: zbridge::VERSION,
            id: request.id,
            generation: request.operation.generation().cloned(),
            body: ResponseBody::Error(if command { error.uncertain() } else { error }),
        },
    };
    Ok(Json(response))
}

async fn events(
    State(state): State<Arc<Gateway>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let input: ObserveRequest = input(request).await?;
    let (core, _) = state.connection(&id)?;
    let mut observer = EventObserver::new(input.generation.clone(), input.cursor);
    let page = observer
        .wait_with(
            input.request_id.clone(),
            input.limit,
            input.wait_ms,
            |request| core.request(request),
        )
        .await?;
    let reply = json!({"schema":"ssmcp-gui-events-v1","request_id":input.request_id,"generation":input.generation,"page":page});
    zbridge::encoded_len(&reply, zbridge::MAX_FRAME_BYTES)?;
    Ok(Json(reply))
}

async fn close(
    State(state): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let connection = {
        let mut data = state.data.lock().unwrap_or_else(|e| e.into_inner());
        let connection = data.connections.get_mut(&id).map(|connection| {
            connection.closing = true;
            connection.used = Instant::now();
            connection.core.clone()
        });
        if connection.is_none()
            && !data
                .opens
                .iter()
                .any(|r| r.connection_id == id && matches!(r.outcome, OpenOutcome::Connected))
        {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "no retained detach acknowledgement for this connection",
            )
            .into());
        }
        connection
    };
    let duplicate = connection.is_none();
    if let Some(connection) = connection {
        timeout(NATIVE_DEADLINE, connection.close())
            .await
            .map_err(|_| unavailable("detach acknowledgement deadline elapsed").uncertain())?
            .map_err(BridgeError::uncertain)?;
        state.acknowledge_close(&id);
    }
    Ok(Json(
        json!({"closed":true,"session_stopped":false,"duplicate":duplicate}),
    ))
}

async fn magisk_index() -> axum::response::Html<String> {
    axum::response::Html(include_str!("../../../apps/ssmcp-web/index.html").replacen(
        "<html ",
        "<html data-ssmcp-magisk ",
        1,
    ))
}

async fn index(State(state): State<Arc<Gateway>>) -> Response {
    serve_asset(state, "index.html".into()).await
}
async fn asset(State(state): State<Arc<Gateway>>, Path(path): Path<String>) -> Response {
    serve_asset(state, path).await
}
async fn serve_asset(state: Arc<Gateway>, path: String) -> Response {
    if path.len() > 512
        || path.split('/').any(|part| {
            part.is_empty()
                || part.starts_with('.')
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mime = match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "webmanifest" => "application/manifest+json",
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let Some(root) = &state.assets else {
        // Only runtime assets ship over HTTP. Tests and the development proxy stay private.
        macro_rules! assets {
            ($($file:literal),+ $(,)?) => {
                match path.as_str() {
                    $($file => include_str!(concat!("../../../apps/ssmcp-web/", $file)),)+
                    _ => return StatusCode::NOT_FOUND.into_response(),
                }
            };
        }
        let body = assets!(
            "index.html",
            "styles.css",
            "app.mjs",
            "src/client.mjs",
            "src/gateway.mjs",
            "src/presentation.mjs",
            "src/protocol.mjs",
            "src/store.mjs",
            "src/wait-view.mjs",
            "src/waits.mjs",
        );
        return ([(header::CONTENT_TYPE, mime)], body).into_response();
    };
    let result = async {
        let target = tokio::fs::canonicalize(root.join(path)).await.ok()?;
        if !target.starts_with(root) {
            return None;
        }
        let file = tokio::fs::File::open(target).await.ok()?;
        let metadata = file.metadata().await.ok()?;
        if !metadata.is_file() || metadata.len() > MAX_ASSET_BYTES {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_ASSET_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            return None;
        }
        Some(bytes)
    };
    match timeout(Duration::from_secs(5), result).await {
        Ok(Some(bytes)) => ([(header::CONTENT_TYPE, mime)], Body::from(bytes)).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
#[path = "gateway/tests.rs"]
mod tests;
