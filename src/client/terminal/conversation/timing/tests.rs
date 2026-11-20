use super::*;

fn entry(remaining_ms: Option<u64>, elapsed_ms: u64) -> Entry {
    Entry {
        id: "timer_1_1".into(),
        kind: "timer",
        label: "Owned fixture timer".into(),
        status: Status::Running,
        elapsed_ms,
        remaining_ms,
        cancellable: true,
        episode: None,
        attribution: None,
    }
}

fn screen(state: &mut State) -> String {
    let frame = state.chat_frame(Instant::now());
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
        std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
        0,
    );
    parser.process(&frame.diff(None));
    parser.screen().contents()
}

#[test]
fn clocks_round_deadlines_up_and_elapsed_down_without_negative_or_wrapped_time() {
    for (ms, countdown, elapsed) in [
        (0, "00:00", "00:00"),
        (1, "00:01", "00:00"),
        (1001, "00:02", "00:01"),
        (59_999, "01:00", "00:59"),
        (3_600_001, "1:00:01", "1:00:00"),
        (86_400_000, "24:00:00", "24:00:00"),
    ] {
        assert_eq!(duration(ms, true), countdown);
        assert_eq!(duration(ms, false), elapsed);
    }
    assert_eq!(counter(&entry(None, 2500)), "+00:02");
    assert_eq!(counter(&entry(Some(2500), 0)), "00:03");
}

#[test]
fn real_frames_keep_a_live_clock_visible_beside_a_draft_and_specific_notice() {
    for cols in [48, 120] {
        let mut state = super::super::super::preview_state(30, cols, &Ui::default());
        state
            .conversation
            .composer
            .set_text("keep this draft exactly");
        state.conversation.composer.left();
        let cursor = state.conversation.composer.cursor();
        state.notice = Some("Specific local notice".into());
        state.conversation.timing_fixture = Some(Snapshot {
            active: vec![entry(Some(12_001), 0)],
            recent: vec![],
        });
        let first = screen(&mut state);
        assert!(first.contains("Timer 00:13"), "{cols}: {first}");
        assert!(first.contains("Specific local notice"), "{cols}: {first}");
        state.conversation.timing_fixture.as_mut().unwrap().active[0].remaining_ms = Some(10_999);
        let later = screen(&mut state);
        assert!(later.contains("Timer 00:11"), "{cols}: {later}");
        state.conversation.timing_fixture.as_mut().unwrap().active[0] = entry(None, 65_999);
        let elapsed = screen(&mut state);
        assert!(elapsed.contains("Timer +01:05"), "{cols}: {elapsed}");
        assert_eq!(
            state.conversation.composer.text(),
            "keep this draft exactly"
        );
        assert_eq!(state.conversation.composer.cursor(), cursor);
        assert!(state.actions.is_empty());
        assert!(!state.conversation.exit_requested);
    }
}

#[test]
fn timing_details_preserve_full_cancel_ids_and_do_not_restore_live_actions() {
    let snapshot = Snapshot {
        active: vec![entry(Some(1000), 2000)],
        recent: vec![],
    };
    let text = detail(&snapshot).unwrap();
    assert!(text.contains("#timer_1_1"));
    assert!(text.contains("exact ID"));
    assert!(text.contains("never cancels its job, consumes input or approves"));
    assert!(hint(&snapshot).unwrap().width() <= 48);
    let preview = super::super::super::preview_state(30, 48, &Ui::default());
    assert!(preview.chat_timing_snapshot().active.is_empty());
    assert!(preview.chat_clock_label().is_none());
}
