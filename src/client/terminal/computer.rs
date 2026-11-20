//! Local computer controls: cached drawing state and I/O outside the compositor lock.
use super::*;
use crate::client::interface::computer as operator;
use serde_json::Value;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const PENDING_NOTICE: &str = "Updating computer controls…";

#[derive(Default)]
pub(super) struct Control {
    status: Option<Value>,
    pending: Vec<operator::Command>,
    checked: Option<Instant>,
}

impl Control {
    fn badge(&self, width: u16) -> String {
        let (state, short) = match self.status.as_ref() {
            Some(status) if status["error"].is_string() => ("关闭（状态异常）", "关!"),
            Some(status) if status["enabled"] == true => ("开启", "开"),
            Some(_) => ("关闭", "关"),
            None => ("未确认", "?"),
        };
        [format!("电脑控制：{state}"), format!("键鼠：{short}")]
            .into_iter()
            .find(|text| text.width() <= usize::from(width))
            .unwrap_or_default()
    }
}

impl State {
    pub(super) fn computer_command(&mut self, command: operator::Command) -> Result<(), String> {
        if self.preview {
            return Err("Preview cannot change computer controls; use the local terminal".into());
        }
        if self.computer.pending.len() >= 64 {
            return Err("Computer controls are busy; retry the local command".into());
        }
        self.computer.pending.push(command);
        self.notice = Some(PENDING_NOTICE.into());
        self.dirty = true;
        Ok(())
    }

    pub(super) fn computer_toggle(&mut self) {
        if let Err(error) = self.computer_command(operator::Command::Toggle) {
            self.notice = Some(error);
            self.dirty = true;
        }
    }

    pub(super) fn computer_badge(&self, width: u16) -> String {
        self.computer.badge(width)
    }
}

/// Every display loop checks this even without keyboard input. Preview never reads
/// or changes the real gate, and neither status reads nor writes hold State.
pub(super) fn refresh(shared: &Shared, now: Instant) {
    refresh_with(shared, now, operator::execute);
}

fn refresh_with(
    shared: &Shared,
    now: Instant,
    mut execute: impl FnMut(operator::Command) -> Result<Value, String>,
) {
    let (commands, expected_notice) = {
        let mut state = shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.preview
            || (state.computer.pending.is_empty()
                && state
                    .computer
                    .checked
                    .is_some_and(|checked| now.saturating_duration_since(checked) < POLL_INTERVAL))
        {
            return;
        }
        state.computer.checked = Some(now);
        (
            std::mem::take(&mut state.computer.pending),
            state.notice.clone(),
        )
    };
    let requested = !commands.is_empty();
    let mut status = None;
    let mut error = None;
    for command in commands {
        match execute(command) {
            Ok(value) => {
                status = Some(value);
                error = None;
            }
            Err(message) => {
                status = None;
                error = Some(message);
            }
        }
    }
    let status = status.unwrap_or_else(|| {
        execute(operator::Command::Status).unwrap_or_else(|message| {
            serde_json::json!({"enabled":false,"background_enabled":true,"error":message})
        })
    });
    let notice = requested.then(|| error.unwrap_or_else(|| operator::describe(&status)));
    let mut state = shared
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if state.computer.status.as_ref() != Some(&status) {
        state.computer.status = Some(status);
        state.dirty = true;
    }
    if let Some(notice) = notice
        && state.notice == expected_notice
        && state.computer.pending.is_empty()
    {
        state.notice = Some(notice);
        state.dirty = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn display() -> Shared {
        Shared {
            state: Mutex::new(State::new(24, 96, &Ui::default())),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        }
    }

    #[test]
    fn computer_idle_displays_refresh_from_one_fake_gate_without_holding_state() {
        let first = display();
        let second = display();
        let enabled = Cell::new(false);
        let calls = Cell::new(0);
        let execute = |command| {
            assert!(first.state.try_lock().is_ok());
            assert!(second.state.try_lock().is_ok());
            calls.set(calls.get() + 1);
            match command {
                operator::Command::On => enabled.set(true),
                operator::Command::Off => enabled.set(false),
                operator::Command::Toggle => enabled.set(!enabled.get()),
                operator::Command::Status => {}
            }
            Ok(serde_json::json!({"enabled":enabled.get(),"background_enabled":true}))
        };
        let now = Instant::now();
        refresh_with(&first, now, execute);
        refresh_with(&second, now, execute);
        assert_eq!(calls.get(), 2);
        refresh_with(&second, now + Duration::from_millis(100), execute);
        assert_eq!(calls.get(), 2);
        {
            let mut state = first.state.lock().unwrap();
            state.conversation.composer.set_text("unsent draft");
            state.computer_command(operator::Command::On).unwrap();
        }
        refresh_with(&first, now + Duration::from_millis(100), execute);
        assert!(enabled.get());
        refresh_with(&second, now + POLL_INTERVAL, execute);
        for shared in [&first, &second] {
            let state = shared.state.lock().unwrap();
            assert!(state.computer_badge(96).contains("电脑控制：开启"));
        }
        let state = first.state.lock().unwrap();
        assert!(enabled.get());
        assert!(state.computer.pending.is_empty());
        assert_eq!(state.conversation.composer.text(), "unsent draft");
    }

    #[test]
    fn computer_preview_never_reads_or_writes_the_gate_and_badges_fit() {
        let shared = display();
        {
            let mut state = shared.state.lock().unwrap();
            state.preview = true;
            assert!(state.computer_command(operator::Command::On).is_err());
            for width in 0..100 {
                assert!(state.computer_badge(width).width() <= usize::from(width));
            }
        }
        refresh_with(&shared, Instant::now(), |_| {
            panic!("preview touched shared gate")
        });
    }

    #[test]
    fn computer_shortcut_survives_every_read_split_and_ignores_repeat_release() {
        for sequence in [
            b"\x1b[99;11u".as_slice(),
            b"\x1b[99;11:1u\x1b[99;11:2u\x1b[99;11:3u",
            b"\x1b[99::99;203:1u",
        ] {
            for split in 0..=sequence.len() {
                let mut keyboard = Keyboard::default();
                let now = Instant::now();
                let mut output = Vec::new();
                let mut toggles = 0;
                for part in [&sequence[..split], &sequence[split..]] {
                    let (text, keys) = keyboard.feed(part, now, false, true, false);
                    output.extend(text);
                    for key in keys {
                        assert!(matches!(key, Key::ComputerToggle));
                        toggles += 1;
                    }
                }
                assert_eq!(toggles, 1, "{sequence:?} split {split}");
                assert!(output.is_empty(), "{output:?}");
            }
        }
        let mut keyboard = Keyboard::default();
        let now = Instant::now();
        let (output, keys) = keyboard.feed(b"\t\x1bc", now, false, true, false);
        assert_eq!(output, b"\t");
        assert_eq!(keys.len(), 1);
        assert!(matches!(keys[0], Key::Select));
    }

    #[test]
    fn computer_shortcut_is_literal_inside_bracketed_paste_at_every_split() {
        let bytes = b"\x1b[200~\x1b[99;11u\t\x1bc\x1b[201~";
        for split in 0..=bytes.len() {
            let mut keyboard = Keyboard::default();
            let now = Instant::now();
            let mut output = Vec::new();
            for part in [&bytes[..split], &bytes[split..]] {
                let (text, keys) = keyboard.feed(part, now, false, true, false);
                assert!(keys.is_empty());
                output.extend(text);
            }
            assert_eq!(output, bytes);
        }
    }

    #[test]
    fn computer_shortcut_queues_local_action_without_changing_editor_or_focus() {
        for focus in [false, true] {
            for selection in [false, true] {
                let mut state = State::new(24, 96, &Ui::default());
                state.conversation.composer.set_text("unfinished 界 draft");
                state.conversation.composer.home();
                state.conversation.composer.right();
                state.conversation.content_focus = focus;
                state.selection = selection;
                state.computer_toggle();
                assert_eq!(state.computer.pending, vec![operator::Command::Toggle]);
                assert_eq!(state.conversation.composer.text(), "unfinished 界 draft");
                assert_eq!(state.conversation.composer.cursor(), 1);
                assert_eq!(state.conversation.content_focus, focus);
                assert_eq!(state.selection, selection);
            }
        }
    }

    #[test]
    fn computer_controls_leave_tab_completion_and_focus_unchanged() {
        let mut state = State::new(36, 120, &Ui::default());
        state.preview = true;
        state.conversation.composer.set_text("/aud");
        state.chat_keyboard(b"\t", Instant::now(), false);
        assert_eq!(state.conversation.composer.text(), "/audit ");
        assert!(!state.conversation.content_focus);
        state.chat_keyboard(b"\t", Instant::now(), false);
        assert!(state.conversation.content_focus);
        state.chat_keyboard(b"\t", Instant::now(), false);
        assert!(!state.conversation.content_focus);
        assert!(state.computer.pending.is_empty());
        state.conversation.composer.set_text("/computer on");
        state.chat_keyboard(b"\r", Instant::now(), false);
        assert!(
            state
                .notice
                .as_deref()
                .unwrap_or_default()
                .contains("Preview")
        );
        assert!(state.computer.pending.is_empty());
    }
}
