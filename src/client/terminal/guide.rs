//! A public guide link and cached QR matrices. No I/O or encoding during redraw.
use super::super::{account, ui::Ui};
use super::images::{CellSize, Native, Placement};
use qrcodegen::{QrCode, QrCodeEcc, QrSegment, Version};
use reqwest::Url;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

mod native;

const QUIET: i32 = 4;
const STATUS_ROWS: usize = 8;
const MAX_URL_BYTES: usize = 512;

pub(super) struct Guide {
    url: String,
    // Fixed square pixels with Medium preferred over a compact Low fallback.
    // Encoding occurs once at connection setup, never during a redraw.
    codes: Vec<Arc<Native>>,
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
            .filter_map(|code| native::prepare(&code))
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

    /// Complete footer rows and an optional, wholly visible native QR. The
    /// terminal must report graphics support and actual cell dimensions first.
    pub(super) fn rows(
        &self,
        width: u16,
        height: u16,
        ui: &Ui,
        cell: Option<CellSize>,
        origin: (u16, u16),
    ) -> (Vec<String>, Option<Placement>) {
        if width == 0 || height == 0 {
            return (Vec::new(), None);
        }
        let links = self.url_lines(width);
        let available = usize::from(height);
        let image_rows = available.saturating_sub(STATUS_ROWS + 1 + links.len()) as u16;
        let code = cell.filter(|_| ui.color).and_then(|cell| {
            self.codes.iter().find_map(|image| {
                // QR pixels stay square and unscaled. A cramped pane gets the
                // complete URL instead of a cropped or font-dependent symbol.
                if image.width > u32::from(width) * u32::from(cell.width)
                    || image.height > u32::from(image_rows) * u32::from(cell.height)
                {
                    return None;
                }
                image
                    .layout(width, image_rows, cell)
                    .map(|bounds| (image, bounds))
            })
        });
        if code.is_none() && 1 + links.len() > available {
            return (vec![line("Guide · F1 for URL", width, ui, "muted")], None);
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
        if let Some((_, bounds)) = code {
            rows.extend((0..bounds.rows).map(|_| line("", width, ui, "foreground")));
        }
        rows.extend(links.into_iter().map(|text| line(text, width, ui, "muted")));
        let placement = code.and_then(|(image, bounds)| {
            let row = origin
                .0
                .checked_add(height.saturating_sub(rows.len() as u16))?
                .checked_add(1)?;
            let col = origin
                .1
                .checked_add(width.saturating_sub(bounds.columns) / 2)?;
            bounds.placement(Arc::clone(image), row, col, 0, bounds.rows)
        });
        (rows, placement)
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
    fn supported_default_footer_places_whole_square_pixels_inside_its_rows() {
        let guide = Guide::from_endpoint("https://ssmcp.lmm.best/mcp").unwrap();
        let (rows, placement) = guide.rows(
            37,
            29,
            &Ui::default(),
            Some(CellSize {
                width: 8,
                height: 16,
            }),
            (0, 0),
        );
        assert_eq!(rows.len(), 21);
        assert!(rows.join("\n").contains(guide.url()));
        assert!(!rows.iter().any(|row| row.contains(['█', '▀', '▄'])));
        let (_, wire) = super::super::images::Renderer::default().update(
            vec![placement.expect("a supported 120x30 terminal has room for the QR")],
            (0, 0),
            true,
        );
        assert!(wire.contains("a=t,t=d,f=100"));
        assert!(wire.contains("w=296,h=296"));
        assert!(wire.contains("\x1b[10;1H"));
        assert!(
            !wire.contains(",c=") && !wire.contains(",r="),
            "QR pixels are never scaled"
        );
    }

    #[test]
    fn unsupported_small_or_uncolored_sidebars_keep_complete_links_without_a_partial_qr() {
        let guide = Guide::from_endpoint("https://ssmcp.lmm.best/alice").unwrap();
        let cell = Some(CellSize {
            width: 8,
            height: 16,
        });
        let mono = Ui {
            color: false,
            ..Ui::default()
        };
        for (width, height, ui, graphics) in [
            (30, 29, Ui::default(), cell),
            (37, 20, Ui::default(), cell),
            (37, 29, mono, cell),
            (37, 29, Ui::default(), None),
        ] {
            let (rows, placement) = guide.rows(width, height, &ui, graphics, (0, 0));
            assert!(placement.is_none());
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
        assert!(guide.rows(33, 29, &Ui::default(), cell, (0, 0)).1.is_some());
        let long =
            Guide::from_endpoint(&format!("https://{}.example/alice", "a".repeat(60))).unwrap();
        let (rows, placement) = long.rows(
            20,
            25,
            &Ui {
                color: false,
                ..Ui::default()
            },
            None,
            (0, 0),
        );
        assert!(placement.is_none());
        assert_eq!(
            rows.iter()
                .skip(1)
                .map(|row| row.trim_end())
                .collect::<String>(),
            long.url()
        );
        assert!(guide.rows(0, 20, &Ui::default(), cell, (0, 0)).0.is_empty());
    }
}
