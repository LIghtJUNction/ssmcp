use super::*;

#[test]
fn history_cursor_rejects_identity_scope_and_duplicate_fields() {
    assert_eq!(before_parameter(None), Ok(None));
    assert_eq!(before_parameter(Some("before=42")), Ok(Some(42)));
    for query in [
        "before=0",
        "before=-1",
        "before=x",
        "before=2&before=1",
        "subject=123",
        "scope=all",
        "before=2&admin=true",
    ] {
        assert_eq!(
            before_parameter(Some(query)),
            Err(Problem::Invalid),
            "{query}"
        );
    }
}

#[test]
fn rate_input_has_explicit_units_bounds_and_round_trips_saved_bytes() {
    assert_eq!(parse_rate_limit(""), Ok(None));
    assert_eq!(parse_rate_limit("  "), Ok(None));
    assert_eq!(parse_rate_limit("8"), Ok(Some(8 * 1_048_576)));
    assert_eq!(parse_rate_limit(".5"), Ok(Some(524_288)));
    assert_eq!(parse_rate_limit("0.1"), Ok(Some(104_858)));
    assert_eq!(parse_rate_limit("1024"), Ok(Some(1_073_741_824)));
    for rate in [
        1,
        103,
        104_858,
        1_048_575,
        1_048_576,
        8_388_609,
        1_073_741_824,
    ] {
        let displayed = dashboard_view::mib_per_second(rate);
        assert_eq!(parse_rate_limit(&displayed), Ok(Some(rate)), "{displayed}");
    }
    for invalid in [
        "0",
        "-1",
        "+1",
        "NaN",
        "inf",
        "1e3",
        "1024.000001",
        "1025",
        "1,5",
        ".",
        "8.",
        "1..2",
        "0.000000000000000000001",
        "1 2",
        "99999999999999999999999999999999999999999999999999999999999999999999",
    ] {
        assert_eq!(
            parse_rate_limit(invalid),
            Err(Problem::ManagementInvalid),
            "{invalid}"
        );
    }
}

#[test]
fn policy_forms_patch_one_field_and_reject_self_ban_or_identity_injection() {
    let rate = parameters("csrf=x&subject=123&action=rate&rate_mib=0.5").unwrap();
    assert_eq!(
        policy_change(&rate, "999").unwrap(),
        UserPolicyChange {
            subject: "123".into(),
            banned: None,
            rate_limit_bps: Some(Some(524_288)),
        }
    );
    let default = parameters("csrf=x&subject=123&action=rate&rate_mib=").unwrap();
    assert_eq!(
        policy_change(&default, "999").unwrap().rate_limit_bps,
        Some(None)
    );
    let ban = parameters("csrf=x&subject=123&action=ban").unwrap();
    assert_eq!(policy_change(&ban, "999").unwrap().banned, Some(true));
    assert_eq!(policy_change(&ban, "999").unwrap().rate_limit_bps, None);
    assert_eq!(policy_change(&ban, "123"), Err(Problem::Forbidden));
    let unban = parameters("csrf=x&subject=123&action=unban").unwrap();
    assert_eq!(policy_change(&unban, "999").unwrap().banned, Some(false));
    for forged in [
        "csrf=x&subject=123&action=rate&rate_mib=4&banned=false",
        "csrf=x&subject=123&action=ban&rate_mib=4",
        "csrf=x&subject=123&action=ban&admin=true",
        "csrf=x&subject=123&action=ban&actor=999",
        "csrf=x&subject=01&action=ban",
        "csrf=x&subject=0&action=ban",
        "csrf=x&subject=123&action=delete",
    ] {
        assert!(
            policy_change(&parameters(forged).unwrap(), "999").is_err(),
            "{forged}"
        );
    }
}

struct Temp(std::path::PathBuf);
impl Temp {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "ssmcp-user-policy-{}",
            crate::portal::random().unwrap()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn browser_session(portal: &Portal, subject: &str) -> (String, String) {
    let cookie = crate::portal::random().unwrap();
    let csrf = crate::portal::random().unwrap();
    portal.browser.lock().unwrap().sessions.insert(
        crate::portal::digest(&cookie),
        crate::portal::Session {
            subject: Some(subject.into()),
            csrf: csrf.clone(),
            expires: std::time::Instant::now() + crate::portal::SESSION_TTL,
        },
    );
    (cookie, csrf)
}

async fn submit(
    broker: Arc<Broker>,
    cookie: &str,
    origin: &str,
    body: String,
) -> Result<Response, Problem> {
    let request = Request::builder()
        .method("POST")
        .uri("/account/admin/users")
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .header(axum::http::header::ORIGIN, origin)
        .header(
            axum::http::header::COOKIE,
            format!("{}={cookie}", crate::portal::COOKIE),
        )
        .body(axum::body::Body::from(body))
        .unwrap();
    user_policy(State(broker), request).await
}

#[tokio::test]
async fn user_policy_requires_admin_and_csrf_and_keeps_ban_when_saving_rate() {
    use crate::{
        history::{AuditConfig, AuditLog},
        management::{ManagementPolicy, ManagementServer},
    };
    let temp = Temp::new();
    let socket = temp.0.join("management.sock");
    let server = ManagementServer::bind(&socket).await.unwrap();
    let mut broker = Broker::new("https://ssmcp.test".into());
    broker.portal = Portal::open(&temp.0.join("accounts")).unwrap();
    broker.portal.configure_management(socket);
    broker.configure_management(
        ManagementPolicy::new(vec!["999".into()], HashMap::new()).unwrap(),
        Arc::new(AuditLog::open(&temp.0.join("audit"), AuditConfig::default()).unwrap()),
    );
    broker
        .portal
        .storage(|store| {
            for (subject, login) in [("999", "owner"), ("123", "alice"), ("456", "bob")] {
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
    let (admin_cookie, admin_csrf) = browser_session(&broker.portal, "999");
    let (user_cookie, user_csrf) = browser_session(&broker.portal, "123");
    let broker = Arc::new(broker);
    let task = tokio::spawn(server.run(broker.clone()));

    assert!(matches!(
        submit(
            broker.clone(),
            &user_cookie,
            "https://ssmcp.test",
            format!("csrf={user_csrf}&subject=456&action=ban")
        )
        .await,
        Err(Problem::Forbidden)
    ));
    assert!(matches!(
        submit(
            broker.clone(),
            &admin_cookie,
            "https://other.test",
            format!("csrf={admin_csrf}&subject=456&action=ban")
        )
        .await,
        Err(Problem::Csrf)
    ));
    assert!(matches!(
        submit(
            broker.clone(),
            &admin_cookie,
            "https://ssmcp.test",
            "csrf=wrong&subject=456&action=ban".into()
        )
        .await,
        Err(Problem::Csrf)
    ));
    assert!(
        !broker
            .portal
            .storage(|s| s.account("456"))
            .await
            .unwrap()
            .banned
    );
    let response = submit(
        broker.clone(),
        &admin_cookie,
        "https://ssmcp.test",
        format!("csrf={admin_csrf}&subject=456&action=ban"),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers()[axum::http::header::LOCATION],
        "/account/admin"
    );

    // A rate form opened before the ban must preserve the new banned state.
    submit(
        broker.clone(),
        &admin_cookie,
        "https://ssmcp.test",
        format!("csrf={admin_csrf}&subject=456&action=rate&rate_mib=0.5"),
    )
    .await
    .unwrap();
    let users = broker.portal.storage(|s| s.summaries()).await.unwrap();
    let bob = users.iter().find(|a| a.subject == "456").unwrap();
    assert!(bob.banned);
    assert_eq!(bob.rate_limit_bps, Some(524_288));
    submit(
        broker.clone(),
        &admin_cookie,
        "https://ssmcp.test",
        format!("csrf={admin_csrf}&subject=456&action=unban"),
    )
    .await
    .unwrap();
    let period = broker.traffic_period();
    let form = |csrf: &str, subject: &str, period: &str, bytes: u64| {
        format!(
            "csrf={csrf}&subject={subject}&action=traffic&period={period}&remaining_bytes={bytes}"
        )
    };
    for (cookie, fields, expected) in [
        (
            &user_cookie,
            form(&user_csrf, "456", &period, 0),
            Problem::Forbidden,
        ),
        (
            &admin_cookie,
            form("wrong", "456", &period, 0),
            Problem::Csrf,
        ),
        (
            &admin_cookie,
            form(&admin_csrf, "999", &period, 0),
            Problem::Forbidden,
        ),
        (
            &admin_cookie,
            form(&admin_csrf, "456", "2000-01", 0),
            Problem::ManagementInvalid,
        ),
    ] {
        assert!(
            matches!(submit(broker.clone(), cookie, "https://ssmcp.test", fields).await, Err(error) if error == expected)
        );
    }
    let response = submit(
        broker.clone(),
        &admin_cookie,
        "https://ssmcp.test",
        form(&admin_csrf, "456", &period, 250_000_000),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::SEE_OTHER);
    let quota = broker.traffic_quota("456").await.unwrap();
    assert_eq!(quota.remaining_bytes, Some(250_000_000));
    assert_eq!(quota.paid_bytes, 0);
    let balances = management(&broker.portal)
        .unwrap()
        .traffic_balances(&Identity {
            provider: "github".into(),
            subject: "999".into(),
            login: "owner".into(),
        })
        .await
        .unwrap();
    assert_eq!(balances.period, period);
    assert_eq!(balances.remaining.get("456"), Some(&250_000_000));
    assert!(!balances.remaining.contains_key("999"));

    let bob = broker.portal.storage(|s| s.account("456")).await.unwrap();
    assert!(!bob.banned);
    assert_eq!(bob.rate_limit_bps, Some(524_288));
    assert!(matches!(
        submit(
            broker.clone(),
            &admin_cookie,
            "https://ssmcp.test",
            format!("csrf={admin_csrf}&subject=999&action=ban")
        )
        .await,
        Err(Problem::Forbidden)
    ));
    submit(
        broker.clone(),
        &admin_cookie,
        "https://ssmcp.test",
        format!("csrf={admin_csrf}&subject=456&action=rate&rate_mib="),
    )
    .await
    .unwrap();
    assert_eq!(
        broker
            .portal
            .storage(|s| s.account("456"))
            .await
            .unwrap()
            .rate_limit_bps,
        None
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn account_sessions_matches_live_mcp_grants_without_granting_management() {
    use crate::{
        history::{AuditConfig, AuditLog},
        management::{ManagementPolicy, ManagementServer},
    };
    let temp = Temp::new();
    let socket = temp.0.join("management.sock");
    let server = ManagementServer::bind(&socket).await.unwrap();
    let mut broker = Broker::new("https://ssmcp.test".into());
    broker.configure_management(
        ManagementPolicy::default(),
        Arc::new(AuditLog::open(&temp.0.join("audit"), AuditConfig::default()).unwrap()),
    );
    broker.portal = Portal::open(&temp.0.join("accounts")).unwrap();
    broker.portal.configure_management(socket);
    let alice = broker
        .portal
        .storage(|store| {
            let identity = |subject: &str, login: &str| Identity {
                provider: "github".into(),
                subject: subject.into(),
                login: login.into(),
            };
            let alice = identity("123", "alice");
            store.login(&alice)?;
            store.login(&identity("456", "owner"))?;
            store.login(&identity("789", "outsider"))?;
            Ok(alice)
        })
        .await
        .unwrap();
    let browser = |id| {
        let (raw, csrf) = browser_session(&broker.portal, id);
        (format!("{}={raw}", crate::portal::COOKIE), csrf)
    };
    let (guest_cookie, csrf) = browser("123");
    let (owner_cookie, _) = browser("456");
    let (outsider_cookie, _) = browser("789");
    let shared = broker.test_register_identity("gh-456", alice.clone()).await;
    let private = broker.test_register("gh-456", "fixture").await;
    let token = broker.oauth.test_identity_tokens("mcp", "123");
    let broker = Arc::new(broker);
    let management = tokio::spawn(server.run(broker.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let routes = crate::http::router(broker.clone());
    let http = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    let client = crate::http_client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let listed: serde_json::Value = client.post(format!("{base}/mcp"))
        .bearer_auth(token["access_token"].as_str().unwrap())
        .header("accept", "application/json")
        .header("mcp-protocol-version", "2025-03-26")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":{}}}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let clients = listed["result"]["structuredContent"]["clients"]
        .as_array()
        .unwrap();
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["id"], shared.client_id());

    let page = async |cookie: &str| {
        client
            .get(format!("{base}/account/sessions"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap()
    };
    let guest = page(&guest_cookie).await;
    assert!(
        guest.contains(shared.client_id()),
        "MCP lists the granted client, but the account page omitted it"
    );
    assert!(guest.contains("<dt>在线会话</dt><dd>1</dd>") && guest.contains("获准使用的会话"));
    assert!(!guest.contains(private.client_id()) && !guest.contains("disconnect-control"));
    assert!(!guest.contains("session-access") && !guest.contains("当前没有在线会话"));
    let owner = page(&owner_cookie).await;
    assert!(owner.contains(shared.client_id()) && owner.contains(private.client_id()));
    assert!(owner.contains("我拥有的会话") && owner.contains("disconnect-control"));
    let outsider = page(&outsider_cookie).await;
    assert!(outsider.contains("<dt>在线会话</dt><dd>0</dd>"));
    assert!(!outsider.contains(shared.client_id()) && !outsider.contains(private.client_id()));
    let denied = client
        .post(format!("{base}/account/sessions/disconnect"))
        .header("cookie", &guest_cookie)
        .header("origin", "https://ssmcp.test")
        .form(&[
            ("csrf", csrf.as_str()),
            ("session_id", shared.client_id()),
            ("return_to", "/account/sessions"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 404);
    assert!(shared.is_alive());

    let owned = broker.test_register_identity("gh-123", alice).await;
    let refreshed = page(&guest_cookie).await;
    assert!(refreshed.contains("<dt>在线会话</dt><dd>2</dd>"));
    assert!(refreshed.contains("<dt>报告的主机</dt><dd>1</dd>"));
    assert_eq!(
        refreshed
            .matches(&format!(r#"id="session-{}""#, owned.client_id()))
            .count(),
        1
    );
    http.abort();
    management.abort();
}
