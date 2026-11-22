use super::*;
use crate::worker::queue::TaskStatus;

fn actor(id: &str) -> Actor {
    Actor {
        provider: "fixture".into(),
        subject: "owner".into(),
        participant: Some(id.into()),
        connection: 7,
    }
}

fn service(count: usize) -> (Queue<()>, Remote<()>) {
    let queue = Queue::new("lifecycle-fixture".into(), count).unwrap();
    let remote = Remote::new();
    remote.start(&queue).unwrap();
    (queue, remote)
}

fn claim(remote: &Remote<()>, queue: &Queue<()>, actor: &Actor, request: &str) -> Claim {
    let Claimed::Assigned { claim, .. } = remote.claim(queue, actor, request, 10).unwrap() else {
        panic!("expected assigned task")
    };
    claim
}

fn retirement(remote: &Remote<()>, queue: &Queue<()>, actor: &Actor) -> Retirement {
    let Claimed::Retiring { retirement } = remote.claim(queue, actor, "retirement", 20).unwrap()
    else {
        panic!("expected retirement acknowledgement request")
    };
    retirement
}

fn complete(claim: Claim) -> Completion {
    Completion {
        claim,
        status: Status::Completed,
        output: Some("real result".into()),
        error: None,
    }
}

fn counts(queue: &Queue<()>, expected: (usize, usize, usize, usize, usize)) {
    let view = queue.snapshot();
    assert_eq!(
        (
            view.total,
            view.idle,
            view.working,
            view.offline,
            view.retiring
        ),
        expected
    );
    assert_eq!(
        view.total,
        view.idle + view.working + view.offline + view.retiring
    );
    assert_eq!(
        view.registered_workers,
        view.slots.iter().filter(|slot| slot.registered).count()
    );
    assert!(view.total <= 128);
}

#[test]
fn remote_capacity_stays_offline_until_each_actor_really_waits() {
    let (queue, remote) = service(3);
    counts(&queue, (3, 0, 0, 3, 0));
    assert_eq!(queue.snapshot().registered_workers, 0);
    assert!(matches!(
        remote.claim(&queue, &actor("one"), "wait", 1).unwrap(),
        Claimed::Waiting
    ));
    counts(&queue, (3, 1, 0, 2, 0));
    let first_epoch = queue.snapshot().slots[0].epoch;
    for request in ["wait", "next-wait", "again"] {
        assert!(matches!(
            remote.claim(&queue, &actor("one"), request, 2).unwrap(),
            Claimed::Waiting
        ));
    }
    counts(&queue, (3, 1, 0, 2, 0));
    assert_eq!(queue.snapshot().slots[0].epoch, first_epoch);
    remote.claim(&queue, &actor("two"), "wait", 3).unwrap();
    counts(&queue, (3, 2, 0, 1, 0));
    assert_eq!(queue.snapshot().registered_workers, 2);
}

#[test]
fn growth_and_empty_shrink_do_not_start_workers_or_change_claim_epochs() {
    let (queue, remote) = service(3);
    remote.claim(&queue, &actor("one"), "idle", 1).unwrap();
    let epoch = queue.snapshot().slots[0].epoch;
    queue.resize(128).unwrap();
    counts(&queue, (128, 1, 0, 127, 0));
    assert!(queue.resize(129).is_err());
    queue.resize(1).unwrap();
    counts(&queue, (1, 1, 0, 0, 0));
    assert_eq!(queue.snapshot().slots[0].slot, 0);
    assert_eq!(queue.snapshot().slots[0].epoch, epoch);
    assert_eq!(queue.snapshot().desired_total, 1);
    queue.resize(0).unwrap();
    counts(&queue, (1, 0, 0, 0, 1));
    assert_eq!(queue.snapshot().registered_workers, 1);
    assert!(queue.resize(2).is_err());
    queue.resize(0).unwrap();
    let release = retirement(&remote, &queue, &actor("one"));
    assert!(
        !remote
            .release(&queue, &actor("one"), release.clone(), 30)
            .unwrap()
    );
    counts(&queue, (0, 0, 0, 0, 0));
    queue.resize(1).unwrap();
    remote
        .claim(&queue, &actor("replacement"), "wait", 40)
        .unwrap();
    counts(&queue, (1, 1, 0, 0, 0));
    assert!(queue.snapshot().slots[0].epoch > epoch);
    assert!(remote.release(&queue, &actor("one"), release, 41).unwrap());
    counts(&queue, (1, 1, 0, 0, 0));
    assert!(matches!(
        remote
            .claim(&queue, &actor("one"), "after-release", 42)
            .unwrap(),
        Claimed::Released { .. }
    ));
}

#[test]
fn shrink_selects_offline_then_idle_and_never_discards_working_results() {
    let (queue, remote) = service(3);
    let idle = actor("idle");
    let working = actor("working");
    let owner = actor("manager");
    let client = "a".repeat(64);
    remote.claim(&queue, &idle, "idle", 1).unwrap();
    let task = queue
        .submit_managed(owner.clone(), &client, "task", "real work", (), 2)
        .unwrap();
    let current = claim(&remote, &queue, &working, "work");
    assert_eq!(current.slot, 1);
    queue.resize_managed(&owner, &client, 2).unwrap();
    counts(&queue, (2, 1, 1, 0, 0));
    assert!(queue.resize_managed(&idle, &client, 1).is_err());
    queue.resize_managed(&owner, &client, 1).unwrap();
    counts(&queue, (2, 0, 1, 0, 1));
    assert_eq!(
        queue.result(&owner, &task.task_id).unwrap().status,
        TaskStatus::Working
    );
    let idle_release = retirement(&remote, &queue, &idle);
    remote.release(&queue, &idle, idle_release, 3).unwrap();
    counts(&queue, (1, 0, 1, 0, 0));
    assert_eq!(
        queue.snapshot().slots[0].slot,
        1,
        "remaining slots are never renumbered"
    );
    queue.resize(0).unwrap();
    counts(&queue, (1, 0, 1, 0, 0));
    assert_eq!(
        queue.result(&owner, &task.task_id).unwrap().status,
        TaskStatus::Cancelling
    );
    assert!(queue.retirement(&working).is_none());
    let Claimed::Assigned {
        claim: retry,
        lease,
    } = remote.claim(&queue, &working, "work", 5).unwrap()
    else {
        panic!("resize lost active claim")
    };
    assert_eq!(retry, current);
    assert!(queue.cancellation_requested(&lease));
    assert!(queue.retire_requested(&lease));
    let done = complete(current);
    let (result, duplicate) = remote.complete(&queue, &working, done.clone(), 6).unwrap();
    assert!(!duplicate);
    assert_eq!(result.output.as_deref(), Some("real result"));
    counts(&queue, (1, 0, 0, 0, 1));
    let release = retirement(&remote, &queue, &working);
    remote.release(&queue, &working, release, 7).unwrap();
    counts(&queue, (0, 0, 0, 0, 0));
    assert!(remote.complete(&queue, &working, done, 8).unwrap().1);
    assert_eq!(
        queue.result(&owner, &task.task_id).unwrap().status,
        TaskStatus::Completed
    );
    assert!(queue.is_manager(&owner, &client));
}

#[test]
fn retirement_release_requires_exact_actor_and_unmodified_epoch_receipt() {
    let (queue, remote) = service(1);
    let worker = actor("worker");
    remote.claim(&queue, &worker, "wait", 1).unwrap();
    queue.resize(0).unwrap();
    let receipt = retirement(&remote, &queue, &worker);
    let mut reconnected = worker.clone();
    reconnected.connection += 1;
    for other in [actor("another"), reconnected] {
        assert!(remote.release(&queue, &other, receipt.clone(), 2).is_err());
    }
    let mut altered = receipt.clone();
    altered.epoch += 1;
    assert!(remote.release(&queue, &worker, altered, 2).is_err());
    let mut altered = receipt.clone();
    altered.generation.push('x');
    assert!(remote.release(&queue, &worker, altered, 2).is_err());
    let mut altered = receipt.clone();
    altered.slot += 1;
    assert!(remote.release(&queue, &worker, altered, 2).is_err());
    let mut altered = receipt.clone();
    altered.id.push('x');
    assert!(remote.release(&queue, &worker, altered, 2).is_err());
    counts(&queue, (1, 0, 0, 0, 1));
    remote.release(&queue, &worker, receipt.clone(), 3).unwrap();
    assert!(
        remote
            .release(&queue, &actor("another"), receipt, 4)
            .is_err()
    );
    counts(&queue, (0, 0, 0, 0, 0));
}

#[test]
fn worker_reports_are_claim_scoped_bounded_and_idempotent() {
    let (queue, remote) = service(1);
    let owner = actor("manager");
    let worker = actor("worker");
    queue
        .submit(owner.clone(), "task", "Inspect actual code", (), 1)
        .unwrap();
    let current = claim(&remote, &queue, &worker, "wait");
    let report = Report {
        claim: current.clone(),
        request_id: "command-one".into(),
        event: ReportEvent::CommandStart,
        command: Some("rg -n worker src\n".into()),
        progress: None,
        detail: None,
    };
    assert!(remote.report(&queue, &owner, report.clone(), 2).is_err());
    assert!(!remote.report(&queue, &worker, report.clone(), 2).unwrap().0);
    let sequence = queue.snapshot().slots[0].next_log_sequence;
    assert!(remote.report(&queue, &worker, report.clone(), 3).unwrap().0);
    assert_eq!(queue.snapshot().slots[0].next_log_sequence, sequence);
    let mut changed = report.clone();
    changed.command = Some("different command".into());
    assert!(remote.report(&queue, &worker, changed, 4).is_err());
    let mut stale = report.clone();
    stale.request_id = "stale".into();
    stale.claim.epoch += 1;
    assert!(remote.report(&queue, &worker, stale, 4).is_err());
    let mut invalid = report.clone();
    invalid.request_id = "huge".into();
    invalid.command = Some("x".repeat(4097));
    assert!(remote.report(&queue, &worker, invalid, 4).is_err());
    let slot = &queue.snapshot().slots[0];
    assert_eq!(slot.last_command.as_deref(), Some("rg -n worker src"));
    assert_eq!(slot.task_summary.as_deref(), Some("Inspect actual code"));
    assert!(slot.logs.iter().any(
        |log| log.task_id.as_deref() == Some(current.task_id.as_str())
            && log.text.starts_with("Command started:")
    ));
    assert_eq!(slot.state, "working");
    assert!(slot.current_task_elapsed_ms.is_some());
    let progress = Report {
        claim: current.clone(),
        request_id: "progress-one".into(),
        event: ReportEvent::Progress,
        command: None,
        progress: Some("Checked the parser".into()),
        detail: None,
    };
    remote.report(&queue, &worker, progress, 5).unwrap();
    assert_eq!(
        queue.snapshot().slots[0].progress.as_deref(),
        Some("Checked the parser")
    );
    queue.resize(0).unwrap();
    let flags = remote.report(&queue, &worker, report, 6).unwrap();
    assert!(flags.1 && flags.2);
    remote
        .complete(&queue, &worker, complete(current), 7)
        .unwrap();
    let view = queue.snapshot();
    assert!(view.slots[0].current_task_elapsed_ms.is_none());
    assert_eq!(view.slots[0].recent_task_durations_ms.len(), 1);
}

#[test]
fn foreign_wait_does_not_register_or_consume_another_accounts_station() {
    let (queue, remote) = service(1);
    queue.attach(&actor("manager"), &"a".repeat(64), 1).unwrap();
    let mut foreign = actor("worker");
    foreign.subject = "foreign".into();
    assert!(matches!(
        remote.claim(&queue, &foreign, "wait", 2).unwrap(),
        Claimed::Waiting
    ));
    counts(&queue, (1, 0, 0, 1, 0));
    assert_eq!(queue.snapshot().registered_workers, 0);
}

#[test]
fn native_command_end_keeps_its_original_claim_when_the_next_task_started() {
    let (queue, remote) = service(1);
    let owner = actor("manager");
    let worker = actor("worker");
    assert!(
        remote
            .command_started(&queue, &worker, "not-working", "pwd", 1)
            .unwrap()
            .is_none()
    );
    let first = queue
        .submit(owner.clone(), "one", "first task", (), 1)
        .unwrap();
    let first_claim = claim(&remote, &queue, &worker, "first");
    let command = remote
        .command_started(&queue, &worker, "exec-1", "sleep 1", 2)
        .unwrap()
        .unwrap();
    remote
        .complete(&queue, &worker, complete(first_claim.clone()), 3)
        .unwrap();
    let second = queue.submit(owner, "two", "second task", (), 4).unwrap();
    claim(&remote, &queue, &worker, "second");
    remote
        .command_started(&queue, &worker, "exec-2", "pwd", 5)
        .unwrap()
        .unwrap();
    remote
        .command_finished(&queue, &command, "exit 0", 6)
        .unwrap();
    let view = queue.snapshot();
    assert_eq!(
        view.slots[0].task_id.as_deref(),
        Some(second.task_id.as_str())
    );
    assert_eq!(view.slots[0].last_command.as_deref(), Some("pwd"));
    let end = view.slots[0]
        .logs
        .iter()
        .find(|event| event.text.starts_with("Command finished: sleep 1"))
        .unwrap();
    assert_eq!(end.task_id.as_deref(), Some(first.task_id.as_str()));
    let next_sequence = view.slots[0].next_log_sequence;
    remote
        .command_finished(&queue, &command, "exit 0", 7)
        .unwrap();
    assert_eq!(queue.snapshot().slots[0].next_log_sequence, next_sequence);
    assert!(
        remote
            .command_finished(&queue, &command, "exit 1", 8)
            .is_err()
    );
    // A caller cannot manufacture the native-only historical-completion path.
    let forged_late = Report {
        claim: first_claim,
        request_id: "late-external".into(),
        event: ReportEvent::CommandEnd,
        command: Some("sleep 1".into()),
        progress: None,
        detail: Some("exit 0".into()),
    };
    assert!(remote.report(&queue, &worker, forged_late, 9).is_err());
}

#[test]
fn activity_inputs_cannot_supply_identity_or_forge_release_properties() {
    let value = serde_json::json!({
        "claim":{"id":"id","generation":"pool","slot":0,"epoch":1,"task_id":"task","attempt":1},
        "request_id":"progress-1","event":"progress","progress":"Observed one result"
    });
    assert!(serde_json::from_value::<Report>(value.clone()).is_ok());
    for field in [
        "actor",
        "owner",
        "provider",
        "subject",
        "agent_id",
        "connection",
        "native_end",
    ] {
        let mut forged = value.clone();
        forged[field] = serde_json::json!("forged");
        assert!(serde_json::from_value::<Report>(forged).is_err());
    }
    let receipt = serde_json::json!({"id":"id","generation":"pool","slot":0,"epoch":1});
    assert!(serde_json::from_value::<Retirement>(receipt.clone()).is_ok());
    let mut forged = receipt;
    forged["actor"] = serde_json::json!("another worker");
    assert!(serde_json::from_value::<Retirement>(forged).is_err());
}

#[test]
fn queued_tasks_survive_zero_capacity_and_resume_only_on_explicit_growth() {
    let (queue, remote) = service(2);
    let owner = actor("manager");
    let task = queue
        .submit(owner.clone(), "queued", "work", (), 1)
        .unwrap();
    queue.resize(0).unwrap();
    counts(&queue, (0, 0, 0, 0, 0));
    assert_eq!(queue.snapshot().queued, 1);
    assert_eq!(
        queue.result(&owner, &task.task_id).unwrap().status,
        TaskStatus::Queued
    );
    assert!(matches!(
        remote.claim(&queue, &actor("worker"), "wait", 2).unwrap(),
        Claimed::Waiting
    ));
    assert!(queue.submit(owner, "new", "work", (), 3).is_err());
    queue.resize(1).unwrap();
    assert_eq!(
        claim(&remote, &queue, &actor("worker"), "wait").task_id,
        task.task_id
    );
    counts(&queue, (1, 0, 1, 0, 0));
}
