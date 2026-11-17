//! Short-lived, browser-consented client setup. Device secrets stay in the CLI;
//! only their SHA-256 digest is retained here. No GitHub tokens or private keys
//! are exposed, persisted in grants, or accepted by the setup API.

use super::{
    DigestKey, LOGIN_TTL, Portal, Problem, Session, digest, form, only, parameters, parse_key,
    random, set_cookie, start_login, store::Account, valid_secret, view,
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
const MAX_BODY: usize = 4096;
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
    expires: Instant,
    decision: Decision,
    last_poll: Option<Instant>,
    registered_fingerprint: Option<String>,
    registration_saved: bool,
}
#[derive(Clone)]
enum Decision {
    Pending,
    Authorized(String),
    Denied,
}
#[derive(Clone)]
pub(super) struct ReturnTo {
    device: DigestKey,
    code: String,
}
impl ReturnTo {
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
                    expires,
                    decision: Decision::Pending,
                    last_poll: None,
                    registered_fingerprint: None,
                    registration_saved: false,
                })),
            },
        );
        self.codes.insert(user_code.clone(), key);
        Ok((device_code, user_code))
    }
    fn by_device(&mut self, secret: &str) -> Result<Option<Arc<AsyncMutex<Grant>>>, ApiError> {
        if !valid_secret(secret) {
            return Err(ApiError::invalid(
                "device_code must be the secret returned by setup/start",
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
    fn owner(&self) -> Result<String, ApiError> {
        self.alive()?;
        match &self.decision {
            Decision::Authorized(owner) => Ok(owner.clone()),
            Decision::Pending => Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "authorization_required",
                "Approve this computer in the browser before registering a key.",
            )),
            Decision::Denied => Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "access_denied",
                "The browser denied this setup request.",
            )),
        }
    }
    fn decide(&mut self, owner: &str, allow: bool) -> Result<(), ApiError> {
        self.alive()?;
        match &self.decision {
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
            Problem::Busy => Self::rate(60),
            Problem::Limit => Self::new(
                StatusCode::CONFLICT,
                "key_limit",
                "This account has reached its SSH public key limit.",
            ),
            Problem::Duplicate => Self::new(
                StatusCode::CONFLICT,
                "key_already_registered",
                "This setup request already registered another public key.",
            ),
            Problem::NotFound | Problem::SessionNotFound => Self::expired(),
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
    .map_err(|_| ApiError::invalid("The JSON request body exceeds 4096 bytes."))?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::invalid("The JSON request contains invalid, duplicate or unknown fields.")
    })
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    client_name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    device_code: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Register {
    device_code: String,
    public_key: String,
    label: String,
}
fn public_account(account: Account) -> Value {
    json!({"github_login":account.github_login,"username":account.username,"keys":account.keys})
}

pub(super) fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/setup", get(page))
        .route("/setup/authorize", post(authorize))
        .route("/api/setup/start", post(start).fallback(method_not_allowed))
        .route("/api/setup/poll", post(poll).fallback(method_not_allowed))
        .route(
            "/api/setup/key",
            post(register).fallback(method_not_allowed),
        )
        .route(
            "/api/setup/cancel",
            post(cancel).fallback(method_not_allowed),
        )
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "invalid_request",
        "Use POST with an application/json request body for this endpoint.",
    )
}

async fn start(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let request: Start = body(request, &broker.public_url).await?;
    let name = request.client_name.trim();
    if name.is_empty() || name.len() > 80 || name.chars().any(char::is_control) {
        return Err(ApiError::invalid(
            "client_name must contain 1–80 UTF-8 bytes without control characters.",
        ));
    }
    let (device_code, user_code) = {
        let mut state = lock(&broker.portal)?;
        state.charge("start", 60)?;
        state.issue(name.into())?
    };
    let uri = format!(
        "{}/setup?code={user_code}",
        broker.public_url.trim_end_matches('/')
    );
    Ok(Json(json!({
        "device_code":device_code,"user_code":user_code,
        "verification_uri_complete":uri,"expires_in":TTL.as_secs(),"interval":POLL_INTERVAL.as_secs()
    })))
}
async fn poll(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let request: Device = body(request, &broker.public_url).await?;
    let grant = {
        let mut state = lock(&broker.portal)?;
        state.charge("poll", 18000)?;
        state.by_device(&request.device_code)?
    };
    let Some(grant) = grant else {
        return Ok(Json(json!({"status":"expired"})));
    };
    let mut grant = grant.lock().await;
    if grant.alive().is_err() {
        return Ok(Json(json!({"status":"expired"})));
    }
    let now = Instant::now();
    if grant
        .last_poll
        .is_some_and(|previous| now.saturating_duration_since(previous) < POLL_INTERVAL)
    {
        return Err(ApiError::rate(POLL_INTERVAL.as_secs()));
    }
    grant.last_poll = Some(now);
    match &grant.decision {
        Decision::Pending => Ok(Json(json!({"status":"pending"}))),
        Decision::Denied => Ok(Json(json!({"status":"denied"}))),
        Decision::Authorized(owner) => {
            let owner = owner.clone();
            let account = broker
                .portal
                .storage(move |store| store.account(&owner))
                .await?;
            Ok(Json(
                json!({"status":"authorized","account":public_account(account)}),
            ))
        }
    }
}
async fn register(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    enabled(&broker)?;
    let request: Register = body(request, &broker.public_url).await?;
    let key = parse_key(&request.public_key, &request.label)?;
    let grant = {
        let mut state = lock(&broker.portal)?;
        state.charge("key", 1000)?;
        state
            .by_device(&request.device_code)?
            .ok_or_else(ApiError::expired)?
    };
    let mut grant = grant.lock().await;
    let owner = grant.owner()?;
    if grant
        .registered_fingerprint
        .as_ref()
        .is_some_and(|existing| existing != &key.fingerprint)
    {
        return Err(Problem::Duplicate.into());
    }
    if grant.registered_fingerprint.is_none() {
        broker
            .portal
            .browser
            .lock()
            .map_err(|_| ApiError::unavailable())?
            .charge(format!("setup:key:{owner}"), 20)?;
        // Reserve before storage I/O. Cancellation/ambiguous durability never
        // grants permission to try a second different public key.
        grant.registered_fingerprint = Some(key.fingerprint.clone());
    }
    // The per-grant async mutex serializes registration with cancel/consent.
    // The store closure rechecks the owner's keys atomically with persistence.
    let already_saved = grant.registration_saved;
    let identity = broker.portal.identity_for_subject(owner.clone()).await?;
    if !already_saved {
        broker
            .portal
            .record_audit(
                Some(&identity),
                crate::history::AuditAction::KeyAdded,
                crate::history::AuditOutcome::Pending,
            )
            .await?;
    }
    let result = broker
        .portal
        .storage(move |store| {
            let account = store.account(&owner)?;
            if let Some(existing) = account
                .keys
                .iter()
                .find(|stored| stored.fingerprint == key.fingerprint)
            {
                if existing.public_key != key.public_key {
                    return Err(Problem::Invalid);
                }
                return Ok(account);
            }
            if already_saved {
                return Err(Problem::NotFound);
            }
            store.add(&owner, key)?;
            store.account(&owner)
        })
        .await;
    if result.is_ok() {
        // Preserve idempotency even if the completion audit cannot be written.
        grant.registration_saved = true;
    }
    if !already_saved {
        broker
            .portal
            .record_audit(
                Some(&identity),
                crate::history::AuditAction::KeyAdded,
                match &result {
                    Ok(_) => crate::history::AuditOutcome::Success,
                    Err(Problem::Storage | Problem::Busy) => crate::history::AuditOutcome::Error,
                    Err(_) => crate::history::AuditOutcome::Denied,
                },
            )
            .await?;
    }
    let account = result.map_err(|problem| if problem == Problem::NotFound {
        ApiError::new(StatusCode::CONFLICT, "key_registration_revoked", "This grant already saved its key, which has since been removed. Start a new setup request to register it again.")
    } else { problem.into() })?;
    Ok(Json(
        json!({"status":"saved","account":public_account(account)}),
    ))
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
fn consent_page(name: &str, code: &str, csrf: &str, account: &Account) -> String {
    view::page(
        "连接这台电脑",
        &format!(
            "<section class=card><p class=eyebrow>SSMCP · COMPUTER USE</p><h1>连接这台电脑</h1>\
        <p>正在为 <strong>{}</strong> 设置连接。</p><p>请确认下面的代码与你刚启动的终端一致：</p>\
        <pre aria-label=\"连接确认代码\">{}</pre><p>GitHub 账户：<strong>{}</strong></p>\
        <p>允许后，这台电脑可以读取你账户中的 SSH 公钥，并登记一把新的 Ed25519 公钥。私钥始终保留在电脑上。</p>\
        <p>只批准你自己刚发起的请求。不要批准聊天、邮件或其他人发来的代码。</p>\
        <form method=post action=\"/setup/authorize\"><input type=hidden name=\"csrf\" value=\"{}\">\
        <input type=hidden name=\"code\" value=\"{}\">\
        <button class=button type=submit name=\"decision\" value=\"allow\">允许连接并登记公钥</button>\
        <button class=secondary type=submit name=\"decision\" value=\"deny\">拒绝</button></form></section>",
            escape(name),
            escape(code),
            escape(&account.github_login),
            escape(csrf),
            escape(code)
        ),
    )
}
fn completed_page(allowed: bool) -> String {
    let (title, message) = if allowed {
        (
            "已允许这台电脑",
            "请返回终端继续。客户端会检查已有 SSH 公钥，完成本机连接配置。",
        )
    } else {
        (
            "已拒绝连接",
            "这次请求不能再读取账户信息或登记公钥。需要连接时，请从终端重新开始。",
        )
    };
    view::page(
        title,
        &format!(
            "<section class=card><p class=eyebrow>SSMCP · COMPUTER USE</p><h1>{title}</h1><p>{message}</p><a class=button href=/>查看账户</a></section>"
        ),
    )
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
    let (name, decision) = {
        let grant = grant.lock().await;
        grant.alive()?;
        (grant.client_name.clone(), grant.decision.clone())
    };
    if matches!(decision, Decision::Denied) {
        return Ok(Html(completed_page(false)).into_response());
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
        return Ok(Html(completed_page(true)).into_response());
    }
    if subject.is_none() {
        let mut response = start_login(broker, browser, Some(return_to))?;
        if let Some(cookie) = new_cookie {
            set_cookie(&mut response, &cookie, LOGIN_TTL.as_secs());
        }
        return Ok(response);
    }
    let subject = subject.expect("subject checked");
    let account = broker
        .portal
        .storage(move |store| store.account(&subject))
        .await?;
    Ok(Html(consent_page(&name, &values["code"], &csrf, &account)).into_response())
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
    // Verify the signed-in identity still has a durable account. A code alone is
    // never authority: both the authenticated session and origin+CSRF are needed.
    let owner = subject.clone();
    broker
        .portal
        .storage(move |store| store.account(&owner))
        .await?;
    grant.lock().await.decide(&subject, allow)?;
    Ok(Redirect::to(&format!("/setup?code={}", form["code"])).into_response())
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
