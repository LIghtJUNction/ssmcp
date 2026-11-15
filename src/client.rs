//! Foreground SSH client. Credentials are emitted once, as a JSON object on stdout.
use crate::{
    pty::Terminal,
    wire::{Frame, MAX_INFLIGHT, VERSION, read_frame, write_frame},
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

const HELP: &str = "ssmcp — expose one local terminal through an SSH-authenticated MCP endpoint\n\nUsage: ssmcp [-i identity] [-p port] [-F ssh_config] [--shell /path/to/shell] user@host\n\nUses SSH public-key authentication and the standard known_hosts checks.\nPrints {\"url\":\"https://host/user\",\"secret\":\"...\"} once on stdout.\nKeep this process running; Ctrl-C disconnects and closes the terminal.\n";

struct Options {
    destination: String,
    identity: Option<OsString>,
    port: Option<u16>,
    config: Option<OsString>,
    shell: PathBuf,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let mut destination = None;
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
                    shell = PathBuf::from(args.next().ok_or("--shell requires an absolute path")?)
                }
                Some(value) if !value.starts_with('-') && destination.is_none() => {
                    destination = Some(value.to_owned())
                }
                _ => return Err("unexpected argument; see ssmcp --help".into()),
            }
        }
        let destination = destination.ok_or("expected user@host; see ssmcp --help")?;
        validate_destination(&destination)?;
        if !shell.is_absolute() {
            return Err("--shell requires an absolute path".into());
        }
        Ok(Some(Self {
            destination,
            identity,
            port,
            config,
            shell,
        }))
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
            .arg(&self.destination)
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
    let mut ssh = options
        .command()
        .spawn()
        .map_err(|e| format!("could not start OpenSSH ssh: {e}"))?;
    let mut writer = ssh.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut reader = BufReader::new(ssh.stdout.take().ok_or("SSH stdout unavailable")?);
    write_frame(&mut writer, &Frame::Hello { version: VERSION }).await?;
    let ready = tokio::select! {
        result = timeout(Duration::from_secs(120),read_frame(&mut reader)) => result.map_err(|_| "SSH authentication timed out")??,
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(()); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(()); },
    };
    let (url, secret) = match ready {
        Some(Frame::Ready { url, secret }) => (url, secret),
        Some(Frame::Reject { message }) => return Err(message.into()),
        None => return Err(
            "SSH disconnected before registration; check the SSH key, host key, and server setup"
                .into(),
        ),
        _ => return Err("unexpected registration response".into()),
    };
    let terminal = Terminal::spawn(&options.shell)?;
    let output = json!({"url":url,"secret":secret});
    if let Err(error) = writeln!(io::stdout().lock(), "{output}").and_then(|_| io::stdout().flush())
    {
        terminal.shutdown().await;
        return Err(error.into());
    }
    eprintln!("Connected. Keep ssmcp running; Ctrl-C disconnects and closes the terminal.");
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
                    let terminal = terminal.clone();
                    let writer = writer.clone();
                    requests.spawn(async move {
                        let result = terminal.perform(operation).await;
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
    terminal.shutdown().await;
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
