//! Explicit frontend selection. The destination is always data, never shell input.
use std::{ffi::OsString, path::PathBuf};

macro_rules! usage {
    () => {
        "Usage: ssmcp username@server [--tui] [options]
       ssmcp username@server --gui
       ssmcp username@server --webui
       ssmcp gui
       ssmcp webui"
    };
}
pub(super) use usage;

const USAGE: &str = usage!();

pub(super) fn missing_target() -> &'static str {
    USAGE
}

#[derive(PartialEq, Eq, Debug)]
pub(super) enum Launch {
    Tui,
    Web(Option<String>),
    Desktop(Option<String>),
}

fn selected(value: &str) -> bool {
    matches!(value, "--tui" | "--webui" | "--gui")
}

pub(super) fn parse(args: &mut Vec<OsString>) -> crate::Result<Launch> {
    if args.is_empty() {
        return Err(USAGE.into());
    }
    if matches!(args.as_slice(), [flag] if flag == "webui" || flag == "--webui") {
        return Ok(Launch::Web(None));
    }
    if matches!(args.as_slice(), [flag] if flag == "gui" || flag == "--gui") {
        return Ok(Launch::Desktop(None));
    }
    // A selector is position-independent among connection options but never
    // consumes an option value, a subcommand argument, or a harness argument.
    let mut selector = None;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_str().unwrap_or("");
        if arg == "--" {
            break;
        }
        if selected(arg) {
            if selector.replace(i).is_some() {
                return Err("select exactly one frontend".into());
            }
        } else if super::connection_option_takes_value(arg) {
            i += 1;
        }
        i += 1;
    }
    let Some(index) = selector else {
        return Ok(Launch::Tui);
    };
    let flag = args.remove(index);
    if flag == "--tui" {
        return Ok(Launch::Tui);
    }
    let [target] = args.as_slice() else {
        return Err(USAGE.into());
    };
    let target = target.to_str().ok_or(USAGE)?;
    crate::server_endpoint::parse_target(target)?;
    if flag == "--gui" {
        Ok(Launch::Desktop(Some(target.into())))
    } else {
        Ok(Launch::Web(Some(target.into())))
    }
}

pub(super) async fn desktop(target: Option<String>) -> crate::Result<()> {
    let name = if cfg!(windows) {
        "ssmcp-desktop.exe"
    } else {
        "ssmcp-desktop"
    };
    let sibling = std::env::current_exe()?.with_file_name(name);
    let exe = if sibling.is_file() {
        sibling
    } else {
        PathBuf::from(name)
    };
    let mut command = tokio::process::Command::new(exe);
    if let Some(target) = target {
        command.arg("--target").arg(target);
    }
    let status = command.status().await.map_err(|e| {
        format!(
            "Could not launch {name}: {e}. Install the desktop frontend alongside ssmcp or on PATH."
        )
    })?;
    if !status.success() {
        return Err(format!("The desktop frontend exited with {status}").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse_args(values: &[&str]) -> crate::Result<Launch> {
        parse(&mut values.iter().map(OsString::from).collect())
    }
    #[test]
    fn explicit_frontend_target() {
        assert_eq!(
            parse_args(&["--webui", "alice@relay.test"]).unwrap(),
            Launch::Web(Some("alice@relay.test".into()))
        );
        assert_eq!(
            parse_args(&["alice@relay.test", "--gui"]).unwrap(),
            Launch::Desktop(Some("alice@relay.test".into()))
        );
        assert_eq!(parse_args(&["gui"]).unwrap(), Launch::Desktop(None));
        assert_eq!(parse_args(&["webui"]).unwrap(), Launch::Web(None));
        for selector in ["--gui", "--webui"] {
            let error =
                parse_args(&["alice@relay.test", selector, "--title", "phone"]).unwrap_err();
            assert_eq!(error.to_string(), missing_target());
        }
        for invalid in [
            &[][..],
            &["--webui", "--tui"],
            &["--gui", "--webui", "alice@relay.test"],
            &["alice@", "--webui"],
        ] {
            assert!(parse_args(invalid).is_err(), "{invalid:?}");
        }
        let mut args = [
            "alice@relay.test",
            "--title",
            "--gui",
            "--tui",
            "--",
            "--webui",
        ]
        .map(OsString::from)
        .to_vec();
        assert_eq!(parse(&mut args).unwrap(), Launch::Tui);
        assert_eq!(
            args,
            ["alice@relay.test", "--title", "--gui", "--", "--webui"].map(OsString::from)
        );
    }
}
