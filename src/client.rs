//! Foreground SSH client. Connection details are emitted once as JSON on stdout.
mod git_status;
mod input;
mod panel;
pub(crate) mod reload;
pub(crate) mod render;
pub(crate) mod terminal;
mod ui;

use crate::{
    backend::Backend,
    platform::{interrupt_signal, terminate_signal},
    runtime::Runtime,
    wire::{
        ClientMetadata, Frame, MAX_ALLOWED_PRINCIPALS, MAX_FRAME, MAX_INFLIGHT, Mode, VERSION,
        canonical_github_principal, read_frame, validate_title, write_frame,
    },
};
use serde_json::json;
use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStdout, Command},
    sync::{Mutex, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const HELP: &str = r#"ssmcp — expose a local shell and every installed coding harness over SSH

Usage: ssmcp [options] [user@]host
       ssmcp sessions <list|status|start|stop|restart|logs|auth|remove> [NAME]

  -i identity          SSH private key file
  -p port              SSH port
  -F ssh_config        OpenSSH configuration file
  --name NAME          Display name (default: hostname-pid)
  --daemon             Start a quiet, user-owned native background service
  --watchdog           Retry disconnected/failed connections until explicitly stopped
  --reconnect-delay N  Seconds between retries, 1..=3600 (default 5; needs --watchdog)
  --title TITLE        Session task shown to agents (at most 512 UTF-8 bytes)
  --allow USER@github  Allow GitHub identities; repeat or separate with commas
                       (at most 32 distinct users; case-insensitive)
  --shell /path        Shell executable (default: platform shell)
  --context-root PATH  Add a read-only skill/instruction root (repeat up to 8 times)
  --log-format FORMAT  pretty (default), pretty-verbose, or jsonl
  --no-color           Disable ANSI colors (also honors NO_COLOR)
  --verbose            Alias for --log-format pretty-verbose
  --raw, --raw-events   Alias for --log-format jsonl
  --ui-config FILE     Local [ui] TOML; default ~/.config/ssmcp/config.toml
  --theme NAME         minimal, codex-dark, nord, or solarized
  --timestamps         Show UTC timestamps on event headings
  --max-inline-output N  Maximum lines per buffered output block (default 12)
  --list-tools         Discover native and workspace tools as JSON, then exit without SSH

The client always connects as shell. Installed Codex, Claude Code, Pi and OpenCode
harnesses are discovered automatically and their native tools are registered in
one code_mode. Tools keep their native schemas and permission/approval behavior.

Examples:
  ssmcp --name laptop my-server
  ssmcp --title "Fix ssmcp client reload" my-server
  ssmcp -F ~/.ssh/work_config my-server
  ssmcp -i ~/.ssh/id_ed25519 -p 2222 alice@ssmcp.example
  ssmcp --allow octocat@github,alice@github my-server
  ssmcp --daemon --watchdog --name workstation --allow octocat@github my-server
  ssmcp sessions status workstation
  ssmcp sessions stop workstation

Redirected stdout (or jsonl mode) emits one connection JSON object.
Interactive stdout shows the MCP URL and OAuth login instructions.
Add url to your MCP client, then enter login_code on its OAuth login page.
login_expires_in deducts local backend startup time.
With --allow, prints url, client information, allow and auth:"github" instead
of a login_code. Add url to the MCP client and sign in with an allowed GitHub
account in the browser. The CLI needs no GitHub login; SSH keys stay unchanged.
--allow requires a server that supports GitHub authorization and cannot be
combined with --list-tools.
Live input, output and native tool events appear on stderr as readable blocks.
Independent commands run concurrently and support status, output, wait and cancel.
Color is automatic on terminals; NO_COLOR or redirected stderr disables ANSI.
Unified diffs show old/new line numbers. --raw and --raw-events select jsonl.
--verbose expands folded output and shows the full original JSON event.
Workspace file operations are rooted at the startup directory. Loopback TCP
forwards work on every client; system rsync transfers require Linux clients.
Skill summaries and AGENTS.md views stay inside the workspace unless an operator
adds --context-root. These extra roots expose context documents, not file writes.
Keep this process running; Ctrl-C disconnects and stops local harnesses and jobs.
Without --allow, disconnecting revokes this connection's OAuth grants and
reconnecting needs a new OAuth login. An MCP app restart can reuse valid tokens.
Background services use systemd --user on Linux, launchd on macOS, and a current-
user Scheduled Task on Windows. They do not enable startup at boot or login.
Use sessions auth NAME to read private OAuth connection details explicitly.
Background logs use JSONL without terminal colors; foreground display options
do not change service logging. Stopping a session suppresses its watchdog.
Reconnection starts a fresh client; jobs from the disconnected client are cleaned up.
"#;

// A single reader owns every partial frame after Ready, including while a native
// backend starts. A separate persistent status channel records a closed bridge
// even when its frame queue has not yet been consumed.
// A reconnect owns its stderr reader, including early failures.
struct SshStderr(JoinHandle<()>);

impl Drop for SshStderr {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Incoming {
    frames: mpsc::Receiver<io::Result<Option<Frame>>>,
    closed: watch::Receiver<Option<String>>,
    task: JoinHandle<()>,
}

impl Incoming {
    fn start(mut reader: BufReader<ChildStdout>) -> Self {
        let (frames_tx, frames) = mpsc::channel(MAX_INFLIGHT);
        let (closed_tx, closed) = watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                let frame = read_frame(&mut reader).await;
                let error = match &frame {
                    Ok(None) => Some("SSH disconnected before local backend readiness".into()),
                    Err(error) => Some(format!(
                        "SSH stream failed before local backend readiness: {error}"
                    )),
                    Ok(Some(Frame::Reject { message })) => {
                        Some(format!("SSH server rejected the connection: {message}"))
                    }
                    _ => None,
                };
                let finished = error.is_some();
                if let Some(error) = error {
                    let _ = closed_tx.send(Some(error));
                }
                if frames_tx.send(frame).await.is_err() || finished {
                    break;
                }
            }
        });
        Self {
            frames,
            closed,
            task,
        }
    }

    async fn disconnected(&mut self) -> String {
        loop {
            if let Some(error) = self.closed.borrow().clone() {
                return error;
            }
            if self.closed.changed().await.is_err() {
                return "SSH reader stopped before local backend readiness".into();
            }
        }
    }

    fn ensure_open(&self, ssh: &mut Child) -> Result<(), String> {
        if let Some(error) = self.closed.borrow().clone() {
            return Err(error);
        }
        if self.closed.has_changed().is_err() {
            return Err("SSH reader stopped before local backend readiness".into());
        }
        // The process can exit before its reader task observes EOF (or a proxy
        // child can retain stdout). Check both sources before publishing a code.
        if let Some(status) = ssh.try_wait().map_err(|error| error.to_string())? {
            return Err(ssh_ended(Ok(status)));
        }
        Ok(())
    }

    async fn stop(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Incoming {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn ssh_ended(status: io::Result<ExitStatus>) -> String {
    match status {
        Ok(status) => format!("SSH exited before local backend readiness ({status})"),
        Err(error) => format!("Could not wait for SSH during local backend startup: {error}"),
    }
}

struct Options {
    destination: Option<String>,
    name: Option<String>,
    title: Option<String>,
    allow: Vec<String>,
    context_roots: Vec<PathBuf>,
    list_tools: bool,
    daemon: bool,
    watchdog: bool,
    reconnect_delay_secs: u64,
    log_format: Option<render::LogFormat>,
    ui_config: Option<PathBuf>,
    theme: Option<ui::Theme>,
    timestamps: bool,
    max_inline_output: Option<usize>,
    no_color: bool,
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
        let mut title = None;
        let mut allow = Vec::new();
        let mut context_roots = Vec::new();
        let mut list_tools = false;
        let mut daemon = false;
        let mut watchdog = false;
        let mut reconnect_delay_secs = None;
        let mut log_format = None;
        let mut ui_config = None;
        let mut theme = None;
        let mut timestamps = false;
        let mut max_inline_output = None;
        let mut no_color = false;
        let (mut identity, mut port, mut config) = (None, None, None);
        let mut shell = crate::platform::default_shell();
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
                Some("-t" | "--type" | "--backend") => {
                    return Err("harness selection was removed; remove -t/--type/--backend and connect directly: ssmcp [options] user@server; installed harnesses are discovered automatically".into());
                }
                Some(value)
                    if value.starts_with("--type=")
                        || value.starts_with("--backend=")
                        || value.starts_with("-t=") =>
                {
                    return Err("harness selection was removed; remove -t/--type/--backend; installed harnesses are discovered automatically".into());
                }
                Some("--name") => {
                    name = Some(
                        args.next()
                            .ok_or("--name requires a client name")?
                            .into_string()
                            .map_err(|_| "client name must be UTF-8")?,
                    );
                }
                Some("--title") => {
                    title = Some(
                        args.next()
                            .ok_or("--title requires a session title")?
                            .into_string()
                            .map_err(|_| "session title must be UTF-8")?,
                    );
                }
                Some(value) if value.starts_with("--title=") => {
                    title = Some(value["--title=".len()..].to_owned());
                }
                Some("--allow") => {
                    let value = args.next().ok_or("--allow requires username@github")?;
                    add_allowed_principals(
                        &mut allow,
                        value.to_str().ok_or("--allow must be UTF-8")?,
                    )?;
                }
                Some("--context-root") => {
                    if context_roots.len() >= 8 {
                        return Err("--context-root accepts at most 8 directories".into());
                    }
                    let value = args.next().ok_or("--context-root requires a directory")?;
                    if value.is_empty()
                        || value.to_string_lossy().len() > 4096
                        || value.to_string_lossy().chars().any(char::is_control)
                    {
                        return Err("--context-root requires a valid directory path".into());
                    }
                    let path = PathBuf::from(value)
                        .canonicalize()
                        .map_err(|error| format!("--context-root: {error}"))?;
                    if !path.is_dir() {
                        return Err("--context-root must name an existing directory".into());
                    }
                    context_roots.push(path);
                }
                Some("--list-tools") => list_tools = true,
                Some("--daemon") => daemon = true,
                Some("--watchdog") => watchdog = true,
                Some("--reconnect-delay") => {
                    reconnect_delay_secs = Some(
                        args.next()
                            .and_then(|value| {
                                value.to_str().and_then(|value| value.parse::<u64>().ok())
                            })
                            .filter(|value| (1..=3600).contains(value))
                            .ok_or("--reconnect-delay must be in 1..=3600 seconds")?,
                    );
                }
                Some("--log-format") => {
                    let value = args
                        .next()
                        .ok_or("--log-format requires pretty, pretty-verbose, or jsonl")?;
                    log_format = Some(render::LogFormat::parse(
                        value.to_str().ok_or("--log-format must be UTF-8")?,
                    )?);
                }
                Some("--no-color") => no_color = true,
                Some("--verbose") => log_format = Some(render::LogFormat::PrettyVerbose),
                Some("--raw" | "--raw-events") => log_format = Some(render::LogFormat::Jsonl),
                Some("--ui-config") => {
                    ui_config = Some(PathBuf::from(
                        args.next().ok_or("--ui-config requires a file")?,
                    ))
                }
                Some("--theme") => {
                    let value = args.next().ok_or("--theme requires a name")?;
                    theme = Some(ui::Theme::parse(
                        value.to_str().ok_or("--theme must be UTF-8")?,
                    )?);
                }
                Some("--timestamps") => timestamps = true,
                Some("--max-inline-output") => {
                    max_inline_output = Some(
                        args.next()
                            .and_then(|a| a.to_str().and_then(|v| v.parse::<usize>().ok()))
                            .filter(|n| (1..=10000).contains(n))
                            .ok_or("--max-inline-output must be in 1..=10000")?,
                    );
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
                    shell = PathBuf::from(args.next().ok_or("--shell requires an absolute path")?);
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
            return Err("expected an SSH host alias or user@host; see ssmcp --help".into());
        }
        if list_tools && destination.is_some() {
            return Err("--list-tools is local and does not take an SSH destination".into());
        }
        if list_tools && (daemon || watchdog || reconnect_delay_secs.is_some()) {
            return Err("--list-tools cannot be combined with --daemon or --watchdog".into());
        }
        if reconnect_delay_secs.is_some() && !watchdog {
            return Err("--reconnect-delay requires --watchdog".into());
        }
        if list_tools && !allow.is_empty() {
            return Err(
                "--allow requires an SSH connection and cannot be used with --list-tools".into(),
            );
        }
        if !shell.is_absolute() {
            return Err("--shell requires an absolute path".into());
        }
        validate_title(title.as_deref())?;
        if let Some(name) = &name
            && (name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control))
        {
            return Err("client name must be nonempty, contain no control characters, and be at most 128 bytes".into());
        }
        Ok(Some(Self {
            destination,
            name,
            title,
            allow,
            context_roots,
            list_tools,
            daemon,
            watchdog,
            reconnect_delay_secs: reconnect_delay_secs.unwrap_or(5),
            log_format,
            ui_config,
            theme,
            timestamps,
            max_inline_output,
            no_color,
            identity,
            port,
            config,
            shell,
        }))
    }

    fn metadata(&self) -> crate::Result<ClientMetadata> {
        let hostname = crate::platform::hostname()?;
        let cwd = std::env::current_dir()?
            .into_os_string()
            .into_string()
            .map_err(|_| "client cwd must be UTF-8")?;
        let short_hostname: String = hostname.chars().take(24).collect();
        let metadata = ClientMetadata {
            title: self.title.clone(),
            name: self
                .name
                .clone()
                .unwrap_or_else(|| format!("{short_hostname}-{}", std::process::id())),
            cwd,
            hostname,
            allow: self.allow.clone(),
        };
        metadata.validate()?;
        Ok(metadata)
    }

    /// Store only validated connection settings. Service stdout/stderr have a
    /// fixed non-interactive format, so display themes are deliberately omitted.
    fn session_spec(&self) -> crate::Result<crate::session::SessionSpec> {
        let metadata = self.metadata()?;
        let mut args = vec![OsString::from("--name"), OsString::from(&metadata.name)];
        for (flag, value) in [("-i", &self.identity), ("-F", &self.config)] {
            if let Some(value) = value {
                args.extend([OsString::from(flag), value.clone()]);
            }
        }
        if let Some(port) = self.port {
            args.extend([OsString::from("-p"), OsString::from(port.to_string())]);
        }
        if let Some(title) = &self.title {
            args.extend([OsString::from("--title"), OsString::from(title)]);
        }
        for principal in &self.allow {
            args.extend([OsString::from("--allow"), OsString::from(principal)]);
        }
        for path in &self.context_roots {
            args.extend([
                OsString::from("--context-root"),
                path.clone().into_os_string(),
            ]);
        }
        args.extend([
            OsString::from("--shell"),
            self.shell.clone().into_os_string(),
        ]);
        if self.watchdog {
            args.extend([
                OsString::from("--watchdog"),
                OsString::from("--reconnect-delay"),
                OsString::from(self.reconnect_delay_secs.to_string()),
            ]);
        }
        args.push(OsString::from(
            self.destination.as_ref().ok_or("missing SSH destination")?,
        ));
        let args = args
            .into_iter()
            .map(|arg| {
                arg.into_string()
                    .map_err(|_| "background session arguments must be UTF-8".into())
            })
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(crate::session::SessionSpec {
            name: metadata.name,
            cwd: PathBuf::from(metadata.cwd),
            args,
            watchdog: self.watchdog,
            reconnect_delay_secs: self.reconnect_delay_secs,
        })
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
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

fn add_allowed_principals(allow: &mut Vec<String>, value: &str) -> Result<(), String> {
    for entry in value.split(',') {
        let principal = canonical_github_principal(entry.trim())
            .map_err(|error| format!("--allow: {error}"))?;
        if !allow.contains(&principal) {
            if allow.len() == MAX_ALLOWED_PRINCIPALS {
                return Err(format!(
                    "--allow accepts at most {MAX_ALLOWED_PRINCIPALS} distinct GitHub users"
                ));
            }
            allow.push(principal);
        }
    }
    Ok(())
}

fn validate_destination(destination: &str) -> Result<(), String> {
    // Let OpenSSH resolve Host aliases and User from its own configuration.
    // An explicit user@host still takes precedence, just as it does with ssh.
    let host = if let Some((user, host)) = destination.split_once('@') {
        if user.is_empty()
            || user.len() > 64
            || user.starts_with('-')
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        {
            return Err("invalid SSH username".into());
        }
        host
    } else {
        destination
    };
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
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__rsync-transport")) {
        return crate::rsync::transport_main().await;
    }
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "sessions") {
        return crate::session::manage(args.into_iter().skip(1).collect()).await;
    }
    let mut service = if args.first().is_some_and(|arg| arg == "__session-run") {
        if args.len() != 2 {
            return Err("internal session runner requires exactly one session name".into());
        }
        let name = args[1].to_str().ok_or("session name must be UTF-8")?;
        let runner = crate::session::Runner::open(name)?;
        if runner.stop_requested() {
            runner.record("stopped", 0, None, None, None)?;
            return Ok(());
        }
        std::env::set_current_dir(&runner.spec().cwd)?;
        args = runner.spec().args.iter().map(OsString::from).collect();
        Some(runner)
    } else {
        None
    };
    let Some(options) = Options::parse(args)? else {
        return Ok(());
    };
    if let Some(runner) = &service
        && (options.daemon
            || options.list_tools
            || options.watchdog != runner.spec().watchdog
            || options.reconnect_delay_secs != runner.spec().reconnect_delay_secs)
    {
        return Err("saved background session options do not match its manifest".into());
    }
    if options.daemon {
        let result = crate::session::start(options.session_spec()?).await?;
        use std::io::IsTerminal;
        if io::stdout().is_terminal() {
            let name = result
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("session");
            println!(
                "Started background session {name}. Check connection: ssmcp sessions status {name}"
            );
        } else {
            println!("{result}");
        }
        return Ok(());
    }
    let mut ui = if service.is_some() {
        ui::Ui::default()
    } else {
        ui::Ui::load(options.ui_config.as_deref())?
    };
    if let Some(format) = options.log_format {
        ui.log_format = format;
    }
    if let Some(theme) = options.theme {
        ui.theme = theme;
    }
    if let Some(limit) = options.max_inline_output {
        ui.max_inline_output = limit;
    }
    ui.timestamps |= options.timestamps;
    ui.color &= !options.no_color;
    if service.is_some() {
        ui.dashboard = false;
        ui.color = false;
        ui.log_format = render::LogFormat::Jsonl;
        ui.show_raw_json = false;
    }
    render::configure(ui.clone());
    let mut interrupt = interrupt_signal()?;
    let mut terminate = terminate_signal()?;
    if options.list_tools {
        let backend = tokio::select! {
            result = timeout(Duration::from_secs(120), Backend::start(&options.shell)) => result.map_err(|_| "local backend startup timed out")??,
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        };
        let backend = match Runtime::with_context_roots(
            backend.clone(),
            std::env::current_dir()?,
            options.context_roots.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                backend.shutdown().await;
                return Err(error.into());
            }
        };
        let tools = tokio::select! {
            result = timeout(Duration::from_secs(120),async { backend.wait_discovery().await; backend.tools().await }) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
            _ = interrupt.recv() => { backend.shutdown().await; return Ok(()); },
            _ = terminate.recv() => { backend.shutdown().await; return Ok(()); },
        };
        backend.shutdown().await;
        println!("{}", json!({"type":"shell","tools":tools?}));
        return Ok(());
    }
    let mut attempt = 0u64;
    loop {
        if let Some(runner) = &service
            && runner.stop_requested()
        {
            runner.record("stopped", attempt, None, None, None)?;
            return Ok(());
        }
        attempt = attempt.saturating_add(1);
        if let Some(runner) = &service {
            runner.record("starting", attempt, None, None, None)?;
        }
        let result = match connect(&options, &ui, service.as_ref(), attempt).await {
            Ok(ConnectionExit::Reload { restart, output }) => {
                // The ACK and broker receipt were sent before connect cleaned
                // up every old task. Stop wins over a reload arriving at exit.
                let stopped = tokio::select! {
                    biased;
                    _ = interrupt.recv() => true,
                    _ = terminate.recv() => true,
                    _ = std::future::ready(()) => service.as_ref().is_some_and(|runner| runner.stop_requested()),
                };
                if stopped {
                    if let Some(runner) = &service {
                        runner.record("stopped", attempt, None, None, None)?;
                    }
                    return Ok(());
                }
                if let Some(runner) = &service {
                    runner.record("reconnecting", attempt, None, None, None)?;
                }
                // Windows waits for its replacement rather than exec'ing.
                // Release the old service lease before the new runner opens it.
                drop(service.take());
                return restart.execute(output);
            }
            other => other,
        };
        if matches!(result, Ok(ConnectionExit::Stopped)) {
            if let Some(runner) = &service {
                runner.record("stopped", attempt, None, None, None)?;
            }
            return Ok(());
        }
        let error = result.as_ref().err().map(ToString::to_string);
        if !options.watchdog {
            if let Some(runner) = &service {
                runner.record("disconnected", attempt, None, None, error.as_deref())?;
            }
            return result.map(|_| ());
        }
        if let Some(runner) = &service {
            if runner.stop_requested() {
                runner.record("stopped", attempt, None, None, None)?;
                return Ok(());
            }
            runner.record(
                "reconnecting",
                attempt,
                Some(options.reconnect_delay_secs),
                None,
                error.as_deref(),
            )?;
        }
        if let Some(error) = &error {
            report_error(error);
        }
        render::status(
            "watchdog",
            "Progress",
            &format!(
                "Disconnected; retrying in {}s (attempt {}). Stop with Ctrl-C or ssmcp sessions stop NAME.",
                options.reconnect_delay_secs,
                attempt.saturating_add(1)
            ),
        );
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(options.reconnect_delay_secs)) => {},
            _ = interrupt.recv() => {
                if let Some(runner) = &service { runner.record("stopped", attempt, None, None, None)?; }
                return Ok(());
            },
            _ = terminate.recv() => {
                if let Some(runner) = &service { runner.record("stopped", attempt, None, None, None)?; }
                return Ok(());
            },
        }
    }
}

enum ConnectionExit {
    Stopped,
    Disconnected,
    Reload {
        restart: reload::Restart,
        output: Arc<crate::output::OutputStore>,
    },
}

async fn connect(
    options: &Options,
    ui: &ui::Ui,
    service: Option<&crate::session::Runner>,
    attempt: u64,
) -> crate::Result<ConnectionExit> {
    let mut interrupt = interrupt_signal()?;
    let mut terminate = terminate_signal()?;
    let restart = reload::Restart::capture()?;
    let metadata = options.metadata()?;
    render::status(
        "ssh",
        "Progress",
        &format!(
            "Connecting to {} with OpenSSH",
            options.destination.as_deref().unwrap_or("configured host")
        ),
    );
    let mut command = options.command();
    if service.is_some() {
        // No terminal exists for a service to answer SSH authentication prompts.
        // Existing keys, agents and host-key policy still decide access.
        // Insert before the destination: OpenSSH parses options in order.
        let arguments = command
            .as_std()
            .get_args()
            .map(OsString::from)
            .collect::<Vec<_>>();
        let mut background = Command::new("ssh");
        background
            .args(["-o", "BatchMode=yes"])
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command = background;
    }
    let mut ssh = command
        .spawn()
        .map_err(|e| format!("could not start OpenSSH ssh: {e}"))?;
    let _stderr_pump = ssh.stderr.take().map(|mut stderr| {
        SshStderr(tokio::spawn(async move {
            let mut bytes = [0u8; 4096];
            while let Ok(n) = stderr.read(&mut bytes).await {
                if n == 0 {
                    break;
                }
                render::event(
                    "ssh",
                    &json!({"stderr":String::from_utf8_lossy(&bytes[..n])}),
                );
            }
        }))
    });
    let mut writer = ssh.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut reader = BufReader::new(ssh.stdout.take().ok_or("SSH stdout unavailable")?);
    write_frame(
        &mut writer,
        &Frame::Hello {
            version: VERSION,
            mode: Mode::Shell,
            metadata: metadata.clone(),
        },
    )
    .await?;
    let ready = tokio::select! {
        result = timeout(Duration::from_secs(120),read_frame(&mut reader)) => result.map_err(|_| "SSH authentication timed out")??,
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
    };
    let (url, login_code, login_expires_in, client_id) = match ready {
        Some(Frame::Ready { url, login_code, login_expires_in, client_id }) => (url, login_code, login_expires_in, client_id),
        Some(Frame::Reject { message }) => return Err(message.into()),
        None => return Err(
            "SSH disconnected before registration; check the SSH key, host key, server setup, and matching ssmcp/ssmcpd versions"
                .into(),
        ),
        _ => return Err("unexpected registration response".into()),
    };
    let github_auth = !metadata.allow.is_empty();
    if github_auth && (!login_code.is_empty() || login_expires_in != 0) {
        let _ = ssh.kill().await;
        return Err("server did not acknowledge the --allow GitHub access policy; upgrade ssmcpd with GitHub authorization configured before connecting".into());
    }
    if !github_auth && (login_code.is_empty() || login_expires_in == 0) {
        let _ = ssh.kill().await;
        return Err("server returned no login code for this connection; use --allow only when GitHub access is intended".into());
    }
    let login_received_at = std::time::Instant::now();
    let display_session = match terminal::Session::start(ui) {
        Ok(session) => session,
        Err(error) => {
            render::status(
                "display",
                "Warning",
                &format!("Dashboard unavailable: {error}"),
            );
            None
        }
    };
    let mut incoming = Incoming::start(reader);
    render::status(
        "shell",
        "Progress",
        "Starting shell and discovering installed harness tools",
    );
    let backend = tokio::select! {
        biased;
        error = incoming.disconnected() => { let _ = ssh.kill().await; return Err(error.into()); },
        status = ssh.wait() => return Err(ssh_ended(status).into()),
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(120), Backend::start(&options.shell)) => result.map_err(|_| "local backend startup timed out")??,
    };
    let backend = match Runtime::with_context_roots(
        backend.clone(),
        PathBuf::from(&metadata.cwd),
        options.context_roots.clone(),
    ) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ssh.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = ssh.wait().await;
            return Err(error.into());
        }
    };
    // Core tools are ready immediately; optional providers continue discovery independently.
    let discovered = tokio::select! {
        biased;
        error = incoming.disconnected() => Err(error),
        status = ssh.wait() => Err(ssh_ended(status)),
        _ = interrupt.recv() => { let _ = ssh.start_kill(); backend.shutdown().await; let _ = ssh.wait().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = ssh.start_kill(); backend.shutdown().await; let _ = ssh.wait().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(120), backend.tools()) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
    };
    if let Err(error) = discovered.and_then(|_| incoming.ensure_open(&mut ssh)) {
        let _ = ssh.start_kill();
        backend.shutdown().await;
        incoming.stop().await;
        let _ = ssh.wait().await;
        return Err(error.into());
    }
    let login_expires_in = login_expires_in.saturating_sub(login_received_at.elapsed().as_secs());
    let mut output = json!({"url":url,"client_id":client_id,"name":metadata.name,"cwd":metadata.cwd,"hostname":metadata.hostname,"type":"shell"});
    if let Some(title) = &metadata.title {
        output["title"] = json!(title);
    }
    if github_auth {
        output["auth"] = json!("github");
        output["allow"] = json!(metadata.allow);
    } else {
        output["login_code"] = json!(login_code);
        output["login_expires_in"] = json!(login_expires_in);
    }
    let published: crate::Result<()> = if let Some(runner) = service {
        runner.record("connected", attempt, None, Some(&output), None)
    } else {
        render::connection(&output).map_err(Into::into)
    };
    if let Err(error) = published {
        let _ = ssh.start_kill();
        backend.shutdown().await;
        incoming.stop().await;
        let _ = ssh.wait().await;
        return Err(error);
    }
    render::status(
        "shell",
        "Connected",
        &format!(
            "CONNECTED  client={}  mode={}  cwd={}  id={}",
            metadata.name, "shell", metadata.cwd, client_id
        ),
    );
    render::status(
        "shell",
        "Result",
        if github_auth {
            "Sign in with an allowed GitHub account. Ctrl-C disconnects this client."
        } else {
            "Enter login_code on the MCP OAuth page. Ctrl-C disconnects and revokes access."
        },
    );
    let local_input = match input::Input::attach(backend.terminal()) {
        Ok(input) => input,
        Err(error) => {
            let _ = ssh.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = ssh.wait().await;
            return Err(error.into());
        }
    };
    let git_status = display_session
        .as_ref()
        .map(|_| git_status::GitStatus::start(PathBuf::from(&metadata.cwd)));
    let mut ui_updates = tokio::time::interval(Duration::from_secs(1));
    ui_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let writer = Arc::new(Mutex::new(writer));
    let mut requests: JoinSet<io::Result<bool>> = JoinSet::new();
    let mut reload_requested = false;
    let mut intentionally_stopped = false;
    let mut service_updates = tokio::time::interval(Duration::from_secs(1));
    service_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: crate::Result<()> = 'connected: loop {
        tokio::select! {
            _ = service_updates.tick(), if service.is_some() => {
                if service.expect("guarded service runner").stop_requested() {
                    intentionally_stopped = true;
                    break Ok(());
                }
            },
            _ = ui_updates.tick(), if git_status.is_some() => {
                if let Some(observer) = &git_status {
                    render::set_git_status(observer.snapshot());
                }
            },
            frame = incoming.frames.recv() => match frame {
                Some(Ok(Some(Frame::Call {id,operation}))) => {
                    while let Some(completed) = requests.try_join_next() {
                        match completed {
                            Ok(Ok(was_tool)) => {
                                if was_tool && let Some(observer) = &git_status { observer.refresh(); }
                            },
                            Ok(Err(error)) => break 'connected Err(error.into()),
                            Err(error) => break 'connected Err(error.into()),
                        }
                    }
                    if reload::is_reload(&operation) {
                        let crate::wire::Operation::CallTool { arguments, .. } = &operation else { unreachable!() };
                        let response = backend.validate_reload().and_then(|()| restart.accept(arguments, requests.len(), &client_id, github_auth));
                        let accepted = response.is_ok();
                        let sent = timeout(Duration::from_secs(10), async {
                            let mut writer = writer.lock().await;
                            write_frame(&mut *writer, &Frame::Reply { id, result: response }).await
                        }).await;
                        match sent {
                            Ok(Ok(())) => {},
                            Ok(Err(error)) => break Err(error.into()),
                            Err(_) => break Err("SSH reload acknowledgement timed out".into()),
                        }
                        if accepted {
                            // Flushing the pipe only hands bytes to local ssh.
                            // Wait for the broker's receipt before terminating
                            // that process, so the ACK cannot be lost in transit.
                            let confirmed = timeout(Duration::from_secs(10), async {
                                loop {
                                    match incoming.frames.recv().await {
                                        Some(Ok(Some(Frame::ReloadCommit { id: acknowledged }))) if acknowledged == id => return Ok::<(), crate::Error>(()),
                                        Some(Ok(Some(Frame::Call { id: next_id, .. }))) => {
                                            let mut writer = writer.lock().await;
                                            write_frame(&mut *writer, &Frame::Reply { id: next_id, result: Err("client reload is already pending; call list after reconnection".into()) }).await?;
                                        }
                                        Some(Err(error)) => return Err(error.into()),
                                        _ => return Err("SSH ended before the server confirmed reload receipt".into()),
                                    }
                                }
                            }).await;
                            match confirmed {
                                Ok(Ok(())) => {},
                                Ok(Err(error)) => break Err(error),
                                Err(_) => {
                                    render::status("ssmcp", "Error", "Server did not confirm reload receipt; client remains running. Update ssmcpd before using reload.");
                                    continue 'connected;
                                }
                            }
                            render::status("ssmcp", "Progress", "Reload accepted; stopping local jobs and restarting the installed client");
                            reload_requested = true;
                            break Ok(());
                        }
                        continue;
                    }
                    if requests.len() >= MAX_INFLIGHT { break Err("server exceeded the in-flight request limit".into()); }
                    let backend = backend.clone();
                    let writer = writer.clone();
                    requests.spawn(async move {
                        let was_tool = matches!(&operation, crate::wire::Operation::CallTool { .. });
                        let mut result = backend.perform(operation).await;
                        // A large native response must not corrupt/disconnect the SSH stream.
                        if serde_json::to_vec(&result).map_or(true, |bytes|bytes.len() + 256 > MAX_FRAME) {
                            result = Err("native call finished but its result exceeds the 4 MiB bridge frame limit; do not blindly repeat a mutating call, inspect native state or request a smaller output page".into());
                        }
                        if let Err(error) = &result { render::status("ssmcp", "Error", error); }
                        timeout(Duration::from_secs(10),async {
                            let mut writer = writer.lock().await;
                            write_frame(&mut *writer,&Frame::Reply {id,result}).await
                        }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut,"SSH write timed out"))??;
                        Ok(was_tool)
                    });
                }
                Some(Ok(None)) | None => break Ok(()),
                Some(Ok(Some(Frame::Reject {message}))) => break Err(message.into()),
                Some(Ok(Some(_))) => break Err("unexpected SSH protocol frame".into()),
                Some(Err(error)) => break Err(error.into()),
            },
            response = requests.join_next(), if !requests.is_empty() => match response {
                Some(Ok(Ok(was_tool))) => {
                    if was_tool && let Some(observer) = &git_status { observer.refresh(); }
                },
                Some(Ok(Err(error))) => break Err(error.into()),
                Some(Err(error)) => break Err(error.into()),
                None => {},
            },
            _ = interrupt.recv() => { intentionally_stopped = true; break Ok(()); },
            _ = terminate.recv() => { intentionally_stopped = true; break Ok(()); },
        }
    };
    if let Some(input) = local_input {
        input.shutdown().await;
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    if let Some(observer) = git_status {
        observer.shutdown().await;
    }
    incoming.stop().await;
    drop(writer);
    let _ = ssh.start_kill();
    backend.shutdown().await;
    let _ = ssh.wait().await;
    drop(display_session);
    render::status("shell", "Result", "DISCONNECTED");
    if reload_requested {
        return Ok(ConnectionExit::Reload {
            restart,
            output: backend.output(),
        });
    }
    result.map(|()| {
        if intentionally_stopped {
            ConnectionExit::Stopped
        } else {
            ConnectionExit::Disconnected
        }
    })
}

/// Print a terminal failure using the selected client log format.
pub fn report_error(error: &str) {
    render::status("ssmcp", "Error", error);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_and_watchdog_options_have_bounded_explicit_lifecycles() {
        let normal = Options::parse(["host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(!normal.daemon && !normal.watchdog);
        let retry = Options::parse(["--watchdog", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(retry.reconnect_delay_secs, 5);
        for delay in ["1", "3600"] {
            let options = Options::parse(
                ["--daemon", "--watchdog", "--reconnect-delay", delay, "host"].map(OsString::from),
            )
            .unwrap()
            .unwrap();
            assert!(options.daemon && options.watchdog);
            assert_eq!(options.reconnect_delay_secs.to_string(), delay);
        }
        for delay in ["0", "3601", "-1", "five"] {
            assert!(
                Options::parse(
                    ["--watchdog", "--reconnect-delay", delay, "host"].map(OsString::from)
                )
                .is_err()
            );
        }
        assert!(Options::parse(["--reconnect-delay", "5", "host"].map(OsString::from)).is_err());
        for flag in ["--daemon", "--watchdog"] {
            assert!(Options::parse([flag, "--list-tools"].map(OsString::from)).is_err());
        }
    }

    #[test]
    fn background_manifest_retains_native_connection_arguments_without_recursing() {
        let options = Options::parse(
            [
                "--daemon",
                "--watchdog",
                "--reconnect-delay",
                "7",
                "--name",
                "workstation",
                "--title",
                "--daemon",
                "--allow",
                "OctoCat@GitHub",
                "-i",
                "key with spaces",
                "-F",
                "config with spaces",
                "-p",
                "2222",
                "--theme",
                "nord",
                "user@host",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let spec = options.session_spec().unwrap();
        assert_eq!(spec.name, "workstation");
        assert!(spec.cwd.is_absolute());
        assert!(spec.watchdog);
        assert_eq!(spec.reconnect_delay_secs, 7);
        let restored = Options::parse(spec.args.into_iter().map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(!restored.daemon);
        assert_eq!(restored.title.as_deref(), Some("--daemon"));
        assert_eq!(restored.allow, ["octocat@github"]);
        assert_eq!(restored.identity, options.identity);
        assert_eq!(restored.config, options.config);
        assert_eq!(restored.port, options.port);
        assert_eq!(restored.destination, options.destination);
        assert_eq!(restored.shell, options.shell);
        assert!(restored.theme.is_none());
    }

    #[test]
    fn log_format_options_only_select_display_and_are_not_forwarded() {
        let pretty = Options::parse(["host"].map(OsString::from))
            .unwrap()
            .unwrap();
        let jsonl = Options::parse(["--log-format", "jsonl", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        let verbose = Options::parse(["--verbose", "--no-color", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(pretty.log_format, None);
        assert_eq!(jsonl.log_format, Some(render::LogFormat::Jsonl));
        assert_eq!(verbose.log_format, Some(render::LogFormat::PrettyVerbose));
        assert!(verbose.no_color);
        assert_eq!(
            pretty.command().as_std().get_args().collect::<Vec<_>>(),
            jsonl.command().as_std().get_args().collect::<Vec<_>>()
        );
        assert!(Options::parse(["--log-format", "xml", "host"].map(OsString::from)).is_err());
    }

    #[test]
    fn harness_selection_flags_are_removed() {
        for args in [
            ["-t", "codex", "host"],
            ["--type", "pi", "host"],
            ["--backend", "/bin/codex", "host"],
        ] {
            assert!(Options::parse(args.map(OsString::from)).is_err());
        }
    }

    #[test]
    fn destination_validation_rejects_shell_and_option_injection() {
        for destination in [
            "my-server",
            "ssmcp.example",
            "127.0.0.1",
            "[::1]",
            "user@host",
            "user@127.0.0.1",
            "user@[::1]",
            "user@ssmcp-test",
        ] {
            assert!(validate_destination(destination).is_ok());
        }
        for destination in [
            "",
            "-oProxyCommand=id",
            "host;id",
            "host name",
            "host\n",
            "ssh://user@host",
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
    fn ssh_alias_is_passed_to_openssh_without_inventing_a_username() {
        let options = Options::parse(["-F", "config", "my-server"].map(OsString::from))
            .unwrap()
            .unwrap();
        let command = options.command();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|v| v.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            &args[args.len() - 6..],
            ["-F", "config", "--", "my-server", "ssmcpd", "attach"]
        );
        assert!(Options::parse(["--list-tools", "my-server"].map(OsString::from)).is_err());
    }

    #[test]
    fn client_names_are_bounded_display_metadata() {
        let options = Options::parse(["--name", "My workstation", "user@host"].map(OsString::from))
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
    fn session_title_preserves_unicode_task_context_without_becoming_an_ssh_argument() {
        let title = "修复会话重启 · issue #42";
        let options = Options::parse(
            ["--name", "workstation", "--title", title, "user@host"].map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let metadata = options.metadata().unwrap();
        assert_eq!(metadata.name, "workstation");
        assert_eq!(metadata.title.as_deref(), Some(title));
        assert!(
            !options
                .command()
                .as_std()
                .get_args()
                .any(|arg| arg == title)
        );

        let equals = Options::parse(["--title=修复客户端", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(equals.title.as_deref(), Some("修复客户端"));
        let absent = Options::parse(["host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(absent.title.is_none());
        assert!(
            serde_json::to_value(absent.metadata().unwrap())
                .unwrap()
                .get("title")
                .is_none()
        );
    }

    #[test]
    fn session_title_rejects_missing_blank_controls_and_utf8_byte_overflow() {
        assert!(Options::parse([OsString::from("--title")]).is_err());
        for title in [
            "".to_owned(),
            "  ".to_owned(),
            "bad\ntitle".to_owned(),
            "bad\u{1b}[31mtitle".to_owned(),
            "a".repeat(crate::wire::MAX_TITLE_BYTES + 1),
            "界".repeat(crate::wire::MAX_TITLE_BYTES / 3 + 1),
        ] {
            assert!(
                Options::parse([
                    OsString::from("--title"),
                    OsString::from(title),
                    OsString::from("host"),
                ])
                .is_err()
            );
        }
        let maximum = "a".repeat(crate::wire::MAX_TITLE_BYTES);
        assert!(Options::parse(["--title", &maximum, "host"].map(OsString::from)).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn session_title_rejects_non_utf8_arguments() {
        use std::os::unix::ffi::OsStringExt;
        assert!(
            Options::parse([
                OsString::from("--title"),
                OsString::from_vec(vec![0xff]),
                OsString::from("host"),
            ])
            .is_err()
        );
    }

    #[test]
    fn github_allow_flags_combine_canonicalize_and_deduplicate() {
        let options = Options::parse(
            [
                "--allow",
                "OctoCat@GitHub, Alice-Dev@github",
                "--allow",
                "OCTOCAT@github,bob@github",
                "my-server",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            options.allow,
            ["octocat@github", "alice-dev@github", "bob@github"]
        );
        assert_eq!(options.metadata().unwrap().allow, options.allow);
        assert_eq!(options.destination.as_deref(), Some("my-server"));
        assert!(
            !options
                .command()
                .as_std()
                .get_args()
                .any(|arg| arg.to_string_lossy().contains("@github"))
        );
    }

    #[test]
    fn github_allow_limits_distinct_users_and_rejects_invalid_or_local_options() {
        let all = (0..MAX_ALLOWED_PRINCIPALS)
            .map(|i| format!("user-{i}@github"))
            .collect::<Vec<_>>()
            .join(",");
        let options =
            Options::parse(["--allow", &all, "--allow", &all, "my-server"].map(OsString::from))
                .unwrap()
                .unwrap();
        assert_eq!(options.allow.len(), MAX_ALLOWED_PRINCIPALS);
        let error = Options::parse(
            ["--allow", &all, "--allow", "another@github", "my-server"].map(OsString::from),
        )
        .err()
        .unwrap();
        assert!(error.contains("32 distinct"), "{error}");
        for value in [
            "",
            "alice",
            "alice@example.com",
            "alice@github,",
            ",alice@github",
        ] {
            let error = Options::parse(["--allow", value, "my-server"].map(OsString::from))
                .err()
                .unwrap();
            assert!(error.contains("--allow"), "{error}");
        }
        assert!(
            Options::parse(["--allow"].map(OsString::from))
                .err()
                .unwrap()
                .contains("requires username@github")
        );
        assert!(
            Options::parse(["--list-tools", "--allow", "alice@github"].map(OsString::from))
                .err()
                .unwrap()
                .contains("cannot be used with --list-tools")
        );
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
    #[test]
    fn context_roots_are_local_configuration_and_never_ssh_or_backend_arguments() {
        let root = std::env::temp_dir().canonicalize().unwrap();
        let options = Options::parse([
            OsString::from("--context-root"),
            root.clone().into_os_string(),
            OsString::from("--context-root"),
            root.clone().into_os_string(),
            OsString::from("my-server"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(options.context_roots, vec![root.clone(), root.clone()]);
        assert!(
            !options
                .command()
                .as_std()
                .get_args()
                .any(|arg| arg == root.as_os_str())
        );
        let local = Options::parse([
            OsString::from("--context-root"),
            root.into_os_string(),
            OsString::from("--list-tools"),
        ])
        .unwrap()
        .unwrap();
        assert!(local.list_tools);
        assert_eq!(local.context_roots.len(), 1);
        assert!(
            Options::parse(
                [
                    "-t",
                    "codex",
                    "my-server",
                    "--",
                    "--context-root",
                    "native-option"
                ]
                .map(OsString::from)
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_or_excessive_context_roots_fail_during_option_parsing() {
        for args in [
            vec![OsString::from("--context-root")],
            vec![
                OsString::from("--context-root"),
                OsString::from(""),
                OsString::from("my-server"),
            ],
            vec![
                OsString::from("--context-root"),
                std::env::current_exe().unwrap().into_os_string(),
                OsString::from("my-server"),
            ],
        ] {
            let error = Options::parse(args).err().unwrap();
            assert!(error.contains("--context-root"), "{error}");
        }
        let mut args = Vec::new();
        for _ in 0..9 {
            args.push(OsString::from("--context-root"));
            args.push(std::env::temp_dir().into_os_string());
        }
        args.push(OsString::from("my-server"));
        let error = Options::parse(args).err().unwrap();
        assert!(error.contains("at most 8"), "{error}");
    }
}
