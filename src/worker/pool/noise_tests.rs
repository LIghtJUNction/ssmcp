// Re-poll only the futures woken by an actual report so each observation is
// re-armed exactly as it would be by the executor. No wall-clock sampling.
use super::tests::{AVAILABILITY_CLIENT, availability_cursor, availability_pool, submit_observed};
use super::*;
use std::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Wake, Waker},
};

#[derive(Default)]
struct Wakes(AtomicUsize);

impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct Observer<'a> {
    future: Pin<Box<dyn Future<Output = Result<Value, String>> + 'a>>,
    wakes: Arc<Wakes>,
}

impl<'a> Observer<'a> {
    fn park(future: impl Future<Output = Result<Value, String>> + 'a) -> Self {
        let mut observer = Self {
            future: Box::pin(future),
            wakes: Arc::default(),
        };
        assert!(observer.poll().is_pending());
        observer
    }

    fn poll(&mut self) -> Poll<Result<Value, String>> {
        self.future
            .as_mut()
            .poll(&mut Context::from_waker(&Waker::from(self.wakes.clone())))
    }

    fn take_wakes(&self) -> usize {
        self.wakes.0.swap(0, Ordering::Relaxed)
    }

    fn rearm(&mut self) -> usize {
        let count = self.take_wakes();
        if count > 0 {
            assert!(self.poll().is_pending());
        }
        count
    }
}

#[tokio::test(start_paused = true)]
async fn worker_scheduler_progress_wakes() {
    const REPORTS: usize = 8;
    let mut noise_wakes = 0;
    for count in [3, 7, 17] {
        let (pool, owner) = availability_pool(count);
        let actors: Vec<_> = (0..count)
            .map(|n| Actor {
                participant: Some(format!("worker-{n}")),
                ..owner.clone()
            })
            .collect();
        for actor in &actors {
            assert_eq!(
                pool.wait_remote(actor, "register", 0).await.unwrap()["status"],
                "waiting"
            );
        }
        submit_observed(&pool, &owner, "busy-task");
        let busy = pool.wait_remote(&actors[0], "busy", 0).await.unwrap();
        assert_eq!(busy["status"], "assigned");

        let current = pool.availability_value(
            pool.queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        let mut capacity = Observer::park(pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&current)),
                wait_ms: 55000,
            },
        ));
        let mut idle: Vec<_> = actors[1..]
            .iter()
            .map(|actor| Observer::park(pool.wait_remote(actor, "next", 55000)))
            .collect();

        let (mut idle_wakes, mut capacity_wakes) = (0, 0);
        for n in 0..REPORTS {
            // Each custom-waker observer is polled by this one test task.
            // Refresh Tokio's cooperative budget between independent reports.
            tokio::task::yield_now().await;
            let report = serde_json::from_value(json!({
                "claim":busy["claim"],"request_id":format!("progress-{n}"),"event":"progress",
                "progress":format!("step {n}")
            }))
            .unwrap();
            assert!(
                !pool
                    .remote()
                    .unwrap()
                    .report(&pool.queue, &actors[0], report, now_ms())
                    .unwrap()
                    .0
            );
            capacity_wakes += capacity.rearm();
            idle_wakes += idle.iter_mut().map(Observer::rearm).sum::<usize>();
        }
        noise_wakes += idle_wakes + capacity_wakes;
        tokio::task::yield_now().await;
        let wanted = submit_observed(&pool, &owner, "next-task");
        assert!(capacity.take_wakes() > 0);
        let Poll::Ready(Ok(changed)) = capacity.poll() else {
            panic!("capacity stayed parked after submit")
        };
        assert_eq!(changed["status"], "changed");
        // Snapshot which waiters were notified by this exact submission, before
        // any worker runs and publishes a second state change by claiming it.
        let ready: Vec<_> = idle
            .iter()
            .map(|pending| pending.take_wakes() > 0)
            .collect();
        let next = ready
            .iter()
            .position(|value| *value)
            .expect("no worker woke on submit");
        let Poll::Ready(Ok(assigned)) = idle[next].poll() else {
            panic!("worker stayed parked after submit")
        };
        assert_eq!(assigned["claim"]["task_id"], wanted.task_id);
        println!(
            "workers={count} progress={REPORTS} idle_wakes={idle_wakes} capacity_wakes={capacity_wakes} submit_woke_worker=true submit_woke_capacity=true"
        );
        drop(idle);
        drop(capacity);
        pool.shutdown().await.unwrap();
    }
    assert_eq!(
        noise_wakes, 0,
        "progress must not wake idle workers or a capacity observer"
    );
}
