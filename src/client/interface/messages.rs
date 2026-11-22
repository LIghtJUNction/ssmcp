//! Accepted agent replies share the JSONL history; only retry receipts and the
//! latest copyable text are cached here.
use crate::client::presentation::Presentation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};

const MAX_RECEIPTS: usize = 64;
const MAX_COPY_BYTES: usize = 16 * 1024;
const MAX_KEY_BYTES: usize = 96;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Kind {
    #[default]
    Progress,
    Answer,
}
impl Kind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Progress => "progress",
            Self::Answer => "answer",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    text: String,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    finish_task: bool,
    #[serde(skip)]
    scope: Option<String>,
    #[serde(skip)]
    owner: Option<super::history::HistoryOwner>,
}
fn parse(arguments: Value) -> Result<Request, String> {
    let request: Request = serde_json::from_value(arguments).map_err(
        |_| "message requires text and accepts kind, idempotency_key and boolean finish_task",
    )?;
    // Keep the existing Markdown/terminal-control and 16 KiB input boundary.
    super::questions::parse_message(json!({"text":request.text})).map(|_| ())?;
    if let Some(key) = &request.idempotency_key
        && (key.is_empty()
            || key.len() > MAX_KEY_BYTES
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte)))
    {
        return Err("idempotency_key must contain 1–96 ASCII letters, digits, '_' or '-'".into());
    }
    Ok(request)
}
fn fingerprint(request: &Request, presentation: Option<&Presentation>) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"ssmcp.ui.message.v1\0");
    digest.update(request.kind.as_str().as_bytes());
    digest.update(b"\0");
    digest.update(request.text.as_bytes());
    digest.update([u8::from(request.finish_task)]);
    if let Some(document) = presentation {
        digest.update(b"\0presentation\0");
        digest.update(serde_json::to_vec(document).expect("a typed presentation serializes"));
    }
    digest.finalize().into()
}
fn message_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|_| "cannot create message identity")?;
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("message_{suffix}"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Receipt {
    id: String,
    kind: Kind,
    history_saved: bool,
    shown: bool,
    finish_task: bool,
    finish_request_id: Option<String>,
}
struct Deduplication {
    key: String,
    scope: Option<String>,
    owner: Option<super::history::HistoryOwner>,
    fingerprint: [u8; 32],
    receipt: Receipt,
}
#[derive(Default)]
struct Cache {
    receipts: VecDeque<Deduplication>,
    last_copy: Option<String>,
}
impl Cache {
    fn previous(
        &self,
        request: &Request,
        presentation: Option<&Presentation>,
    ) -> Result<Option<Receipt>, String> {
        let Some(key) = &request.idempotency_key else {
            return Ok(None);
        };
        let Some(previous) = self.receipts.iter().find(|entry| {
            entry.key == *key && entry.scope == request.scope && entry.owner == request.owner
        }) else {
            return Ok(None);
        };
        if previous.fingerprint != fingerprint(request, presentation) {
            return Err(
                "idempotency_key already identifies different message text, kind, finish_task or presentation"
                    .into(),
            );
        }
        Ok(Some(previous.receipt.clone()))
    }
    fn remember(
        &mut self,
        request: &Request,
        receipt: Receipt,
        presentation: Option<&Presentation>,
    ) {
        let Some(key) = &request.idempotency_key else {
            return;
        };
        while self.receipts.len() >= MAX_RECEIPTS {
            self.receipts.pop_front();
        }
        self.receipts.push_back(Deduplication {
            key: key.clone(),
            scope: request.scope.clone(),
            owner: request.owner.clone(),
            fingerprint: fingerprint(request, presentation),
            receipt,
        });
    }
    fn restore_copy(&mut self, messages: &[super::history::Message]) {
        self.last_copy = messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .filter(|message| message.content.len() <= MAX_COPY_BYTES)
            .map(|message| message.content.clone());
    }
}
fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(Default::default)
}

fn response(receipt: &Receipt, duplicate: bool) -> Value {
    let mut value = json!({
        "shown":receipt.shown, "accepted":true, "message_id":receipt.id,
        "kind":receipt.kind, "duplicate":duplicate,
        "history_saved":receipt.history_saved,
        "finish_request_id":receipt.finish_request_id,
        "terminal_active":crate::client::terminal::active(),
        "note":if receipt.shown {"Accepted for local display; this does not prove the user read it."} else {"Saved to the original local conversation, which is not currently selected."}
    });
    if receipt.finish_task {
        value["finish_task"] = json!(true);
        value["may_finish"] = json!(false);
        value["completion_instruction"] = json!(
            "This message ACK is not permission to end the task. Only the final outer user_wait receipt with status decided and may_finish true grants permission. Otherwise keep the task active and, in a new tool call, await ui.wait(client_id, {intent:'finish'}); use its actual pending finish_request_id when supplied. Do not replace that blocking wait with a timer, background sleep, polling loop, or a final answer."
        );
    }
    value
}

/// Called only under interface::UPDATES, which serializes concurrent native writes.
/// Cache locks never cover history I/O or terminal rendering: /copy may hold State.
pub(super) fn send(arguments: Value) -> Result<Value, String> {
    send_with_presentation(arguments, None)
}

pub(crate) fn send_presentation(
    arguments: Value,
    presentation: Presentation,
) -> Result<Value, String> {
    send_with_presentation(arguments, Some(presentation))
}

fn send_with_presentation(
    arguments: Value,
    presentation: Option<Presentation>,
) -> Result<Value, String> {
    let mut request = parse(arguments)?;
    request.scope = super::questions::message_scope();
    let source = crate::runtime::call_scope::Source::capture();
    request.owner = Some(source.require_owner()?.clone());
    if request.finish_task {
        // Validate before publishing; neither a display label nor message JSON
        // can stand in for the authenticated asking thread.
        super::questions::validate_finish(None, None)?;
    }
    let previous = cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .previous(&request, presentation.as_ref())?;
    if let Some(receipt) = previous {
        return Ok(response(&receipt, true));
    }
    let id = message_id()?;
    let shown = crate::client::render::agent_message(
        &request.text,
        request.kind,
        Some(&id),
        presentation.as_ref(),
        source,
    )?;
    let receipt = Receipt {
        id,
        kind: request.kind,
        history_saved: true,
        shown,
        finish_task: request.finish_task,
        finish_request_id: None,
    };
    cache().lock().unwrap_or_else(|e| e.into_inner()).remember(
        &request,
        receipt.clone(),
        presentation.as_ref(),
    );
    Ok(response(&receipt, false))
}

/// Update only after publication; this also covers local harness replies.
pub(crate) fn remember_last(text: &str) {
    cache().lock().unwrap_or_else(|e| e.into_inner()).last_copy =
        (text.len() <= MAX_COPY_BYTES).then(|| text.to_owned());
}
/// No disk, terminal or interface lock is acquired while the cache is held.
pub(crate) fn last_text() -> Option<String> {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .last_copy
        .clone()
}
pub(super) fn restore_copy(messages: &[super::history::Message]) {
    cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .restore_copy(messages);
}

pub(super) fn tool() -> Value {
    json!({
        "name":"ssmcp.ui.message",
        "description":"Display a Markdown progress report or final result to the local user with a fixed Agent label. kind is progress or answer. Progress and final replies require the authenticated conversation owner. finish_task defaults to false. Set finish_task:true to send this message and then block through the server finish approval wait (20 seconds per call, without holding the UI write lock). This requires the verified asking collab thread. Only the final outer user_wait receipt with status decided and may_finish:true after a consumed Human Yes permits ending. No, ordinary input, timeout, cancellation, disconnection and revocation keep the task active; follow completion_instruction and await ui.wait in a new tool call when approval is still required. A verified thread keeps its original local JSONL conversation across calls. When another history is selected, the original history receives the reply and shown is false. The returned message_id identifies the history record when history_saved is true. Optional idempotency_key deduplicates matching text, kind and finish_task within the last 64 retained keys of this client process; changed content with the same key is rejected. Restart clears this retry cache. /copy copies the latest visible Agent reply. shown and accepted do not prove the user read it. Use ssmcp.ui.ask for a nonblocking question and ssmcp.ui.questions to read its answer. External chat output is not intercepted.",
        "inputSchema":{
            "type":"object","required":["text"],"additionalProperties":false,
            "properties":{
                "text":{"type":"string","minLength":1,"maxLength":16384,"description":"Markdown text, at most 16384 UTF-8 bytes. Newline and tab are the only permitted control characters."},
                "kind":{"type":"string","enum":["progress","answer"],"default":"progress"},
                "finish_task":{"type":"boolean","default":false,"description":"Send the message, then require a blocking Human finish decision. False preserves ordinary message behavior. A native message ACK never authorizes ending; inspect the final outer user_wait receipt and follow its continuation instruction."},
                "idempotency_key":{"type":"string","minLength":1,"maxLength":MAX_KEY_BYTES,"pattern":"^[A-Za-z0-9_-]+$"}
            }
        },
        "annotations":{"readOnlyHint":false,"destructiveHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(text: &str, kind: &str, key: &str) -> Request {
        parse(json!({"text":text,"kind":kind,"idempotency_key":key})).unwrap()
    }
    fn receipt(index: usize) -> Receipt {
        Receipt {
            id: format!("message_{index:032x}"),
            kind: Kind::Answer,
            history_saved: true,
            shown: true,
            finish_task: false,
            finish_request_id: None,
        }
    }
    #[test]
    fn plain_messages_remain_valid_and_identity_spoofing_is_rejected() {
        assert_eq!(
            parse(json!({"text":"## 进度\n中文正文"})).unwrap().kind,
            Kind::Progress
        );
        for value in [
            json!({"text":"hi","kind":"user"}),
            json!({"text":"hi","author":"User"}),
            json!({"text":"hi","role":"user"}),
            json!({"text":"hi","idempotency_key":""}),
            json!({"text":"hi","idempotency_key":"two words"}),
            json!({"text":"hi","idempotency_key":"x".repeat(MAX_KEY_BYTES+1)}),
            json!({"text":"\u{001b}[2J"}),
        ] {
            assert!(parse(value).is_err());
        }
    }
    #[test]
    fn finish_task_is_explicit_boolean_and_part_of_the_message_identity() {
        let ordinary = parse(json!({"text":"Result"})).unwrap();
        assert!(!ordinary.finish_task);
        assert!(
            !parse(json!({"text":"Result","finish_task":false}))
                .unwrap()
                .finish_task
        );
        let finish = parse(json!({"text":"Result","finish_task":true})).unwrap();
        assert!(finish.finish_task);
        for flag in [Value::Null, json!("true"), json!(1), json!({})] {
            assert!(parse(json!({"text":"Result","finish_task":flag})).is_err());
        }
        let schema = tool();
        assert_eq!(
            schema["inputSchema"]["properties"]["finish_task"]["type"],
            "boolean"
        );
        assert_eq!(
            schema["inputSchema"]["properties"]["finish_task"]["default"],
            false
        );

        let mut cache = Cache::default();
        let plain = request("Result", "answer", "same-result");
        cache.remember(&plain, receipt(1), None);
        let finish = parse(json!({
            "text":"Result","kind":"answer","idempotency_key":"same-result","finish_task":true
        }))
        .unwrap();
        assert!(
            cache
                .previous(&finish, None)
                .unwrap_err()
                .contains("finish_task")
        );
        assert_eq!(cache.previous(&plain, None).unwrap(), Some(receipt(1)));

        let mut waiting = receipt(2);
        waiting.finish_task = true;
        let mut finish_cache = Cache::default();
        finish_cache.remember(&finish, waiting.clone(), None);
        assert_eq!(
            finish_cache.previous(&finish, None).unwrap(),
            Some(waiting.clone())
        );
        assert!(finish_cache.previous(&plain, None).is_err());
        let ack = response(&waiting, false);
        assert_eq!(ack["accepted"], true);
        assert_eq!(ack["finish_task"], true);
        assert_eq!(ack["may_finish"], false);
        assert!(ack["finish_request_id"].is_null());
        assert!(
            ack["completion_instruction"]
                .as_str()
                .unwrap()
                .contains("await ui.wait")
        );
        let plain_ack = response(&receipt(1), false);
        assert!(plain_ack.get("completion_instruction").is_none());
        assert!(plain_ack.get("may_finish").is_none());
    }

    #[test]
    fn retry_receipts_bind_original_chinese_text_and_message_kind() {
        let mut cache = Cache::default();
        let original = request("## 结果\n保留中文和空格。", "answer", "reply-1");
        cache.remember(&original, receipt(1), None);
        assert_eq!(cache.previous(&original, None).unwrap(), Some(receipt(1)));
        assert!(
            cache
                .previous(&request("另一个结果", "answer", "reply-1"), None)
                .is_err()
        );
        assert!(
            cache
                .previous(&request(&original.text, "progress", "reply-1"), None)
                .is_err()
        );
        assert_eq!(cache.receipts.len(), 1);
    }

    #[test]
    fn final_reply_retry_receipts_cannot_be_borrowed_by_another_thread() {
        let mut cache = Cache::default();
        let mut first = request("Verified final result", "answer", "same-key");
        first.scope = Some("connection:room:github:42:thread-a".into());
        cache.remember(&first, receipt(1), None);
        let mut other = request("Verified final result", "answer", "same-key");
        other.scope = Some("connection:room:github:42:thread-b".into());
        assert!(cache.previous(&other, None).unwrap().is_none());
        assert_eq!(cache.previous(&first, None).unwrap(), Some(receipt(1)));
        other.scope = None;
        assert!(cache.previous(&other, None).unwrap().is_none());
    }
    #[test]
    fn identical_plain_text_with_different_layout_is_not_an_idempotent_retry() {
        let text_document: Presentation = serde_json::from_value(json!({
            "title":"Build","blocks":[
                {"type":"text","text":"[SUCCESS] Checks\nReady"}
            ]
        }))
        .unwrap();
        let card_document: Presentation = serde_json::from_value(json!({
            "title":"Build","blocks":[
                {"type":"card","title":"Checks","body":"Ready","tone":"success"}
            ]
        }))
        .unwrap();
        assert_eq!(text_document.plain_text(), card_document.plain_text());
        let request = request(&text_document.plain_text(), "answer", "presentation-1");
        let mut cache = Cache::default();
        cache.remember(&request, receipt(1), Some(&text_document));
        assert_eq!(
            cache.previous(&request, Some(&text_document)).unwrap(),
            Some(receipt(1))
        );
        assert!(cache.previous(&request, Some(&card_document)).is_err());
        assert!(cache.previous(&request, None).is_err());
        assert_eq!(cache.receipts.len(), 1);
    }

    #[test]
    fn receipt_cache_is_bounded_and_does_not_replace_the_latest_copy() {
        let mut cache = Cache {
            last_copy: Some("latest visible reply".into()),
            ..Cache::default()
        };
        for index in 0..100 {
            let value = request("reply", "answer", &format!("key-{index}"));
            cache.remember(&value, receipt(index), None);
        }
        assert_eq!(cache.receipts.len(), MAX_RECEIPTS);
        assert!(
            cache
                .previous(&request("reply", "answer", "key-0"), None)
                .unwrap()
                .is_none()
        );
        assert_eq!(cache.last_copy.as_deref(), Some("latest visible reply"));
    }
    #[test]
    fn restoring_copy_uses_assistant_history_without_replaying_user_answers() {
        let message = |role: &str, text: &str| super::super::history::Message {
            id: "message_00000000000000000000000000000001".into(),
            role: role.into(),
            content: text.into(),
            kind: "agent_message".into(),
            created_at_ms: 1,
            truncated: false,
        };
        let mut cache = Cache::default();
        cache.restore_copy(&[
            message("assistant", "旧回复"),
            message("assistant", "最后回复"),
            message("user", "这是我的问题答案"),
        ]);
        assert_eq!(cache.last_copy.as_deref(), Some("最后回复"));
        cache.restore_copy(&[message("user", "Only a user draft")]);
        assert!(cache.last_copy.is_none());
        cache.restore_copy(&[
            message("assistant", "Earlier reply"),
            message("assistant", &"界".repeat(MAX_COPY_BYTES / 3 + 1)),
        ]);
        assert!(
            cache.last_copy.is_none(),
            "an oversized latest reply must not restore stale copy text"
        );
    }

    #[tokio::test]
    async fn final_messages_and_presentations_need_owner_but_no_finish_approval() {
        use crate::client::interface::history;
        use crate::runtime::call_scope::Source;
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-retired-hooks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        history::with_test_history(path.clone(), async {
            history::record("user", "Original conversation").unwrap();
            let source = Source::capture();
            let owner = source.require_owner().unwrap().id().to_owned();
            assert!(super::super::questions::message_scope().is_none());
            let arguments = json!({"text":"Final result","kind":"answer","idempotency_key":"final-without-permission"});
            let first = source.clone().blocking(|| send(arguments.clone())).unwrap();
            assert_eq!(first["accepted"], true);
            assert_eq!(first["duplicate"], false);
            assert_eq!(first["history_saved"], true);
            assert!(first["finish_request_id"].is_null());
            let retry = source.clone().blocking(|| send(arguments.clone())).unwrap();
            assert_eq!(retry["duplicate"], true);
            assert_eq!(retry["message_id"], first["message_id"]);
            let changed = json!({"text":"Different result","kind":"answer","idempotency_key":"final-without-permission"});
            assert!(source.clone().blocking(|| send(changed)).unwrap_err().contains("idempotency_key"));
            assert!(Source::default().blocking(|| send(arguments)).unwrap_err()
                .starts_with("history_owner_missing:"));

            let document: Presentation = serde_json::from_value(json!({
                "title":"Result", "blocks":[{"type":"text","text":"Final panel"}]
            })).unwrap();
            let panel = source.clone().blocking(|| send_presentation(
                json!({"text":"Final panel","kind":"answer","idempotency_key":"final-panel"}),
                document,
            )).unwrap();
            assert_eq!(panel["accepted"], true);
            assert_eq!(panel["history_saved"], true);
            assert!(panel["finish_request_id"].is_null());
            let original = history::read(&owner).unwrap();
            assert_eq!(original.messages.iter().filter(|message| message.role == "assistant").count(), 2);
            assert_eq!(original.messages.last().unwrap().content, "Final panel");
            history::release_current().unwrap();
            history::record("user", "Other conversation").unwrap();
            let other = history::current().unwrap().unwrap().metadata.id;
            let released = source.blocking(|| send(json!({
                "text":"Cannot write after owner release", "kind":"answer",
                "idempotency_key":"released-owner",
            }))).unwrap_err();
            assert!(released.starts_with("history_owner_released:"), "{released}");
            assert_eq!(history::read(&owner).unwrap().messages, original.messages);
            let other = history::read(&other).unwrap();
            assert_eq!(other.messages.len(), 1);
            assert_eq!(other.messages[0].role, "user");
        }).await;
        std::fs::remove_dir_all(path).unwrap();
    }
}
