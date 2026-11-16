use super::store::Account;
use super::*;
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("ssmcp-portal-{}", random().unwrap()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn identity(id: &str, login: &str) -> Identity {
    Identity {
        provider: "github".into(),
        subject: id.into(),
        login: login.into(),
    }
}
fn key() -> String {
    let mut bytes = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
    bytes.extend_from_slice(&[1; 32]);
    format!("ssh-ed25519 {} device", STANDARD.encode(bytes))
}
#[test]
fn key_parser_rejects_options_private_keys_wrong_algorithms_lengths_and_controls() {
    let expected = parse_key(&key(), "laptop").unwrap();
    for pasted in [format!("{}\n", key()), format!(" \t{}\r\n", key())] {
        let parsed = parse_key(&pasted, "laptop").unwrap();
        assert_eq!(parsed.public_key, expected.public_key);
        assert_eq!(parsed.fingerprint, expected.fingerprint);
    }
    for raw in [
        "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
        format!("command=\"id\" {}", key()),
        key().replace("ssh-ed25519", "ssh-rsa"),
        format!("{}\nssh-ed25519 bad", key()),
        key().replace(' ', "\t"),
        format!("{}\0", key()),
        "\r\n \t".into(),
        "ssh-ed25519 AAAA".into(),
        "x".repeat(1025),
    ] {
        assert!(parse_key(&raw, "").is_err(), "accepted {raw}");
    }
    assert!(parse_key(&key(), "bad\nlabel").is_err());
    let mut bytes = STANDARD
        .decode(key().split_whitespace().nth(1).unwrap())
        .unwrap();
    bytes.push(1);
    assert!(parse_key(&format!("ssh-ed25519 {}", STANDARD.encode(bytes)), "").is_err());
}
#[test]
fn session_capacity_recycles_only_idle_anonymous_visitors() {
    let mut browser = BrowserState::default();
    let now = Instant::now();
    for index in 0..MAX_SESSIONS {
        browser.sessions.insert(
            digest(&index.to_string()),
            Session {
                subject: (index == 0).then(|| "123".to_owned()),
                csrf: "fixture".to_owned(),
                expires: now + Duration::from_secs(60 + index as u64),
            },
        );
    }
    browser.pending.insert(
        digest("pending"),
        Pending {
            browser: digest("1"),
            verifier: "fixture".to_owned(),
            expires: now + LOGIN_TTL,
        },
    );
    browser.make_room_for_session().unwrap();
    assert_eq!(browser.sessions.len(), MAX_SESSIONS - 1);
    assert!(browser.sessions.contains_key(&digest("0")));
    assert!(browser.sessions.contains_key(&digest("1")));
    assert!(browser.pending.contains_key(&digest("pending")));
    assert!(!browser.sessions.contains_key(&digest("2")));
    assert!(browser.sessions.contains_key(&digest("3")));

    browser.sessions.insert(
        digest("replacement"),
        Session {
            subject: Some("456".to_owned()),
            csrf: "fixture".to_owned(),
            expires: now + SESSION_TTL,
        },
    );
    for (key, session) in &mut browser.sessions {
        if key != &digest("1") {
            session.subject = Some("123".to_owned());
        }
    }
    assert_eq!(browser.make_room_for_session(), Err(Problem::Busy));
    assert_eq!(browser.sessions.len(), MAX_SESSIONS);
    assert!(browser.sessions.contains_key(&digest("1")));
}

#[test]
fn durable_accounts_survive_rename_and_keys_are_owner_scoped() {
    let tmp = Temp::new();
    let mut s = Store::open(&tmp.0).unwrap();
    let first = s.login(&identity("123", "alice")).unwrap();
    s.login(&identity("456", "bob")).unwrap();
    let k = parse_key(&key(), "laptop").unwrap();
    s.add("123", k.clone()).unwrap();
    assert_eq!(s.remove("456", &k.fingerprint), Err(Problem::NotFound));
    assert_eq!(s.add("123", k.clone()), Err(Problem::Duplicate));
    let renamed = s.login(&identity("123", "renamed")).unwrap();
    assert_eq!(first.username, renamed.username);
    assert_eq!(renamed.username, "gh-123");
    assert_eq!(renamed.keys.len(), 1);
    assert!(
        Store::open(&tmp.0).is_err(),
        "must lock out concurrent writers"
    );
    drop(s);
    let mut s = Store::open(&tmp.0).unwrap();
    assert_eq!(s.account("123").unwrap().github_login, "renamed");
    s.remove("123", &k.fingerprint).unwrap();
    assert!(s.account("123").unwrap().keys.is_empty());
    for id in ["../123", "01", "0", "-1", "18446744073709551616"] {
        assert!(s.login(&identity(id, "alice")).is_err());
    }
}
#[test]
fn storage_rejects_insecure_permissions_and_symlinks() {
    let tmp = Temp::new();
    fs::set_permissions(&tmp.0, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Store::open(&tmp.0).is_err());
    fs::set_permissions(&tmp.0, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", tmp.0.join("accounts.json")).unwrap();
    assert!(Store::open(&tmp.0).is_err());
}
#[test]
fn csrf_requires_cookie_origin_and_token_and_strict_fields() {
    let p = Portal::default();
    let cookie = random().unwrap();
    let csrf = random().unwrap();
    p.browser.lock().unwrap().sessions.insert(
        digest(&cookie),
        Session {
            subject: Some("1".into()),
            csrf: csrf.clone(),
            expires: Instant::now() + SESSION_TTL,
        },
    );
    let mut h = HeaderMap::new();
    h.insert(
        header::COOKIE,
        format!("{COOKIE}={cookie}").parse().unwrap(),
    );
    let f = HashMap::from([("csrf".into(), csrf)]);
    assert_eq!(p.verify(&h, &f, "https://ssmcp.test"), Err(Problem::Csrf));
    h.insert(header::ORIGIN, "null".parse().unwrap());
    assert_eq!(p.verify(&h, &f, "https://ssmcp.test"), Err(Problem::Csrf));
    h.insert(header::ORIGIN, "https://evil.test".parse().unwrap());
    assert_eq!(p.verify(&h, &f, "https://ssmcp.test"), Err(Problem::Csrf));
    h.insert(header::ORIGIN, "https://ssmcp.test".parse().unwrap());
    assert_eq!(
        p.verify(&h, &f, "https://ssmcp.test").unwrap().1.as_deref(),
        Some("1")
    );
    assert_eq!(
        p.verify(
            &h,
            &HashMap::from([("csrf".into(), "wrong".into())]),
            "https://ssmcp.test"
        ),
        Err(Problem::Csrf)
    );
    assert_eq!(parameters("csrf=a&csrf=b"), Err(Problem::Invalid));
    h.append(
        header::COOKIE,
        format!("{COOKIE}={cookie}").parse().unwrap(),
    );
    assert!(p.session(&h).is_err());
}
#[tokio::test]
async fn login_rotates_session_and_logout_during_exchange_cannot_restore_it() {
    let tmp = Temp::new();
    let p = Portal::open(&tmp.0).unwrap();
    let old = digest("old");
    p.browser.lock().unwrap().sessions.insert(
        old,
        Session {
            subject: None,
            csrf: random().unwrap(),
            expires: Instant::now() + LOGIN_TTL,
        },
    );
    let r = finish_identity(&p, old, identity("123", "alice"))
        .await
        .unwrap();
    let cookie = r.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("Secure; HttpOnly; SameSite=Lax"));
    assert!(!p.browser.lock().unwrap().sessions.contains_key(&old));
    assert!(
        finish_identity(&p, old, identity("123", "alice"))
            .await
            .is_err()
    );
}
#[test]
fn view_escapes_untrusted_text_and_includes_complete_tutorial() {
    let a = Account {
        subject: "1".into(),
        username: "gh-1".into(),
        github_login: "<script>".into(),
        keys: vec![],
    };
    let html = view::home(Some(&a), Some("csrf"), true);
    assert!(!html.contains("<script>"));
    assert!(html.contains("&lt;script&gt;"));
    for step in [
        "https://chatgpt.com/plugins",
        "Add",
        "Name",
        "Connection",
        "https://ssmcp.lmm.best/mcp",
        "Create",
        "gh-1",
    ] {
        assert!(html.contains(step), "missing {step}");
    }
}

#[tokio::test]
async fn http_login_callback_replay_and_cross_account_mutations() {
    use crate::identity::{GitHubConfig, IdentityProviders};
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    use serde_json::json;
    use tokio::net::TcpListener;
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_url = format!("http://{}", upstream.local_addr().unwrap());
    let mock = Router::new()
        .route(
            "/login/oauth/access_token",
            post(
                |axum::Form(f): axum::Form<HashMap<String, String>>| async move {
                    assert_eq!(
                        f["redirect_uri"],
                        "https://ssmcp.test/oauth/callback/github"
                    );
                    assert_eq!(f["code_verifier"].len(), 43);
                    Json(json!({"access_token":"test-upstream-token", "token_type":"bearer"}))
                },
            ),
        )
        .route(
            "/user",
            get(|| async { Json(json!({"id":123, "login":"alice", "type":"User"})) }),
        );
    let mock_task = tokio::spawn(async move { axum::serve(upstream, mock).await.unwrap() });
    let tmp = Temp::new();
    let mut broker = Broker::new("https://ssmcp.test".into());
    broker.identities = IdentityProviders::with_github_test_endpoints(
        "https://ssmcp.test",
        GitHubConfig {
            client_id: "existing-app".into(),
            client_secret: "fixture-secret".into(),
        },
        &upstream_url,
    )
    .unwrap();
    broker.portal = Portal::open(&tmp.0).unwrap();
    broker
        .portal
        .storage(|s| {
            s.login(&identity("456", "bob"))?;
            s.add("456", parse_key(&key(), "bob key")?)
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
    let home = client.get(&base).send().await.unwrap();
    assert_eq!(home.headers()["cache-control"], "no-store");
    assert_eq!(home.headers()["referrer-policy"], "same-origin");
    let cookie = home.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let html = home.text().await.unwrap();
    let csrf = html
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let start = client
        .post(format!("{base}/account/login"))
        .header("cookie", &cookie)
        .header("origin", "https://ssmcp.test")
        .form(&[("csrf", csrf)])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 303);
    let url = reqwest::Url::parse(start.headers()["location"].to_str().unwrap()).unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(params["client_id"], "existing-app");
    let callback_url = format!(
        "{base}/oauth/callback/github?state={}&code=fixture-code",
        params["state"]
    );
    let missing_cookie = client.get(&callback_url).send().await.unwrap();
    assert_eq!(missing_cookie.status(), 401);
    let policy = missing_cookie.headers()["content-security-policy"]
        .to_str()
        .unwrap();
    assert!(policy.contains("style-src 'self'"));
    assert!(policy.contains("default-src 'none'"));
    assert!(policy.contains("frame-ancestors 'none'"));
    assert!(!policy.contains("'unsafe-inline'"));
    assert_eq!(missing_cookie.headers()["cache-control"], "no-store");
    assert_eq!(missing_cookie.headers()["referrer-policy"], "same-origin");
    assert!(
        missing_cookie
            .text()
            .await
            .unwrap()
            .contains("href=\"/portal.css\"")
    );
    let callback = client
        .get(&callback_url)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(callback.status(), 303);
    let new_cookie = callback.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_ne!(new_cookie, cookie);
    assert_eq!(
        client
            .get(&callback_url)
            .header("cookie", &new_cookie)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let html = client
        .get(&base)
        .header("cookie", &new_cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("alice") && html.contains("gh-123"));
    assert!(!html.contains("bob key"));
    let csrf = html
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    let fingerprint = parse_key(&key(), "").unwrap().fingerprint;
    let denied = client
        .post(format!("{base}/account/keys/delete"))
        .header("cookie", &new_cookie)
        .header("origin", "https://ssmcp.test")
        .form(&[("csrf", csrf), ("fingerprint", &fingerprint)])
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
    let injection = client
        .post(format!("{base}/account/keys"))
        .header("cookie", &new_cookie)
        .header("origin", "https://ssmcp.test")
        .form(&[
            ("csrf", csrf),
            ("label", "mine"),
            ("public_key", &key()),
            ("owner", "456"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(injection.status(), 400);
    let added = client
        .post(format!("{base}/account/keys"))
        .header("cookie", &new_cookie)
        .header("origin", "https://ssmcp.test")
        .form(&[("csrf", csrf), ("label", "mine"), ("public_key", &key())])
        .send()
        .await
        .unwrap();
    assert_eq!(added.status(), 303);
    assert_eq!(
        broker
            .portal
            .storage(|s| Ok(s.account("456")?.keys.len()))
            .await
            .unwrap(),
        1
    );
    server.abort();
    mock_task.abort();
}
