//! Account credentials and outbound WebSocket devices. Enrollment grants can only
//! create a controlled device; they never authenticate controller API calls.
use crate::{
    broker::{Authorization, Broker, IDENTITY_RESOURCE},
    identity::Identity,
    portal::Problem,
    wire::{self, Operation},
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{
        Query, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

pub(crate) const LOGIN_TTL: u64 = 30 * 24 * 3600;
const MAX_RECORDS: usize = 8192;
const MAX_DATABASE: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Login,
    Invite,
    Device,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    subject: String,
    kind: Kind,
    expires_at: u64,
    parent: Option<String>,
    session_ttl_secs: u64,
}

/// Hashed secrets only. The portal's existing exclusive writer lock also protects
/// this file; accounts.json remains unchanged for the SSH account synchronizer.
pub(crate) struct Credentials {
    directory: File,
    records: BTreeMap<String, Record>,
    failed: bool,
}

fn now() -> u64 {
    crate::management::now()
}
fn hash(raw: &str) -> String {
    Sha256::digest(raw.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn secret(prefix: &str) -> Result<String, Problem> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Problem::Storage)?;
    Ok(format!(
        "{prefix}{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}
fn valid_hash(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn valid_secret(raw: &str) -> bool {
    ["cl_", "ci_", "cd_"]
        .iter()
        .any(|p| raw.strip_prefix(p).is_some_and(valid_hash))
}
impl Credentials {
    pub(crate) fn open(path: &Path) -> crate::Result<Self> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()));
        let records = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(anchored.join("client-credentials.json"))
        {
            Ok(file) => {
                let m = file.metadata()?;
                // SAFETY: geteuid has no preconditions.
                if !m.is_file()
                    || m.uid() != unsafe { libc::geteuid() }
                    || m.mode() & 0o777 != 0o600
                    || m.nlink() != 1
                    || m.len() > MAX_DATABASE
                {
                    return Err("client credentials must be a bounded, service-owned, singly linked 0600 file".into());
                }
                let mut bytes = Vec::new();
                file.take(MAX_DATABASE + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 > MAX_DATABASE {
                    return Err("client credentials file is too large".into());
                }
                serde_json::from_slice::<BTreeMap<String, Record>>(&bytes)
                    .map_err(|_| "invalid client credentials file")?
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error.into()),
        };
        if records.len() > MAX_RECORDS
            || records.iter().any(|(key, r)| {
                !valid_hash(key)
                    || crate::portal::store::stable_username(&r.subject).is_none()
                    || (r.kind == Kind::Login && (r.parent.is_some() || r.session_ttl_secs != 0))
                    || (r.kind != Kind::Login && r.parent.as_deref().is_none_or(|p| !valid_hash(p)))
                    || (r.kind == Kind::Invite && !(1..=86400).contains(&r.session_ttl_secs))
                    || (r.kind == Kind::Device && r.session_ttl_secs != 0)
            })
        {
            return Err("invalid client credential records".into());
        }
        Ok(Self {
            directory,
            records,
            failed: false,
        })
    }
    fn save(&mut self, next: BTreeMap<String, Record>) -> Result<(), Problem> {
        if self.failed {
            return Err(Problem::Storage);
        }
        let data = serde_json::to_vec(&next).map_err(|_| Problem::Storage)?;
        if data.len() as u64 > MAX_DATABASE {
            return Err(Problem::Limit);
        }
        let dir = PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd()));
        let temporary = dir.join(secret(".client-")?);
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.write_all(&data)?;
            file.sync_all()?;
            std::fs::rename(&temporary, dir.join("client-credentials.json"))?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
            self.failed = true;
            return Err(Problem::Storage);
        }
        self.records = next;
        Ok(())
    }
    fn live(&self, key: &str) -> Option<&Record> {
        if self.failed {
            return None;
        }
        let record = self.records.get(key)?;
        if record.expires_at <= now() {
            return None;
        }
        if let Some(parent) = &record.parent {
            let parent = self.records.get(parent)?;
            if parent.kind != Kind::Login
                || parent.subject != record.subject
                || parent.expires_at <= now()
            {
                return None;
            }
        }
        Some(record)
    }
    fn pruned(&self) -> BTreeMap<String, Record> {
        self.records
            .iter()
            .filter(|(k, _)| self.live(k).is_some())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
    fn issue(
        &mut self,
        subject: String,
        kind: Kind,
        parent: Option<String>,
        ttl: u64,
        session_ttl_secs: u64,
    ) -> Result<(String, u64), Problem> {
        let mut next = self.pruned();
        if next.len() >= MAX_RECORDS
            || next
                .values()
                .filter(|r| r.subject == subject && r.kind == kind)
                .count()
                >= if kind == Kind::Login { 32 } else { 128 }
        {
            return Err(Problem::Busy);
        }
        let expires_at = now().checked_add(ttl).ok_or(Problem::Invalid)?;
        let expires_at = match &parent {
            Some(key) => expires_at.min(
                self.live(key)
                    .filter(|r| r.kind == Kind::Login && r.subject == subject)
                    .ok_or(Problem::Unauthorized)?
                    .expires_at,
            ),
            None => expires_at,
        };
        let raw = secret(match kind {
            Kind::Login => "cl_",
            Kind::Invite => "ci_",
            Kind::Device => "cd_",
        })?;
        let key = hash(&raw);
        if next.contains_key(&key) {
            return Err(Problem::Storage);
        }
        next.insert(
            key,
            Record {
                subject,
                kind,
                expires_at,
                parent,
                session_ttl_secs,
            },
        );
        self.save(next)?;
        Ok((raw, expires_at.saturating_sub(now())))
    }
    pub(crate) fn login(&mut self, subject: String) -> Result<(String, u64), Problem> {
        self.issue(subject, Kind::Login, None, LOGIN_TTL, 0)
    }
    fn authenticate(&self, raw: &str, controller: bool) -> Result<(String, Record), Problem> {
        if !valid_secret(raw) {
            return Err(Problem::Unauthorized);
        }
        let key = hash(raw);
        let record = self.live(&key).ok_or(Problem::Unauthorized)?;
        if record.kind == Kind::Invite || (controller && record.kind != Kind::Login) {
            return Err(Problem::Unauthorized);
        }
        Ok((key, record.clone()))
    }
    fn invite(&mut self, raw: &str, ttl: u64, session_ttl: u64) -> Result<(String, u64), Problem> {
        let (parent, record) = self.authenticate(raw, true)?;
        self.issue(record.subject, Kind::Invite, Some(parent), ttl, session_ttl)
    }
    fn enroll(&mut self, raw: &str) -> Result<(String, u64, String), Problem> {
        if !valid_secret(raw) {
            return Err(Problem::Unauthorized);
        }
        let key = hash(raw);
        let record = self
            .live(&key)
            .filter(|r| r.kind == Kind::Invite)
            .cloned()
            .ok_or(Problem::Unauthorized)?;
        let parent = record
            .parent
            .as_ref()
            .and_then(|k| self.live(k))
            .ok_or(Problem::Unauthorized)?;
        let expires_at = now()
            .saturating_add(record.session_ttl_secs)
            .min(parent.expires_at);
        let device = secret("cd_")?;
        let mut next = self.pruned();
        next.remove(&key);
        let device_key = hash(&device);
        if next.contains_key(&device_key) {
            return Err(Problem::Storage);
        }
        next.insert(
            device_key,
            Record {
                subject: record.subject.clone(),
                kind: Kind::Device,
                expires_at,
                parent: record.parent,
                session_ttl_secs: 0,
            },
        );
        self.save(next)?;
        Ok((device, expires_at.saturating_sub(now()), record.subject))
    }
    fn logout(&mut self, raw: &str) -> Result<(), Problem> {
        let (key, _) = self.authenticate(raw, true)?;
        let mut next = self.pruned();
        next.retain(|k, r| k != &key && r.parent.as_ref() != Some(&key));
        self.save(next)
    }
}

#[derive(Clone)]
pub(crate) struct Registration {
    pub identity: Identity,
    pub username: String,
    key: String,
}

impl Registration {
    pub(crate) async fn is_live(&self, broker: &Broker) -> bool {
        let key = self.key.clone();
        broker
            .portal
            .client_storage(move |store| Ok(store.live(&key).is_some()))
            .await
            == Ok(true)
    }
    pub(crate) async fn wait_revoked(&self, broker: &Broker) {
        loop {
            if !self.is_live(broker).await {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}
pub(crate) fn identity_json(identity: &Identity) -> Value {
    json!({"provider":identity.provider,"subject":identity.subject,"login":identity.login,"principal":format!("{}@github",identity.login.to_ascii_lowercase())})
}
/// Keep handler errors small; allocate the HTTP response only at the boundary.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error":self.code}))).into_response()
    }
}
fn fail(status: StatusCode, code: &'static str) -> ApiError {
    ApiError { status, code }
}
fn problem(error: Problem) -> ApiError {
    let code = match error {
        Problem::Unauthorized => "invalid_token",
        Problem::Busy => "rate_limited",
        Problem::Invalid => "invalid_request",
        _ => "temporarily_unavailable",
    };
    fail(error.status(), code)
}
fn bearer(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .filter(|s| valid_secret(s))
        .ok_or_else(|| fail(StatusCode::UNAUTHORIZED, "invalid_token"))?;
    if values.next().is_some() {
        return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
    }
    Ok(value.into())
}
fn origin(headers: &HeaderMap, broker: &Broker) -> Result<(), ApiError> {
    let mut values = headers.get_all(header::ORIGIN).iter();
    if let Some(v) = values.next()
        && (v.to_str().ok() != Some(broker.public_url.trim_end_matches('/'))
            || values.next().is_some())
    {
        return Err(fail(StatusCode::FORBIDDEN, "invalid_origin"));
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|v| v != "same-origin" && v != "none")
    {
        return Err(fail(StatusCode::FORBIDDEN, "invalid_origin"));
    }
    Ok(())
}
async fn authenticate(
    broker: &Broker,
    headers: &HeaderMap,
    controller: bool,
) -> Result<Registration, ApiError> {
    origin(headers, broker)?;
    if !broker.portal.enabled(broker) {
        return Err(fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "client_login_unavailable",
        ));
    }
    let raw = bearer(headers)?;
    let (key, record) = broker
        .portal
        .client_storage(move |s| s.authenticate(&raw, controller))
        .await
        .map_err(problem)?;
    let subject = record.subject;
    let account = broker
        .portal
        .storage(move |s| s.account(&subject))
        .await
        .map_err(problem)?;
    Ok(Registration {
        identity: Identity {
            provider: "github".into(),
            subject: account.subject,
            login: account.github_login,
        },
        username: account.username,
        key,
    })
}
async fn body<T: serde::de::DeserializeOwned>(
    request: Request,
    broker: &Broker,
) -> Result<T, ApiError> {
    origin(request.headers(), broker)?;
    let mut values = request.headers().get_all(header::CONTENT_TYPE).iter();
    if values
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        != Some("application/json")
        || values.next().is_some()
    {
        return Err(fail(StatusCode::BAD_REQUEST, "json_required"));
    }
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), 128 * 1024),
    )
    .await
    .map_err(|_| fail(StatusCode::REQUEST_TIMEOUT, "request_timeout"))?
    .map_err(|_| fail(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large"))?;
    serde_json::from_slice(&bytes).map_err(|_| fail(StatusCode::BAD_REQUEST, "invalid_request"))
}
pub fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/api/client/whoami", get(whoami))
        .route("/api/client/logout", post(logout))
        .route("/api/client/sessions", get(sessions))
        .route("/api/client/invites", post(invite))
        .route("/api/client/enroll", post(enroll))
        .route("/api/client/connect", get(connect))
        .route("/api/client/call", post(call_tool))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct VerifyIdentity {
    #[serde(default)]
    verify_github: bool,
}
async fn whoami(
    State(broker): State<Arc<Broker>>,
    Query(options): Query<VerifyIdentity>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, &headers, true).await?;
    if options.verify_github {
        let principal = format!(
            "{}@github",
            registration.identity.login.to_ascii_lowercase()
        );
        let resolved = broker.identities.resolve_allow(&[principal]).await;
        if !resolved.is_ok_and(|identities| {
            identities.len() == 1
                && identities[0].provider == registration.identity.provider
                && identities[0].subject == registration.identity.subject
        }) {
            return Err(fail(StatusCode::CONFLICT, "github_identity_unverified"));
        }
        if !registration.is_live(&broker).await {
            return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
        }
    }
    Ok(Json(
        json!({"identity":identity_json(&registration.identity)}),
    ))
}
async fn logout(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    authenticate(&broker, &headers, true).await?;
    let raw = bearer(&headers)?;
    broker
        .portal
        .client_storage(move |s| s.logout(&raw))
        .await
        .map_err(problem)?;
    Ok(Json(json!({"status":"logged_out"})))
}
async fn sessions(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, &headers, true).await?;
    let authorization = Authorization::Identity(registration.identity);
    let sessions = broker
        .clients(IDENTITY_RESOURCE, &authorization)
        .await
        .map_err(|_| fail(StatusCode::FORBIDDEN, "access_denied"))?;
    let clients:Vec<Value>=sessions.into_iter().map(|s|{let i=s.info();json!({"id":i.id,"name":i.name,"title":i.title,"type":i.mode.name(),"hostname":i.hostname,"cwd":i.cwd})}).collect();
    Ok(Json(json!({"clients":clients})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invite {
    #[serde(default = "invite_ttl")]
    ttl_secs: u64,
    #[serde(default = "session_ttl")]
    session_ttl_secs: u64,
}
fn invite_ttl() -> u64 {
    600
}
fn session_ttl() -> u64 {
    3600
}
async fn invite(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    authenticate(&broker, request.headers(), true).await?;
    let raw = bearer(request.headers())?;
    let options: Invite = body(request, &broker).await?;
    if !(1..=3600).contains(&options.ttl_secs) || !(1..=86400).contains(&options.session_ttl_secs) {
        return Err(fail(StatusCode::BAD_REQUEST, "invalid_lifetime"));
    }
    let (token, expires) = broker
        .portal
        .client_storage(move |s| s.invite(&raw, options.ttl_secs, options.session_ttl_secs))
        .await
        .map_err(problem)?;
    Ok(Json(
        json!({"token":token,"expires_in":expires,"session_expires_in":options.session_ttl_secs}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Enroll {
    token: String,
}
async fn enroll(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    if !broker.portal.enabled(&broker) {
        return Err(fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "client_login_unavailable",
        ));
    }
    let input: Enroll = body(request, &broker).await?;
    let (token, expires, subject) = broker
        .portal
        .client_storage(move |s| s.enroll(&input.token))
        .await
        .map_err(problem)?;
    let identity = broker
        .portal
        .identity_for_subject(subject)
        .await
        .map_err(problem)?;
    Ok(Json(
        json!({"device_token":token,"principal":format!("{}@github",identity.login.to_ascii_lowercase()),"expires_in":expires}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolCall {
    client_id: String,
    name: String,
    #[serde(default)]
    arguments: Value,
}
async fn call_tool(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, request.headers(), true).await?;
    let input: ToolCall = body(request, &broker).await?;
    if input.client_id.len() != 64 || input.name.is_empty() || input.name.len() > 512 {
        return Err(fail(StatusCode::BAD_REQUEST, "invalid_request"));
    }
    let authorization = Authorization::Identity(registration.identity.clone());
    let session = broker
        .client(IDENTITY_RESOURCE, &authorization, &input.client_id)
        .await
        .map_err(|_| fail(StatusCode::NOT_FOUND, "client_unavailable"))?;
    // Reading a request body can take several seconds. Revalidate after that
    // await and session lookup, before any native operation is dispatched.
    if !registration.is_live(&broker).await {
        return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
    }
    let result=tokio::select!{
        result=session.call_with_authorization(Operation::CallTool{name:input.name,arguments:input.arguments},None,&authorization)=>result,
        _=registration.wait_revoked(&broker)=>return Err(fail(StatusCode::UNAUTHORIZED,"invalid_token")),
    }.map_err(|_|fail(StatusCode::BAD_GATEWAY,"client_call_failed"))?;
    Ok(Json(result))
}
async fn connect(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let registration = authenticate(&broker, &headers, false).await?;
    Ok(ws
        .max_message_size(wire::MAX_FRAME)
        .max_frame_size(wire::MAX_FRAME)
        .on_upgrade(move |socket| socket_bridge(broker, registration, socket)))
}
async fn socket_bridge(broker: Arc<Broker>, registration: Registration, socket: WebSocket) {
    let (transport, stream) = tokio::io::duplex(256 * 1024);
    let (input, mut output) = tokio::io::split(transport);
    let mut input = tokio::io::BufReader::new(input);
    let (mut sender, mut receiver) = socket.split();
    let inbound = async move {
        while let Some(Ok(message)) = receiver.next().await {
            let data = match message {
                Message::Binary(data) => data,
                Message::Text(text) => text.as_bytes().to_vec().into(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(frame) = serde_json::from_slice::<wire::Frame>(&data) else {
                break;
            };
            if wire::write_frame(&mut output, &frame).await.is_err() {
                break;
            }
        }
        let _ = output.shutdown().await;
    };
    let outbound = async move {
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        loop {
            let next = wire::read_frame(&mut input);
            tokio::pin!(next);
            let frame = loop {
                tokio::select! {
                    frame=&mut next=>break frame,
                    _=ping.tick()=>if sender.send(Message::Ping(Vec::new().into())).await.is_err(){return;},
                }
            };
            match frame {
                Ok(Some(frame)) => {
                    let Ok(mut data) = serde_json::to_vec(&frame) else {
                        break;
                    };
                    data.push(b'\n');
                    if sender.send(Message::Binary(data.into())).await.is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        let _ = sender.close().await;
    };
    let pumps = async move {
        tokio::select! {_=inbound=>{},_=outbound=>{}}
    };
    tokio::pin!(pumps);
    let mut session = tokio::spawn(broker.attach_device(stream, registration));
    tokio::select! {
        _=&mut session=>{
            // Flush the final authorization rejection before closing the socket.
            let _=tokio::time::timeout(Duration::from_secs(2), &mut pumps).await;
        },
        _=&mut pumps=>{
            if !session.is_finished() {let _=session.await;}
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Temporary(PathBuf);
    impl Temporary {
        fn new() -> Self {
            let p = std::env::temp_dir().join(secret("ssmcp-client-auth-").unwrap());
            std::fs::create_dir(&p).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(p)
        }
    }
    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn credentials_persist_as_hashes_and_enrollment_is_single_use_and_device_only() {
        let directory = Temporary::new();
        let mut store = Credentials::open(&directory.0).unwrap();
        let (login, _) = store.login("123".into()).unwrap();
        let (invite, _) = store.invite(&login, 600, 3600).unwrap();
        assert!(store.authenticate(&invite, true).is_err());
        assert!(store.authenticate(&invite, false).is_err());
        let (device, ttl, subject) = store.enroll(&invite).unwrap();
        assert_eq!(subject, "123");
        assert!((3598..=3600).contains(&ttl));
        assert!(store.enroll(&invite).is_err());
        assert!(store.authenticate(&device, true).is_err());
        assert!(store.authenticate(&device, false).is_ok());
        let disk = std::fs::read_to_string(directory.0.join("client-credentials.json")).unwrap();
        for raw in [&login, &invite, &device] {
            assert!(!disk.contains(raw));
        }
        assert_eq!(
            std::fs::metadata(directory.0.join("client-credentials.json"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        drop(store);
        let mut store = Credentials::open(&directory.0).unwrap();
        assert!(store.authenticate(&login, true).is_ok());
        assert!(store.authenticate(&device, false).is_ok());
        store.logout(&login).unwrap();
        assert!(store.authenticate(&login, true).is_err());
        assert!(store.authenticate(&device, false).is_err());
        drop(store);
        assert!(
            Credentials::open(&directory.0)
                .unwrap()
                .authenticate(&device, false)
                .is_err()
        );
    }

    #[test]
    fn expiry_parent_revocation_and_credential_files_fail_closed() {
        let directory = Temporary::new();
        let mut store = Credentials::open(&directory.0).unwrap();
        let (alice, _) = store.login("123".into()).unwrap();
        let (bob, _) = store.login("456".into()).unwrap();
        let (invite, _) = store.invite(&alice, 600, 3600).unwrap();
        store.records.get_mut(&hash(&invite)).unwrap().expires_at = now() - 1;
        assert!(store.enroll(&invite).is_err());
        let (invite, _) = store.invite(&alice, 600, 3600).unwrap();
        store.logout(&alice).unwrap();
        assert!(store.enroll(&invite).is_err());
        assert!(store.authenticate(&bob, true).is_ok());
        drop(store);
        std::fs::set_permissions(
            directory.0.join("client-credentials.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(Credentials::open(&directory.0).is_err());
    }

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    async fn receive(socket: &mut Socket) -> Option<wire::Frame> {
        loop {
            match tokio::time::timeout(Duration::from_secs(4), socket.next())
                .await
                .ok()??
                .ok()?
            {
                tokio_tungstenite::tungstenite::Message::Text(data) => {
                    return serde_json::from_slice(data.as_bytes()).ok();
                }
                tokio_tungstenite::tungstenite::Message::Binary(data) => {
                    return serde_json::from_slice(&data).ok();
                }
                tokio_tungstenite::tungstenite::Message::Close(_) => return None,
                _ => {}
            }
        }
    }
    async fn open_socket(base: &str, token: &str, allow: Vec<String>) -> Socket {
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let hello = wire::Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: wire::ClientMetadata {
                name: "borrowed-device".into(),
                title: None,
                cwd: "/tmp".into(),
                hostname: "device-b".into(),
                allow,
            },
        };
        socket
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap().into()))
            .await
            .unwrap();
        socket
    }

    #[tokio::test]
    async fn websocket_registration_enforces_owner_acl_and_logout_disconnects() {
        let directory = Temporary::new();
        let mut broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(crate::identity::GitHubConfig {
                client_id: "test-client".into(),
                client_secret: "test-secret".into(),
            }),
        )
        .unwrap();
        broker.portal = crate::portal::Portal::open(&directory.0).unwrap();
        broker
            .portal
            .storage(|store| {
                for (subject, login) in [("123", "alice"), ("456", "bob")] {
                    store.login(&Identity {
                        provider: "github".into(),
                        subject: subject.into(),
                        login: login.into(),
                    })?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let alice = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        let bob = broker
            .portal
            .client_storage(|store| store.login("456".into()))
            .await
            .unwrap()
            .0;
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/api/client/invites"))
            .bearer_auth(&alice)
            .json(&json!({"ttl_secs":600,"session_ttl_secs":3600}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let invitation: Value = response.json().await.unwrap();
        let raw = invitation["token"].as_str().unwrap();
        let response = client
            .post(format!("{base}/api/client/enroll"))
            .json(&json!({"token":raw}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let enrolled: Value = response.json().await.unwrap();
        let device = enrolled["device_token"].as_str().unwrap();
        let response = client
            .post(format!("{base}/api/client/enroll"))
            .json(&json!({"token":raw}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        for endpoint in ["whoami", "sessions"] {
            let response = client
                .get(format!("{base}/api/client/{endpoint}"))
                .bearer_auth(device)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
        }
        let response = client
            .post(format!("{base}/api/client/invites"))
            .bearer_auth(device)
            .json(&json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        let mut invalid = open_socket(&base, device, vec!["bob@github".into()]).await;
        assert!(matches!(
            receive(&mut invalid).await,
            Some(wire::Frame::Reject { .. })
        ));
        let mut socket = open_socket(&base, device, vec![]).await;
        let id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Ready {
                client_id,
                login_code,
                login_expires_in,
                ..
            } => {
                assert!(login_code.is_empty());
                assert_eq!(login_expires_in, 0);
                client_id
            }
            _ => panic!("expected ready"),
        };
        let alice_list: Value = client
            .get(format!("{base}/api/client/sessions"))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(alice_list["clients"].as_array().unwrap().len(), 1);
        assert_eq!(alice_list["clients"][0]["id"], id);
        let bob_list: Value = client
            .get(format!("{base}/api/client/sessions"))
            .bearer_auth(&bob)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(bob_list["clients"].as_array().unwrap().is_empty());
        let response = client
            .post(format!("{base}/api/client/call"))
            .bearer_auth(&bob)
            .json(&json!({"client_id":id,"name":"terminal.status","arguments":{}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        let request = client
            .post(format!("{base}/api/client/call"))
            .bearer_auth(&alice)
            .json(&json!({"client_id":id,"name":"terminal.status","arguments":{}}))
            .send();
        let responder = async {
            let frame = receive(&mut socket).await.unwrap();
            let wire::Frame::Call {
                id: call_id,
                operation: Operation::CallTool { name, .. },
                ..
            } = frame
            else {
                panic!("expected call")
            };
            assert_eq!(name, "terminal.status");
            socket
                .send(tokio_tungstenite::tungstenite::Message::Binary(
                    serde_json::to_vec(&wire::Frame::Reply {
                        id: call_id,
                        result: Ok(json!({"content":[{"type":"text","text":"ok"}]})),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
        };
        let (response, _) = tokio::join!(request, responder);
        assert_eq!(response.unwrap().status(), 200);

        let slow_token = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        let payload =
            serde_json::to_vec(&json!({"client_id":id,"name":"terminal.status","arguments":{}}))
                .unwrap();
        let (body_started, started) = tokio::sync::oneshot::channel();
        let (finish_body, remaining) = tokio::sync::oneshot::channel::<Vec<u8>>();
        let stream = futures_util::stream::once(async move {
            let _ = body_started.send(());
            remaining
                .await
                .map(axum::body::Bytes::from)
                .map_err(std::io::Error::other)
        });
        let slow_request = Request::builder()
            .method("POST")
            .uri("/api/client/call")
            .header("authorization", format!("Bearer {slow_token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from_stream(stream))
            .unwrap();
        let mut uploading = tokio::spawn(call_tool(State(broker.clone()), slow_request));
        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            client
                .post(format!("{base}/api/client/logout"))
                .bearer_auth(&slow_token)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        finish_body.send(payload).unwrap();
        let response = tokio::select! {
            result = &mut uploading => result.unwrap(),
            frame = receive(&mut socket) => panic!("revoked slow-body request reached device: {}", matches!(frame, Some(wire::Frame::Call{..}))),
        };
        assert_eq!(
            response.unwrap_err().into_response().status(),
            StatusCode::UNAUTHORIZED
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), receive(&mut socket))
                .await
                .is_err()
        );
        assert_eq!(
            client
                .post(format!("{base}/api/client/logout"))
                .bearer_auth(&alice)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert!(
            matches!(receive(&mut socket).await,Some(wire::Frame::Reject{message}) if message.contains("revoked"))
        );
        let authorization = Authorization::Identity(Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        });
        assert!(
            broker
                .clients(IDENTITY_RESOURCE, &authorization)
                .await
                .unwrap()
                .is_empty()
        );
        server.abort();
    }
}
