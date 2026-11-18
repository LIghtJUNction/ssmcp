//! Bounded, terminal-safe Markdown. Unsupported syntax remains readable.
use crate::client::{panel, ui::Ui};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Face {
    bold: bool,
    italic: bool,
    code: bool,
    muted: bool,
    heading: bool,
}

fn plain(text: &str, face: Face, out: &mut Vec<(char, Face)>) {
    out.extend(text.chars().map(|c| (c, face)));
}

fn inline(mut text: &str, face: Face, depth: u8, out: &mut Vec<(char, Face)>) {
    if depth > 4 {
        plain(text, face, out);
        return;
    }
    while !text.is_empty() {
        if let Some(rest) = text.strip_prefix('\\')
            && let Some(c) = rest.chars().next()
            && "\\*_`[]()#>~-".contains(c)
        {
            out.push((c, face));
            text = &rest[c.len_utf8()..];
            continue;
        }
        if text.starts_with('`') {
            let n = text.bytes().take_while(|b| *b == b'`').count();
            let fence = &text[..n];
            if let Some(end) = text[n..].find(fence) {
                plain(&text[n..n + end], Face { code: true, ..face }, out);
                text = &text[n + end + n..];
                continue;
            }
        }
        if let Some(rest) = text.strip_prefix('[')
            && let Some(end) = rest.find("](")
            && let Some(close) = rest[end + 2..].find(')')
        {
            inline(&rest[..end], Face { bold: true, ..face }, depth + 1, out);
            let url = &rest[end + 2..end + 2 + close];
            if !url.is_empty() {
                plain(
                    &format!(" ({url})"),
                    Face {
                        muted: true,
                        ..face
                    },
                    out,
                );
            }
            text = &rest[end + 3 + close..];
            continue;
        }
        let mut matched = false;
        for marker in ["**", "__", "*", "_"] {
            if let Some(rest) = text.strip_prefix(marker)
                && let Some(end) = rest.find(marker).filter(|end| *end > 0)
            {
                let next = if marker.len() == 2 {
                    Face { bold: true, ..face }
                } else {
                    Face {
                        italic: true,
                        ..face
                    }
                };
                inline(&rest[..end], next, depth + 1, out);
                text = &rest[end + marker.len()..];
                matched = true;
                break;
            }
        }
        if !matched {
            let c = text.chars().next().unwrap();
            out.push((c, face));
            text = &text[c.len_utf8()..];
        }
    }
}

fn paint(cells: &[(char, Face)], width: usize, ui: &Ui) -> String {
    let mut out = String::new();
    let mut previous = None;
    let mut used = 0;
    for &(c, face) in cells {
        if ui.color && previous != Some(face) {
            let color = if face.code {
                "accent"
            } else if face.heading {
                "heading"
            } else if face.muted {
                "muted"
            } else {
                "foreground"
            };
            out.push_str(&format!(
                "\x1b[0;{};{}{}{}m",
                ui.background(),
                ui.color(color),
                if face.bold { ";1" } else { "" },
                if face.italic { ";3" } else { "" }
            ));
            previous = Some(face);
        }
        out.push(c);
        used += c.width().unwrap_or(0);
    }
    if ui.color {
        out.push_str(&format!(
            "\x1b[0;{};{}m",
            ui.background(),
            ui.color("foreground")
        ));
    }
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    if ui.color {
        out.push_str("\x1b[0m");
    }
    out
}

fn wrap(cells: Vec<(char, Face)>, prefix: &str, width: usize, ui: &Ui, out: &mut Vec<String>) {
    let margin = if width >= 4 { "  " } else { "" };
    let available = width.saturating_sub(margin.len()).max(1);
    let prefix = if prefix
        .chars()
        .map(|c| c.width().unwrap_or(0))
        .sum::<usize>()
        < available
    {
        prefix
    } else {
        ""
    };
    let mut row: Vec<_> = margin.chars().map(|c| (c, Face::default())).collect();
    plain(
        prefix,
        Face {
            muted: true,
            ..Face::default()
        },
        &mut row,
    );
    let indent: usize = row.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
    let mut used = indent;
    for (mut c, face) in cells {
        let mut size = c.width().unwrap_or(0);
        if size > width.saturating_sub(indent) {
            c = '…';
            size = 1;
        }
        if used + size > width && used > indent {
            out.push(paint(&row, width, ui));
            row = " "
                .repeat(indent)
                .chars()
                .map(|c| (c, Face::default()))
                .collect();
            used = indent;
        }
        row.push((c, face));
        used += size;
    }
    out.push(paint(&row, width, ui));
}

/// Headings, emphasis, inline/fenced code, links, lists and block quotes.
/// Input escape sequences, URLs and HTML never execute terminal commands.
pub(crate) fn rows(text: &str, width: u16, ui: &Ui) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    for raw in text.lines() {
        let safe = panel::safe_text(raw).replace("\\t", "    ");
        let line = safe.trim_end();
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if matches!(marker, '`' | '~') && count >= 3 {
            if let Some((open, length)) = fence {
                if marker == open && count >= length && trimmed[count..].trim().is_empty() {
                    fence = None;
                    continue;
                }
            } else {
                fence = Some((marker, count));
                let label = trimmed[count..].trim();
                let mut cells = Vec::new();
                plain(
                    if label.is_empty() { "code" } else { label },
                    Face {
                        muted: true,
                        ..Face::default()
                    },
                    &mut cells,
                );
                wrap(cells, "", width, ui, &mut rows);
                continue;
            }
        }
        let mut face = Face::default();
        let mut prefix = String::new();
        let mut body = line;
        if fence.is_some() {
            face.code = true;
            prefix.push_str("│ ");
        } else {
            let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
            if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
                face.bold = true;
                face.heading = true;
                body = trimmed[hashes + 1..].trim_end_matches('#').trim_end();
            } else if let Some(quote) = trimmed.strip_prefix("> ") {
                prefix.push_str("│ ");
                face.muted = true;
                body = quote;
            } else if let Some(item) = ["- ", "* ", "+ "]
                .iter()
                .find_map(|p| trimmed.strip_prefix(p))
            {
                prefix.push_str("• ");
                body = item;
            } else {
                let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
                if digits > 0
                    && digits <= 9
                    && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") "))
                {
                    prefix.push_str(&trimmed[..digits + 2]);
                    body = &trimmed[digits + 2..];
                }
            }
        }
        let mut cells = Vec::new();
        if face.code {
            plain(body, face, &mut cells);
        } else {
            inline(body, face, 0, &mut cells);
        }
        wrap(cells, &prefix, width, ui, &mut rows);
        if rows.len() > 2048 {
            rows.truncate(2048);
            rows.push(paint(
                &"… display limit reached"
                    .chars()
                    .take(width)
                    .map(|c| (c, Face::default()))
                    .collect::<Vec<_>>(),
                width,
                ui,
            ));
            break;
        }
    }
    if rows.is_empty() {
        rows.push(paint(&[], width, ui));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plain_rows(text: &str, width: u16) -> Vec<String> {
        rows(
            text,
            width,
            &Ui {
                color: false,
                ..Ui::default()
            },
        )
    }
    #[test]
    fn headings_lists_links_and_inline_styles_are_readable() {
        let text = plain_rows(
            "# Progress\n- **Done** and `cargo test`\n> *review*\n[report](https://example.test)",
            60,
        )
        .join("\n");
        assert!(text.contains("Progress") && !text.contains("# Progress"));
        assert!(text.contains("• Done and cargo test") && !text.contains("**"));
        assert!(text.contains("│ review"));
        assert!(text.contains("report (https://example.test)"));
        let styled = rows("**bold** `code`", 30, &Ui::default()).join("");
        assert!(styled.contains(";1m"));
        assert!(styled.contains(&Ui::default().color("accent")));
    }
    #[test]
    fn fenced_code_keeps_markers_and_terminal_controls_are_literal() {
        let text = plain_rows("```rust\n**literal**\n```\n\u{1b}[2J\u{202e}unsafe", 70).join("\n");
        assert!(text.contains("**literal**"));
        assert!(text.contains("\\x1b[2J") && text.contains("\\u{202e}"));
        assert!(!text.contains('\u{1b}'));
    }
    #[test]
    fn unicode_and_narrow_terminals_never_overflow() {
        for width in [1, 2, 4, 9, 40] {
            for line in plain_rows("- 中文🙂 **内容** e\u{301}", width) {
                assert!(unicode_width::UnicodeWidthStr::width(line.as_str()) <= usize::from(width));
            }
        }
    }
}
