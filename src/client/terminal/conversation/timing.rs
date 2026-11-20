//! Live timing is a display projection; rendering never advances a user queue.
use super::*;
use crate::runtime::time::{Entry, ExecutionTiming, Snapshot, Status};

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

fn caption(entry: &Entry) -> &'static str {
    match entry.kind {
        "timer" => "计时",
        "task" => "任务",
        _ => "等待",
    }
}

fn selected(snapshot: &Snapshot) -> Option<&Entry> {
    // Show the nearest actual deadline, then a short expiry receipt, then
    // elapsed work. An open-ended operation never gains an invented deadline.
    snapshot
        .active
        .iter()
        .filter(|entry| entry.remaining_ms.is_some())
        .min_by_key(|entry| entry.remaining_ms)
        .or_else(|| {
            snapshot.recent.iter().find(|entry| {
                matches!(entry.status, Status::Elapsed | Status::Expired)
                    && entry.finished_age_ms.is_some_and(|age| age < 10_000)
            })
        })
        .or_else(|| snapshot.active.iter().max_by_key(|entry| entry.elapsed_ms))
}

fn clock_label(entry: &Entry) -> String {
    if matches!(entry.status, Status::Elapsed | Status::Expired) {
        return "计时 已到期".into();
    }
    match entry.remaining_ms {
        // A job/wait can still be cleaning up after its deadline. This is a
        // running state, not permission to declare completion or cancel it.
        Some(0) => format!(
            "{}·已到期",
            if entry.kind == "task" {
                "运行"
            } else {
                "等待"
            }
        ),
        Some(remaining) => format!("剩余 {}", duration(remaining, true)),
        None => format!(
            "{} {}",
            if entry.kind == "task" {
                "运行"
            } else {
                caption(entry)
            },
            duration(entry.elapsed_ms, false)
        ),
    }
}

fn hint(snapshot: &Snapshot) -> Option<String> {
    let entry = selected(snapshot)?;
    let others = snapshot
        .active
        .len()
        .saturating_sub(usize::from(entry.status == Status::Running));
    Some(format!(
        "{} · {}{} · /wait",
        caption(entry),
        clock_label(entry),
        if others == 0 {
            String::new()
        } else {
            format!(" · +{others}")
        }
    ))
}

fn detail(snapshot: &Snapshot) -> Option<String> {
    let mut lines = vec!["时间 · 计时与等待".into()];
    if let Ok(utc) = crate::runtime::time::utc_now() {
        lines.push(format!("当前时间 · {utc}"));
    }
    if snapshot.active.is_empty() && snapshot.recent.is_empty() {
        lines.push("暂无运行中的计时或等待".into());
    }
    for entry in &snapshot.active {
        lines.push(format!(
            "#{} · {}",
            entry.id,
            panel::safe_text(&entry.label)
        ));
        lines.push(format!(
            "{} · 已用 {}{}",
            clock_label(entry),
            duration(entry.elapsed_ms, false),
            if entry.cancellable {
                " · /wait cancel <exact ID>"
            } else {
                " · 仅显示"
            }
        ));
        if entry.remaining_ms == Some(0) {
            lines.push("截止时间已到，仍等待实际操作返回。".into());
        }
    }
    for entry in &snapshot.recent {
        let status = match entry.status {
            Status::Running => "运行",
            Status::Elapsed | Status::Expired => "已到期",
            Status::Cancelled => "已取消",
            Status::Disconnected => "已断开",
        };
        lines.push(format!(
            "#{} · {} · {status} · 已用 {}",
            entry.id,
            panel::safe_text(&entry.label),
            duration(entry.elapsed_ms, false)
        ));
    }
    lines.push("经过与剩余时间由单调时钟计算，不受系统日期调整影响。".into());
    lines.push("计时到期不代表任务完成；取消计时不会停止任务。".into());
    Some(lines.join("\n"))
}

impl State {
    pub(in crate::client::terminal) fn chat_timing_snapshot(&self) -> Snapshot {
        self.chat_timing_snapshot_at(Instant::now())
    }

    fn chat_timing_snapshot_at(&self, now: Instant) -> Snapshot {
        #[cfg(test)]
        let fixture = self.conversation.timing_fixture.clone();
        #[cfg(not(test))]
        let fixture: Option<Snapshot> = None;
        if self.preview && fixture.is_none() {
            return Snapshot::default();
        }
        let mut snapshot = fixture.unwrap_or_else(|| crate::runtime::time::snapshot_at(now));
        // This is the existing task-panel state, already holding the native
        // job's exact Instant projection. Never acquire the renderer mutex
        // from a frame: emission publishes renderer -> compositor.
        snapshot.active.extend(self.panel.tasks.iter().map(|task| {
            let timing = ExecutionTiming {
                started: task.started,
                deadline: task.deadline,
            };
            Entry {
                id: format!("task_{}", task.id),
                kind: "task",
                label: task.description.clone(),
                status: Status::Running,
                elapsed_ms: timing.elapsed_ms(now),
                remaining_ms: timing.remaining_ms(now),
                deadline_unix_ms: None,
                finished_age_ms: None,
                cancellable: false,
                episode: None,
                attribution: None,
            }
        }));
        snapshot
    }

    pub(super) fn chat_timing_hint(&self) -> Option<String> {
        hint(&self.chat_timing_snapshot())
    }

    pub(super) fn chat_timing_detail(&self) -> Option<String> {
        detail(&self.chat_timing_snapshot())
    }

    #[cfg(test)]
    pub(super) fn chat_clock_label(&self) -> Option<String> {
        self.chat_clock_label_at(Instant::now())
    }

    pub(super) fn chat_clock_label_at(&self, now: Instant) -> Option<String> {
        let timing = self.chat_timing_snapshot_at(now);
        let entry = selected(&timing);
        let user = self.chat_user_wait_snapshot();
        if let Some(remaining) = user.active.iter().map(|entry| entry.remaining_ms).min()
            && entry
                .and_then(|entry| entry.remaining_ms)
                .is_none_or(|other| remaining <= other)
        {
            return Some(format!("等待 {}", duration(remaining, true)));
        }
        entry.map(clock_label)
    }
}

#[cfg(test)]
mod tests;
