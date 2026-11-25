//! Shared terminal message catalog. The environment is read once per process.
use std::sync::OnceLock;
use zi18n::{Catalog, Catalogs, Locale, Message};

const EN: &[Message<'static>] = &[
    Message::text("reminder.schedule", "Schedule reminder"),
    Message::text("reminder.pause", "Pause reminder"),
    Message::text("reminder.resume", "Resume reminder"),
    Message::text("reminder.cancel", "Cancel reminder"),
    Message::text("reminder.ack", "Mark reminder as processed"),
    Message::text("reminder.read", "Check for due reminders"),
    Message::text("reminder.list", "View scheduled reminders"),
    Message::text("reminder.status", "View reminder status"),
    Message::text("reminder.unknown", "Reminder operation"),
    Message::text("reminder.after", "after"),
    Message::text("reminder.every", "every"),
    Message::text("connection.preview", "preview · sample data"),
    Message::text("connection.offline", "disconnected"),
    Message::text(
        "connection.queue_update",
        "server update required for queue",
    ),
    Message::text("connection.online", "connected"),
    Message::text("shell.preview", "Preview has no live shell to change."),
    Message::text("shell.pending", "Checking shell preference"),
    Message::text("shell.current", "Current shell"),
    Message::text(
        "shell.usage",
        "Use /chsh auto, /chsh system or /chsh NAME|PATH.",
    ),
    Message::text("shell.changed", "Shell preference saved. Using"),
    Message::text("shell.next", "Applies to new terminals and ! commands."),
    Message::text("tip.help", "/help · show commands"),
    Message::text("tip.invite", "/invite · copy coordinator instructions"),
    Message::text("tip.worker_list", "/worker ls · show worker pool status"),
    Message::text(
        "tip.worker_invite",
        "/invite worker · copy instructions for an open station",
    ),
];
const ZH: &[Message<'static>] = &[
    Message::text("reminder.schedule", "安排定时提醒"),
    Message::text("reminder.pause", "暂停定时提醒"),
    Message::text("reminder.resume", "恢复定时提醒"),
    Message::text("reminder.cancel", "取消定时提醒"),
    Message::text("reminder.ack", "确认提醒已处理"),
    Message::text("reminder.read", "检查到期提醒"),
    Message::text("reminder.list", "查看定时提醒列表"),
    Message::text("reminder.status", "查看定时提醒状态"),
    Message::text("reminder.unknown", "定时提醒操作"),
    Message::text("reminder.after", "延后"),
    Message::text("reminder.every", "每隔"),
    Message::text("connection.preview", "预览 · 示例数据"),
    Message::text("connection.offline", "未连接"),
    Message::text("connection.queue_update", "队列需要更新服务端"),
    Message::text("connection.online", "已连接"),
    Message::text("shell.preview", "预览没有可切换的 shell。"),
    Message::text("shell.pending", "正在检查 shell 偏好"),
    Message::text("shell.current", "当前 shell"),
    Message::text(
        "shell.usage",
        "使用 /chsh auto、/chsh system 或 /chsh 名称或绝对路径。",
    ),
    Message::text("shell.changed", "已保存 shell 偏好，当前使用"),
    Message::text("shell.next", "适用于新终端及 ! 命令。"),
    Message::text("tip.help", "/help · 查看操作帮助"),
    Message::text("tip.invite", "/invite · 复制主 Agent 协作说明"),
    Message::text("tip.worker_list", "/worker ls · 查看 worker 池状态"),
    Message::text(
        "tip.worker_invite",
        "/invite worker · 复制空闲席位的接入说明",
    ),
];

pub(in crate::client) fn text(key: &'static str) -> &'static str {
    static REQUESTED: OnceLock<String> = OnceLock::new();
    let requested = REQUESTED.get_or_init(|| {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
            .map_or_else(|| "en".into(), |value| normalize(&value))
    });
    lookup(requested, key).unwrap_or(key)
}

fn normalize(locale: &str) -> String {
    let tag = locale.split(['.', '@']).next().unwrap_or_default();
    if matches!(tag, "" | "C" | "POSIX") {
        "en".into()
    } else {
        tag.replace('_', "-")
    }
}

fn lookup(requested: &str, key: &str) -> Option<&'static str> {
    static CATALOGS: OnceLock<[Catalog<'static>; 2]> = OnceLock::new();
    let en = Locale::parse("en").expect("English locale");
    let catalog = CATALOGS.get_or_init(|| {
        let zh = Locale::parse("zh").expect("Chinese locale");
        [
            Catalog::new(en, EN).expect("English terminal messages"),
            Catalog::new(zh, ZH).expect("Chinese terminal messages"),
        ]
    });
    let fallback = [en];
    Catalogs::new(catalog, &fallback)
        .ok()?
        .lookup(Locale::parse(requested).unwrap_or(en), key)
        .ok()?
        .text()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_catalog_uses_language_and_english_fallback() {
        assert_eq!(
            lookup(&normalize("zh_CN.UTF-8"), "connection.online"),
            Some("已连接")
        );
        assert_eq!(
            lookup(&normalize("C.UTF-8"), "connection.online"),
            Some("connected")
        );
        assert_eq!(
            lookup("fr-CA", "connection.queue_update"),
            Some("server update required for queue")
        );
    }
}
