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
        wait: None,
    }
}

fn parse(state: &mut State) -> vt100::Parser {
    let frame = state.chat_frame(Instant::now());
    let mut parser = vt100::Parser::new(
        std::num::NonZeroU16::new(frame.geometry.rows).unwrap(),
        std::num::NonZeroU16::new(frame.geometry.cols).unwrap(),
        0,
    );
    parser.process(&frame.diff(None));
    parser
}

fn screen(state: &mut State) -> String {
    parse(state).screen().contents()
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
        state.notice = None;
        let parsed = parse(&mut state);
        assert!(parsed.screen().contents().contains("计时 · 剩余 00:13"));
        assert!(!parsed.screen().contents().contains("等待你的输入"));
        assert_ne!(
            parsed.screen().cell(29, 1).unwrap().fgcolor(),
            vt100::Color::Rgb(242, 139, 130)
        );
        state.conversation.timing_fixture.as_mut().unwrap().active[0].remaining_ms = Some(10_999);
        let later = screen(&mut state);
        assert!(later.contains("剩余 00:11"), "{cols}: {later}");
        state.conversation.timing_fixture.as_mut().unwrap().active[0] = entry(None, 65_999);
        let elapsed = screen(&mut state);
        assert!(elapsed.contains("计时 01:05"), "{cols}: {elapsed}");
        let mut observation = entry(Some(5_001), 20_000);
        observation.kind = "observation";
        observation.cancellable = false;
        state.conversation.timing_fixture.as_mut().unwrap().active[0] = observation;
        let wait = screen(&mut state);
        assert!(wait.contains("观察剩余 00:06"), "{cols}: {wait}");
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

use crate::wire::wait_display::{Mode as WaitMode, Outcome as WaitOutcome};

fn observed_wait(mode: WaitMode, outcome: WaitOutcome) -> Entry {
    let mut entry = entry(Some(18_000), 2_000);
    entry.id = "watch_fixture_connection_0123456789abcdef_exact".into();
    entry.kind = "relay_wait";
    entry.label = "Owned wait fixture".into();
    entry.episode = Some(3);
    entry.cancellable = false;
    entry.wait = Some(crate::runtime::time::WaitView {
        operation: "await".into(),
        mode,
        targets: vec!["Command exits".into(), "Worker task result".into()],
        references: vec!["task_id: worker_task_exact_fixture_0123456789abcdef".into()],
        outcome,
        window_ms: Some(20_000),
    });
    if outcome != WaitOutcome::Pending {
        entry.status = match outcome {
            WaitOutcome::Triggered => Status::Returned,
            WaitOutcome::WindowExpired => Status::Expired,
            WaitOutcome::ObservationStopped => Status::Cancelled,
            _ => Status::Unknown,
        };
        entry.finished_age_ms = Some(0);
        if outcome == WaitOutcome::WindowExpired {
            entry.elapsed_ms = 20_000;
            entry.remaining_ms = Some(0);
        }
    }
    entry
}

fn wait_snapshot(entry: Entry) -> Snapshot {
    if entry.wait.as_ref().unwrap().outcome == WaitOutcome::Pending {
        Snapshot {
            active: vec![entry],
            recent: vec![],
        }
    } else {
        Snapshot {
            active: vec![],
            recent: vec![entry],
        }
    }
}

#[test]
fn wait_cards_explain_any_all_budget_and_terminal_outcomes_without_id_noise() {
    for mode in [WaitMode::Any, WaitMode::All] {
        for outcome in [
            WaitOutcome::Pending,
            WaitOutcome::Triggered,
            WaitOutcome::WindowExpired,
            WaitOutcome::ObservationStopped,
            WaitOutcome::Unknown,
        ] {
            let entry = observed_wait(mode, outcome);
            let card = wait_card(&entry).unwrap();
            assert!(card.heading.contains("Command exits"));
            assert!(card.heading.contains(wait_mode(mode)));
            assert!(card.heading.contains(wait_outcome(outcome)));
            assert!(!card.heading.contains(&entry.id));
            assert!(!card.heading.contains("worker_task_exact_fixture"));
            assert!(card.detail.contains(&entry.id));
            assert!(card.detail.contains("window limit 00:20"));
            assert!(card.detail.contains("Worker task result"));
            if mode == WaitMode::Any {
                assert!(card.detail.contains("at least one target"));
            } else {
                assert!(card.detail.contains("every target must report ready"));
            }
            match outcome {
                WaitOutcome::WindowExpired => {
                    assert!(
                        card.detail
                            .contains("does not cancel execution or establish task completion")
                    )
                }
                WaitOutcome::ObservationStopped => {
                    assert!(card.detail.contains("does not cancel execution"))
                }
                WaitOutcome::Unknown => assert!(card.detail.contains("Do not infer success")),
                _ => {}
            }
        }
    }
}

#[test]
fn real_wait_frames_keep_one_card_visible_and_preserve_draft_at_wide_and_narrow_widths() {
    for cols in [120, 48] {
        let mut state = super::super::super::preview_state(36, cols, &Ui::default());
        state.events.clear();
        state.panel.tasks.clear();
        state.conversation.composer.set_text("KEEP_WAIT_DRAFT");
        state.conversation.composer.left();
        let cursor = state.conversation.composer.cursor();
        state.conversation.timing_fixture = Some(wait_snapshot(observed_wait(
            WaitMode::All,
            WaitOutcome::Pending,
        )));
        let collapsed = screen(&mut state);
        assert!(collapsed.contains("Command exits"), "{cols}: {collapsed}");
        assert!(collapsed.contains("ALL"), "{cols}: {collapsed}");
        assert!(collapsed.contains("Pending"), "{cols}: {collapsed}");
        assert_eq!(state.events.snapshot()["retained_events"], 1);
        state.events.open();
        let original = state.events.snapshot()["events"][0]["id"].clone();
        for outcome in [
            WaitOutcome::Pending,
            WaitOutcome::WindowExpired,
            WaitOutcome::Triggered,
            WaitOutcome::ObservationStopped,
            WaitOutcome::Unknown,
        ] {
            state.conversation.timing_fixture =
                Some(wait_snapshot(observed_wait(WaitMode::All, outcome)));
            let rendered = screen(&mut state);
            assert!(
                rendered.contains(wait_outcome(outcome)),
                "{cols}: {rendered}"
            );
            assert!(rendered.contains("Command exits"), "{cols}: {rendered}");
            assert!(
                rendered
                    .lines()
                    .all(|line| line.width() <= usize::from(cols))
            );
            assert_eq!(state.events.snapshot()["retained_events"], 1);
            assert_eq!(state.events.snapshot()["events"][0]["id"], original);
            assert_eq!(state.events.snapshot()["events"][0]["expanded"], true);
            assert_eq!(state.conversation.composer.text(), "KEEP_WAIT_DRAFT");
            assert_eq!(state.conversation.composer.cursor(), cursor);
            assert!(state.actions.is_empty());
        }
    }
}

#[test]
fn disappearing_wait_is_unknown_and_never_left_as_a_live_pending_card() {
    let mut feed = WaitFeed::default();
    let first = feed.update(
        wait_snapshot(observed_wait(WaitMode::Any, WaitOutcome::Pending)),
        None,
    );
    assert_eq!(first.len(), 1);
    let missing = feed.update(Snapshot::default(), None);
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].key, first[0].key);
    assert!(missing[0].heading.contains("Unknown"));
    assert!(!missing[0].heading.contains("Triggered"));
    assert!(feed.update(Snapshot::default(), None).is_empty());
}

#[test]
#[ignore = "export actual renderer frames using the coordinator's frozen test binary"]
fn export_wait_observation_frames() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("SSMCP_WAIT_FIXTURE_DIR").expect("owned fixture output directory"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    for cols in [120, 48] {
        for (name, mode, outcome) in [
            ("pending-any", WaitMode::Any, WaitOutcome::Pending),
            ("pending-all", WaitMode::All, WaitOutcome::Pending),
            ("triggered", WaitMode::Any, WaitOutcome::Triggered),
            ("window-expired", WaitMode::All, WaitOutcome::WindowExpired),
            (
                "observation-stopped",
                WaitMode::Any,
                WaitOutcome::ObservationStopped,
            ),
            ("unknown", WaitMode::All, WaitOutcome::Unknown),
        ] {
            let mut state = super::super::super::preview_state(36, cols, &Ui::default());
            state.events.clear();
            state.panel.tasks.clear();
            state.conversation.composer.set_text("KEEP_WAIT_DRAFT");
            state.conversation.timing_fixture = Some(wait_snapshot(observed_wait(mode, outcome)));
            for expanded in [false, true] {
                if expanded {
                    state.events.open();
                }
                let frame = state.chat_frame(Instant::now());
                let suffix = if expanded { "detail" } else { "summary" };
                let stem = format!("wait-{name}-{cols}-{suffix}");
                std::fs::write(directory.join(format!("{stem}.ansi")), frame.diff(None)).unwrap();
                std::fs::write(
                    directory.join(format!("{stem}.json")),
                    serde_json::to_vec(&serde_json::json!({"rows":36,"cols":cols,"scenario":name,
                        "expanded":expanded,"renderer":"conversation::chat_frame"}))
                    .unwrap(),
                )
                .unwrap();
            }
        }
    }
}
