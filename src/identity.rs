//! Upstream identity verification. Provider tokens are used once and never retained.
//!
//! GitHub OAuth: <https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps>
//! A GitHub numeric user ID is the authorization subject; a login is display metadata.
use crate::wire::{MAX_ALLOWED_PRINCIPALS, canonical_github_principal};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, Response, Url, header, redirect::Policy};
use serde::{Deserialize, de::DeserializeOwned};
use std::{collections::HashSet, time::Duration};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const API_VERSION: &str = "2026-03-10";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    pub provider: String,
    pub subject: String,
    pub login: String,
}

/// Secret-bearing configuration deliberately has no Debug or Serialize implementation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    pub client_id: String,
    pub client_secret: String,
}

pub struct IdentityProviders {
    github: Option<GitHubProvider>,
}

struct GitHubProvider {
    config: GitHubConfig,
    callback: Url,
    client: Client,
    endpoints: GitHubEndpoints,
}

struct GitHubEndpoints {
    authorize: Url,
    token: Url,
    api: Url,
}

#[derive(Deserialize)]
struct GitHubToken {
    access_token: Option<String>,
    token_type: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct GitHubUser {
    id: u64,
    login: String,
    #[serde(rename = "type")]
    account_type: String,
}

impl IdentityProviders {
    /// Local OS-user sessions do not configure or contact an identity provider.
    pub(crate) fn disabled() -> Self {
        Self { github: None }
    }

    pub fn new(public_url: &str, github: Option<GitHubConfig>) -> crate::Result<Self> {
        let mut callback =
            Url::parse(public_url).map_err(|_| "invalid identity callback origin")?;
        if callback.scheme() != "https"
            || callback.host_str().is_none()
            || !callback.username().is_empty()
            || callback.password().is_some()
            || callback.path() != "/"
            || callback.query().is_some()
            || callback.fragment().is_some()
        {
            return Err("identity callback origin must be HTTPS with no credentials, path, query or fragment".into());
        }
        callback.set_path("/oauth/callback/github");
        let github = github.map(|config| {
            if !credential(&config.client_id, 256) || !credential(&config.client_secret, 4096) {
                return Err("GitHub client_id and client_secret must be nonempty, bounded ASCII values without whitespace".into());
            }
            Ok::<_, crate::Error>(GitHubProvider {
                config,
                callback,
                client: http_client(true)?,
                endpoints: GitHubEndpoints {
                    authorize: Url::parse("https://github.com/login/oauth/authorize")?,
                    token: Url::parse("https://github.com/login/oauth/access_token")?,
                    api: Url::parse("https://api.github.com/")?,
                },
            })
        }).transpose()?;
        Ok(Self { github })
    }

    pub fn names(&self) -> Vec<&str> {
        if self.github.is_some() {
            vec!["github"]
        } else {
            Vec::new()
        }
    }

    pub fn enabled(&self, provider: &str) -> bool {
        provider == "github" && self.github.is_some()
    }

    /// Test-only transport override. Public deployments cannot select provider endpoints.
    #[cfg(test)]
    pub(crate) fn with_github_test_endpoints(
        public_url: &str,
        github: GitHubConfig,
        base: &str,
    ) -> crate::Result<Self> {
        let base = Url::parse(base)?;
        if base.scheme() != "http"
            || !base
                .host_str()
                .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]" | "::1"))
            || !base.username().is_empty()
            || base.password().is_some()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err("mock identity endpoint must be an HTTP loopback origin".into());
        }
        let mut providers = Self::new(public_url, Some(github))?;
        let github = providers.github.as_mut().unwrap();
        github.client = http_client(false)?;
        github.endpoints = GitHubEndpoints {
            authorize: base.join("login/oauth/authorize")?,
            token: base.join("login/oauth/access_token")?,
            api: base,
        };
        Ok(providers)
    }

    /// Resolve GitHub subjects for an initial ACL or an explicit grant. No username-to-ID cache is retained.
    pub async fn resolve_allow(&self, principals: &[String]) -> Result<Vec<Identity>, String> {
        if principals.is_empty() || principals.len() > MAX_ALLOWED_PRINCIPALS {
            return Err(format!(
                "allow list must contain 1..={MAX_ALLOWED_PRINCIPALS} identities"
            ));
        }
        let github = self.provider("github")?;
        let mut seen = HashSet::new();
        let mut logins = Vec::new();
        for principal in principals {
            let canonical = canonical_github_principal(principal)?;
            if seen.insert(canonical.clone()) {
                logins.push(canonical.strip_suffix("@github").unwrap().to_owned());
            }
        }
        // A bounded fan-out keeps a multi-user ACL below the SSH startup deadline.
        let identities: Vec<Identity> = stream::iter(logins.into_iter().map(|login| async move {
            let url = github
                .endpoints
                .api
                .join(&format!("users/{login}"))
                .map_err(|_| "invalid GitHub user endpoint")?;
            let response = github
                .client
                .get(url)
                .basic_auth(&github.config.client_id, Some(&github.config.client_secret))
                .header(header::ACCEPT, "application/vnd.github+json")
                .header("X-GitHub-Api-Version", API_VERSION)
                .send()
                .await
                .map_err(|_| "GitHub identity lookup failed")?;
            let identity = read_identity(response).await?;
            if !identity.login.eq_ignore_ascii_case(&login) {
                return Err(
                    "GitHub lookup returned a different username; update the allow list".into(),
                );
            }
            Ok::<Identity, String>(identity)
        }))
        .buffered(4)
        .try_collect()
        .await?;
        let mut subjects = HashSet::new();
        Ok(identities
            .into_iter()
            .filter(|identity| {
                subjects.insert((identity.provider.clone(), identity.subject.clone()))
            })
            .collect())
    }

    pub fn authorization_url(
        &self,
        provider: &str,
        state: &str,
        challenge: &str,
    ) -> Result<String, String> {
        let github = self.provider(provider)?;
        if !(32..=512).contains(&state.len())
            || !unreserved(state)
            || challenge.len() != 43
            || URL_SAFE_NO_PAD
                .decode(challenge)
                .map_or(true, |bytes| bytes.len() != 32)
        {
            return Err("invalid upstream OAuth state or S256 challenge".into());
        }
        let mut url = github.endpoints.authorize.clone();
        url.query_pairs_mut().extend_pairs([
            ("client_id", github.config.client_id.as_str()),
            ("redirect_uri", github.callback.as_str()),
            ("scope", ""),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ]);
        Ok(url.into())
    }

    pub async fn exchange(
        &self,
        provider: &str,
        code: &str,
        verifier: &str,
    ) -> Result<Identity, String> {
        let github = self.provider(provider)?;
        if !credential(code, 1024) || !(43..=128).contains(&verifier.len()) || !unreserved(verifier)
        {
            return Err("invalid upstream authorization code or PKCE verifier".into());
        }
        let response = github
            .client
            .post(github.endpoints.token.clone())
            .header(header::ACCEPT, "application/json")
            .form(&[
                ("client_id", github.config.client_id.as_str()),
                ("client_secret", github.config.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", github.callback.as_str()),
                ("code_verifier", verifier),
            ])
            .send()
            .await
            .map_err(|_| "GitHub token exchange failed")?;
        let token: GitHubToken = bounded_json(response).await?;
        if token.error.is_some() {
            return Err("GitHub rejected the authorization code".into());
        }
        let access = token
            .access_token
            .filter(|token| {
                !token.is_empty()
                    && token.len() <= 4096
                    && token
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b))
            })
            .ok_or("GitHub returned an invalid access token")?;
        if !token
            .token_type
            .is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
        {
            return Err("GitHub returned an unsupported token type".into());
        }
        let response = github
            .client
            .get(
                github
                    .endpoints
                    .api
                    .join("user")
                    .map_err(|_| "invalid GitHub identity endpoint")?,
            )
            .bearer_auth(&access)
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await
            .map_err(|_| "GitHub authenticated identity lookup failed")?;
        // The upstream token dies here; ssmcp issues its own independently bound grant.
        read_identity(response).await
    }

    fn provider(&self, provider: &str) -> Result<&GitHubProvider, String> {
        match provider {
            "github" => self
                .github
                .as_ref()
                .ok_or_else(|| "GitHub identity provider is not configured".into()),
            _ => Err("unsupported identity provider".into()),
        }
    }
}

fn credential(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.len() <= maximum && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn unreserved(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
}

fn http_client(https_only: bool) -> crate::Result<Client> {
    Client::builder()
        .https_only(https_only)
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .user_agent(concat!("ssmcp/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| "could not create GitHub HTTP client".into())
}

async fn bounded_json<T: DeserializeOwned>(mut response: Response) -> Result<T, String> {
    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "GitHub identity request failed (HTTP {})",
            response.status().as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err("GitHub identity response exceeds the size limit".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "could not read GitHub identity response")?
    {
        if chunk.len() > MAX_RESPONSE_BYTES - bytes.len() {
            return Err("GitHub identity response exceeds the size limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| "GitHub returned invalid identity JSON".into())
}

async fn read_identity(response: Response) -> Result<Identity, String> {
    let user: GitHubUser = bounded_json(response).await?;
    if user.id == 0
        || user.account_type != "User"
        || canonical_github_principal(&format!("{}@github", user.login)).is_err()
    {
        return Err("GitHub returned an invalid user identity".into());
    }
    Ok(Identity {
        provider: "github".into(),
        subject: user.id.to_string(),
        login: user.login,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::{Request, State},
        http::{HeaderMap, StatusCode},
        response::Response,
    };
    use serde_json::{Value, json};
    use std::{
        collections::{BTreeMap, HashMap},
        sync::{Arc, Mutex},
    };
    use tokio::{net::TcpListener, task::JoinHandle};

    const PUBLIC: &str = "https://ssmcp.example.test";
    const TOKEN_PATH: &str = "/login/oauth/access_token";
    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn config() -> GitHubConfig {
        GitHubConfig {
            client_id: "fixture-client".into(),
            client_secret: "fixture-client-secret".into(),
        }
    }

    fn user(id: u64, login: &str) -> Value {
        json!({"id":id, "login":login, "type":"User"})
    }

    fn token() -> Value {
        json!({"access_token":"fixture-access-token", "token_type":"bearer", "scope":""})
    }

    #[derive(Clone)]
    struct Reply {
        status: StatusCode,
        body: String,
        location: Option<String>,
        chunked: bool,
    }

    struct Recorded {
        method: String,
        path: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    #[derive(Default)]
    struct MockState {
        replies: HashMap<String, Reply>,
        requests: Vec<Recorded>,
    }

    struct Mock {
        base: String,
        state: Arc<Mutex<MockState>>,
        task: JoinHandle<()>,
    }

    impl Mock {
        async fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(Mutex::new(MockState::default()));
            let router = Router::new()
                .fallback(mock_request)
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            Self { base, state, task }
        }

        fn reply(&self, path: &str, value: Value) {
            self.state.lock().unwrap().replies.insert(
                path.into(),
                Reply {
                    status: StatusCode::OK,
                    body: value.to_string(),
                    location: None,
                    chunked: false,
                },
            );
        }

        fn providers(&self) -> IdentityProviders {
            IdentityProviders::with_github_test_endpoints(PUBLIC, config(), &self.base).unwrap()
        }
    }

    impl Drop for Mock {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn mock_request(
        State(state): State<Arc<Mutex<MockState>>>,
        request: Request,
    ) -> Response {
        let (parts, body) = request.into_parts();
        let body = to_bytes(body, 16 * 1024).await.unwrap().to_vec();
        let reply = {
            let mut state = state.lock().unwrap();
            let path = parts.uri.path().to_owned();
            state.requests.push(Recorded {
                method: parts.method.to_string(),
                path: path.clone(),
                headers: parts.headers,
                body,
            });
            state.replies.get(&path).cloned().unwrap_or(Reply {
                status: StatusCode::NOT_FOUND,
                body: "{}".into(),
                location: None,
                chunked: false,
            })
        };
        let mut response = Response::builder()
            .status(reply.status)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(location) = reply.location {
            response = response.header(header::LOCATION, location);
        }
        let body = if reply.chunked {
            let chunks: Vec<_> = reply
                .body
                .as_bytes()
                .chunks(16384)
                .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
                .collect();
            Body::from_stream(stream::iter(chunks))
        } else {
            Body::from(reply.body)
        };
        response.body(body).unwrap()
    }

    #[test]
    fn configuration_requires_https_and_authorization_uses_minimal_scope_and_s256() {
        for origin in [
            "http://ssmcp.example.test",
            "https://user:secret@host",
            "https://host/path",
            "https://host/?query",
            "https://host/#fragment",
        ] {
            assert!(IdentityProviders::new(origin, Some(config())).is_err());
        }
        assert!(
            IdentityProviders::new(
                PUBLIC,
                Some(GitHubConfig {
                    client_id: String::new(),
                    client_secret: "secret".into()
                })
            )
            .is_err()
        );
        assert!(serde_json::from_value::<GitHubConfig>(json!({"client_id":"id", "client_secret":"secret", "token_url":"https://attacker.test"})).is_err());
        let providers = IdentityProviders::new(PUBLIC, Some(config())).unwrap();
        assert_eq!(providers.names(), vec!["github"]);
        assert!(providers.enabled("github"));
        assert!(!providers.enabled("other"));
        let state = "a".repeat(64);
        let url = Url::parse(
            &providers
                .authorization_url("github", &state, CHALLENGE)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(url.origin().ascii_serialization(), "https://github.com");
        assert_eq!(url.path(), "/login/oauth/authorize");
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "fixture-client");
        assert_eq!(
            query["redirect_uri"],
            format!("{PUBLIC}/oauth/callback/github")
        );
        assert_eq!(query["state"], state);
        assert_eq!(query["scope"], "");
        assert_eq!(query["code_challenge"], CHALLENGE);
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!url.as_str().contains("fixture-client-secret"));
        assert!(
            providers
                .authorization_url("other", &state, CHALLENGE)
                .is_err()
        );
        assert!(
            providers
                .authorization_url("github", "short", CHALLENGE)
                .is_err()
        );
        assert!(
            providers
                .authorization_url("github", &state, &"/".repeat(43))
                .is_err()
        );
        let disabled = IdentityProviders::new(PUBLIC, None).unwrap();
        assert!(disabled.names().is_empty());
        assert!(!disabled.enabled("github"));
        assert!(
            disabled
                .authorization_url("github", &state, CHALLENGE)
                .is_err()
        );
    }

    #[tokio::test]
    async fn exchange_verifies_current_user_and_retains_only_stable_numeric_identity() {
        let mock = Mock::new().await;
        mock.reply(TOKEN_PATH, token());
        mock.reply("/user", user(9_007_199_254_740_993, "Octocat"));
        let identity = mock
            .providers()
            .exchange("github", "fixture-code", VERIFIER)
            .await
            .unwrap();
        assert_eq!(
            identity,
            Identity {
                provider: "github".into(),
                subject: "9007199254740993".into(),
                login: "Octocat".into()
            }
        );
        let state = mock.state.lock().unwrap();
        assert_eq!(state.requests.len(), 2);
        let exchange = &state.requests[0];
        assert_eq!(exchange.method, "POST");
        assert_eq!(exchange.path, TOKEN_PATH);
        assert_eq!(exchange.headers[header::ACCEPT], "application/json");
        let form: BTreeMap<String, String> = serde_urlencoded::from_bytes(&exchange.body).unwrap();
        assert_eq!(form["client_id"], "fixture-client");
        assert_eq!(form["client_secret"], "fixture-client-secret");
        assert_eq!(form["code_verifier"], VERIFIER);
        assert_eq!(form["code"], "fixture-code");
        assert_eq!(
            form["redirect_uri"],
            format!("{PUBLIC}/oauth/callback/github")
        );
        let profile = &state.requests[1];
        assert_eq!(profile.method, "GET");
        assert_eq!(profile.path, "/user");
        assert_eq!(
            profile.headers[header::AUTHORIZATION],
            "Bearer fixture-access-token"
        );
        assert_eq!(profile.headers["x-github-api-version"], API_VERSION);
    }

    #[tokio::test]
    async fn upstream_errors_malformed_tokens_and_invalid_ids_fail_without_secret_echo() {
        let mock = Mock::new().await;
        let providers = mock.providers();
        for value in [
            json!({"error":"bad_verification_code", "error_description":"fixture-client-secret fixture-access-token"}),
            json!({"access_token":"fixture-access-token", "token_type":"mac"}),
            json!({"access_token":"bad\nsecret", "token_type":"bearer"}),
            json!({"token_type":"bearer"}),
        ] {
            mock.reply(TOKEN_PATH, value);
            let error = providers
                .exchange("github", "fixture-code", VERIFIER)
                .await
                .unwrap_err();
            assert!(
                !error.contains("fixture-client-secret") && !error.contains("fixture-access-token")
            );
        }
        assert!(
            mock.state
                .lock()
                .unwrap()
                .requests
                .iter()
                .all(|request| request.path == TOKEN_PATH)
        );
        mock.reply(TOKEN_PATH, token());
        for id in [json!(0), json!(-1), json!(1.5), json!("123"), Value::Null] {
            mock.reply("/user", json!({"id":id, "login":"octocat", "type":"User"}));
            assert!(
                providers
                    .exchange("github", "fixture-code", VERIFIER)
                    .await
                    .is_err()
            );
        }
        for value in [
            json!({"id":1, "login":"octocat", "type":"Organization"}),
            user(1, "bad/user"),
            user(1, ""),
        ] {
            mock.reply("/user", value);
            assert!(
                providers
                    .exchange("github", "fixture-code", VERIFIER)
                    .await
                    .is_err()
            );
        }
        let count = mock.state.lock().unwrap().requests.len();
        assert!(
            providers
                .exchange("github", "fixture-code", "short")
                .await
                .is_err()
        );
        assert!(
            providers
                .exchange("other", "fixture-code", VERIFIER)
                .await
                .is_err()
        );
        assert_eq!(mock.state.lock().unwrap().requests.len(), count);
    }

    #[tokio::test]
    async fn allow_resolution_is_bounded_canonical_atomic_and_not_cached() {
        let mock = Mock::new().await;
        mock.reply("/users/octocat", user(1, "Octocat"));
        mock.reply("/users/alice", user(2, "alice"));
        let providers = mock.providers();
        let identities = providers
            .resolve_allow(&[
                "Octocat@GitHub".into(),
                "octocat@github".into(),
                "alice@github".into(),
            ])
            .await
            .unwrap();
        assert_eq!(
            identities
                .iter()
                .map(|id| id.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["1", "2"]
        );
        {
            let state = mock.state.lock().unwrap();
            assert_eq!(state.requests.len(), 2);
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD
                    .encode("fixture-client:fixture-client-secret")
            );
            assert!(state.requests.iter().all(|request| {
                request.headers[header::AUTHORIZATION] == expected && request.body.is_empty()
            }));
        }
        mock.reply("/users/octocat", user(3, "Octocat"));
        assert_eq!(
            providers
                .resolve_allow(&["octocat@github".into()])
                .await
                .unwrap()[0]
                .subject,
            "3"
        );
        assert_eq!(identities[0].subject, "1");
        assert!(
            providers
                .resolve_allow(&["octocat@github".into(), "missing@github".into()])
                .await
                .is_err()
        );
        mock.reply("/users/octocat", user(3, "different-user"));
        assert!(
            providers
                .resolve_allow(&["octocat@github".into()])
                .await
                .is_err()
        );
        let count = mock.state.lock().unwrap().requests.len();
        for principals in [
            vec![],
            vec!["../user@github".into()],
            vec!["alice@other".into()],
            vec!["alice@github".into(); MAX_ALLOWED_PRINCIPALS + 1],
        ] {
            assert!(providers.resolve_allow(&principals).await.is_err());
        }
        assert_eq!(mock.state.lock().unwrap().requests.len(), count);
        assert!(
            IdentityProviders::new(PUBLIC, None)
                .unwrap()
                .resolve_allow(&["alice@github".into()])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn provider_transport_rejects_redirects_oversized_bodies_and_insecure_urls() {
        let mock = Mock::new().await;
        let providers = mock.providers();
        mock.state.lock().unwrap().replies.insert(
            TOKEN_PATH.into(),
            Reply {
                status: StatusCode::FOUND,
                body: "fixture-client-secret".into(),
                location: Some(format!("{}/leak", mock.base)),
                chunked: false,
            },
        );
        let error = providers
            .exchange("github", "fixture-code", VERIFIER)
            .await
            .unwrap_err();
        assert!(!error.contains("fixture-client-secret"));
        assert_eq!(mock.state.lock().unwrap().requests.len(), 1);
        for chunked in [false, true] {
            mock.state.lock().unwrap().replies.insert(
                TOKEN_PATH.into(),
                Reply {
                    status: StatusCode::OK,
                    body: "x".repeat(MAX_RESPONSE_BYTES + 1),
                    location: None,
                    chunked,
                },
            );
            let error = providers
                .exchange("github", "fixture-code", VERIFIER)
                .await
                .unwrap_err();
            assert!(error.contains("size limit"));
        }
        let mut production = IdentityProviders::new(PUBLIC, Some(config())).unwrap();
        // Keep the production HTTPS-only client while pointing at an insecure test endpoint.
        production.github.as_mut().unwrap().endpoints.token =
            Url::parse(&format!("{}{TOKEN_PATH}", mock.base)).unwrap();
        let count = mock.state.lock().unwrap().requests.len();
        assert!(
            production
                .exchange("github", "fixture-code", VERIFIER)
                .await
                .is_err()
        );
        assert_eq!(mock.state.lock().unwrap().requests.len(), count);
        assert!(
            IdentityProviders::with_github_test_endpoints(PUBLIC, config(), "http://example.test")
                .is_err()
        );
    }
}
