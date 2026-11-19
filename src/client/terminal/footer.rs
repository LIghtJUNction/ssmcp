//! Connection identity and hints share the existing, single bottom row.
use super::super::{account, language::Language};
use reqwest::Url;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

pub(super) struct Origin {
    full: String,
    authority: String,
}

impl Origin {
    #[cfg(target_os = "linux")]
    pub(super) fn parse_local(endpoint: &str) -> Option<Self> {
        if endpoint.len() > 2048
            || endpoint
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
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
        let origin = crate::oauth::local_origin(&endpoint.origin().ascii_serialization()).ok()?;
        let full = origin.origin().ascii_serialization();
        let authority = full.split_once("://")?.1.to_owned();
        Some(Self { full, authority })
    }

    /// Retain only the validated public origin, never endpoint credentials or tokens.
    pub(super) fn parse(endpoint: &str) -> Option<Self> {
        if endpoint.len() > 2048
            || endpoint
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace() || byte == b'\\')
        {
            return None;
        }
        let endpoint = Url::parse(endpoint).ok()?;
        let full = account::normalize_server(&endpoint.origin().ascii_serialization()).ok()?;
        let authority = full.split_once("://")?.1.to_owned();
        Some(Self { full, authority })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Connection {
    Preview,
    Disconnected,
    QueueUpdate,
    Connected,
}

impl Connection {
    pub(super) fn from_snapshot(preview: bool, data: &Value) -> Self {
        if preview {
            Self::Preview
        } else if !data["connected"].as_bool().unwrap_or(false) {
            Self::Disconnected
        } else if !data["supported"].as_bool().unwrap_or(false) {
            Self::QueueUpdate
        } else {
            Self::Connected
        }
    }

    fn label(self, language: Language) -> &'static str {
        match self {
            Self::Preview => language.text("preview · sample data", "预览 · 示例数据"),
            Self::Disconnected => language.text("disconnected", "已断开"),
            Self::QueueUpdate => {
                language.text("server update required for queue", "队列需要更新服务端")
            }
            Self::Connected => language.text("connected", "已连接"),
        }
    }

    fn compact(self, language: Language) -> &'static str {
        match self {
            Self::Preview => language.text("preview", "预览"),
            Self::Disconnected => language.text("offline", "离线"),
            Self::QueueUpdate => language.text("queue update", "队列待更新"),
            Self::Connected => language.text("connected", "已连接"),
        }
    }

    fn short(self, language: Language) -> &'static str {
        match self {
            Self::Disconnected => language.text("off", "断开"),
            Self::Connected => language.text("on", "在线"),
            Self::QueueUpdate => language.text("update", "更新"),
            Self::Preview => language.text("preview", "预览"),
        }
    }
}

fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut prefix = String::new();
    for character in text.chars() {
        let previous = prefix.len();
        prefix.push(character);
        prefix.push('…');
        // Variation selectors and joined emoji are not additive character widths.
        // Check exactly what will be drawn, including the truncation marker.
        if prefix.width() > width {
            prefix.truncate(previous);
            break;
        }
        prefix.pop();
    }
    prefix.push('…');
    prefix
}

fn compact_origin(text: &str, width: usize) -> String {
    // URL origin serialization is ASCII, including internationalized hostnames.
    debug_assert!(text.is_ascii());
    if text.len() <= width {
        return text.into();
    }
    if width <= 1 {
        return "…".repeat(width);
    }
    let tail = ((width - 1) * 2 / 3).max(1);
    let head = width - 1 - tail;
    format!("{}…{}", &text[..head], &text[text.len() - tail..])
}

fn connection_text(
    origin: Option<&Origin>,
    state: Connection,
    width: usize,
    language: Language,
) -> String {
    let Some(origin) = origin else {
        return clip(state.label(language), width);
    };
    let full = format!("{} · {}", state.label(language), origin.full);
    if full.width() <= width {
        return full;
    }
    let compact = format!("{} · {}", state.compact(language), origin.authority);
    if compact.width() <= width {
        return compact;
    }
    let status = if width < 20 {
        state.short(language)
    } else {
        state.compact(language)
    };
    let prefix = format!("{status} · ");
    if prefix.width() < width {
        format!(
            "{prefix}{}",
            compact_origin(&origin.authority, width - prefix.width())
        )
    } else {
        clip(status, width)
    }
}

fn hint_text(hint: &str, width: usize) -> String {
    if hint.width() <= width {
        return clip(hint, width);
    }
    let mut result = String::new();
    for part in hint.split(" · ") {
        let candidate = if result.is_empty() {
            part.to_owned()
        } else {
            format!("{result} · {part}")
        };
        if candidate.width() > width {
            break;
        }
        result = candidate;
    }
    if result.is_empty() && width >= 8 {
        clip(hint, width)
    } else {
        result
    }
}

/// Notices receive space before optional shortcuts; neither side can add a row.
pub(super) fn render_in(
    language: Language,
    hint: &str,
    origin: Option<&Origin>,
    state: Connection,
    columns: u16,
    priority: bool,
) -> String {
    let width = usize::from(columns);
    let margin = usize::from(width > 2);
    let available = width.saturating_sub(margin * 2);
    let clean: String = hint
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let hint = clean.trim();
    let (left, right) = if priority && !hint.is_empty() {
        // Preserve a complete notice whenever this row can hold it. A growing
        // window must not shorten it just because connection details now fit.
        let left = clip(hint, available);
        let remaining = available.saturating_sub(left.width() + 2);
        let right = if remaining >= state.short(language).width() {
            connection_text(origin, state, remaining, language)
        } else {
            String::new()
        };
        (left, right)
    } else {
        let right = connection_text(origin, state, available, language);
        let left = hint_text(hint, available.saturating_sub(right.width() + 2));
        (left, right)
    };
    let gap = available.saturating_sub(left.width() + right.width());
    format!(
        "{}{left}{}{right}{}",
        " ".repeat(margin),
        " ".repeat(gap),
        " ".repeat(margin)
    )
}

#[cfg(test)]
fn render(
    hint: &str,
    origin: Option<&Origin>,
    state: Connection,
    columns: u16,
    priority: bool,
) -> String {
    render_in(Language::English, hint, origin, state, columns, priority)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[cfg(target_os = "linux")]
    #[test]
    fn local_origin_retains_loopback_and_named_proxy_without_credentials() {
        for endpoint in [
            "http://127.0.0.1:43123/alice",
            "http://[::1]:43123/alice",
            "https://workstation.local/alice",
        ] {
            let origin = Origin::parse_local(endpoint).unwrap();
            let row = render("", Some(&origin), Connection::Connected, 100, false);
            assert!(row.contains(&origin.full));
            assert!(!row.contains("/alice"));
        }
        for endpoint in [
            "http://localhost:43123/alice",
            "http://192.0.2.1/alice",
            "https://remote.example/alice",
            "http://user:secret@127.0.0.1/alice",
            "http://127.0.0.1/alice?token=secret",
        ] {
            assert!(Origin::parse_local(endpoint).is_none());
        }
    }

    #[test]
    fn endpoint_identity_removes_credentials_path_query_and_fragment() {
        let origin = Origin::parse(
            "https://user:password@example.com:8443/mcp/private?token=secret#login-code",
        )
        .unwrap();
        assert_eq!(origin.full, "https://example.com:8443");
        assert_eq!(origin.authority, "example.com:8443");
        assert_eq!(
            Origin::parse("http://[::1]:8000/mcp").unwrap().full,
            "http://[::1]:8000"
        );
        for invalid in [
            "http://example.com/mcp",
            "file:///tmp/token",
            "https://example.com:0/mcp",
            "https://example.com/\nsecret",
            "https://example.com/\\secret",
            "not a server",
        ] {
            assert!(Origin::parse(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn connection_status_comes_from_the_live_snapshot() {
        assert_eq!(
            Connection::from_snapshot(false, &json!({})),
            Connection::Disconnected
        );
        assert_eq!(
            Connection::from_snapshot(true, &json!({})),
            Connection::Preview
        );
        assert_eq!(
            Connection::from_snapshot(false, &json!({"connected":true})),
            Connection::QueueUpdate
        );
        assert_eq!(
            Connection::from_snapshot(false, &json!({"connected":true,"supported":true})),
            Connection::Connected
        );
    }

    #[test]
    fn notices_and_connection_identity_share_one_bounded_row() {
        let origin = Origin::parse("https://ssmcp.lmm.best/mcp?token=hidden").unwrap();
        let notice = "/exit must be used without arguments";
        let line = render(notice, Some(&origin), Connection::Connected, 110, true);
        assert!(line.contains(notice), "{line}");
        assert!(
            line.contains("connected · https://ssmcp.lmm.best"),
            "{line}"
        );
        assert!(!line.contains("hidden"));
        let long = Origin::parse("https://a-very-long-development-host.ssmcp.example.com:8443/mcp")
            .unwrap();
        let narrow = render("", Some(&long), Connection::Connected, 40, false);
        assert!(
            narrow.contains('…') && narrow.contains("example.com:8443"),
            "{narrow}"
        );
        for width in 0..=160 {
            for priority in [false, true] {
                for state in [
                    Connection::Preview,
                    Connection::Disconnected,
                    Connection::QueueUpdate,
                    Connection::Connected,
                ] {
                    let line = render(
                        "保留草稿界 · Enter send\n · Tab browse",
                        Some(&long),
                        state,
                        width,
                        priority,
                    );
                    assert_eq!(line.width(), usize::from(width), "{width}: {line:?}");
                    assert!(!line.chars().any(char::is_control), "{line:?}");
                }
            }
        }
    }

    #[test]
    fn composed_unicode_notices_stay_inside_their_terminal_cells() {
        let origin = Origin::parse("https://ssmcp.lmm.best/mcp").unwrap();
        let selectors = "#\u{fe0f}".repeat(8);
        let joined = "\u{1f469}\u{200d}\u{1f4bb}".repeat(8);
        let combining = "e\u{301}".repeat(8);
        assert_eq!(selectors.width(), 16);
        assert_eq!(joined.width(), 16);
        assert_eq!(combining.width(), 8);

        // This exact VS16 notice used to draw 21 cells into a 20-column row.
        let reported = render(&selectors, Some(&origin), Connection::Connected, 20, true);
        assert_eq!(reported.width(), 20);
        assert!(reported.contains(selectors.as_str()), "{reported:?}");
        let clipped = clip(&selectors, 4);
        assert!(
            clipped.width() <= 4 && clipped.ends_with('…'),
            "{clipped:?}"
        );

        for notice in [&selectors, &joined, &combining] {
            for columns in [0, 1, 2, 3, 4, 5, 8, 17, 18, 20, 40] {
                let clipped = clip(notice, usize::from(columns));
                assert!(
                    clipped.width() <= usize::from(columns),
                    "{columns}: {clipped:?}"
                );
                for priority in [false, true] {
                    let row = render(
                        notice,
                        Some(&origin),
                        Connection::Connected,
                        columns,
                        priority,
                    );
                    assert_eq!(row.width(), usize::from(columns), "{columns}: {row:?}");
                    assert!(!row.chars().any(char::is_control), "{row:?}");
                }
            }
        }
    }

    #[test]
    fn notices_keep_their_words_when_the_window_grows() {
        let origin = Origin::parse("https://ssmcp.lmm.best/mcp").unwrap();
        let notice = "/exit must be used without arguments";
        for (columns, visible) in [
            (17, "/exit must be …"),
            (18, "/exit must be u…"),
            (20, "/exit must be use…"),
            (40, notice),
        ] {
            let row = render(notice, Some(&origin), Connection::Connected, columns, true);
            assert!(
                row.starts_with(format!(" {visible}").as_str()),
                "{columns}: {row:?}"
            );
            assert_eq!(row.width(), usize::from(columns), "{columns}: {row:?}");
        }
    }
}
