//! Content-free server audit records. Never log bodies, query strings or credentials.
use axum::{extract::Request, http::HeaderValue, middleware::Next, response::Response};
use std::{
    io::{self, IsTerminal, Write},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);
tokio::task_local! { pub static REQUEST_ID: String; }

fn level(value: &str) -> Option<u8> {
    match value {
        "off" => Some(0),
        "error" => Some(1),
        "warn" => Some(2),
        "info" => Some(3),
        "debug" => Some(4),
        "trace" => Some(5),
        _ => None,
    }
}
fn enabled(filter: &str, category: &str, severity: &str) -> bool {
    let mut threshold = 3;
    let mut specificity = 0;
    for directive in filter.split(',').map(str::trim) {
        if let Some((target, value)) = directive.split_once('=') {
            let target = target.trim();
            let Some(value) = level(value.trim()) else {
                continue;
            };
            if [format!("ssmcpd::{category}"), format!("ssmcp::{category}")]
                .iter()
                .any(|actual| {
                    actual == target
                        || actual
                            .strip_prefix(target)
                            .is_some_and(|tail| tail.starts_with("::"))
                })
                && target.len() >= specificity
            {
                threshold = value;
                specificity = target.len();
            }
        } else if let Some(value) = level(directive)
            && specificity == 0
        {
            threshold = value;
        }
    }
    level(severity).is_some_and(|n| n > 0 && n <= threshold)
}
fn safe_key(key: &str) -> bool {
    matches!(
        key,
        "event"
            | "request_id"
            | "rpc_id"
            | "client_id"
            | "session_id"
            | "user"
            | "github_subject_id"
            | "method"
            | "duration_ms"
            | "status"
            | "outcome"
            | "clients"
            | "draining"
            | "transports"
            | "listen"
            | "socket"
            | "mode"
            | "timeout_ms"
    )
}
fn record(severity: &str, category: &str, fields: &[(&str, String)], now: u128) -> String {
    let mut line = format!("ts_ms={now} level={severity} target=ssmcpd::{category}");
    for (key, value) in fields {
        if safe_key(key) {
            let value: String = value.chars().take(256).collect();
            line += &format!(
                " {key}={}",
                serde_json::to_string(&value).unwrap_or_default()
            );
        }
    }
    line.push('\n');
    line
}
pub fn log(severity: &str, category: &str, fields: &[(&str, String)]) {
    let filter = std::env::var("RUST_LOG").unwrap_or_default();
    if !enabled(&filter, category, severity) {
        return;
    }
    let mut fields = fields.to_vec();
    if !fields.iter().any(|(key, _)| *key == "request_id")
        && let Ok(id) = REQUEST_ID.try_with(Clone::clone)
    {
        fields.push(("request_id", id));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let line = record(severity, category, &fields, now);
    let mut out = io::stderr().lock();
    let color = out.is_terminal()
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|t| t != "dumb");
    if color {
        let code = match severity {
            "error" => 31,
            "warn" => 33,
            "debug" | "trace" => 90,
            _ => 36,
        };
        let _ = write!(out, "\x1b[{code}m{line}\x1b[0m");
    } else {
        let _ = out.write_all(line.as_bytes());
    }
}

pub async fn http(mut request: Request, next: Next) -> Response {
    let id = format!(
        "{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let path = request.uri().path();
    let category = if path == "/healthz" {
        "health"
    } else if path == "/oauth/callback/github" || path.starts_with("/account/") {
        "identity"
    } else if path.starts_with("/oauth/") || path.starts_with("/.well-known/") {
        "oauth"
    } else {
        "mcp"
    };
    let method = request.method().to_string();
    request.extensions_mut().insert(RequestId(id.clone()));
    let started = std::time::Instant::now();
    let mut response = REQUEST_ID.scope(id.clone(), next.run(request)).await;
    let status = response.status().as_u16();
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&id).expect("generated request ID is ASCII"),
    );
    let severity = if status >= 500 {
        "error"
    } else if status == 401 || status == 403 {
        "warn"
    } else if category == "health" {
        "debug"
    } else {
        "info"
    };
    log(
        severity,
        category,
        &[
            ("event", "http_complete".into()),
            ("request_id", id),
            ("method", method),
            ("status", status.to_string()),
            ("duration_ms", started.elapsed().as_millis().to_string()),
        ],
    );
    response
}
#[derive(Clone)]
pub struct RequestId(pub String);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rust_log_supports_global_and_target_thresholds() {
        assert!(enabled("", "mcp", "info"));
        assert!(!enabled("", "mcp", "debug"));
        assert!(enabled("warn,ssmcpd::mcp=debug", "mcp", "debug"));
        assert!(!enabled("warn,ssmcpd::mcp=debug", "oauth", "info"));
        assert!(!enabled("ssmcpd::mcp=off,trace", "mcp", "error"));
        assert!(!enabled("ssmcpd::mc=trace,warn", "mcp", "info"));
        assert!(enabled("ssmcp=debug", "persistence", "debug"));
    }
    #[test]
    fn audit_fields_are_allowlisted_bounded_and_single_line() {
        let line = record(
            "info",
            "oauth",
            &[
                ("event", "registered\nforged=1".into()),
                ("access_token", "DO_NOT_LOG".into()),
                ("client_secret", "DO_NOT_LOG".into()),
                ("request_id", "x".repeat(500)),
            ],
            123,
        );
        assert!(!line.contains("DO_NOT_LOG"));
        assert_eq!(line.lines().count(), 1);
        assert!(line.contains("registered\\nforged"));
        assert!(line.len() < 400);
    }
    #[tokio::test]
    async fn http_request_ids_are_unique_and_ignore_untrusted_headers() {
        let app = axum::Router::new()
            .route("/healthz", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(http));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/healthz", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::http_client::builder().build().unwrap();
        let a = client
            .get(&url)
            .header("x-request-id", "untrusted")
            .send()
            .await
            .unwrap();
        let b = client.get(&url).send().await.unwrap();
        assert_ne!(a.headers()["x-request-id"], "untrusted");
        assert_ne!(a.headers()["x-request-id"], b.headers()["x-request-id"]);
        task.abort();
    }
}
