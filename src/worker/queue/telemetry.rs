//! Bounded, claim-scoped activity. Durations use a monotonic clock; displayed
//! wall time is metadata only and never used to manufacture task completion.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportEvent {
    FileRead,
    CommandStart,
    CommandEnd,
    Progress,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReportUpdate {
    pub(crate) activity_title: Option<String>,
    pub(crate) event: ReportEvent,
    pub(crate) command: Option<String>,
    pub(crate) progress: Option<String>,
    pub(crate) detail: Option<String>,
}

impl ReportUpdate {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let valid = |text: &Option<String>, maximum: usize| {
            text.as_ref().is_none_or(|text| {
                !text.trim().is_empty() && text.len() <= maximum && !text.contains('\0')
            })
        };
        if !valid(&self.command, 4096) || !valid(&self.progress, 1024) || !valid(&self.detail, 1024)
        {
            return Err("worker activity fields must be nonempty, bounded text without NUL".into());
        }
        match self.event {
            ReportEvent::FileRead if self.command.is_none() && self.progress.is_none() && self.detail.as_deref().is_some_and(station::valid_path) => Ok(()),
            ReportEvent::CommandStart if self.command.is_some() && self.progress.is_none() && self.detail.is_none() => Ok(()),
            ReportEvent::CommandEnd if self.command.is_some() && self.progress.is_none() => Ok(()),
            ReportEvent::Progress if self.progress.is_some() && self.command.is_none() && self.detail.is_none() => Ok(()),
            _ => Err("file_read requires a relative workspace path in detail; command_start requires command; command_end requires command and optional detail; progress requires progress only".into()),
        }
    }
}

impl<T: Clone> Queue<T> {
    pub(in crate::worker) fn report(
        &self,
        lease: &Lease<T>,
        update: &ReportUpdate,
        now_ms: u64,
    ) -> Result<(), String> {
        update.validate()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !valid(&state, &self.generation, lease) {
            return Err("worker activity requires a current exact task claim".into());
        }
        let slot = &mut state.slots[lease.slot];
        let text = match update.event {
            ReportEvent::FileRead => {
                slot.notebook.read(
                    update.detail.as_deref().expect("validated path"),
                    "self_reported",
                    &lease.task_id,
                    now_ms,
                );
                "Worker reported a file read".into()
            }
            ReportEvent::CommandStart => {
                let command = display_text(update.command.as_deref().unwrap_or_default(), 768);
                slot.last_command = Some(command.clone());
                format!("Command started: {command}")
            }
            ReportEvent::CommandEnd => {
                let command = display_text(update.command.as_deref().unwrap_or_default(), 768);
                // A command may finish after a later command has started. The
                // last-started command remains the truthful current display.
                let detail = update
                    .detail
                    .as_deref()
                    .map(|value| display_text(value, 192));
                match detail {
                    Some(detail) => format!("Command finished: {command}; {detail}"),
                    None => format!("Command finished: {command}"),
                }
            }
            ReportEvent::Progress => {
                let progress = display_text(update.progress.as_deref().unwrap_or_default(), 512);
                slot.progress = Some(progress.clone());
                format!("Progress: {progress}")
            }
        };
        log_with_title(
            slot,
            now_ms,
            "info",
            &text,
            Some(&lease.task_id),
            update.activity_title.as_deref(),
        );
        self.record_task_event(
            &mut state,
            "worker.progress",
            &lease.task_id,
            // Older relays accept only these established journal activity kinds.
            // The station notebook retains the specific read path and provenance.
            Some(match update.event {
                ReportEvent::FileRead => ReportEvent::Progress,
                _ => update.event.clone(),
            }),
            now_ms,
        );
        self.changed(&mut state);
        Ok(())
    }

    /// A native command can end after its task was reported complete. Keep its
    /// original attribution, but never overwrite a later task's live fields.
    pub(in crate::worker) fn command_end_after_completion(
        &self,
        claim: &super::super::remote::Claim,
        update: &ReportUpdate,
        now_ms: u64,
    ) -> Result<(), String> {
        update.validate()?;
        if update.event != ReportEvent::CommandEnd {
            return Err("only an observed native command end may follow task completion".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if claim.generation != self.generation
            || !state.tasks.get(&claim.task_id).is_some_and(|task| {
                task.view.slot == Some(claim.slot)
                    && task.view.attempt == claim.attempt
                    && task.view.status.terminal()
            })
        {
            return Err("completed command task is no longer retained".into());
        }
        let slot = state
            .slots
            .get_mut(claim.slot)
            .filter(|slot| slot.enabled && slot.epoch == claim.epoch)
            .ok_or("completed command station epoch is no longer current")?;
        let command = display_text(update.command.as_deref().unwrap_or_default(), 768);
        let detail = update
            .detail
            .as_deref()
            .map(|value| display_text(value, 192));
        let text = match detail {
            Some(detail) => format!("Command finished: {command}; {detail}"),
            None => format!("Command finished: {command}"),
        };
        log_with_title(
            slot,
            now_ms,
            "info",
            &text,
            Some(&claim.task_id),
            update.activity_title.as_deref(),
        );
        self.record_task_event(
            &mut state,
            "worker.progress",
            &claim.task_id,
            Some(update.event.clone()),
            now_ms,
        );
        self.changed(&mut state);
        Ok(())
    }
}

/// Keep only a bounded single-line display copy. The native job retains its original title.
pub(in crate::worker) fn activity_title(title: &str) -> Option<String> {
    let redacted = crate::audit::event(serde_json::Value::String(title.into()));
    let clean = redacted.as_str()?.replace(
        |c| matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'),
        " ",
    );
    let title = display_text(&clean, 256);
    (!title.is_empty()).then_some(title)
}

pub(in crate::worker) fn display_text(text: &str, limit: usize) -> String {
    let mut display = String::new();
    for character in text.chars() {
        if display.len() > limit {
            break;
        }
        if character.is_control() {
            if matches!(character, '\n' | '\r' | '\t') && !display.ends_with(' ') {
                display.push(' ');
            }
        } else {
            display.push(character);
        }
    }
    bounded(display.trim(), limit)
}

pub(super) fn elapsed_ms(slot: &Slot) -> Option<u64> {
    slot.started
        .map(|started| started.elapsed().as_millis().min(u64::MAX as u128) as u64)
}

pub(super) fn settle_time(slot: &mut Slot, confirmed: bool) {
    if let Some(elapsed) = elapsed_ms(slot) {
        slot.total_work_ms = slot.total_work_ms.saturating_add(elapsed);
        if confirmed {
            slot.recent_task_durations_ms.push_back(elapsed);
            while slot.recent_task_durations_ms.len() > 3 {
                slot.recent_task_durations_ms.pop_front();
            }
        }
    }
    slot.started = None;
    slot.task_started_at_ms = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn durations_use_observed_monotonic_time_and_keep_only_three_confirmed_tasks() {
        let queue = Queue::new("time-fixture".into(), 1).unwrap();
        let epoch = queue.online(0).unwrap();
        let owner = Actor {
            provider: "fixture".into(),
            subject: "owner".into(),
            participant: Some("sender".into()),
            connection: 1,
        };
        for index in 0..5 {
            queue
                .submit(
                    owner.clone(),
                    &format!("task-{index}"),
                    "actual task",
                    (),
                    u64::MAX - index,
                )
                .unwrap();
            let lease = queue.claim(0, epoch, u64::MAX - index).unwrap().unwrap();
            {
                let mut state = queue.state.lock().unwrap();
                state.slots[0].started = Some(
                    Instant::now() - Duration::from_millis(if index == 4 { 2500 } else { 25 }),
                );
            }
            let current = queue.snapshot();
            assert!(current.slots[0].current_task_elapsed_ms.unwrap() >= 25);
            assert!(current.slots[0].total_work_ms >= 25 * (index + 1));
            let outcome = if index == 4 {
                Outcome::Unknown("connection lost".into())
            } else {
                Outcome::Completed("confirmed".into())
            };
            let result = queue.finish(&lease, outcome, true, true, 1).unwrap();
            let view = queue.snapshot();
            assert_eq!(
                view.slots[0].recent_task_durations_ms.len(),
                (index as usize + 1).min(3)
            );
            assert!(
                view.slots[0]
                    .recent_task_durations_ms
                    .iter()
                    .all(|elapsed| *elapsed >= 25)
            );
            assert!(view.slots[0].current_task_elapsed_ms.is_none());
            assert!(view.slots[0].task_started_at_ms.is_none());
            assert!(
                queue
                    .finish(&lease, Outcome::Completed("retry".into()), true, true, 2)
                    .is_err()
            );
            if index == 4 {
                assert_eq!(result.status, TaskStatus::Unknown);
                assert_eq!(
                    view.slots[0].recent_task_durations_ms,
                    current.slots[0].recent_task_durations_ms
                );
            }
        }
        assert!(queue.snapshot().slots[0].total_work_ms >= 125);
    }

    #[test]
    fn native_activity_titles_are_single_line_redacted_and_utf8_bounded() {
        let bounded =
            activity_title(&format!("  核验\n本机\u{202e}{}  ", "步骤".repeat(100))).unwrap();
        assert!(bounded.starts_with("核验 本机 "));
        assert!(bounded.ends_with('…'));
        assert!(bounded.len() <= 256);
        assert!(activity_title(" \r\t\u{202a} ").is_none());
        let private = activity_title("Check password=fixture-value").unwrap();
        assert!(!private.contains("fixture-value"));
    }

    #[test]
    fn display_text_is_utf8_bounded_and_cannot_inject_terminal_controls() {
        let shown = display_text("\x1b[31mhello\nworld\t\0\u{7}", 14);
        assert!(!shown.chars().any(char::is_control));
        assert!(shown.len() <= 14);
        let unicode = display_text(&"执行".repeat(300), 256);
        assert!(unicode.len() <= 256);
        assert!(unicode.ends_with('…'));
    }
}
