//! Public guide links and same-origin route checks, plus the local ASCII footer.
use super::super::account;
use reqwest::Url;
use std::time::Duration;

mod pet;
pub(super) use pet::rows as pet_rows;

const MAX_URL_BYTES: usize = 512;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Guide {
    url: String,
}

impl Guide {
    pub(super) fn from_endpoint(endpoint: &str) -> Option<Self> {
        if endpoint.len() > 2048
            || endpoint
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\')
        {
            return None;
        }
        let endpoint = Url::parse(endpoint).ok()?;
        if !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return None;
        }
        // Reuse the selected-server origin rules, including literal loopback HTTP
        // for local fixtures. Neither the endpoint path nor any login code survives.
        let origin = account::normalize_server(&endpoint.origin().ascii_serialization()).ok()?;
        let url = format!("{origin}/guide/");
        if url.len() > MAX_URL_BYTES {
            return None;
        }
        Some(Self::new(url))
    }

    fn new(url: String) -> Self {
        Self { url }
    }

    /// The established portal is usable while a live session checks the newer route.
    pub(super) fn portal(self) -> Self {
        let origin = self
            .url
            .strip_suffix("/guide/")
            .expect("guide URLs are constructed from a validated origin");
        Self::new(format!("{origin}/"))
    }

    /// One anonymous, same-origin request. No response body or redirect is needed.
    /// The caller owns this task and cancels it when its display session ends.
    pub(super) async fn resolve(endpoint: &str) -> Option<Self> {
        let guide = Self::from_endpoint(endpoint)?;
        let available = tokio::time::timeout(PROBE_TIMEOUT, async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .referer(false)
                .connect_timeout(PROBE_TIMEOUT)
                .timeout(PROBE_TIMEOUT)
                .build()
                .ok()?;
            client.get(guide.url()).send().await.ok()
        })
        .await
        .ok()
        .flatten()
        .is_some_and(|response| response.status().is_success());
        Some(if available { guide } else { guide.portal() })
    }

    pub(super) fn url(&self) -> &str {
        &self.url
    }

    pub(super) fn url_lines(&self, width: u16) -> Vec<&str> {
        self.url
            .as_bytes()
            .chunks(usize::from(width.max(1)))
            .map(|bytes| std::str::from_utf8(bytes).expect("serialized origin is ASCII"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_uses_only_a_valid_public_origin() {
        assert_eq!(
            Guide::from_endpoint("https://EXAMPLE.com:443/private-user")
                .unwrap()
                .url(),
            "https://example.com/guide/"
        );
        assert_eq!(
            Guide::from_endpoint("http://[::1]:8080/alice")
                .unwrap()
                .url(),
            "http://[::1]:8080/guide/"
        );
        for raw in [
            "https://user:secret@example.com/mcp",
            "https://example.com/mcp?token=secret",
            "https://example.com/mcp#secret",
            "http://example.com/mcp",
            "http://localhost:8080/mcp",
            "javascript:alert(1)",
            "https://example.com\\evil",
            "https://example.com/\nsecret",
        ] {
            assert!(Guide::from_endpoint(raw).is_none(), "accepted {raw:?}");
        }
    }

    async fn reply_once(response: String) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
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
    async fn probe_selects_the_available_route_without_sending_endpoint_or_auth_data() {
        for (status, available) in [
            ("200 OK", true),
            ("404 Not Found", false),
            ("401 Unauthorized", false),
            ("503 Service Unavailable", false),
        ] {
            let (origin, request) = reply_once(format!(
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ))
            .await;
            let guide = Guide::resolve(&format!("{origin}/private-user/opaque-login-code"))
                .await
                .unwrap();
            assert_eq!(
                guide.url(),
                format!("{origin}/{}", if available { "guide/" } else { "" })
            );
            let request = request.await.unwrap();
            assert!(request.starts_with("GET /guide/ HTTP/1.1\r\n"), "{request}");
            let headers = request.to_ascii_lowercase();
            for forbidden in [
                "private-user",
                "opaque-login-code",
                "authorization:",
                "cookie:",
                "referer:",
            ] {
                assert!(!headers.contains(forbidden), "request leaked {forbidden}");
            }
            assert_eq!(guide.url_lines(80).concat(), guide.url());
        }
    }

    #[tokio::test]
    async fn probe_rejects_credentials_and_never_follows_redirects() {
        let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination_origin = format!("http://{}", destination.local_addr().unwrap());
        for endpoint in [
            destination_origin.replacen("://", "://user:secret@", 1),
            format!("{destination_origin}/mcp?token=secret"),
            format!("{destination_origin}/mcp#secret"),
        ] {
            assert!(Guide::resolve(&endpoint).await.is_none());
        }
        let (origin, request) = reply_once(format!(
            "HTTP/1.1 302 Found\r\nLocation: {destination_origin}/login?token=secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;
        let guide = Guide::resolve(&format!("{origin}/mcp")).await.unwrap();
        assert_eq!(guide.url(), format!("{origin}/"));
        assert!(
            request
                .await
                .unwrap()
                .starts_with("GET /guide/ HTTP/1.1\r\n")
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), destination.accept())
                .await
                .is_err(),
            "credentials and redirect destinations must never produce a request"
        );
    }

    #[tokio::test]
    async fn unanswered_probe_returns_the_portal_within_its_network_budget() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (accepted, connected) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            accepted.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let endpoint = format!("{origin}/private-user");
        let probe = tokio::spawn(async move { Guide::resolve(&endpoint).await });
        connected.await.unwrap();
        let guide = tokio::time::timeout(PROBE_TIMEOUT + Duration::from_secs(1), probe)
            .await
            .expect("a connected server that sends no headers cannot keep discovery pending")
            .unwrap()
            .unwrap();
        server.abort();
        let _ = server.await;
        assert_eq!(guide.url(), format!("{origin}/"));
    }

    #[test]
    fn full_public_links_remain_available_without_graphics() {
        let guide = Guide::from_endpoint("https://ssmcp.lmm.best/private-user").unwrap();
        for width in [0, 1, 12, 80] {
            let lines = guide.url_lines(width);
            assert_eq!(lines.concat(), guide.url());
            assert!(
                lines
                    .iter()
                    .all(|line| line.len() <= usize::from(width.max(1)))
            );
        }
        assert_eq!(guide.portal().url(), "https://ssmcp.lmm.best/");
    }
}
