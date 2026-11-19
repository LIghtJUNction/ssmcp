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
}

impl State {
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
                View::Queue => Hint::Queue,
                _ => return None,
            }
        };
        Some(shortcuts(hint, self.geometry.cols, status))
    }
}

fn shortcuts(hint: Hint, width: u16, status: &str) -> String {
    let alternatives = match hint {
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
            "↑ edit queued · @ files · $ skills · / commands · Alt+↑/↓ input height · F1 help",
            "@ files · $ skills · / commands",
            "@ files · $ skills · F1 help",
        ],
        Hint::Draft => [
            "Enter send · Ctrl+J newline · Ctrl+C clear · Alt+↑/↓ input height · Tab browse",
            "Enter send · Ctrl+J newline · Tab browse",
            "Enter send · Tab browse",
        ],
        Hint::Conversation => [
            "↑↓ select · Enter expand · PgUp/PgDn scroll · ←/→ panels · Tab input",
            "↑↓ select · Enter expand · Tab input",
            "Enter expand · Tab input",
        ],
        Hint::Audit => [
            "↑↓ browse · Enter details · PgUp/PgDn scroll · ←/→ panels · Tab input",
            "Enter details · PgUp/PgDn scroll · Tab input",
            "Enter details · Tab input",
        ],
        Hint::Queue => [
            "↑↓ select · Enter take back · i inspect · Delete cancel · Tab input",
            "Enter take back · i inspect · Tab input",
            "Enter take back · Tab input",
        ],
    };
    let available = usize::from(width.saturating_sub(1));
    let hint = alternatives
        .into_iter()
        .chain(["F1 help", "F1", "?"])
        .find(|text| text.width() <= available)
        .unwrap_or("");
    let with_status = format!("{hint} · {status}");
    if !hint.is_empty() && !status.is_empty() && with_status.width() <= available {
        with_status
    } else {
        hint.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_hints_match_editor_and_content_actions() {
        let menu = shortcuts(Hint::Completion, 100, "connected");
        assert!(menu.contains("Tab insert") && menu.contains("Esc close"));
        assert!(!menu.contains("Enter send"));
        let draft = shortcuts(Hint::Draft, 100, "connected");
        assert!(draft.contains("Enter send") && draft.contains("Ctrl+J"));
        assert!(draft.contains("Alt+↑/↓ input height"));
        let empty = shortcuts(Hint::EmptyInput, 100, "connected");
        assert!(empty.contains("@ files") && empty.contains("$ skills"));
        assert!(empty.contains("Alt+↑/↓ input height"));
        let queue = shortcuts(Hint::Queue, 100, "connected");
        assert!(queue.contains("i inspect") && queue.contains("Delete cancel"));
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
