//! Export local context as an editable file reference. This never sends or replays it.
use super::{
    interface::{history, questions},
    references, steering,
};
use serde_json::{Value, json};
use std::{fmt::Write as _, path::Path};

pub(crate) mod clipboard;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_NOTE_BYTES: usize = 16 * 1024;
const MAX_ITEM_BYTES: usize = 16 * 1024;
const MAX_HISTORY_ITEMS: usize = 500;
const MAX_QUEUE_ITEMS: usize = 128;
const MAX_QUESTIONS: usize = 32;

/// Called by the existing attachment worker, never while the terminal lock is held.
pub(crate) fn prepare(root: &Path, note: &str) -> Result<String, String> {
    validate_note(note)?;
    // Acquire and release each store independently. This is a local snapshot,
    // not an atomic transaction spanning UI, history and steering locks.
    let queue = steering::snapshot();
    let pending = questions::pending();
    let conversation = history::current()
        .map_err(|error| format!("Could not export the current conversation: {error}"))?;
    let document = markdown(root, note, conversation.as_ref(), &queue, &pending)?;
    references::paste::store_markdown_at(root, &document)
}

fn validate_note(note: &str) -> Result<(), String> {
    if note.len() > MAX_NOTE_BYTES {
        return Err("The handoff note exceeds 16 KiB; reference a workspace file instead.".into());
    }
    if note
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return Err("The handoff note contains terminal control characters.".into());
    }
    Ok(())
}

fn safe_text(text: &str) -> String {
    let mut end = text.len().min(MAX_ITEM_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    crate::audit::event(json!(&text[..end]))
        .as_str()
        .unwrap_or("[redacted]")
        .to_owned()
}

fn body(text: &str) -> String {
    let mut safe = safe_text(text);
    if text.len() > MAX_ITEM_BYTES {
        safe.push_str("\n[This entry was truncated at 16 KiB.]");
    }
    // Choose the shorter safe fence so quoted Markdown cannot close the block.
    let longest = |mark: char| {
        let mut run = 0;
        let mut longest = 0;
        for ch in safe.chars() {
            run = if ch == mark { run + 1 } else { 0 };
            longest = longest.max(run);
        }
        longest
    };
    let backticks = longest('`');
    let tildes = longest('~');
    let (mark, count) = if backticks <= tildes {
        ('`', (backticks + 1).max(3))
    } else {
        ('~', (tildes + 1).max(3))
    };
    let fence: String = std::iter::repeat_n(mark, count).collect();
    format!("{fence}text\n{safe}\n{fence}\n\n")
}

fn inline(text: &str) -> String {
    serde_json::to_string(&safe_text(text)).unwrap_or_else(|_| "\"[unavailable]\"".into())
}

/// History arrives newest first and is displayed in chronological order after
/// selection. Pending work retains its existing queue order.
fn section(
    title: &str,
    blocks: impl IntoIterator<Item = String>,
    already_omitted: usize,
    budget: usize,
    chronological: bool,
) -> String {
    let mut kept = Vec::new();
    let mut used = title.len() + 512;
    let mut omitted = already_omitted;
    for block in blocks {
        if used + block.len() <= budget {
            used += block.len();
            kept.push(block);
        } else {
            omitted += 1;
        }
    }
    if chronological {
        kept.reverse();
    }
    let mut text = format!("## {title}\n\n");
    if kept.is_empty() {
        text.push_str("No records to export.\n\n");
    }
    for block in kept {
        text.push_str(&block);
    }
    if omitted != 0 {
        let _ = writeln!(
            text,
            "> Export size or count limits omitted {omitted} records.\n"
        );
    }
    text
}

fn markdown(
    root: &Path,
    note: &str,
    conversation: Option<&history::Conversation>,
    queue: &Value,
    pending: &[questions::Question],
) -> Result<String, String> {
    validate_note(note)?;
    let mut document = format!(
        "# SSMCP conversation handoff\n\n\
         This is a read-only snapshot of local context. Creating it does not send requests, execute commands, call a model, or restore an external session. \
         Pending requests remain in their original queue, and questions still require the user. Do not resend or execute old content solely because it appears here.\n\n\
         Stores are read separately, so their snapshots can differ slightly in time. File references still point to the original workspace; attachments and credentials are not copied. \
         Raw logs and complete outputs retain their existing archive rules.\n\n\
         - Format: ssmcp.handoff.v1\n\
         - Workspace: {}\n\
         - Export limit: 2 MiB; per-entry text preview: 16 KiB.\n\n",
        inline(&root.to_string_lossy())
    );
    if let Some(conversation) = conversation {
        let _ = writeln!(
            document,
            "- Current JSONL conversation: {}\n- History format: {} (version {})\n- History previously trimmed: {} records.\n",
            inline(&conversation.metadata.id),
            inline(&conversation.metadata.schema),
            conversation.metadata.schema_version,
            conversation.metadata.truncated_messages,
        );
    } else {
        document.push_str("This process has not created or resumed a JSONL conversation.\n\n");
    }
    if !note.trim().is_empty() {
        document.push_str("## Additional note\n\n");
        document.push_str(&body(note));
    }

    let records: Vec<&Value> = queue["queue"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|record| !matches!(record["status"].as_str(), Some("delivered" | "cancelled")))
        .filter(|record| record["text"].is_string())
        .collect();
    document.push_str(&section(
        "Pending requests (state unchanged)",
        records.iter().take(MAX_QUEUE_ITEMS).map(|record| {
            let source = references::source(record["text"].as_str().unwrap_or_default());
            format!(
                "### Request {}\n\nStatus: {}; recipient: {}; uncertain delivery: {}.\n\n{}",
                inline(record["id"].as_str().unwrap_or("unknown")),
                inline(record["status"].as_str().unwrap_or("unknown")),
                inline(record["target"].as_str().unwrap_or("next")),
                record["uncertain"].as_bool().unwrap_or(false),
                body(source),
            )
        }),
        records.len().saturating_sub(MAX_QUEUE_ITEMS),
        768 * 1024,
        false,
    ));

    let questions: Vec<_> = pending
        .iter()
        .filter(|question| question.state == questions::QuestionState::Pending)
        .collect();
    document.push_str(&section(
        "Pending questions (the user still answers)",
        questions.iter().take(MAX_QUESTIONS).map(|question| {
            let mut block = format!("### Question {}\n\n", inline(&question.id));
            if let Some(title) = &question.title {
                let _ = writeln!(block, "Title: {}\n", inline(title));
            }
            block.push_str(&body(&question.question));
            if !question.options.is_empty() {
                block.push_str("Suggested answers:\n\n");
                for option in question.options.iter().take(8) {
                    let _ = writeln!(block, "- {}", inline(option));
                }
                block.push('\n');
            }
            block
        }),
        questions.len().saturating_sub(MAX_QUESTIONS),
        320 * 1024,
        false,
    ));

    if let Some(conversation) = conversation {
        document.push_str(&section(
            "Current conversation history (chronological)",
            conversation
                .messages
                .iter()
                .rev()
                .take(MAX_HISTORY_ITEMS)
                .map(|message| {
                    format!(
                        "### {} · {}\n\nRecord kind: {}; timestamp: {} ms; original record truncated: {}.\n\n{}",
                        inline(&message.role),
                        inline(&message.id),
                        inline(&message.kind),
                        message.created_at_ms,
                        message.truncated,
                        body(&message.content),
                    )
                }),
            conversation
                .messages
                .len()
                .saturating_sub(MAX_HISTORY_ITEMS),
            768 * 1024,
            true,
        ));
    }
    if document.len() > MAX_BYTES {
        return Err("The handoff export exceeds 2 MiB; shorten the note and try again.".into());
    }
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(messages: Vec<history::Message>) -> history::Conversation {
        history::Conversation {
            metadata: history::SessionMetadata {
                schema: "ssmcp.conversation.v1".into(),
                schema_version: 1,
                id: "conversation_00000000000000000000000000000001".into(),
                workspace: "/fixture".into(),
                created_at_ms: 1,
                truncated_messages: 2,
            },
            messages,
        }
    }
    fn message(text: &str) -> history::Message {
        history::Message {
            input_origin: None,
            id: "message_00000000000000000000000000000001".into(),
            role: "assistant".into(),
            kind: "agent_message".into(),
            content: text.into(),
            created_at_ms: 1,
            truncated: false,
        }
    }
    fn question(state: questions::QuestionState) -> questions::Question {
        questions::Question {
            id: "question-one".into(),
            title: Some("确认交接".into()),
            question: "下一步做什么？".into(),
            options: vec!["继续".into()],
            state,
            answer: None,
            created_at_ms: 1,
            answered_at_ms: None,
        }
    }

    #[test]
    fn export_preserves_pending_work_and_chinese_without_hidden_state_or_replay() {
        let queue = json!({
            "draft":{"text":"private unfinished draft"}, "token":"unexported-token",
            "queue":[
                {"id":"pending","status":"uncertain","target":"agent_one","text":"保留这项要求","uncertain":true},
                {"id":"done","status":"delivered","text":"already delivered"},
                {"id":"cancelled","status":"cancelled","text":"cancelled requirement"}
            ]
        });
        let history = conversation(vec![message("## 结果\n保留 Markdown")]);
        let output = markdown(
            Path::new("/fixture"),
            "先检查回归。",
            Some(&history),
            &queue,
            &[question(questions::QuestionState::Pending)],
        )
        .unwrap();
        for text in [
            "保留这项要求",
            "agent_one",
            "下一步做什么",
            "先检查回归",
            "## 结果",
            "History previously trimmed: 2",
            "does not send requests",
        ] {
            assert!(output.contains(text), "{text}");
        }
        for text in [
            "private unfinished draft",
            "unexported-token",
            "already delivered",
            "cancelled requirement",
        ] {
            assert!(!output.contains(text), "{text}");
        }
        assert_eq!(queue["queue"][0]["status"], "uncertain");
    }

    #[test]
    fn empty_history_still_exports_pending_questions_and_excludes_answered_questions() {
        let mut answered = question(questions::QuestionState::Answered);
        answered.question = "answered and not pending".into();
        let output = markdown(
            Path::new("/fixture"),
            "",
            None,
            &json!({}),
            &[question(questions::QuestionState::Pending), answered],
        )
        .unwrap();
        assert!(output.contains("has not created or resumed"));
        assert!(output.contains("下一步做什么"));
        assert!(!output.contains("answered and not pending"));
    }

    #[test]
    fn export_is_bounded_and_retains_recent_history_with_explicit_omissions() {
        let mut messages: Vec<_> = (0..1000)
            .map(|index| message(&format!("记录 {index}\n{}", "中".repeat(6000))))
            .collect();
        messages.last_mut().unwrap().content = "latest retained 中文".into();
        let output = markdown(
            Path::new("/fixture"),
            "",
            Some(&conversation(messages)),
            &json!({}),
            &[],
        )
        .unwrap();
        assert!(output.len() <= MAX_BYTES);
        assert!(output.contains("latest retained 中文"));
        assert!(!output.contains("记录 0\n"));
        assert!(output.contains("omitted"));
        assert!(output.contains("truncated at 16 KiB"));
    }

    #[test]
    fn quoted_markdown_fences_and_unicode_limits_remain_well_formed() {
        let source = "```markdown\n危险的围栏\n```\n~~~text\n嵌套内容\n~~~";
        let quoted = body(source);
        assert!(quoted.starts_with("````text\n"));
        assert!(quoted.ends_with("\n````\n\n"));
        assert!(quoted.contains(source));
        let unicode = body(&"🙂".repeat(5000));
        assert!(unicode.contains("truncated at 16 KiB"));
        assert!(!unicode.contains('\u{fffd}'));
    }

    #[test]
    fn invalid_handoff_notes_are_rejected_before_export() {
        assert!(validate_note(&"a".repeat(MAX_NOTE_BYTES + 1)).is_err());
        assert!(validate_note("control\u{1b}[2J").is_err());
        assert!(validate_note("中文\n\t备注").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn export_uses_the_shared_private_markdown_input_writer() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let id: String = crate::hex::encode(random);
        let root = std::env::temp_dir().join(format!("ssmcp-handoff-test-{id}"));
        fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        let document = markdown(&root, "可继续编辑", None, &json!({}), &[]).unwrap();
        let reference = references::paste::store_markdown_at(&root, &document).unwrap();
        assert!(reference.starts_with("@\".ssmcp/inputs/handoff-"));
        assert!(reference.ends_with(".md\""));
        let path = root.join(reference.trim_start_matches("@\"").trim_end_matches('"'));
        assert_eq!(fs::read_to_string(&path).unwrap(), document);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(root.join(".ssmcp/inputs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}
