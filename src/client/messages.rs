//! User-facing agent messages delivered through native tools into this session.
use super::{panel, presentation::Presentation, render, terminal, ui::Ui};
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
pub(crate) enum Kind {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    presentation: Option<Presentation>,
    created_at_unix_ms: u64,
    #[serde(skip)]
    idempotency_key: Option<String>,
    #[serde(skip)]
    history_saved: bool,
}

impl Message {
    fn retained_bytes(&self) -> usize {
        self.text.len()
            + self.author.len()
            + self.idempotency_key.as_ref().map_or(0, String::len)
            + self.presentation.as_ref().map_or(0, |value| {
                serde_json::to_vec(value).map_or(MAX_HISTORY_BYTES + 1, |bytes| bytes.len())
            })
            + 256
    }
}

#[derive(Default)]
struct History {
    messages: VecDeque<Message>,
    bytes: usize,
    last_copy: Option<String>,
}
impl History {
    fn record(&mut self, request: Request) -> Result<(Message, bool), String> {
        self.record_presentation(request, None)
    }

    fn record_presentation(
        &mut self,
        request: Request,
        presentation: Option<Presentation>,
    ) -> Result<(Message, bool), String> {
        request.validate()?;
        let author = request.author.unwrap_or_else(|| "Agent".into());
        if let Some(key) = &request.idempotency_key
            && let Some(prior) = self
                .messages
                .iter()
                .find(|m| m.idempotency_key.as_ref() == Some(key))
        {
            if prior.kind == request.kind
                && prior.text == request.text
                && prior.author == author
                && prior.presentation == presentation
            {
                return Ok((prior.clone(), true));
            }
            return Err("idempotency_key already identifies a different message".into());
        }
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        let message = Message {
            id: message_id()?,
            kind: request.kind,
            author,
            text: request.text,
            presentation,
            created_at_unix_ms,
            idempotency_key: request.idempotency_key,
            history_saved: false,
        };
        let bytes = message.retained_bytes();
        if bytes > MAX_HISTORY_BYTES {
            return Err("message and presentation exceed the 256 KiB history budget".into());
        }
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

    fn remember_last(&mut self, text: &str) {
        self.last_copy = (text.len() <= MAX_TEXT).then(|| text.to_owned());
    }

    fn restore_copy(&mut self, messages: &[super::interface::history::Message]) {
        self.last_copy = messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .filter(|message| message.content.len() <= MAX_TEXT)
            .map(|message| message.content.clone());
    }
}

fn message_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|_| "cannot create message identity")?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("message_{suffix}"))
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
    if let Some(presentation) = &message.presentation {
        output.push_str(&presentation.render(render::terminal_width(), ui, color));
    } else {
        output.push_str(&render::message_body(&message.text, ui, color));
    }
    output.push('\n');
    output
}

pub(crate) fn send(arguments: Value) -> Result<Value, String> {
    send_with_presentation(arguments, None)
}

pub(crate) fn send_presentation(
    text_arguments: Value,
    presentation: Presentation,
) -> Result<Value, String> {
    send_with_presentation(text_arguments, Some(presentation))
}

fn send_with_presentation(
    arguments: Value,
    presentation: Option<Presentation>,
) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "message requires text and accepts kind, author and idempotency_key")?;
    let (mut message, duplicate) = {
        let mut history = HISTORY
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match presentation {
            Some(presentation) => history.record_presentation(request, Some(presentation))?,
            None => history.record(request)?,
        }
    };
    let active = terminal::active();
    if !duplicate {
        message.history_saved = if message.presentation.is_none() && message.author == "Agent" {
            render::message_identified(&message.text, message.kind, &message.id).is_ok()
        } else {
            let saved = super::interface::history::record_agent(&message.id, &message.text);
            if let Err(error) = &saved {
                terminal::notice(&format!(
                    "Message recorded; history could not be saved: {error}"
                ));
            }
            crate::session::attach::record_event(
                "agent",
                &json!({"type":"ssmcp.message","format":"markdown","text":message.text,
                    "kind":message.kind,"message_id":message.id}),
            );
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
            remember_last(&message.text);
            saved.is_ok()
        };
        let mut history = HISTORY
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(retained) = history
            .messages
            .iter_mut()
            .find(|item| item.id == message.id)
        {
            retained.history_saved = message.history_saved;
        }
    }
    Ok(json!({
        "shown":true, "accepted":true, "message_id":message.id, "kind":message.kind,
        "duplicate":duplicate, "terminal_active":active,
        "history_saved":message.history_saved,
        "text_bytes":message.text.len(),
        "note":"Recorded in the client conversation; does not assert that the user read it."
    }))
}

pub(super) fn last_text() -> Option<String> {
    HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_copy
        .clone()
}

/// Called after publication, including local harness replies; acquires no display lock.
pub(crate) fn remember_last(text: &str) {
    HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remember_last(text);
}

/// Restore only copy state after /resume, without replaying or rewriting conversation history.
pub(crate) fn restore_copy(messages: &[super::interface::history::Message]) {
    HISTORY
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .restore_copy(messages);
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
            "description":"Deliver text addressed to the human into this client's conversation. Use for every user-facing progress update, explanation, question and final answer; code_mode say(clientId,{text,kind}) is the shortcut. kind is progress or answer. This is separate from the user's input queue and does not launch a model. Markdown and fenced source/diff are rendered locally; preserve complete plain text and links in text. Use ssmcp.ui.ask for a nonblocking question with a separately readable answer. Optional author is caller-reported display attribution. Optional idempotency_key prevents duplicate retries within retained history. Acceptance records a message; it does not prove the user read it. Exact message bodies remain in bounded process history read by ssmcp.ui.messages and /copy. A redacted plain-text copy is also saved to the durable conversation history read by ssmcp.ui.history.",
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
    fn message_receipts_use_jsonl_identities_and_copy_updates_only_after_publication() {
        let mut history = History::default();
        history.remember_last("previously published reply");
        let (first, duplicate) = history.record(request("new reply", Some("once"))).unwrap();
        assert!(!duplicate);
        let suffix = first.id.strip_prefix("message_").unwrap();
        assert_eq!(suffix.len(), 32);
        assert!(suffix.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!first.history_saved);
        assert_eq!(
            history.last_copy.as_deref(),
            Some("previously published reply")
        );
        history.messages.back_mut().unwrap().history_saved = true;
        history.remember_last("later harness reply");
        let (duplicate, repeated) = history.record(request("new reply", Some("once"))).unwrap();
        assert!(repeated && duplicate.history_saved);
        assert_eq!(duplicate.id, first.id);
        assert_eq!(history.last_copy.as_deref(), Some("later harness reply"));
        assert_eq!(history.messages.len(), 1);
    }

    #[test]
    fn resume_restores_latest_assistant_copy_without_replaying_or_importing_messages() {
        let message = |role: &str, text: &str| super::super::interface::history::Message {
            id: "message_00000000000000000000000000000001".into(),
            role: role.into(),
            content: text.into(),
            kind: "agent_message".into(),
            created_at_ms: 1,
            truncated: false,
        };
        let mut history = History::default();
        history.restore_copy(&[
            message("assistant", "old reply"),
            message("assistant", "完整最后答复：界"),
            message("user", "pending requirement stays in its own queue"),
        ]);
        assert_eq!(history.last_copy.as_deref(), Some("完整最后答复：界"));
        assert!(history.messages.is_empty());
        assert_eq!(history.bytes, 0);
        history.restore_copy(&[message("user", "not an agent reply")]);
        assert!(history.last_copy.is_none());
        history.restore_copy(&[
            message("assistant", "earlier reply"),
            message("assistant", &"界".repeat(MAX_TEXT / 3 + 1)),
        ]);
        assert!(
            history.last_copy.is_none(),
            "never restore an older stale reply"
        );
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
        assert!(colored.contains(&format!(
            "\x1b[0;{};{};1mconst",
            ui.background(),
            ui.color("accent")
        )));
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
    fn structured_messages_keep_plain_copy_and_include_layout_in_retry_identity() {
        let presentation: Presentation = serde_json::from_value(json!({
            "title":"Build", "blocks":[
                {"type":"card","title":"Checks","body":"Ready","tone":"success"}
            ]
        }))
        .unwrap();
        let changed: Presentation = serde_json::from_value(json!({
            "title":"Build", "blocks":[
                {"type":"card","title":"Checks","body":"Ready","tone":"warning"}
            ]
        }))
        .unwrap();
        let text = presentation.plain_text();
        let mut history = History::default();
        let (first, duplicate) = history
            .record_presentation(
                request(&text, Some("structured")),
                Some(presentation.clone()),
            )
            .unwrap();
        assert!(!duplicate);
        let (same, duplicate) = history
            .record_presentation(
                request(&text, Some("structured")),
                Some(presentation.clone()),
            )
            .unwrap();
        assert!(duplicate);
        assert_eq!(first.id, same.id);
        assert_eq!(first.text, text);
        assert_eq!(history.messages.back().unwrap().text, text);
        assert_eq!(history.bytes, first.retained_bytes());
        assert!(history.bytes >= text.len() + serde_json::to_vec(&presentation).unwrap().len());
        assert_eq!(
            serde_json::to_value(&first).unwrap()["presentation"],
            json!(presentation)
        );
        assert!(!rendered(&first, &Ui::default(), false).contains('\x1b'));
        assert!(
            history
                .record_presentation(request(&text, Some("structured")), Some(changed))
                .is_err()
        );
        assert!(history.record(request(&text, Some("structured"))).is_err());
        assert_eq!(history.messages.len(), 1);
        let (plain, _) = history.record(request("plain", None)).unwrap();
        assert!(
            serde_json::to_value(plain)
                .unwrap()
                .get("presentation")
                .is_none()
        );
    }

    #[test]
    fn oversized_presentation_cannot_evict_retained_messages() {
        let mut history = History::default();
        let (retained, _) = history.record(request("Keep this", None)).unwrap();
        let bytes = history.bytes;
        let oversized: Presentation = serde_json::from_value(json!({
            "title":"Oversized", "blocks":[
                {"type":"text","text":"x".repeat(MAX_HISTORY_BYTES)}
            ]
        }))
        .unwrap();
        assert!(
            history
                .record_presentation(request("fallback", None), Some(oversized))
                .is_err()
        );
        assert_eq!(history.bytes, bytes);
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages.front().unwrap().id, retained.id);
    }

    #[test]
    fn message_schema_preserves_markdown_and_rejects_impersonation_fields() {
        let markdown: Request = serde_json::from_value(json!({
            "text":"# Heading\r\n\r\n**Hello**\n\tcode","kind":"answer"
        }))
        .unwrap();
        assert!(markdown.validate().is_ok());
        assert!(
            serde_json::from_value::<Request>(json!({
                "text":"Cannot impersonate","role":"user"
            }))
            .is_err()
        );
        assert!(request("hi\u{0007}", None).validate().is_err());
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
