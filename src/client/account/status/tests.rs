use super::*;
use crate::client::account::{create_tree, now, save_at};
use serde_json::json;
use std::path::PathBuf;

fn credential(server: &str) -> Credential {
    Credential {
        server: server.into(),
        token: "a".repeat(64),
        principal: "alice@github".into(),
        subject: Some("123".into()),
        username: None,
        expires_at: now() + 3600,
    }
}

fn reply() -> Value {
    json!({
        "identity": {"provider":"github", "subject":"123", "login":"alice",
                     "principal":"alice@github", "username":"alice-dev"},
        "traffic": {"limit_bytes":1_000_000_000u64, "remaining_bytes":750_000_000u64}
    })
}

struct AuthHome(PathBuf);
impl AuthHome {
    fn new() -> Self {
        let mut random = [0; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!("ssmcp-status-{:x?}", random));
        create_tree(&root).unwrap();
        Self(root)
    }
}
impl Drop for AuthHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn absent_and_partial_traffic_never_mean_unlimited_or_zero() {
    for value in [
        json!({}),
        json!({"traffic":null}),
        json!({"traffic":{}}),
        json!({"traffic":{"limit_bytes":null}}),
        json!({"traffic":{"remaining_bytes":null}}),
        json!({"traffic":{"remaining_bytes":0}}),
        json!({"traffic":{"limit_bytes":null,"remaining_bytes":0}}),
        json!({"traffic":{"limit_bytes":100,"remaining_bytes":null}}),
        json!({"traffic":{"limit_bytes":100,"remaining_bytes":101}}),
        json!({"traffic":{"limit_bytes":100,"remaining_bytes":-1}}),
        json!({"traffic":{"limit_bytes":100,"remaining_bytes":"0"}}),
    ] {
        let traffic = Traffic::from_reply(&value);
        assert_eq!(traffic, Traffic::Unknown, "{value}");
        assert_ne!(traffic.label(), "∞");
    }
    assert_eq!(
        Traffic::from_reply(&json!({"traffic":{"limit_bytes":null,"remaining_bytes":null}})),
        Traffic::Unlimited
    );
    assert_eq!(
        Traffic::from_reply(&json!({"traffic":{"limit_bytes":100,"remaining_bytes":0}})),
        Traffic::Remaining(0)
    );
    assert_eq!(
        Traffic::from_reply(&reply()),
        Traffic::Remaining(750_000_000)
    );
}

#[test]
fn remaining_uses_decimal_gb_without_rounding_positive_traffic_to_zero() {
    for (bytes, expected) in [
        (0, "0 GB"),
        (1, "<0.01 GB"),
        (9_999_999, "<0.01 GB"),
        (10_000_000, "0.01 GB"),
        (750_000_000, "0.75 GB"),
        (999_999_999, "0.99 GB"),
        (1_000_000_000, "1.00 GB"),
        (u64::MAX, "18446744073.70 GB"),
    ] {
        assert_eq!(Traffic::Remaining(bytes).label(), expected);
    }
    assert_eq!(Traffic::Unlimited.label(), "∞");
    for status in [Status::Loading, Status::SignedOut, Status::Unavailable] {
        let rows = status.rows().join("\n");
        assert!(!rows.contains("GB") && !rows.contains('∞'));
    }
}

#[test]
fn public_identity_must_match_the_saved_principal_and_subject() {
    let saved = credential("https://selected.example");
    let status = Status::from_reply(&reply(), &saved);
    assert_eq!(
        status.rows(),
        [" 账号  alice (alice-dev)", " 本月剩余  0.75 GB"]
    );
    let mut unlimited = reply();
    unlimited["traffic"] = json!({"limit_bytes":null, "remaining_bytes":null});
    assert_eq!(
        Status::from_reply(&unlimited, &saved).rows(),
        [" 账号  alice (alice-dev)", " 本月剩余  ∞"]
    );
    let mut other = reply();
    other["identity"]["subject"] = json!("456");
    assert_eq!(Status::from_reply(&other, &saved), Status::Unavailable);
    other = reply();
    other["identity"]["login"] = json!("bob");
    other["identity"]["principal"] = json!("bob@github");
    assert_eq!(Status::from_reply(&other, &saved), Status::Unavailable);
    other = reply();
    other.as_object_mut().unwrap().remove("traffic");
    assert_eq!(
        Status::from_reply(&other, &saved).rows()[1],
        " 本月剩余  未提供"
    );
}

#[test]
fn selected_endpoint_retains_only_a_valid_server_origin() {
    assert_eq!(
        server_from_endpoint("https://SELECTED.example:443/alice").as_deref(),
        Some("https://selected.example")
    );
    assert_eq!(
        server_from_endpoint("http://127.0.0.1:8123/mcp").as_deref(),
        Some("http://127.0.0.1:8123")
    );
    for endpoint in [
        "https://user:secret@example.com/mcp",
        "https://example.com/mcp?token=secret",
        "https://example.com/mcp#secret",
        "http://example.com/mcp",
        "ws://example.com/mcp",
        "https://example.com/\nsecret",
        "https://example.com\\evil",
    ] {
        assert!(server_from_endpoint(endpoint).is_none(), "{endpoint:?}");
    }
}

async fn server(status: &str, body: String) -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let request = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0 && request.len() + read <= 8192);
            request.extend_from_slice(&buffer[..read]);
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (origin, request)
}

#[tokio::test]
async fn refresh_uses_selected_server_even_when_another_account_is_default() {
    let root = AuthHome::new();
    let (origin, request) = server("200 OK", reply().to_string()).await;
    save_at(&root.0, &credential(&origin)).unwrap();
    let mut other = credential("https://other.example");
    other.token = "b".repeat(64);
    save_at(&root.0, &other).unwrap();
    let status = Status::refresh_at(&root.0, &origin).await;
    assert_eq!(status.rows()[1], " 本月剩余  0.75 GB");
    let request = request.await.unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /api/client/whoami http/1.1\r\n"));
    assert!(request.contains(&format!("authorization: bearer {}\r\n", "a".repeat(64))));
    assert!(!request.contains(&"b".repeat(64)));
    assert_eq!(
        Status::refresh_at(&root.0, "https://missing.example").await,
        Status::SignedOut
    );
}

#[tokio::test]
async fn failed_api_calls_never_reuse_or_invent_account_quota() {
    for (http_status, body, expected) in [
        (
            "401 Unauthorized",
            json!({"error":"expired"}).to_string(),
            Status::SignedOut,
        ),
        (
            "403 Forbidden",
            json!({"error":"account_banned"}).to_string(),
            Status::Unavailable,
        ),
        (
            "500 Internal Server Error",
            reply().to_string(),
            Status::Unavailable,
        ),
        ("200 OK", "not json".into(), Status::Unavailable),
        ("302 Found", reply().to_string(), Status::Unavailable),
    ] {
        let (origin, request) = server(http_status, body).await;
        let status = Status::fetch(&credential(&origin)).await;
        assert_eq!(status, expected, "{http_status}");
        assert!(!status.rows().join(" ").contains(&"a".repeat(64)));
        request.await.unwrap();
    }
}
