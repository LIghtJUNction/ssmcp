//! Re-execute the installed client entry point after an acknowledged reload.
use crate::wire::Operation;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::{
    fs::{MetadataExt, PermissionsExt},
    process::CommandExt,
};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

pub(crate) const TOOL: &str = "ssmcp.client.reload";

pub(crate) fn tool() -> Value {
    json!({
        "name": TOOL,
        "description": "Acknowledge and restart this foreground client from its installed executable, preserving its resolved connection target, other startup arguments, cwd and environment. Reject while other requests are in flight. Stops local background commands, harnesses and forwards. The client reconnects with a new ID; use list again. Server-qualified --allow accounts are checked again after reconnect; control apps retain their independent sign-in.",
        "inputSchema": {"type":"object","properties":{},"additionalProperties":false},
        "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}
    })
}

pub(crate) fn is_reload(operation: &Operation) -> bool {
    matches!(operation, Operation::CallTool { name, .. } if name == TOOL)
}

#[cfg(target_os = "linux")]
pub(crate) fn accepted(result: &Value) -> bool {
    result["isError"] != true
        && result["structuredContent"]["accepted"] == true
        && result["structuredContent"]["status"] == "restarting"
}

pub(crate) fn acknowledgement(client_id: &str, github_auth: bool) -> Value {
    crate::backend::result(json!({
        "accepted":true,
        "status":"restarting",
        "client_id":client_id,
        "auth":if github_auth { "github" } else { "login_code" },
        "oauth_login_required":!github_auth,
        "background_jobs":"stopped",
        "next_step":"Call list again and select the new client ID by title, name and cwd."
    }))
}

/// The active conversation is a local history identity, never a server client
/// or agent identity. The existing private outbox handoff owns this checkpoint.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionState {
    conversation_id: Option<String>,
    terminal: Option<super::terminal::ReloadState>,
}

impl SessionState {
    pub(super) fn capture() -> Result<Self, String> {
        let state = Self {
            conversation_id: super::interface::history::current()?
                .map(|conversation| conversation.metadata.id),
            terminal: super::terminal::reload_snapshot(),
        };
        state.validate()?;
        Ok(state)
    }

    pub(super) fn validate(&self) -> Result<(), String> {
        if self
            .conversation_id
            .as_ref()
            .is_some_and(|id| id.len() > 128 || id.chars().any(char::is_control))
        {
            return Err("invalid reload conversation identity".into());
        }
        if let Some(terminal) = &self.terminal {
            terminal.validate()?;
        }
        Ok(())
    }

    pub(super) fn discard_display(&mut self) {
        if let Some(terminal) = &mut self.terminal {
            terminal.discard_display();
        }
    }

    pub(super) fn restore(self) {
        let (restored, notice) = match self.conversation_id.as_deref() {
            Some(id) => match super::interface::history::resume(id) {
                Ok(_) => (true, None),
                Err(error) => (
                    false,
                    Some(format!(
                        "Could not restore conversation: {error} · use /resume to choose history"
                    )),
                ),
            },
            None => (false, None),
        };
        if let Some(notice) = &notice {
            super::render::status("shell", "Warning", notice);
        }
        super::terminal::restore_reload(self.terminal, restored, notice);
    }
}

pub(crate) struct Restart {
    program: PathBuf,
    arguments: Vec<OsString>,
    cwd: PathBuf,
}

impl Restart {
    pub(super) fn capture(options: &super::Options, saved_session: bool) -> crate::Result<Self> {
        let mut args = std::env::args_os();
        let entry = args.next().ok_or("client executable path is missing")?;
        let cwd = std::env::current_dir()?;
        #[cfg(target_os = "linux")]
        let running = PathBuf::from("/proc/self/exe");
        #[cfg(not(target_os = "linux"))]
        let running = std::env::current_exe()?;
        let program = resolve_entry(&entry, &cwd, std::env::var_os("PATH").as_deref())
            .and_then(|path| {
                if same_executable(&path, &running).map_err(|error| error.to_string())? {
                    // Compare the resolved file identity but retain the entry
                    // path, so changing its installation symlink still works.
                    Ok(path)
                } else {
                    Err("client argv[0] names a different executable".into())
                }
            })
            .or_else(|_| {
                // exec -a can supply any display name, including the name of
                // another program in PATH. Only the actual client is a fallback.
                let path = std::env::current_exe().map_err(|error| error.to_string())?;
                validate_executable(&path)?;
                Ok::<_, String>(path)
            })?;
        Ok(Self {
            program,
            arguments: connection_arguments(args.collect(), options, saved_session)?,
            cwd,
        })
    }

    pub(crate) fn accept(
        &self,
        arguments: &Value,
        inflight: usize,
        client_id: &str,
        github_auth: bool,
    ) -> Result<Value, String> {
        if !arguments.as_object().is_some_and(|args| args.is_empty()) {
            return Err("reload takes no arguments".into());
        }
        if inflight != 0 {
            return Err("client busy: wait for in-flight requests to finish before reload".into());
        }
        validate_executable(&self.program)?;
        Ok(acknowledgement(client_id, github_auth))
    }

    pub(crate) fn execute(
        &self,
        output: std::sync::Arc<crate::output::OutputStore>,
    ) -> crate::Result<()> {
        validate_executable(&self.program)?;
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.arguments).current_dir(&self.cwd);
        let _resume = output.prepare_reload(&mut command)?;
        let _steering = super::steering::prepare_reload(&mut command)?;
        super::interface::history::release_current()?;
        #[cfg(windows)]
        {
            // Windows has no exec. All old backends have already stopped; keep
            // the original foreground invocation alive while its replacement
            // inherits the console, pipes, environment, arguments, and cwd.
            // Returning immediately would let the calling shell reuse stdin.
            let status = command.status().map_err(|error| {
                format!(
                    "could not reload client from {}: {error}",
                    self.program.display()
                )
            })?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("reloaded client exited with {status}").into())
            }
        }
        #[cfg(unix)]
        {
            let error = command.exec();
            Err(format!(
                "could not reload client from {}: {error}",
                self.program.display()
            )
            .into())
        }
    }
}

/// Reload is a continuation of this connection, not a new context selection.
/// Keep the service entry point intact so its saved SessionSpec remains the
/// authority. Foreground invocations record only the resolved endpoint, never
/// credentials; all other arguments retain their original values and order.
fn connection_arguments(
    arguments: Vec<OsString>,
    options: &super::Options,
    saved_session: bool,
) -> crate::Result<Vec<OsString>> {
    if saved_session {
        return Ok(arguments);
    }
    let mut pinned = if options.is_local() {
        vec![OsString::from("local")]
    } else {
        vec![
            OsString::from("--server"),
            OsString::from(
                options
                    .server
                    .as_deref()
                    .ok_or("reload requires a resolved relay server")?,
            ),
        ]
    };
    let mut arguments = arguments.into_iter().peekable();
    if arguments.peek().is_some_and(|argument| argument == "local") {
        arguments.next();
    }
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--context" | "--server") => {
                arguments
                    .next()
                    .ok_or("reload connection selector is missing its value")?;
            }
            Some(value) if value.starts_with("--context=") => {}
            Some(value) if super::connection_option_takes_value(value) => {
                pinned.push(argument);
                pinned.push(
                    arguments
                        .next()
                        .ok_or("reload option is missing its value")?,
                );
            }
            _ => pinned.push(argument),
        }
    }
    Ok(pinned)
}

fn validate_executable(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        format!(
            "reload executable {} is unavailable: {error}",
            path.display()
        )
    })?;
    let executable = metadata.is_file();
    #[cfg(unix)]
    let executable = executable && metadata.permissions().mode() & 0o111 != 0;
    #[cfg(windows)]
    let executable = executable && valid_windows_image(path).map_err(|error| error.to_string())?;
    if !executable {
        return Err(format!(
            "reload target {} is not an executable file",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn same_executable(candidate: &Path, running: &Path) -> std::io::Result<bool> {
    let candidate = std::fs::metadata(candidate)?;
    let running = std::fs::metadata(running)?;
    Ok(candidate.dev() == running.dev() && candidate.ino() == running.ino())
}

#[cfg(windows)]
fn same_executable(candidate: &Path, running: &Path) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    fn identity(path: &Path) -> std::io::Result<(u32, u32, u32)> {
        let file = std::fs::File::open(path)?;
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    }
    Ok(identity(candidate)? == identity(running)?)
}

#[cfg(windows)]
fn valid_windows_image(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    if !path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        return Ok(false);
    }
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size < 64 {
        return Ok(false);
    }
    let mut dos = [0u8; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Ok(false);
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
    if offset < 64 || offset.saturating_add(24) > size {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header)?;
    Ok(&header[..4] == b"PE\0\0" && u16::from_le_bytes([header[22], header[23]]) & 2 != 0)
}

fn resolve_entry(entry: &OsStr, cwd: &Path, path: Option<&OsStr>) -> Result<PathBuf, String> {
    let entry = Path::new(entry);
    let absolute = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            cwd.join(path)
        }
    };
    let program = if entry.is_absolute() || entry.components().count() > 1 {
        absolute(entry.to_path_buf())
    } else {
        std::env::split_paths(path.unwrap_or_default())
            .map(|directory| absolute(directory.join(entry)))
            .flat_map(|candidate| {
                #[cfg(windows)]
                {
                    let mut candidates = vec![candidate.clone()];
                    if candidate.extension().is_none() {
                        candidates.push(candidate.with_extension("exe"));
                    }
                    candidates
                }
                #[cfg(unix)]
                {
                    [candidate]
                }
            })
            .find(|candidate| validate_executable(candidate).is_ok())
            .ok_or("cannot resolve the client executable in PATH for reload")?
    };
    validate_executable(&program)?;
    // Keep the installation entry, including its final symlink. Canonicalizing
    // it would pin reload to an old version after an atomic symlink update.
    Ok(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn foreground_reload_pins_the_resolved_server_and_preserves_literal_values() {
        let root = std::env::current_dir().unwrap();
        let chat_token = root.join("test input token").into_os_string();
        let trailing = vec![
            OsString::from("--title"),
            OsString::from("--context"),
            OsString::from("--name"),
            OsString::from("local"),
            OsString::from("--context-root"),
            root.into_os_string(),
            OsString::from("--ui-config"),
            OsString::from("--context=other"),
            OsString::from("--theme"),
            OsString::from("nord"),
            OsString::from("--no-color"),
            OsString::from("--no-watchdog"),
            OsString::from("--input"),
            OsString::from("mcp-http"),
            OsString::from("--input-host"),
            OsString::from("localhost"),
            OsString::from("--input-port"),
            OsString::from("08181"),
            OsString::from("--input-token-file"),
            chat_token,
            OsString::from("--agent"),
            OsString::from("claude-code"),
        ];
        let mut initial = args(&["--server", "https://original.example"]);
        initial.extend(trailing.clone());
        let options = super::super::Options::parse(initial).unwrap().unwrap();
        for selector in [
            args(&[]),
            args(&["--context", "removed"]),
            args(&["--context=edited"]),
            args(&["--server", "https://original.example"]),
        ] {
            let mut original = selector;
            original.extend(trailing.clone());
            let pinned = connection_arguments(original, &options, false).unwrap();
            let mut expected = args(&["--server", "https://original.example"]);
            expected.extend(trailing.clone());
            assert_eq!(pinned, expected);
            // This is the same new-connection parser used after actual reexec.
            // An explicit origin bypasses all ambient context selection.
            let restored = super::super::Options::parse_new(pinned).unwrap().unwrap();
            assert_eq!(restored.server, options.server);
            assert_eq!(restored.title, options.title);
            assert_eq!(restored.name, options.name);
            assert_eq!(restored.context_roots, options.context_roots);
            assert_eq!(restored.ui_config, options.ui_config);
            assert_eq!(restored.theme, options.theme);
            assert_eq!(restored.no_color, options.no_color);
            assert_eq!(restored.watchdog, options.watchdog);
            assert_eq!(restored.chat, options.chat);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn foreground_local_reload_preserves_mode_and_listener_options() {
        let trailing = args(&[
            "--listen",
            "127.0.0.1:34819",
            "--public-url",
            "https://workstation.local",
            "--title",
            "--context",
            "--no-watchdog",
        ]);
        let mut initial = args(&["local"]);
        initial.extend(trailing.clone());
        let options = super::super::Options::parse(initial).unwrap().unwrap();
        for selector in [
            args(&[]),
            args(&["--context", "removed"]),
            args(&["--context=edited"]),
            args(&["local"]),
        ] {
            let mut original = selector;
            original.extend(trailing.clone());
            let pinned = connection_arguments(original, &options, false).unwrap();
            let mut expected = args(&["local"]);
            expected.extend(trailing.clone());
            assert_eq!(pinned, expected);
            let restored = super::super::Options::parse_new(pinned).unwrap().unwrap();
            assert!(restored.is_local());
            assert_eq!(
                restored.local.as_ref().unwrap().listen,
                options.local.as_ref().unwrap().listen
            );
            assert_eq!(
                restored.local.as_ref().unwrap().public_url,
                options.local.as_ref().unwrap().public_url
            );
            assert_eq!(restored.title, options.title);
        }
    }

    #[test]
    fn saved_runner_reload_keeps_its_manifest_entry_point() {
        let options = super::super::Options::parse(args(&["--server", "https://saved.example"]))
            .unwrap()
            .unwrap();
        let original = args(&["__session-run", "saved"]);
        assert_eq!(
            connection_arguments(original.clone(), &options, true).unwrap(),
            original
        );
    }

    #[test]
    fn foreground_reload_without_a_resolved_endpoint_fails_closed() {
        let options = super::super::Options::parse(args(&[])).unwrap().unwrap();
        assert!(connection_arguments(args(&[]), &options, false).is_err());
    }

    #[test]
    fn reload_rejects_arguments_and_inflight_requests_before_acceptance() {
        let restart = Restart {
            program: std::env::current_exe().unwrap(),
            arguments: Vec::new(),
            cwd: std::env::current_dir().unwrap(),
        };
        assert!(
            restart
                .accept(&json!({"force":true}), 0, "old", true)
                .is_err()
        );
        assert!(restart.accept(&json!({}), 1, "old", true).is_err());
        let ack = restart.accept(&json!({}), 0, "old", false).unwrap();
        assert_eq!(ack["structuredContent"]["accepted"], true);
        assert_eq!(ack["structuredContent"]["oauth_login_required"], true);
        assert_eq!(ack["structuredContent"]["client_id"], "old");
    }

    #[cfg(unix)]
    #[test]
    fn reload_resolves_path_without_canonicalizing_the_installation_entry() {
        assert_eq!(
            resolve_entry(OsStr::new("/bin/sh"), Path::new("/tmp"), None).unwrap(),
            Path::new("/bin/sh")
        );
        assert_eq!(
            resolve_entry(OsStr::new("sh"), Path::new("/"), Some(OsStr::new("/bin"))).unwrap(),
            Path::new("/bin/sh")
        );
        assert!(
            resolve_entry(
                OsStr::new("missing-ssmcp-executable"),
                Path::new("/"),
                Some(OsStr::new("/bin"))
            )
            .is_err()
        );
    }

    #[test]
    fn reload_resolves_the_current_executable_from_its_directory() {
        let current = std::env::current_exe().unwrap();
        let directory = current.parent().unwrap();
        let path = std::env::join_paths([directory]).unwrap();
        assert_eq!(
            resolve_entry(current.file_name().unwrap(), directory, Some(&path)).unwrap(),
            current
        );
    }
}
