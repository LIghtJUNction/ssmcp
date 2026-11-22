//! Small parent-window projection of authoritative queue state. This is not
//! execution authority and never contains full task inputs, results or logs.
use super::queue::TaskStatus;
use serde::{Deserialize, Serialize};

pub(crate) const MAX_ACTIVITY_TASKS: usize = 8;
pub(crate) const MAX_ACTIVITY_TEXT: usize = 160;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskActivity {
    pub(crate) task_id: String,
    pub(crate) status: TaskStatus,
    pub(crate) slot: Option<usize>,
    pub(crate) epoch: Option<u64>,
    pub(crate) attempt: u64,
    pub(crate) owner_thread_id: Option<String>,
    pub(crate) worker_thread_id: Option<String>,
    pub(crate) summary: String,
    pub(crate) progress: Option<String>,
    pub(crate) last_command: Option<String>,
    pub(crate) last_log_sequence: Option<u64>,
    pub(crate) replay_safe: bool,
}

impl TaskActivity {
    pub(crate) fn status_label(&self) -> &'static str {
        match self.status {
            TaskStatus::Queued => "queued",
            TaskStatus::Working => "working",
            TaskStatus::Cancelling => "cancelling; worker still owns the claim",
            TaskStatus::Completed => "completed",
            TaskStatus::Failed => "failed",
            TaskStatus::Cancelled => "cancelled",
            TaskStatus::Unknown => "unknown; effects unconfirmed",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activity {
    pub(crate) generation: String,
    pub(crate) tasks: Vec<TaskActivity>,
    pub(crate) omitted: usize,
    pub(crate) truncated: bool,
}

impl Activity {
    pub(crate) fn valid(&self) -> bool {
        let identity = |value: &str| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        };
        let text =
            |value: &str| value.len() <= MAX_ACTIVITY_TEXT && !value.chars().any(char::is_control);
        let mut seen = std::collections::BTreeSet::new();
        self.tasks.len() <= MAX_ACTIVITY_TASKS
            && (identity(&self.generation)
                || (self.generation.is_empty() && self.tasks.is_empty() && self.omitted == 0))
            && self.truncated == (self.omitted > 0)
            && self
                .tasks
                .len()
                .checked_add(self.omitted)
                .is_some_and(|total| total <= 384)
            && self.tasks.iter().all(|task| {
                identity(&task.task_id)
                    && task
                        .task_id
                        .strip_prefix(&format!("worker_task_{}_", self.generation))
                        .is_some_and(|sequence| {
                            !sequence.is_empty()
                                && sequence.bytes().all(|byte| byte.is_ascii_digit())
                        })
                    && seen.insert(&task.task_id)
                    && task.owner_thread_id.as_deref().is_none_or(identity)
                    && task.worker_thread_id.as_deref().is_none_or(identity)
                    && task
                        .slot
                        .is_none_or(|slot| slot < super::config::MAX_WORKERS)
                    && task.slot.is_some() == (task.attempt > 0)
                    && task.slot.is_some() == task.epoch.is_some()
                    && task.epoch.is_none_or(|epoch| epoch > 0)
                    && task.last_log_sequence.is_none_or(|sequence| sequence > 0)
                    && (task.worker_thread_id.is_none() || task.slot.is_some())
                    && (task.status != TaskStatus::Queued || task.slot.is_none())
                    && (!matches!(task.status, TaskStatus::Working | TaskStatus::Cancelling)
                        || task.slot.is_some())
                    && (task.status != TaskStatus::Unknown || !task.replay_safe)
                    && text(&task.summary)
                    && task.progress.as_deref().is_none_or(text)
                    && task.last_command.as_deref().is_none_or(text)
            })
    }
}
