//! A public guide link and cached QR matrices. No I/O or encoding during redraw.
use super::super::{account, ui::Ui};
use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};
use reqwest::Url;
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

const QUIET: i32 = 4;
const STATUS_ROWS: usize = 8;
const MAX_URL_BYTES: usize = 512;
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) struct Guide {
    url: String,
    // Prefer Medium; Low is a compact fallback for a narrower sidebar.
    codes: Vec<QrCode>,
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
        let segments = QrSegment::make_segments(&url);
        let codes = [QrCodeEcc::Medium, QrCodeEcc::Low]
            .into_iter()
            .filter_map(|ecc| {
                QrCode::encode_segments_advanced(
                    &segments,
                    ecc,
                    Version::MIN,
                    Version::new(5),
                    None,
                    true,
                )
                .ok()
            })
            .collect();
        Self { url, codes }
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

    /// Complete, padded footer rows. A QR is never cropped or scaled.
    pub(super) fn rows(&self, width: u16, height: u16, ui: &Ui) -> Vec<String> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let cols = usize::from(width);
        let available = usize::from(height);
        // Origin serialization is ASCII (including IDNA hosts), so chunks never
        // split UTF-8, and a narrow footer retains every byte of the public URL.
        let links = self.url_lines(width);
        let code = if ui.color {
            self.codes.iter().find(|code| {
                let modules = (code.size() + 2 * QUIET) as usize;
                modules <= cols && 1 + modules.div_ceil(2) + links.len() + STATUS_ROWS <= available
            })
        } else {
            None
        };
        if code.is_none() && 1 + links.len() > available {
            // Do not present a cut-off URL as a usable destination. Scrollable F1
            // help always retains the complete link, even on a very small screen.
            return vec![line("Guide · F1 for URL", width, ui, "muted")];
        }
        let mut rows = vec![line(
            if code.is_some() {
                "Scan for the guide"
            } else {
                "Online guide · F1"
            },
            width,
            ui,
            "heading",
        )];
        if let Some(code) = code {
            let modules = code.size() + 2 * QUIET;
            let padding = (cols - modules as usize) / 2;
            for y in (0..modules).step_by(2) {
                let mut text = " ".repeat(padding);
                for x in 0..modules {
                    let top = code.get_module(x - QUIET, y - QUIET);
                    let bottom = code.get_module(x - QUIET, y + 1 - QUIET);
                    text.push(match (top, bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    });
                }
                text.push_str(&" ".repeat(cols - padding - modules as usize));
                // Machine-readable contrast must not inherit a low-contrast theme.
                // The only payload here is our own fixed half-block alphabet.
                rows.push(format!("\x1b[38;2;0;0;0;48;2;255;255;255m{text}\x1b[0m"));
            }
        }
        rows.extend(links.into_iter().map(|text| line(text, width, ui, "muted")));
        rows
    }
}

fn line(text: &str, width: u16, ui: &Ui, color: &str) -> String {
    let text = super::fit(text, width);
    let padding = " ".repeat(usize::from(width).saturating_sub(text.width()));
    if ui.color {
        format!(
            "\x1b[{};{}m{text}{padding}\x1b[0m",
            ui.background(),
            ui.color(color)
        )
    } else {
        format!("{text}{padding}")
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
            let shown = guide.rows(
                80,
                5,
                &Ui {
                    color: false,
                    ..Ui::default()
                },
            );
            assert!(shown.iter().any(|row| row.trim_end() == guide.url()));
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
    fn default_footer_preserves_every_module_and_four_module_quiet_zone() {
        let guide = Guide::from_endpoint("https://ssmcp.lmm.best/mcp").unwrap();
        let rows = guide.rows(37, 29, &Ui::default());
        let code = &guide.codes[0];
        assert_eq!(code.size(), 29);
        assert_eq!(rows.len(), 21);
        let mut parser = vt100::Parser::new(
            super::super::nonzero(rows.len() as u16),
            super::super::nonzero(37),
            0,
        );
        parser.process(rows.join("\r\n").as_bytes());
        let screen = parser.screen();
        for y in 0..38 {
            for x in 0..37 {
                let cell = screen.cell(1 + y / 2, x).unwrap();
                assert_eq!(cell.fgcolor(), vt100::Color::Rgb(0, 0, 0));
                assert_eq!(cell.bgcolor(), vt100::Color::Rgb(255, 255, 255));
                let value = cell.contents();
                let dark = if y % 2 == 0 {
                    matches!(value, "█" | "▀")
                } else {
                    matches!(value, "█" | "▄")
                };
                assert_eq!(
                    dark,
                    code.get_module(i32::from(x) - QUIET, i32::from(y) - QUIET)
                );
            }
        }
        assert!(screen.contents().contains(guide.url()));
    }

    #[test]
    fn small_or_uncolored_sidebars_keep_complete_links_without_partial_qr() {
        let guide = Guide::from_endpoint("https://ssmcp.lmm.best/alice").unwrap();
        let mono = Ui {
            color: false,
            ..Ui::default()
        };
        for (width, height, ui) in [
            (30, 29, Ui::default()),
            (37, 20, Ui::default()),
            (37, 29, mono),
        ] {
            let rows = guide.rows(width, height, &ui);
            let mut parser = vt100::Parser::new(
                super::super::nonzero(rows.len() as u16),
                super::super::nonzero(width),
                0,
            );
            parser.process(rows.join("\r\n").as_bytes());
            assert!(parser.screen().contents().contains(guide.url()));
            assert!(!rows.iter().any(|row| row.contains(['█', '▀', '▄'])));
            if !ui.color {
                assert!(!rows.iter().any(|row| row.contains('\x1b')));
            }
        }
        let low = guide.rows(33, 29, &Ui::default());
        assert!(low.iter().any(|row| row.contains(['█', '▀', '▄'])));
        let long =
            Guide::from_endpoint(&format!("https://{}.example/alice", "a".repeat(60))).unwrap();
        let rows = long.rows(
            20,
            25,
            &Ui {
                color: false,
                ..Ui::default()
            },
        );
        assert_eq!(
            rows.iter()
                .skip(1)
                .map(|row| row.trim_end())
                .collect::<String>(),
            long.url()
        );
        assert!(guide.rows(0, 20, &Ui::default()).is_empty());
    }
}
