//! Local presentation language. Protocol keys and user/tool content stay unchanged.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh")]
    Chinese,
}

/// Both format strings are checked at compile time. Values are never translated.
macro_rules! localized {
    ($language:expr, $english:literal, $chinese:literal $(, $($args:tt)*)?) => {
        match $language {
            $crate::client::language::Language::English => format!($english $(, $($args)*)?),
            $crate::client::language::Language::Chinese => format!($chinese $(, $($args)*)?),
        }
    };
}
pub(crate) use localized;

impl Language {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "en" => Ok(Self::English),
            "zh" => Ok(Self::Chinese),
            _ => Err("language must be en or zh".into()),
        }
    }

    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Chinese => "zh",
        }
    }

    pub(in crate::client) fn receipt(
        self,
        receipt: super::steering::receipt::Receipt,
    ) -> &'static str {
        use super::steering::receipt::Receipt::*;
        self.text(
            receipt.label(),
            match receipt {
                Waiting => "待发送",
                Sending => "确认中",
                Offline => "离线",
                Editing => "编辑中",
                Recalling => "取回中",
                Cancelling => "取消中",
                Cancelled => "已取消",
                Review => "需要检查",
                Unconfirmed => "送达未确认",
                Read => "已读",
            },
        )
    }

    /// Translate known machine state labels at presentation call sites only.
    pub(crate) fn state_label(self, state: &str) -> &str {
        self.text(
            state,
            match state {
                "running" => "运行中",
                "working" => "工作中",
                "completed" | "finished" => "已完成",
                "exited" => "已退出",
                "cancelled" => "已取消",
                "failed" => "失败",
                "waiting" => "等待中",
                "queued" => "待发送",
                "editing" => "编辑中",
                "sending" => "发送中",
                "delivered" => "已送达",
                "available" => "可用",
                "idle" => "空闲",
                "unknown" => "未知",
                "offline" => "离线",
                "disconnected" => "已断开",
                "connected" => "已连接",
                "starting" => "启动中",
                "stopped" => "已停止",
                "error" => "错误",
                "timeout" => "超时",
                "done" => "已完成",
                "received" => "已收到",
                _ => state,
            },
        )
    }

    /// Exact local UI notices only. Unknown external details remain verbatim.
    pub(crate) fn local_message(self, message: &str) -> String {
        match message {
            "UI configuration path is unavailable; use --ui-config FILE" => self.text("UI configuration path is unavailable; use --ui-config FILE", "UI 配置路径不可用；请使用 --ui-config FILE").into(),
            "cannot read UI configuration" => self.text("cannot read UI configuration", "无法读取 UI 配置").into(),
            "cannot serialize client configuration" => self.text("cannot serialize client configuration", "无法序列化客户端配置").into(),
            "cannot create UI configuration directory" => self.text("cannot create UI configuration directory", "无法创建 UI 配置目录").into(),
            "cannot write UI configuration" => self.text("cannot write UI configuration", "无法写入 UI 配置").into(),
            "cannot flush UI configuration" => self.text("cannot flush UI configuration", "无法同步 UI 配置到磁盘").into(),
            "cannot replace UI configuration" => self.text("cannot replace UI configuration", "无法替换 UI 配置").into(),
            "host-input handoff is not available on this platform" => self.text("host-input handoff is not available on this platform", "当前平台不支持交出主机输入控制").into(),
            "GitHub login link copied to the system clipboard" => self.text("GitHub login link copied to the system clipboard", "GitHub 登录链接已复制到系统剪贴板").into(),
            "GitHub sign-in opened in your browser" => self.text("GitHub sign-in opened in your browser", "已在浏览器中打开 GitHub 登录").into(),
            "Logout stopped before its result was confirmed" => self.text("Logout stopped before its result was confirmed", "退出登录任务已停止，结果尚未确认").into(),
            "Use /allow, /allow USER, or /allow remove USER" => self.text("Use /allow, /allow USER, or /allow remove USER", "请使用 /allow、/allow USER 或 /allow remove USER").into(),
            "Use /login, /login copy, or /login link" => self.text("Use /login, /login copy, or /login link", "请使用 /login、/login copy 或 /login link").into(),
            "language must be en or zh" => self.text("language must be en or zh", "语言必须为 en 或 zh").into(),
            "发送失败：没有程序在监听" => self.text("Send failed: no program is listening", "发送失败：没有程序在监听").into(),
            "发送结果未确认：已尝试投递，但未收到监听程序的接收确认；不会自动重发" => self.text("Delivery unconfirmed: offered to the listener, but no acknowledgement arrived; it will not be resent automatically", "发送结果未确认：已尝试投递，但未收到监听程序的接收确认；不会自动重发").into(),
            "发送失败：监听程序已离线，消息未投递" => self.text("Send failed: the listener is offline; the message was not delivered", "发送失败：监听程序已离线，消息未投递").into(),
            "发送失败：监听程序没有及时领取消息" => self.text("Send failed: the listener did not claim the message in time", "发送失败：监听程序没有及时领取消息").into(),
            "发送失败：连接已结束，消息未投递" => self.text("Send failed: the connection ended; the message was not delivered", "发送失败：连接已结束，消息未投递").into(),
            "发送失败：监听程序与当前选择的线程不同；请选择对应线程，草稿已保留" => self.text("Send failed: the listener belongs to another thread; choose that thread. Draft preserved", "发送失败：监听程序与当前选择的线程不同；请选择对应线程，草稿已保留").into(),
            "发送失败：上一条消息仍在等待监听程序确认，草稿已保留" => self.text("Send failed: the previous message is awaiting acknowledgement. Draft preserved", "发送失败：上一条消息仍在等待监听程序确认，草稿已保留").into(),
            "发送失败：监听程序或连接已改变，草稿已保留" => self.text("Send failed: the listener or connection changed. Draft preserved", "发送失败：监听程序或连接已改变，草稿已保留").into(),
            "监听程序已接收" => self.text("The listener accepted the message", "监听程序已接收").into(),
            "发送失败：监听程序已退出，消息未投递" => self.text("Send failed: the listener exited; the message was not delivered", "发送失败：监听程序已退出，消息未投递").into(),
            "发送已取消：消息未投递" => self.text("Send cancelled: the message was not delivered", "发送已取消：消息未投递").into(),
            "发送结果未确认：接收通道已结束，不会自动重发" => self.text("Delivery unconfirmed: the receiving channel ended; it will not be resent automatically", "发送结果未确认：接收通道已结束，不会自动重发").into(),
            "发送结果未确认：发送任务已结束，不会自动重发" => self.text("Delivery unconfirmed: the sending task ended; it will not be resent automatically", "发送结果未确认：发送任务已结束，不会自动重发").into(),
            "用法：/wait cancel <等待编号或唯一前缀>" => self.text("Use /wait cancel <wait ID or unique prefix>", "用法：/wait cancel <等待编号或唯一前缀>").into(),
            "没有匹配的活动等待；未取消任何请求" => self.text("No matching active wait; nothing was cancelled", "没有匹配的活动等待；未取消任何请求").into(),
            "等待编号前缀不唯一；未取消任何请求，请使用完整编号" => self.text("The wait ID prefix is ambiguous; nothing was cancelled. Use the full ID", "等待编号前缀不唯一；未取消任何请求，请使用完整编号").into(),
            _ => {
                if let Some(reason) = message.strip_prefix("发送失败：监听程序拒绝接收（").and_then(|text| text.strip_suffix('）')) {
                    return localized!(self, "Send failed: the listener rejected the message ({reason})", "发送失败：监听程序拒绝接收（{reason}）");
                }
                message.into()
            }
        }
    }

    pub(crate) const fn text<'a>(self, english: &'a str, chinese: &'a str) -> &'a str {
        match self {
            Self::English => english,
            Self::Chinese => chinese,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_are_stable_and_default_to_english() {
        assert_eq!(Language::default(), Language::English);
        for (code, language) in [("en", Language::English), ("zh", Language::Chinese)] {
            assert_eq!(Language::parse(code).unwrap(), language);
            assert_eq!(language.code(), code);
            assert_eq!(serde_json::to_value(language).unwrap(), code);
        }
        for invalid in ["", "fr", "EN", "zh-CN", "中文", " en"] {
            assert!(Language::parse(invalid).is_err());
        }
    }

    #[test]
    fn language_formatting_preserves_raw_values_and_unicode() {
        let raw = "English 中文 /tmp/Tasks/queue.json";
        assert_eq!(
            localized!(Language::English, "Saved {raw}", "已保存 {raw}"),
            format!("Saved {raw}")
        );
        assert_eq!(
            localized!(Language::Chinese, "Saved {raw}", "已保存 {raw}"),
            format!("已保存 {raw}")
        );
        assert_eq!(
            localized!(Language::Chinese, "{} items", "{} 项", 3),
            "3 项"
        );
        assert_eq!(
            localized!(Language::English, "{n} items", "{n} 项", n = 3),
            "3 items"
        );
    }
}
