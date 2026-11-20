//! Operator-only controls. None of these commands is registered as a remote UI action.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Status,
    On,
    Off,
    Toggle,
}

pub(crate) fn parse(text: &str) -> Result<Command, String> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] | ["status"] => Ok(Command::Status),
        ["on"] => Ok(Command::On),
        ["off"] | ["stop"] => Ok(Command::Off),
        ["toggle"] => Ok(Command::Toggle),
        _ => Err("Use /computer on, /computer off, /computer toggle, or /computer status".into()),
    }
}

/// Execute only a decoded local command or shortcut, outside the terminal State lock.
/// This also serves attached frontends: the shared gate belongs to this OS user,
/// independently of which connection currently owns a Runtime.
pub(crate) fn execute(command: Command) -> Result<Value, String> {
    if let Ok((runtime, _)) = super::harness::local_runtime() {
        return match command {
            Command::Status => Ok(runtime.host_input_status()),
            Command::On => runtime.set_host_input_local(true),
            Command::Off => runtime.set_host_input_local(false),
            Command::Toggle => runtime.toggle_host_input_local(),
        };
    }
    match command {
        Command::Status => Ok(crate::computer::host_input_status()),
        Command::On => crate::computer::set_host_input_local(true),
        Command::Off => crate::computer::set_host_input_local(false),
        Command::Toggle => crate::computer::toggle_host_input_local(),
    }
}

pub(crate) fn describe(status: &Value) -> String {
    if let Some(error) = status["error"].as_str() {
        return format!("Foreground off · background on · shared control unavailable: {error}");
    }
    format!(
        "Foreground {} · background on · shared by this OS user's ssmcp sessions · Super+Alt+C toggles",
        if status["enabled"] == true {
            "on"
        } else {
            "off"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn computer_commands_are_explicit_and_reject_expired_lease_syntax() {
        assert_eq!(parse("").unwrap(), Command::Status);
        assert_eq!(parse("status").unwrap(), Command::Status);
        assert_eq!(parse("on").unwrap(), Command::On);
        assert_eq!(parse("off").unwrap(), Command::Off);
        assert_eq!(parse("stop").unwrap(), Command::Off);
        assert_eq!(parse("toggle").unwrap(), Command::Toggle);
        for text in [
            "host",
            "host 15",
            "on 15",
            "on extra",
            "on; echo hi",
            "enable",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn computer_status_distinguishes_shared_foreground_from_background() {
        let on = describe(&json!({"enabled":true,"background_enabled":true}));
        assert!(on.contains("Foreground on") && on.contains("background on"));
        assert!(on.contains("OS user") && on.contains("Super+Alt+C"));
        assert!(!on.contains("seconds") && !on.contains("any input"));
        let failed = describe(&json!({"enabled":false,"error":"cannot read state"}));
        assert!(failed.contains("Foreground off") && failed.contains("unavailable"));
    }
}
