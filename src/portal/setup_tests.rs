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
            .route("/user",get(||async{Json(json!({"id":123,"login":"alice","type":"User"}))}))
            .route("/users/alice",get(||async{Json(json!({"id":123,"login":"alice","type":"User"}))}))
            // Simulate a renamed/reassigned login: bob no longer maps to subject 456.
            .route("/users/bob",get(||async{Json(json!({"id":999,"login":"bob","type":"User"}))}));
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
                Ok(())
            })
            .await
            .unwrap();
        let broker = Arc::new(broker);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::http::router(broker.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = crate::http_client::builder()
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
    async fn consent(
        &self,
        cookie: &str,
        csrf: &str,
        code: &str,
        decision: &str,
    ) -> reqwest::Response {
        self.consent_with_checks(cookie, csrf, code, decision, &[])
            .await
    }
    async fn consent_with_checks(
        &self,
        cookie: &str,
        csrf: &str,
        code: &str,
        decision: &str,
        checks: &[(&str, &str)],
    ) -> reqwest::Response {
        let mut fields = vec![("csrf", csrf), ("code", code), ("decision", decision)];
        fields.extend_from_slice(checks);
        self.client
            .post(format!("{}/setup/authorize", self.base))
            .header("cookie", cookie)
            .header("origin", "https://ssmcp.test")
            .form(&fields)
            .send()
            .await
            .unwrap()
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
        requested_username: None,
        registration_only: false,
        device_only: false,
        expires: Instant::now() + TTL,
        decision: Decision::Pending,
        last_poll: None,
        login_issued: false,
    };
    assert!(matches!(grant.decision, Decision::Pending));
    grant.decide("123", true).unwrap();
    assert!(matches!(&grant.decision, Decision::Authorized(owner) if owner == "123"));
    assert!(grant.decide("456", true).is_err());
    assert!(grant.decide("456", false).is_err());
    grant.decide("123", true).unwrap();
    assert!(matches!(&grant.decision, Decision::Authorized(owner) if owner == "123"));
    grant.expires = Instant::now() - Duration::from_secs(1);
    assert_eq!(grant.decide("123", true).unwrap_err().code, "expired_token");
}

#[tokio::test]
async fn client_login_is_purpose_bound_browser_consented_and_single_use() {
    let fixture = Fixture::new().await;
    let response = fixture
        .client
        .post(format!("{}/api/client/login/start", fixture.base))
        .json(&json!({"name":"laptop-a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let start: Value = response.json().await.unwrap();
    let secret = start["device_code"].as_str().unwrap();
    let code = start["user_code"].as_str().unwrap();
    assert!(
        !start["verification_uri_complete"]
            .as_str()
            .unwrap()
            .contains(secret)
    );
    for path in [
        "/api/setup/start",
        "/api/setup/poll",
        "/api/setup/key",
        "/api/setup/cancel",
    ] {
        assert_eq!(
            fixture
                .client
                .post(format!("{}{path}", fixture.base))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
    }
    let poll = || {
        fixture
            .client
            .post(format!("{}/api/client/login/poll", fixture.base))
            .json(&json!({"device_code":secret}))
            .send()
    };
    let pending: Value = poll().await.unwrap().json().await.unwrap();
    assert_eq!(pending["status"], "pending");
    let (cookie, csrf) = fixture.session("123");
    let page = fixture
        .client
        .get(format!("{}/setup?code={code}", fixture.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("允许登录"));
    assert!(!page.contains("允许连接并登记公钥"));
    assert_eq!(
        fixture
            .consent(&cookie, &csrf, code, "allow")
            .await
            .status(),
        303
    );
    let completed = fixture
        .client
        .get(format!("{}/setup?code={code}", fixture.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(completed.contains("客户端会保存"));
    assert!(!completed.contains("客户端会检查已有 SSH"));
    fixture.reset_poll(secret).await;
    let (a, b) = tokio::join!(poll(), poll());
    let a: Value = a.unwrap().json().await.unwrap();
    let b: Value = b.unwrap().json().await.unwrap();
    assert_eq!(
        [&a, &b]
            .iter()
            .filter(|v| v["status"] == "authorized")
            .count(),
        1
    );
    let grant = if a["status"] == "authorized" { a } else { b };
    assert_eq!(grant["identity"]["subject"], "123");
    assert_eq!(grant["identity"]["principal"], "alice@github");
    let token = grant["access_token"].as_str().unwrap();
    assert_eq!(
        fixture
            .client
            .get(format!("{}/api/client/whoami", fixture.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
}

async fn named_login(fixture: &Fixture, username: &str) -> Value {
    let response = fixture
        .client
        .post(format!("{}/api/client/login/start", fixture.base))
        .json(&json!({"name":"laptop","username":username}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn poll_client(fixture: &Fixture, secret: &str) -> (StatusCode, Value) {
    let response = fixture
        .client
        .post(format!("{}/api/client/login/poll", fixture.base))
        .json(&json!({"device_code":secret}))
        .send()
        .await
        .unwrap();
    (response.status(), response.json().await.unwrap())
}

#[tokio::test]
async fn named_login_validates_before_oauth_and_claims_only_after_consent() {
    let fixture = Fixture::new().await;
    for username in ["root", "gh-123", "alice-", "invalid/name", "ab"] {
        let response = fixture
            .client
            .post(format!("{}/api/client/login/start", fixture.base))
            .json(&json!({"name":"laptop","username":username}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["error"], "username_invalid");
    }
    assert!(
        fixture
            .broker
            .portal
            .setup
            .lock()
            .unwrap()
            .grants
            .is_empty()
    );
    let start = named_login(&fixture, "ALice-home").await;
    let secret = start["device_code"].as_str().unwrap();
    let code = start["user_code"].as_str().unwrap();
    assert_eq!(start["username"], "alice-home");
    assert_eq!(start["username_available"], true);
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "pending");
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| store.handle_available("alice-home"))
            .await
            .unwrap()
    );
    let (cookie, csrf) = fixture.session("123");
    let account = fixture
        .broker
        .portal
        .storage(|store| store.account("123"))
        .await
        .unwrap();
    assert_eq!(account.subject, "123");
    let address = format!("{}@ssmcp.test", account.handle.as_deref().unwrap());
    let page = fixture
        .client
        .get(format!("{}/setup?code={code}", fixture.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains(&format!("<p>平台账户：<strong>{address}</strong></p>")));
    assert!(page.contains("<p>客户端请求使用用户名：<strong>alice-home</strong>"));
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| store.handle_available("alice-home"))
            .await
            .unwrap()
    );
    assert_eq!(
        fixture
            .consent(&cookie, &csrf, code, "allow")
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    fixture.reset_poll(secret).await;
    let (status, login) = poll_client(&fixture, secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(login["status"], "authorized");
    assert_eq!(login["identity"]["username"], "alice-home");
    assert_eq!(login["identity"]["subject"], "123");
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "expired");
}

#[tokio::test]
async fn concurrent_username_claims_have_one_owner_and_original_owner_can_login() {
    let fixture = Fixture::new().await;
    let first = named_login(&fixture, "shared-name").await;
    let second = named_login(&fixture, "Shared-name").await;
    let (alice_cookie, alice_csrf) = fixture.session("123");
    let (bob_cookie, bob_csrf) = fixture.session("456");
    let (alice, bob) = tokio::join!(
        fixture.consent(
            &alice_cookie,
            &alice_csrf,
            first["user_code"].as_str().unwrap(),
            "allow"
        ),
        fixture.consent(
            &bob_cookie,
            &bob_csrf,
            second["user_code"].as_str().unwrap(),
            "allow"
        )
    );
    assert_eq!(
        [alice.status(), bob.status()]
            .iter()
            .filter(|status| **status == StatusCode::SEE_OTHER)
            .count(),
        1
    );
    assert_eq!(
        [alice.status(), bob.status()]
            .iter()
            .filter(|status| **status == StatusCode::CONFLICT)
            .count(),
        1
    );
    let owner = fixture
        .broker
        .portal
        .storage(|store| store.account_by_handle("shared-name"))
        .await
        .unwrap()
        .unwrap();
    let (winner_cookie, winner_csrf, loser_cookie, loser_csrf) = if owner.subject == "123" {
        (alice_cookie, alice_csrf, bob_cookie, bob_csrf)
    } else {
        (bob_cookie, bob_csrf, alice_cookie, alice_csrf)
    };
    let existing = named_login(&fixture, "SHARED-name").await;
    assert_eq!(existing["username_available"], false);
    let code = existing["user_code"].as_str().unwrap();
    let secret = existing["device_code"].as_str().unwrap();
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "pending");
    assert_eq!(
        fixture
            .consent(&loser_cookie, &loser_csrf, code, "allow")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    // Poll immediately, without resetting the interval: the browser failure is
    // terminal and must take precedence over slow_down or pending.
    for _ in 0..2 {
        let (status, failure) = poll_client(&fixture, secret).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(failure["error"], "username_taken");
        assert!(failure.get("access_token").is_none());
    }
    assert_eq!(
        fixture
            .consent(&winner_cookie, &winner_csrf, code, "allow")
            .await
            .status(),
        StatusCode::CONFLICT
    );
    // The rightful owner can still log in through a fresh request.
    let retry = named_login(&fixture, "shared-name").await;
    assert_eq!(
        fixture
            .consent(
                &winner_cookie,
                &winner_csrf,
                retry["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let (_, login) = poll_client(&fixture, retry["device_code"].as_str().unwrap()).await;
    assert_eq!(login["identity"]["subject"], owner.subject);
    let extra = named_login(&fixture, "another-name").await;
    let extra_secret = extra["device_code"].as_str().unwrap();
    assert_eq!(
        poll_client(&fixture, extra_secret).await.1["status"],
        "pending"
    );
    assert_eq!(
        fixture
            .consent(
                &winner_cookie,
                &winner_csrf,
                extra["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        StatusCode::CONFLICT
    );
    let (status, failure) = poll_client(&fixture, extra_secret).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(failure["error"], "username_bound");
    assert!(failure.get("access_token").is_none());
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| store.handle_available("another-name"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn denied_and_banned_logins_do_not_issue_username_credentials() {
    let fixture = Fixture::new().await;
    let denied = named_login(&fixture, "declined-name").await;
    let (cookie, csrf) = fixture.session("123");
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
        StatusCode::SEE_OTHER
    );
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| store.handle_available("declined-name"))
            .await
            .unwrap()
    );
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
        StatusCode::FORBIDDEN
    );
    let pending = named_login(&fixture, "alice-home").await;
    assert_eq!(
        fixture
            .consent(
                &cookie,
                &csrf,
                pending["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    fixture
        .broker
        .portal
        .storage(|store| store.set_policy("123", Some(true), None))
        .await
        .unwrap();
    let (status, login) = poll_client(&fixture, pending["device_code"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(login["error"], "account_banned");
    assert!(login.get("access_token").is_none());
    // A subsequent unban cannot resurrect a request that already failed.
    fixture
        .broker
        .portal
        .storage(|store| store.set_policy("123", Some(false), None))
        .await
        .unwrap();
    let (status, retry) = poll_client(&fixture, pending["device_code"].as_str().unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(retry["error"], "account_banned");

    let blocked = named_login(&fixture, "alice-home").await;
    let secret = blocked["device_code"].as_str().unwrap();
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "pending");
    fixture
        .broker
        .portal
        .storage(|store| store.set_policy("123", Some(true), None))
        .await
        .unwrap();
    assert_eq!(
        fixture
            .consent(
                &cookie,
                &csrf,
                blocked["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let (status, failure) = poll_client(&fixture, secret).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(failure["error"], "account_banned");
}

#[tokio::test]
async fn banned_github_callback_terminates_client_poll_without_waiting_for_expiry() {
    let fixture = Fixture::new().await;
    let start = named_login(&fixture, "alice-home").await;
    let secret = start["device_code"].as_str().unwrap();
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "pending");
    let response = fixture
        .client
        .get(format!(
            "{}/setup?code={}",
            fixture.base,
            start["user_code"].as_str().unwrap()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let login = reqwest::Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
    let params: HashMap<_, _> = login.query_pairs().into_owned().collect();

    fixture
        .broker
        .portal
        .storage(|store| store.set_policy("123", Some(true), None))
        .await
        .unwrap();
    let response = fixture
        .client
        .get(format!(
            "{}/oauth/callback/github?state={}&code=fixture-code",
            fixture.base, params["state"]
        ))
        .header("cookie", cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let (status, failure) = poll_client(&fixture, secret).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(failure["error"], "account_banned");
    assert!(failure.get("access_token").is_none());
    assert!(
        fixture
            .broker
            .portal
            .storage(|store| store.handle_available("alice-home"))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn registration_claims_only_new_names_and_returns_no_login_token() {
    let fixture = Fixture::new().await;
    let start = |name| {
        fixture
            .client
            .post(format!("{}/api/client/registration/start", fixture.base))
            .json(&json!({"name": "fixture", "username": name}))
            .send()
    };
    let grant: Value = start("fixture-account")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let code = grant["user_code"].as_str().unwrap();
    let secret = grant["device_code"].as_str().unwrap();
    let (cookie, csrf) = fixture.session("123");
    assert_eq!(
        fixture
            .consent(&cookie, &csrf, code, "allow")
            .await
            .status(),
        303
    );
    let receipt: Value = fixture
        .client
        .post(format!("{}/api/client/login/poll", fixture.base))
        .json(&json!({"device_code":secret}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(receipt["status"], "registered");
    assert_eq!(receipt["identity"]["username"], "fixture-account");
    assert!(receipt.get("access_token").is_none());
    assert_eq!(start("fixture-account").await.unwrap().status(), 409);
    let another: Value = start("another-name")
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        fixture
            .consent(
                &cookie,
                &csrf,
                another["user_code"].as_str().unwrap(),
                "allow"
            )
            .await
            .status(),
        409
    );
}

#[tokio::test]
async fn public_account_shows_default_and_updates_own_announcement() {
    let fixture = Fixture::new().await;
    fixture
        .broker
        .portal
        .storage(|store| {
            store.claim_handle("123", "alice-notices")?;
            store.update_announcement("123", true, "System message".into())
        })
        .await
        .unwrap();
    let read = || {
        fixture
            .client
            .get(format!("{}/api/client/account/alice-notices", fixture.base))
            .send()
    };
    let default: Value = read()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        default["announcement"]
            .as_str()
            .unwrap()
            .starts_with("alice-notices · ")
    );
    assert!(
        default["announcement"]
            .as_str()
            .unwrap()
            .ends_with("Z · 你好")
    );
    assert_eq!(default["server_announcement"], "System message");
    assert!(default.get("subject").is_none());
    let (cookie, csrf) = fixture.session("123");
    let edit = |scope, message| {
        fixture
            .client
            .post(format!("{}/account/announcement", fixture.base))
            .header("cookie", &cookie)
            .header("origin", "https://ssmcp.test")
            .form(&[
                ("csrf", csrf.as_str()),
                ("scope", scope),
                ("announcement", message),
            ])
            .send()
    };
    assert_eq!(
        edit("server", "Unauthorized replacement")
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        edit("account", "Welcome {username}")
            .await
            .unwrap()
            .status(),
        303
    );
    let updated: Value = read()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(updated["announcement"], "Welcome alice-notices");
    assert_eq!(updated["server_announcement"], "System message");
}

#[tokio::test]
async fn device_oauth_binds_the_browser_account_and_issues_one_device_only_credential() {
    let checks = [("account_confirmed", "yes"), ("control_confirmed", "yes")];
    let fixture = Fixture::new().await;
    fixture
        .broker
        .portal
        .storage(|store| store.claim_handle("123", "alice-home"))
        .await
        .unwrap();
    let start = || {
        fixture
            .client
            .post(format!("{}/api/client/login/start", fixture.base))
            .json(&json!({"name":"controlled laptop","username":"alice-home","device_only":true}))
            .send()
    };
    let response: Value = start()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = response["device_code"].as_str().unwrap();
    let code = response["user_code"].as_str().unwrap();
    let (other, csrf) = fixture.session("456");
    assert!(
        !fixture
            .consent_with_checks(&other, &csrf, code, "allow", &checks)
            .await
            .status()
            .is_redirection()
    );
    let (_, denied) = poll_client(&fixture, secret).await;
    assert!(denied.get("device_token").is_none() && denied.get("access_token").is_none());
    let response: Value = start()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let secret = response["device_code"].as_str().unwrap();
    let code = response["user_code"].as_str().unwrap();
    let (cookie, csrf) = fixture.session("123");
    let page = fixture
        .client
        .get(format!("{}/setup?code={code}", fixture.base))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("授权这台设备") && page.contains("alice-home"));
    assert!(page.contains("Confirm the account") && page.contains("Confirm access"));
    for incomplete in [&checks[..0], &checks[..1], &checks[1..]] {
        assert_eq!(
            fixture
                .consent_with_checks(&cookie, &csrf, code, "allow", incomplete)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        fixture
            .consent_with_checks(&cookie, &csrf, code, "allow", &checks)
            .await
            .status(),
        303
    );
    let (status, allowed) = poll_client(&fixture, secret).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(allowed["identity"]["subject"], "123");
    assert!(allowed["device_token"].as_str().unwrap().starts_with("cd_"));
    assert!(allowed.get("access_token").is_none());
    assert_eq!(poll_client(&fixture, secret).await.1["status"], "expired");

    let response: Value = fixture
        .client
        .post(format!("{}/api/client/login/start", fixture.base))
        .json(&json!({"name":"first device","username":"bob-desktop","device_only":true}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response["username_available"], true);
    let (cookie, csrf) = fixture.session("456");
    assert_eq!(
        fixture
            .consent_with_checks(
                &cookie,
                &csrf,
                response["user_code"].as_str().unwrap(),
                "allow",
                &checks
            )
            .await
            .status(),
        StatusCode::SEE_OTHER
    );
    let (_, registered) = poll_client(&fixture, response["device_code"].as_str().unwrap()).await;
    assert_eq!(registered["identity"]["username"], "bob-desktop");
    assert_eq!(registered["identity"]["subject"], "456");
    assert!(registered.get("device_token").is_some());
}
