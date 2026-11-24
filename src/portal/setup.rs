//! Short-lived, browser-consented client setup. Device secrets stay in the CLI;
//! only their SHA-256 digest is retained here. No GitHub tokens or private keys
//! are exposed, persisted in grants, or accepted by the setup API.

use super::{
    DigestKey, LOGIN_TTL, Portal, Problem, Session, digest, form, only, parameters, random,
    set_cookie, start_login, store::Account, valid_secret, view,
};
use crate::broker::Broker;
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, MutexGuard},
    time::{Duration, Instant},
};
use tokio::sync::Mutex as AsyncMutex;

const TTL: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_GRANTS: usize = 512;
const MAX_BODY: usize = 32 * 1024;
const ALPHABET: &[u8; 32] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

#[derive(Default)]
pub(super) struct SetupState {
    grants: HashMap<DigestKey, Entry>,
    codes: HashMap<String, DigestKey>,
    rates: HashMap<&'static str, super::Rate>,
}
struct Entry {
    user_code: String,
    expires: Instant,
    grant: Arc<AsyncMutex<Grant>>,
}
struct Grant {
    client_name: String,
    requested_username: Option<String>,
    registration_only: bool,
    expires: Instant,
    decision: Decision,
    last_poll: Option<Instant>,
    login_issued: bool,
}
#[derive(Clone)]
enum Decision {
    Pending,
    Authorized(String),
    Denied,
    Failed(Problem),
}
#[derive(Clone)]
pub(super) struct ReturnTo {
    device: DigestKey,
    code: String,
}
impl ReturnTo {
    /// Persist a terminal OAuth failure for the CLI without changing an already
    /// approved or denied grant from another browser attempt.
    pub(super) async fn fail(&self, portal: &Portal, problem: Problem) -> Result<(), Problem> {
        let grant = {
            let mut state = portal.setup.lock().map_err(|_| Problem::Storage)?;
            state.prune();
            state
                .grants
                .get(&self.device)
                .filter(|entry| entry.user_code == self.code)
                .map(|entry| entry.grant.clone())
        };
        if let Some(grant) = grant {
            let mut grant = grant.lock().await;
            if matches!(grant.decision, Decision::Pending) {
                grant.decision = Decision::Failed(problem);
            }
        }
        Ok(())
    }
    /// The callback target is an allowlisted local path bound to the original
    /// device grant, never a URL supplied by a browser or OAuth query parameter.
    pub(super) fn location(&self, portal: &Portal) -> Result<String, Problem> {
        let mut state = portal.setup.lock().map_err(|_| Problem::Storage)?;
        state.prune();
        Ok(
            if state
                .grants
                .get(&self.device)
                .is_some_and(|entry| entry.user_code == self.code)
            {
                format!("/setup?code={}", self.code)
            } else {
                "/setup".into()
            },
        )
    }
}
impl SetupState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.grants.retain(|_, entry| entry.expires > now);
        self.codes.retain(|_, key| self.grants.contains_key(key));
        self.rates
            .retain(|_, rate| now.saturating_duration_since(rate.start) < Duration::from_secs(60));
    }
    fn charge(&mut self, category: &'static str, maximum: usize) -> Result<(), ApiError> {
        self.prune();
        let rate = self.rates.entry(category).or_insert(super::Rate {
            start: Instant::now(),
            count: 0,
        });
        if rate.count >= maximum {
            return Err(ApiError::rate(60));
        }
        rate.count += 1;
        Ok(())
    }
    fn issue(&mut self, client_name: String) -> Result<(String, String), ApiError> {
        self.prune();
        if self.grants.len() >= MAX_GRANTS {
            return Err(ApiError::rate(60));
        }
        let device_code = random()?;
        let key = digest(&device_code);
        if self.grants.contains_key(&key) {
            return Err(ApiError::unavailable());
        }
        let user_code = (0..8)
            .find_map(|_| {
                let entropy = random().ok()?;
                let bytes = URL_SAFE_NO_PAD.decode(entropy).ok()?;
                let characters: String = bytes[..10]
                    .iter()
                    .map(|b| char::from(ALPHABET[usize::from(b & 31)]))
                    .collect();
                let code = format!("{}-{}", &characters[..5], &characters[5..]);
                (!self.codes.contains_key(&code)).then_some(code)
            })
            .ok_or_else(ApiError::unavailable)?;
        let expires = Instant::now() + TTL;
        self.grants.insert(
            key,
            Entry {
                user_code: user_code.clone(),
                expires,
                grant: Arc::new(AsyncMutex::new(Grant {
                    client_name,
                    requested_username: None,
                    registration_only: false,
                    expires,
                    decision: Decision::Pending,
                    last_poll: None,
                    login_issued: false,
                })),
            },
        );
        self.codes.insert(user_code.clone(), key);
        Ok((device_code, user_code))
    }
    fn by_device(&mut self, secret: &str) -> Result<Option<Arc<AsyncMutex<Grant>>>, ApiError> {
        if !valid_secret(secret) {
            return Err(ApiError::invalid(
                "device_code must be the secret returned by client/login/start",
            ));
        }
        self.prune();
        Ok(self
            .grants
            .get(&digest(secret))
            .map(|entry| entry.grant.clone()))
    }
    fn by_code(&mut self, code: &str) -> Result<(ReturnTo, Arc<AsyncMutex<Grant>>), ApiError> {
        if !valid_code(code) {
            return Err(ApiError::invalid("The setup comparison code is invalid."));
        }
        self.prune();
        let key = *self.codes.get(code).ok_or_else(ApiError::expired)?;
        let entry = self.grants.get(&key).ok_or_else(ApiError::expired)?;
        Ok((
            ReturnTo {
                device: key,
                code: code.into(),
            },
            entry.grant.clone(),
        ))
    }
}
impl Grant {
    fn alive(&self) -> Result<(), ApiError> {
        if self.expires <= Instant::now() {
            Err(ApiError::expired())
        } else {
            Ok(())
        }
    }
    fn decide(&mut self, owner: &str, allow: bool) -> Result<(), ApiError> {
        self.alive()?;
        match &self.decision {
            Decision::Failed(problem) => Err((*problem).into()),
            Decision::Pending => {
                self.decision = if allow {
                    Decision::Authorized(owner.into())
                } else {
                    Decision::Denied
                };
                Ok(())
            }
            Decision::Authorized(current) if current == owner && allow => Ok(()),
            Decision::Authorized(_) => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "access_denied",
                "This setup request has already been bound to an account.",
            )),
            Decision::Denied => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "access_denied",
                "This setup request was denied. Start a new request from the terminal.",
            )),
        }
    }
}

fn valid_code(code: &str) -> bool {
    code.len() == 11
        && code.as_bytes()[5] == b'-'
        && code
            .bytes()
            .enumerate()
            .all(|(index, byte)| index == 5 || ALPHABET.contains(&byte))
}
fn lock(portal: &Portal) -> Result<MutexGuard<'_, SetupState>, ApiError> {
    portal.setup.lock().map_err(|_| ApiError::unavailable())
}
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retry_after: Option<u64>,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            retry_after: None,
        }
    }
    fn invalid(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "Account setup is temporarily unavailable.",
        )
    }
    fn expired() -> Self {
        Self::new(
            StatusCode::GONE,
            "expired_token",
            "This setup request has expired or was cancelled. Start again from the terminal.",
        )
    }
    fn rate(seconds: u64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "slow_down",
            "Wait before trying this setup operation again.",
        );
        error.retry_after = Some(seconds);
        error
    }
}
impl From<Problem> for ApiError {
    fn from(problem: Problem) -> Self {
        match problem {
            Problem::Invalid | Problem::ManagementInvalid => Self::invalid(
                "The request or SSH public key is invalid. Only a single Ed25519 public key is accepted.",
            ),
            Problem::Unauthorized => Self::new(
                StatusCode::UNAUTHORIZED,
                "authorization_required",
                "Sign in with GitHub to approve this computer.",
            ),
            Problem::Csrf | Problem::Forbidden => Self::new(
                StatusCode::FORBIDDEN,
                "access_denied",
                "The browser origin, session or CSRF token did not match.",
            ),
            Problem::UsernameInvalid => Self::new(
                StatusCode::BAD_REQUEST,
                "username_invalid",
                "Username must use 3-32 ASCII letters, digits, '_' or '-', start with a letter, end with a letter or digit, and not be reserved.",
            ),
            Problem::UsernameTaken => Self::new(
                StatusCode::CONFLICT,
                "username_taken",
                "This username belongs to another GitHub account. Choose another name or sign in with its owner.",
            ),
            Problem::UsernameBound => Self::new(
                StatusCode::CONFLICT,
                "username_bound",
                "This account already has a username. Connect using its current name, or rename it on the account page.",
            ),
            Problem::UsernameRenameLimit => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "username_rename_limit",
                "This account has reached the server's monthly username change limit.",
            ),
            Problem::Banned => Self::new(
                StatusCode::FORBIDDEN,
                "account_banned",
                "This account has been banned by the server administrator.",
            ),
            Problem::TrafficQuotaExceeded => Self::new(
                StatusCode::PAYMENT_REQUIRED,
                "traffic_quota_exceeded",
                "Monthly traffic allowance exhausted. Purchase a package or wait for the next month.",
            ),
            Problem::TrafficPeriodMismatch | Problem::TrafficGrantConflict => Self::new(
                StatusCode::CONFLICT,
                "traffic_grant_conflict",
                "The payment month or recorded order does not match. Contact the administrator.",
            ),
            Problem::Busy => Self::rate(60),
            Problem::Limit => Self::new(
                StatusCode::CONFLICT,
                "key_limit",
                "This account has reached its SSH public key limit.",
            ),
            #[cfg(test)]
            Problem::Duplicate => Self::new(
                StatusCode::CONFLICT,
                "key_already_registered",
                "This setup request already registered another public key.",
            ),
            #[cfg(test)]
            Problem::NotFound => Self::expired(),
            Problem::SessionNotFound => Self::expired(),
            Problem::Storage
            | Problem::Disabled
            | Problem::ManagementDisabled
            | Problem::ManagementUnavailable
            | Problem::AuditUnavailable => Self::unavailable(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error":self.code,"message":self.message})),
        )
            .into_response();
        if let Some(seconds) = self.retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(&seconds.to_string()).expect("numeric retry-after"),
            );
        }
        response
    }
}
fn web_result(result: Result<Response, ApiError>) -> Response {
    match result {
        Ok(response)=>response,
        Err(error)=>(error.status,Html(view::page("连接尚未完成",&format!(
            "<section class=card><p class=eyebrow>SSMCP · COMPUTER USE</p><h1>连接尚未完成</h1><p role=alert>{}</p><p>请返回发起连接的终端，重新开始设置。</p><a class=button href=/>返回首页</a></section>",
            escape(error.message)
        )))).into_response(),
    }
}
fn enabled(broker: &Broker) -> Result<(), ApiError> {
    if broker.portal.enabled(broker) {
        Ok(())
    } else {
        Err(ApiError::unavailable())
    }
}
fn check_api_origin(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origin.to_str().ok() != Some(expected) || origins.next().is_some())
    {
        return Err(Problem::Csrf.into());
    }
    if headers
        .get("sec-fetch-site")
        .is_some_and(|value| value != "same-origin" && value != "none")
    {
        return Err(Problem::Csrf.into());
    }
    Ok(())
}
async fn body<T: serde::de::DeserializeOwned>(
    request: Request,
    origin: &str,
) -> Result<T, ApiError> {
    check_api_origin(request.headers(), origin)?;
    let mut types = request.headers().get_all(header::CONTENT_TYPE).iter();
    if types
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        != Some("application/json")
        || types.next().is_some()
    {
        return Err(ApiError::invalid("Content-Type must be application/json."));
    }
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| ApiError::invalid("The JSON request body timed out."))?
    .map_err(|_| ApiError::invalid("The JSON request body exceeds 32 KiB."))?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::invalid("The JSON request contains invalid, duplicate or unknown fields.")
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_code: String,
}
pub(super) fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/api/client/registration/start", post(registration_start))
        .route(
            "/api/client/login/start",
            post(client_start).fallback(method_not_allowed),
        )
        .route(
            "/api/client/login/poll",
            post(client_poll).fallback(method_not_allowed),
        )
        .route(
            "/api/client/login/cancel",
            post(cancel).fallback(method_not_allowed),
        )
        .route("/setup", get(page))
        .route("/setup/authorize", post(authorize))
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request",
        "Use POST with an application/json request body for this endpoint.",
    )
}

async fn cancel(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let request: Device = body(request, &broker.public_url).await?;
    let grant = {
        let mut state = lock(&broker.portal)?;
        state.charge("cancel", 2000)?;
        state.by_device(&request.device_code)?
    };
    if let Some(grant) = grant {
        // Revocation waits for an already-authorized save to finish. No later
        // registration can start after the state changes to Denied.
        grant.lock().await.decision = Decision::Denied;
        let mut state = lock(&broker.portal)?;
        if let Some(entry) = state.grants.remove(&digest(&request.device_code)) {
            state.codes.remove(&entry.user_code);
        }
    }
    Ok(Json(json!({"status":"cancelled"})))
}

fn browser_session(
    portal: &Portal,
    headers: &HeaderMap,
) -> Result<(DigestKey, String, Option<String>, Option<String>), ApiError> {
    match portal.session(headers) {
        Ok((key, csrf, subject)) => Ok((key, csrf, subject, None)),
        Err(Problem::Unauthorized) => {
            let raw = random()?;
            let csrf = random()?;
            let key = digest(&raw);
            let mut state = portal.browser.lock().map_err(|_| ApiError::unavailable())?;
            state.make_room_for_session()?;
            state.sessions.insert(
                key,
                Session {
                    subject: None,
                    csrf: csrf.clone(),
                    expires: Instant::now() + LOGIN_TTL,
                },
            );
            Ok((key, csrf, None, Some(raw)))
        }
        Err(problem) => Err(problem.into()),
    }
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
async fn page(
    State(broker): State<Arc<Broker>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    web_result(page_inner(&broker, query.as_deref(), &headers).await)
}
async fn page_inner(
    broker: &Broker,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    enabled(broker)?;
    let values = parameters(query.unwrap_or(""))?;
    if values.is_empty() {
        return Err(ApiError::expired());
    }
    only(&values, &["code"])?;
    let (return_to, grant) = {
        let mut state = lock(&broker.portal)?;
        state.charge("page", 600)?;
        state.by_code(&values["code"])?
    };
    let (name, decision, requested_username, registration_only) = {
        let grant = grant.lock().await;
        grant.alive()?;
        (
            grant.client_name.clone(),
            grant.decision.clone(),
            grant.requested_username.clone(),
            grant.registration_only,
        )
    };
    if let Decision::Failed(problem) = &decision {
        return Err((*problem).into());
    }
    if matches!(decision, Decision::Denied) {
        return Ok(Html(client_completed_page(false, registration_only)).into_response());
    }
    let (browser, csrf, subject, new_cookie) = browser_session(&broker.portal, headers)?;
    if let Decision::Authorized(owner) = decision {
        if subject.as_deref() != Some(&owner) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "access_denied",
                "This setup request has already been approved by its account owner.",
            ));
        }
        return Ok(Html(client_completed_page(true, registration_only)).into_response());
    }
    if subject.is_none() {
        let mut response = start_login(broker, browser, Some(return_to))?;
        if let Some(cookie) = new_cookie {
            set_cookie(&mut response, &cookie, LOGIN_TTL.as_secs());
        }
        return Ok(response);
    }
    let subject = subject.expect("subject checked");
    let account = match broker
        .portal
        .storage(move |store| store.account(&subject))
        .await
    {
        Ok(account) => account,
        Err(problem) => {
            if problem == Problem::Banned {
                return_to.fail(&broker.portal, problem).await?;
            }
            return Err(problem.into());
        }
    };
    if registration_only {
        return Ok(Html(registration_page(
            &values["code"],
            &csrf,
            requested_username.as_deref().unwrap_or_default(),
        ))
        .into_response());
    }
    Ok(Html(client_consent_page(
        &name,
        &values["code"],
        &csrf,
        &account,
        requested_username.as_deref(),
        &broker.public_url,
    ))
    .into_response())
}
async fn authorize(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    web_result(authorize_inner(&broker, request).await)
}
async fn authorize_inner(broker: &Broker, request: Request) -> Result<Response, ApiError> {
    enabled(broker)?;
    let (headers, form) = form(request).await?;
    only(&form, &["csrf", "code", "decision"])?;
    let (_, subject) = broker.portal.verify(&headers, &form, &broker.public_url)?;
    let subject = subject.ok_or(Problem::Unauthorized)?;
    let allow = match form["decision"].as_str() {
        "allow" => true,
        "deny" => false,
        _ => return Err(ApiError::invalid("decision must be allow or deny.")),
    };
    let (_, grant) = {
        let mut state = lock(&broker.portal)?;
        state.charge("authorize", 600)?;
        state.by_code(&form["code"])?
    };
    // The grant lock keeps approval invisible until the durable claim succeeds.
    // A verified identity is required even when this request only denies access.
    let mut grant = grant.lock().await;
    let previous = grant.decision.clone();
    grant.decide(&subject, allow)?;
    let username = grant.requested_username.clone().filter(|_| allow);
    let registration_only = grant.registration_only;
    let result = broker
        .portal
        .storage(move |store| match username {
            Some(username) if registration_only => store.register_handle(&subject, &username),
            Some(username) => store.claim_handle(&subject, &username),
            None => store.account(&subject),
        })
        .await;
    if let Err(problem) = result {
        // Conflicts and bans are terminal: the terminal must not wait for the
        // ten-minute expiry after the browser has already rejected this login.
        grant.decision = match problem {
            Problem::UsernameTaken | Problem::UsernameBound | Problem::Banned => {
                Decision::Failed(problem)
            }
            _ => previous,
        };
        return Err(problem.into());
    }
    Ok(Redirect::to(&format!("/setup?code={}", form["code"])).into_response())
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientStart {
    name: String,
    #[serde(default)]
    username: Option<String>,
}

async fn client_start(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    start_client_request(broker, request, false).await
}

async fn registration_start(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    start_client_request(broker, request, true).await
}

async fn start_client_request(
    broker: Arc<Broker>,
    request: Request,
    registration_only: bool,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let input: ClientStart = body(request, &broker.public_url).await?;
    let name = input.name.trim();
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(ApiError::invalid(
            "name must contain 1..80 UTF-8 bytes without control characters.",
        ));
    }
    let username = input
        .username
        .as_deref()
        .map(super::store::normalize_handle)
        .transpose()?;
    // A taken name can still be used by its owner; the GitHub-authenticated
    // confirmation performs the authoritative atomic check and claim.
    let username_available = match username.clone() {
        Some(name) => Some(
            broker
                .portal
                .storage(move |store| store.handle_available(&name))
                .await?,
        ),
        None => None,
    };
    if registration_only {
        match username_available {
            Some(true) => {}
            Some(false) => return Err(Problem::UsernameTaken.into()),
            None => return Err(ApiError::invalid("registration requires a username")),
        }
    }
    let (device_code, user_code, grant) = {
        let mut state = lock(&broker.portal)?;
        state.charge("start", 60)?;
        let (device_code, user_code) = state.issue(name.into())?;
        let grant = state
            .by_device(&device_code)?
            .ok_or_else(ApiError::unavailable)?;
        (device_code, user_code, grant)
    };
    {
        let mut grant = grant.lock().await;
        grant.requested_username = username.clone();
        grant.registration_only = registration_only;
    }
    let base = broker.public_url.trim_end_matches('/');
    Ok(Json(
        json!({"device_code":device_code,"user_code":user_code,
        "username":username,"username_available":username_available,
        "verification_uri":format!("{base}/setup"),
        "verification_uri_complete":format!("{base}/setup?code={user_code}"),
        "expires_in":TTL.as_secs(),"interval":POLL_INTERVAL.as_secs()}),
    ))
}
async fn client_poll(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let input: Device = body(request, &broker.public_url).await?;
    let grant = {
        let mut state = lock(&broker.portal)?;
        state.charge("poll", 18000)?;
        state.by_device(&input.device_code)?
    };
    let Some(grant) = grant else {
        return Ok(Json(json!({"status":"expired"})));
    };
    let mut grant = grant.lock().await;
    if grant.alive().is_err() || grant.login_issued {
        return Ok(Json(json!({"status":"expired"})));
    }
    // Terminal decisions take precedence over polling throttles.
    match &grant.decision {
        Decision::Failed(problem) => return Err((*problem).into()),
        Decision::Denied => return Ok(Json(json!({"status":"denied"}))),
        _ => {}
    }
    let now = Instant::now();
    if grant
        .last_poll
        .is_some_and(|last| now.saturating_duration_since(last) < POLL_INTERVAL)
    {
        return Err(ApiError::rate(POLL_INTERVAL.as_secs()));
    }
    grant.last_poll = Some(now);
    let subject = match &grant.decision {
        Decision::Failed(problem) => return Err((*problem).into()),
        Decision::Pending => return Ok(Json(json!({"status":"pending"}))),
        Decision::Denied => return Ok(Json(json!({"status":"denied"}))),
        Decision::Authorized(subject) => subject.clone(),
    };
    // Serialize credential issuance with a ban so revocation cannot finish
    // before this request writes a late credential that survives an unban.
    let _policy_update = broker.user_policy_updates.lock().await;
    let owner = subject.clone();
    let account = match broker
        .portal
        .storage(move |store| store.account(&owner))
        .await
    {
        Ok(account) => account,
        Err(problem) => {
            if problem == Problem::Banned {
                grant.decision = Decision::Failed(problem);
            }
            return Err(problem.into());
        }
    };
    let identity = crate::identity::Identity {
        provider: "github".into(),
        subject: subject.clone(),
        login: account.github_login.clone(),
    };
    let public_identity = crate::device::account_identity_json(&identity, &account)?;
    if grant.registration_only {
        grant.login_issued = true;
        return Ok(Json(
            json!({"status":"registered","identity":public_identity}),
        ));
    }
    broker
        .portal
        .record_audit(
            Some(&identity),
            crate::history::AuditAction::Login,
            crate::history::AuditOutcome::Pending,
        )
        .await?;
    // The async grant mutex serializes competing poll requests through the durable
    // write. A successful grant can mint exactly one controller credential.
    let (token, expires) = broker
        .portal
        .client_storage(move |store| store.login(subject))
        .await?;
    grant.login_issued = true;
    Ok(Json(json!({"status":"authorized","access_token":token,
        "identity":public_identity,"expires_in":expires})))
}
fn client_consent_page(
    name: &str,
    code: &str,
    csrf: &str,
    account: &Account,
    requested_username: Option<&str>,
    public_url: &str,
) -> String {
    let username_note = requested_username
        .filter(|name| account.handle.as_deref() != Some(*name))
        .map(|name| format!(
            "<p>客户端请求使用用户名：<strong>{}</strong>。首次选择用户名时会保存此名称；之后可在账户页修改。</p>",
            escape(name),
        )).unwrap_or_default();
    view::page(
        "登录 ssmcp 客户端",
        &format!(
            "<section class=card><p class=eyebrow>SSMCP · CLIENT LOGIN</p><h1>登录客户端</h1>        <p>设备：<strong>{}</strong></p><p>请核对终端中的代码：</p><pre>{}</pre>        <p>平台账户：<strong>{}</strong></p>{}        <p>允许后，此客户端可以连接和控制你已授权的会话，并为其他设备生成临时接入命令。登录有效期为 30 天，可随时通过 ssmcp logout 撤销。</p>        <form method=post action=\"/setup/authorize\"><input type=hidden name=csrf value=\"{}\">        <input type=hidden name=code value=\"{}\">        <button class=button type=submit name=decision value=allow>允许登录</button>        <button class=secondary type=submit name=decision value=deny>拒绝</button></form></section>",
            escape(name),
            escape(code),
            escape(&view::account_address(
                account.handle.as_deref(),
                public_url
            )),
            username_note,
            escape(csrf),
            escape(code)
        ),
    )
}
fn registration_page(code: &str, csrf: &str, username: &str) -> String {
    view::page(
        "注册用户名",
        &format!(
            r#"<section class="card"><h1>注册用户名</h1><p>注册 <strong>{}</strong>。请核对本次请求的验证码：<code>{}</code>。</p><form method="post" action="/setup/authorize"><input type="hidden" name="csrf" value="{}"><input type="hidden" name="code" value="{}"><button type="submit" name="decision" value="allow">确认注册</button><button type="submit" name="decision" value="deny">拒绝</button></form></section>"#,
            escape(username),
            escape(code),
            escape(csrf),
            escape(code)
        ),
    )
}

fn client_completed_page(allowed: bool, registration_only: bool) -> String {
    if registration_only {
        return view::page(
            "用户名注册",
            if allowed {
                "<section class=card><h1>注册完成</h1><a href=/>查看账户</a></section>"
            } else {
                "<section class=card><h1>已拒绝注册</h1><a href=/>查看账户</a></section>"
            },
        );
    }
    if !allowed {
        return view::page(
            "已拒绝客户端登录",
            "<section class=card><p class=eyebrow>SSMCP · CLIENT LOGIN</p><h1>已拒绝登录</h1><p>没有为此设备创建登录凭据。需要登录时，请从终端重新开始。</p><a class=button href=/>查看账户</a></section>",
        );
    }
    view::page(
        "客户端已获准登录",
        "<section class=card><p class=eyebrow>SSMCP · CLIENT LOGIN</p><h1>已允许登录</h1>        <p>返回终端继续。客户端会保存此设备的登录凭据。</p><a class=button href=/>查看账户</a></section>",
    )
}
