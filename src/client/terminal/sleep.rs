//! A visual pause for authenticated input waits. Waking never answers a wait.
use super::{Frame, Key, State, nonzero};
use crate::client::interface::{questions, user_wait};
use std::collections::VecDeque;

// At most 32 finish requests plus four ordinary input waits may be active.
const DISMISSED_LIMIT: usize = 64;

#[derive(Clone, Default)]
struct Snapshot {
    connection_id: Option<String>,
    active: Vec<String>,
}

impl From<&user_wait::Snapshot> for Snapshot {
    fn from(wait: &user_wait::Snapshot) -> Self {
        let connection_id = wait.connected.then(|| wait.connection_id.clone()).flatten();
        let active = if connection_id.is_some() {
            wait.active
                .iter()
                .map(|entry| entry.episode_id.clone())
                .collect()
        } else {
            Vec::new()
        };
        Self {
            connection_id,
            active,
        }
    }
}

impl Snapshot {
    fn with_finish(mut self, finish: &questions::FinishWaitSnapshot) -> Self {
        if finish.connection_id.is_some() {
            // Join both generations without inventing a user-wait request.
            self.connection_id = Some(
                serde_json::to_string(&(
                    self.connection_id.as_deref(),
                    finish.connection_id.as_deref(),
                ))
                .expect("connection identifiers serialize"),
            );
            self.active.extend(
                finish
                    .active
                    .iter()
                    .map(|entry| format!("finish:{}", entry.request_id)),
            );
        }
        self
    }
}

#[derive(Default)]
pub(super) struct Sleep {
    connection: Option<String>,
    active: Vec<String>,
    dismissed: VecDeque<String>,
    pub(super) visible: bool,
    // Keep the entire gesture, including a release after disconnect/timeout.
    swallowing_mouse: bool,
}

impl Sleep {
    fn wants(&self, snapshot: &Snapshot) -> bool {
        snapshot.connection_id.is_some()
            && snapshot.active.iter().any(|episode| {
                self.connection != snapshot.connection_id || !self.dismissed.contains(episode)
            })
    }

    fn refresh(&mut self, snapshot: &Snapshot) {
        let connection = snapshot.connection_id.clone();
        if self.connection != connection {
            self.connection = connection;
            self.dismissed.clear();
        }
        self.active.clear();
        if self.connection.is_some() {
            for episode in &snapshot.active {
                if !self.active.contains(episode) {
                    self.active.push(episode.clone());
                }
            }
        }
        self.visible = self.wants(snapshot);
    }

    fn dismiss(&mut self) {
        for episode in &self.active {
            if !self.dismissed.contains(episode) {
                if self.dismissed.len() == DISMISSED_LIMIT {
                    self.dismissed.pop_front();
                }
                self.dismissed.push_back(episode.clone());
            }
        }
        self.visible = false;
    }

    /// Return true only when this overlay owns the mouse event.
    fn consume(&mut self, key: Key) -> bool {
        if let Key::Mouse {
            button, release, ..
        } = key
        {
            if self.swallowing_mouse {
                if release || (button & (32 | 64) == 0 && button & 3 == 3) {
                    self.swallowing_mouse = false;
                }
                return true;
            }
            if self.visible {
                // Wheels and motion do not wake or scroll through the overlay.
                if !release && button & (32 | 64) == 0 && button & 3 < 3 {
                    self.dismiss();
                    self.swallowing_mouse = true;
                }
                return true;
            }
        } else if self.visible {
            self.dismiss();
        }
        false
    }
}

impl State {
    fn sleep_snapshot(&self) -> Snapshot {
        #[cfg(test)]
        if let Some(snapshot) = &self.sleep_fixture {
            return Snapshot::from(snapshot);
        }
        if self.preview {
            Snapshot::default()
        } else {
            Snapshot::from(&user_wait::snapshot()).with_finish(&questions::finish_wait_snapshot())
        }
    }

    fn sleep_user_wait_alert(&self) -> Option<String> {
        #[cfg(test)]
        if let Some(snapshot) = &self.sleep_fixture {
            return Self::user_wait_alert(snapshot);
        }
        if self.preview {
            None
        } else {
            Self::user_wait_alert(&user_wait::snapshot())
        }
    }

    pub(super) fn sleep_wants_overlay(&self) -> bool {
        self.sleep.wants(&self.sleep_snapshot())
    }

    pub(super) fn sleep_update(&mut self) {
        let before = self.sleep.visible;
        let snapshot = self.sleep_snapshot();
        self.sleep.refresh(&snapshot);
        self.dirty |= before != self.sleep.visible;
    }

    pub(super) fn sleep_key(&mut self, key: Key) -> bool {
        // Input follows the last composed frame, not a newer Hub snapshot.
        // A timeout must not let a click through a still-visible overlay.
        let before = (self.sleep.visible, self.sleep.swallowing_mouse);
        let consumed = self.sleep.consume(key);
        self.dirty |= before != (self.sleep.visible, self.sleep.swallowing_mouse);
        consumed
    }

    pub(super) fn sleep_wake_text(&mut self, output: &[u8]) {
        if !output.is_empty() && self.sleep.visible {
            self.sleep.dismiss();
            self.dirty = true;
        }
    }

    pub(super) fn sleep_overlay(&mut self, frame: &mut Frame) {
        self.sleep_update();
        if self.sleep.visible {
            // Renderer::update removes only our Kitty placements. Its cache
            // reuploads them on wake; opaque native pixels cannot cover the dim.
            self.media.clear();
            dim(frame, self.ui.color);
            if let Some(alert) = self.sleep_user_wait_alert()
                && let Some(row) = frame.rows.last_mut()
            {
                // Repaint only the real user-wait alert after the gray pass.
                // Embedded output colors must remain dim, and finish-only or
                // ordinary task snapshots cannot invent a countdown here.
                let margin = if frame.geometry.cols > 2 { " " } else { "" };
                let text = super::fit(&format!("{margin}{alert}"), frame.geometry.cols);
                let padding = " ".repeat(
                    usize::from(frame.geometry.cols)
                        .saturating_sub(unicode_width::UnicodeWidthStr::width(text.as_str())),
                );
                *row = if self.ui.color {
                    format!(
                        "\x1b[0;1;{};48;2;10;13;18m{text}{padding}\x1b[0m",
                        self.ui.color("error")
                    )
                } else {
                    format!("\x1b[0;1m{text}{padding}\x1b[0m")
                };
            }
        }
        // Copy mode must keep reporting the mouse release after a click wakes
        // the overlay. Restore its original capture mode only after release.
        frame.mouse_capture |= self.sleep.swallowing_mouse;
    }
}

fn logo(rows: u16, cols: u16) -> &'static [&'static str] {
    if rows >= 12 && cols >= 24 {
        &[
            "        z",
            "     Z",
            " .----------.",
            " |  -    -  |",
            " |    __    |",
            " '----------'",
            "",
            "Waiting for input",
            "Click or type to wake",
        ]
    } else if rows >= 5 && cols >= 14 {
        &["z Z", "(-.-)", "Waiting", "Click to wake"]
    } else {
        &["z"]
    }
}

fn dim(frame: &mut Frame, color: bool) {
    let rows = frame.geometry.rows;
    let cols = frame.geometry.cols;
    // Parse the real compositor output before recoloring. Prefixing SGR 2
    // alone would let embedded resets/inverse/bright colors escape the dim.
    let mut parser = vt100::Parser::new(nonzero(rows), nonzero(cols), 0);
    parser.process(&frame.diff(None));
    let lines = logo(rows, cols);
    let top = rows.saturating_sub(lines.len() as u16) / 2;
    let mut marks = Vec::new();
    for (offset, line) in lines.iter().enumerate() {
        let row = top + offset as u16;
        let left = cols.saturating_sub(line.len() as u16) / 2;
        parser.process(format!("\x1b[{};{}H{line}", row + 1, left + 1).as_bytes());
        for (offset, byte) in line.bytes().enumerate() {
            if byte != b' ' {
                marks.push((row, left + offset as u16));
            }
        }
    }
    let muted = if color {
        "\x1b[0;2;38;2;90;100;114;48;2;10;13;18m"
    } else {
        "\x1b[0;2m"
    };
    let accent = if color {
        "\x1b[0;2;38;2;166;180;198;48;2;10;13;18m"
    } else {
        "\x1b[0m"
    };
    let screen = parser.screen();
    frame.rows = (0..rows)
        .map(|row| {
            let mut text = String::from(muted);
            let mut highlighted = false;
            let mut col = 0;
            while col < cols {
                let next = marks.binary_search(&(row, col)).is_ok();
                if next != highlighted {
                    text.push_str(if next { accent } else { muted });
                    highlighted = next;
                }
                let Some(cell) = screen.cell(row, col) else {
                    text.push(' ');
                    col += 1;
                    continue;
                };
                if cell.is_wide_continuation()
                    || !cell.has_contents()
                    || (cell.is_wide() && col + 1 >= cols)
                {
                    text.push(' ');
                    col += 1;
                } else {
                    for ch in cell.contents().chars().filter(|ch| !ch.is_control()) {
                        text.push(ch);
                    }
                    col += if cell.is_wide() { 2 } else { 1 };
                }
            }
            text.push_str("\x1b[0m");
            text
        })
        .collect();
    frame.hidden = true;
    frame.mouse_capture = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::terminal::{Layout, Ui, preview_state};
    use std::time::Instant;
    use unicode_width::UnicodeWidthStr;

    fn snapshot(connection: &str, episode: usize, wait: usize) -> user_wait::Snapshot {
        user_wait::Snapshot {
            connected: true,
            connection_id: Some(connection.into()),
            active: vec![user_wait::Entry {
                wait_id: format!("wait-{wait}"),
                episode_id: format!("{episode:064x}"),
                intent: user_wait::Intent::NeedUser,
                recipient: "next".into(),
                reason: Some("Synthetic visual wait".into()),
                remaining_ms: 20_000,
                finish_request_id: None,
                notification_requested: false,
                notification: None,
            }],
            recent: Vec::new(),
        }
    }

    fn state(rows: u16, cols: u16) -> State {
        let ui = Ui {
            layout: Layout::Chat,
            color: true,
            sidebar: false,
            ..Ui::default()
        };
        let mut state = preview_state(rows, cols, &ui);
        state.sleep_fixture = Some(snapshot("connection-a", 1, 1));
        state.sleep_update();
        state
    }

    fn parse(frame: &Frame) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            nonzero(frame.geometry.rows),
            nonzero(frame.geometry.cols),
            0,
        );
        parser.process(&frame.diff(None));
        parser
    }

    fn save(frame: &Frame, name: &str) {
        if let Some(directory) = std::env::var_os("SSMCP_SLEEP_PREVIEW_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("{name}-{}.ansi", frame.geometry.cols)),
                frame.diff(None),
            )
            .unwrap();
        }
    }

    #[test]
    fn sleep_dismissal_follows_episode_and_connection_with_bounded_memory() {
        let mut sleep = Sleep::default();
        sleep.refresh(&Snapshot::from(&snapshot("a", 1, 1)));
        assert!(sleep.visible);
        sleep.dismiss();
        sleep.refresh(&Snapshot::from(&snapshot("a", 1, 2)));
        assert!(
            !sleep.visible,
            "a new native wait ID is still the same episode"
        );
        let mut gap = snapshot("a", 1, 2);
        gap.active.clear();
        sleep.refresh(&Snapshot::from(&gap));
        sleep.refresh(&Snapshot::from(&snapshot("a", 1, 3)));
        assert!(
            !sleep.visible,
            "timeout polling gaps retain the visual dismissal"
        );
        sleep.refresh(&Snapshot::from(&snapshot("a", 2, 4)));
        assert!(sleep.visible);
        sleep.dismiss();
        sleep.refresh(&Snapshot::from(&snapshot("b", 2, 4)));
        assert!(
            sleep.visible,
            "another connection owns another visual episode"
        );
        for episode in 0..DISMISSED_LIMIT + 8 {
            sleep.refresh(&Snapshot::from(&snapshot("b", episode, episode)));
            sleep.dismiss();
        }
        assert_eq!(sleep.dismissed.len(), DISMISSED_LIMIT);
        assert_eq!(sleep.dismissed.front(), Some(&format!("{:064x}", 8)));
        sleep.refresh(&Snapshot::from(&user_wait::Snapshot::default()));
        assert!(!sleep.visible);
        assert!(sleep.dismissed.is_empty());
    }

    #[test]
    fn sleep_mouse_absorbs_complete_gesture_without_mutating_conversation() {
        let now = Instant::now();
        let mut state = state(28, 120);
        state.sleep.dismiss();
        state.keyboard(b"Draft remains", now, false);
        state.sleep_fixture = Some(snapshot("connection-a", 2, 2));
        state.frame(now);
        let before = state.conversation.snapshot();
        let events = state.events.snapshot();
        // A wheel cannot scroll or dismiss. Fragment every byte of a right
        // click: its normal action toggles logs, making click-through visible.
        state.keyboard(b"\x1b[<64;10;5M", now, false);
        assert!(state.sleep.visible);
        for byte in b"\x1b[<2;10;5M" {
            state.keyboard(&[*byte], now, false);
        }
        assert!(!state.sleep.visible);
        state.keyboard(b"\x1b[<34;12;6M", now, false);
        // A connection transition must not release the pending mouse gesture.
        state.sleep_fixture = Some(snapshot("connection-b", 3, 3));
        state.frame(now);
        state.keyboard(b"\x1b[<2;12;6m", now, false);
        assert_eq!(state.conversation.snapshot(), before);
        assert_eq!(state.events.snapshot(), events);
        assert!(!state.sleep.swallowing_mouse);
        assert!(state.sleep.visible);
        // Dismiss this new episode, then the next complete click acts normally.
        state.keyboard(b"\x1b[<0;2;2M\x1b[<0;2;2m", now, false);
        assert_eq!(state.conversation.snapshot(), before);
        state.keyboard(b"\x1b[<2;10;5M\x1b[<2;10;5m", now, false);
        assert_ne!(
            state.conversation.snapshot()["logs_open"],
            before["logs_open"]
        );
    }

    #[test]
    fn sleep_timeout_before_redraw_keeps_visible_overlay_input_barrier() {
        let now = Instant::now();
        let mut state = state(28, 120);
        state.frame(now);
        let before = state.conversation.snapshot();
        state.sleep_fixture.as_mut().unwrap().active.clear();
        // The wait has ended, but the terminal still shows the last frame.
        state.keyboard(b"\x1b[<2;8;4M", now, false);
        assert!(!state.sleep.visible);
        assert_eq!(state.conversation.snapshot(), before);
        state.frame(now);
        state.keyboard(b"\x1b[<2;8;4m", now, false);
        assert!(!state.sleep.swallowing_mouse);
        assert_eq!(state.conversation.snapshot(), before);
        state.sleep_fixture = Some(snapshot("connection-a", 1, 2));
        state.frame(now);
        assert!(
            !state.sleep.visible,
            "the click dismissed the displayed episode"
        );
    }

    #[test]
    fn sleep_keyboard_preserves_first_unicode_key_and_bracketed_paste() {
        let now = Instant::now();
        let mut state = state(28, 120);
        state.conversation.composer.set_text("kept ");
        for byte in "界".as_bytes() {
            state.keyboard(&[*byte], now, false);
        }
        assert!(!state.sleep.visible);
        assert_eq!(state.conversation.snapshot()["draft"]["text"], "kept 界");
        state.sleep_fixture = Some(snapshot("connection-a", 2, 2));
        state.sleep_update();
        state.keyboard(b"\x1b[200~paste\n  stays\x1b[201~", now, false);
        assert!(!state.sleep.visible);
        assert_eq!(
            state.conversation.snapshot()["draft"]["text"],
            "kept 界paste\n  stays"
        );
        assert_eq!(state.sleep_snapshot().active.len(), 1);
    }

    #[test]
    fn sleep_frame_dims_resets_and_preserves_wide_and_narrow_geometry() {
        let now = Instant::now();
        for (rows, cols) in [(28, 120), (24, 48), (5, 16), (1, 1)] {
            let mut state = state(rows, cols);
            state.notice = None;
            state.conversation.wait_fixture = state.sleep_fixture.clone();
            state.conversation.composer.set_text("Draft: 界");
            let before = state.conversation.snapshot();
            let frame = state.frame(now);
            let parsed = parse(&frame);
            assert!(frame.hidden && frame.mouse_capture);
            assert_eq!(frame.rows.len(), usize::from(rows));
            for row in 0..rows {
                assert!(
                    parsed
                        .screen()
                        .rows(0, cols)
                        .nth(row as usize)
                        .unwrap()
                        .width()
                        <= cols as usize
                );
                for col in 0..cols {
                    let cell = parsed.screen().cell(row, col).unwrap();
                    if !cell.is_wide_continuation() {
                        if row == rows - 1 {
                            assert!(!cell.dim() && cell.bold() && !cell.inverse());
                            assert_eq!(cell.fgcolor(), vt100::Color::Rgb(242, 139, 130));
                        } else {
                            assert!(cell.dim());
                            assert!(!cell.bold() && !cell.inverse());
                        }
                        assert_eq!(cell.bgcolor(), vt100::Color::Rgb(10, 13, 18));
                    }
                }
            }
            assert!(parsed.screen().contents().contains(if rows == 1 {
                "!"
            } else if cols >= 24 {
                "Click or type to wake"
            } else {
                "z"
            }));
            if cols >= 48 {
                assert!(
                    parsed
                        .screen()
                        .contents()
                        .contains("! 等待你的输入 · 剩余 00:20 · /wait")
                );
            }
            assert_eq!(state.conversation.snapshot(), before);
            save(&frame, "sleep");
            state.sleep.dismiss();
            let awake = state.frame(now);
            let parsed = parse(&awake);
            assert!(!parsed.screen().contents().contains("Click or type to wake"));
            if cols >= 48 {
                assert!(
                    parsed
                        .screen()
                        .contents()
                        .contains("! 等待你的输入 · 剩余 00:20 · /wait")
                );
                let cell = parsed.screen().cell(rows - 1, 1).unwrap();
                assert_eq!(cell.contents(), "!");
                assert_eq!(cell.fgcolor(), vt100::Color::Rgb(242, 139, 130));
                assert!(!cell.dim());
            }
            assert_eq!(state.conversation.snapshot(), before);
            save(&awake, "awake");
        }
        // Bright/inverse embedded resets are parsed, never copied over dim.
        let mut state = state(12, 48);
        let mut frame = state.base_frame(now);
        frame.rows[0] = "\x1b[1;7;97;107mBRIGHT\x1b[0m reset \x1b[22m tail".into();
        dim(&mut frame, true);
        let parsed = parse(&frame);
        for col in [0, 8, 17] {
            let cell = parsed.screen().cell(0, col).unwrap();
            assert!(cell.dim());
            assert!(!cell.bold() && !cell.inverse());
            assert_eq!(cell.fgcolor(), vt100::Color::Rgb(90, 100, 114));
        }
    }

    #[test]
    fn sleep_transition_breaks_copy_freeze_and_restores_copy_mode() {
        let now = Instant::now();
        let mut state = state(28, 120);
        state.sleep_fixture = Some(user_wait::Snapshot::default());
        state.set_selection_mode(true);
        let before = state.frame(now);
        state.dirty = false;
        assert!(!before.mouse_capture);
        assert!(!state.should_redraw(Some(&before), state.graphics.cell()));
        state.sleep_fixture = Some(snapshot("a", 1, 1));
        assert!(state.should_redraw(Some(&before), state.graphics.cell()));
        let asleep = state.frame(now);
        assert!(asleep.mouse_capture && state.selection);
        state.keyboard(b"\x1b[<0;1;1M", now, false);
        assert!(state.selection && !state.sleep.visible);
        assert!(state.should_redraw(Some(&asleep), state.graphics.cell()));
        let pressed = state.frame(now);
        assert!(
            pressed.mouse_capture,
            "keep terminal release reporting after wake"
        );
        state.dirty = false;
        state.keyboard(b"\x1b[<0;1;1m", now, false);
        assert!(state.should_redraw(Some(&pressed), state.graphics.cell()));
        let after = state.frame(now);
        assert!(!after.mouse_capture);
        assert!(!state.sleep.swallowing_mouse);
        assert_eq!(after.rows, before.rows);
        state.dirty = false;
        assert!(!state.should_redraw(Some(&after), state.graphics.cell()));
    }
    #[test]
    fn one_wake_dismisses_the_full_bounded_combined_wait_set() {
        let combined = Snapshot {
            connection_id: Some("ordinary".into()),
            active: (0..4).map(|id| format!("input-{id}")).collect(),
        }
        .with_finish(&questions::FinishWaitSnapshot {
            connection_id: Some("room:1".into()),
            active: (0..32)
                .map(|id| questions::FinishWaitEntry {
                    request_id: format!("finish-{id}"),
                })
                .collect(),
        });
        let mut sleep = Sleep::default();
        sleep.refresh(&combined);
        assert!(sleep.visible);
        sleep.dismiss();
        sleep.refresh(&combined);
        assert!(
            !sleep.visible,
            "waking the full active set must not evict one of its own episodes"
        );
        assert_eq!(sleep.dismissed.len(), 36);
    }

    #[test]
    fn finish_waits_dim_only_while_active_and_retry_keeps_episode_dismissal() {
        let mut finish = questions::FinishWaitSnapshot {
            connection_id: Some("room:1".into()),
            active: vec![questions::FinishWaitEntry {
                request_id: "finish-1".into(),
            }],
        };
        let mut sleep = Sleep::default();
        sleep.refresh(&Snapshot::default().with_finish(&finish));
        assert!(sleep.visible);
        assert!(sleep.consume(Key::Mouse {
            button: 0,
            x: 1,
            y: 1,
            release: false
        }));
        assert!(sleep.consume(Key::Mouse {
            button: 0,
            x: 1,
            y: 1,
            release: true
        }));
        assert!(!sleep.visible);
        finish.active.clear();
        sleep.refresh(&Snapshot::default().with_finish(&finish));
        assert!(
            !sleep.visible,
            "a retained question without a real wait does not dim"
        );
        finish.active.push(questions::FinishWaitEntry {
            request_id: "finish-1".into(),
        });
        sleep.refresh(&Snapshot::default().with_finish(&finish));
        assert!(!sleep.visible, "retry belongs to the same visible episode");
        finish.active[0].request_id = "finish-2".into();
        sleep.refresh(&Snapshot::default().with_finish(&finish));
        assert!(sleep.visible);
        sleep.dismiss();
        finish.connection_id = Some("room:2".into());
        sleep.refresh(&Snapshot::default().with_finish(&finish));
        assert!(sleep.visible, "a new connection may dim again");
        sleep.refresh(&Snapshot::default());
        assert!(!sleep.visible);
        assert!(sleep.dismissed.is_empty());
    }
}
