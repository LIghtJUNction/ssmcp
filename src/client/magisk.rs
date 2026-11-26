//! Owner-launched, loopback-only Android configuration and session console.
mod frame;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{Read, Write},
    num::NonZeroU16,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

use crate::{
    Result,
    server_endpoint::{allow_principal, parse_target},
};

const STATUS_ENV: &str = "SSMCP_MAGISK_CONNECTED";
const CONSENT: &str = "root-consent-v1";
const ONBOARDING: &str = include_str!("../../packaging/magisk/src/ssmcp/onboarding/index.html");
const STYLE: &str = include_str!("../../packaging/magisk/assets/console.css");

struct ScreenState {
    parser: vt100::Parser,
    revision: u64,
}

struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    screen: Arc<Mutex<ScreenState>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    connected: PathBuf,
}

impl Terminal {
    fn open(target: &str, directory: &Path) -> Result<Self> {
        let size = PtySize {
            rows: 28,
            cols: 72,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system().openpty(size)?;
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let screen = Arc::new(Mutex::new(ScreenState {
            parser: vt100::Parser::new(
                NonZeroU16::new(size.rows).unwrap(),
                NonZeroU16::new(size.cols).unwrap(),
                0,
            ),
            revision: 0,
        }));
        let mut command = CommandBuilder::new(std::env::current_exe()?);
        command.args([target, "--watchdog"]);
        let mut nonce = [0; 16];
        getrandom::fill(&mut nonce)?;
        let connected = directory.join(format!("connected-{}", crate::hex::encode(nonce)));
        command.env(STATUS_ENV, &connected);
        command.env("SSMCP_SESSION_HOME", directory.join("sessions"));
        command.env("TERM", "xterm-256color");
        command.cwd(directory.join("workspace"));
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let output = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                let mut state = output.lock().unwrap();
                state.parser.process(&buf[..n]);
                state.revision += 1;
            }
        });
        Ok(Self {
            master: pair.master,
            writer,
            screen,
            child,
            connected,
        })
    }

    fn stop(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        if let Some(pid) = self.child.process_id().and_then(|id| i32::try_from(id).ok())
            && pid > 0
            // The unreaped PTY child owns this PID. SIGTERM lets the normal
            // client shutdown remove its foreground registration and detach IPC.
            && unsafe { libc::kill(pid, libc::SIGTERM) } == 0
        {
            for _ in 0..20 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let rows = NonZeroU16::new(rows.clamp(10, 80)).unwrap();
        let cols = NonZeroU16::new(cols.clamp(32, 200)).unwrap();
        {
            let mut state = self.screen.lock().unwrap();
            state.parser.set_size(rows, cols);
            state.revision += 1;
        }
        self.master.resize(PtySize {
            rows: rows.get(),
            cols: cols.get(),
            pixel_width: 0,
            pixel_height: 0,
        })?;
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_file(&self.connected);
    }
}

#[derive(Default)]
struct Sessions {
    target: Option<String>,
    tabs: BTreeMap<usize, Terminal>,
    next_tab: usize,
}

impl Sessions {
    fn open(&mut self, target: &str, directory: &Path) -> Result<usize> {
        if self.tabs.len() >= 8 {
            return Err("Close a tab before opening another".into());
        }
        let tab = self.next_tab;
        let terminal = Terminal::open(target, directory)?;
        self.tabs.insert(tab, terminal);
        self.next_tab += 1;
        Ok(tab)
    }
}

struct Console {
    dir: PathBuf,
    token: String,
    host: String,
    sessions: tokio::sync::Mutex<Sessions>,
}

fn rc_target(text: &str) -> Result<(&str, String)> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let line = lines
        .next()
        .ok_or("Expected ssmcp username@server in ssmcp.rc")?;
    let mut words = line.split_ascii_whitespace();
    match (words.next(), words.next(), words.next(), lines.next()) {
        (Some("ssmcp"), Some(target), None, None) => {
            let (name, server) = parse_target(target)?;
            Ok((target, allow_principal(&name, &server)?))
        }
        _ => Err("Expected one ssmcp username@server line in ssmcp.rc".into()),
    }
}

fn saved_target(dir: &Path) -> Result<Option<String>> {
    let rc = std::fs::read_to_string(dir.join("ssmcp.rc"))?;
    let Ok((written, target)) = rc_target(&rc) else {
        return Ok(None);
    };
    // A valid command in this root-owned private file explicitly selects this
    // relay. The placeholder shipped with the module always opens local setup.
    // The module hook compares this marker to the spelling in the owner's rc.
    write_private(&dir.join(CONSENT), written.as_bytes())?;
    Ok(Some(target))
}

fn save(dir: &Path, name: &str, server: &str) -> Result<String> {
    let target = allow_principal(name, server)?;
    for (file, contents) in [
        ("ssmcp.rc", format!("ssmcp {target}\n")),
        (CONSENT, target.clone()),
    ] {
        let staging = dir.join(format!(".{file}.new"));
        write_private(&staging, contents.as_bytes())?;
        std::fs::rename(staging, dir.join(file))?;
    }
    Ok(target)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

fn authorized(state: &Console, headers: &HeaderMap) -> bool {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        == Some(&state.host)
        && headers
            .get(header::ORIGIN)
            .is_none_or(|value| value.as_bytes() == format!("http://{}", state.host).as_bytes())
        && headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| bool::from(token.as_bytes().ct_eq(state.token.as_bytes())))
}

fn failure(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({"error": message.to_string()}))).into_response()
}

#[derive(Deserialize)]
struct Setup {
    target: String,
    root: bool,
    terms: bool,
    privacy: bool,
}

async fn configure(
    State(state): State<Arc<Console>>,
    headers: HeaderMap,
    Json(input): Json<Setup>,
) -> Response {
    if !authorized(&state, &headers) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Open the console from the Magisk module action.",
        );
    }
    match configure_inner(&state, input).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => failure(StatusCode::BAD_REQUEST, error),
    }
}

async fn configure_inner(state: &Console, input: Setup) -> Result<Value> {
    let (name, server) = parse_target(&input.target)?;
    let hosted = reqwest::Url::parse(&server)?
        .host_str()
        .is_some_and(|host| host.trim_end_matches('.') == "ssmcp.lmm.best");
    if !input.root || (hosted && !(input.terms && input.privacy)) {
        return Err("Confirm the root risks and the hosted service policies first.".into());
    }
    let mut sessions = state
        .sessions
        .try_lock()
        .map_err(|_| "Configuration is already in progress")?;
    if sessions.target.is_some() {
        return Err(
            "A target is active. Edit ssmcp.rc and restart the module to reconfigure.".into(),
        );
    }
    super::account::check_magisk_target(&server, &name).await?;
    let target = allow_principal(&name, &server)?;
    let tab = sessions.open(&target, &state.dir)?;
    if let Err(error) = save(&state.dir, &name, &server) {
        sessions.tabs.remove(&tab);
        sessions.next_tab = tab;
        return Err(error);
    }
    sessions.target = Some(target.clone());
    Ok(json!({"target": target, "mcp": format!("{server}/mcp")}))
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Action {
    List,
    Open,
    Close {
        tab: usize,
    },
    Screen {
        tab: usize,
        #[serde(default)]
        styled: bool,
        #[serde(default)]
        since: Option<u64>,
    },
    Input {
        tab: usize,
        text: String,
    },
    Resize {
        tab: usize,
        rows: u16,
        cols: u16,
    },
}

/// Select only the boot process started by this module, even when other tabs exist.
async fn boot_session(State(state): State<Arc<Console>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return failure(StatusCode::UNAUTHORIZED, "Open the module WebUI to pair.");
    }
    let pid = state
        .sessions
        .lock()
        .await
        .tabs
        .get(&0)
        .and_then(|tab| tab.child.process_id());
    let Some(pid) = pid else {
        return Json(json!({"session": null})).into_response();
    };
    let root = state.dir.join("sessions");
    match tokio::task::spawn_blocking(move || crate::session::frontend::target_for_pid(&root, pid))
        .await
    {
        Ok(Ok(target)) => {
            Json(json!({"session": target.map(|(target, _)| target)})).into_response()
        }
        _ => failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "Could not read the boot session.",
        ),
    }
}

async fn terminal(
    State(state): State<Arc<Console>>,
    headers: HeaderMap,
    Json(action): Json<Action>,
) -> Response {
    if !authorized(&state, &headers) {
        return failure(
            StatusCode::UNAUTHORIZED,
            "Open the console from the Magisk module action.",
        );
    }
    match tokio::task::spawn_blocking(move || terminal_action(&state, action)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(error)) => failure(StatusCode::BAD_REQUEST, error),
        Err(_) => failure(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Terminal operation failed.",
        ),
    }
}

fn terminal_action(state: &Console, action: Action) -> Result<Value> {
    // Called on a blocking worker. A screen read must not reject concurrent input.
    let mut sessions = state.sessions.blocking_lock();
    let tab = match action {
        Action::List => {
            let tabs = sessions.tabs.keys().collect::<Vec<_>>();
            let mcp = sessions
                .target
                .as_deref()
                .map(parse_target)
                .transpose()?
                .map(|(_, server)| format!("{server}/mcp"));
            let rc = std::fs::read_to_string(state.dir.join("ssmcp.rc"))?;
            let configured = rc_target(&rc).ok().map(|(_, target)| target);
            return Ok(
                json!({"target": sessions.target, "tabs": tabs, "mcp": mcp, "configured": configured}),
            );
        }
        Action::Open => {
            let target = sessions.target.clone().ok_or("Complete setup first")?;
            let tab = sessions.open(&target, &state.dir)?;
            return Ok(json!({"tab": tab}));
        }
        Action::Close { tab } if tab != 0 => {
            let closed = sessions.tabs.remove(&tab).ok_or("Unknown tab")?;
            drop(sessions);
            drop(closed);
            return Ok(json!({}));
        }
        Action::Close { .. } => {
            return Err("The boot session cannot be closed from this console".into());
        }
        Action::Screen { tab, .. } | Action::Input { tab, .. } | Action::Resize { tab, .. } => tab,
    };
    let terminal = sessions.tabs.get_mut(&tab).ok_or("Unknown tab")?;
    match action {
        Action::Screen { styled, since, .. } => {
            let exited = terminal.child.try_wait()?.is_some();
            let state = terminal.screen.lock().unwrap();
            let screen = state.parser.screen();
            let (row, col) = screen.cursor_position();
            let mut response = json!({
                "cursor": { "row": row, "col": col, "visible": !exited && !screen.hide_cursor() },
                "exited": exited,
                "connected_once": terminal.connected.is_file(),
            });
            if styled && since == Some(state.revision) {
                response["unchanged"] = json!(true);
            } else if styled {
                response["rows"] = serde_json::to_value(frame::rows(screen))?;
            } else {
                response["screen"] = json!(
                    screen
                        .rows(0, screen.size().1.get())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            if styled {
                response["revision"] = json!(state.revision);
            }
            Ok(response)
        }
        Action::Input { text, .. } if text.len() <= 4096 => {
            terminal.writer.write_all(text.as_bytes())?;
            Ok(json!({}))
        }
        Action::Resize { rows, cols, .. } => {
            terminal.resize(rows, cols)?;
            Ok(json!({}))
        }
        _ => Err("Input exceeds 4096 bytes".into()),
    }
}

pub(super) fn signal_connected() {
    if let Some(marker) = std::env::var_os(STATUS_ENV) {
        let _ = write_private(Path::new(&marker), b"connected\n");
    }
}

fn javascript(source: &'static str) -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        source,
    )
        .into_response()
}

pub(super) async fn run(args: &[OsString]) -> Result<()> {
    let [dir, port] = args else {
        return Err("Expected state directory and port".into());
    };
    let dir = PathBuf::from(dir).canonicalize()?;
    std::fs::create_dir_all(dir.join("workspace"))?;
    let port = port.to_str().ok_or("Invalid port")?.parse::<u16>()?;
    let host = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&host).await?;
    for entry in std::fs::read_dir(&dir)?.flatten() {
        if entry
            .file_name()
            .as_encoded_bytes()
            .starts_with(b"connected-")
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)?;
    let token = crate::hex::encode(&bytes);
    let gui = crate::session::gui::foreground_workbench(
        dir.join("sessions"),
        format!("http://{host}"),
        token.clone(),
    )?;
    let mut sessions = Sessions::default();
    if let Some(target) = saved_target(&dir)? {
        sessions.open(&target, &dir)?;
        sessions.target = Some(target);
    }
    let path = dir.join("console.url");
    let staged = dir.join(".console.url.new");
    write_private(
        &staged,
        format!("{}\nhttp://{host}/#{token}\n", std::process::id()).as_bytes(),
    )?;
    std::fs::rename(staged, path)?;
    let state = Arc::new(Console {
        dir,
        token,
        host,
        sessions: tokio::sync::Mutex::new(sessions),
    });
    let app = Router::new()
        .route("/setup.js", get(|| async {
            javascript(include_str!("../../packaging/magisk/src/ssmcp/onboarding/setup.js"))
        }))
        .route("/device-api.js", get(|| async {
            javascript(include_str!("../../packaging/magisk/assets/device-api.js"))
        }))
        .route("/console.css", get(|| async {
            ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE)
        }))
        .route("/setup", get(|| async { Html(ONBOARDING) }).post(configure))
         .route("/zuse", get(|| async {
            ([(header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'self'; frame-src http://127.0.0.1:43497; frame-ancestors 'none'; base-uri 'none'; form-action 'none'")],
                Html(include_str!("../../packaging/magisk/assets/workbench.html")))
        }))
        .route("/api/gui/v1/boot-session", get(boot_session))
        .route("/terminal", post(terminal))
        .layer(DefaultBodyLimit::max(24 * 1024))
        .layer(axum::middleware::map_response(|mut response: Response| async {
            let headers = response.headers_mut();
            headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
            headers.insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
            headers.entry(header::CONTENT_SECURITY_POLICY).or_insert( "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'".parse().unwrap());
            response
        }))
        .with_state(state)
        .fallback_service(gui);
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::session::gui::shutdown())
        .await?;
    Ok(())
}

#[cfg(test)]
mod rc_tests {
    use super::rc_target;

    #[test]
    fn manual_rc_preserves_the_owner_spelling_and_normalizes_the_connection() {
        let (written, target) = rc_target("# phone\nssmcp Alice@https://relay.example\n").unwrap();
        assert_eq!(written, "Alice@https://relay.example");
        assert_eq!(target, "alice@relay.example");
        assert!(rc_target("ssmcp <USERNAME>@ssmcp.lmm.best").is_err());
        assert!(rc_target("ssmcp alice@relay.example\nssmcp bob@relay.example").is_err());
    }
}
