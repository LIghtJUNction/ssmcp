use super::*;
use crate::{identity::Identity, user_policy::FREE_MONTHLY_TRAFFIC_BYTES};
use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::{
    SignatureScheme,
    pki_types::{PrivateKeyDer, pem::PemObject},
};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    time::Instant,
};

struct App {
    broker: Arc<Broker>,
    base: String,
    client: reqwest::Client,
    task: tokio::task::JoinHandle<()>,
    path: PathBuf,
    alice: String,
    bob: String,
    csrf: String,
}
impl Drop for App {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.path);
    }
}
impl App {
    async fn new() -> Self {
        Self::with_environment("test").await
    }

    async fn with_environment(environment: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "ssmcp-commerce-http-{}",
            super::super::random().unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let private = path.join("merchant.pem");
        let public = path.join("webhook.pem");
        for (path, bytes) in [
            (
                &private,
                include_bytes!("../commerce/pancake/fixtures/test-rsa-private.pem").as_slice(),
            ),
            (
                &public,
                include_bytes!("../commerce/pancake/fixtures/test-rsa-public.pem").as_slice(),
            ),
        ] {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .unwrap();
            file.write_all(bytes).unwrap();
        }
        let state_dir = path.join("accounts");
        let mut broker = Broker::new("https://ssmcp.test".into());
        broker.portal = Portal::open(&state_dir).unwrap();
        broker
            .portal
            .storage(|store| {
                for (subject, login) in [("123", "alice"), ("456", "bob")] {
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
        let config = CommerceConfig {
            enabled: true,
            environment: environment.into(),
            monthly_sellable_gb: 10,
            merchant_id: "MER_test".into(),
            store_id: "STO_test".into(),
            product_id: "PROD_test".into(),
            checkout_origin: "https://pancake.waffo.ai".into(),
            private_key_file: Some(private),
            webhook_public_key_file: Some(public),
            ..CommerceConfig::default()
        };
        broker
            .portal
            .configure_commerce(config, &state_dir)
            .unwrap();
        let alice = super::super::random().unwrap();
        let bob = super::super::random().unwrap();
        let csrf = super::super::random().unwrap();
        {
            let mut browser = broker.portal.browser.lock().unwrap();
            for (token, subject) in [(&alice, "123"), (&bob, "456")] {
                browser.sessions.insert(
                    super::super::digest(token),
                    super::super::Session {
                        subject: Some(subject.into()),
                        csrf: csrf.clone(),
                        expires: Instant::now() + super::super::SESSION_TTL,
                    },
                );
            }
        }
        let broker = Arc::new(broker);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = super::super::routes().with_state(broker.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = crate::http_client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        Self {
            broker,
            base,
            client,
            task,
            path,
            alice,
            bob,
            csrf,
        }
    }
    fn cookie(&self, token: &str) -> String {
        format!("{}={token}", super::super::COOKIE)
    }
    async fn reserved(&self) -> Order {
        self.broker
            .portal
            .commerce
            .as_ref()
            .unwrap()
            .reserve("123".into(), format!("{:032x}", 1), 1)
            .await
            .unwrap()
    }
    fn notification(&self, order: &Order, amount: &str, subject: &str) -> (Vec<u8>, String) {
        let body=serde_json::to_vec(&serde_json::json!({
            "id":"EVT_local","eventId":"PAY_local","eventType":"order.completed","storeId":"STO_test","mode":self.broker.portal.commerce.as_ref().unwrap().config.environment.as_str(),
            "data":{"orderId":"ORD_local","orderStatus":"completed","orderMerchantExternalId":order.id,
            "merchantProvidedBuyerIdentity":format!("github:{subject}"),"currency":"CNY",
            "chargedAmount":amount,"listPrice":{"subtotal":"1.00","taxAmount":"0.00","total":"1.00","listTotal":"1.00","discountAmount":"0.00"},"paymentId":"PAY_local","paymentStatus":"succeeded"}
        })).unwrap();
        Self::sign_body(body)
    }
    fn sign_body(body: Vec<u8>) -> (Vec<u8>, String) {
        let timestamp = commerce::now() * 1000;
        let mut raw = format!("{timestamp}.").into_bytes();
        raw.extend_from_slice(&body);
        let key = PrivateKeyDer::from_pem_slice(include_bytes!(
            "../commerce/pancake/fixtures/test-rsa-private.pem"
        ))
        .unwrap();
        let key = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
        let signature = key
            .choose_scheme(&[SignatureScheme::RSA_PKCS1_SHA256])
            .unwrap()
            .sign(&raw)
            .unwrap();
        (
            body,
            format!("t={timestamp},v1={}", STANDARD.encode(signature)),
        )
    }
    async fn notify(&self, body: Vec<u8>, signature: String) -> StatusCode {
        self.client
            .post(format!("{}/payments/waffo/webhook", self.base))
            .header("x-waffo-signature", signature)
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
    }
}
#[tokio::test]
async fn purchase_requires_verified_session_origin_and_csrf() {
    let app = App::new().await;
    let url = format!("{}/account/traffic/orders", app.base);
    let form = [
        ("csrf", app.csrf.clone()),
        ("amount", "1.00".into()),
        ("request_nonce", format!("{:032x}", 1)),
        ("acknowledge_month", "yes".into()),
        ("period", app.broker.traffic_period()),
    ];
    let response = app
        .client
        .post(&url)
        .header("origin", "https://ssmcp.test")
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = app
        .client
        .post(&url)
        .header("cookie", app.cookie(&app.alice))
        .header("origin", "https://attacker.test")
        .form(&form)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let mut bad = form.clone();
    bad[0].1 = "invalid".into();
    let response = app
        .client
        .post(&url)
        .header("cookie", app.cookie(&app.alice))
        .header("origin", "https://ssmcp.test")
        .form(&bad)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    for amount in [
        "0.00",
        "-1.00",
        "1.001",
        "1.01",
        "1e0",
        "18446744073709551616.00",
    ] {
        let mut invalid = form.clone();
        invalid[1].1 = amount.into();
        let response = app
            .client
            .post(&url)
            .header("cookie", app.cookie(&app.alice))
            .header("origin", "https://ssmcp.test")
            .form(&invalid)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{amount}");
    }
    let mut mixed = form.to_vec();
    mixed.push(("quantity_gb", "1".into()));
    let response = app
        .client
        .post(&url)
        .header("cookie", app.cookie(&app.alice))
        .header("origin", "https://ssmcp.test")
        .form(&mixed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        app.broker
            .portal
            .commerce
            .as_ref()
            .unwrap()
            .storage(|s| s.list("123"))
            .await
            .unwrap()
            .is_empty()
    );
}
#[tokio::test]
async fn return_page_and_other_accounts_cannot_confirm_or_read_payment() {
    let app = App::new().await;
    let order = app.reserved().await;
    let url = format!(
        "{}/account/traffic/orders/{}?paid=true&status=success",
        app.base, order.id
    );
    let response = app
        .client
        .get(&url)
        .header("cookie", app.cookie(&app.bob))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = app
        .client
        .get(&url)
        .header("cookie", app.cookie(&app.alice))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(app.broker.traffic_quota("123").await.unwrap().paid_bytes, 0);
}
#[tokio::test]
async fn signed_wrong_amount_or_identity_never_grants_and_duplicate_success_grants_once() {
    let app = App::new().await;
    let order = app.reserved().await;
    for (amount, subject) in [("0.99", "123"), ("1.00", "456")] {
        let (body, signature) = app.notification(&order, amount, subject);
        assert_eq!(app.notify(body, signature).await, StatusCode::BAD_REQUEST);
    }
    assert_eq!(app.broker.traffic_quota("123").await.unwrap().paid_bytes, 0);
    let (body, signature) = app.notification(&order, "1.00", "123");
    assert_eq!(
        app.notify(body.clone(), signature.clone()).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(app.notify(body, signature).await, StatusCode::NO_CONTENT);
    let quota = app.broker.traffic_quota("123").await.unwrap();
    assert_eq!(quota.paid_bytes, commerce::GB);
    assert_eq!(
        quota.limit_bytes,
        Some(FREE_MONTHLY_TRAFFIC_BYTES + commerce::GB)
    );
    assert_eq!(app.broker.traffic_quota("456").await.unwrap().paid_bytes, 0);
    // A settled local fixture makes valid retries stop before any provider request.
    // Both the deployed amount form and previously rendered GB forms remain valid.
    for (field, value) in [("amount", "1.00"), ("amount", "1.0"), ("quantity_gb", "1")] {
        let response = app
            .client
            .post(format!("{}/account/traffic/orders", app.base))
            .header("cookie", app.cookie(&app.alice))
            .header("origin", "https://ssmcp.test")
            .form(&[
                ("csrf", app.csrf.as_str()),
                (field, value),
                ("request_nonce", order.request_nonce.as_str()),
                ("acknowledge_month", "yes"),
                ("period", order.period.as_str()),
            ])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT, "{field}={value}");
        assert!(response.text().await.unwrap().contains("订单状态已变化"));
    }
    let orders = app
        .broker
        .portal
        .commerce
        .as_ref()
        .unwrap()
        .storage(|store| store.list("123"))
        .await
        .unwrap();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].quantity_gb, 1);
    assert_eq!(
        app.broker.traffic_quota("123").await.unwrap().paid_bytes,
        commerce::GB
    );
}
#[tokio::test]
async fn recovery_finishes_durable_paid_intent_without_repeating_a_grant() {
    let app = App::new().await;
    let order = app.reserved().await;
    let (body, signature) = app.notification(&order, "1.00", "123");
    let commerce = app.broker.portal.commerce.as_ref().unwrap();
    assert!(matches!(
        commerce.accept_webhook(&body, &signature).await.unwrap(),
        Settlement::Grant(_)
    ));
    // Simulate a crash after quota journal append but before the order acknowledgement.
    assert!(
        app.broker
            .grant_paid_traffic("123", &order.period, &order.id, order.bytes)
            .await
            .unwrap()
    );
    app.broker.reconcile_commerce().await.unwrap();
    app.broker.reconcile_commerce().await.unwrap();
    assert_eq!(
        app.broker.traffic_quota("123").await.unwrap().paid_bytes,
        commerce::GB
    );
    assert_eq!(
        commerce
            .storage(move |s| s.owned("123", &order.id))
            .await
            .unwrap()
            .state,
        OrderState::Granted
    );
}

#[tokio::test]
async fn signed_added_tax_is_accepted_only_with_exact_due_and_charge() {
    let app = App::new().await;
    let order = app.reserved().await;
    let (body, _) = app.notification(&order, "1.10", "123");
    let mut event: serde_json::Value = serde_json::from_slice(&body).unwrap();
    event["data"]["listPrice"]["taxAmount"] = serde_json::json!("0.10");
    event["data"]["listPrice"]["total"] = serde_json::json!("1.10");
    event["data"]["listPrice"]["listTotal"] = serde_json::json!("1.10");
    let mut mismatch = event.clone();
    mismatch["data"]["chargedAmount"] = serde_json::json!("1.11");
    let (body, signature) = App::sign_body(serde_json::to_vec(&mismatch).unwrap());
    assert_eq!(app.notify(body, signature).await, StatusCode::BAD_REQUEST);
    assert_eq!(app.broker.traffic_quota("123").await.unwrap().paid_bytes, 0);
    let (body, signature) = App::sign_body(serde_json::to_vec(&event).unwrap());
    assert_eq!(app.notify(body, signature).await, StatusCode::NO_CONTENT);
    assert_eq!(
        app.broker.traffic_quota("123").await.unwrap().paid_bytes,
        commerce::GB
    );
}
#[tokio::test]
async fn checkout_page_never_caches_bearer_link_and_discloses_month_end() {
    let app = App::new().await;
    let order = app.reserved().await;
    let expires = commerce::expiry_label(order.month_end, order.utc_offset_minutes).unwrap();
    let response = app
        .client
        .get(format!("{}/account/traffic", app.base))
        .header("cookie", app.cookie(&app.alice))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = response.text().await.unwrap();
    assert!(body.contains(&expires));
    assert!(body.contains("上下行合计"));
    assert!(body.contains("税前金额（CNY）"));
    assert!(body.contains(
        "<input type=number name=amount value=1.00 min=1.00 max=9.00 step=1.00 required>"
    ));
    assert!(!body.contains("name=quantity_gb"));
    assert!(body.contains("<input type=checkbox name=acknowledge_month value=yes required>"));
    if let Some(directory) = std::env::var_os("SSMCP_COMMERCE_PREVIEW_DIR") {
        let directory = PathBuf::from(directory);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("traffic-purchase.html"), &body).unwrap();
        for (path, content) in crate::portal::assets::preview_files() {
            let path = directory.join(path.trim_start_matches('/'));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
    }
    // Exercise the exact response renderer and the same middleware applied to the merged routes.
    let response = super::super::security_headers(checkout_page(
        &order,
        "https://checkout.test/session#token=test-only",
    ))
    .await;
    assert_eq!(response.headers()["cache-control"], "no-store");
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = std::str::from_utf8(&body).unwrap();
    assert!(body.contains(&expires));
    assert!(body.contains("referrerpolicy=no-referrer"));
    assert!(body.contains("noreferrer noopener"));
    assert!(body.contains("协议封装与重试"));
    assert!(body.contains("含税应付金额"));
}

#[tokio::test]
async fn monthly_paid_tier_requires_verified_production_settlement() {
    for environment in ["test", "prod"] {
        let app = App::with_environment(environment).await;
        app.broker
            .portal
            .configure_monthly_allowances(crate::user_policy::MonthlyTrafficAllowances {
                free_user_bytes: 100,
                paid_user_bytes: 1_000,
            })
            .unwrap();
        let order = app.reserved().await;
        assert_eq!(
            app.broker.traffic_quota("123").await.unwrap().free_bytes,
            100
        );
        let (body, signature) = app.notification(&order, "0.99", "123");
        assert_eq!(app.notify(body, signature).await, StatusCode::BAD_REQUEST);
        assert!(
            !app.broker
                .portal
                .storage(|store| Ok(store.account("123")?.has_recharged))
                .await
                .unwrap()
        );
        let (body, signature) = app.notification(&order, "1.00", "123");
        assert_eq!(app.notify(body, signature).await, StatusCode::NO_CONTENT);
        app.broker.reconcile_commerce().await.unwrap();
        let quota = app.broker.traffic_quota("123").await.unwrap();
        let paid = environment == "prod";
        assert_eq!(
            app.broker
                .portal
                .storage(|store| Ok(store.account("123")?.has_recharged))
                .await
                .unwrap(),
            paid
        );
        assert_eq!(quota.free_bytes, if paid { 1_000 } else { 100 });
        assert_eq!(quota.paid_bytes, commerce::GB);
        assert_eq!(quota.limit_bytes, Some(quota.free_bytes + commerce::GB));
        assert_eq!(
            app.broker.traffic_quota("456").await.unwrap().free_bytes,
            100
        );
    }
}
