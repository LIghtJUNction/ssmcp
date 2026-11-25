use super::*;

const MANAGER_CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn availability_requires_the_exact_manager_and_real_remote_registration() {
    let pool = queue(2);
    let owner = actor("manager");
    assert!(pool.availability(&owner, MANAGER_CLIENT).is_err());
    assert!(pool.snapshot().manager.is_none());
    pool.attach(&owner, MANAGER_CLIENT, 1).unwrap();
    assert!(
        !pool
            .availability(&owner, MANAGER_CLIENT)
            .unwrap()
            .accepting()
    );
    pool.remote_ready().unwrap();
    let initial = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(initial.accepting());
    assert_eq!(initial.projection.total, 2);
    assert_eq!(initial.projection.offline, 2);
    assert_eq!(initial.projection.registered_workers, 0);
    assert!(initial.projection.idle_slots.is_empty());
    assert!(pool.claim_remote(&actor("executor"), 2).unwrap().is_none());
    let registered = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(registered.revision > initial.revision);
    assert_eq!(registered.projection.idle, 1);
    assert_eq!(registered.projection.offline, 1);
    assert_eq!(registered.projection.registered_workers, 1);
    assert_eq!(registered.projection.idle_slots.len(), 1);
    assert_eq!(registered.projection.idle_slots[0].slot, 0);
    assert_eq!(
        registered.projection.idle_slots[0].epoch,
        pool.snapshot().slots[0].epoch
    );
    for foreign in [
        actor("observer"),
        Actor {
            provider: "other-provider".into(),
            ..owner.clone()
        },
        Actor {
            subject: "other-account".into(),
            ..owner.clone()
        },
        Actor {
            connection: owner.connection + 1,
            ..owner.clone()
        },
    ] {
        assert!(pool.availability(&foreign, MANAGER_CLIENT).is_err());
    }
    assert!(pool.availability(&owner, &"b".repeat(64)).is_err());
    pool.attach(&owner, MANAGER_CLIENT, 3).unwrap();
    pool.claim_remote(&actor("executor"), 4).unwrap();
    assert_eq!(
        pool.availability(&owner, MANAGER_CLIENT).unwrap().revision,
        registered.revision
    );
    assert_eq!(pool.snapshot().manager.unwrap().thread_id, "manager");

    let local = queue(1);
    local.attach(&owner, MANAGER_CLIENT, 1).unwrap();
    let epoch = local.online(0).unwrap();
    let available = local.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(available.accepting());
    assert_eq!(available.projection.registered_workers, 0);
    assert_eq!(
        available.projection.idle_slots,
        vec![IdleSlot { slot: 0, epoch }]
    );
}

#[test]
fn availability_ignores_retired_tombstones_and_keeps_higher_claim_ids() {
    let pool = queue(3);
    let owner = actor("manager");
    let idle = actor("idle");
    pool.remote_ready().unwrap();
    pool.attach(&owner, MANAGER_CLIENT, 1).unwrap();
    pool.claim_remote(&idle, 2).unwrap();
    for key in ["one", "two"] {
        pool.submit_managed(
            owner.clone(),
            MANAGER_CLIENT,
            key,
            "real work",
            "context",
            3,
        )
        .unwrap();
    }
    let first = pool.claim_remote(&actor("first"), 4).unwrap().unwrap();
    let second = pool.claim_remote(&actor("second"), 4).unwrap().unwrap();
    assert_eq!((first.slot, second.slot), (1, 2));
    pool.resize(2).unwrap();
    let retiring = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert_eq!(retiring.projection.total, 3);
    assert_eq!(retiring.projection.desired_total, 2);
    assert_eq!(retiring.projection.working, 2);
    assert_eq!(retiring.projection.retiring, 1);
    assert!(retiring.projection.idle_slots.is_empty());
    let receipt = pool.retirement(&idle).unwrap();
    pool.release_remote(&idle, &receipt, 5).unwrap();
    let released = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(released.revision > retiring.revision);
    assert_eq!(released.projection.total, 2);
    assert_eq!(released.projection.working, 2);
    assert_eq!(released.projection.registered_workers, 2);
    assert_eq!(released.projection.offline, 0);
    assert_eq!(released.projection.retiring, 0);
    assert_eq!(
        pool.state.lock().unwrap().slots.len(),
        3,
        "slot storage remains stable"
    );
    for lease in [&first, &second] {
        let result = pool
            .finish_remote(lease, Outcome::Completed("real result".into()), true, 6)
            .unwrap();
        assert_eq!(result.slot, Some(lease.slot));
        assert_eq!(result.status, TaskStatus::Completed);
    }
    let available = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert_eq!(
        available.projection.idle_slots,
        vec![
            IdleSlot {
                slot: first.slot,
                epoch: first.epoch
            },
            IdleSlot {
                slot: second.slot,
                epoch: second.epoch
            },
        ]
    );
    assert_eq!(
        available.projection.total,
        available.projection.idle
            + available.projection.working
            + available.projection.offline
            + available.projection.retiring
    );
}

#[test]
fn availability_stops_admission_until_retirement_and_zero_capacity_are_resolved() {
    let pool = queue(1);
    let owner = actor("manager");
    let worker = actor("executor");
    pool.remote_ready().unwrap();
    pool.attach(&owner, MANAGER_CLIENT, 1).unwrap();
    pool.claim_remote(&worker, 2).unwrap();
    pool.submit_managed(
        owner.clone(),
        MANAGER_CLIENT,
        "active",
        "real work",
        "context",
        3,
    )
    .unwrap();
    let claim = pool.claim_remote(&worker, 4).unwrap().unwrap();
    pool.resize(0).unwrap();
    let stopping = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(!stopping.accepting());
    assert_eq!(stopping.projection.total, 1);
    assert_eq!(stopping.projection.desired_total, 0);
    assert_eq!(stopping.projection.working, 1);
    assert_eq!(stopping.projection.retiring, 0);
    assert!(
        pool.submit_managed(
            owner.clone(),
            MANAGER_CLIENT,
            "rejected",
            "work",
            "context",
            5
        )
        .is_err()
    );
    pool.finish_remote(&claim, Outcome::Completed("real result".into()), true, 6)
        .unwrap();
    let waiting = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(!waiting.accepting());
    assert_eq!(waiting.projection.retiring, 1);
    assert_eq!(waiting.projection.working, 0);
    assert!(waiting.projection.idle_slots.is_empty());
    pool.release_remote(&worker, &pool.retirement(&worker).unwrap(), 7)
        .unwrap();
    let empty = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(!empty.accepting());
    assert_eq!(empty.projection.total, 0);
    assert_eq!(empty.projection.offline, 0);
    assert_eq!(empty.projection.registered_workers, 0);
    pool.resize(1).unwrap();
    let grown = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(grown.accepting());
    assert!(grown.revision > empty.revision);
    assert_eq!(grown.projection.total, 1);
    assert_eq!(grown.projection.offline, 1);
    assert!(grown.projection.idle_slots.is_empty());
    assert!(pool.is_manager(&owner, MANAGER_CLIENT));
}

#[test]
fn availability_revision_excludes_progress_and_native_command_log_updates() {
    let pool = queue(1);
    let owner = actor("manager");
    pool.remote_ready().unwrap();
    pool.submit_managed(
        owner.clone(),
        MANAGER_CLIENT,
        "work",
        "real work",
        "context",
        1,
    )
    .unwrap();
    let lease = pool.claim_remote(&actor("executor"), 2).unwrap().unwrap();
    let before = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    let mut pool_updates = pool.subscribe();
    for update in [
        ReportUpdate {
            event: ReportEvent::Progress,
            command: None,
            progress: Some("checked parser".into()),
            detail: None,
        },
        ReportUpdate {
            event: ReportEvent::CommandStart,
            command: Some("cargo check".into()),
            progress: None,
            detail: None,
        },
        ReportUpdate {
            event: ReportEvent::CommandEnd,
            command: Some("cargo check".into()),
            progress: None,
            detail: Some("exit 0".into()),
        },
    ] {
        pool.report(&lease, &update, 3).unwrap();
        assert!(pool_updates.has_changed().unwrap());
        drop(pool_updates.borrow_and_update());
        let after = pool.availability(&owner, MANAGER_CLIENT).unwrap();
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.projection, before.projection);
    }
    pool.finish_remote(&lease, Outcome::Completed("done".into()), true, 4)
        .unwrap();
    let completed = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(completed.revision > before.revision);
    pool.command_end_after_completion(
        &crate::worker::remote::Claim {
            id: "native-receipt".into(),
            generation: lease.generation.clone(),
            slot: lease.slot,
            epoch: lease.epoch,
            task_id: lease.task_id.clone(),
            attempt: lease.attempt,
        },
        &ReportUpdate {
            event: ReportEvent::CommandEnd,
            command: Some("cargo check".into()),
            progress: None,
            detail: Some("observed late exit 0".into()),
        },
        5,
    )
    .unwrap();
    assert_eq!(
        pool.availability(&owner, MANAGER_CLIENT).unwrap().revision,
        completed.revision
    );
    assert_eq!(
        pool.snapshot().slots[0]
            .logs
            .last()
            .unwrap()
            .task_id
            .as_deref(),
        Some(lease.task_id.as_str())
    );
}

#[test]
fn availability_reports_a_full_queue_as_not_accepting_until_a_real_claim() {
    let pool = queue(1);
    let owner = actor("manager");
    pool.remote_ready().unwrap();
    for index in 0..MAX_QUEUED {
        pool.submit_managed(
            owner.clone(),
            MANAGER_CLIENT,
            &format!("task-{index}"),
            "work",
            "context",
            1,
        )
        .unwrap();
    }
    let full = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(!full.accepting());
    assert_eq!(full.projection.queued, MAX_QUEUED);
    assert_eq!(full.projection.offline, 1);
    assert!(full.projection.idle_slots.is_empty());
    assert!(pool.claim_remote(&actor("executor"), 2).unwrap().is_some());
    let accepting = pool.availability(&owner, MANAGER_CLIENT).unwrap();
    assert!(accepting.accepting());
    assert!(accepting.revision > full.revision);
    assert_eq!(accepting.projection.queued, MAX_QUEUED - 1);
}

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

#[test]
fn two_hour_advice_is_strict_and_once_per_epoch_and_human_shrink_ranks_all_durations() {
    let q = queue(3);
    q.remote_ready().unwrap();
    let owner = actor("m");
    for i in 0..3 {
        q.submit(owner.clone(), &format!("key{i}"), "task", "context", 1)
            .unwrap();
        q.claim_remote(&actor(&format!("w{i}")), 2)
            .unwrap()
            .unwrap();
    }
    let t = Instant::now();
    let h = telemetry::TWO_HOURS_MS;
    {
        let mut state = q.state.lock().unwrap();
        for (i, s) in state.slots.iter_mut().enumerate() {
            s.total_work_ms = [h - 100, 2_000, 100][i];
            s.started = Some(t);
        }
        state.slots[0].progress = Some("waiting for build".into());
    }
    let edge = t + std::time::Duration::from_millis(100);
    assert!(q.overwork_notices(edge, 3).is_empty());
    let notices = q.overwork_notices(edge + std::time::Duration::from_millis(1), 4);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("waiting for build") && notices[0].contains("over 2h"));
    assert!(notices[0].find("/worker -").unwrap() < notices[0].find("waiting for build").unwrap());
    assert!(
        q.overwork_notices(edge + std::time::Duration::from_secs(2), 5)
            .is_empty()
    );
    assert!(
        q.human_resize(1).is_err(),
        "one human command cannot retire two stations"
    );
    let tickets = q.human_resize(2).unwrap();
    assert_eq!(tickets[0].slot, 0);
    let q = queue(2);
    q.remote_ready().unwrap();
    for i in 0..2 {
        q.claim_remote(&actor(&format!("member-{i}")), 1).unwrap();
    }
    q.state.lock().unwrap().slots[0].total_work_ms = 2000;
    assert_eq!(
        q.human_resize(1).unwrap()[0].slot,
        0,
        "longest wins even below two hours"
    );
}

#[test]
fn simultaneous_two_hour_advisories_share_one_banner_and_retain_each_station_report() {
    let q = queue(2);
    q.remote_ready().unwrap();
    for i in 0..2 {
        q.claim_remote(&actor(&format!("w{i}")), 1).unwrap();
        let mut state = q.state.lock().unwrap();
        state.slots[i].total_work_ms = telemetry::TWO_HOURS_MS + 1;
        state.slots[i].progress = Some(format!("progress-{i}"));
    }
    let now = Instant::now();
    let batch = q.overwork_notices(now, 2);
    assert_eq!(batch.len(), 1);
    assert!(batch[0].contains("2 workers") && batch[0].contains("#01, #02"));
    assert!(q.overwork_notices(now, 3).is_empty());
    for (i, s) in q.snapshot().slots.iter().enumerate() {
        assert_eq!(
            s.logs
                .iter()
                .filter(|log| log.text.contains(&format!("progress-{i}")))
                .count(),
            1
        );
    }
}

#[test]
fn an_available_station_starts_a_new_work_clock_after_authenticated_membership_ends() {
    let q = queue(1);
    q.remote_ready().unwrap();
    let old = actor("old");
    q.claim_remote(&old, 1).unwrap();
    q.state.lock().unwrap().slots[0].total_work_ms = telemetry::TWO_HOURS_MS + 1;
    let now = Instant::now() + std::time::Duration::from_secs(2);
    assert_eq!(q.overwork_notices(now, 2).len(), 1);
    assert_eq!(
        q.reconcile_roster(old.connection, now, &BTreeMap::new(), |_| None, 3),
        vec![old]
    );
    assert_eq!(q.snapshot().offline, 1);
    q.claim_remote(&actor("next"), 4).unwrap();
    let slot = &q.snapshot().slots[0];
    assert_eq!(slot.total_work_ms, 0);
    assert!(q.overwork_notices(now, 5).is_empty());
    q.state.lock().unwrap().slots[0].total_work_ms = telemetry::TWO_HOURS_MS + 1;
    assert_eq!(q.overwork_notices(now, 6).len(), 1);
}
