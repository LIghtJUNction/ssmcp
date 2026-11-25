//! Account credentials and controlled WebSocket peers. Anonymous devices carry
//! explicit account ACLs; remote control still requires a verified login.
pub(crate) mod routing;
mod session_io;
use crate::{
    broker::{Authorization, Broker, IDENTITY_RESOURCE},
    identity::Identity,
    portal::Problem,
    user_policy::TrafficFailureState,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    oauth_parent: Option<crate::oauth::Delegation>,
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
    crate::hex::encode(Sha256::digest(raw.as_bytes()))
}
fn secret(prefix: &str) -> Result<String, Problem> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Problem::Storage)?;
    Ok(format!("{prefix}{}", crate::hex::encode(bytes)))
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
                    || (r.kind == Kind::Login
                        && (r.parent.is_some() || r.oauth_parent.is_some() || r.session_ttl_secs != 0))
                    || (r.kind != Kind::Login
                        && !matches!((&r.parent, &r.oauth_parent), (Some(parent), None) if valid_hash(parent))
                        && !matches!((&r.parent, &r.oauth_parent), (None, Some(parent))
                            if parent.valid_for(&r.subject) && r.expires_at <= parent.expires_at))
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
        if record.expires_at <= now()
            || record.oauth_parent.as_ref().is_some_and(|parent| {
                !parent.valid_for(&record.subject) || parent.expires_at <= now()
            })
        {
            return None;
        }
        // OAuth liveness is checked outside this ledger's lock by every
        // enrollment/registration boundary, never while saving credentials.
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
        oauth_parent: Option<crate::oauth::Delegation>,
        ttl: u64,
        session_ttl_secs: u64,
    ) -> Result<(String, u64), Problem> {
        if (kind == Kind::Login && (parent.is_some() || oauth_parent.is_some()))
            || (kind != Kind::Login && parent.is_some() == oauth_parent.is_some())
            || oauth_parent
                .as_ref()
                .is_some_and(|value| !value.valid_for(&subject))
        {
            return Err(Problem::Unauthorized);
        }
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
        let expires_at = oauth_parent
            .as_ref()
            .map_or(expires_at, |parent| expires_at.min(parent.expires_at));
        if expires_at <= now() {
            return Err(Problem::Unauthorized);
        }
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
                oauth_parent,
                session_ttl_secs,
            },
        );
        self.save(next)?;
        Ok((raw, expires_at.saturating_sub(now())))
    }
    pub(crate) fn login(&mut self, subject: String) -> Result<(String, u64), Problem> {
        self.issue(subject, Kind::Login, None, None, LOGIN_TTL, 0)
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
    #[cfg(test)]
    fn invite(&mut self, raw: &str, ttl: u64, session_ttl: u64) -> Result<(String, u64), Problem> {
        let (parent, record) = self.authenticate(raw, true)?;
        self.issue(
            record.subject,
            Kind::Invite,
            Some(parent),
            None,
            ttl,
            session_ttl,
        )
    }
    #[cfg(test)]
    fn invite_oauth(
        &mut self,
        parent: crate::oauth::Delegation,
        ttl: u64,
        session_ttl: u64,
    ) -> Result<(String, u64), Problem> {
        if !(1..=3600).contains(&ttl) || !(1..=86400).contains(&session_ttl) {
            return Err(Problem::Invalid);
        }
        self.issue(
            parent.subject.clone(),
            Kind::Invite,
            None,
            Some(parent),
            ttl,
            session_ttl,
        )
    }
    #[cfg(test)]
    fn invitation(&self, raw: &str) -> Result<Record, Problem> {
        if !valid_secret(raw) {
            return Err(Problem::Unauthorized);
        }
        self.live(&hash(raw))
            .filter(|record| record.kind == Kind::Invite)
            .cloned()
            .ok_or(Problem::Unauthorized)
    }
    #[cfg(test)]
    fn enroll(&mut self, raw: &str) -> Result<(String, u64, String), Problem> {
        let record = self.invitation(raw)?;
        let key = hash(raw);
        let parent_expires = match (&record.parent, &record.oauth_parent) {
            (Some(parent), None) => self.live(parent).ok_or(Problem::Unauthorized)?.expires_at,
            (None, Some(parent)) if parent.valid_for(&record.subject) => parent.expires_at,
            _ => return Err(Problem::Unauthorized),
        };
        let expires_at = now()
            .saturating_add(record.session_ttl_secs)
            .min(parent_expires);
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
                oauth_parent: record.oauth_parent,
                session_ttl_secs: 0,
            },
        );
        self.save(next)?;
        Ok((device, expires_at.saturating_sub(now()), record.subject))
    }
    pub(crate) fn revoke_subject(&mut self, subject: &str) -> Result<(), Problem> {
        let mut next = self.pruned();
        next.retain(|_, record| record.subject != subject);
        self.save(next)
    }
    fn logout(&mut self, raw: &str) -> Result<(), Problem> {
        let (key, _) = self.authenticate(raw, true)?;
        let mut next = self.pruned();
        next.retain(|k, r| k != &key && r.parent.as_ref() != Some(&key));
        self.save(next)
    }
}

/// One local controlled connection; account identity is optional display provenance.
pub(crate) struct DevicePeer {
    pub account: Option<Registration>,
    pub session_access: bool,
    pub control_attribution: bool,
    pub native_messages: bool,
    pub version_notice: bool,
    pub wait_display: Option<wire::wait_display::Version>,
    pub time_observation: bool,
    pub chat_disabled: bool,
    pub openai_context: bool,
    pub main_lifecycle: bool,
    pub worker_dreaming: bool,
    pub repository_events: bool,
    pub project_continuity: bool,
    pub peer_ports: bool,
    pub peer_ports_auto: bool,
}

impl From<Registration> for DevicePeer {
    fn from(account: Registration) -> Self {
        Self {
            session_access: account.session_access,
            control_attribution: account.control_attribution,
            native_messages: false,
            version_notice: false,
            wait_display: None,
            time_observation: false,
            chat_disabled: false,
            openai_context: false,
            main_lifecycle: false,
            worker_dreaming: false,
            repository_events: false,
            project_continuity: false,
            peer_ports: false,
            peer_ports_auto: false,
            account: Some(account),
        }
    }
}

#[derive(Clone)]
pub(crate) struct Registration {
    pub identity: Identity,
    pub device_identity: Option<wire::device_address::Identity>,
    pub username: String,
    /// Capability accepted from this authenticated login registration.
    pub session_access: bool,
    /// Verified caller attribution negotiated by this peer.
    pub control_attribution: bool,
    key: String,
}

impl Registration {
    pub(crate) async fn is_live(&self, broker: &Broker) -> bool {
        if broker.user_policies.is_banned(&self.identity.subject) {
            return false;
        }
        let subject = self.identity.subject.clone();
        if broker
            .portal
            .storage(move |store| store.account(&subject))
            .await
            .is_err()
        {
            return false;
        }
        let key = self.key.clone();
        let record = broker
            .portal
            .client_storage(move |store| Ok(store.live(&key).cloned()))
            .await;
        matches!(record, Ok(Some(record)) if record_authorized(broker, &record))
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
// Always call after releasing the credential ledger and account storage locks.
fn record_authorized(broker: &Broker, record: &Record) -> bool {
    !broker.user_policies.is_banned(&record.subject)
        && record
            .oauth_parent
            .as_ref()
            .is_none_or(|parent| broker.oauth.delegation_is_live(parent))
}

pub(crate) fn identity_json(identity: &Identity) -> Value {
    json!({"provider":identity.provider,"subject":identity.subject,"login":identity.login,"principal":format!("{}@github",identity.login.to_ascii_lowercase())})
}

/// Account names are claims of the verified subject, never alternate identities.
pub(crate) fn account_identity_json(
    identity: &Identity,
    account: &crate::portal::store::Account,
) -> Result<Value, Problem> {
    if identity.provider != "github" || identity.subject != account.subject {
        return Err(Problem::Unauthorized);
    }
    let mut public = identity_json(identity);
    public["username"] = json!(account.handle);
    public["username_aliases"] = json!(account.handle_aliases);
    public["username_generated"] = json!(account.generated_handle);
    Ok(public)
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
        Problem::UsernameInvalid => "invalid_username",
        Problem::UsernameTaken => "username_taken",
        Problem::UsernameBound => "username_bound",
        Problem::Banned => "account_banned",
        Problem::TrafficQuotaExceeded => "traffic_quota_exceeded",
        Problem::TrafficPeriodMismatch => "traffic_period_mismatch",
        Problem::TrafficGrantConflict => "traffic_grant_conflict",
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
    if !record_authorized(broker, &record) {
        return Err(problem(Problem::Unauthorized));
    }
    let subject = record.subject.clone();
    let account = broker
        .portal
        .storage(move |s| s.account(&subject))
        .await
        .map_err(problem)?;
    if !record_authorized(broker, &record) {
        return Err(problem(Problem::Unauthorized));
    }
    Ok(Registration {
        device_identity: None,
        identity: Identity {
            provider: "github".into(),
            subject: account.subject,
            login: account.github_login,
        },
        username: account.username,
        session_access: false,
        control_attribution: false,
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
        .route("/api/client/username", post(claim_username))
        .route("/api/client/logout", post(logout))
        .route("/api/client/sessions", get(sessions))
        .route("/api/client/connect", get(connect))
        .route(
            wire::device_address::CHALLENGE_PATH,
            post(routing::challenge),
        )
        .route("/api/client/call", post(call_tool))
        .route("/api/client/io", post(session_io::request))
}
pub(crate) fn relay_routes() -> Router<Arc<Broker>> {
    Router::new().route("/api/client/connect", get(connect))
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
    let subject = registration.identity.subject.clone();
    let account = broker
        .portal
        .storage(move |store| store.account(&subject))
        .await
        .map_err(problem)?;
    let identity = account_identity_json(&registration.identity, &account).map_err(problem)?;
    let traffic = broker
        .traffic_quota(&registration.identity.subject)
        .await
        .map_err(problem)?;
    Ok(Json(json!({"identity":identity,"traffic":traffic})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimUsername {
    username: String,
}

async fn claim_username(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, request.headers(), true).await?;
    let input: ClaimUsername = body(request, &broker).await?;
    if !registration.is_live(&broker).await {
        return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
    }
    let subject = registration.identity.subject.clone();
    let username = crate::portal::store::normalize_handle(&input.username).map_err(problem)?;
    let account = broker
        .portal
        .storage(move |store| store.claim_handle(&subject, &username))
        .await
        .map_err(problem)?;
    let identity = account_identity_json(&registration.identity, &account).map_err(problem)?;
    Ok(Json(json!({"identity":identity})))
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
    let actor = registration.identity.clone();
    let authorization = Authorization::Identity(actor.clone());
    let sessions = broker
        .clients(IDENTITY_RESOURCE, &authorization)
        .await
        .map_err(|_| fail(StatusCode::FORBIDDEN, "access_denied"))?;
    if !registration.is_live(&broker).await {
        return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
    }
    let clients: Vec<Value> = sessions
        .into_iter()
        .map(|s| session_io::directory(&broker, &s, &actor))
        .collect();
    Ok(Json(
        json!({"clients":clients,"device_sessions":broker.device_routes.inventory(IDENTITY_RESOURCE,&authorization)}),
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
fn session_access_requested(headers: &HeaderMap) -> bool {
    capability_requested(headers, wire::SESSION_ACCESS_CAPABILITY)
}

fn capability_requested(headers: &HeaderMap, wanted: &str) -> bool {
    let mut values = headers.get_all(wire::CAPABILITIES_HEADER).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    value.to_str().ok().is_some_and(|value| {
        value.len() <= 256
            && value
                .split(',')
                .any(|capability| capability.trim() == wanted)
    })
}

async fn connect(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    origin(&headers, &broker)?;
    if let Some(access) = &broker.relay_access {
        if !access.permit_ingress(&headers) || headers.contains_key(header::AUTHORIZATION) {
            return Err(problem(Problem::Unauthorized));
        }
    } else if !broker.portal.enabled(&broker) {
        return Err(fail(
            StatusCode::SERVICE_UNAVAILABLE,
            "client_login_unavailable",
        ));
    }
    // A malformed or revoked credential is never silently treated as anonymous.
    let mut account = if headers.contains_key(header::AUTHORIZATION) {
        Some(authenticate(&broker, &headers, true).await?)
    } else {
        None
    };
    let mut proofs = headers.get_all(wire::device_address::PROOF_HEADER).iter();
    if let Some(header) = proofs.next() {
        if proofs.next().is_some() {
            return Err(problem(Problem::Invalid));
        }
        let registration = account
            .as_mut()
            .ok_or_else(|| problem(Problem::Unauthorized))?;
        let proof = header.to_str().map_err(|_| problem(Problem::Invalid))?;
        registration.device_identity = Some(
            broker
                .device_routes
                .verify(registration, broker.public_url.trim_end_matches('/'), proof)
                .map_err(problem)?,
        );
    }
    let session_access = session_access_requested(&headers);
    let control_attribution = capability_requested(&headers, wire::CONTROL_ATTRIBUTION_CAPABILITY);
    if account.is_none() && (!session_access || !control_attribution) {
        return Err(fail(
            StatusCode::BAD_REQUEST,
            "anonymous_control_requires_current_client",
        ));
    }
    let native_messages = capability_requested(&headers, wire::messages::CAPABILITY);
    let peer = DevicePeer {
        account,
        session_access,
        control_attribution,
        native_messages,
        version_notice: capability_requested(&headers, wire::versions::CAPABILITY),
        wait_display: if capability_requested(&headers, wire::wait_display::DURABLE_CAPABILITY) {
            Some(wire::wait_display::Version::V4)
        } else if capability_requested(&headers, wire::wait_display::CAPABILITY) {
            Some(wire::wait_display::Version::V3)
        } else {
            None
        },
        time_observation: capability_requested(&headers, wire::wait_display::TIME_CAPABILITY),
        chat_disabled: capability_requested(&headers, wire::CHAT_DISABLED_CAPABILITY),
        peer_ports: capability_requested(&headers, wire::peer_port::CAPABILITY),
        peer_ports_auto: capability_requested(&headers, wire::peer_port::AUTO_CAPABILITY),
        openai_context: capability_requested(&headers, wire::openai::CAPABILITY),
        main_lifecycle: capability_requested(&headers, wire::MAIN_LIFECYCLE_CAPABILITY),
        project_continuity: {
            let mut values = headers
                .get_all(wire::project_continuity::OFFER_HEADER)
                .iter();
            values.next().is_some_and(|v| v.as_bytes() == b"1") && values.next().is_none()
        },
        worker_dreaming: capability_requested(&headers, wire::WORKER_DREAMING_CAPABILITY),
        repository_events: capability_requested(&headers, wire::repository_events::CAPABILITY),
    };
    let transport = broker
        .admit_transport()
        .ok_or_else(|| fail(StatusCode::SERVICE_UNAVAILABLE, "gateway_draining"))?;
    Ok(ws
        .max_message_size(wire::MAX_FRAME)
        .max_frame_size(wire::MAX_FRAME)
        .on_upgrade(move |socket| async move {
            let _transport = transport;
            socket_bridge(broker, peer, socket).await;
        }))
}

async fn socket_bridge(broker: Arc<Broker>, peer: DevicePeer, socket: WebSocket) {
    // The broker attributes request/reply frames to the authenticated controller.
    // A transport peer, Hello, Ping, or allow label is not a billable identity.
    let (transport, stream) = tokio::io::duplex(256 * 1024);
    let (input, mut output) = tokio::io::split(transport);
    let mut input = tokio::io::BufReader::new(input);
    let (mut sender, mut receiver) = socket.split();
    let pumps = async move {
        let inbound = async move {
            while let Ok(Some(Ok(message))) =
                tokio::time::timeout(Duration::from_secs(75), receiver.next()).await
            {
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
        let outbound = async {
            let mut ping = tokio::time::interval(Duration::from_secs(25));
            loop {
                let next = wire::read_frame(&mut input);
                tokio::pin!(next);
                let frame = loop {
                    tokio::select! {
                        frame = &mut next => break frame,
                        _ = ping.tick() => if !matches!(tokio::time::timeout(Duration::from_secs(15), sender.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) {
                            return;
                        },
                    }
                };
                match frame {
                    Ok(Some(frame)) => {
                        let Ok(mut data) = serde_json::to_vec(&frame) else {
                            break;
                        };
                        data.push(b'\n');
                        if !matches!(
                            tokio::time::timeout(
                                Duration::from_secs(15),
                                sender.send(Message::Binary(data.into()))
                            )
                            .await,
                            Ok(Ok(()))
                        ) {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        };
        tokio::select! { _ = inbound => {}, _ = outbound => {} }
        let _ = tokio::time::timeout(Duration::from_secs(2), sender.close()).await;
    };
    tokio::pin!(pumps);
    let mut session =
        tokio::spawn(broker.attach_device(stream, peer, TrafficFailureState::default()));
    tokio::select! {
        _ = &mut session => {
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut pumps).await;
        },
        _ = &mut pumps => {
            if !session.is_finished() { let _ = session.await; }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn account_identity_metadata_requires_the_same_verified_subject() {
        let account: crate::portal::store::Account = serde_json::from_value(json!({
            "subject":"123", "username":"gh-123", "github_login":"previous-login",
            "keys":[], "handle":"current-name", "handle_aliases":["former-name"],
            "generated_handle":false, "banned":false, "rate_limit_bps":1024
        }))
        .unwrap();
        let identity = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "Current-Login".into(),
        };
        assert_eq!(
            account_identity_json(&identity, &account).unwrap(),
            json!({
                "provider":"github", "subject":"123", "login":"Current-Login",
                "principal":"current-login@github", "username":"current-name",
                "username_aliases":["former-name"], "username_generated":false
            })
        );
        for (provider, subject) in [("github", "456"), ("other", "123")] {
            let unrelated = Identity {
                provider: provider.into(),
                subject: subject.into(),
                login: identity.login.clone(),
            };
            assert_eq!(
                account_identity_json(&unrelated, &account),
                Err(Problem::Unauthorized)
            );
        }
    }

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

    #[tokio::test]
    async fn platform_rename_preserves_existing_login_and_device_credentials_and_bans() {
        let directory = Temporary::new();
        let mut broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture".into(),
            }),
        )
        .unwrap();
        broker.portal = crate::portal::Portal::open(&directory.0).unwrap();
        broker
            .portal
            .storage(|store| {
                store.login(&Identity {
                    provider: "github".into(),
                    subject: "123".into(),
                    login: "upstream".into(),
                })?;
                store.claim_handle("123", "alice-home")
            })
            .await
            .unwrap();
        let (login, device) = broker
            .portal
            .client_storage(|store| {
                let (login, _) = store.login("123".into())?;
                let (invite, _) = store.invite(&login, 600, 3600)?;
                let (device, _, _) = store.enroll(&invite)?;
                Ok((login, device))
            })
            .await
            .unwrap();
        let mut login_headers = HeaderMap::new();
        login_headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {login}").parse().unwrap(),
        );
        let mut device_headers = HeaderMap::new();
        device_headers.insert(
            header::AUTHORIZATION,
            format!("Bearer {device}").parse().unwrap(),
        );
        let held_login = authenticate(&broker, &login_headers, true).await.unwrap();
        let held_device = authenticate(&broker, &device_headers, false).await.unwrap();
        broker
            .portal
            .storage(|store| store.rename_handle("123", "alice-work"))
            .await
            .unwrap();
        assert!(held_login.is_live(&broker).await);
        assert!(held_device.is_live(&broker).await);
        for registration in [
            authenticate(&broker, &login_headers, true).await.unwrap(),
            authenticate(&broker, &device_headers, false).await.unwrap(),
        ] {
            assert_eq!(registration.identity.provider, "github");
            assert_eq!(registration.identity.subject, "123");
            assert_eq!(registration.username, "gh-123");
        }
        broker
            .portal
            .storage(|store| {
                assert_eq!(
                    store.account_by_handle("alice-home")?.unwrap().subject,
                    "123"
                );
                assert_eq!(
                    store.account_by_handle("alice-work")?.unwrap().subject,
                    "123"
                );
                store.set_policy("123", Some(true), None)
            })
            .await
            .unwrap();
        assert!(!held_login.is_live(&broker).await);
        assert!(!held_device.is_live(&broker).await);
        assert!(authenticate(&broker, &login_headers, true).await.is_err());
        assert!(authenticate(&broker, &device_headers, false).await.is_err());
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
        let (alice_other, _) = store.login("123".into()).unwrap();
        let (bob, _) = store.login("456".into()).unwrap();
        let (other_invite, _) = store.invite(&alice_other, 600, 3600).unwrap();
        let (invite, _) = store.invite(&alice, 600, 3600).unwrap();
        store.records.get_mut(&hash(&invite)).unwrap().expires_at = now() - 1;
        assert!(store.enroll(&invite).is_err());
        let (invite, _) = store.invite(&alice, 600, 3600).unwrap();
        store.logout(&alice).unwrap();
        assert!(store.enroll(&invite).is_err());
        assert!(store.authenticate(&alice_other, true).is_ok());
        assert!(store.authenticate(&bob, true).is_ok());
        assert!(store.enroll(&other_invite).is_ok());
        assert!(store.logout(&alice).is_err());
        drop(store);
        let reopened = Credentials::open(&directory.0).unwrap();
        assert!(reopened.authenticate(&alice, true).is_err());
        assert!(reopened.authenticate(&alice_other, true).is_ok());
        assert!(reopened.authenticate(&bob, true).is_ok());
        drop(reopened);
        std::fs::set_permissions(
            directory.0.join("client-credentials.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(Credentials::open(&directory.0).is_err());
    }

    #[test]
    fn oauth_invitation_ledger_has_no_controller_credential_and_keeps_parent_deadline() {
        let directory = Temporary::new();
        let oauth = crate::oauth::OAuth::new("https://ssmcp.test".into());
        let tokens = oauth.test_identity_tokens("mcp", "123");
        let access = tokens["access_token"].as_str().unwrap();
        let (parent, _) = oauth.delegation("mcp", access).unwrap();
        let deadline = parent.expires_at;
        let mut store = Credentials::open(&directory.0).unwrap();
        let (invite, _) = store.invite_oauth(parent, 600, 86400).unwrap();
        assert_eq!(store.records.len(), 1);
        assert!(
            store
                .records
                .values()
                .all(|record| record.kind != Kind::Login)
        );
        let (device, ttl, subject) = store.enroll(&invite).unwrap();
        assert_eq!(subject, "123");
        assert!(ttl <= 8 * 3600);
        assert!(store.enroll(&invite).is_err());
        assert!(store.authenticate(&device, true).is_err());
        let (_, record) = store.authenticate(&device, false).unwrap();
        assert_eq!(record.expires_at, deadline);
        assert!(record.parent.is_none());
        assert!(oauth.delegation_is_live(record.oauth_parent.as_ref().unwrap()));
        let disk = std::fs::read_to_string(directory.0.join("client-credentials.json")).unwrap();
        for raw in [access, invite.as_str(), device.as_str()] {
            assert!(!disk.contains(raw));
        }
        assert!(!disk.contains("cl_"));
        drop(store);
        let reopened = Credentials::open(&directory.0).unwrap();
        assert!(reopened.authenticate(&device, false).is_ok());
        oauth.revoke_subject("123").unwrap();
        assert!(!oauth.delegation_is_live(record.oauth_parent.as_ref().unwrap()));
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
        open_socket_with_access(base, token, allow, false).await
    }
    async fn open_socket_with_access(
        base: &str,
        token: &str,
        allow: Vec<String>,
        access: bool,
    ) -> Socket {
        open_socket_with_capabilities(
            base,
            token,
            allow,
            if access {
                &[wire::SESSION_ACCESS_CAPABILITY]
            } else {
                &[]
            },
        )
        .await
    }
    async fn open_socket_with_capabilities(
        base: &str,
        token: &str,
        allow: Vec<String>,
        capabilities: &[&str],
    ) -> Socket {
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        if !capabilities.is_empty() {
            request.headers_mut().insert(
                wire::CAPABILITIES_HEADER,
                capabilities.join(", ").parse().unwrap(),
            );
        }
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
    async fn version_notice_websocket_preserves_legacy_ready_and_uses_real_publication() {
        let directory = Temporary::new();
        let assets = directory.0.join("release");
        std::fs::create_dir(&assets).unwrap();
        let target = "x86_64-unknown-linux-gnu";
        let binary = assets.join(format!("ssmcp-{target}"));
        std::fs::write(&binary, b"fixture").unwrap();
        std::fs::write(assets.join("release.json"), serde_json::to_vec(&json!({
            "schema": 1, "version": "0.10.99", "wire_version": wire::VERSION,
            "targets": {"x86_64-unknown-linux-gnu": {"size": 7, "sha256": crate::hex::encode(Sha256::digest(b"fixture"))}},
        })).unwrap()).unwrap();
        std::fs::set_permissions(&assets, std::fs::Permissions::from_mode(0o700)).unwrap();
        for path in [&binary, &assets.join("release.json")] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let mut broker = Broker::with_identity_config(
            "https://ssmcp.test".into(),
            crate::oauth::OAuthConfig::default(),
            Some(crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture".into(),
            }),
        )
        .unwrap();
        broker.portal = crate::portal::Portal::open(&directory.0).unwrap();
        broker.client_downloads = Some(crate::client_downloads::Publisher::test_fixture(&assets));
        let alice = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        };
        broker
            .portal
            .storage(move |store| store.login(&alice))
            .await
            .unwrap();
        let login = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        // An actual old offer gets byte-compatible v7 field names, even though
        // this gateway has a publication ready for newer clients.
        let mut old = open_socket(&base, &login, vec![]).await;
        let ready = receive(&mut old).await.unwrap();
        let encoded = serde_json::to_value(ready).unwrap();
        assert_eq!(encoded.as_object().unwrap().len(), 5);
        assert!(encoded.get("capabilities").is_none());
        assert!(encoded.get("version_notice").is_none());
        old.close(None).await.unwrap();

        let offered = [
            wire::SESSION_ACCESS_CAPABILITY,
            wire::CONTROL_ATTRIBUTION_CAPABILITY,
            wire::versions::CAPABILITY,
        ];
        let mut current = open_socket_with_capabilities(&base, &login, vec![], &offered).await;
        let wire::Frame::Ready {
            version_notice: Some(notice),
            capabilities,
            ..
        } = receive(&mut current).await.unwrap()
        else {
            panic!("negotiated version notice");
        };
        assert_eq!(capabilities, offered);
        assert_eq!(notice.server_version, env!("CARGO_PKG_VERSION"));
        let published = notice.published_client.unwrap();
        assert_eq!(published.version, "0.10.99");
        assert_eq!(published.targets, [target]);
        current.close(None).await.unwrap();

        // A missing or partially published executable cannot become an update
        // hint and must not prevent otherwise compatible clients connecting.
        std::fs::remove_file(binary).unwrap();
        let mut incomplete = open_socket_with_capabilities(&base, &login, vec![], &offered).await;
        let wire::Frame::Ready {
            version_notice: Some(notice),
            ..
        } = receive(&mut incomplete).await.unwrap()
        else {
            panic!("server version remains available without a publication");
        };
        assert_eq!(notice.server_version, env!("CARGO_PKG_VERSION"));
        assert!(notice.published_client.is_none());
        incomplete.close(None).await.unwrap();
        broker.disconnect_all().await;
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn control_attribution_websocket_negotiates_without_changing_legacy_frames() {
        use tokio_tungstenite::tungstenite::Message;

        #[derive(serde::Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
        enum LegacyCall {
            Call {
                #[serde(rename = "id")]
                _id: u64,
                #[serde(rename = "operation")]
                _operation: Operation,
                #[serde(rename = "controller")]
                _controller: Option<wire::Controller>,
                #[serde(rename = "attribution")]
                _attribution: Option<wire::ControlAttribution>,
                #[serde(rename = "code_context")]
                _code_context: Option<wire::CodeContext>,
            },
        }
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
        let alice = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        };
        broker
            .portal
            .storage(move |store| store.login(&alice))
            .await
            .unwrap();
        let login = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        let access = broker.oauth.test_identity_tokens("mcp", "123")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let broker = Arc::new(broker);
        let authorization = broker
            .authenticate(IDENTITY_RESOURCE, &access)
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let offers: [&[&str]; 5] = [
            &[],
            &[wire::SESSION_ACCESS_CAPABILITY],
            &[wire::CONTROL_ATTRIBUTION_CAPABILITY],
            &[
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
            ],
            &[
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
                wire::openai::CAPABILITY,
            ],
        ];
        for capabilities in offers {
            let mut socket =
                open_socket_with_capabilities(&base, &login, vec![], capabilities).await;
            let id = match receive(&mut socket).await.unwrap() {
                wire::Frame::Ready {
                    client_id,
                    capabilities: accepted,
                    ..
                } => {
                    assert_eq!(accepted, capabilities);
                    client_id
                }
                _ => panic!("ready frame"),
            };
            let session = broker
                .client(IDENTITY_RESOURCE, &authorization, &id)
                .await
                .unwrap();
            for bound in [false, true] {
                let mut run = crate::code_mode::AuditRun::new("return 1;", None).unwrap();
                run.openai_context = wire::openai::OpenAiContext::from_meta(Some(&json!({
                    "openai/session":"synthetic-session",
                    "openai/subject":"untrusted-subject",
                    "account":{"subject":"impostor"}
                })));
                let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
                let agent = if bound {
                    let agent = session
                        .collaboration
                        .dispatch(&owner, "join", json!({"name":"fixture worker"}))
                        .await
                        .unwrap();
                    run.bind(
                        &id,
                        &owner,
                        agent["agent_id"].as_str().unwrap(),
                        agent["token"].as_str().unwrap(),
                    )
                    .unwrap();
                    Some(agent)
                } else {
                    None
                };
                let called = session.clone();
                let caller = authorization.clone();
                let request = tokio::spawn(crate::code_mode::AUDIT_RUN.scope(run, async move {
                    called
                        .call_with_authorization(
                            Operation::Status {},
                            Some(wire::Controller {
                                name: Some("bob (unverified claim)".into()),
                                model: Some("claimed-model".into()),
                                source: wire::ControllerSource::CallerReported,
                            }),
                            &caller,
                        )
                        .await
                }));
                if bound {
                    // A verified binding first asks this connection whether it
                    // accepts thread scope. This is not the requested Status.
                    let probe = receive(&mut socket).await.expect("thread scope probe");
                    let probe_id = match probe {
                        wire::Frame::Call {
                            id,
                            operation: Operation::CallTool { name, arguments },
                            controller,
                            attribution,
                            code_context,
                            openai_context,
                        } => {
                            assert!(openai_context.is_none());
                            assert_eq!(name, "ssmcp.client.capabilities");
                            assert_eq!(arguments, json!({}));
                            assert!(controller.is_none());
                            assert!(attribution.is_none());
                            assert!(code_context.is_none());
                            id
                        }
                        _ => panic!("thread scope capability call"),
                    };
                    socket
                        .send(Message::Binary(
                            serde_json::to_vec(&wire::Frame::Reply {
                                id: probe_id,
                                result: Ok(json!({"thread_scope":1})),
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await
                        .unwrap();
                }
                let bytes = loop {
                    match tokio::time::timeout(Duration::from_secs(4), socket.next())
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap()
                    {
                        Message::Binary(bytes) => break bytes.to_vec(),
                        Message::Text(text) => break text.as_bytes().to_vec(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        _ => panic!("call frame"),
                    }
                };
                let raw: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(raw["type"], "call");
                let frame: wire::Frame = serde_json::from_slice(&bytes).unwrap();
                let (call_id, attribution) = match frame {
                    wire::Frame::Call {
                        id: call_id,
                        operation,
                        attribution,
                        controller,
                        code_context,
                        openai_context,
                    } => {
                        let expected_openai =
                            bound && capabilities.contains(&wire::openai::CAPABILITY);
                        assert_eq!(openai_context.is_some(), expected_openai);
                        assert_eq!(raw.get("openai_context").is_some(), expected_openai);
                        assert_eq!(
                            serde_json::from_value::<LegacyCall>(raw.clone()).is_err(),
                            expected_openai
                        );
                        if let Some(context) = openai_context {
                            assert_eq!(context.session.as_deref(), Some("synthetic-session"));
                            assert_eq!(context.subject.as_deref(), Some("untrusted-subject"));
                        }
                        assert!(matches!(operation, Operation::Status {}));
                        assert_eq!(
                            controller.unwrap().name.as_deref(),
                            Some("bob (unverified claim)")
                        );
                        let context = code_context.expect("code-mode audit context");
                        assert_eq!(
                            context.session_id.as_deref(),
                            agent.as_ref().map(|_| id.as_str())
                        );
                        assert_eq!(
                            context.thread_id.as_deref(),
                            agent.as_ref().and_then(|agent| agent["agent_id"].as_str())
                        );
                        (call_id, attribution)
                    }
                    _ => panic!("call frame"),
                };
                if capabilities.contains(&wire::CONTROL_ATTRIBUTION_CAPABILITY) {
                    let attribution = attribution.unwrap();
                    assert_eq!(attribution.account.subject, "123");
                    assert_eq!(attribution.account.login, "alice");
                    assert_eq!(attribution.owner.unwrap().subject, "123");
                    if let Some(agent) = &agent {
                        assert_eq!(
                            attribution.agent.unwrap().id,
                            agent["agent_id"].as_str().unwrap()
                        );
                        assert!(
                            !String::from_utf8_lossy(&bytes)
                                .contains(agent["token"].as_str().unwrap())
                        );
                    } else {
                        assert!(attribution.agent.is_none());
                    }
                } else {
                    assert!(attribution.is_none());
                    assert!(
                        raw.get("attribution").is_none(),
                        "legacy strict decoder must see no new field"
                    );
                }
                socket
                    .send(Message::Binary(
                        serde_json::to_vec(&wire::Frame::Reply {
                            id: call_id,
                            result: Ok(json!({"fixture":true})),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(4), request)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap(),
                    json!({"fixture":true})
                );
                if let Some(agent) = agent {
                    session
                        .collaboration
                        .dispatch(
                            &owner,
                            "leave",
                            json!({
                                "agent_id":agent["agent_id"], "token":agent["token"],
                            }),
                        )
                        .await
                        .unwrap();
                }
            }
            socket.close(None).await.unwrap();
        }
        let mut socket = open_socket_with_capabilities(&base, &login, vec![], offers[4]).await;
        let Some(wire::Frame::Ready {
            client_id,
            capabilities,
            ..
        }) = receive(&mut socket).await
        else {
            panic!("missing Ready for the HTTP metadata fixture");
        };
        assert_eq!(capabilities, offers[4]);
        let session = broker
            .client(IDENTITY_RESOURCE, &authorization, &client_id)
            .await
            .unwrap();
        let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
        openai_metadata_from_http_stays_with_each_original_history(
            &base,
            &access,
            &mut socket,
            &session,
            &owner,
            directory.0.join("histories"),
        )
        .await;
        socket.close(None).await.unwrap();
        server.abort();
        let _ = server.await;
    }

    async fn openai_metadata_from_http_stays_with_each_original_history(
        base: &str,
        access: &str,
        socket: &mut Socket,
        session: &crate::broker::Session,
        owner: &str,
        directory: PathBuf,
    ) {
        use crate::{client::interface::history, runtime::call_scope::CallScope};
        use tokio_tungstenite::tungstenite::Message;

        history::with_test_history(directory, async {
            // Both conversations must outlive the in-flight calls in one live store.
            history::record("user", "Conversation B").unwrap();
            let b_history = history::current().unwrap().unwrap().metadata.id;
            history::release_current().unwrap();
            history::record("user", "Conversation A").unwrap();
            let a_owner = history::current_owner().unwrap();
            let client = crate::http_client::builder().no_proxy().build().unwrap();
            let frame_reply = |id, value| Message::Binary(
                serde_json::to_vec(&wire::Frame::Reply { id, result: Ok(value) }).unwrap().into(),
            );
            let mut calls = Vec::new();
            let mut threads = Vec::new();
            let mut runs = Vec::new();
            for (rpc_id, label) in [(1, "A"), (2, "B")] {
                if label == "B" {
                    history::resume_test_history(&b_history).unwrap();
                }
                let name = format!("fixture-{label}");
                let agent = session.collaboration.dispatch(owner, "join", json!({"name":name})).await.unwrap();
                let thread = agent["agent_id"].as_str().unwrap();
                let job = format!("job-{label}");
                let expected_session = format!("anonymous-session-{label}");
                let expected_subject = format!("anonymous-subject-{label}");
                let request = client.post(format!("{base}/{IDENTITY_RESOURCE}"))
                    .bearer_auth(access)
                    .timeout(Duration::from_secs(12))
                    .json(&json!({"jsonrpc":"2.0","id":rpc_id,"method":"tools/call","params":{
                        "name":"await",
                        "arguments":{"client_id":session.client_id(),"timeout_ms":10000,
                            "targets":[{"type":"exec","job_id":job}],
                            "agent":{"id":thread,"token":agent["token"]}},
                        "_meta":{"openai/session":expected_session,"openai/subject":expected_subject,
                            "account":{"subject":"not-authenticated"}}
                    }}));
                let response = tokio::spawn(request.send());
                if label == "A" {
                    let Some(wire::Frame::Call {
                        id, operation: Operation::CallTool {name, arguments},
                        attribution: None, controller: None, code_context: None, openai_context: None,
                    }) = receive(socket).await else {
                        panic!("missing unbound thread-scope probe");
                    };
                    assert_eq!(name, "ssmcp.client.capabilities");
                    assert_eq!(arguments, json!({}));
                    socket.send(frame_reply(id, json!({"thread_scope":1}))).await.unwrap();
                }

                let Some(wire::Frame::Call { id, operation: Operation::CallTool {name, arguments},
                    attribution: Some(attribution), controller: None, code_context: Some(context),
                    openai_context: Some(metadata),
                }) = receive(socket).await else {
                    panic!("missing authenticated business call with OpenAI metadata");
                };
                assert_eq!(name, "ssmcp.exec.wait");
                assert_eq!(arguments["job_id"], job);
                assert_eq!(attribution.account.subject, "123");
                assert_eq!(attribution.account.login, "alice");
                assert_eq!(attribution.owner.as_ref().unwrap().subject, "123");
                assert_eq!(attribution.agent.as_ref().unwrap().id, thread);
                assert_eq!(context.session_id.as_deref(), Some(session.client_id()));
                assert_eq!(context.thread_id.as_deref(), Some(thread));
                assert_eq!(metadata.session.as_deref(), Some(expected_session.as_str()));
                assert_eq!(metadata.subject.as_deref(), Some(expected_subject.as_str()));
                let scope = CallScope::from_relay_for_client(
                    1, Some(session.client_id()), id, Some(&attribution), Some(&context),
                ).unwrap();
                let observation = scope.openai_observation(metadata, context.timestamp_ms).unwrap();
                threads.push(thread.to_owned());
                runs.push(context.run_id);
                calls.push((rpc_id, id, job, observation, response));
            }
            assert_ne!(threads[0], threads[1]);
            assert_ne!(runs[0], runs[1]);
            assert_ne!(calls[0].1, calls[1].1);
            let b_owner = history::current_owner().unwrap();
            assert_eq!(b_owner.id(), b_history);
            assert_ne!(a_owner.id(), b_owner.id());
            assert_ne!(a_owner, b_owner);

            // Both native calls are now in flight. Complete B first, then A
            // while B is still selected, using only the already-captured owners.
            for (rpc_id, id, job, observation, response) in calls.into_iter().rev() {
                assert!(observation.record().unwrap());
                socket.send(frame_reply(id, json!({
                    "structuredContent":{"job_id":job,"running":false,"exit_code":0}
                }))).await.unwrap();
                let response = response.await.unwrap().unwrap().error_for_status().unwrap();
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["id"], rpc_id);
                assert_eq!(body["result"]["structuredContent"]["status"], "ready");
                assert_eq!(body["result"]["structuredContent"]["results"][0]["data"]["job_id"], job);
            }

            assert_eq!(history::current_owner().unwrap().id(), b_history);
            assert!(history::openai::read_selected(&a_owner).is_err());
            let b = history::openai::read_selected(&b_owner).unwrap().unwrap();
            assert_eq!(b.session.unwrap().value, "anonymous-session-B");
            assert_eq!(b.subject.unwrap().value, "anonymous-subject-B");
            history::resume_test_history(a_owner.id()).unwrap();
            let a = history::openai::read_selected(&a_owner).unwrap().unwrap();
            assert_eq!(a.session.unwrap().value, "anonymous-session-A");
            assert_eq!(a.subject.unwrap().value, "anonymous-subject-A");
            assert!(history::openai::read_selected(&b_owner).is_err());
            history::resume_test_history(&b_history).unwrap();
            assert_eq!(history::current_owner().unwrap().id(), b_history);
        }).await;
    }

    #[test]
    fn session_access_capability_requires_an_exact_bounded_http_token() {
        let mut control = HeaderMap::new();
        control.insert(
            wire::CAPABILITIES_HEADER,
            wire::CONTROL_ATTRIBUTION_CAPABILITY.parse().unwrap(),
        );
        assert!(capability_requested(
            &control,
            wire::CONTROL_ATTRIBUTION_CAPABILITY
        ));
        assert!(!session_access_requested(&control));
        control.insert(
            wire::CAPABILITIES_HEADER,
            "control_attribution_v10".parse().unwrap(),
        );
        assert!(!capability_requested(
            &control,
            wire::CONTROL_ATTRIBUTION_CAPABILITY
        ));
        for value in ["session_access_v1", "other, session_access_v1"] {
            let mut headers = HeaderMap::new();
            headers.insert(wire::CAPABILITIES_HEADER, value.parse().unwrap());
            assert!(session_access_requested(&headers));
        }
        for value in [
            "",
            "session_access_v10",
            "prefix_session_access_v1",
            "session_access_v1;admin",
            "SESSION_ACCESS_V1",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(wire::CAPABILITIES_HEADER, value.parse().unwrap());
            assert!(!session_access_requested(&headers), "{value}");
        }
        let mut headers = HeaderMap::new();
        assert!(!session_access_requested(&headers));
        headers.append(
            wire::CAPABILITIES_HEADER,
            wire::SESSION_ACCESS_CAPABILITY.parse().unwrap(),
        );
        headers.append(wire::CAPABILITIES_HEADER, "other".parse().unwrap());
        assert!(!session_access_requested(&headers));
    }

    #[tokio::test]
    async fn session_access_wss_negotiation_and_live_oauth_revocation() {
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        async fn grant(
            socket: &mut Socket,
            id: u64,
            command: wire::AccessCommand,
        ) -> Result<wire::AccessSnapshot, String> {
            socket
                .send(Message::Binary(
                    serde_json::to_vec(&wire::Frame::Access { id, command })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            match receive(socket).await.unwrap() {
                wire::Frame::AccessReply {
                    id: reply_id,
                    result,
                } => {
                    assert_eq!(reply_id, id);
                    result
                }
                _ => panic!("missing access reply"),
            }
        }
        let directory = Temporary::new();
        let mock = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock_origin = format!("http://{}", mock.local_addr().unwrap());
        let mock_server = tokio::spawn(async move {
            axum::serve(
                mock,
                Router::new().route(
                    "/users/bob",
                    get(|| async { Json(json!({"id":456,"login":"bob","type":"User"})) }),
                ),
            )
            .await
            .unwrap();
        });
        let mut broker = Broker::new("https://ssmcp.test".into());
        broker.identities = crate::identity::IdentityProviders::with_github_test_endpoints(
            "https://ssmcp.test",
            crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture-secret".into(),
            },
            &mock_origin,
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
                    store.claim_handle(subject, login)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let device = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        // Issued before the grant, and deliberately reused after revocation.
        let bob_token = broker.oauth.test_identity_tokens("mcp", "456")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let forged_token = broker.oauth.test_identity_tokens("mcp", "999")["access_token"]
            .as_str()
            .unwrap()
            .to_owned();
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        // A capability header conveys no identity and cannot skip credential checks.
        let mut unauthorized = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        unauthorized.headers_mut().insert(
            wire::CAPABILITIES_HEADER,
            wire::SESSION_ACCESS_CAPABILITY.parse().unwrap(),
        );
        assert!(
            matches!(tokio_tungstenite::connect_async(unauthorized).await, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == StatusCode::BAD_REQUEST)
        );
        // Sharing is explicit and requires the controller capability negotiation.
        let mut unnegotiated_registration =
            open_socket_with_access(&base, &device, vec!["bob@ssmcp.test".into()], false).await;
        assert!(matches!(
            receive(&mut unnegotiated_registration).await,
            Some(wire::Frame::Reject { .. })
        ));

        let mut legacy = open_socket(&base, &device, vec![]).await;
        let bytes = loop {
            match tokio::time::timeout(Duration::from_secs(4), legacy.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
            {
                Message::Binary(data) => break data.to_vec(),
                Message::Text(text) => break text.as_bytes().to_vec(),
                Message::Ping(_) | Message::Pong(_) => continue,
                _ => panic!("ready frame"),
            }
        };
        let old_ready: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(old_ready["type"], "ready");
        assert_eq!(old_ready.as_object().unwrap().len(), 5);
        assert!(old_ready.get("capabilities").is_none());
        let other_id = old_ready["client_id"].as_str().unwrap().to_owned();

        let mut socket = open_socket_with_access(&base, &device, vec![], true).await;
        let id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Ready {
                client_id,
                capabilities,
                ..
            } => {
                assert_eq!(capabilities, [wire::SESSION_ACCESS_CAPABILITY]);
                client_id
            }
            _ => panic!("capable ready"),
        };
        let initial = grant(&mut socket, 1, wire::AccessCommand::List {})
            .await
            .unwrap();
        assert_eq!(initial.entries.len(), 1);
        assert!(initial.entries[0].owner);
        let bob = broker
            .authenticate(IDENTITY_RESOURCE, &bob_token)
            .await
            .unwrap();
        assert!(broker.client(IDENTITY_RESOURCE, &bob, &id).await.is_err());
        grant(
            &mut socket,
            2,
            wire::AccessCommand::Add {
                principal: "bob@ssmcp.test".into(),
            },
        )
        .await
        .unwrap();
        let bob = broker
            .authenticate(IDENTITY_RESOURCE, &bob_token)
            .await
            .unwrap();
        let held = broker.client(IDENTITY_RESOURCE, &bob, &id).await.unwrap();
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &bob, &other_id)
                .await
                .is_err()
        );
        let forged = broker
            .authenticate(IDENTITY_RESOURCE, &forged_token)
            .await
            .unwrap();
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &forged, &id)
                .await
                .is_err()
        );
        let info_uri = format!("ssmcp://clients/{id}/info");
        assert!(
            crate::resources::read(&broker, IDENTITY_RESOURCE, &bob, &info_uri)
                .await
                .is_ok()
        );
        let without_registrar = grant(
            &mut socket,
            3,
            wire::AccessCommand::Remove {
                principal: "alice@ssmcp.test".into(),
            },
        )
        .await
        .unwrap();
        assert!(without_registrar.entries.iter().all(|entry| !entry.owner));
        grant(
            &mut socket,
            6,
            wire::AccessCommand::Add {
                principal: "alice@ssmcp.test".into(),
            },
        )
        .await
        .unwrap();

        let called = held.clone();
        let calling_bob = bob.clone();
        let call = tokio::spawn(async move {
            called
                .call_with_authorization(Operation::Status {}, None, &calling_bob)
                .await
        });
        let call_id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Call { id, .. } => id,
            _ => panic!("granted call"),
        };
        socket
            .send(Message::Binary(
                serde_json::to_vec(&wire::Frame::Reply {
                    id: call_id,
                    result: Ok(json!({"ok":true})),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert_eq!(call.await.unwrap().unwrap()["ok"], true);

        // A read that was already dispatched must withhold its data after revocation.
        let reading_broker = broker.clone();
        let reading_bob = bob.clone();
        let tools_uri = format!("ssmcp://clients/{id}/tools");
        let read = tokio::spawn(async move {
            crate::resources::read(&reading_broker, IDENTITY_RESOURCE, &reading_bob, &tools_uri)
                .await
        });
        let read_id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Call { id, .. } => id,
            _ => panic!("resource call"),
        };
        let removed = grant(
            &mut socket,
            4,
            wire::AccessCommand::Remove {
                principal: "bob@ssmcp.test".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(removed.entries.len(), 1);
        socket
            .send(Message::Binary(
                serde_json::to_vec(&wire::Frame::Reply {
                    id: read_id,
                    result: Ok(json!({"tools":[],"secret":"withhold"})),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        assert!(matches!(
            read.await.unwrap(),
            Err(crate::resources::ResourceError::NotFound)
        ));
        let revoked = broker
            .authenticate(IDENTITY_RESOURCE, &bob_token)
            .await
            .unwrap();
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &revoked, &id)
                .await
                .is_err()
        );
        assert!(
            held.call_with_authorization(Operation::Status {}, None, &revoked)
                .await
                .unwrap_err()
                .contains("revoked")
        );
        assert!(
            crate::resources::read(&broker, IDENTITY_RESOURCE, &revoked, &info_uri)
                .await
                .is_err()
        );
        assert!(
            grant(&mut socket, 5, wire::AccessCommand::List {})
                .await
                .unwrap()
                .entries[0]
                .owner
        );
        socket.close(None).await.unwrap();
        legacy.close(None).await.unwrap();
        server.abort();
        mock_server.abort();
    }

    #[tokio::test]
    async fn session_access_login_rechecks_slow_body_and_retired_device_tokens_are_rejected() {
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
                    store.claim_handle(subject, login)?;
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
        let client = crate::http_client::builder().build().unwrap();
        let legacy_device = {
            let login = alice.clone();
            broker
                .portal
                .client_storage(move |store| {
                    let invite = store.invite(&login, 600, 3600)?.0;
                    Ok(store.enroll(&invite)?.0)
                })
                .await
                .unwrap()
        };
        for endpoint in ["whoami", "sessions"] {
            assert_eq!(
                client
                    .get(format!("{base}/api/client/{endpoint}"))
                    .bearer_auth(&legacy_device)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                401
            );
        }
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {legacy_device}").parse().unwrap(),
        );
        assert!(matches!(tokio_tungstenite::connect_async(request).await,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status()==StatusCode::UNAUTHORIZED));
        let mut socket = open_socket_with_access(&base, &alice, vec![], true).await;
        let id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Ready { client_id, .. } => client_id,
            _ => panic!("ready"),
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
            tokio::time::timeout(Duration::from_millis(100), receive(&mut socket))
                .await
                .is_err()
        );
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami"))
                .bearer_auth(&alice)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let authorization = Authorization::Identity(Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        });
        assert_eq!(
            broker
                .clients(IDENTITY_RESOURCE, &authorization)
                .await
                .unwrap()
                .len(),
            1
        );
        socket.close(None).await.unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn username_binding_and_admin_ban_cover_real_http_and_live_websockets() {
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
                for (subject, login) in [("123", "alice"), ("456", "bob"), ("999", "maintainer")] {
                    store.login(&Identity {
                        provider: "github".into(),
                        subject: subject.into(),
                        login: login.into(),
                    })?;
                }
                store.claim_handle("999", "maintainer")?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            broker
                .portal
                .resolve_admin_usernames(&["Maintainer".into()])
                .await
                .unwrap(),
            ["999"]
        );
        assert!(
            broker
                .portal
                .resolve_admin_usernames(&["unregistered".into()])
                .await
                .is_err()
        );
        broker.configure_management(
            crate::management::ManagementPolicy::new(vec!["999".into()], Default::default())
                .unwrap(),
            Arc::new(
                crate::history::AuditLog::open(
                    &directory.0.join("audit"),
                    crate::history::AuditConfig::default(),
                )
                .unwrap(),
            ),
        );
        broker
            .configure_user_policies(crate::user_policy::DEFAULT_RATE_LIMIT_BPS)
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
        let client = crate::http_client::builder().build().unwrap();
        let before_claim: Value = client
            .get(format!("{base}/api/client/whoami"))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let generated = before_claim["identity"]["username"].as_str().unwrap();
        assert!(generated.starts_with("member-"));
        assert_eq!(before_claim["identity"]["username_generated"], true);
        assert_eq!(before_claim["identity"]["username_aliases"], json!([]));
        let claimed: Value = client
            .post(format!("{base}/api/client/username"))
            .bearer_auth(&alice)
            .json(&json!({"username":"Alice"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(claimed["identity"]["username"], "alice");
        assert_eq!(claimed["identity"]["subject"], "123");
        assert_eq!(claimed["identity"]["username_generated"], false);
        assert_eq!(claimed["identity"]["username_aliases"], json!([generated]));
        let current: Value = client
            .get(format!("{base}/api/client/whoami"))
            .bearer_auth(&alice)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current["identity"], claimed["identity"]);
        let duplicate = client
            .post(format!("{base}/api/client/username"))
            .bearer_auth(&bob)
            .json(&json!({"username":"ALICE"}))
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let invalid = client
            .post(format!("{base}/api/client/username"))
            .bearer_auth(&bob)
            .json(&json!({"username":"admin"}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        let mut first = open_socket(&base, &alice, vec![]).await;
        let mut second = open_socket(&base, &alice, vec![]).await;
        let mut foreign = open_socket(&base, &bob, vec![]).await;
        assert!(matches!(
            receive(&mut first).await,
            Some(wire::Frame::Ready { .. })
        ));
        assert!(matches!(
            receive(&mut second).await,
            Some(wire::Frame::Ready { .. })
        ));
        assert!(matches!(
            receive(&mut foreign).await,
            Some(wire::Frame::Ready { .. })
        ));
        let admin = Identity {
            provider: "github".into(),
            subject: "999".into(),
            login: "maintainer".into(),
        };
        let user = Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        };
        assert_eq!(
            broker
                .management_set_user_policy(&user, "456", Some(true), None)
                .await,
            Err(crate::management::ManagementError::Forbidden)
        );
        assert_eq!(
            broker
                .management_set_user_policy(&admin, "999", Some(true), None)
                .await,
            Err(crate::management::ManagementError::Forbidden)
        );
        broker
            .management_set_user_policy(&admin, "123", None, Some(Some(2 * 1024 * 1024)))
            .await
            .unwrap();
        broker
            .management_set_user_policy(&admin, "123", Some(true), None)
            .await
            .unwrap();
        for socket in [&mut first, &mut second] {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), receive(socket))
                    .await
                    .is_err()
            );
        }
        let stored = broker
            .portal
            .storage(|store| store.account_record("123"))
            .await
            .unwrap();
        assert!(stored.banned);
        assert_eq!(stored.rate_limit_bps, Some(2 * 1024 * 1024));
        assert!(
            broker
                .clients(IDENTITY_RESOURCE, &Authorization::Identity(user))
                .await
                .is_err()
        );
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami"))
                .bearer_auth(&alice)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let live: Value = client
            .get(format!("{base}/api/client/sessions"))
            .bearer_auth(&bob)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(live["clients"].as_array().unwrap().len(), 1);
        broker
            .management_set_user_policy(&admin, "123", Some(false), None)
            .await
            .unwrap();
        // Unbanning never resurrects revoked credentials.
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami"))
                .bearer_auth(&alice)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        broker
            .management_set_user_policy(&admin, "123", None, Some(None))
            .await
            .unwrap();
        let restored = broker
            .portal
            .storage(|store| store.account("123"))
            .await
            .unwrap();
        assert!(!restored.banned);
        assert_eq!(restored.rate_limit_bps, None);
        let fresh = broker
            .portal
            .client_storage(|store| store.login("123".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami"))
                .bearer_auth(&fresh)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        broker.disconnect_all().await;
        server.abort();
        let _ = server.await;
    }
    async fn traffic_http_fixture() -> (Temporary, Arc<Broker>, String, String, String) {
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
                for (subject, login) in [("123", "alice"), ("999", "maintainer")] {
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
        broker.configure_management(
            crate::management::ManagementPolicy::new(vec!["999".into()], Default::default())
                .unwrap(),
            Arc::new(
                crate::history::AuditLog::open(
                    &directory.0.join("audit"),
                    crate::history::AuditConfig::default(),
                )
                .unwrap(),
            ),
        );
        let (login, admin, device) = broker
            .portal
            .client_storage(|store| {
                let login = store.login("123".into())?.0;
                let admin = store.login("999".into())?.0;
                let invitation = store.invite(&login, 600, 3600)?.0;
                let device = store.enroll(&invitation)?.0;
                Ok((login, admin, device))
            })
            .await
            .unwrap();
        (directory, Arc::new(broker), login, admin, device)
    }

    #[tokio::test]
    async fn traffic_whoami_reports_monthly_quota_and_server_admin_only() {
        let (_directory, broker, login, admin, _device) = traffic_http_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::http_client::builder().build().unwrap();
        let ordinary: Value = client
            .get(format!("{base}/api/client/whoami"))
            .bearer_auth(&login)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(ordinary["identity"]["subject"], "123");
        assert_eq!(ordinary["traffic"]["limit_bytes"], 1_000_000_000u64);
        assert_eq!(ordinary["traffic"]["remaining_bytes"], 1_000_000_000u64);
        assert_eq!(ordinary["traffic"]["used_bytes"], 0);
        assert_eq!(ordinary["traffic"]["period"], broker.traffic_period());
        assert_eq!(ordinary["traffic"]["utc_offset_minutes"], 480);
        let unlimited: Value = client
            .get(format!("{base}/api/client/whoami"))
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unlimited["identity"]["subject"], "999");
        assert!(unlimited["traffic"]["limit_bytes"].is_null());
        assert!(unlimited["traffic"]["remaining_bytes"].is_null());
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami?is_admin=true"))
                .bearer_auth(&login)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn traffic_invalid_websocket_json_does_not_consume_credit() {
        use tokio_tungstenite::tungstenite::{Message as PeerMessage, client::IntoClientRequest};
        let (_directory, broker, login, _admin, _device) = traffic_http_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {login}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        socket
            .send(PeerMessage::Binary(b"not valid json".to_vec().into()))
            .await
            .unwrap();
        assert!(receive(&mut socket).await.is_none());
        assert_eq!(broker.traffic_quota("123").await.unwrap().used_bytes, 0);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn traffic_whoami_reports_configured_tier_and_server_reset_for_the_real_account() {
        let (_directory, broker, login, admin, _device) = traffic_http_fixture().await;
        broker
            .portal
            .configure_monthly_allowances(crate::user_policy::MonthlyTrafficAllowances {
                free_user_bytes: 120,
                paid_user_bytes: 350,
            })
            .unwrap();
        let period = broker.traffic_period();
        let current_period = period.clone();
        broker
            .portal
            .storage(move |store| {
                store.reserve_traffic_frame(
                    "123",
                    &current_period,
                    480,
                    crate::user_policy::TrafficFrame {
                        uploaded: true,
                        bytes: 20,
                    },
                    false,
                )?;
                store.reserve_traffic_frame(
                    "123",
                    &current_period,
                    480,
                    crate::user_policy::TrafficFrame {
                        uploaded: false,
                        bytes: 15,
                    },
                    false,
                )
            })
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://{}/api/client/whoami",
            listener.local_addr().unwrap()
        );
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::http_client::builder().build().unwrap();
        let before: Value = client
            .get(&url)
            .bearer_auth(&login)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(before["identity"]["subject"], "123");
        assert_eq!(before["traffic"]["free_bytes"], 120);
        assert_eq!(before["traffic"]["used_bytes"], 20);
        assert_eq!(before["traffic"]["limit_bytes"], 120);
        assert_eq!(before["traffic"]["remaining_bytes"], 100);
        assert_eq!(
            before["traffic"]["reset_at"],
            crate::user_policy::traffic_reset_at(&period, 480).unwrap()
        );
        let current_period = period.clone();
        broker
            .portal
            .storage(move |store| {
                store.record_recharge("123")?;
                store.grant_paid_traffic(
                    "123",
                    &current_period,
                    &current_period,
                    "http-monthly-pack",
                    60,
                )
            })
            .await
            .unwrap();
        let after: Value = client
            .get(&url)
            .bearer_auth(&login)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(after["traffic"]["free_bytes"], 350);
        assert_eq!(after["traffic"]["paid_bytes"], 60);
        assert_eq!(after["traffic"]["limit_bytes"], 410);
        assert_eq!(after["traffic"]["remaining_bytes"], 390);
        assert_eq!(after["traffic"]["uploaded_bytes"], 20);
        assert_eq!(after["traffic"]["downloaded_bytes"], 15);
        assert_eq!(after["traffic"]["reset_at"], before["traffic"]["reset_at"]);
        let unlimited: Value = client
            .get(&url)
            .bearer_auth(&admin)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(unlimited["identity"]["subject"], "999");
        assert!(unlimited["traffic"]["limit_bytes"].is_null());
        assert!(unlimited["traffic"]["remaining_bytes"].is_null());
        assert_eq!(
            client
                .get(format!("{url}?paid_user=true"))
                .bearer_auth(&login)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        server.abort();
        let _ = server.await;
    }
    include!("device/routing_acceptance.rs");
}
