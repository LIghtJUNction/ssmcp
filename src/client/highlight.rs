//! One display lexer shared by Markdown, source inspection and tool streams.
//! It retains source bytes and language state; it never adds escape sequences.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Token<'a> {
    pub text: &'a str,
    pub tone: &'static str,
    pub bold: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Language {
    Plain,
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

pub(super) struct Lexer {
    language: Language,
    block_depth: usize,
    quote: Option<Quote>,
}

impl Lexer {
    pub(super) fn for_language(label: &str) -> Self {
        let language = match label
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
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
            Language::Plain => false,
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

    pub(super) fn for_program(program: &str) -> Self {
        let name = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe")
            .to_ascii_lowercase();
        let language = if name.starts_with("python") || name.starts_with("pypy") {
            "python"
        } else {
            match name.as_str() {
                "node" | "nodejs" | "bun" | "deno" => "javascript",
                "sh" | "bash" | "zsh" | "dash" | "ash" | "ksh" | "fish" => "shell",
                name => name,
            }
        };
        Self::for_language(language)
    }

    /// Tokens borrow this logical line without escaping, trimming or changing
    /// source bytes. Renderers own terminal safety, widths and ANSI painting.
    pub(super) fn line<'a>(&mut self, text: &'a str) -> Vec<Token<'a>> {
        let mut tokens = Vec::new();
        self.lex(text, &mut tokens);
        tokens
    }

    fn lex<'a>(&mut self, text: &'a str, out: &mut Vec<Token<'a>>) {
        if self.language == Language::Plain {
            syntax(text, "foreground", false, out);
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
                    syntax(word, "foreground", false, out);
                }
                at += end;
                continue;
            }
            syntax(&rest[..first.len_utf8()], "foreground", false, out);
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

fn syntax<'a>(text: &'a str, tone: &'static str, bold: bool, out: &mut Vec<Token<'a>>) {
    if !text.is_empty() {
        out.push(Token { text, tone, bold });
    }
}
