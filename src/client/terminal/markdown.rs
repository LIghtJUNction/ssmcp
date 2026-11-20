//! Bounded, terminal-safe Markdown. Unsupported syntax remains readable.
use super::links;
use crate::client::{highlight::Lexer, panel, ui::Ui};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Face {
    bold: bool,
    italic: bool,
    code: bool,
    muted: bool,
    heading: bool,
    syntax: Option<&'static str>,
    link: Option<u16>,
}

fn plain(mut text: &str, mut face: Face, out: &mut Vec<(char, Face)>) -> Option<u16> {
    while !text.is_empty() {
        let (len, token) = links::token(text);
        match token {
            links::Token::Text(c) => out.push((c, face)),
            links::Token::Osc(payload) => {
                if let Some(link) = links::local_id(payload) {
                    face.link = link;
                }
            }
            _ => {}
        }
        text = &text[len..];
    }
    face.link
}

fn url_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (at, ch) in text.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(at),
            ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn inline(
    mut text: &str,
    mut face: Face,
    depth: u8,
    out: &mut Vec<(char, Face)>,
    targets: &mut links::Targets,
) -> Option<u16> {
    if depth > 4 {
        return plain(text, face, out);
    }
    let source = text;
    let bare = links::bare(source);
    let mut next_bare = 0;
    while !text.is_empty() {
        let offset = source.len() - text.len();
        while bare.get(next_bare).is_some_and(|range| range.end <= offset) {
            next_bare += 1;
        }
        if text.starts_with('\x1b') {
            let (len, token) = links::token(text);
            if let links::Token::Osc(payload) = token
                && let Some(link) = links::local_id(payload)
            {
                face.link = link;
            }
            text = &text[len..];
            continue;
        }
        if let Some(range) = bare.get(next_bare).filter(|range| range.start == offset) {
            let url = &source[range.clone()];
            let link = face.link.or_else(|| targets.insert(url));
            plain(url, Face { link, ..face }, out);
            text = &text[url.len()..];
            continue;
        }
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
                face.link = plain(&text[n..n + end], Face { code: true, ..face }, out);
                text = &text[n + end + n..];
                continue;
            }
        }
        if let Some(rest) = text.strip_prefix('[')
            && let Some(end) = rest.find("](")
            && let Some(close) = url_end(&rest[end + 2..])
        {
            let url = &rest[end + 2..end + 2 + close];
            let link = targets.insert(url).or(face.link);
            inline(
                &rest[..end],
                Face {
                    bold: true,
                    link,
                    ..face
                },
                depth + 1,
                out,
                targets,
            );
            if !url.is_empty() {
                plain(
                    &format!(" ({url})"),
                    Face {
                        muted: true,
                        link,
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
            let underscore = marker.starts_with('_');
            // Underscores inside identifiers cannot open emphasis. Use source
            // context rather than emitted cells, including within nested spans.
            let before = source[..source.len() - text.len()].chars().next_back();
            if underscore && before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let Some(rest) = text.strip_prefix(marker) else {
                continue;
            };
            let Some(end) = rest.match_indices(marker).map(|(end, _)| end).find(|&end| {
                end > 0
                    && (!underscore
                        || (rest.chars().next().is_some_and(|c| !c.is_whitespace())
                            && rest[..end]
                                .chars()
                                .next_back()
                                .is_some_and(|c| !c.is_whitespace())
                            && rest[end + marker.len()..]
                                .chars()
                                .next()
                                .is_none_or(|c| !c.is_alphanumeric() && c != '_')))
            }) else {
                continue;
            };
            let next = if marker.len() == 2 {
                Face { bold: true, ..face }
            } else {
                Face {
                    italic: true,
                    ..face
                }
            };
            face.link = inline(&rest[..end], next, depth + 1, out, targets);
            text = &rest[end + marker.len()..];
            matched = true;
            break;
        }
        if !matched {
            let c = text.chars().next().unwrap();
            out.push((c, face));
            text = &text[c.len_utf8()..];
        }
    }
    face.link
}

struct Fence {
    marker: char,
    length: usize,
    highlight: Lexer,
}

fn paint(cells: &[(char, Face)], width: usize, ui: &Ui) -> String {
    let mut out = String::new();
    let mut previous = None;
    let mut previous_link = None;
    let used = cells.iter().map(|(ch, _)| ch).collect::<String>().width();
    for &(c, face) in cells {
        if previous_link != face.link {
            out.push_str(
                &face
                    .link
                    .map(links::marker)
                    .unwrap_or_else(|| links::CLOSE.into()),
            );
            previous_link = face.link;
        }
        if ui.color && previous != Some(face) {
            let color = if let Some(color) = face.syntax {
                color
            } else if face.code {
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
    }
    if previous_link.is_some() {
        out.push_str(links::CLOSE);
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
    let prefix = if prefix.width() < available {
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
    let indent = margin.width() + prefix.width();
    let mut used = indent;
    let text: String = cells.iter().map(|(ch, _)| ch).collect();
    let mut at = 0;
    for cluster in text.graphemes(true) {
        let count = cluster.chars().count();
        let group = &cells[at..at + count];
        let mut face = group[0].1;
        if group.iter().any(|(_, other)| other.link != face.link) {
            face.link = None;
        }
        at += count;
        let cluster = if cluster.width() > width.saturating_sub(indent) {
            "…"
        } else {
            cluster
        };
        let size = cluster.width();
        if used + size > width && used > indent {
            out.push(paint(&row, width, ui));
            row = " "
                .repeat(indent)
                .chars()
                .map(|c| (c, Face::default()))
                .collect();
            used = indent;
        }
        // One style/target per visible cluster prevents SGR or Markdown marks
        // from splitting an emoji or giving its combining suffix another URI.
        row.extend(cluster.chars().map(|ch| (ch, face)));
        used += size;
    }
    out.push(paint(&row, width, ui));
}

/// Headings, emphasis, inline/fenced code, links, lists and block quotes.
/// Input escape sequences, URLs and HTML never execute terminal commands.
pub(crate) fn rows(text: &str, width: u16, ui: &Ui) -> Vec<String> {
    linked_rows(text, width, ui).plain()
}

pub(super) fn linked_rows(text: &str, width: u16, ui: &Ui) -> links::Rows {
    let source = links::Source::new(&safe_source(text), false);
    let marked = reopen_lines(&source.marked());
    let mut targets = source.targets;
    let text = marked.as_str();
    let width = usize::from(width.max(1));
    let mut rows = Vec::new();
    let mut fence: Option<Fence> = None;
    for raw in text.lines() {
        let safe = raw.replace('\t', "    ");
        let line = safe.trim_end();
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == marker).count();
        if matches!(marker, '`' | '~') && count >= 3 {
            if let Some(open) = fence.as_ref() {
                if marker == open.marker
                    && count >= open.length
                    && trimmed[count..].trim().is_empty()
                {
                    fence = None;
                    continue;
                }
            } else {
                let label = trimmed[count..].trim();
                fence = Some(Fence {
                    marker,
                    length: count,
                    highlight: Lexer::for_language(label),
                });
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
        if let Some(fence) = fence.as_mut() {
            plain(body, Face::default(), &mut cells);
            let text: String = cells.iter().map(|(ch, _)| ch).collect();
            let mut index = 0;
            for token in fence.highlight.line(&text) {
                for _ in token.text.chars() {
                    if let Some((_, face)) = cells.get_mut(index) {
                        face.syntax = Some(token.tone);
                        face.bold = token.bold;
                    }
                    index += 1;
                }
            }
        } else {
            inline(body, face, 0, &mut cells, &mut targets);
        }
        auto_links(&mut cells, &mut targets);
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
    links::Rows::new(rows, targets)
}

/// Preserve the existing literal-control/bidi and redaction policy for text.
/// OSC8 alone is parsed as metadata; other control-string payloads never become
/// visible text or automatic URLs. Only our generated SGR reaches the screen.
fn safe_source(raw: &str) -> String {
    fn string_control(text: &str) -> bool {
        text.starts_with(['\u{90}', '\u{98}', '\u{9d}', '\u{9e}', '\u{9f}'])
            || ["\x1b]", "\x1bP", "\x1b_", "\x1b^", "\x1bX"]
                .iter()
                .any(|prefix| text.starts_with(prefix))
    }
    fn flush(text: &mut String, out: &mut String) {
        for line in text.split_inclusive('\n') {
            let (body, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |body| (body, "\n"));
            out.push_str(&panel::safe_text(&body.replace('\t', "    ")));
            out.push_str(newline);
        }
        text.clear();
    }
    // Redaction must see complete visible context, even when OSC metadata
    // splits an Authorization header or another sensitive field. If it rewrites
    // text, discard the original link metadata instead of reusing old offsets.
    let mut text = raw;
    let mut visible = String::with_capacity(raw.len());
    while !text.is_empty() {
        let (len, token) = links::token(text);
        if !matches!(token, links::Token::Osc(_)) && !string_control(text) {
            visible.push_str(&text[..len]);
        }
        text = &text[len..];
    }
    let redacted = crate::audit::event(serde_json::Value::String(visible.clone()));
    if redacted.as_str() != Some(visible.as_str()) {
        let mut out = String::new();
        flush(
            &mut redacted.as_str().unwrap_or("[redacted]").to_owned(),
            &mut out,
        );
        return out;
    }
    let mut text = raw;
    let mut out = String::with_capacity(text.len());
    let mut literal = String::new();
    while !text.is_empty() {
        let (len, token) = links::token(text);
        match token {
            links::Token::Osc(payload) => {
                flush(&mut literal, &mut out);
                if links::osc_uri(payload).is_some() {
                    out.push_str(&text[..len]);
                }
            }
            links::Token::Ignore if string_control(text) => {}
            _ => literal.push_str(&text[..len]),
        }
        text = &text[len..];
    }
    flush(&mut literal, &mut out);
    out
}

/// OSC8 is cell state, so a hard newline must reopen the same local ID before
/// a separate Markdown line is parsed. Full targets remain in one bounded table.
fn reopen_lines(mut text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut link = None;
    while !text.is_empty() {
        let (len, token) = links::token(text);
        match token {
            links::Token::Osc(payload) => {
                if let Some(id) = links::local_id(payload) {
                    link = id;
                }
                out.push_str(&text[..len]);
            }
            links::Token::Text('\n') if link.is_some() => {
                out.push_str(links::CLOSE);
                out.push('\n');
                out.push_str(&links::marker(link.unwrap()));
            }
            _ => out.push_str(&text[..len]),
        }
        text = &text[len..];
    }
    out
}

fn auto_links(cells: &mut [(char, Face)], targets: &mut links::Targets) {
    let text: String = cells.iter().map(|(ch, _)| ch).collect();
    let positions: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
    for range in links::bare(&text) {
        let start = positions.partition_point(|at| *at < range.start);
        let end = positions.partition_point(|at| *at < range.end);
        if cells[start..end]
            .iter()
            .any(|(_, face)| face.link.is_some())
        {
            continue;
        }
        if let Some(id) = targets.insert(&text[range]) {
            for (_, face) in &mut cells[start..end] {
                face.link = Some(id);
            }
        }
    }
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

    fn colored_screen(source: &str, width: u16) -> vt100::Screen {
        use std::num::NonZeroU16;
        let mut ui = Ui::default();
        ui.colors.accent = "#112233".into();
        ui.colors.success = "#445566".into();
        ui.colors.warning = "#778899".into();
        ui.colors.muted = "#99aabb".into();
        ui.colors.heading = "#ccddee".into();
        ui.colors.foreground = "#010203".into();
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(40).unwrap(),
            NonZeroU16::new(width).unwrap(),
            0,
        );
        parser.process(rows(source, width, &ui).join("\r\n").as_bytes());
        parser.screen().clone()
    }

    fn assert_color(screen: &vt100::Screen, context: &str, token: &str, color: (u8, u8, u8)) {
        let (row, line) = screen
            .rows(0, screen.size().1.get())
            .enumerate()
            .find(|(_, line)| line.contains(context))
            .unwrap_or_else(|| panic!("missing {context}: {}", screen.contents()));
        let start = line.find(token).unwrap();
        let column = unicode_width::UnicodeWidthStr::width(&line[..start]);
        assert_eq!(
            screen.cell(row as u16, column as u16).unwrap().fgcolor(),
            vt100::Color::Rgb(color.0, color.1, color.2),
            "{context}: {token}"
        );
    }

    #[test]
    fn recognized_fences_color_actual_language_tokens() {
        for (language, code, keyword, string, comment) in [
            (
                "rust",
                r#"let n = 42; let s = "界"; // note"#,
                "let",
                "\"界\"",
                Some("// note"),
            ),
            (
                "js",
                r#"const n = 42; let s = "界"; // note"#,
                "const",
                "\"界\"",
                Some("// note"),
            ),
            (
                "typescript",
                r#"let n: number = 42; let s = "界"; // note"#,
                "number",
                "\"界\"",
                Some("// note"),
            ),
            (
                "json",
                r#"{ "count": 42, "name": "界", "active": true, "escape": "\t" }"#,
                "true",
                "\"界\"",
                None,
            ),
            (
                "python",
                "n = 42 if True else '界' # note",
                "if",
                "'界'",
                Some("# note"),
            ),
            (
                "bash",
                "if test $COUNT -gt 42; then echo '界'; fi # note",
                "if",
                "'界'",
                Some("# note"),
            ),
        ] {
            let source = format!("```{language}\n{code}\n```");
            let screen = colored_screen(&source, 100);
            assert_color(&screen, code, keyword, (17, 34, 51));
            assert_color(&screen, code, string, (68, 85, 102));
            assert_color(&screen, code, "42", (119, 136, 153));
            if let Some(comment) = comment {
                assert_color(&screen, code, comment, (153, 170, 187));
            }
            if language == "json" {
                assert_color(&screen, code, "\"count\"", (204, 221, 238));
            }
            if language == "bash" {
                assert_color(&screen, code, "$COUNT", (17, 34, 51));
            }
            let plain = plain_rows(&source, 100).join("\n");
            assert!(plain.contains(code), "{plain}");
            assert!(!plain.contains('\x1b'));
        }
    }

    #[test]
    fn code_state_handles_multiline_literals_comments_and_unknown_languages() {
        let source = concat!(
            "```rust\n/* outer\n/* nested */ still comment\n",
            "*/ let s = r#\"// literal\"#;\nlet borrow: &'a str = \"ok\";\n```\n",
            "```python\ns = \"\"\"# literal\nstill string\"\"\" # comment\n```\n",
            "```js\nconst s = \"escaped \\\" // string\"; // real comment\n```\n",
            "```unknown\nlet n = 42; // neutral\n```",
        );
        let screen = colored_screen(source, 100);
        assert_color(&screen, "still comment", "still comment", (153, 170, 187));
        assert_color(&screen, "*/ let", "let", (17, 34, 51));
        assert_color(&screen, "// literal", "// literal", (68, 85, 102));
        assert_color(&screen, "let borrow", "borrow", (1, 2, 3));
        assert_color(&screen, "let borrow", "\"ok\"", (68, 85, 102));
        assert_color(&screen, "still string", "still string", (68, 85, 102));
        assert_color(&screen, "still string", "# comment", (153, 170, 187));
        assert_color(&screen, "escaped", "// string", (68, 85, 102));
        assert_color(&screen, "escaped", "// real comment", (153, 170, 187));
        assert_color(&screen, "neutral", "let", (1, 2, 3));
        assert_color(&screen, "neutral", "42", (1, 2, 3));
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
    fn intraword_underscores_stay_literal_while_word_emphasis_renders() {
        for identifier in [
            "DETAIL_OPTIONS_FIRST",
            "NAME__PART__VALUE",
            "snake_case_name",
            "中文_选项_末尾",
        ] {
            let plain = plain_rows(identifier, 80).join("\n");
            assert!(plain.contains(identifier), "{plain}");
            assert!(
                colored_screen(identifier, 80)
                    .contents()
                    .contains(identifier)
            );
        }
        let source = "_emphasis_ __strong__ _snake_case_ (__nested_word__)";
        let plain = plain_rows(source, 80).join("\n");
        assert!(
            plain.contains("emphasis strong snake_case (nested_word)"),
            "{plain}"
        );
        let screen = colored_screen(source, 80);
        for (token, italic, bold) in [
            ("emphasis", true, false),
            ("strong", false, true),
            ("snake_case", true, false),
            ("nested_word", false, true),
        ] {
            let (row, line) = screen
                .rows(0, screen.size().1.get())
                .enumerate()
                .find(|(_, line)| line.contains(token))
                .unwrap_or_else(|| panic!("missing {token}: {}", screen.contents()));
            let start = line.find(token).unwrap();
            let column = unicode_width::UnicodeWidthStr::width(&line[..start]);
            for column in column..column + unicode_width::UnicodeWidthStr::width(token) {
                let cell = screen.cell(row as u16, column as u16).unwrap();
                assert_eq!(cell.italic(), italic, "{token}");
                assert_eq!(cell.bold(), bold, "{token}");
            }
        }
    }

    #[test]
    fn fenced_code_keeps_markers_and_terminal_controls_are_literal() {
        let source = "```rust\n**literal**\n\tlet s = \"\u{1b}[2J\u{202e}unsafe\";\n```";
        let text = plain_rows(source, 70).join("\n");
        assert!(text.contains("**literal**"));
        assert!(text.contains("    let s ="));
        assert!(text.contains("\\x1b[2J") && text.contains("\\u{202e}"));
        assert!(!text.contains('\u{1b}'));
        let visible = colored_screen(source, 70).contents();
        assert!(visible.contains("\\x1b[2J") && visible.contains("\\u{202e}"));
    }
    #[test]
    fn unicode_and_narrow_terminals_never_overflow() {
        for width in [1, 2, 4, 9, 40] {
            for source in [
                "- 中文🙂 **内容** e\u{301}",
                "```python\nprint('中文🙂 e\u{301} \\x1b[2J') # 界\n```",
            ] {
                for line in plain_rows(source, width) {
                    assert!(
                        unicode_width::UnicodeWidthStr::width(line.as_str()) <= usize::from(width)
                    );
                    assert!(!line.contains('\x1b'));
                }
            }
        }
    }
}
