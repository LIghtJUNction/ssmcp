use super::*;

const MANAGER_CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn management_observation_and_rejected_work_never_acquire_ownership() {
    let pool = queue(1);
    let owner = actor("manager");
    assert!(pool.snapshot().manager.is_none());
    assert!(pool.claim_remote(&actor("executor"), 0).unwrap().is_none());
    assert!(
        pool.submit_managed(owner.clone(), MANAGER_CLIENT, "first", "work", "context", 1)
            .is_err()
    );
    assert!(pool.snapshot().manager.is_none());
    pool.online(0).unwrap();
    for (key, text) in [("", "work"), ("first", " "), ("bad\nkey", "work")] {
        assert!(
            pool.submit_managed(owner.clone(), MANAGER_CLIENT, key, text, "context", 2)
                .is_err()
        );
        assert!(pool.snapshot().manager.is_none());
        assert_eq!(pool.snapshot().queued, 0);
    }
    assert!(pool.attach(&owner, "not-a-client", 3).is_err());
    let mut unbound = owner.clone();
    unbound.participant = None;
    assert!(pool.attach(&unbound, MANAGER_CLIENT, 3).is_err());
    assert!(pool.snapshot().manager.is_none());
    let manager = pool.attach(&owner, MANAGER_CLIENT, 4).unwrap();
    assert_eq!(manager.thread_id, "manager");
    assert_eq!(manager.client_id, MANAGER_CLIENT);
    assert_eq!(manager.attached_at_ms, 4);
    assert_eq!(
        pool.attach(&owner, MANAGER_CLIENT, 5)
            .unwrap()
            .attached_at_ms,
        4
    );
    for foreign in [
        actor("other"),
        Actor {
            subject: "other-account".into(),
            ..owner.clone()
        },
        Actor {
            connection: 8,
            ..owner.clone()
        },
    ] {
        assert!(pool.attach(&foreign, MANAGER_CLIENT, 6).is_err());
        assert!(
            pool.submit_managed(
                foreign.clone(),
                MANAGER_CLIENT,
                "foreign",
                "work",
                "context",
                6
            )
            .is_err()
        );
        assert!(pool.tasks(&foreign, MANAGER_CLIENT, None, 32).is_err());
        assert!(pool.stop(&foreign, MANAGER_CLIENT, 6).is_err());
    }
    assert_eq!(pool.snapshot().queued, 0);
    assert!(!pool.snapshot().stopping);
}

#[test]
fn simultaneous_first_submissions_bind_only_the_successful_manager() {
    let pool = Arc::new(queue(2));
    pool.online(0).unwrap();
    pool.online(1).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let mut joins = Vec::new();
    for participant in ["a", "b"] {
        let pool = pool.clone();
        let barrier = barrier.clone();
        joins.push(std::thread::spawn(move || {
            barrier.wait();
            (
                participant,
                pool.submit_managed(
                    actor(participant),
                    MANAGER_CLIENT,
                    "first",
                    "work",
                    "context",
                    1,
                ),
            )
        }));
    }
    let results: Vec<_> = joins.into_iter().map(|join| join.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|(_, result)| result.is_ok()).count(),
        1
    );
    let (winner, result) = results.iter().find(|(_, result)| result.is_ok()).unwrap();
    let first = result.as_ref().unwrap();
    assert_eq!(pool.snapshot().manager.unwrap().participant_id, *winner);
    assert_eq!(pool.snapshot().queued, 1);
    assert_eq!(
        pool.submit_managed(actor(winner), MANAGER_CLIENT, "first", "work", "context", 2)
            .unwrap()
            .task_id,
        first.task_id
    );
    assert!(
        pool.submit_managed(
            actor(winner),
            MANAGER_CLIENT,
            "first",
            "different",
            "context",
            2
        )
        .is_err()
    );
    assert_eq!(pool.snapshot().queued, 1);
}

#[test]
fn manager_task_pages_are_bounded_without_exposing_bodies_or_results() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let owner = actor("manager");
    let first = pool
        .submit_managed(
            owner.clone(),
            MANAGER_CLIENT,
            "a",
            "private prompt",
            "private expanded input",
            1,
        )
        .unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    pool.finish(
        &lease,
        Outcome::Completed("private result".into()),
        true,
        true,
        3,
    )
    .unwrap();
    let second = pool
        .submit_managed(
            owner.clone(),
            MANAGER_CLIENT,
            "b",
            "another private prompt",
            "context",
            4,
        )
        .unwrap();
    let page = pool.tasks(&owner, MANAGER_CLIENT, None, 1).unwrap();
    assert_eq!(page.tasks.len(), 1);
    assert_eq!(page.tasks[0].task_id, first.task_id);
    let next = pool
        .tasks(&owner, MANAGER_CLIENT, page.next_cursor.as_deref(), 1)
        .unwrap();
    assert_eq!(next.tasks.len(), 1);
    assert_eq!(next.tasks[0].task_id, second.task_id);
    assert!(next.next_cursor.is_none());
    let encoded = serde_json::to_value(&page).unwrap();
    assert!(encoded["tasks"][0].get("text").is_none());
    assert!(encoded["tasks"][0].get("output").is_none());
    assert!(encoded["tasks"][0].get("error").is_none());
    assert!(!encoded.to_string().contains("private"));
    assert_eq!(
        pool.result(&owner, &first.task_id)
            .unwrap()
            .output
            .as_deref(),
        Some("private result")
    );
    assert!(
        pool.tasks(&actor("executor"), MANAGER_CLIENT, None, 1)
            .is_err()
    );
    assert!(pool.tasks(&owner, MANAGER_CLIENT, None, 0).is_err());
    assert!(pool.tasks(&owner, MANAGER_CLIENT, None, 129).is_err());
}

#[test]
fn slot_logs_are_bounded_real_events_without_task_bodies_or_results() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let owner = actor("sender");
    for index in 0..100 {
        let task = pool
            .submit(
                owner.clone(),
                &format!("key-{index}"),
                "private task text",
                "private context",
                index,
            )
            .unwrap();
        let lease = pool.claim(0, epoch, index).unwrap().unwrap();
        assert_eq!(lease.task_id, task.task_id);
        pool.finish(
            &lease,
            Outcome::Completed("private result text".into()),
            true,
            true,
            index,
        )
        .unwrap();
    }
    let snapshot = pool.snapshot();
    let slot = &snapshot.slots[0];
    assert_eq!(slot.logs.len(), MAX_LOGS);
    assert!(
        slot.logs
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    );
    assert_eq!(
        slot.next_log_sequence,
        slot.logs.last().unwrap().sequence + 1
    );
    assert!(
        slot.logs
            .iter()
            .all(|entry| entry.text.len() <= MAX_LOG_TEXT)
    );
    assert!(
        slot.logs
            .iter()
            .map(|entry| entry.text.len()
                + entry.level.len()
                + entry.task_id.as_ref().map_or(0, String::len)
                + 64)
            .sum::<usize>()
            <= MAX_LOG_BYTES
    );
    assert_eq!(slot.task_summary.as_deref(), Some("private task text"));
    let serialized = serde_json::to_string(&slot.logs).unwrap();
    assert!(!serialized.contains("private task text"));
    assert!(!serialized.contains("private context"));
    assert!(!serialized.contains("private result text"));
    assert!(serialized.contains("Task assigned"));
    assert!(serialized.contains("output 19 bytes"));
}
use std::sync::{Arc, Barrier};

fn actor(participant: &str) -> Actor {
    Actor {
        provider: "fixture".into(),
        subject: "account".into(),
        participant: Some(participant.into()),
        connection: 7,
    }
}

fn queue(count: usize) -> Queue<&'static str> {
    Queue::new("fixture_generation".into(), count).unwrap()
}

#[test]
fn configuration_alone_never_advertises_an_idle_executor() {
    let pool = queue(10);
    assert_eq!(pool.snapshot().offline, 10);
    assert_eq!(pool.snapshot().idle, 0);
    assert!(
        pool.submit(actor("a"), "task", "hello", "owner", 1)
            .is_err()
    );
    assert_eq!(pool.snapshot().queued, 0);
}

#[test]
fn simultaneous_idle_claims_take_distinct_tasks_without_fanout() {
    let pool = Arc::new(queue(2));
    let epochs = [pool.online(0).unwrap(), pool.online(1).unwrap()];
    let first = pool.submit(actor("a"), "one", "one", "owner A", 1).unwrap();
    let second = pool.submit(actor("b"), "two", "two", "owner B", 2).unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let joins: Vec<_> = epochs
        .into_iter()
        .enumerate()
        .map(|(slot, epoch)| {
            let pool = pool.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                pool.claim(slot, epoch, 3).unwrap().unwrap()
            })
        })
        .collect();
    barrier.wait();
    let leases: Vec<_> = joins
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect();
    assert_ne!(leases[0].task_id, leases[1].task_id);
    let mut ids: Vec<_> = leases.iter().map(|lease| lease.task_id.clone()).collect();
    ids.sort();
    let mut expected = vec![first.task_id, second.task_id];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(
        leases
            .iter()
            .any(|lease| lease.text == "one" && lease.context == "owner A")
    );
    assert!(
        leases
            .iter()
            .any(|lease| lease.text == "two" && lease.context == "owner B")
    );
    assert_eq!(pool.snapshot().working, 2);
    assert_eq!(pool.snapshot().queued, 0);
    assert!(pool.claim(0, epochs[0], 4).unwrap().is_none());
}

#[test]
fn busy_fifo_waits_until_real_cleanup_and_cancel_does_not_free_the_slot() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let who = actor("a");
    let first = pool
        .submit(who.clone(), "one", "first", "original owner", 1)
        .unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    let second = pool
        .submit(who.clone(), "two", "second", "other owner", 3)
        .unwrap();
    let third = pool
        .submit(who.clone(), "three", "third", "third owner", 4)
        .unwrap();
    assert_eq!(
        pool.cancel(&who, &first.task_id, 5).unwrap().status,
        TaskStatus::Cancelling
    );
    assert!(pool.cancellation_requested(&lease));
    assert!(
        pool.finish(&lease, Outcome::Cancelled, false, false, 6)
            .is_err()
    );
    assert_eq!(pool.snapshot().working, 1);
    assert!(pool.claim(0, epoch, 7).unwrap().is_none());
    pool.finish(&lease, Outcome::Cancelled, true, false, 8)
        .unwrap();
    assert_eq!(pool.snapshot().offline, 1);
    let next_epoch = pool.online(0).unwrap();
    let next = pool.claim(0, next_epoch, 9).unwrap().unwrap();
    assert_eq!(next.task_id, second.task_id);
    assert_eq!(next.context, "other owner");
    pool.finish(&next, Outcome::Completed("done".into()), true, true, 10)
        .unwrap();
    assert_eq!(
        pool.claim(0, next_epoch, 11).unwrap().unwrap().task_id,
        third.task_id
    );
}

#[test]
fn authenticated_actor_and_exact_lease_keep_results_and_cancellation_scoped() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let a = actor("a");
    let b = actor("b");
    let task = pool
        .submit(a.clone(), "same display label", "task", "owner A", 1)
        .unwrap();
    assert!(pool.result(&b, &task.task_id).is_err());
    assert!(pool.cancel(&b, &task.task_id, 2).is_err());
    let lease = pool.claim(0, epoch, 3).unwrap().unwrap();
    assert!(pool.offline(0, epoch, "pretend cleanup").is_err());
    pool.finish(
        &lease,
        Outcome::Unknown("lost reply after dispatch".into()),
        true,
        false,
        4,
    )
    .unwrap();
    let receipt = pool.result(&a, &task.task_id).unwrap();
    assert_eq!(receipt.status, TaskStatus::Unknown);
    assert!(!receipt.replay_safe);
    let new_epoch = pool.online(0).unwrap();
    assert_ne!(epoch, new_epoch);
    assert!(
        pool.finish(
            &lease,
            Outcome::Completed("late old result".into()),
            true,
            true,
            5
        )
        .is_err()
    );
    assert!(pool.claim(0, epoch, 6).is_err());
}

#[test]
fn idempotent_retry_retains_one_task_and_cannot_change_its_body() {
    let pool = queue(1);
    pool.online(0).unwrap();
    let who = actor("a");
    let first = pool
        .submit(who.clone(), "retry", "task", "owner A", 1)
        .unwrap();
    let again = pool
        .submit(who.clone(), "retry", "task", "different owner", 2)
        .unwrap();
    assert_eq!(first.task_id, again.task_id);
    assert_eq!(pool.snapshot().queued, 1);
    assert!(pool.submit(who, "retry", "changed", "owner B", 3).is_err());
    let other = pool
        .submit(actor("b"), "retry", "task", "owner B", 4)
        .unwrap();
    assert_ne!(first.task_id, other.task_id);
}

#[test]
fn retained_wakeup_close_and_confirmed_completion_preserve_execution_truth() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let who = actor("a");
    let mut wake = pool.subscribe();
    let task = pool
        .submit(who.clone(), "first", "task", "owner", 1)
        .unwrap();
    assert!(wake.has_changed().unwrap());
    wake.borrow_and_update();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    let pending = pool
        .submit(who.clone(), "later", "pending", "owner", 3)
        .unwrap();
    pool.close(4);
    assert!(pool.cancellation_requested(&lease));
    assert_eq!(
        pool.result(&who, &pending.task_id).unwrap().status,
        TaskStatus::Cancelled
    );
    assert_eq!(pool.snapshot().working, 1);
    pool.finish(
        &lease,
        Outcome::Completed("confirmed before cancellation".into()),
        true,
        false,
        5,
    )
    .unwrap();
    assert_eq!(
        pool.result(&who, &task.task_id).unwrap().status,
        TaskStatus::Completed
    );
    assert_eq!(pool.snapshot().offline, 1);
    assert!(pool.online(0).is_err());
}

#[test]
fn failed_execution_is_not_silently_replayed() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let who = actor("a");
    let task = pool
        .submit(who.clone(), "failure", "task", "owner", 1)
        .unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    pool.finish(&lease, Outcome::Failed("exit 7".into()), true, true, 3)
        .unwrap();
    assert_eq!(
        pool.result(&who, &task.task_id).unwrap().status,
        TaskStatus::Failed
    );
    assert!(!pool.result(&who, &task.task_id).unwrap().replay_safe);
    assert_eq!(pool.snapshot().idle, 1);
    assert!(pool.claim(0, epoch, 4).unwrap().is_none());
}

#[test]
fn confirmed_cancel_cleanup_can_reuse_a_live_slot_and_confirmed_results_survive_history_failure() {
    let pool = queue(1);
    let epoch = pool.online(0).unwrap();
    let who = actor("a");
    let task = pool.submit(who.clone(), "one", "task", "owner", 1).unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    pool.cancel(&who, &task.task_id, 3).unwrap();
    pool.finish(&lease, Outcome::Cancelled, true, true, 4)
        .unwrap();
    assert_eq!(pool.snapshot().idle, 1);
    pool.submit(who.clone(), "two", "next", "owner", 5).unwrap();
    let lease = pool.claim(0, epoch, 6).unwrap().unwrap();
    let result = pool
        .finish(
            &lease,
            Outcome::CompletedWithWarning(
                "actual reply".into(),
                "original history unavailable".into(),
            ),
            true,
            true,
            7,
        )
        .unwrap();
    assert_eq!(result.status, TaskStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("actual reply"));
    assert!(result.error.is_some());
    assert!(pool.cleaned_closed_pool(8).is_err());
    pool.close(9);
    pool.cleaned_closed_pool(10).unwrap();
    assert_eq!(pool.snapshot().working, 0);
    assert_eq!(pool.snapshot().offline, 1);
}

#[test]
fn unknown_result_eviction_keeps_an_actor_bound_retry_tombstone() {
    let pool = queue(1);
    let mut epoch = pool.online(0).unwrap();
    let who = actor("a");
    let original = pool
        .submit(who.clone(), "uncertain", "task", "owner A", 1)
        .unwrap();
    let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
    pool.finish(
        &lease,
        Outcome::Unknown("reply lost".into()),
        true,
        false,
        3,
    )
    .unwrap();
    epoch = pool.online(0).unwrap();
    for index in 0..MAX_FINISHED {
        pool.submit(
            who.clone(),
            &format!("later-{index}"),
            "later",
            "owner A",
            4,
        )
        .unwrap();
        let lease = pool.claim(0, epoch, 5).unwrap().unwrap();
        pool.finish(&lease, Outcome::Completed("done".into()), true, true, 6)
            .unwrap();
    }
    assert!(pool.result(&who, &original.task_id).is_err());
    assert!(
        pool.submit(who, "uncertain", "task", "new owner", 7)
            .unwrap_err()
            .contains("expired unknown")
    );
    assert_eq!(pool.snapshot().queued, 0);
    assert!(
        pool.submit(actor("b"), "uncertain", "unrelated", "owner B", 8)
            .is_ok()
    );
}

#[test]
fn full_unknown_retry_guard_refuses_new_admission_instead_of_forgetting_effects() {
    let pool = queue(1);
    let mut epoch = pool.online(0).unwrap();
    let who = actor("a");
    let mut last = None;
    for index in 0..MAX_FINISHED + MAX_UNKNOWN_KEYS + 1 {
        last = Some(
            pool.submit(who.clone(), &format!("unknown-{index}"), "task", "owner", 1)
                .unwrap(),
        );
        let lease = pool.claim(0, epoch, 2).unwrap().unwrap();
        pool.finish(
            &lease,
            Outcome::Unknown("no confirmed reply".into()),
            true,
            false,
            3,
        )
        .unwrap();
        epoch = pool.online(0).unwrap();
    }
    assert!(
        pool.submit(who.clone(), "new", "task", "owner", 4)
            .unwrap_err()
            .contains("guard is full")
    );
    assert_eq!(
        pool.result(&who, &last.unwrap().task_id).unwrap().status,
        TaskStatus::Unknown
    );
    assert_eq!(pool.snapshot().queued, 0);
}
