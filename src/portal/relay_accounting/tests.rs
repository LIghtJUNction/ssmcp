use super::*;
use crate::broker::RelayAccess;
use std::{path::PathBuf, sync::Arc};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "ssmcp-external-billing-{}",
            super::super::random().unwrap()
        )))
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn digest(raw: &str) -> String {
    crate::hex::encode(Sha256::digest(raw.as_bytes()))
}

fn input(dir: &Directory) -> Value {
    json!({
        "device_ingress_sha256": digest(&"d".repeat(64)),
        "principals": { "alice": digest(&"a".repeat(64)), "betty": digest(&"b".repeat(64)) },
        "accounting": {
            "issuer": "https://example.invalid", "state_dir": dir.0,
            "users": {"alice":"123","betty":"456"},
            "service_token_sha256": digest(&"e".repeat(64)),
            "utc_offset_minutes":480, "free_monthly_bytes":12, "paid_monthly_bytes":24
        }
    })
}

fn broker(value: Value) -> crate::Result<Arc<Broker>> {
    let access: RelayAccess = serde_json::from_value(value)?;
    Broker::headless("https://relay.invalid".into(), access).map(Arc::new)
}

#[test]
fn rejects_ambiguous_identity_or_credential_and_refuses_rebinding() {
    let dir = Directory::new();
    let base = input(&dir);
    for (key, bad) in [
        (
            "service_token_sha256",
            base["device_ingress_sha256"].clone(),
        ),
        ("service_token_sha256", base["principals"]["alice"].clone()),
        ("users", json!({"alice":"123","betty":"123"})),
        ("users", json!({"alice":"123"})),
        ("users", json!({"alice":"0123","betty":"456"})),
    ] {
        let mut spec = base.clone();
        spec["accounting"][key] = bad;
        assert!(broker(spec).is_err());
    }
    assert!(!dir.0.exists());
    drop(broker(base.clone()).unwrap());
    let mut changed = base.clone();
    changed["accounting"]["users"]["alice"] = json!("789");
    assert!(broker(changed).is_err());
    let mut changed = base.clone();
    changed["accounting"]["issuer"] = json!("https://another.invalid");
    assert!(broker(changed).is_err());
    let mut changed = base.clone();
    changed["accounting"]["utc_offset_minutes"] = json!(0);
    assert!(broker(changed).is_err());
    drop(broker(base).unwrap());
    assert!(crate::portal::Portal::open(&dir.0).is_err());
    // A lost journal in an existing store must not manufacture a fresh zero.
    std::fs::remove_file(dir.0.join("traffic.jsonl")).unwrap();
    assert!(broker(input(&dir)).is_err());
    assert!(!dir.0.join("traffic.jsonl").exists());
}

async fn post(url: &str, token: &str, body: &Value) -> (StatusCode, Value) {
    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

#[tokio::test]
async fn default_external_allowance_is_one_decimal_gb_before_and_after_purchase() {
    let dir = Directory::new();
    let mut config = input(&dir);
    let settings = config["accounting"].as_object_mut().unwrap();
    settings.remove("free_monthly_bytes");
    settings.remove("paid_monthly_bytes");
    let relay = broker(config).unwrap();
    let accounting = relay.relay_accounting.as_ref().unwrap();
    let snapshot = |id| accounting.storage(move |ledger| ledger.snapshot(id));
    let initial = snapshot("123").await.unwrap();
    let allowance = 1_000_000_000;
    assert_eq!(initial.remaining_bytes, Some(allowance));
    assert_eq!(initial.free_bytes, allowance);
    assert_eq!(initial.paid_bytes, 0);

    // Reserve a byte count directly without allocating or transferring a full month of traffic.
    accounting
        .reserve("alice", true, allowance as usize)
        .await
        .unwrap();
    let exhausted = snapshot("123").await.unwrap();
    assert_eq!(exhausted.remaining_bytes, Some(0));
    assert_eq!(
        accounting.reserve("alice", true, 1).await,
        Err(TrafficError::Exhausted)
    );
    assert_eq!(snapshot("123").await.unwrap(), exhausted);

    let period = initial.period;
    let grant = || {
        accounting.storage({
            let period = period.clone();
            move |ledger| ledger.grant("123", &period, "lmm:boundary", 7, true)
        })
    };
    assert!(grant().await.unwrap());
    assert!(!grant().await.unwrap());
    let paid = snapshot("123").await.unwrap();
    assert_eq!(paid.free_bytes, allowance);
    assert_eq!(paid.paid_bytes, 7);
    assert_eq!(paid.remaining_bytes, Some(7));
    accounting.reserve("alice", true, 7).await.unwrap();
    assert_eq!(snapshot("123").await.unwrap().remaining_bytes, Some(0));
    assert_eq!(
        accounting.reserve("alice", true, 1).await,
        Err(TrafficError::Exhausted)
    );
    assert_eq!(
        snapshot("456").await.unwrap().remaining_bytes,
        Some(allowance)
    );
}

#[tokio::test]
async fn scoped_monthly_meter_and_paid_order_replay_are_durable() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = Directory::new();
    let configured = input(&dir);
    let relay = broker(configured.clone()).unwrap();
    let app = crate::http::relay_router(relay.clone());
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let origin = format!("http://{}", tcp.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(tcp, app).await.unwrap() });
    let quota = format!("{origin}/internal/v1/ssmcp/quota");
    let grant = format!("{origin}/internal/v1/ssmcp/entitlements");
    let service = "e".repeat(64);
    let principal = json!({"user_id":"123"});
    assert_eq!(
        post(&quota, &"a".repeat(64), &principal).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(&quota, &service, &json!({"user_id":"alice"})).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        post(&quota, &service, &json!({"user_id":"789"})).await.0,
        StatusCode::NOT_FOUND
    );
    let first = post(&quota, &service, &principal).await.1;
    assert_eq!(first["username"], "alice");
    assert_eq!(first["used_bytes"], "0");
    assert_eq!(first["free_bytes"], "12");
    relay.charge_relay("alice", true, 10).await.unwrap();
    relay.charge_relay("alice", false, 6).await.unwrap();
    assert!(relay.charge_relay("alice", true, 3).await.is_err());
    let used = post(&quota, &service, &principal).await.1;
    assert_eq!(used["uploaded_bytes"], "10");
    assert_eq!(used["downloaded_bytes"], "6");
    assert_eq!(used["used_bytes"], "10");
    assert_eq!(used["remaining_bytes"], "2");
    let other = post(&quota, &service, &json!({"user_id":"456"})).await.1;
    assert_eq!(other["used_bytes"], "0");

    let order =
        json!({"user_id":"123","order_id":"demo-order-1","period":first["period"],"bytes":"30"});
    assert_eq!(
        post(&grant, &"d".repeat(64), &order).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (a, b) = tokio::join!(
        post(&grant, &service, &order),
        post(&grant, &service, &order)
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    let mut statuses = [
        a.1["status"].as_str().unwrap(),
        b.1["status"].as_str().unwrap(),
    ];
    statuses.sort();
    assert_eq!(statuses, ["granted", "replayed"]);
    for (key, value) in [("bytes", json!("31")), ("user_id", json!("456"))] {
        let mut conflict = order.clone();
        conflict[key] = value;
        assert_eq!(
            post(&grant, &service, &conflict).await.1["error"],
            "order_conflict"
        );
    }
    let expired = json!({"user_id":"123","order_id":"late-order","period":"1999-01","bytes":"30"});
    assert_eq!(
        post(&grant, &service, &expired).await.1["error"],
        "period_mismatch"
    );
    let settled = post(&quota, &service, &principal).await.1;
    assert_eq!(settled["paid_bytes"], "30");
    assert_eq!(settled["free_bytes"], "24");
    assert_eq!(settled["remaining_bytes"], "44");
    task.abort();
    let _ = task.await;
    drop(relay);
    let restarted = broker(configured).unwrap();
    let accounting = restarted.relay_accounting.as_ref().unwrap();
    assert!(accounting.has_recharged("123").await.unwrap());
    assert!(!accounting.has_recharged("456").await.unwrap());
    let q = accounting
        .storage(|ledger| ledger.snapshot("123"))
        .await
        .unwrap();
    assert_eq!((q.paid_bytes, q.free_bytes, q.used_bytes), (30, 24, 10));
    let period = q.period;
    assert!(
        !accounting
            .storage(move |ledger| ledger.grant("123", &period, "lmm:demo-order-1", 30, true))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn external_sessions_require_both_credentials_and_count_the_registering_account() {
    use crate::wire;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{
        MaybeTlsStream, WebSocketStream, connect_async,
        tungstenite::{Message, client::IntoClientRequest, handshake::client::Request},
    };

    type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

    fn request(url: &str, ingress: Option<&str>, account: Option<&str>) -> Request {
        let mut req = url.into_client_request().unwrap();
        let headers = req.headers_mut();
        if let Some(ingress) = ingress {
            headers.insert("x-ssmcp-device-ingress", ingress.parse().unwrap());
        }
        if let Some(account) = account {
            headers.insert(
                header::AUTHORIZATION,
                format!("Bearer {account}").parse().unwrap(),
            );
        }
        headers.insert(
            wire::CAPABILITIES_HEADER,
            [
                wire::SESSION_ACCESS_CAPABILITY,
                wire::CONTROL_ATTRIBUTION_CAPABILITY,
            ]
            .join(", ")
            .parse()
            .unwrap(),
        );
        req
    }

    async fn expect_unauthorized(req: Request) {
        assert!(matches!(connect_async(req).await,
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) if response.status() == StatusCode::UNAUTHORIZED));
    }

    async fn register(req: Request) -> (Socket, wire::Frame) {
        let (mut socket, _) = connect_async(req).await.unwrap();
        // Both registrants share to Betty. The ACL cannot select whose slot gets used.
        let hello = wire::Frame::Hello {
            version: wire::VERSION,
            mode: wire::Mode::Shell,
            metadata: wire::ClientMetadata {
                name: "work".into(),
                title: None,
                cwd: "/test".into(),
                hostname: "device".into(),
                allow: vec!["betty@relay.invalid".into()],
            },
        };
        socket
            .send(Message::Binary(serde_json::to_vec(&hello).unwrap().into()))
            .await
            .unwrap();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if let Message::Binary(bytes) = socket.next().await.unwrap().unwrap() {
                    break serde_json::from_slice::<wire::Frame>(&bytes).unwrap();
                }
            }
        })
        .await
        .unwrap();
        (socket, frame)
    }

    let dir = Directory::new();
    let relay = broker(input(&dir)).unwrap();
    let app = crate::http::relay_router(relay.clone());
    let tcp = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("ws://{}/api/client/connect", tcp.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(tcp, app).await.unwrap() });
    let ingress = "d".repeat(64);
    let alice = "a".repeat(64);
    let betty = "b".repeat(64);
    expect_unauthorized(request(&url, None, Some(&alice))).await;
    expect_unauthorized(request(&url, Some(&ingress), None)).await;
    expect_unauthorized(request(&url, Some(&ingress), Some(&ingress))).await;
    expect_unauthorized(request(&url, Some(&ingress), Some(&"e".repeat(64)))).await;
    let mut ambiguous = request(&url, Some(&ingress), Some(&alice));
    ambiguous.headers_mut().append(
        header::AUTHORIZATION,
        format!("Bearer {betty}").parse().unwrap(),
    );
    expect_unauthorized(ambiguous).await;

    let alice_req = || request(&url, Some(&ingress), Some(&alice));
    let (left, right) = tokio::join!(register(alice_req()), register(alice_req()));
    let (accepted, denied) = if matches!(left.1, wire::Frame::Ready { .. }) {
        (left, right)
    } else {
        (right, left)
    };
    assert!(matches!(accepted.1, wire::Frame::Ready { .. }));
    assert!(
        matches!(denied.1, wire::Frame::Reject { message } if message.starts_with("online_session_limit:"))
    );
    let (other, frame) = register(request(&url, Some(&ingress), Some(&betty))).await;
    assert!(matches!(frame, wire::Frame::Ready { .. }));
    let accounting = relay.relay_accounting.as_ref().unwrap();
    accounting
        .storage(|ledger| {
            let period = ledger.snapshot("123")?.period;
            ledger.grant("123", &period, "first-purchase", 12, true)
        })
        .await
        .unwrap();
    let (additional, frame) = register(alice_req()).await;
    assert!(matches!(frame, wire::Frame::Ready { .. }));
    drop((accepted.0, denied.0, other, additional));
    task.abort();
    let _ = task.await;
}
