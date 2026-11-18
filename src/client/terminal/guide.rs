//! A public guide link and cached QR matrices. No I/O or encoding during redraw.
use super::super::{account, ui::Ui};
use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};
use reqwest::Url;
use unicode_width::UnicodeWidthStr;

const QUIET: i32 = 4;
const STATUS_ROWS: usize = 8;
const MAX_URL_BYTES: usize = 512;

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
        Some(Self { url, codes })
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
