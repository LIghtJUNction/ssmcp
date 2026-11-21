//! Local Zuse lifecycle commands and an explicit manual legacy compatibility switch.
use super::interface::computer::{self as compatibility, Action as CompatibilityAction};
use crate::computer::inspect::Request as Observation;
use std::ffi::OsString;

use serde_json::{Value, json};

const HELP: &str = "\
Usage: ssmcp computer [version|download [VERSION]|install DOWNLOAD_ID|uninstall INSTALLATION_ID]
                     [--json]
       ssmcp computer <on|off|toggle|status> [--json]
       ssmcp computer observation <on|off|status>
       ssmcp computer check
       ssmcp computer locate-window WINDOW_ID EXACT_LABEL
       ssmcp computer observe-tab LOOPBACK_PORT PAGE_TARGET_ID CSS_CONTAINER

Manage the standalone Zuse CLI. Desktop and browser operations run directly in Zuse;
SSMCP does not embed those controls or provide an action proxy.

  version                    Query the Zuse CLI on PATH, or a managed install (default)
  download [VERSION]         Download an official release for this platform
  install DOWNLOAD_ID        Install one verified managed download to a private directory
  uninstall INSTALLATION_ID  Remove only the matching managed installation

An omitted VERSION selects the latest stable release. Downloading does not install
or execute it. Installation does not overwrite an external Zuse executable or change PATH.
Use the returned installation path directly, or configure your own PATH explicitly.
--json returns the complete native MCP result for lifecycle commands.

旧版 SSMCP 兼容开关：on / off / toggle / status · 不控制独立 Zuse CLI
Use observation on to authorize scoped observations separately from legacy input.
observation off, computer off and Ctrl+C revoke in-flight observations.
check verifies the CLI contract using
a timer without opening a window or connecting to a page. Results are JSON.
Ordinary exec and standalone Zuse outside these commands use OS permissions.
status only observes existing state; an absent record is unset, not an Off claim.
--json returns the compatibility state for these four manual commands.
";

#[derive(Debug, PartialEq)]
enum Request {
    Help,
    Inspect(Observation),
    ObservationState(Option<bool>),
    Compatibility {
        action: CompatibilityAction,
        json: bool,
    },
    Action {
        name: &'static str,
        arguments: Value,
        json: bool,
    },
}

fn parse(args: &[OsString]) -> Result<Request, String> {
    if args.len() == 1 && matches!(args[0].to_str(), Some("-h" | "--help")) {
        return Ok(Request::Help);
    }
    // A target's accessible label or selector is literal data, including "--json".
    if let [op, window, label] = args
        && op == "locate-window"
    {
        return Ok(Request::Inspect(Observation::locate(
            window.to_str().ok_or("window ID must be UTF-8")?,
            label.to_str().ok_or("accessible label must be UTF-8")?,
        )?));
    }
    if let [op, port, page, selector] = args
        && op == "observe-tab"
    {
        return Ok(Request::Inspect(Observation::observe(
            port.to_str().ok_or("port must be UTF-8")?,
            page.to_str().ok_or("page ID must be UTF-8")?,
            selector.to_str().ok_or("selector must be UTF-8")?,
        )?));
    }
    let mut operands = Vec::new();
    let mut json = false;
    for arg in args {
        let value = arg.to_str().ok_or("computer arguments must be UTF-8")?;
        if value == "--json" && !json {
            json = true;
        } else if value.starts_with('-') {
            return Err("unknown or repeated computer option; use ssmcp computer --help".into());
        } else {
            operands.push(value);
        }
    }
    match operands.as_slice() {
        ["observation", "on"] => return Ok(Request::ObservationState(Some(true))),
        ["observation", "off"] => return Ok(Request::ObservationState(Some(false))),
        ["observation", "status"] => return Ok(Request::ObservationState(None)),
        ["check"] => return Ok(Request::Inspect(Observation::Check)),
        _ => {}
    }
    let compatibility = match operands.as_slice() {
        ["on"] => Some(CompatibilityAction::On),
        ["off"] => Some(CompatibilityAction::Off),
        ["toggle"] => Some(CompatibilityAction::Toggle),
        ["status"] => Some(CompatibilityAction::Status),
        _ => None,
    };
    if let Some(action) = compatibility {
        return Ok(Request::Compatibility { action, json });
    }
    let (name, arguments) = match operands.as_slice() {
        [] | ["version"] => ("ssmcp.computer.version", json!({})),
        ["download"] => ("ssmcp.computer.download", json!({})),
        ["download", version] => ("ssmcp.computer.download", json!({"version":version})),
        ["install", download_id] => ("ssmcp.computer.install", json!({"download_id":download_id})),
        ["uninstall", installation_id] => (
            "ssmcp.computer.uninstall",
            json!({"installation_id":installation_id}),
        ),
        _ => {
            return Err(
                "use ssmcp computer --help for lifecycle and manual compatibility commands".into(),
            );
        }
    };
    Ok(Request::Action {
        name,
        arguments,
        json,
    })
}

pub(crate) async fn dispatch(args: &[OsString]) -> crate::Result<bool> {
    if !args.first().is_some_and(|arg| arg == "computer") {
        return Ok(false);
    }
    match parse(&args[1..])? {
        Request::Help => print!("{HELP}"),
        Request::ObservationState(enabled) => {
            println!("{}", crate::computer_control::observation_state(enabled)?)
        }
        Request::Inspect(request) => {
            let computer = crate::computer::Computer::new(std::env::current_dir()?)?;
            println!("{}", computer.inspect_local(request).await?);
        }
        Request::Compatibility { action, json } => {
            let state = compatibility::apply(action)?;
            if json {
                println!("{}", serde_json::to_string(&state)?);
            } else {
                println!("{}", compatibility::describe(&state));
            }
        }
        Request::Action {
            name,
            arguments,
            json,
        } => {
            let computer = crate::computer::Computer::new(std::env::current_dir()?)?;
            let result = computer.perform(name, arguments).await?;
            if json {
                println!("{}", serde_json::to_string(&result)?);
            } else {
                let body = result.get("structuredContent").unwrap_or(&result);
                println!("{}", serde_json::to_string_pretty(body)?);
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
    fn computer_cli_defaults_to_version_and_preserves_lifecycle_arguments() {
        assert_eq!(
            parse(&[]).unwrap(),
            Request::Action {
                name: "ssmcp.computer.version",
                arguments: json!({}),
                json: false,
            }
        );
        for (values, name, arguments) in [
            (vec!["version"], "version", json!({})),
            (vec!["download"], "download", json!({})),
            (
                vec!["download", "v0.3.0"],
                "download",
                json!({"version":"v0.3.0"}),
            ),
            (
                vec!["install", "download-fixture"],
                "install",
                json!({"download_id":"download-fixture"}),
            ),
            (
                vec!["uninstall", "installation-fixture"],
                "uninstall",
                json!({"installation_id":"installation-fixture"}),
            ),
        ] {
            let mut values = values;
            values.push("--json");
            let Request::Action {
                name: actual_name,
                arguments: actual_arguments,
                json,
            } = parse(&args(&values)).unwrap()
            else {
                panic!("expected lifecycle command");
            };
            assert_eq!(actual_name, format!("ssmcp.computer.{name}"));
            assert_eq!(actual_arguments, arguments);
            assert!(json);
        }
        assert_eq!(parse(&args(&["--help"])).unwrap(), Request::Help);
    }

    #[test]
    fn computer_cli_requires_explicit_manual_compatibility_actions() {
        for (command, action) in [
            ("status", CompatibilityAction::Status),
            ("on", CompatibilityAction::On),
            ("off", CompatibilityAction::Off),
            ("toggle", CompatibilityAction::Toggle),
        ] {
            assert_eq!(
                parse(&args(&[command])).unwrap(),
                Request::Compatibility {
                    action,
                    json: false
                }
            );
            assert_eq!(
                parse(&args(&["--json", command])).unwrap(),
                Request::Compatibility { action, json: true }
            );
        }
        assert!(matches!(
            parse(&args(&["--json"])).unwrap(),
            Request::Action {
                name: "ssmcp.computer.version",
                ..
            }
        ));
        assert!(HELP.contains("旧版 SSMCP 兼容开关"));
        assert!(HELP.contains("不控制独立 Zuse CLI"));
    }

    #[test]
    fn local_observations_keep_literal_targets_and_reject_open_programs() {
        assert_eq!(
            parse(&args(&["locate-window", "0x24", "--json"])).unwrap(),
            Request::Inspect(Observation::locate("0x24", "--json").unwrap())
        );
        assert_eq!(
            parse(&args(&["observe-tab", "9", "FIXTURE", "#scope"])).unwrap(),
            Request::Inspect(Observation::observe("9", "FIXTURE", "#scope").unwrap())
        );
        assert_eq!(
            parse(&args(&["observation", "on"])).unwrap(),
            Request::ObservationState(Some(true))
        );
        for cmd in ["eval", "run", "shell", "click", "launch"] {
            assert!(parse(&args(&[cmd, "arbitrary"])).is_err());
        }
    }

    #[test]
    fn computer_cli_rejects_native_actions_and_ambiguous_commands() {
        for values in [
            vec!["on", "extra"],
            vec!["toggle", "off"],
            vec!["status", "version"],
            vec!["click", "1", "2"],
            vec!["install"],
            vec!["uninstall"],
            vec!["version", "unexpected"],
            vec!["download", "v0.3.0", "v0.4.0"],
            vec!["--json", "--json"],
            vec!["version", "--help"],
            vec!["--remote", "version"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
    }
}
