//! A worker connection is a read-only monitor, never an alternate composer.
use super::{Frame, Key, PanelTone, State, fit, panel, request_interrupt};
use crate::worker::{SlotView, Snapshot};
use std::{collections::BTreeMap, time::Instant};
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
    log_rows: usize,
    keys: Vec<LogAnchor>,
    start: usize,
    following: bool,
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
    panels: Vec<WorkerPanel>,
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
            .map_or(1, |panel| panel.log_rows.saturating_sub(1).max(1))
    }

    fn render(
        &mut self,
        snapshot: &Snapshot,
        top: usize,
        available: usize,
        cols: u16,
    ) -> Vec<Vec<(PanelTone, String)>> {
        if self.generation.as_deref() != Some(snapshot.generation.as_str()) {
            self.generation = Some(snapshot.generation.clone());
            self.selected = 0;
            self.first = 0;
            self.anchors.clear();
        }
        self.order = snapshot.slots.iter().map(|slot| slot.slot).collect();
        self.selected = self.selected.min(self.order.len().saturating_sub(1));
        self.anchors.retain(|slot, _| self.order.contains(slot));
        self.panels.clear();
        if available == 0 || snapshot.slots.is_empty() {
            return Vec::new();
        }
        let count = (available / 6).max(1).min(snapshot.slots.len());
        self.first = self.first.min(self.selected);
        if self.selected >= self.first + count {
            self.first = self.selected + 1 - count;
        }
        self.first = self.first.min(snapshot.slots.len().saturating_sub(count));
        let selected = self.selected_slot();
        let mut output = Vec::with_capacity(available);
        for (index, slot) in snapshot
            .slots
            .iter()
            .skip(self.first)
            .take(count)
            .enumerate()
        {
            let height = available / count + usize::from(index < available % count);
            let row = top + output.len();
            let log_rows = height.saturating_sub(3);
            let entries = log_lines(slot, usize::from(cols.saturating_sub(2).max(1)));
            let keys: Vec<_> = entries.iter().map(|entry| entry.0).collect();
            let maximum = keys.len().saturating_sub(log_rows);
            let anchor = self.anchors.get(&slot.slot).copied().flatten();
            let start = anchor.map_or(maximum, |anchor| {
                keys.iter()
                    .position(|key| *key >= anchor)
                    .unwrap_or(maximum)
                    .min(maximum)
            });
            let following = anchor.is_none();
            if !following {
                self.anchors.insert(slot.slot, keys.get(start).copied());
            }
            let (tone, symbol, label) = match slot.state {
                "idle" => (PanelTone::Success, "○", "idle"),
                "working" => (PanelTone::Warning, "●", "working"),
                "offline" => (PanelTone::Error, "×", "offline"),
                _ => (PanelTone::Muted, "?", "unknown"),
            };
            let detail = slot.task_id.as_deref().or(slot.reason.as_deref());
            let suffix = detail
                .map(|text| format!(" · {}", panel::safe_text(text)))
                .unwrap_or_default();
            output.push(vec![
                (
                    PanelTone::Accent,
                    if selected == Some(slot.slot) {
                        "› "
                    } else {
                        "  "
                    }
                    .into(),
                ),
                (
                    tone,
                    format!("#{:02} {symbol} {label}{suffix}", slot.slot + 1),
                ),
            ]);
            for offset in 0..log_rows {
                output.push(if let Some((_, tone, text)) = entries.get(start + offset) {
                    vec![(*tone, format!("  {text}"))]
                } else if offset == 0 && entries.is_empty() {
                    vec![(PanelTone::Muted, "  No log entries yet".into())]
                } else {
                    Vec::new()
                });
            }
            if height >= 2 {
                let summary = if log_rows == 0 {
                    "  Enlarge terminal to view logs".into()
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
                    format!(
                        "  Logs {range} · {}",
                        if following {
                            "Live"
                        } else {
                            "Paused · End to follow"
                        }
                    )
                };
                output.push(vec![(PanelTone::Muted, summary)]);
            }
            if height >= 3 {
                output.push(Vec::new());
            }
            self.panels.push(WorkerPanel {
                slot: slot.slot,
                row,
                rows: height,
                log_row: row + 1,
                log_rows,
                keys,
                start,
                following,
            });
        }
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
                    let hovered = monitor
                        .panels
                        .iter()
                        .find(|panel| {
                            usize::from(y) >= panel.row && usize::from(y) < panel.row + panel.rows
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
                "logs":{"row":panel.log_row,"rows":panel.log_rows,"visible":panel.log_rows>0,
                    "retained_lines":panel.keys.len(),"first_line":panel.start,"following":panel.following}
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
                (PanelTone::Accent, format!("Σ Total {}", snapshot.total)),
            ];
            let per_row = if g.cols >= 68 {
                4
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
            let status = if (snapshot.stopping || snapshot.closed) && snapshot.working > 0 {
                "Stopping intake · waiting for task acknowledgement"
            } else if snapshot.stopping {
                "Intake stopped"
            } else if snapshot.closed {
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
            generation: "fixture".into(),
            process_id: 31415,
            manager: None,
            stopping: false,
            total: 3,
            idle: 1,
            working: 1,
            offline: 1,
            queued: 2,
            closed: false,
            slots: vec![
                SlotView {
                    slot: 0,
                    epoch: 1,
                    state: "idle",
                    task_id: None,
                    reason: None,
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
                SlotView {
                    slot: 1,
                    epoch: 1,
                    state: "working",
                    task_id: Some("task-actual".into()),
                    reason: None,
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
                SlotView {
                    slot: 2,
                    epoch: 0,
                    state: "offline",
                    task_id: None,
                    reason: Some("waiting for readiness".into()),
                    logs: Vec::new(),
                    next_log_sequence: 0,
                },
            ],
        }
    }

    #[test]
    fn worker_manager_identity_is_explicit_without_abbreviating_ids_or_hiding_active_work() {
        let mut snapshot = snapshot();
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
        assert!(text.contains("waiting for task acknowledgement"));
        let mut narrow = state(18, 18, false);
        let text = screen(&narrow.worker_frame(Some(&snapshot)))
            .screen()
            .contents();
        assert!(!text.contains(&client[..16]));
        assert!(text.contains("Manager attached"));
        assert!(text.contains("Working 1"));
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
                for label in ["Idle 1", "Working 1", "Offline 1", "Total 3", "Queued 2"] {
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
        snapshot.offline = 18;
        snapshot.slots.extend((3..20).map(|slot| SlotView {
            slot,
            epoch: 0,
            state: "offline",
            task_id: None,
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
    fn wheel_targets_the_hovered_worker_and_snapshot_reports_real_log_geometry() {
        let mut state = state(24, 90, false);
        state.worker_frame(Some(&populated()));
        let y = state.worker.as_ref().unwrap().panels[1].log_row + 1;
        state.keyboard(format!("\x1b[<64;3;{y}M").as_bytes(), Instant::now(), false);
        state.worker_frame(Some(&populated()));
        let monitor = state.worker.as_ref().unwrap();
        assert_eq!(monitor.selected_slot(), Some(1));
        assert!(!monitor.anchors.contains_key(&0));
        assert!(monitor.anchors[&1].is_some());
        let view = state.worker_snapshot(true);
        assert_eq!(view["panes"]["input"]["visible"], false);
        assert_eq!(view["worker_panels"][1]["selected"], true);
        assert_eq!(view["worker_panels"][1]["logs"]["following"], false);
        assert_eq!(view["worker_panels"][1]["logs"]["row"], y - 1);
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
