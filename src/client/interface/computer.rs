//! Operator-only terminal commands. These functions are not remote UI actions.
use super::{harness, terminal};
use crate::client::language::{Language, localized};

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Status,
    Stop,
    Host(u64),
}

fn parse(text: &str) -> Result<Command, String> {
    let words = text.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] | ["status"] => Ok(Command::Status),
        ["stop"] => Ok(Command::Stop),
        ["host"] => Ok(Command::Host(15)),
        ["host", seconds]
            if !seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let seconds = seconds.parse::<u64>().map_err(|_| "use 1..=60 seconds")?;
            if (1..=60).contains(&seconds) {
                Ok(Command::Host(seconds))
            } else {
                Err("use 1..=60 seconds".into())
            }
        }
        _ => Err("Use /computer status, /computer host [1..60 seconds], or /computer stop".into()),
    }
}

/// This is called only by the local terminal's slash-command path, never by ui.action/message.
pub(crate) fn local_command(text: &str, language: Language) -> Result<String, String> {
    let command = parse(text)?;
    if !terminal::active() {
        return Err(language
            .text(
                "host handoff requires the local interactive ssmcp terminal",
                "主机输入授权需要本机交互式 ssmcp 终端",
            )
            .into());
    }
    let (runtime, _) = harness::local_runtime()?;
    match command {
        Command::Status => {
            let status = runtime.host_input_status();
            Ok(if status["enabled"] == true {
                localized!(
                    language,
                    "Host input handed over for up to {}s · input in this terminal returns control",
                    "主机输入已授权，最多 {} 秒 · 在此终端输入即可收回控制",
                    status["remaining_seconds"].as_u64().unwrap_or(0)
                )
            } else {
                language.text("Host input locked · isolated desktop and background browser control remain available", "主机输入已锁定 · 隔离桌面和后台浏览器仍可用").into()
            })
        }
        Command::Stop => {
            runtime.revoke_host_input();
            Ok(language
                .text(
                    "Host input locked · control returned",
                    "主机输入已锁定 · 已收回控制",
                )
                .into())
        }
        Command::Host(seconds) => {
            runtime.handoff_host_input_local(seconds)?;
            Ok(localized!(
                language,
                "Host input handed over for {seconds}s · any input in this terminal stops it · /computer stop",
                "主机输入已授权 {seconds} 秒 · 在此终端输入即可停止 · /computer stop"
            ))
        }
    }
}

/// Called on decoded local key/mouse/paste input. Tool output cannot invoke this path.
pub(crate) fn local_input() {
    if let Ok((runtime, _)) = harness::local_runtime() {
        runtime.revoke_host_input();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_handoff_command_defaults_to_fifteen_seconds_and_has_no_unbounded_mode() {
        assert_eq!(parse("host").unwrap(), Command::Host(15));
        assert_eq!(parse("host 1").unwrap(), Command::Host(1));
        assert_eq!(parse("host 60").unwrap(), Command::Host(60));
        for text in [
            "host 0",
            "host 61",
            "host unlimited",
            "host -1",
            "host 15 extra",
            "host 15; echo hi",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        assert_eq!(parse("").unwrap(), Command::Status);
        assert_eq!(parse("status").unwrap(), Command::Status);
        assert_eq!(parse("stop").unwrap(), Command::Stop);
    }
}
