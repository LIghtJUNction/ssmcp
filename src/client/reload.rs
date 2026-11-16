//! Re-execute the installed client entry point after an acknowledged reload.
use crate::wire::Operation;
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
        "description": "Acknowledge and restart this foreground client from its installed executable, preserving startup arguments, cwd and environment. Reject while other requests are in flight. Stops local background commands, harnesses and forwards. The client reconnects with a new ID; use list again. Login-code connections require new OAuth login; --allow GitHub access persists.",
        "inputSchema": {"type":"object","properties":{},"additionalProperties":false},
        "annotations": {"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":false}
    })
}

pub(crate) fn is_reload(operation: &Operation) -> bool {
    matches!(operation, Operation::CallTool { name, .. } if name == TOOL)
}

#[cfg(any(target_os = "linux", test))]
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

pub(crate) struct Restart {
    program: PathBuf,
    arguments: Vec<OsString>,
    cwd: PathBuf,
}

impl Restart {
    pub(crate) fn capture() -> crate::Result<Self> {
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
            arguments: args.collect(),
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
