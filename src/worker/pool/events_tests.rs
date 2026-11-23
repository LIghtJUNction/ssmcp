use super::tests::{AVAILABILITY_CLIENT, availability_pool, submit_observed};
use super::*;

fn reader(owner: &Actor, name: &str) -> Actor {
    Actor {
        participant: Some(name.into()),
        ..owner.clone()
    }
}

fn request(after: Option<&Value>, limit: usize, wait_ms: u64) -> EventRequest {
    serde_json::from_value(json!({
        "after":after,"limit":limit,"wait_ms":wait_ms
    }))
    .unwrap()
}

fn progress(assigned: &Value, id: &str, text: &str) -> Report {
    serde_json::from_value(json!({
        "claim":assigned["claim"],"request_id":id,
        "event":"progress","progress":text
    }))
    .unwrap()
}

fn completed(assigned: &Value, output: &str) -> Completion {
    serde_json::from_value(json!({
        "claim":assigned["claim"],"status":"completed","output":output
    }))
    .unwrap()
}

async fn page(pool: &Pool, actor: &Actor, after: Option<&Value>) -> Value {
    pool.wait_events(actor, AVAILABILITY_CLIENT, request(after, 128, 0))
        .await
        .unwrap()
}

#[tokio::test]
async fn events_survive_lost_replies_and_idempotent_retries_without_bodies() {
    let (pool, owner) = availability_pool(1);
    let initial = page(&pool, &owner, None).await;
    assert_eq!(initial["status"], "empty");
    assert_eq!(initial["latest_seq"], 0);
    let task = submit_observed(&pool, &owner, "journal-task");
    let worker = reader(&owner, "journal-worker");
    let assigned = pool.wait_remote(&worker, "journal-claim", 0).await.unwrap();
    let report = progress(&assigned, "journal-progress", "PRIVATE PROGRESS CREDENTIAL");
    pool.remote()
        .unwrap()
        .report(&pool.queue, &worker, report.clone(), 10)
        .unwrap();
    let completion = completed(&assigned, "PRIVATE RESULT BODY");
    pool.remote()
        .unwrap()
        .complete(&pool.queue, &worker, completion.clone(), 11)
        .unwrap();

    // No RPC response was needed to publish these queue commits.
    let events = page(&pool, &owner, Some(&initial["next_cursor"])).await;
    let types: Vec<_> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "task.submitted",
            "worker.idle",
            "worker.started",
            "worker.progress",
            "worker.completed",
            "worker.idle"
        ]
    );
    assert_eq!(events["latest_seq"], 6);
    assert_eq!(events["has_more"], false);
    for (index, event) in events["events"].as_array().unwrap().iter().enumerate() {
        assert_eq!(event["seq"], index + 1);
        assert_eq!(event["generation"], pool.generation);
        if event["task_id"].is_string() {
            assert_eq!(event["task_id"], task.task_id);
        }
        if event["type"] == "worker.started" || event["type"] == "worker.completed" {
            assert_eq!(event["slot"], assigned["claim"]["slot"]);
            assert_eq!(event["epoch"], assigned["claim"]["epoch"]);
            assert_eq!(event["attempt"], assigned["claim"]["attempt"]);
            assert_eq!(event["participant_id"], "journal-worker");
        }
    }
    assert_eq!(events["events"][3]["activity"], "progress");
    assert_eq!(events["events"][4]["task_status"], "completed");
    let encoded = serde_json::to_string(&events).unwrap();
    assert!(!encoded.contains("PRIVATE PROGRESS"));
    assert!(!encoded.contains("PRIVATE RESULT"));
    assert!(!encoded.contains(assigned["text"].as_str().unwrap()));

    let before = pool.queue.compact_snapshot();
    assert_eq!(
        page(&pool, &owner, Some(&initial["next_cursor"])).await,
        events
    );
    let observer = reader(&owner, "metadata-observer");
    assert_eq!(
        page(&pool, &observer, Some(&initial["next_cursor"])).await,
        events
    );
    assert_eq!(
        page(&pool, &worker, Some(&initial["next_cursor"])).await,
        events
    );
    assert_eq!(pool.queue.compact_snapshot(), before);

    let (_, duplicate) = pool
        .remote()
        .unwrap()
        .complete(&pool.queue, &worker, completion, 12)
        .unwrap();
    assert!(duplicate);
    let (duplicate, _, _) = pool
        .remote()
        .unwrap()
        .report(&pool.queue, &worker, report, 13)
        .unwrap();
    assert!(duplicate);
    assert_eq!(
        pool.wait_remote(&worker, "journal-claim", 0).await.unwrap()["status"],
        "already_completed"
    );
    let unchanged = page(&pool, &owner, Some(&events["next_cursor"])).await;
    assert_eq!(unchanged["latest_seq"], events["latest_seq"]);
    assert!(unchanged["events"].as_array().unwrap().is_empty());
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn events_bridge_idle_windows_and_wake_a_waiter_after_commits() {
    let (pool, owner) = availability_pool(1);
    let initial = page(&pool, &owner, None).await;
    let idle = pool
        .wait_events(
            &owner,
            AVAILABILITY_CLIENT,
            request(Some(&initial["next_cursor"]), 128, 1),
        )
        .await
        .unwrap();
    assert_eq!(idle["status"], "timed_out");
    let task = submit_observed(&pool, &owner, "after-event-timeout");
    let submitted = pool
        .wait_events(
            &owner,
            AVAILABILITY_CLIENT,
            request(Some(&idle["next_cursor"]), 128, MAX_OBSERVATION_WAIT_MS),
        )
        .await
        .unwrap();
    assert_eq!(submitted["events"][0]["task_id"], task.task_id);
    let worker = reader(&owner, "event-wait-worker");
    let assigned = pool
        .wait_remote(&worker, "event-wait-claim", 0)
        .await
        .unwrap();
    let cursor = page(&pool, &owner, None).await["next_cursor"].clone();

    let wait_pool = pool.clone();
    let wait_owner = owner.clone();
    let wait_request = request(Some(&cursor), 128, 1000);
    let waiter = tokio::spawn(async move {
        wait_pool
            .wait_events(&wait_owner, AVAILABILITY_CLIENT, wait_request)
            .await
    });
    tokio::task::yield_now().await;
    pool.remote()
        .unwrap()
        .report(
            &pool.queue,
            &worker,
            progress(&assigned, "wake-progress", "actual observation"),
            20,
        )
        .unwrap();
    pool.remote()
        .unwrap()
        .complete(
            &pool.queue,
            &worker,
            completed(&assigned, "actual result"),
            21,
        )
        .unwrap();
    let wake = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(wake["status"], "events");
    let retained = page(&pool, &owner, Some(&cursor)).await;
    let types: Vec<_> = retained["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        ["worker.progress", "worker.completed", "worker.idle"]
    );
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn journal_overflow_is_explicit_and_pagination_never_consumes_events() {
    let (pool, owner) = availability_pool(1);
    submit_observed(&pool, &owner, "overflow-task");
    let worker = reader(&owner, "overflow-worker");
    let assigned = pool
        .wait_remote(&worker, "overflow-claim", 0)
        .await
        .unwrap();
    let origin = page(&pool, &owner, None).await["next_cursor"].clone();
    for index in 0..1100 {
        pool.remote()
            .unwrap()
            .report(
                &pool.queue,
                &worker,
                progress(
                    &assigned,
                    &format!("overflow-progress-{index}"),
                    "bounded metadata",
                ),
                index + 100,
            )
            .unwrap();
    }
    let first = pool
        .wait_events(&owner, AVAILABILITY_CLIENT, request(Some(&origin), 17, 0))
        .await
        .unwrap();
    assert_eq!(first["gap"], true);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["events"].as_array().unwrap().len(), 17);
    assert_eq!(first["events"][0]["seq"], first["oldest_seq"]);
    assert_eq!(
        pool.wait_events(&owner, AVAILABILITY_CLIENT, request(Some(&origin), 17, 0),)
            .await
            .unwrap(),
        first
    );

    let mut all = first["events"].as_array().unwrap().clone();
    let mut current = first;
    while current["has_more"] == true {
        current = pool
            .wait_events(
                &owner,
                AVAILABILITY_CLIENT,
                request(Some(&current["next_cursor"]), 128, 0),
            )
            .await
            .unwrap();
        assert_eq!(current["gap"], false);
        all.extend(current["events"].as_array().unwrap().iter().cloned());
    }
    assert_eq!(all.len(), 1024);
    for pair in all.windows(2) {
        assert_eq!(
            pair[0]["seq"].as_u64().unwrap() + 1,
            pair[1]["seq"].as_u64().unwrap()
        );
    }
    assert_eq!(current["next_cursor"]["seq"], current["latest_seq"]);
    let exhausted = pool
        .wait_events(
            &owner,
            AVAILABILITY_CLIENT,
            request(Some(&current["next_cursor"]), 32, 1),
        )
        .await
        .unwrap();
    assert_eq!(exhausted["status"], "timed_out");
    assert_eq!(exhausted["gap"], false);
    assert!(exhausted["events"].as_array().unwrap().is_empty());
    pool.remote()
        .unwrap()
        .complete(&pool.queue, &worker, completed(&assigned, "finished"), 2000)
        .unwrap();
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn event_cursor_bounds_and_account_checks_leave_authority_unchanged() {
    let (pool, owner) = availability_pool(1);
    submit_observed(&pool, &owner, "validation-task");
    let before = pool.queue.compact_snapshot();
    let wrong_generation = json!({"generation":"different-pool","seq":0});
    let future = json!({"generation":pool.generation,"seq":100});
    for request in [
        request(Some(&wrong_generation), 32, 0),
        request(Some(&future), 32, 0),
        request(None, 0, 0),
        request(None, 129, 0),
        request(None, 32, MAX_OBSERVATION_WAIT_MS + 1),
    ] {
        assert!(
            pool.wait_events(&owner, AVAILABILITY_CLIENT, request)
                .await
                .is_err()
        );
    }
    for foreign in [
        Actor {
            subject: "foreign-account".into(),
            ..owner.clone()
        },
        Actor {
            connection: owner.connection + 1,
            ..owner.clone()
        },
        Actor {
            participant: None,
            ..owner.clone()
        },
    ] {
        assert!(
            pool.wait_events(&foreign, AVAILABILITY_CLIENT, request(None, 32, 0),)
                .await
                .is_err()
        );
    }
    let ready = pool
        .wait_events(
            &owner,
            AVAILABILITY_CLIENT,
            request(None, 32, MAX_OBSERVATION_WAIT_MS),
        )
        .await
        .unwrap();
    assert_eq!(ready["status"], "events");
    assert_eq!(pool.queue.compact_snapshot(), before);
    let tool = Pool::tools()
        .into_iter()
        .find(|tool| tool["name"] == "ssmcp.worker.events")
        .unwrap();
    assert_eq!(
        tool["inputSchema"]["properties"]["wait_ms"]["maximum"],
        55_000
    );
    assert_eq!(tool["inputSchema"]["properties"]["limit"]["maximum"], 128);
    assert_eq!(
        tool["inputSchema"]["properties"]["after"]["additionalProperties"],
        false
    );
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn disconnect_events_preserve_unknown_without_fabricating_completion() {
    let (pool, owner) = availability_pool(1);
    let task = submit_observed(&pool, &owner, "event-unknown-task");
    let worker = reader(&owner, "event-unknown-worker");
    let assigned = pool
        .wait_remote(&worker, "event-unknown-claim", 0)
        .await
        .unwrap();
    let cursor = page(&pool, &owner, None).await["next_cursor"].clone();
    pool.shutdown().await.unwrap();
    let closed = page(&pool, &owner, Some(&cursor)).await;
    assert_eq!(closed["closed"], true);
    let events = closed["events"].as_array().unwrap();
    assert!(events.iter().any(|event| event["type"] == "worker.unknown"
        && event["task_id"] == task.task_id
        && event["task_status"] == "unknown"));
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "worker.completed" && event["task_id"] == task.task_id)
    );
    assert!(!events.iter().any(|event| event["type"] == "worker.idle"));
    assert!(
        pool.remote()
            .unwrap()
            .complete(
                &pool.queue,
                &worker,
                completed(&assigned, "late unconfirmed output"),
                90,
            )
            .is_err()
    );
    let final_page = pool
        .wait_events(
            &owner,
            AVAILABILITY_CLIENT,
            request(Some(&closed["next_cursor"]), 128, MAX_OBSERVATION_WAIT_MS),
        )
        .await
        .unwrap();
    assert_eq!(final_page["status"], "closed");
    assert_eq!(final_page["latest_seq"], closed["latest_seq"]);
    assert!(final_page["events"].as_array().unwrap().is_empty());
    let retained = pool.queue.result(&owner, &task.task_id).unwrap();
    assert_eq!(retained.status, TaskStatus::Unknown);
    assert_eq!(retained.attempt, 1);
    assert!(!retained.replay_safe);
}

#[tokio::test]
async fn cancellation_event_is_distinct_from_confirmed_completion() {
    let (pool, owner) = availability_pool(1);
    let task = submit_observed(&pool, &owner, "active-cancel-event");
    let worker = reader(&owner, "cancel-event-worker");
    let assigned = pool
        .wait_remote(&worker, "cancel-event-claim", 0)
        .await
        .unwrap();
    let queued = submit_observed(&pool, &owner, "queued-cancel-event");
    let cursor = page(&pool, &owner, None).await["next_cursor"].clone();
    pool.queue.cancel(&owner, &task.task_id, 50).unwrap();
    pool.queue.cancel(&owner, &queued.task_id, 51).unwrap();
    let cancellation = page(&pool, &owner, Some(&cursor)).await;
    let events = cancellation["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "worker.cancelling" && event["task_id"] == task.task_id)
    );
    assert!(
        !events
            .iter()
            .any(|event| event["type"] == "worker.completed" && event["task_id"] == task.task_id)
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "worker.completed"
                && event["task_id"] == queued.task_id
                && event["task_status"] == "cancelled"
                && event["slot"].is_null())
    );
    pool.queue.stop(&owner, AVAILABILITY_CLIENT, 52).unwrap();
    let acknowledged: Completion = serde_json::from_value(json!({
        "claim":assigned["claim"],"status":"cancelled"
    }))
    .unwrap();
    pool.remote()
        .unwrap()
        .complete(&pool.queue, &worker, acknowledged, 53)
        .unwrap();
    let completed = page(&pool, &owner, Some(&cancellation["next_cursor"])).await;
    assert!(
        completed["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "worker.completed"
                && event["task_id"] == task.task_id
                && event["task_status"] == "cancelled")
    );
    assert!(
        !completed["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["type"] == "worker.idle")
    );
    pool.shutdown().await.unwrap();
}

#[test]
fn closed_unbound_journal_keeps_its_original_account_boundary() {
    let queue = Queue::<()>::new("unbound-event-journal".into(), 1).unwrap();
    queue.remote_ready().unwrap();
    let worker = Actor {
        provider: "test-provider".into(),
        subject: "original-account".into(),
        participant: Some("original-worker".into()),
        connection: 1,
    };
    assert!(queue.claim_remote(&worker, 1).unwrap().is_none());
    queue.close(2);
    queue.cleaned_closed_pool(3).unwrap();
    let retained = serde_json::to_value(
        queue
            .events(&worker, AVAILABILITY_CLIENT, &request(None, 32, 0))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retained["events"][0]["type"], "worker.idle");
    assert_eq!(retained["closed"], true);
    assert_eq!(queue.compact_snapshot()["manager"], Value::Null);
    let foreign = Actor {
        subject: "different-account".into(),
        ..worker
    };
    assert!(
        queue
            .events(&foreign, AVAILABILITY_CLIENT, &request(None, 32, 0),)
            .is_err()
    );
}
