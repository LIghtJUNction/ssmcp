//! Re-execute the installed client entry point after an acknowledged reload.
use crate::wire::Operation;
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::{fs::MetadataExt, process::CommandExt};
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
        let current = std::env::current_exe()?;
        #[cfg(target_os = "linux")]
        let running = std::fs::metadata("/proc/self/exe")?;
        #[cfg(not(target_os = "linux"))]
        let running = std::fs::metadata(&current)?;
        let program = resolve_entry(&entry, &cwd, std::env::var_os("PATH").as_deref())
            .and_then(|path| {
                if same_executable(&path, &current, &running) {
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
                validate_executable(&current)?;
                Ok::<_, String>(current)
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

    pub(crate) fn execute(&self) -> crate::Result<()> {
        let mut command = std::process::Command::new(&self.program);
        command.args(&self.arguments).current_dir(&self.cwd);
        #[cfg(unix)]
        {
            let error = command.exec();
            Err(format!(
                "could not reload client from {}: {error}",
                self.program.display()
            )
            .into())
        }
        #[cfg(windows)]
        {
            // Windows has no exec. Keep the original console invocation alive
            // while its replacement uses the same inherited standard handles.
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
    }
}

fn same_executable(candidate: &Path, _current: &Path, running: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        std::fs::metadata(candidate).is_ok_and(|metadata| {
            metadata.dev() == running.dev() && metadata.ino() == running.ino()
        })
    }
    #[cfg(windows)]
    {
        let _ = running;
        match (candidate.canonicalize(), _current.canonicalize()) {
            (Ok(candidate), Ok(current)) => candidate == current,
            _ => false,
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
    if !metadata.is_file() || !crate::platform::is_executable(path) {
        return Err(format!(
            "reload target {} is not an executable file",
            path.display()
        ));
    }
    Ok(())
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
            cwd: std::env::temp_dir(),
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

    #[test]
    #[cfg(unix)]
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
