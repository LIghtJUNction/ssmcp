//! Live timing is a display projection; rendering never advances a user queue.
use super::*;
mod dashboard;
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
        _ => "观察",
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
    if let Some(wait) = &entry.wait {
        use crate::wire::wait_display::Outcome;
        if wait.outcome != Outcome::Pending {
            return wait_outcome(wait.outcome).into();
        }
    }
    if matches!(entry.status, Status::Elapsed | Status::Expired) {
        return format!("{} 已到期", caption(entry));
    }
    match entry.remaining_ms {
        // A job/wait can still be cleaning up after its deadline. This is a
        // running state, not permission to declare completion or cancel it.
        Some(0) => format!(
            "{}·已到期",
            if entry.kind == "task" {
                "运行"
            } else {
                caption(entry)
            }
        ),
        Some(remaining) if !matches!(entry.kind, "task" | "timer") => {
            if let Some(window) = entry.wait.as_ref().and_then(|wait| wait.window_ms) {
                return format!(
                    "观察剩余 ≤ {}",
                    exact_wait_time(live_wait_bound(remaining, Some(window)))
                );
            }
            format!("观察剩余 {}", duration(remaining, true))
        }
        Some(remaining) => format!("剩余 {}", duration(remaining, true)),
        None => format!(
            "{} {}",
            if entry.kind == "task" {
                "运行"
            } else {
                match entry.kind {
                    "timer" => "计时",
                    _ => "观察已用",
                }
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
    Some(
        dashboard::lines(snapshot)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

impl State {
    pub(in crate::client::terminal) fn chat_timing_snapshot(&self) -> Snapshot {
        self.chat_timing_snapshot_at(Instant::now())
    }

    fn chat_timing_snapshot_at(&self, now: Instant) -> Snapshot {
        #[cfg(test)]
        let mut snapshot = {
            let fixture = self.conversation.timing_fixture.clone();
            if self.preview && fixture.is_none() {
                return Snapshot::default();
            }
            fixture.unwrap_or_else(|| crate::runtime::time::snapshot_at(now))
        };
        #[cfg(not(test))]
        let mut snapshot = {
            if self.preview {
                return Snapshot::default();
            }
            crate::runtime::time::snapshot_at(now)
        };
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
                wait: None,
                durable: None,
            }
        }));
        snapshot
    }

    pub(super) fn chat_timing_hint(&self) -> Option<String> {
        hint(&self.chat_timing_snapshot())
    }

    pub(super) fn chat_wait_rows(&mut self, height: u16) -> Vec<String> {
        use unicode_width::UnicodeWidthChar;
        let width = usize::from(self.geometry.left.saturating_sub(2).max(1));
        let mut source = dashboard::lines(&self.chat_timing_snapshot());
        source.push((String::new(), PanelTone::Normal));
        source.push(("等待你的输入".into(), PanelTone::Heading));
        source.extend(
            user_wait::detail(&self.chat_user_wait_snapshot())
                .lines()
                .map(|line| (line.to_owned(), PanelTone::Muted)),
        );
        let mut rows = Vec::new();
        for (line, tone) in source {
            let safe = panel::safe_text(&line);
            let mut row = String::new();
            let mut used = 0;
            for character in safe.chars() {
                let cells = character.width().unwrap_or(0);
                if used + cells > width && !row.is_empty() {
                    rows.push((std::mem::take(&mut row), tone));
                    used = 0;
                }
                row.push(character);
                used += cells;
            }
            rows.push((row, tone));
        }
        self.conversation.detail_scroll = self
            .conversation
            .detail_scroll
            .min(rows.len().saturating_sub(usize::from(height)));
        rows.into_iter()
            .skip(self.conversation.detail_scroll)
            .take(usize::from(height))
            .map(|(text, tone)| self.chat_line(&text, tone, self.geometry.left))
            .collect()
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

fn wait_duration(ms: u64) -> String {
    if ms > 0 && ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", ms / 1000)
    } else {
        duration(ms, false)
    }
}

// The budget is fixed. Keep its millisecond precision even when the live clock
// ticks in whole seconds.
fn exact_wait_time(ms: u64) -> String {
    match ms {
        1..1000 => format!("{ms}ms"),
        _ if !ms.is_multiple_of(1000) => format!("{}.{:03}", duration(ms, false), ms % 1000),
        _ => duration(ms, false),
    }
}

fn wait_budget(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        wait_duration(ms)
    } else {
        exact_wait_time(ms)
    }
}

// A live countdown is an upper bound until the next whole-second tick. Its
// displayed value cannot exceed the configured window, including at startup.
fn live_wait_bound(remaining: u64, window: Option<u64>) -> u64 {
    let rounded = remaining.div_ceil(1000).saturating_mul(1000);
    window.map_or(rounded, |limit| rounded.min(limit))
}

fn wait_mode(mode: crate::wire::wait_display::Mode) -> &'static str {
    use crate::wire::wait_display::Mode;
    match mode {
        Mode::Any => "ANY",
        Mode::All => "ALL",
        Mode::Single => "SINGLE",
        Mode::Change => "CHANGE",
    }
}

fn wait_outcome(outcome: crate::wire::wait_display::Outcome) -> &'static str {
    use crate::wire::wait_display::Outcome;
    match outcome {
        Outcome::Pending => "Pending",
        Outcome::Triggered => "Triggered",
        Outcome::WindowExpired => "Window expired",
        Outcome::ObservationStopped => "Observation stopped",
        Outcome::Unknown => "Unknown",
    }
}

fn compact_wait_outcome(outcome: crate::wire::wait_display::Outcome) -> &'static str {
    if outcome == crate::wire::wait_display::Outcome::ObservationStopped {
        "Wait stopped"
    } else {
        wait_outcome(outcome)
    }
}

#[derive(Clone)]
struct WaitCard {
    key: String,
    heading: String,
    detail: String,
    entry: Entry,
}

fn wait_card(entry: &Entry) -> Option<WaitCard> {
    if entry.durable.is_some() {
        return dashboard::card(entry);
    }
    use crate::wire::wait_display::{Mode, Outcome};
    let wait = entry.wait.as_ref()?;
    let target = wait
        .targets
        .first()
        .map(String::as_str)
        .unwrap_or("Requested condition");
    let budget = wait
        .window_ms
        .map(|ms| format!("/{}", wait_budget(ms)))
        .unwrap_or_default();
    let heading = format!(
        "{} · {} · {}{budget} · {}",
        wait_mode(wait.mode),
        compact_wait_outcome(wait.outcome),
        wait_duration(entry.elapsed_ms),
        if wait.targets.len() > 1 {
            format!(
                "{} +{}",
                panel::fit(&panel::safe_text(target), 18),
                wait.targets.len() - 1
            )
        } else {
            panel::fit(&panel::safe_text(target), 22)
        }
    );
    let mut detail = vec![
        format!("Outcome: {}", wait_outcome(wait.outcome)),
        match wait.mode {
            Mode::Any => "ANY: at least one target must report ready.",
            Mode::All if wait.operation == "ssmcp.worker.wait_for" => {
                "ALL: every condition must be satisfied in the same observation."
            }
            Mode::All => "ALL: every target must report ready.",
            Mode::Single => "SINGLE: observe only the named target.",
            Mode::Change => "CHANGE: observe the requested capacity revision.",
        }
        .to_owned(),
    ];
    for (index, target) in wait.targets.iter().enumerate() {
        detail.push(format!("{}. {}", index + 1, panel::safe_text(target)));
    }
    let mut timing = format!(
        "Elapsed {} · window limit {}",
        if wait.outcome == Outcome::Pending {
            duration(entry.elapsed_ms, false)
        } else {
            exact_wait_time(entry.elapsed_ms)
        },
        wait.window_ms
            .map(exact_wait_time)
            .unwrap_or_else(|| "not specified".into())
    );
    if wait.outcome == Outcome::Pending
        && let Some(remaining) = entry.remaining_ms
    {
        timing.push_str(&format!(
            " · remaining ≤ {}",
            exact_wait_time(live_wait_bound(remaining, wait.window_ms))
        ));
    }
    detail.push(timing);
    detail.push(match wait.outcome {
        Outcome::Pending if entry.remaining_ms == Some(0) =>
            "The observation budget has elapsed; the actual return is still pending.",
        Outcome::Pending =>
            "Observation pending. The window budget is not the task's execution deadline.",
        Outcome::Triggered =>
            "The requested condition triggered a return. This does not establish overall task completion.",
        Outcome::WindowExpired =>
            "This observation window expired. Expiry does not cancel execution or establish task completion.",
        Outcome::ObservationStopped =>
            "Only this observation stopped. Stopping observation does not cancel execution.",
        Outcome::Unknown if entry.remaining_ms == Some(0) =>
            "The local observation budget elapsed without a terminal receipt. The return remains unknown.",
        Outcome::Unknown =>
            "The outcome is unknown. Do not infer success, cancellation or permission to replay.",
    }.into());
    detail.push(format!("Operation {}", panel::safe_text(&wait.operation)));
    detail.extend(
        wait.references
            .iter()
            .map(|reference| panel::safe_text(reference)),
    );
    detail.push(format!(
        "Observer ID {}{}",
        panel::safe_text(&entry.id),
        entry
            .episode
            .map(|episode| format!(" · episode {episode}"))
            .unwrap_or_default()
    ));
    if let Some(attribution) = &entry.attribution
        && let Some(agent) = &attribution.agent
    {
        detail.push(format!(
            "Observed by {} @ {}",
            panel::safe_text(&agent.name),
            panel::safe_text(&attribution.account.login)
        ));
    }
    if entry.cancellable {
        detail.push(format!(
            "/wait cancel {} stops only this observation.",
            panel::safe_text(&entry.id)
        ));
    }
    Some(WaitCard {
        key: format!(
            "wait-observation\0{}\0{}",
            entry.id,
            entry.episode.unwrap_or(0)
        ),
        heading,
        detail: detail.join("\n"),
        entry: entry.clone(),
    })
}

/// The same bounded, stable-card replacement used by worker operations. It
/// stores presentation only and cannot acquire, complete or replay any work.
#[derive(Default)]
pub(super) struct WaitFeed {
    owner: Option<Option<crate::client::interface::history::HistoryOwner>>,
    observed: std::collections::BTreeMap<String, WaitCard>,
}

impl WaitFeed {
    fn update(
        &mut self,
        snapshot: Snapshot,
        owner: Option<crate::client::interface::history::HistoryOwner>,
    ) -> Vec<WaitCard> {
        use crate::wire::wait_display::Outcome;
        if self.owner.as_ref() != Some(&owner) {
            self.observed.clear();
            self.owner = Some(owner);
        }
        let cards: Vec<_> = snapshot
            .active
            .iter()
            .chain(&snapshot.recent)
            .filter_map(wait_card)
            .collect();
        let present: std::collections::BTreeSet<_> =
            cards.iter().map(|card| card.key.clone()).collect();
        let mut updates = Vec::new();
        for card in cards {
            if self.observed.get(&card.key).is_none_or(|previous| {
                previous.heading != card.heading || previous.detail != card.detail
            }) {
                updates.push(card.clone());
            }
            self.observed.insert(card.key.clone(), card);
        }
        for (key, previous) in &mut self.observed {
            if !present.contains(key)
                && previous
                    .entry
                    .wait
                    .as_ref()
                    .is_some_and(|wait| wait.outcome == Outcome::Pending)
            {
                let mut ended = previous.entry.clone();
                ended.wait.as_mut().expect("checked wait").outcome = Outcome::Unknown;
                ended.status = Status::Unknown;
                ended.remaining_ms = None;
                ended.cancellable = false;
                let card = wait_card(&ended).expect("retained wait");
                *previous = card.clone();
                updates.push(card);
            }
        }
        while self.observed.len() > 128 {
            self.observed.pop_first();
        }
        updates
    }
}

impl State {
    pub(super) fn chat_wait_observations(&mut self, snapshot: Snapshot) {
        let owner = crate::client::interface::history::current_owner();
        for card in self.conversation.wait_observations.update(snapshot, owner) {
            self.events
                .ingest_wait_observation(&card.key, &card.heading, &card.detail);
        }
    }
}

#[cfg(test)]
mod tests;
