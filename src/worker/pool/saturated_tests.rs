use super::tests::{availability_pool, submit_observed};
use super::*;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};
use tokio::{sync::Barrier, task::JoinSet};

// All jobs are ready. Time the concurrent drain, from releasing the workers
// until their final completion, without simulated service time or result polling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "wall-clock saturated worker throughput"]
async fn worker_scheduler_saturated() {
    const TASKS: usize = 120;
    for workers in [3, 7, 17] {
        for sample in 0..6 {
            let (pool, owner) = availability_pool(workers);
            let expected: BTreeSet<_> = (0..TASKS)
                .map(|n| submit_observed(&pool, &owner, &format!("submit-{n}")).task_id)
                .collect();
            assert_eq!(expected.len(), TASKS);
            let barrier = Arc::new(Barrier::new(workers + 1));
            let next = Arc::new(AtomicUsize::new(workers));
            let mut jobs = JoinSet::new();
            for n in 0..workers {
                let actor = Actor {
                    participant: Some(format!("worker-{n}")),
                    ..owner.clone()
                };
                let mut receipt = pool.wait_remote(&actor, "first", 0).await.unwrap();
                assert_eq!(receipt["status"], "assigned");
                let (pool, barrier, next) = (pool.clone(), barrier.clone(), next.clone());
                jobs.spawn(async move {
                    let mut completed = Vec::new();
                    barrier.wait().await;
                    loop {
                        let completion = serde_json::from_value(json!({
                            "claim":receipt["claim"], "status":"completed", "output":"done"
                        }))
                        .unwrap();
                        let (task, duplicate) = pool
                            .remote()
                            .unwrap()
                            .complete(&pool.queue, &actor, completion, now_ms())
                            .unwrap();
                        assert!(!duplicate);
                        completed.push(task.task_id);
                        let ticket = next.fetch_add(1, Ordering::Relaxed);
                        if ticket >= TASKS {
                            return (Instant::now(), completed);
                        }
                        receipt = pool
                            .wait_remote(&actor, &format!("claim-{ticket}"), 0)
                            .await
                            .unwrap();
                        assert_eq!(receipt["status"], "assigned");
                        tokio::task::yield_now().await;
                    }
                });
            }
            let peak = pool.queue.snapshot().working;
            assert_eq!(peak, workers);
            let start = Instant::now();
            barrier.wait().await;
            let mut finish = start;
            let mut actual = BTreeSet::new();
            while let Some(done) = jobs.join_next().await {
                let (at, completed) = done.unwrap();
                finish = finish.max(at);
                for task in completed {
                    assert!(actual.insert(task));
                }
            }
            assert_eq!(actual, expected);
            let wall_ns = finish.duration_since(start).as_nanos();
            // sample 0 warms the runtime and allocator for this pool size.
            println!(
                "saturated workers={workers} sample={sample} tasks={TASKS} peak={peak} wall_ns={wall_ns}"
            );
            pool.shutdown().await.unwrap();
        }
    }
}
