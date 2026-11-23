//! Shared terminal message catalog. The environment is read once per process.
use std::sync::OnceLock;
use zi18n::{Catalog, Catalogs, Locale, Message};

const EN: &[Message<'static>] = &[
    Message::text("connection.preview", "preview · sample data"),
    Message::text("connection.offline", "disconnected"),
    Message::text(
        "connection.queue_update",
        "server update required for queue",
    ),
    Message::text("connection.online", "connected"),
];
const ZH: &[Message<'static>] = &[
    Message::text("connection.preview", "预览 · 示例数据"),
    Message::text("connection.offline", "未连接"),
    Message::text("connection.queue_update", "队列需要更新服务端"),
    Message::text("connection.online", "已连接"),
];

pub(super) fn text(key: &'static str) -> &'static str {
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
