// cargo test worker_scheduler_comparison -- --nocapture --test-threads=1
// cargo test --release worker_scheduler_wall -- --ignored --nocapture --test-threads=1
use super::tests::{AVAILABILITY_CLIENT, availability_pool, condition_request, submit_observed};
use super::*;
use std::collections::BTreeSet;
use tokio::time::{Instant, sleep};

#[derive(Default)]
struct Counts {
    wait: usize,
    empty_wait: usize,
    complete: usize,
    result: usize,
    pending_result: usize,
    condition: usize,
}

struct Run {
    calls: usize,
    empty: usize,
    execution_ms: u128,
    observed_ms: u128,
}

// Every participant executes two fixed-duration jobs. Model work is replaced by a timer.
async fn executor(
    pool: Arc<Pool>,
    actor: Actor,
    slot: usize,
    wait_ms: u64,
    unit: u64,
) -> (Counts, Instant) {
    let mut counts = Counts::default();
    for round in 0..2 {
        let id = format!("claim-{slot}-{round}");
        let assigned = loop {
            counts.wait += 1;
            let receipt = pool.wait_remote(&actor, &id, wait_ms).await.unwrap();
            if receipt["status"] == "assigned" {
                break receipt;
            }
            assert_eq!(receipt["status"], "waiting");
            counts.empty_wait += 1;
        };
        if slot == 0 && round == 0 {
            counts.wait += 1;
            let replay = pool.wait_remote(&actor, &id, wait_ms).await.unwrap();
            assert_eq!(replay["claim"], assigned["claim"]);
        }
        sleep(Duration::from_millis((7 + slot as u64 % 3) * unit)).await;
        let completion: Completion = serde_json::from_value(json!({
            "claim":assigned["claim"],"status":"completed","output":id
        }))
        .unwrap();
        let remote = pool.remote().unwrap();
        counts.complete += 1;
        assert!(
            !remote
                .complete(&pool.queue, &actor, completion.clone(), now_ms())
                .unwrap()
                .1
        );
        if slot == 0 && round == 0 {
            counts.complete += 1;
            assert!(
                remote
                    .complete(&pool.queue, &actor, completion, now_ms())
                    .unwrap()
                    .1
            );
        }
    }
    (counts, Instant::now())
}

// Reconcile from committed event pages after a lost response. The same cursor must
// reproduce the same page, and replayed submit/claim/complete must add no work.
async fn check_events(
    pool: &Pool,
    owner: &Actor,
    submitted: &BTreeSet<String>,
    workers: usize,
) -> usize {
    let mut cursor = Value::Null;
    let mut active = BTreeSet::new();
    let mut completed = BTreeSet::new();
    let mut peak = 0;
    let mut calls = 0;
    loop {
        let request =
            || serde_json::from_value(json!({"after":cursor,"limit":128,"wait_ms":0})).unwrap();
        let page = pool
            .wait_events(owner, AVAILABILITY_CLIENT, request())
            .await
            .unwrap();
        calls += 1;
        assert_eq!(page["gap"], false);
        if cursor.is_null() {
            calls += 1;
            assert_eq!(
                pool.wait_events(owner, AVAILABILITY_CLIENT, request())
                    .await
                    .unwrap(),
                page
            );
        }
        for event in page["events"].as_array().unwrap() {
            match event["type"].as_str().unwrap() {
                "worker.started" => {
                    let id = event["task_id"].as_str().unwrap();
                    assert!(submitted.contains(id) && !completed.contains(id));
                    assert!(active.insert(id.to_owned()));
                    peak = peak.max(active.len());
                }
                "worker.completed" => {
                    let id = event["task_id"].as_str().unwrap();
                    assert!(active.remove(id) && completed.insert(id.to_owned()));
                    assert_eq!(event["task_status"], "completed");
                }
                _ => {}
            }
        }
        if page["has_more"] == false {
            break;
        }
        cursor = page["next_cursor"].clone();
    }
    assert!(active.is_empty());
    assert_eq!(peak, workers);
    assert_eq!(&completed, submitted);
    calls
}

async fn compare_policy(workers: usize, blocking: bool, unit: u64) -> Run {
    let (pool, owner) = availability_pool(workers);
    let mut executors = Vec::new();
    let window = if blocking { 55 } else { 5 } * unit;
    for n in 0..workers {
        let actor = Actor {
            participant: Some(format!("worker-{n}")),
            ..owner.clone()
        };
        executors.push(tokio::spawn(executor(pool.clone(), actor, n, window, unit)));
    }
    sleep(Duration::from_millis(12 * unit)).await;
    let start = Instant::now();
    let mut pending = BTreeSet::new();
    for n in 0..2 * workers {
        let id = format!("submit-{n}");
        let task = submit_observed(&pool, &owner, &id);
        assert!(pending.insert(task.task_id.clone()));
        if n == 0 {
            assert_eq!(submit_observed(&pool, &owner, &id).task_id, task.task_id);
        }
    }
    let submitted = pending.clone();
    let mut counts = Counts::default();
    while !pending.is_empty() {
        let mut inspect = pending.iter().cloned().collect::<Vec<_>>();
        if blocking {
            counts.condition += 1;
            let targets = inspect.iter().map(String::as_str).collect::<Vec<_>>();
            let condition = pool
                .wait_for(
                    &owner,
                    AVAILABILITY_CLIENT,
                    condition_request(&pool, "any", &targets, window),
                )
                .await
                .unwrap();
            assert_eq!(condition["status"], "satisfied");
            inspect = condition["conditions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|c| c["state"] == "satisfied")
                .map(|c| c["task_id"].as_str().unwrap().to_owned())
                .collect();
            assert!(!inspect.is_empty());
        }
        for id in inspect {
            counts.result += 1;
            let reply = pool
                .wait_result(
                    &owner,
                    AVAILABILITY_CLIENT,
                    Task {
                        task_id: id.clone(),
                        wait_ms: 0,
                    },
                )
                .await
                .unwrap();
            match reply["task"]["status"].as_str().unwrap() {
                "completed" => {
                    assert!(reply["task"]["output"].is_string());
                    assert!(pending.remove(&id));
                }
                "working" | "queued" if !blocking => counts.pending_result += 1,
                other => panic!("unexpected task state {other}"),
            }
        }
        if !blocking && !pending.is_empty() {
            sleep(Duration::from_millis(5 * unit)).await;
        }
    }
    let observed_ms = start.elapsed().as_millis();
    let mut finish = start;
    for executor in executors {
        let (worker, at) = executor.await.unwrap();
        counts.wait += worker.wait;
        counts.empty_wait += worker.empty_wait;
        counts.complete += worker.complete;
        finish = finish.max(at);
    }
    let execution_ms = finish.duration_since(start).as_millis();
    let submits = 2 * workers + 1;
    let events = check_events(&pool, &owner, &submitted, workers).await;
    let calls = submits + counts.wait + counts.complete + counts.result + counts.condition + events;
    let empty = counts.empty_wait + counts.pending_result;
    println!(
        "workers={workers} policy={} tasks={} peak={workers} submit={submits} events={events} wait={} complete={} result={} wait_for={} calls={calls} empty={empty} execution_ms={execution_ms} observed_ms={observed_ms}",
        if blocking { "blocking" } else { "poll" },
        2 * workers,
        counts.wait,
        counts.complete,
        counts.result,
        counts.condition
    );
    pool.shutdown().await.unwrap();
    Run {
        calls,
        empty,
        execution_ms,
        observed_ms,
    }
}

#[tokio::test(start_paused = true)]
async fn worker_scheduler_comparison() {
    for workers in [3, 7, 17] {
        let before = compare_policy(workers, false, 1000).await;
        let after = compare_policy(workers, true, 1000).await;
        assert!(after.calls < before.calls);
        assert!(after.empty < before.empty);
        assert_eq!(after.execution_ms, before.execution_ms);
        assert!(after.observed_ms <= before.observed_ms);
    }
}

// Compressed real time keeps repetitions short. The paused-clock test checks the
// exact execution schedule; this run reports measured execution and delivery times.
#[tokio::test]
#[ignore = "wall-clock benchmark"]
async fn worker_scheduler_wall() {
    for workers in [3, 7, 17] {
        for repeat in 0..5 {
            for blocking in [repeat % 2 == 1, repeat % 2 == 0] {
                println!("pair={repeat} clock=wall unit_ms=20");
                compare_policy(workers, blocking, 20).await;
            }
        }
    }
}
