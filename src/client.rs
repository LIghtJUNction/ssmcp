//! Foreground relay or local client. Connection details are emitted once as JSON.
mod access;
pub(crate) mod account;
pub(crate) mod agent;
pub(crate) mod attachments;
mod audit_view;
pub(crate) mod chat_io;
mod clipboard;
pub(crate) mod completion;
mod composer;
mod computer_control;
mod config;
mod connection_context;
pub(crate) mod context_memory;

#[cfg(test)]
mod account_compat_tests;
mod git_status;
mod handoff;
mod highlight;
#[cfg(test)]
mod incoming_tests;
pub(crate) mod input;
pub(crate) mod interface;
mod invite;
mod link_metrics;
#[cfg(target_os = "linux")]
mod local;
pub(crate) mod messaging;
pub(crate) mod notification;
#[cfg(all(test, unix))]
mod output_expiry_fixture;
mod panel;
mod paths;
pub(crate) mod presentation;
#[cfg(test)]
mod protocol_tests;
pub(crate) mod references;
mod relay;
pub(crate) mod reload;
pub(crate) mod render;
pub(crate) mod steering;
pub(crate) mod terminal;
mod trust;
pub(crate) mod ui;
mod ui_command;
mod update;
mod version;
mod worker_control;
mod worker_window;
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

const WORKER_WINDOW_CHILD_FLAG: &str = "--ssmcp-worker-window-child";

const HELP: &str = r#"ssmcp — connect a device; control its tools remotely

Usage: ssmcp [username@server] [options]

Start
  ssmcp                         Reuse your last authorized account
  ssmcp --worker N              Open chat and a worker window with N slots
  ssmcp local                   Run a local MCP session on Linux
  ssmcp alice@relay.example     Connect and sign in when needed
  ssmcp list                    List online processes and registered threads
  ssmcp -i                      Open an authorized remote terminal
  ssmcp attach [process|thread] ID|N   Attach to remote chat
  ssmcp send [process|thread] ID|N TEXT   Submit chat input; print its receipt
  ssmcp wait [REQUEST_ID ...]   Print actual replies; accepts piped receipts

Manage
  config · whoami · logout      Account and server settings
  context                     Saved local and HTTPS connection targets
  update [--check]              Update the installed client
  cleanup                      Inspect and clean registered workspace files
  paths [NAME ...]             Show configuration and state paths without startup
  tools [status|check|update]    Inspect or update this client's tool repository
  sessions · attach · service  Saved and background sessions
  computer                     Zuse CLI version and installation
  setup · ui · 007             Setup, appearance and recovery

Options
  --name NAME   --title TEXT   --context NAME   --server URL   --context-root PATH
  --worker N    Open chat and a separate worker monitor
  --version [--all]    Show the version or the locked dependency tree
  --no-browser  --no-watchdog  --theme NAME   --no-color
  --allow USER@SERVER    Allow a registered account on this relay; repeat or comma-separate
  --account-compat       Use an existing verified self account on an older relay

Use ssmcp COMMAND --help for command options; --help-full lists advanced connection and executor flags.
Foreground --worker keeps chat here and opens a separate worker window.
Workers use remote agents by default; no local model starts.
In a connected chat: /help, @file, $skill, !command. From the initial conversation, Left at draft start opens Statistics; /sessions chooses recipients.
"#;

const HELP_FULL: &str = r#"ssmcp — connect your devices and control authorized sessions

Usage: ssmcp username@server [options]
       ssmcp [--server URL] [options]
       ssmcp [connection options] --worker N
       ssmcp whoami|logout [--server URL] [--json]
       ssmcp list [--json | -i] [--client ID]
       ssmcp -i [--server URL] [--client ID]
       ssmcp config <set|get|unset> server [URL]
       ssmcp config path
       ssmcp paths [NAME ...] [--json | --plain] [--list]
       ssmcp context <create|list|show|inspect|use|rm> [options]
       ssmcp update [--check]
       ssmcp cleanup [--dry-run|--list|--config|--set-config FILE]
       ssmcp tools [status|check|update]
       ssmcp --version [--all]
       ssmcp trust <add|list|remove|path> [--server URL] [--public-key FILE|--pin PIN]
       ssmcp [options] [--server URL]
       ssmcp setup [--server URL] [--no-browser]
       ssmcp sessions <list|status|start|stop|close|restart|logs|auth|remove> [NAME]
       ssmcp sessions create NAME [options] [user@]host
       ssmcp attach [process|thread] ID|N [--remote] [--read-only]
       ssmcp attach --local NAME [--read-only]
       ssmcp attach process ID|N --terminal TERMINAL_ID [--read-only]
       ssmcp send [process|thread] ID|N [TEXT|--stdin] [--wait] [--json]
       ssmcp wait [REQUEST_ID ...] [--stdin] [--any|--all|--count N]
                  [--timeout SECONDS] [--json] [--server URL]
       ssmcp computer [version|download [VERSION]|install DOWNLOAD_ID|uninstall INSTALLATION_ID]
                      [--json]
       ssmcp service <install|uninstall|start|stop|restart|status|run> [options]
       ssmcp ui <show|check|init|preview> [--ui-config FILE]
       ssmcp 007 <install|upgrade|run|start|stop|status|uninstall|sessions> [options]

  --server URL         Relay for this command; overrides config and last login
  --context NAME       Named local or HTTPS target; overrides SSMCP_CONTEXT and context use
  --input MODE         terminal (default), none, pipe, web, mcp-stdio, mcp-http or sock
  --input-host HOST    Loopback host for web or MCP HTTP (required)
  --input-port PORT    Port for web or MCP HTTP (required)
  --input-token-file PATH  Private bearer token file for web or MCP HTTP
  --input-socket PATH  Private Linux Unix socket path (default /tmp/ssmcp-UID/chat-input.sock)
  --agent RECIPIENT    Advanced chat recipient or explicitly configured local executor
  --worker N          Open chat plus a separate remote task monitor
  --worker-k K        Resource share multiplier for explicit local worker executors (default 1)
  --load-harness NAME  Explicitly preload one native protocol outside worker mode
  --list-harnesses    List supported harness providers
  --agent-exec PATH    Absolute executable for exec; optional --agent-argv-file PATH
  --agent-peer ID      Exact native message recipient for ssmcp; also set --agent-account PROVIDER:SUBJECT
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
  --account-compat     Explicit older-relay mode; requires one --allow for your saved account
                       Verifies its registered handle; never signs in or registers a name
                       This account owns the connection and pays its account quota
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
  --load-harness NAME  Preload one native protocol at startup
  --list-harnesses     List supported providers without starting them (optional --json)
  -- ARGS              Native arguments for an explicit --load-harness; never parsed as ssmcp flags

The client provides native tools without another harness. Optional Codex, Claude
Code, Pi and OpenCode bridges stay unloaded until explicitly needed.
Use ssmcp.harnesses.load for one provider, or the local /cc, /codex and /pi commands.
--load-harness codex|claude-code preloads the protocol without submitting a model turn.
--load-harness ssmcp requires -- --endpoint URL --token-env NAME [--socket PATH].
The peer URL identifies an authenticated MCP gateway, not a device/session.
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
  ssmcp --account-compat --allow alice@ssmcp.lmm.best
  ssmcp --server https://relay.example --allow alice@relay.example,bob@relay.example
  ssmcp sessions status workstation
  ssmcp sessions stop workstation
  ssmcp service install --system --user alice
  sudo systemctl enable --now ssmcp
  ssmcp sessions create workstation --watchdog alice@my-server.example
  ssmcp sessions create shared --allow alice@ssmcp.lmm.best
  ssmcp attach --local workstation
  ssmcp attach thread THREAD_ID
  ssmcp send process PROCESS_ID 'Review the patch' | ssmcp wait --count 2
  ssmcp sessions close workstation

On the controlled device, use --allow username@relay-server. No login or invitation
is needed there. The account must already exist on that relay; a different server,
an unknown or banned account, bare names and the former @github syntax are rejected.
Only the listed accounts receive access. Controllers must sign in through OAuth.
Names resolve to stable account IDs; billing follows the account making each call.

--account-compat is an explicit authenticated mode for an older relay, not anonymous
--allow. It requires a valid saved login for exactly the allowed account, verifies
its stable identity and registered handle with that relay, and never starts OAuth
or claims a username. The verified account owns this connection (including account
management and audit visibility) and its quota applies. Access-list editing is
unavailable. Missing, expired, revoked or mismatched credentials fail closed.

Without --allow, a saved valid login selects only your own registered username.
username@server signs in and registers that username when needed. whoami displays
your account, and logout removes the selected server's saved login. Other servers
and independent logins are preserved. --no-browser prints the authorization URL.
An update or a controlled-device restart does not grant a new controller identity.

ssmcp list shows authorized processes and their registered threads. Numbers bind
the last displayed directory for 5 minutes. Full IDs work directly; exact unique
process names also work. Display names never identify a thread.
Remote attach opens chat stdin/stdout. Process input fans out to its selected main
and registered threads; thread input reaches only that exact recipient. Enter
submits input. Ctrl-] or Ctrl-C detaches; pending requests remain running.
Use -i or attach process ID --terminal TERMINAL_ID for a remote shell PTY;
Ctrl-C reaches that shell. --read-only sends no chat/terminal input or resize.
ssmcp gui serve --assets PATH starts a paired GUI gateway for existing sessions.
It prints its loopback URL and private pairing-file path; see gui --help.
sessions list and bare attach show saved local sessions. Existing local names
work offline; --local selects them explicitly, and --remote selects online chat.
send prints a receipt. wait prints actual final replies: --any waits for one,
--all/--gather for all, and --count N for N distinct input results. Progress and
delivery acknowledgements are not completion. --json prints result JSONL.
Input is delivered on the recipient's next eligible native call; no model is
started. Stopping attach or wait never cancels the remaining work.
Chat I/O requires an upgraded relay and recipient client. Requests and outputs
have bounded live-server-memory retention of at most 1 hour; a server restart
loses pending requests and retained replies.
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
Bare `ssmcp attach` lists local saved sessions. Ctrl-] detaches without stopping the session;
the dashboard provides audit navigation. Use `ssmcp -i` for interactive input,
or disable the dashboard when attaching to a saved session.
--session-control explicitly grants this OS user's saved-session management.
New remote-created sessions inherit connection settings and stay in the current
workspace; children do not inherit this management grant.
Use sessions auth NAME to read private OAuth connection details explicitly.
Use the standalone Zuse CLI directly for desktop and browser operations.
SSMCP does not embed computer or browser control. The local `ssmcp computer`
command only queries, downloads, installs or uninstalls the Zuse CLI.
Managed installs do not replace an external executable or change PATH.
Background logs use JSONL without terminal colors; foreground display options
do not change service logging. Stopping a session suppresses its watchdog.
Reconnection starts a fresh client; jobs from the disconnected client are cleaned up.
"#;

// A single reader owns every partial frame after Ready, including while a native
// backend starts. A separate persistent status channel records a closed bridge
// even when its frame queue has not yet been consumed.
// A reconnect owns its stderr reader, including early failures.
#[derive(Clone, Copy)]
enum TransportKind {
    Relay,
    #[cfg(any(target_os = "linux", test))]
    Local,
}

impl TransportKind {
    fn label(self) -> &'static str {
        match self {
            Self::Relay => "Relay transport",
            #[cfg(any(target_os = "linux", test))]
            Self::Local => "Local transport",
        }
    }

    fn event_source(self) -> &'static str {
        match self {
            Self::Relay => "relay",
            #[cfg(any(target_os = "linux", test))]
            Self::Local => "local",
        }
    }

    fn ended(self, status: io::Result<ExitStatus>) -> String {
        let label = self.label();
        match status {
            Ok(status) => format!("{label} exited before local backend readiness ({status})"),
            Err(error) => {
                format!("Could not wait for {label} during local backend startup: {error}")
            }
        }
    }

    fn registration_timeout(self) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("{} registration timed out", self.label()),
        )
    }

    fn result_write_timeout(self) -> io::Error {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "{} result write timed out; the operation may have executed. Inspect native state before retrying a mutating call.",
                self.label()
            ),
        )
    }
}

struct TransportStderr(JoinHandle<()>);

impl Drop for TransportStderr {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Incoming {
    transport_kind: TransportKind,
    frames: mpsc::Receiver<io::Result<Option<Frame>>>,
    closed: watch::Receiver<Option<String>>,
    task: JoinHandle<()>,
}

impl Incoming {
    fn start<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
        mut reader: BufReader<R>,
        transport_kind: TransportKind,
    ) -> Self {
        let (frames_tx, frames) = mpsc::channel(MAX_INFLIGHT);
        let (closed_tx, closed) = watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                let frame = read_frame(&mut reader).await;
                let error = match &frame {
                    Ok(None) => Some(format!(
                        "{} closed before tools became ready",
                        transport_kind.label()
                    )),
                    Err(error) => Some(format!(
                        "{} stream failed before tools became ready: {error}",
                        transport_kind.label()
                    )),
                    Ok(Some(Frame::Reject { message })) => {
                        if message == relay::AUTH_EXPIRED {
                            Some(message.clone())
                        } else {
                            Some(format!(
                                "{} server rejected the connection: {message}",
                                transport_kind.label()
                            ))
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
            transport_kind,
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
                return "Connection reader stopped before tools became ready".into();
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

    // A connected writer can fail before the relay's final Reject is read.
    // No queued work may run after that failure, but dropping this reader would
    // turn a revoked credential into a transient error and retry it. Drain the
    // bounded frame queue so even a reader blocked on a full queue can deliver
    // the raw rejection. One deadline bounds the whole drain, including peers
    // that keep sending ordinary frames. EOF, reader errors and timeout retain
    // the original error and its type.
    async fn after_connection_error(&mut self, fallback: crate::Error) -> crate::Error {
        if fallback.downcast_ref::<relay::PermanentFailure>().is_some() {
            return fallback;
        }
        let rejection = timeout(Duration::from_secs(2), async {
            loop {
                match self.frames.recv().await {
                    Some(Ok(Some(Frame::Reject { message }))) => return Some(message),
                    Some(Ok(Some(_))) => {}
                    Some(Ok(None)) | Some(Err(_)) | None => return None,
                }
            }
        })
        .await;
        if let Ok(Some(message)) = rejection {
            let error = relay::failure(message);
            if error.downcast_ref::<relay::PermanentFailure>().is_some() {
                return error;
            }
        }
        fallback
    }

    fn ensure_open(&self, process: &mut Child) -> Result<(), String> {
        if let Some(error) = self.closed.borrow().clone() {
            return Err(error);
        }
        if self.closed.has_changed().is_err() {
            return Err("Connection reader stopped before tools became ready".into());
        }
        // The process can exit before its reader task observes EOF (or a proxy
        // child can retain stdout). Check both sources before publishing a code.
        if let Some(status) = process.try_wait().map_err(|error| error.to_string())? {
            return Err(self.transport_kind.ended(Ok(status)));
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

struct Options {
    #[cfg(target_os = "linux")]
    local: Option<local::Settings>,
    destination: Option<String>,
    server: Option<String>,
    username: Option<String>,
    no_browser: bool,
    account_compat: bool,
    relay: Option<account::Credential>,
    temporary: bool,
    name: Option<String>,
    title: Option<String>,
    allow: Vec<String>,
    context_roots: Vec<PathBuf>,
    chat: chat_io::Settings,
    worker: Option<crate::worker::Config>,
    worker_window_child: bool,
    worker_window: Option<Mutex<WorkerController>>,
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
    harness: Option<agent::Selection>,
}

impl Options {
    #[cfg(test)]
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        Self::parse_with_default(args, None)
    }

    fn parse_new(args: impl IntoIterator<Item = OsString>) -> Result<Option<Self>, String> {
        Self::parse_with_policy(args, None, true, false, true)
    }

    #[cfg(test)]
    fn parse_with_default(
        args: impl IntoIterator<Item = OsString>,
        default_destination: Option<&str>,
    ) -> Result<Option<Self>, String> {
        Self::parse_with_policy(
            args,
            default_destination,
            true,
            default_destination.is_some(),
            false,
        )
    }

    // Older saved manifests omitted --no-watchdog. Their recorded policy remains
    // authoritative as the parsing default; explicit flags must still match it.
    fn parse_saved(spec: &crate::session::SessionSpec) -> Result<Option<Self>, String> {
        // Published legacy harness records used --agent without any chat input
        // option. An explicit non-default chat recipient could only be saved
        // with a separate, background-capable input. Never reinterpret the
        // legacy argv as permission to send chat or to launch a harness.
        let mut args = spec.args.iter();
        let mut explicit_input = false;
        let mut explicit_worker = false;
        let mut ambiguous_agent = false;
        while let Some(arg) = args.next() {
            if arg == "--" {
                break;
            }
            match arg.as_str() {
                "--input" => {
                    explicit_input = true;
                    args.next();
                }
                "--worker" => {
                    explicit_worker = true;
                    args.next();
                }
                value if value.starts_with("--worker=") => explicit_worker = true,
                "--agent" => {
                    ambiguous_agent |= args.next().is_some_and(|value| value != "next-caller");
                }
                value if value.starts_with("--agent=") => {
                    ambiguous_agent |= value != "--agent=next-caller";
                }
                value if connection_option_takes_value(value) => {
                    args.next();
                }
                _ => {}
            }
        }
        if !explicit_input && ambiguous_agent && !explicit_worker {
            return Err("saved --agent has no explicit chat input and may select a legacy harness; recreate the session with --load-harness for protocol startup or configure a supported saved chat input and recipient".into());
        }
        let mut parsed = Self::parse_with_policy(
            spec.args.iter().map(OsString::from),
            None,
            spec.watchdog,
            true,
            false,
        )?;
        if parsed
            .as_ref()
            .is_some_and(|options| options.worker_window_child)
        {
            return Err(
                "the internal worker window marker cannot be saved as a background session".into(),
            );
        }
        if let Some(options) = &mut parsed
            && options.worker.is_some()
        {
            // A saved worker has no foreground monitor or chat input.
            options.chat.input = chat_io::Input::None;
        }
        Ok(parsed)
    }

    fn parse_with_policy(
        args: impl IntoIterator<Item = OsString>,
        default_destination: Option<&str>,
        default_watchdog: bool,
        legacy_destination: bool,
        new_connection: bool,
    ) -> Result<Option<Self>, String> {
        let mut args = args.into_iter().peekable();
        if args.peek().is_some_and(|arg| arg == "resume") {
            return Err(
                "conversation resume is no longer available; start ssmcp for a live session".into(),
            );
        }
        let mut local_mode = args.peek().is_some_and(|arg| arg == "local");
        if local_mode {
            args.next();
        }
        #[cfg(not(target_os = "linux"))]
        if local_mode {
            return Err("ssmcp local currently requires Linux".into());
        }
        #[cfg(target_os = "linux")]
        let (mut listen, mut public_url, mut socket) = (None, None, None);
        let mut destination = None;
        let mut server = None;
        let mut context = None;
        let mut username = None;
        let mut no_browser = false;
        let mut account_compat = false;
        let mut ssh = false;
        let mut name = None;
        let mut title = None;
        let mut allow = Vec::new();
        let mut context_roots = Vec::new();
        let mut chat = chat_io::Flags::default();
        let mut worker_count = None;
        let mut worker_factor = None;
        let mut worker_window_child = false;
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
        let mut harness_name = None;
        let mut harness_arguments = Vec::new();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some(WORKER_WINDOW_CHILD_FLAG) => {
                    if std::mem::replace(&mut worker_window_child, true) {
                        return Err(
                            "the internal worker window marker may be supplied only once".into(),
                        );
                    }
                }
                Some("--worker" | "--worker-k") => {
                    let name = arg.to_str().expect("matched UTF-8 worker flag");
                    let value = args
                        .next()
                        .ok_or_else(|| format!("{name} requires a value"))?
                        .into_string()
                        .map_err(|_| format!("{name} must be UTF-8"))?;
                    let target = if name == "--worker" {
                        &mut worker_count
                    } else {
                        &mut worker_factor
                    };
                    if target.replace(value).is_some() {
                        return Err(format!("{name} may be supplied only once"));
                    }
                }
                Some(value)
                    if value.starts_with("--worker=") || value.starts_with("--worker-k=") =>
                {
                    let (name, value) = value.split_once('=').expect("worker equals flag");
                    let target = if name == "--worker" {
                        &mut worker_count
                    } else {
                        &mut worker_factor
                    };
                    if target.replace(value.to_owned()).is_some() {
                        return Err(format!("{name} may be supplied only once"));
                    }
                }
                Some("--load-harness") => {
                    if harness_name.is_some() {
                        return Err("--load-harness may be supplied only once".into());
                    }
                    harness_name = Some(
                        args.next()
                            .ok_or(
                                "--load-harness requires a provider; use ssmcp --list-harnesses",
                            )?
                            .into_string()
                            .map_err(|_| "--load-harness must be UTF-8")?,
                    );
                }
                Some(value) if value.starts_with("--load-harness=") => {
                    if harness_name
                        .replace(value["--load-harness=".len()..].to_owned())
                        .is_some()
                    {
                        return Err("--load-harness may be supplied only once".into());
                    }
                }
                Some("--") => {
                    if harness_name.is_none() {
                        return Err("arguments after -- require an explicit --load-harness".into());
                    }
                    harness_arguments.extend(args.by_ref());
                    break;
                }
                Some(value) if value.starts_with("--agent=") => {
                    chat.set("--agent", OsString::from(&value["--agent=".len()..]))?;
                }
                Some("-h" | "--help") => {
                    #[cfg(target_os = "linux")]
                    if local_mode {
                        print!("{}", local::HELP);
                        return Ok(None);
                    }
                    print!("{HELP}");
                    return Ok(None);
                }
                Some("--help-full") => {
                    print!("{HELP_FULL}");
                    return Ok(None);
                }
                Some("--all") => return Err("--all requires a top-level --version request".into()),
                Some("-V" | "--version") => {
                    return Err("use --version alone or with --all at the top level".into());
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
                #[cfg(target_os = "linux")]
                Some("--socket") => {
                    if socket.is_some() {
                        return Err("--socket may be supplied only once".into());
                    }
                    socket = Some(
                        args.next_if(|value| !value.to_string_lossy().starts_with('-'))
                            .map(std::path::PathBuf::from),
                    );
                }
                #[cfg(target_os = "linux")]
                Some(value) if value.starts_with("--socket=") => {
                    if socket.is_some() {
                        return Err("--socket may be supplied only once".into());
                    }
                    socket = Some(Some(std::path::PathBuf::from(&value["--socket=".len()..])));
                }
                #[cfg(target_os = "linux")]
                Some("--listen") => {
                    let value = args.next().ok_or("--listen requires IP:PORT")?;
                    listen = Some(value.into_string().map_err(|_| "--listen must be UTF-8")?);
                }
                #[cfg(target_os = "linux")]
                Some("--public-url") => {
                    let value = args
                        .next()
                        .ok_or("--public-url requires https://NAME.local")?;
                    public_url = Some(
                        value
                            .into_string()
                            .map_err(|_| "--public-url must be UTF-8")?,
                    );
                }
                Some("--no-browser") => no_browser = true,
                Some("--account-compat") => account_compat = true,
                Some("--username") => {
                    let value = args.next().ok_or("--username requires a name")?;
                    username = Some(crate::username::normalize(
                        value.to_str().ok_or("--username must be UTF-8")?,
                    )?);
                }
                Some("--server") => {
                    if server.is_some() {
                        return Err("--server may be supplied only once".into());
                    }
                    let value = args
                        .next()
                        .ok_or("--server requires an HTTPS origin")?
                        .into_string()
                        .map_err(|_| "--server must be UTF-8")?;
                    server = Some(account::normalize_server(&value).map_err(|e| e.to_string())?);
                }
                Some("--context") => {
                    let value = args.next().ok_or("--context requires a name")?;
                    connection_context::set_name(
                        &mut context,
                        value.to_str().ok_or("--context must be UTF-8")?,
                    )
                    .map_err(|e| e.to_string())?;
                }
                Some(value) if value.starts_with("--context=") => {
                    connection_context::set_name(&mut context, &value["--context=".len()..])
                        .map_err(|e| e.to_string())?;
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
                Some(flag) if chat_io::takes_value(flag) => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("{flag} requires a value"))?;
                    if flag == "--agent" {
                        match value.to_str() {
                            Some("-h" | "--help") => {
                                print!("{HELP}");
                                return Ok(None);
                            }
                            Some("--help-full") => {
                                print!("{HELP_FULL}");
                                return Ok(None);
                            }
                            _ => {}
                        }
                    }
                    chat.set(flag, value)?;
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
        let worker = match worker_count {
            Some(count) => Some(crate::worker::Config::parse(
                &count,
                worker_factor.as_deref(),
            )?),
            None if worker_factor.is_some() => return Err("--worker-k requires --worker N".into()),
            None => None,
        };
        let mut chat = if worker.is_some() {
            chat.finish_for_worker(true)?
        } else {
            chat.finish()?
        };
        #[cfg(not(target_os = "linux"))]
        if worker.is_some() && chat.agent != chat_io::Agent::Remote {
            return Err("local worker executors require Linux cgroup v2 and the user's systemd manager; omit --agent to use the remote task pool".into());
        }
        if worker.is_some()
            && (list_tools
                || harness_name.is_some()
                || !matches!(chat.input, chat_io::Input::Terminal | chat_io::Input::None))
        {
            return Err("--worker opens chat and a separate worker window; omit --list-tools, --load-harness and external chat inputs".into());
        }
        if worker.is_some() && daemon {
            chat.input = chat_io::Input::None;
        }
        if worker_window_child
            && (worker.is_none() || daemon || chat.input != chat_io::Input::Terminal)
        {
            return Err(
                "the internal worker window marker requires a foreground --worker monitor".into(),
            );
        }
        if list_tools && !chat.arguments().is_empty() {
            return Err("--list-tools cannot configure chat input or an agent".into());
        }
        let harness = harness_name
            .as_deref()
            .map(|name| agent::Selection::new(name, harness_arguments))
            .transpose()?;
        if context.is_some() && !new_connection {
            return Err("saved sessions must record a concrete server, not --context".into());
        }
        if context.is_some() && list_tools {
            return Err("--list-tools does not select a connection context".into());
        }
        if new_connection && !list_tools {
            let explicit_target = local_mode
                || server.is_some()
                || destination.is_some()
                || ssh
                || identity.is_some()
                || port.is_some()
                || config.is_some();
            if let Some(selection) =
                connection_context::resolve(context.as_deref(), explicit_target)
                    .map_err(|e| e.to_string())?
            {
                match selection.endpoint {
                    connection_context::Endpoint::Local => local_mode = true,
                    connection_context::Endpoint::Https(origin) => server = Some(origin),
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        if local_mode {
            return Err("ssmcp local currently requires Linux".into());
        }
        #[cfg(target_os = "linux")]
        let mut local = local_mode.then(local::Settings::default);
        #[cfg(target_os = "linux")]
        {
            if let Some(path) = socket {
                local
                    .as_mut()
                    .ok_or("--socket is only available with a local connection")?
                    .set_socket(path)?;
            }
            if let Some(value) = listen {
                local
                    .as_mut()
                    .ok_or("--listen is only available with a local connection")?
                    .set_listen(&value)?;
            }
            if let Some(value) = public_url {
                local
                    .as_mut()
                    .ok_or("--public-url is only available with a local connection")?
                    .set_public_url(&value)?;
            }
        }
        if local_mode
            && (destination.is_some()
                || server.is_some()
                || username.is_some()
                || ssh
                || identity.is_some()
                || port.is_some()
                || config.is_some()
                || !allow.is_empty()
                || daemon
                || session_control
                || list_tools)
        {
            return Err("ssmcp local is a foreground OS-user session; omit destinations, account/SSH options, --allow, --daemon, --session-control and --list-tools".into());
        }
        #[cfg(target_os = "linux")]
        if let Some(settings) = &local {
            settings.validate()?;
        }
        if !local_mode && destination.is_none() && server.is_none() && !list_tools {
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
        if new_connection && !ssh && destination.is_some() {
            return Err("unrecognized command or destination; see ssmcp --help. For tool sources use ssmcp tools [status|check|update]; for a connection use ssmcp username@server or ssmcp --server https://server".into());
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
        if account_compat
            && (allow.len() != 1
                || username.is_some()
                || destination.is_some()
                || ssh
                || local_mode
                || list_tools)
        {
            return Err("--account-compat requires exactly one --allow USER@SERVER and no username, SSH destination, local mode or --list-tools".into());
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
            #[cfg(target_os = "linux")]
            local,
            destination,
            server,
            username,
            no_browser,
            account_compat,
            relay: None,
            temporary: false,
            name,
            title,
            allow,
            context_roots,
            chat,
            worker,
            worker_window_child,
            worker_window: None,
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
            harness,
        }))
    }

    fn is_local(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.local.is_some()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
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

    /// Only a freshly verified saved account may use the old self-only wire shape.
    /// The operator's allow policy remains unchanged in saved settings and output.
    fn registration_metadata(&self) -> crate::Result<ClientMetadata> {
        let mut metadata = self.metadata()?;
        if self.account_compat {
            let credential = self
                .relay
                .as_ref()
                .ok_or("--account-compat requires a verified saved account before connecting")?;
            let handle = credential
                .username
                .as_deref()
                .ok_or("--account-compat requires the account's registered relay username")?;
            if credential.subject.is_none()
                || self.server.as_deref() != Some(credential.server.as_str())
                || self.allow.as_slice()
                    != [crate::server_endpoint::allow_principal(
                        handle,
                        &credential.server,
                    )?]
            {
                return Err(
                    "--account-compat account binding does not match the allow policy".into(),
                );
            }
            metadata.allow.clear();
        }
        Ok(metadata)
    }

    fn worker_window_arguments(&self, args: &[OsString]) -> Option<Vec<OsString>> {
        if self.worker_window_child
            || self.daemon
            || self.list_tools
            || self.chat.input != chat_io::Input::Terminal
        {
            return None;
        }
        let worker = self.worker?;
        worker_arguments(args, worker.count, true, false).ok()
    }

    fn become_worker_controller(&mut self) {
        self.worker = None;
        // Executor flags belong only to the child; the controller uses ordinary chat.
        self.chat = chat_io::Settings::default();
    }

    /// Store only validated connection settings. Service stdout/stderr have a
    /// fixed non-interactive format, so display themes are deliberately omitted.
    fn session_spec(&self) -> crate::Result<crate::session::SessionSpec> {
        if self.is_local() {
            return Err(
                "local sessions currently run in the foreground; keep this terminal open".into(),
            );
        }
        let metadata = self.metadata()?;
        let mut args = vec![OsString::from("--name"), OsString::from(&metadata.name)];
        args.extend_from_slice(self.chat.arguments());
        if let Some(worker) = self.worker {
            args.extend([
                OsString::from("--worker"),
                OsString::from(worker.count.to_string()),
                OsString::from("--worker-k"),
                OsString::from(worker.factor.to_string()),
            ]);
        }
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
        // A signed-in account re-derives its self-allow from the saved credential.
        if self.relay.is_none() || self.account_compat {
            for principal in &self.allow {
                args.extend([OsString::from("--allow"), OsString::from(principal)]);
            }
        }
        if self.account_compat {
            args.push(OsString::from("--account-compat"));
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
        if let Some(harness) = &self.harness {
            harness.append_saved(&mut args);
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
        if arg == "--" {
            break;
        }
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
    if options.worker.is_some() && options.chat.input != chat_io::Input::None {
        return Err("saved worker pools must disable chat input internally".into());
    }
    options.chat.validate_saved_input()?;
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

/// Read the chat mode of an already validated saved session without opening
/// context roots or resolving the session's credentials.
pub(crate) fn saved_chat_enabled(spec: &crate::session::SessionSpec) -> bool {
    let mut args = spec.args.iter();
    let mut enabled = true;
    let mut worker = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--" => break,
            "--input" => enabled = args.next().is_none_or(|value| value != "none"),
            "--worker" => {
                worker = true;
                args.next();
            }
            value if value.starts_with("--worker=") => worker = true,
            arg if connection_option_takes_value(arg) => {
                args.next();
            }
            _ => {}
        }
    }
    enabled && !worker
}

/// Read display metadata without reopening optional context roots or transports.
pub(crate) fn session_title(spec: &crate::session::SessionSpec) -> crate::Result<Option<String>> {
    let mut args = spec.args.iter();
    let mut title = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
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
    if !matches!(
        options.chat.input,
        chat_io::Input::Terminal | chat_io::Input::None
    ) || options.chat.agent != chat_io::Agent::NextCaller
    {
        return Err(
            "remote child sessions cannot inherit a privately authorized chat input or agent"
                .into(),
        );
    }
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
            Some("--") => {
                connection.push(arg);
                connection.extend(args);
                break;
            }
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
    let Some(mut options) = Options::parse_new(connection)? else {
        return Ok(());
    };
    if options.destination.is_some() {
        return Err("Use ssmcp sessions create NAME username@server for an OAuth session".into());
    }
    options.chat.validate_saved_input()?;
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

const MAX_WORKER_TOTAL: usize = 128;

fn worker_total_after_delta(total: usize, delta: i32) -> Result<usize, String> {
    total
        .checked_add_signed(delta as isize)
        .filter(|total| *total <= MAX_WORKER_TOTAL)
        .ok_or_else(|| format!("Worker target must stay between 0 and {MAX_WORKER_TOTAL}."))
}

fn worker_arguments(
    arguments: &[OsString],
    total: usize,
    child: bool,
    reset_chat: bool,
) -> Result<Vec<OsString>, String> {
    if total > MAX_WORKER_TOTAL || (child && total == 0) {
        return Err("invalid worker window size".into());
    }
    let mut result = Vec::with_capacity(arguments.len() + 3);
    let mut tail = Vec::new();
    let mut found = false;
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        match argument.to_str() {
            Some("--") => {
                tail.push(argument.clone());
                tail.extend(arguments.cloned());
                break;
            }
            Some("--worker") => {
                arguments.next().ok_or("--worker requires a value")?;
                found = true;
                if total != 0 {
                    result.extend([argument.clone(), OsString::from(total.to_string())]);
                }
            }
            Some(value) if value.starts_with("--worker=") => {
                found = true;
                if total != 0 {
                    result.push(OsString::from(format!("--worker={total}")));
                }
            }
            Some("--worker-k") => {
                let value = arguments.next().ok_or("--worker-k requires a value")?;
                if total != 0 {
                    result.extend([argument.clone(), value.clone()]);
                }
            }
            Some(value) if value.starts_with("--worker-k=") => {
                if total != 0 {
                    result.push(argument.clone());
                }
            }
            Some(WORKER_WINDOW_CHILD_FLAG) => {}
            Some(value) if reset_chat && chat_io::takes_value(value) => {
                arguments.next().ok_or("chat option requires a value")?;
            }
            Some(value)
                if reset_chat
                    && value
                        .split_once('=')
                        .is_some_and(|(name, _)| chat_io::takes_value(name)) => {}
            Some(value) if connection_option_takes_value(value) => {
                result.push(argument.clone());
                result.push(
                    arguments
                        .next()
                        .ok_or("connection option requires a value")?
                        .clone(),
                );
            }
            _ => result.push(argument.clone()),
        }
    }
    if total != 0 && !found {
        result.extend([
            OsString::from("--worker"),
            OsString::from(total.to_string()),
        ]);
    }
    if child {
        result.push(OsString::from(WORKER_WINDOW_CHILD_FLAG));
    }
    result.extend(tail);
    Ok(result)
}

struct WorkerController {
    executable: PathBuf,
    arguments: Vec<OsString>,
    cwd: PathBuf,
    preserve_worker_agent: bool,
    desired: usize,
    window: Option<worker_window::WindowChild>,
    launching: Option<JoinHandle<Result<worker_window::WindowChild, String>>>,
    pending: Option<(u64, usize)>,
}

impl WorkerController {
    fn new(options: &Options, arguments: &[OsString]) -> crate::Result<Self> {
        Ok(Self {
            executable: std::env::current_exe()?,
            arguments: reload::connection_arguments(arguments.to_vec(), options, false)?,
            cwd: std::env::current_dir()?,
            preserve_worker_agent: options.worker.is_some(),
            desired: options.worker.map_or(0, |worker| worker.count),
            window: None,
            launching: None,
            pending: None,
        })
    }

    async fn poll(&mut self) {
        if self.launching.as_ref().is_some_and(JoinHandle::is_finished) {
            let launch = self
                .launching
                .take()
                .expect("finished worker window launch");
            match launch.await {
                Ok(Ok(window)) => {
                    self.window = Some(window);
                    terminal::notice("Worker window connected.");
                }
                result => {
                    let error = match result {
                        Ok(Err(error)) => error,
                        Err(error) => format!("worker window launch ended: {error}"),
                        Ok(Ok(_)) => unreachable!(),
                    };
                    self.desired = 0;
                    self.pending = None;
                    terminal::notice(&format!("Could not open worker window: {error}"));
                }
            }
        }
        let transition = self.window.as_ref().map(|window| {
            // Read closed first: its release follows the final reload message.
            // A concurrently arriving reload is handled now or on the next poll.
            let closed = window.poll_closed();
            (closed, window.take_reload_request())
        });
        match transition {
            Some((Ok(_), true)) => {
                // The child sent this only after its old runtime was cleaned up.
                // Drop its owned handle before opening exactly one replacement.
                self.window = None;
                self.pending = None;
                terminal::notice("Reloading worker window; this chat session stays connected.");
                // Re-check parent stop intent on the next UI tick before launch.
                return;
            }
            Some((Ok(true), false)) => {
                self.window = None;
                self.pending = None;
                self.desired = 0;
                terminal::notice("Worker window closed; this chat session remains available.");
            }
            Some((Err(error), _)) => {
                self.window = None;
                self.pending = None;
                self.desired = 0;
                terminal::notice(&format!(
                    "Worker window: {error}. This chat session remains available."
                ));
            }
            _ => {}
        }

        let snapshot = self
            .window
            .as_ref()
            .and_then(|_| worker_window::linked_snapshot());
        if let (Some((id, target)), Some(snapshot)) = (self.pending, snapshot.as_ref()) {
            let receipt = &snapshot["last_resize"];
            if receipt["request_id"].as_u64() == Some(id) {
                self.pending = None;
                if receipt["accepted"].as_bool() != Some(true) {
                    let actual = snapshot["desired_total"].as_u64().unwrap_or(0) as usize;
                    let later_delta = self.desired as i32 - target as i32;
                    self.desired = worker_total_after_delta(actual, later_delta).unwrap_or(actual);
                    let error = receipt["error"]
                        .as_str()
                        .unwrap_or("worker resize was rejected");
                    terminal::notice(&format!("Worker target unchanged: {error}"));
                }
            }
        }

        // A bounded batch keeps input responsive even if another producer is busy.
        let previous_target = self.desired;
        for _ in 0..8 {
            let Some(delta) = worker_control::take() else {
                break;
            };
            match worker_total_after_delta(self.desired, delta) {
                Ok(total) => self.desired = total,
                Err(error) => terminal::notice(&error),
            }
        }

        if self.desired != previous_target
            && snapshot.as_ref().is_some_and(|snapshot| {
                snapshot["retiring"]
                    .as_u64()
                    .is_some_and(|count| count != 0)
                    && snapshot["desired_total"].as_u64() != Some(self.desired as u64)
            })
        {
            terminal::notice("Worker target queued; waiting for retiring workers to leave.");
        }

        if self.window.is_none() && self.launching.is_none() && self.desired != 0 {
            let arguments = match worker_arguments(
                &self.arguments,
                self.desired,
                true,
                !self.preserve_worker_agent,
            ) {
                Ok(arguments) => arguments,
                Err(error) => {
                    self.desired = 0;
                    terminal::notice(&error);
                    return;
                }
            };
            let executable = self.executable.clone();
            let cwd = self.cwd.clone();
            self.launching = Some(tokio::task::spawn_blocking(move || {
                worker_window::launch(&executable, &arguments, &cwd)
            }));
            terminal::notice("Opening worker window.");
            return;
        }

        if let (Some(window), Some(snapshot)) = (self.window.as_ref(), snapshot.as_ref()) {
            let actual = snapshot["desired_total"]
                .as_u64()
                .map(|value| value as usize);
            let settled = snapshot["retiring"].as_u64() == Some(0)
                && snapshot["total"].as_u64() == snapshot["desired_total"].as_u64();
            if self.pending.is_none()
                && settled
                && snapshot["stopping"].as_bool() != Some(true)
                && snapshot["closed"].as_bool() != Some(true)
                && actual.is_some_and(|actual| actual != self.desired)
            {
                match window.request_resize(self.desired) {
                    Ok(id) => self.pending = Some((id, self.desired)),
                    Err(error) => {
                        self.desired = actual.unwrap_or(0);
                        terminal::notice(&format!("Could not change worker target: {error}"));
                    }
                }
            }
        }
    }
}

impl Drop for WorkerController {
    fn drop(&mut self) {
        if let Some(launch) = self.launching.take() {
            // A blocking launch that already started owns its eventual result;
            // dropping the cancelled handle drops that WindowChild on completion.
            launch.abort();
        }
    }
}

fn connection_option_takes_value(option: &str) -> bool {
    chat_io::takes_value(option)
        || matches!(
            option,
            "--name"
                | "--load-harness"
                | "--worker"
                | "--worker-k"
                | "--server"
                | "--context"
                | "--username"
                | "--identity"
                | "--title"
                | "--allow"
                | "--context-root"
                | "--listen"
                | "--public-url"
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
    if options.is_local() {
        return Ok(());
    }
    if options.list_tools {
        return Ok(());
    }
    let server = account::resolve_server(options.server.as_deref())?;
    for principal in &options.allow {
        crate::server_endpoint::allow_handle_for_server(principal, &server)?;
    }
    if options.account_compat {
        options.relay = Some(account::compat_credential(&server, &options.allow).await?);
        options.server = Some(server);
        return Ok(());
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

/// Bare/named account connections already carry account ownership and quota.
/// Only their exact pre-Ready legacy-format rejection may retry as verified self.
/// Explicit anonymous --allow has no credential here and can never enter this path.
async fn retry_legacy_self_account(
    options: &mut Options,
    rejection: &str,
    registered: bool,
) -> crate::Result<bool> {
    if registered
        || rejection != relay::LEGACY_ACCOUNT_ALLOW
        || options.account_compat
        || options.temporary
        || options.username.is_none()
    {
        return Ok(false);
    }
    let Some(previous) = options.relay.as_ref() else {
        return Ok(false);
    };
    let verified = account::compat_credential(&previous.server, &options.allow).await?;
    if verified.subject != previous.subject
        || verified.principal != previous.principal
        || verified.token != previous.token
    {
        return Err(
            "saved login changed during legacy account verification; start a new connection".into(),
        );
    }
    options.relay = Some(verified);
    options.account_compat = true;
    options.username = None;
    render::status(
        "account",
        "Notice",
        "Older relay: reconnecting once with the same verified account and self-only access.",
    );
    Ok(true)
}

fn tool_repository_arguments(args: &[OsString]) -> Option<&[OsString]> {
    let (command, arguments) = args.split_first()?;
    matches!(command.to_str(), Some("tool" | "tools")).then_some(arguments)
}

pub async fn run() -> crate::Result<()> {
    if let Some(result) = crate::worker::child_entry().await {
        return result;
    }
    let mut args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if let Some(request) = version::request(&args)? {
        print!("{}", version::render(request)?);
        return Ok(());
    }
    if let Some(output) = paths::output(&args)? {
        print!("{output}");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    if args.first().map(OsString::as_os_str) == Some(std::ffi::OsStr::new("__local-relay")) {
        let code = match local::relay().await {
            Ok(()) => 0,
            Err(error) => {
                report_error(&error.to_string());
                1
            }
        };
        // Like __relay, this transport-only process may have a blocking stdin
        // reader when the host closes first. It owns no tool processes.
        std::process::exit(code);
    }
    if args.first().map(OsString::as_os_str) == Some(std::ffi::OsStr::new("__tty-view")) {
        let args = &args[1..];
        if args.len() != 2 {
            return Err("terminal viewer requires endpoint directory and slot".into());
        }
        let slot = args[1]
            .to_str()
            .and_then(|value| value.parse().ok())
            .ok_or("invalid observer slot")?;
        return crate::observer::viewer(std::path::Path::new(&args[0]), slot).await;
    }
    if args.first().map(OsString::as_os_str) == Some(std::ffi::OsStr::new("__relay")) {
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
    if args.first().map(OsString::as_os_str) == Some(std::ffi::OsStr::new("__rsync-transport")) {
        return crate::rsync::transport_main().await;
    }
    if agent::dispatch(&args)? {
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "cleanup") {
        let arguments = args
            .iter()
            .map(|arg| {
                arg.to_str()
                    .map(str::to_owned)
                    .ok_or("cleanup arguments must be valid UTF-8")
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(output) =
            crate::managed_files::command::run(&arguments, std::path::Path::new("."))?
        {
            println!("{output}");
            return Ok(());
        }
    }
    if let Some(arguments) = tool_repository_arguments(&args) {
        return crate::tool_repository::run(arguments.to_vec()).await;
    }
    if args.first().is_some_and(|arg| arg == "update") {
        return update::run(args.into_iter().skip(1).collect()).await;
    }
    if args.first().is_some_and(|arg| arg == "gui") {
        return crate::session::gui::run(args.into_iter().skip(1).collect()).await;
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
    connection_context::normalize_args(&mut args)?;
    config::normalize_args(&mut args)?;
    if computer_control::dispatch(&args).await? {
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "setup")
        && args.get(1).is_some_and(|arg| arg == "browser")
    {
        return Err(
            "Browser setup is retired; use the standalone Zuse CLI. See ssmcp computer --help"
                .into(),
        );
    }
    if connection_context::dispatch(&args)? || config::dispatch(&args)? || trust::dispatch(&args)? {
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
        Options::parse_new(args.clone())?
    };
    let Some(mut options) = parsed else {
        return Ok(());
    };
    let mut worker_session = if options.worker_window_child {
        Some(worker_window::child_session()?)
    } else {
        None
    };
    if service.is_some() || options.daemon {
        if options.worker.is_some() && options.chat.input != chat_io::Input::None {
            return Err("background worker pools must disable chat input internally".into());
        }
        options.chat.validate_saved_input()?;
    }
    if options.destination.is_some() {
        return Err("SSH device connections have been removed. Use ssmcp username@server or ssmcp login --server https://server".into());
    }
    if options.is_local() {
        if service.is_some() {
            return Err("local mode requires a foreground session".into());
        }
    } else {
        let interactive_login = service.is_none() && !options.chat.input.uses_stdio();
        apply_account(&mut options, interactive_login).await?;
    }
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
    let child_arguments = options.worker_window_arguments(&args);
    let interactive_dashboard = {
        use std::io::IsTerminal;
        ui.dashboard
            && ui.log_format != render::LogFormat::Jsonl
            && io::stdin().is_terminal()
            && io::stderr().is_terminal()
    };
    if child_arguments.is_some() && !interactive_dashboard {
        return Err("opening a worker window requires an interactive chat dashboard".into());
    }
    if interactive_dashboard
        && service.is_none()
        && !options.list_tools
        && !options.worker_window_child
        && options.chat.input == chat_io::Input::Terminal
    {
        let mut controller = WorkerController::new(&options, &args)?;
        if let Some(child_args) = child_arguments {
            controller.window = Some(worker_window::launch(
                &controller.executable,
                &child_args,
                &controller.cwd,
            )?);
            options.become_worker_controller();
            ui.layout = ui::Layout::Chat;
        }
        options.worker_window = Some(Mutex::new(controller));
    }
    interface::initialize_path(options.ui_config.as_deref());
    render::configure(ui.clone());
    if let Err(error) = context_memory::initialize() {
        render::status(
            "context",
            "Warning",
            &format!("Live context unavailable: {error}"),
        );
    }
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
            result = timeout(Duration::from_secs(120), Backend::start_with_harness(&options.shell, options.tty.as_deref(), options.harness.as_ref())) => result.map_err(|_| "local backend startup timed out")??,
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
            // Offline discovery lists this binary's native tools. It has no
            // relay, queued requests or public-await display to negotiate.
            runtime.configure_timing(true, false)?;
            runtime.configure_chat(options.chat.input != chat_io::Input::None)?;
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
            result = timeout(Duration::from_secs(180),async {
                if let Some(harness) = &options.harness { harness.activate(&backend).await?; }
                backend.wait_discovery().await;
                backend.tools().await
            }) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
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
        if options.worker_window_child && worker_window::child_stop_requested() {
            return Ok(());
        }
        if let Some(runner) = &service
            && runner.stop_requested()
        {
            runner.record("stopped", attempt, None, None, None)?;
            return Ok(());
        }
        if options.account_compat && attempt > 0 {
            // Re-check the same saved account before every reconnect. This path
            // never authorizes, claims a username or changes the local allow list.
            let previous = options
                .relay
                .clone()
                .ok_or("--account-compat lost its verified account; start a new connection")?;
            apply_account(&mut options, false).await?;
            let current = options
                .relay
                .as_ref()
                .ok_or("--account-compat lost its verified account; start a new connection")?;
            if current.subject != previous.subject
                || current.principal != previous.principal
                || current.token != previous.token
            {
                return Err(
                    "--account-compat saved login changed; start a new explicit connection".into(),
                );
            }
        }
        if options.relay.as_ref().is_some_and(|credential| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                >= credential.expires_at
        }) {
            if service.is_none()
                && !options.account_compat
                && !options.temporary
                && !options.chat.input.uses_stdio()
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
            Ok(ConnectionExit::Reload { mut restart }) => {
                // The ACK and broker receipt were sent before connect cleaned
                // up every old task. Stop wins over a reload arriving at exit.
                let stopped = tokio::select! {
                    biased;
                    _ = interrupt.recv() => true,
                    _ = terminate.recv() => true,
                    _ = std::future::ready(()) => service.as_ref().is_some_and(|runner| runner.stop_requested())
                        || (options.worker_window_child && worker_window::child_stop_requested()),
                };
                if stopped {
                    if let Some(runner) = &service {
                        runner.record("stopped", attempt, None, None, None)?;
                    }
                    return Ok(());
                }
                if options.worker_window_child {
                    // An attached child cannot re-use the one-shot parent
                    // handshake after exec. Its existing owner replaces it.
                    worker_window::child_request_reload()?;
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
                if let Some(controller) = &options.worker_window {
                    let controller = controller.lock().await;
                    restart
                        .set_worker_total(controller.desired, controller.preserve_worker_agent)?;
                }
                // exec does not run Rust destructors; stop this controller's child first.
                drop(options.worker_window.take());
                drop(worker_session.take());
                return restart.execute();
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
        if let Some(message) = error.as_deref()
            && retry_legacy_self_account(&mut options, message, registered).await?
        {
            continue;
        }
        let permanent = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<relay::PermanentFailure>().is_some());
        if permanent
            && error.as_deref() == Some(relay::AUTH_EXPIRED)
            && service.is_none()
            && !options.account_compat
            && !options.temporary
            && !options.chat.input.uses_stdio()
            && options.username.is_some()
            && !reauthenticated
        {
            reauthenticated = true;
            options.allow.clear();
            apply_account(&mut options, true).await?;
            continue;
        }
        if !watchdog_retries(options.watchdog, result.as_ref().err().map(Box::as_ref)) {
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
            _ = worker_parent_stopped(&options) => return Ok(()),
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

async fn worker_parent_stopped(options: &Options) {
    if !options.worker_window_child {
        std::future::pending::<()>().await;
    }
    while !worker_window::child_stop_requested() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn worker_monitor_closed() -> crate::Error {
    Box::new(relay::PermanentFailure(
        "worker monitor closed; stopping its owned execution slots".into(),
    ))
}

pub(crate) fn watchdog_retries(
    enabled: bool,
    error: Option<&(dyn std::error::Error + Send + Sync + 'static)>,
) -> bool {
    enabled
        && error.is_none_or(|error| {
            !error.is::<UnsupportedWireProtocol>() && !error.is::<relay::PermanentFailure>()
        })
}

enum ConnectionExit {
    Stopped,
    Disconnected,
    Reload { restart: reload::Restart },
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

struct RegisteredTransport {
    process: Child,
    transport_kind: TransportKind,
    stderr_pump: Option<TransportStderr>,
    writer: tokio::process::ChildStdin,
    reader: BufReader<ChildStdout>,
    ready: (String, String, u64, String),
    access_supported: bool,
    messaging_supported: bool,
    chat_disabled_acknowledged: bool,
    time_observation_supported: bool,
    wait_display_supported: bool,
    openai_context_supported: bool,
    peer_ports: Option<&'static str>,
    version_notice: Option<crate::wire::versions::Notice>,
}

impl RegisteredTransport {
    async fn require_allow_access(&mut self, allow: &[String]) -> crate::Result<()> {
        if !self.access_supported && !allow.is_empty() {
            stop_registration(
                &mut self.process,
                &mut self.stderr_pump,
                self.transport_kind,
            )
            .await?;
            return Err(Box::new(relay::PermanentFailure(
                "server did not acknowledge account access controls; update ssmcpd before using --allow".into()
            )));
        }
        Ok(())
    }
}

// A replacement transport must not start until the rejected transport process and its
// reader task are gone. No local backend or input binding exists at this stage.
async fn stop_registration(
    process: &mut Child,
    stderr_pump: &mut Option<TransportStderr>,
    transport_kind: TransportKind,
) -> crate::Result<()> {
    let _ = process.start_kill();
    let reaped = timeout(Duration::from_secs(10), process.wait()).await;
    if let Some(pump) = stderr_pump {
        pump.0.abort();
        let _ = (&mut pump.0).await;
    }
    reaped.map_err(|_| {
        format!(
            "{} process did not terminate before registration cleanup",
            transport_kind.label()
        )
    })??;
    Ok(())
}

async fn register_transport(
    transport_kind: TransportKind,
    metadata: &ClientMetadata,
    account_compat: bool,
    registered: &mut bool,
    mut command: impl FnMut() -> Command,
    cancelled: impl std::future::Future<Output = ()>,
    deadline: Duration,
) -> crate::Result<Option<RegisteredTransport>> {
    tokio::pin!(cancelled);
    tokio::select! {
        biased;
        _ = &mut cancelled => return Ok(None),
        _ = std::future::ready(()) => {},
    }
    let mut transport_command = command();
    let timing_offer = relay::timing_offer(&transport_command);
    let peer_offer = relay::peer_ports_offer(&transport_command);
    let openai_context_offer = relay::openai_context_offer(&transport_command);
    let mut process = transport_command.spawn().map_err(|error| {
        format!(
            "could not start {} process: {error}",
            transport_kind.label()
        )
    })?;
    let mut stderr_pump = process.stderr.take().map(|mut stderr| {
        TransportStderr(tokio::spawn(async move {
            let mut bytes = [0u8; 4096];
            while let Ok(count) = stderr.read(&mut bytes).await {
                if count == 0 {
                    break;
                }
                render::event(
                    transport_kind.event_source(),
                    &json!({"stderr":String::from_utf8_lossy(&bytes[..count])}),
                );
            }
        }))
    });
    let mut writer = process
        .stdin
        .take()
        .ok_or_else(|| format!("{} stdin unavailable", transport_kind.label()))?;
    let mut reader = BufReader::new(
        process
            .stdout
            .take()
            .ok_or_else(|| format!("{} stdout unavailable", transport_kind.label()))?,
    );
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
        stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
        return Err(error.into());
    }
    let response = tokio::select! {
        result = timeout(deadline, read_frame(&mut reader)) => {
            result.map_err(|_| transport_kind.registration_timeout())
                .and_then(|frame| frame)
        },
        _ = &mut cancelled => {
            stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
            return Ok(None);
        },
    };
    let response = match response {
        Ok(frame) => frame,
        Err(error) => {
            stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
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
            version_notice,
        }) => (
            (url, login_code, login_expires_in, client_id),
            capabilities
                .iter()
                .any(|capability| capability == crate::wire::SESSION_ACCESS_CAPABILITY),
            capabilities.iter().all(|capability| {
                matches!(
                    capability.as_str(),
                    crate::wire::CONTROL_ATTRIBUTION_CAPABILITY
                        | crate::wire::CHAT_DISABLED_CAPABILITY
                )
            }),
            capabilities
                .iter()
                .any(|capability| capability == crate::wire::messages::CAPABILITY),
            version_notice.filter(|notice| {
                capabilities
                    .iter()
                    .any(|capability| capability == crate::wire::versions::CAPABILITY)
                    && notice.valid()
            }),
            relay::negotiated_timing(timing_offer, &capabilities).0,
            relay::negotiated_timing(timing_offer, &capabilities).1,
            relay::negotiated_peer_ports(peer_offer, &capabilities),
            openai_context_offer
                && capabilities
                    .iter()
                    .any(|capability| capability == crate::wire::openai::CAPABILITY),
            capabilities
                .iter()
                .any(|capability| capability == crate::wire::CHAT_DISABLED_CAPABILITY),
        ),
        Some(Frame::Reject { message }) => {
            stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
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
            stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
            return Err(format!(
                "{} disconnected before registration; check the connection and preceding {} error",
                transport_kind.label(),
                transport_kind.event_source()
            )
            .into());
        }
        _ => {
            stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
            return Err(format!(
                "{} received an unexpected registration response",
                transport_kind.label()
            )
            .into());
        }
    };
    *registered = true;
    let (
        ready,
        access_supported,
        self_only_capabilities,
        messaging_supported,
        version_notice,
        time_observation_supported,
        wait_display_supported,
        peer_ports,
        openai_context_supported,
        chat_disabled_acknowledged,
    ) = ready;
    if account_compat
        && (!metadata.allow.is_empty()
            || !ready.1.is_empty()
            || ready.2 != 0
            || !self_only_capabilities)
    {
        stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
        return Err(Box::new(relay::PermanentFailure(
            "server did not acknowledge self-only account compatibility; no login code or access delegation is permitted".into(),
        )));
    }
    let github_auth = account_compat || !metadata.allow.is_empty();
    if github_auth && (!ready.1.is_empty() || ready.2 != 0) {
        stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
        return Err("server did not acknowledge the explicit account allow policy; upgrade ssmcpd before connecting".into());
    }
    if !github_auth && (ready.1.is_empty() || ready.2 == 0) {
        stop_registration(&mut process, &mut stderr_pump, transport_kind).await?;
        return Err("server returned no account access policy for this connection; use --allow username@relay-server".into());
    }
    Ok(Some(RegisteredTransport {
        process,
        transport_kind,
        stderr_pump,
        writer,
        reader,
        ready,
        access_supported,
        messaging_supported,
        chat_disabled_acknowledged,
        time_observation_supported,
        wait_display_supported,
        peer_ports,
        openai_context_supported,
        version_notice,
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
    let command = if let crate::wire::SteeringCommand::Publish { text, .. } = &command
        && steering::needs_preparation(id)
    {
        let prepared = match timeout(Duration::from_secs(5), backend.prepare_references(text)).await {
            Ok(Ok(prepared)) => steering::prepare_publish(id, prepared),
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
                    Some(Ok(Some(Frame::MessagesReply { id, result }))) => messaging::reply(id, result),
                    Some(Ok(Some(Frame::AccessReply { id, result }))) => access::reply(id, result),
                    Some(Ok(Some(Frame::CodeModeFinished { completion }))) => backend.code_mode_finished(completion)?,
                    Some(Ok(Some(Frame::WaitDisplay { snapshot }))) => backend.wait_display(snapshot)?,
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

enum Transport<'a> {
    Account {
        server: &'a str,
        credential: Option<&'a account::Credential>,
    },
    #[cfg(target_os = "linux")]
    Local(&'a local::Host),
}

impl Transport<'_> {
    fn kind(&self) -> TransportKind {
        match self {
            Self::Account { .. } => TransportKind::Relay,
            #[cfg(target_os = "linux")]
            Self::Local(_) => TransportKind::Local,
        }
    }

    fn timing_support(&self, acknowledged: (bool, bool)) -> (bool, bool) {
        match self {
            Self::Account { .. } => acknowledged,
            // The private host and native runtime are compiled into this same
            // process, including the passive time-only delivery policy. Local
            // OAuth still has no account identity for public typed awaits.
            #[cfg(target_os = "linux")]
            Self::Local(_) => (true, false),
        }
    }

    fn command(&self, executable: &std::path::Path) -> Command {
        match self {
            Self::Account { server, credential } => relay::command(executable, server, *credential),
            #[cfg(target_os = "linux")]
            Self::Local(host) => host.command(executable),
        }
    }

    fn report_connecting(&self) {
        match self {
            Self::Account { server, .. } => render::status(
                "https",
                "Progress",
                &format!("Connecting to {server} with an explicit account allow list"),
            ),
            #[cfg(target_os = "linux")]
            Self::Local(host) => {
                render::status(
                    "local",
                    "Progress",
                    &format!(
                        "Starting local MCP at {} with private OS-user transport",
                        host.origin
                    ),
                );
                if let Some(path) = &host.socket {
                    render::status(
                        "socket",
                        "Info",
                        &format!("MCP HTTP socket: {}", path.display()),
                    );
                }
            }
        }
    }

    fn start_display(
        &self,
        ui: &ui::Ui,
        cwd: &str,
        title: Option<&str>,
        url: &str,
        worker: bool,
    ) -> io::Result<Option<terminal::Session>> {
        if worker {
            return terminal::Session::start_worker(
                ui,
                cwd,
                title,
                Some(url),
                matches!(self, Self::Account { .. }),
            );
        }
        match self {
            Self::Account { .. } => terminal::Session::start(ui, cwd, title, Some(url)),
            #[cfg(target_os = "linux")]
            Self::Local(_) => terminal::Session::start_local(ui, cwd, title, Some(url)),
        }
    }
}

async fn connect(
    options: &Options,
    ui: &ui::Ui,
    service: Option<&crate::session::Runner>,
    attachment: Option<&crate::session::attach::Handle>,
    attempt: u64,
    registered: &mut bool,
) -> crate::Result<ConnectionExit> {
    #[cfg(target_os = "linux")]
    if let Some(settings) = &options.local {
        let host = local::Host::start(
            settings,
            options.worker.is_some() || options.chat.input == chat_io::Input::None,
        )
        .await?;
        let result = connect_inner(
            options,
            ui,
            service,
            attachment,
            attempt,
            registered,
            Transport::Local(&host),
        )
        .await;
        // Complete cleanup before /reload can replace this process or the
        // watchdog begins a new connection with a new one-use authorization.
        host.shutdown().await;
        return result;
    }
    let server = options
        .server
        .as_deref()
        .ok_or("a relay server is required")?;
    connect_inner(
        options,
        ui,
        service,
        attachment,
        attempt,
        registered,
        Transport::Account {
            server,
            credential: options.relay.as_ref(),
        },
    )
    .await
}

/// Keep an oversized native result off the wire without losing a host reminder.
/// The callback only cancels the reservation carried by this rejected reply.
pub(crate) fn bounded_native_reply(
    result: Result<serde_json::Value, String>,
    restore_reminder: impl FnOnce(&serde_json::Value),
) -> Result<serde_json::Value, String> {
    if serde_json::to_vec(&result).map_or(true, |bytes| bytes.len() + 256 > MAX_FRAME) {
        if let Ok(value) = &result
            && let Some(reminder) = value
                .get("_meta")
                .and_then(|meta| meta.get(interface::tool_summary::META_KEY))
        {
            restore_reminder(reminder);
        }
        Err("native call finished but its result exceeds the 4 MiB bridge frame limit; do not blindly repeat a mutating call, inspect native state or request a smaller output page".into())
    } else {
        result
    }
}

async fn join_managed_file_cleanup(task: &mut JoinHandle<()>) {
    if let Err(error) = task.await {
        render::status(
            "cleanup",
            "Warning",
            &format!("Managed-file cleanup task failed: {error}"),
        );
    }
}

async fn periodic_managed_file_cleanup(
    task: &mut JoinHandle<()>,
    start: impl FnOnce() -> JoinHandle<()>,
) {
    // A slow scan owns its slot until it has finished and been joined.
    if task.is_finished() {
        join_managed_file_cleanup(task).await;
        *task = start();
    }
}

async fn connect_inner(
    options: &Options,
    ui: &ui::Ui,
    service: Option<&crate::session::Runner>,
    attachment: Option<&crate::session::attach::Handle>,
    attempt: u64,
    registered: &mut bool,
    transport: Transport<'_>,
) -> crate::Result<ConnectionExit> {
    let management = session_control(options, service)?;
    let mut interrupt = interrupt_signal()?;
    let mut terminate = {
        let signal = terminate_signal()?;
        #[cfg(all(test, unix))]
        {
            output_expiry_fixture::StopSignal::new(signal)
        }
        #[cfg(not(all(test, unix)))]
        {
            signal
        }
    };
    let restart = reload::Restart::capture(options, service.is_some())?;
    let metadata = options.metadata()?;
    let wire_metadata = options.registration_metadata()?;
    let executable = std::env::current_exe()?;
    #[cfg(all(test, unix))]
    let executable = output_expiry_fixture::transport_executable(executable)?;
    transport.report_connecting();
    if options.account_compat {
        render::status(
            "account",
            "Notice",
            &format!(
                "Authenticated compatibility: {} owns this connection, including account management and audit visibility; its account quota applies. Access-list editing is disabled.",
                options.allow[0]
            ),
        );
    }
    let cancelled = async {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
            _ = worker_parent_stopped(options) => {},
        }
    };
    let mut metric_channel = None;
    let chat_enabled = options.worker.is_none() && options.chat.input != chat_io::Input::None;
    let registration = register_transport(
        transport.kind(),
        &wire_metadata,
        options.account_compat,
        registered,
        || {
            let mut command = transport.command(&executable);
            if options.account_compat {
                relay::restrict_to_self(&mut command);
            }
            if !chat_enabled && matches!(&transport, Transport::Account { .. }) {
                relay::offer_chat_disabled(&mut command);
            }
            if matches!(&transport, Transport::Account { .. })
                && ui.dashboard
                && ui.log_format != render::LogFormat::Jsonl
            {
                metric_channel = link_metrics::prepare(&mut command);
            }
            command
        },
        cancelled,
        Duration::from_secs(120),
    )
    .await?;
    let Some(mut registration) = registration else {
        return Ok(ConnectionExit::Stopped);
    };
    registration
        .require_allow_access(&wire_metadata.allow)
        .await?;
    if chat_enabled == registration.chat_disabled_acknowledged {
        stop_registration(
            &mut registration.process,
            &mut registration.stderr_pump,
            registration.transport_kind,
        )
        .await?;
        return Err(Box::new(relay::PermanentFailure(if chat_enabled {
            "relay disabled human chat without a matching local request".into()
        } else if options.worker.is_some() {
            "worker mode requires a relay that acknowledges disabled human chat".into()
        } else {
            "--input none requires a relay that acknowledges disabled human chat".into()
        })));
    }
    let RegisteredTransport {
        mut process,
        transport_kind,
        stderr_pump: _stderr_pump,
        writer,
        reader,
        ready: (url, login_code, login_expires_in, client_id),
        access_supported,
        messaging_supported,
        chat_disabled_acknowledged: _,
        time_observation_supported,
        wait_display_supported,
        peer_ports,
        openai_context_supported,
        version_notice,
    } = registration;
    let peer_ports = crate::peer_port::PeerPorts::from_env(peer_ports, &client_id)?;
    let (time_observation_supported, wait_display_supported) =
        transport.timing_support((time_observation_supported, wait_display_supported));
    let github_auth = !metadata.allow.is_empty();
    let login_received_at = std::time::Instant::now();
    if let Some(worker) = options.worker {
        crate::worker::prepare_display(worker);
    }
    let display_session = if options.chat.input == chat_io::Input::Terminal {
        match transport.start_display(
            ui,
            &metadata.cwd,
            metadata.title.as_deref(),
            &url,
            options.worker.is_some(),
        ) {
            Ok(session) => session,
            Err(error) => {
                render::status(
                    "display",
                    "Warning",
                    &format!("Dashboard unavailable: {error}"),
                );
                None
            }
        }
    } else {
        None
    };
    if options.worker.is_some()
        && options.chat.input == chat_io::Input::Terminal
        && display_session.is_none()
    {
        let _ = process.kill().await;
        return Err(Box::new(relay::PermanentFailure(
            "the worker window requires an interactive terminal dashboard".into(),
        )));
    }
    if options.worker.is_none()
        && options.chat.input == chat_io::Input::Terminal
        && options.chat.agent != chat_io::Agent::NextCaller
        && display_session.is_none()
    {
        let _ = process.kill().await;
        return Err(Box::new(relay::PermanentFailure(
            "terminal agent dispatch requires a live dashboard; select another --input mode for headless use".into(),
        )));
    }
    if let Some(notice) = version_notice {
        update::report_connection(notice, options.server.as_deref().unwrap_or(&url));
    }
    terminal::set_logout_target(if options.temporary || options.is_local() {
        None
    } else {
        options
            .relay
            .as_ref()
            .map(|credential| (credential.server.as_str(), credential.token.as_str()))
    });
    // Explicit --allow uses the same relay transport without an account token.
    // Preserve its final authorization outcome even when the child exits first.
    let mut incoming = Incoming::start(reader, transport_kind);
    let _link_metrics = display_session
        .as_ref()
        .and_then(|_| link_metrics::observe(metric_channel, &client_id, incoming.closed.clone()));
    if let Some(harness) = &options.harness
        && let Err(error) = harness.check_target(Some(&url))
    {
        let _ = process.kill().await;
        incoming.stop().await;
        return Err(error.into());
    }
    render::status(
        "shell",
        "Progress",
        if options.harness.is_some() {
            "Starting native tools and the explicitly selected harness protocol"
        } else {
            "Starting native tools; optional harnesses stay unloaded"
        },
    );
    let backend = tokio::select! {
        biased;
        error = incoming.disconnected() => { let _ = process.kill().await; return Err(relay::failure(error)); },
        status = process.wait() => {
            let mut error = transport_kind.ended(status);
            if matches!(&transport, Transport::Account { .. }) { error = incoming.after_relay_exit(error).await; }
            return Err(relay::failure(error));
        },
        _ = interrupt.recv() => { let _ = process.kill().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = process.kill().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(120), Backend::start_with_harness(&options.shell, options.tty.as_deref(), options.harness.as_ref())) => result.map_err(|_| "local backend startup timed out")??,
    };
    let backend = match Runtime::with_session_control(
        backend.clone(),
        PathBuf::from(&metadata.cwd),
        options.context_roots.clone(),
        management,
    )
    .and_then(|runtime| {
        runtime.bind_client_id(&client_id)?;
        runtime.configure_timing(time_observation_supported, wait_display_supported)?;
        runtime.configure_chat(chat_enabled)?;
        if let Some(worker) = options.worker {
            runtime.configure_workers(worker, &options.chat.agent)?;
        }
        runtime.enable_default_skill_sources()?;
        Ok(runtime)
    }) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = process.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = process.wait().await;
            return Err(error.into());
        }
    };
    let _attachment_runtime = match attachment
        .map(|handle| handle.bind_runtime(backend.clone()))
        .transpose()
    {
        Ok(binding) => binding,
        Err(error) => {
            let _ = process.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = process.wait().await;
            return Err(error);
        }
    };
    // An explicit --load-harness uses the same lazy registry as a later tool call.
    let discovered = tokio::select! {
        biased;
        error = incoming.disconnected() => Err(error),
        status = process.wait() => {
            let error = transport_kind.ended(status);
            Err(if matches!(&transport, Transport::Account { .. }) { incoming.after_relay_exit(error).await } else { error })
        },
        _ = interrupt.recv() => { let _ = process.start_kill(); backend.shutdown().await; let _ = process.wait().await; return Ok(ConnectionExit::Stopped); },
        _ = terminate.recv() => { let _ = process.start_kill(); backend.shutdown().await; let _ = process.wait().await; return Ok(ConnectionExit::Stopped); },
        result = timeout(Duration::from_secs(180), async {
            if let Some(harness) = &options.harness { harness.activate(&backend).await?; }
            backend.tools().await
        }) => result.map_err(|_| "native tool discovery timed out".to_string()).and_then(|r|r),
    };
    let readiness = discovered.and_then(|_| incoming.ensure_open(&mut process));
    if let Err(mut error) = readiness {
        if matches!(&transport, Transport::Account { .. }) && process.try_wait()?.is_some() {
            error = incoming.after_relay_exit(error).await;
        }
        let _ = process.start_kill();
        backend.shutdown().await;
        incoming.stop().await;
        let _ = process.wait().await;
        return Err(relay::failure(error));
    }
    let login_expires_in = login_expires_in.saturating_sub(login_received_at.elapsed().as_secs());
    let mut output = json!({"url":url,"client_id":client_id,"name":metadata.name,"cwd":metadata.cwd,"hostname":metadata.hostname,"type":"shell"});
    if options.worker.is_some() {
        output["worker"] = json!(crate::worker::display());
    }
    if let Some(harness) = &options.harness {
        output["agent"] = json!(harness.provider.label());
        output["agent_scope"] = json!("protocol_catalog");
    }
    if options.is_local() {
        output["transport"] = json!("local");
        output["auth"] = json!("local_user");
    }
    if let Some(title) = &metadata.title {
        output["title"] = json!(title);
    }
    if let Some(credential) = &options.relay {
        output["transport"] = json!("https");
        output["temporary"] = json!(options.temporary);
        output["expires_at"] = json!(credential.expires_at);
        if options.account_compat {
            output["account_compat"] = json!(true);
            output["ownership"] = json!("verified_account");
            output["quota"] = json!("account");
        }
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
    } else if chat_enabled && options.chat.input != chat_io::Input::Terminal {
        // External input owns stdout. A local OAuth client still needs its
        // connection URL and one-time code on the operator's stderr.
        if options.is_local() {
            render::local_connection_stderr(&output).map_err(Into::into)
        } else {
            Ok(())
        }
    } else {
        render::connection(&output).map_err(Into::into)
    };
    if let Err(error) = published {
        let _ = process.start_kill();
        backend.shutdown().await;
        incoming.stop().await;
        let _ = process.wait().await;
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
        if !chat_enabled {
            "Human chat is disabled. Ctrl-C disconnects this client."
        } else if options.chat.input != chat_io::Input::Terminal {
            "Chat input is enabled. Ctrl-C disconnects this client."
        } else if github_auth {
            "Sign in with an allowed GitHub account. Ctrl-C disconnects this client."
        } else {
            "Enter login_code on the MCP OAuth page. Ctrl-C disconnects and revokes access."
        },
    );
    let local_input = match if options.chat.input == chat_io::Input::Terminal {
        if options.worker.is_some() {
            input::Input::attach_monitor(backend.terminals())
        } else {
            input::Input::attach(backend.terminals())
        }
    } else {
        Ok(None)
    } {
        Ok(input)
            if options.worker.is_none()
                || options.chat.input != chat_io::Input::Terminal
                || input.is_some() =>
        {
            input
        }
        Ok(_) => {
            let _ = process.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = process.wait().await;
            return Err(Box::new(relay::PermanentFailure(
                "worker monitor input is unavailable; no hidden terminal fallback is permitted"
                    .into(),
            )));
        }
        Err(error) => {
            let _ = process.start_kill();
            backend.shutdown().await;
            incoming.stop().await;
            let _ = process.wait().await;
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
    let mut steering_commands = steering::connect_with_chat(&client_id, VERSION, chat_enabled);
    let mut access_commands = access::connect(access_supported);
    let mut message_commands = messaging::connect(messaging_supported);
    let mut chat = if !chat_enabled
        || (options.chat.input == chat_io::Input::Terminal
            && options.chat.agent == chat_io::Agent::NextCaller)
    {
        None
    } else {
        match chat_io::Session::start(&options.chat, backend.clone()).await {
            Ok(session) => Some(session),
            Err(error) => {
                steering::disconnect();
                access::disconnect();
                messaging::disconnect();
                if let Some(input) = local_input {
                    input.shutdown().await;
                }
                let _ = process.start_kill();
                backend.shutdown().await;
                incoming.stop().await;
                let _ = process.wait().await;
                return Err(Box::new(relay::PermanentFailure(error)));
            }
        }
    };
    let mut steering_updates = tokio::time::interval(Duration::from_secs(1));
    steering_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut requests: JoinSet<io::Result<bool>> = JoinSet::new();
    let mut reload_requested = false;
    let mut intentionally_stopped = false;
    let mut service_updates = tokio::time::interval(Duration::from_secs(1));
    service_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Only registered, connected sessions scan the workspace.
    let cleanup_period = Duration::from_secs(5 * 60);
    let mut cleanup_updates =
        tokio::time::interval_at(tokio::time::Instant::now() + cleanup_period, cleanup_period);
    cleanup_updates.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut managed_cleanup = backend.start_managed_file_cleanup("startup");
    backend.start_workers();
    let mut worker_start_error = if options.worker_window_child {
        worker_window::child_ready(&client_id).err()
    } else {
        None
    };
    #[cfg(all(test, unix))]
    let output_fixture_ready = output_expiry_fixture::runtime_ready(&backend, &client_id, attempt);
    let mut result: crate::Result<()> = 'connected: loop {
        #[cfg(all(test, unix))]
        if let Err(error) = &output_fixture_ready {
            intentionally_stopped = true;
            break Err(error.clone().into());
        }
        if let Some(error) = worker_start_error.take() {
            intentionally_stopped = true;
            break Err(error.into());
        }
        tokio::select! {
            ended = chat_io::input_ended(&mut chat) => match ended {
                Ok(()) if options.chat.input == chat_io::Input::Pipe => {
                    render::status("chat input", "Ready", "Pipe reached EOF; the connected session stays available");
                }
                Ok(()) => { intentionally_stopped = true; break Ok(()); }
                Err(error) => break Err(Box::new(relay::PermanentFailure(error)) as crate::Error),
            },
            _ = steering_updates.tick() => {
                if chat_enabled && let Err(error) = steering::poll() { terminal::notice(&error); }
                access::poll();
                messaging::poll();
            },
            Some(request) = message_commands.recv() => {
                let sent = timeout(Duration::from_secs(10), async {
                    let mut writer = writer.lock().await;
                    if messaging::pending(request.id) {
                        write_frame(&mut *writer, &Frame::Messages { id: request.id, command: request.command }).await?;
                    }
                    Ok::<(), io::Error>(())
                }).await;
                match sent {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => break Err(error.into()),
                    Err(_) => break Err("native message request write timed out".into()),
                }
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
            _ = cleanup_updates.tick() => {
                periodic_managed_file_cleanup(&mut managed_cleanup, || {
                    backend.start_managed_file_cleanup("periodic")
                }).await;
            },
            _ = service_updates.tick(), if service.is_some() => {
                if service.expect("guarded service runner").stop_requested() {
                    intentionally_stopped = true;
                    break Ok(());
                }
            },
            _ = ui_updates.tick(), if display_session.is_some() => {
                if terminal::exit_requested()
                    || service.is_some_and(|runner| runner.stop_requested())
                    || (options.worker_window_child && worker_window::child_stop_requested())
                {
                    intentionally_stopped = true;
                    break Ok(());
                }
                if options.worker_window.is_some() {
                    let stopping = tokio::select! {
                        biased;
                        _ = interrupt.recv() => true,
                        _ = terminate.recv() => true,
                        _ = std::future::ready(()) => false,
                    };
                    if stopping {
                        intentionally_stopped = true;
                        break Ok(());
                    }
                }
                if options.worker_window_child
                    && let Some(total) = worker_window::take_resize_request()
                {
                    let result = backend.resize_workers(total);
                    if let Err(error) = &result {
                        terminal::notice(&format!("Worker resize rejected: {error}"));
                    }
                    if let Err(error) = worker_window::report_resize_result(total, result) {
                        terminal::notice(&format!("Worker resize receipt: {error}"));
                    }
                }
                if let Some(controller) = &options.worker_window {
                    controller.lock().await.poll().await;
                }
                if options.worker.is_some() && local_input.as_ref().is_none_or(input::Input::is_finished) {
                    intentionally_stopped = true;
                    break Err(worker_monitor_closed());
                }
                render::set_git_status(git_status.snapshot());
            },
            frame = incoming.frames.recv() => match frame {
                Some(Ok(Some(Frame::Call {id,operation,controller,attribution,code_context,openai_context}))) => {
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
                    let (call_scope, openai_observation) = match backend.relay_call_scope(
                        id, attribution.as_deref(), code_context.as_ref(),
                        openai_context.filter(|_| openai_context_supported),
                    ) {
                        Ok(scoped) => scoped,
                        Err(error) => break Err(error.into()),
                    };
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
                            Err(_) => break Err(format!("{} reload acknowledgement write timed out", transport_kind.label()).into()),
                        }
                        if accepted {
                            // Flushing the pipe only hands bytes to the local transport process.
                            // Wait for the broker's receipt before terminating
                            // that process, so the ACK cannot be lost in transit.
                            let confirmed = timeout(Duration::from_secs(10), async {
                                loop {
                                    match incoming.frames.recv().await {
                                        Some(Ok(Some(Frame::ReloadCommit { id: acknowledged }))) if acknowledged == id => return Ok::<(), crate::Error>(()),
                                        Some(Ok(Some(Frame::CodeModeFinished { completion }))) => { backend.code_mode_finished(completion)?; }
                                        Some(Ok(Some(Frame::WaitDisplay { snapshot }))) => { backend.wait_display(snapshot)?; }
                                        Some(Ok(Some(Frame::SteeringReply { id, result }))) => { steering::reply(id, result)?; }
                                        Some(Ok(Some(Frame::MessagesReply { id, result }))) => { messaging::reply(id, result); }
                                        Some(Ok(Some(Frame::AccessReply { id, result }))) => { access::reply(id, result); }
                                        Some(Ok(Some(Frame::Call { id: next_id, .. }))) => {
                                            let mut writer = writer.lock().await;
                                            write_frame(&mut *writer, &Frame::Reply { id: next_id, result: Err("client reload is already pending; call list after reconnection".into()) }).await?;
                                        }
                                        Some(Err(error)) => return Err(error.into()),
                                        _ => return Err(format!("{} ended before the server confirmed reload receipt", transport_kind.label()).into()),
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
                    let peers = peer_ports.clone();
                    requests.spawn(async move {
                        let _controller = (!matches!(&operation, crate::wire::Operation::ListTools { .. }))
                            .then(|| render::controller_started(id, controller.as_ref(), attribution.as_deref(), &operation));
                        let was_tool = matches!(&operation, crate::wire::Operation::CallTool { .. });
                        let pending_observation = openai_observation.filter(|observation| {
                            matches!(observation.record(), Ok(false))
                        });
                        let mut result = call_scope.scope(async { match operation {
                            crate::wire::Operation::PeerPort { command } => {
                                if !matches!(*command, crate::wire::peer_port::Command::Close { .. }) { backend.validate_peer_ports()?; }
                                peers.perform(*command).await
                            }
                            op => backend.perform(op).await,
                        }}).await;
                        if let Some(observation) = pending_observation {
                            let _ = observation.record();
                        }
                        if let Some(call) = &mut code_call { call.finish(&result); }
                        // A large native response must not corrupt/disconnect the transport stream.
                        result = call_scope.scope(async {
                            bounded_native_reply(result, |reminder| {
                                terminal::restore_tool_summary_reminder(
                                    &crate::runtime::call_scope::Source::capture(),
                                    reminder,
                                );
                            })
                        }).await;
                        if let Err(error) = &result {
                            call_scope.scope(async {
                                render::scoped_event(
                                    "ssmcp",
                                    &serde_json::json!({"type":"ssmcp.status","level":"Error","message":error}),
                                    crate::runtime::call_scope::Source::capture(),
                                );
                            }).await;
                        }
                        timeout(Duration::from_secs(10),async {
                            let mut writer = writer.lock().await;
                            write_frame(&mut *writer,&Frame::Reply {id,result}).await
                        }).await.map_err(|_| transport_kind.result_write_timeout())??;
                        crate::memory::release_unused_heap();
                        Ok(was_tool)
                    });
                }
                Some(Ok(Some(Frame::CodeModeFinished { completion }))) => {
                    if let Err(error) = backend.code_mode_finished(completion) { break Err(error.into()); }
                }
                Some(Ok(Some(Frame::WaitDisplay { snapshot }))) => {
                    if let Err(error) = backend.wait_display(snapshot) { break Err(error.into()); }
                }
                Some(Ok(Some(Frame::SteeringReply { id, result }))) => {
                    if let Err(error) = steering::reply(id, result) { terminal::notice(&error); }
                }
                Some(Ok(Some(Frame::MessagesReply { id, result }))) => messaging::reply(id, result),
                Some(Ok(Some(Frame::AccessReply { id, result }))) => access::reply(id, result),
                Some(Ok(None)) | None => break Ok(()),
                Some(Ok(Some(Frame::Reject {message}))) => break Err(relay::failure(message)),
                Some(Ok(Some(_))) => break Err(format!("{} received an unexpected protocol frame", transport_kind.label()).into()),
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
    if reload_requested && options.worker_window_child {
        worker_window::child_prepare_reload();
    }
    backend.stop_workers();
    peer_ports.shutdown().await;
    if let Some(chat) = &chat {
        chat.stop();
    }
    if let Some(input) = local_input {
        input.shutdown().await;
    }
    if let Some(chat) = chat.take() {
        chat.shutdown().await;
    }
    backend.disconnect_timing();
    steering::disconnect();
    access::disconnect();
    messaging::disconnect();
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    let pending_logout_message = terminal::finish_pending_logout().await;
    if pending_logout_message.is_some() || terminal::exit_requested() {
        intentionally_stopped = true;
        reload_requested = false;
        result = Ok(());
    }
    join_managed_file_cleanup(&mut managed_cleanup).await;
    git_status.shutdown().await;
    if matches!(&transport, Transport::Account { .. })
        && !intentionally_stopped
        && !reload_requested
    {
        result = match result {
            Err(error) => Err(incoming.after_connection_error(error).await),
            other => other,
        };
    }
    incoming.stop().await;
    drop(writer);
    let _ = process.start_kill();
    if reload_requested {
        backend.shutdown_for_reload().await;
    } else {
        backend.shutdown().await;
    }
    let _ = process.wait().await;
    let exit_message = terminal::take_exit_message().or(pending_logout_message);
    drop(display_session);
    if let Some(message) = exit_message {
        // Print the complete safe outcome after restoring the terminal.
        // Status rows clip long messages and can hide revocation failures.
        eprintln!("{message}");
    }
    render::status("shell", "Result", "DISCONNECTED");
    if reload_requested {
        return Ok(ConnectionExit::Reload { restart });
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

/// Report a CLI failure unless its diagnostic was already written as a tool result.
pub fn report_run_error(error: &(dyn std::error::Error + 'static)) {
    if !error.is::<computer_control::ReportedOperationError>() {
        report_error(&error.to_string());
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn managed_file_cleanup_does_not_overlap_and_shutdown_waits_for_the_scan() {
        use std::future::Future;

        let (release_first, first) = tokio::sync::oneshot::channel::<()>();
        let mut task = tokio::spawn(async move {
            first.await.unwrap();
        });
        periodic_managed_file_cleanup(&mut task, || {
            panic!("an active cleanup scan must not be replaced")
        })
        .await;

        release_first.send(()).unwrap();
        timeout(Duration::from_secs(2), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the released scan completes");

        let (release_second, second) = tokio::sync::oneshot::channel::<()>();
        let mut replacements = 0;
        periodic_managed_file_cleanup(&mut task, || {
            replacements += 1;
            tokio::spawn(async move {
                second.await.unwrap();
            })
        })
        .await;
        assert_eq!(replacements, 1);

        let joining = join_managed_file_cleanup(&mut task);
        tokio::pin!(joining);
        let pending =
            std::future::poll_fn(|cx| std::task::Poll::Ready(joining.as_mut().poll(cx))).await;
        assert!(pending.is_pending(), "shutdown must await the owned scan");
        release_second.send(()).unwrap();
        timeout(Duration::from_secs(2), joining)
            .await
            .expect("shutdown joins the released scan");
    }

    #[test]
    fn worker_capacity_changes_accumulate_with_checked_bounds() {
        let total = worker_total_after_delta(0, 3).unwrap();
        let total = worker_total_after_delta(total, 2).unwrap();
        assert_eq!(total, 5);
        assert_eq!(worker_total_after_delta(total, -5).unwrap(), 0);
        assert!(worker_total_after_delta(0, -1).is_err());
        assert!(worker_total_after_delta(128, 1).is_err());
        assert!(worker_total_after_delta(1, i32::MAX).is_err());
        assert!(worker_total_after_delta(1, i32::MIN).is_err());
    }

    #[test]
    fn worker_resize_arguments_preserve_literals_and_harness_tail() {
        let original = [
            "--title",
            "--worker",
            "--worker=3",
            "--worker-k",
            "2",
            "--load-harness",
            "fixture",
            "--",
            "--worker",
            "99",
        ]
        .map(OsString::from);
        let rewritten = worker_arguments(&original, 7, true, false).unwrap();
        let expected = [
            "--title",
            "--worker",
            "--worker=7",
            "--worker-k",
            "2",
            "--load-harness",
            "fixture",
            WORKER_WINDOW_CHILD_FLAG,
            "--",
            "--worker",
            "99",
        ]
        .map(OsString::from);
        assert_eq!(rewritten, expected);
    }

    #[test]
    fn worker_resize_zero_discards_only_worker_startup_settings() {
        let original = [
            "--title",
            "--worker",
            "--worker",
            "3",
            "--worker-k=2",
            "--agent",
            "codex",
            "--agent-argv-file",
            "--worker=99",
        ]
        .map(OsString::from);
        let rewritten = worker_arguments(&original, 0, false, true).unwrap();
        assert_eq!(rewritten, ["--title", "--worker"].map(OsString::from));
        assert!(worker_arguments(&original, 0, true, false).is_err());
        assert!(worker_arguments(&original, 129, true, false).is_err());
    }

    #[test]
    fn dynamic_worker_creation_uses_external_agents_by_default() {
        let original = [
            "--server",
            "https://relay.test",
            "--agent",
            "codex",
            "--agent-argv-file",
            "literal",
            "--title",
            "worker pool",
        ]
        .map(OsString::from);
        let rewritten = worker_arguments(&original, 2, true, true).unwrap();
        assert_eq!(
            rewritten,
            [
                "--server",
                "https://relay.test",
                "--title",
                "worker pool",
                "--worker",
                "2",
                WORKER_WINDOW_CHILD_FLAG,
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn worker_window_split_preserves_argv_and_normal_chat() {
        let args = [
            "--server",
            "https://relay.example",
            "--name",
            "worker fixture",
            "--worker=3",
            "--title",
            "spaces; $(literal) 中文",
            "--no-watchdog",
        ]
        .map(OsString::from)
        .to_vec();
        let mut parent = Options::parse(args.clone()).unwrap().unwrap();
        let child_args = parent.worker_window_arguments(&args).unwrap();
        assert_eq!(&child_args[..args.len()], args.as_slice());
        assert_eq!(child_args.last().unwrap(), WORKER_WINDOW_CHILD_FLAG);
        let child = Options::parse(child_args.clone()).unwrap().unwrap();
        assert!(child.worker_window_child);
        assert_eq!(child.worker.unwrap().count, 3);
        assert_eq!(child.chat.agent, chat_io::Agent::Remote);
        assert_eq!(child.chat.input, chat_io::Input::Terminal);
        assert!(child.worker_window_arguments(&child_args).is_none());
        parent.become_worker_controller();
        assert!(parent.worker.is_none());
        assert_eq!(parent.chat, chat_io::Settings::default());
        assert!(parent.chat.arguments().is_empty());
        assert_eq!(parent.server, child.server);
        assert_eq!(parent.name, child.name);
        assert_eq!(parent.title, child.title);
        assert_eq!(parent.watchdog, child.watchdog);
        assert!(parent.worker_window_arguments(&args).is_none());
    }

    #[test]
    fn worker_window_keeps_background_and_headless_pools_single() {
        for flags in [
            vec!["--worker", "3", "--daemon"],
            vec!["--worker", "3", "--input", "none"],
        ] {
            let args = ["--server", "https://relay.example", "--name", "single-pool"]
                .into_iter()
                .chain(flags)
                .map(OsString::from)
                .collect::<Vec<_>>();
            let options = Options::parse(args.clone()).unwrap().unwrap();
            assert!(options.worker_window_arguments(&args).is_none());
            assert_eq!(options.worker.unwrap().count, 3);
            assert_eq!(options.chat.agent, chat_io::Agent::Remote);
            let saved = options.session_spec().unwrap();
            let restored = Options::parse_saved(&saved).unwrap().unwrap();
            let saved_args = saved.args.iter().map(OsString::from).collect::<Vec<_>>();
            assert!(restored.worker_window_arguments(&saved_args).is_none());
            assert_eq!(restored.chat.input, chat_io::Input::None);
        }
    }

    #[test]
    fn worker_window_marker_requires_one_foreground_pool() {
        for flags in [
            vec![WORKER_WINDOW_CHILD_FLAG],
            vec![
                "--worker",
                "3",
                WORKER_WINDOW_CHILD_FLAG,
                WORKER_WINDOW_CHILD_FLAG,
            ],
            vec!["--worker", "3", "--daemon", WORKER_WINDOW_CHILD_FLAG],
            vec!["--worker", "3", "--input", "none", WORKER_WINDOW_CHILD_FLAG],
        ] {
            let args = ["--server", "https://relay.example"]
                .into_iter()
                .chain(flags)
                .map(OsString::from);
            assert!(Options::parse(args).is_err());
        }
        let title = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--worker",
                "3",
                "--title",
                WORKER_WINDOW_CHILD_FLAG,
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert!(!title.worker_window_child);
        let child = Options::parse(
            [
                "--server",
                "https://relay.example",
                "--name",
                "child-fixture",
                "--worker",
                "3",
                WORKER_WINDOW_CHILD_FLAG,
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let mut saved = child.session_spec().unwrap();
        saved.args.push(WORKER_WINDOW_CHILD_FLAG.into());
        assert!(Options::parse_saved(&saved).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_window_explicit_executor_remains_only_in_child() {
        let executable = std::env::current_exe().unwrap();
        let mut args = [
            "--server",
            "https://relay.example",
            "--worker",
            "2",
            "--worker-k",
            "0.5",
            "--agent",
            "exec",
            "--agent-exec",
        ]
        .map(OsString::from)
        .to_vec();
        args.push(executable.as_os_str().to_owned());
        let mut parent = Options::parse(args.clone()).unwrap().unwrap();
        let child_args = parent.worker_window_arguments(&args).unwrap();
        let child = Options::parse(child_args).unwrap().unwrap();
        assert_eq!(child.worker.unwrap().factor, 0.5);
        assert_eq!(
            child.chat.agent,
            chat_io::Agent::Exec {
                program: executable,
                argv_file: None
            }
        );
        parent.become_worker_controller();
        assert!(parent.worker.is_none());
        assert_eq!(parent.chat.agent, chat_io::Agent::NextCaller);
        assert!(parent.chat.arguments().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn worker_flags_default_remote_and_save_explicit_headless_execution() {
        let parse = |args: &[&str]| {
            Options::parse(
                ["--server", "https://relay.example"]
                    .into_iter()
                    .chain(args.iter().copied())
                    .map(OsString::from),
            )
            .unwrap()
            .unwrap()
        };
        let default = parse(&["--worker", "10"]);
        assert_eq!(
            default.worker,
            Some(crate::worker::Config {
                count: 10,
                factor: 1.0
            })
        );
        assert_eq!(default.chat.agent, chat_io::Agent::Remote);
        assert_eq!(default.chat.input, chat_io::Input::Terminal);
        assert!(default.harness.is_none());
        let explicit = parse(&[
            "--name",
            "worker-fixture",
            "--worker",
            "3",
            "--worker-k",
            "0.5",
            "--input",
            "none",
            "--agent",
            "codex",
        ]);
        let saved = explicit.session_spec().unwrap();
        let restored = Options::parse_saved(&saved).unwrap().unwrap();
        assert_eq!(restored.worker, explicit.worker);
        assert_eq!(restored.chat.agent, chat_io::Agent::Codex);
        assert_eq!(restored.chat.input, chat_io::Input::None);
        assert!(!saved_chat_enabled(&saved));
    }

    #[test]
    fn remote_worker_defaults_and_saved_pools_need_no_advanced_chat_flags() {
        let parse = |args: &[&str]| {
            Options::parse(
                ["--server", "https://relay.example"]
                    .into_iter()
                    .chain(args.iter().copied())
                    .map(OsString::from),
            )
            .unwrap()
            .unwrap()
        };
        let normal = parse(&[]);
        assert_eq!(normal.chat.agent, chat_io::Agent::NextCaller);
        assert_eq!(normal.chat.input, chat_io::Input::Terminal);
        assert!(normal.worker.is_none());
        for args in [
            vec!["--name", "remote-fixture", "--worker", "2"],
            vec![
                "--name",
                "remote-fixture",
                "--worker",
                "2",
                "--agent",
                "remote",
            ],
            vec![
                "--name",
                "remote-fixture",
                "--worker",
                "2",
                "--agent",
                "next-caller",
            ],
        ] {
            let options = parse(&args);
            assert_eq!(options.chat.agent, chat_io::Agent::Remote);
            assert_eq!(options.chat.input, chat_io::Input::Terminal);
            assert_eq!(options.worker.unwrap().count, 2);
            assert!(options.harness.is_none());
            let saved = options.session_spec().unwrap();
            validate_session_spec(&saved).unwrap();
            let restored = Options::parse_saved(&saved).unwrap().unwrap();
            assert_eq!(restored.chat.agent, chat_io::Agent::Remote);
            assert_eq!(restored.chat.input, chat_io::Input::None);
            assert_eq!(restored.worker, options.worker);
            assert!(!saved_chat_enabled(&saved));
        }
        let background = parse(&["--worker", "2", "--daemon"]);
        assert_eq!(background.chat.agent, chat_io::Agent::Remote);
        assert_eq!(background.chat.input, chat_io::Input::None);
        #[cfg(not(target_os = "linux"))]
        assert!(
            Options::parse(
                [
                    "--server",
                    "https://relay.example",
                    "--worker",
                    "2",
                    "--agent",
                    "codex"
                ]
                .map(OsString::from)
            )
            .is_err()
        );
        for flag in ["--help", "-h", "--help-full"] {
            assert!(
                Options::parse(["--worker", "2", "--agent", flag].map(OsString::from))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn worker_flags_reject_ambiguous_counts_factors_and_hidden_chat_inputs() {
        for args in [
            vec!["--worker", "0"],
            vec!["--worker", "129"],
            vec!["--worker-k", "1"],
            vec!["--worker", "2", "--worker-k", "NaN"],
            vec!["--worker", "2", "--worker", "3"],
            vec!["--worker", "2", "--input", "pipe"],
            vec!["--worker", "2", "--load-harness", "codex"],
            vec!["--worker", "2", "--list-tools"],
        ] {
            assert!(
                Options::parse(
                    ["--server", "https://relay.example"]
                        .into_iter()
                        .chain(args)
                        .map(OsString::from)
                )
                .is_err()
            );
        }
    }

    #[test]
    fn tool_alias_dispatch_preserves_arguments_and_only_matches_the_command() {
        for command in ["tool", "tools"] {
            for tail in [
                vec![],
                vec!["status"],
                vec!["check"],
                vec!["update"],
                vec!["--help"],
                vec!["unsupported"],
            ] {
                let args = std::iter::once(command)
                    .chain(tail.iter().copied())
                    .map(OsString::from)
                    .collect::<Vec<_>>();
                assert_eq!(tool_repository_arguments(&args), Some(&args[1..]));
            }
        }
        for args in [
            vec![],
            vec!["--name", "tool"],
            vec!["resume", "tool"],
            vec!["toolish"],
        ] {
            let args = args.into_iter().map(OsString::from).collect::<Vec<_>>();
            assert!(tool_repository_arguments(&args).is_none());
        }
    }

    #[test]
    fn unknown_new_commands_do_not_report_a_removed_ssh_connection() {
        for args in [
            vec!["unknown-command"],
            vec!["relay.example"],
            vec!["--server", "https://relay.example", "unknown-command"],
        ] {
            let error = Options::parse_new(args.into_iter().map(OsString::from))
                .err()
                .unwrap();
            assert_eq!(
                error,
                "unrecognized command or destination; see ssmcp --help. For tool sources use ssmcp tools [status|check|update]; for a connection use ssmcp username@server or ssmcp --server https://server"
            );
            assert!(!error.contains("SSH"));
        }
    }

    #[test]
    fn connection_diagnostics_preserve_relay_targets_and_explicit_legacy_ssh() {
        let named = Options::parse_new(["alice@relay.example"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(named.username.as_deref(), Some("alice"));
        assert_eq!(named.server.as_deref(), Some("https://relay.example"));
        assert!(named.destination.is_none());
        let server = Options::parse_new(["--server", "https://relay.example"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(server.server.as_deref(), Some("https://relay.example"));
        assert!(server.destination.is_none());
        let legacy = Options::parse_new(["--ssh", "alice@relay.example"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(legacy.destination.as_deref(), Some("alice@relay.example"));
        assert!(legacy.server.is_none());
        assert!(legacy.username.is_none());
    }

    #[test]
    fn retired_resume_command_is_rejected_before_startup() {
        let expected = "conversation resume is no longer available; start ssmcp for a live session";
        for flags in [
            vec![],
            vec!["--server", "https://relay.example"],
            vec!["--worker", "3"],
            vec!["--daemon"],
            vec!["--input", "none"],
            vec!["--help"],
        ] {
            let args = ["resume"].into_iter().chain(flags).map(OsString::from);
            assert_eq!(Options::parse_new(args).err().unwrap(), expected);
        }
        assert!(!HELP.contains("ssmcp resume"));
        assert!(!HELP_FULL.contains("ssmcp resume"));
    }

    #[test]
    fn resume_remains_literal_in_titles_and_provider_arguments() {
        let title = Options::parse_new(
            ["--server", "https://relay.example", "--title", "resume"].map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(title.title.as_deref(), Some("resume"));
        let options = Options::parse_new(
            [
                "--server",
                "https://relay.example",
                "--load-harness",
                "codex",
                "--",
                "resume",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            options.harness.unwrap().arguments,
            vec![OsString::from("resume")]
        );
    }

    #[test]
    fn watchdog_keeps_retrying_transient_transport_errors() {
        let closed_monitor = worker_monitor_closed();
        assert!(!watchdog_retries(true, Some(closed_monitor.as_ref())));
        let disconnected = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert!(watchdog_retries(true, Some(&disconnected)));
        assert!(!watchdog_retries(false, Some(&disconnected)));
        assert!(watchdog_retries(true, None));
        assert!(!watchdog_retries(true, Some(&UnsupportedWireProtocol)));
        assert!(!watchdog_retries(
            true,
            Some(&relay::PermanentFailure("rejected".into()))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_socket_option_accepts_default_and_custom_paths_without_eating_flags() {
        let default =
            Options::parse(["local", "--socket", "--name", "socket-fixture"].map(OsString::from))
                .unwrap()
                .unwrap();
        assert!(default.is_local());
        assert_eq!(default.name.as_deref(), Some("socket-fixture"));
        assert_eq!(
            default
                .local
                .as_ref()
                .unwrap()
                .socket_path()
                .unwrap()
                .file_name()
                .unwrap(),
            "mcp.sock"
        );
        for args in [
            vec!["local", "--socket", "/tmp/private/mcp.sock"],
            vec!["local", "--socket=/tmp/private/mcp.sock"],
        ] {
            let parsed = Options::parse(args.into_iter().map(OsString::from))
                .unwrap()
                .unwrap();
            assert_eq!(
                parsed.local.as_ref().unwrap().socket_path(),
                Some(std::path::Path::new("/tmp/private/mcp.sock"))
            );
        }
        let title = Options::parse(["local", "--title", "--socket"].map(OsString::from))
            .unwrap()
            .unwrap();
        assert_eq!(title.title.as_deref(), Some("--socket"));
        assert!(title.local.as_ref().unwrap().socket_path().is_none());
        for args in [
            vec!["local", "--socket=relative.sock"],
            vec!["local", "--socket="],
            vec!["local", "--socket", "--socket"],
            vec![
                "--server",
                "https://remote.example",
                "--socket",
                "/tmp/private/mcp.sock",
            ],
        ] {
            assert!(Options::parse(args.into_iter().map(OsString::from)).is_err());
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_options_do_not_select_or_persist_a_remote_account() {
        let options = Options::parse_with_default(
            ["local", "--listen", "127.0.0.1:0", "--title", "--server"].map(OsString::from),
            Some("saved@remote.example"),
        )
        .unwrap()
        .unwrap();
        assert!(options.is_local());
        assert!(options.destination.is_none());
        assert!(options.server.is_none());
        assert!(options.relay.is_none());
        assert_eq!(options.title.as_deref(), Some("--server"));
        assert!(options.session_spec().is_err());
        assert!(
            Options::parse(
                ["local", "--public-url", "https://workstation.local"].map(OsString::from)
            )
            .is_err()
        );
        for flags in [
            [
                "local",
                "--public-url",
                "https://workstation.local",
                "--listen",
                "127.0.0.1:8765",
            ],
            [
                "local",
                "--listen",
                "127.0.0.1:8765",
                "--public-url",
                "https://workstation.local",
            ],
        ] {
            assert!(
                Options::parse(flags.map(OsString::from))
                    .unwrap()
                    .unwrap()
                    .is_local()
            );
        }
        for flags in [
            vec!["local", "--server", "https://remote.example"],
            vec!["local", "--username", "alice"],
            vec!["local", "--allow", "alice@github"],
            vec!["local", "--daemon"],
            vec!["local", "--session-control"],
            vec!["local", "--list-tools"],
            vec!["local", "alice@remote.example"],
        ] {
            assert!(Options::parse(flags.into_iter().map(OsString::from)).is_err());
        }
    }

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
                "octocat@relay.example",
                "--server",
                "https://relay.example",
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
        assert_eq!(parsed.server, parent.server);
        assert_eq!(parsed.title.as_deref(), Some("--help"));
        assert_eq!(parsed.name.as_deref(), Some("worker"));
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
    fn disabled_chat_is_saved_as_a_literal_and_inherited_by_local_children() {
        let options = Options::parse(
            [
                "--name",
                "quiet",
                "--server",
                "https://relay.example",
                "--title",
                "--input",
                "--input",
                "none",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let spec = options.session_spec().unwrap();
        validate_session_spec(&spec).unwrap();
        assert!(!saved_chat_enabled(&spec));
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.chat.input, chat_io::Input::None);
        assert_eq!(restored.chat.agent, chat_io::Agent::NextCaller);
        assert_eq!(restored.title.as_deref(), Some("--input"));
        let child = child_session_spec(
            &spec,
            "quiet-child".into(),
            spec.cwd.clone(),
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!saved_chat_enabled(&child));
        assert_eq!(
            Options::parse_saved(&child).unwrap().unwrap().chat.input,
            chat_io::Input::None
        );
        assert!(
            Options::parse(
                [
                    "--server",
                    "https://relay.example",
                    "--input",
                    "none",
                    "--agent",
                    "codex",
                ]
                .map(OsString::from)
            )
            .is_err()
        );
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
        for (args, expected_allow) in [
            (
                vec![
                    "--daemon",
                    "--watchdog",
                    "--reconnect-delay",
                    "7",
                    "--name",
                    "workstation",
                    "--title",
                    "--daemon",
                    "--server",
                    "https://relay.example",
                    "--allow",
                    "OctoCat@Relay.Example",
                    "--theme",
                    "nord",
                ],
                vec!["octocat@relay.example"],
            ),
            (
                vec![
                    "--daemon",
                    "--watchdog",
                    "--reconnect-delay",
                    "7",
                    "--name",
                    "workstation",
                    "--title",
                    "--daemon",
                    "-i",
                    "key with spaces",
                    "-F",
                    "config with spaces",
                    "-p",
                    "2222",
                    "--theme",
                    "nord",
                    "user@host",
                ],
                vec![],
            ),
        ] {
            let options = Options::parse(args.into_iter().map(OsString::from))
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
            assert_eq!(restored.allow, expected_allow);
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
            assert_eq!(restored.server, options.server);
            assert_eq!(restored.shell, options.shell);
            assert!(restored.theme.is_none());
        }
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
        let mut options = options(&["Alice@EXAMPLE.com:8443", "--no-browser", "--name", "dev"]);
        assert!(options.destination.is_none());
        assert_eq!(options.server.as_deref(), Some("https://example.com:8443"));
        assert_eq!(options.username.as_deref(), Some("alice"));
        assert!(options.no_browser);
        assert!(options.identity.is_none());

        let server = options.server.as_deref().unwrap();
        let username = options.username.as_deref().unwrap();
        let token = "a".repeat(64);
        options.allow = vec![crate::server_endpoint::allow_principal(username, server).unwrap()];
        options.relay = Some(account::Credential {
            server: server.into(),
            token: token.clone(),
            principal: "octocat@github".into(),
            subject: Some("123".into()),
            username: Some(username.into()),
            expires_at: u64::MAX,
        });
        assert_eq!(
            options.registration_metadata().unwrap().allow,
            options.allow
        );
        let spec = options.session_spec().unwrap();
        assert!(
            spec.args
                .windows(2)
                .any(|args| args == ["--username", "alice"])
        );
        assert!(!spec.args.iter().any(|arg| arg == "--allow"));
        assert!(!serde_json::to_string(&spec).unwrap().contains(&token));
        let restored = Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.username.as_deref(), Some("alice"));
        assert_eq!(restored.server, options.server);
        assert!(restored.destination.is_none());
        assert!(restored.allow.is_empty());
        assert!(restored.relay.is_none());
        assert!(!restored.account_compat);
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
