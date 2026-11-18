//! Small, deterministic display lexer. Spans retain source bytes; callers own
//! terminal escaping and wrapping. This never parses or executes tool output.

pub(super) type Spans = Vec<(String, &'static str)>;

#[derive(Clone, Copy, Debug)]
struct Quote {
    mark: char,
    triple: bool,
}

#[derive(Clone, Debug)]
pub(super) struct Lexer {
    block_depth: usize,
    quote: Option<Quote>,
    escaped: bool,
    hash_comments: bool,
    shell: bool,
    triple_quotes: bool,
    nested_comments: bool,
    c_comments: bool,
}

impl Default for Lexer {
    fn default() -> Self {
        Self {
            block_depth: 0,
            quote: None,
            escaped: false,
            hash_comments: false,
            shell: false,
            triple_quotes: false,
            nested_comments: false,
            c_comments: true,
        }
    }
}

impl Lexer {
    pub(super) fn for_language(language: &str) -> Option<Self> {
        let language = language.trim().to_ascii_lowercase();
        let mut lexer = Self::default();
        match language.as_str() {
            "js" | "javascript" | "jsx" | "ts" | "typescript" | "tsx" | "node" | "nodejs"
            | "bun" | "json" | "jsonc" | "c" | "cpp" | "c++" | "java" | "go" | "css" | "swift"
            | "kotlin" => {}
            "rs" | "rust" => lexer.nested_comments = true,
            "py" | "python" | "python3" | "pypy" | "pypy3" => {
                lexer.hash_comments = true;
                lexer.triple_quotes = true;
                lexer.c_comments = false;
            }
            "sh" | "shell" | "bash" | "zsh" | "fish" | "dash" | "ash" | "ksh" => {
                lexer.hash_comments = true;
                lexer.shell = true;
                lexer.c_comments = false;
            }
            "toml" | "yaml" | "yml" | "ruby" | "rb" | "perl" => {
                lexer.hash_comments = true;
                lexer.c_comments = false;
            }
            _ => return None,
        }
        Some(lexer)
    }

    pub(super) fn for_program(program: &str) -> Self {
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe");
        let language = if name.starts_with("python") || name.starts_with("pypy") {
            "python"
        } else {
            name
        };
        Self::for_language(language).unwrap_or_default()
    }

    pub(super) fn line(&mut self, line: &str) -> Spans {
        let mut spans = Vec::new();
        let mut index = 0;
        while index < line.len() {
            let tail = &line[index..];
            if self.block_depth > 0 {
                let mut length = 0;
                while length < tail.len() {
                    let rest = &tail[length..];
                    if rest.starts_with("*/") {
                        self.block_depth -= 1;
                        length += 2;
                        if self.block_depth == 0 {
                            break;
                        }
                    } else if self.nested_comments && rest.starts_with("/*") {
                        self.block_depth += 1;
                        length += 2;
                    } else {
                        length += rest.chars().next().unwrap().len_utf8();
                    }
                }
                push(&mut spans, &tail[..length], "muted");
                index += length;
                continue;
            }
            if let Some(quote) = self.quote {
                let mut length = 0;
                while length < tail.len() {
                    let rest = &tail[length..];
                    let ch = rest.chars().next().unwrap();
                    length += ch.len_utf8();
                    if self.escaped {
                        self.escaped = false;
                    } else if ch == '\\' && !(self.shell && quote.mark == '\'') {
                        self.escaped = true;
                    } else if ch == quote.mark
                        && (!quote.triple
                            || rest.chars().take(3).filter(|ch| *ch == quote.mark).count() == 3)
                    {
                        if quote.triple {
                            length += 2;
                        }
                        self.quote = None;
                        break;
                    }
                }
                push(&mut spans, &tail[..length], "success");
                index += length;
                continue;
            }
            let hash_comment = tail.starts_with('#')
                && self.hash_comments
                && (!self.shell
                    || index == 0
                    || line[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace));
            if (self.c_comments && tail.starts_with("//")) || hash_comment {
                push(&mut spans, tail, "muted");
                break;
            }
            if self.c_comments && tail.starts_with("/*") {
                self.block_depth = 1;
                push(&mut spans, "/*", "muted");
                index += 2;
                continue;
            }
            let ch = tail.chars().next().unwrap();
            if matches!(ch, '\'' | '"' | '\u{60}') {
                let triple = self.triple_quotes
                    && ch != '\u{60}'
                    && tail.chars().take(3).filter(|next| *next == ch).count() == 3;
                let length = if triple { 3 } else { 1 };
                self.quote = Some(Quote { mark: ch, triple });
                push(&mut spans, &tail[..length], "success");
                index += length;
                continue;
            }
            if ch.is_ascii_digit()
                || (ch == '.' && tail.as_bytes().get(1).is_some_and(u8::is_ascii_digit))
            {
                let length = number_len(tail);
                push(&mut spans, &tail[..length], "warning");
                index += length;
                continue;
            }
            if ch.is_alphabetic() || matches!(ch, '_' | '$') {
                let length: usize = tail
                    .chars()
                    .take_while(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '$'))
                    .map(char::len_utf8)
                    .sum();
                let word = &tail[..length];
                let tone = if matches!(
                    word,
                    "const"
                        | "let"
                        | "var"
                        | "await"
                        | "async"
                        | "return"
                        | "if"
                        | "else"
                        | "for"
                        | "while"
                        | "of"
                        | "in"
                        | "new"
                        | "function"
                        | "try"
                        | "catch"
                        | "finally"
                        | "throw"
                        | "class"
                        | "extends"
                        | "import"
                        | "export"
                        | "from"
                        | "switch"
                        | "case"
                        | "break"
                        | "continue"
                        | "typeof"
                        | "instanceof"
                        | "delete"
                        | "yield"
                        | "do"
                        | "default"
                        | "void"
                        | "static"
                        | "this"
                        | "super"
                        | "interface"
                        | "type"
                        | "as"
                        | "implements"
                        | "private"
                        | "public"
                        | "protected"
                        | "readonly"
                        | "def"
                        | "elif"
                        | "except"
                        | "raise"
                        | "with"
                        | "lambda"
                        | "pass"
                        | "and"
                        | "or"
                        | "not"
                        | "is"
                        | "assert"
                        | "global"
                        | "nonlocal"
                        | "del"
                        | "fn"
                        | "pub"
                        | "use"
                        | "mod"
                        | "impl"
                        | "trait"
                        | "struct"
                        | "enum"
                        | "match"
                        | "mut"
                        | "ref"
                        | "move"
                        | "where"
                        | "loop"
                        | "unsafe"
                        | "extern"
                        | "self"
                        | "Self"
                        | "then"
                        | "fi"
                        | "done"
                        | "esac"
                ) {
                    "accent"
                } else if matches!(
                    word,
                    "true" | "false" | "null" | "undefined" | "True" | "False" | "None"
                ) {
                    "warning"
                } else if tail[length..].trim_start().starts_with('(') {
                    "purple"
                } else {
                    "foreground"
                };
                push(&mut spans, word, tone);
                index += length;
                continue;
            }
            push(&mut spans, &tail[..ch.len_utf8()], "foreground");
            index += ch.len_utf8();
        }
        // A backslash before a physical newline escapes that newline, not the
        // first character on the next line. Templates/triple strings persist.
        if self
            .quote
            .is_some_and(|quote| quote.mark != '\u{60}' && !quote.triple)
            && !self.shell
            && !self.escaped
        {
            self.quote = None;
        }
        self.escaped = false;
        spans
    }
}

fn push(spans: &mut Spans, text: &str, tone: &'static str) {
    if let Some((previous, _)) = spans.last_mut().filter(|(_, previous)| *previous == tone) {
        previous.push_str(text);
    } else {
        spans.push((text.to_owned(), tone));
    }
}

fn number_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut end = 0;
    let radix = match bytes.get(..2) {
        Some(b"0x" | b"0X") => Some(16),
        Some(b"0o" | b"0O") => Some(8),
        Some(b"0b" | b"0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        end = 2;
        while bytes
            .get(end)
            .is_some_and(|byte| *byte == b'_' || char::from(*byte).is_digit(radix))
        {
            end += 1;
        }
    } else {
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
        {
            end += 1;
        }
        if bytes.get(end) == Some(&b'.') && bytes.get(end + 1) != Some(&b'.') {
            end += 1;
            while bytes
                .get(end)
                .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
            {
                end += 1;
            }
        }
        if matches!(bytes.get(end), Some(b'e' | b'E')) {
            let mut exponent = end + 1;
            if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
                exponent += 1;
            }
            if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
                end = exponent + 1;
                while bytes
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'_')
                {
                    end += 1;
                }
            }
        }
    }
    if bytes.get(end) == Some(&b'n') {
        end += 1;
    }
    end.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &Spans) -> String {
        spans.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn tokens_keep_unicode_and_numeric_boundaries() {
        let source = "const 数值 = [1e-3, 0xffn, .25, 42]; call('值'); // 注释";
        let spans = Lexer::default().line(source);
        assert_eq!(text(&spans), source);
        for (text, tone) in [
            ("const", "accent"),
            ("1e-3", "warning"),
            ("0xffn", "warning"),
            (".25", "warning"),
            ("42", "warning"),
            ("call", "purple"),
            ("'值'", "success"),
            ("// 注释", "muted"),
        ] {
            assert!(
                spans.iter().any(|span| span == &(text.into(), tone)),
                "{spans:?}"
            );
        }
    }

    #[test]
    fn comments_and_template_escapes_continue_across_lines() {
        let mut lexer = Lexer::default();
        assert_eq!(lexer.line("/* 注释"), vec![("/* 注释".into(), "muted")]);
        let tail = lexer.line("still */ const n = 1;");
        assert!(tail.contains(&("still */".into(), "muted")));
        assert!(tail.contains(&("const".into(), "accent")));
        let first = "const x = \u{60}line\\";
        assert_eq!(text(&lexer.line(first)), first);
        let next = lexer.line("\u{60}; return 2;");
        assert!(next.contains(&("\u{60}".into(), "success")));
        assert!(next.contains(&("return".into(), "accent")));
        let escaped = r#"const x = "say \"hi\""; // tail"#;
        let spans = lexer.line(escaped);
        assert_eq!(text(&spans), escaped);
        assert!(spans.contains(&(r#""say \"hi\"""#.into(), "success")));
        assert!(spans.contains(&("// tail".into(), "muted")));
    }

    #[test]
    fn python_triple_strings_and_rust_nested_comments_keep_state() {
        let mut python = Lexer::for_program("/usr/bin/python3.13");
        assert_eq!(python.line("'''你好"), vec![("'''你好".into(), "success")]);
        let spans = python.line("text''' # comment");
        assert_eq!(text(&spans), "text''' # comment");
        assert!(spans.contains(&("text'''".into(), "success")));
        assert!(spans.contains(&("# comment".into(), "muted")));
        let mut rust = Lexer::for_language("rust").unwrap();
        assert_eq!(
            rust.line("/* one /* two */"),
            vec![("/* one /* two */".into(), "muted")]
        );
        let spans = rust.line("end */ let x = 4;");
        assert!(spans.contains(&("end */".into(), "muted")));
        assert!(spans.contains(&("let".into(), "accent")));
    }
}
