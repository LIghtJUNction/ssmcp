use super::*;
use crate::{
    runtime::call_scope::CallScope,
    wire::{ControlAccount, ControlAgent, ControlAttribution},
};

fn owner(connection: u64, subject: &str, agent: char) -> Owner {
    let attribution = ControlAttribution {
        account: ControlAccount {
            provider: "github".into(),
            subject: subject.into(),
            login: "fixture-owner".into(),
        },
        owner: None,
        agent: Some(ControlAgent {
            id: format!("agent_{}", agent.to_string().repeat(32)),
            name: "fixture participant".into(),
        }),
    };
    let call = CallScope::from_relay(connection, 1, Some(&attribution), None).unwrap();
    Owner {
        connection: Some(call.connection()),
        attribution: call.attribution().cloned(),
        thread: None,
        history: None,
    }
}

#[test]
fn deadlines_are_monotonic_and_expiry_stops_elapsed_at_the_deadline() {
    let clock = Clock::new().unwrap();
    let now = Instant::now();
    let owner = owner(7, "123", 'a');
    let id = clock.start(owner.clone(), Some(2500), None, now).unwrap();
    let before = clock
        .read(&owner, &id, now + Duration::from_millis(999))
        .unwrap()
        .0;
    assert_eq!(
        (before.elapsed_ms, before.remaining_ms, before.status),
        (999, Some(1501), Status::Running)
    );
    let late = clock
        .read(&owner, &id, now + Duration::from_secs(10))
        .unwrap()
        .0;
    assert_eq!(
        (late.elapsed_ms, late.remaining_ms, late.status),
        (2500, Some(0), Status::Elapsed)
    );
    assert!(!late.cancellable);
    let wall = clock.now(json!({})).unwrap();
    assert_eq!(wall["deadline_clock"], "monotonic");
    assert_eq!(wall["wall_clock"], "UTC Unix epoch");
    assert!(wall["unix_ms"].as_u64().unwrap() >= 1_000_000_000_000);
    assert!(wall["monotonic_ms"].as_u64().unwrap() <= millis(clock.started.elapsed()));
    assert_eq!(receipt(late, false)["completion_permission"], false);
}

#[test]
fn timer_ids_cannot_cross_account_participant_or_connection_and_cancellation_is_final() {
    let clock = Clock::new().unwrap();
    let now = Instant::now();
    let original = owner(9, "123", 'a');
    let id = clock
        .start(original.clone(), Some(30_000), None, now)
        .unwrap();
    for other in [
        owner(10, "123", 'a'),
        owner(9, "456", 'a'),
        owner(9, "123", 'b'),
    ] {
        assert!(clock.read(&other, &id, now).is_err());
        assert!(clock.cancel_timer(&other, &id, now).is_err());
    }
    let stopped = clock
        .cancel_timer(&original, &id, now + Duration::from_secs(2))
        .unwrap();
    assert_eq!(
        (stopped.status, stopped.elapsed_ms, stopped.remaining_ms),
        (Status::Cancelled, 2000, Some(28_000))
    );
    let repeated = clock
        .cancel_timer(&original, &id, now + Duration::from_secs(35))
        .unwrap();
    assert_eq!(
        (repeated.status, repeated.elapsed_ms),
        (Status::Cancelled, 2000)
    );
    clock.close();
    assert!(clock.read(&original, &id, now).is_err());
    assert!(clock.snapshot(&None, now).active.is_empty());
    let replacement = Clock::new().unwrap();
    assert!(replacement.read(&original, &id, now).is_err());
}

#[test]
fn timer_capacity_retention_and_elapsed_lifetime_are_bounded() {
    let clock = Clock::new().unwrap();
    let now = Instant::now();
    let original = owner(1, "123", 'a');
    let mut ids = Vec::new();
    for _ in 0..MAX_TIMERS {
        ids.push(clock.start(original.clone(), None, None, now).unwrap());
    }
    assert!(clock.start(original.clone(), Some(1), None, now).is_err());
    let elapsed = clock
        .read(&original, &ids[0], now + Duration::from_secs(5))
        .unwrap()
        .0;
    assert_eq!(
        (elapsed.elapsed_ms, elapsed.remaining_ms, elapsed.status),
        (5000, None, Status::Running)
    );
    clock.cancel_timer(&original, &ids[0], now).unwrap();
    assert!(clock.start(original.clone(), Some(0), None, now).is_ok());
    for _ in 0..(MAX_RECORDS * 2) {
        clock.start(original.clone(), Some(0), None, now).unwrap();
    }
    assert!(clock.state.lock().unwrap().timers.len() <= MAX_RECORDS);
    for id in ids.iter().skip(1) {
        assert_eq!(
            clock
                .read(&original, id, now + Duration::from_millis(MAX_DURATION_MS))
                .unwrap()
                .0
                .status,
            Status::Expired
        );
    }
    assert!(
        clock
            .snapshot(
                &None,
                now + Duration::from_millis(MAX_DURATION_MS) + RETENTION
            )
            .active
            .is_empty()
    );
    assert!(clock.state.lock().unwrap().timers.is_empty());
}

#[tokio::test]
async fn timer_and_late_observation_stay_with_the_original_history_lease() {
    let clock = Clock::new().unwrap();
    let path = std::env::temp_dir().join(format!(
        "ssmcp-time-history-{}-{}",
        std::process::id(),
        clock.id
    ));
    std::fs::create_dir_all(&path).unwrap();
    history::with_test_history(path.clone(), async {
        let source_a = Source::capture();
        let owner_a = Owner::from_source(&source_a);
        history::record("user", "timer history A").unwrap();
        let id = clock
            .start(owner_a.clone(), None, None, Instant::now())
            .unwrap();
        let observation = clock
            .observe("fixture.wait", &json!({}), &source_a)
            .unwrap();
        assert_eq!(
            clock
                .snapshot(&history::current_owner(), Instant::now())
                .active
                .len(),
            2
        );
        history::release_current().unwrap();
        let source_b = Source::capture();
        history::record("user", "timer history B").unwrap();
        assert!(
            clock
                .snapshot(&history::current_owner(), Instant::now())
                .active
                .is_empty()
        );
        assert!(
            clock
                .cancel_local(&id, &history::current_owner(), Instant::now())
                .is_err()
        );
        assert!(
            clock
                .cancel_timer(&Owner::from_source(&source_b), &id, Instant::now())
                .is_err()
        );
        history::resume(source_a.owner().unwrap().id()).unwrap();
        assert_ne!(history::current_owner(), source_a.owner().cloned());
        assert!(
            clock
                .snapshot(&history::current_owner(), Instant::now())
                .active
                .is_empty()
        );
        assert!(
            clock
                .cancel_timer(&Owner::from_source(&Source::capture()), &id, Instant::now())
                .is_err()
        );
        drop(observation);
        assert_eq!(
            clock.read(&owner_a, &id, Instant::now()).unwrap().0.status,
            Status::Running
        );
    })
    .await;
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn bounded_wait_timeout_preserves_timer_and_local_cancel_wakes_the_same_id() {
    let clock = Clock::new().unwrap();
    Source::default()
        .scope(async {
            let first = clock
                .wait(json!({"duration_ms":60_000,"wait_ms":1}))
                .await
                .unwrap();
            let id = first["timer_id"].as_str().unwrap();
            assert_eq!(first["status"], "running");
            assert_eq!(first["wait_timed_out"], true);
            let pending = clock.wait(json!({"op":"status","timer_id":id,"wait_ms":25_000}));
            tokio::pin!(pending);
            tokio::select! {
                result = &mut pending => panic!("timer finished early: {result:?}"),
                _ = tokio::task::yield_now() => {},
            }
            clock.cancel_local(id, &None, Instant::now()).unwrap();
            let result = tokio::time::timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result["timer_id"], id);
            assert_eq!(result["status"], "cancelled");
            assert_eq!(result["completion_permission"], false);
        })
        .await;
}

#[tokio::test]
async fn observation_drop_and_cancel_only_remove_the_observer() {
    let clock = Clock::new().unwrap();
    let source = Source::default();
    let timer = clock
        .start(
            Owner::from_source(&source),
            Some(60_000),
            None,
            Instant::now(),
        )
        .unwrap();
    let mut waiting = clock
        .observe(
            "ssmcp.exec.wait",
            &json!({"job_id":"existing-job","wait_ms":25_000}),
            &source,
        )
        .unwrap();
    let displayed = clock.snapshot(&None, Instant::now());
    let entry = displayed
        .active
        .iter()
        .find(|entry| entry.id == waiting.id)
        .unwrap();
    assert!(
        entry
            .remaining_ms
            .is_some_and(|remaining| remaining <= 25_000)
    );
    clock
        .cancel_local(&waiting.id, &None, Instant::now())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), waiting.cancelled())
        .await
        .unwrap();
    drop(waiting);
    assert!(clock.state.lock().unwrap().observations.is_empty());
    assert_eq!(
        clock
            .read(&Owner::from_source(&source), &timer, Instant::now())
            .unwrap()
            .0
            .status,
        Status::Running
    );
    let elapsed = clock.observe("fixture.wait", &json!({}), &source).unwrap();
    let entry = clock
        .snapshot(&None, Instant::now())
        .active
        .into_iter()
        .find(|entry| entry.id == elapsed.id)
        .unwrap();
    assert_eq!(entry.remaining_ms, None);
    assert!(!entry.cancellable);
    assert!(
        clock
            .cancel_local(&elapsed.id, &None, Instant::now())
            .is_err()
    );
    assert!(
        clock
            .observe("ssmcp.exec.start", &json!({"command":["fixture"]}), &source)
            .is_none()
    );
    let finish = clock
        .observe("ssmcp.ui.finish", &json!({"timeout_ms":1000}), &source)
        .unwrap();
    assert!(
        !clock
            .snapshot(&None, Instant::now())
            .active
            .into_iter()
            .find(|entry| entry.id == finish.id)
            .unwrap()
            .cancellable
    );
    assert!(clock.observe("ssmcp.ui.get", &json!({}), &source).is_none());
}

#[tokio::test]
async fn malformed_time_requests_and_control_labels_never_admit_work() {
    let clock = Clock::new().unwrap();
    for arguments in [
        json!({"duration_ms":MAX_DURATION_MS+1}),
        json!({"duration_ms":1,"wait_ms":MAX_WAIT_MS+1}),
        json!({"op":"cancel","timer_id":"timer_unknown","wait_ms":0}),
        json!({"duration_ms":1,"approved":true}),
        json!({"duration_ms":1,"label":"escape\u{1b}[2J"}),
        json!({"duration_ms":1,"label":"line\u{2028}break"}),
    ] {
        assert!(clock.wait(arguments).await.is_err());
    }
    assert!(clock.state.lock().unwrap().timers.is_empty());
    let label = clean_label(Some("API_TOKEN=private_fixture_clock_secret".into()))
        .unwrap()
        .unwrap();
    assert!(!label.contains("private_fixture_clock_secret"));
    assert!(clock.now(json!({"set_unix_ms":0})).is_err());
}

#[test]
fn fresh_clock_namespace_rejects_old_timer_and_observer_ids_even_for_the_same_owner() {
    let source = Source::default();
    let owner = Owner::from_source(&source);
    let now = Instant::now();
    let original = Clock::new().unwrap();
    let old_timer = original.start(owner.clone(), None, None, now).unwrap();
    let old_watch = original
        .observe("ssmcp.exec.wait", &json!({"wait_ms":25_000}), &source)
        .unwrap();
    let old_watch_id = old_watch.id.clone();
    let original_scope = original.now(json!({})).unwrap()["monotonic_scope"].clone();
    original.close();

    // Both fresh clocks start their object sequence at zero. Their scope must
    // be a random namespace, not a process-local counter that reload resets.
    let replacement = Clock::new().unwrap();
    for scope in [&original.id, &replacement.id] {
        assert_eq!(scope.len(), 32);
        assert!(scope.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
    assert_ne!(original.id, replacement.id);
    let timer = replacement.start(owner.clone(), None, None, now).unwrap();
    let watch = replacement
        .observe("ssmcp.exec.wait", &json!({"wait_ms":25_000}), &source)
        .unwrap();
    assert_ne!(old_timer, timer);
    assert_ne!(old_watch_id, watch.id);
    assert_ne!(
        original_scope,
        replacement.now(json!({})).unwrap()["monotonic_scope"]
    );
    assert!(replacement.read(&owner, &old_timer, now).is_err());
    assert!(replacement.cancel_timer(&owner, &old_timer, now).is_err());
    assert!(replacement.cancel_local(&old_watch_id, &None, now).is_err());
    drop(old_watch);
    assert_eq!(
        replacement.read(&owner, &timer, now).unwrap().0.status,
        Status::Running
    );
    assert!(
        replacement
            .state
            .lock()
            .unwrap()
            .observations
            .contains_key(&watch.id)
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn remote_time_catalog_and_hard_coded_calls_fail_closed_without_negotiation() {
    let seed = Clock::new().unwrap();
    let root = std::env::temp_dir().join(format!(
        "ssmcp-time-capability-{}-{}",
        std::process::id(),
        seed.id
    ));
    std::fs::create_dir_all(&root).unwrap();
    let backend = crate::backend::Backend::start_with_tty("/bin/sh", None)
        .await
        .unwrap();
    let runtime = crate::runtime::Runtime::new(backend, root.clone()).unwrap();
    let operation = || crate::wire::Operation::CallTool {
        name: "ssmcp.time.wait".into(),
        arguments: json!({"duration_ms":60_000}),
    };
    Source::default()
        .scope(async {
            assert!(
                runtime
                    .tools()
                    .await
                    .unwrap()
                    .iter()
                    .all(|tool| tool["name"] != "ssmcp.time.wait"
                        && tool["name"] != "ssmcp.time.now")
            );
            assert!(
                runtime
                    .perform(operation())
                    .await
                    .unwrap_err()
                    .starts_with("time_observation_not_negotiated:")
            );
            assert!(runtime.time.state.lock().unwrap().timers.is_empty());
            runtime.configure_timing(true, false).unwrap();
            assert_eq!(
                runtime
                    .tools()
                    .await
                    .unwrap()
                    .iter()
                    .filter(|tool| matches!(
                        tool["name"].as_str(),
                        Some("ssmcp.time.now" | "ssmcp.time.wait")
                    ))
                    .count(),
                2
            );
            let now = runtime
                .perform(crate::wire::Operation::CallTool {
                    name: "ssmcp.time.now".into(),
                    arguments: json!({}),
                })
                .await
                .unwrap();
            assert_eq!(now["structuredContent"]["deadline_clock"], "monotonic");
            runtime.configure_timing(false, false).unwrap();
            assert!(
                runtime
                    .perform(operation())
                    .await
                    .unwrap_err()
                    .starts_with("time_observation_not_negotiated:")
            );
            assert!(runtime.time.state.lock().unwrap().timers.is_empty());
        })
        .await;
    runtime.shutdown().await;
    std::fs::remove_dir_all(root).unwrap();
}
