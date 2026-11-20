use super::Agent;
use crate::backend::process::Managed;
use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

pub(super) enum Target {
    NextCaller,
    Ssmcp {
        id: String,
        account: (String, String),
    },
    Program(Program),
}

impl Target {
    pub(super) fn resolve(agent: &Agent) -> Result<Self, String> {
        let (program, args, argv_message) = match agent {
            Agent::NextCaller => return Ok(Self::NextCaller),
            Agent::Ssmcp { id, account } => {
                return Ok(Self::Ssmcp {
                    id: id.clone(),
                    account: account.clone(),
                });
            }
            Agent::Codex => (
                crate::platform::executable("codex")?,
                strings(&["exec", "--color", "never", "-"]),
                false,
            ),
            Agent::Claude => (
                crate::platform::executable("claude")?,
                strings(&[
                    "--print",
                    "--input-format",
                    "text",
                    "--output-format",
                    "text",
                ]),
                false,
            ),
            Agent::OpenCode => (
                crate::platform::executable("opencode")?,
                strings(&["--standalone", "run", "--"]),
                true,
            ),
            Agent::Exec { program, argv_file } => {
                let resolved = program
                    .canonicalize()
                    .map_err(|_| "cannot resolve the configured chat executable")?;
                let name = resolved
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if [
                    "sh",
                    "bash",
                    "dash",
                    "zsh",
                    "fish",
                    "ksh",
                    "csh",
                    "tcsh",
                    "ash",
                    "busybox",
                    "cmd",
                    "powershell",
                    "pwsh",
                ]
                .contains(&name.as_str())
                {
                    return Err("--agent exec requires a message-processing executable, not a command shell".into());
                }
                let mut args = if let Some(path) = argv_file {
                    serde_json::from_slice::<Vec<String>>(&super::private_file(path)?)
                        .map_err(|_| "agent argv file must be a JSON array of strings")?
                } else {
                    Vec::new()
                };
                if args.len() > 16
                    || args
                        .iter()
                        .any(|value| value.len() > 4096 || value.contains('\0'))
                {
                    return Err("agent argv file exceeds argument limits".into());
                }
                args.push("--".into());
                (program.clone(), args, true)
            }
        };
        if !native_executable(&program) {
            return Err("the selected chat agent requires an executable file; shell wrappers using .cmd or .bat are unsupported".into());
        }
        Ok(Self::Program(Program {
            program,
            args,
            argv_message,
        }))
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn native_executable(path: &Path) -> bool {
    #[cfg(windows)]
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("com"))
    {
        return false;
    }
    crate::platform::is_executable(path)
}

pub(super) struct Program {
    program: PathBuf,
    args: Vec<String>,
    argv_message: bool,
}

const OUTPUT_LIMIT: usize = 64 * 1024;
const PROCESS_LIMIT: Duration = Duration::from_secs(120);

async fn bounded_output(reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((OUTPUT_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > OUTPUT_LIMIT {
        return Err(io::Error::other("chat agent output exceeded 64 KiB"));
    }
    Ok(bytes)
}

impl Program {
    fn command(&self, text: &str) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .env_remove("SSMCP_RELAY_TOKEN")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.argv_message {
            command.arg(text).stdin(Stdio::null());
        } else {
            command.stdin(Stdio::piped());
        }
        command
    }

    pub(super) async fn run(&self, text: &str) -> Result<String, String> {
        let mut process = Managed::spawn(&mut self.command(text))
            .map_err(|error| format!("could not start the selected chat agent: {error}"))?;
        let stdin = process.child.stdin.take();
        let stdout = process.child.stdout.take().expect("piped stdout");
        let stderr = process.child.stderr.take().expect("piped stderr");
        let write = async {
            if let Some(mut stdin) = stdin {
                stdin.write_all(text.as_bytes()).await?;
                stdin.shutdown().await?;
            }
            Ok::<(), io::Error>(())
        };
        let result = tokio::time::timeout(PROCESS_LIMIT, async {
            tokio::try_join!(
                write,
                bounded_output(stdout),
                bounded_output(stderr),
                process.wait()
            )
        })
        .await;
        match result {
            Ok(Ok(((), output, _, status))) if status.success() => {
                let text =
                    String::from_utf8(output).map_err(|_| "chat agent output is not UTF-8")?;
                Ok(crate::audit::event(serde_json::Value::String(text))
                    .as_str()
                    .unwrap_or_default()
                    .to_owned())
            }
            Ok(Ok(((), _, _, status))) => Err(format!("chat agent exited with {status}")),
            Ok(Err(error)) => {
                process.shutdown().await;
                Err(format!("chat agent I/O failed: {error}"))
            }
            Err(_) => {
                process.shutdown().await;
                Err("chat agent timed out and its process was stopped".into())
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn owned_process_gets_message_as_data_and_real_exit_status() {
        let data = "--option\n'$(not a command)' 中文";
        let mut program = Program {
            program: "/bin/cat".into(),
            args: Vec::new(),
            argv_message: false,
        };
        assert_eq!(program.run(data).await.unwrap(), data);
        program.program = "/usr/bin/printf".into();
        program.args = strings(&["%s"]);
        program.argv_message = true;
        assert_eq!(program.run(data).await.unwrap(), data);
        program.program = "/usr/bin/false".into();
        assert!(program.run(data).await.unwrap_err().contains("exited"));
    }
}
