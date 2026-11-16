//! Pure sidebar layout. The compositor alone owns terminal escape sequences.
use super::git_status::{GitSnapshot, GitState};
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
    pub fn new(cols: u16, rows: u16, enabled: bool) -> Self {
        if !enabled || cols < 96 || rows < 18 {
            return Self {
                log_cols: cols,
                sidebar: None,
            };
        }
        let width = (cols / 4).clamp(28, 36);
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
}

#[derive(Clone, Debug)]
pub(super) struct ProgressView {
    pub source: String,
    pub message: Option<String>,
    pub value: Option<f64>,
    pub total: Option<f64>,
    pub updated: Instant,
}

#[derive(Clone, Debug, Default)]
pub(super) struct PanelSnapshot {
    pub tasks: Vec<TaskView>,
    pub progress: Vec<ProgressView>,
    pub git: Option<Arc<GitSnapshot>>,
}

/// Always returns exactly `height` rows so an old task cannot remain on screen.
pub(super) fn render(
    snapshot: &PanelSnapshot,
    width: u16,
    height: u16,
    now: Instant,
) -> Vec<PanelLine> {
    let width = usize::from(width);
    let height = usize::from(height);
    if height == 0 {
        return Vec::new();
    }
    let git_budget = (height / 3).clamp(2, 10).min(height);
    let git = git_lines(snapshot.git.as_deref(), git_budget, now);
    let top_budget = height.saturating_sub(git.len() + usize::from(height > git.len()));
    let progress_budget = if snapshot.progress.is_empty() {
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
    lines.resize_with(height.saturating_sub(git.len()), PanelLine::default);
    lines.extend(git);
    lines.truncate(height);
    lines.resize_with(height, PanelLine::default);
    for line in &mut lines {
        line.text = fit(&line.text, width);
    }
    lines
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
        lines.push(line(branch, PanelTone::Normal));
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
}
