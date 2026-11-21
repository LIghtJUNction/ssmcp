use super::*;
use std::sync::{Arc, Barrier};

fn actor(participant: &str) -> Actor {
    Actor { provider: "fixture".into(), subject: "account".into(), participant: Some(participant.into()), connection: 7 }
}

fn queue(count: usize) -> Queue<&'static str> {
    Queue::new("fixture_generation".into(), count).unwrap()
}

#[test]
fn configuration_alone_never_advertises_an_idle_executor() {
    let pool = queue(10);
    assert_eq!(pool.snapshot().offline, 10);
    assert_eq!(pool.snapshot().idle, 0);
    assert!(pool.submit(actor("a"), "task", "hello", "owner", 1).is_err());
    assert_eq!(pool.snapshot().queued, 0);
}

#[test]
fn simultaneous_idle_claims_take_distinct_tasks_without_fanout() {
    let pool = Arc::new(queue(2));
    let epochs = [pool.online(0).unwrap(), pool.online(1).unwrap()];
    let first = pool.submit(actor("a"), "one", "one", "owner A", 1).unwrap();
    let second = pool.submit(actor("b"), "two", "two", "owner B", 2).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let joins: Vec<_> = epochs.into_iter().enumerate().map(|(slot, epoch)| {
        let pool = pool.clone(); let barrier = barrier.clone();
        std::thread::spawn(move || { barrier.wait(); pool.claim(slot, epoch, 3).unwrap().unwrap() })
    }).collect();
    barrier.wait();
    let leases: Vec<_> = joins.into_iter().map(|thread| thread.join().unwrap()).collect();
    assert_ne!(leases[0].task_id, leases[1].task_id);
    let mut ids: Vec<_> = leases.iter().map(|lease| lease.task_id.clone()).collect(); ids.sort();
    let mut expected = vec![first.task_id, second.task_id]; expected.sort();
    assert_eq!(ids, expected);
    assert!(leases.iter().any(|lease| lease.text == "one" && lease.context == "owner A"));
    assert!(leases.iter().any(|lease| lease.text == "two" && lease.context == "owner B"));
    assert_eq!(pool.snapshot().working, 2);
    assert_eq!(pool.snapshot().queued, 0);
    assert!(pool.claim(0, epochs[0], 4).unwrap().is_none());
}

#[test]
fn busy_fifo_waits_until_real_cleanup_and_cancel_does_not_free_the_slot() {
    let pool = queue(1); let epoch = pool.online(0).unwrap(); let who = actor("a");
    let first = pool.submit(who.clone(), "one", "first", "original owner", 1).unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    let second = pool.submit(who.clone(), "two", "second", "other owner", 3).unwrap();
    let third = pool.submit(who.clone(), "three", "third", "third owner", 4).unwrap();
    assert_eq!(pool.cancel(&who, &first.task_id, 5).unwrap().status, TaskStatus::Cancelling);
    assert!(pool.cancellation_requested(&lease));
    assert!(pool.finish(&lease, Outcome::Cancelled, false, false, 6).is_err());
    assert_eq!(pool.snapshot().working, 1);
    assert!(pool.claim(0, epoch, 7).unwrap().is_none());
    pool.finish(&lease, Outcome::Cancelled, true, false, 8).unwrap();
    assert_eq!(pool.snapshot().offline, 1);
    let next_epoch = pool.online(0).unwrap();
    let next = pool.claim(0, next_epoch, 9).unwrap().unwrap();
    assert_eq!(next.task_id, second.task_id);
    assert_eq!(next.context, "other owner");
    pool.finish(&next, Outcome::Completed("done".into()), true, true, 10).unwrap();
    assert_eq!(pool.claim(0, next_epoch, 11).unwrap().unwrap().task_id, third.task_id);
}

#[test]
fn authenticated_actor_and_exact_lease_keep_results_and_cancellation_scoped() {
    let pool = queue(1); let epoch = pool.online(0).unwrap();
    let a = actor("a"); let b = actor("b");
    let task = pool.submit(a.clone(), "same display label", "task", "owner A", 1).unwrap();
    assert!(pool.result(&b, &task.task_id).is_err());
    assert!(pool.cancel(&b, &task.task_id, 2).is_err());
    let lease = pool.claim(0, epoch, 3).unwrap().unwrap();
    assert!(pool.offline(0, epoch, "pretend cleanup").is_err());
    pool.finish(&lease, Outcome::Unknown("lost reply after dispatch".into()), true, false, 4).unwrap();
    let receipt = pool.result(&a, &task.task_id).unwrap();
    assert_eq!(receipt.status, TaskStatus::Unknown);
    assert!(!receipt.replay_safe);
    let new_epoch = pool.online(0).unwrap();
    assert_ne!(epoch, new_epoch);
    assert!(pool.finish(&lease, Outcome::Completed("late old result".into()), true, true, 5).is_err());
    assert!(pool.claim(0, epoch, 6).is_err());
}

#[test]
fn idempotent_retry_retains_one_task_and_cannot_change_its_body() {
    let pool = queue(1); pool.online(0).unwrap(); let who = actor("a");
    let first = pool.submit(who.clone(), "retry", "task", "owner A", 1).unwrap();
    let again = pool.submit(who.clone(), "retry", "task", "different owner", 2).unwrap();
    assert_eq!(first.task_id, again.task_id);
    assert_eq!(pool.snapshot().queued, 1);
    assert!(pool.submit(who, "retry", "changed", "owner B", 3).is_err());
    let other = pool.submit(actor("b"), "retry", "task", "owner B", 4).unwrap();
    assert_ne!(first.task_id, other.task_id);
}

#[test]
fn retained_wakeup_close_and_confirmed_completion_preserve_execution_truth() {
    let pool = queue(1); let epoch = pool.online(0).unwrap(); let who = actor("a");
    let mut wake = pool.subscribe();
    let task = pool.submit(who.clone(), "first", "task", "owner", 1).unwrap();
    assert!(wake.has_changed().unwrap()); wake.borrow_and_update();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    let pending = pool.submit(who.clone(), "later", "pending", "owner", 3).unwrap();
    pool.close(4);
    assert!(pool.cancellation_requested(&lease));
    assert_eq!(pool.result(&who, &pending.task_id).unwrap().status, TaskStatus::Cancelled);
    assert_eq!(pool.snapshot().working, 1);
    pool.finish(&lease, Outcome::Completed("confirmed before cancellation".into()), true, false, 5).unwrap();
    assert_eq!(pool.result(&who, &task.task_id).unwrap().status, TaskStatus::Completed);
    assert_eq!(pool.snapshot().offline, 1);
    assert!(pool.online(0).is_err());
}

#[test]
fn failed_execution_is_not_silently_replayed() {
    let pool = queue(1); let epoch = pool.online(0).unwrap(); let who = actor("a");
    let task = pool.submit(who.clone(), "failure", "task", "owner", 1).unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    pool.finish(&lease, Outcome::Failed("exit 7".into()), true, true, 3).unwrap();
    assert_eq!(pool.result(&who, &task.task_id).unwrap().status, TaskStatus::Failed);
    assert!(!pool.result(&who, &task.task_id).unwrap().replay_safe);
    assert_eq!(pool.snapshot().idle, 1);
    assert!(pool.claim(0, epoch, 4).unwrap().is_none());
}
