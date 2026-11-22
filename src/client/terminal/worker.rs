//! A worker connection is a read-only monitor, never an alternate composer.
mod metrics;

use super::{Frame, Key, PanelTone, State, fit, panel, request_interrupt};
use crate::worker::{SlotView, Snapshot};
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LogAnchor {
    sequence: u64,
    part: usize,
}

struct WorkerPanel {
    slot: usize,
    row: usize,
    rows: usize,
    log_row: usize,
    log_col: usize,
    log_rows: usize,
    log_cols: usize,
    details_row: usize,
    details_col: usize,
    details_rows: usize,
    details_cols: usize,
    keys: Vec<LogAnchor>,
    start: usize,
    following: bool,
}

struct WorkerTab {
    slot: usize,
    row: usize,
    col: usize,
    cols: usize,
}

#[derive(Default)]
struct PendingScroll {
    delta: i64,
    follow: bool,
}

#[derive(Default)]
struct ActivitySummary<'a> {
    task: Option<&'a str>,
    total_work_ms: u64,
    current_task_elapsed_ms: Option<u64>,
    recent_task_durations_ms: &'a [u64],
}

impl<'a> From<&'a SlotView> for ActivitySummary<'a> {
    fn from(slot: &'a SlotView) -> Self {
        Self {
            task: slot.task_summary.as_deref().or(slot.task_id.as_deref()),
            total_work_ms: slot.total_work_ms,
            current_task_elapsed_ms: slot.current_task_elapsed_ms,
            recent_task_durations_ms: &slot.recent_task_durations_ms,
        }
    }
}

#[derive(Default)]
pub(super) struct Monitor {
    generation: Option<String>,
    selected: usize,
    first: usize,
    order: Vec<usize>,
    // None follows new entries. A sequence anchor keeps a paused view stable
    // when another entry arrives or the bounded ring drops its oldest entry.
    anchors: BTreeMap<usize, Option<LogAnchor>>,
    pending_scroll: BTreeMap<usize, PendingScroll>,
    panels: Vec<WorkerPanel>,
    tabs: Vec<WorkerTab>,
    resources: metrics::Sampler,
}

impl Monitor {
    fn selected_slot(&self) -> Option<usize> {
        self.order.get(self.selected).copied()
    }

    fn select(&mut self, next: bool) {
        self.selected = if next {
            self.selected
                .saturating_add(1)
                .min(self.order.len().saturating_sub(1))
        } else {
            self.selected.saturating_sub(1)
        };
    }

    fn scroll(&mut self, up: bool, amount: usize) {
        let Some(slot) = self.selected_slot() else {
            return;
        };
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.slot == slot) else {
            // Selection and scrolling may arrive in one input packet, before
            // the newly selected station has a rendered log viewport.
            let pending = self.pending_scroll.entry(slot).or_default();
            let amount = i64::try_from(amount).unwrap_or(i64::MAX);
            pending.delta = pending
                .delta
                .saturating_add(if up { -amount } else { amount });
            pending.follow = !up;
            return;
        };
        let maximum = panel.keys.len().saturating_sub(panel.log_rows);
        panel.start = if up {
            panel.start.saturating_sub(amount)
        } else {
            panel.start.saturating_add(amount).min(maximum)
        };
        panel.following = !up && panel.start == maximum;
        self.anchors.insert(
            slot,
            if panel.following {
                None
            } else {
                panel.keys.get(panel.start).copied()
            },
        );
    }

    fn log_edge(&mut self, latest: bool) {
        let Some(slot) = self.selected_slot() else {
            return;
        };
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.slot == slot) else {
            self.pending_scroll.remove(&slot);
            self.anchors.insert(
                slot,
                if latest {
                    None
                } else {
                    Some(LogAnchor {
                        sequence: 0,
                        part: 0,
                    })
                },
            );
            return;
        };
        panel.start = if latest {
            panel.keys.len().saturating_sub(panel.log_rows)
        } else {
            0
        };
        panel.following = latest;
        self.anchors.insert(
            slot,
            if latest {
                None
            } else {
                panel.keys.first().copied()
            },
        );
    }

    fn page(&self) -> usize {
        self.panels
            .iter()
            .find(|panel| Some(panel.slot) == self.selected_slot())
            .or_else(|| self.panels.first())
            .map_or(1, |panel| panel.log_rows.saturating_sub(1).max(1))
    }

    fn render(
        &mut self,
        snapshot: &Snapshot,
        top: usize,
        available: usize,
        cols: u16,
    ) -> Vec<Vec<(PanelTone, String)>> {
        let previous_slot = if self.generation.as_deref() == Some(snapshot.generation.as_str()) {
            self.selected_slot()
        } else {
            None
        };
        if self.generation.as_deref() != Some(snapshot.generation.as_str()) {
            self.generation = Some(snapshot.generation.clone());
            self.selected = 0;
            self.first = 0;
            self.anchors.clear();
            self.pending_scroll.clear();
        }
        self.order = snapshot.slots.iter().map(|slot| slot.slot).collect();
        self.selected = previous_slot
            .and_then(|slot| self.order.iter().position(|value| *value == slot))
            .unwrap_or_else(|| self.selected.min(self.order.len().saturating_sub(1)));
        self.anchors.retain(|slot, _| self.order.contains(slot));
        self.pending_scroll
            .retain(|slot, _| self.order.contains(slot));
        self.panels.clear();
        self.tabs.clear();
        if available == 0 || snapshot.slots.is_empty() {
            return Vec::new();
        }
        self.resources.sample(snapshot.process_id, Instant::now());
        let width = usize::from(cols);
        let count = (width.saturating_sub(4) / 19)
            .max(1)
            .min(snapshot.slots.len());
        self.first = self.first.min(self.selected);
        if self.selected >= self.first + count {
            self.first = self.selected + 1 - count;
        }
        self.first = self.first.min(snapshot.slots.len().saturating_sub(count));
        let mut navigation = Vec::new();
        let mut col = 0;
        for slot in snapshot.slots.iter().skip(self.first).take(count) {
            let (tone, symbol, label) = slot_state(slot.state);
            let text = if width < 24 {
                format!(
                    "{}#{:02} {symbol} ",
                    if self.selected_slot() == Some(slot.slot) {
                        "›"
                    } else {
                        " "
                    },
                    slot.slot + 1
                )
            } else {
                format!(
                    "{}#{:02} {symbol} {label}  ",
                    if self.selected_slot() == Some(slot.slot) {
                        "› "
                    } else {
                        "  "
                    },
                    slot.slot + 1
                )
            };
            let tab_width = text.width().min(width.saturating_sub(col));
            self.tabs.push(WorkerTab {
                slot: slot.slot,
                row: top,
                col,
                cols: tab_width,
            });
            col += tab_width;
            navigation.push((tone, text));
        }
        let mut output = vec![navigation];
        if available == 1 {
            return output;
        }
        if available >= 12 {
            output.push(Vec::new());
        }
        let slot = &snapshot.slots[self.selected];
        let activity = ActivitySummary::from(slot);
        let body_top = top + output.len();
        let body_rows = available - output.len();
        let split = width >= 80;
        let right_cols = if split {
            (width / 3).clamp(32, 48)
        } else {
            width
        };
        let gap = if split { 3 } else { 0 };
        let left_cols = if split {
            width - right_cols - gap
        } else {
            width
        };
        let compact_budget = if split {
            0
        } else if body_rows >= 9 {
            4
        } else if body_rows >= 6 {
            2
        } else {
            0
        };
        let compact = compact_details(&activity, &self.resources, width, compact_budget);
        output.extend(compact.iter().cloned().map(|line| vec![line]));
        let log_area_row = body_top + compact.len();
        let log_area_rows = body_rows - compact.len();
        let header_rows = usize::from(log_area_rows >= 2);
        let log_row = log_area_row + header_rows;
        let log_rows = log_area_rows.saturating_sub(header_rows + 1);
        let entries = log_lines(slot, left_cols.saturating_sub(2).max(1));
        let keys: Vec<_> = entries.iter().map(|entry| entry.0).collect();
        let maximum = keys.len().saturating_sub(log_rows);
        let anchor = self.anchors.get(&slot.slot).copied().flatten();
        let mut start = anchor.map_or(maximum, |anchor| {
            keys.iter()
                .position(|key| *key >= anchor)
                .unwrap_or(maximum)
                .min(maximum)
        });
        let mut following = anchor.is_none();
        if let Some(pending) = self.pending_scroll.remove(&slot.slot) {
            if pending.delta < 0 {
                start = start.saturating_sub(
                    usize::try_from(pending.delta.unsigned_abs()).unwrap_or(usize::MAX),
                );
            } else {
                start = start
                    .saturating_add(usize::try_from(pending.delta).unwrap_or(usize::MAX))
                    .min(maximum);
            }
            following = pending.follow && start == maximum;
            self.anchors.insert(
                slot.slot,
                if following {
                    None
                } else {
                    keys.get(start).copied()
                },
            );
        }
        if !following {
            self.anchors.insert(slot.slot, keys.get(start).copied());
        }
        let details = if split {
            station_details(
                &activity,
                &self.resources,
                snapshot.process_id,
                right_cols,
                log_area_rows.saturating_sub(header_rows),
            )
        } else {
            Vec::new()
        };
        if header_rows > 0 {
            let heading = (PanelTone::Heading, "  Work log".into());
            output.push(if split {
                columns(
                    heading,
                    Some((PanelTone::Heading, "Task and metrics".into())),
                    left_cols,
                    gap,
                )
            } else {
                vec![heading]
            });
        }
        for offset in 0..log_area_rows.saturating_sub(header_rows) {
            let left = if offset < log_rows {
                if let Some((_, tone, text)) = entries.get(start + offset) {
                    (*tone, format!("  {text}"))
                } else if offset == 0 && entries.is_empty() {
                    (
                        PanelTone::Muted,
                        format!(
                            "  {}",
                            panel::safe_text(
                                slot.reason.as_deref().unwrap_or("No work reported yet")
                            )
                        ),
                    )
                } else {
                    (PanelTone::Normal, String::new())
                }
            } else if log_rows == 0 {
                (
                    PanelTone::Muted,
                    "Enlarge terminal to view work and metrics".into(),
                )
            } else {
                let range = if keys.is_empty() {
                    "0".into()
                } else {
                    format!(
                        "{}–{}/{}",
                        start + 1,
                        (start + log_rows).min(keys.len()),
                        keys.len()
                    )
                };
                (
                    PanelTone::Muted,
                    format!(
                        "  Log {range} · {}",
                        if following {
                            "Live"
                        } else {
                            "Paused · End to follow"
                        }
                    ),
                )
            };
            output.push(if split {
                columns(left, details.get(offset).cloned(), left_cols, gap)
            } else {
                vec![left]
            });
        }
        self.panels.push(WorkerPanel {
            slot: slot.slot,
            row: body_top,
            rows: body_rows,
            log_row,
            log_col: 0,
            log_rows,
            log_cols: left_cols,
            details_row: if split { log_row } else { body_top },
            details_col: if split { left_cols + gap } else { 0 },
            details_rows: if split { details.len() } else { compact.len() },
            details_cols: right_cols,
            keys,
            start,
            following,
        });
        output
    }
}

impl State {
    pub(super) fn worker_keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) {
        // The raw decoder keeps split paste markers and pasted Ctrl-C literal.
        // Discard every text byte; only monitor navigation and a real Ctrl-C act.
        let (_, actions) = self.keyboard.feed(bytes, now, flush, false, true);
        for action in actions {
            if matches!(action, Key::Interrupt) {
                request_interrupt(self);
                return;
            }
            let Some(monitor) = self.worker.as_mut() else {
                return;
            };
            match action {
                Key::Back | Key::FocusBack => monitor.select(false),
                Key::Open => monitor.select(true),
                Key::HistoryStart => monitor.selected = 0,
                Key::HistoryEnd => monitor.selected = monitor.order.len().saturating_sub(1),
                Key::AuditUp => monitor.scroll(true, 1),
                Key::AuditDown => monitor.scroll(false, 1),
                Key::Up => monitor.scroll(true, monitor.page()),
                Key::Down => monitor.scroll(false, monitor.page()),
                Key::Home => monitor.log_edge(false),
                Key::End => monitor.log_edge(true),
                Key::Mouse {
                    button,
                    x,
                    y,
                    release,
                } if !release && x < self.geometry.cols && y < self.geometry.rows => {
                    if button & 35 == 0 && button & 64 == 0 {
                        if let Some(slot) = monitor
                            .tabs
                            .iter()
                            .find(|tab| {
                                usize::from(y) == tab.row
                                    && usize::from(x) >= tab.col
                                    && usize::from(x) < tab.col + tab.cols
                            })
                            .map(|tab| tab.slot)
                        {
                            if let Some(index) =
                                monitor.order.iter().position(|value| *value == slot)
                            {
                                monitor.selected = index;
                                self.dirty = true;
                            }
                            continue;
                        }
                    }
                    let hovered = monitor
                        .panels
                        .iter()
                        .find(|panel| {
                            usize::from(y) >= panel.log_row
                                && usize::from(y) < panel.log_row + panel.log_rows
                                && usize::from(x) >= panel.log_col
                                && usize::from(x) < panel.log_col + panel.log_cols
                        })
                        .map(|panel| panel.slot);
                    if let Some(index) = hovered
                        .and_then(|slot| monitor.order.iter().position(|value| *value == slot))
                    {
                        if button & 64 != 0 {
                            monitor.selected = index;
                            monitor.scroll(button & 1 == 0, 3);
                        } else if button & 35 == 0 {
                            monitor.selected = index;
                        } else {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                _ => continue,
            }
            self.dirty = true;
        }
    }

    pub(super) fn worker_snapshot(&self, active: bool) -> serde_json::Value {
        let monitor = self.worker.as_ref();
        let panels: Vec<_> = monitor.into_iter().flat_map(|monitor| &monitor.panels).map(|panel| {
            serde_json::json!({
                "slot":panel.slot,"row":panel.row,"col":0,"rows":panel.rows,
                "cols":self.geometry.cols,"selected":monitor.and_then(Monitor::selected_slot)==Some(panel.slot),
                "logs":{"row":panel.log_row,"col":panel.log_col,"rows":panel.log_rows,"cols":panel.log_cols,"visible":panel.log_rows>0,
                    "retained_lines":panel.keys.len(),"first_line":panel.start,"following":panel.following},
                "details":{"row":panel.details_row,"col":panel.details_col,"rows":panel.details_rows,"cols":panel.details_cols,"visible":panel.details_rows>0,"scrollable":false}
            })
        }).collect();
        serde_json::json!({
            "active":active, "mode":"worker", "focus":"workers",
            "rows":self.geometry.rows, "cols":self.geometry.cols,
            "sleeping":false, "selection_mode":false,
            "panes":{
                "workers":{"row":0,"col":0,"rows":self.geometry.rows,
                    "cols":self.geometry.cols,"visible":true},
                "input":{"visible":false}, "logs":{"visible":false},
                "audit":{"visible":false}, "tasks":{"visible":false}
            },
            "worker_panels":panels,
            "worker_tabs":monitor.into_iter().flat_map(|monitor| &monitor.tabs).map(|tab| {
                serde_json::json!({"slot":tab.slot,"row":tab.row,"col":tab.col,"cols":tab.cols})
            }).collect::<Vec<_>>(),
            "selected_worker":monitor.and_then(Monitor::selected_slot),
            "workers":crate::worker::display(),
        })
    }

    pub(super) fn worker_frame(&mut self, snapshot: Option<&Snapshot>) -> Frame {
        let g = self.geometry;
        let height = usize::from(g.rows);
        let mut rows = vec![
            line(
                self,
                &[(
                    PanelTone::Heading,
                    format!("SSMCP · Workers{}", self.context_label),
                )],
            ),
            String::new(),
        ];
        if let Some(snapshot) = snapshot {
            let counts = [
                (PanelTone::Success, format!("○ Idle {}", snapshot.idle)),
                (
                    PanelTone::Warning,
                    format!("● Working {}", snapshot.working),
                ),
                (PanelTone::Error, format!("× Offline {}", snapshot.offline)),
                (
                    PanelTone::Warning,
                    format!("◌ Retiring {}", snapshot.retiring),
                ),
                (PanelTone::Accent, format!("Σ Total {}", snapshot.total)),
                (
                    PanelTone::Muted,
                    format!(
                        "Registered {}/{}",
                        snapshot.registered_workers, snapshot.desired_total
                    ),
                ),
            ];
            let per_row = if g.cols >= 120 {
                6
            } else if g.cols >= 74 {
                3
            } else if g.cols >= 36 {
                2
            } else {
                1
            };
            for group in counts.chunks(per_row) {
                let parts: Vec<_> = group
                    .iter()
                    .enumerate()
                    .map(|(index, (tone, text))| {
                        (
                            *tone,
                            format!("{}{text}", if index == 0 { "" } else { "    " }),
                        )
                    })
                    .collect();
                rows.push(line(self, &parts));
            }
            let stopping = snapshot.stopping || snapshot.closed;
            let status = if stopping && snapshot.working > 0 {
                "Stopping · waiting for worker confirmation"
            } else if stopping && snapshot.retiring > 0 {
                "Stopping · waiting for retirement confirmation"
            } else if stopping {
                "Stopped"
            } else {
                "FIFO assignment"
            };
            rows.push(line(
                self,
                &[(
                    PanelTone::Muted,
                    format!("Queued {} · {status}", snapshot.queued),
                )],
            ));
            let identity = identity_lines(snapshot, g.cols);
            if identity.len() + 6 <= height.saturating_sub(rows.len() + 1) {
                rows.extend(
                    identity
                        .iter()
                        .map(|text| line(self, &[(PanelTone::Muted, text.clone())])),
                );
            } else {
                // Never show a truncated identifier as if it were complete.
                rows.push(line(
                    self,
                    &[(PanelTone::Muted, format!("OS PID {}", snapshot.process_id))],
                ));
                rows.push(line(
                    self,
                    &[(
                        PanelTone::Muted,
                        if snapshot.manager.is_some() {
                            "Manager attached · enlarge for IDs"
                        } else {
                            "Manager unbound"
                        }
                        .into(),
                    )],
                ));
            }
            rows.push(String::new());
            let available = height.saturating_sub(rows.len() + 1);
            let panel_rows = self
                .worker
                .as_mut()
                .expect("worker frame requires monitor mode")
                .render(snapshot, rows.len(), available, g.cols);
            rows.extend(panel_rows.iter().map(|parts| line(self, parts)));
        } else {
            if let Some(monitor) = self.worker.as_mut() {
                monitor.panels.clear();
                monitor.tabs.clear();
            }
            // Configuration is not evidence that a worker is ready.
            rows.push(line(
                self,
                &[(PanelTone::Muted, "Waiting for worker readiness".into())],
            ));
        }
        rows.truncate(height.saturating_sub(1));
        rows.resize_with(height.saturating_sub(1), String::new);
        let footer = if height == 1 {
            "Workers · Ctrl+C Stop"
        } else if g.cols < 28 {
            "←→ ↑↓ End ^C Stop"
        } else if g.cols < 36 {
            "←→ Worker ↑↓ Logs ^C Stop"
        } else if g.cols < 68 {
            "←→ Worker · ↑↓ Logs · End Live · ^C Stop"
        } else {
            "←→ Worker · ↑↓ / PgUp / PgDn Logs · End Live · Ctrl+C Stop"
        };
        rows.push(line(self, &[(PanelTone::Muted, footer.into())]));
        Frame {
            geometry: g,
            rows,
            cursor: (0, 0),
            hidden: true,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: true,
            mouse_capture: true,
        }
    }
}

fn identity_lines(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let mut text = vec![format!("OS PID {}", snapshot.process_id)];
    if let Some(manager) = &snapshot.manager {
        text.extend([
            "Manager session ID".into(),
            manager.client_id.clone(),
            "Manager thread ID".into(),
            manager.thread_id.clone(),
        ]);
    } else {
        text.push("Manager unbound".into());
    }
    let width = usize::from(width.max(1));
    let mut output = Vec::new();
    for text in text {
        let mut row = String::new();
        let mut used = 0;
        for character in panel::safe_text(&text).chars() {
            let cells = character.width().unwrap_or(0);
            if used + cells > width && !row.is_empty() {
                output.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(character);
            used += cells;
        }
        output.push(row);
    }
    output
}

fn log_lines(slot: &SlotView, width: usize) -> Vec<(LogAnchor, PanelTone, String)> {
    let mut output = Vec::new();
    for entry in &slot.logs {
        let seconds = entry.at_ms / 1000;
        let text = panel::safe_text(&format!(
            "{:02}:{:02}:{:02} {} {}",
            seconds / 3600 % 24,
            seconds / 60 % 60,
            seconds % 60,
            entry.level,
            entry.text
        ));
        let tone = match entry.level.as_str() {
            "error" => PanelTone::Error,
            "warning" | "warn" => PanelTone::Warning,
            _ => PanelTone::Normal,
        };
        let mut row = String::new();
        let mut used = 0;
        let mut part = 0;
        for character in text.chars() {
            let cells = character.width().unwrap_or(0);
            if used + cells > width && !row.is_empty() {
                output.push((
                    LogAnchor {
                        sequence: entry.sequence,
                        part,
                    },
                    tone,
                    std::mem::take(&mut row),
                ));
                part += 1;
                used = 0;
            }
            if cells > width {
                row.push('…');
                used += 1;
            } else {
                row.push(character);
                used += cells;
            }
        }
        if !row.is_empty() {
            output.push((
                LogAnchor {
                    sequence: entry.sequence,
                    part,
                },
                tone,
                row,
            ));
        }
    }
    output
}

fn slot_state(state: &str) -> (PanelTone, &'static str, &'static str) {
    match state {
        "idle" => (PanelTone::Success, "○", "idle"),
        "working" => (PanelTone::Warning, "●", "working"),
        "offline" => (PanelTone::Error, "×", "offline"),
        "retiring" => (PanelTone::Warning, "◌", "retiring"),
        _ => (PanelTone::Muted, "?", "unknown"),
    }
}

fn columns(
    left: (PanelTone, String),
    right: Option<(PanelTone, String)>,
    left_cols: usize,
    gap: usize,
) -> Vec<(PanelTone, String)> {
    let left_text = wrap_activity(&left.1, left_cols, 1)
        .into_iter()
        .next()
        .unwrap_or_default();
    let padding = left_cols.saturating_sub(left_text.width()) + gap;
    let mut parts = vec![(left.0, left_text), (PanelTone::Muted, " ".repeat(padding))];
    if let Some(right) = right {
        parts.push(right);
    }
    parts
}

fn compact_details(
    activity: &ActivitySummary<'_>,
    resources: &metrics::Sampler,
    width: usize,
    budget: usize,
) -> Vec<(PanelTone, String)> {
    if budget == 0 {
        return Vec::new();
    }
    let mut rows = activity_field("Task", activity.task, "No task assigned", width, 1);
    if budget >= 2 {
        rows.push(detail(
            "Current",
            &activity
                .current_task_elapsed_ms
                .map(duration)
                .unwrap_or_else(|| "No active task".into()),
            width,
        ));
    }
    if budget >= 3 {
        rows.push(detail("Client CPU", &cpu_text(resources.latest()), width));
    }
    if budget >= 4 {
        rows.push((
            PanelTone::Muted,
            clipped("Enlarge for full metrics and chart", width),
        ));
    }
    rows
}

fn station_details(
    activity: &ActivitySummary<'_>,
    resources: &metrics::Sampler,
    process_id: u32,
    width: usize,
    budget: usize,
) -> Vec<(PanelTone, String)> {
    if budget == 0 {
        return Vec::new();
    }
    let mut rows = activity_field("Task", activity.task, "No task assigned", width, 2);
    rows.resize_with(2, || (PanelTone::Normal, String::new()));
    rows.push(detail(
        "Current",
        &activity
            .current_task_elapsed_ms
            .map(duration)
            .unwrap_or_else(|| "No active task".into()),
        width,
    ));
    rows.push(detail("Total", &duration(activity.total_work_ms), width));
    let recent = activity
        .recent_task_durations_ms
        .iter()
        .take(3)
        .map(|ms| duration(*ms))
        .collect::<Vec<_>>();
    rows.push(detail(
        "Last 3",
        &if recent.is_empty() {
            "No completed tasks".into()
        } else {
            recent.join(" / ")
        },
        width,
    ));
    rows.push((PanelTone::Normal, String::new()));
    rows.push((
        PanelTone::Muted,
        clipped("Agent CPU / RAM unavailable", width),
    ));
    rows.push((
        PanelTone::Muted,
        clipped(&format!("Shared client PID {process_id}"), width),
    ));
    let cpu = cpu_text(resources.latest());
    rows.push(detail("CPU", &format!("{cpu} of machine"), width));
    rows.push(detail(
        "RSS",
        &resources
            .latest()
            .and_then(|sample| sample.memory_bytes)
            .map(memory)
            .unwrap_or_else(|| "Unavailable".into()),
        width,
    ));
    rows.push(detail("Heat", &format!("{cpu} CPU"), width));
    let samples = resources.samples();
    let valid = samples
        .iter()
        .filter(|sample| sample.cpu_percent.is_some())
        .count();
    rows.push((
        PanelTone::Muted,
        clipped(&format!("CPU trend · {valid} samples"), width),
    ));
    let chart_rows = budget.saturating_sub(13).min(3);
    if chart_rows > 0 {
        if valid > 0 {
            let (plot, upper) = cpu_chart(samples, width, chart_rows);
            rows.extend(plot.into_iter().map(|line| (PanelTone::Accent, line)));
            rows.push((
                PanelTone::Muted,
                clipped(&format!("Scale 0–{upper:.1}% of machine"), width),
            ));
        } else {
            rows.push((
                PanelTone::Muted,
                clipped(
                    if resources.latest().is_some_and(|sample| sample.cpu_pending) {
                        "Waiting for the next CPU sample"
                    } else {
                        "CPU samples unavailable"
                    },
                    width,
                ),
            ));
        }
    } else if budget > rows.len() {
        rows.push((
            PanelTone::Muted,
            clipped("Enlarge to view the CPU chart", width),
        ));
    }
    rows.truncate(budget);
    rows
}

fn detail(label: &str, value: &str, width: usize) -> (PanelTone, String) {
    (
        PanelTone::Normal,
        clipped(&format!("{label:<10}{value}"), width),
    )
}

fn clipped(text: &str, width: usize) -> String {
    wrap_activity(&panel::safe_text(text), width, 1)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}.{:01}s", seconds, ms / 100 % 10)
    }
}

fn memory(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    }
}

fn cpu_text(sample: Option<&metrics::Sample>) -> String {
    if let Some(percent) = sample.and_then(|sample| sample.cpu_percent) {
        format!("{percent:.1}%")
    } else if sample.is_some_and(|sample| sample.cpu_pending) {
        "Sampling…".into()
    } else {
        "Unavailable".into()
    }
}

fn cpu_chart(samples: &VecDeque<metrics::Sample>, cols: usize, rows: usize) -> (Vec<String>, f64) {
    if cols == 0 || rows == 0 {
        return (Vec::new(), 0.1);
    }
    let upper = (samples
        .iter()
        .filter_map(|sample| sample.cpu_percent)
        .fold(0.0_f64, f64::max)
        * 10.0)
        .ceil()
        .max(1.0)
        / 10.0;
    let mut cells = vec![0_u8; cols * rows];
    let width = cols * 2;
    let height = rows * 4;
    let first = samples.front().map(|sample| sample.at);
    let span = samples.back().zip(first).map_or(0.0, |(last, first)| {
        last.at.saturating_duration_since(first).as_secs_f64()
    });
    let mut previous = None;
    for sample in samples {
        let Some(value) = sample
            .cpu_percent
            .filter(|value| value.is_finite() && *value >= 0.0)
        else {
            previous = None;
            continue;
        };
        let x = if span > 0.0 {
            ((sample
                .at
                .saturating_duration_since(first.unwrap())
                .as_secs_f64()
                / span)
                * (width - 1) as f64)
                .round() as usize
        } else {
            width - 1
        };
        let y = ((1.0 - (value / upper).clamp(0.0, 1.0)) * (height - 1) as f64).round() as usize;
        let point = (x.min(width - 1), y.min(height - 1));
        draw_chart_line(&mut cells, cols, previous.unwrap_or(point), point);
        previous = Some(point);
    }
    let plot = cells
        .chunks(cols)
        .map(|row| {
            row.iter()
                .map(|bits| {
                    if *bits == 0 {
                        ' '
                    } else {
                        char::from_u32(0x2800 + u32::from(*bits)).unwrap_or(' ')
                    }
                })
                .collect()
        })
        .collect();
    (plot, upper)
}

fn draw_chart_line(cells: &mut [u8], cols: usize, from: (usize, usize), to: (usize, usize)) {
    let (mut x, mut y) = (from.0 as isize, from.1 as isize);
    let (target_x, target_y) = (to.0 as isize, to.1 as isize);
    let dx = (target_x - x).abs();
    let dy = -(target_y - y).abs();
    let sx = if x < target_x { 1 } else { -1 };
    let sy = if y < target_y { 1 } else { -1 };
    let mut error = dx + dy;
    loop {
        let (px, py) = (x as usize, y as usize);
        let bits = [[0, 1, 2, 6], [3, 4, 5, 7]];
        if let Some(cell) = cells.get_mut(py / 4 * cols + px / 2) {
            *cell |= 1 << bits[px % 2][py % 4];
        }
        if x == target_x && y == target_y {
            break;
        }
        let twice = 2 * error;
        if twice >= dy {
            error += dy;
            x += sx;
        }
        if twice <= dx {
            error += dx;
            y += sy;
        }
    }
}
fn activity_field(
    label: &str,
    value: Option<&str>,
    missing: &str,
    width: usize,
    max_rows: usize,
) -> Vec<(PanelTone, String)> {
    let value = value
        .map(panel::safe_text)
        .filter(|text| !text.trim().is_empty());
    let tone = if value.is_some() {
        PanelTone::Normal
    } else {
        PanelTone::Muted
    };
    let prefix = format!("{label:<10}");
    let indent = prefix.width();
    if width <= indent {
        return wrap_activity(&prefix, width, 1)
            .into_iter()
            .map(|text| (tone, text))
            .collect();
    }
    wrap_activity(
        value.as_deref().unwrap_or(missing),
        width - indent,
        max_rows,
    )
    .into_iter()
    .enumerate()
    .map(|(index, text)| {
        (
            tone,
            format!(
                "{}{text}",
                if index == 0 {
                    prefix.clone()
                } else {
                    " ".repeat(indent)
                }
            ),
        )
    })
    .collect()
}

fn wrap_activity(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut row = String::new();
    let mut used = 0;
    for character in text.chars() {
        let cells = character.width().unwrap_or(0);
        if used + cells > width {
            if lines.len() + 1 == max_rows || cells > width {
                while row.width() + 1 > width {
                    row.pop();
                }
                row.push('…');
                lines.push(row);
                return lines;
            }
            lines.push(std::mem::take(&mut row));
            used = 0;
        }
        row.push(character);
        used += cells;
    }
    lines.push(row);
    lines
}

fn line(state: &State, parts: &[(PanelTone, String)]) -> String {
    let mut width = 0;
    let mut output = String::new();
    for (tone, text) in parts {
        let text = fit(
            &panel::safe_text(text),
            state.geometry.cols.saturating_sub(width),
        );
        width += u16::try_from(text.width()).unwrap_or(u16::MAX);
        output.push_str(&state.tone(*tone, &text));
    }
    output.push_str(&state.tone(
        PanelTone::Normal,
        &" ".repeat(usize::from(state.geometry.cols.saturating_sub(width))),
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{
        terminal::nonzero,
        ui::{Layout, Ui},
    };
    use crate::worker::SlotLog;
    use std::time::Duration;

    fn state(rows: u16, cols: u16, color: bool) -> State {
        let mut state = State::new(
            rows,
            cols,
            &Ui {
                color,
                layout: Layout::Chat,
                ..Ui::default()
            },
        );
        state.preview = true;
        state.worker = Some(Monitor::default());
        state
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            process_id: std::process::id(),
            generation: "fixture".into(),
            manager: None,
            total: 3,
            desired_total: 3,
            registered_workers: 2,
            retiring: 0,
            idle: 1,
            working: 1,
            offline: 1,
            queued: 2,
            closed: false,
            stopping: false,
            slots: vec![
                SlotView {
                    slot: 0,
                    epoch: 1,
                    state: "idle",
                    registered: true,
                    retire_requested: false,
                    task_id: None,
                    task_summary: None,
                    last_command: None,
                    progress: None,
                    total_work_ms: 0,
                    current_task_elapsed_ms: None,
                    task_started_at_ms: None,
                    recent_task_durations_ms: Vec::new(),
                    reason: None,
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
                SlotView {
                    slot: 1,
                    epoch: 1,
                    state: "working",
                    registered: true,
                    retire_requested: false,
                    task_id: Some("task-actual".into()),
                    task_summary: None,
                    last_command: None,
                    progress: None,
                    total_work_ms: 0,
                    current_task_elapsed_ms: None,
                    task_started_at_ms: None,
                    recent_task_durations_ms: Vec::new(),
                    reason: None,
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
                SlotView {
                    slot: 2,
                    epoch: 0,
                    state: "offline",
                    registered: false,
                    retire_requested: false,
                    task_id: None,
                    task_summary: None,
                    last_command: None,
                    progress: None,
                    total_work_ms: 0,
                    current_task_elapsed_ms: None,
                    task_started_at_ms: None,
                    recent_task_durations_ms: Vec::new(),
                    reason: Some("waiting for readiness".into()),
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
            ],
        }
    }

    fn log(sequence: u64) -> SlotLog {
        SlotLog {
            sequence,
            at_ms: 1_700_000_000_000 + sequence * 1000,
            level: "info".into(),
            text: format!("event-{sequence}"),
            task_id: None,
        }
    }

    fn populated() -> Snapshot {
        let mut snapshot = snapshot();
        snapshot.total = 2;
        snapshot.desired_total = 2;
        snapshot.offline = 0;
        snapshot.slots.truncate(2);
        for slot in &mut snapshot.slots {
            slot.logs = (0..40).map(log).collect();
            slot.next_log_sequence = 40;
        }
        snapshot
    }

    fn screen(frame: &Frame) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            nonzero(frame.geometry.rows),
            nonzero(frame.geometry.cols),
            0,
        );
        parser.process(&frame.diff(None));
        parser
    }

    #[test]
    fn worker_monitor_frames_keep_real_counts_and_labels_at_wide_and_narrow_widths() {
        for (rows, cols) in [(20, 100), (18, 48), (18, 32), (18, 18)] {
            for color in [true, false] {
                let mut state = state(rows, cols, color);
                let frame = state.worker_frame(Some(&snapshot()));
                let text = screen(&frame).screen().contents();
                for label in [
                    "Idle 1",
                    "Working 1",
                    "Offline 1",
                    "Retiring 0",
                    "Total 3",
                    "Registered 2/3",
                    "Queued 2",
                ] {
                    assert!(text.contains(label), "{rows}x{cols}: {text}");
                }
                assert!(!text.contains("Type a message"));
                assert!(!text.contains("Next caller"));
                assert!(frame.hidden);
                assert!(frame.bracketed_paste);
                assert_eq!(frame.rows.len(), usize::from(rows));
                if !color {
                    assert!(frame.rows.iter().all(|row| !row.contains('\u{1b}')));
                }
            }
        }
    }

    #[test]
    fn worker_manager_identity_is_explicit_without_abbreviating_ids_or_hiding_active_work() {
        let mut snapshot = snapshot();
        snapshot.process_id = 31415;
        assert!(identity_lines(&snapshot, 80).contains(&"Manager unbound".into()));
        let client = "c".repeat(64);
        let thread = "agent_verified_manager";
        snapshot.manager = Some(crate::worker::ManagerView {
            client_id: client.clone(),
            thread_id: thread.into(),
            participant_id: thread.into(),
            connection: 1,
            attached_at_ms: 1,
        });
        for width in [1, 4, 18, 48, 80] {
            let identity = identity_lines(&snapshot, width);
            assert!(identity.concat().contains(&client));
            assert!(identity.concat().contains(thread));
            assert!(
                identity
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
        }
        let mut wide = state(30, 100, false);
        let text = screen(&wide.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(text.contains(&client) && text.contains(thread));
        assert!(text.contains("OS PID 31415"));
        snapshot.stopping = true;
        let text = screen(&wide.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(text.contains("Working 1"));
        assert!(text.contains("waiting for worker confirmation"));
        let mut narrow = state(18, 18, false);
        let text = screen(&narrow.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains(&client[..16]));
        assert!(text.contains("Manager attached"));
        assert!(text.contains("Working 1"));
    }

    #[test]
    fn worker_monitor_never_invents_readiness_and_selects_only_reported_slots() {
        let mut state = state(14, 80, false);
        let first = state.worker_frame(None);
        assert!(
            screen(&first)
                .screen()
                .contents()
                .contains("Waiting for worker readiness")
        );
        assert!(!screen(&first).screen().contents().contains("Idle 0"));
        let mut snapshot = snapshot();
        snapshot.total = 20;
        snapshot.desired_total = 20;
        snapshot.offline = 18;
        snapshot.slots.extend((3..20).map(|slot| SlotView {
            slot,
            epoch: 0,
            state: "offline",
            registered: false,
            retire_requested: false,
            task_id: None,
            task_summary: None,
            last_command: None,
            progress: None,
            total_work_ms: 0,
            current_task_elapsed_ms: None,
            task_started_at_ms: None,
            recent_task_durations_ms: Vec::new(),
            reason: None,
            logs: Vec::new(),
            next_log_sequence: 0,
        }));
        assert!(
            !screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents()
                .contains("#20")
        );
        state.keyboard(b"\x1b[1;5F", Instant::now(), false);
        assert!(
            screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents()
                .contains("#20")
        );
        state.keyboard(b"\x1b[1;5H", Instant::now(), false);
        assert!(
            screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents()
                .contains("#01")
        );
        snapshot.closed = true;
        assert!(
            screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents()
                .contains("Stopping")
        );
    }

    #[test]
    fn worker_monitor_reports_manager_stop_while_work_is_unconfirmed() {
        for (rows, cols) in [(20, 100), (18, 48), (18, 32)] {
            for color in [true, false] {
                let mut state = state(rows, cols, color);
                let mut snapshot = snapshot();
                snapshot.stopping = true;
                snapshot.queued = 0;
                snapshot.idle = 0;
                snapshot.offline = 2;
                snapshot.slots[0].state = "offline";
                assert!(!snapshot.closed);

                let text = screen(&state.worker_frame(Some(&snapshot)))
                    .screen()
                    .contents();
                assert!(text.contains("Working 1"), "{rows}x{cols}: {text}");
                assert!(text.contains("Stopping"), "{rows}x{cols}: {text}");
                assert!(!text.contains("FIFO assignment"), "{rows}x{cols}: {text}");
                assert!(!text.contains("Type a message"));
            }
        }
    }

    #[test]
    fn worker_monitor_waits_for_retirement_after_active_work_finishes() {
        for (rows, cols) in [(20, 100), (18, 48), (18, 32)] {
            let mut state = state(rows, cols, false);
            let mut snapshot = snapshot();
            snapshot.stopping = true;
            snapshot.total = 1;
            snapshot.desired_total = 0;
            snapshot.registered_workers = 1;
            snapshot.retiring = 1;
            snapshot.queued = 0;
            snapshot.idle = 0;
            snapshot.working = 0;
            snapshot.offline = 0;
            snapshot.slots.truncate(1);
            snapshot.slots[0].state = "retiring";
            snapshot.slots[0].retire_requested = true;
            snapshot.slots[0].registered = true;
            snapshot.slots[0].task_id = None;
            snapshot.slots[0].current_task_elapsed_ms = None;
            snapshot.slots[0].task_started_at_ms = None;

            let text = screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents();
            assert!(text.contains("Working 0"), "{rows}x{cols}: {text}");
            assert!(text.contains("Retiring 1"), "{rows}x{cols}: {text}");
            assert!(text.contains("Stopping"), "{rows}x{cols}: {text}");
            assert!(!text.contains("Stopped"), "{rows}x{cols}: {text}");
            if cols >= 80 {
                assert!(
                    text.contains("waiting for retirement confirmation"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn worker_monitor_reports_stopped_after_manager_stop_is_confirmed() {
        for (rows, cols) in [(20, 100), (18, 48), (18, 32)] {
            let mut state = state(rows, cols, false);
            let mut snapshot = snapshot();
            snapshot.stopping = true;
            snapshot.queued = 0;
            snapshot.idle = 0;
            snapshot.working = 0;
            snapshot.offline = snapshot.total;
            for slot in &mut snapshot.slots {
                slot.state = "offline";
                slot.task_id = None;
            }
            assert!(!snapshot.closed);

            let text = screen(&state.worker_frame(Some(&snapshot)))
                .screen()
                .contents();
            assert!(text.contains("Working 0"), "{rows}x{cols}: {text}");
            assert!(text.contains("Stopped"), "{rows}x{cols}: {text}");
            assert!(!text.contains("Stopping"), "{rows}x{cols}: {text}");
            assert!(!text.contains("FIFO assignment"), "{rows}x{cols}: {text}");
        }
    }

    #[test]
    fn worker_logs_scroll_independently_and_keep_their_anchor_when_entries_arrive() {
        let mut state = state(24, 90, false);
        let mut snapshot = populated();
        state.worker_frame(Some(&snapshot));
        state.keyboard(b"\x1b[A", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let first_anchor = state.worker.as_ref().unwrap().anchors[&0].unwrap();
        state.keyboard(b"\x1b[C\x1b[A\x1b[A", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let monitor = state.worker.as_ref().unwrap();
        assert_eq!(monitor.selected_slot(), Some(1));
        assert_eq!(monitor.anchors[&0], Some(first_anchor));
        assert!(monitor.anchors[&1].is_some());
        snapshot.slots[0].logs.push(log(40));
        snapshot.slots[0].next_log_sequence = 41;
        state.worker_frame(Some(&snapshot));
        assert_eq!(
            state.worker.as_ref().unwrap().anchors[&0],
            Some(first_anchor)
        );
        state.keyboard(b"\x1b[D\x1b[F", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let monitor = state.worker.as_ref().unwrap();
        assert_eq!(monitor.anchors[&0], None);
        assert!(monitor.anchors[&1].is_some());
        assert!(
            monitor
                .panels
                .iter()
                .find(|panel| panel.slot == 0)
                .unwrap()
                .following
        );
    }

    #[test]
    fn worker_log_retention_resize_and_generation_change_keep_valid_views() {
        let mut state = state(24, 90, false);
        let mut snapshot = populated();
        state.worker_frame(Some(&snapshot));
        state.keyboard(b"\x1b[H", Instant::now(), false);
        snapshot.slots[0].logs.drain(..30);
        state.worker_frame(Some(&snapshot));
        assert_eq!(
            state.worker.as_ref().unwrap().anchors[&0].unwrap().sequence,
            30
        );
        state.resize(12, 24);
        state.worker_frame(Some(&snapshot));
        for panel in &state.worker.as_ref().unwrap().panels {
            assert!(panel.start <= panel.keys.len().saturating_sub(panel.log_rows));
        }
        snapshot.generation = "replacement".into();
        state.worker_frame(Some(&snapshot));
        assert!(state.worker.as_ref().unwrap().anchors.is_empty());
        assert!(
            state
                .worker
                .as_ref()
                .unwrap()
                .panels
                .iter()
                .all(|panel| panel.following)
        );
    }

    #[test]
    fn worker_tabs_select_stations_and_wheel_only_scrolls_the_left_log() {
        let mut state = state(28, 100, false);
        let snapshot = populated();
        state.worker_frame(Some(&snapshot));
        let tab = &state.worker.as_ref().unwrap().tabs[1];
        let (x, y) = (tab.col + 1, tab.row + 1);
        state.keyboard(
            format!("\x1b[<0;{x};{y}M").as_bytes(),
            Instant::now(),
            false,
        );
        state.worker_frame(Some(&snapshot));
        let panel = &state.worker.as_ref().unwrap().panels[0];
        let (x, y) = (panel.log_col + 1, panel.log_row + 1);
        state.keyboard(
            format!("\x1b[<64;{x};{y}M").as_bytes(),
            Instant::now(),
            false,
        );
        state.worker_frame(Some(&snapshot));
        let monitor = state.worker.as_ref().unwrap();
        assert_eq!(monitor.selected_slot(), Some(1));
        assert!(!monitor.anchors.contains_key(&0));
        assert!(monitor.anchors[&1].is_some());
        let anchor = monitor.anchors[&1];
        let panel = &monitor.panels[0];
        let (right_x, right_y) = (panel.details_col + 1, panel.details_row + 1);
        state.keyboard(
            format!("\x1b[<64;{right_x};{right_y}M").as_bytes(),
            Instant::now(),
            false,
        );
        state.worker_frame(Some(&snapshot));
        assert_eq!(state.worker.as_ref().unwrap().anchors[&1], anchor);
        let view = state.worker_snapshot(true);
        assert_eq!(view["panes"]["input"]["visible"], false);
        assert_eq!(view["worker_panels"][0]["slot"], 1);
        assert_eq!(view["worker_panels"][0]["selected"], true);
        assert_eq!(view["worker_panels"][0]["logs"]["following"], false);
        assert_eq!(view["worker_panels"][0]["logs"]["row"], y - 1);
        assert_eq!(view["worker_panels"][0]["details"]["scrollable"], false);
    }

    #[test]
    fn worker_right_panel_refreshes_real_timing_without_following_log_scroll() {
        let mut state = state(30, 110, false);
        let mut snapshot = populated();
        snapshot.slots[0].state = "working";
        snapshot.slots[0].task_id = Some("current-fixture-task".into());
        snapshot.slots[1].state = "idle";
        snapshot.slots[1].task_id = None;
        snapshot.slots[0].task_summary = Some("Verify private worker cleanup".into());
        snapshot.slots[0].total_work_ms = 125_000;
        snapshot.slots[0].current_task_elapsed_ms = Some(9_000);
        snapshot.slots[0].recent_task_durations_ms = vec![2_500, 12_000, 65_000];
        snapshot.slots[0].logs[39].text = "command: check --private-fixture".into();
        let first = state.worker_frame(Some(&snapshot));
        let panel = &state.worker.as_ref().unwrap().panels[0];
        let (row, col, width) = (panel.details_row, panel.details_col, panel.details_cols);
        let fixed = |frame: &Frame| {
            frame
                .rows
                .iter()
                .skip(row)
                .take(5)
                .map(|line| line.chars().skip(col).take(width).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let before = fixed(&first);
        assert!(before.contains("Verify private worker"));
        assert!(before.contains("9.0s"));
        assert!(before.contains("2m05s"));
        assert!(
            screen(&first)
                .screen()
                .contents()
                .contains("command: check --private-fixture")
        );
        state.keyboard(b"\x1b[A", Instant::now(), false);
        let scrolled = state.worker_frame(Some(&snapshot));
        assert_eq!(fixed(&scrolled), before);
        let anchor = state.worker.as_ref().unwrap().anchors[&0];
        snapshot.slots[0].current_task_elapsed_ms = Some(12_000);
        let updated = state.worker_frame(Some(&snapshot));
        assert!(fixed(&updated).contains("12.0s"));
        assert!(!fixed(&updated).contains("9.0s"));
        assert_eq!(state.worker.as_ref().unwrap().anchors[&0], anchor);
        let text = screen(&updated).screen().contents();
        assert!(text.contains("Agent CPU / RAM unavailable"));
        assert!(text.contains("Shared client PID"));
    }

    #[test]
    fn worker_log_wrapping_preserves_text_and_sanitizes_controls_and_secrets() {
        let mut snapshot = populated();
        snapshot.slots[0].logs = vec![SlotLog {
            text: "测试 long entry with a visible ending END password=fixture-secret\x1b[2J\r\n"
                .into(),
            ..log(7)
        }];
        let lines = log_lines(&snapshot.slots[0], 20);
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|(_, _, text)| text.width() <= 20));
        let text: String = lines.iter().map(|(_, _, text)| text.as_str()).collect();
        assert!(text.contains("visible ending END"));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert!(lines.iter().all(|(key, _, _)| key.sequence == 7));
    }

    #[test]
    fn worker_fixed_details_wrap_task_and_never_invent_resource_samples() {
        let activity = ActivitySummary {
            task: Some("Check Unicode rendering and retain the terminal ownership boundary"),
            total_work_ms: 125_000,
            current_task_elapsed_ms: Some(1_250),
            recent_task_durations_ms: &[500, 2_000, 61_000],
        };
        let resources = metrics::Sampler::default();
        let rows = station_details(&activity, &resources, 42, 42, 16);
        assert!(rows.iter().all(|(_, text)| text.width() <= 42));
        assert!(rows[0].1.contains("Task"));
        assert!(rows[1].1.ends_with('…'));
        assert!(rows[2].1.contains("1.2s"));
        assert!(rows[3].1.contains("2m05s"));
        let text = rows
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Agent CPU / RAM unavailable"));
        assert!(text.contains("Shared client PID 42"));
        assert!(text.contains("CPU samples unavailable"));
        assert!(!text.contains("0.0%"));
        assert!(!text.contains("0.0 MiB"));
    }

    #[test]
    fn worker_fixed_details_fit_small_dimensions_and_sanitize_task_text() {
        let resources = metrics::Sampler::default();
        for width in 0..48 {
            for budget in 0..20 {
                let rows =
                    station_details(&ActivitySummary::default(), &resources, 42, width, budget);
                assert!(rows.len() <= budget);
                assert!(rows.iter().all(|(_, text)| text.width() <= width));
            }
        }
        let lines = activity_field(
            "Task",
            Some("检查连接 password=fixture-secret\r\n\x1b[2J"),
            "No task assigned",
            42,
            2,
        );
        let text = lines
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<String>();
        assert!(lines.iter().all(|(_, text)| text.width() <= 42));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert_eq!(wrap_activity("中文", 1, 2), vec!["…"]);
    }

    #[test]
    fn cpu_chart_leaves_missing_samples_blank_and_labels_the_actual_scale() {
        let now = Instant::now();
        let mut samples = VecDeque::new();
        for (index, cpu_percent) in [Some(0.0), None, Some(1.0)].into_iter().enumerate() {
            samples.push_back(metrics::Sample {
                at: now + Duration::from_secs(index as u64),
                cpu_percent,
                memory_bytes: None,
                cpu_pending: false,
            });
        }
        let (plot, upper) = cpu_chart(&samples, 9, 2);
        assert_eq!(upper, 1.0);
        assert_eq!(plot.len(), 2);
        assert!(plot.iter().all(|row| row.width() == 9));
        assert_eq!(
            plot.iter()
                .flat_map(|row| row.chars())
                .filter(|character| *character != ' ')
                .count(),
            2
        );
        samples[1].cpu_percent = Some(0.5);
        let (connected, _) = cpu_chart(&samples, 9, 2);
        assert!(
            connected
                .iter()
                .flat_map(|row| row.chars())
                .filter(|character| *character != ' ')
                .count()
                > 2
        );
        for sample in &mut samples {
            sample.cpu_percent = None;
        }
        assert!(
            cpu_chart(&samples, 9, 2)
                .0
                .iter()
                .all(|row| row.trim().is_empty())
        );
    }

    #[test]
    fn worker_monitor_consumes_layout_keys_mouse_and_split_pastes_without_changing_the_draft() {
        let bytes = b"\x1bOQignored\r\x1b\x1b[<0;2;3M\x1b[18~\x1b[200~pasted\x03\x1bOQ\r\x1b[201~";
        for split in 0..=bytes.len() {
            let mut state = state(20, 80, false);
            state
                .conversation
                .composer
                .set_text("preserved draft @file $skill");
            let mut before = state.conversation.snapshot();
            before.as_object_mut().unwrap().remove("pending_questions");
            let now = Instant::now();
            assert!(state.keyboard(&bytes[..split], now, false).is_empty());
            assert!(state.keyboard(&bytes[split..], now, false).is_empty());
            assert!(
                state
                    .keyboard(&[], now + Duration::from_millis(100), true)
                    .is_empty()
            );
            let mut after = state.conversation.snapshot();
            after.as_object_mut().unwrap().remove("pending_questions");
            assert_eq!(after, before);
            assert!(state.actions.is_empty());
            assert!(!state.selection);
            assert!(!state.help);
            assert_eq!(state.ui.layout, Layout::Chat);
            assert!(!state.pty.screen().contents().contains("ignored"));
            for layout in [Layout::Chat, Layout::Split, Layout::Logs, Layout::Audit] {
                state.ui.layout = layout;
                let frame = state.frame(now);
                assert!(screen(&frame).screen().contents().contains("Workers"));
                assert!(
                    !screen(&frame)
                        .screen()
                        .contents()
                        .contains("preserved draft")
                );
            }
        }
    }
}
