//! Local Zuse CLI lifecycle commands, using the same manager as the native tools.
use std::ffi::OsString;
use std::io::{self, Write};

use serde_json::{Value, json};

const HELP: &str = "\
Usage: ssmcp computer [version|download [VERSION]|install DOWNLOAD_ID|uninstall INSTALLATION_ID]
                     [--json]

Manage the standalone Zuse CLI. Desktop and browser operations run directly in Zuse;
SSMCP does not embed those controls or provide an action proxy.

  version                    Query the Zuse CLI on PATH, or a managed install (default)
  download [VERSION]         Download an official release for this platform
  install DOWNLOAD_ID        Install one verified managed download to a private directory
  uninstall INSTALLATION_ID  Remove only the matching managed installation

An omitted VERSION selects the latest stable release. Downloading does not install
or execute it. Installation does not overwrite an external Zuse executable or change PATH.
Use the returned installation path directly, or configure your own PATH explicitly.
--json writes one complete native MCP result to stdout. Operation failures
include isError: true and exit nonzero. Syntax and initialization errors
exit nonzero and write a diagnostic to stderr.
";

#[derive(Debug)]
pub(super) struct ReportedOperationError(String);

impl std::fmt::Display for ReportedOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ReportedOperationError {}

#[derive(Debug, PartialEq)]
enum Request {
    Help,
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
    let (name, arguments) = match operands.as_slice() {
        [] | ["version"] => ("ssmcp.computer.version", json!({})),
        ["download"] => ("ssmcp.computer.download", json!({})),
        ["download", version] => ("ssmcp.computer.download", json!({"version":version})),
        ["install", download_id] => ("ssmcp.computer.install", json!({"download_id":download_id})),
        ["uninstall", installation_id] => (
            "ssmcp.computer.uninstall",
            json!({"installation_id":installation_id}),
        ),
        _ => return Err("use ssmcp computer --help for Zuse CLI lifecycle commands".into()),
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
        Request::Action {
            name,
            arguments,
            json,
        } => {
            let computer = crate::computer::Computer::new(std::env::current_dir()?)?;
            let result = match computer
                .perform_with_child_diagnostics(name, arguments, !json)
                .await
            {
                Ok(result) => result,
                Err(message) if json => {
                    let diagnostic = json!({"type":"stderr","text": message});
                    let safe = super::render::diagnostic_text(&diagnostic)
                        .unwrap_or_else(|| "Zuse operation failed".into());
                    let result = crate::backend::error_result(&safe);
                    writeln!(io::stdout().lock(), "{}", serde_json::to_string(&result)?)?;
                    return Err(ReportedOperationError(safe).into());
                }
                Err(message) => return Err(message.into()),
            };
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
    fn computer_cli_defaults_to_version_and_maps_only_lifecycle_arguments() {
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
    fn computer_cli_rejects_retired_controls_and_ambiguous_commands() {
        for values in [
            vec!["on"],
            vec!["off"],
            vec!["toggle"],
            vec!["status"],
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
