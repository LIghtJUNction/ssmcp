//! Browser account portal, sharing the MCP gateway's verified GitHub identity provider.
//! A separate privileged synchronizer provisions restricted SSH accounts and public keys.
mod commerce;
mod dashboard;
mod dashboard_view;
mod guide;
mod setup;
pub(crate) mod store;
#[cfg(test)]
mod tests;
mod traffic;
mod view;

use crate::{
    broker::Broker,
    history::{AuditAction, AuditOutcome},
    identity::Identity,
    management::ManagementClient,
};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
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
    setup: Mutex<setup::SetupState>,
    store: Option<Arc<Mutex<Store>>>,
    credentials: Option<Arc<Mutex<crate::device::Credentials>>>,
    management: Option<ManagementClient>,
    commerce: Option<Arc<crate::commerce::Commerce>>,
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
    return_to: Option<setup::ReturnTo>,
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
pub(crate) enum Problem {
    Invalid,
    Unauthorized,
    Csrf,
    Busy,
    Storage,
    Limit,
    Duplicate,
    NotFound,
    Disabled,
    Forbidden,
    ManagementDisabled,
    ManagementUnavailable,
    AuditUnavailable,
    SessionNotFound,
    ManagementInvalid,
    UsernameInvalid,
    UsernameTaken,
    UsernameBound,
    Banned,
    TrafficQuotaExceeded,
    TrafficPeriodMismatch,
    TrafficGrantConflict,
}
impl Problem {
    pub(crate) fn status(self) -> StatusCode {
        match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Csrf | Self::Forbidden | Self::Banned => StatusCode::FORBIDDEN,
            Self::Busy => StatusCode::TOO_MANY_REQUESTS,
            Self::TrafficQuotaExceeded => StatusCode::PAYMENT_REQUIRED,
            Self::TrafficPeriodMismatch | Self::TrafficGrantConflict => StatusCode::CONFLICT,
            Self::Storage
            | Self::ManagementDisabled
            | Self::ManagementUnavailable
            | Self::AuditUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Limit | Self::Duplicate | Self::UsernameTaken | Self::UsernameBound => {
                StatusCode::CONFLICT
            }
            Self::NotFound | Self::SessionNotFound => StatusCode::NOT_FOUND,
            Self::Disabled => StatusCode::SERVICE_UNAVAILABLE,
            Self::Invalid | Self::ManagementInvalid | Self::UsernameInvalid => {
                StatusCode::BAD_REQUEST
            }
        }
    }
    fn message(self) -> &'static str {
        match self {
            Self::Invalid => {
                "输入无效。仅接受 OpenSSH 支持的单行 SSH 公钥，不接受私钥或 authorized_keys 授权选项。"
            }
            Self::Unauthorized => "登录已失效，请回到首页重新登录。",
            Self::Csrf => "页面验证未通过，请回到首页刷新后重试。",
            Self::Busy => "操作过于频繁或服务繁忙，请稍后重试。",
            Self::Storage => "账户存储暂不可用，本次操作未确认完成，请稍后重新查看。",
            Self::Limit => "已达到存储上限；每个账户最多保存 20 把公钥。",
            Self::Duplicate => "你的账户中已存在这把公钥。",
            Self::NotFound => "你的账户中没有这把公钥。",
            Self::Disabled => "账户功能尚未启用，请联系管理员配置 GitHub 登录与账户存储。",
            Self::Forbidden => "你的账户无权查看或操作这项资源。",
            Self::ManagementDisabled => "会话管理尚未配置，请联系管理员设置网关管理连接。",
            Self::ManagementUnavailable => "暂时无法连接会话网关，请稍后重试。没有读取到实时状态。",
            Self::AuditUnavailable => {
                "审计记录暂时无法保存，本次操作未确认完成。请在网关恢复后重新核对账户状态。"
            }
            Self::SessionNotFound => "该会话已断开或不属于你的账户，请刷新列表后重试。",
            Self::ManagementInvalid => "管理请求参数无效，请返回会话列表后重试。",
            Self::UsernameInvalid => {
                "用户名须为 3–32 个英文字母、数字、下划线或连字符，以字母开头、字母或数字结尾，且不能使用保留名称。"
            }
            Self::UsernameTaken => {
                "该用户名已绑定其他 GitHub 账户，请换一个名称，或使用原绑定账户登录。"
            }
            Self::UsernameBound => "此 GitHub 账户已有用户名，请使用已绑定的名称连接。",
            Self::Banned => "此账户已被服务器管理员封禁。",
            Self::TrafficQuotaExceeded => {
                "本月流量额度不足。服务器流量有限，经常使用请购买流量包。"
            }
            Self::TrafficPeriodMismatch => {
                "订单所属月份已经结束，无法将旧订单流量计入新月份，请联系管理员处理。"
            }
            Self::TrafficGrantConflict => "订单到账记录与本次请求不一致，请联系管理员核对。",
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
            setup: Mutex::default(),
            store: Some(Arc::new(Mutex::new(Store::open(path)?))),
            credentials: Some(Arc::new(Mutex::new(crate::device::Credentials::open(
                path,
            )?))),
            management: None,
            commerce: None,
        })
    }
    /// The private socket carries only requests authenticated by this portal's sessions.
    pub fn configure_management(&mut self, socket: PathBuf) {
        self.management = Some(ManagementClient::new(socket));
    }

    /// Resolve names from existing local registrations, never from a future claimant.
    pub async fn resolve_admin_usernames(&self, names: &[String]) -> crate::Result<Vec<String>> {
        let mut subjects = Vec::with_capacity(names.len());
        for name in names {
            let handle = crate::username::normalize(name)?;
            let account = self.storage(move |store| store.account_by_handle(&handle))
                .await
                .map_err(|_| "could not read registered administrator usernames")?
                .ok_or("administrator username is not registered; register it before configuring admin_users")?;
            if account.banned {
                return Err("a banned account cannot be configured as an administrator".into());
            }
            subjects.push(account.subject);
        }
        Ok(subjects)
    }

    /// Invalidate browser sessions after a durable ban; account guards also cover races.
    pub(crate) fn revoke_browser_sessions(&self, subject: &str) -> Result<(), Problem> {
        let mut browser = self.browser.lock().map_err(|_| Problem::Storage)?;
        browser
            .sessions
            .retain(|_, session| session.subject.as_deref() != Some(subject));
        let active: HashSet<_> = browser.sessions.keys().copied().collect();
        browser
            .pending
            .retain(|_, pending| active.contains(&pending.browser));
        Ok(())
    }

    pub(crate) async fn identity_for_subject(&self, subject: String) -> Result<Identity, Problem> {
        self.storage(move |store| store.account(&subject))
            .await
            .map(|account| Identity {
                provider: "github".into(),
                subject: account.subject,
                login: account.github_login,
            })
    }

    fn charge_management(&self, subject: &str) -> Result<(), Problem> {
        let mut state = self.browser.lock().map_err(|_| Problem::Storage)?;
        state.charge(format!("management:{subject}"), 120)?;
        state.charge("management:global".into(), 600)
    }

    async fn identity_for_audit(&self, headers: &HeaderMap) -> Option<Identity> {
        let subject = self.session(headers).ok()?.2?;
        self.storage(move |store| store.account(&subject))
            .await
            .ok()
            .map(|account| Identity {
                provider: "github".into(),
                subject: account.subject,
                login: account.github_login,
            })
    }

    async fn record_audit(
        &self,
        identity: Option<&Identity>,
        action: AuditAction,
        outcome: AuditOutcome,
    ) -> Result<(), Problem> {
        let Some(client) = &self.management else {
            return Ok(());
        };
        {
            let mut state = self.browser.lock().map_err(|_| Problem::Storage)?;
            match identity {
                Some(identity) => state.charge(format!("audit:{}", identity.subject), 120)?,
                None => state.charge("audit:anonymous".into(), 30)?,
            }
            state.charge("audit:global".into(), 600)?;
        }
        let result = match identity {
            Some(identity) => client.record_audit(identity, action, outcome).await,
            None => {
                // Invalid/anonymous requests never get a claimed actor from form data.
                let action = if action == AuditAction::Login {
                    AuditAction::Login
                } else {
                    AuditAction::AccessDenied
                };
                client.record_anonymous_audit(action, outcome).await
            }
        };
        result.map_err(|_| Problem::AuditUnavailable)
    }

    pub(crate) fn has_storage(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) fn enabled(&self, broker: &Broker) -> bool {
        self.store.is_some() && broker.identities.enabled("github")
    }
    pub(crate) async fn storage<T: Send + 'static>(
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
    pub(crate) async fn client_storage<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut crate::device::Credentials) -> Result<T, Problem> + Send + 'static,
    ) -> Result<T, Problem> {
        let store = self.credentials.clone().ok_or(Problem::Disabled)?;
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
        .merge(guide::routes())
        .merge(setup::routes())
        .merge(dashboard::routes())
        .merge(commerce::routes())
        .layer(middleware::map_response(security_headers))
}

async fn security_headers(mut response: Response) -> Response {
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Keep Origin on same-origin form POSTs for strict CSRF validation.
    h.insert("referrer-policy", HeaderValue::from_static("same-origin"));
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
        return Ok(Html(view::home(
            None,
            None,
            false,
            false,
            &broker.public_url,
            None,
        ))
        .into_response());
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
    let traffic = if let Some(account) = &account {
        if let Some(management) = &broker.portal.management {
            let identity = Identity {
                provider: "github".into(),
                subject: account.subject.clone(),
                login: account.github_login.clone(),
            };
            management
                .snapshot(&identity, crate::management::Scope::Own)
                .await
                .map_err(|_| Problem::ManagementUnavailable)?
                .traffic
        } else {
            Some(broker.traffic_quota(&account.subject).await?)
        }
    } else {
        None
    };
    let mut response = Html(view::home(
        account.as_ref(),
        Some(&csrf),
        true,
        broker.portal.management.is_some(),
        &broker.public_url,
        traffic.as_ref(),
    ))
    .into_response();
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

fn audit_outcome(result: &Result<Response, Problem>) -> AuditOutcome {
    match result {
        Ok(_) => AuditOutcome::Success,
        Err(
            Problem::Storage
            | Problem::Busy
            | Problem::Disabled
            | Problem::ManagementDisabled
            | Problem::ManagementUnavailable
            | Problem::AuditUnavailable,
        ) => AuditOutcome::Error,
        Err(_) => AuditOutcome::Denied,
    }
}

fn completed_outcome(result: &Result<Response, Problem>) -> Option<AuditOutcome> {
    // Do not turn rate limits or a failed audit write into another audit request.
    (!matches!(result, Err(Problem::Busy | Problem::AuditUnavailable)))
        .then(|| audit_outcome(result))
}

async fn record_result(
    portal: &Portal,
    identity: Option<&Identity>,
    action: AuditAction,
    outcome: Option<AuditOutcome>,
) -> Result<(), Problem> {
    if let Some(outcome) = outcome {
        portal.record_audit(identity, action, outcome).await?;
    }
    Ok(())
}

async fn login(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    let identity = broker.portal.identity_for_audit(request.headers()).await;
    let result = login_inner(State(broker.clone()), request).await;
    // A redirect starts OAuth; a successful login is recorded only after identity verification.
    if result.is_err() {
        record_result(
            &broker.portal,
            identity.as_ref(),
            AuditAction::Login,
            completed_outcome(&result),
        )
        .await?;
    }
    result
}

async fn logout(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    let identity = broker.portal.identity_for_audit(request.headers()).await;
    let result = logout_inner(State(broker.clone()), request).await;
    record_result(
        &broker.portal,
        identity.as_ref(),
        AuditAction::Logout,
        completed_outcome(&result),
    )
    .await?;
    result
}

async fn add_key(State(broker): State<Arc<Broker>>, request: Request) -> Result<Response, Problem> {
    let identity = broker.portal.identity_for_audit(request.headers()).await;
    let result = add_key_inner(State(broker.clone()), request).await;
    record_result(
        &broker.portal,
        identity.as_ref(),
        AuditAction::KeyAdded,
        completed_outcome(&result),
    )
    .await?;
    result
}

async fn delete_key(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let identity = broker.portal.identity_for_audit(request.headers()).await;
    let result = delete_key_inner(State(broker.clone()), request).await;
    record_result(
        &broker.portal,
        identity.as_ref(),
        AuditAction::KeyRemoved,
        completed_outcome(&result),
    )
    .await?;
    result
}

async fn login_inner(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    if !broker.portal.enabled(&broker) {
        return Err(Problem::Disabled);
    }
    let (headers, form) = form(request).await?;
    only(&form, &["csrf"])?;
    let (browser_key, _) = broker.portal.verify(&headers, &form, &broker.public_url)?;
    start_login(&broker, browser_key, None)
}

fn start_login(
    broker: &Broker,
    browser_key: DigestKey,
    return_to: Option<setup::ReturnTo>,
) -> Result<Response, Problem> {
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
            return_to,
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
    let result = finish_callback(&broker, query, &headers).await;
    if let Err(error) = &result
        && *error != Problem::AuditUnavailable
        && *error != Problem::Busy
    {
        let identity = broker.portal.identity_for_audit(&headers).await;
        if let Err(error) = broker
            .portal
            .record_audit(
                identity.as_ref(),
                AuditAction::Login,
                audit_outcome(&result),
            )
            .await
        {
            return security_headers(error.into_response()).await;
        }
    }
    security_headers(match result {
        Ok(response) => response,
        Err(error) => error.into_response(),
    })
    .await
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
    // Rotate the browser session under the same lock as account revocation.
    let result = {
        let _policy_update = broker.user_policy_updates.lock().await;
        finish_identity(&broker.portal, browser_key, identity).await
    };
    let mut response = match result {
        Ok(response) => response,
        Err(problem) => {
            // Release the policy lock before taking the grant lock: polling
            // holds the grant lock while serializing credential issuance.
            if problem == Problem::Banned
                && let Some(return_to) = &pending.return_to
            {
                return_to.fail(&broker.portal, problem).await?;
            }
            return Err(problem);
        }
    };
    if let Some(return_to) = pending.return_to {
        let location = return_to.location(&broker.portal)?;
        response.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&location).map_err(|_| Problem::Invalid)?,
        );
    }
    Ok(response)
}
async fn finish_identity(
    portal: &Portal,
    previous: DigestKey,
    identity: Identity,
) -> Result<Response, Problem> {
    {
        let mut browser = portal.browser.lock().map_err(|_| Problem::Storage)?;
        browser.prune();
        if !browser.sessions.contains_key(&previous) {
            return Err(Problem::Unauthorized);
        }
    }
    let audit_identity = identity.clone();
    portal
        .record_audit(
            Some(&audit_identity),
            AuditAction::Login,
            AuditOutcome::Pending,
        )
        .await?;
    let account = portal.storage(move |s| s.login(&identity)).await?;
    let raw = random()?;
    let csrf = random()?;
    {
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
    }
    if let Err(error) = portal
        .record_audit(
            Some(&audit_identity),
            AuditAction::Login,
            AuditOutcome::Success,
        )
        .await
    {
        // No unaudited authenticated browser session survives a failed audit write.
        portal
            .browser
            .lock()
            .map_err(|_| Problem::Storage)?
            .sessions
            .remove(&digest(&raw));
        return Err(error);
    }
    let mut response = Redirect::to("/#account").into_response();
    set_cookie(&mut response, &raw, SESSION_TTL.as_secs());
    Ok(response)
}

async fn logout_inner(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let (headers, form) = form(request).await?;
    only(&form, &["csrf"])?;
    let (key, subject) = broker.portal.verify(&headers, &form, &broker.public_url)?;
    let identity = broker
        .portal
        .identity_for_subject(subject.ok_or(Problem::Unauthorized)?)
        .await?;
    broker
        .portal
        .record_audit(Some(&identity), AuditAction::Logout, AuditOutcome::Pending)
        .await?;
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
async fn add_key_inner(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let (headers, form) = form(request).await?;
    only(&form, &["csrf", "label", "public_key"])?;
    let owner = key_action(&broker, &headers, &form).await?;
    let key = parse_key(&form["public_key"], &form["label"])?;
    let identity = broker.portal.identity_for_subject(owner.clone()).await?;
    broker
        .portal
        .record_audit(
            Some(&identity),
            AuditAction::KeyAdded,
            AuditOutcome::Pending,
        )
        .await?;
    broker.portal.storage(move |s| s.add(&owner, key)).await?;
    Ok(Redirect::to("/#keys").into_response())
}
async fn delete_key_inner(
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
    let identity = broker.portal.identity_for_subject(owner.clone()).await?;
    broker
        .portal
        .record_audit(
            Some(&identity),
            AuditAction::KeyRemoved,
            AuditOutcome::Pending,
        )
        .await?;
    broker
        .portal
        .storage(move |s| s.remove(&owner, &fingerprint))
        .await?;
    Ok(Redirect::to("/#keys").into_response())
}

/// Validate a single public-key line through the installed OpenSSH implementation.
fn parse_key(raw: &str, label: &str) -> Result<PublicKey, Problem> {
    if label.len() > 80 || label.chars().any(char::is_control) {
        return Err(Problem::Invalid);
    }
    let (public_key, fingerprint) =
        crate::ssh_public_key::parse(raw).map_err(|_| Problem::Invalid)?;
    Ok(PublicKey {
        fingerprint,
        label: label.trim().to_owned(),
        public_key,
    })
}

#[cfg(test)]
use base64::engine::general_purpose::STANDARD;
