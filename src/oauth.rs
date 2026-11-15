//! In-memory OAuth authorization-code server for an SSH-owned MCP session.
//!
//! MCP 2026-07-28 authorization, RFC 7636, 8414, 8707, 9207, 9728 and 9700.
//! No external metadata fetching: DCR is the advertised registration mechanism.
use crate::broker::{Broker, Session, valid_username};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, RawQuery, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Read,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;

const CLIENT_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
const TRANSACTION_TTL: Duration = Duration::from_secs(600);
const CODE_TTL: Duration = Duration::from_secs(60);
const ACCESS_TTL: Duration = Duration::from_secs(3600);
const REFRESH_TTL: Duration = Duration::from_secs(24 * 3600);
const MAX_CLIENTS: usize = 1024;
const MAX_TRANSACTIONS: usize = 2048;
const MAX_CODES: usize = 4096;
const MAX_FAMILIES: usize = 2048;
const MAX_ACCESS: usize = 8192;
const MAX_REFRESH: usize = 65536;
const MAX_BODY: usize = 32 * 1024;
const COOKIE: &str = "__Host-ssmcp-oauth";
type Key = [u8; 32];
type OResult<T> = Result<T, OAuthError>;

pub struct OAuth {
    issuer: String,
    state: Arc<Mutex<Store>>,
}

struct Store {
    clients: HashMap<String, Client>,
    transactions: HashMap<Key, Transaction>,
    codes: HashMap<Key, AuthorizationCode>,
    families: HashMap<Key, Family>,
    access: HashMap<Key, AccessToken>,
    refresh: HashMap<Key, RefreshToken>,
    pending_codes: usize,
    next_sweep: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMethod {
    None,
    Basic,
    Post,
}

impl AuthMethod {
    fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Basic => "client_secret_basic",
            Self::Post => "client_secret_post",
        }
    }
}

#[derive(Clone)]
struct Client {
    name: String,
    redirects: Vec<String>,
    method: AuthMethod,
    secret_hash: Option<Key>,
    refresh: bool,
    expires: Instant,
}

#[derive(Clone)]
struct Transaction {
    client_id: String,
    client_name: String,
    user: String,
    resource: String,
    redirect: RedirectTarget,
    challenge: Key,
    csrf: Key,
    expires: Instant,
    attempts: u8,
    busy: bool,
}

#[derive(Clone)]
struct Binding {
    user: String,
    resource: String,
    client_id: String,
    session: Weak<Session>,
}

impl Binding {
    fn live(&self) -> Option<Arc<Session>> {
        self.session.upgrade().filter(|session| session.is_alive())
    }
}

#[derive(Clone)]
struct AuthorizationCode {
    binding: Binding,
    redirect_uri: String,
    challenge: Key,
    redeem_until: Instant,
    retain_until: Instant,
    used_family: Option<Key>,
}

struct Family {
    binding: Binding,
    expires: Instant,
    revoked: bool,
}

struct AccessToken {
    family: Key,
    expires: Instant,
}

#[derive(Clone, Copy)]
struct RefreshToken {
    family: Key,
    expires: Instant,
    used: bool,
}

#[derive(Clone)]
struct RedirectTarget {
    uri: String,
    state: Option<String>,
}

#[derive(Clone)]
struct FormCallbackOrigin(String);

struct Consent {
    id: String,
    csrf: String,
    transaction: Transaction,
}

// Reserve capacity before consuming the one-time SSH login code. Cancellation or
// a wrong login code releases the reservation and leaves the form retryable.
struct LoginAttempt {
    state: Arc<Mutex<Store>>,
    key: Key,
    code: String,
    transaction: Transaction,
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        if let Some(transaction) = state.transactions.get_mut(&self.key)
            && transaction.busy
        {
            transaction.busy = false;
            state.pending_codes = state.pending_codes.saturating_sub(1);
        }
    }
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    application_type: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    scope: Option<String>,
}

#[derive(Default)]
struct Parameters(BTreeMap<String, String>);

impl Parameters {
    fn parse(encoded: &str) -> OResult<Self> {
        if encoded.len() > MAX_BODY {
            return Err(OAuthError::invalid("Request is too large"));
        }
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(encoded)
            .map_err(|_| OAuthError::invalid("Malformed form encoding"))?;
        if pairs.len() > 32 {
            return Err(OAuthError::invalid("Too many parameters"));
        }
        let mut map = BTreeMap::new();
        for (key, value) in pairs {
            if map.insert(key, value).is_some() {
                return Err(OAuthError::invalid("Duplicate parameters are not allowed"));
            }
        }
        Ok(Self(map))
    }
    fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
    fn required(&self, name: &str) -> OResult<&str> {
        self.get(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| OAuthError::invalid("A required parameter is missing"))
    }
}

struct OAuthError {
    code: &'static str,
    description: &'static str,
    status: StatusCode,
    redirect: Option<RedirectTarget>,
}

impl OAuthError {
    fn new(code: &'static str, description: &'static str) -> Self {
        Self {
            code,
            description,
            status: StatusCode::BAD_REQUEST,
            redirect: None,
        }
    }
    fn invalid(description: &'static str) -> Self {
        Self::new("invalid_request", description)
    }
    fn grant() -> Self {
        Self::new(
            "invalid_grant",
            "The grant is invalid, expired, already used, or its SSH session ended",
        )
    }
    fn client() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            ..Self::new("invalid_client", "Client authentication failed")
        }
    }
    fn capacity() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            ..Self::new(
                "temporarily_unavailable",
                "Authorization capacity is full; retry later",
            )
        }
    }
    fn internal() -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            ..Self::new(
                "server_error",
                "Could not create an authorization credential",
            )
        }
    }
    fn json(self) -> Response {
        let mut response = (
            self.status,
            Json(json!({"error":self.code,"error_description":self.description})),
        )
            .into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"ssmcp OAuth\""),
            );
        }
        response
    }
    fn page(self, issuer: &str) -> Response {
        if let Some(target) = self.redirect {
            return redirect(
                &target,
                issuer,
                &[
                    ("error", self.code),
                    ("error_description", self.description),
                ],
            );
        }
        (
            self.status,
            Html(page(
                "无法继续授权",
                &format!(
                    "<p>{}</p><p class=muted>请返回 MCP 客户端重新连接。</p>",
                    escape(self.description)
                ),
            )),
        )
            .into_response()
    }
}

impl OAuth {
    pub fn new(public_url: String) -> Self {
        Self {
            issuer: public_url.trim_end_matches('/').to_owned(),
            state: Arc::new(Mutex::new(Store {
                clients: HashMap::new(),
                transactions: HashMap::new(),
                codes: HashMap::new(),
                families: HashMap::new(),
                access: HashMap::new(),
                refresh: HashMap::new(),
                pending_codes: 0,
                next_sweep: Instant::now(),
            })),
        }
    }

    pub fn authenticate(&self, user: &str, access_token: &str) -> Option<Arc<Session>> {
        if !valid_username(user) || !opaque(access_token, "at_") {
            return None;
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.sweep(now);
        let access = state.access.get(&hash(access_token))?;
        let family = state.families.get(&access.family)?;
        if now >= access.expires
            || now >= family.expires
            || family.revoked
            || family.binding.user != user
            || family.binding.resource != self.resource(user)
        {
            return None;
        }
        family.binding.live()
    }

    fn resource(&self, user: &str) -> String {
        format!("{}/{user}", self.issuer)
    }

    fn resource_user<'a>(&self, resource: &'a str) -> OResult<&'a str> {
        let prefix = format!("{}/", self.issuer);
        let user = resource
            .strip_prefix(&prefix)
            .filter(|user| valid_username(user))
            .ok_or_else(|| {
                OAuthError::new(
                    "invalid_target",
                    "resource must exactly identify one MCP endpoint at this issuer",
                )
            })?;
        if resource != self.resource(user) {
            return Err(OAuthError::new(
                "invalid_target",
                "resource must be an exact MCP endpoint",
            ));
        }
        Ok(user)
    }

    fn metadata(&self) -> Value {
        json!({"issuer":self.issuer,"authorization_endpoint":format!("{}/oauth/authorize",self.issuer),
            "token_endpoint":format!("{}/oauth/token",self.issuer),"registration_endpoint":format!("{}/oauth/register",self.issuer),
            "response_types_supported":["code"],"response_modes_supported":["query"],
            "grant_types_supported":["authorization_code","refresh_token"],"scopes_supported":["mcp"],
            "code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":["none","client_secret_basic","client_secret_post"],
            "client_id_metadata_document_supported":false,"authorization_response_iss_parameter_supported":true})
    }

    fn protected_metadata(&self, user: &str) -> OResult<Value> {
        if !valid_username(user) {
            return Err(OAuthError::new("invalid_target", "Invalid resource path"));
        }
        // This document describes configuration, never whether an SSH user is online.
        Ok(
            json!({"resource":self.resource(user),"authorization_servers":[self.issuer],"scopes_supported":["mcp"],
            "bearer_methods_supported":["header"],"resource_name":"ssmcp"}),
        )
    }

    fn register(&self, registration: Registration) -> OResult<Value> {
        let Registration {
            redirect_uris,
            client_name,
            application_type,
            token_endpoint_auth_method,
            grant_types,
            response_types,
            scope,
        } = registration;
        if redirect_uris.is_empty() || redirect_uris.len() > 8 {
            return Err(OAuthError::new(
                "invalid_redirect_uri",
                "Register between one and eight redirect URIs",
            ));
        }
        let application_type = application_type.unwrap_or_else(|| {
            if redirect_uris.iter().all(|r| loopback_redirect(r)) {
                "native".into()
            } else {
                "web".into()
            }
        });
        if !matches!(application_type.as_str(), "web" | "native") {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "application_type must be web or native",
            ));
        }
        let mut unique = HashSet::new();
        for uri in &redirect_uris {
            if !valid_redirect(uri, &application_type) || !unique.insert(uri) {
                return Err(OAuthError::new(
                    "invalid_redirect_uri",
                    "Use distinct HTTPS callbacks, or HTTP loopback callbacks for a native client",
                ));
            }
        }
        let name = client_name.unwrap_or_else(|| "MCP client".into());
        if name.trim().is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "client_name must contain between 1 and 200 bytes of text",
            ));
        }
        let method = match token_endpoint_auth_method
            .as_deref()
            .unwrap_or("client_secret_basic")
        {
            "none" => AuthMethod::None,
            "client_secret_basic" => AuthMethod::Basic,
            "client_secret_post" => AuthMethod::Post,
            _ => {
                return Err(OAuthError::new(
                    "invalid_client_metadata",
                    "Unsupported token endpoint authentication method",
                ));
            }
        };
        let grants = grant_types.unwrap_or_else(|| vec!["authorization_code".into()]);
        if !grants.iter().any(|g| g == "authorization_code")
            || grants.len() > 2
            || grants.iter().collect::<HashSet<_>>().len() != grants.len()
            || grants
                .iter()
                .any(|g| !matches!(g.as_str(), "authorization_code" | "refresh_token"))
        {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "Only authorization_code and refresh_token grants are supported",
            ));
        }
        if response_types
            .as_ref()
            .is_some_and(|types| types != &["code".to_owned()])
        {
            return Err(OAuthError::new(
                "invalid_client_metadata",
                "Only response_type code is supported",
            ));
        }
        scope_mcp(scope.as_deref())?;
        let now = Instant::now();
        let issued = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let id = random("client_")?;
        let secret = if method == AuthMethod::None {
            None
        } else {
            Some(random("cs_")?)
        };
        let mut state = self.state.lock().unwrap();
        state.sweep(now);
        if state.clients.len() >= MAX_CLIENTS {
            return Err(OAuthError::capacity());
        }
        state.clients.insert(
            id.clone(),
            Client {
                name: name.clone(),
                redirects: redirect_uris.clone(),
                method,
                secret_hash: secret.as_deref().map(hash),
                refresh: grants.iter().any(|g| g == "refresh_token"),
                expires: now + CLIENT_TTL,
            },
        );
        let mut reply = json!({"client_id":id,"client_id_issued_at":issued,"client_name":name,"application_type":application_type,
            "redirect_uris":redirect_uris,"token_endpoint_auth_method":method.name(),"grant_types":grants,"response_types":["code"],"scope":"mcp"});
        if let Some(secret) = secret {
            reply["client_secret"] = secret.into();
            reply["client_secret_expires_at"] = (issued + CLIENT_TTL.as_secs()).into();
        }
        Ok(reply)
    }

    fn begin_authorization(
        &self,
        parameters: &Parameters,
        headers: &HeaderMap,
    ) -> OResult<Consent> {
        if parameters.get("login_code").is_some() {
            return Err(OAuthError::invalid(
                "Login codes are accepted only in the authorization form POST",
            ));
        }
        let id = parameters.required("client_id")?;
        let uri = parameters.required("redirect_uri")?;
        let state_value = parameters.get("state");
        if state_value.is_some_and(|s| s.len() > 2048) {
            return Err(OAuthError::invalid("state is too large"));
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.sweep(now);
        let client = state
            .clients
            .get(id)
            .filter(|c| now < c.expires)
            .cloned()
            .ok_or_else(|| {
                OAuthError::new(
                    "invalid_client",
                    "Register a client with this issuer before authorizing",
                )
            })?;
        if !client.redirects.iter().any(|registered| registered == uri) {
            return Err(OAuthError::invalid(
                "redirect_uri is not an exact registered callback",
            ));
        }
        let target = RedirectTarget {
            uri: uri.into(),
            state: state_value.map(str::to_owned),
        };
        let validate = || -> OResult<(String, String, Key)> {
            if parameters.required("response_type")? != "code" {
                return Err(OAuthError::new(
                    "unsupported_response_type",
                    "Only response_type code is supported",
                ));
            }
            if parameters
                .get("response_mode")
                .is_some_and(|mode| mode != "query")
            {
                return Err(OAuthError::invalid(
                    "Only query authorization responses are supported",
                ));
            }
            scope_mcp(parameters.get("scope"))?;
            let resource = parameters.required("resource")?;
            let user = self.resource_user(resource)?;
            if parameters.get("code_challenge_method") != Some("S256") {
                return Err(OAuthError::invalid("S256 PKCE is required"));
            }
            let challenge = decode_challenge(parameters.required("code_challenge")?)?;
            Ok((user.into(), resource.into(), challenge))
        };
        let (user, resource, challenge) = validate().map_err(|mut error| {
            error.redirect = Some(target.clone());
            error
        })?;
        if state.transactions.len() >= MAX_TRANSACTIONS {
            return Err(OAuthError::capacity());
        }
        let transaction_id = random("tx_")?;
        let csrf = cookie(headers)
            .filter(|value| opaque(value, "csrf_"))
            .map(str::to_owned)
            .map_or_else(|| random("csrf_"), Ok)?;
        let transaction = Transaction {
            client_id: id.into(),
            client_name: client.name,
            user,
            resource,
            redirect: target,
            challenge,
            csrf: hash(&csrf),
            expires: (now + TRANSACTION_TTL).min(client.expires),
            attempts: 0,
            busy: false,
        };
        state
            .transactions
            .insert(hash(&transaction_id), transaction.clone());
        Ok(Consent {
            id: transaction_id,
            csrf,
            transaction,
        })
    }

    fn begin_login(&self, id: &str, csrf: &str) -> OResult<LoginAttempt> {
        if !opaque(id, "tx_") || !opaque(csrf, "csrf_") {
            return Err(OAuthError::invalid(
                "Authorization form has expired; restart the connection",
            ));
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.sweep(now);
        let key = hash(id);
        let transaction = state
            .transactions
            .get(&key)
            .filter(|t| now < t.expires)
            .cloned()
            .ok_or_else(|| {
                OAuthError::invalid("Authorization form has expired; restart the connection")
            })?;
        if !bool::from(transaction.csrf.ct_eq(&hash(csrf))) {
            return Err(OAuthError::invalid(
                "Authorization form does not belong to this browser",
            ));
        }
        if transaction.busy || transaction.attempts >= 5 {
            return Err(OAuthError::invalid(
                "Restart authorization before another login attempt",
            ));
        }
        if !state
            .clients
            .get(&transaction.client_id)
            .is_some_and(|c| now < c.expires)
        {
            return Err(OAuthError::client());
        }
        if state.codes.len() + state.pending_codes >= MAX_CODES {
            return Err(OAuthError::capacity());
        }
        let code = random("ac_")?;
        let stored = state.transactions.get_mut(&key).unwrap();
        stored.busy = true;
        stored.attempts += 1;
        state.pending_codes += 1;
        Ok(LoginAttempt {
            state: self.state.clone(),
            key,
            code,
            transaction,
        })
    }

    fn finish_login(&self, attempt: LoginAttempt, session: Arc<Session>) -> OResult<Response> {
        if !session.is_alive() {
            return Err(OAuthError::grant());
        }
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        let transaction = state
            .transactions
            .remove(&attempt.key)
            .ok_or_else(|| OAuthError::invalid("Authorization form has expired"))?;
        if transaction.busy {
            state.pending_codes = state.pending_codes.saturating_sub(1);
        }
        if now >= transaction.expires {
            return Err(OAuthError::invalid("Authorization form has expired"));
        }
        let binding = Binding {
            user: transaction.user,
            resource: transaction.resource,
            client_id: transaction.client_id,
            session: Arc::downgrade(&session),
        };
        state.codes.insert(
            hash(&attempt.code),
            AuthorizationCode {
                binding,
                redirect_uri: transaction.redirect.uri.clone(),
                challenge: transaction.challenge,
                redeem_until: now + CODE_TTL,
                retain_until: now + CODE_TTL,
                used_family: None,
            },
        );
        let response = form_redirect(
            &transaction.redirect,
            &self.issuer,
            &[("code", attempt.code.as_str())],
        );
        drop(state); // LoginAttempt::drop also locks the store.
        Ok(response)
    }

    fn token(&self, parameters: &Parameters, headers: &HeaderMap) -> OResult<Value> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap();
        state.sweep(now);
        let (client_id, client) = authenticate_client(&state, parameters, headers, now)?;
        let resource = parameters.required("resource")?;
        self.resource_user(resource)?;
        scope_mcp(parameters.get("scope"))?;
        match parameters.required("grant_type")? {
            "authorization_code" => {
                self.exchange_code(&mut state, parameters, &client_id, &client, resource, now)
            }
            "refresh_token" => {
                self.rotate_refresh(&mut state, parameters, &client_id, &client, resource, now)
            }
            _ => Err(OAuthError::new(
                "unsupported_grant_type",
                "Only authorization_code and refresh_token grants are supported",
            )),
        }
    }

    fn exchange_code(
        &self,
        state: &mut Store,
        parameters: &Parameters,
        client_id: &str,
        client: &Client,
        resource: &str,
        now: Instant,
    ) -> OResult<Value> {
        let raw = parameters.required("code")?;
        if !opaque(raw, "ac_") {
            return Err(OAuthError::grant());
        }
        let key = hash(raw);
        let code = state
            .codes
            .get(&key)
            .cloned()
            .ok_or_else(OAuthError::grant)?;
        let verifier = parameters.required("code_verifier")?;
        if code.binding.client_id != client_id
            || code.binding.resource != resource
            || code.binding.live().is_none()
            || parameters
                .get("redirect_uri")
                .is_some_and(|uri| uri != code.redirect_uri)
            || !verify_pkce(verifier, &code.challenge)
        {
            return Err(OAuthError::grant());
        }
        // Invalid bindings/verifiers cannot be used to revoke somebody else's grant.
        if let Some(family) = code.used_family {
            if let Some(family) = state.families.get_mut(&family) {
                family.revoked = true;
            }
            return Err(OAuthError::grant());
        }
        if now >= code.redeem_until {
            return Err(OAuthError::grant());
        }
        if state.families.len() >= MAX_FAMILIES
            || state.access.len() >= MAX_ACCESS
            || (client.refresh && state.refresh.len() >= MAX_REFRESH)
        {
            return Err(OAuthError::capacity());
        }
        let family_id = hash(&random("family_")?);
        let access = random("at_")?;
        let refresh = if client.refresh {
            Some(random("rt_")?)
        } else {
            None
        };
        let end = (now
            + if client.refresh {
                REFRESH_TTL
            } else {
                ACCESS_TTL
            })
        .min(client.expires);
        let access_end = (now + ACCESS_TTL).min(end);
        if access_end.duration_since(now).as_secs() == 0 {
            return Err(OAuthError::grant());
        }
        state.families.insert(
            family_id,
            Family {
                binding: code.binding,
                expires: end,
                revoked: false,
            },
        );
        state.access.insert(
            hash(&access),
            AccessToken {
                family: family_id,
                expires: access_end,
            },
        );
        if let Some(refresh) = &refresh {
            state.refresh.insert(
                hash(refresh),
                RefreshToken {
                    family: family_id,
                    expires: end,
                    used: false,
                },
            );
        }
        let code = state.codes.get_mut(&key).unwrap();
        code.used_family = Some(family_id);
        code.retain_until = end;
        Ok(token_response(
            access,
            refresh,
            resource,
            access_end.duration_since(now).as_secs(),
        ))
    }

    fn rotate_refresh(
        &self,
        state: &mut Store,
        parameters: &Parameters,
        client_id: &str,
        client: &Client,
        resource: &str,
        now: Instant,
    ) -> OResult<Value> {
        if !client.refresh {
            return Err(OAuthError::grant());
        }
        let raw = parameters.required("refresh_token")?;
        if !opaque(raw, "rt_") {
            return Err(OAuthError::grant());
        }
        let key = hash(raw);
        let refresh = state
            .refresh
            .get(&key)
            .copied()
            .ok_or_else(OAuthError::grant)?;
        let family = state
            .families
            .get_mut(&refresh.family)
            .ok_or_else(OAuthError::grant)?;
        if family.revoked
            || now >= family.expires
            || now >= refresh.expires
            || family.binding.live().is_none()
            || family.binding.client_id != client_id
            || family.binding.resource != resource
        {
            return Err(OAuthError::grant());
        }
        if refresh.used {
            family.revoked = true;
            return Err(OAuthError::grant());
        }
        let end = family.expires;
        if state.access.len() >= MAX_ACCESS || state.refresh.len() >= MAX_REFRESH {
            return Err(OAuthError::capacity());
        }
        let next = random("rt_")?;
        let access = random("at_")?;
        let access_end = (now + ACCESS_TTL).min(end);
        if access_end.duration_since(now).as_secs() == 0 {
            return Err(OAuthError::grant());
        }
        state.refresh.get_mut(&key).unwrap().used = true;
        state.refresh.insert(
            hash(&next),
            RefreshToken {
                family: refresh.family,
                expires: end,
                used: false,
            },
        );
        state.access.insert(
            hash(&access),
            AccessToken {
                family: refresh.family,
                expires: access_end,
            },
        );
        Ok(token_response(
            access,
            Some(next),
            resource,
            access_end.duration_since(now).as_secs(),
        ))
    }
}

impl Store {
    fn sweep(&mut self, now: Instant) {
        if now < self.next_sweep {
            return;
        }
        self.next_sweep = now + Duration::from_secs(30);
        self.clients.retain(|_, client| now < client.expires);
        self.transactions
            .retain(|_, t| now < t.expires && self.clients.contains_key(&t.client_id));
        self.pending_codes = self.transactions.values().filter(|t| t.busy).count();
        self.families.retain(|_, f| {
            !f.revoked
                && now < f.expires
                && f.binding.live().is_some()
                && self.clients.contains_key(&f.binding.client_id)
        });
        self.access
            .retain(|_, a| now < a.expires && self.families.contains_key(&a.family));
        self.refresh
            .retain(|_, r| now < r.expires && self.families.contains_key(&r.family));
        self.codes.retain(|_, c| {
            now < c.retain_until
                && c.binding.live().is_some()
                && c.used_family
                    .is_none_or(|id| self.families.contains_key(&id))
        });
    }
}

fn token_response(access: String, refresh: Option<String>, resource: &str, expires: u64) -> Value {
    let mut response = json!({"access_token":access,"token_type":"Bearer","expires_in":expires,"scope":"mcp","resource":resource});
    if let Some(refresh) = refresh {
        response["refresh_token"] = refresh.into();
    }
    response
}

fn authenticate_client(
    state: &Store,
    parameters: &Parameters,
    headers: &HeaderMap,
    now: Instant,
) -> OResult<(String, Client)> {
    let authorization = one_header(headers, "authorization").map_err(|_| OAuthError::client())?;
    let (id, secret, method) = if let Some(header) = authorization {
        if parameters.get("client_secret").is_some() {
            return Err(OAuthError::client());
        }
        let (scheme, data) = header.split_once(' ').ok_or_else(OAuthError::client)?;
        if !scheme.eq_ignore_ascii_case("Basic") || data.len() > 4096 {
            return Err(OAuthError::client());
        }
        let bytes = STANDARD.decode(data).map_err(|_| OAuthError::client())?;
        let decoded = std::str::from_utf8(&bytes).map_err(|_| OAuthError::client())?;
        let (id, secret) = decoded.split_once(':').ok_or_else(OAuthError::client)?;
        let id = form_component(id)?;
        let secret = form_component(secret)?;
        if parameters.get("client_id").is_some_and(|body| body != id) {
            return Err(OAuthError::client());
        }
        (id, Some(secret), AuthMethod::Basic)
    } else {
        let id = parameters
            .required("client_id")
            .map_err(|_| OAuthError::client())?
            .to_owned();
        let secret = parameters.get("client_secret").map(str::to_owned);
        let method = if secret.is_some() {
            AuthMethod::Post
        } else {
            AuthMethod::None
        };
        (id, secret, method)
    };
    let client = state
        .clients
        .get(&id)
        .filter(|client| now < client.expires)
        .ok_or_else(OAuthError::client)?;
    if client.method != method {
        return Err(OAuthError::client());
    }
    if let Some(expected) = client.secret_hash {
        let secret = secret
            .filter(|s| s.len() <= 256)
            .ok_or_else(OAuthError::client)?;
        if !bool::from(expected.ct_eq(&hash(&secret))) {
            return Err(OAuthError::client());
        }
    }
    Ok((id, client.clone()))
}

fn form_component(encoded: &str) -> OResult<String> {
    let pairs: Vec<(String, String)> =
        serde_urlencoded::from_str(&format!("v={encoded}")).map_err(|_| OAuthError::client())?;
    if pairs.len() != 1 || pairs[0].0 != "v" {
        return Err(OAuthError::client());
    }
    Ok(pairs.into_iter().next().unwrap().1)
}

fn random(prefix: &str) -> OResult<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| OAuthError::internal())?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn hash(text: &str) -> Key {
    Sha256::digest(text.as_bytes()).into()
}

fn opaque(text: &str, prefix: &str) -> bool {
    text.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 43
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    })
}

fn decode_challenge(challenge: &str) -> OResult<Key> {
    let bytes = URL_SAFE_NO_PAD
        .decode(challenge)
        .map_err(|_| OAuthError::invalid("Invalid S256 code challenge"))?;
    if challenge.len() != 43 || bytes.len() != 32 || URL_SAFE_NO_PAD.encode(&bytes) != challenge {
        return Err(OAuthError::invalid("Invalid S256 code challenge"));
    }
    bytes
        .try_into()
        .map_err(|_| OAuthError::invalid("Invalid S256 code challenge"))
}

fn verify_pkce(verifier: &str, challenge: &Key) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
        && bool::from(challenge.ct_eq(&hash(verifier)))
}

fn scope_mcp(scope: Option<&str>) -> OResult<()> {
    if scope.is_none_or(|scope| scope == "mcp") {
        Ok(())
    } else {
        Err(OAuthError::new(
            "invalid_scope",
            "Only the mcp scope is supported",
        ))
    }
}

fn loopback_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    url.scheme() == "http" && matches!(url.host_str(), Some("localhost" | "[::1]" | "::1"))
        || url.scheme() == "http"
            && url
                .host_str()
                .and_then(|h| h.parse::<std::net::IpAddr>().ok())
                .is_some_and(|ip| ip.is_loopback())
}

fn valid_redirect(uri: &str, application_type: &str) -> bool {
    if uri.len() > 2048
        || uri.trim() != uri
        || uri.chars().any(char::is_control)
        || uri.contains('\\')
    {
        return false;
    }
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
        || url.port() == Some(0)
    {
        return false;
    }
    // URL parsers accept punctuation in some domain names. Origins are also
    // inserted into CSP, so allow only literal DNS/IP characters, never policy
    // delimiters or wildcard sources. International names are already punycode.
    if !url
        .host_str()
        .unwrap()
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte))
    {
        return false;
    }
    if url.query_pairs().any(|(key, _)| {
        matches!(
            key.as_ref(),
            "code" | "state" | "iss" | "error" | "error_description"
        )
    }) {
        return false;
    }
    url.scheme() == "https" || (application_type == "native" && loopback_redirect(uri))
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    };
    value.to_str().map(Some).map_err(|_| ())
}

fn cookie(headers: &HeaderMap) -> Option<&str> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for part in value.to_str().ok()?.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name == COOKIE {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found
}

fn callback_url(target: &RedirectTarget, issuer: &str, values: &[(&str, &str)]) -> Url {
    // The target was validated against the exact registered URI before this point.
    let mut url = Url::parse(&target.uri).expect("validated redirect URI");
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in values {
            query.append_pair(key, value);
        }
        if let Some(state) = &target.state {
            query.append_pair("state", state);
        }
        query.append_pair("iss", issuer);
    }
    url
}

fn redirect(target: &RedirectTarget, issuer: &str, values: &[(&str, &str)]) -> Response {
    let url = callback_url(target, issuer, values);
    let mut response = StatusCode::SEE_OTHER.into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(url.as_str()).expect("URL is a valid HTTP header"),
    );
    response
}

fn csp_callback_origin(uri: &str) -> Option<String> {
    let url = Url::parse(uri).expect("validated callback URL");
    let host = url
        .host_str()
        .unwrap()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if let Ok(ip) = host.parse::<std::net::IpAddr>()
        && ip != std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    {
        // CSP host-source does not consistently support IP literals other than
        // 127.0.0.1, in particular native clients' IPv6 loopback callbacks.
        return None;
    }
    Some(url.origin().ascii_serialization())
}

fn form_redirect(target: &RedirectTarget, issuer: &str, values: &[(&str, &str)]) -> Response {
    if csp_callback_origin(&target.uri).is_some() {
        return redirect(target, issuer, values);
    }
    // A fresh document makes this a navigation rather than a continuation of the
    // form submission. This supports IPv6 callbacks without allowing a broad
    // http: source in form-action or executing script. Only the already-bound
    // code/state/iss reach the callback; the one-time login POST stays here.
    let callback = escape(callback_url(target, issuer, values).as_str());
    Html(page_with_head(
        "连接已授权",
        &format!("<p>正在返回 MCP 客户端。</p><p><a href=\"{callback}\" rel=noreferrer>返回客户端</a></p>"),
        &format!("<meta http-equiv=refresh content=\"0;url={callback}\">"),
    )).into_response()
}

const CSS: &str = "*{box-sizing:border-box}body{margin:0;background:#fff;color:#202124;font:16px/1.6 system-ui,sans-serif}main{max-width:460px;margin:10vh auto;padding:32px}header{font:14px ui-monospace,monospace;color:#5f6368;letter-spacing:.05em}h1{font-size:28px;line-height:1.3;font-weight:500;margin:32px 0 20px}p{margin:14px 0}.muted,dt{color:#5f6368;font-size:14px}dl{margin:28px 0}dd{margin:2px 0 14px;overflow-wrap:anywhere;font-family:ui-monospace,monospace;font-size:13px}label{display:block;margin:24px 0 8px}input{width:100%;padding:13px;border:1px solid #dadce0;border-radius:6px;font:14px ui-monospace,monospace}input:focus{outline:2px solid #1a73e8;outline-offset:1px}button{display:block;width:100%;margin-top:20px;padding:12px;border:0;border-radius:6px;background:#1a73e8;color:white;font:500 16px system-ui;cursor:pointer}.error{color:#b3261e;font-size:14px}@media(max-width:540px){main{margin:4vh auto;padding:24px}}";

fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&#39;".into(),
            c => c.to_string(),
        })
        .collect()
}

fn page(title: &str, content: &str) -> String {
    page_with_head(title, content, "")
}

fn page_with_head(title: &str, content: &str, extra_head: &str) -> String {
    format!(
        "<!doctype html><html lang=zh-CN><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>{} · ssmcp</title><style>{CSS}</style>{extra_head}</head><body><main><header>ssmcp</header><h1>{}</h1>{content}</main></body></html>",
        escape(title),
        escape(title)
    )
}

fn consent_response(
    id: &str,
    transaction: &Transaction,
    error: Option<&str>,
    status: StatusCode,
) -> Response {
    let mut response = (status, Html(consent_page(id, transaction, error))).into_response();
    if let Some(origin) = csp_callback_origin(&transaction.redirect.uri) {
        response.extensions_mut().insert(FormCallbackOrigin(origin));
    }
    response
}

fn consent_page(id: &str, transaction: &Transaction, error: Option<&str>) -> String {
    let callback = Url::parse(&transaction.redirect.uri)
        .unwrap()
        .origin()
        .ascii_serialization();
    let error = error
        .map(|error| format!("<p class=error role=alert>{}</p>", escape(error)))
        .unwrap_or_default();
    page(
        "连接你的终端",
        &format!(
            "<p>粘贴本机 ssmcp 显示的一次性登录码，授权此客户端控制当前连接。</p><dl><dt>客户端</dt><dd>{}</dd><dt>回调来源</dt><dd>{}</dd><dt>MCP 资源</dt><dd>{}</dd></dl>{error}<form method=post action=/oauth/authorize><input type=hidden name=transaction_id value=\"{}\"><label for=login_code>一次性登录码</label><input id=login_code name=login_code type=password autocomplete=one-time-code minlength=64 maxlength=64 required spellcheck=false autofocus><button type=submit>授权连接</button></form><p class=muted>登录码只使用一次。SSH 断开后，访问立即失效。</p>",
            escape(&transaction.client_name),
            escape(&callback),
            escape(&transaction.resource),
            escape(id)
        ),
    )
}

pub fn routes() -> Router<Arc<Broker>> {
    let public = Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(metadata).options(preflight),
        )
        .route(
            "/.well-known/oauth-protected-resource/{user}",
            get(protected_metadata).options(preflight),
        )
        .route("/oauth/register", post(register).options(preflight))
        .route("/oauth/token", post(token).options(preflight))
        .layer(middleware::map_response(public_headers));
    Router::new()
        .merge(public)
        .route("/oauth/authorize", get(authorize).post(login))
        .layer(middleware::map_response(security_headers))
}

async fn metadata(State(broker): State<Arc<Broker>>) -> Json<Value> {
    Json(broker.oauth.metadata())
}

async fn protected_metadata(
    State(broker): State<Arc<Broker>>,
    Path(user): Path<String>,
) -> Response {
    match broker.oauth.protected_metadata(&user) {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.json(),
    }
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn public_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    response
}

async fn security_headers(mut response: Response) -> Response {
    let callback = response.extensions_mut().remove::<FormCallbackOrigin>();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    let style = STANDARD.encode(Sha256::digest(CSS.as_bytes()));
    let form = callback.map_or_else(
        || "'self'".to_owned(),
        |origin| format!("'self' {}", origin.0),
    );
    headers.insert("content-security-policy",HeaderValue::from_str(&format!("default-src 'none'; style-src 'sha256-{style}'; form-action {form}; base-uri 'none'; frame-ancestors 'none'")).unwrap());
    response
}

async fn body(request: Request, expected: &str) -> OResult<Vec<u8>> {
    let content = one_header(request.headers(), "content-type")
        .map_err(|_| OAuthError::invalid("Duplicate content-type"))?
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !content.is_some_and(|value| value.eq_ignore_ascii_case(expected)) {
        return Err(OAuthError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ..OAuthError::invalid("Unsupported content type")
        });
    }
    tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), MAX_BODY),
    )
    .await
    .map_err(|_| OAuthError::invalid("Request body timed out"))?
    .map(|bytes| bytes.to_vec())
    .map_err(|_| OAuthError::invalid("Request body is too large"))
}

async fn register(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    let outcome = async {
        let bytes = body(request, "application/json").await?;
        let registration = serde_json::from_slice::<Registration>(&bytes).map_err(|_| {
            OAuthError::new("invalid_client_metadata", "Invalid registration metadata")
        })?;
        broker.oauth.register(registration)
    }
    .await;
    match outcome {
        Ok(value) => (StatusCode::CREATED, Json(value)).into_response(),
        Err(error) => error.json(),
    }
}

async fn authorize(
    State(broker): State<Arc<Broker>>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let outcome = Parameters::parse(query.as_deref().unwrap_or(""))
        .and_then(|parameters| broker.oauth.begin_authorization(&parameters, &headers));
    match outcome {
        Ok(consent) => {
            let mut response =
                consent_response(&consent.id, &consent.transaction, None, StatusCode::OK);
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&format!(
                    "{COOKIE}={}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age=600",
                    consent.csrf
                ))
                .unwrap(),
            );
            response
        }
        Err(error) => error.page(&broker.oauth.issuer),
    }
}

async fn login(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    if one_header(request.headers(), "origin") != Ok(Some(broker.oauth.issuer.as_str())) {
        return OAuthError {
            status: StatusCode::FORBIDDEN,
            ..OAuthError::invalid("Authorization form must be submitted from this origin")
        }
        .page(&broker.oauth.issuer);
    }
    let csrf = cookie(request.headers()).map(str::to_owned);
    let outcome = async {
        let bytes = body(request, "application/x-www-form-urlencoded").await?;
        let parameters = Parameters::parse(
            std::str::from_utf8(&bytes).map_err(|_| OAuthError::invalid("Invalid form text"))?,
        )?;
        if parameters
            .0
            .keys()
            .any(|key| !matches!(key.as_str(), "transaction_id" | "login_code"))
        {
            return Err(OAuthError::invalid("Unexpected authorization form field"));
        }
        let id = parameters.required("transaction_id")?;
        let csrf = csrf.as_deref().ok_or_else(|| {
            OAuthError::invalid("Authorization cookie is missing; restart the connection")
        })?;
        let code = parameters.required("login_code")?.trim();
        let attempt = broker.oauth.begin_login(id, csrf)?;
        if code.len() != 64 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(consent_response(
                id,
                &attempt.transaction,
                Some("登录码无效。请粘贴本机终端显示的 64 位登录码。"),
                StatusCode::BAD_REQUEST,
            ));
        }
        let Some(session) = broker.redeem_login(&attempt.transaction.user, code).await else {
            return Ok(consent_response(
                id,
                &attempt.transaction,
                Some("登录码无效、已使用或已过期。请检查终端，必要时重新运行 ssmcp。"),
                StatusCode::BAD_REQUEST,
            ));
        };
        broker.oauth.finish_login(attempt, session)
    }
    .await;
    match outcome {
        Ok(response) => response,
        Err(error) => error.page(&broker.oauth.issuer),
    }
}

async fn token(State(broker): State<Arc<Broker>>, request: Request) -> Response {
    let headers = request.headers().clone();
    let outcome = async {
        let bytes = body(request, "application/x-www-form-urlencoded").await?;
        let parameters = Parameters::parse(
            std::str::from_utf8(&bytes).map_err(|_| OAuthError::invalid("Invalid form text"))?,
        )?;
        broker.oauth.token(&parameters, &headers)
    }
    .await;
    match outcome {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.json(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUER: &str = "https://ssmcp.example";
    const RESOURCE: &str = "https://ssmcp.example/alice";
    const CALLBACK: &str = "https://client.example/callback";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn ok<T>(result: OResult<T>) -> T {
        result.unwrap_or_else(|error| panic!("{}: {}", error.code, error.description))
    }

    fn parameters(pairs: &[(&str, &str)]) -> Parameters {
        Parameters(
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        )
    }

    fn registration(refresh: bool) -> Registration {
        serde_json::from_value(json!({
            "client_name":"A <testing> client",
            "redirect_uris":[CALLBACK],
            "token_endpoint_auth_method":"none",
            "grant_types":if refresh {vec!["authorization_code", "refresh_token"]} else {vec!["authorization_code"]}
        })).unwrap()
    }

    fn consent(oauth: &OAuth, client: &str) -> Consent {
        ok(oauth.begin_authorization(
            &parameters(&[
                ("client_id", client),
                ("redirect_uri", CALLBACK),
                ("response_type", "code"),
                ("resource", RESOURCE),
                ("scope", "mcp"),
                ("code_challenge_method", "S256"),
                ("code_challenge", CHALLENGE),
                ("state", "client state & = +"),
            ]),
            &HeaderMap::new(),
        ))
    }

    struct Fixture {
        broker: Broker,
        session: Arc<Session>,
        client: String,
        code: String,
    }

    impl Fixture {
        async fn new(refresh: bool) -> Self {
            let broker = Broker::new(ISSUER.into());
            let login_code = "a".repeat(64);
            let session = broker.test_register("alice", &login_code).await;
            let registration = ok(broker.oauth.register(registration(refresh)));
            let client = registration["client_id"].as_str().unwrap().to_owned();
            let consent = consent(&broker.oauth, &client);
            let attempt = ok(broker.oauth.begin_login(&consent.id, &consent.csrf));
            let code = attempt.code.clone();
            let redeemed = broker.redeem_login("alice", &login_code).await.unwrap();
            let before = Instant::now();
            let response = ok(broker.oauth.finish_login(attempt, redeemed));
            let after = Instant::now();
            assert_eq!(response.status(), StatusCode::SEE_OTHER);
            let location =
                Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
            let query: BTreeMap<_, _> = location.query_pairs().into_owned().collect();
            assert_eq!(query["state"], "client state & = +");
            assert_eq!(query["iss"], ISSUER);
            assert_eq!(query["code"], code);
            assert!(!query.contains_key("login_code"));
            let oauth_code = broker.oauth.state.lock().unwrap().codes[&hash(&code)].clone();
            assert!(oauth_code.redeem_until >= before + CODE_TTL);
            assert!(oauth_code.redeem_until <= after + CODE_TTL);
            assert_eq!(oauth_code.redeem_until, oauth_code.retain_until);
            Self {
                broker,
                session,
                client,
                code,
            }
        }

        fn exchange_parameters(&self) -> Parameters {
            // OAuth 2.1 does not require redirect_uri in the token request.
            parameters(&[
                ("client_id", &self.client),
                ("grant_type", "authorization_code"),
                ("code", &self.code),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ])
        }

        fn tokens(&self) -> Value {
            ok(self
                .broker
                .oauth
                .token(&self.exchange_parameters(), &HeaderMap::new()))
        }
    }

    #[test]
    fn pkce_rfc7636_vector_and_strict_lengths() {
        let challenge = ok(decode_challenge(CHALLENGE));
        assert!(verify_pkce(VERIFIER, &challenge));
        assert!(!verify_pkce(&format!("{VERIFIER}="), &challenge));
        assert!(!verify_pkce("short", &hash("short")));
        assert!(!verify_pkce(&"a".repeat(129), &hash(&"a".repeat(129))));
        assert!(!verify_pkce(&"+".repeat(43), &hash(&"+".repeat(43))));
        assert!(decode_challenge(&format!("{CHALLENGE}=")).is_err());
        assert!(decode_challenge("invalid").is_err());
        assert!(Parameters::parse("code=a&code=b").is_err());
        assert!(Parameters::parse("state=a&%73tate=b").is_err());
    }

    #[test]
    fn transaction_expiry_csrf_and_cancelled_login_rollback() {
        let oauth = OAuth::new(ISSUER.into());
        let registered = ok(oauth.register(registration(true)));
        let before = Instant::now();
        let form = consent(&oauth, registered["client_id"].as_str().unwrap());
        let after = Instant::now();
        assert!(form.transaction.expires >= before + TRANSACTION_TTL);
        assert!(form.transaction.expires <= after + TRANSACTION_TTL);
        assert!(oauth.begin_login(&form.id, &ok(random("csrf_"))).is_err());
        assert_eq!(oauth.state.lock().unwrap().pending_codes, 0);
        {
            let attempt = ok(oauth.begin_login(&form.id, &form.csrf));
            assert_eq!(oauth.state.lock().unwrap().pending_codes, 1);
            assert!(oauth.begin_login(&form.id, &form.csrf).is_err());
            drop(attempt);
        }
        {
            let state = oauth.state.lock().unwrap();
            assert_eq!(state.pending_codes, 0);
            assert!(!state.transactions[&hash(&form.id)].busy);
        }
        let attempt = ok(oauth.begin_login(&form.id, &form.csrf));
        drop(attempt);
        oauth
            .state
            .lock()
            .unwrap()
            .transactions
            .get_mut(&hash(&form.id))
            .unwrap()
            .expires = Instant::now();
        assert!(oauth.begin_login(&form.id, &form.csrf).is_err());
        let mut state = oauth.state.lock().unwrap();
        state.next_sweep = Instant::now();
        state.sweep(Instant::now());
        assert!(state.transactions.is_empty());
        assert_eq!(state.pending_codes, 0);
    }

    #[test]
    fn login_capacity_is_reserved_before_code_consumption() {
        let oauth = OAuth::new(ISSUER.into());
        let registered = ok(oauth.register(registration(true)));
        let form = consent(&oauth, registered["client_id"].as_str().unwrap());
        // Filling the reservation counter reproduces a full code store without
        // creating thousands of sessions or testing a HashMap implementation.
        oauth.state.lock().unwrap().pending_codes = MAX_CODES;
        let error = oauth.begin_login(&form.id, &form.csrf).err().unwrap();
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
        let state = oauth.state.lock().unwrap();
        let transaction = &state.transactions[&hash(&form.id)];
        assert!(!transaction.busy);
        assert_eq!(transaction.attempts, 0);
    }

    #[tokio::test]
    async fn code_expiration_and_access_expiration_are_enforced() {
        let fixture = Fixture::new(false).await;
        let oauth = &fixture.broker.oauth;
        let mut state = oauth.state.lock().unwrap();
        let deadline = state.codes[&hash(&fixture.code)].redeem_until;
        let client = state.clients[&fixture.client].clone();
        let parameters = fixture.exchange_parameters();
        assert!(
            oauth
                .exchange_code(
                    &mut state,
                    &parameters,
                    &fixture.client,
                    &client,
                    RESOURCE,
                    deadline
                )
                .is_err()
        );
        assert!(state.codes[&hash(&fixture.code)].used_family.is_none());
        drop(state);
        let tokens = fixture.tokens();
        assert_eq!(tokens["expires_in"], 3600);
        assert!(tokens.get("refresh_token").is_none());
        let access = tokens["access_token"].as_str().unwrap();
        assert!(Arc::ptr_eq(
            &oauth.authenticate("alice", access).unwrap(),
            &fixture.session
        ));
        assert!(oauth.authenticate("alice", &"a".repeat(64)).is_none());
        assert!(oauth.authenticate("bob", access).is_none());
        oauth
            .state
            .lock()
            .unwrap()
            .access
            .get_mut(&hash(access))
            .unwrap()
            .expires = Instant::now();
        assert!(oauth.authenticate("alice", access).is_none());
    }

    #[tokio::test]
    async fn refresh_uses_absolute_24_hour_deadline_and_caps_access() {
        let fixture = Fixture::new(true).await;
        let tokens = fixture.tokens();
        assert_eq!(tokens["expires_in"], 3600);
        let refresh = tokens["refresh_token"].as_str().unwrap();
        let oauth = &fixture.broker.oauth;
        let mut state = oauth.state.lock().unwrap();
        let old = state.refresh[&hash(refresh)];
        let end = state.families[&old.family].expires;
        let initial_access = &state.access[&hash(tokens["access_token"].as_str().unwrap())];
        assert_eq!(
            end.duration_since(initial_access.expires),
            REFRESH_TTL - ACCESS_TTL
        );
        assert_eq!(end, old.expires);
        assert_eq!(state.codes[&hash(&fixture.code)].retain_until, end);
        let now = end - Duration::from_secs(7);
        let parameters = parameters(&[("refresh_token", refresh)]);
        let client = state.clients[&fixture.client].clone();
        let next = ok(oauth.rotate_refresh(
            &mut state,
            &parameters,
            &fixture.client,
            &client,
            RESOURCE,
            now,
        ));
        assert_eq!(next["expires_in"], 7);
        assert_eq!(state.families[&old.family].expires, end);
        assert_eq!(
            state.access[&hash(next["access_token"].as_str().unwrap())].expires,
            end
        );
        assert_eq!(
            state.refresh[&hash(next["refresh_token"].as_str().unwrap())].expires,
            end
        );
        assert!(state.refresh[&hash(refresh)].used);
        assert!(
            oauth
                .rotate_refresh(
                    &mut state,
                    &parameters,
                    &fixture.client,
                    &client,
                    RESOURCE,
                    end
                )
                .is_err()
        );
        assert!(!state.families[&old.family].revoked);
        let next_parameters = parameters_for_refresh(next["refresh_token"].as_str().unwrap());
        assert!(
            oauth
                .rotate_refresh(
                    &mut state,
                    &next_parameters,
                    &fixture.client,
                    &client,
                    RESOURCE,
                    end
                )
                .is_err()
        );
        state.next_sweep = end;
        state.sweep(end);
        assert!(state.families.is_empty());
        assert!(state.codes.is_empty());
        assert!(state.access.is_empty());
        assert!(state.refresh.is_empty());
    }

    fn parameters_for_refresh(refresh: &str) -> Parameters {
        parameters(&[("refresh_token", refresh)])
    }

    #[tokio::test]
    async fn client_expiry_caps_grants_and_disconnection_invalidates_original_binding() {
        let fixture = Fixture::new(true).await;
        let oauth = &fixture.broker.oauth;
        oauth
            .state
            .lock()
            .unwrap()
            .clients
            .get_mut(&fixture.client)
            .unwrap()
            .expires = Instant::now() + Duration::from_secs(30);
        let tokens = fixture.tokens();
        assert!(tokens["expires_in"].as_u64().unwrap() <= 30);
        let access = tokens["access_token"].as_str().unwrap();
        assert!(oauth.authenticate("alice", access).is_some());
        fixture.broker.disconnect_all().await;
        assert!(oauth.authenticate("alice", access).is_none());
        let replacement = fixture.broker.test_register("alice", &"b".repeat(64)).await;
        assert!(!Arc::ptr_eq(&replacement, &fixture.session));
        assert!(oauth.authenticate("alice", access).is_none());
        let refresh = parameters(&[
            ("client_id", &fixture.client),
            ("grant_type", "refresh_token"),
            ("refresh_token", tokens["refresh_token"].as_str().unwrap()),
            ("resource", RESOURCE),
        ]);
        assert!(oauth.token(&refresh, &HeaderMap::new()).is_err());
    }

    #[tokio::test]
    async fn failed_login_finalization_releases_reservation_without_deadlocking() {
        let broker = Broker::new(ISSUER.into());
        let session = broker.test_register("alice", &"a".repeat(64)).await;
        let registered = ok(broker.oauth.register(registration(true)));
        let form = consent(&broker.oauth, registered["client_id"].as_str().unwrap());
        let attempt = ok(broker.oauth.begin_login(&form.id, &form.csrf));
        broker
            .oauth
            .state
            .lock()
            .unwrap()
            .transactions
            .get_mut(&hash(&form.id))
            .unwrap()
            .expires = Instant::now();
        assert!(broker.oauth.finish_login(attempt, session).is_err());
        let state = broker.oauth.state.lock().unwrap();
        assert!(state.codes.is_empty());
        assert_eq!(state.pending_codes, 0);
    }

    #[test]
    fn metadata_is_configuration_and_html_escapes_untrusted_client_name() {
        let oauth = OAuth::new(ISSUER.into());
        let metadata = ok(oauth.protected_metadata("never_connected"));
        assert_eq!(
            metadata["resource"],
            "https://ssmcp.example/never_connected"
        );
        assert!(
            !oauth.metadata()["client_id_metadata_document_supported"]
                .as_bool()
                .unwrap()
        );
        let registered = ok(oauth.register(registration(true)));
        let form = consent(&oauth, registered["client_id"].as_str().unwrap());
        let html = consent_page(&form.id, &form.transaction, None);
        assert!(html.contains("A &lt;testing&gt; client"));
        assert!(!html.contains("A <testing> client"));
        assert!(html.contains("https://client.example"));
        assert!(html.contains(RESOURCE));
        for resource in [
            ISSUER,
            "https://ssmcp.example/alice/",
            "https://ssmcp.example/alice?x=1",
            "https://ssmcp.example/alice#fragment",
            "https://ssmcp.example/%61lice",
        ] {
            assert!(oauth.resource_user(resource).is_err(), "{resource}");
        }
        assert!(valid_redirect("http://127.0.0.1:54768/callback", "native"));
        assert!(valid_redirect("http://[::1]:54768/callback", "native"));
        assert!(!valid_redirect("http://client.example/callback", "native"));
        assert!(!valid_redirect(
            "https://user:pass@client.example/callback",
            "web"
        ));
        assert!(!valid_redirect(
            "https://client.example/callback#fragment",
            "web"
        ));
    }

    #[tokio::test]
    async fn consent_csp_allows_only_the_registered_callback_origin() {
        let oauth = OAuth::new(ISSUER.into());
        let registered = ok(oauth.register(registration(true)));
        let form = consent(&oauth, registered["client_id"].as_str().unwrap());
        for error in [None, Some("retry")] {
            let response = security_headers(consent_response(
                &form.id,
                &form.transaction,
                error,
                StatusCode::OK,
            ))
            .await;
            let policy = response.headers()["content-security-policy"]
                .to_str()
                .unwrap();
            assert!(policy.contains("form-action 'self' https://client.example;"));
            assert!(policy.contains("frame-ancestors 'none'"));
            assert!(policy.contains("default-src 'none'"));
            assert!(!policy.contains("'unsafe-inline'"));
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        }
        for uri in [
            "https://*.example/callback",
            "https://example;script-src/callback",
            "https://example,other/callback",
            " https://client.example/callback",
            "https://client.example/callback ",
        ] {
            assert!(!valid_redirect(uri, "web"), "{uri}");
        }
    }

    #[tokio::test]
    async fn ipv6_callback_gets_script_free_navigation_after_the_login_post() {
        let target = RedirectTarget {
            uri: "http://[::1]:54321/callback".into(),
            state: Some("state & <angle>".into()),
        };
        let response =
            security_headers(form_redirect(&target, ISSUER, &[("code", "ac_test")])).await;
        assert_eq!(response.status(), StatusCode::OK);
        let policy = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(policy.contains("form-action 'self';"));
        assert!(!policy.contains("http:"));
        let bytes = to_bytes(response.into_body(), MAX_BODY).await.unwrap();
        let html = std::str::from_utf8(&bytes).unwrap();
        assert!(html.contains("<meta http-equiv=refresh content=\"0;url=http://[::1]:54321/callback?code=ac_test&amp;state=state+%26+%3Cangle%3E&amp;iss=https%3A%2F%2Fssmcp.example\">"));
        assert!(html.contains("rel=noreferrer"));
        assert!(!html.contains("<script"));
        assert!(!html.contains("login_code"));
    }
}
