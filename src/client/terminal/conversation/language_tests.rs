//! Exercise actual localized projections; snapshots and raw content remain stable.
use super::*;

// Frame rows may retain SGR resets even when colors are disabled.
fn visible_width(row: &str) -> usize {
    let mut text = String::new();
    let mut chars = row.chars();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            assert_eq!(chars.next(), Some('['), "unexpected fixture escape");
            for code in chars.by_ref() {
                if ('@'..='~').contains(&code) {
                    break;
                }
            }
        } else {
            text.push(ch);
        }
    }
    text.width()
}

fn state(language: Language, cols: u16) -> State {
    super::super::preview_state(
        30,
        cols,
        &Ui {
            language,
            color: false,
            sidebar: false,
            ..Ui::default()
        },
    )
}

fn screen(state: &mut State) -> String {
    let frame = state.chat_frame(Instant::now());
    let mut parser = vt100::Parser::new(
        nonzero(frame.geometry.rows),
        nonzero(frame.geometry.cols),
        0,
    );
    parser.process(&frame.diff(None));
    parser.screen().contents()
}

#[test]
fn language_navigation_composer_help_and_errors_use_the_selected_language() {
    for language in [Language::English, Language::Chinese] {
        let mut state = state(language, 120);
        let text = screen(&mut state);
        assert!(
            text.contains(language.text("Message the agent", "向智能体发送消息")),
            "{text}"
        );
        let error = state.chat_command("/not-a-command").unwrap_err();
        assert!(
            error.contains(language.text("Unknown command", "未知命令")),
            "{error}"
        );
        state.chat_command("/help").unwrap();
        let text = screen(&mut state);
        assert!(
            text.contains(language.text("Write and send", "输入与发送")),
            "{text}"
        );
        for view in [
            View::Activity,
            View::Audit,
            View::Agents,
            View::Queue,
            View::Questions,
            View::History,
            View::Logs,
            View::Tasks,
        ] {
            assert_ne!(view.label(Language::English), view.label(Language::Chinese));
        }
        assert!(help::body(language).contains("/language en|zh"));
    }
}

#[test]
fn language_queue_receipts_localize_without_rewriting_raw_requests_or_delivery() {
    let raw = "RAW Tasks next caller 中文 /tmp/Errors.json";
    let data = json!({"agents":[],"queue":[
        {"id":"language-pending","text":raw,"status":"queued","target":"next","revision":1},
        {"id":"language-delivered","text":raw,"status":"delivered","delivery":{"run_id":"response-exact","agent_id":null,"included_at_ms":1000}}
    ]});
    for language in [Language::English, Language::Chinese] {
        let mut state = state(language, 120);
        let before = data.clone();
        let rows = state.chat_queue_rows(&data, 20).join("\n");
        assert!(rows.contains(raw), "{rows}");
        assert!(rows.contains(language.text("Waiting", "待发送")), "{rows}");
        assert!(
            rows.contains(language.text("next caller", "下一个调用方")),
            "{rows}"
        );
        assert!(
            rows.contains("●") && rows.contains(language.text("Read", "已读")),
            "{rows}"
        );
        assert_eq!(data, before);
        assert_eq!(
            language.receipt(Receipt::Read),
            language.text("Read", "已读")
        );
    }
}

#[test]
fn language_switch_preserves_draft_navigation_and_protocol_state_at_narrow_widths() {
    for cols in [1, 2, 10, 24, 45, 80, 120] {
        let mut state = state(Language::English, cols);
        state
            .conversation
            .composer
            .set_text("draft 中文 👩🏽‍💻 /tmp/Tasks");
        state.conversation.composer.left();
        state.conversation.content_focus = true;
        let draft = state.chat_saved_draft();
        for language in [Language::Chinese, Language::English] {
            let ui = Ui {
                language,
                ..state.ui.clone()
            };
            assert_eq!(state.apply_settings(&ui), None);
            for view in [
                View::Activity,
                View::Audit,
                View::Agents,
                View::Queue,
                View::Questions,
                View::History,
                View::Logs,
                View::Tasks,
            ] {
                state.conversation.view = view;
                let frame = state.chat_frame(Instant::now());
                assert_eq!(frame.rows.len(), 30);
                assert!(
                    frame
                        .rows
                        .iter()
                        .all(|row| visible_width(row) <= usize::from(cols)),
                    "{language:?} {view:?} width={cols}: {:?}",
                    frame.rows
                );
                assert!(frame.cursor.0 < 30 && frame.cursor.1 < cols);
            }
            assert_eq!(state.chat_saved_draft(), draft);
            assert_eq!(
                state.conversation.snapshot()["draft"]["text"],
                "draft 中文 👩🏽‍💻 /tmp/Tasks"
            );
        }
    }
}

#[test]
fn language_render_cache_changes_labels_and_keeps_tool_output_and_snapshot() {
    let mut state = state(Language::English, 120);
    state.events.clear();
    state.events.set_scope(events::EventScope::Conversation);
    let raw = "RAW waiting done 中文 /tmp/Tasks";
    state.log_event(
        "native",
        &json!({"type":"exec.started","job_id":"lang-job","command":["printf","fixture"]}),
        b"[RUN] native lang-job printf fixture started\n",
    );
    state.log_event(
        "native",
        &json!({"type":"exec.exited","job_id":"lang-job","exit_code":0}),
        format!("[STDOUT] lang-job\n{raw}\n").as_bytes(),
    );
    let ui = state.ui.clone();
    let english = state.events.render(120, 20, &ui, false).join("\n");
    let before = state.events.snapshot();
    let chinese_ui = Ui {
        language: Language::Chinese,
        ..ui
    };
    let chinese = state.events.render(120, 20, &chinese_ui, false).join("\n");
    assert!(
        english.contains(raw) && chinese.contains(raw),
        "{english}\n{chinese}"
    );
    assert!(
        english.contains("done") && chinese.contains("已完成"),
        "{english}\n{chinese}"
    );
    assert_eq!(state.events.snapshot(), before);
}
