use super::*;
use crate::identity::{GitHubConfig, Identity, IdentityProviders};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
use tokio::{net::TcpListener, task::JoinHandle};

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ssmcp-setup-test-{}", random().unwrap()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn identity(subject: &str, login: &str) -> Identity {
    Identity {
        provider: "github".into(),
        subject: subject.into(),
        login: login.into(),
    }
}
fn public_key(seed: u8) -> String {
    let mut bytes = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
    bytes.extend_from_slice(&[seed; 32]);
    format!(
        "ssh-ed25519 {} setup-test",
        super::super::STANDARD.encode(bytes)
    )
}
struct Fixture {
    broker: Arc<Broker>,
    base: String,
    client: reqwest::Client,
    server: JoinHandle<()>,
    upstream: JoinHandle<()>,
    _temporary: Temporary,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
        self.upstream.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
        let mock=Router::new()
            .route("/login/oauth/access_token",post(
                |axum::Form(fields):axum::Form<HashMap<String,String>>|async move{
                    assert_eq!(fields["redirect_uri"],"https://ssmcp.test/oauth/callback/github");
                    assert_eq!(fields["code_verifier"].len(),43);
                    Json(json!({"access_token":"test-setup-upstream-token","token_type":"bearer"}))
                }))
            .route("/user",get(||async{Json(json!({"id":123,"login":"alice","type":"User"}))}));
        let upstream = tokio::spawn(async move { axum::serve(upstream, mock).await.unwrap() });
        let temporary = Temporary::new();
        let mut broker = Broker::new("https://ssmcp.test".into());
        broker.identities = IdentityProviders::with_github_test_endpoints(
            "https://ssmcp.test",
            GitHubConfig {
                client_id: "fixture-app".into(),
                client_secret: "fixture-secret".into(),
            },
            &upstream_url,
        )
        .unwrap();
        broker.portal = Portal::open(&temporary.0).unwrap();
        broker
            .portal
            .storage(|store| {
                store.login(&identity("123", "alice"))?;
                store.login(&identity("456", "bob"))?;
                store.add("456", parse_key(&public_key(2), "bob-only key")?)?;
                Ok(())
            })
            .await
            .unwrap();
        let broker = Arc::new(broker);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Self {
            broker,
            base,
            client,
            server,
            upstream,
            _temporary: temporary,
        }
    }
    async fn start(&self, name: &str) -> Value {
        let response = self
            .client
            .post(format!("{}/api/setup/start", self.base))
            .json(&json!({"client_name":name}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        response.json().await.unwrap()
    }
    fn session(&self, subject: &str) -> (String, String) {
        let cookie = random().unwrap();
        let csrf = random().unwrap();
        self.broker.portal.browser.lock().unwrap().sessions.insert(
            digest(&cookie),
            Session {
                subject: Some(subject.into()),
                csrf: csrf.clone(),
                expires: Instant::now() + super::super::SESSION_TTL,
            },
        );
        (format!("{}={cookie}", super::super::COOKIE), csrf)
    }
    async fn reset_poll(&self, secret: &str) {
        let grant = self
            .broker
            .portal
            .setup
            .lock()
            .unwrap()
            .by_device(secret)
            .unwrap()
            .unwrap();
        grant.lock().await.last_poll = None;
    }
    async fn poll(&self, secret: &str) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/api/setup/poll", self.base))
            .json(&json!({"device_code":secret}))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
    async fn consent(
        &self,
        cookie: &str,
        csrf: &str,
        code: &str,
        decision: &str,
    ) -> reqwest::Response {
        self.client
            .post(format!("{}/setup/authorize", self.base))
            .header("cookie", cookie)
            .header("origin", "https://ssmcp.test")
            .form(&[("csrf", csrf), ("code", code), ("decision", decision)])
            .send()
            .await
            .unwrap()
    }
    async fn register(&self, secret: &str, key: &str) -> (StatusCode, Value) {
        let response = self
            .client
            .post(format!("{}/api/setup/key", self.base))
            .json(&json!({"device_code":secret,"public_key":key,"label":"this computer"}))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap())
    }
}

#[test]
fn grant_capacity_expiry_and_callback_binding_are_bounded() {
    let mut state = SetupState::default();
    let (first_secret, first_code) = state.issue("first computer".into()).unwrap();
    assert!(valid_secret(&first_secret));
    assert!(valid_code(&first_code));
    for index in 1..MAX_GRANTS {
        state.issue(format!("computer {index}")).unwrap();
    }
    assert_eq!(state.grants.len(), MAX_GRANTS);
    assert_eq!(state.codes.len(), MAX_GRANTS);
    assert_eq!(
        state.issue("overflow".into()).unwrap_err().code,
        "slow_down"
    );
    state
        .grants
        .get_mut(&digest(&first_secret))
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    state.prune();
    assert!(!state.codes.contains_key(&first_code));
    assert!(state.by_device(&first_secret).unwrap().is_none());
    state.issue("replacement".into()).unwrap();
    assert_eq!(state.grants.len(), MAX_GRANTS);
    let portal = Portal::default();
    let (secret, code) = portal
        .setup
        .lock()
        .unwrap()
        .issue("callback test".into())
        .unwrap();
    let (bound, _) = portal.setup.lock().unwrap().by_code(&code).unwrap();
    assert_eq!(
        bound.location(&portal).unwrap(),
        format!("/setup?code={code}")
    );
    let wrong = ReturnTo {
        device: digest(&secret),
        code: "https://evil.invalid/".into(),
    };
    assert_eq!(wrong.location(&portal).unwrap(), "/setup");
    portal.setup.lock().unwrap().grants.remove(&digest(&secret));
    assert_eq!(bound.location(&portal).unwrap(), "/setup");
}

#[test]
fn comparison_code_alone_cannot_authorize_and_decisions_are_owner_bound() {
    let mut grant = Grant {
        client_name: "test".into(),
        expires: Instant::now() + TTL,
        decision: Decision::Pending,
        last_poll: None,
        registered_fingerprint: None,
        registration_saved: false,
    };
    assert_eq!(grant.owner().unwrap_err().code, "authorization_required");
    grant.decide("123", true).unwrap();
    assert_eq!(grant.owner().unwrap(), "123");
    assert!(grant.decide("456", true).is_err());
    assert!(grant.decide("456", false).is_err());
    grant.decide("123", true).unwrap();
    assert_eq!(grant.owner().unwrap(), "123");
    grant.expires = Instant::now() - Duration::from_secs(1);
    assert_eq!(grant.owner().unwrap_err().code, "expired_token");
    assert_eq!(grant.decide("123", true).unwrap_err().code, "expired_token");
}

#[test]
fn setup_html_escapes_device_names_and_explains_the_exact_permission() {
    let account = Account {
        subject: "123".into(),
        username: "gh-123".into(),
        github_login: "alice".into(),
        keys: vec![],
    };
    let page = consent_page(
        "<script>alert('x')</script>",
        "ABCDE-FGHJK",
        "csrf",
        &account,
    );
    assert!(!page.contains("<script>"));
    assert!(page.contains("&lt;script&gt;"));
    assert!(page.contains("ABCDE-FGHJK"));
    assert!(page.contains("一把新的 Ed25519 公钥"));
    assert!(page.contains("私钥始终保留在电脑上"));
    assert!(page.contains("只批准你自己刚发起的请求"));
    assert!(page.contains("name=\"decision\" value=\"allow\""));
    assert!(page.contains("name=\"decision\" value=\"deny\""));

    if let Some(directory) = std::env::var_os("SSMCP_SETUP_PREVIEW_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("index.html"),
            consent_page(
                "computer-use · MacBook Pro",
                "K7M2R-H8T4W",
                "synthetic-preview-only",
                &account,
            ),
        )
        .unwrap();
        fs::write(directory.join("portal.css"), view::CSS).unwrap();
    }
}

#[tokio::test]
async fn browser_pkce_return_requires_explicit_consent_before_device_authority() {
    let fixture = Fixture::new().await;
    let grant = fixture.start("<script>my computer</script>").await;
    let secret = grant["device_code"].as_str().unwrap();
    let short = grant["user_code"].as_str().unwrap();
    assert_eq!(grant["expires_in"], 600);
    assert_eq!(grant["interval"], 2);
    assert_eq!(
        grant["verification_uri_complete"],
        format!("https://ssmcp.test/setup?code={short}")
    );
    let (status, state) = fixture.poll(secret).await;
    assert_eq!(status, 200);
    assert_eq!(state["status"], "pending");
    assert!(state.get("account").is_none());
    let (status, error) = fixture.register(secret, &public_key(1)).await;
    assert_eq!(status, 401);
    assert_eq!(error["error"], "authorization_required");
    let response = fixture
        .client
        .get(format!("{}/setup?code={short}", fixture.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 303);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let login = reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let params: HashMap<_, _> = login.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "fixture-app");
    assert_eq!(params["code_challenge_method"], "S256");
    let callback = format!(
        "{}/oauth/callback/github?state={}&code=fixture-code&return_to=https%3A%2F%2Fevil.invalid",
        fixture.base, params["state"]
    );
    assert_eq!(
        fixture.client.get(&callback).send().await.unwrap().status(),
        401
    );
    let (other_cookie, _) = fixture.session("456");
    assert_eq!(
        fixture
            .client
            .get(&callback)
            .header("cookie", &other_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let callback_response = fixture
        .client
        .get(&callback)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(callback_response.status(), 303);
    assert_eq!(
        callback_response.headers()["location"],
        format!("/setup?code={short}")
    );
    let authenticated = callback_response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_ne!(authenticated, cookie);
    assert_eq!(
        fixture
            .client
            .get(&callback)
            .header("cookie", &authenticated)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    fixture.reset_poll(secret).await;
    assert_eq!(
        fixture.poll(secret).await.1["status"],
        "pending",
        "GitHub login alone must not grant device authority"
    );
    let consent = fixture
        .client
        .get(format!("{}/setup?code={short}", fixture.base))
        .header("cookie", &authenticated)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!consent.contains("<script>"));
    assert!(consent.contains("&lt;script&gt;"));
    assert!(!consent.contains(secret));
    let csrf = consent
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let wrong_origin = fixture
        .client
        .post(format!("{}/setup/authorize", fixture.base))
        .header("cookie", &authenticated)
        .header("origin", "https://evil.invalid")
        .form(&[("csrf", csrf), ("code", short), ("decision", "allow")])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_origin.status(), 403);
    assert_eq!(
        fixture
            .consent(&authenticated, "wrong", short, "allow")
            .await
            .status(),
        403
    );
    assert_eq!(
        fixture
            .consent(&authenticated, csrf, short, "allow")
            .await
            .status(),
        303
    );
    fixture.reset_poll(secret).await;
    let (status, authorized) = fixture.poll(secret).await;
    assert_eq!(status, 200);
    assert_eq!(authorized["status"], "authorized");
    assert_eq!(authorized["account"]["github_login"], "alice");
    assert_eq!(authorized["account"]["username"], "gh-123");
    assert!(authorized["account"]["keys"].as_array().unwrap().is_empty());
    assert!(authorized["account"].get("subject").is_none());
    assert!(!authorized.to_string().contains("bob-only"));
}

#[tokio::test]
async fn one_key_registration_is_idempotent_isolated_revocable_and_not_resurrected() {
    let fixture = Fixture::new().await;
    let grant = fixture.start("alice laptop").await;
    let secret = grant["device_code"].as_str().unwrap();
    let short = grant["user_code"].as_str().unwrap();
    let (alice, alice_csrf) = fixture.session("123");
    let (bob, bob_csrf) = fixture.session("456");
    assert_eq!(
        fixture
            .consent(&alice, &alice_csrf, short, "allow")
            .await
            .status(),
        303
    );
    assert_eq!(
        fixture
            .consent(&bob, &bob_csrf, short, "allow")
            .await
            .status(),
        403
    );
    let injected = fixture
        .client
        .post(format!("{}/api/setup/key", fixture.base))
        .json(
            &json!({"device_code":secret,"public_key":public_key(1),"label":"test","owner":"456"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(injected.status(), 400);
    assert_eq!(
        fixture
            .register(secret, "-----BEGIN OPENSSH PRIVATE KEY-----")
            .await
            .0,
        400
    );
    let (status, saved) = fixture.register(secret, &public_key(1)).await;
    assert_eq!(status, 200);
    assert_eq!(saved["status"], "saved");
    assert_eq!(saved["account"]["username"], "gh-123");
    assert_eq!(saved["account"]["keys"].as_array().unwrap().len(), 1);
    let (status, retry) = fixture.register(secret, &public_key(1)).await;
    assert_eq!(status, 200);
    assert_eq!(retry, saved);
    let (status, different) = fixture.register(secret, &public_key(3)).await;
    assert_eq!(status, 409);
    assert_eq!(different["error"], "key_already_registered");
    let bob_count = fixture
        .broker
        .portal
        .storage(|store| Ok(store.account("456")?.keys.len()))
        .await
        .unwrap();
    assert_eq!(bob_count, 1);
    let fingerprint = parse_key(&public_key(1), "").unwrap().fingerprint;
    fixture
        .broker
        .portal
        .storage(move |store| store.remove("123", &fingerprint))
        .await
        .unwrap();
    let (status, removed) = fixture.register(secret, &public_key(1)).await;
    assert_eq!(status, 409);
    assert_eq!(removed["error"], "key_registration_revoked");
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| Ok(store.account("123")?.keys.is_empty()))
            .await
            .unwrap()
    );
    let cancelled = fixture
        .client
        .post(format!("{}/api/setup/cancel", fixture.base))
        .json(&json!({"device_code":secret}))
        .send()
        .await
        .unwrap();
    assert_eq!(cancelled.status(), 200);
    assert_eq!(
        cancelled.json::<Value>().await.unwrap()["status"],
        "cancelled"
    );
    assert_eq!(fixture.poll(secret).await.1["status"], "expired");
    assert_eq!(fixture.register(secret, &public_key(1)).await.0, 410);
    let again = fixture
        .client
        .post(format!("{}/api/setup/cancel", fixture.base))
        .json(&json!({"device_code":secret}))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 200);
}

#[tokio::test]
async fn denied_expired_and_poll_rate_limit_never_expose_accounts() {
    let fixture = Fixture::new().await;
    let (cookie, csrf) = fixture.session("123");
    let denied = fixture.start("denied computer").await;
    let denied_secret = denied["device_code"].as_str().unwrap();
    assert_eq!(
        fixture
            .consent(
                &cookie,
                &csrf,
                denied["user_code"].as_str().unwrap(),
                "deny"
            )
            .await
            .status(),
        303
    );
    let (status, body) = fixture.poll(denied_secret).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "denied");
    assert!(body.get("account").is_none());
    assert_eq!(fixture.register(denied_secret, &public_key(1)).await.0, 403);
    assert_eq!(
        fixture
            .consent(
                &cookie,
                &csrf,
                denied["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        403
    );
    let pending = fixture.start("pending computer").await;
    let pending_secret = pending["device_code"].as_str().unwrap();
    assert_eq!(fixture.poll(pending_secret).await.1["status"], "pending");
    let too_fast = fixture
        .client
        .post(format!("{}/api/setup/poll", fixture.base))
        .json(&json!({"device_code":pending_secret}))
        .send()
        .await
        .unwrap();
    assert_eq!(too_fast.status(), 429);
    assert_eq!(too_fast.headers()["retry-after"], "2");
    assert_eq!(
        too_fast.json::<Value>().await.unwrap()["error"],
        "slow_down"
    );
    {
        let mut state = fixture.broker.portal.setup.lock().unwrap();
        state
            .grants
            .get_mut(&digest(pending_secret))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
    }
    let (status, expired) = fixture.poll(pending_secret).await;
    assert_eq!(status, 200);
    assert_eq!(expired["status"], "expired");
    assert!(expired.get("account").is_none());
    assert_eq!(
        fixture.register(pending_secret, &public_key(1)).await.0,
        410
    );
    assert_eq!(
        fixture
            .client
            .get(format!(
                "{}/setup?code={}",
                fixture.base,
                pending["user_code"].as_str().unwrap()
            ))
            .send()
            .await
            .unwrap()
            .status(),
        410
    );
}

#[tokio::test]
async fn setup_apis_reject_cross_origin_unknown_duplicate_and_oversized_input() {
    let fixture = Fixture::new().await;
    let endpoint = format!("{}/api/setup/start", fixture.base);
    for body in [
        r#"{"client_name":"one","client_name":"two"}"#,
        r#"{"client_name":"one","unknown":"value"}"#,
        r#"{"client_name":""}"#,
        r#"{"client_name":"line\nbreak"}"#,
    ] {
        let response = fixture
            .client
            .post(&endpoint)
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"],
            "invalid_request"
        );
    }
    let oversized = fixture
        .client
        .post(&endpoint)
        .header("content-type", "application/json")
        .body(format!("{{\"client_name\":\"{}\"}}", "x".repeat(5000)))
        .send()
        .await
        .unwrap();
    assert_eq!(oversized.status(), 400);
    let wrong_origin = fixture
        .client
        .post(&endpoint)
        .header("origin", "https://evil.invalid")
        .json(&json!({"client_name":"not same origin"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_origin.status(), 403);
    let missing_type = fixture
        .client
        .post(&endpoint)
        .body(r#"{"client_name":"computer"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(missing_type.status(), 400);
    let wrong_method = fixture.client.get(&endpoint).send().await.unwrap();
    assert_eq!(wrong_method.status(), 405);
    assert_eq!(
        wrong_method.json::<Value>().await.unwrap()["error"],
        "invalid_request"
    );
    let grant = fixture.start("valid").await;
    let invalid_query = fixture
        .client
        .get(format!(
            "{}/setup?code={}&return_to=https://evil.invalid",
            fixture.base,
            grant["user_code"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_query.status(), 400);
    let fake_secret = fixture
        .client
        .post(format!("{}/api/setup/poll", fixture.base))
        .json(&json!({"device_code":"not a secret"}))
        .send()
        .await
        .unwrap();
    assert_eq!(fake_secret.status(), 400);
}
