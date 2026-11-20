use super::*;

fn entry(remaining_ms: Option<u64>, elapsed_ms: u64) -> Entry {
    Entry {
        id: "timer_1_1".into(),
        kind: "timer",
        label: "Owned fixture timer".into(),
        status: Status::Running,
        elapsed_ms,
        remaining_ms,
        deadline_unix_ms: None,
        finished_age_ms: None,
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
    assert_eq!(clock_label(&entry(None, 2500)), "计时 00:02");
    assert_eq!(clock_label(&entry(Some(2500), 0)), "剩余 00:03");
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
        assert!(first.contains("剩余 00:13"), "{cols}: {first}");
        assert!(first.contains("Specific local notice"), "{cols}: {first}");
        state.conversation.timing_fixture.as_mut().unwrap().active[0].remaining_ms = Some(10_999);
        let later = screen(&mut state);
        assert!(later.contains("剩余 00:11"), "{cols}: {later}");
        state.conversation.timing_fixture.as_mut().unwrap().active[0] = entry(None, 65_999);
        let elapsed = screen(&mut state);
        assert!(elapsed.contains("计时 01:05"), "{cols}: {elapsed}");
        for frame in [&first, &later, &elapsed] {
            let divider = frame
                .lines()
                .find(|row| row.contains("剩余 ") || row.contains("计时 "))
                .unwrap();
            assert!(
                divider.contains("电脑控制：未确认")
                    || divider.contains("FG ? · BG on")
                    || divider.contains("F:? B:on"),
                "{cols}: {divider}"
            );
        }
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
    assert!(text.contains("计时到期不代表任务完成；取消计时不会停止任务"));
    assert!(hint(&snapshot).unwrap().width() <= 48);
    let preview = super::super::super::preview_state(30, 48, &Ui::default());
    assert!(preview.chat_timing_snapshot().active.is_empty());
    assert!(preview.chat_clock_label().is_none());
}

#[test]
fn nearest_real_deadline_and_short_expiry_receipt_win_over_open_elapsed_work() {
    let mut open = entry(None, 10_000);
    open.kind = "task";
    let mut late = entry(Some(15_000), 0);
    late.id = "later".into();
    let mut early = entry(Some(5_001), 0);
    early.id = "earlier".into();
    let mut snapshot = Snapshot {
        active: vec![open.clone(), late, early],
        recent: vec![],
    };
    assert_eq!(selected(&snapshot).unwrap().id, "earlier");
    assert_eq!(clock_label(selected(&snapshot).unwrap()), "剩余 00:06");
    snapshot.active = vec![open];
    let mut ended = entry(Some(0), 5000);
    ended.status = Status::Elapsed;
    ended.finished_age_ms = Some(9999);
    ended.cancellable = false;
    snapshot.recent.push(ended);
    assert_eq!(clock_label(selected(&snapshot).unwrap()), "计时 已到期");
    snapshot.recent[0].finished_age_ms = Some(10_000);
    assert_eq!(clock_label(selected(&snapshot).unwrap()), "运行 00:10");
    assert!(detail(&snapshot).unwrap().contains("已到期"));
}

#[test]
fn native_job_time_stays_live_without_events_and_only_changes_on_second_buckets() {
    let now = Instant::now();
    let mut state = super::super::super::preview_state(30, 48, &Ui::default());
    state.conversation.timing_fixture = Some(Snapshot::default());
    state.panel.tasks = vec![crate::client::panel::TaskView {
        id: "#native".into(),
        description: "独立任务 · fixture".into(),
        cwd: None,
        started: now,
        deadline: None,
    }];
    assert_eq!(
        state.chat_clock_label_at(now).as_deref(),
        Some("运行 00:00")
    );
    assert_eq!(
        state
            .chat_clock_label_at(now + Duration::from_millis(999))
            .as_deref(),
        Some("运行 00:00")
    );
    assert_eq!(
        state
            .chat_clock_label_at(now + Duration::from_secs(1))
            .as_deref(),
        Some("运行 00:01")
    );
    state.panel.tasks[0].deadline = Some(now + Duration::from_millis(2500));
    assert_eq!(
        state
            .chat_clock_label_at(now + Duration::from_millis(500))
            .as_deref(),
        Some("剩余 00:02")
    );
    let late = state.chat_timing_snapshot_at(now + Duration::from_secs(3));
    assert_eq!(late.active[0].status, Status::Running);
    assert_eq!(late.active[0].remaining_ms, Some(0));
    assert!(!late.active[0].cancellable);
    assert_eq!(
        state
            .chat_clock_label_at(now + Duration::from_secs(3))
            .as_deref(),
        Some("运行·已到期")
    );
    assert!(detail(&late).unwrap().contains("仍等待实际操作返回"));
    assert_eq!(
        state.panel.tasks.len(),
        1,
        "rendering cannot finish the job"
    );
    state.panel.tasks.clear();
    assert!(
        state
            .chat_clock_label_at(now + Duration::from_secs(4))
            .is_none()
    );
}

#[test]
fn mixed_width_time_labels_fit_real_frames_without_changing_draft_or_actions() {
    for cols in [24, 32, 48, 120] {
        let mut state = super::super::super::preview_state(30, cols, &Ui::default());
        state.conversation.composer.set_text("中文 draft 不变");
        state.panel.tasks.clear();
        let mut ended = entry(Some(0), 1234);
        ended.status = Status::Elapsed;
        ended.finished_age_ms = Some(1000);
        ended.cancellable = false;
        state.conversation.timing_fixture = Some(Snapshot {
            active: vec![],
            recent: vec![ended],
        });
        let frame = screen(&mut state);
        assert!(
            frame.lines().all(|line| line.width() <= usize::from(cols)),
            "{cols}: {frame}"
        );
        if cols >= 32 {
            assert!(frame.contains("已到期"), "{cols}: {frame}");
        }
        assert_eq!(state.conversation.composer.text(), "中文 draft 不变");
        assert!(state.actions.is_empty());
    }
}
