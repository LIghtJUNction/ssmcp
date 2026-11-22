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
            Agent::Remote => return Err(
                "remote worker tasks are claimed through worker tools, not a local chat executable"
                    .into(),
            ),
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Program {
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

// Output can contain arbitrary provider data, including echoed credentials.
// Report only stable diagnostic categories and byte counts on failure.
fn failure_output(stdout: &[u8], stderr: &[u8]) -> String {
    let cause = [stderr, stdout].into_iter().find_map(|stream| {
        let text = String::from_utf8_lossy(stream).to_ascii_lowercase();
        [
            ("permission denied", "permission denied"),
            ("authentication failed", "authentication failed"),
            ("not logged in", "sign-in required"),
            ("unauthorized", "authentication rejected"),
            ("unknown option", "unsupported command option"),
            ("unknown model", "model unavailable"),
            ("rate limit", "rate limited"),
        ]
        .into_iter()
        .find_map(|(needle, code)| text.contains(needle).then_some(code))
    });
    format!(
        "{}stdout {} bytes, stderr {} bytes",
        cause.map_or(String::new(), |cause| format!("{cause}; ")),
        stdout.len(),
        stderr.len()
    )
}

impl Program {
    #[cfg(target_os = "linux")]
    pub(crate) fn for_worker(agent: &Agent) -> Result<Self, String> {
        match Target::resolve(agent)? {
            Target::Program(program) => Ok(program),
            _ => Err("a local worker executor requires an explicit --agent codex, claude, opencode or exec; the default remote pool uses worker task tools instead".into()),
        }
    }

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
        self.run_cancellable(text, None).await
    }

    pub(crate) async fn run_cancellable(
        &self,
        text: &str,
        mut cancelled: Option<tokio::sync::watch::Receiver<bool>>,
    ) -> Result<String, String> {
        if cancelled
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow())
        {
            return Err("worker task cancelled before process dispatch".into());
        }
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
        let cancel = async {
            let Some(receiver) = cancelled.as_mut() else {
                return std::future::pending::<()>().await;
            };
            while !*receiver.borrow_and_update() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        };
        let result = tokio::select! {
            biased;
            result = tokio::time::timeout(PROCESS_LIMIT, async {
                tokio::try_join!(
                    async { Ok::<_, io::Error>(write.await) },
                    bounded_output(stdout),
                    bounded_output(stderr),
                    process.wait()
                )
            }) => Some(result),
            _ = cancel => None,
        };
        let Some(result) = result else {
            process.shutdown().await;
            return Err(
                "worker task cancelled after owned process cleanup; prior effects may remain"
                    .into(),
            );
        };
        match result {
            Ok(Ok((written, output, _, status))) if status.success() => {
                written.map_err(|error| format!("chat agent input failed: {error}"))?;
                let text =
                    String::from_utf8(output).map_err(|_| "chat agent output is not UTF-8")?;
                Ok(crate::audit::event(serde_json::Value::String(text))
                    .as_str()
                    .unwrap_or_default()
                    .to_owned())
            }
            Ok(Ok((_, stdout, stderr, status))) => Err(format!(
                "chat agent exited with {status} ({})",
                failure_output(&stdout, &stderr)
            )),
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

    #[test]
    fn remote_workers_never_resolve_to_a_local_chat_executable() {
        assert!(Target::resolve(&Agent::Remote).is_err());
        assert!(matches!(
            Target::resolve(&Agent::NextCaller).unwrap(),
            Target::NextCaller
        ));
    }

    #[test]
    fn failure_output_keeps_the_error_category_without_raw_provider_data() {
        let diagnostic = failure_output(
            b"provider stdout with opaque secret",
            b"Permission denied: /secret/path token=arbitrary-secret",
        );
        assert_eq!(
            diagnostic,
            "permission denied; stdout 34 bytes, stderr 54 bytes"
        );
    }

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
