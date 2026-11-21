//! Manual compatibility state for legacy SSMCP providers, not standalone Zuse.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Status,
    On,
    Off,
    Toggle,
}

impl Action {
    pub(crate) fn parse(arguments: &str) -> Result<Self, String> {
        match arguments {
            "" | "status" => Ok(Self::Status),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            "toggle" => Ok(Self::Toggle),
            _ => Err(
                "旧版 SSMCP 兼容开关：使用 /computer [status|on|off|toggle] · 不控制独立 Zuse CLI"
                    .into(),
            ),
        }
    }

    pub(crate) fn queued(self) -> &'static str {
        match self {
            Self::Status => "computer.status",
            Self::On => "computer.on",
            Self::Off => "computer.off",
            Self::Toggle => "computer.toggle",
        }
    }

    pub(crate) fn from_queued(action: &str) -> Option<Self> {
        match action {
            "computer.status" => Some(Self::Status),
            "computer.on" => Some(Self::On),
            "computer.off" => Some(Self::Off),
            "computer.toggle" => Some(Self::Toggle),
            _ => None,
        }
    }
}

/// Read only: observing this state must never initialize the legacy gate.
pub(crate) fn snapshot() -> Value {
    crate::computer_control::status()
}

/// Called only by explicit local CLI commands or the terminal's manual queue.
pub(crate) fn apply(action: Action) -> Result<Value, String> {
    let result = match action {
        Action::Status => Ok(snapshot()),
        Action::On => crate::computer_control::set_local(true),
        Action::Off => crate::computer_control::set_local(false),
        Action::Toggle => crate::computer_control::toggle_local(),
    };
    result.map_err(|error| format!("旧版 SSMCP 兼容开关：操作失败 · 不控制独立 Zuse CLI · {error}"))
}

pub(crate) fn describe(state: &Value) -> String {
    let mode = match (
        state["available"].as_bool(),
        state["mode"].as_str(),
        state["enabled"].as_bool(),
    ) {
        (Some(true), Some("on"), Some(true)) => "开启",
        (Some(true), Some("off"), Some(false)) => "关闭",
        (Some(false), Some("unset"), _) => "未设置（无有效旧记录）",
        _ => "不可用",
    };
    let mut message = format!("旧版 SSMCP 兼容开关：{mode} · 不控制独立 Zuse CLI");
    if state["scoped_observations_disabled"] == true {
        message.push_str(" · 已关闭本机限定目标观察");
    }
    if let Some(error) = state["error"].as_str() {
        message.push_str(" · ");
        message.push_str(&crate::client::panel::safe_text(error));
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn manual_actions_are_exact_and_never_accept_native_operations() {
        for (arguments, action) in [
            ("", Action::Status),
            ("status", Action::Status),
            ("on", Action::On),
            ("off", Action::Off),
            ("toggle", Action::Toggle),
        ] {
            assert_eq!(Action::parse(arguments).unwrap(), action);
            assert_eq!(Action::from_queued(action.queued()), Some(action));
        }
        for arguments in ["on extra", "click", "type", "version", "status --json"] {
            assert!(Action::parse(arguments).is_err());
        }
        assert_eq!(Action::from_queued("computer.click"), None);
        assert_eq!(Action::from_queued("theme.next"), None);
    }

    #[test]
    fn missing_or_invalid_legacy_state_never_claims_that_zuse_is_off() {
        for (state, label) in [
            (json!({"available":true,"enabled":true,"mode":"on"}), "开启"),
            (
                json!({"available":true,"enabled":false,"mode":"off"}),
                "关闭",
            ),
            (
                json!({"available":false,"enabled":false,"mode":"unset"}),
                "未设置",
            ),
            (
                json!({"available":false,"enabled":false,"mode":"unavailable"}),
                "不可用",
            ),
            (
                json!({"available":true,"enabled":false,"mode":"on"}),
                "不可用",
            ),
        ] {
            let message = describe(&state);
            assert!(message.starts_with("旧版 SSMCP 兼容开关："));
            assert!(message.contains(label));
            assert!(message.contains("不控制独立 Zuse CLI"));
        }
    }
}
