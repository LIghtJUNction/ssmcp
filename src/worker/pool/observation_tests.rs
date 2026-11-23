use super::tests::{
    AVAILABILITY_CLIENT, availability_cursor, availability_pool, condition_request, submit_observed,
};
use super::*;

fn executor(owner: &Actor, name: &str) -> Actor {
    Actor {
        participant: Some(name.into()),
        ..owner.clone()
    }
}

fn completion(assigned: &Value, output: &str) -> Completion {
    serde_json::from_value(json!({
        "claim":assigned["claim"],"status":"completed","output":output
    }))
    .unwrap()
}

fn assert_compact(worker: &Value) {
    assert!(worker.get("manager_operations").is_none());
    for slot in worker["slots"].as_array().unwrap() {
        for detail in [
            "logs",
            "last_command",
            "progress",
            "task_summary",
            "task_text",
            "recent_task_durations_ms",
            "current_task_elapsed_ms",
        ] {
            assert!(
                slot.get(detail).is_none(),
                "routine response copied {detail}"
            );
        }
        assert!(slot["slot"].is_u64());
        assert!(slot["epoch"].is_u64());
        assert!(slot["state"].is_string());
    }
}

#[tokio::test]
async fn task_claim_and_completion_survive_gaps_between_wait_windows() {
    let (pool, owner) = availability_pool(1);
    let worker = executor(&owner, "window-worker");
    let idle = pool.wait_remote(&worker, "resume-one", 1).await.unwrap();
    assert_eq!(idle["status"], "waiting");

    // The first observer has returned before the manager submits anything.
    let first = submit_observed(&pool, &owner, "first-between-windows");
    let assigned = pool.wait_remote(&worker, "resume-one", 0).await.unwrap();
    assert_eq!(assigned["status"], "assigned");
    assert_eq!(assigned["claim"]["task_id"], first.task_id);
    assert_eq!(assigned["claim"]["attempt"], 1);
    let second = submit_observed(&pool, &owner, "second-stays-queued");

    // Recover a lost assignment reply without taking the next queued task.
    let recovered = pool.wait_remote(&worker, "resume-one", 0).await.unwrap();
    assert_eq!(recovered["claim"], assigned["claim"]);
    assert_eq!(recovered["text"], assigned["text"]);
    assert_eq!(pool.queue.snapshot().working, 1);
    assert_eq!(pool.queue.snapshot().queued, 1);
    let pending = pool
        .wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: first.task_id.clone(),
                wait_ms: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(pending["task"]["status"], "working");
    let conditions = pool
        .wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &[&first.task_id], 1),
        )
        .await
        .unwrap();
    assert_eq!(conditions["status"], "timed_out");

    // Completion occurs with neither manager observation currently waiting.
    let report = completion(&assigned, "completed between calls");
    let (completed, duplicate) = pool
        .remote()
        .unwrap()
        .complete(&pool.queue, &worker, report.clone(), 10)
        .unwrap();
    assert!(!duplicate);
    tokio::task::yield_now().await;
    let result = pool
        .wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: first.task_id.clone(),
                wait_ms: MAX_OBSERVATION_WAIT_MS,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["task"]["status"], "completed");
    assert_eq!(result["task"]["output"], "completed between calls");
    assert_eq!(result["task"]["attempt"], 1);
    assert_compact(&result["worker"]);
    let satisfied = pool
        .wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &[&first.task_id], MAX_OBSERVATION_WAIT_MS),
        )
        .await
        .unwrap();
    assert_eq!(satisfied["status"], "satisfied");
    assert_compact(&satisfied["worker"]);

    let (again, duplicate) = pool
        .remote()
        .unwrap()
        .complete(&pool.queue, &worker, report, 11)
        .unwrap();
    assert!(duplicate);
    assert_eq!(
        serde_json::to_value(again).unwrap(),
        serde_json::to_value(completed).unwrap()
    );
    let old_wait = pool.wait_remote(&worker, "resume-one", 0).await.unwrap();
    assert_eq!(old_wait["status"], "already_completed");
    assert_eq!(old_wait["claim"], assigned["claim"]);
    assert_eq!(old_wait["task"], result["task"]);
    assert_eq!(pool.queue.snapshot().queued, 1);

    let next = pool.wait_remote(&worker, "resume-two", 0).await.unwrap();
    assert_eq!(next["claim"]["task_id"], second.task_id);
    assert_ne!(next["claim"]["id"], assigned["claim"]["id"]);
    pool.remote()
        .unwrap()
        .complete(&pool.queue, &worker, completion(&next, "second result"), 12)
        .unwrap();
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn availability_revision_covers_changes_between_observation_windows() {
    let (pool, owner) = availability_pool(1);
    let initial = pool
        .wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: None,
                wait_ms: 0,
            },
        )
        .await
        .unwrap();
    let unchanged = pool
        .wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&initial["availability"])),
                wait_ms: 1,
            },
        )
        .await
        .unwrap();
    assert_eq!(unchanged["status"], "unchanged");
    let worker = executor(&owner, "capacity-between-windows");
    pool.wait_remote(&worker, "capacity-wait", 0).await.unwrap();
    let changed = pool
        .wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&initial["availability"])),
                wait_ms: MAX_OBSERVATION_WAIT_MS,
            },
        )
        .await
        .unwrap();
    assert_eq!(changed["status"], "changed");
    assert_eq!(changed["availability"]["idle"], 1);
    assert_eq!(changed["availability"]["registered_workers"], 1);
    assert!(
        changed["availability"]["revision"].as_u64().unwrap()
            > initial["availability"]["revision"].as_u64().unwrap()
    );
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn all_worker_observers_share_the_55_second_boundary() {
    let (pool, owner) = availability_pool(1);
    let worker = executor(&owner, "boundary-worker");
    let task = submit_observed(&pool, &owner, "boundary-task");
    assert_eq!(MAX_OBSERVATION_WAIT_MS, 55_000);
    let assigned = pool
        .wait_remote(&worker, "boundary-claim", MAX_OBSERVATION_WAIT_MS)
        .await
        .unwrap();
    pool.remote()
        .unwrap()
        .complete(&pool.queue, &worker, completion(&assigned, "ready"), 3)
        .unwrap();

    let result = pool
        .wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: task.task_id.clone(),
                wait_ms: MAX_OBSERVATION_WAIT_MS,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["task"]["output"], "ready");
    assert_eq!(
        pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: None,
                wait_ms: MAX_OBSERVATION_WAIT_MS,
            }
        )
        .await
        .unwrap()["status"],
        "current"
    );
    assert_eq!(
        pool.wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &[&task.task_id], MAX_OBSERVATION_WAIT_MS),
        )
        .await
        .unwrap()["status"],
        "satisfied"
    );

    let too_long = MAX_OBSERVATION_WAIT_MS + 1;
    let before = pool.queue.compact_snapshot();
    for error in [
        pool.wait_remote(&worker, "invalid-window", too_long)
            .await
            .unwrap_err(),
        pool.wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: task.task_id.clone(),
                wait_ms: too_long,
            },
        )
        .await
        .unwrap_err(),
        pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: None,
                wait_ms: too_long,
            },
        )
        .await
        .unwrap_err(),
        pool.wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &[&task.task_id], too_long),
        )
        .await
        .unwrap_err(),
    ] {
        assert!(error.contains("55000"), "{error}");
    }
    assert_eq!(pool.queue.compact_snapshot(), before);
    let tools = Pool::tools();
    for name in ["wait", "result", "wait_available", "wait_for"] {
        let tool = tools
            .iter()
            .find(|tool| tool["name"] == format!("ssmcp.worker.{name}"))
            .unwrap();
        assert_eq!(
            tool["inputSchema"]["properties"]["wait_ms"]["maximum"],
            55_000
        );
        assert_eq!(tool["inputSchema"]["properties"]["wait_ms"]["minimum"], 0);
    }
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn routine_snapshots_stay_small_while_full_logs_and_results_are_retained() {
    let (pool, owner) = availability_pool(3);
    let mut assignments = Vec::new();
    for index in 0..3 {
        let worker = executor(&owner, &format!("payload-worker-{index}"));
        let task = submit_observed(&pool, &owner, &format!("payload-task-{index}"));
        let request_id = format!("payload-wait-{index}");
        let assigned = pool.wait_remote(&worker, &request_id, 0).await.unwrap();
        let command: Report = serde_json::from_value(json!({
            "claim":assigned["claim"],"request_id":format!("command-{index}"),
            "event":"command_start","command":"long command ".repeat(250)
        }))
        .unwrap();
        pool.remote()
            .unwrap()
            .report(&pool.queue, &worker, command, 4)
            .unwrap();
        for report_index in 0..24 {
            let progress: Report = serde_json::from_value(json!({
                "claim":assigned["claim"],
                "request_id":format!("progress-{index}-{report_index}"),
                "event":"progress","progress":"observed progress ".repeat(52)
            }))
            .unwrap();
            pool.remote()
                .unwrap()
                .report(&pool.queue, &worker, progress, 5)
                .unwrap();
        }
        assignments.push((worker, task, request_id, assigned));
    }
    let observed = pool
        .wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &[&assignments[0].1.task_id], 0),
        )
        .await
        .unwrap();
    assert_eq!(observed["status"], "timed_out");
    assert_compact(&observed["worker"]);
    let full = serde_json::to_value(pool.queue.snapshot()).unwrap();
    let compact = pool.queue.compact_snapshot();
    assert!(serde_json::to_vec(&full).unwrap().len() > 32 * 1024);
    assert!(serde_json::to_vec(&compact).unwrap().len() < 2 * 1024);
    assert!(!full["manager_operations"].as_array().unwrap().is_empty());
    for slot in full["slots"].as_array().unwrap() {
        assert!(!slot["logs"].as_array().unwrap().is_empty());
        assert!(slot["last_command"].as_str().unwrap().len() > 700);
        assert!(slot["progress"].as_str().unwrap().len() > 500);
    }
    assert_compact(&compact);
    for key in [
        "process_id",
        "generation",
        "manager",
        "total",
        "idle",
        "working",
        "offline",
        "retiring",
        "registered_workers",
        "desired_total",
        "queued",
        "closed",
        "stopping",
    ] {
        assert_eq!(compact[key], full[key], "{key}");
    }
    for (index, slot) in compact["slots"].as_array().unwrap().iter().enumerate() {
        for key in [
            "slot",
            "epoch",
            "state",
            "task_id",
            "registered",
            "retire_requested",
        ] {
            assert_eq!(slot[key], full["slots"][index][key], "{key}");
        }
    }
    let (worker, task, request_id, original) = &assignments[0];
    let recovered = pool.wait_remote(worker, request_id, 0).await.unwrap();
    assert_eq!(recovered["claim"], original["claim"]);
    assert_eq!(recovered["text"], original["text"]);
    assert_compact(&recovered["worker"]);
    assert!(serde_json::to_vec(&recovered).unwrap().len() < 4 * 1024);

    // The task's real payload remains intact even when larger than its metadata.
    let payload = "x".repeat(64 * 1024);
    pool.remote()
        .unwrap()
        .complete(&pool.queue, worker, completion(&recovered, &payload), 6)
        .unwrap();
    let result = pool
        .wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: task.task_id.clone(),
                wait_ms: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["task"]["output"], payload);
    assert_compact(&result["worker"]);
    let receipt = pool.wait_remote(worker, request_id, 0).await.unwrap();
    assert_eq!(receipt["claim"], original["claim"]);
    assert_eq!(receipt["task"]["output"], payload);
    for (worker, _, _, assigned) in &assignments[1..] {
        pool.remote()
            .unwrap()
            .complete(&pool.queue, worker, completion(assigned, "finished"), 7)
            .unwrap();
    }
    pool.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_connection_keeps_unconfirmed_claim_unknown_across_retry_attempts() {
    let (pool, owner) = availability_pool(1);
    let worker = executor(&owner, "unknown-worker");
    let task = submit_observed(&pool, &owner, "unknown-task");
    let assigned = pool.wait_remote(&worker, "unknown-wait", 0).await.unwrap();
    pool.shutdown().await.unwrap();
    let retained = pool.queue.result(&owner, &task.task_id).unwrap();
    assert_eq!(retained.status, TaskStatus::Unknown);
    assert!(!retained.replay_safe);
    assert!(pool.wait_remote(&worker, "unknown-wait", 0).await.is_err());
    assert!(
        pool.remote()
            .unwrap()
            .complete(
                &pool.queue,
                &worker,
                completion(&assigned, "unconfirmed late report"),
                9,
            )
            .is_err()
    );
    let after = pool.queue.result(&owner, &task.task_id).unwrap();
    assert_eq!(after.status, TaskStatus::Unknown);
    assert_eq!(after.attempt, 1);
    assert!(!after.replay_safe);
}
