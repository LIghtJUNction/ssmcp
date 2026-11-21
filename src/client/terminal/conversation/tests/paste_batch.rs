//! Deterministic work counts complement the real PTY latency fixture.
use super::*;

fn isolated(name: &str) -> bool {
    const CHILD: &str = "SSMCP_TEST_PASTE_BATCH";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    // Keep Runtime, clipboard and local input bindings from parallel tests
    // out of the retained-paste and input-work fixtures.
    let test = format!("client::terminal::conversation::tests::paste_batch::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &test, "--nocapture", "--test-threads=1"])
        .env(CHILD, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn live_input_state() -> State {
    let state = State::new(36, 120, &Ui::default());
    assert!(!state.preview);
    assert!(!state.conversation.content_focus);
    assert!(state.conversation.native_recipient.is_none());
    state
}

fn synthetic_log(bytes: usize) -> String {
    let mut text = "synthetic diagnostic 界\r\n".repeat(8);
    assert!(text.len() <= bytes);
    text.push_str(&"x".repeat(bytes - text.len()));
    text
}

#[test]
fn bracketed_paste_work_tracks_read_spans_not_payload_bytes() {
    if isolated("bracketed_paste_work_tracks_read_spans_not_payload_bytes") {
        return;
    }
    for size in [2738, 64 * 1024, references::paste::MAX_PASTE_BYTES] {
        let text = synthetic_log(size);
        assert!(references::paste::should_store(&text));
        let mut state = live_input_state();
        state.conversation.composer.set_text("prefix suffix");
        state.conversation.composer.home();
        let original = state.chat_input_stamp().unwrap();
        let now = Instant::now();
        state.chat_process_input(b"\x1b[200~", now, false);
        for chunk in text.as_bytes().chunks(4096) {
            state.chat_process_input(chunk, now, false);
        }

        assert_eq!(state.conversation.paste.as_deref(), Some(text.as_bytes()));
        assert!(!state.conversation.paste_overflow);
        assert!(state.conversation.input_task.is_none());
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&original));
        let work = &state.conversation.input_work;
        let reads = size.div_ceil(4096);
        assert_eq!(
            work.decoder_calls, 6,
            "only the start marker needs decoding"
        );
        assert_eq!(work.paste_spans, reads);
        assert_eq!(work.paste_bytes, size);

        // No Runtime or local input binding exists in this isolated process.
        // Ending the paste exercises the normal retained-text failure contract
        // without creating a file or importing anything into a real session.
        state.chat_process_input(b"\x1b[201~", now, false);
        assert_eq!(
            state.conversation.paste_recovery.as_deref(),
            Some(text.as_str())
        );
        assert_eq!(state.chat_input_stamp().as_ref(), Some(&original));
        assert!(state.conversation.input_task.is_none());
        assert_eq!(state.conversation.input_work.decoder_calls, 12);
    }
}

#[test]
fn plain_text_completion_work_is_linear_in_each_input_batch() {
    if isolated("plain_text_completion_work_is_linear_in_each_input_batch") {
        return;
    }
    for size in [4096, 8192, 16 * 1024] {
        for unicode in [false, true] {
            let mut text = if unicode {
                "普通正文 界\n".repeat(size / "普通正文 界\n".len())
            } else {
                String::new()
            };
            text.push_str(&"x".repeat(size - text.len()));
            let mut state = live_input_state();
            state.chat_process_input(text.as_bytes(), Instant::now(), false);
            assert_eq!(state.conversation.composer.text(), text);
            assert_eq!(state.conversation.composer.cursor(), text.len());
            assert_eq!(state.conversation.input_work.completion_queries, 1);
            assert_eq!(state.conversation.input_work.completion_input_bytes, size);
            assert_eq!(state.conversation.input_work.paste_bytes, 0);
            assert!(state.conversation.submitting.is_none());
        }
    }
}

#[test]
fn batched_paste_preserves_split_unicode_navigation_and_literal_commands() {
    let bytes = "ab\x1b[D\x1b[200~界\r\n/reload\x1b[201~\x1b[C!".as_bytes();
    for size in [1, 2, 3, 5, 4096] {
        let mut state = preview(36, 120);
        for chunk in bytes.chunks(size) {
            input(&mut state, chunk);
        }
        assert_eq!(state.conversation.composer.text(), "a界\n/reloadb!");
        assert_eq!(state.conversation.composer.cursor(), "a界\n/reloadb!".len());
        assert!(state.actions.is_empty(), "pasted /reload is only text");
        assert!(state.conversation.submitting.is_none());
        assert!(!state.keyboard.pasting);
        assert!(state.conversation.paste.is_none());
    }
}

#[test]
fn batched_binary_paste_keeps_embedded_markers_opaque() {
    let png = crate::client::input::paste::fixture_png();
    for size in [1, 2, 7, 4096] {
        let mut state = preview(36, 120);
        state.conversation.composer.set_text("draft");
        input(&mut state, b"\x1b[200~");
        for chunk in png.chunks(size) {
            input(&mut state, chunk);
        }
        assert!(state.keyboard.pasting);
        assert_eq!(state.conversation.paste.as_deref(), Some(png.as_slice()));
        assert_eq!(state.conversation.composer.text(), "draft");
        input(&mut state, b"\x1b[201~ tail");
        assert!(!state.keyboard.pasting);
        assert!(state.conversation.paste.is_none());
        assert_eq!(state.conversation.composer.text(), "draft tail");
        assert!(state.actions.is_empty());
    }
}

#[test]
fn oversized_literal_span_rejects_the_paste_without_changing_the_draft() {
    let mut state = preview(36, 120);
    state.conversation.composer.set_text("preserved 界");
    state.conversation.target = Some("chosen-agent".into());
    let original = state.chat_input_stamp().unwrap();
    input(&mut state, b"\x1b[200~");
    input(
        &mut state,
        &vec![b'x'; references::paste::MAX_PASTE_BYTES + 1],
    );
    assert!(state.conversation.paste_overflow);
    assert!(state.conversation.paste.as_ref().unwrap().len() <= references::paste::MAX_PASTE_BYTES);
    input(&mut state, b"\x1b[201~");
    assert_eq!(state.chat_input_stamp().as_ref(), Some(&original));
    assert!(
        state
            .notice
            .as_deref()
            .unwrap()
            .contains("Paste exceeds its limit")
    );
    assert!(state.conversation.input_task.is_none());
    assert!(state.conversation.submitting.is_none());
}

#[test]
fn coalesced_controls_refresh_suggestions_before_accepting_or_dismissing() {
    let mut state = preview(36, 120);
    input(&mut state, b"/aud\t");
    assert_eq!(state.conversation.composer.text(), "/audit ");
    assert!(!state.conversation.content_focus);
    state.conversation.composer.take_text();
    input(&mut state, b"/aud\x1b");
    state.chat_process_input(&[], Instant::now() + Duration::from_millis(100), true);
    assert!(state.conversation.composer.suggestions().is_empty());
    assert_eq!(state.conversation.composer.text(), "/aud");
    assert!(!state.conversation.content_focus);
}

#[test]
fn carriage_return_executes_the_command_before_paste_and_tail_input() {
    let mut state = preview(36, 120);
    state.events.set_diff_mode(events::DiffMode::Single);
    state.conversation.target = Some("example-builder".into());
    let queue = state.chat_data()["queue"].clone();
    input(
        &mut state,
        b"/diff split\r\x1b[200~literal\r\n/reload\x1b[201~x",
    );
    assert_eq!(state.events.diff_mode(), events::DiffMode::Split);
    assert_eq!(
        state.notice.as_deref(),
        Some("Diff layout · split · narrow screens use single")
    );
    let stamp = state.chat_input_stamp().unwrap();
    assert_eq!(stamp.text, "literal\n/reloadx");
    assert_eq!(stamp.cursor, stamp.text.len());
    assert_eq!(stamp.target.as_deref(), Some("example-builder"));
    assert!(stamp.native_recipient.is_none());
    assert!(state.conversation.submitting.is_none());
    assert_eq!(state.chat_data()["queue"], queue);
    assert!(state.actions.is_empty());
    assert_eq!(state.conversation.input_work.paste_spans, 1);
    assert_eq!(
        state.conversation.input_work.paste_bytes,
        b"literal\r\n/reload".len()
    );
}
