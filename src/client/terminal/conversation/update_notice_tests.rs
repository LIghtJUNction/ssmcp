use crate::client::terminal::{Layout, State, Ui, nonzero, preview_state};
use std::time::Instant;

fn screen(state: &mut State) -> String {
    let frame = state.frame(Instant::now());
    let mut parser = vt100::Parser::new(
        nonzero(frame.geometry.rows),
        nonzero(frame.geometry.cols),
        0,
    );
    parser.process(&frame.diff(None));
    parser.screen().contents()
}

#[test]
fn startup_update_frame_is_readable_at_120_and_48_columns_without_changing_drafts() {
    for width in [120, 48] {
        let mut state = preview_state(
            30,
            width,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                ..Ui::default()
            },
        );
        state.conversation.composer.set_text("");
        state.conversation.content_focus = false;
        state.conversation.target = Some("chosen-agent".into());
        state.events.latest();
        state.update_notice = Some("ssmcp update --check · v1.2.4 available".into());
        let canvas = state.conversation.canvas.snapshot();
        let shown = screen(&mut state);
        assert!(shown.contains("ssmcp update --check"), "{width}: {shown}");
        assert!(shown.contains("v1.2.4"), "{width}: {shown}");
        assert!(state.conversation.composer.is_empty());
        assert_eq!(state.conversation.target.as_deref(), Some("chosen-agent"));
        assert_eq!(state.conversation.canvas.snapshot(), canvas);
        state.conversation.composer.set_text("unsent\nrequest");
        let cursor = state.conversation.composer.cursor();
        assert!(!screen(&mut state).contains("ssmcp update --check"));
        assert_eq!(state.conversation.composer.text(), "unsent\nrequest");
        assert_eq!(state.conversation.composer.cursor(), cursor);
        state.conversation.composer.set_text("");
        state.notice = Some("Input recovery required".into());
        let shown = screen(&mut state);
        assert!(shown.contains("Input recovery required"));
        assert!(!shown.contains("ssmcp update --check"));
        assert!(
            state.actions.is_empty(),
            "a notice never queues an update/install action"
        );
    }
}

#[test]
fn startup_update_frame_keeps_help_and_question_navigation_visible() {
    use super::View;
    for view in [
        View::Queue,
        View::Tasks,
        View::History,
        View::Questions,
        View::Agents,
        View::Audit,
        View::Logs,
    ] {
        let mut state = preview_state(
            30,
            120,
            &Ui {
                layout: Layout::Chat,
                sidebar: false,
                ..Ui::default()
            },
        );
        state.conversation.view = view;
        state.conversation.content_focus = true;
        let before = state.frame(Instant::now()).rows.pop().unwrap();
        state.update_notice = Some("ssmcp update --check · v1.2.4 available".into());
        let after = state.frame(Instant::now()).rows.pop().unwrap();
        assert_eq!(after, before, "{view:?}");
    }
    let mut state = preview_state(
        30,
        120,
        &Ui {
            layout: Layout::Split,
            ..Ui::default()
        },
    );
    state.update_notice = Some("ssmcp update --check · v1.2.4 available".into());
    assert!(screen(&mut state).contains("ssmcp update --check"));
    state.filter_editing = true;
    state.filter_draft = "error".into();
    let shown = screen(&mut state);
    assert!(shown.contains("Enter Apply"));
    assert!(!shown.contains("ssmcp update --check"));
}
