use super::*;

fn durable_entry(id: &str, elapsed_ms: u64) -> Entry {
    use crate::wire::wait_display::{DurableObservation, DurableState, DurableStatus};
    let mut entry = observed_wait(WaitMode::All, WaitOutcome::Triggered);
    entry.id = id.into();
    entry.elapsed_ms = elapsed_ms;
    entry.durable = Some(DurableObservation {
        state: DurableState::Cached,
        result_status: Some(DurableStatus::Ready),
        started_at_ms: 1_795_000_000_000,
        completed_at_ms: Some(1_795_000_000_000 + elapsed_ms),
        first_read_at_ms: None,
        last_read_at_ms: None,
        acknowledged_at_ms: None,
        retained_until_ms: Some(1_795_001_800_000 + elapsed_ms),
        delivery_lag_ms: None,
        elapsed_total_ms: elapsed_ms,
        lifetime_ms: 3_600_000,
        read_window_ms: 55_000,
        read_windows: 2,
        replay_count: 0,
        cursor: 1,
    });
    entry
}

#[test]
fn retained_duration_samples_require_real_completion_and_exclude_cancelled_unknown_and_legacy() {
    use crate::wire::wait_display::DurableStatus;
    let first = durable_entry("first", 1000);
    let mut cancelled = durable_entry("cancelled", 999_999);
    cancelled.status = Status::Cancelled;
    cancelled.durable.as_mut().unwrap().result_status = Some(DurableStatus::Cancelled);
    let mut unknown = durable_entry("unknown", 999_999);
    unknown.status = Status::Unknown;
    let mut no_completion = durable_entry("no-completion", 999_999);
    no_completion.durable.as_mut().unwrap().completed_at_ms = None;
    let snapshot = Snapshot {
        active: vec![],
        recent: vec![
            first.clone(),
            first,
            durable_entry("second", 9000),
            durable_entry("third", 2000),
            cancelled,
            unknown,
            no_completion,
            observed_wait(WaitMode::Any, WaitOutcome::Triggered),
        ],
        prompts: vec![],
    };
    let stats = dashboard::Stats::from_snapshot(&snapshot);
    assert_eq!(
        (stats.count, stats.mean, stats.p50, stats.p95),
        (3, Some(4000), Some(2000), Some(9000))
    );
    assert_eq!((stats.cancelled, stats.unknown), (1, 2));
    assert_eq!(
        dashboard::Stats::from_snapshot(&Snapshot::default()).mean,
        None
    );
}

#[test]
fn durable_read_attempt_ack_and_stale_receipt_remain_distinct() {
    use crate::wire::wait_display::DurableState;
    let mut entry = durable_entry("read-states", 81_000);
    assert_eq!(dashboard::phase(&entry), dashboard::Phase::Unread);
    entry.durable.as_mut().unwrap().first_read_at_ms = Some(1_795_000_082_000);
    assert_eq!(dashboard::phase(&entry), dashboard::Phase::ReadAttempt);
    entry.durable.as_mut().unwrap().state = DurableState::Acknowledged;
    assert_eq!(dashboard::phase(&entry), dashboard::Phase::Acknowledged);
    entry.status = Status::Unknown;
    assert_eq!(dashboard::phase(&entry), dashboard::Phase::Unknown);
    let text = dashboard::entry_lines(&entry).join("\n");
    assert!(text.contains("本轮读取窗口 55s"));
    assert!(text.contains("生命周期上限 1:00:00"));
    assert!(text.contains("显式确认 · 未知"));
}

fn dashboard_fixture() -> Snapshot {
    use crate::wire::wait_display::{DurableState, DurableStatus};
    let mut active = durable_entry("background-original-id", 83_000);
    active.status = Status::Running;
    active.wait.as_mut().unwrap().outcome = WaitOutcome::Pending;
    active.remaining_ms = Some(3_517_000);
    let durable = active.durable.as_mut().unwrap();
    durable.state = DurableState::Waiting;
    durable.result_status = None;
    durable.completed_at_ms = None;
    durable.retained_until_ms = None;
    durable.cursor = 0;
    let mut cancelled = durable_entry("cancelled", 4000);
    cancelled.status = Status::Cancelled;
    cancelled.durable.as_mut().unwrap().result_status = Some(DurableStatus::Cancelled);
    let mut unknown = durable_entry("unknown", 6000);
    unknown.status = Status::Unknown;
    Snapshot {
        active: vec![active],
        recent: vec![
            durable_entry("completed-unread", 81_000),
            cancelled,
            unknown,
        ],
        prompts: vec![crate::runtime::time::PromptSummary {
            id: "schedule-fixture".into(),
            status: crate::runtime::time::PromptStatus::Scheduled,
            scope: "native_connection",
            created_at_unix_ms: 1_795_000_000_000,
            expires_at_unix_ms: 1_795_003_600_000,
            lifetime_ms: 3_600_000,
            next_fire_unix_ms: Some(1_795_000_120_000),
            interval_ms: Some(60_000),
            last_fired_at_unix_ms: Some(1_795_000_060_000),
            pending_count: 1,
            visibility: "queued",
            missed_count: 2,
            tick: 3,
            retention_after_ack_ms: 300_000,
        }],
    }
}

#[test]
fn dedicated_wait_view_renders_narrow_wide_light_dark_without_changing_draft_or_queue() {
    for theme in [
        crate::client::ui::Theme::Light,
        crate::client::ui::Theme::Dark,
    ] {
        let ui = Ui {
            theme,
            ..Ui::default()
        };
        for cols in [48, 120] {
            let mut state = super::super::super::preview_state(40, cols, &ui);
            state.events.clear();
            state.panel.tasks.clear();
            state.conversation.timing_fixture = Some(dashboard_fixture());
            state.conversation.composer.set_text("KEEP_WAIT_DRAFT");
            let cursor = state.conversation.composer.cursor();
            state.chat_change_view(View::Waits);
            let rendered = screen(&mut state);
            assert!(rendered.contains("等待与计时"), "{rendered}");
            assert!(rendered.contains("真实完成样本"), "{rendered}");
            assert!(rendered.contains("进行中"), "{rendered}");
            state.chat_move(15);
            let _ = screen(&mut state);
            state.chat_escape();
            assert_eq!(state.conversation.view, View::Activity);
            assert_eq!(state.conversation.composer.text(), "KEEP_WAIT_DRAFT");
            assert_eq!(state.conversation.composer.cursor(), cursor);
            assert!(state.actions.is_empty());
            assert!(!state.conversation.exit_requested);
        }
    }
}

#[test]
#[ignore = "export only with the coordinator's matching final test binary and an owned evidence directory"]
fn export_wait_dashboard_frames() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("SSMCP_WAIT_DASHBOARD_FIXTURE_DIR")
            .expect("owned fixture output directory"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    for (theme_name, theme) in [
        ("light", crate::client::ui::Theme::Light),
        ("dark", crate::client::ui::Theme::Dark),
    ] {
        let ui = Ui {
            theme,
            ..Ui::default()
        };
        for cols in [120, 48] {
            let mut state = super::super::super::preview_state(40, cols, &ui);
            state.events.clear();
            state.panel.tasks.clear();
            state.conversation.composer.set_text("KEEP_WAIT_DRAFT");
            state.conversation.timing_fixture = Some(dashboard_fixture());
            state.chat_change_view(View::Waits);
            for (part, offset) in [("active", 0), ("plans", usize::MAX)] {
                state.conversation.detail_scroll = offset;
                let frame = state.chat_frame(Instant::now());
                let stem = format!("wait-dashboard-{theme_name}-{cols}-{part}");
                std::fs::write(directory.join(format!("{stem}.ansi")), frame.diff(None)).unwrap();
                std::fs::write(directory.join(format!("{stem}.json")), serde_json::to_vec(&serde_json::json!({
                    "rows":40,"cols":cols,"theme":theme_name,"part":part,"renderer":"conversation::chat_frame","fixture":"synthetic"})).unwrap()).unwrap();
            }
        }
    }
}

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
        durable: None,
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
            prompts: vec![],
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
        prompts: vec![],
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
        prompts: vec![],
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
            prompts: vec![],
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
            prompts: vec![],
        }
    } else {
        Snapshot {
            active: vec![],
            recent: vec![entry],
            prompts: vec![],
        }
    }
}

#[test]
fn fractional_wait_limits_bound_live_clocks_and_receipts_stop_counting_down() {
    fn sample(outcome: WaitOutcome, elapsed: u64, window: u64) -> Entry {
        let mut entry = observed_wait(WaitMode::Any, outcome);
        entry.wait.as_mut().unwrap().window_ms = Some(window);
        entry.elapsed_ms = elapsed;
        entry.remaining_ms = Some(window.saturating_sub(elapsed));
        entry
    }
    for (elapsed, window, limit, bound) in [
        (42, 6500, "00:06.500", "00:06.500"),
        (625, 6500, "00:06.500", "00:06"),
        (1, 250, "250ms", "250ms"),
        (250, 250, "250ms", "00:00"),
    ] {
        let entry = sample(WaitOutcome::Pending, elapsed, window);
        let card = wait_card(&entry).unwrap();
        assert!(
            card.detail
                .contains(&format!("window limit {limit} · remaining ≤ {bound}"))
        );
        if elapsed < window {
            assert_eq!(clock_label(&entry), format!("观察剩余 ≤ {bound}"));
        }
    }
    for outcome in [
        WaitOutcome::Triggered,
        WaitOutcome::WindowExpired,
        WaitOutcome::ObservationStopped,
        WaitOutcome::Unknown,
    ] {
        let entry = sample(outcome, 42, 6500);
        let card = wait_card(&entry).unwrap();
        assert!(
            card.detail
                .contains("Elapsed 42ms · window limit 00:06.500")
        );
        assert!(!card.detail.contains(" · remaining"));
        assert!(
            dashboard::entry_lines(&entry)
                .iter()
                .any(|row| row == "本轮读取窗口 00:06.500")
        );
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
            assert!(card.heading.contains(compact_wait_outcome(outcome)));
            assert!(
                card.detail
                    .contains(&format!("Outcome: {}", wait_outcome(outcome)))
            );
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
        let original = state.events.snapshot()["events"][0]["id"].clone();
        for outcome in [
            WaitOutcome::Pending,
            WaitOutcome::WindowExpired,
            WaitOutcome::Triggered,
            WaitOutcome::ObservationStopped,
            WaitOutcome::Unknown,
        ] {
            state.events.back();
            state.conversation.timing_fixture =
                Some(wait_snapshot(observed_wait(WaitMode::All, outcome)));
            let collapsed = screen(&mut state);
            let budget = if outcome == WaitOutcome::WindowExpired {
                "20s/20s"
            } else {
                "2s/20s"
            };
            for visible in ["ALL", compact_wait_outcome(outcome), budget, "Command"] {
                assert!(
                    collapsed.contains(visible),
                    "{cols}: {visible}: {collapsed}"
                );
            }
            assert_eq!(state.events.snapshot()["retained_events"], 1);
            assert_eq!(state.events.snapshot()["events"][0]["id"], original);
            assert_eq!(state.events.snapshot()["events"][0]["expanded"], false);
            assert_eq!(state.conversation.composer.text(), "KEEP_WAIT_DRAFT");
            assert_eq!(state.conversation.composer.cursor(), cursor);
            state.events.open();
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
