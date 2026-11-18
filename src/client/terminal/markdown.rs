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
    syntax: Option<&'static str>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Plain,
    Diff,
    Rust,
    Javascript,
    Json,
    Python,
    Shell,
}

struct Quote {
    end: String,
    escapes: bool,
    multiline: bool,
}

struct Highlight {
    language: Language,
    block_depth: usize,
    quote: Option<Quote>,
}

struct Fence {
    marker: char,
    length: usize,
    highlight: Highlight,
}

impl Highlight {
    fn new(label: &str) -> Self {
        let language = match label
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "diff" | "patch" => Language::Diff,
            "rust" | "rs" => Language::Rust,
            "javascript" | "js" | "jsx" | "mjs" | "cjs" | "typescript" | "ts" | "tsx" => {
                Language::Javascript
            }
            "json" => Language::Json,
            "python" | "py" | "python3" => Language::Python,
            "shell" | "sh" | "bash" | "zsh" => Language::Shell,
            _ => Language::Plain,
        };
        Self {
            language,
            block_depth: 0,
            quote: None,
        }
    }

    fn keyword(&self, word: &str) -> bool {
        match self.language {
            Language::Rust => matches!(
                word,
                "as" | "async"
                    | "await"
                    | "break"
                    | "const"
                    | "continue"
                    | "crate"
                    | "dyn"
                    | "else"
                    | "enum"
                    | "extern"
                    | "false"
                    | "fn"
                    | "for"
                    | "if"
                    | "impl"
                    | "in"
                    | "let"
                    | "loop"
                    | "match"
                    | "mod"
                    | "move"
                    | "mut"
                    | "pub"
                    | "ref"
                    | "return"
                    | "self"
                    | "Self"
                    | "static"
                    | "struct"
                    | "super"
                    | "trait"
                    | "true"
                    | "type"
                    | "unsafe"
                    | "use"
                    | "where"
                    | "while"
            ),
            Language::Javascript => matches!(
                word,
                "abstract"
                    | "any"
                    | "as"
                    | "async"
                    | "await"
                    | "boolean"
                    | "break"
                    | "case"
                    | "catch"
                    | "class"
                    | "const"
                    | "continue"
                    | "declare"
                    | "default"
                    | "delete"
                    | "do"
                    | "else"
                    | "enum"
                    | "export"
                    | "extends"
                    | "false"
                    | "finally"
                    | "for"
                    | "from"
                    | "function"
                    | "if"
                    | "implements"
                    | "import"
                    | "in"
                    | "instanceof"
                    | "interface"
                    | "keyof"
                    | "let"
                    | "namespace"
                    | "never"
                    | "new"
                    | "null"
                    | "number"
                    | "of"
                    | "private"
                    | "protected"
                    | "public"
                    | "readonly"
                    | "return"
                    | "satisfies"
                    | "static"
                    | "string"
                    | "super"
                    | "switch"
                    | "this"
                    | "throw"
                    | "true"
                    | "try"
                    | "type"
                    | "typeof"
                    | "undefined"
                    | "unknown"
                    | "var"
                    | "void"
                    | "while"
                    | "yield"
            ),
            Language::Json => matches!(word, "true" | "false" | "null"),
            Language::Python => matches!(
                word,
                "and"
                    | "as"
                    | "assert"
                    | "async"
                    | "await"
                    | "break"
                    | "case"
                    | "class"
                    | "continue"
                    | "def"
                    | "del"
                    | "elif"
                    | "else"
                    | "except"
                    | "False"
                    | "finally"
                    | "for"
                    | "from"
                    | "global"
                    | "if"
                    | "import"
                    | "in"
                    | "is"
                    | "lambda"
                    | "match"
                    | "None"
                    | "nonlocal"
                    | "not"
                    | "or"
                    | "pass"
                    | "raise"
                    | "return"
                    | "True"
                    | "try"
                    | "while"
                    | "with"
                    | "yield"
            ),
            Language::Shell => matches!(
                word,
                "case"
                    | "cd"
                    | "declare"
                    | "do"
                    | "done"
                    | "echo"
                    | "elif"
                    | "else"
                    | "esac"
                    | "exec"
                    | "export"
                    | "false"
                    | "fi"
                    | "for"
                    | "function"
                    | "if"
                    | "in"
                    | "local"
                    | "printf"
                    | "read"
                    | "readonly"
                    | "select"
                    | "set"
                    | "source"
                    | "test"
                    | "then"
                    | "time"
                    | "trap"
                    | "true"
                    | "until"
                    | "unset"
                    | "while"
            ),
            Language::Plain | Language::Diff => false,
        }
    }

    fn quote_start(&self, text: &str) -> Option<(usize, Quote)> {
        if self.language == Language::Rust {
            let prefix = if text.starts_with("br") {
                2
            } else if text.starts_with('r') {
                1
            } else {
                0
            };
            if prefix != 0 {
                let hashes = text[prefix..].bytes().take_while(|c| *c == b'#').count();
                if text.as_bytes().get(prefix + hashes) == Some(&b'"') {
                    return Some((
                        prefix + hashes + 1,
                        Quote {
                            end: format!("\"{}", "#".repeat(hashes)),
                            escapes: false,
                            multiline: true,
                        },
                    ));
                }
            }
        }
        let character = text.chars().next()?;
        if !matches!(character, '\'' | '"' | '`')
            || (self.language == Language::Json && character != '"')
            || (character == '`' && self.language != Language::Javascript)
        {
            return None;
        }
        // Rust lifetimes such as 'a and 'static are identifiers, not strings.
        if self.language == Language::Rust && character == '\'' {
            let next = text[1..].chars().next()?;
            if next != '\\' && !text[1 + next.len_utf8()..].starts_with('\'') {
                return None;
            }
        }
        let triple = self.language == Language::Python
            && text
                .as_bytes()
                .iter()
                .take(3)
                .filter(|c| **c == character as u8)
                .count()
                == 3;
        let length = if triple { 3 } else { 1 };
        Some((
            length,
            Quote {
                end: character.to_string().repeat(length),
                escapes: !(self.language == Language::Shell && character == '\''),
                multiline: triple
                    || character == '`'
                    || matches!(self.language, Language::Rust | Language::Shell),
            },
        ))
    }

    fn block_end(&mut self, text: &str, mut at: usize) -> usize {
        while at < text.len() {
            if self.language == Language::Rust && text[at..].starts_with("/*") {
                self.block_depth += 1;
                at += 2;
            } else if text[at..].starts_with("*/") {
                self.block_depth -= 1;
                at += 2;
                if self.block_depth == 0 {
                    break;
                }
            } else {
                at += text[at..].chars().next().unwrap().len_utf8();
            }
        }
        at
    }

    fn line(&mut self, text: &str, out: &mut Vec<(char, Face)>) {
        if self.language == Language::Plain {
            plain(text, Face::default(), out);
            return;
        }
        if self.language == Language::Diff {
            let (tone, bold) = if text.starts_with("diff --git") {
                ("heading", true)
            } else if text.starts_with("@@") {
                ("accent", true)
            } else if text.starts_with('+') {
                ("success", false)
            } else if text.starts_with('-') {
                ("error", false)
            } else if text.starts_with("index ") || text.starts_with("\\ No newline") {
                ("muted", false)
            } else {
                ("foreground", false)
            };
            syntax(text, tone, bold, out);
            return;
        }
        let mut at = 0;
        while at < text.len() {
            let rest = &text[at..];
            if let Some(quote) = self.quote.take() {
                let (end, closed) = quoted_end(rest, &quote);
                syntax(&rest[..end], "success", false, out);
                if !closed && quote.multiline {
                    self.quote = Some(quote);
                }
                at += end;
                continue;
            }
            if self.block_depth != 0 {
                let end = self.block_end(rest, 0);
                syntax(&rest[..end], "muted", false, out);
                at += end;
                continue;
            }
            let c_comments = matches!(self.language, Language::Rust | Language::Javascript);
            let hash_comment = self.language == Language::Python
                || (self.language == Language::Shell
                    && text[..at]
                        .chars()
                        .next_back()
                        .is_none_or(|c| c.is_whitespace() || ";|&()".contains(c)));
            if (c_comments && rest.starts_with("//")) || (hash_comment && rest.starts_with('#')) {
                syntax(rest, "muted", false, out);
                break;
            }
            if c_comments && rest.starts_with("/*") {
                self.block_depth = 1;
                let end = self.block_end(rest, 2);
                syntax(&rest[..end], "muted", false, out);
                at += end;
                continue;
            }
            if let Some((opening, quote)) = self.quote_start(rest) {
                let (end, closed) = quoted_end(&rest[opening..], &quote);
                let end = opening + end;
                let color = if self.language == Language::Json
                    && closed
                    && rest[end..].trim_start().starts_with(':')
                {
                    "heading"
                } else {
                    "success"
                };
                syntax(&rest[..end], color, false, out);
                if !closed && quote.multiline {
                    self.quote = Some(quote);
                }
                at += end;
                continue;
            }
            if self.language == Language::Shell && rest.starts_with('$') {
                let end = if rest.as_bytes().starts_with(b"$\x7b") {
                    rest.find('}').map_or(rest.len(), |end| end + 1)
                } else {
                    let name = rest[1..]
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .map(char::len_utf8)
                        .sum::<usize>();
                    if name == 0 {
                        1 + rest[1..]
                            .chars()
                            .next()
                            .filter(|c| "?@*#!-$".contains(*c))
                            .map_or(0, char::len_utf8)
                    } else {
                        1 + name
                    }
                };
                syntax(&rest[..end], "accent", false, out);
                at += end;
                continue;
            }
            let first = rest.chars().next().unwrap();
            if first.is_ascii_digit() {
                let bytes = rest.as_bytes();
                let mut end = 1;
                while end < bytes.len() {
                    let byte = bytes[end];
                    let decimal =
                        byte == b'.' && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
                    let exponent_sign = matches!(byte, b'+' | b'-')
                        && matches!(bytes[end - 1], b'e' | b'E' | b'p' | b'P');
                    if !byte.is_ascii_alphanumeric() && byte != b'_' && !decimal && !exponent_sign {
                        break;
                    }
                    end += 1;
                }
                syntax(&rest[..end], "warning", false, out);
                at += end;
                continue;
            }
            if first.is_alphabetic() || first == '_' {
                let end = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .map(char::len_utf8)
                    .sum::<usize>();
                let word = &rest[..end];
                if self.keyword(word) {
                    syntax(word, "accent", true, out);
                } else {
                    plain(word, Face::default(), out);
                }
                at += end;
                continue;
            }
            out.push((first, Face::default()));
            at += first.len_utf8();
        }
    }
}

fn quoted_end(text: &str, quote: &Quote) -> (usize, bool) {
    let mut at = 0;
    while at < text.len() {
        if text[at..].starts_with(&quote.end) {
            return (at + quote.end.len(), true);
        }
        let character = text[at..].chars().next().unwrap();
        at += character.len_utf8();
        if quote.escapes
            && character == '\\'
            && let Some(escaped) = text[at..].chars().next()
        {
            at += escaped.len_utf8();
        }
    }
    (at, false)
}

fn syntax(text: &str, color: &'static str, bold: bool, out: &mut Vec<(char, Face)>) {
    plain(
        text,
        Face {
            syntax: Some(color),
            bold,
            ..Face::default()
        },
        out,
    );
}

fn paint(cells: &[(char, Face)], width: usize, ui: &Ui) -> String {
    let mut out = String::new();
    let mut previous = None;
    let mut used = 0;
    for &(c, face) in cells {
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
    let mut fence: Option<Fence> = None;
    for raw in text.lines() {
        let safe = panel::safe_text(&raw.replace('\t', "    "));
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
                    highlight: Highlight::new(label),
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
            fence.highlight.line(body, &mut cells);
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

    fn colored_screen(source: &str, width: u16) -> vt100::Screen {
        use std::num::NonZeroU16;
        let mut ui = Ui::default();
        ui.colors.accent = "#112233".into();
        ui.colors.success = "#445566".into();
        ui.colors.warning = "#778899".into();
        ui.colors.error = "#bb1122".into();
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
    fn diff_fences_preserve_markers_and_use_semantic_line_colors() {
        for label in ["diff", "patch"] {
            let body = "diff --git a/界 b/界\nindex 123..456 100644\n--- a/界\n+++ b/界\n@@ -1 +1 @@\n-old 界\n+new 界\n context\n\\ No newline at end of file";
            let source = format!("\u{60}\u{60}\u{60}{label}\n{body}\n\u{60}\u{60}\u{60}");
            let screen = colored_screen(&source, 100);
            for (token, color) in [
                ("diff --git", (204, 221, 238)),
                ("index 123", (153, 170, 187)),
                ("--- a/界", (187, 17, 34)),
                ("+++ b/界", (68, 85, 102)),
                ("@@ -1 +1 @@", (17, 34, 51)),
                ("-old 界", (187, 17, 34)),
                ("+new 界", (68, 85, 102)),
                ("context", (1, 2, 3)),
                ("\\ No newline", (153, 170, 187)),
            ] {
                assert_color(&screen, token, token, color);
            }
            let plain = plain_rows(&source, 100).join("\n");
            assert!(!plain.contains('\x1b'));
            for line in body.lines() {
                assert!(plain.contains(line), "{line}");
            }
        }
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
