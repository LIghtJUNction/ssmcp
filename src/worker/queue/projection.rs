//! A bounded view for the owning parent window, taken under the queue lock.
use super::super::activity::{Activity, MAX_ACTIVITY_TASKS, MAX_ACTIVITY_TEXT, TaskActivity};
use super::*;

pub(super) fn activity<T>(state: &State<T>, generation: &str) -> Activity {
    task_activity(state, generation, |_| true, state.tasks.len())
}

pub(super) fn managed_activity<T>(state: &State<T>, generation: &str) -> Activity {
    let count = state.tasks.values().filter(|task| task.managed).count();
    task_activity(state, generation, |task| task.managed, count)
}

fn task_activity<T>(
    state: &State<T>,
    generation: &str,
    visible: impl Fn(&Task<T>) -> bool,
    count: usize,
) -> Activity {
    // Working slots first, then the actual FIFO, then most recently finished.
    // Wall-clock timestamps and log text never choose or manufacture a result.
    let working = state.slots.iter().filter_map(|slot| match &slot.state {
        SlotState::Working(id) => Some(id),
        _ => None,
    });
    let mut seen = BTreeSet::new();
    let tasks: Vec<_> = working
        .chain(state.fifo.iter())
        .chain(state.finished.iter().rev())
        .filter(|id| seen.insert(*id))
        .filter_map(|id| state.tasks.get(id))
        .filter(|task| visible(task))
        .take(MAX_ACTIVITY_TASKS)
        .map(|task| {
            let original_station = task
                .view
                .slot
                .and_then(|index| state.slots.get(index))
                .filter(|slot| Some(slot.epoch) == task.claim_epoch);
            // A completed task must not inherit the next occupant's reports.
            let live = task.view.slot.and_then(|index| state.slots.get(index)).filter(|slot| {
                matches!(&slot.state, SlotState::Working(id) if id == &task.view.task_id)
            });
            TaskActivity {
                task_id: task.view.task_id.clone(),
                status: task.view.status,
                slot: task.view.slot,
                epoch: task.claim_epoch,
                attempt: task.view.attempt,
                owner_thread_id: task.actor.participant.clone(),
                worker_thread_id: task.claimed_by.clone(),
                summary: display_text(&task.text, MAX_ACTIVITY_TEXT),
                progress: live
                    .and_then(|slot| slot.progress.as_deref())
                    .map(|text| display_text(text, MAX_ACTIVITY_TEXT)),
                last_command: live
                    .and_then(|slot| slot.last_command.as_deref())
                    .map(|text| display_text(text, MAX_ACTIVITY_TEXT)),
                replay_safe: task.view.replay_safe,
                last_log_sequence: original_station.and_then(|slot| {
                    slot.logs
                        .iter()
                        .rev()
                        .find(|entry| entry.task_id.as_deref() == Some(&task.view.task_id))
                        .map(|entry| entry.sequence)
                }),
            }
        })
        .collect();
    Activity {
        generation: generation.to_owned(),
        omitted: count.saturating_sub(tasks.len()),
        truncated: count > tasks.len(),
        tasks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(thread: &str) -> Actor {
        Actor {
            provider: "fixture".into(),
            subject: "account".into(),
            participant: Some(thread.into()),
            connection: 1,
        }
    }

    #[test]
    fn activity_keeps_real_claimants_and_terminal_results_across_station_reuse() {
        let queue = Queue::new("activity".into(), 1).unwrap();
        queue.remote_ready().unwrap();
        let owner = actor("actual-owner");
        let worker = actor("actual-worker");
        queue.attach(&owner, &"a".repeat(64), 1).unwrap();
        let first = queue
            .submit(owner.clone(), "first", "First\nreal task", (), 2)
            .unwrap();
        let (_, queued) = queue.snapshot_with_activity();
        assert!(queued.valid());
        assert_eq!(queued.generation, "activity");
        assert_eq!(queued.tasks[0].status, TaskStatus::Queued);
        assert_eq!(
            queued.tasks[0].owner_thread_id.as_deref(),
            Some("actual-owner")
        );
        assert_eq!(queued.tasks[0].worker_thread_id, None);
        assert_eq!(queued.tasks[0].attempt, 0);
        assert_eq!(queued.tasks[0].epoch, None);

        let claim = queue.claim_remote(&worker, 3).unwrap().unwrap();
        queue
            .report(
                &claim,
                &ReportUpdate {
                    event: ReportEvent::Progress,
                    command: None,
                    progress: Some("Observed fixture pass".into()),
                    detail: None,
                },
                4,
            )
            .unwrap();
        queue
            .report(
                &claim,
                &ReportUpdate {
                    event: ReportEvent::CommandStart,
                    command: Some("cargo test fixture".into()),
                    progress: None,
                    detail: None,
                },
                5,
            )
            .unwrap();
        let (snapshot, running) = queue.snapshot_with_activity();
        assert_eq!(snapshot.working, 1);
        assert!(running.valid());
        assert_eq!(running.tasks[0].epoch, Some(claim.epoch));
        assert_eq!(
            running.tasks[0].last_log_sequence,
            snapshot.slots[0].logs.last().map(|entry| entry.sequence)
        );
        assert_eq!(
            running.tasks[0].worker_thread_id.as_deref(),
            Some("actual-worker")
        );
        assert_eq!(running.tasks[0].summary, "First real task");
        assert_eq!(
            running.tasks[0].progress.as_deref(),
            Some("Observed fixture pass")
        );
        assert_eq!(
            running.tasks[0].last_command.as_deref(),
            Some("cargo test fixture")
        );
        queue
            .finish_remote(&claim, Outcome::Completed("PRIVATE RESULT".into()), true, 6)
            .unwrap();

        queue.resize(0).unwrap();
        let retirement = queue.retirement(&worker).unwrap();
        queue.release_remote(&worker, &retirement, 7).unwrap();
        queue.resize(1).unwrap();
        queue
            .submit(owner.clone(), "second", "Second task", (), 8)
            .unwrap();
        let next = queue
            .claim_remote(&actor("next-worker"), 9)
            .unwrap()
            .unwrap();
        let (snapshot, reused) = queue.snapshot_with_activity();
        assert_eq!(snapshot.working, 1);
        assert_eq!(reused.tasks[0].task_id, next.task_id);
        let completed = reused
            .tasks
            .iter()
            .find(|task| task.task_id == first.task_id)
            .unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(completed.epoch, Some(claim.epoch));
        assert_ne!(completed.epoch, Some(next.epoch));
        assert_eq!(completed.last_log_sequence, None);
        assert_eq!(completed.worker_thread_id.as_deref(), Some("actual-worker"));
        assert_eq!(completed.progress, None);
        assert_eq!(completed.last_command, None);
        assert!(
            !serde_json::to_string(&reused)
                .unwrap()
                .contains("PRIVATE RESULT")
        );

        queue
            .finish_remote(
                &next,
                Outcome::Unknown("effects unconfirmed".into()),
                true,
                10,
            )
            .unwrap();
        let (_, uncertain) = queue.snapshot_with_activity();
        assert!(uncertain.valid());
        assert_eq!(uncertain.tasks[0].status, TaskStatus::Unknown);
        assert!(!uncertain.tasks[0].replay_safe);
        assert!(
            queue
                .claim_remote(&actor("next-worker"), 11)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn activity_is_bounded_and_local_executors_have_no_invented_thread_identity() {
        let queue = Queue::new("bounded-activity".into(), 1).unwrap();
        let epoch = queue.online(0).unwrap();
        let owner = actor("owner");
        for index in 0..12 {
            queue
                .submit(
                    owner.clone(),
                    &format!("task-{index}"),
                    &"中文\n\t\u{1b}\\\"".repeat(200),
                    (),
                    index,
                )
                .unwrap();
            let claim = queue.claim(0, epoch, index).unwrap().unwrap();
            queue
                .finish(
                    &claim,
                    Outcome::Completed("result".into()),
                    true,
                    true,
                    index,
                )
                .unwrap();
        }
        let (_, activity) = queue.snapshot_with_activity();
        assert!(activity.valid());
        assert_eq!(activity.tasks.len(), MAX_ACTIVITY_TASKS);
        assert_eq!(activity.omitted, 4);
        assert!(activity.truncated);
        assert!(activity.tasks[0].task_id.ends_with("_12"));
        assert!(
            activity
                .tasks
                .iter()
                .all(|task| task.worker_thread_id.is_none())
        );
        assert!(
            activity
                .tasks
                .iter()
                .all(|task| task.summary.len() <= MAX_ACTIVITY_TEXT)
        );
    }
    #[test]
    fn full_task_text_is_kept_for_the_local_view_without_expanding_serialized_snapshots() {
        let queue = Queue::new("full-text".into(), 1).unwrap();
        queue.remote_ready().unwrap();
        let text = "First line 中文\nSecond line with details\n".repeat(200);
        queue
            .submit(actor("owner"), "full-text", &text, (), 1)
            .unwrap();
        queue.claim_remote(&actor("worker"), 2).unwrap().unwrap();
        let (snapshot, activity) = queue.snapshot_with_activity();
        assert_eq!(snapshot.slots[0].task_text.as_deref(), Some(text.as_str()));
        assert!(snapshot.slots[0].task_summary.as_ref().unwrap().len() <= 256);
        assert!(activity.tasks[0].summary.len() <= MAX_ACTIVITY_TEXT);
        let serialized = serde_json::to_value(&snapshot).unwrap();
        assert!(serialized["slots"][0].get("task_text").is_none());
    }
}
