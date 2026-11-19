//! Foreground account or SSH client. Connection details are emitted once as JSON.
mod access;
pub(crate) mod account;
pub(crate) mod attachments;
mod audit_view;
mod browser_setup;
mod clipboard;
pub(crate) mod completion;
mod composer;
mod config;
pub(crate) mod context_memory;

pub(crate) use config::BrowserSettings;

/// Settings are read on demand so setup changes apply on the next client connection.
pub(crate) fn browser_settings() -> crate::Result<BrowserSettings> {
    config::browser()
}
mod git_status;
mod handoff;
mod highlight;
pub(crate) mod input;
pub(crate) mod interface;
mod panel;
pub(crate) mod presentation;
#[cfg(test)]
mod protocol_tests;
pub(crate) mod references;
mod relay;
pub(crate) mod reload;
pub(crate) mod render;
mod steering;
pub(crate) mod terminal;
mod trust;
pub(crate) mod ui;
mod ui_command;
mod update;
#[cfg(target_os = "linux")]
mod zero;

use crate::{
    backend::Backend,
    platform::{interrupt_signal, terminate_signal},
    runtime::Runtime,
    wire::{
        ClientMetadata, Frame, MAX_ALLOWED_PRINCIPALS, MAX_FRAME, MAX_INFLIGHT, Mode, VERSION,
        canonical_allow_principal, read_frame, validate_title, write_frame,
    },
};
use serde_json::json;
use std::{ffi::OsString, io, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, ChildStdout, Command},
    sync::{Mutex, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::timeout,
};

const HELP: &str = r#"ssmcp — connect a device; control its tools remotely

Usage: ssmcp [username@server] [options]

Start
  ssmcp                         Reuse your last authorized account
  ssmcp alice@relay.example     Connect and sign in when needed
  ssmcp list                    List your online devices
  ssmcp -i                      Open an authorized remote terminal

Manage
  config · whoami · logout      Account and server settings
  update [--check]              Update the installed client
  sessions · attach · service  Saved and background sessions
  setup · ui · 007             Setup, appearance and recovery

Options
  --name NAME   --title TEXT   --server URL   --context-root PATH
  --no-browser  --no-watchdog  --theme NAME   --no-color
  --allow USER@SERVER    Allow a registered account on this relay; repeat or comma-separate

Use ssmcp COMMAND --help for command options; --help-full lists all connection flags.
In a connected chat: /help, @file, $skill, !command; Left opens Agents.
"#;

const HELP_FULL: &str = r#"ssmcp — connect your devices and control authorized sessions

Usage: ssmcp username@server [options]
       ssmcp [--server URL] [options]
       ssmcp whoami|logout [--server URL] [--json]
       ssmcp list [--json | -i] [--client ID]
       ssmcp -i [--server URL] [--client ID]
       ssmcp config <set|get|unset> server [URL]
       ssmcp config path
       ssmcp update [--check]
       ssmcp trust <add|list|remove|path> [--server URL] [--public-key FILE|--pin PIN]
       ssmcp [options] [--server URL]
       ssmcp setup [--server URL] [--no-browser]
       ssmcp setup browser [--check | --install] [--playwright-mcp PATH] [--json]
       ssmcp sessions <list|status|start|stop|close|restart|logs|auth|remove> [NAME]
       ssmcp sessions create NAME [options] [user@]host
       ssmcp attach [NAME|list] [--read-only]
       ssmcp service <install|uninstall|start|stop|restart|status|run> [options]
       ssmcp ui <show|check|init|preview> [--ui-config FILE]
       ssmcp 007 <install|run|start|stop|status|uninstall|sessions> [options]

  --server URL         Relay for this command; overrides config and last login
  --username NAME      Named account on the selected server
  --no-browser         Print the GitHub authorization link without opening it
  -i, --interactive    Select an authorized account session and open its terminal
  --name NAME          Display name (default: hostname-pid)
  --daemon             Start a quiet, user-owned native background service
  --session-control    Grant this connection local saved-session management tools
  --no-watchdog        Disable automatic reconnects (enabled by default)
  --watchdog           Enable automatic reconnects explicitly (compatibility flag)
  --reconnect-delay N  Seconds between retries, 1..=3600 (default 5)
  --title TITLE        Session title in the header and agent list (max 512 UTF-8 bytes)
  --allow USER@SERVER  Allow only these registered relay accounts to control this device
                       Repeat or comma-separate; at most 32 accounts
                       No device login required; controllers must sign in
  --tty TERMINAL       Observe commands in reusable terminal windows (kitty, ztty, ...)
  --shell /path        Shell executable (default: platform shell)
  --context-root PATH  Add a read-only skill/instruction root (repeat up to 8 times)
  --log-format FORMAT  pretty (default), pretty-verbose, or jsonl
  --no-color           Disable ANSI colors (also honors NO_COLOR)
  --verbose            Alias for --log-format pretty-verbose
  --raw, --raw-events   Alias for --log-format jsonl
  --ui-config FILE     Local [ui] TOML; default ~/.config/ssmcp/config.toml
  --theme NAME         dark, codex-dark, nord, solarized, light, or minimal
  --timestamps         Show UTC timestamps on event headings
  --max-inline-output N  Maximum lines per buffered output block (default 8)
  --list-tools         List native and workspace tools as JSON, then exit

The client provides native tools without another harness. Optional Codex, Claude
Code, Pi, OpenCode and Playwright bridges stay unloaded until explicitly needed.
Use ssmcp.harnesses.load for one provider, or the local /cc, /codex and /pi commands.
Tools keep their native schemas and permission/approval behavior.

Examples:
  ssmcp alice@ssmcp.lmm.best
  ssmcp alice@my-server.example:8443 --name laptop
  ssmcp config set server https://relay.example.com
  ssmcp whoami
  ssmcp --name laptop
  ssmcp list
  ssmcp -i
  ssmcp --no-watchdog --name one-shot
  ssmcp --allow alice@ssmcp.lmm.best
  ssmcp --server https://relay.example --allow alice@relay.example,bob@relay.example
  ssmcp sessions status workstation
  ssmcp sessions stop workstation
  ssmcp service install --system --user alice
  sudo systemctl enable --now ssmcp
  ssmcp sessions create workstation --watchdog alice@my-server.example
  ssmcp sessions create shared --allow alice@ssmcp.lmm.best
  ssmcp attach workstation
  ssmcp sessions close workstation

On the controlled device, use --allow username@relay-server. No login or invitation
is needed there. The account must already exist on that relay; a different server,
an unknown or banned account, bare names and the former @github syntax are rejected.
Only the listed accounts receive access. Controllers must sign in through OAuth.
Names resolve to stable account IDs; billing follows the account making each call.

Without --allow, a saved valid login selects only your own registered username.
username@server signs in and registers that username when needed. whoami displays
your account, and logout removes the selected server's saved login. Other servers
and independent logins are preserved. --no-browser prints the authorization URL.
An update or a controlled-device restart does not grant a new controller identity.

ssmcp list shows remotely controllable sessions; sessions list shows local saved
sessions. Ctrl-] leaves remote interactive control; Ctrl-C is sent to its terminal.
Redirected stdout (or jsonl mode) emits one connection JSON object. Add its URL to
an MCP client and sign in as an allowed account. --allow cannot use --list-tools.
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
Install the persistent session manager with `ssmcp service install`; choose
--system with an explicit non-root owner for boot startup, or a per-user service.
New sessions use the running manager. Legacy --daemon also supports per-session
systemd --user, launchd and Task Scheduler when the manager is not installed.
Bare `ssmcp attach` lists sessions. Ctrl-] detaches without stopping the session;
the dashboard provides audit navigation. Use `ssmcp -i` for interactive input,
or disable the dashboard when attaching to a saved session.
--session-control explicitly grants this OS user's saved-session management.
New remote-created sessions inherit connection settings and stay in the current
workspace; children do not inherit this management grant.
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
                        if message == relay::AUTH_EXPIRED {
                            Some(message.clone())
                        } else {
                            Some(format!("SSH server rejected the connection: {message}"))
                        }
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

    // A relay can flush its final Reject and exit before this task consumes
    // stdout. Preserve that authorization outcome instead of treating the child
    // exit as a transient network failure and restarting the watchdog.
    async fn after_relay_exit(&mut self, fallback: String) -> String {
        timeout(Duration::from_secs(2), self.disconnected())
            .await
            .unwrap_or(fallback)
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
    server: Option<String>,
    username: Option<String>,
    no_browser: bool,
    relay: Option<account::Credential>,
    temporary: bool,
    name: Option<String>,
    title: Option<String>,
    allow: Vec<String>,
    context_roots: Vec<PathBuf>,
    list_tools: bool,
    daemon: bool,
    session_control: bool,
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
    tty: Option<String>,
}

impl Options {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        Self::parse_with_default(args, None)
    }

    fn parse_with_default(
        args: impl IntoIterator<Item = OsString>,
        default_destination: Option<&str>,
    ) -> Result<Option<Self>, String> {
        Self::parse_with_policy(
            args,
            default_destination,
            true,
            default_destination.is_some(),
        )
    }

    // Older saved manifests omitted --no-watchdog. Their recorded policy remains
    // authoritative as the parsing default; explicit flags must still match it.
    fn parse_saved(spec: &crate::session::SessionSpec) -> Result<Option<Self>, String> {
        Self::parse_with_policy(
            spec.args.iter().map(OsString::from),
            None,
            spec.watchdog,
            true,
        )
    }

    fn parse_with_policy(
        args: impl IntoIterator<Item = OsString>,
        default_destination: Option<&str>,
        default_watchdog: bool,
        legacy_destination: bool,
    ) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let mut destination = None;
        let mut server = None;
        let mut username = None;
        let mut no_browser = false;
        let mut ssh = false;
        let mut name = None;
        let mut title = None;
        let mut allow = Vec::new();
        let mut context_roots = Vec::new();
        let mut list_tools = false;
        let mut daemon = false;
        let mut session_control = false;
        let mut watchdog = default_watchdog;
        let mut explicit_watchdog = None;
        let mut reconnect_delay_secs = None;
        let mut log_format = None;
        let mut ui_config = None;
        let mut theme = None;
        let mut timestamps = false;
        let mut max_inline_output = None;
        let mut no_color = false;
        let (mut identity, mut port, mut config) = (None, None, None);
        let mut shell = crate::platform::default_shell();
        let mut tty = None;
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("-h" | "--help") => {
                    print!("{HELP}");
                    return Ok(None);
                }
                Some("--help-full") => {
                    print!("{HELP_FULL}");
                    return Ok(None);
                }
                Some("-V" | "--version") => {
                    println!("ssmcp {}", env!("CARGO_PKG_VERSION"));
                    return Ok(None);
                }
                Some("-t" | "--type" | "--backend") => {
                    return Err("harness selection was removed; remove -t/--type/--backend and connect directly: ssmcp [options] user@server; optional harnesses load only when explicitly requested".into());
                }
                Some(value)
                    if value.starts_with("--type=")
                        || value.starts_with("--backend=")
                        || value.starts_with("-t=") =>
                {
                    return Err("harness selection was removed; remove -t/--type/--backend; optional harnesses load only when explicitly requested".into());
                }
                Some("--ssh") => ssh = true,
                Some("--no-browser") => no_browser = true,
                Some("--username") => {
                    let value = args.next().ok_or("--username requires a name")?;
                    username = Some(crate::username::normalize(
                        value.to_str().ok_or("--username must be UTF-8")?,
                    )?);
                }
                Some("--server") => {
                    let value = args
                        .next()
                        .ok_or("--server requires an HTTPS origin")?
                        .into_string()
                        .map_err(|_| "--server must be UTF-8")?;
                    server = Some(account::normalize_server(&value).map_err(|e| e.to_string())?);
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
                    let value = args.next().ok_or("--allow requires USER@SERVER")?;
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
                Some("--no-setup") => {}
                Some("--portal") => return Err("--portal has been replaced by --server URL".into()),
                Some("--daemon") => daemon = true,
                Some("--session-control") => session_control = true,
                Some("--watchdog" | "--no-watchdog") => {
                    let enabled = arg == "--watchdog";
                    if explicit_watchdog.is_some_and(|previous| previous != enabled) {
                        return Err("--watchdog and --no-watchdog cannot be combined".into());
                    }
                    explicit_watchdog = Some(enabled);
                    watchdog = enabled;
                }
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
                Some("-i" | "--identity") => {
                    identity = Some(args.next().ok_or("--identity requires a file")?)
                }
                Some("-F") => config = Some(args.next().ok_or("-F requires a file")?),
                Some("-p") => {
                    port = Some(
                        args.next()
                            .and_then(|p| p.to_str().and_then(|s| s.parse::<u16>().ok()))
                            .filter(|p| *p > 0)
                            .ok_or("-p requires a port in 1..=65535")?,
                    );
                }
                Some("--tty") => {
                    let value = args
                        .next()
                        .ok_or("--tty requires a terminal executable")?
                        .into_string()
                        .map_err(|_| "--tty must be UTF-8")?;
                    crate::observer::validate_terminal(&value)?;
                    tty = Some(value);
                }
                Some(value) if value.starts_with("--tty=") => {
                    let value = &value["--tty=".len()..];
                    crate::observer::validate_terminal(value)?;
                    tty = Some(value.into());
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
        if destination.is_none() && server.is_none() && !list_tools {
            destination = default_destination.map(str::to_owned);
        }
        // Explicit SSH switches and old saved manifests retain their established
        // transport. A new username@host invocation always selects HTTPS.
        ssh |= identity.is_some() || port.is_some() || config.is_some();
        if !ssh
            && !legacy_destination
            && let Some(target) = destination.as_deref().filter(|value| value.contains('@'))
        {
            let (account_name, origin) =
                account::parse_target(target).map_err(|e| e.to_string())?;
            if username
                .as_ref()
                .is_some_and(|value| value != &account_name)
            {
                return Err("--username differs from the username in the destination".into());
            }
            if server.as_ref().is_some_and(|value| value != &origin) {
                return Err("--server differs from the server in the destination".into());
            }
            username = Some(account_name);
            server = Some(origin);
            destination = None;
        }
        if username.is_some() && (destination.is_some() || ssh) {
            return Err("a username selects HTTPS and cannot be combined with SSH options".into());
        }
        if ssh && destination.is_none() {
            return Err("--ssh and SSH options require an explicit SSH destination".into());
        }
        if let Some(destination) = &destination {
            validate_destination(destination)?;
        }
        if server.is_some()
            && (destination.is_some() || identity.is_some() || port.is_some() || config.is_some())
        {
            return Err(
                "--server uses HTTPS and cannot be combined with an SSH destination or SSH options"
                    .into(),
            );
        }
        if list_tools && (server.is_some() || username.is_some()) {
            return Err(
                "--list-tools is local and cannot be combined with an account destination".into(),
            );
        }
        if list_tools && destination.is_some() {
            return Err("--list-tools is local and does not take an SSH destination".into());
        }
        if list_tools
            && (daemon || explicit_watchdog == Some(true) || reconnect_delay_secs.is_some())
        {
            return Err(
                "--list-tools cannot be combined with --daemon, --watchdog or --reconnect-delay"
                    .into(),
            );
        }
        if list_tools {
            watchdog = false;
        }
        if reconnect_delay_secs.is_some() && !watchdog {
            return Err("--reconnect-delay cannot be combined with --no-watchdog".into());
        }
        if list_tools && !allow.is_empty() {
            return Err(
                "--allow requires a relay connection and cannot be used with --list-tools".into(),
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
            server,
            username,
            no_browser,
            relay: None,
            temporary: false,
            name,
            title,
            allow,
            context_roots,
            list_tools,
            daemon,
            session_control,
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
            tty,
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
                args.extend([
                    OsString::from(flag),
                    std::path::absolute(value)?.into_os_string(),
                ]);
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
        if let Some(tty) = &self.tty {
            args.extend([OsString::from("--tty"), OsString::from(tty)]);
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
        } else {
            args.push(OsString::from("--no-watchdog"));
        }
        if self.session_control {
            args.push(OsString::from("--session-control"));
        }
        if let Some(server) = &self.server {
            args.extend([OsString::from("--server"), OsString::from(server)]);
            if let Some(username) = &self.username {
                args.extend([OsString::from("--username"), OsString::from(username)]);
            }
        } else {
            // Explicitly serialize the legacy transport; @ no longer chooses SSH.
            args.push(OsString::from("--ssh"));
            args.push(OsString::from(
                self.destination
                    .as_ref()
                    .ok_or("missing connection destination")?,
            ));
        }
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

    #[cfg(test)]
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
            "ConnectTimeout=8",
            "ConnectionAttempts=1",
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
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// Validate a persistent worker before the supervisor accepts its definition.
pub(crate) fn validate_session_spec(spec: &crate::session::SessionSpec) -> crate::Result<()> {
    let mut args = spec.args.iter();
    while let Some(arg) = args.next() {
        if matches!(
            arg.as_str(),
            "--help" | "--help-full" | "-h" | "--version" | "-V"
        ) {
            return Err("saved sessions cannot contain help or version actions".into());
        }
        if connection_option_takes_value(arg) {
            args.next();
        }
    }
    let options =
        Options::parse_saved(spec)?.ok_or("saved session must contain connection options")?;
    if options.daemon
        || options.list_tools
        || (options.destination.is_none() && options.server.is_none())
        || options.name.as_deref() != Some(spec.name.as_str())
        || options.watchdog != spec.watchdog
        || options.reconnect_delay_secs != spec.reconnect_delay_secs
        || !spec.cwd.is_absolute()
        || !spec.cwd.is_dir()
    {
        return Err(
            "saved session arguments do not match its name, working directory or reconnect policy"
                .into(),
        );
    }
    Ok(())
}

/// Read display metadata without reopening optional context roots or transports.
pub(crate) fn session_title(spec: &crate::session::SessionSpec) -> crate::Result<Option<String>> {
    let mut args = spec.args.iter();
    let mut title = None;
    while let Some(arg) = args.next() {
        if arg == "--title" {
            title = Some(args.next().ok_or("saved session title is missing")?.clone());
        } else if let Some(value) = arg.strip_prefix("--title=") {
            title = Some(value.to_owned());
        } else if connection_option_takes_value(arg) {
            args.next();
        }
    }
    validate_title(title.as_deref())?;
    Ok(title)
}

/// Child sessions inherit connection identity, but not host-management authority.
pub(crate) fn child_session_spec(
    template: &crate::session::SessionSpec,
    name: String,
    cwd: PathBuf,
    title: Option<String>,
    watchdog: Option<bool>,
    reconnect_delay_secs: Option<u64>,
) -> crate::Result<crate::session::SessionSpec> {
    let mut options =
        Options::parse_saved(template)?.ok_or("invalid management connection template")?;
    options.name = Some(name);
    options.title = title.or(options.title);
    options.daemon = false;
    options.session_control = false;
    options.watchdog = watchdog.unwrap_or(template.watchdog);
    options.reconnect_delay_secs = reconnect_delay_secs.unwrap_or(template.reconnect_delay_secs);
    if reconnect_delay_secs.is_some() && !options.watchdog {
        return Err("reconnect_delay_secs requires watchdog=true".into());
    }
    if !options.watchdog {
        options.reconnect_delay_secs = 5;
    }
    if !(1..=3600).contains(&options.reconnect_delay_secs) {
        return Err("reconnect delay must be 1–3600 seconds".into());
    }
    let mut spec = options.session_spec()?;
    spec.cwd = cwd;
    crate::wire::validate_title(options.title.as_deref())?;
    validate_session_spec(&spec)?;
    Ok(spec)
}

fn session_control(
    options: &Options,
    current_session: Option<&crate::session::Runner>,
) -> crate::Result<Option<crate::session::control::Control>> {
    if !options.session_control {
        return Ok(None);
    }
    let template = if options.destination.is_some() || options.server.is_some() {
        Some(options.session_spec()?)
    } else {
        None
    };
    Ok(Some(crate::session::control::Control::new(
        std::env::current_dir()?,
        template,
        current_session.map(|runner| runner.spec().name.clone()),
    )?))
}

async fn create_saved_session(args: Vec<OsString>) -> crate::Result<()> {
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!(
            "Usage: ssmcp sessions create NAME [--cwd DIR] [--json] [connection options] USERNAME@SERVER\nRequires a running ssmcp service.\n\n  --allow USER@SERVER  Allow a registered account on the selected relay\n  No login is needed on this controlled device; controllers must sign in.\n  Repeat --allow or comma-separate targets. Without it, a saved login selects yourself."
        );
        return Ok(());
    }
    let mut args = args.into_iter();
    let name = args
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or(
            "usage: ssmcp sessions create NAME [--cwd DIR] [--json] [connection options] HOST",
        )?;
    let mut cwd = std::env::current_dir()?;
    let mut json_output = false;
    let mut connection = vec![OsString::from("--name"), OsString::from(&name)];
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--cwd") => cwd = PathBuf::from(args.next().ok_or("--cwd requires a directory")?).canonicalize()?,
            Some("--json") => json_output = true,
            Some("--name" | "--daemon" | "--list-tools") => return Err("sessions create takes its name before the connection options and starts a managed background session".into()),
            Some(flag) if connection_option_takes_value(flag) => {
                let value = args.next().ok_or_else(|| format!("{flag} requires a value"))?;
                connection.extend([arg, value]);
            }
            _ => connection.push(arg),
        }
    }
    let Some(mut options) = Options::parse(connection)? else {
        return Ok(());
    };
    if options.destination.is_some() {
        return Err("Use ssmcp sessions create NAME username@server for an OAuth session".into());
    }
    apply_account(&mut options, true).await?;
    let mut spec = options.session_spec()?;
    spec.cwd = cwd;
    validate_session_spec(&spec)?;
    let result = crate::session::service::request(json!({"op":"create","spec":spec})).await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Created session {name}. Attach: ssmcp attach {name}");
        println!(
            "Process: {} · Connection: {}",
            result["manager_state"].as_str().unwrap_or("starting"),
            result["connection_state"].as_str().unwrap_or("unknown")
        );
    }
    Ok(())
}

fn connection_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "--name"
            | "--server"
            | "--username"
            | "--identity"
            | "--title"
            | "--allow"
            | "--context-root"
            | "--portal"
            | "--reconnect-delay"
            | "--log-format"
            | "--ui-config"
            | "--theme"
            | "--max-inline-output"
            | "-i"
            | "-p"
            | "-F"
            | "--shell"
            | "--tty"
    )
}

fn add_allowed_principals(allow: &mut Vec<String>, value: &str) -> Result<(), String> {
    for entry in value.split(',') {
        let principal =
            canonical_allow_principal(entry.trim()).map_err(|error| format!("--allow: {error}"))?;
        if !allow.contains(&principal) {
            if allow.len() == MAX_ALLOWED_PRINCIPALS {
                return Err(format!(
                    "--allow accepts at most {MAX_ALLOWED_PRINCIPALS} distinct allow targets"
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

async fn apply_account(options: &mut Options, allow_login: bool) -> crate::Result<()> {
    if options.list_tools {
        return Ok(());
    }
    let server = account::resolve_server(options.server.as_deref())?;
    for principal in &options.allow {
        crate::server_endpoint::allow_handle_for_server(principal, &server)?;
    }
    if options.allow.is_empty() {
        let saved = account::credentials(Some(&server)).await?;
        if saved.is_none() && options.username.is_none() {
            return Err("no saved sign-in; use --allow username@relay-server for an unsigned controlled device, or sign in to allow yourself".into());
        }
        let credential = account::connect_credential(
            Some(&server),
            options.username.as_deref(),
            options.no_browser,
            allow_login && options.username.is_some(),
        )
        .await?;
        let handle = credential
            .username
            .as_deref()
            .ok_or("register a server username before connecting without --allow")?;
        options
            .allow
            .push(crate::server_endpoint::allow_principal(handle, &server)?);
        options.username = credential.username.clone();
        options.relay = Some(credential);
    } else {
        // An explicit local policy needs no account token on the controlled device.
        options.relay = None;
    }
    options.server = Some(server);
    Ok(())
}

pub async fn run() -> crate::Result<()> {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__tty-view")) {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        if args.len() != 2 {
            return Err("terminal viewer requires endpoint directory and slot".into());
        }
        let slot = args[1]
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or("invalid observer slot")?;
        return crate::observer::viewer(std::path::Path::new(&args[0]), slot).await;
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__relay")) {
        let code = match relay::run().await {
            Ok(()) => 0,
            Err(error) => {
                report_error(&error.to_string());
                1
            }
        };
        // This transport child owns no native tools. Exit also ends Tokio's
        // blocking stdin reader when the remote WebSocket closes first.
        std::process::exit(code);
    }
    #[cfg(target_os = "linux")]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__rsync-transport")) {
        return crate::rsync::transport_main().await;
    }
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "update") {
        return update::run(args.into_iter().skip(1).collect()).await;
    }
    if args.first().is_some_and(|arg| arg == "ui") {
        return ui_command::run(args.into_iter().skip(1).collect()).await;
    }
    if args.first().is_some_and(|arg| arg == "007") {
        #[cfg(target_os = "linux")]
        return zero::dispatch(args.into_iter().skip(1).collect()).await;
        #[cfg(not(target_os = "linux"))]
        return Err("007 recovery mode currently requires Linux and systemd".into());
    }
    config::normalize_args(&mut args)?;
    if browser_setup::dispatch(&args).await? || config::dispatch(&args)? || trust::dispatch(&args)?
    {
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "setup") {
        args[0] = OsString::from("login");
    }
    if account::dispatch(&args).await? {
        return Ok(());
    }
    if args
        .first()
        .is_some_and(|arg| arg == "invite" || arg == "join")
    {
        return Err("invite and join were removed; start the controlled device with ssmcp --allow username@relay-server".into());
    }
    if args.first().is_some_and(|arg| arg == "service") {
        return crate::session::install::manage(args.into_iter().skip(1).collect()).await;
    }
    if args.first().is_some_and(|arg| arg == "attach") {
        return crate::session::attach::manage(args.into_iter().skip(1).collect()).await;
    }
    if args.first().is_some_and(|arg| arg == "sessions")
        && args.get(1).is_some_and(|arg| arg == "create")
    {
        return create_saved_session(args.into_iter().skip(2).collect()).await;
    }
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
    let parsed = if let Some(runner) = &service {
        Options::parse_saved(runner.spec())?
    } else {
        Options::parse(args)?
    };
    let Some(mut options) = parsed else {
        return Ok(());
    };
    if options.destination.is_some() {
        return Err("SSH device connections have been removed. Use ssmcp username@server or ssmcp login --server https://server".into());
    }
    apply_account(&mut options, service.is_none()).await?;
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
    interface::initialize_path(options.ui_config.as_deref());
    render::configure(ui.clone());
    if let Err(error) = context_memory::initialize() {
        render::status(
            "context",
            "Warning",
            &format!(
                "Local memory unavailable; conversation history and drafts remain usable: {error}"
            ),
        );
    }
    steering::restore_handoff()?;
    let mut attachment = if let Some(runner) = &service {
        Some(crate::session::attach::serve(runner.directory().to_path_buf(), None).await?)
    } else {
        None
    };
    let mut interrupt = interrupt_signal()?;
    let mut terminate = terminate_signal()?;
    if options.list_tools {
        let management = session_control(&options, service.as_ref())?;
        let backend = tokio::select! {
            result = timeout(Duration::from_secs(120), Backend::start_with_tty(&options.shell, options.tty.as_deref())) => result.map_err(|_| "local backend startup timed out")??,
            _ = interrupt.recv() => return Ok(()),
            _ = terminate.recv() => return Ok(()),
        };
        let backend = match Runtime::with_session_control(
            backend.clone(),
            std::env::current_dir()?,
            options.context_roots.clone(),
            management,
        )
        .and_then(|runtime| {
            runtime.enable_default_skill_sources()?;
            Ok(runtime)
        }) {
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
    let mut reauthenticated = false;
    loop {
        if let Some(runner) = &service
            && runner.stop_requested()
        {
            runner.record("stopped", attempt, None, None, None)?;
            return Ok(());
        }
        if options.relay.as_ref().is_some_and(|credential| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                >= credential.expires_at
        }) {
            if service.is_none()
                && !options.temporary
                && options.username.is_some()
                && !reauthenticated
            {
                reauthenticated = true;
                options.allow.clear();
                apply_account(&mut options, true).await?;
            } else {
                if let Some(runner) = &service {
                    runner.record(
                        "disconnected",
                        attempt,
                        None,
                        None,
                        Some(relay::AUTH_EXPIRED),
                    )?;
                }
                return Err(relay::failure(relay::AUTH_EXPIRED.into()));
            }
        }
        attempt = attempt.saturating_add(1);
        if let Some(runner) = &service {
            runner.record("starting", attempt, None, None, None)?;
        }
        let mut registered = false;
        let result = match connect(
            &options,
            &ui,
            service.as_ref(),
            attachment.as_ref(),
            attempt,
            &mut registered,
        )
        .await
        {
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
                if let Some(handle) = attachment.take() {
                    handle.shutdown().await;
                }
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
        let unsupported_protocol = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<UnsupportedWireProtocol>().is_some());
        let permanent = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<relay::PermanentFailure>().is_some());
        if permanent
            && error.as_deref() == Some(relay::AUTH_EXPIRED)
            && service.is_none()
            && !options.temporary
            && options.username.is_some()
            && !reauthenticated
        {
            reauthenticated = true;
            options.allow.clear();
            apply_account(&mut options, true).await?;
            continue;
        }
        if !options.watchdog || unsupported_protocol || permanent {
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

const UNSUPPORTED_WIRE_PROTOCOL: &str = "unsupported ssmcp wire protocol";

#[derive(Debug)]
struct UnsupportedWireProtocol;

impl std::fmt::Display for UnsupportedWireProtocol {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{UNSUPPORTED_WIRE_PROTOCOL}: gateway rejected wire protocol {VERSION}; update ssmcp and ssmcpd; older protocol versions are not supported"
        )
    }
}

impl std::error::Error for UnsupportedWireProtocol {}

struct RegisteredSsh {
    ssh: Child,
    stderr_pump: Option<SshStderr>,
    writer: tokio::process::ChildStdin,
    reader: BufReader<ChildStdout>,
    ready: (String, String, u64, String),
    access_supported: bool,
}

impl RegisteredSsh {
    async fn require_allow_access(&mut self, allow: &[String]) -> crate::Result<()> {
        if !self.access_supported && !allow.is_empty() {
            stop_registration(&mut self.ssh, &mut self.stderr_pump).await?;
            return Err(Box::new(relay::PermanentFailure(
                "server did not acknowledge account access controls; update ssmcpd before using --allow".into()
            )));
        }
        Ok(())
    }
}

// A replacement transport must not start until the rejected SSH process and its
// reader task are gone. No local backend or input binding exists at this stage.
async fn stop_registration(
    ssh: &mut Child,
    stderr_pump: &mut Option<SshStderr>,
) -> crate::Result<()> {
    let _ = ssh.start_kill();
    let reaped = timeout(Duration::from_secs(10), ssh.wait()).await;
    if let Some(pump) = stderr_pump {
        pump.0.abort();
        let _ = (&mut pump.0).await;
    }
    reaped.map_err(|_| "SSH process did not terminate before registration cleanup")??;
    Ok(())
}

async fn register_ssh(
    metadata: &ClientMetadata,
    registered: &mut bool,
    mut command: impl FnMut() -> Command,
    cancelled: impl std::future::Future<Output = ()>,
    deadline: Duration,
) -> crate::Result<Option<RegisteredSsh>> {
    tokio::pin!(cancelled);
    tokio::select! {
        biased;
        _ = &mut cancelled => return Ok(None),
        _ = std::future::ready(()) => {},
    }
    let mut ssh = command()
        .spawn()
        .map_err(|error| format!("could not start OpenSSH ssh: {error}"))?;
    let mut stderr_pump = ssh.stderr.take().map(|mut stderr| {
        SshStderr(tokio::spawn(async move {
            let mut bytes = [0u8; 4096];
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    break;
                }
                render::event(
                    "ssh",
                    &json!({"stderr":String::from_utf8_lossy(&bytes[..count])}),
                );
            }
        }))
    });
    let mut writer = ssh.stdin.take().ok_or("SSH stdin unavailable")?;
    let mut reader = BufReader::new(ssh.stdout.take().ok_or("SSH stdout unavailable")?);
    if let Err(error) = write_frame(
        &mut writer,
        &Frame::Hello {
            version: VERSION,
            mode: Mode::Shell,
            metadata: metadata.clone(),
        },
    )
    .await
    {
        stop_registration(&mut ssh, &mut stderr_pump).await?;
        return Err(error.into());
    }
    let response = tokio::select! {
        result = timeout(deadline, read_frame(&mut reader)) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "SSH authentication timed out"))
                .and_then(|frame| frame)
        },
        _ = &mut cancelled => {
            stop_registration(&mut ssh, &mut stderr_pump).await?;
            return Ok(None);
        },
    };
    let response = match response {
        Ok(frame) => frame,
        Err(error) => {
            stop_registration(&mut ssh, &mut stderr_pump).await?;
            return Err(error.into());
        }
    };
    let ready = match response {
        Some(Frame::Ready {
            url,
            login_code,
            login_expires_in,
            client_id,
            capabilities,
        }) => (
            (url, login_code, login_expires_in, client_id),
            capabilities
                .iter()
                .any(|capability| capability == crate::wire::SESSION_ACCESS_CAPABILITY),
        ),
        Some(Frame::Reject { message }) => {
            stop_registration(&mut ssh, &mut stderr_pump).await?;
            // Only a parsed registration rejection is terminal for the protocol.
            // Stderr, EOF, timeouts and unrelated authorization errors stay distinct.
            if message == UNSUPPORTED_WIRE_PROTOCOL
                || message.starts_with(&format!("{UNSUPPORTED_WIRE_PROTOCOL};"))
            {
                return Err(UnsupportedWireProtocol.into());
            }
            return Err(relay::failure(message));
        }
        None => {
            stop_registration(&mut ssh, &mut stderr_pump).await?;
            return Err(
                "Account relay disconnected before registration; check the server connection and preceding relay error".into()
            );
        }
        _ => {
            stop_registration(&mut ssh, &mut stderr_pump).await?;
            return Err("unexpected registration response".into());
        }
    };
    *registered = true;
    let (ready, access_supported) = ready;
    let github_auth = !metadata.allow.is_empty();
    if github_auth && (!ready.1.is_empty() || ready.2 != 0) {
        stop_registration(&mut ssh, &mut stderr_pump).await?;
        return Err("server did not acknowledge the explicit account allow policy; upgrade ssmcpd before connecting".into());
    }
    if !github_auth && (ready.1.is_empty() || ready.2 == 0) {
        stop_registration(&mut ssh, &mut stderr_pump).await?;
        return Err("server returned no account access policy for this connection; use --allow username@relay-server".into());
    }
    Ok(Some(RegisteredSsh {
        ssh,
        stderr_pump,
        writer,
        reader,
        ready,
        access_supported,
    }))
}

async fn send_steering(
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    id: u64,
    backend: &Runtime,
) -> crate::Result<()> {
    let Some(command) = steering::pending_command(id) else {
        return Ok(());
    };
    let command = if let crate::wire::SteeringCommand::Publish { text, .. } = &command {
        let prepared = match timeout(Duration::from_secs(5), backend.expand_references(text)).await {
            Ok(Ok(text)) => steering::prepare_publish(id, text),
            Ok(Err(error)) => Err(error),
            Err(_) => Err("Preparing the file or skill references timed out. The request was not sent; review the retained draft and retry.".into()),
        };
        match prepared {
            Ok(Some(command)) => command,
            Ok(None) => return Ok(()),
            Err(error) => {
                steering::fail_unsent(id, &error);
                terminal::notice(&error);
                return Ok(());
            }
        }
    } else {
        command
    };
    timeout(Duration::from_secs(10), async {
        let mut writer = writer.lock().await;
        // Preparation and lock acquisition cannot consume the reply deadline.
        // A cancelled or superseded request must never reach the wire.
        if !steering::sent(id) {
            return Ok(());
        }
        write_frame(&mut *writer, &Frame::Steering { id, command }).await
    })
    .await
    .map_err(|_| "request queue write timed out")??;
    Ok(())
}

/// Freeze mutations and reclaim queued requests before tearing down their room.
/// Delivery can win a takeback race; those requests remain delivered and never replay.
async fn reclaim_steering(
    incoming: &mut Incoming,
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    commands: &mut mpsc::Receiver<steering::Transport>,
    backend: &Runtime,
) -> crate::Result<()> {
    steering::begin_reload()?;
    timeout(Duration::from_secs(15), async {
        loop {
            if steering::reload_ready()? { return Ok::<(), crate::Error>(()); }
            tokio::select! {
                command = commands.recv() => match command {
                    Some(steering::Transport::Wire { id, .. }) => send_steering(writer, id, backend).await?,
                    Some(steering::Transport::Reload) => {},
                    None => return Err("client control channel ended while preparing reload".into()),
                },
                frame = incoming.frames.recv() => match frame {
                    Some(Ok(Some(Frame::SteeringReply { id, result }))) => steering::reply(id, result)?,
                    Some(Ok(Some(Frame::AccessReply { id, result }))) => access::reply(id, result),
                    Some(Ok(Some(Frame::CodeModeFinished { completion }))) => backend.code_mode_finished(completion)?,
                    Some(Ok(Some(Frame::Call { id, .. }))) => {
                        let mut writer = writer.lock().await;
                        write_frame(&mut *writer, &Frame::Reply { id, result: Err("client reload is preparing; refresh clients after reconnection".into()) }).await?;
                    },
                    Some(Ok(Some(Frame::Reject { message }))) => return Err(message.into()),
                    Some(Err(error)) => return Err(error.into()),
                    _ => return Err("connection ended while preserving queued requests".into()),
                }
            }
        }
    }).await.map_err(|_| "queued requests could not be reclaimed in time; client remains running")?
}

async fn connect(
    options: &Options,
    ui: &ui::Ui,
    service: Option<&crate::session::Runner>,
    attachment: Option<&crate::session::attach::Handle>,
    attempt: u64,
    registered: &mut bool,
) -> crate::Result<ConnectionExit> {
    let management = session_control(options, service)?;
    let mut interrupt = interrupt_signal()?;
    let mut terminate = terminate_signal()?;
    let restart = reload::Restart::capture()?;
    let metadata = options.metadata()?;
    let server = options
        .server
        .as_deref()
        .ok_or("a relay server is required")?;
    let credential = options.relay.as_ref();
    let executable = std::env::current_exe()?;
    render::status(
        "https",
        "Progress",
        &format!(
            "Connecting to {} with an explicit account allow list",
            server
        ),
    );
    let cancelled = async {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    };
    let registration = register_ssh(
        &metadata,
        registered,
        || relay::command(&executable, server, credential),
        cancelled,
        Duration::from_secs(120),
    )
    .await?;
    let Some(mut registration) = registration else {
        return Ok(ConnectionExit::Stopped);
    };
    registration.require_allow_access(&metadata.allow).await?;
    let RegisteredSsh {
        mut ssh,
        stderr_pump: _stderr_pump,
        writer,
        reader,
        ready: (url, login_code, login_expires_in, client_id),
        access_supported,
    } = registration;
    let github_auth = !metadata.allow.is_empty();
    let login_received_at = std::time::Instant::now();
    let display_session =
        match terminal::Session::start(ui, &metadata.cwd, metadata.title.as_deref(), Some(&url)) {
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
    terminal::set_logout_target(if options.temporary {
        None
    } else {
        options
            .relay
            .as_ref()
            .map(|credential| (credential.server.as_str(), credential.token.as_str()))
    });
    let mut incoming = Incoming::start(reader);
    render::status(
        "shell",
        "Progress",
        "Starting native tools; optional harnesses stay unloaded",
    );
    let backend = tokio::select! {
        biased;
        error = incoming.disconnected() => { let _ = ssh.kill().await; return Err(relay::failure(error)); },
        status = ssh.wait() => {
            let mut error = ssh_ended(status);
            if options.relay.is_some() { error = incoming.after_relay_exit(error).await; }
            return Err(relay::failure(error));
        },
        _ = interrupt.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = ssh.kill().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(120), Backend::start_with_tty(&options.shell, options.tty.as_deref())) => result.map_err(|_| "local backend startup timed out")??,
    };
    let backend = match Runtime::with_session_control(
        backend.clone(),
        PathBuf::from(&metadata.cwd),
        options.context_roots.clone(),
        management,
    )
    .and_then(|runtime| {
        runtime.enable_default_skill_sources()?;
        Ok(runtime)
    }) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ssh.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = ssh.wait().await;
            return Err(error.into());
        }
    };
    let _attachment_runtime = match attachment
        .map(|handle| handle.bind_runtime(backend.clone()))
        .transpose()
    {
        Ok(binding) => binding,
        Err(error) => {
            let _ = ssh.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = ssh.wait().await;
            return Err(error);
        }
    };
    // Native tools are ready immediately; optional providers stay unloaded until requested.
    let discovered = tokio::select! {
        biased;
        error = incoming.disconnected() => Err(error),
        status = ssh.wait() => {
            let error = ssh_ended(status);
            Err(if options.relay.is_some() { incoming.after_relay_exit(error).await } else { error })
        },
        _ = interrupt.recv() => { let _ = ssh.start_kill(); backend.shutdown().await; let _ = ssh.wait().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = ssh.start_kill(); backend.shutdown().await; let _ = ssh.wait().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(120), backend.tools()) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
    };
    let readiness = discovered.and_then(|_| incoming.ensure_open(&mut ssh));
    if let Err(mut error) = readiness {
        if options.relay.is_some() && ssh.try_wait()?.is_some() {
            error = incoming.after_relay_exit(error).await;
        }
        let _ = ssh.start_kill();
        backend.shutdown().await;
        incoming.stop().await;
        let _ = ssh.wait().await;
        return Err(relay::failure(error));
    }
    let login_expires_in = login_expires_in.saturating_sub(login_received_at.elapsed().as_secs());
    let mut output = json!({"url":url,"client_id":client_id,"name":metadata.name,"cwd":metadata.cwd,"hostname":metadata.hostname,"type":"shell"});
    if let Some(title) = &metadata.title {
        output["title"] = json!(title);
    }
    if let Some(credential) = &options.relay {
        output["transport"] = json!("https");
        output["temporary"] = json!(options.temporary);
        output["expires_at"] = json!(credential.expires_at);
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
    let local_input = match input::Input::attach(backend.terminals()) {
        Ok(input) => input,
        Err(error) => {
            let _ = ssh.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = ssh.wait().await;
            return Err(error.into());
        }
    };
    let git_settings = config::git()?;
    // Git refresh belongs to the connected workspace, including JSONL and
    // background sessions. The display only consumes its optional snapshot.
    let git_status = git_status::GitStatus::start(PathBuf::from(&metadata.cwd), git_settings);
    let mut ui_updates = tokio::time::interval(Duration::from_secs(1));
    ui_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let writer = Arc::new(Mutex::new(writer));
    let mut steering_commands = steering::connect(&client_id, VERSION);
    let mut access_commands = access::connect(access_supported);
    let mut steering_updates = tokio::time::interval(Duration::from_secs(1));
    steering_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut requests: JoinSet<io::Result<bool>> = JoinSet::new();
    let mut reload_requested = false;
    let mut intentionally_stopped = false;
    let mut service_updates = tokio::time::interval(Duration::from_secs(1));
    service_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut result: crate::Result<()> = 'connected: loop {
        tokio::select! {
            _ = steering_updates.tick() => {
                if let Err(error) = steering::poll() { terminal::notice(&error); }
                access::poll();
            },
            Some(request) = access_commands.recv() => {
                let sent = timeout(Duration::from_secs(10), async {
                    let mut writer = writer.lock().await;
                    if access::pending(request.id) {
                        write_frame(&mut *writer, &Frame::Access { id: request.id, command: request.command }).await?;
                    }
                    Ok::<(), io::Error>(())
                }).await;
                match sent {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => break Err(error.into()),
                    Err(_) => break Err("session access request write timed out".into()),
                }
            },
            command = steering_commands.recv() => match command {
                Some(steering::Transport::Wire { id, .. }) => {
                    if let Err(error) = send_steering(&writer, id, &backend).await { break Err(error); }
                },
                Some(steering::Transport::Reload) => {
                    while let Some(completed) = requests.try_join_next() {
                        match completed {
                            Ok(Ok(_)) => {},
                            Ok(Err(error)) => break 'connected Err(error.into()),
                            Err(error) => break 'connected Err(error.into()),
                        }
                    }
                    let accepted = if options.temporary {
                        Err("this retired temporary credential cannot restart a session; start again with --allow username@relay-server".into())
                    } else {
                        backend.validate_reload().and_then(|()| restart.accept(&json!({}), requests.len(), &client_id, github_auth)).map(|_| ())
                    };
                    match accepted {
                        Err(error) => { steering::abort_reload(&error); terminal::notice(&error); }
                        Ok(()) => match reclaim_steering(&mut incoming, &writer, &mut steering_commands, &backend).await {
                            Ok(()) => { reload_requested = true; break Ok(()); }
                            Err(error) => { steering::abort_reload(&error.to_string()); terminal::notice(&error.to_string()); }
                        },
                    }
                },
                None => {},
            },
            _ = service_updates.tick(), if service.is_some() => {
                if service.expect("guarded service runner").stop_requested() {
                    intentionally_stopped = true;
                    break Ok(());
                }
            },
            _ = ui_updates.tick(), if display_session.is_some() => {
                if terminal::exit_requested() {
                    intentionally_stopped = true;
                    break Ok(());
                }
                render::set_git_status(git_status.snapshot());
            },
            frame = incoming.frames.recv() => match frame {
                Some(Ok(Some(Frame::Call {id,operation,controller,attribution,code_context}))) => {
                    if let Some(attribution) = &attribution
                        && let Err(error) = attribution.validate() {
                        break Err(error.into());
                    }
                    while let Some(completed) = requests.try_join_next() {
                        match completed {
                            Ok(Ok(was_tool)) => {
                                if was_tool { git_status.refresh(); }
                            },
                            Ok(Err(error)) => break 'connected Err(error.into()),
                            Err(error) => break 'connected Err(error.into()),
                        }
                    }
                    let mut code_call = match backend.begin_code_call(&operation, id, controller.as_ref(), attribution.as_deref(), code_context) {
                        Ok(call) => call,
                        Err(error) => break Err(error.into()),
                    };
                    if reload::is_reload(&operation) {
                        let _controller = render::controller_started(id, controller.as_ref(), attribution.as_deref(), &operation);
                        let crate::wire::Operation::CallTool { arguments, .. } = &operation else { unreachable!() };
                        let mut response = if options.temporary {
                            Err("this retired temporary credential cannot restart a session; start again with --allow username@relay-server".into())
                        } else {
                            backend.validate_reload().and_then(|()| restart.accept(arguments, requests.len(), &client_id, github_auth))
                        };
                        if response.is_ok()
                            && let Err(error) = reclaim_steering(&mut incoming, &writer, &mut steering_commands, &backend).await {
                                steering::abort_reload(&error.to_string());
                                response = Err(error.to_string());
                            }
                        let accepted = response.is_ok();
                        if let Some(call) = &mut code_call { call.finish(&response); }
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
                                        Some(Ok(Some(Frame::CodeModeFinished { completion }))) => { backend.code_mode_finished(completion)?; }
                                        Some(Ok(Some(Frame::SteeringReply { id, result }))) => { steering::reply(id, result)?; }
                                        Some(Ok(Some(Frame::AccessReply { id, result }))) => { access::reply(id, result); }
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
                                    steering::abort_reload("Server did not confirm reload receipt; client remains running");
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
                        let _controller = (!matches!(&operation, crate::wire::Operation::ListTools { .. }))
                            .then(|| render::controller_started(id, controller.as_ref(), attribution.as_deref(), &operation));
                        let was_tool = matches!(&operation, crate::wire::Operation::CallTool { .. });
                        let mut result = backend.perform(operation).await;
                        if let Some(call) = &mut code_call { call.finish(&result); }
                        // A large native response must not corrupt/disconnect the SSH stream.
                        if serde_json::to_vec(&result).map_or(true, |bytes|bytes.len() + 256 > MAX_FRAME) {
                            result = Err("native call finished but its result exceeds the 4 MiB bridge frame limit; do not blindly repeat a mutating call, inspect native state or request a smaller output page".into());
                        }
                        if let Err(error) = &result { render::status("ssmcp", "Error", error); }
                        timeout(Duration::from_secs(10),async {
                            let mut writer = writer.lock().await;
                            write_frame(&mut *writer,&Frame::Reply {id,result}).await
                        }).await.map_err(|_| io::Error::new(io::ErrorKind::TimedOut,"SSH write timed out"))??;
                        crate::memory::release_unused_heap();
                        Ok(was_tool)
                    });
                }
                Some(Ok(Some(Frame::CodeModeFinished { completion }))) => {
                    if let Err(error) = backend.code_mode_finished(completion) { break Err(error.into()); }
                }
                Some(Ok(Some(Frame::SteeringReply { id, result }))) => {
                    if let Err(error) = steering::reply(id, result) { terminal::notice(&error); }
                }
                Some(Ok(Some(Frame::AccessReply { id, result }))) => access::reply(id, result),
                Some(Ok(None)) | None => break Ok(()),
                Some(Ok(Some(Frame::Reject {message}))) => break Err(relay::failure(message)),
                Some(Ok(Some(_))) => break Err("unexpected SSH protocol frame".into()),
                Some(Err(error)) => break Err(error.into()),
            },
            response = requests.join_next(), if !requests.is_empty() => match response {
                Some(Ok(Ok(was_tool))) => {
                    if was_tool { git_status.refresh(); }
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
    steering::disconnect();
    access::disconnect();
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    let pending_logout_message = terminal::finish_pending_logout().await;
    if pending_logout_message.is_some() || terminal::exit_requested() {
        intentionally_stopped = true;
        reload_requested = false;
        result = Ok(());
    }
    git_status.shutdown().await;
    incoming.stop().await;
    drop(writer);
    let _ = ssh.start_kill();
    backend.shutdown().await;
    let _ = ssh.wait().await;
    let exit_message = terminal::take_exit_message().or(pending_logout_message);
    drop(display_session);
    if let Some(message) = exit_message {
        // Print the complete safe outcome after restoring the terminal.
        // Status rows clip long messages and can hide revocation failures.
        eprintln!("{message}");
    }
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
    fn setup_defaults_the_host_after_parsing_options_without_swallowing_explicit_hosts() {
        let options = Options::parse_with_default(
            ["--name", "dev machine", "--daemon"].map(OsString::from),
            Some("ssmcp.lmm.best"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.destination.as_deref(), Some("ssmcp.lmm.best"));
        assert_eq!(options.name.as_deref(), Some("dev machine"));
        assert!(options.daemon);
        let explicit = Options::parse_with_default(
            ["--name", "dev", "user@example.test"].map(OsString::from),
            Some("ssmcp.lmm.best"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(explicit.destination.as_deref(), Some("user@example.test"));
        let account = Options::parse(["--name", "dev"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(account.destination.is_none() && account.server.is_none());
        assert_eq!(account.name.as_deref(), Some("dev"));
    }

    #[test]
    fn session_children_inherit_access_without_management_authority() {
        let parent = Options::parse(
            [
                "--name",
                "manager",
                "--session-control",
                "--watchdog",
                "--reconnect-delay",
                "20",
                "--allow",
                "octocat@github",
                "-F",
                "relative-ssh-config",
                "work-server",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let template = parent.session_spec().unwrap();
        assert!(template.args.iter().any(|arg| arg == "--session-control"));
        let child = child_session_spec(
            &template,
            "worker".into(),
            std::env::current_dir().unwrap(),
            Some("--help".into()),
            Some(true),
            Some(2),
        )
        .unwrap();
        let parsed = Options::parse(child.args.iter().map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.allow, parent.allow);
        assert_eq!(parsed.destination, parent.destination);
        assert_eq!(parsed.title.as_deref(), Some("--help"));
        assert_eq!(parsed.name.as_deref(), Some("worker"));
        assert!(PathBuf::from(parsed.config.unwrap()).is_absolute());
        assert!(!parsed.session_control);
        assert!(!parsed.daemon);
        assert_eq!(parsed.reconnect_delay_secs, 2);
        validate_session_spec(&child).unwrap();
        let without_watchdog = child_session_spec(
            &template,
            "manual".into(),
            std::env::current_dir().unwrap(),
            None,
            Some(false),
            None,
        )
        .unwrap();
        assert!(!without_watchdog.watchdog);
        assert_eq!(without_watchdog.reconnect_delay_secs, 5);
        assert!(
            child_session_spec(
                &template,
                "worker".into(),
                std::env::current_dir().unwrap(),
                None,
                Some(false),
                Some(2),
            )
            .is_err()
        );
    }

    #[test]
    fn service_runner_rejects_recursive_and_mismatched_manifests() {
        let parent = Options::parse(["--name", "worker", "work-server"].map(OsString::from))
            .unwrap()
            .unwrap();
        let mut spec = parent.session_spec().unwrap();
        validate_session_spec(&spec).unwrap();
        spec.args.insert(0, "--daemon".into());
        assert!(validate_session_spec(&spec).is_err());
        spec.args.remove(0);
        spec.name = "different".into();
        assert!(validate_session_spec(&spec).is_err());
    }

    #[test]
    fn background_and_watchdog_options_have_bounded_explicit_lifecycles() {
        let normal = Options::parse(["host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(!normal.daemon && normal.watchdog);
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
        let custom = Options::parse(["--reconnect-delay", "5", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(custom.watchdog);
        for args in [
            vec!["--no-watchdog", "--reconnect-delay", "5", "host"],
            vec!["--watchdog", "--no-watchdog", "host"],
            vec!["--no-watchdog", "--watchdog", "host"],
        ] {
            assert!(Options::parse(args.into_iter().map(OsString::from)).is_err());
        }
        let disabled = Options::parse(["--no-watchdog", "host"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(!disabled.watchdog);
        let saved = disabled.session_spec().unwrap();
        assert!(saved.args.iter().any(|arg| arg == "--no-watchdog"));
        validate_session_spec(&saved).unwrap();
        assert!(
            !Options::parse(saved.args.iter().map(OsString::from))
                .unwrap()
                .unwrap()
                .watchdog
        );
        for args in [vec!["--list-tools"], vec!["--list-tools", "--no-watchdog"]] {
            let discovery = Options::parse(args.into_iter().map(OsString::from))
                .unwrap()
                .unwrap();
            assert!(discovery.list_tools && !discovery.watchdog);
        }
        for flag in ["--daemon", "--watchdog"] {
            assert!(Options::parse([flag, "--list-tools"].map(OsString::from)).is_err());
        }
    }

    #[test]
    fn account_connections_preserve_origin_and_never_store_credentials_in_session_args() {
        let options = Options::parse(
            [
                "--server",
                "https://example.com",
                "--name",
                "account-device",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert!(options.destination.is_none());
        assert_eq!(options.server.as_deref(), Some("https://example.com"));
        let spec = options.session_spec().unwrap();
        validate_session_spec(&spec).unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|pair| pair == ["--server", "https://example.com"])
        );
        assert!(
            !spec
                .args
                .iter()
                .any(|arg| arg.contains("token") || arg.contains("credential"))
        );
        for args in [
            vec!["--server", "https://example.com", "ssh-host"],
            vec!["--server", "https://example.com", "-i", "key"],
            vec!["--server", "https://example.com", "-p", "22"],
            vec!["--server", "https://example.com", "--list-tools"],
            vec!["--server", "http://example.com"],
        ] {
            assert!(Options::parse(args.into_iter().map(OsString::from)).is_err());
        }
    }

    #[test]
    fn legacy_ssh_identity_and_new_long_identity_option_remain_equivalent() {
        for flag in ["-i", "--identity"] {
            let options = Options::parse([flag, "/tmp/key", "ssh-host"].map(OsString::from))
                .unwrap()
                .unwrap();
            assert_eq!(
                options.identity.as_deref(),
                Some(std::ffi::OsStr::new("/tmp/key"))
            );
            assert!(options.server.is_none());
        }
    }

    #[test]
    fn retired_join_is_not_a_reusable_connection_destination() {
        assert!(Options::parse(["join", "--token", "retired"].map(OsString::from)).is_err());
    }

    #[test]
    fn legacy_saved_sessions_preserve_disabled_watchdog() {
        let options =
            Options::parse(["--name", "legacy", "--no-watchdog", "host"].map(OsString::from))
                .unwrap()
                .unwrap();
        let mut legacy = options.session_spec().unwrap();
        legacy.args.retain(|arg| arg != "--no-watchdog");
        validate_session_spec(&legacy).unwrap();
        assert!(!Options::parse_saved(&legacy).unwrap().unwrap().watchdog);
        let child = child_session_spec(
            &legacy,
            "child".into(),
            std::env::current_dir().unwrap(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!child.watchdog);
        assert!(child.args.iter().any(|arg| arg == "--no-watchdog"));
        legacy.args.insert(0, "--watchdog".into());
        assert!(validate_session_spec(&legacy).is_err());
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
        assert_eq!(session_title(&spec).unwrap().as_deref(), Some("--daemon"));
        let restored = Options::parse(spec.args.into_iter().map(OsString::from))
            .unwrap()
            .unwrap();
        assert!(!restored.daemon);
        assert_eq!(restored.title.as_deref(), Some("--daemon"));
        assert_eq!(restored.allow, ["octocat@github"]);
        assert_eq!(
            restored.identity,
            options
                .identity
                .as_ref()
                .map(|path| spec.cwd.join(path).into_os_string())
        );
        assert_eq!(
            restored.config,
            options
                .config
                .as_ref()
                .map(|path| spec.cwd.join(path).into_os_string())
        );
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
        let options =
            Options::parse(["--name", "My workstation", "alice@host"].map(OsString::from))
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
                    OsString::from("alice@host")
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn session_title_preserves_unicode_task_context_without_becoming_an_ssh_argument() {
        let title = "修复会话重启 · issue #42";
        let options = Options::parse(
            [
                "--ssh",
                "--name",
                "workstation",
                "--title",
                title,
                "user@host",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let metadata = options.metadata().unwrap();
        assert_eq!(metadata.name, "workstation");
        assert_eq!(metadata.title.as_deref(), Some(title));
        assert_eq!(
            session_title(&options.session_spec().unwrap())
                .unwrap()
                .as_deref(),
            Some(title)
        );
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
            session_title(&absent.session_spec().unwrap())
                .unwrap()
                .is_none()
        );
        assert!(
            serde_json::to_value(absent.metadata().unwrap())
                .unwrap()
                .get("title")
                .is_none()
        );
    }

    #[test]
    fn session_title_reads_saved_display_metadata_without_context_root_validation() {
        let spec = crate::session::SessionSpec {
            name: "fixture".into(),
            cwd: std::env::temp_dir(),
            args: [
                "--context-root",
                "/unused/display/metadata-only",
                "--name",
                "--title",
                "--title=修复客户端",
                "--title",
                "--daemon",
                "--ssh",
                "fixture.invalid",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            watchdog: true,
            reconnect_delay_secs: 5,
        };
        assert_eq!(session_title(&spec).unwrap().as_deref(), Some("--daemon"));
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
    fn qualified_allow_flags_combine_canonicalize_and_deduplicate() {
        let options = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--allow",
                "OctoCat@RELAY.Example:443, Alice-Dev@relay.example",
                "--allow",
                "OCTOCAT@relay.example,bob@relay.example",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            options.allow,
            [
                "octocat@relay.example",
                "alice-dev@relay.example",
                "bob@relay.example"
            ]
        );
        assert_eq!(options.metadata().unwrap().allow, options.allow);
        assert_eq!(options.server.as_deref(), Some("https://relay.example"));
        assert!(options.destination.is_none());
    }

    #[test]
    fn qualified_allow_survives_saved_sessions_without_credentials() {
        let options = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--name",
                "allow-roundtrip",
                "--allow",
                "Alice@relay.example, ALICE_DEV@relay.example",
                "--allow",
                "alice@relay.example",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            options.allow,
            ["alice@relay.example", "alice_dev@relay.example"]
        );
        let restored = Options::parse_saved(&options.session_spec().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(restored.allow, options.allow);
        assert_eq!(restored.server, options.server);
        assert!(restored.relay.is_none());
        assert!(restored.destination.is_none());
    }

    #[test]
    fn allow_limits_targets_and_rejects_invalid_or_local_options() {
        let all = (0..MAX_ALLOWED_PRINCIPALS)
            .map(|i| format!("user-{i}@relay.example"))
            .collect::<Vec<_>>()
            .join(",");
        let options = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--allow",
                &all,
                "--allow",
                &all,
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.allow.len(), MAX_ALLOWED_PRINCIPALS);
        let error = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--allow",
                &all,
                "--allow",
                "another@relay.example",
            ]
            .map(OsString::from),
        )
        .err()
        .unwrap();
        assert!(error.contains("32 distinct"), "{error}");
        for value in [
            "",
            "alice",
            "root@relay.example",
            "alice@github",
            "alice@relay.example,",
            ",alice@relay.example",
            "alice@https://relay.example/path",
        ] {
            let error = Options::parse(
                ["--server", "https://relay.example", "--allow", value].map(OsString::from),
            )
            .err()
            .unwrap();
            assert!(error.contains("--allow"), "{error}");
        }
        assert!(
            Options::parse(["--allow"].map(OsString::from))
                .err()
                .unwrap()
                .contains("requires USER@SERVER")
        );
        assert!(
            Options::parse(["--list-tools", "--allow", "alice@relay.example"].map(OsString::from))
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

#[cfg(test)]
mod named_entry_tests {
    use super::*;

    fn options(args: &[&str]) -> Options {
        Options::parse(args.iter().map(OsString::from))
            .unwrap()
            .unwrap()
    }

    #[test]
    fn username_destination_uses_https_without_ssh_options() {
        let options = options(&["Alice@EXAMPLE.com:8443", "--no-browser", "--name", "dev"]);
        assert!(options.destination.is_none());
        assert_eq!(options.server.as_deref(), Some("https://example.com:8443"));
        assert_eq!(options.username.as_deref(), Some("alice"));
        assert!(options.no_browser);
        assert!(options.identity.is_none());
        let spec = options.session_spec().unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|args| args == ["--username", "alice"])
        );
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.username.as_deref(), Some("alice"));
        assert_eq!(restored.server, options.server);
        assert!(restored.destination.is_none());
    }

    #[test]
    fn named_destination_rejects_conflicting_transport_or_identity() {
        for args in [
            vec!["--server", "https://other.example", "alice@example.com"],
            vec!["--username", "bob", "alice@example.com"],
            vec!["--username", "alice", "--ssh", "alice@example.com"],
            vec!["--list-tools", "alice@example.com"],
            vec!["admin@example.com"],
            vec!["alice@example.com/path"],
        ] {
            assert!(
                Options::parse(args.iter().map(OsString::from)).is_err(),
                "{args:?}"
            );
        }
    }

    #[test]
    fn explicit_ssh_and_existing_saved_sessions_preserve_their_transport() {
        for args in [
            vec!["--ssh", "alice@example.com"],
            vec!["-p", "22", "alice@example.com"],
            vec!["--identity", "/tmp/key", "alice@example.com"],
        ] {
            let options = options(&args);
            assert_eq!(options.destination.as_deref(), Some("alice@example.com"));
            assert!(options.server.is_none());
            assert!(options.username.is_none());
        }
        let saved = crate::session::SessionSpec {
            name: "legacy".into(),
            cwd: std::env::current_dir().unwrap(),
            args: vec!["--name".into(), "legacy".into(), "old@host".into()],
            watchdog: true,
            reconnect_delay_secs: 5,
        };
        let restored = Options::parse_saved(&saved).unwrap().unwrap();
        assert_eq!(restored.destination.as_deref(), Some("old@host"));
        assert!(restored.server.is_none());
        assert!(
            restored
                .session_spec()
                .unwrap()
                .args
                .iter()
                .any(|arg| arg == "--ssh")
        );
    }
}

#[cfg(test)]
mod tty_option_tests {
    use super::*;
    #[test]
    fn terminal_selection_survives_saved_sessions_without_entering_ssh_argv() {
        let options =
            Options::parse(["--ssh", "fixture.invalid", "--tty", "kitty"].map(OsString::from))
                .unwrap()
                .unwrap();
        assert_eq!(options.tty.as_deref(), Some("kitty"));
        let saved = options.session_spec().unwrap();
        let restored = Options::parse_saved(&saved).unwrap().unwrap();
        assert_eq!(restored.tty.as_deref(), Some("kitty"));
        let options = Options::parse(
            [
                "--ssh",
                "fixture.invalid",
                "--tty=/tmp/terminal with spaces",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(options.tty.as_deref(), Some("/tmp/terminal with spaces"));
        assert!(Options::parse(["--tty"].map(OsString::from)).is_err());
        assert!(Options::parse(["--tty="].map(OsString::from)).is_err());
    }
}
