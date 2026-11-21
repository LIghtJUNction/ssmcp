//! CLI parsing for the managed-file operations. The client owns dispatch and exit.
use std::{io::Read, path::Path};

const HELP: &str = "Usage: ssmcp cleanup [OPTION]\n\
\n\
  (no option)        Clean registered temporary files using the workspace rules\n\
  --dry-run          Report the cleanup without removing files\n\
  --list             List registered managed files\n\
  --config           Show the effective cleanup configuration\n\
  --set-config FILE  Validate a JSON file and save the workspace configuration\n\
  --help, -h         Show this help without accessing the workspace\n\
\n\
Choose at most one option. Rules are stored in .ssmcp/cleanup.json.\n\
--set-config only changes configuration; it does not run cleanup.\n";

#[derive(Debug, PartialEq, Eq)]
enum Action<'a> {
    Clean { dry_run: bool },
    List,
    Config,
    SetConfig(&'a str),
    Help,
}

fn parse(args: &[String]) -> Result<Option<Action<'_>>, String> {
    if args.first().map(String::as_str) != Some("cleanup") {
        return Ok(None);
    }
    match &args[1..] {
        [] => Ok(Some(Action::Clean { dry_run: false })),
        [option] => match option.as_str() {
            "--dry-run" => Ok(Some(Action::Clean { dry_run: true })),
            "--list" => Ok(Some(Action::List)),
            "--config" => Ok(Some(Action::Config)),
            "--help" | "-h" => Ok(Some(Action::Help)),
            "--set-config" => Err("cleanup --set-config requires a JSON file path".into()),
            _ => Err("unknown cleanup option; use ssmcp cleanup --help".into()),
        },
        [option, path] if option == "--set-config" => {
            if path.is_empty() || path.starts_with('-') {
                return Err(
                    "cleanup --set-config requires a file path; prefix a name starting with '-' with ./"
                        .into(),
                );
            }
            Ok(Some(Action::SetConfig(path)))
        }
        _ => Err("cleanup options are mutually exclusive and accept no extra arguments".into()),
    }
}

/// Dispatch argv after the executable name, including the leading `cleanup`.
/// `None` leaves other commands to the client. The caller prints `Some` and exits.
/// Parse the complete command before any filesystem access or cleanup mutation.
pub fn run(args: &[String], root: &Path) -> Result<Option<String>, String> {
    let Some(action) = parse(args)? else {
        return Ok(None);
    };
    let output = match action {
        Action::Help => HELP.to_owned(),
        Action::Clean { dry_run } => json(
            &super::cleanup(root, dry_run, "cli")
                .map_err(|error| format!("cleanup failed: {error}"))?,
        )?,
        Action::List => json(
            &super::list(root).map_err(|error| format!("cannot list managed files: {error}"))?,
        )?,
        Action::Config => json(
            &super::config(root)
                .map_err(|error| format!("cannot read cleanup configuration: {error}"))?,
        )?,
        Action::SetConfig(path) => {
            let config = read_config(Path::new(path))?;
            let output = json(&config)?;
            super::set_config(root, &config)
                .map_err(|error| format!("cannot save cleanup configuration: {error}"))?;
            output
        }
    };
    Ok(Some(output))
}

fn json(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value)
        .map_err(|error| format!("cannot encode cleanup result: {error}"))
}

fn read_config(path: &Path) -> Result<super::CleanupConfig, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO supplied as FILE must not turn configuration loading into a wait.
        options.custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("cannot open cleanup configuration {path:?}: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect cleanup configuration {path:?}: {error}"))?;
    if !metadata.is_file() {
        return Err("cleanup configuration input must be a regular JSON file".into());
    }
    let limit = super::CONFIG_BYTES_LIMIT as u64;
    if metadata.len() > limit {
        return Err(format!("cleanup configuration exceeds {limit} bytes"));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read cleanup configuration {path:?}: {error}"))?;
    if bytes.len() > super::CONFIG_BYTES_LIMIT {
        return Err(format!("cleanup configuration exceeds {limit} bytes"));
    }
    let config: super::CleanupConfig = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid cleanup configuration JSON: {error}"))?;
    config
        .validate()
        .map_err(|error| format!("invalid cleanup configuration: {error}"))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }

    #[test]
    fn cleanup_modes_are_explicit_and_preserve_the_config_path() {
        for (values, expected) in [
            (vec!["cleanup"], Action::Clean { dry_run: false }),
            (vec!["cleanup", "--dry-run"], Action::Clean { dry_run: true }),
            (vec!["cleanup", "--list"], Action::List),
            (vec!["cleanup", "--config"], Action::Config),
            (vec!["cleanup", "--help"], Action::Help),
            (vec!["cleanup", "-h"], Action::Help),
            (
                vec!["cleanup", "--set-config", "rules 中文.json"],
                Action::SetConfig("rules 中文.json"),
            ),
            (
                vec!["cleanup", "--set-config", "./--rules.json"],
                Action::SetConfig("./--rules.json"),
            ),
        ] {
            assert_eq!(parse(&args(&values)).unwrap(), Some(expected));
        }
    }

    #[test]
    fn cleanup_rejects_ambiguous_modes_and_trailing_arguments() {
        for values in [
            vec!["cleanup", "--dry-run", "--list"],
            vec!["cleanup", "--list", "--list"],
            vec!["cleanup", "--config", "extra"],
            vec!["cleanup", "--help", "--dry-run"],
            vec!["cleanup", "--unknown"],
            vec!["cleanup", "--set-config"],
            vec!["cleanup", "--set-config", ""],
            vec!["cleanup", "--set-config", "--list"],
            vec!["cleanup", "--set-config", "rules.json", "--dry-run"],
        ] {
            assert!(parse(&args(&values)).is_err(), "{values:?}");
        }
    }

    #[test]
    fn help_and_unrelated_commands_do_not_access_the_workspace() {
        // An invalid filesystem path makes accidental workspace access observable.
        let root = Path::new("\0cleanup-command-does-not-access-this-path");
        for values in [
            vec![],
            vec!["config"],
            vec!["--title", "cleanup"],
            vec!["--load-harness", "codex", "--", "cleanup"],
        ] {
            assert_eq!(parse(&args(&values)).unwrap(), None, "{values:?}");
            assert_eq!(run(&args(&values), root).unwrap(), None, "{values:?}");
        }
        for option in ["--help", "-h"] {
            assert_eq!(
                run(&args(&["cleanup", option]), root).unwrap().as_deref(),
                Some(HELP)
            );
        }
        assert_eq!(
            run(&args(&["cleanup", "--list", "--dry-run"]), root).unwrap_err(),
            "cleanup options are mutually exclusive and accept no extra arguments"
        );
    }
}

