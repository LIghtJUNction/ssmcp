//! Local commands used by terminals and desktop shortcut bindings.
use std::ffi::OsString;

use super::interface::computer::{self, Command};

const HELP: &str = "\
Usage: ssmcp computer [on|off|toggle|status] [--json]

统一管理 SSMCP 的电脑、浏览器和系统界面操作。
这台电脑上同一系统用户的 ssmcp 会话共用开关，直到你再次更改。
关闭后仍可观察状态和手动操作。Shell 与独立程序仍使用原有系统权限。

  on       开启 Computer Use
  off      关闭 Computer Use
  toggle   切换电脑控制开关
  status   查看当前开关状态（默认）

Bind Super+Alt+C in your desktop to: ssmcp computer toggle
In terminals with enhanced keyboard reporting, the same shortcut works in chat.
";

#[derive(Clone, Copy)]
enum Request {
    Help,
    Action { command: Command, json: bool },
}

fn parse(args: &[OsString]) -> Result<Request, String> {
    let mut action = None;
    let mut json = false;
    for arg in args {
        let value = arg.to_str().ok_or("computer arguments must be UTF-8")?;
        match value {
            "-h" | "--help" if args.len() == 1 => return Ok(Request::Help),
            "--json" if !json => json = true,
            "on" | "off" | "toggle" | "status" if action.is_none() => {
                action = Some(match value {
                    "on" => Command::On,
                    "off" => Command::Off,
                    "toggle" => Command::Toggle,
                    _ => Command::Status,
                });
            }
            _ => {
                return Err(format!(
                    "unknown or repeated computer argument: {value}; use ssmcp computer --help"
                ));
            }
        }
    }
    Ok(Request::Action {
        command: action.unwrap_or(Command::Status),
        json,
    })
}

pub(crate) fn dispatch(args: &[OsString]) -> crate::Result<bool> {
    if !args.first().is_some_and(|arg| arg == "computer") {
        return Ok(false);
    }
    match parse(&args[1..])? {
        Request::Help => print!("{HELP}"),
        Request::Action { command, json } => {
            let status = computer::execute(command)?;
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!("{}", computer::describe(&status));
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn computer_cli_defaults_to_read_only_status_and_accepts_explicit_actions() {
        assert!(matches!(
            parse(&[]).unwrap(),
            Request::Action {
                command: Command::Status,
                json: false
            }
        ));
        for (text, expected) in [
            ("on", Command::On),
            ("off", Command::Off),
            ("toggle", Command::Toggle),
            ("status", Command::Status),
        ] {
            let Request::Action { command, json } = parse(&args(&[text, "--json"])).unwrap() else {
                panic!("expected command");
            };
            assert_eq!(
                std::mem::discriminant(&command),
                std::mem::discriminant(&expected)
            );
            assert!(json);
        }
        assert!(matches!(parse(&args(&["--help"])).unwrap(), Request::Help));
    }

    #[test]
    fn computer_cli_rejects_ambiguous_or_legacy_timed_arguments() {
        for values in [
            vec!["on", "off"],
            vec!["host", "60"],
            vec!["60"],
            vec!["--json", "--json"],
            vec!["on", "--help"],
            vec!["--remote", "on"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
    }
}
