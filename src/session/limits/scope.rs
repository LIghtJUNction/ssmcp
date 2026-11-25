//! Keep the manager connection separate from the environment of the command it starts.
use crate::Result;
use std::{
    ffi::{OsStr, OsString},
    path::Path,
    process::Command,
};

fn env_prefix(changes: &[(OsString, Option<OsString>)]) -> Vec<OsString> {
    if changes.is_empty() {
        return Vec::new();
    }
    let mut words = vec![OsString::from("/usr/bin/env")];
    for (key, value) in changes {
        if value.is_none() {
            words.extend([OsString::from("-u"), key.clone()]);
        }
    }
    words.push("--".into());
    for (key, value) in changes {
        if let Some(value) = value {
            let mut assignment = key.clone();
            assignment.push("=");
            assignment.push(value);
            words.push(assignment);
        }
    }
    words
}

// Terminal adapters accept argv rather than a Command. Apply the manager's
// environment only inside the terminal, so the terminal can still find its display.
pub(crate) fn argv(command: &Command) -> Vec<OsString> {
    let changes = command
        .get_envs()
        .filter(|(key, value)| std::env::var_os(key).as_deref() != *value)
        .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
        .collect::<Vec<_>>();
    let mut words = env_prefix(&changes);
    words.push(command.get_program().to_owned());
    words.extend(command.get_args().map(OsStr::to_owned));
    words
}

pub(crate) fn launch(options: &[String], executable: &Path, args: &[OsString]) -> Result<Command> {
    let manager = crate::session_budget::manager_command("systemd-run")?;
    configure(manager, options, executable, args, supports_literal)
}

fn supports_literal(program: &OsStr) -> Result<bool> {
    let version = Command::new(program).arg("--version").output()?;
    let major: u32 = std::str::from_utf8(&version.stdout)?
        .split_whitespace()
        .nth(1)
        .ok_or("missing systemd-run version")?
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("")
        .parse()?;
    if !version.status.success() {
        return Err("systemd-run --version failed".into());
    }
    Ok(major >= 254)
}

pub(super) fn configure(
    mut manager: Command,
    options: &[String],
    executable: &Path,
    args: &[OsString],
    supports_literal: impl FnOnce(&OsStr) -> Result<bool>,
) -> Result<Command> {
    let restore = manager
        .get_envs()
        .filter_map(|(key, value)| {
            let original = std::env::var_os(key);
            (original.as_deref() != value).then(|| (key.to_owned(), original))
        })
        .collect::<Vec<_>>();
    let mut payload = env_prefix(&restore);
    payload.push(executable.as_os_str().to_owned());
    payload.extend_from_slice(args);
    manager.args(options);
    // Older scope launchers pass argv literally and do not know this switch.
    if payload
        .iter()
        .any(|arg| arg.as_encoded_bytes().contains(&b'$'))
        && supports_literal(manager.get_program())?
    {
        manager.arg("--expand-environment=no");
    }
    manager.arg("--").args(payload);
    Ok(manager)
}
