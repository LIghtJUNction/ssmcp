//! Shortcuts describe the active editor or content pane without changing bindings.
use super::*;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug)]
enum Hint {
    Completion,
    Loading,
    EmptyInput,
    Draft,
    Conversation,
    Audit,
    Queue,
    QueueInspect,
}

const IDLE_DELAY: Duration = Duration::from_secs(5);
const TIP_PERIOD: Duration = Duration::from_secs(8);

#[derive(Default)]
pub(super) struct IdleTips {
    last: Option<Instant>,
    elapsed: Duration,
}

impl IdleTips {
    pub(super) fn pause(&mut self) {
        self.last = None;
    }

    pub(super) fn tick(&mut self, now: Instant, eligible: bool) -> Option<u64> {
        if !eligible {
            self.pause();
            return None;
        }
        if let Some(previous) = self.last.replace(now) {
            self.elapsed = self
                .elapsed
                .saturating_add(now.saturating_duration_since(previous));
        }
        let visible = self.elapsed.checked_sub(IDLE_DELAY)?;
        Some(visible.as_secs() / TIP_PERIOD.as_secs())
    }
}

pub(super) fn idle_tip_key(index: u64, pool: Option<&Value>) -> &'static str {
    let live = pool.filter(|pool| pool["connected"] == true && pool["closed"] != true);
    let can_invite_worker = live.is_some_and(|pool| {
        pool["stopping"] != true
            && pool["retiring"].as_u64() == Some(0)
            && matches!(
                (pool["registered_workers"].as_u64(), pool["desired_total"].as_u64(), pool["total"].as_u64()),
                (Some(registered), Some(desired), Some(total))
                    if total == desired && registered < desired
            )
    });
    let tips: &[&str] = if can_invite_worker {
        &[
            "tip.help",
            "tip.invite",
            "tip.worker_list",
            "tip.worker_invite",
        ]
    } else if live.is_some() {
        &["tip.help", "tip.invite", "tip.worker_list"]
    } else {
        &["tip.help", "tip.invite"]
    };
    tips[(index % tips.len() as u64) as usize]
}

impl State {
    pub(super) fn chat_idle_tip(
        &mut self,
        now: Instant,
        connected: bool,
        priority: bool,
    ) -> Option<&'static str> {
        let eligible = connected
            && !priority
            && !self.preview
            && !self.help
            && !self.selection
            && !self.sleep_wants_overlay()
            && self.conversation.view == View::Activity
            && !self.conversation.content_focus
            && self.conversation.composer.is_empty()
            && !self.chat_menu_visible()
            && !self.conversation.composer.completion_loading()
            && !self.keyboard.pasting;
        let index = self.conversation.idle_tips.tick(now, eligible)?;
        let pool = crate::client::worker_window::linked_snapshot();
        Some(crate::client::terminal::locale::text(idle_tip_key(
            index,
            pool.as_ref(),
        )))
    }

    pub(super) fn chat_contextual_hint(&self, status: &str) -> Option<String> {
        if self.help
            || self.selection
            || (self.conversation.logs_open && self.conversation.content_focus)
            || self.conversation.view == View::Questions
        {
            return None;
        }
        let hint = if !self.conversation.content_focus {
            if self.chat_menu_visible() {
                Hint::Completion
            } else if self.conversation.composer.completion_loading() {
                Hint::Loading
            } else if self.conversation.composer.is_empty() {
                Hint::EmptyInput
            } else {
                Hint::Draft
            }
        } else {
            match self.conversation.view {
                View::Activity => Hint::Conversation,
                View::Audit => Hint::Audit,
                View::Queue => {
                    if self.chat_data()["queue"]
                        .as_array()
                        .and_then(|entries| entries.get(self.conversation.selected))
                        .is_some_and(|entry| Receipt::from_entry(entry).editable())
                    {
                        Hint::Queue
                    } else {
                        Hint::QueueInspect
                    }
                }
                _ => return None,
            }
        };
        Some(shortcuts(hint, self.geometry.cols, status))
    }
}

fn shortcuts(hint_kind: Hint, width: u16, status: &str) -> String {
    let alternatives = match hint_kind {
        Hint::Completion => [
            "↑↓ choose · Tab insert · Esc close · /help",
            "↑↓ choose · Tab insert · Esc close",
            "Tab insert · Esc close",
        ],
        Hint::Loading => [
            "Loading references · keep typing · Esc close",
            "Loading references · Esc close",
            "Loading… · Esc close",
        ],
        Hint::EmptyInput => [
            "Alt+C select/copy · ↑ edit queued · Alt+↑/↓ input height · @ files · $ skills · / commands",
            "Alt+C select/copy · ↑ edit queued · @ files · / commands",
            "Alt+C select/copy · ↑ edit queued · F1 help",
        ],
        Hint::Draft => [
            "Enter send · Ctrl+J newline · Ctrl+C clear · Alt+C select/copy · Alt+↑/↓ input height",
            "Enter send · Ctrl+J newline · Ctrl+C clear · Alt+C select/copy",
            "Enter send · Alt+C select/copy · F1 help",
        ],
        Hint::Conversation => [
            "↑↓ select · Enter expand · f full-screen tool · PgUp/PgDn scroll · Alt+C select/copy · Tab input",
            "Enter expand · f full-screen tool · Tab input",
            "Enter expand · f tool · Tab",
        ],
        Hint::Audit => [
            "Alt+C select/copy · ↑↓ browse · Enter details · PgUp/PgDn scroll · ←/→ panels · Tab input",
            "Alt+C select/copy · Enter details · PgUp/PgDn scroll · Tab input",
            "Alt+C select/copy · Enter details · Tab input",
        ],
        Hint::Queue => [
            "Alt+C select/copy · ↑↓ select · Enter edit · i inspect · Delete cancel · Tab input",
            "Alt+C select/copy · Enter edit · i inspect · Tab input",
            "Alt+C select/copy · Enter edit · Tab input",
        ],
        Hint::QueueInspect => [
            "Alt+C select/copy · ↑↓ select · Enter inspect · i inspect · Tab input",
            "Alt+C select/copy · Enter inspect · Tab input",
            "Enter inspect · Tab input",
        ],
    };
    let available = usize::from(width.saturating_sub(1));
    let hint = alternatives
        .into_iter()
        .chain(["F1 help", "F1", "?"])
        .find(|text| text.width() <= available)
        .unwrap_or("");
    let shortcut = format!("{hint} · Super+Alt+C computer");
    let hint = if !matches!(hint_kind, Hint::Completion | Hint::Loading)
        && !hint.is_empty()
        && shortcut.width() <= available
    {
        shortcut
    } else {
        hint.to_owned()
    };
    let with_status = format!("{hint} · {status}");
    if !hint.is_empty() && !status.is_empty() && with_status.width() <= available {
        with_status
    } else {
        hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_idle_tip_yields_to_notice_draft_and_copy_mode() {
        let mut state = State::new(24, 100, &Ui::default());
        let start = Instant::now();
        assert!(state.chat_idle_tip(start, true, false).is_none());
        let mature = start + IDLE_DELAY;
        assert!(
            state
                .chat_idle_tip(mature, true, false)
                .unwrap()
                .starts_with("/help")
        );
        assert!(state.chat_idle_tip(mature, true, true).is_none());
        state.conversation.composer.set_text("unfinished");
        assert!(
            state
                .chat_idle_tip(mature + TIP_PERIOD, true, false)
                .is_none()
        );
        assert_eq!(state.conversation.composer.text(), "unfinished");
        state.conversation.composer.set_text("");
        state.selection = true;
        assert!(
            state
                .chat_idle_tip(mature + TIP_PERIOD, true, false)
                .is_none()
        );
        state.selection = false;
        assert!(
            state
                .chat_idle_tip(mature + TIP_PERIOD, false, false)
                .is_none()
        );
    }

    #[test]
    fn idle_tips_rotate_after_the_delay_and_pause_without_catching_up() {
        let start = Instant::now();
        let mut tips = IdleTips::default();
        assert_eq!(tips.tick(start, true), None);
        assert_eq!(tips.tick(start + IDLE_DELAY, true), Some(0));
        assert_eq!(tips.tick(start + IDLE_DELAY + TIP_PERIOD, true), Some(1));
        assert_eq!(tips.tick(start + Duration::from_secs(14), false), None);
        let resume = start + Duration::from_secs(100);
        assert_eq!(tips.tick(resume, true), Some(1));
        assert_eq!(tips.tick(resume + TIP_PERIOD, true), Some(2));
    }

    #[test]
    fn idle_tips_use_only_current_worker_capabilities() {
        for index in 0..8 {
            assert!(matches!(
                idle_tip_key(index, None),
                "tip.help" | "tip.invite"
            ));
        }
        let mut pool = json!({"connected":true,"total":2,"desired_total":2,
                              "retiring":0,"registered_workers":1});
        assert_eq!(idle_tip_key(2, Some(&pool)), "tip.worker_list");
        assert_eq!(idle_tip_key(3, Some(&pool)), "tip.worker_invite");
        pool["registered_workers"] = json!(2);
        assert_eq!(idle_tip_key(3, Some(&pool)), "tip.help");
        pool["connected"] = json!(false);
        assert_eq!(idle_tip_key(2, Some(&pool)), "tip.help");
    }

    #[test]
    fn shortcut_hints_match_editor_and_content_actions() {
        let menu = shortcuts(Hint::Completion, 100, "connected");
        assert!(menu.contains("Tab insert") && menu.contains("Esc close"));
        assert!(!menu.contains("Enter send"));
        let draft = shortcuts(Hint::Draft, 100, "connected");
        assert!(draft.contains("Enter send") && draft.contains("Ctrl+J"));
        assert!(draft.contains("Alt+↑/↓ input height") && draft.contains("Alt+C select/copy"));
        let empty = shortcuts(Hint::EmptyInput, 100, "connected");
        assert!(empty.contains("@ files") && empty.contains("$ skills"));
        assert!(empty.contains("Alt+↑/↓ input height") && empty.contains("Alt+C select/copy"));
        assert!(shortcuts(Hint::Conversation, 32, "").contains("Enter expand"));
        let narrow = shortcuts(Hint::Conversation, 48, "");
        assert!(narrow.contains("Enter expand") && narrow.contains("f full-screen tool"));
        let queue = shortcuts(Hint::Queue, 100, "connected");
        assert!(queue.contains("i inspect") && queue.contains("Delete cancel"));
        let delivered = shortcuts(Hint::QueueInspect, 100, "connected");
        assert!(delivered.contains("Enter inspect"));
        assert!(!delivered.contains("edit") && !delivered.contains("cancel"));
    }

    #[test]
    fn shortcut_hints_fit_small_display_widths() {
        for width in [0, 1, 2, 3, 8, 16, 32, 60, 100] {
            for hint in [
                Hint::Completion,
                Hint::Loading,
                Hint::EmptyInput,
                Hint::Draft,
                Hint::Conversation,
                Hint::Audit,
                Hint::Queue,
                Hint::QueueInspect,
            ] {
                let text = shortcuts(hint, width, "server update required for queue");
                assert!(
                    text.width() <= usize::from(width.saturating_sub(1)),
                    "{width}: {text}"
                );
                assert!(!text.chars().any(char::is_control));
            }
        }
    }
}
