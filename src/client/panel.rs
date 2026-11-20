//! Pure sidebar layout. The compositor alone owns terminal escape sequences.
use super::{
    git_status::{FetchState, GitSnapshot, GitState},
    ui::{Panel, Ui},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Rect {
    pub col: u16,
    pub row: u16,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PanelLayout {
    pub log_cols: u16,
    pub sidebar: Option<Rect>,
}

impl PanelLayout {
    #[cfg(test)]
    pub fn new(cols: u16, rows: u16, enabled: bool) -> Self {
        Self::configured(cols, rows, enabled, (cols / 4).clamp(28, 36))
    }

    pub fn configured(cols: u16, rows: u16, enabled: bool, width: u16) -> Self {
        if !enabled || cols < 96 || rows < 18 {
            return Self {
                log_cols: cols,
                sidebar: None,
            };
        }
        let width = width.clamp(20, 60).min(cols.saturating_sub(49));
        let left = cols - width - 1;
        Self {
            log_cols: left,
            sidebar: Some(Rect {
                col: left + 1,
                row: 0,
                width,
                height: rows,
            }),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum PanelTone {
    #[default]
    Normal,
    Muted,
    Heading,
    Accent,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PanelLine {
    pub text: String,
    pub tone: PanelTone,
}

#[derive(Clone, Debug)]
pub(super) struct TaskView {
    pub id: String,
    pub description: String,
    pub cwd: Option<String>,
    pub started: Instant,
    pub deadline: Option<Instant>,
}

#[derive(Clone, Debug)]
pub(super) struct ProgressView {
    pub source: String,
    pub message: Option<String>,
    pub value: Option<f64>,
    pub total: Option<f64>,
    pub updated: Instant,
}

#[derive(Clone, Debug)]
pub(super) struct ControllerView {
    pub request_id: String,
    pub identity: Option<crate::wire::Controller>,
    pub attribution: Option<crate::wire::ControlAttribution>,
    pub operation: String,
    pub started: Instant,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PanelSnapshot {
    pub controllers: Vec<ControllerView>,
    pub last_controller: Option<ControllerView>,
    pub tasks: Vec<TaskView>,
    pub progress: Vec<ProgressView>,
    pub git: Option<Arc<GitSnapshot>>,
}

/// Ordered sections share the available space; empty sections consume no rows.
pub(super) fn render_configured(
    snapshot: &PanelSnapshot,
    width: u16,
    height: u16,
    now: Instant,
    ui: &Ui,
) -> Vec<PanelLine> {
    let panels = ui
        .panels
        .iter()
        .copied()
        .filter(|panel| !matches!(panel, Panel::Agents | Panel::Queue))
        .filter(|panel| {
            !ui.hide_empty
                || match panel {
                    Panel::Agents | Panel::Queue => false,
                    Panel::Controller => {
                        !snapshot.controllers.is_empty() || snapshot.last_controller.is_some()
                    }
                    Panel::Tasks => !snapshot.tasks.is_empty(),
                    Panel::Progress => !snapshot.progress.is_empty(),
                    Panel::Git => snapshot
                        .git
                        .as_ref()
                        .is_some_and(|git| git.status != GitState::NotRepository),
                    Panel::History => snapshot.git.as_deref().is_some_and(show_history),
                }
        })
        .collect::<Vec<_>>();
    let mut rows = Vec::new();
    let available = usize::from(height);
    for (index, panel) in panels.iter().enumerate() {
        let remaining = available.saturating_sub(rows.len());
        let reserve = panels.len().saturating_sub(index + 1) * 3;
        let budget = remaining.saturating_sub(reserve).max(2).min(remaining);
        if budget == 0 {
            break;
        }
        let mut section = match panel {
            Panel::Agents | Panel::Queue => Vec::new(),
            Panel::Controller => controller_lines(snapshot),
            Panel::Tasks => task_lines(&snapshot.tasks, budget.min(10), now),
            Panel::Progress => {
                progress_lines(&snapshot.progress, budget.min(8), usize::from(width), now)
            }
            Panel::Git => git_lines(snapshot.git.as_deref(), budget.min(9), now),
            Panel::History => {
                history_lines(snapshot.git.as_deref(), budget, usize::from(width), now)
            }
        };
        section.truncate(budget);
        if !rows.is_empty() && !section.is_empty() && rows.len() < available {
            rows.push(PanelLine::default());
        }
        rows.extend(
            section
                .into_iter()
                .take(available.saturating_sub(rows.len())),
        );
    }
    if rows.is_empty() && available > 0 {
        rows.push(line("Ready", PanelTone::Accent));
        if available > 1 {
            rows.push(line("F1 Help & appearance", PanelTone::Muted));
        }
    }
    rows.resize_with(available, PanelLine::default);
    for row in &mut rows {
        row.text = fit(&row.text, usize::from(width));
    }
    rows
}

#[cfg(test)]
/// Always returns exactly `height` rows so an old task cannot remain on screen.
pub(super) fn render(
    snapshot: &PanelSnapshot,
    width: u16,
    height: u16,
    now: Instant,
) -> Vec<PanelLine> {
    let mut lines = controller_lines(snapshot);
    lines.truncate(usize::from(height));
    let remaining = usize::from(height).saturating_sub(lines.len() + 1);
    if remaining > 0 {
        lines.push(PanelLine::default());
        lines.extend(render_body(snapshot, width, remaining as u16, now));
    }
    lines.resize_with(usize::from(height), PanelLine::default);
    for row in &mut lines {
        row.text = fit(&row.text, usize::from(width));
    }
    lines
}

fn controller_lines(snapshot: &PanelSnapshot) -> Vec<PanelLine> {
    let active = !snapshot.controllers.is_empty();
    let controllers = if active {
        snapshot.controllers.iter().take(2).collect::<Vec<_>>()
    } else {
        snapshot.last_controller.iter().collect::<Vec<_>>()
    };
    let heading = if active {
        format!("Controller · {} active", snapshot.controllers.len())
    } else if controllers.is_empty() {
        "Controller".into()
    } else {
        "Last controller".into()
    };
    let mut rows = vec![line(heading, PanelTone::Heading)];
    if controllers.is_empty() {
        rows.push(line("Model unknown", PanelTone::Muted));
        rows.push(line("No controller metadata yet", PanelTone::Muted));
        return rows;
    }
    for controller in controllers {
        if let Some(attribution) = &controller.attribution {
            rows.push(line(
                format!("Account · {}", attribution.account.label()),
                PanelTone::Accent,
            ));
            rows.push(line(
                match &attribution.agent {
                    Some(agent) => format!(
                        "Agent · {} · {}",
                        agent.name,
                        &agent.id[..agent.id.len().min(14)]
                    ),
                    None => "Agent · not bound".into(),
                },
                PanelTone::Normal,
            ));
            rows.push(line(
                match &attribution.owner {
                    Some(owner) => format!("Owner · {}", owner.label()),
                    None => "Owner · unavailable".into(),
                },
                PanelTone::Muted,
            ));
        } else {
            rows.push(line("Account · unverified", PanelTone::Muted));
        }
        if !controller.operation.is_empty() {
            rows.push(line(
                format!("Op · {}", controller.operation),
                PanelTone::Normal,
            ));
        }
        let identity = controller.identity.as_ref();
        rows.push(line(
            identity
                .and_then(|value| value.model.as_deref())
                .unwrap_or("Model unknown"),
            PanelTone::Accent,
        ));
        rows.push(line(
            identity
                .and_then(|value| value.name.as_deref())
                .unwrap_or("Client unknown"),
            PanelTone::Normal,
        ));
        let source = match identity.map(|value| value.source) {
            Some(crate::wire::ControllerSource::CallerReported) => "caller-reported",
            Some(crate::wire::ControllerSource::McpClientInfo) => "MCP clientInfo",
            None => "metadata missing",
        };
        rows.push(line(
            format!("{source} · #{}", controller.request_id),
            PanelTone::Muted,
        ));
    }
    if snapshot.controllers.len() > 2 {
        rows.push(line(
            format!("+{} other active requests", snapshot.controllers.len() - 2),
            PanelTone::Muted,
        ));
    }
    rows
}

#[cfg(test)]
fn render_body(snapshot: &PanelSnapshot, width: u16, height: u16, now: Instant) -> Vec<PanelLine> {
    let width = usize::from(width);
    let height = usize::from(height);
    if height == 0 {
        return Vec::new();
    }
    let git_budget = (height / 3).clamp(2, 10).min(height);
    let git = git_lines(snapshot.git.as_deref(), git_budget, now);
    let history_minimum = snapshot
        .git
        .as_deref()
        .filter(|git| show_history(git))
        .map(|git| {
            if git.head.is_some() && git.history.head == git.head && !git.history.commits.is_empty()
            {
                1 + git.history.commits.len().min(3) * 2
            } else {
                2
            }
        })
        .unwrap_or(0);
    let top_available = height.saturating_sub(git.len() + usize::from(!git.is_empty()));
    let top_minimum = if snapshot.tasks.is_empty() { 2 } else { 4 }
        + if snapshot.progress.is_empty() { 0 } else { 3 };
    let history_reserved = history_minimum.min(top_available.saturating_sub(top_minimum + 1));
    let top_budget =
        top_available.saturating_sub(history_reserved + usize::from(history_reserved > 0));
    let progress_budget = if snapshot.progress.is_empty() || top_budget < 7 {
        0
    } else {
        top_budget.saturating_sub(4).min(8)
    };
    let task_budget = top_budget.saturating_sub(progress_budget);
    let mut lines = task_lines(&snapshot.tasks, task_budget, now);
    lines.extend(progress_lines(
        &snapshot.progress,
        progress_budget,
        width,
        now,
    ));
    lines.truncate(top_budget);
    append_section(&mut lines, git, width, height);
    let history_budget = height.saturating_sub(lines.len() + usize::from(!lines.is_empty()));
    let history = history_lines(snapshot.git.as_deref(), history_budget, width, now);
    append_section(&mut lines, history, width, height);
    lines.truncate(height);
    lines.resize_with(height, PanelLine::default);
    for line in &mut lines {
        line.text = fit(&line.text, width);
    }
    lines
}

#[cfg(test)]
fn append_section(rows: &mut Vec<PanelLine>, section: Vec<PanelLine>, width: usize, height: usize) {
    if section.is_empty() || rows.len() >= height {
        return;
    }
    if !rows.is_empty() {
        rows.push(line("─".repeat(width), PanelTone::Muted));
    }
    rows.extend(section.into_iter().take(height.saturating_sub(rows.len())));
}

fn line(text: impl Into<String>, tone: PanelTone) -> PanelLine {
    PanelLine {
        text: text.into(),
        tone,
    }
}

fn task_lines(tasks: &[TaskView], budget: usize, now: Instant) -> Vec<PanelLine> {
    if budget == 0 {
        return Vec::new();
    }
    let mut lines = vec![line(
        format!("Tasks · {} running", tasks.len()),
        PanelTone::Heading,
    )];
    if tasks.is_empty() {
        if budget > 1 {
            lines.push(line("No running tasks", PanelTone::Muted));
        }
        return lines;
    }
    let slots = budget.saturating_sub(1) / 2;
    let shown = if tasks.len() > slots {
        budget.saturating_sub(2) / 2
    } else {
        slots
    };
    for task in tasks.iter().take(shown) {
        lines.push(line(
            format!("{} {}", task.id, task.description),
            PanelTone::Normal,
        ));
        let age = now.saturating_duration_since(task.started);
        let mark = ["·", "◦", "○", "◦"][(age.as_secs() % 4) as usize];
        let cwd = task
            .cwd
            .as_deref()
            .map(|v| format!(" · {v}"))
            .unwrap_or_default();
        lines.push(line(
            format!("{mark} {}{cwd}", elapsed(age)),
            PanelTone::Muted,
        ));
    }
    if tasks.len() > shown && lines.len() < budget {
        lines.push(line(
            format!("+{} more running", tasks.len() - shown),
            PanelTone::Accent,
        ));
    }
    lines
}

fn progress_lines(
    progress: &[ProgressView],
    budget: usize,
    width: usize,
    now: Instant,
) -> Vec<PanelLine> {
    if progress.is_empty() || budget == 0 {
        return Vec::new();
    }
    let mut lines = vec![line("Native progress", PanelTone::Heading)];
    for update in progress.iter().take(2) {
        if lines.len() + 2 > budget {
            break;
        }
        let title = update
            .message
            .as_deref()
            .map(|m| format!("{} · {m}", update.source))
            .unwrap_or_else(|| update.source.clone());
        lines.push(line(title, PanelTone::Normal));
        let (detail, complete) = progress_text(update, width);
        lines.push(line(
            detail,
            if complete {
                PanelTone::Success
            } else {
                PanelTone::Accent
            },
        ));
        let age = now.saturating_duration_since(update.updated);
        if age >= Duration::from_secs(5) && lines.len() < budget {
            lines.push(line(
                format!("updated {} ago", elapsed(age)),
                PanelTone::Muted,
            ));
        }
    }
    lines
}

fn progress_text(update: &ProgressView, width: usize) -> (String, bool) {
    let value = update.value.filter(|v| v.is_finite() && *v >= 0.0);
    let total = update.total.filter(|v| v.is_finite() && *v > 0.0);
    if let Some((value, total)) = value.zip(total).filter(|(v, t)| v <= t) {
        let ratio = value / total;
        let cells = width.saturating_sub(7).clamp(3, 10);
        let filled = (ratio * cells as f64).floor() as usize;
        let percent = (ratio * 100.0).floor() as u8;
        return (
            format!(
                "{}{} {percent}%",
                "━".repeat(filled),
                "─".repeat(cells - filled)
            ),
            value == total,
        );
    }
    (
        value
            .map(|v| format!("{v} · total unknown"))
            .unwrap_or_else(|| "total unknown".into()),
        false,
    )
}

fn git_lines(snapshot: Option<&GitSnapshot>, budget: usize, now: Instant) -> Vec<PanelLine> {
    if budget == 0 {
        return Vec::new();
    }
    let mut lines = vec![line("Git", PanelTone::Heading)];
    let Some(git) = snapshot else {
        lines.push(line("Status pending", PanelTone::Muted));
        lines.truncate(budget);
        return lines;
    };
    let state = match git.status {
        GitState::Checking => Some(("Checking repository", PanelTone::Muted)),
        GitState::NotRepository => Some(("Not a repository", PanelTone::Muted)),
        GitState::Unavailable => Some(("Git unavailable", PanelTone::Warning)),
        GitState::Error => Some(("Status unavailable", PanelTone::Warning)),
        GitState::Ready => None,
    };
    if let Some((state, tone)) = state {
        lines.push(line(state, tone));
    }
    let previous_repository =
        git.observed_at.is_some() && (git.branch.is_some() || git.head.is_some() || git.unborn);
    if git.stale && previous_repository {
        let age = git
            .observed_at
            .map(|at| format!(" · {} old", elapsed(now.saturating_duration_since(at))))
            .unwrap_or_default();
        lines.push(line(format!("Last known state{age}"), PanelTone::Warning));
    }
    if git.status == GitState::Ready || (git.stale && previous_repository) {
        let branch = git
            .branch
            .as_deref()
            .map(str::to_owned)
            .or_else(|| {
                git.head
                    .as_deref()
                    .map(|v| format!("detached {}", v.chars().take(8).collect::<String>()))
            })
            .unwrap_or_else(|| "Branch unavailable".into());
        lines.push(line(branch, PanelTone::Accent));
        if git.conflicts > 0 {
            lines.push(line(
                format!("{} conflicts", git.conflicts),
                PanelTone::Error,
            ));
        }
        if git.staged == 0 && git.unstaged == 0 && git.untracked == 0 && git.conflicts == 0 {
            lines.push(line(
                if git.stale {
                    "Previously clean"
                } else {
                    "Clean working tree"
                },
                if git.stale {
                    PanelTone::Muted
                } else {
                    PanelTone::Success
                },
            ));
        } else {
            lines.push(line(
                format!(
                    "stage {} · work {} · new {}",
                    git.staged, git.unstaged, git.untracked
                ),
                PanelTone::Normal,
            ));
        }
        if let Some((ahead, behind)) = git.ahead.zip(git.behind) {
            lines.push(line(
                format!("↑{ahead} ahead · ↓{behind} behind"),
                PanelTone::Muted,
            ));
        }
        let remote = git.fetch.remote.as_deref().unwrap_or("upstream");
        let (fetch, tone) = match git.fetch.state {
            FetchState::Disabled => ("Fetch off".into(), PanelTone::Muted),
            FetchState::Waiting => (
                format!(
                    "Fetch every {}",
                    elapsed(Duration::from_secs(git.fetch.interval_seconds))
                ),
                PanelTone::Muted,
            ),
            FetchState::Running => (format!("Fetching {remote}"), PanelTone::Accent),
            FetchState::Fresh => {
                let age = git
                    .fetch
                    .last_success_at
                    .map(|at| format!(" · {} ago", elapsed(now.saturating_duration_since(at))))
                    .unwrap_or_default();
                (format!("Fetched {remote}{age}"), PanelTone::Muted)
            }
            FetchState::Failed => ("Fetch failed · retry scheduled".into(), PanelTone::Warning),
            FetchState::NoUpstream => ("Fetch needs an upstream".into(), PanelTone::Muted),
        };
        if git.fetch.state != FetchState::Disabled {
            lines[0] = line(
                format!("Git · {fetch}"),
                if tone == PanelTone::Warning {
                    tone
                } else {
                    PanelTone::Heading
                },
            );
        }
        for change in &git.changed_files {
            if lines.len() >= budget {
                break;
            }
            lines.push(line(
                format!("{} {}", change.status, change.path),
                PanelTone::Muted,
            ));
        }
    }
    lines.truncate(budget);
    lines
}

fn show_history(git: &GitSnapshot) -> bool {
    git.status == GitState::Ready || (git.stale && git.head.is_some())
}

fn history_lines(
    snapshot: Option<&GitSnapshot>,
    budget: usize,
    width: usize,
    now: Instant,
) -> Vec<PanelLine> {
    let Some(git) = snapshot.filter(|git| show_history(git)) else {
        return Vec::new();
    };
    if budget < 2 {
        return Vec::new();
    }
    let history = &git.history;
    let stale = git.stale
        || git.status != GitState::Ready
        || (history.stale && history.checked_at.is_some());
    let mut rows = vec![line(
        if stale {
            "Recent commits · stale"
        } else {
            "Recent commits"
        },
        if stale {
            PanelTone::Warning
        } else {
            PanelTone::Heading
        },
    )];
    if git.status == GitState::Ready && !git.stale && git.unborn && git.head.is_none() {
        rows.push(line("No commits yet", PanelTone::Muted));
        return rows;
    }
    if git.head.is_none() || history.head != git.head {
        rows.push(line(
            if history.error.is_some() {
                "History unavailable"
            } else {
                "History pending for HEAD"
            },
            if history.error.is_some() {
                PanelTone::Warning
            } else {
                PanelTone::Muted
            },
        ));
        return rows;
    }
    if history.commits.is_empty() {
        rows.push(line(
            if history.error.is_some() {
                "History unavailable"
            } else if history.stale {
                "Loading recent commits"
            } else {
                "No history returned"
            },
            if history.error.is_some() {
                PanelTone::Warning
            } else {
                PanelTone::Muted
            },
        ));
        return rows;
    }
    let shown = history.commits.len().min(5).min((budget - 1) / 2);
    if shown == 0 {
        rows.push(line("Expand to show commits", PanelTone::Muted));
        return rows;
    }
    if !stale && shown < history.commits.len().min(5) {
        rows[0].text = format!("Recent commits · {shown}/{}", history.commits.len().min(5));
    }
    let observed_now = history
        .observed_at_unix
        .zip(history.checked_at)
        .map(|(unix, checked)| {
            unix.saturating_add(
                i64::try_from(now.saturating_duration_since(checked).as_secs()).unwrap_or(i64::MAX),
            )
        });
    for (index, commit) in history.commits.iter().take(shown).enumerate() {
        let remaining_commits = shown - index - 1;
        let subject_rows = budget
            .saturating_sub(rows.len() + 1 + remaining_commits * 2)
            .clamp(1, 2);
        rows.push(line(
            format!(
                "{} · {}",
                commit.short_hash,
                commit_age(commit.committed_at_unix, observed_now)
            ),
            PanelTone::Muted,
        ));
        for subject in wrap_subject(&commit.subject, width.saturating_sub(2), subject_rows) {
            rows.push(line(format!("  {subject}"), PanelTone::Normal));
        }
    }
    rows.truncate(budget);
    rows
}

fn commit_age(committed_at_unix: i64, now_unix: Option<i64>) -> String {
    let Some(now) = now_unix else {
        return "time unknown".into();
    };
    let delta = now.saturating_sub(committed_at_unix);
    let seconds = delta.unsigned_abs();
    let age = if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    };
    if delta < 0 {
        format!("in {age}")
    } else {
        format!("{age} ago")
    }
}

/// Sanitize the whole subject before wrapping, so a split cannot conceal a secret.
fn wrap_subject(subject: &str, width: usize, max_lines: usize) -> Vec<String> {
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let safe = safe_text(subject);
    let mut remaining = safe.trim();
    if remaining.is_empty() {
        return vec!["(no subject)".into()];
    }
    let mut rows = Vec::new();
    while !remaining.is_empty() && rows.len() < max_lines {
        if UnicodeWidthStr::width(remaining) <= width || rows.len() + 1 == max_lines {
            rows.push(fit(remaining, width));
            break;
        }
        let mut used = 0;
        let mut end = 0;
        let mut word_end = None;
        for (index, character) in remaining.char_indices() {
            let cells = UnicodeWidthChar::width(character).unwrap_or(0);
            if used + cells > width {
                break;
            }
            used += cells;
            end = index + character.len_utf8();
            if character == ' ' && index > 0 {
                word_end = Some(index);
            }
        }
        let end = word_end.unwrap_or(end);
        if end == 0 {
            rows.push(fit(remaining, width));
            break;
        }
        rows.push(remaining[..end].trim_end().into());
        remaining = remaining[end..].trim_start();
    }
    rows
}

fn elapsed(duration: Duration) -> String {
    if duration.as_secs() < 60 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!(
            "{}m {:02}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    }
}

/// Untrusted identifiers are text, never terminal controls or bidi directives.
pub(super) fn safe_text(text: &str) -> String {
    let redacted = crate::audit::event(serde_json::Value::String(text.to_owned()));
    let text = redacted.as_str().unwrap_or("[redacted]");
    let mut safe = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' => safe.push_str("\\n"),
            '\r' => safe.push_str("\\r"),
            '\t' => safe.push_str("\\t"),
            '\u{1b}' => safe.push_str("\\x1b"),
            '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => safe.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c if c.is_control() => safe.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => safe.push(c),
        }
    }
    safe
}

pub(super) fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let safe = safe_text(text);
    if UnicodeWidthStr::width(safe.as_str()) <= width {
        return safe;
    }
    let mut output = String::new();
    let mut used = 0;
    for c in safe.chars() {
        let cells = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cells > width.saturating_sub(1) {
            break;
        }
        output.push(c);
        used += cells;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_panel_distinguishes_active_last_reported_and_unknown() {
        let now = Instant::now();
        let controller = ControllerView {
            request_id: "42".into(),
            identity: Some(crate::wire::Controller {
                name: Some("ChatGPT".into()),
                model: Some("model-a".into()),
                source: crate::wire::ControllerSource::CallerReported,
            }),
            attribution: None,
            operation: String::new(),
            started: now,
        };
        let mut snapshot = PanelSnapshot {
            controllers: vec![controller.clone()],
            ..PanelSnapshot::default()
        };
        let text = render(&snapshot, 36, 24, now)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Controller · 1 active")
                && text.contains("model-a")
                && text.contains("caller-reported")
        );
        snapshot.controllers.clear();
        snapshot.last_controller = Some(controller);
        let text = render(&snapshot, 36, 24, now)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Last controller") && !text.contains("1 active"));
        snapshot.last_controller = None;
        let text = render(&snapshot, 36, 24, now)
            .into_iter()
            .map(|row| row.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Model unknown"));
        assert!(!text.contains("model-a"));
        for height in 0..=24 {
            let rows = render(&snapshot, 28, height, now);
            assert_eq!(rows.len(), usize::from(height));
            assert!(
                rows.iter()
                    .all(|row| UnicodeWidthStr::width(row.text.as_str()) <= 28)
            );
        }
    }

    #[test]
    fn sidebar_text_uses_the_same_secret_redaction_as_logs() {
        assert_eq!(
            safe_text("branch password=panel-secret-fixture"),
            "branch password=[redacted]"
        );
        assert_eq!(
            safe_text("Authorization: Bearer panel-secret-fixture"),
            "Authorization:[redacted]"
        );
    }

    #[test]
    fn physical_106_column_layout_and_resize_collapse() {
        let layout = PanelLayout::new(106, 58, true);
        assert_eq!(layout.log_cols, 77);
        assert_eq!(
            layout.sidebar,
            Some(Rect {
                col: 78,
                row: 0,
                width: 28,
                height: 58
            })
        );
        for (cols, rows, enabled) in [(95, 58, true), (106, 17, true), (106, 58, false)] {
            let collapsed = PanelLayout::new(cols, rows, enabled);
            assert_eq!(collapsed.log_cols, cols);
            assert!(collapsed.sidebar.is_none());
        }
        assert_eq!(PanelLayout::new(200, 58, true).sidebar.unwrap().width, 36);
    }

    #[test]
    fn panel_is_cell_bounded_plain_text_and_clears_stale_rows() {
        let now = Instant::now();
        let snapshot = PanelSnapshot {
            tasks: vec![TaskView {
                id: "#a".into(),
                description: "修复中文 e\u{301}\u{1b}[2J\nnext".into(),
                cwd: Some("/repo\rhidden".into()),
                started: now - Duration::from_secs(6),
                deadline: None,
            }],
            ..PanelSnapshot::default()
        };
        let rows = render(&snapshot, 28, 18, now);
        assert_eq!(rows.len(), 18);
        assert!(
            rows.iter()
                .all(|line| UnicodeWidthStr::width(line.text.as_str()) <= 28
                    && !line.text.chars().any(char::is_control))
        );
        assert!(rows.iter().any(|line| line.text.contains("6.0s")));
        assert!(!rows.iter().any(|line| line.text.contains('%')));
        let empty = render(&PanelSnapshot::default(), 28, 18, now);
        assert_eq!(empty.len(), 18);
        assert!(!empty.iter().any(|line| line.text.contains("#a")));
        assert_eq!(fit("ab界cd", 5), "ab界…");
        assert_eq!(fit("e\u{301}界", 3), "e\u{301}界");
        assert_eq!(
            safe_text("x\x1b[2J\r\n\u{202e}y"),
            "x\\x1b[2J\\r\\n\\u{202e}y"
        );
    }

    #[test]
    fn progress_requires_explicit_valid_total() {
        let mut p = ProgressView {
            source: "codex:7".into(),
            message: None,
            value: Some(3.0),
            total: None,
            updated: Instant::now(),
        };
        assert_eq!(progress_text(&p, 28).0, "3 · total unknown");
        for total in [0.0, -1.0, f64::NAN, 2.0] {
            p.total = Some(total);
            assert!(!progress_text(&p, 28).0.contains('%'));
        }
        p.total = Some(12.0);
        assert!(progress_text(&p, 28).0.ends_with("25%"));
        p.value = Some(12.0);
        assert!(progress_text(&p, 28).1);
    }

    #[test]
    fn missing_or_failed_git_observation_never_claims_clean() {
        let now = Instant::now();
        let pending = PanelSnapshot {
            git: Some(Arc::new(GitSnapshot::default())),
            ..PanelSnapshot::default()
        };
        let text = render(&pending, 28, 18, now)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Checking repository"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("clean"), "{text}");
        let stale = PanelSnapshot {
            git: Some(Arc::new(GitSnapshot {
                status: GitState::Error,
                branch: Some("main".into()),
                observed_at: Some(now - Duration::from_secs(30)),
                stale: true,
                ..GitSnapshot::default()
            })),
            ..PanelSnapshot::default()
        };
        let text = render(&stale, 28, 30, now)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("Status unavailable")
                && text.contains("Last known state")
                && text.contains("Previously clean"),
            "{text}"
        );
        assert!(!text.contains("Clean working tree"), "{text}");
    }

    #[test]
    fn git_conflicts_counts_and_paths_are_observations() {
        let now = Instant::now();
        let git = GitSnapshot {
            status: GitState::Ready,
            branch: Some("feature\x1b[2J".into()),
            staged: 1,
            unstaged: 2,
            untracked: 3,
            conflicts: 1,
            stale: false,
            changed_files: vec![super::super::git_status::GitChange {
                status: "MM".into(),
                path: "src/控制\nfile.rs".into(),
            }],
            observed_at: Some(now),
            ..GitSnapshot::default()
        };
        let rows = render(
            &PanelSnapshot {
                git: Some(Arc::new(git)),
                ..PanelSnapshot::default()
            },
            36,
            36,
            now,
        );
        let text = rows
            .iter()
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("1 conflicts")
                && text.contains("stage 1 · work 2 · new 3")
                && text.contains("src/控制\\nfile.rs"),
            "{text}"
        );
        assert!(
            !text.contains("ahead") && !text.contains("behind"),
            "{text}"
        );
        assert!(
            rows.iter()
                .all(|line| !line.text.chars().any(char::is_control))
        );
    }
    fn git_with_history(now: Instant) -> GitSnapshot {
        use super::super::git_status::{GitCommit, GitHistory};

        let commits = (1..=5)
            .map(|index| GitCommit {
                full_hash: format!("{index:08x}{}", "0".repeat(32)),
                short_hash: format!("{index:08x}"),
                subject: format!("Commit {index} keeps native workspace history readable"),
                committed_at_unix: 1_800_000_000 - index * 60,
            })
            .collect::<Vec<_>>();
        let head = Some(commits[0].full_hash.clone());
        GitSnapshot {
            status: GitState::Ready,
            branch: Some("main".into()),
            head: head.clone(),
            observed_at: Some(now),
            stale: false,
            history: GitHistory {
                head,
                commits,
                checked_at: Some(now),
                observed_at_unix: Some(1_800_000_000),
                error: None,
                stale: false,
            },
            ..GitSnapshot::default()
        }
    }

    #[test]
    fn git_history_follows_activity_and_adapts_to_available_rows() {
        let now = Instant::now();
        let mut git = git_with_history(now);
        git.fetch.state = FetchState::Waiting;
        git.fetch.interval_seconds = 300;
        let snapshot = PanelSnapshot {
            tasks: vec![
                TaskView {
                    id: "#a".into(),
                    description: "cargo test".into(),
                    cwd: Some("project".into()),
                    started: now - Duration::from_secs(6),
                    deadline: None,
                },
                TaskView {
                    id: "#b".into(),
                    description: "cargo check".into(),
                    cwd: None,
                    started: now - Duration::from_secs(5),
                    deadline: None,
                },
            ],
            git: Some(Arc::new(git.clone())),
            ..PanelSnapshot::default()
        };
        let full = render(&snapshot, 28, 57, now);
        assert_eq!(full.len(), 57);
        let git_row = full
            .iter()
            .position(|row| row.text.starts_with("Git · Fetch"))
            .unwrap();
        assert_eq!(
            git_row, 10,
            "Git follows the controller block and two task rows without bottom anchoring"
        );
        let history_row = full
            .iter()
            .position(|row| row.text.starts_with("Recent commits"))
            .unwrap();
        assert_eq!(history_row, git_row + 4);
        for commit in &git.history.commits {
            assert!(full.iter().any(|row| row.text.contains(&commit.short_hash)));
        }
        assert!(full.iter().any(|row| row.text.contains("1m ago")));
        assert!(
            full.iter()
                .all(|row| UnicodeWidthStr::width(row.text.as_str()) <= 28)
        );
        let compact = render(&snapshot, 28, 17, now);
        let shown = git
            .history
            .commits
            .iter()
            .filter(|commit| {
                compact
                    .iter()
                    .any(|row| row.text.contains(&commit.short_hash))
            })
            .count();
        assert_eq!(shown, 1);
        assert!(compact.iter().any(|row| row.text == "Recent commits · 1/5"));
        assert_eq!(compact.len(), 17);
        assert!(
            compact
                .iter()
                .all(|row| UnicodeWidthStr::width(row.text.as_str()) <= 28)
        );
        assert!(compact.iter().any(|row| row.text == "Model unknown"));
    }

    #[test]
    fn history_never_labels_a_different_head_or_missing_observation_as_current() {
        let now = Instant::now();
        let mut git = git_with_history(now);
        let old_subject = git.history.commits[0].subject.clone();
        git.head = Some("f".repeat(40));
        let pending = history_lines(Some(&git), 20, 28, now);
        assert!(
            pending
                .iter()
                .any(|row| row.text == "History pending for HEAD")
        );
        assert!(
            !pending
                .iter()
                .any(|row| row.text.contains(&git.history.commits[0].short_hash))
        );
        assert!(!pending.iter().any(|row| row.text.contains(&old_subject)));

        git.head = git.history.head.clone();
        git.status = GitState::Error;
        git.stale = true;
        git.history.stale = true;
        let stale = history_lines(Some(&git), 20, 28, now);
        assert_eq!(stale[0].text, "Recent commits · stale");
        assert_eq!(stale[0].tone, PanelTone::Warning);
        assert!(
            stale
                .iter()
                .any(|row| row.text.contains(&git.history.commits[0].short_hash))
        );

        git.status = GitState::Ready;
        git.stale = false;
        git.unborn = true;
        git.head = None;
        git.history = super::super::git_status::GitHistory::default();
        let unborn = history_lines(Some(&git), 20, 28, now);
        assert!(unborn.iter().any(|row| row.text == "No commits yet"));
        assert!(!unborn.iter().any(|row| row.text.contains("stale")));
    }

    #[test]
    fn commit_subject_wrap_and_time_keep_native_text_safe_and_truthful() {
        let rows = wrap_subject(
            "password=panel-secret-fixture 修复 e\u{301}\x1b[2J\nnext",
            26,
            2,
        );
        let text = rows.join("\n");
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("panel-secret-fixture"));
        assert!(rows.iter().all(|row| {
            UnicodeWidthStr::width(row.as_str()) <= 26 && !row.chars().any(char::is_control)
        }));
        assert_eq!(wrap_subject("e\u{301}界", 3, 2), ["e\u{301}界"]);
        assert_eq!(
            wrap_subject("Keep commit subjects readable", 18, 2),
            ["Keep commit", "subjects readable"]
        );
        assert_eq!(commit_age(100, None), "time unknown");
        assert_eq!(commit_age(100, Some(40)), "in 1m");
        assert_eq!(commit_age(40, Some(100)), "1m ago");
        assert_eq!(commit_age(-5, Some(10)), "15s ago");
    }
}
