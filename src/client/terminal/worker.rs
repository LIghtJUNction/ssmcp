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
    progress: Option<&'a str>,
    command: Option<&'a str>,
    total_work_ms: u64,
    current_task_elapsed_ms: Option<u64>,
    recent_task_durations_ms: &'a [u64],
}

impl<'a> From<&'a SlotView> for ActivitySummary<'a> {
    fn from(slot: &'a SlotView) -> Self {
        Self {
            task: slot.task_summary.as_deref().or(slot.task_id.as_deref()),
            progress: slot.progress.as_deref(),
            command: slot.last_command.as_deref(),
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
    // Each station keeps independent progress and diagnostic scroll positions.
    details: BTreeMap<usize, bool>,
    other_anchors: BTreeMap<usize, Option<LogAnchor>>,
    progress_tasks: BTreeMap<usize, (u64, Option<String>)>,
    expanded_commands: BTreeMap<usize, bool>,
    task_views: BTreeMap<usize, TaskView>,
    pending_scroll: BTreeMap<usize, PendingScroll>,
    panels: Vec<WorkerPanel>,
    tabs: Vec<WorkerTab>,
    coordination_row: usize,
    coordination_rows: usize,
    resources: metrics::Sampler,
    help: bool,
}

#[derive(Default)]
struct TaskView {
    saved_log_anchor: Option<LogAnchor>,
    task_id: Option<String>,
    initialized: bool,
}

impl Monitor {
    fn showing_details(&self, slot: usize) -> bool {
        self.details.get(&slot).copied().unwrap_or(false)
    }

    fn toggle_details(&mut self) {
        let Some(slot) = self.selected_slot() else {
            return;
        };
        if self.task_views.contains_key(&slot) {
            self.toggle_task();
        }
        let current = self.anchors.get(&slot).copied().flatten();
        let next = self.other_anchors.insert(slot, current).flatten();
        self.anchors.insert(slot, next);
        let details = self.details.entry(slot).or_default();
        *details = !*details;
        self.pending_scroll.remove(&slot);
        self.panels.retain(|panel| panel.slot != slot);
    }

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

    fn cycle(&mut self, next: bool) {
        let count = self.order.len();
        if count > 0 {
            self.selected = if next {
                (self.selected + 1) % count
            } else {
                (self.selected + count - 1) % count
            };
        }
    }

    fn toggle_task(&mut self) {
        let Some(slot) = self.selected_slot() else {
            return;
        };
        if let Some(view) = self.task_views.remove(&slot) {
            self.anchors.insert(slot, view.saved_log_anchor);
        } else {
            self.task_views.insert(
                slot,
                TaskView {
                    saved_log_anchor: self.anchors.get(&slot).copied().flatten(),
                    ..TaskView::default()
                },
            );
            self.anchors.insert(
                slot,
                Some(LogAnchor {
                    sequence: 0,
                    part: 0,
                }),
            );
        }
        self.pending_scroll.remove(&slot);
        self.panels.retain(|panel| panel.slot != slot);
    }

    fn toggle_commands(&mut self) {
        if let Some(slot) = self.selected_slot() {
            if self.task_views.contains_key(&slot) {
                return;
            }
            if !self.showing_details(slot) {
                self.toggle_details();
            }
            let expanded = self.expanded_commands.entry(slot).or_default();
            *expanded = !*expanded;
            if let Some(Some(anchor)) = self.anchors.get_mut(&slot) {
                anchor.part = 0;
            }
        }
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
            self.details.clear();
            self.other_anchors.clear();
            self.progress_tasks.clear();
            self.expanded_commands.clear();
            self.task_views.clear();
            self.pending_scroll.clear();
        }
        self.order = snapshot.slots.iter().map(|slot| slot.slot).collect();
        self.selected = previous_slot
            .and_then(|slot| self.order.iter().position(|value| *value == slot))
            .unwrap_or_else(|| self.selected.min(self.order.len().saturating_sub(1)));
        self.anchors.retain(|slot, _| self.order.contains(slot));
        self.details.retain(|slot, _| self.order.contains(slot));
        self.other_anchors
            .retain(|slot, _| self.order.contains(slot));
        self.progress_tasks
            .retain(|slot, _| self.order.contains(slot));
        self.expanded_commands
            .retain(|slot, _| self.order.contains(slot));
        self.task_views.retain(|slot, _| self.order.contains(slot));
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
            let (tone, symbol, label) = slot_state(slot);
            let age = if slot.registered && slot.total_work_ms > crate::worker::TWO_HOURS_MS {
                " 2h+"
            } else {
                ""
            };
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
                    "{}#{:02} {symbol} {label}{age}  ",
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
        if self.help {
            output.extend(
                monitor_help(width)
                    .into_iter()
                    .take(available - output.len())
                    .map(|row| vec![row]),
            );
            return output;
        }
        let activity = ActivitySummary::from(slot);
        let task_view = self.task_views.contains_key(&slot.slot);
        let details_view = self.showing_details(slot.slot);
        let context = (slot.epoch, slot.task_id.clone());
        if self.progress_tasks.get(&slot.slot) != Some(&context) {
            // A new task must not inherit a paused position from its predecessor.
            // Diagnostic logs deliberately retain their station-wide history.
            if details_view {
                self.other_anchors.remove(&slot.slot);
            } else if let Some(view) = self.task_views.get_mut(&slot.slot) {
                view.saved_log_anchor = None;
            } else {
                self.anchors.remove(&slot.slot);
            }
            self.progress_tasks.insert(slot.slot, context);
        }
        if let Some(view) = self.task_views.get_mut(&slot.slot) {
            if view.initialized && view.task_id != slot.task_id {
                self.anchors.insert(
                    slot.slot,
                    Some(LogAnchor {
                        sequence: 0,
                        part: 0,
                    }),
                );
            }
            view.task_id.clone_from(&slot.task_id);
            view.initialized = true;
        }
        let body_top = top + output.len();
        let body_rows = available - output.len();
        let split = width >= 96 && details_view && !task_view;
        let right_cols = if split {
            (width / 4).clamp(32, 38)
        } else {
            width
        };
        let gap = if split { 3 } else { 0 };
        let left_cols = if split {
            width - right_cols - gap
        } else {
            width
        };
        let compact_budget = if split || task_view {
            0
        } else if !details_view && body_rows >= 8 {
            3
        } else if details_view && body_rows >= 14 {
            8
        } else if body_rows >= 6 {
            2
        } else {
            0
        };
        let compact = if details_view {
            compact_details(&self.resources, width, compact_budget)
        } else {
            progress_summary(slot, width, compact_budget)
        };
        output.extend(compact.iter().cloned().map(|line| vec![line]));
        let log_area_row = body_top + compact.len();
        let log_area_rows = body_rows - compact.len();
        let header_rows = usize::from(log_area_rows >= 2);
        let log_row = log_area_row + header_rows;
        let log_rows = log_area_rows.saturating_sub(header_rows + 1);
        let entries = if task_view {
            task_lines(slot, snapshot, left_cols.saturating_sub(2).max(1))
        } else if details_view {
            log_lines(
                slot,
                left_cols.saturating_sub(2).max(1),
                self.expanded_commands
                    .get(&slot.slot)
                    .copied()
                    .unwrap_or(false),
            )
        } else {
            progress_lines(slot, left_cols.saturating_sub(2).max(1))
        };
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
            let heading = (
                PanelTone::Heading,
                if task_view {
                    "  Full task · T Back".into()
                } else if !details_view {
                    if slot.task_id.is_some() {
                        "  Agent progress".into()
                    } else {
                        "  Recent agent progress".into()
                    }
                } else {
                    "  Work log".into()
                },
            );
            output.push(if split {
                columns(
                    vec![heading],
                    Some((PanelTone::Muted, "Activity · T Full task".into())),
                    left_cols,
                    gap,
                )
            } else {
                vec![heading]
            });
        }
        for offset in 0..log_area_rows.saturating_sub(header_rows) {
            let left = if offset < log_rows {
                if let Some((_, parts)) = entries.get(start + offset) {
                    let mut row = vec![(PanelTone::Muted, "  ".into())];
                    row.extend(parts.iter().cloned());
                    row
                } else if offset == 0 && entries.is_empty() {
                    vec![(
                        PanelTone::Muted,
                        format!(
                            "  {}",
                            panel::safe_text(
                                slot.reason.as_deref().unwrap_or("No work reported yet")
                            )
                        ),
                    )]
                } else {
                    Vec::new()
                }
            } else if log_rows == 0 {
                vec![(PanelTone::Muted, "Enlarge terminal to view progress".into())]
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
                vec![(
                    PanelTone::Muted,
                    format!(
                        "  {} {range} · {}",
                        if task_view {
                            "Task"
                        } else if details_view {
                            "Log"
                        } else {
                            "Progress"
                        },
                        if following {
                            "Live"
                        } else {
                            "Paused · End to follow"
                        }
                    ),
                )]
            };
            output.push(if split {
                columns(left, details.get(offset).cloned(), left_cols, gap)
            } else {
                left
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
        // The raw decoder keeps split paste markers and pasted control keys literal.
        // Text is discarded; only navigation and actual monitor controls act.
        let (_, actions) = self.keyboard.feed_worker(bytes, now, flush);
        for action in actions {
            if matches!(action, Key::Interrupt) {
                request_interrupt(self);
                return;
            }
            if matches!(action, Key::ClientReload) {
                if self.preview {
                    self.notice = Some("Preview has no connected worker to reload".into());
                } else if self.actions.len() < 64 && !self.actions.contains(&"client.reload") {
                    self.actions.push("client.reload");
                    self.notice = Some("Requesting worker reload".into());
                }
                self.dirty = true;
                continue;
            }
            let Some(monitor) = self.worker.as_mut() else {
                return;
            };
            if monitor.help {
                if matches!(action, Key::Help | Key::Escape) {
                    monitor.help = false;
                    self.dirty = true;
                }
                continue;
            }
            match action {
                Key::Help => {
                    monitor.help = true;
                    monitor.panels.clear();
                }
                Key::Layout => monitor.toggle_details(),
                Key::Escape => {
                    if monitor
                        .selected_slot()
                        .is_some_and(|slot| monitor.task_views.contains_key(&slot))
                    {
                        monitor.toggle_task();
                    } else if monitor
                        .selected_slot()
                        .is_some_and(|slot| monitor.showing_details(slot))
                    {
                        monitor.toggle_details();
                    }
                }
                Key::Back => monitor.select(false),
                Key::Open => monitor.select(true),
                Key::FocusBack => monitor.cycle(false),
                Key::CanvasRight => monitor.cycle(true),
                Key::CanvasLeft => monitor.toggle_task(),
                Key::Toggle => monitor.toggle_commands(),
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
                    if button & 35 == 0
                        && button & 64 == 0
                        && let Some(slot) = monitor
                            .tabs
                            .iter()
                            .find(|tab| {
                                usize::from(y) == tab.row
                                    && usize::from(x) >= tab.col
                                    && usize::from(x) < tab.col + tab.cols
                            })
                            .map(|tab| tab.slot)
                    {
                        if let Some(index) = monitor.order.iter().position(|value| *value == slot) {
                            monitor.selected = index;
                            self.dirty = true;
                        }
                        continue;
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
                "view":if monitor.is_some_and(|monitor| monitor.task_views.contains_key(&panel.slot)) {"task"}
                    else if monitor.is_some_and(|monitor| monitor.showing_details(panel.slot)) {"details"} else {"progress"},
                "logs":{"row":panel.log_row,"col":panel.log_col,"rows":panel.log_rows,"cols":panel.log_cols,"visible":panel.log_rows>0,
                    "retained_lines":panel.keys.len(),"first_line":panel.start,"following":panel.following,
                    "commands_expanded":monitor.and_then(|monitor| monitor.expanded_commands.get(&panel.slot)).copied().unwrap_or(false),
                    "task_view":monitor.is_some_and(|monitor| monitor.task_views.contains_key(&panel.slot))},
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
                "worker_activity":{"row":monitor.map_or(0, |monitor| monitor.coordination_row),"col":0,
                    "rows":monitor.map_or(0, |monitor| monitor.coordination_rows),"cols":self.geometry.cols,
                    "visible":monitor.is_some_and(|monitor| monitor.coordination_rows>0)},
                "input":{"visible":false}, "logs":{"visible":false},
                "audit":{"visible":false}, "tasks":{"visible":false}
            },
            "worker_panels":panels,
            "worker_tabs":monitor.into_iter().flat_map(|monitor| &monitor.tabs).map(|tab| {
                serde_json::json!({"slot":tab.slot,"row":tab.row,"col":tab.col,"cols":tab.cols})
            }).collect::<Vec<_>>(),
            "selected_worker":monitor.and_then(Monitor::selected_slot),
            "help":monitor.is_some_and(|monitor| monitor.help),
            "workers":crate::worker::display(),
        })
    }

    pub(super) fn worker_frame(&mut self, snapshot: Option<&Snapshot>) -> Frame {
        let g = self.geometry;
        let height = usize::from(g.rows);
        if let Some(monitor) = self.worker.as_mut() {
            monitor.coordination_row = 0;
            monitor.coordination_rows = 0;
        }
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
        if let Some(notice) = &self.notice {
            rows[1] = line(self, &[(PanelTone::Warning, notice.clone())]);
        }
        if let Some(snapshot) = snapshot {
            let counts = [
                (PanelTone::Success, format!("○ Idle {}", snapshot.idle)),
                (
                    PanelTone::Warning,
                    format!("● Working {}", snapshot.working),
                ),
                (
                    if snapshot.offline == 0 {
                        PanelTone::Muted
                    } else {
                        PanelTone::Error
                    },
                    format!("× Offline {}", snapshot.offline),
                ),
                (
                    if snapshot.retiring == 0 {
                        PanelTone::Muted
                    } else {
                        PanelTone::Warning
                    },
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
            let coordination = serde_json::to_value(snapshot)
                .ok()
                .map(|pool| {
                    super::worker_operations::compact(
                        &pool,
                        g.cols,
                        height.saturating_sub(rows.len() + 10).min(1),
                    )
                })
                .unwrap_or_default();
            if let Some(monitor) = self.worker.as_mut() {
                monitor.coordination_row = rows.len();
                monitor.coordination_rows = coordination.len();
            }
            rows.extend(
                coordination
                    .into_iter()
                    .map(|(text, tone)| line(self, &[(tone, text)])),
            );
            // Machine identifiers belong to diagnostics. T still exposes the
            // complete identity alongside the full original task.
            let identity = if self.worker.as_ref().is_some_and(|monitor| {
                monitor.generation.as_deref() == Some(snapshot.generation.as_str())
                    && !monitor.help
                    && monitor.selected_slot().is_some_and(|slot| {
                        monitor.showing_details(slot) && !monitor.task_views.contains_key(&slot)
                    })
            }) {
                compact_identity(snapshot, g.cols)
            } else {
                Vec::new()
            };
            let space = height.saturating_sub(rows.len() + 2);
            let panel_reserve = if g.cols >= 96 { 14 } else { 8 };
            let identity: Vec<_> = identity
                .into_iter()
                .take(space.saturating_sub(panel_reserve))
                .collect();
            rows.extend(
                identity
                    .into_iter()
                    .map(|text| line(self, &[(PanelTone::Muted, text)])),
            );
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
            if self.worker.as_ref().is_some_and(|monitor| monitor.help) {
                rows.extend(
                    monitor_help(usize::from(g.cols))
                        .into_iter()
                        .map(|row| line(self, &[row])),
                );
            }
        }
        rows.truncate(height.saturating_sub(1));
        rows.resize_with(height.saturating_sub(1), String::new);
        let details = self.worker.as_ref().is_some_and(|monitor| {
            monitor
                .selected_slot()
                .is_some_and(|slot| monitor.showing_details(slot))
        });
        let help = self.worker.as_ref().is_some_and(|monitor| monitor.help);
        let footer = if g.cols < 28 {
            "^R Reload ^C Stop"
        } else if help {
            "F1/Esc Back · ^R Reload · ^C Stop"
        } else if height == 1 {
            "Workers · ^R Reload · ^C Stop"
        } else if g.cols < 48 {
            "F2 Logs · F1 Help · ^R Reload · ^C"
        } else if g.cols < 80 {
            if details {
                "F2 Back · Enter Cmd · F1 Help · ^R Reload · ^C"
            } else {
                "Tab Worker · F2 Logs · F1 Help · ^R Reload · ^C"
            }
        } else if g.cols < 104 {
            if details {
                "Tab Worker · F2 Progress · Enter Cmd · T Task · F1 Help · ^R Reload · ^C"
            } else {
                "Tab/Shift+Tab Worker · F2 Logs & metrics · T Task · F1 Help · ^R Reload · ^C"
            }
        } else {
            if details {
                "Tab/Shift+Tab ←→ Worker · F2 Progress · Enter Cmd · T Task · End Live · F1 Help · ^R Reload · ^C Stop"
            } else {
                "Tab/Shift+Tab ←→ Worker · F2 Logs & metrics · T Task · ↑↓ PgUp/PgDn · End Live · F1 Help · ^R Reload · ^C"
            }
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

fn compact_identity(snapshot: &Snapshot, width: u16) -> Vec<String> {
    let manager = if snapshot.manager.is_some() {
        "Manager attached"
    } else {
        "Manager unbound"
    };
    let text = format!("OS PID {} · {manager} · T Details", snapshot.process_id);
    if text.width() <= usize::from(width) {
        vec![text]
    } else {
        vec![
            clipped(
                &format!("OS PID {}", snapshot.process_id),
                usize::from(width),
            ),
            clipped(manager, usize::from(width)),
        ]
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

type LogLine = (LogAnchor, Vec<(PanelTone, String)>);

fn monitor_help(width: usize) -> Vec<(PanelTone, String)> {
    [
        "Worker monitor · F1 / Esc Back",
        "Tab / Shift+Tab / ← →   Choose worker",
        "↑ ↓ / PgUp PgDn   Scroll this view",
        "Home / End   Oldest / follow latest",
        "F2   Progress / logs and client metrics",
        "Enter   Open logs / expand command text",
        "T   Full task / back",
        "Ctrl+R Reload · Ctrl+C Stop",
    ]
    .into_iter()
    .enumerate()
    .map(|(index, text)| {
        (
            if index == 0 {
                PanelTone::Heading
            } else {
                PanelTone::Normal
            },
            clipped(text, width),
        )
    })
    .collect()
}

fn progress_summary(slot: &SlotView, width: usize, budget: usize) -> Vec<(PanelTone, String)> {
    if budget == 0 {
        return Vec::new();
    }
    let task = slot.task_id.as_ref().and(slot.task_summary.as_deref());
    let mut rows = activity_field(
        "Task",
        task,
        if slot.task_id.is_some() {
            "Assigned · T Full task"
        } else {
            "No active task"
        },
        width,
        budget.saturating_sub(1).max(1),
    );
    if rows.len() < budget {
        let (tone, _, state) = slot_state(slot);
        let elapsed = slot
            .current_task_elapsed_ms
            .map(|ms| format!(" · {}", duration(ms)))
            .unwrap_or_default();
        let reason = slot
            .reason
            .as_deref()
            .map(|reason| format!(" · {}", panel::safe_text(reason)))
            .unwrap_or_default();
        let retiring = if slot.retire_requested {
            " · retirement requested"
        } else {
            ""
        };
        rows.push((
            tone,
            clipped(&format!("{state}{elapsed}{retiring}{reason}"), width),
        ));
    }
    rows
}

fn task_lines(slot: &SlotView, snapshot: &Snapshot, width: usize) -> Vec<LogLine> {
    let width = width.max(1);
    let mut metadata = vec![format!(
        "Task ID {}",
        slot.task_id.as_deref().unwrap_or("unassigned")
    )];
    metadata.extend(identity_lines(
        snapshot,
        width.min(u16::MAX as usize) as u16,
    ));
    let mut output = Vec::new();
    for text in metadata {
        for text in wrap_activity(&panel::safe_text(&text), width, usize::MAX) {
            output.push((
                LogAnchor {
                    sequence: 0,
                    part: output.len(),
                },
                vec![(PanelTone::Muted, text)],
            ));
        }
    }
    output.push((
        LogAnchor {
            sequence: 0,
            part: output.len(),
        },
        vec![(PanelTone::Muted, String::new())],
    ));
    let text = slot
        .task_text
        .as_deref()
        .or(slot.task_summary.as_deref())
        .unwrap_or("No task assigned");
    for paragraph in text.split('\n') {
        for text in wrap_activity(&panel::safe_text(paragraph), width, usize::MAX) {
            output.push((
                LogAnchor {
                    sequence: 0,
                    part: output.len(),
                },
                vec![(PanelTone::Normal, text)],
            ));
        }
    }
    output
}

fn command_summary(command: &str) -> String {
    let text = panel::safe_text(command);
    let mut input = text.trim().strip_prefix('[').unwrap_or("");
    let mut args = Vec::new();
    for _ in 0..5 {
        let mut values =
            serde_json::Deserializer::from_str(input.trim_start()).into_iter::<String>();
        let Some(Ok(argument)) = values.next() else {
            break;
        };
        let consumed = values.byte_offset();
        args.push(argument);
        let program = args.first().map(String::as_str).unwrap_or("");
        let program = std::path::Path::new(program)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(program);
        let interpreter = program.starts_with("python")
            || matches!(
                program,
                "sh" | "bash" | "zsh" | "node" | "nodejs" | "ruby" | "perl"
            );
        if interpreter
            && args
                .last()
                .is_some_and(|arg| matches!(arg.as_str(), "-c" | "-e" | "--eval"))
        {
            args.push("[inline script]".into());
            break;
        }
        let rest = input.trim_start()[consumed..].trim_start();
        let Some(rest) = rest.strip_prefix(',') else {
            break;
        };
        input = rest;
    }
    if args.is_empty() {
        clipped(&text, 88)
    } else {
        clipped(&args.join(" "), 88)
    }
}

fn command_log_summary(text: &str) -> String {
    for prefix in ["Command started: ", "Command finished: "] {
        if let Some(command) = text.strip_prefix(prefix) {
            let (command, outcome) = (prefix == "Command finished: ")
                .then(|| command.rsplit_once("; "))
                .flatten()
                .filter(|(_, outcome)| {
                    outcome
                        .strip_prefix("exit ")
                        .or_else(|| outcome.strip_prefix("signal "))
                        .is_some_and(|code| code.parse::<i32>().is_ok())
                        || matches!(*outcome, "cancelled" | "timed out")
                })
                .map_or((command, None), |(command, outcome)| {
                    (command, Some(outcome))
                });
            return format!(
                "{prefix}{}{}",
                command_summary(command),
                outcome.map_or_else(String::new, |outcome| format!("; {outcome}"))
            );
        }
    }
    text.to_owned()
}

fn log_event(entry: &crate::worker::SlotLog, expanded: bool) -> (&'static str, PanelTone, String) {
    let text = if expanded {
        entry.text.clone()
    } else {
        command_log_summary(&entry.text)
    };
    let (tag, tone, body) = if let Some(command) = text.strip_prefix("Command started: ") {
        ("RUN", PanelTone::Accent, command)
    } else if let Some(command) = text.strip_prefix("Command finished: ") {
        // Only an explicit result suffix is evidence of success. An inline
        // script containing "exit 0" or a missing outcome never becomes DONE.
        let outcome = command.rsplit_once("; ").map(|(_, outcome)| outcome);
        let (tag, tone) = match outcome {
            Some("exit 0") => ("DONE", PanelTone::Success),
            Some("cancelled") => ("STOP", PanelTone::Warning),
            Some("timed out") => ("FAIL", PanelTone::Error),
            Some(outcome)
                if outcome
                    .strip_prefix("exit ")
                    .or_else(|| outcome.strip_prefix("signal "))
                    .is_some_and(|code| code.parse::<i32>().is_ok()) =>
            {
                ("FAIL", PanelTone::Error)
            }
            _ => ("END", PanelTone::Muted),
        };
        (tag, tone, command)
    } else if let Some(progress) = text.strip_prefix("Progress: ") {
        ("NOTE", PanelTone::Accent, progress)
    } else if text.starts_with("Task ") {
        ("TASK", PanelTone::Accent, text.as_str())
    } else {
        ("INFO", PanelTone::Muted, text.as_str())
    };
    let (tag, tone) = match entry.level.as_str() {
        "error" => ("FAIL", PanelTone::Error),
        "warning" | "warn" => ("WARN", PanelTone::Warning),
        _ => (tag, tone),
    };
    (tag, tone, panel::safe_text(body))
}

fn wrap_log_body(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rest = text;
    let mut lines = Vec::new();
    while !rest.is_empty() {
        let mut used = 0;
        let mut end = 0;
        let mut word_end = 0;
        for (index, character) in rest.char_indices() {
            let cells = character.width().unwrap_or(0);
            if used + cells > width {
                break;
            }
            used += cells;
            end = index + character.len_utf8();
            if character.is_whitespace() {
                word_end = end;
            }
        }
        if end == 0 {
            // A full-width glyph cannot fit a one-cell viewport.
            lines.push("…".into());
            rest = &rest[rest.chars().next().unwrap().len_utf8()..];
            continue;
        }
        if end < rest.len() && word_end > 0 {
            end = word_end;
        }
        lines.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn event_lines(
    entry: &crate::worker::SlotLog,
    width: usize,
    tag: &str,
    tone: PanelTone,
    body: &str,
) -> Vec<LogLine> {
    let width = width.max(1);
    let mut output = Vec::new();
    let seconds = entry.at_ms / 1000;
    let time = if width >= 40 {
        format!(
            "{:02}:{:02}:{:02}  ",
            seconds / 3600 % 24,
            seconds / 60 % 60,
            seconds % 60
        )
    } else if width >= 28 {
        format!("{:02}:{:02}  ", seconds / 60 % 60, seconds % 60)
    } else {
        String::new()
    };
    let label = if width >= 10 && !tag.is_empty() {
        format!("{tag:<4}  ")
    } else {
        String::new()
    };
    let indent = time.width() + label.width();
    let body_tone = match tone {
        PanelTone::Error | PanelTone::Warning => tone,
        _ => PanelTone::Normal,
    };
    for (part, text) in wrap_log_body(body, width.saturating_sub(indent))
        .into_iter()
        .enumerate()
    {
        let parts = if part == 0 {
            vec![
                (PanelTone::Muted, time.clone()),
                (tone, label.clone()),
                (body_tone, text),
            ]
        } else {
            vec![(PanelTone::Muted, " ".repeat(indent)), (body_tone, text)]
        };
        output.push((
            LogAnchor {
                sequence: entry.sequence,
                part,
            },
            parts,
        ));
    }
    output
}

fn log_lines(slot: &SlotView, width: usize, expanded: bool) -> Vec<LogLine> {
    let mut output = Vec::new();
    for (index, entry) in slot.logs.iter().enumerate() {
        let (tag, tone, body) = log_event(entry, expanded);
        let lines = event_lines(entry, width, tag, tone, &body);
        let count = lines.len();
        output.extend(lines);
        // Breathing room belongs to this event's stable sequence. Never merge
        // start/end records: commands from the same worker may overlap.
        if width >= 48
            && index + 1 < slot.logs.len()
            && (count > 1 || matches!(tag, "TASK" | "FAIL" | "WARN"))
        {
            output.push((
                LogAnchor {
                    sequence: entry.sequence,
                    part: count,
                },
                vec![(PanelTone::Muted, String::new())],
            ));
        }
    }
    output
}

fn progress_lines(slot: &SlotView, width: usize) -> Vec<LogLine> {
    // These prefixes are written by queue::telemetry only after the exact task
    // claim is validated. Command text is always nested under a distinct
    // "Command started/finished: " prefix, never interpreted as agent progress.
    let reported = |entry: &&crate::worker::SlotLog| {
        entry.level == "info"
            && entry.task_id.is_some()
            && slot
                .task_id
                .as_ref()
                .is_none_or(|task| entry.task_id.as_ref() == Some(task))
            && entry.text.starts_with("Progress: ")
    };
    let has_progress = slot.logs.iter().any(|entry| reported(&entry));
    let latest_retained = slot.progress.as_deref().is_some_and(|latest| {
        slot.logs
            .iter()
            .filter(reported)
            .any(|entry| entry.text.strip_prefix("Progress: ") == Some(latest))
    });
    let mut output = Vec::new();
    // The latest reported text survives ordinary log retention. It has no
    // retained event timestamp, so show it explicitly without inventing one.
    let fallback = if !latest_retained {
        slot.progress
            .as_deref()
            .map(|text| format!("Latest report · {}", panel::safe_text(text)))
    } else {
        None
    };
    let intro = fallback
        .as_deref()
        .or_else(|| (!has_progress).then_some("Waiting for agent progress updates"));
    if let Some(intro) = intro {
        let tone = if fallback.is_some() {
            PanelTone::Normal
        } else {
            PanelTone::Muted
        };
        for (part, text) in wrap_log_body(intro, width).into_iter().enumerate() {
            output.push((LogAnchor { sequence: 0, part }, vec![(tone, text)]));
        }
    }
    for entry in &slot.logs {
        let event = if reported(&entry) {
            Some((
                "",
                PanelTone::Normal,
                panel::safe_text(entry.text.strip_prefix("Progress: ").unwrap_or_default()),
            ))
        } else {
            let (tag, tone, body) = log_event(entry, false);
            if !matches!(tone, PanelTone::Warning | PanelTone::Error) {
                None
            } else if entry.text.starts_with("Command started: ")
                || entry.text.starts_with("Command finished: ")
            {
                let result = entry
                    .text
                    .strip_prefix("Command finished: ")
                    .and_then(|text| text.rsplit_once("; "))
                    .map(|(_, result)| result)
                    .filter(|result| {
                        matches!(*result, "cancelled" | "timed out")
                            || result
                                .strip_prefix("exit ")
                                .or_else(|| result.strip_prefix("signal "))
                                .is_some_and(|code| code.parse::<i32>().is_ok())
                    });
                Some((
                    tag,
                    tone,
                    result.map_or_else(
                        || "Command alert · F2 Logs".into(),
                        |result| format!("Command {result} · F2 Logs"),
                    ),
                ))
            } else {
                Some((tag, tone, body))
            }
        };
        let Some((tag, tone, mut body)) = event else {
            continue;
        };
        if slot.task_id.is_some() && entry.task_id.is_some() && entry.task_id != slot.task_id {
            body = format!("Earlier task · {body}");
        }
        let lines = event_lines(entry, width, tag, tone, &body);
        let count = lines.len();
        output.extend(lines);
        output.push((
            LogAnchor {
                sequence: entry.sequence,
                part: count,
            },
            vec![(PanelTone::Muted, String::new())],
        ));
    }
    if output
        .last()
        .is_some_and(|(_, parts)| parts.len() == 1 && parts[0].1.is_empty())
    {
        output.pop();
    }
    output
}

fn slot_state(slot: &SlotView) -> (PanelTone, &'static str, &'static str) {
    match slot.contact {
        Some("stale_contact") => return (PanelTone::Warning, "?", "stale contact"),
        Some("unverified") => return (PanelTone::Warning, "?", "unverified"),
        Some("declared_offline") => return (PanelTone::Muted, "◌", "off duty"),
        Some("membership_only") if slot.state == "working" => {
            return (PanelTone::Muted, "◌", "quiet claim");
        }
        Some(_) if slot.state == "working" => return (PanelTone::Warning, "●", "claimed"),
        _ => {}
    }
    match slot.state {
        "idle" => (PanelTone::Success, "○", "idle"),
        "working" => (PanelTone::Warning, "●", "working"),
        "offline" => (PanelTone::Error, "×", "offline"),
        "retiring" => (PanelTone::Warning, "◌", "retiring"),
        _ => (PanelTone::Muted, "?", "unknown"),
    }
}

fn columns(
    left: Vec<(PanelTone, String)>,
    right: Option<(PanelTone, String)>,
    left_cols: usize,
    gap: usize,
) -> Vec<(PanelTone, String)> {
    let mut used = 0;
    let mut parts = Vec::new();
    for (tone, text) in left {
        let text = clipped(&text, left_cols.saturating_sub(used));
        used += text.width();
        parts.push((tone, text));
    }
    parts.push((
        PanelTone::Muted,
        " ".repeat(left_cols.saturating_sub(used) + gap),
    ));
    if let Some(right) = right {
        parts.push(right);
    }
    parts
}

fn compact_details(
    resources: &metrics::Sampler,
    width: usize,
    budget: usize,
) -> Vec<(PanelTone, String)> {
    if budget == 0 {
        return Vec::new();
    }
    let mut rows = vec![detail(
        "Client CPU",
        &format!("{} of machine", cpu_text(resources.latest())),
        width,
    )];
    rows.push(detail(
        "Client RSS",
        &resources
            .latest()
            .and_then(|sample| sample.memory_bytes)
            .map(memory)
            .unwrap_or_else(|| "Unavailable".into()),
        width,
    ));
    if budget >= 8 {
        rows.push((
            PanelTone::Muted,
            clipped("SSMCP process · agents not sampled", width),
        ));
        rows.push((PanelTone::Normal, String::new()));
        let samples = resources.samples();
        for (cpu, label) in [(true, "CPU"), (false, "RSS")] {
            let value: fn(&metrics::Sample) -> Option<f64> = if cpu {
                |sample| sample.cpu_percent
            } else {
                |sample| {
                    sample
                        .memory_bytes
                        .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
                }
            };
            if samples.iter().any(|sample| value(sample).is_some()) {
                let (plot, upper) = resource_chart(samples, width, 1, value);
                rows.push((
                    PanelTone::Muted,
                    clipped(
                        &format!(
                            "{label} trend · 0–{upper:.1}{}",
                            if cpu { "% of machine" } else { " MiB" }
                        ),
                        width,
                    ),
                ));
                rows.extend(plot.into_iter().map(|line| (PanelTone::Muted, line)));
            } else {
                rows.push((
                    PanelTone::Muted,
                    clipped(&format!("{label} trend · samples unavailable"), width),
                ));
                rows.push((PanelTone::Normal, String::new()));
            }
        }
    }
    rows.truncate(budget);
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
    let extra = budget.saturating_sub(20);
    let mut rows = activity_field(
        "Task",
        activity.task,
        "No task assigned",
        width,
        (2 + extra / 16).min(3),
    );
    rows.resize_with(rows.len().max(2), || (PanelTone::Normal, String::new()));
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
        .map(|elapsed| duration(*elapsed))
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
    if budget >= 24 && activity.progress.is_some() {
        rows.extend(activity_field(
            "Progress",
            activity.progress,
            "",
            width,
            (extra / 6).clamp(2, 3),
        ));
    }
    if budget >= 28
        && let Some(command) = activity.command
    {
        let summary = command_summary(command);
        rows.extend(activity_field("Command", Some(&summary), "", width, 2));
    }
    rows.push((PanelTone::Normal, String::new()));
    rows.push((PanelTone::Heading, clipped("Shared SSMCP client", width)));
    rows.push((
        PanelTone::Muted,
        clipped(&format!("PID {process_id} · process sample"), width),
    ));
    rows.push((
        PanelTone::Muted,
        clipped("External agents are not sampled", width),
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
    let samples = resources.samples();
    let valid = samples
        .iter()
        .filter(|sample| sample.cpu_percent.is_some())
        .count();
    let remaining = budget.saturating_sub(rows.len());
    let memory_rows = if remaining >= 12 {
        (remaining / 2).min(9)
    } else {
        0
    };
    let chart_rows = remaining.saturating_sub(memory_rows + 2).min(3);
    rows.push((
        PanelTone::Muted,
        clipped(&format!("Client CPU trend · {valid} samples"), width),
    ));
    if valid > 0 && chart_rows > 0 {
        let (plot, upper) = resource_chart(samples, width, chart_rows, |sample| sample.cpu_percent);
        rows.extend(plot.into_iter().map(|line| (PanelTone::Muted, line)));
        rows.push((
            PanelTone::Muted,
            clipped(&format!("Scale 0–{upper:.1}% of machine"), width),
        ));
    } else {
        rows.push((
            PanelTone::Muted,
            clipped(
                if valid > 0 {
                    "Enlarge to view the CPU chart"
                } else if resources.latest().is_some_and(|sample| sample.cpu_pending) {
                    "Waiting for the next CPU sample"
                } else {
                    "CPU samples unavailable"
                },
                width,
            ),
        ));
    }
    let remaining = budget.saturating_sub(rows.len());
    if remaining >= 4 {
        rows.push((PanelTone::Normal, String::new()));
        let valid = samples
            .iter()
            .filter(|sample| sample.memory_bytes.is_some())
            .count();
        rows.push((
            PanelTone::Muted,
            clipped(&format!("Client RSS trend · {valid} samples"), width),
        ));
        if valid > 0 {
            let (plot, upper) = resource_chart(
                samples,
                width,
                remaining.saturating_sub(3).min(3),
                |sample| {
                    sample
                        .memory_bytes
                        .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
                },
            );
            rows.extend(plot.into_iter().map(|line| (PanelTone::Muted, line)));
            rows.push((
                PanelTone::Muted,
                clipped(&format!("Scale 0–{upper:.1} MiB"), width),
            ));
        } else {
            rows.push((PanelTone::Muted, clipped("RSS samples unavailable", width)));
        }
    }
    rows.truncate(budget);
    rows
}

fn detail(label: &str, value: &str, width: usize) -> (PanelTone, String) {
    (
        PanelTone::Normal,
        clipped(&format!("{label:<9} {value}"), width),
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

fn resource_chart(
    samples: &VecDeque<metrics::Sample>,
    cols: usize,
    rows: usize,
    measure: fn(&metrics::Sample) -> Option<f64>,
) -> (Vec<String>, f64) {
    if cols == 0 || rows == 0 {
        return (Vec::new(), 0.1);
    }
    let upper = (samples
        .iter()
        .filter_map(measure)
        .filter(|value| value.is_finite() && *value >= 0.0)
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
        let Some(value) = measure(sample).filter(|value| value.is_finite() && *value >= 0.0) else {
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

    #[test]
    fn remote_tabs_distinguish_a_claim_from_contact_and_confirmed_departure() {
        let mut slot = snapshot().slots.remove(1);
        slot.state = "working";
        for (contact, expected) in [
            ("confirmed", "claimed"),
            ("membership_only", "quiet claim"),
            ("stale_contact", "stale contact"),
            ("unverified", "unverified"),
            ("declared_offline", "off duty"),
        ] {
            slot.contact = Some(contact);
            assert_eq!(slot_state(&slot).2, expected);
        }
        slot.contact = None;
        slot.state = "offline";
        assert_eq!(slot_state(&slot).2, "offline");
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            process_id: std::process::id(),
            generation: "fixture".into(),
            manager: None,
            manager_operations: Vec::new(),
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
                    contact: None,
                    retire_requested: false,
                    task_id: None,
                    task_summary: None,
                    task_text: None,
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
                    contact: None,
                    retire_requested: false,
                    task_id: Some("task-actual".into()),
                    task_summary: None,
                    task_text: None,
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
                    contact: None,
                    retire_requested: false,
                    task_id: None,
                    task_summary: None,
                    task_text: None,
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
                assert!(text.contains("Ctrl+R Reload") || text.contains("^R Reload"));
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
    fn worker_manager_identity_and_metrics_are_in_details_not_default_progress() {
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
        let mut wide = state(44, 100, false);
        let text = screen(&wide.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains(&client));
        assert!(!text.contains("Manager attached"));
        assert!(!text.contains("OS PID 31415"));
        assert!(!text.contains("CPU trend"));
        wide.keyboard(b"t", Instant::now(), false);
        let detail = screen(&wide.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(detail.contains(&client) && detail.contains(thread));
        wide.keyboard(b"t", Instant::now(), false);
        snapshot.stopping = true;
        let text = screen(&wide.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(text.contains("Working 1"));
        assert!(text.contains("waiting for worker confirmation"));
        let mut narrow = state(26, 18, false);
        let text = screen(&narrow.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains(&client[..16]));
        assert!(!text.contains("Manager attached"));
        assert!(text.contains("Working 1"));

        let mut compact = state(30, 100, false);
        let text = screen(&compact.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains("Manager attached"));
        assert!(!text.contains("CPU trend"));
        assert!(text.contains("Recent agent progress"));
        compact.keyboard(b"\x1bOQ", Instant::now(), false);
        let text = screen(&compact.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(text.contains("Manager attached"));
        assert!(text.contains("OS PID 31415"));
        assert!(text.contains("Activity · T Full task"));
        assert!(text.contains("CPU trend"));
        let panels = compact.worker_snapshot(true);
        assert_eq!(panels["worker_panels"][0]["details"]["visible"], true);
        assert_eq!(panels["worker_panels"][0]["logs"]["visible"], true);

        let mut tiny = state(18, 18, false);
        let text = screen(&tiny.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains(&client[..16]));
        assert!(text.contains("Working 1"));
        assert_eq!(
            tiny.worker_snapshot(true)["worker_panels"][0]["logs"]["visible"],
            true
        );
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
            contact: None,
            retire_requested: false,
            task_id: None,
            task_summary: None,
            task_text: None,
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
        state.keyboard(b"\x1bOQ", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        state.keyboard(b"\x1b[A", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let first_anchor = state.worker.as_ref().unwrap().anchors[&0].unwrap();
        state.keyboard(b"\x1b[C\x1bOQ\x1b[A\x1b[A", Instant::now(), false);
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
        state.keyboard(b"\x1bOQ", Instant::now(), false);
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
        state.keyboard(b"\x1bOQ", Instant::now(), false);
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
        state.worker_frame(Some(&snapshot));
        state.keyboard(b"\x1bOQ", Instant::now(), false);
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
        assert!(text.contains("External agents are not sampled"));
        assert!(text.contains("Shared SSMCP client"));
    }

    fn log_body(lines: &[LogLine]) -> String {
        lines
            .iter()
            .filter_map(|(_, parts)| parts.last())
            .map(|(_, text)| text.as_str())
            .collect()
    }

    #[test]
    fn progress_projection_only_promotes_claimed_reports_and_preserves_alerts() {
        let mut snapshot = snapshot();
        let slot = &mut snapshot.slots[1];
        let records = [
            ("info", Some("task-actual"), "Progress: verified progress"),
            ("info", Some("task-previous"), "Progress: OLD_PROGRESS"),
            ("info", None, "Progress: UNATTRIBUTED_PROGRESS"),
            (
                "info",
                Some("task-actual"),
                "Command started: Progress: COMMAND_BODY",
            ),
            (
                "info",
                Some("task-actual"),
                "Command finished: RAW_FAILED_COMMAND; exit 7",
            ),
            (
                "warning",
                Some("task-actual"),
                "Cancellation requested; awaiting completion acknowledgement",
            ),
            (
                "warning",
                Some("task-previous"),
                "Remote agent report: unknown; output 0 bytes",
            ),
        ];
        slot.logs = records
            .into_iter()
            .enumerate()
            .map(|(index, (level, task_id, text))| SlotLog {
                level: level.into(),
                task_id: task_id.map(str::to_owned),
                text: text.into(),
                ..log(index as u64 + 1)
            })
            .collect();
        slot.progress = Some("verified progress".into());
        for width in [1, 12, 46, 130] {
            let lines = progress_lines(slot, width);
            assert!(lines.iter().all(|(_, parts)| {
                parts.iter().map(|(_, text)| text.width()).sum::<usize>() <= width
            }));
            if width > 1 {
                let text = log_body(&lines);
                for visible in [
                    "verified progress",
                    "Command exit 7",
                    "Cancellation requested",
                    "Earlier task",
                    "unknown",
                ] {
                    assert!(text.contains(visible), "{width}: {text}");
                }
                for hidden in [
                    "OLD_PROGRESS",
                    "UNATTRIBUTED_PROGRESS",
                    "COMMAND_BODY",
                    "RAW_FAILED_COMMAND",
                    "Latest report",
                ] {
                    assert!(!text.contains(hidden), "{width}: {text}");
                }
            }
        }
        slot.logs.clear();
        let lines = progress_lines(slot, 46);
        assert_eq!(log_body(&lines), "Latest report · verified progress");
        assert!(
            lines.iter().all(|(_, parts)| parts.len() == 1),
            "retained text has no invented timestamp"
        );
        slot.progress = None;
        slot.logs.push(SlotLog {
            text: "Command started: COMMAND_ONLY".into(),
            ..log(20)
        });
        assert_eq!(
            log_body(&progress_lines(slot, 46)),
            "Waiting for agent progress updates"
        );
    }

    #[test]
    fn progress_is_full_width_and_real_task_context_survives_small_frames() {
        for (rows, cols) in [(18, 48), (28, 48), (32, 132)] {
            for color in [false, true] {
                let mut state = state(rows, cols, color);
                let mut snapshot = snapshot();
                let slot = &mut snapshot.slots[0];
                slot.state = "working";
                slot.task_id = Some("current-task".into());
                slot.task_summary = Some("核对真实进度".into());
                slot.progress = Some("已完成第一项检查".into());
                slot.logs = vec![
                    SlotLog {
                        text: "Progress: 已完成第一项检查".into(),
                        task_id: slot.task_id.clone(),
                        ..log(1)
                    },
                    SlotLog {
                        text: "Command started: RAW_COMMAND_DETAIL".into(),
                        task_id: slot.task_id.clone(),
                        ..log(2)
                    },
                ];
                let frame = state.worker_frame(Some(&snapshot));
                let text = screen(&frame).screen().contents();
                assert!(
                    text.contains("核对真实进度") && text.contains("已完成第一项检查"),
                    "{rows}x{cols}: {text}"
                );
                for hidden in ["RAW_COMMAND_DETAIL", "OS PID", "CPU trend", "Work log"] {
                    assert!(!text.contains(hidden), "{rows}x{cols}: {text}");
                }
                let view = state.worker_snapshot(true);
                assert_eq!(view["worker_panels"][0]["view"], "progress");
                assert_eq!(view["worker_panels"][0]["logs"]["cols"], cols);
                state.keyboard(b"\x1bOQ", Instant::now(), false);
                let text = screen(&state.worker_frame(Some(&snapshot)))
                    .screen()
                    .contents();
                assert!(text.contains("RAW_COMMAND_DETAIL"), "{rows}x{cols}: {text}");
                assert_eq!(
                    state.worker_snapshot(true)["worker_panels"][0]["view"],
                    "details"
                );
                state.keyboard(b"\x1bOQ", Instant::now(), false);
                snapshot.slots[0].task_id = None;
                snapshot.slots[0].state = "idle";
                let text = screen(&state.worker_frame(Some(&snapshot)))
                    .screen()
                    .contents();
                assert!(
                    text.contains("No active task") && text.contains("Recent agent progress"),
                    "{text}"
                );
                assert!(
                    !text.contains("核对真实进度"),
                    "a retained task summary is not a current assignment"
                );
                assert!(state.actions.is_empty());
            }
        }
    }

    #[test]
    fn progress_and_diagnostic_anchors_are_independent_across_task_changes() {
        let mut state = state(24, 100, false);
        let mut snapshot = populated();
        let slot = &mut snapshot.slots[0];
        slot.state = "working";
        slot.task_id = Some("task-progress".into());
        slot.logs = (1..40)
            .map(|sequence| SlotLog {
                task_id: Some("task-progress".into()),
                text: if sequence % 2 == 0 {
                    format!("Progress: update-{sequence}")
                } else {
                    format!("Command started: command-{sequence}")
                },
                ..log(sequence)
            })
            .collect();
        state.worker_frame(Some(&snapshot));
        state.keyboard(b"\x1b[H", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let progress = state.worker.as_ref().unwrap().anchors[&0];
        state.keyboard(b"\x1bOQ\x1b[5~", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        let logs = state.worker.as_ref().unwrap().anchors[&0];
        assert_ne!(logs, progress);
        state.keyboard(b"\x1bOQ", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        assert_eq!(state.worker.as_ref().unwrap().anchors[&0], progress);
        state.keyboard(b"\t", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        assert_eq!(
            state.worker_snapshot(true)["worker_panels"][0]["view"],
            "progress"
        );
        state.keyboard(b"\x1b[Z\x1bOQ", Instant::now(), false);
        state.worker_frame(Some(&snapshot));
        assert_eq!(state.worker.as_ref().unwrap().anchors[&0], logs);
        snapshot.slots[0].task_id = Some("next-task".into());
        snapshot.slots[0].progress = None;
        state.worker_frame(Some(&snapshot));
        assert_eq!(state.worker.as_ref().unwrap().anchors[&0], logs);
        state.keyboard(b"\x1bOQ", Instant::now(), false);
        let text = screen(&state.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(text.contains("Waiting for agent progress updates"));
        assert!(!text.contains("update-"));
        assert!(state.worker.as_ref().unwrap().panels[0].following);
    }

    #[test]
    fn monitor_help_and_detail_keys_never_execute_inside_split_pastes() {
        let bytes = b"\x1b[200~\x1bOP\x1bOQ\r\tt\x1b[201~";
        for split in 0..=bytes.len() {
            let mut state = state(18, 48, false);
            state.worker_frame(Some(&snapshot()));
            state.keyboard(&bytes[..split], Instant::now(), false);
            state.keyboard(&bytes[split..], Instant::now(), false);
            assert!(!state.worker.as_ref().unwrap().help);
            assert!(!state.worker.as_ref().unwrap().showing_details(0));
            assert!(state.worker.as_ref().unwrap().task_views.is_empty());
            assert_eq!(state.worker.as_ref().unwrap().selected_slot(), Some(0));
            state.keyboard(b"\x1bOP", Instant::now(), false);
            let text = screen(&state.worker_frame(Some(&snapshot())))
                .screen()
                .contents();
            assert!(
                text.contains("F1 / Esc Back") && text.contains("F2"),
                "{text}"
            );
            assert!(text.contains("Ctrl+R Reload") && text.contains("Ctrl+C Stop"));
            state.keyboard(b"\x1bOP", Instant::now(), false);
            assert!(!state.worker.as_ref().unwrap().help);
            assert!(state.actions.is_empty());
            assert!(state.conversation.composer.text().is_empty());
        }
    }

    #[test]
    fn worker_log_wrapping_preserves_text_and_sanitizes_controls_and_secrets() {
        let mut snapshot = populated();
        snapshot.slots[0].logs = vec![SlotLog {
            text: "测试 long entry with a visible ending END password=fixture-secret\x1b[2J\r\n"
                .into(),
            ..log(7)
        }];
        let lines = log_lines(&snapshot.slots[0], 20, false);
        assert!(lines.len() > 1);
        assert!(
            lines.iter().all(|(_, parts)| parts
                .iter()
                .map(|(_, text)| text.width())
                .sum::<usize>()
                <= 20)
        );
        let text = log_body(&lines);
        assert!(text.contains("visible ending END"));
        assert!(text.contains("[redacted]"));
        assert!(!text.contains("fixture-secret"));
        assert!(!text.contains('\x1b'));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
        assert!(lines.iter().all(|(key, _)| key.sequence == 7));
    }

    #[test]
    fn worker_log_columns_preserve_interleaved_commands_results_and_unicode() {
        let mut snapshot = populated();
        let records = [
            ("info", r#"Command started: ["sh","-c","echo ok; exit 0"]"#),
            ("info", r#"Command started: ["cargo","test","--lib"]"#),
            (
                "error",
                r#"Command finished: ["cargo","test","--lib"]; exit 7"#,
            ),
            ("info", r#"Command finished: ["sh","-c","echo ok; exit 0"]"#),
            (
                "info",
                r#"Command finished: ["git","diff","--check"]; exit 0"#,
            ),
            (
                "info",
                "Progress: 检查中英文长正文，保留顺序与每条日志的稳定锚点 visible END",
            ),
        ];
        snapshot.slots[0].logs = records
            .iter()
            .enumerate()
            .map(|(index, (level, text))| SlotLog {
                level: (*level).into(),
                text: (*text).into(),
                ..log(index as u64)
            })
            .collect();
        for width in [1, 8, 18, 30, 60, 100] {
            let lines = log_lines(&snapshot.slots[0], width, false);
            assert!(lines.iter().all(|(_, spans)| {
                spans.iter().map(|(_, text)| text.width()).sum::<usize>() <= width
            }));
            assert!(lines.windows(2).all(|pair| pair[0].0 < pair[1].0));
            let sequences: std::collections::BTreeSet<_> =
                lines.iter().map(|line| line.0.sequence).collect();
            assert_eq!(sequences, (0..6).collect());
            assert!(log_body(&lines).contains("exit 7"));
            if width >= 18 {
                assert!(log_body(&lines).contains("visible END"));
                assert!(
                    lines
                        .iter()
                        .filter(|(key, spans)| key.part > 0 && spans.len() > 1)
                        .all(|(_, spans)| spans[0].1.trim().is_empty())
                );
            }
        }
        assert_eq!(log_event(&snapshot.slots[0].logs[0], false).0, "RUN");
        assert_eq!(log_event(&snapshot.slots[0].logs[2], false).0, "FAIL");
        assert_eq!(log_event(&snapshot.slots[0].logs[3], false).0, "END");
        assert_eq!(log_event(&snapshot.slots[0].logs[4], false).0, "DONE");
    }

    #[test]
    fn worker_compact_metrics_keep_labels_separate_at_narrow_sizes() {
        let resources = metrics::Sampler::default();
        for (width, budget) in [(60, 8), (48, 8), (48, 2)] {
            let rows = compact_details(&resources, width, budget);
            assert_eq!(rows[0].1, "Client CPU Unavailable of machine");
            assert_eq!(rows[1].1, "Client RSS Unavailable");
            assert!(rows.len() <= budget);
            assert!(rows.iter().all(|(_, text)| text.width() <= width));
            for (label, value, expected) in [
                (
                    "Client CPU",
                    "0.1% of machine",
                    "Client CPU 0.1% of machine",
                ),
                (
                    "Client CPU",
                    "Sampling… of machine",
                    "Client CPU Sampling… of machine",
                ),
                ("Client RSS", "34.7 MiB", "Client RSS 34.7 MiB"),
                ("CPU", "0.1% of machine", "CPU       0.1% of machine"),
                ("RSS", "34.7 MiB", "RSS       34.7 MiB"),
            ] {
                assert_eq!(detail(label, value, width).1, expected);
            }
        }
        for (rows, cols) in [(26, 60), (26, 48), (18, 48)] {
            for color in [true, false] {
                let mut state = state(rows, cols, color);
                let snapshot = populated();
                state.worker_frame(Some(&snapshot));
                state.keyboard(b"\x1bOQ", Instant::now(), false);
                let text = screen(&state.worker_frame(Some(&snapshot)))
                    .screen()
                    .contents();
                assert!(text.contains("Client CPU "), "{rows}x{cols}: {text}");
                assert!(text.contains("Client RSS "), "{rows}x{cols}: {text}");
                assert!(text.contains("Work log"), "{rows}x{cols}: {text}");
            }
        }
    }

    #[test]
    fn worker_fixed_details_wrap_task_and_never_invent_resource_samples() {
        let activity = ActivitySummary {
            task: Some("Check Unicode rendering and retain the terminal ownership boundary"),
            total_work_ms: 125_000,
            current_task_elapsed_ms: Some(1_250),
            recent_task_durations_ms: &[500, 2_000, 61_000],
            ..ActivitySummary::default()
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
        assert!(text.contains("External agents are not sampled"));
        assert!(text.contains("Shared SSMCP client"));
        assert!(text.contains("PID 42 · process sample"));
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
        let (plot, upper) = resource_chart(&samples, 9, 2, |sample| sample.cpu_percent);
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
        let (connected, _) = resource_chart(&samples, 9, 2, |sample| sample.cpu_percent);
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
            resource_chart(&samples, 9, 2, |sample| sample.cpu_percent)
                .0
                .iter()
                .all(|row| row.trim().is_empty())
        );
    }

    #[test]
    fn worker_reload_queues_one_restart_and_never_edits_or_interrupts_the_composer() {
        for bytes in [b"\x12\x12".as_slice(), b"\x1b[114;5u", b"\x1b[27;5;114~"] {
            for split in 0..=bytes.len() {
                let mut state = state(20, 100, false);
                state.preview = false;
                state
                    .conversation
                    .composer
                    .set_text("preserved worker draft");
                let before = state.conversation.composer.text().to_owned();
                let now = Instant::now();
                assert!(state.keyboard(&bytes[..split], now, false).is_empty());
                assert!(state.keyboard(&bytes[split..], now, false).is_empty());
                assert_eq!(state.actions, vec!["client.reload"]);
                assert_eq!(state.conversation.composer.text(), before);
                assert_eq!(state.notice.as_deref(), Some("Requesting worker reload"));
            }
        }
        let now = Instant::now();
        let mut keyboard = super::super::Keyboard::default();
        let (_, actions) = keyboard.feed_worker(b"\x03", now, false);
        assert!(matches!(actions.as_slice(), [Key::Interrupt]));
        let (literal, actions) = keyboard.feed(b"\x12", now, false, false, false);
        assert_eq!(literal, b"\x12");
        assert!(actions.is_empty());

        let mut preview = state(20, 100, false);
        preview.keyboard(b"\x12", now, false);
        assert!(preview.actions.is_empty());
        assert!(preview.notice.as_deref().unwrap().contains("Preview"));
    }

    #[test]
    fn worker_monitor_consumes_layout_keys_mouse_and_split_pastes_without_changing_the_draft() {
        let bytes = b"\x1bOQignored\r\x1b\x1b[<0;2;3M\x1b[18~\x1b[200~pasted\x03\x12\x1b[114;5u\x1bOQ\r\x1b[201~";
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
            assert!(state.notice.is_none());
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
    #[test]
    fn worker_tall_details_use_extra_height_for_task_progress_and_commands() {
        let activity = ActivitySummary {
            task: Some(
                "Inspect the worker monitor layout, keep task ownership and finish the isolated verification",
            ),
            progress: Some(
                "The task is running in an isolated worktree; the selected worker has retained its own log position.",
            ),
            command: Some(r#"["python3","-c","print('FULL_SCRIPT_BODY')"]"#),
            current_task_elapsed_ms: Some(9_000),
            recent_task_durations_ms: &[500, 2_000, 61_000],
            ..ActivitySummary::default()
        };
        let resources = metrics::Sampler::default();
        let compact = station_details(&activity, &resources, 42, 44, 16);
        let tall = station_details(&activity, &resources, 42, 44, 48);
        let text = tall
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(tall.len() > compact.len());
        assert!(
            !compact
                .iter()
                .any(|(_, text)| { text.starts_with("Progress") || text.starts_with("Command") })
        );
        for label in [
            "Progress",
            "Command",
            "[inline script]",
            "Last 3",
            "RSS trend",
        ] {
            assert!(text.contains(label), "{text}");
        }
        assert!(!text.contains("FULL_SCRIPT_BODY"));
        assert!(text.contains("External agents are not sampled"));
        assert!(!text.contains("0.0%"));
        assert!(tall.len() <= 48 && tall.iter().all(|(_, text)| text.width() <= 44));
    }

    #[test]
    fn worker_tabs_and_command_details_preserve_other_workers_and_paste_boundaries() {
        let mut state = state(30, 100, false);
        let mut snapshot = populated();
        let command = serde_json::json!([
            "python3",
            "-c",
            format!("{} FULL_COMMAND_END", "script; ".repeat(40))
        ])
        .to_string();
        snapshot.slots[0].logs = vec![SlotLog {
            text: format!("Command finished: {command}; exit 0"),
            ..log(7)
        }];
        let compact = log_lines(&snapshot.slots[0], 32, false);
        let full = log_lines(&snapshot.slots[0], 32, true);
        assert!(compact.len() < full.len());
        let compact = log_body(&compact);
        let full = log_body(&full);
        assert!(compact.contains("[inline script]") && compact.contains("exit 0"));
        assert!(!compact.contains("FULL_COMMAND_END"));
        assert!(full.contains("FULL_COMMAND_END"));
        state.worker_frame(Some(&snapshot));
        let now = Instant::now();
        state.keyboard(b"\t", now, false);
        assert_eq!(state.worker.as_ref().unwrap().selected_slot(), Some(1));
        state.keyboard(b"\t", now, false);
        assert_eq!(state.worker.as_ref().unwrap().selected_slot(), Some(0));
        state.keyboard(b"\x1b[Z", now, false);
        assert_eq!(state.worker.as_ref().unwrap().selected_slot(), Some(1));
        state.keyboard(b"\x1b[Z\r", now, false);
        state.worker_frame(Some(&snapshot));
        assert_eq!(
            state.worker_snapshot(true)["worker_panels"][0]["logs"]["commands_expanded"],
            true
        );
        state.keyboard(b"\x1b[200~\t\r\x1b[Z\x1b[201~", now, false);
        assert_eq!(state.worker.as_ref().unwrap().selected_slot(), Some(0));
        assert!(state.worker.as_ref().unwrap().expanded_commands[&0]);
        assert!(
            !state
                .worker
                .as_ref()
                .unwrap()
                .expanded_commands
                .contains_key(&1)
        );
        state.keyboard(b"\r", now, false);
        assert!(!state.worker.as_ref().unwrap().expanded_commands[&0]);
        assert!(state.actions.is_empty());
        assert!(state.conversation.composer.text().is_empty());
    }

    #[test]
    fn worker_resource_chart_uses_recorded_rss_and_keeps_missing_samples_blank() {
        let now = Instant::now();
        let samples: VecDeque<_> = [Some(8 * 1024 * 1024), None, Some(16 * 1024 * 1024)]
            .into_iter()
            .enumerate()
            .map(|(index, memory_bytes)| metrics::Sample {
                at: now + Duration::from_secs(index as u64),
                cpu_percent: None,
                memory_bytes,
                cpu_pending: false,
            })
            .collect();
        let (plot, upper) = resource_chart(&samples, 9, 4, |sample| {
            sample
                .memory_bytes
                .map(|bytes| bytes as f64 / (1024.0 * 1024.0))
        });
        assert_eq!(upper, 16.0);
        assert_eq!(plot.len(), 4);
        assert_eq!(
            plot.iter()
                .flat_map(|row| row.chars())
                .filter(|ch| *ch != ' ')
                .count(),
            2
        );
    }

    #[test]
    #[ignore = "writes production compositor frames to SSMCP_WORKER_SNAPSHOT_DIR"]
    fn worker_layout_visual_fixture() {
        let directory = std::path::PathBuf::from(
            std::env::var_os("SSMCP_WORKER_SNAPSHOT_DIR").expect("snapshot output directory"),
        );
        std::fs::create_dir_all(&directory).unwrap();
        let mut snapshot = populated();
        snapshot.slots[0].state = "working";
        snapshot.slots[0].task_summary =
            Some("检查默认进度与日志详情，保留每个 worker 的阅读位置".into());
        snapshot.slots[0].task_text = Some(format!(
            "TASK_BEGIN 完整任务\n{}\nTASK_MIDDLE\n{}\nTASK_END",
            "检查实际布局 / inspect the real worker layout and preserve every instruction.\n"
                .repeat(24),
            "保留独立日志位置 / keep independent log positions while reading task details.\n"
                .repeat(24),
        ));
        snapshot.slots[0].progress =
            Some("默认页已显示真实进度；正在核对 48 列窗口和日志详情的切换。".into());
        snapshot.slots[0].last_command = Some(
            serde_json::json!(["python3", "-c", "print('isolated verification fixture')"])
                .to_string(),
        );
        snapshot.slots[0].current_task_elapsed_ms = Some(42_000);
        snapshot.slots[0].total_work_ms = 184_000;
        snapshot.slots[0].recent_task_durations_ms = vec![18_000, 63_000, 21_000];
        snapshot.idle = 0;
        snapshot.working = 2;
        snapshot.slots[0].task_id = Some("worker_task_layout_fixture_5".into());
        let command = serde_json::json!([
            "python3",
            "-c",
            format!("{} COMMAND_END", "inspect_layout(); ".repeat(50))
        ])
        .to_string();
        let records = [
            ("info", "Task assigned".to_owned()),
            ("info", "Progress: Inspecting the monitor layout and independent scroll positions.".to_owned()),
            ("info", r#"Command started: ["rg","-n","LogAnchor","src/client/terminal/worker.rs"]"#.into()),
            ("info", r#"Command finished: ["rg","-n","LogAnchor","src/client/terminal/worker.rs"]; exit 0"#.into()),
            ("info", format!("Command started: {command}")),
            ("info", "Progress: 检查中英文续行对齐，任务详情与日志位置保持独立。".into()),
            ("info", format!("Command finished: {command}; exit 0")),
            ("info", r#"Command started: ["cargo","test","--lib","client::terminal::worker"]"#.into()),
            ("error", r#"Command finished: ["cargo","test","--lib","client::terminal::worker"]; exit 7"#.into()),
            ("info", "Progress: Inspecting the failed layout assertion before the next check.".into()),
            ("info", r#"Command started: ["git","diff","--check"]"#.into()),
            ("info", r#"Command finished: ["git","diff","--check"]; exit 0"#.into()),
            ("info", "Progress: 默认页已显示真实进度；正在核对 48 列窗口和日志详情的切换。".into()),
        ];
        snapshot.slots[0].logs = records
            .into_iter()
            .enumerate()
            .map(|(index, (level, text))| SlotLog {
                level: level.into(),
                text,
                task_id: Some("worker_task_layout_fixture_5".into()),
                ..log(index as u64)
            })
            .collect();
        snapshot.slots[0].next_log_sequence = snapshot.slots[0].logs.len() as u64;
        let mut views: Vec<_> = [
            (64, 112, "tall"),
            (32, 132, "wide"),
            (26, 60, "narrow"),
            (26, 48, "compact"),
            (18, 48, "short"),
        ]
        .into_iter()
        .map(|(rows, cols, name)| {
            let mut state = state(rows, cols, true);
            state.worker_frame(Some(&snapshot));
            (state, name)
        })
        .collect();
        // Resource readings come from this test process; task and log text are fixtures.
        std::thread::sleep(Duration::from_millis(1100));
        let write = |name: &str, frame: &Frame| {
            std::fs::write(directory.join(format!("{name}.ansi")), frame.diff(None)).unwrap();
            std::fs::write(
                directory.join(format!("{name}.txt")),
                screen(frame).screen().contents(),
            )
            .unwrap();
            std::fs::write(
                directory.join(format!("{name}.json")),
                serde_json::to_vec(&serde_json::json!({
                    "rows":frame.geometry.rows,"cols":frame.geometry.cols
                }))
                .unwrap(),
            )
            .unwrap();
        };
        for (mut state, name) in views.drain(..) {
            let frame = state.worker_frame(Some(&snapshot));
            write(name, &frame);
            state.keyboard(b"\x1bOQ", Instant::now(), false);
            let frame = state.worker_frame(Some(&snapshot));
            write(&format!("{name}-logs-metrics"), &frame);
            if name == "wide" {
                state.keyboard(b"\r", Instant::now(), false);
                let frame = state.worker_frame(Some(&snapshot));
                write("wide-command-detail", &frame);
                state.keyboard(b"\r", Instant::now(), false);
            }
            state.keyboard(b"\x1bOQ", Instant::now(), false);
            if name == "short" {
                state.keyboard(b"\x1bOP", Instant::now(), false);
                let frame = state.worker_frame(Some(&snapshot));
                write("48-help", &frame);
                state.keyboard(b"\x1bOP", Instant::now(), false);
            }
            if matches!(name, "tall" | "narrow" | "compact") {
                state.keyboard(b"t", Instant::now(), false);
                let frame = state.worker_frame(Some(&snapshot));
                write(&format!("{name}-task-start"), &frame);
                state.keyboard(b"\x1b[F", Instant::now(), false);
                let frame = state.worker_frame(Some(&snapshot));
                write(&format!("{name}-task-end"), &frame);
            }
        }
        let slot = &mut snapshot.slots[0];
        slot.progress = None;
        slot.logs = vec![SlotLog {
            text: "Command started: command text is available in the log detail".into(),
            task_id: slot.task_id.clone(),
            ..log(1)
        }];
        let mut state = state(18, 48, true);
        write("48-waiting-progress", &state.worker_frame(Some(&snapshot)));
        snapshot.slots[0].logs.extend([
            SlotLog {
                level: "warning".into(),
                text: "Cancellation requested; awaiting completion acknowledgement".into(),
                task_id: Some("worker_task_layout_fixture_5".into()),
                ..log(2)
            },
            SlotLog {
                level: "warning".into(),
                text: "Remote agent report: unknown; output 0 bytes".into(),
                task_id: Some("previous-task".into()),
                ..log(3)
            },
        ]);
        write("48-alerts", &state.worker_frame(Some(&snapshot)));
    }
    #[test]
    fn worker_full_task_view_reaches_the_end_and_restores_independent_logs() {
        for (rows, cols) in [(18, 48), (64, 112), (32, 132), (26, 60)] {
            let mut state = state(rows, cols, false);
            let mut snapshot = populated();
            snapshot.slots[0].task_id = Some("task-full-view".into());
            snapshot.slots[0].task_summary = Some("Short task summary only".into());
            snapshot.slots[0].task_text = Some(format!(
                "TASK_BEGIN 完整任务\n{}\nTASK_MIDDLE\n{}\nTASK_END",
                "需要查看每一行 / keep every instruction readable.\n".repeat(30),
                "每条要求都应保留 / preserve the full original task.\n".repeat(30),
            ));
            state.worker_frame(Some(&snapshot));
            let now = Instant::now();
            state.keyboard(b"\x1b[A", now, false);
            state.worker_frame(Some(&snapshot));
            let saved = state.worker.as_ref().unwrap().anchors[&0];
            state.keyboard(b"t", now, false);
            let frame = state.worker_frame(Some(&snapshot));
            assert!(screen(&frame).screen().contents().contains("TASK_BEGIN"));
            let mut middle_seen = false;
            for _ in 0..64 {
                state.keyboard(b"\x1b[6~", now, false);
                let frame = state.worker_frame(Some(&snapshot));
                if screen(&frame).screen().contents().contains("TASK_MIDDLE") {
                    middle_seen = true;
                    break;
                }
            }
            assert!(middle_seen, "middle of the full task is reachable");
            state.keyboard(b"\x1b[F", now, false);
            let frame = state.worker_frame(Some(&snapshot));
            assert!(screen(&frame).screen().contents().contains("TASK_END"));
            snapshot.slots[0].logs.push(log(41));
            let frame = state.worker_frame(Some(&snapshot));
            assert!(screen(&frame).screen().contents().contains("TASK_END"));
            state.keyboard(b"\t", now, false);
            state.worker_frame(Some(&snapshot));
            assert!(
                !state.worker_snapshot(true)["worker_panels"][0]["logs"]["task_view"]
                    .as_bool()
                    .unwrap()
            );
            state.keyboard(b"\x1b[Z", now, false);
            let frame = state.worker_frame(Some(&snapshot));
            assert!(screen(&frame).screen().contents().contains("TASK_END"));
            state.keyboard(b"\x1b[200~tT\x1b[201~", now, false);
            assert!(state.worker.as_ref().unwrap().task_views.contains_key(&0));
            state.keyboard(b"T", now, false);
            state.worker_frame(Some(&snapshot));
            assert_eq!(state.worker.as_ref().unwrap().anchors[&0], saved);
            state.keyboard(b"t\x1b[F", now, false);
            let frame = state.worker_frame(Some(&snapshot));
            assert!(screen(&frame).screen().contents().contains("TASK_END"));
            snapshot.slots[0].task_id = Some("next-task".into());
            snapshot.slots[0].task_text = Some("NEXT_TASK_BEGIN".into());
            let frame = state.worker_frame(Some(&snapshot));
            assert!(
                screen(&frame)
                    .screen()
                    .contents()
                    .contains("NEXT_TASK_BEGIN")
            );
            assert!(state.actions.is_empty());
            assert!(state.conversation.composer.text().is_empty());
        }
    }
    #[test]
    fn worker_command_start_does_not_report_script_exit_as_an_observed_result() {
        let text = command_log_summary(r#"Command started: ["sh","-c","echo ok; exit 0"]"#);
        assert_eq!(text, "Command started: sh -c [inline script]");
        let text = command_log_summary(r#"Command finished: ["sh","-c","echo ok; exit 0"]"#);
        assert_eq!(text, "Command finished: sh -c [inline script]");
        let text =
            command_log_summary(r#"Command finished: ["sh","-c","echo ok; exit 0"]; exit 7"#);
        assert_eq!(text, "Command finished: sh -c [inline script]; exit 7");
    }
}
