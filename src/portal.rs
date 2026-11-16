//! Browser account portal, sharing the MCP gateway's verified GitHub identity provider.
//! A separate privileged synchronizer provisions restricted SSH accounts and public keys.
mod store;
#[cfg(test)]
mod tests;
mod view;

use crate::{broker::Broker, identity::Identity};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use store::{PublicKey, Store};
use subtle::ConstantTimeEq;

const COOKIE: &str = "__Host-ssmcp-portal";
const LOGIN_TTL: Duration = Duration::from_secs(600);
const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
const MAX_SESSIONS: usize = 4096;
type DigestKey = [u8; 32];

#[derive(Default)]
pub struct Portal {
    browser: Mutex<BrowserState>,
    store: Option<Arc<Mutex<Store>>>,
}

#[derive(Default)]
struct BrowserState {
    sessions: HashMap<DigestKey, Session>,
    pending: HashMap<DigestKey, Pending>,
    rates: HashMap<String, Rate>,
}

struct Session {
    subject: Option<String>,
    csrf: String,
    expires: Instant,
}
struct Pending {
    browser: DigestKey,
    verifier: String,
    expires: Instant,
}
struct Rate {
    start: Instant,
    count: usize,
}

impl BrowserState {
    fn prune(&mut self) {
        let now = Instant::now();
        self.sessions.retain(|_, s| s.expires > now);
        self.pending
            .retain(|_, p| p.expires > now && self.sessions.contains_key(&p.browser));
        self.rates
            .retain(|_, r| now.duration_since(r.start) < Duration::from_secs(60));
    }
    fn make_room_for_session(&mut self) -> Result<(), Problem> {
        self.prune();
        if self.sessions.len() < MAX_SESSIONS {
            return Ok(());
        }
        let pending: HashSet<_> = self.pending.values().map(|p| p.browser).collect();
        let oldest = self
            .sessions
            .iter()
            .filter(|(key, session)| session.subject.is_none() && !pending.contains(*key))
            .min_by_key(|(_, session)| session.expires)
            .map(|(key, _)| *key);
        let key = oldest.ok_or(Problem::Busy)?;
        self.sessions.remove(&key);
        Ok(())
    }
    fn charge(&mut self, name: String, maximum: usize) -> Result<(), Problem> {
        self.prune();
        if !self.rates.contains_key(&name) && self.rates.len() >= MAX_SESSIONS * 2 {
            return Err(Problem::Busy);
        }
        let rate = self.rates.entry(name).or_insert(Rate {
            start: Instant::now(),
            count: 0,
        });
        if rate.count >= maximum {
            return Err(Problem::Busy);
        }
        rate.count += 1;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Problem {
    Invalid,
    Unauthorized,
    Csrf,
    Busy,
    Storage,
    Limit,
    Duplicate,
    NotFound,
    Disabled,
}
impl Problem {
    fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Csrf => StatusCode::FORBIDDEN,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::Storage => StatusCode::SERVICE_UNAVAILABLE,
            Self::Limit | Self::Duplicate => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Disabled => StatusCode::SERVICE_UNAVAILABLE,
            Self::Invalid => StatusCode::BAD_REQUEST,
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::Invalid => "输入无效。仅接受单行 ssh-ed25519 公钥，不接受私钥、授权选项或证书。",
            Self::Unauthorized => "登录已失效，请回到首页重新登录。",
            Self::Csrf => "页面验证未通过，请回到首页刷新后重试。",
            Self::Busy => "操作过于频繁或服务繁忙，请稍后重试。",
            Self::Storage => "账户存储暂不可用，本次操作未确认完成，请稍后重新查看。",
            Self::Limit => "已达到存储上限；每个账户最多保存 20 把公钥。",
            Self::Duplicate => "你的账户中已存在这把公钥。",
            Self::NotFound => "你的账户中没有这把公钥。",
            Self::Disabled => "账户功能尚未启用，请联系管理员配置 GitHub 登录与账户存储。",
        }
    }
}
impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut response = (self.status(), Html(view::page("暂未完成", &format!("<section class=card><h1>暂未完成</h1><p role=alert>{}</p><a class=button href=/>返回首页</a></section>", self.message())))).into_response();
        if self == Self::Busy {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        response
    }
}

fn digest(value: &str) -> DigestKey {
    Sha256::digest(value.as_bytes()).into()
}
fn random() -> Result<String, Problem> {
    let mut bytes = [0; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| Problem::Storage)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
fn valid_secret(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn browser_cookie(headers: &HeaderMap) -> Option<&str> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for part in value.to_str().ok()?.split(';') {
            if let Some((name, value)) = part.trim().split_once('=')
                && name == COOKIE
            {
                if found.is_some() || !valid_secret(value) {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}
fn set_cookie(response: &mut Response, value: &str, ttl: u64) {
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={ttl}"
        ))
        .expect("bounded base64url cookie"),
    );
}

impl Portal {
    /// Opt-in durable portal storage. Startup fails if permissions, format or locking are unsafe.
    pub fn open(path: &Path) -> crate::Result<Self> {
        Ok(Self {
            browser: Mutex::default(),
            store: Some(Arc::new(Mutex::new(Store::open(path)?))),
        })
    }
    fn enabled(&self, broker: &Broker) -> bool {
        self.store.is_some() && broker.identities.enabled("github")
    }
    async fn storage<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Store) -> Result<T, Problem> + Send + 'static,
    ) -> Result<T, Problem> {
        let store = self.store.clone().ok_or(Problem::Disabled)?;
        tokio::task::spawn_blocking(move || {
            let mut guard = store.lock().map_err(|_| Problem::Storage)?;
            operation(&mut guard)
        })
        .await
        .map_err(|_| Problem::Storage)?
    }
    fn session(&self, headers: &HeaderMap) -> Result<(DigestKey, String, Option<String>), Problem> {
        let raw = browser_cookie(headers).ok_or(Problem::Unauthorized)?;
        let key = digest(raw);
        let mut browser = self.browser.lock().map_err(|_| Problem::Storage)?;
        browser.prune();
        let session = browser.sessions.get(&key).ok_or(Problem::Unauthorized)?;
        Ok((key, session.csrf.clone(), session.subject.clone()))
    }
    fn verify(
        &self,
        headers: &HeaderMap,
        form: &HashMap<String, String>,
        origin: &str,
    ) -> Result<(DigestKey, Option<String>), Problem> {
        let mut origins = headers.get_all(header::ORIGIN).iter();
        if origins.next().and_then(|v| v.to_str().ok()) != Some(origin) || origins.next().is_some()
        {
            return Err(Problem::Csrf);
        }
        if headers
            .get("sec-fetch-site")
            .is_some_and(|v| v != "same-origin" && v != "none")
        {
            return Err(Problem::Csrf);
        }
        let (key, csrf, subject) = self.session(headers)?;
        let supplied = form.get("csrf").ok_or(Problem::Csrf)?;
        if !bool::from(digest(supplied).ct_eq(&digest(&csrf))) {
            return Err(Problem::Csrf);
        }
        Ok((key, subject))
    }
}

pub fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/", get(home))
        .route(
            "/portal.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    view::CSS,
                )
            }),
        )
        .route("/account/login", post(login))
        .route("/account/logout", post(logout))
        .route("/account/keys", post(add_key))
        .route("/account/keys/delete", post(delete_key))
        .layer(middleware::map_response(security_headers))
}

async fn security_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("content-security-policy", HeaderValue::from_static("default-src 'none'; style-src 'self'; form-action 'self' https://github.com; base-uri 'none'; frame-ancestors 'none'"));
    h.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    response
}

async fn home(State(broker): State<Arc<Broker>>, headers: HeaderMap) -> Result<Response, Problem> {
    if !broker.portal.enabled(&broker) {
        return Ok(Html(view::home(None, None, false)).into_response());
    }
    let mut new_cookie = None;
    let (_, csrf, subject) = match broker.portal.session(&headers) {
        Ok(session) => session,
        Err(Problem::Unauthorized) => {
            let raw = random()?;
            let csrf = random()?;
            let key = digest(&raw);
            let mut state = broker.portal.browser.lock().map_err(|_| Problem::Storage)?;
            state.make_room_for_session()?;
            state.sessions.insert(
                key,
                Session {
                    subject: None,
                    csrf: csrf.clone(),
                    expires: Instant::now() + LOGIN_TTL,
                },
            );
            new_cookie = Some(raw);
            (key, csrf, None)
        }
        Err(error) => return Err(error),
    };
    let account = match subject {
        Some(subject) => Some(broker.portal.storage(move |s| s.account(&subject)).await?),
        None => None,
    };
    let mut response = Html(view::home(account.as_ref(), Some(&csrf), true)).into_response();
    if let Some(raw) = new_cookie {
        set_cookie(&mut response, &raw, LOGIN_TTL.as_secs());
    }
    Ok(response)
}

fn parameters(raw: &str) -> Result<HashMap<String, String>, Problem> {
    if raw.len() > 8192 {
        return Err(Problem::Invalid);
    }
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(raw).map_err(|_| Problem::Invalid)?;
    let mut map = HashMap::new();
    for (key, value) in pairs {
        if map.insert(key, value).is_some() {
            return Err(Problem::Invalid);
        }
    }
    Ok(map)
}
async fn form(request: Request) -> Result<(HeaderMap, HashMap<String, String>), Problem> {
    let headers = request.headers().clone();
    let mut types = headers.get_all(header::CONTENT_TYPE).iter();
    if types
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        != Some("application/x-www-form-urlencoded")
        || types.next().is_some()
    {
        return Err(Problem::Invalid);
    }
    let bytes = tokio::time::timeout(Duration::from_secs(10), to_bytes(request.into_body(), 8192))
        .await
        .map_err(|_| Problem::Invalid)?
        .map_err(|_| Problem::Invalid)?;
    let values = parameters(std::str::from_utf8(&bytes).map_err(|_| Problem::Invalid)?)?;
    Ok((headers, values))
}
fn only(form: &HashMap<String, String>, fields: &[&str]) -> Result<(), Problem> {
    if form.len() != fields.len() || fields.iter().any(|key| !form.contains_key(*key)) {
        return Err(Problem::Invalid);
    }
    Ok(())
}

async fn login(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    if !broker.portal.enabled(&broker) {
        return Err(Problem::Disabled);
    }
    let (headers, form) = form(request).await?;
    only(&form, &["csrf"])?;
    let (browser_key, _) = broker.portal.verify(&headers, &form, &broker.public_url)?;
    let state = format!("portal_{}", random()?);
    let verifier = random()?;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let url = broker
        .identities
        .authorization_url("github", &state, &challenge)
        .map_err(|_| Problem::Disabled)?;
    let mut browser = broker.portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.charge("login:global".into(), 60)?;
    browser.charge(format!("login:{}", URL_SAFE_NO_PAD.encode(browser_key)), 3)?;
    browser.pending.retain(|_, p| p.browser != browser_key);
    if browser.pending.len() >= 1024 {
        return Err(Problem::Busy);
    }
    browser.pending.insert(
        digest(&state),
        Pending {
            browser: browser_key,
            verifier,
            expires: Instant::now() + LOGIN_TTL,
        },
    );
    Ok(Redirect::to(&url).into_response())
}

/// Distinct state namespace shares the already configured provider callback and PKCE exchange.
pub(crate) fn is_callback(query: Option<&str>) -> bool {
    // Route ambiguous/duplicate portal states here too, then reject them during strict parsing.
    serde_urlencoded::from_str::<Vec<(String, String)>>(query.unwrap_or("")).is_ok_and(|pairs| {
        pairs
            .iter()
            .any(|(k, v)| k == "state" && v.starts_with("portal_"))
    })
}
pub(crate) async fn callback(broker: Arc<Broker>, query: &str, headers: HeaderMap) -> Response {
    let response = match finish_callback(&broker, query, &headers).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    security_headers(response).await
}
async fn finish_callback(
    broker: &Broker,
    query: &str,
    headers: &HeaderMap,
) -> Result<Response, Problem> {
    if !broker.portal.enabled(broker) {
        return Err(Problem::Disabled);
    }
    let values = parameters(query)?;
    let state = values
        .get("state")
        .filter(|s| s.strip_prefix("portal_").is_some_and(valid_secret))
        .ok_or(Problem::Csrf)?;
    let (browser_key, _, _) = broker.portal.session(headers)?;
    let pending = {
        let mut browser = broker.portal.browser.lock().map_err(|_| Problem::Storage)?;
        browser.prune();
        let pending = browser.pending.get(&digest(state)).ok_or(Problem::Csrf)?;
        if !bool::from(pending.browser.ct_eq(&browser_key)) {
            return Err(Problem::Csrf);
        }
        browser
            .pending
            .remove(&digest(state))
            .ok_or(Problem::Csrf)?
    };
    if values.contains_key("error") {
        return Err(Problem::Unauthorized);
    }
    let code = values.get("code").ok_or(Problem::Invalid)?;
    let identity = broker
        .identities
        .exchange("github", code, &pending.verifier)
        .await
        .map_err(|_| Problem::Unauthorized)?;
    finish_identity(&broker.portal, browser_key, identity).await
}
async fn finish_identity(
    portal: &Portal,
    previous: DigestKey,
    identity: Identity,
) -> Result<Response, Problem> {
    let account = portal.storage(move |s| s.login(&identity)).await?;
    let raw = random()?;
    let csrf = random()?;
    let mut browser = portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.prune();
    // A logout/expired session during the upstream exchange must not recreate a login.
    if browser.sessions.remove(&previous).is_none() {
        return Err(Problem::Unauthorized);
    }
    browser.pending.retain(|_, p| p.browser != previous);
    browser.sessions.insert(
        digest(&raw),
        Session {
            subject: Some(account.subject),
            csrf,
            expires: Instant::now() + SESSION_TTL,
        },
    );
    let mut response = Redirect::to("/#account").into_response();
    set_cookie(&mut response, &raw, SESSION_TTL.as_secs());
    Ok(response)
}

async fn logout(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    let (headers, form) = form(request).await?;
    only(&form, &["csrf"])?;
    let (key, _) = broker.portal.verify(&headers, &form, &broker.public_url)?;
    let mut browser = broker.portal.browser.lock().map_err(|_| Problem::Storage)?;
    browser.sessions.remove(&key);
    browser.pending.retain(|_, p| p.browser != key);
    let mut response = Redirect::to("/").into_response();
    set_cookie(&mut response, "", 0);
    Ok(response)
}
async fn key_action(
    broker: &Broker,
    headers: &HeaderMap,
    form: &HashMap<String, String>,
) -> Result<String, Problem> {
    let (_, subject) = broker.portal.verify(headers, form, &broker.public_url)?;
    let subject = subject.ok_or(Problem::Unauthorized)?;
    broker
        .portal
        .browser
        .lock()
        .map_err(|_| Problem::Storage)?
        .charge(format!("keys:{subject}"), 20)?;
    Ok(subject)
}
async fn add_key(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    let (headers, form) = form(request).await?;
    only(&form, &["csrf", "label", "public_key"])?;
    let owner = key_action(&broker, &headers, &form).await?;
    let key = parse_key(&form["public_key"], &form["label"])?;
    broker.portal.storage(move |s| s.add(&owner, key)).await?;
    Ok(Redirect::to("/#keys").into_response())
}
async fn delete_key(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let (headers, form) = form(request).await?;
    only(&form, &["csrf", "fingerprint"])?;
    let owner = key_action(&broker, &headers, &form).await?;
    let fingerprint = form["fingerprint"].clone();
    if fingerprint.len() != 50 || !fingerprint.starts_with("SHA256:") {
        return Err(Problem::Invalid);
    }
    broker
        .portal
        .storage(move |s| s.remove(&owner, &fingerprint))
        .await?;
    Ok(Redirect::to("/#keys").into_response())
}

/// Accept a single RFC 4253 Ed25519 blob, not authorized_keys options or certificates.
fn parse_key(raw: &str, label: &str) -> Result<PublicKey, Problem> {
    if raw.len() > 1024 {
        return Err(Problem::Invalid);
    }
    let raw = raw.trim_ascii();
    if raw.is_empty()
        || raw.chars().any(char::is_control)
        || label.len() > 80
        || label.chars().any(char::is_control)
    {
        return Err(Problem::Invalid);
    }
    let mut fields = raw.split_ascii_whitespace();
    if fields.next() != Some("ssh-ed25519") {
        return Err(Problem::Invalid);
    }
    let encoded = fields.next().ok_or(Problem::Invalid)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| Problem::Invalid)?;
    // string("ssh-ed25519") + string(32-byte public key); no trailing data.
    if bytes.len() != 51
        || &bytes[..19] != b"\0\0\0\x0bssh-ed25519\0\0\0\x20"
        || bytes[19..].iter().all(|b| *b == 0)
        || STANDARD.encode(&bytes) != encoded
    {
        return Err(Problem::Invalid);
    }
    Ok(PublicKey {
        fingerprint: format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&bytes))),
        label: label.trim().to_owned(),
        public_key: format!("ssh-ed25519 {}", STANDARD.encode(bytes)),
    })
}
