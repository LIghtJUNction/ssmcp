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
        Some(shortcuts_in(
            hint,
            self.geometry.cols,
            status,
            self.ui.language,
        ))
    }
}

#[cfg(test)]
fn shortcuts(hint: Hint, width: u16, status: &str) -> String {
    shortcuts_in(hint, width, status, Language::English)
}

fn shortcuts_in(hint: Hint, width: u16, status: &str, language: Language) -> String {
    let alternatives = match hint {
        Hint::Completion => [
            language.text(
                "↑↓ choose · Tab insert · Esc close · /help",
                "↑↓ 选择 · Tab 插入 · Esc 关闭 · /help",
            ),
            language.text(
                "↑↓ choose · Tab insert · Esc close",
                "↑↓ 选择 · Tab 插入 · Esc 关闭",
            ),
            language.text("Tab insert · Esc close", "Tab 插入 · Esc 关闭"),
        ],
        Hint::Loading => [
            language.text(
                "Loading references · keep typing · Esc close",
                "加载引用中 · 可继续输入 · Esc 关闭",
            ),
            language.text("Loading references · Esc close", "加载引用中 · Esc 关闭"),
            language.text("Loading… · Esc close", "加载中… · Esc 关闭"),
        ],
        Hint::EmptyInput => [
            language.text(
                "F7 copy · ↑ edit queued · Alt+↑/↓ input height · @ files · $ skills · / commands",
                "F7 复制 · ↑ 编辑待发项 · Alt+↑/↓ 输入框高度 · @ 文件 · $ 技能 · / 命令",
            ),
            language.text(
                "F7 copy · ↑ edit queued · @ files · / commands",
                "F7 复制 · ↑ 编辑待发项 · @ 文件 · / 命令",
            ),
            language.text(
                "F7 copy · ↑ edit queued · F1 help",
                "F7 复制 · ↑ 编辑待发项 · F1 帮助",
            ),
        ],
        Hint::Draft => [
            language.text(
                "Enter send · Ctrl+J newline · Ctrl+C clear · F7 copy · Alt+↑/↓ input height",
                "Enter 发送 · Ctrl+J 换行 · Ctrl+C 清空 · F7 复制 · Alt+↑/↓ 高度",
            ),
            language.text(
                "Enter send · Ctrl+J newline · Ctrl+C clear · F7 copy",
                "Enter 发送 · Ctrl+J 换行 · Ctrl+C 清空 · F7 复制",
            ),
            language.text(
                "Enter send · F7 copy · F1 help",
                "Enter 发送 · F7 复制 · F1 帮助",
            ),
        ],
        Hint::Conversation => [
            language.text(
                "F7 copy · ↑↓ select · Enter expand · PgUp/PgDn scroll · ←/→ panels · Tab input",
                "F7 复制 · ↑↓ 选择 · Enter 展开 · PgUp/PgDn 滚动 · ←/→ 面板 · Tab 输入",
            ),
            language.text(
                "F7 copy · ↑↓ select · Enter expand · Tab input",
                "F7 复制 · ↑↓ 选择 · Enter 展开 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter expand · Tab input",
                "F7 复制 · Enter 展开 · Tab 输入",
            ),
        ],
        Hint::Audit => [
            language.text(
                "F7 copy · ↑↓ browse · Enter details · PgUp/PgDn scroll · ←/→ panels · Tab input",
                "F7 复制 · ↑↓ 浏览 · Enter 详情 · PgUp/PgDn 滚动 · ←/→ 面板 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter details · PgUp/PgDn scroll · Tab input",
                "F7 复制 · Enter 详情 · PgUp/PgDn 滚动 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter details · Tab input",
                "F7 复制 · Enter 详情 · Tab 输入",
            ),
        ],
        Hint::Queue => [
            language.text(
                "F7 copy · ↑↓ select · Enter edit · i inspect · Delete cancel · Tab input",
                "F7 复制 · ↑↓ 选择 · Enter 编辑 · i 查看 · Delete 取消 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter edit · i inspect · Tab input",
                "F7 复制 · Enter 编辑 · i 查看 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter edit · Tab input",
                "F7 复制 · Enter 编辑 · Tab 输入",
            ),
        ],
        Hint::QueueInspect => [
            language.text(
                "F7 copy · ↑↓ select · Enter inspect · i inspect · Tab input",
                "F7 复制 · ↑↓ 选择 · Enter 查看 · i 查看 · Tab 输入",
            ),
            language.text(
                "F7 copy · Enter inspect · Tab input",
                "F7 复制 · Enter 查看 · Tab 输入",
            ),
            language.text("Enter inspect · Tab input", "Enter 查看 · Tab 输入"),
        ],
    };
    let available = usize::from(width.saturating_sub(1));
    let hint = alternatives
        .into_iter()
        .chain([language.text("F1 help", "F1 帮助"), "F1", "?"])
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
        assert!(draft.contains("Alt+↑/↓ input height") && draft.contains("F7 copy"));
        let empty = shortcuts(Hint::EmptyInput, 100, "connected");
        assert!(empty.contains("@ files") && empty.contains("$ skills"));
        assert!(empty.contains("Alt+↑/↓ input height") && empty.contains("F7 copy"));
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
