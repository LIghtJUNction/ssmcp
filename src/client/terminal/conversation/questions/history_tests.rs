use super::*;
use crate::client::interface::history as saved;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-question-key-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(!path.exists());
        crate::platform::private_directory(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn state(width: u16) -> State {
    let mut state = super::super::super::preview_state(
        28,
        width,
        &Ui {
            sidebar: false,
            ..Ui::default()
        },
    );
    state.events.clear();
    state.conversation.composer.set_text("keep the main draft");
    state.chat_questions();
    state
}

fn press(state: &mut State, bytes: &[u8]) {
    assert!(state.chat_keyboard(bytes, Instant::now(), true).is_empty());
}

#[tokio::test]
async fn full_keyboard_csi_and_ss3_select_and_commit_only_explicit_human_decisions() {
    for (case, (right, left)) in [
        (b"\x1b[C".as_slice(), b"\x1b[D".as_slice()),
        (b"\x1bOC".as_slice(), b"\x1bOD".as_slice()),
    ]
    .into_iter()
    .enumerate()
    {
        for (choice, width) in [(FinishChoice::Yes, 120), (FinishChoice::No, 48)] {
            let fixture = Fixture::new();
            saved::with_test_history(
                fixture.0.join("history"),
                queue::with_test_queue(async {
                    let request = queue::enqueue_for_test(
                        true,
                        "Finish this fixture?",
                        &format!("key-{case}"),
                    );
                    let id = request.question.id;
                    let mut state = state(width);
                    assert!(state.conversation.content_focus);
                    press(&mut state, right);
                    assert_eq!(state.conversation.view, View::Questions);
                    assert_eq!(state.question_choice(&id), FinishChoice::Yes);
                    assert_eq!(
                        queue::get_view(&id).unwrap().finish.unwrap().status,
                        FinishStatus::Pending
                    );
                    if choice == FinishChoice::No {
                        press(&mut state, left);
                    }
                    assert_eq!(state.conversation.view, View::Questions);
                    assert_eq!(state.question_choice(&id), choice);
                    press(&mut state, b"\r");
                    let decided = queue::get_view(&id).unwrap().finish.unwrap();
                    let decision = decided.decision.unwrap();
                    assert_eq!(decision.actor, DecisionActor::Human);
                    assert_eq!(decision.choice, choice);
                    assert_eq!(decided.approved, choice == FinishChoice::Yes);
                    assert_eq!(state.conversation.composer.text(), "keep the main draft");
                    assert!(state.actions.is_empty());
                }),
            )
            .await;
        }
    }
}

#[tokio::test]
async fn full_keyboard_question_editor_edges_do_not_leave_the_question_panel() {
    let fixture = Fixture::new();
    saved::with_test_history(
        fixture.0.join("history"),
        queue::with_test_queue(async {
            let id = queue::enqueue_for_test(false, "Which format?", "ordinary-key")
                .question
                .id;
            let mut state = state(120);
            press(&mut state, b"\x1b[D\x1b[C");
            assert_eq!(state.conversation.view, View::Questions);
            press(&mut state, b"Markdown\x1bOC\r");
            assert_eq!(
                queue::get_view(&id).unwrap().question.answer.as_deref(),
                Some("Markdown")
            );
            assert_eq!(state.conversation.composer.text(), "keep the main draft");
        }),
    )
    .await;
}

#[tokio::test]
async fn full_keyboard_empty_questions_list_arrows_move_between_panels() {
    let fixture = Fixture::new();
    saved::with_test_history(
        fixture.0.join("history"),
        queue::with_test_queue(async {
            let requests = queue::snapshot();
            assert!(queue::retained_views().is_empty());
            for (left, right) in [
                (b"\x1b[D".as_slice(), b"\x1b[C".as_slice()),
                (b"\x1bOD".as_slice(), b"\x1bOC".as_slice()),
            ] {
                for content_focus in [false, true] {
                    let mut state = state(120);
                    assert_eq!(state.conversation.view, View::Questions);
                    assert!(!state.conversation.detail);
                    assert!(state.conversation.question_id.is_none());
                    state.conversation.content_focus = content_focus;
                    state.conversation.target = Some("example-builder".into());
                    assert!(state.conversation.composer.left());
                    let draft = state.conversation.snapshot()["draft"].clone();
                    let queued_messages = state.chat_data()["queue"].clone();

                    press(&mut state, right);
                    assert_eq!(state.conversation.view, View::Logs);
                    press(&mut state, left);
                    assert_eq!(state.conversation.view, View::Questions);
                    assert_eq!(state.conversation.content_focus, content_focus);
                    assert!(!state.conversation.detail);
                    press(&mut state, left);
                    assert_eq!(state.conversation.view, View::Queue);

                    assert_eq!(state.conversation.snapshot()["draft"], draft);
                    assert_eq!(
                        state.conversation.target.as_deref(),
                        Some("example-builder")
                    );
                    assert_eq!(state.chat_data()["queue"], queued_messages);
                    assert_eq!(queue::snapshot(), requests);
                    assert!(state.conversation.submitting.is_none());
                    assert!(state.actions.is_empty());
                }
            }
        }),
    )
    .await;
}

#[tokio::test]
async fn full_keyboard_questions_list_arrows_preserve_selection_until_enter() {
    let fixture = Fixture::new();
    saved::with_test_history(
        fixture.0.join("history"),
        queue::with_test_queue(async {
            let ordinary = queue::enqueue_for_test(false, "Which format?", "nav-question")
                .question
                .id;
            let finish = queue::enqueue_for_test(true, "Ready to finish?", "nav-finish")
                .question
                .id;
            let requests = queue::retained_views();
            let mut state = state(120);
            assert_eq!(
                state.conversation.question_id.as_deref(),
                Some(ordinary.as_str())
            );
            assert!(state.conversation.detail);
            press(&mut state, b"partial answer");
            // A second escape prefix resolves the standalone Escape before Down.
            press(&mut state, b"\x1b\x1b[B");
            assert!(!state.conversation.detail);

            press(&mut state, b"\x1bOC");
            assert_eq!(state.conversation.view, View::Logs);
            press(&mut state, b"\x1bOD\x1bOD");
            assert_eq!(state.conversation.view, View::Queue);
            press(&mut state, b"\x1bOC");
            assert_eq!(state.conversation.view, View::Questions);
            assert!(!state.conversation.detail);

            press(&mut state, b"\r");
            assert!(state.conversation.detail);
            assert_eq!(
                state.conversation.question_id.as_deref(),
                Some(finish.as_str())
            );
            assert_eq!(
                state.conversation.answers[&ordinary].text(),
                "partial answer"
            );
            assert_eq!(queue::retained_views(), requests);
            assert_eq!(state.conversation.composer.text(), "keep the main draft");
            assert!(state.conversation.submitting.is_none());
            assert!(state.actions.is_empty());
        }),
    )
    .await;
}

#[tokio::test]
async fn question_and_finish_replies_stay_in_original_live_context_after_switch_and_release() {
    for finish in [false, true] {
        let fixture = Fixture::new();
        saved::with_test_history(
            fixture.0.join("history"),
            queue::with_test_queue(async {
                saved::record("user", "Second conversation seed").unwrap();
                let second = saved::current().unwrap().unwrap();
                saved::release_current().unwrap();
                saved::record("user", "First conversation seed").unwrap();
                let first = saved::current_owner().unwrap();
                let id = queue::enqueue_for_test(
                    finish,
                    "Original conversation question",
                    "history-answer",
                )
                .question
                .id;
                saved::resume_test_history(&second.metadata.id).unwrap();
                let recorded = if finish {
                    queue::decide_finish(&id, FinishChoice::Yes, Some("answer-for-A-only")).unwrap()
                } else {
                    queue::answer(&id, "answer-for-A-only").unwrap();
                    queue::get_view(&id).unwrap()
                };
                let mut state = state(120);
                state.preview = false;
                state.question_recorded(recorded.clone());
                assert_eq!(saved::current().unwrap().unwrap(), second);
                let answers = saved::read(first.id())
                    .unwrap()
                    .messages
                    .into_iter()
                    .filter(|message| message.kind == "answer")
                    .collect::<Vec<_>>();
                assert_eq!(answers.len(), 1);
                assert!(answers[0].content.contains("answer-for-A-only"));
                state.question_recorded(recorded.clone());
                assert_eq!(
                    saved::read(first.id())
                        .unwrap()
                        .messages
                        .iter()
                        .filter(|message| message.kind == "answer")
                        .count(),
                    1
                );
                state.chat_change_view(View::Activity);
                state.preview = true;
                let frame = state.chat_frame(Instant::now());
                let mut parser = vt100::Parser::new(
                    std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
                    std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
                    0,
                );
                parser.process(&frame.diff(None));
                assert!(!parser.screen().contents().contains("answer-for-A-only"));
                // Same ID with a new lease is not the old request's writer capability.
                saved::release_current().unwrap();
                saved::resume_test_history(first.id()).unwrap();
                assert_ne!(saved::current_owner().unwrap(), first);
                let unchanged = saved::current().unwrap().unwrap();
                state.preview = false;
                state.question_recorded(recorded);
                assert!(
                    state
                        .notice
                        .as_deref()
                        .unwrap()
                        .contains("history could not be saved")
                );
                assert_eq!(saved::current().unwrap().unwrap(), unchanged);
                assert_eq!(saved::read(&second.metadata.id).unwrap(), second);
            }),
        )
        .await;
        assert!(!fixture.0.join("history").exists());
    }
}
