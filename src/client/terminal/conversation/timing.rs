//! Live timing is a display projection; rendering never advances a user queue.
use super::*;
use crate::runtime::time::{Entry, Snapshot, Status};

fn duration(ms: u64, remaining: bool) -> String {
    let seconds = if remaining {
        ms.div_ceil(1000)
    } else {
        ms / 1000
    };
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

fn counter(entry: &Entry) -> String {
    entry.remaining_ms.map_or_else(
        || format!("+{}", duration(entry.elapsed_ms, false)),
        |remaining| duration(remaining, true),
    )
}

fn hint(snapshot: &Snapshot) -> Option<String> {
    let entry = snapshot.active.first()?;
    let mode = if entry.remaining_ms.is_some() {
        "left"
    } else {
        "elapsed"
    };
    let others = snapshot.active.len().saturating_sub(1);
    Some(format!(
        "{} {} {mode}{} · /wait",
        if entry.kind == "timer" {
            "Timer"
        } else {
            "Wait"
        },
        counter(entry),
        if others == 0 {
            String::new()
        } else {
            format!(" · +{others}")
        }
    ))
}

fn detail(snapshot: &Snapshot) -> Option<String> {
    if snapshot.active.is_empty() && snapshot.recent.is_empty() {
        return None;
    }
    let mut lines = vec!["Timers and passive waits (monotonic)".into()];
    for entry in &snapshot.active {
        lines.push(format!(
            "#{} · {}",
            entry.id,
            panel::safe_text(&entry.label)
        ));
        lines.push(format!(
            "{} {}{}",
            counter(entry),
            if entry.remaining_ms.is_some() {
                "remaining"
            } else {
                "elapsed"
            },
            if entry.cancellable {
                " · /wait cancel <exact ID>"
            } else {
                " · display only"
            }
        ));
    }
    for entry in &snapshot.recent {
        let status = match entry.status {
            Status::Running => "running",
            Status::Elapsed => "elapsed",
            Status::Cancelled => "cancelled",
            Status::Expired => "expired",
            Status::Disconnected => "disconnected",
        };
        lines.push(format!(
            "#{} · {status} · {} elapsed",
            entry.id,
            duration(entry.elapsed_ms, false)
        ));
    }
    lines.push(
        "Cancelling a wait never cancels its job, consumes input or approves task completion."
            .into(),
    );
    Some(lines.join("\n"))
}

impl State {
    fn chat_timing_snapshot(&self) -> Snapshot {
        #[cfg(test)]
        if let Some(snapshot) = &self.conversation.timing_fixture {
            return snapshot.clone();
        }
        if self.preview {
            return Snapshot::default();
        }
        crate::runtime::time::snapshot()
    }

    pub(super) fn chat_timing_hint(&self) -> Option<String> {
        hint(&self.chat_timing_snapshot())
    }

    pub(super) fn chat_timing_detail(&self) -> Option<String> {
        detail(&self.chat_timing_snapshot())
    }

    pub(super) fn chat_clock_label(&self) -> Option<String> {
        let user = self.chat_user_wait_snapshot();
        if let Some(remaining) = user.active.iter().map(|entry| entry.remaining_ms).min() {
            return Some(format!("Wait {}", duration(remaining, true)));
        }
        let timing = self.chat_timing_snapshot();
        timing.active.first().map(|entry| {
            format!(
                "{} {}",
                if entry.kind == "timer" {
                    "Timer"
                } else {
                    "Wait"
                },
                counter(entry)
            )
        })
    }
}

#[cfg(test)]
mod tests;
