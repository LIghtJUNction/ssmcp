//! Account credentials and outbound WebSocket devices. Enrollment grants can only
//! create a controlled device; they never authenticate controller API calls.
use crate::{
    broker::{Authorization, Broker, IDENTITY_RESOURCE},
    identity::Identity,
    portal::Problem,
    user_policy::{
        TrafficError, TrafficFailure, TrafficFailureState, TrafficOutcome, traffic_recharge_url,
    },
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
    fn invitation(&self, raw: &str) -> Result<Record, Problem> {
        if !valid_secret(raw) {
            return Err(Problem::Unauthorized);
        }
        self.live(&hash(raw))
            .filter(|record| record.kind == Kind::Invite)
            .cloned()
            .ok_or(Problem::Unauthorized)
    }
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

#[derive(Clone)]
pub(crate) struct Registration {
    pub identity: Identity,
    pub username: String,
    /// Negotiated only for an authenticated controller login, never an enrolled device.
    pub session_access: bool,
    /// Display-only account attribution is available to login and enrolled peers.
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
    let subject = registration.identity.subject.clone();
    let account = broker
        .portal
        .storage(move |store| store.account(&subject))
        .await
        .map_err(problem)?;
    let mut identity = identity_json(&registration.identity);
    identity["username"] = json!(account.handle);
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
    let mut identity = identity_json(&registration.identity);
    identity["username"] = json!(account.handle);
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
    let (release, release_error) = match &broker.client_downloads {
        Some(publication) => match publication.metadata().await {
            Ok(release) => (Some(release), None),
            Err(error) => (None, Some(error)),
        },
        None => (
            None,
            Some("gateway client downloads are not configured".to_owned()),
        ),
    };
    let (token, expires, session_expires) = broker
        .portal
        .client_storage(move |s| {
            let (_, parent) = s.authenticate(&raw, true)?;
            let cap = options
                .session_ttl_secs
                .min(parent.expires_at.saturating_sub(now()));
            let (token, expires) = s.invite(&raw, options.ttl_secs, options.session_ttl_secs)?;
            Ok((token, expires, cap))
        })
        .await
        .map_err(problem)?;
    Ok(Json(
        json!({"token":token,"expires_in":expires,"session_expires_in":session_expires,
            "release":release,"release_error":release_error,"wire_version":wire::VERSION,
            "supported_targets":crate::client_downloads::TARGETS}),
    ))
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum InviteShell {
    #[default]
    Sh,
    PowerShell,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControllerInvite {
    #[serde(default = "invite_ttl")]
    ttl_secs: u64,
    #[serde(default = "session_ttl")]
    session_ttl_secs: u64,
    #[serde(default)]
    shell: InviteShell,
}

impl ControllerInvite {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(1..=3600).contains(&self.ttl_secs) || !(1..=86400).contains(&self.session_ttl_secs) {
            return Err(
                "invite ttl_secs must be 1..=3600 and session_ttl_secs must be 1..=86400".into(),
            );
        }
        Ok(())
    }
}

/// Mint a one-use enrollment command from the caller's existing OAuth grant.
/// No controller/GitHub token is returned, copied into a command, or stored in
/// the device ledger. Every OAuth check happens outside its disk transaction.
pub(crate) async fn controller_invite(
    broker: &Broker,
    user: &str,
    access_token: &str,
    options: ControllerInvite,
) -> Result<Value, String> {
    options.validate()?;
    if !broker.portal.enabled(broker) {
        return Err("account invitations are unavailable; configure the existing GitHub identity and account store".into());
    }
    let (parent, identity) = broker.oauth.delegation(user, access_token).ok_or(
        "invite requires a live GitHub OAuth grant for /mcp or this account's own resource",
    )?;
    if broker.user_policies.is_banned(&identity.subject) {
        return Err("account access has been revoked".into());
    }
    let publication = broker.client_downloads.as_ref()
        .ok_or("gateway client downloads are not configured; an installed compatible client may still use ssmcp invite --no-bootstrap")?;
    let release = publication.metadata().await?;
    let verified = identity.clone();
    let account = broker
        .portal
        .storage(move |store| store.login(&verified))
        .await
        .map_err(|error| format!("account invitation unavailable: {}", problem(error).code))?;
    if user != IDENTITY_RESOURCE && user != account.username {
        return Err("this OAuth resource cannot authorize another account's device".into());
    }
    if !broker.oauth.delegation_is_live(&parent)
        || broker.user_policies.is_banned(&identity.subject)
    {
        return Err("the original OAuth authorization expired or was revoked".into());
    }
    let stored_parent = parent.clone();
    let (token, expires_in) = broker
        .portal
        .client_storage(move |store| {
            store.invite_oauth(stored_parent, options.ttl_secs, options.session_ttl_secs)
        })
        .await
        .map_err(|error| format!("could not create invitation: {}", problem(error).code))?;
    if !broker.oauth.delegation_is_live(&parent)
        || broker.user_policies.is_banned(&identity.subject)
    {
        return Err("the original OAuth authorization expired or was revoked".into());
    }
    let command_sh = crate::client::account::command(&broker.public_url, &token, true, false)
        .map_err(|_| "could not format the enrollment command")?;
    let command_powershell =
        crate::client::account::command(&broker.public_url, &token, true, true)
            .map_err(|_| "could not format the enrollment command")?;
    let selected = match options.shell {
        InviteShell::Sh => command_sh.clone(),
        InviteShell::PowerShell => command_powershell.clone(),
    };
    Ok(json!({
        "server": broker.public_url,
        "command": selected,
        "command_sh": command_sh,
        "command_powershell": command_powershell,
        "expires_in": expires_in,
        "session_expires_in": options.session_ttl_secs.min(parent.expires_at.saturating_sub(now())),
        "authorization_expires_at": parent.expires_at,
        "owner": identity_json(&identity),
        "resource": format!("{}/{}", broker.public_url.trim_end_matches('/'), parent.user),
        "release": release,
        "supported_targets": crate::client_downloads::TARGETS,
        "supported_os": ["Linux GNU (glibc 2.35+) x86_64/aarch64", "macOS Intel/Apple Silicon", "Windows x64/ARM64"],
        "installation": "Verify the gateway's currently published compatible release, install in the user's bin directory, then join in the foreground. The exact release is checked again at install time.",
        "authorization": "One-use invitation; the device needs no GitHub OAuth login. Access is restricted to this verified controller identity and ends no later than the original OAuth grant. Keep the terminal open; Ctrl-C stops the session."
    }))
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
    let raw = input.token.clone();
    let record = broker
        .portal
        .client_storage(move |store| store.invitation(&raw))
        .await
        .map_err(problem)?;
    if !record_authorized(&broker, &record) {
        return Err(problem(Problem::Unauthorized));
    }
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
    // A concurrent family revocation during the disk transaction must not
    // return a usable credential. Registration and the live watcher recheck it.
    if !record_authorized(&broker, &record) {
        return Err(problem(Problem::Unauthorized));
    }
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
    let mut registration = authenticate(&broker, &headers, false).await?;
    // Enrollment permits a controlled device, not delegation to other identities.
    // The existing controller check accepts only a full login credential. Keep
    // invited devices connected, but leave the broker's Access gate disabled.
    registration.session_access =
        session_access_requested(&headers) && authenticate(&broker, &headers, true).await.is_ok();
    registration.control_attribution =
        capability_requested(&headers, wire::CONTROL_ATTRIBUTION_CAPABILITY);
    let quota = broker
        .traffic_quota(&registration.identity.subject)
        .await
        .map_err(problem)?;
    if quota.remaining_bytes == Some(0) {
        return Ok((
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "traffic_quota_exceeded",
                "message": TrafficError::Exhausted.notice(&broker.public_url, TrafficOutcome::NotDispatched),
                "recharge_url": traffic_recharge_url(&broker.public_url),
            })),
        ).into_response());
    }
    Ok(ws
        .max_message_size(wire::MAX_FRAME)
        .max_frame_size(wire::MAX_FRAME)
        .on_upgrade(move |socket| socket_bridge(broker, registration, socket)))
}
async fn socket_bridge(broker: Arc<Broker>, registration: Registration, socket: WebSocket) {
    let bandwidth = broker.user_policies.get(&registration.identity.subject);
    let incoming_broker = broker.clone();
    let outgoing_broker = broker.clone();
    let incoming_subject = registration.identity.subject.clone();
    let outgoing_subject = incoming_subject.clone();
    let traffic_failure = TrafficFailureState::default();
    let incoming_failure = traffic_failure.clone();
    let outgoing_failure = traffic_failure.clone();
    let banned_failure = traffic_failure.clone();
    let public_url = broker.public_url.clone();
    let (transport, stream) = tokio::io::duplex(256 * 1024);
    let (input, mut output) = tokio::io::split(transport);
    let mut input = tokio::io::BufReader::new(input);
    let (mut sender, mut receiver) = socket.split();
    let pumps = async move {
        let failure = {
            let inbound = async move {
                while let Some(Ok(message)) = receiver.next().await {
                    let data = match message {
                        Message::Binary(data) => data,
                        Message::Text(text) => text.as_bytes().to_vec().into(),
                        Message::Close(_) => break,
                        _ => continue,
                    };
                    // Only a valid application frame can consume account credit.
                    let Ok(frame) = serde_json::from_slice::<wire::Frame>(&data) else {
                        break;
                    };
                    if let Err(error) = incoming_broker
                        .charge_relay(&incoming_subject, false, data.len())
                        .await
                    {
                        let failure = match &frame {
                            wire::Frame::Reply { id, .. } => {
                                TrafficFailure::rejected_reply(error, *id)
                            }
                            _ => TrafficFailure::new(error),
                        };
                        // Store the trusted reason before dropping the duplex writer.
                        return Some(incoming_failure.record(failure));
                    }
                    if wire::write_frame(&mut output, &frame).await.is_err() {
                        break;
                    }
                }
                let _ = output.shutdown().await;
                None
            };
            let outbound = async {
                let mut ping = tokio::time::interval(Duration::from_secs(25));
                loop {
                    let next = wire::read_frame(&mut input);
                    tokio::pin!(next);
                    let frame = loop {
                        tokio::select! {
                            frame = &mut next => break frame,
                            _ = ping.tick() => if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                                return None;
                            },
                        }
                    };
                    match frame {
                        Ok(Some(frame)) => {
                            let Ok(mut data) = serde_json::to_vec(&frame) else {
                                break;
                            };
                            data.push(b'\n');
                            if let Err(error) = outgoing_broker
                                .charge_relay(&outgoing_subject, true, data.len())
                                .await
                            {
                                let failure = match &frame {
                                    wire::Frame::Call { id, .. } => {
                                        TrafficFailure::rejected_call(error, *id)
                                    }
                                    _ => TrafficFailure::new(error),
                                };
                                return Some(outgoing_failure.record(failure));
                            }
                            if sender.send(Message::Binary(data.into())).await.is_err() {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
                None
            };
            tokio::select! {
                result = inbound => result,
                result = outbound => result,
                _ = bandwidth.wait_banned() => Some(banned_failure.record(TrafficFailure::new(TrafficError::Banned))),
            }
        };
        // Release the relay before flushing a single, uncharged terminal error.
        // Cancelling the other pump must not cancel this explanation to the peer.
        drop(input);
        if let Some(error) = failure
            && let Ok(mut data) = serde_json::to_vec(&wire::Frame::Reject {
                message: error.notice(&public_url),
            })
        {
            data.push(b'\n');
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                sender.send(Message::Binary(data.into())),
            )
            .await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), sender.close()).await;
    };
    tokio::pin!(pumps);
    let mut session = tokio::spawn(broker.attach_device(stream, registration, traffic_failure));
    tokio::select! {
        _ = &mut session => {
            // Flush the final authorization rejection before closing the socket.
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

    fn oauth_fixture() -> (Temporary, Broker, String) {
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
        let bytes = b"fixture release";
        let target = "x86_64-unknown-linux-gnu";
        std::fs::write(directory.0.join(format!("ssmcp-{target}")), bytes).unwrap();
        std::fs::write(directory.0.join("release.json"), serde_json::to_vec(&json!({
            "schema":1,"version":"0.9.0","wire_version":wire::VERSION,
            "targets":{target:{"size":bytes.len(),"sha256":hash(std::str::from_utf8(bytes).unwrap())}}
        })).unwrap()).unwrap();
        broker.client_downloads = Some(crate::client_downloads::Publisher::test_fixture(
            &directory.0,
        ));
        let tokens = broker.oauth.test_identity_tokens("mcp", "123");
        (
            directory,
            broker,
            tokens["access_token"].as_str().unwrap().into(),
        )
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
        oauth.revoke_subject("123");
        assert!(!oauth.delegation_is_live(record.oauth_parent.as_ref().unwrap()));
    }

    #[tokio::test]
    async fn oauth_mcp_invite_enrollment_and_live_socket_keep_scope_and_revocation() {
        let (directory, broker, access) = oauth_fixture();
        let wrong = broker.oauth.test_identity_tokens("gh-456", "123");
        assert!(
            controller_invite(
                &broker,
                "gh-456",
                wrong["access_token"].as_str().unwrap(),
                serde_json::from_value(json!({})).unwrap()
            )
            .await
            .is_err()
        );
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&access)
            .header("accept", "application/json")
            .header("mcp-protocol-version", "2025-11-25")
            .json(
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name":"invite","arguments":{"ttl_secs":600,"session_ttl_secs":86400}
                }}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await.unwrap();
        assert_ne!(response["result"]["isError"], true, "{response}");
        let invitation = &response["result"]["structuredContent"];
        assert_eq!(invitation["owner"]["subject"], "123");
        assert_eq!(invitation["release"]["version"], "0.9.0");
        assert!(invitation["session_expires_in"].as_u64().unwrap() <= 8 * 3600);
        let command = invitation["command_sh"].as_str().unwrap();
        assert!(!command.contains(&access));
        assert!(!command.contains("cl_"));
        let raw = command
            .split('\'')
            .find(|part| part.starts_with("ci_") && part.len() == 67)
            .unwrap();
        let enrolled: Value = client
            .post(format!("{base}/api/client/enroll"))
            .json(&json!({"token":raw}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let device = enrolled["device_token"].as_str().unwrap();
        assert_eq!(
            client
                .post(format!("{base}/api/client/enroll"))
                .json(&json!({"token":raw}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert_eq!(
            client
                .get(format!("{base}/api/client/whoami"))
                .bearer_auth(device)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        let mut invalid = open_socket(&base, device, vec!["bob@github".into()]).await;
        assert!(matches!(
            receive(&mut invalid).await,
            Some(wire::Frame::Reject { .. })
        ));
        let mut socket = open_socket(&base, device, vec![]).await;
        assert!(matches!(
            receive(&mut socket).await,
            Some(wire::Frame::Ready { .. })
        ));
        let pending = controller_invite(
            &broker,
            "mcp",
            &access,
            serde_json::from_value(json!({})).unwrap(),
        )
        .await
        .unwrap();
        let pending_token = pending["command_sh"]
            .as_str()
            .unwrap()
            .split('\'')
            .find(|part| part.starts_with("ci_") && part.len() == 67)
            .unwrap();
        broker.oauth.revoke_subject("123");
        assert_eq!(
            client
                .post(format!("{base}/api/client/enroll"))
                .json(&json!({"token":pending_token}))
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
        assert!(
            matches!(receive(&mut socket).await, Some(wire::Frame::Reject { message }) if message.contains("revoked"))
        );
        let disk = std::fs::read_to_string(directory.0.join("client-credentials.json")).unwrap();
        assert!(!disk.contains(&access));
        assert!(!disk.contains("cl_"));
        server.abort();
    }

    #[tokio::test]
    async fn oauth_invite_reports_unconfigured_publication_without_creating_credentials() {
        let (_directory, mut broker, access) = oauth_fixture();
        broker.client_downloads = None;
        let error = controller_invite(
            &broker,
            "mcp",
            &access,
            serde_json::from_value(json!({})).unwrap(),
        )
        .await
        .unwrap_err();
        assert!(error.contains("not configured"));
        assert_eq!(
            broker
                .portal
                .client_storage(|store| Ok(store.records.len()))
                .await
                .unwrap(),
            0
        );
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
    async fn control_attribution_websocket_negotiates_without_changing_legacy_frames() {
        use tokio_tungstenite::tungstenite::Message;
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
        let offers: [&[&str]; 4] = [
            &[],
            &[wire::SESSION_ACCESS_CAPABILITY],
            &[wire::CONTROL_ATTRIBUTION_CAPABILITY],
            &[
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
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
                let run = crate::code_mode::AuditRun::new("return 1;", None).unwrap();
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
                        id,
                        attribution,
                        controller,
                        ..
                    } => {
                        assert_eq!(
                            controller.unwrap().name.as_deref(),
                            Some("bob (unverified claim)")
                        );
                        (id, attribution)
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
        server.abort();
        let _ = server.await;
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
            matches!(tokio_tungstenite::connect_async(unauthorized).await, Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == StatusCode::UNAUTHORIZED)
        );
        // Sharing is explicit and requires the controller capability negotiation.
        let mut unnegotiated_registration =
            open_socket_with_access(&base, &device, vec!["bob@github".into()], false).await;
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
                principal: "bob@github".into(),
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
        assert!(
            grant(
                &mut socket,
                3,
                wire::AccessCommand::Remove {
                    principal: "alice@github".into()
                }
            )
            .await
            .unwrap_err()
            .contains("所有者")
        );

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
                principal: "bob@github".into(),
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
    async fn session_access_invited_device_keeps_controller_scope_and_logout() {
        use tokio_tungstenite::tungstenite::Message;
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
        // This is a real invitation-enrolled Kind::Device credential, not a login.
        // Claiming the capability must neither reject ordinary device use nor
        // turn the invitation into authority to share the inviter's session.
        let mut socket = open_socket_with_access(&base, device, vec![], true).await;
        let id = match receive(&mut socket).await.unwrap() {
            wire::Frame::Ready {
                client_id,
                login_code,
                login_expires_in,
                capabilities,
                ..
            } => {
                assert!(login_code.is_empty());
                assert_eq!(login_expires_in, 0);
                assert!(capabilities.is_empty());
                client_id
            }
            _ => panic!("expected ready"),
        };
        // Exercise the server's authorization gate directly: hiding the Ready
        // capability alone would not stop a peer from writing these frames.
        for (index, command) in [
            wire::AccessCommand::List {},
            wire::AccessCommand::Add {
                principal: "bob@github".into(),
            },
            wire::AccessCommand::Remove {
                principal: "alice@github".into(),
            },
        ]
        .into_iter()
        .enumerate()
        {
            let request_id = index as u64 + 1;
            socket
                .send(Message::Binary(
                    serde_json::to_vec(&wire::Frame::Access {
                        id: request_id,
                        command,
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            match receive(&mut socket).await.unwrap() {
                wire::Frame::AccessReply { id, result } => {
                    assert_eq!(id, request_id);
                    assert!(
                        result
                            .unwrap_err()
                            .contains("not negotiated on an authenticated account connection")
                    );
                }
                _ => panic!("expected access rejection"),
            }
        }
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
        let invite = {
            let alice = alice.clone();
            broker
                .portal
                .client_storage(move |store| store.invite(&alice, 600, 3600))
                .await
                .unwrap()
                .0
        };
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
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
            tokio::time::timeout(Duration::from_secs(2), async {
                match receive(socket).await {
                    Some(wire::Frame::Reject { message }) => {
                        // A terminal authorization explanation may precede EOF;
                        // no application message may pass after the durable ban.
                        if message != "ssmcp authorization expired or revoked" {
                            let (reason, notice) = message.split_once("; ").expect(
                                "a ban rejection includes the interrupted-connection notice",
                            );
                            assert_eq!(reason, "account is banned");
                            assert_eq!(
                                notice,
                                "连接已中断；已发送的请求可能已经执行，重试前请先检查实际状态。"
                            );
                        }
                        assert!(receive(socket).await.is_none());
                    }
                    None => {}
                    Some(_) => panic!("a banned connection forwarded an application frame"),
                }
            })
            .await
            .unwrap();
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
        assert_eq!(
            client
                .post(format!("{base}/api/client/enroll"))
                .json(&json!({"token":invite}))
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
        let client = reqwest::Client::new();
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
    async fn traffic_websocket_refuses_both_directions_and_reconnect_cannot_reset_quota() {
        use tokio_tungstenite::tungstenite::{Message as PeerMessage, client::IntoClientRequest};
        let (_directory, broker, login, _admin, device) = traffic_http_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut incoming = open_socket(&base, &device, vec![]).await;
        let first_id = match receive(&mut incoming).await {
            Some(wire::Frame::Ready { client_id, .. }) => client_id,
            _ => panic!("expected a ready frame"),
        };
        let mut outgoing = open_socket(&base, &device, vec![]).await;
        let second_id = match receive(&mut outgoing).await {
            Some(wire::Frame::Ready { client_id, .. }) => client_id,
            _ => panic!("expected a ready frame"),
        };
        let authorization = Authorization::Identity(Identity {
            provider: "github".into(),
            subject: "123".into(),
            login: "alice".into(),
        });
        let first_session = broker
            .client(IDENTITY_RESOURCE, &authorization, &first_id)
            .await
            .unwrap();
        let first_call = tokio::spawn(async move {
            first_session
                .call(Operation::ListTools { cursor: None })
                .await
        });
        let first_request = match receive(&mut incoming).await {
            Some(wire::Frame::Call { id, .. }) => id,
            _ => panic!("expected a dispatched call"),
        };
        let quota = broker.traffic_quota("123").await.unwrap();
        assert!(
            quota.used_bytes > 0,
            "Hello and Ready are application messages"
        );
        let consume = quota.remaining_bytes.unwrap() - 10;
        let period = broker.traffic_period();
        broker
            .portal
            .storage(move |store| store.reserve_traffic("123", &period, 480, consume, false))
            .await
            .unwrap();

        // A valid inbound application frame exceeding the remaining allowance
        // receives a terminal error instead of reaching the broker's reply loop.
        incoming
            .send(PeerMessage::Binary(
                serde_json::to_vec(&wire::Frame::Reply {
                    id: first_request,
                    result: Ok(json!({"quota_marker":"must not be forwarded"})),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let incoming_error = match receive(&mut incoming).await {
            Some(wire::Frame::Reject { message }) => message,
            _ => panic!("expected incoming quota rejection"),
        };
        assert!(incoming_error.starts_with("traffic_quota_exceeded:"));
        assert!(incoming_error.contains("https://ssmcp.test/account/traffic"));
        assert!(incoming_error.contains("客户端结果未能送达"));
        assert!(incoming_error.contains("可能已经执行"));
        assert!(!incoming_error.contains("未执行"));
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(4), first_call)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            incoming_error
        );
        assert_eq!(
            broker.traffic_quota("123").await.unwrap().remaining_bytes,
            Some(10)
        );

        // The second connection shares exactly the same budget for outgoing calls.
        let session = broker
            .client(IDENTITY_RESOURCE, &authorization, &second_id)
            .await
            .unwrap();
        let mut calls = Vec::new();
        for _ in 0..3 {
            let session = session.clone();
            calls.push(tokio::spawn(async move {
                session.call(Operation::ListTools { cursor: None }).await
            }));
        }
        let outgoing_error = match receive(&mut outgoing).await {
            Some(wire::Frame::Reject { message }) => message,
            _ => panic!("expected outgoing quota rejection"),
        };
        assert!(outgoing_error.starts_with("traffic_quota_exceeded:"));
        assert!(outgoing_error.contains("https://ssmcp.test/account/traffic"));
        assert!(outgoing_error.contains("未执行"));
        for call in calls {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(4), call)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err(),
                outgoing_error
            );
        }
        assert_eq!(
            broker.traffic_quota("123").await.unwrap().remaining_bytes,
            Some(10)
        );

        let mut reconnected = open_socket(&base, &device, vec![]).await;
        assert!(matches!(receive(&mut reconnected).await,
            Some(wire::Frame::Reject { message }) if message.contains("traffic_quota_exceeded")));
        assert_eq!(
            broker.traffic_quota("123").await.unwrap().remaining_bytes,
            Some(10)
        );
        let period = broker.traffic_period();
        broker
            .portal
            .storage(move |store| store.reserve_traffic("123", &period, 480, 10, false))
            .await
            .unwrap();
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {device}").parse().unwrap());
        match tokio_tungstenite::connect_async(request).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
            }
            _ => panic!("an empty allowance must reject a new WebSocket upgrade"),
        }
        let rejected = reqwest::Client::new()
            .get(format!("{base}/api/client/connect"))
            .bearer_auth(&device)
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::PAYMENT_REQUIRED);
        let rejected: Value = rejected.json().await.unwrap();
        assert_eq!(rejected["error"], "traffic_quota_exceeded");
        assert_eq!(
            rejected["recharge_url"],
            "https://ssmcp.test/account/traffic"
        );
        assert_eq!(rejected["message"], outgoing_error);
        let status: Value = reqwest::Client::new()
            .get(format!("{base}/api/client/whoami"))
            .bearer_auth(&login)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["traffic"]["used_bytes"], 1_000_000_000u64);
        assert_eq!(status["traffic"]["remaining_bytes"], 0);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn traffic_invalid_websocket_json_does_not_consume_credit() {
        use tokio_tungstenite::tungstenite::{Message as PeerMessage, client::IntoClientRequest};
        let (_directory, broker, _login, _admin, device) = traffic_http_fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut request = format!("{}/api/client/connect", base.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {device}").parse().unwrap());
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
}
