//! Foreground SSH client. Credentials are emitted once, as a JSON object on stdout.
use crate::{
    backend::Backend,
    wire::{
        ClientMetadata, Frame, MAX_FRAME, MAX_INFLIGHT, Mode, VERSION, read_frame, write_frame,
    },
};
use serde_json::json;
use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::BufReader,
    process::Command,
    signal::unix::{SignalKind, signal},
    sync::{Mutex, mpsc},
    task::JoinSet,
    time::timeout,
};

const HELP: &str = "ssmcp — expose a local shell or coding agent over an SSH-authenticated MCP endpoint\n\nUsage: ssmcp [-t shell|codex|cc|pi|opencode] [options] user@host [-- backend_args...]\n       ssmcp -t TYPE [--backend PROGRAM] --list-tools [-- backend_args...]\n\n  -t, --type TYPE       Backend (default: shell)\n  -i identity          SSH private key file\n  -p port              SSH port\n  -F ssh_config        OpenSSH configuration file\n  --name NAME          Display name (default: hostname-type-pid)\n  --shell /path        Shell executable (shell mode only)\n  --backend PROGRAM    Override the native agent executable\n  --list-tools         Discover local tools as JSON, then exit without SSH\n\nUses only SSH public-key authentication and standard known_hosts checks.\nPrints {\"url\":\"https://host/user\",\"secret\":\"...\",\"client_id\":\"...\",\"type\":\"shell\"} once on stdout.\nLive input, output and native agent events appear on stderr.\nKeep this process running; Ctrl-C disconnects and stops the local backend.\n";

struct Options {
    destination: Option<String>,
    name: Option<String>,
    mode: Mode,
    backend: Option<OsString>,
    backend_args: Vec<OsString>,
    list_tools: bool,
    identity: Option<OsString>,
    port: Option<u16>,
    config: Option<OsString>,
    shell: PathBuf,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let mut destination = None;
        let mut name = None;
        let mut mode = Mode::Shell;
        let mut backend = None;
        let mut backend_args = Vec::new();
        let mut list_tools = false;
        let mut explicit_shell = false;
        let (mut identity, mut port, mut config) = (None, None, None);
        let mut shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"));
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-h" | "--help") => {
                    print!("{HELP}");
                    return Ok(None);
                }
                Some("-V" | "--version") => {
                    println!("ssmcp {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                Some("-t" | "--type") => {
                    let value = args.next().ok_or("-t requires a backend type")?;
                    mode = Mode::parse(value.to_str().ok_or("backend type must be UTF-8")?)?;
                }
                Some("--backend") => {
                    backend = Some(args.next().ok_or("--backend requires a program")?)
                }
                Some("--name") => {
                    name = Some(
                        args.next()
                            .ok_or("--name requires a client name")?
                            .into_string()
                            .map_err(|_| "client name must be UTF-8")?,
                    );
                }
                Some("--list-tools") => list_tools = true,
                Some("--") => {
                    backend_args.extend(args);
                    break;
                }
                Some("-i") => identity = Some(args.next().ok_or("-i requires a file")?),
                Some("-F") => config = Some(args.next().ok_or("-F requires a file")?),
                Some("-p") => {
                    port = Some(
                        args.next()
                            .and_then(|p| p.to_str().and_then(|s| s.parse::<u16>().ok()))
                            .filter(|p| *p > 0)
                            .ok_or("-p requires a port in 1..=65535")?,
                    );
                }
                Some("--shell") => {
                    explicit_shell = true;
                    shell = PathBuf::from(args.next().ok_or("--shell requires an absolute path")?)
                }
                Some(value) if !value.starts_with('-') && destination.is_none() => {
                    destination = Some(value.to_owned())
                }
                _ => return Err("unexpected argument; see ssmcp --help".into()),
            }
        }
        if let Some(destination) = &destination {
            validate_destination(destination)?;
        } else if !list_tools {
            return Err("expected user@host; see ssmcp --help".into());
        }
        if list_tools && destination.is_some() {
            return Err("--list-tools is local and does not take an SSH destination".into());
        }
        if mode == Mode::Shell && (backend.is_some() || !backend_args.is_empty()) {
            return Err(
                "shell mode uses --shell; --backend and backend arguments select an agent only"
                    .into(),
            );
        }
        if mode != Mode::Shell && explicit_shell {
            return Err("--shell is only valid with -t shell".into());
        }
        if !shell.is_absolute() {
            return Err("--shell requires an absolute path".into());
        }
        if let Some(name) = &name
            && (name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control))
        {
            return Err("client name must be nonempty, contain no control characters, and be at most 128 bytes".into());
        }
        Ok(Some(Self {
            destination,
            name,
            mode,
            backend,
            backend_args,
            list_tools,
            identity,
            port,
            config,
            shell,
        }))
    }

    fn metadata(&self) -> crate::Result<ClientMetadata> {
        let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")?
            .trim_end_matches('\n')
            .to_owned();
        let cwd = std::env::current_dir()?
            .into_os_string()
            .into_string()
            .map_err(|_| "client cwd must be UTF-8")?;
        let short_hostname: String = hostname.chars().take(24).collect();
        let metadata = ClientMetadata {
            name: self.name.clone().unwrap_or_else(|| {
                format!(
                    "{short_hostname}-{}-{}",
                    self.mode.name(),
                    std::process::id()
                )
            }),
            cwd,
            hostname,
        };
        metadata.validate()?;
        Ok(metadata)
    }

    fn command(&self) -> Command {
        let mut command = Command::new("ssh");
        command.arg("-T");
        // OpenSSH uses the first value for each option, so enforce these before user config.
        for option in [
            "PreferredAuthentications=publickey",
            "PubkeyAuthentication=yes",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "GSSAPIAuthentication=no",
            "HostbasedAuthentication=no",
            "ForwardAgent=no",
            "ForwardX11=no",
            "ClearAllForwardings=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "RequestTTY=no",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=3",
        ] {
            command.args(["-o", option]);
        }
        if let Some(path) = &self.identity {
            command.arg("-i").arg(path);
        }
        if let Some(port) = self.port {
            command.arg("-p").arg(port.to_string());
        }
        if let Some(config) = &self.config {
            command.arg("-F").arg(config);
        }
        command
            .arg("--")
            .arg(self.destination.as_ref().expect("SSH destination"))
            .args(["ssmcpd", "attach"]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        command
    }
}

fn validate_destination(destination: &str) -> Result<(), String> {
    let (user, host) = destination
        .split_once('@')
        .ok_or("destination must be user@host")?;
    if user.is_empty()
        || user.len() > 64
        || user.starts_with('-')
        || !user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("invalid SSH username".into());
    }
    if host.is_empty()
        || host.len() > 253
        || host.starts_with('-')
        || !host.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':' | b'[' | b']')
        })
    {
        return Err("invalid SSH hostname or address".into());
    }
    Ok(())
}

pub async fn run() -> crate::Result<()> {
    let Some(options) = Options::parse(std::env::args_os().skip(1))? else {
        return Ok(());
    };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    if options.list_tools {
        let backend = tokio::select! {
            result = timeout(Duration::from_secs(120), Backend::start(options.mode, &options.shell, options.backend.as_deref(), &options.backend_args)) => result.map_err(|_| "local backend startup timed out")??,
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        };
        let tools = tokio::select! {
            result = timeout(Duration::from_secs(120),backend.tools()) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
            _ = interrupt.recv() => { backend.shutdown().await; return Ok(()); },
            _ = terminate.recv() => { backend.shutdown().await; return Ok(()); },
        };
        backend.shutdown().await;
        println!("{}", json!({"type":options.mode.name(),"tools":tools?}));
        return Ok(());
    }
    let metadata = options.metadata()?;
    let mut ssh = options
        .command()
        .spawn()
        .map_err(|e| format!("could not start OpenSSH ssh: {e}"))?;
    let mut writer = ssh.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut reader = BufReader::new(ssh.stdout.take().ok_or("SSH stdout unavailable")?);
    write_frame(
        &mut writer,
        &Frame::Hello {
            version: VERSION,
            mode: options.mode,
            metadata: metadata.clone(),
        },
    )
    .await?;
    let ready = tokio::select! {
        result = timeout(Duration::from_secs(120),read_frame(&mut reader)) => result.map_err(|_| "SSH authentication timed out")??,
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(()); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(()); },
    };
    let (url, secret, client_id) = match ready {
        Some(Frame::Ready { url, secret, client_id }) => (url, secret, client_id),
        Some(Frame::Reject { message }) => return Err(message.into()),
        None => return Err(
            "SSH disconnected before registration; check the SSH key, host key, server setup, and matching ssmcp/ssmcpd versions"
                .into(),
        ),
        _ => return Err("unexpected registration response".into()),
    };
    eprintln!("Starting local {} backend...", options.mode.name());
    let backend = tokio::select! {
        result = timeout(Duration::from_secs(120), Backend::start(options.mode, &options.shell, options.backend.as_deref(), &options.backend_args)) => result.map_err(|_| "local backend startup timed out")??,
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(()); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(()); },
    };
    // Discover before publishing credentials, so a missing/incompatible backend fails locally.
    let discovered = tokio::select! {
        result = timeout(Duration::from_secs(120), backend.tools()) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
        _ = interrupt.recv() => { backend.shutdown().await; let _ = ssh.kill().await; return Ok(()); },
        _ = terminate.recv() => { backend.shutdown().await; let _ = ssh.kill().await; return Ok(()); },
    };
    if let Err(error) = discovered {
        backend.shutdown().await;
        return Err(error.into());
    }
    let output = json!({"url":url,"secret":secret,"client_id":client_id,"name":metadata.name,"cwd":metadata.cwd,"hostname":metadata.hostname,"type":options.mode.name()});
    if let Err(error) = writeln!(io::stdout().lock(), "{output}").and_then(|_| io::stdout().flush())
    {
        backend.shutdown().await;
        return Err(error.into());
    }
    eprintln!(
        "Connected ({}). This secret controls all clients of the same SSH account while this connection is live. Live activity is shown here; Ctrl-C disconnects and revokes this secret.",
        options.mode.name()
    );
    // One reader owns partial frames, so a completed request cannot cancel and discard them.
    let (frames_tx, mut frames) = mpsc::channel(MAX_INFLIGHT);
    let reader_task = tokio::spawn(async move {
        loop {
            let frame = read_frame(&mut reader).await;
            let finished = !matches!(frame, Ok(Some(_)));
            if frames_tx.send(frame).await.is_err() || finished {
                break;
            }
        }
    });
    let writer = Arc::new(Mutex::new(writer));
    let mut requests: JoinSet<io::Result<()>> = JoinSet::new();
    let result: crate::Result<()> = 'connected: loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Some(Ok(Some(Frame::Call {id,operation}))) => {
                    while let Some(completed) = requests.try_join_next() {
                        match completed {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => break 'connected Err(error.into()),
                            Err(error) => break 'connected Err(error.into()),
                        }
                    }
                    if requests.len() >= MAX_INFLIGHT { break Err("server exceeded the in-flight request limit".into()); }
                    let backend = backend.clone();
                    let writer = writer.clone();
                    requests.spawn(async move {
                        let mut result = backend.perform(operation).await;
                        // A large native response must not corrupt/disconnect the SSH stream.
                        if serde_json::to_vec(&result).map_or(true, |bytes|bytes.len() + 256 > MAX_FRAME) {
                            result = Err("native call finished but its result exceeds the 4 MiB bridge frame limit; do not blindly repeat a mutating call, inspect native state or request a smaller output page".into());
                        }
                        if let Err(error) = &result { eprintln!("[ssmcp error] {error}"); }
                        timeout(Duration::from_secs(10),async {
                            let mut writer = writer.lock().await;
                            write_frame(&mut *writer,&Frame::Reply {id,result}).await
                        }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut,"SSH write timed out"))?
                    });
                }
                Some(Ok(None)) | None => break Ok(()),
                Some(Ok(Some(Frame::Reject {message}))) => break Err(message.into()),
                Some(Ok(Some(_))) => break Err("unexpected SSH protocol frame".into()),
                Some(Err(error)) => break Err(error.into()),
            },
            response = requests.join_next(), if !requests.is_empty() => match response {
                Some(Ok(Ok(()))) => {},
                Some(Ok(Err(error))) => break Err(error.into()),
                Some(Err(error)) => break Err(error.into()),
                None => {},
            },
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
        }
    };
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    reader_task.abort();
    let _ = reader_task.await;
    drop(writer);
    let _ = ssh.start_kill();
    backend.shutdown().await;
    let _ = ssh.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_validation_rejects_shell_and_option_injection() {
        for destination in [
            "user@host",
            "user@127.0.0.1",
            "user@[::1]",
            "user@ssmcp-test",
        ] {
            assert!(validate_destination(destination).is_ok());
        }
        for destination in [
            "host",
            "user@host;id",
            "-o@host",
            "user@-oProxyCommand=id",
            "user@a@b",
            "user@host\n",
            "@host",
        ] {
            assert!(validate_destination(destination).is_err());
        }
    }

    #[test]
    fn client_names_are_bounded_display_metadata() {
        let options = Options::parse(
            ["--name", "My workstation", "-t", "codex", "user@host"].map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let info = options.metadata().unwrap();
        assert_eq!(info.name, "My workstation");
        assert!(std::path::Path::new(&info.cwd).is_absolute());
        assert!(!info.hostname.is_empty());
        for bad in ["".into(), "   ".into(), "bad\nname".into(), "a".repeat(129)] {
            assert!(
                Options::parse([
                    OsString::from("--name"),
                    OsString::from(bad),
                    OsString::from("user@host")
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn ssh_command_enforces_public_keys_without_overriding_host_trust() {
        let options = Options::parse(
            ["-F", "config", "-p", "2222", "-i", "key", "user@alias"].map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let command = options.command();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|v| v.to_string_lossy().to_string())
            .collect();
        assert!(args.contains(&"PasswordAuthentication=no".into()));
        assert!(args.contains(&"ControlPath=none".into()));
        assert!(args.contains(&"PreferredAuthentications=publickey".into()));
        assert!(
            !args
                .iter()
                .any(|v| v.contains("StrictHostKeyChecking") || v.contains("BatchMode"))
        );
        assert_eq!(
            &args[args.len() - 4..],
            ["--", "user@alias", "ssmcpd", "attach"]
        );
    }
}
