//! User-facing agent messages delivered through native tools into this session.
use super::{panel, render, terminal, ui::Ui};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_TEXT: usize = 16 * 1024;
const MAX_MESSAGES: usize = 64;
const MAX_HISTORY_BYTES: usize = 256 * 1024;
static HISTORY: OnceLock<Mutex<History>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    #[default]
    Progress,
    Answer,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    text: String,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

impl Request {
    fn validate(&self) -> Result<(), String> {
        if self.text.trim().is_empty() || self.text.len() > MAX_TEXT {
            return Err("message text must contain 1–16384 UTF-8 bytes".into());
        }
        if self
            .text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
        {
            return Err("message text must not contain terminal control characters".into());
        }
        if let Some(author) = &self.author
            && (author.trim().is_empty()
                || author.len() > 80
                || author.chars().any(char::is_control))
        {
            return Err("author must contain 1–80 UTF-8 bytes without controls".into());
        }
        if let Some(key) = &self.idempotency_key
            && (key.is_empty()
                || key.len() > 96
                || !key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-')))
        {
            return Err(
                "idempotency_key must contain 1–96 ASCII letters, digits, '_' or '-'".into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Serialize)]
struct Message {
    id: String,
    kind: Kind,
    author: String,
    text: String,
    created_at_unix_ms: u64,
    #[serde(skip)]
    idempotency_key: Option<String>,
}

impl Message {
    fn retained_bytes(&self) -> usize {
        self.text.len()
            + self.author.len()
            + self.idempotency_key.as_ref().map_or(0, String::len)
            + 256
    }
}

#[derive(Default)]
struct History {
    messages: VecDeque<Message>,
    bytes: usize,
    sequence: u64,
}
impl History {
    fn record(&mut self, request: Request) -> Result<(Message, bool), String> {
        request.validate()?;
        let author = request.author.unwrap_or_else(|| "Agent".into());
        if let Some(key) = &request.idempotency_key
            && let Some(prior) = self
                .messages
                .iter()
                .find(|m| m.idempotency_key.as_ref() == Some(key))
        {
            if prior.kind == request.kind && prior.text == request.text && prior.author == author {
                return Ok((prior.clone(), true));
            }
            return Err("idempotency_key already identifies a different message".into());
        }
        self.sequence = self.sequence.wrapping_add(1);
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let message = Message {
            id: format!("message-{created_at_unix_ms:x}-{:x}", self.sequence),
            kind: request.kind,
            author,
            text: request.text,
            created_at_unix_ms,
            idempotency_key: request.idempotency_key,
        };
        let bytes = message.retained_bytes();
        while self.messages.len() >= MAX_MESSAGES
            || self.bytes.saturating_add(bytes) > MAX_HISTORY_BYTES
        {
            let Some(old) = self.messages.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(old.retained_bytes());
        }
        self.bytes += bytes;
        self.messages.push_back(message.clone());
        Ok((message, false))
    }
}

fn rendered(message: &Message, ui: &Ui, color: bool) -> String {
    let (label, semantic) = match message.kind {
        Kind::Progress => ("UPDATE", "blue"),
        Kind::Answer => ("ANSWER", "success"),
    };
    let heading = format!("[{label}] {}", panel::safe_text(&message.author));
    let mut output = if color {
        format!("\n\x1b[1;{}m{heading}\x1b[0m\n", ui.color(semantic))
    } else {
        format!("\n{heading}\n")
    };
    output.push_str(&render::message_body(&message.text, ui, color));
    output.push('\n');
    output
}

pub(crate) fn send(arguments: Value) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "message requires text and accepts kind, author and idempotency_key")?;
    let (message, duplicate) = HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .record(request)?;
    let active = terminal::active();
    if !duplicate {
        let ui = render::current_ui();
        let color = ui.color
            && std::env::var_os("NO_COLOR").is_none()
            && (active || io::stderr().is_terminal());
        let output = rendered(&message, &ui, color);
        if !terminal::log(output.as_bytes()) {
            io::stderr()
                .lock()
                .write_all(output.as_bytes())
                .map_err(|_| "message was recorded, but writing the client display failed")?;
        }
    }
    Ok(json!({
        "accepted":true, "message_id":message.id, "kind":message.kind,
        "duplicate":duplicate, "terminal_active":active,
        "text_bytes":message.text.len(),
        "note":"Recorded in the client conversation; does not assert that the user read it."
    }))
}

pub(super) fn last_text() -> Option<String> {
    HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages
        .back()
        .map(|message| message.text.clone())
}

pub(crate) fn read(arguments: Value) -> Result<Value, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Read {
        #[serde(default)]
        offset: usize,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        8
    }
    let args: Read =
        serde_json::from_value(arguments).map_err(|_| "messages accepts offset and limit")?;
    if !(1..=32).contains(&args.limit) {
        return Err("message history limit must be in 1–32".into());
    }
    let history = HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let messages: Vec<_> = history
        .messages
        .iter()
        .skip(args.offset)
        .take(args.limit)
        .cloned()
        .collect();
    let next = args.offset.saturating_add(messages.len());
    Ok(
        json!({"schema_version":1,"total":history.messages.len(),"messages":messages,
        "next_offset":if next < history.messages.len() {Some(next)} else {None},
        "retention":"current client process, at most 64 messages / 256 KiB"}),
    )
}

pub(crate) fn tools() -> Vec<Value> {
    vec![
        json!({
            "name":"ssmcp.ui.message",
            "description":"Deliver text addressed to the human into this client's conversation. Use for every user-facing progress update, explanation, question and final answer; code_mode say(clientId,{text,kind}) is the shortcut. kind is progress or answer. This is separate from the user's input queue and does not launch a model. Preserve Markdown/plain text and links in text. Optional author is caller-reported display attribution. Optional idempotency_key prevents duplicate retries within retained history. Acceptance records a message; it does not prove the user read it. Messages remain in bounded local history; ssmcp.ui.messages reads them.",
            "inputSchema":{"type":"object","required":["text"],"additionalProperties":false,"properties":{
                "text":{"type":"string","minLength":1,"maxLength":MAX_TEXT,"description":"Complete user-facing text, at most 16384 UTF-8 bytes; use separate messages for longer answers."},
                "kind":{"type":"string","enum":["progress","answer"],"default":"progress"},
                "author":{"type":"string","minLength":1,"maxLength":80},
                "idempotency_key":{"type":"string","minLength":1,"maxLength":96,"pattern":"^[A-Za-z0-9_-]+$"}
            }},
            "annotations":{"readOnlyHint":false,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.ui.messages",
            "description":"Read retained user-facing agent messages in this client process, oldest first, without claiming the user's request queue. History is bounded to 64 messages and 256 KiB. Use a small limit to avoid oversized native results; offsets count messages.",
            "inputSchema":{"type":"object","additionalProperties":false,"properties":{
                "offset":{"type":"integer","minimum":0,"default":0},
                "limit":{"type":"integer","minimum":1,"maximum":32,"default":8}
            }},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(text: &str, key: Option<&str>) -> Request {
        Request {
            text: text.into(),
            kind: Kind::Progress,
            author: None,
            idempotency_key: key.map(str::to_owned),
        }
    }
    #[test]
    fn agent_reply_code_and_diff_share_highlighting_without_changing_copy_text() {
        let text = "说明\n\u{0060}\u{0060}\u{0060}js\nconst 界 = 42; // note\n\u{0060}\u{0060}\u{0060}\n\u{0060}\u{0060}\u{0060}diff\n--- a/demo\n+++ b/demo\n@@ -1 +1 @@\n-old\n+new\n\u{0060}\u{0060}\u{0060}\nhttps://example.test/path";
        let mut history = History::default();
        let (message, _) = history.record(request(text, None)).unwrap();
        let ui = Ui::default();
        let plain = rendered(&message, &ui, false);
        let colored = rendered(&message, &ui, true);
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("const 界 = 42; // note"));
        assert!(colored.contains("mconst\x1b[0m"));
        for expected in [
            "--- a/demo",
            "+++ b/demo",
            "@@ -1 +1 @@",
            "-old",
            "+new",
            "https://example.test/path",
        ] {
            assert!(plain.contains(expected));
            assert!(colored.contains(expected));
        }
        assert_eq!(message.text, text);
        assert_eq!(history.messages.back().unwrap().text, text);
    }

    #[test]
    fn retry_is_idempotent_but_cannot_rewrite_an_answer() {
        let mut history = History::default();
        let (first, duplicate) = history
            .record(request("工作已完成", Some("finish")))
            .unwrap();
        assert!(!duplicate);
        let (same, duplicate) = history
            .record(request("工作已完成", Some("finish")))
            .unwrap();
        assert!(duplicate);
        assert_eq!(first.id, same.id);
        assert_eq!(history.messages.len(), 1);
        assert!(
            history
                .record(request("different", Some("finish")))
                .is_err()
        );
    }
    #[test]
    fn message_size_controls_and_retention_are_bounded() {
        let mut history = History::default();
        assert!(history.record(request("\x1b[2J", None)).is_err());
        assert!(
            history
                .record(request(&"中".repeat(MAX_TEXT / 3 + 1), None))
                .is_err()
        );
        for _ in 0..100 {
            history
                .record(request(&"a".repeat(MAX_TEXT), None))
                .unwrap();
        }
        assert!(history.messages.len() <= MAX_MESSAGES);
        assert!(history.bytes <= MAX_HISTORY_BYTES);
        assert!(history.messages.len() < 100);
    }
}
