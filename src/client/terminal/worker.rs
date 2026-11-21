//! A worker connection is a read-only monitor, never an alternate composer.
use super::{Frame, Key, PanelTone, State, fit, panel, request_interrupt};
use crate::worker::Snapshot;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

#[derive(Default)]
pub(super) struct Monitor {
    offset: usize,
}

impl State {
    pub(super) fn worker_keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) {
        // Reuse the raw decoder so split paste markers and pasted Ctrl-C remain
        // literal bytes. Discard every byte; only scrolling and a real Ctrl-C act.
        let (_, actions) = self.keyboard.feed(bytes, now, flush, false, true);
        let page = usize::from(self.geometry.rows.saturating_sub(10).max(1));
        for action in actions {
            if matches!(action, Key::Interrupt) {
                request_interrupt(self);
                return;
            }
            let Some(monitor) = self.worker.as_mut() else {
                return;
            };
            match action {
                Key::AuditUp => monitor.offset = monitor.offset.saturating_sub(1),
                Key::AuditDown => monitor.offset = monitor.offset.saturating_add(1),
                Key::Up => monitor.offset = monitor.offset.saturating_sub(page),
                Key::Down => monitor.offset = monitor.offset.saturating_add(page),
                Key::Home | Key::HistoryStart => monitor.offset = 0,
                Key::End | Key::HistoryEnd => monitor.offset = usize::MAX,
                Key::Mouse { button, x, y, release }
                    if !release && button & 64 != 0
                        && x < self.geometry.cols && y < self.geometry.rows =>
                {
                    if button & 1 == 0 {
                        monitor.offset = monitor.offset.saturating_sub(3);
                    } else {
                        monitor.offset = monitor.offset.saturating_add(3);
                    }
                }
                _ => continue,
            }
            self.dirty = true;
        }
    }

    pub(super) fn worker_snapshot(&self, active: bool) -> serde_json::Value {
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
            "workers":crate::worker::display(),
        })
    }

    pub(super) fn worker_frame(&mut self, snapshot: Option<&Snapshot>) -> Frame {
        let g = self.geometry;
        let height = usize::from(g.rows);
        let mut rows = vec![line(self, &[(PanelTone::Heading,
            format!("SSMCP · Workers{}", self.context_label))]), String::new()];
        if let Some(snapshot) = snapshot {
            let counts = [
                (PanelTone::Success, format!("○ Idle {}", snapshot.idle)),
                (PanelTone::Warning, format!("● Working {}", snapshot.working)),
                (PanelTone::Error, format!("× Offline {}", snapshot.offline)),
                (PanelTone::Accent, format!("Σ Total {}", snapshot.total)),
            ];
            let per_row = if g.cols >= 68 { 4 } else if g.cols >= 36 { 2 } else { 1 };
            for group in counts.chunks(per_row) {
                let parts: Vec<_> = group.iter().enumerate().map(|(index, (tone, text))| {
                    (*tone, format!("{}{text}", if index == 0 { "" } else { "    " }))
                }).collect();
                rows.push(line(self, &parts));
            }
            let status = if snapshot.closed && snapshot.working > 0 {
                "Stopping · waiting for owned work to stop"
            } else if snapshot.closed {
                "Stopped"
            } else {
                "FIFO assignment"
            };
            rows.push(line(self, &[(PanelTone::Muted,
                format!("Queued {} · {status}", snapshot.queued))]));
            rows.push(String::new());
            rows.push(line(self, &[(PanelTone::Muted, "Worker   State / task".into())]));
            let visible = height.saturating_sub(rows.len() + 1);
            let monitor = self.worker.as_mut().expect("worker frame requires monitor mode");
            monitor.offset = monitor.offset.min(snapshot.slots.len().saturating_sub(visible));
            let offset = monitor.offset;
            for slot in snapshot.slots.iter().skip(offset).take(visible) {
                let (tone, symbol, label) = match slot.state {
                    "idle" => (PanelTone::Success, "○", "idle"),
                    "working" => (PanelTone::Warning, "●", "working"),
                    "offline" => (PanelTone::Error, "×", "offline"),
                    _ => (PanelTone::Muted, "?", "unknown"),
                };
                let detail = slot.task_id.as_deref().or(slot.reason.as_deref());
                let suffix = detail.map(|text| format!(" · {}", panel::safe_text(text)))
                    .unwrap_or_default();
                rows.push(line(self, &[(tone,
                    format!("{symbol} #{:02}  {label}{suffix}", slot.slot + 1))]));
            }
        } else {
            // Configuration is not evidence that an executor is online.
            rows.push(line(self, &[(PanelTone::Muted, "Waiting for worker readiness".into())]));
        }
        rows.truncate(height.saturating_sub(1));
        rows.resize_with(height.saturating_sub(1), String::new);
        let footer = if height == 1 {
            "Workers · Ctrl+C Stop"
        } else if g.cols < 54 {
            "↑↓ Scroll · Ctrl+C Stop"
        } else {
            "↑↓ / PgUp / PgDn Scroll · Ctrl+C Stop"
        };
        rows.push(line(self, &[(PanelTone::Muted, footer.into())]));
        Frame {
            geometry: g, rows, cursor: (0, 0), hidden: true,
            application_cursor: false, application_keypad: false,
            bracketed_paste: true, mouse_capture: true,
        }
    }
}

fn line(state: &State, parts: &[(PanelTone, String)]) -> String {
    let mut width = 0;
    let mut output = String::new();
    for (tone, text) in parts {
        let text = fit(&panel::safe_text(text), state.geometry.cols.saturating_sub(width));
        width += u16::try_from(text.width()).unwrap_or(u16::MAX);
        output.push_str(&state.tone(*tone, &text));
    }
    output.push_str(&state.tone(PanelTone::Normal,
        &" ".repeat(usize::from(state.geometry.cols.saturating_sub(width)))));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{terminal::nonzero, ui::{Layout, Ui}};
    use crate::worker::SlotView;
    use std::time::Duration;

    fn state(rows: u16, cols: u16, color: bool) -> State {
        let mut state = State::new(rows, cols, &Ui { color, layout: Layout::Chat, ..Ui::default() });
        state.preview = true;
        state.worker = Some(Monitor::default());
        state
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            generation:"fixture".into(), total:3, idle:1, working:1,
            offline:1, queued:2, closed:false,
            slots:vec![
                SlotView { slot:0, epoch:1, state:"idle", task_id:None, reason:None },
                SlotView { slot:1, epoch:1, state:"working", task_id:Some("task-actual".into()), reason:None },
                SlotView { slot:2, epoch:0, state:"offline", task_id:None, reason:Some("waiting for readiness".into()) },
            ],
        }
    }

    fn screen(frame: &Frame) -> vt100::Parser {
        let mut parser = vt100::Parser::new(nonzero(frame.geometry.rows), nonzero(frame.geometry.cols), 0);
        parser.process(&frame.diff(None));
        parser
    }

    #[test]
    fn worker_monitor_frames_keep_real_counts_and_labels_at_wide_and_narrow_widths() {
        for (rows, cols) in [(20, 100), (18, 48), (18, 32), (18, 18)] {
            for color in [true, false] {
                let mut state = state(rows, cols, color);
                let frame = state.worker_frame(Some(&snapshot()));
                let parser = screen(&frame);
                let text = parser.screen().contents();
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
    fn worker_monitor_never_invents_readiness_and_scrolls_only_reported_slots() {
        let mut state = state(14, 80, false);
        let first = state.worker_frame(None);
        assert!(screen(&first).screen().contents().contains("Waiting for worker readiness"));
        assert!(!screen(&first).screen().contents().contains("Idle 0"));
        let mut snapshot = snapshot();
        snapshot.total = 20;
        snapshot.offline = 18;
        snapshot.slots.extend((3..20).map(|slot| SlotView {
            slot, epoch:0, state:"offline", task_id:None, reason:None,
        }));
        assert!(!screen(&state.worker_frame(Some(&snapshot))).screen().contents().contains("#20"));
        state.keyboard(b"\x1b[F", Instant::now(), false);
        assert!(screen(&state.worker_frame(Some(&snapshot))).screen().contents().contains("#20"));
        state.keyboard(b"\x1b[H", Instant::now(), false);
        assert!(screen(&state.worker_frame(Some(&snapshot))).screen().contents().contains("#01"));
        snapshot.closed = true;
        assert!(screen(&state.worker_frame(Some(&snapshot))).screen().contents().contains("Stopping"));
    }

    #[test]
    fn worker_monitor_consumes_layout_keys_mouse_and_split_pastes_without_changing_the_draft() {
        let bytes = b"\x1bOQignored\r\x1b\x1b[<0;2;3M\x1b[18~\x1b[200~pasted\x03\x1bOQ\r\x1b[201~";
        for split in 0..=bytes.len() {
            let mut state = state(20, 80, false);
            state.conversation.composer.set_text("preserved draft @file $skill");
            let mut before = state.conversation.snapshot();
            // This global queue belongs to other concurrent tests as well.
            before.as_object_mut().unwrap().remove("pending_questions");
            let now = Instant::now();
            assert!(state.keyboard(&bytes[..split], now, false).is_empty());
            assert!(state.keyboard(&bytes[split..], now, false).is_empty());
            assert!(state.keyboard(&[], now + Duration::from_millis(100), true).is_empty());
            let mut after = state.conversation.snapshot();
            after.as_object_mut().unwrap().remove("pending_questions");
            assert_eq!(after, before);
            assert!(state.actions.is_empty());
            assert!(!state.selection);
            assert!(!state.help);
            assert_eq!(state.ui.layout, Layout::Chat);
            assert!(!state.pty.screen().contents().contains("ignored"));
            // Even an external appearance change cannot select a chat renderer.
            for layout in [Layout::Chat, Layout::Split, Layout::Logs, Layout::Audit] {
                state.ui.layout = layout;
                let frame = state.frame(now);
                assert!(screen(&frame).screen().contents().contains("Workers"));
                assert!(!screen(&frame).screen().contents().contains("preserved draft"));
            }
        }
    }
}
