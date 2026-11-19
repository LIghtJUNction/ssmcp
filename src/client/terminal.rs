//! Compose logs and source audit with a configurable observational sidebar.
//! Only this module writes the physical terminal while a Session is active.
use super::{
    account::status::{self as account_status, Status as AccountStatus},
    audit_view::AuditView,
    panel::{self, PanelLayout, PanelSnapshot, PanelTone},
    render::LogFormat,
    ui::{Layout, Ui},
};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::{self, IsTerminal, Write},
    num::NonZeroU16,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod conversation;
mod events;
mod footer;
mod guide;
mod images;
pub(crate) mod markdown;

const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const MAX_REPLIES: usize = 8192;
const MAX_SESSIONS: usize = 16;
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";
// Rendered cells cost much more than UTF-8 output. Keep a short interactive
// window; raw command/event captures keep their existing retention policy.
const SCROLLBACK_ROWS: usize = 128;
const ENTER: &[u8] = b"\x1b[?1;6;25;66;1000;1006;2004s\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?6l\x1b[r\x1b[?25l\x1b[0m\x1b[H\x1b[2J";
const LEAVE: &[u8] = b"\x1b[?2026l\x1b[0m\x1b[?1l\x1b>\x1b[?2004l\x1b[?25h\x1b[?1000l\x1b[?1006l\x1b[?1049l\x1b[?1;6;25;66;1000;1006;2004r";
static DISPLAY: OnceLock<Mutex<Option<Arc<Shared>>>> = OnceLock::new();

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).unwrap()
}

fn shared() -> Option<Arc<Shared>> {
    DISPLAY
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
}

pub(crate) struct Session {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    guide_probe: Option<tokio::task::JoinHandle<()>>,
    account_probe: Option<tokio::task::JoinHandle<()>>,
}

impl Session {
    pub(crate) fn start(
        ui: &Ui,
        cwd: &str,
        title: Option<&str>,
        endpoint: Option<&str>,
    ) -> io::Result<Option<Self>> {
        if !ui.dashboard
            || ui.log_format == LogFormat::Jsonl
            || !io::stdin().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
            || !owns_terminal()
        {
            return Ok(None);
        }
        let Some((rows, cols)) = physical_size() else {
            return Ok(None);
        };
        let output = PhysicalOutput::open()?;
        let mut options = ui.clone();
        options.color &= std::env::var_os("NO_COLOR").is_none();
        let mut state = State::new(rows, cols, &options);
        state.set_context(cwd, title);
        state.server_origin = endpoint.and_then(footer::Origin::parse);
        let guide = endpoint.and_then(guide::Guide::from_endpoint);
        let guide_url = guide.as_ref().map(|guide| guide.url().to_owned());
        state.guide = guide.map(guide::Guide::portal);
        let account_server = endpoint
            .and_then(account_status::server_from_endpoint)
            .filter(|_| tokio::runtime::Handle::try_current().is_ok());
        if account_server.is_some() {
            state.account = AccountStatus::Loading;
        }
        state.graphics = images::Graphics::environment(&options);
        let shared = Arc::new(Shared {
            state: Mutex::new(state),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });
        let mut global = DISPLAY
            .get_or_init(|| Mutex::new(None))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if global.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal compositor already active",
            ));
        }
        *global = Some(shared.clone());
        drop(global);
        let worker = shared.clone();
        match std::thread::Builder::new()
            .name("ssmcp-display".into())
            .spawn(move || {
                draw_loop(&worker, output);
            }) {
            Ok(thread) => {
                let guide_probe = guide_url.and_then(|url| {
                    let handle = tokio::runtime::Handle::try_current().ok()?;
                    let display = Arc::downgrade(&shared);
                    Some(handle.spawn(async move {
                        // Keep guide route discovery outside the compositor lock.
                        let Some(guide) = guide::Guide::resolve(&url).await else {
                            return;
                        };
                        let Some(shared) = display.upgrade() else {
                            return;
                        };
                        if shared.stop.load(Ordering::Acquire)
                            || !shared.alive.load(Ordering::Acquire)
                        {
                            return;
                        }
                        let mut state = shared
                            .state
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if !state.preview {
                            state.guide = Some(guide);
                            state.dirty = true;
                        }
                    }))
                });
                let account_probe = account_server.and_then(|server| {
                    let handle = tokio::runtime::Handle::try_current().ok()?;
                    let display = Arc::downgrade(&shared);
                    Some(handle.spawn(async move {
                        loop {
                            // Resolve this session's selected origin again on each refresh.
                            // Requests and waits never hold the compositor lock.
                            let status = AccountStatus::refresh(&server).await;
                            {
                                let Some(shared) = display.upgrade() else {
                                    return;
                                };
                                if shared.stop.load(Ordering::Acquire)
                                    || !shared.alive.load(Ordering::Acquire)
                                {
                                    return;
                                }
                                let mut state =
                                    shared.state.lock().unwrap_or_else(|e| e.into_inner());
                                if !state.preview && state.account != status {
                                    state.account = status;
                                    state.dirty = true;
                                }
                            }
                            tokio::time::sleep(Duration::from_secs(60)).await;
                        }
                    }))
                });
                Ok(Some(Self {
                    shared,
                    thread: Some(thread),
                    guide_probe,
                    account_probe,
                }))
            }
            Err(error) => {
                *DISPLAY
                    .get()
                    .unwrap()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = None;
                Err(error)
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(probe) = self.guide_probe.take() {
            probe.abort();
        }
        if let Some(probe) = self.account_probe.take() {
            probe.abort();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if let Some(global) = DISPLAY.get() {
            let mut global = global.lock().unwrap_or_else(|e| e.into_inner());
            if global
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.shared))
            {
                *global = None;
            }
        }
    }
}

pub(crate) fn active() -> bool {
    shared().is_some_and(|shared| shared.alive.load(Ordering::Acquire))
}

/// A local command requests normal frontend cleanup; it does not signal other processes.
pub(crate) fn exit_requested() -> bool {
    shared().is_some_and(|shared| {
        shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .conversation
            .exit_requested
    })
}

/// Wake the display after asynchronous local discovery without publishing a log.
pub(super) fn invalidate() {
    if let Some(shared) = shared() {
        shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dirty = true;
    }
}

pub(super) fn log_width() -> Option<usize> {
    let shared = shared()?;
    if !shared.alive.load(Ordering::Acquire) {
        return None;
    }
    let width = shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .geometry
        .left;
    Some(usize::from(width))
}

pub(super) fn log(bytes: &[u8]) -> bool {
    let Some(shared) = shared() else {
        return false;
    };
    if !shared.alive.load(Ordering::Acquire) {
        return false;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.log(bytes);
    true
}

/// Keep correlated activity and the original transcript in the same update.
pub(super) fn log_event(label: &str, value: &serde_json::Value, bytes: &[u8]) -> bool {
    let Some(shared) = shared() else {
        return false;
    };
    if !shared.alive.load(Ordering::Acquire) {
        return false;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.log_event(label, value, bytes);
    true
}

/// The renderer releases its lock before publishing these display-only updates.
pub(super) fn diagnostics(updates: &[super::render::DiagnosticUpdate]) {
    if updates.is_empty() {
        return;
    }
    let Some(shared) = shared() else {
        return;
    };
    if !shared.alive.load(Ordering::Acquire) {
        return;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    for update in updates {
        if state.events.ingest_diagnostic(update) {
            // Keep the retained raw pane's safe first warning. Unfolded original
            // records remain available in explicit verbose/JSONL and the archive.
            state.log_raw(update.text().as_bytes());
        }
    }
    state.dirty = true;
}

/// Decode outside the State lock; preview failures do not alter native output.
pub(crate) fn image(block: &serde_json::Value) {
    let Some(shared) = shared() else {
        return;
    };
    if !shared.alive.load(Ordering::Acquire) {
        return;
    }
    let native = shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .graphics
        .prepare_native();
    let preview = match images::decode(block, native) {
        Ok(preview) => preview,
        Err(error) => {
            notice(&format!("Image preview unavailable: {error}"));
            return;
        }
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.image(preview, block["mimeType"].as_str().unwrap_or("image"));
}

/// Every PTY has its own parser, including terminals that are not on screen.
/// Unknown IDs are late output from a closed session and must not recreate it.
pub(crate) fn pty_session(id: &str, bytes: &[u8]) -> bool {
    let Some(shared) = shared() else {
        return false;
    };
    if !shared.alive.load(Ordering::Acquire) {
        return false;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.pty_session(id, bytes);
    true
}

pub(crate) fn register_session(id: &str, title: &str) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.register_session(id, title);
    }
}

pub(crate) fn select_session(id: &str) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.select_session(id);
    }
}

pub(crate) fn close_session(id: &str) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.close_session(id);
    }
}

pub(super) fn set_panel(snapshot: PanelSnapshot) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.preview {
            state.panel = snapshot;
        }
        state.dirty = true;
    }
}

/// The caller measures the outer terminal, then applies only the returned PTY
/// viewport to the child. Both Unix SIGWINCH and Windows resize use this path.
pub(crate) fn physical_resize(rows: u16, cols: u16) -> (u16, u16) {
    let rows = rows.clamp(1, 1000);
    let cols = cols.clamp(1, 1000);
    if let Some(shared) = shared()
        && shared.alive.load(Ordering::Acquire)
    {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.graphics.resized();
        state.resize(rows, cols);
        return (state.geometry.pty_rows, state.geometry.left);
    }
    (rows, cols)
}

/// Apply visual settings without restarting the connection or losing VT history.
pub(crate) fn apply_ui(ui: &Ui) -> Option<(u16, u16)> {
    let shared = shared()?;
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.ui = ui.clone();
    state.conversation.history_width = 0;
    state.ui.color &= std::env::var_os("NO_COLOR").is_none();
    let (rows, cols) = (state.geometry.rows, state.geometry.cols);
    state.resize(rows, cols);
    state.keyboard.logs = ui.layout == Layout::Logs;
    state.notice = None;
    state.dirty = true;
    Some((state.geometry.pty_rows, state.geometry.left))
}

/// Called after the steering lock is released; recipients and audit runs stay intact.
pub(super) fn callers_disconnected() {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.caller_tracker.disconnect();
        state.dirty = true;
    }
}

pub(crate) fn audit(value: &serde_json::Value) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let data = state.chat_data();
        state.caller_tracker.observe(value, &data);
        if state.audit.ingest(value) {
            state.dirty = true;
        }
    }
}

pub(crate) fn filter_logs(text: Option<&str>, level: Option<&str>) -> Result<(), String> {
    if text.is_some_and(|text| text.len() > 256 || text.chars().any(char::is_control)) {
        return Err("log filter must contain at most 256 bytes of plain text".into());
    }
    let level = match level {
        None => None,
        Some("all") => Some(0),
        Some("commands") => Some(1),
        Some("errors") => Some(2),
        _ => return Err("log level must be all, commands, or errors".into()),
    };
    let shared = shared()
        .ok_or("built-in display is inactive; filter ssmcp.audit.events in your frontend")?;
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(text) = text {
        state.log_query = text.into();
    }
    if let Some(level) = level {
        state.log_level = level;
    }
    state.filter_offset = 0;
    state.notice = None;
    state.dirty = true;
    Ok(())
}

pub(crate) fn browse(action: &str) -> Result<(), String> {
    let shared = shared().ok_or("built-in display is inactive")?;
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let page = isize::try_from(state.geometry.log_rows.saturating_sub(1).max(1)).unwrap_or(1);
    if state.ui.layout == Layout::Chat {
        if action.starts_with("audit.") {
            state.conversation.view = conversation::View::Audit;
            state.conversation.content_focus = true;
        } else if action.starts_with("logs.") {
            state.conversation.view = conversation::View::Activity;
        }
    }
    match action {
        "audit.up" => state.audit.up(),
        "audit.down" => state.audit.down(),
        "audit.open" => state.audit.open(),
        "audit.back" => state.audit.back(),
        "logs.page-up" => {
            state.scroll_logs(page);
            state.events.scroll_by(-page);
        }
        "logs.page-down" => {
            state.scroll_logs(-page);
            state.events.scroll_by(page);
        }
        "logs.end" => {
            state.scroll_logs(isize::MIN);
            state.events.latest();
        }
        _ => return Err("unknown display browsing action".into()),
    }
    state.dirty = true;
    Ok(())
}

pub(crate) fn take_actions() -> Vec<&'static str> {
    let Some(shared) = shared() else {
        return Vec::new();
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut state.actions)
}

pub(crate) fn notice(message: &str) {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.notice = Some(panel::safe_text(message).chars().take(240).collect());
        state.dirty = true;
    }
}

pub(crate) fn clear_logs() {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.events.clear();
        state.logs = vt100::Parser::new(
            nonzero(state.geometry.log_rows),
            nonzero(state.geometry.left),
            SCROLLBACK_ROWS,
        );
        state.previous_cr = false;
        state.dirty = true;
    }
}

pub(crate) fn snapshot() -> serde_json::Value {
    let Some(shared) = shared() else {
        return serde_json::json!({"active": false});
    };
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let g = state.geometry;
    let chat = state.ui.layout == Layout::Chat;
    let conversation = state.conversation.snapshot();
    serde_json::json!({
        "active": shared.alive.load(Ordering::Acquire), "rows":g.rows, "cols":g.cols,
        "panes":{"logs":{"row":if chat {conversation::body_top(g.rows)} else {0},"col":0,"rows":if chat {conversation::body_rows(g.rows)} else {g.log_rows},"cols":g.left,"visible":!chat || state.conversation.logs_open},
            "audit":{"row":if chat {conversation::body_top(g.rows)} else {g.pty_top},"col":0,"rows":if chat {conversation::body_rows(g.rows)} else {g.pty_rows},"cols":g.left,"visible":if chat {!state.conversation.logs_open && state.conversation.view == conversation::View::Audit} else {state.ui.layout != Layout::Logs}},
            "tasks":{"row":conversation::body_top(g.rows),"col":0,"rows":conversation::body_rows(g.rows),"cols":g.left,"visible":chat && state.conversation.view == conversation::View::Tasks},
            "input":{"row":conversation::prompt_top(g.rows)+1,"col":0,"rows":conversation::prompt_rows(g.rows),"cols":g.left,"visible":chat && conversation::prompt_rows(g.rows)>0}},
        "focus":if chat && state.conversation.logs_open && state.conversation.content_focus {"logs"} else if state.help {"help"} else if chat {if state.conversation.content_focus {"content"} else {"input"}} else if state.keyboard.logs || state.ui.layout == Layout::Logs {"logs"} else {"audit"},
        "active_terminal":state.active_session,
        "terminals":state.session_titles.iter().map(|(id,title)| serde_json::json!({"id":id,"title":panel::safe_text(title)})).collect::<Vec<_>>(),
        "conversation":conversation,"events":state.events.snapshot(),"selection_mode":state.selection,"audit":state.audit.snapshot(),"filter":{"text":state.log_query,"level":(["all","commands","errors"][usize::from(state.log_level)])},"help":state.help,"log_scrollback":state.logs.screen().scrollback(),"log_history":state.logs.screen().scrollback_len(),"raw_logs":state.raw_logs,
    })
}

/// A fixture rendered through the production compositor; no commands execute.
fn preview_state(rows: u16, cols: u16, ui: &Ui) -> State {
    let mut state = State::new(rows, cols, ui);
    state.preview = true;
    state.server_origin = footer::Origin::parse(super::account::DEFAULT_SERVER);
    state.guide = guide::Guide::from_endpoint(super::account::DEFAULT_SERVER);
    state.account = AccountStatus::Ready {
        login: "example".into(),
        username: None,
        traffic: account_status::Traffic::Remaining(750_000_000),
    };
    state.log_message("user", "Improve the client experience\nKeep commands, results and the original code easy to inspect.", "\n[USER] Improve the client experience\n  Keep commands, results and the original code easy to inspect.\n\n".as_bytes());
    state.log_event("preview", &serde_json::json!({"type":"exec.exited","job_id":"17","exit_code":0}), "[RUN] #17  cargo test --lib\n  cwd  ~/project/ssmcp\n\n[STDOUT] #17\n  test client::ui::custom_colors ... ok\n  test client::audit_view::browse ... ok\n\n[OK] #17  exit=0  1.42s\n  Command completed successfully.\n\n  Source retained in the code audit. Ctrl+O to inspect.\n\n".as_bytes());
    state.log_event("preview", &serde_json::json!({"type":"exec.exited","job_id":"preview-diff","exit_code":0}), "[DIFF] src/client/terminal/events.rs\n@@ -1 +1 @@\n- let expanded = true;\n+ let expanded = false;\n\n".as_bytes());
    state.log("[READY] A quieter workspace, with the details one key away.\n\n  Scroll directly · /filter cargo · ← agents · /queue\n  Write a requirement below; Enter queues it for the next tool response.\n\n".as_bytes());
    state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","phase":"started","run_id":"example-01","code":"const client = clients.find(c => c.name === \"workstation\");\nconst result = await exec.run(client.id, {\n  command: [\"cargo\", \"test\", \"--lib\"],\n  wait_ms: 1000,\n});\nprint({ exit: result.exit_code, output: result.stdout });"}));
    state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","phase":"finished","run_id":"example-01","success":true,"duration_ms":1420}));
    state.audit.open();
    state.panel.tasks.push(panel::TaskView {
        id: "example".into(),
        description: "Example task · custom frontend".into(),
        cwd: Some("~/project".into()),
        started: Instant::now(),
    });
    state.panel.progress.push(panel::ProgressView {
        source: "Example".into(),
        message: Some("Preparing workspace".into()),
        value: Some(3.0),
        total: Some(4.0),
        updated: Instant::now(),
    });
    state
}

pub(crate) fn preview_bytes(ui: &Ui, rows: u16, cols: u16) -> Vec<u8> {
    let mut state = preview_state(rows, cols, ui);
    if let Ok(cwd) = std::env::current_dir() {
        state.set_context(&cwd.to_string_lossy(), None);
    }
    state.frame(Instant::now()).diff(None)
}

pub(crate) fn seed_preview() {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let context_label = std::mem::take(&mut state.context_label);
        *state = preview_state(state.geometry.rows, state.geometry.cols, &state.ui);
        state.context_label = context_label;
    }
}

/// Used by live PTY creation before its first prompt is generated.
pub(crate) fn pane_size(rows: u16, cols: u16) -> (u16, u16) {
    if let Some(shared) = shared()
        && shared.alive.load(Ordering::Acquire)
    {
        let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        return (state.geometry.pty_rows, state.geometry.left);
    }
    (rows, cols)
}

/// Replies describe the virtual PTY, never absolute physical-screen positions.
/// Input owns the write side and drains this bounded queue without holding it
/// across await or renderer locks.
pub(crate) fn take_session_replies() -> Vec<(String, Vec<u8>)> {
    let Some(shared) = shared() else {
        return Vec::new();
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.take_session_replies()
}

/// The display consumes browsing and filter input while it is active.
/// A headless connection keeps the existing native terminal input path.
pub(crate) fn keyboard(text: &str) -> String {
    keyboard_bytes(text.as_bytes())
        .map(|output| String::from_utf8_lossy(&output).into_owned())
        .unwrap_or_else(|| text.into())
}

/// Give the active compositor original stdin bytes before any UTF-8 decoding.
/// Binary bracketed pastes are consumed here; headless input keeps its text path.
pub(crate) fn keyboard_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    let shared = shared()?;
    if !shared.alive.load(Ordering::Acquire) {
        return None;
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    Some(state.keyboard(bytes, Instant::now(), false))
}

/// Preserve an outer reader's actual image-input time when it retains a split
/// framing marker instead of returning bytes to the compositor immediately.
pub(crate) fn paste_input_progress(now: Instant) {
    let Some(shared) = shared() else {
        return;
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.paste_input_progress(now);
}

/// The attach decoder emits an explicit recovery boundary. Reset every
/// compositor layer before bytes following that boundary are accepted.
pub(crate) fn discard_image_paste() {
    let Some(shared) = shared() else {
        return;
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.discard_image_paste();
}

/// Called by the input pump so a lone Escape never waits indefinitely for a
/// possible shortcut suffix. Escape cancels filtering or returns to the list.
pub(crate) fn take_keyboard() -> String {
    let Some(shared) = shared() else {
        return String::new();
    };
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&state.keyboard(&[], Instant::now(), true)).into_owned()
}

#[cfg(windows)]
pub(crate) fn application_cursor() -> bool {
    let Some(shared) = shared() else {
        return false;
    };
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    state.pty.screen().application_cursor()
}

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
    alive: AtomicBool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    rows: u16,
    cols: u16,
    left: u16,
    log_rows: u16,
    pty_top: u16,
    pty_rows: u16,
}

impl Geometry {
    fn new(rows: u16, cols: u16, ui: &Ui) -> Self {
        let rows = rows.clamp(1, 1000);
        let cols = cols.clamp(1, 1000);
        let layout = PanelLayout::configured(
            cols,
            rows,
            ui.sidebar && !ui.panels.is_empty(),
            ui.sidebar_width,
        );
        let available = if rows >= 3 { rows - 1 } else { rows };
        let log_rows = match ui.layout {
            Layout::Chat => conversation::body_rows(rows),
            Layout::Logs => available,
            Layout::Split if rows >= 5 => {
                ((u32::from(available - 1) * u32::from(ui.log_height_percent) / 100) as u16)
                    .clamp(1, available - 2)
            }
            _ => 0,
        };
        let pty_top = if ui.layout == Layout::Split && log_rows > 0 {
            log_rows + 1
        } else {
            0
        };
        let pty_rows = if ui.layout == Layout::Chat {
            log_rows.max(1)
        } else {
            available.saturating_sub(pty_top).max(1)
        };
        Self {
            rows,
            cols,
            left: layout.log_cols.max(1),
            log_rows,
            pty_top,
            pty_rows,
        }
    }
}

fn request_interrupt() {
    crate::client::interface::computer::local_input();
    #[cfg(unix)]
    // SAFETY: this targets only the current process, whose normal signal handler
    // owns graceful shutdown and restoration of the original terminal mode.
    let _ = unsafe { libc::kill(libc::getpid(), libc::SIGINT) };
}

#[derive(Clone, Copy)]
enum Key {
    Interrupt,
    Help,
    Questions,
    Select,
    Layout,
    Sidebar,
    Theme,
    Reload,
    Save,
    Toggle,
    Up,
    Down,
    Home,
    End,
    Escape,
    AuditUp,
    AuditDown,
    Open,
    Back,
    Filter,
    FocusBack,
    Delete,
    Newline,
    HistoryStart,
    HistoryEnd,
    CanvasLeft,
    CanvasRight,
    Mouse {
        button: u16,
        x: u16,
        y: u16,
        release: bool,
    },
    PasteStart,
    PasteEnd,
}
const KEYS: &[(&[u8], Key)] = &[
    (b"\x1b[1;3D", Key::CanvasLeft),
    (b"\x1b[1;3C", Key::CanvasRight),
    (b"\x1b[57350;3u", Key::CanvasLeft),
    (b"\x1b[57351;3u", Key::CanvasRight),
    (b"\x1b[1;2A", Key::Questions),
    (b"\x1b[18~", Key::Select),
    (b"\x1b[Z", Key::FocusBack),
    (b"\x1b[3~", Key::Delete),
    (b"\x1b[13;2u", Key::Newline),
    (b"\x1b[27;2;13~", Key::Newline),
    (b"\x1b[1;5H", Key::HistoryStart),
    (b"\x1b[1;5~", Key::HistoryStart),
    (b"\x1b[7;5~", Key::HistoryStart),
    (b"\x1b[1;5F", Key::HistoryEnd),
    (b"\x1b[4;5~", Key::HistoryEnd),
    (b"\x1b[8;5~", Key::HistoryEnd),
    (b"\x1b[A", Key::AuditUp),
    (b"\x1bOA", Key::AuditUp),
    (b"\x1b[B", Key::AuditDown),
    (b"\x1bOB", Key::AuditDown),
    (b"\x1b[C", Key::Open),
    (b"\x1bOC", Key::Open),
    (b"\x1b[D", Key::Back),
    (b"\x1bOD", Key::Back),
    (b"\x1b[20~", Key::Filter),
    (b"\x1bOP", Key::Help),
    (b"\x1b[11~", Key::Help),
    (b"\x1bOQ", Key::Layout),
    (b"\x1b[12~", Key::Layout),
    (b"\x1bOR", Key::Sidebar),
    (b"\x1b[13~", Key::Sidebar),
    (b"\x1bOS", Key::Theme),
    (b"\x1b[14~", Key::Theme),
    (b"\x1b[15~", Key::Reload),
    (b"\x1b[19~", Key::Save),
    (b"\x1b[17~", Key::Toggle),
    (b"\x1b[5~", Key::Up),
    (b"\x1b[6~", Key::Down),
    (b"\x1b[H", Key::Home),
    (b"\x1bOH", Key::Home),
    (b"\x1b[1~", Key::Home),
    (b"\x1b[7~", Key::Home),
    (b"\x1b[F", Key::End),
    (b"\x1bOF", Key::End),
    (b"\x1b[4~", Key::End),
    (b"\x1b[8~", Key::End),
    (b"\x1b[200~", Key::PasteStart),
    (b"\x1b[201~", Key::PasteEnd),
];
#[derive(Default)]
struct Keyboard {
    pending: Vec<u8>,
    since: Option<Instant>,
    logs: bool,
    pasting: bool,
    paste_png: crate::client::input::paste::PngFraming,
    paste_last_input: Option<Instant>,
}
impl Keyboard {
    fn image_paste_expired(&self, now: Instant) -> bool {
        self.pasting
            && self.paste_png.is_image_prefix()
            && self.paste_last_input.is_some_and(|last| {
                now.saturating_duration_since(last)
                    >= crate::client::input::paste::IMAGE_IDLE_TIMEOUT
            })
    }

    fn discard_image_paste(&mut self) -> bool {
        if !self.pasting || !self.paste_png.is_image_prefix() {
            return false;
        }
        self.pasting = false;
        self.paste_png = Default::default();
        self.paste_last_input = None;
        self.pending.clear();
        self.since = None;
        true
    }

    fn accepts(&self, key: Key, allow_logs: bool) -> bool {
        if self.pasting {
            return matches!(key, Key::PasteEnd) && self.paste_png.allows_end();
        }
        match key {
            Key::Interrupt
            | Key::Help
            | Key::Questions
            | Key::Select
            | Key::Layout
            | Key::Sidebar
            | Key::Theme
            | Key::Reload
            | Key::Save
            | Key::AuditUp
            | Key::AuditDown
            | Key::Open
            | Key::Back
            | Key::Filter
            | Key::FocusBack
            | Key::Delete
            | Key::Newline
            | Key::HistoryStart
            | Key::HistoryEnd
            | Key::CanvasLeft
            | Key::CanvasRight
            | Key::Mouse { .. } => true,
            Key::Toggle => allow_logs,
            Key::Up | Key::Down | Key::Home | Key::End => true,
            Key::PasteStart => true,
            Key::PasteEnd | Key::Escape => false,
        }
    }
    fn feed(
        &mut self,
        bytes: &[u8],
        now: Instant,
        flush: bool,
        allow_logs: bool,
        force_logs: bool,
    ) -> (Vec<u8>, Vec<Key>) {
        if self.image_paste_expired(now) {
            self.discard_image_paste();
        }
        self.logs |= force_logs;
        let mut output = Vec::new();
        let mut actions = Vec::new();
        for &byte in bytes {
            if self.pasting {
                self.paste_last_input = Some(now);
            }
            if byte == 0x03 && !self.pasting {
                self.pending.clear();
                self.since = None;
                actions.push(Key::Interrupt);
                continue;
            }
            // A fresh ESC starts a new key. Resolve a preceding standalone ESC
            // immediately, even when both keys arrive in one terminal read.
            if byte == 0x1b && !self.pasting && !self.pending.is_empty() {
                if self.logs && self.pending == b"\x1b" {
                    self.logs = force_logs;
                    self.pending.clear();
                    actions.push(Key::Escape);
                } else {
                    output.append(&mut self.pending);
                }
                self.since = None;
            }
            if self.pending.is_empty() && byte != 0x1b {
                if !self.pasting {
                    self.logs = force_logs;
                }
                if self.pasting {
                    self.paste_png.feed(&[byte]);
                }
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
            if !self.pasting && self.pending.starts_with(b"\x1b[<") {
                if matches!(byte, b'M' | b'm') {
                    let values = std::str::from_utf8(&self.pending[3..self.pending.len() - 1])
                        .ok()
                        .map(|text| {
                            text.split(';')
                                .filter_map(|value| value.parse::<u16>().ok())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    if values.len() == 3 {
                        actions.push(Key::Mouse {
                            button: values[0],
                            x: values[1].saturating_sub(1),
                            y: values[2].saturating_sub(1),
                            release: byte == b'm',
                        });
                    }
                    self.pending.clear();
                    self.since = None;
                } else if self.pending.len() > 64 {
                    self.pending.clear();
                    self.since = None;
                } else {
                    self.since.get_or_insert(now);
                }
                continue;
            }
            if let Some((sequence, key)) = KEYS.iter().find(|(sequence, key)| {
                self.accepts(*key, allow_logs) && *sequence == self.pending.as_slice()
            }) {
                match key {
                    Key::Toggle => {
                        self.logs = force_logs || !self.logs;
                        actions.push(*key);
                    }
                    Key::PasteStart => {
                        self.logs = force_logs;
                        self.pasting = true;
                        self.paste_png = Default::default();
                        self.paste_last_input = Some(now);
                        output.extend_from_slice(sequence);
                    }
                    Key::PasteEnd => {
                        self.pasting = false;
                        self.paste_png = Default::default();
                        self.paste_last_input = None;
                        output.extend_from_slice(sequence);
                    }
                    key => actions.push(*key),
                }
                self.pending.clear();
                self.since = None;
            } else if KEYS.iter().any(|(sequence, key)| {
                self.accepts(*key, allow_logs) && sequence.starts_with(&self.pending)
            }) {
                self.since.get_or_insert(now);
            } else {
                if !self.pasting {
                    self.logs = force_logs;
                }
                if self.pasting {
                    self.paste_png.feed(&self.pending);
                }
                output.append(&mut self.pending);
                self.since = None;
            }
        }
        if flush
            && !(self.pasting && self.paste_png.is_image_prefix())
            && self.since.is_some_and(|since| {
                now.saturating_duration_since(since) >= Duration::from_millis(80)
            })
        {
            if self.logs && self.pending == b"\x1b" {
                self.logs = force_logs;
                self.pending.clear();
                actions.push(Key::Escape);
            } else {
                if !self.pasting {
                    self.logs = force_logs;
                }
                if self.pasting {
                    self.paste_png.feed(&self.pending);
                }
                output.append(&mut self.pending);
            }
            self.since = None;
        }
        (output, actions)
    }
}

#[derive(Default)]
struct Replies {
    bytes: Vec<u8>,
    dropped: u64,
}
impl Replies {
    fn push(&mut self, bytes: &[u8]) {
        if self.bytes.len().saturating_add(bytes.len()) <= MAX_REPLIES {
            self.bytes.extend_from_slice(bytes);
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }
}
impl vt100::Callbacks for Replies {
    fn unhandled_escape(
        &mut self,
        _: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        byte: u8,
    ) {
        if i1.is_none() && i2.is_none() && byte == b'Z' {
            self.push(b"\x1b[?1;2c");
        }
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        command: char,
    ) {
        if i2.is_some() {
            return;
        }
        let param = params
            .first()
            .and_then(|values| values.first())
            .copied()
            .unwrap_or(0);
        match (i1, command, param) {
            (None, 'n', 5) => self.push(b"\x1b[0n"),
            (None | Some(b'?'), 'n', 6) => {
                let (row, col) = screen.cursor_position();
                let (rows, cols) = screen.size();
                let private = if i1 == Some(b'?') { "?" } else { "" };
                self.push(
                    format!(
                        "\x1b[{private}{};{}R",
                        row.min(rows.get() - 1) + 1,
                        col.min(cols.get() - 1) + 1
                    )
                    .as_bytes(),
                );
            }
            (None, 'c', 0) => self.push(b"\x1b[?1;2c"),
            (Some(b'>'), 'c', 0) => self.push(b"\x1b[>0;0;0c"),
            (None, 't', 18 | 19) => {
                let (rows, cols) = screen.size();
                self.push(format!("\x1b[8;{};{}t", rows.get(), cols.get()).as_bytes());
            }
            _ => {}
        }
    }
}

struct State {
    context_label: String,
    server_origin: Option<footer::Origin>,
    guide: Option<guide::Guide>,
    account: AccountStatus,
    graphics: images::Graphics,
    media: Vec<images::Placement>,
    geometry: Geometry,
    logs: vt100::Parser,
    events: events::EventView,
    pty: vt100::Parser<Replies>,
    active_session: String,
    inactive: BTreeMap<String, vt100::Parser<Replies>>,
    session_titles: BTreeMap<String, String>,
    panel: PanelSnapshot,
    ui: Ui,
    help: bool,
    selection: bool,
    audit: AuditView,
    caller_tracker: super::render::CallerTracker,
    raw_logs: bool,
    log_query: String,
    log_level: u8,
    filter_editing: bool,
    filter_draft: String,
    filter_offset: usize,
    preview: bool,
    notice: Option<String>,
    actions: Vec<&'static str>,
    previous_cr: bool,
    dirty: bool,
    keyboard: Keyboard,
    conversation: conversation::Conversation,
}
impl State {
    fn new(rows: u16, cols: u16, ui: &Ui) -> Self {
        let geometry = Geometry::new(rows, cols, ui);
        let conversation = conversation::Conversation::default();
        let mut state = Self {
            events: events::EventView::new(),
            context_label: String::new(),
            server_origin: None,
            guide: None,
            account: AccountStatus::Unavailable,
            graphics: images::Graphics::default(),
            media: Vec::new(),
            logs: vt100::Parser::new(
                nonzero(geometry.log_rows),
                nonzero(geometry.left),
                SCROLLBACK_ROWS,
            ),
            pty: vt100::Parser::new_with_callbacks(
                nonzero(geometry.pty_rows),
                nonzero(geometry.left),
                SCROLLBACK_ROWS,
                Replies::default(),
            ),
            active_session: "main".into(),
            inactive: BTreeMap::new(),
            session_titles: BTreeMap::from([("main".into(), "main".into())]),
            geometry,
            panel: PanelSnapshot::default(),
            ui: ui.clone(),
            help: false,
            selection: false,
            audit: AuditView::default(),
            caller_tracker: super::render::CallerTracker::default(),
            raw_logs: ui.log_format == LogFormat::PrettyVerbose || ui.show_raw_json,
            log_query: String::new(),
            log_level: 0,
            filter_editing: false,
            filter_draft: String::new(),
            filter_offset: 0,
            preview: false,
            notice: None,
            actions: Vec::new(),
            previous_cr: false,
            dirty: true,
            keyboard: Keyboard {
                logs: ui.layout == Layout::Logs,
                ..Keyboard::default()
            },
            conversation,
        };
        if ui.log_format == LogFormat::PrettyVerbose || ui.show_raw_json {
            state.chat_initial_logs();
        }
        state
    }
    fn set_context(&mut self, cwd: &str, title: Option<&str>) {
        let project = std::path::Path::new(cwd)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new(cwd))
            .to_string_lossy();
        self.context_label.clear();
        for label in [Some(project.as_ref()), title].into_iter().flatten() {
            let label = panel::safe_text(label);
            let label = label.trim();
            if !label.is_empty() {
                self.context_label.push_str("  /  ");
                self.context_label.push_str(label);
            }
        }
        self.dirty = true;
    }

    fn register_session(&mut self, id: &str, title: &str) {
        if id.is_empty() {
            return;
        }
        if !self.session_titles.contains_key(id) {
            if self.session_titles.len() >= MAX_SESSIONS {
                return;
            }
            self.inactive.insert(
                id.into(),
                vt100::Parser::new_with_callbacks(
                    nonzero(self.geometry.pty_rows),
                    nonzero(self.geometry.left),
                    SCROLLBACK_ROWS,
                    Replies::default(),
                ),
            );
        }
        self.session_titles
            .insert(id.into(), if title.is_empty() { id } else { title }.into());
        self.dirty = true;
    }
    fn select_session(&mut self, id: &str) {
        if id == self.active_session {
            return;
        }
        let Some(next) = self.inactive.remove(id) else {
            return;
        };
        let previous = std::mem::replace(&mut self.pty, next);
        self.inactive.insert(
            std::mem::replace(&mut self.active_session, id.into()),
            previous,
        );
        self.keyboard.logs = self.ui.layout == Layout::Logs;
        self.logs.screen_mut().set_scrollback(0);
        self.dirty = true;
    }
    fn close_session(&mut self, id: &str) {
        if id == "main" {
            return;
        }
        if id == self.active_session {
            self.select_session("main");
        }
        self.inactive.remove(id);
        self.session_titles.remove(id);
        self.dirty = true;
    }
    fn pty_session(&mut self, id: &str, bytes: &[u8]) {
        if id == self.active_session {
            self.pty.process(bytes);
            self.dirty = true;
        } else if let Some(parser) = self.inactive.get_mut(id) {
            parser.process(bytes);
        }
    }
    fn take_session_replies(&mut self) -> Vec<(String, Vec<u8>)> {
        let mut replies = Vec::new();
        let bytes = std::mem::take(&mut self.pty.callbacks_mut().bytes);
        if !bytes.is_empty() {
            replies.push((self.active_session.clone(), bytes));
        }
        for (id, parser) in &mut self.inactive {
            let bytes = std::mem::take(&mut parser.callbacks_mut().bytes);
            if !bytes.is_empty() {
                replies.push((id.clone(), bytes));
            }
        }
        replies
    }
    fn image(&mut self, preview: images::Preview, mime: &str) {
        let text = format!(
            "\n[IMAGE] {} × {} · {}\n  Image attachment\n\n",
            preview.width, preview.height, mime
        );
        self.log(text.as_bytes());
        self.events.attach_image(preview);
    }

    fn log(&mut self, bytes: &[u8]) {
        self.events.ingest(bytes);
        self.log_raw(bytes);
    }
    fn log_message(&mut self, role: &str, text: &str, bytes: &[u8]) {
        self.log_event(
            "message",
            &serde_json::json!({"type":"ssmcp.message", "role":role, "text":text}),
            bytes,
        );
    }
    fn log_event(&mut self, label: &str, value: &serde_json::Value, bytes: &[u8]) {
        self.events.ingest_event(label, value, bytes);
        self.log_raw(bytes);
    }
    fn log_raw(&mut self, bytes: &[u8]) {
        let mut normalized = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if byte == b'\n' && !self.previous_cr {
                normalized.push(b'\r');
            }
            normalized.push(byte);
            self.previous_cr = byte == b'\r';
        }
        self.logs.process(&normalized);
        self.dirty = true;
    }
    fn resize(&mut self, rows: u16, cols: u16) {
        let geometry = Geometry::new(rows, cols, &self.ui);
        if self.geometry != geometry {
            self.logs
                .set_size(nonzero(geometry.log_rows), nonzero(geometry.left));
            self.pty
                .set_size(nonzero(geometry.pty_rows), nonzero(geometry.left));
            for parser in self.inactive.values_mut() {
                parser.set_size(nonzero(geometry.pty_rows), nonzero(geometry.left));
            }
            self.geometry = geometry;
            if geometry.log_rows == 0 {
                self.keyboard.logs = false;
                self.logs.screen_mut().set_scrollback(0);
            }
            self.dirty = true;
        }
    }
    fn scroll_logs(&mut self, lines: isize) {
        if !self.log_query.is_empty() || self.log_level != 0 {
            let max = self.logs.screen().scrollback_len() + usize::from(self.geometry.log_rows);
            self.filter_offset = if lines >= 0 {
                self.filter_offset
                    .saturating_add(lines.unsigned_abs())
                    .min(max)
            } else {
                self.filter_offset.saturating_sub(lines.unsigned_abs())
            };
        } else {
            let screen = self.logs.screen_mut();
            let next = if lines >= 0 {
                screen.scrollback().saturating_add(lines.unsigned_abs())
            } else {
                screen.scrollback().saturating_sub(lines.unsigned_abs())
            };
            screen.set_scrollback(next);
        }
        self.notice = None;
        self.dirty = true;
    }

    /// Filter whole retained display blocks so the matching command keeps its output.
    fn filtered_logs(&mut self) -> Option<Vec<String>> {
        if self.log_query.is_empty() && self.log_level == 0 {
            return None;
        }
        let g = self.geometry;
        let current = self.logs.screen().scrollback();
        let history = self.logs.screen().scrollback_len();
        let visible = usize::from(self.logs.screen().size().0.get());
        let query = self.log_query.to_lowercase();
        let mut groups: Vec<(String, Vec<String>, Option<String>)> = Vec::new();
        let mut group_index = 0usize;
        for index in 0..history + visible {
            self.logs
                .screen_mut()
                .set_scrollback(history.saturating_sub(index));
            let row = index.saturating_sub(history) as u16;
            let plain = self
                .logs
                .screen()
                .rows(0, g.left)
                .nth(usize::from(row))
                .unwrap_or_default();
            let ansi = screen_row(self.logs.screen(), row, g.left, &self.ui);
            let trimmed = plain.trim_start();
            let heading = trimmed.starts_with('[');
            let id = if heading {
                trimmed
                    .split_once('#')
                    .map(|(_, tail)| {
                        tail.chars()
                            .take_while(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                            .collect::<String>()
                    })
                    .filter(|id| !id.is_empty())
            } else {
                None
            };
            let lower = trimmed.to_lowercase();
            let start = ["[run", "[exec", "[cmd"]
                .iter()
                .any(|tag| lower.starts_with(tag));
            if let Some(index) = id
                .as_ref()
                .and_then(|id| groups.iter().position(|group| group.2.as_ref() == Some(id)))
            {
                group_index = index;
            } else if groups.is_empty()
                || start
                || (heading && id.is_some())
                || (heading
                    && !["[out", "[stdout", "[stderr", "[ok", "[err", "[result"]
                        .iter()
                        .any(|tag| lower.starts_with(tag)))
            {
                group_index = groups.len();
                groups.push((String::new(), Vec::new(), id));
            }
            let (text, rows, _) = &mut groups[group_index];
            text.push_str(&plain);
            text.push('\n');
            rows.push(ansi);
        }
        self.logs.screen_mut().set_scrollback(current);
        let mut rows = Vec::new();
        for (text, lines, _) in groups {
            let lower = text.to_lowercase();
            let level_matches = match self.log_level {
                1 => [
                    "[run", "[exec", "[cmd", "[ok", "[err", "[out", "[stdout", "[stderr", "[result",
                ]
                .iter()
                .any(|tag| lower.trim_start().starts_with(tag)),
                2 => {
                    ["[err", "[warn", "[stderr"]
                        .iter()
                        .any(|tag| lower.contains(tag))
                        || lower.contains("exit=1")
                        || lower.contains("failed")
                }
                _ => true,
            };
            if level_matches && lower.contains(&query) {
                rows.extend(lines);
            }
        }
        let height = usize::from(g.log_rows);
        self.filter_offset = self.filter_offset.min(rows.len().saturating_sub(height));
        let end = rows.len().saturating_sub(self.filter_offset);
        let start = end.saturating_sub(height);
        let mut result = rows[start..end].to_vec();
        if result.is_empty() && height > 0 {
            result.push(self.tone(
                PanelTone::Muted,
                &panel::fit(
                    "No matching log entries · / Edit filter · Esc Clear",
                    usize::from(g.left),
                ),
            ));
        }
        result.resize_with(height, || {
            self.tone(PanelTone::Normal, &" ".repeat(usize::from(g.left)))
        });
        Some(result)
    }

    fn paste_input_progress(&mut self, now: Instant) {
        if self.keyboard.pasting && self.keyboard.paste_png.is_image_prefix() {
            self.keyboard.paste_last_input = Some(
                self.keyboard
                    .paste_last_input
                    .map_or(now, |last| last.max(now)),
            );
        }
        self.graphics.paste_input_progress(now);
    }

    fn discard_image_paste(&mut self) {
        let discarded = self.keyboard.discard_image_paste() | self.graphics.discard_image_paste();
        if discarded {
            self.chat_discard_image_paste();
            self.dirty = true;
        }
    }

    fn keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        if self.keyboard.image_paste_expired(now) {
            self.discard_image_paste();
        }
        // Graphics may retain a split framing marker. Every layer uses the same
        // raw input progress, so it cannot expire while another still buffers it.
        if !bytes.is_empty() && self.keyboard.pasting {
            self.keyboard.paste_last_input = Some(now);
        }
        let filtered = self.graphics.input(bytes, now, flush);
        for id in self.graphics.take_rejected_images() {
            self.events.reject_image(id);
        }
        self.dirty |= self.graphics.take_changed();
        let bytes = filtered.as_slice();
        if self.ui.layout == Layout::Chat {
            return self.chat_keyboard(bytes, now, flush);
        }
        let old_focus = self.keyboard.logs;
        let showing_help = self.help;
        let (output, actions) = self.keyboard.feed(
            bytes,
            now,
            flush,
            self.geometry.log_rows > 0,
            self.ui.layout == Layout::Logs,
        );
        if !self.preview && (!output.is_empty() || !actions.is_empty()) {
            crate::client::interface::computer::local_input();
        }
        let mut layout_changed = false;
        for action in actions {
            if matches!(action, Key::Interrupt) {
                request_interrupt();
                return Vec::new();
            }
            if matches!(action, Key::Escape) && showing_help {
                self.help = false;
                self.keyboard.logs = old_focus;
                self.dirty = true;
                continue;
            }
            if matches!(action, Key::Help) {
                self.help = !self.help;
                self.dirty = true;
                continue;
            }
            let ui_action = match action {
                Key::Layout => Some("layout.next"),
                Key::Sidebar => Some("sidebar.toggle"),
                Key::Theme => Some("theme.next"),
                Key::Reload => Some("reload"),
                Key::Save => Some("save"),
                _ => None,
            };
            if let Some(action) = ui_action {
                // Display actions are applied after this input read. Never send
                // coalesced text to a shell that may have just become hidden.
                layout_changed |= matches!(action, "layout.next" | "reload");
                if self.actions.len() < 64 {
                    self.actions.push(action);
                }
                continue;
            }
            match action {
                Key::Mouse {
                    button,
                    x,
                    y,
                    release,
                } => {
                    if !release && x < self.geometry.left && !self.help {
                        if y < self.geometry.log_rows {
                            if button & 64 != 0 {
                                self.scroll_logs(if button & 1 == 0 { 3 } else { -3 });
                            }
                        } else {
                            match button {
                                64 => self.audit.scroll_by(-3),
                                65 => self.audit.scroll_by(3),
                                0 => self.audit.back(),
                                2 => self.audit.open(),
                                _ => {}
                            }
                        }
                        self.dirty = true;
                    }
                    continue;
                }
                Key::AuditUp if self.ui.layout != Layout::Logs => {
                    self.audit.up();
                    self.dirty = true;
                    continue;
                }
                Key::AuditDown if self.ui.layout != Layout::Logs => {
                    self.audit.down();
                    self.dirty = true;
                    continue;
                }
                Key::AuditUp => {
                    self.scroll_logs(1);
                    continue;
                }
                Key::AuditDown => {
                    self.scroll_logs(-1);
                    continue;
                }
                Key::Open => {
                    self.audit.open();
                    self.dirty = true;
                    continue;
                }
                Key::Back => {
                    self.audit.back();
                    self.dirty = true;
                    continue;
                }
                Key::Filter => {
                    self.log_level = (self.log_level + 1) % 3;
                    self.filter_offset = 0;
                    self.notice = None;
                    self.dirty = true;
                    continue;
                }
                Key::Escape if self.filter_editing => {
                    self.filter_editing = false;
                    self.dirty = true;
                    continue;
                }
                Key::Escape => {
                    self.audit.back();
                    self.log_query.clear();
                    self.log_level = 0;
                    self.filter_offset = 0;
                    self.dirty = true;
                    continue;
                }
                _ => {}
            }
            if matches!(action, Key::Up | Key::Down | Key::Home | Key::End) {
                if self.ui.layout == Layout::Audit {
                    let height = usize::from(self.geometry.pty_rows).saturating_sub(1);
                    match action {
                        Key::Up => self.audit.page_up(height),
                        Key::Down => self.audit.page_down(height),
                        Key::Home => self.audit.scroll_by(isize::MIN),
                        Key::End => self.audit.scroll_by(isize::MAX),
                        _ => {}
                    }
                    self.dirty = true;
                    continue;
                }
                let page = self.geometry.log_rows.saturating_sub(1).max(1) as isize;
                match action {
                    Key::Up => self.scroll_logs(page),
                    Key::Down => self.scroll_logs(-page),
                    Key::Home => self.scroll_logs(isize::MAX),
                    Key::End => self.scroll_logs(isize::MIN),
                    _ => {}
                }
                continue;
            }
            self.dirty = true;
        }

        if old_focus != self.keyboard.logs {
            self.dirty = true;
        }
        if showing_help && output == b"\x1b" {
            self.help = false;
            self.dirty = true;
        }
        if !showing_help && !self.help && !layout_changed {
            for character in String::from_utf8_lossy(&output).chars() {
                if self.filter_editing {
                    match character {
                        '\r' | '\n' => {
                            self.log_query = self.filter_draft.clone();
                            self.filter_editing = false;
                            self.filter_offset = 0;
                        }
                        '\u{1b}' => self.filter_editing = false,
                        '\u{7f}' | '\u{8}' => {
                            self.filter_draft.pop();
                        }
                        c if !c.is_control() && self.filter_draft.len() < 256 => {
                            self.filter_draft.push(c)
                        }
                        _ => {}
                    }
                    self.dirty = true;
                } else {
                    match character {
                        '/' => {
                            self.filter_editing = true;
                            self.filter_draft = self.log_query.clone();
                            self.notice = None;
                            self.dirty = true;
                        }
                        '\r' | '\n' => {
                            self.audit.open();
                            self.dirty = true;
                        }
                        '\u{1b}' => {
                            self.audit.back();
                            self.log_query.clear();
                            self.log_level = 0;
                            self.filter_offset = 0;
                            self.dirty = true;
                        }
                        _ => {}
                    }
                }
            }
        }
        Vec::new()
    }

    fn should_redraw(
        &self,
        previous: Option<&Frame>,
        previous_cell: Option<images::CellSize>,
    ) -> bool {
        let pixels_changed = self.graphics.cell() != previous_cell;
        let frozen = self.selection
            && !pixels_changed
            && previous.is_some_and(|old| !old.mouse_capture && old.geometry == self.geometry);
        (self.dirty || pixels_changed) && !frozen
    }

    fn frame(&mut self, now: Instant) -> Frame {
        self.media.clear();
        self.events
            .image_cells(self.graphics.cell().filter(|_| self.ui.color));
        if self.ui.layout == Layout::Chat {
            let frame = self.chat_frame(now);
            if !self.help
                && self.conversation.view == conversation::View::Activity
                && !self.raw_logs
            {
                self.media.extend(
                    self.events
                        .image_placements(conversation::body_top(self.geometry.rows)),
                );
            }
            return frame;
        }
        let g = self.geometry;
        let filtered = self.filtered_logs();
        let audit_rows = self.audit.render(g.left, g.pty_rows, &self.ui);
        let layout = PanelLayout::configured(
            g.cols,
            g.rows,
            self.ui.sidebar && !self.ui.panels.is_empty(),
            self.ui.sidebar_width,
        );
        let sidebar = layout.sidebar.map(|rect| {
            let height = rect.height.saturating_sub(u16::from(g.rows >= 3));
            let footer = self.guide_rows(rect.width, height, rect.row, rect.col);
            let mut panel: Vec<String> = panel::render_configured(
                &self.panel,
                rect.width,
                height.saturating_sub(footer.len() as u16),
                now,
                &self.ui,
            )
            .into_iter()
            .map(|item| {
                let text = fit(&item.text, rect.width);
                let padded = format!(
                    "{text}{}",
                    " ".repeat(usize::from(rect.width).saturating_sub(text.width()))
                );
                self.tone(item.tone, &padded)
            })
            .collect();
            panel.extend(footer);
            (rect, panel)
        });
        let mut rows = Vec::with_capacity(usize::from(g.rows));
        for row in 0..g.rows {
            if g.rows >= 3 && row == g.rows - 1 {
                let level = ["all", "commands", "errors"][usize::from(self.log_level)];
                let hint = if self.filter_editing {
                    format!(" / {}_   Enter Apply · Esc Cancel ", self.filter_draft)
                } else if let Some(notice) = &self.notice {
                    format!(" {notice} · F1 Help ")
                } else if !self.log_query.is_empty() || self.log_level != 0 {
                    format!(
                        " Logs: {level} /{} · / Edit · Esc Clear · Wheel Scroll · ↑↓ Audit · Enter Open ",
                        self.log_query
                    )
                } else if g.cols < 100 {
                    " Wheel Logs · ↑↓ Audit · Enter/→ Open · ← Back · / Filter · F1 Help ".into()
                } else {
                    " Wheel / PgUp Logs · ↑↓ Audit · Enter/→ Open · ← Back · / Filter · F9 Level · F1 Help · F4 Theme ".into()
                };
                let connection = footer::Connection::from_snapshot(self.preview, &self.chat_data());
                let text = footer::render(
                    &hint,
                    self.server_origin.as_ref(),
                    connection,
                    g.cols,
                    self.notice.is_some() || self.filter_editing,
                );
                rows.push(self.tone(
                    if self.keyboard.logs {
                        PanelTone::Accent
                    } else {
                        PanelTone::Muted
                    },
                    &text,
                ));
                continue;
            }
            let mut line = if row < g.log_rows {
                filtered
                    .as_ref()
                    .and_then(|lines| lines.get(usize::from(row)))
                    .cloned()
                    .unwrap_or_else(|| screen_row(self.logs.screen(), row, g.left, &self.ui))
            } else if row == g.log_rows && g.log_rows > 0 {
                let title = " CODE_MODE  ·  ↑↓ Browse  ·  Enter/→ Details  ·  ← Back ";
                let text = fit(title, g.left);
                let padding = usize::from(g.left).saturating_sub(text.width());
                self.tone(PanelTone::Muted, &format!("{text}{}", "─".repeat(padding)))
            } else {
                audit_rows
                    .get(usize::from(row.saturating_sub(g.pty_top)))
                    .cloned()
                    .unwrap_or_else(|| {
                        self.tone(PanelTone::Normal, &" ".repeat(usize::from(g.left)))
                    })
            };
            if let Some((rect, panel)) = &sidebar {
                line.push_str(&self.tone(PanelTone::Muted, "│"));
                line.push_str(&panel[usize::from(row - rect.row)]);
            }
            rows.push(line);
        }
        if self.help {
            self.media.clear();
            let mut lines = vec![
                "SSMCP  /  Help & appearance".to_string(),
                String::new(),
                format!(
                    "Theme  {}     View  {}",
                    self.ui.theme.name(),
                    self.ui.layout.name()
                ),
                format!(
                    "Logs  {}%     Sidebar  {} · {} columns",
                    self.ui.log_height_percent,
                    if self.ui.sidebar { "shown" } else { "hidden" },
                    self.ui.sidebar_width
                ),
                String::new(),
                "F1 / Esc   Close this view".into(),
                "F2         Switch split / audit / logs".into(),
                "F3         Show or hide the sidebar".into(),
                "F4         Cycle color themes".into(),
                "F5         Reload your configuration".into(),
                "Wheel      Scroll logs directly; PgUp / PgDn also work".into(),
                "Up / Down  Select a code_mode call; Enter / Right opens".into(),
                "Left       Return to the audit list".into(),
                "/          Filter logs by text; F9 cycles all / commands / errors".into(),
                "F8         Save the current appearance".into(),
                String::new(),
                "Edit colors, panel order and sizes in your TOML file.".into(),
                "ssmcp ui show     ssmcp ui check     ssmcp ui preview".into(),
                String::new(),
                self.notice.clone().unwrap_or_else(|| {
                    "Appearance changes keep running tasks and audit history.".into()
                }),
            ];
            if let Some(guide) = &self.guide {
                let mut links = vec!["Online guide".to_owned()];
                links.extend(
                    guide
                        .url_lines(g.cols.saturating_sub(4))
                        .into_iter()
                        .map(str::to_owned),
                );
                links.push(String::new());
                lines.splice(2..2, links);
            }
            for (row, line) in rows.iter_mut().enumerate() {
                let content = lines
                    .get(row.saturating_sub(1))
                    .filter(|_| row > 0)
                    .map(String::as_str)
                    .unwrap_or("");
                let text = fit(&format!("  {content}"), g.cols);
                let padded = format!(
                    "{text}{}",
                    " ".repeat(usize::from(g.cols).saturating_sub(text.width()))
                );
                *line = self.tone(
                    if row == 1 {
                        PanelTone::Heading
                    } else {
                        PanelTone::Normal
                    },
                    &padded,
                );
            }
        }
        let screen = self.pty.screen();
        let (row, col) = screen.cursor_position();
        Frame {
            geometry: g,
            rows,
            cursor: (g.pty_top + row.min(g.pty_rows - 1), col.min(g.left - 1)),
            hidden: true,
            application_cursor: false,
            application_keypad: false,
            bracketed_paste: true,
            mouse_capture: !self.selection,
        }
    }
    fn guide_rows(&mut self, width: u16, height: u16, _row: u16, _col: u16) -> Vec<String> {
        let mut rows = Vec::new();
        // Keep eight rows for the configured panels before reserving account space.
        if height >= 10 && width >= 20 {
            for text in self.account.rows() {
                let text = fit(&text, width);
                let padded = format!(
                    "{text}{}",
                    " ".repeat(usize::from(width).saturating_sub(text.width()))
                );
                rows.push(self.tone(PanelTone::Muted, &padded));
            }
        }
        rows.extend(guide::pet_rows(
            width,
            height.saturating_sub(rows.len() as u16),
            &self.ui,
        ));
        rows
    }
    fn tone(&self, tone: PanelTone, text: &str) -> String {
        if !self.ui.color {
            return text.into();
        }
        let semantic = match tone {
            PanelTone::Normal => "foreground",
            PanelTone::Muted => "muted",
            PanelTone::Heading => "heading",
            PanelTone::Accent => "accent",
            PanelTone::Success => "success",
            PanelTone::Warning => "warning",
            PanelTone::Error => "error",
        };
        format!(
            "\x1b[{};{}m{text}\x1b[0m",
            self.ui.background(),
            self.ui.color(semantic)
        )
    }
}

fn fit(text: &str, columns: u16) -> String {
    let mut output = String::new();
    let mut width = 0;
    for character in text.chars().filter(|character| !character.is_control()) {
        let cells = character.width().unwrap_or(0);
        if width + cells > usize::from(columns) {
            break;
        }
        output.push(character);
        width += cells;
    }
    output
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Style {
    fg: vt100::Color,
    bg: vt100::Color,
    flags: u8,
}
impl Style {
    fn of(cell: &vt100::Cell, color: bool) -> Self {
        Self {
            fg: if color {
                cell.fgcolor()
            } else {
                vt100::Color::Default
            },
            bg: if color {
                cell.bgcolor()
            } else {
                vt100::Color::Default
            },
            flags: u8::from(cell.bold())
                | (u8::from(cell.dim()) << 1)
                | (u8::from(cell.italic()) << 2)
                | (u8::from(cell.underline()) << 3)
                | (u8::from(cell.inverse()) << 4),
        }
    }
    fn append(self, text: &mut String) {
        text.push_str("\x1b[0");
        for (mask, code) in [(1, 1), (2, 2), (4, 3), (8, 4), (16, 7)] {
            if self.flags & mask != 0 {
                let _ = write!(text, ";{code}");
            }
        }
        for (color, prefix) in [(self.fg, 38), (self.bg, 48)] {
            match color {
                vt100::Color::Default => {}
                vt100::Color::Idx(index) => {
                    let _ = write!(text, ";{prefix};5;{index}");
                }
                vt100::Color::Rgb(r, g, b) => {
                    let _ = write!(text, ";{prefix};2;{r};{g};{b}");
                }
            }
        }
        text.push('m');
    }
}

// Serialize cells ourselves. rows_formatted may include cursor/erase sequences
// relative to its own screen; forwarding those would escape the pane.
fn screen_row(screen: &vt100::Screen, row: u16, width: u16, ui: &Ui) -> String {
    let mut text = String::from("\x1b[0m");
    if ui.color {
        let _ = write!(text, "\x1b[{};{}m", ui.background(), ui.color("foreground"));
    }
    let mut style = Style::default();
    let mut col = 0;
    let mut padding = 0;
    if ui.layout == Layout::Chat {
        let prefix: String = (0..width.min(12))
            .filter_map(|index| screen.cell(row, index))
            .map(|cell| cell.contents())
            .collect();
        if let Some(close) = prefix.find(']') {
            let replacement = match prefix[..=close].trim_matches(['[', ']']).trim() {
                "RUN" | "CALL" => Some(("  ●", "accent")),
                "OK" | "READY" => Some(("  ●", "success")),
                "ERR" | "ERROR" | "FAIL" => Some(("  ●", "error")),
                "USER" => Some(("  ❯", "foreground")),
                "STDOUT" => Some(("  stdout", "muted")),
                "STDERR" => Some(("  stderr", "warning")),
                _ => None,
            };
            if prefix.starts_with('[')
                && let Some((label, semantic)) = replacement
                && label.width() <= close + 1
            {
                if ui.color {
                    let _ = write!(
                        text,
                        "\x1b[{}m{label}\x1b[{}m",
                        ui.color(semantic),
                        ui.color("foreground")
                    );
                } else {
                    text.push_str(label);
                }
                col = u16::try_from(close + 1).unwrap_or(0);
                padding = close + 1 - label.width();
            }
        }
    }
    while col < width {
        let Some(cell) = screen.cell(row, col) else {
            text.push(' ');
            col += 1;
            continue;
        };
        let next_style = Style::of(cell, ui.color);
        if next_style != style {
            next_style.append(&mut text);
            if ui.color {
                if next_style.fg == vt100::Color::Default {
                    let _ = write!(text, "\x1b[{}m", ui.color("foreground"));
                }
                if next_style.bg == vt100::Color::Default {
                    let _ = write!(text, "\x1b[{}m", ui.background());
                }
            }
            style = next_style;
        }
        if cell.is_wide_continuation()
            || !cell.has_contents()
            || (cell.is_wide() && col + 1 >= width)
        {
            text.push(' ');
            col += 1;
            continue;
        }
        for character in cell.contents().chars().filter(|c| !c.is_control()) {
            text.push(character);
        }
        col += if cell.is_wide() { 2 } else { 1 };
    }
    if padding > 0 {
        let _ = write!(
            text,
            "\x1b[0m\x1b[{}m{}",
            if ui.color {
                ui.background()
            } else {
                "49".into()
            },
            " ".repeat(padding)
        );
    }
    text.push_str("\x1b[0m");
    text
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Frame {
    geometry: Geometry,
    rows: Vec<String>,
    cursor: (u16, u16),
    hidden: bool,
    application_cursor: bool,
    application_keypad: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}
impl ztui::Frame for Frame {
    fn size(&self) -> ztui::Size {
        ztui::Size {
            rows: self.geometry.rows,
            columns: self.geometry.cols,
        }
    }

    fn row(&self, row: u16) -> &str {
        self.rows
            .get(usize::from(row))
            .map(String::as_str)
            .unwrap_or("")
    }

    fn cursor(&self) -> ztui::Cursor {
        ztui::Cursor {
            position: ztui::Point {
                row: self.cursor.0,
                column: self.cursor.1,
            },
            visible: !self.hidden,
        }
    }

    fn modes(&self) -> ztui::Modes {
        ztui::Modes {
            application_cursor: self.application_cursor,
            application_keypad: self.application_keypad,
            bracketed_paste: self.bracketed_paste,
            mouse_capture: self.mouse_capture,
        }
    }

    fn same_layout(&self, previous: &Self) -> bool {
        self.geometry == previous.geometry
    }
}

impl Frame {
    fn diff(&self, previous: Option<&Self>) -> Vec<u8> {
        let mut output = String::new();
        ztui::write_diff(&mut output, self, previous)
            .expect("writing an ANSI frame into String cannot fail");
        output.into_bytes()
    }
}

// Keep row diffs and native-image changes in one terminal presentation, even
// when nonblocking output splits the bytes across several drain attempts.
fn append_frame(
    output: &mut Vec<u8>,
    frame: &Frame,
    previous: Option<&Frame>,
    before: &str,
    after: &str,
) {
    let diff = frame.diff(previous);
    if previous.is_some() && before.is_empty() && diff.is_empty() && after.is_empty() {
        return;
    }
    output.extend_from_slice(SYNC_BEGIN);
    if previous.is_none() {
        output.extend_from_slice(ENTER);
    }
    output.extend_from_slice(before.as_bytes());
    output.extend_from_slice(&diff);
    output.extend_from_slice(after.as_bytes());
    output.extend_from_slice(SYNC_END);
}

struct RestoringOutput<'a> {
    shared: &'a Shared,
    output: PhysicalOutput,
    wrote: bool,
    media: images::Renderer,
}
impl Write for RestoringOutput<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let count = self.output.write(bytes)?;
        self.wrote |= count > 0;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
impl Drop for RestoringOutput<'_> {
    fn drop(&mut self) {
        // This guard also runs if parsing or drawing panics. Restoration is
        // bounded even when an enclosing PTY has stopped draining its output.
        let until = Instant::now() + Duration::from_millis(250);
        let mut leave = self.media.clear();
        leave.extend_from_slice(LEAVE);
        let mut offset = 0;
        while self.wrote && offset < leave.len() && Instant::now() < until {
            if drain(&mut self.output, &leave, &mut offset).is_err() {
                break;
            }
            if offset < leave.len() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let _ = self.output.flush();
        self.shared.alive.store(false, Ordering::Release);
    }
}

fn draw_loop(shared: &Shared, output: PhysicalOutput) {
    let mut output = RestoringOutput {
        shared,
        output,
        wrote: false,
        media: images::Renderer::default(),
    };
    let mut pending = Vec::new();
    let mut offset = 0;
    let mut previous = None;
    let mut previous_cell = None;
    let mut last_panel = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        if offset >= pending.len() {
            output.media.complete();
            let (refresh, requests) = {
                let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
                if last_panel.elapsed() >= Duration::from_secs(1) {
                    state.dirty = true;
                    last_panel = Instant::now();
                }
                // Selection freezes normal updates. Grid or font-cell changes
                // still redraw so images cannot retain their old pixel bounds.
                let cell = state.graphics.cell();
                let refresh = if state.should_redraw(previous.as_ref(), previous_cell) {
                    let frame = state.frame(Instant::now());
                    let media = std::mem::take(&mut state.media);
                    state.dirty = false;
                    previous_cell = cell;
                    Some((frame, media))
                } else {
                    None
                };
                (refresh, state.graphics.take_requests())
            };
            pending = requests;
            if let Some((frame, media)) = refresh {
                let (before, after) = output.media.update(media, frame.cursor, frame.hidden);
                append_frame(&mut pending, &frame, previous.as_ref(), &before, &after);
                previous = Some(frame);
            }
            offset = 0;
        }
        match drain(&mut output, &pending, &mut offset) {
            Ok(()) => {}
            Err(_) => break,
        }
        std::thread::sleep(FRAME_INTERVAL);
    }
    // RestoringOutput restores alternate-screen/input state and marks the
    // display inactive only after the worker has finished its bounded cleanup.
}

fn drain(output: &mut impl Write, bytes: &[u8], offset: &mut usize) -> io::Result<()> {
    let end = bytes.len().min(offset.saturating_add(256 * 1024));
    while *offset < end {
        match output.write(&bytes[*offset..end]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "terminal output closed",
                ));
            }
            Ok(count) => *offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn owns_terminal() -> bool {
    use std::mem::MaybeUninit;
    let mut input = MaybeUninit::<libc::stat>::uninit();
    let mut output = MaybeUninit::<libc::stat>::uninit();
    // SAFETY: inspect borrowed descriptors and write into valid stat storage.
    unsafe {
        if libc::tcgetpgrp(libc::STDIN_FILENO) != libc::getpgrp()
            || libc::fstat(libc::STDIN_FILENO, input.as_mut_ptr()) != 0
            || libc::fstat(libc::STDERR_FILENO, output.as_mut_ptr()) != 0
        {
            return false;
        }
        let input = input.assume_init();
        let output = output.assume_init();
        input.st_dev == output.st_dev
            && input.st_ino == output.st_ino
            && input.st_rdev == output.st_rdev
    }
}
#[cfg(not(unix))]
fn owns_terminal() -> bool {
    true
}

#[cfg(unix)]
fn physical_size() -> Option<(u16, u16)> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl writes a winsize into valid aligned storage.
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } != 0 {
        return None;
    }
    let size = unsafe { size.assume_init() };
    (size.ws_row > 0 && size.ws_col > 0).then_some((size.ws_row, size.ws_col))
}
#[cfg(windows)]
fn physical_size() -> Option<(u16, u16)> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
    };
    let mut info = std::mem::MaybeUninit::<CONSOLE_SCREEN_BUFFER_INFO>::zeroed();
    // SAFETY: the borrowed console handle and output storage remain valid.
    if unsafe { GetConsoleScreenBufferInfo(GetStdHandle(STD_ERROR_HANDLE), info.as_mut_ptr()) } == 0
    {
        return None;
    }
    let info = unsafe { info.assume_init() };
    Some((
        (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16,
        (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16,
    ))
}

struct PhysicalOutput {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(windows)]
    stderr: io::Stderr,
    #[cfg(windows)]
    handle: isize,
    #[cfg(windows)]
    saved_mode: u32,
}
impl PhysicalOutput {
    #[cfg(unix)]
    fn open() -> io::Result<Self> {
        use std::{
            ffi::CStr,
            os::fd::{AsRawFd, FromRawFd},
        };
        let mut name = [0 as libc::c_char; 4096];
        // SAFETY: ttyname_r writes a bounded NUL-terminated terminal path.
        let error = unsafe { libc::ttyname_r(libc::STDERR_FILENO, name.as_mut_ptr(), name.len()) };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let name = unsafe { CStr::from_ptr(name.as_ptr()) };
        // Reopen, never set O_NONBLOCK on a dup which shares the parent's flags.
        let fd = unsafe {
            libc::open(
                name.as_ptr(),
                libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut before = std::mem::MaybeUninit::<libc::stat>::uninit();
        let mut after = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: both descriptors are open and stat destinations are valid.
        if unsafe { libc::fstat(libc::STDERR_FILENO, before.as_mut_ptr()) } != 0
            || unsafe { libc::fstat(file.as_raw_fd(), after.as_mut_ptr()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let (before, after) = unsafe { (before.assume_init(), after.assume_init()) };
        if (before.st_dev, before.st_ino, before.st_rdev)
            != (after.st_dev, after.st_ino, after.st_rdev)
        {
            return Err(io::Error::other("terminal changed while opening display"));
        }
        Ok(Self { file })
    }
    #[cfg(windows)]
    fn open() -> io::Result<Self> {
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE,
            SetConsoleMode,
        };
        let handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
        let mut saved_mode = 0;
        // SAFETY: GetStdHandle returned the borrowed stderr console handle.
        if unsafe { GetConsoleMode(handle, &mut saved_mode) } == 0
            || unsafe { SetConsoleMode(handle, saved_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) }
                == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            stderr: io::stderr(),
            handle: handle as isize,
            saved_mode,
        })
    }
}
impl Write for PhysicalOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.file.write(bytes)
        }
        #[cfg(windows)]
        {
            self.stderr.write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.file.flush()
        }
        #[cfg(windows)]
        {
            self.stderr.flush()
        }
    }
}
#[cfg(windows)]
impl Drop for PhysicalOutput {
    fn drop(&mut self) {
        // SAFETY: the borrowed console handle remains valid for the client lifetime.
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleMode(self.handle as _, self.saved_mode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_preview_uses_the_shared_sidebar_and_preserves_the_ascii_pet() {
        for layout in [Layout::Chat, Layout::Split, Layout::Logs] {
            let mut ui = Ui {
                color: false,
                layout,
                sidebar: true,
                ..Ui::default()
            };
            let mut screen = vt100::Parser::new(nonzero(30), nonzero(120), 0);
            screen.process(&preview_bytes(&ui, 30, 120));
            let contents = screen.screen().contents();
            assert!(contents.contains("账号  example"), "{layout:?}: {contents}");
            assert!(
                contents.contains("本月剩余  0.75 GB"),
                "{layout:?}: {contents}"
            );
            assert!(contents.contains("( o.o )"), "{layout:?}: {contents}");
            ui.sidebar = false;
            let mut hidden = vt100::Parser::new(nonzero(30), nonzero(120), 0);
            hidden.process(&preview_bytes(&ui, 30, 120));
            assert!(!hidden.screen().contents().contains("账号"));
        }
    }

    fn split_ui() -> Ui {
        Ui {
            layout: Layout::Split,
            sidebar: true,
            sidebar_width: 30,
            ..Ui::default()
        }
    }

    fn physical(frame: &Frame) -> vt100::Parser {
        let mut parser = vt100::Parser::new(
            nonzero(frame.geometry.rows),
            nonzero(frame.geometry.cols),
            0,
        );
        parser.process(&frame.diff(None));
        parser
    }
    #[test]
    fn rejected_native_image_keeps_other_images_and_future_uploads_available() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([30, 120, 210, 255]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let block = serde_json::json!({
            "type":"image", "mimeType":"image/png",
            "data":STANDARD.encode(bytes.into_inner()),
        });
        let ui = Ui {
            layout: Layout::Chat,
            color: true,
            sidebar: false,
            ..Ui::default()
        };
        let mut state = State::new(40, 100, &ui);
        state.graphics = images::Graphics::probe_for_test();
        let now = Instant::now();
        assert!(
            state
                .keyboard(b"\x1b_Gi=4294967295;OK\x1b\\\x1b[6;16;8t", now, false)
                .is_empty()
        );
        let rejected = images::decode(&block, state.graphics.prepare_native()).unwrap();
        let rejected_id = rejected.native.as_ref().unwrap().id();
        state.image(rejected, "image/png");
        state.events.set_scope(events::EventScope::Conversation);
        state.events.latest();
        assert!(state.events.open());
        let retained = images::decode(&block, state.graphics.prepare_native()).unwrap();
        state.image(retained, "image/png");
        state.events.set_scope(events::EventScope::Conversation);
        state.events.latest();
        assert!(state.events.open());
        let before = state.frame(now);
        assert_eq!(state.media.len(), 2);
        let mut media = images::Renderer::default();
        media.update(
            std::mem::take(&mut state.media),
            before.cursor,
            before.hidden,
        );
        media.complete();

        let rejection =
            format!("\x1b_Gi={rejected_id};E2BIG:image exceeds decoded size limit\x1b\\");
        assert!(state.keyboard(rejection.as_bytes(), now, false).is_empty());
        assert!(state.graphics.prepare_native());
        let after = state.frame(now);
        assert_eq!(
            state.media.len(),
            1,
            "only the rejected image loses its placement"
        );
        assert!(
            physical(&after)
                .screen()
                .contents()
                .contains("Image attachment")
        );
        let (delete, _) =
            media.update(std::mem::take(&mut state.media), after.cursor, after.hidden);
        assert!(delete.contains(&format!("a=d,d=I,i={rejected_id},")));

        let next = images::decode(&block, state.graphics.prepare_native()).unwrap();
        assert!(next.native.is_some());
        let next_id = next.native.as_ref().unwrap().id();
        state.image(next, "image/png");
        state.events.set_scope(events::EventScope::Conversation);
        state.events.latest();
        assert!(state.events.open());
        let final_frame = state.frame(now);
        assert_eq!(state.media.len(), 2);
        let (_, upload) = media.update(
            std::mem::take(&mut state.media),
            final_frame.cursor,
            final_frame.hidden,
        );
        assert!(upload.contains(&format!("a=t,t=d,f=100,i={next_id},")));
        assert!(!upload.contains(&format!("a=t,t=d,f=100,i={rejected_id},")));
    }

    #[test]
    fn selection_freeze_releases_native_placements_when_cell_measurement_expires() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let ui = Ui {
            layout: Layout::Chat,
            color: true,
            sidebar: true,
            sidebar_width: 37,
            panels: vec![crate::client::ui::Panel::Agents],
            ..Ui::default()
        };
        let mut state = State::new(30, 120, &ui);
        state.graphics = images::Graphics::probe_for_test();
        let now = Instant::now();
        assert!(
            state
                .graphics
                .input(b"\x1b_Gi=4294967295;OK\x1b\\\x1b[6;16;8t", now, false)
                .is_empty()
        );
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            16,
            16,
            image::Rgba([30, 120, 210, 255]),
        ));
        let mut bytes = std::io::Cursor::new(Vec::new());
        source
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let block = serde_json::json!({
            "type":"image", "mimeType":"image/png",
            "data":STANDARD.encode(bytes.into_inner()),
        });
        let image = images::decode(&block, state.graphics.prepare_native()).unwrap();
        state.image(image, "image/png");
        state.events.set_scope(events::EventScope::Conversation);
        state.events.latest();
        assert!(state.events.open());
        state.selection = true;
        let before = state.frame(now);
        let previous_cell = state.graphics.cell();
        let mut screen = physical(&before);
        let mut media = images::Renderer::default();
        let (_, upload) = media.update(
            std::mem::take(&mut state.media),
            before.cursor,
            before.hidden,
        );
        assert!(
            upload.contains("a=p"),
            "the frame has an actual user image placement"
        );
        media.complete();
        state.dirty = true;
        assert!(
            !state.should_redraw(Some(&before), previous_cell),
            "ordinary updates remain frozen"
        );

        state.dirty = false;
        state.graphics.resized();
        assert!(state.geometry == before.geometry, "the grid stays the same");
        assert!(state.should_redraw(Some(&before), previous_cell));
        let after = state.frame(now);
        let (delete, output) =
            media.update(std::mem::take(&mut state.media), after.cursor, after.hidden);
        assert!(
            delete.contains("a=d,d=I"),
            "the next frame removes stale native pixels"
        );
        assert!(!output.contains("a=p"));
        screen.process(&after.diff(Some(&before)));
        assert!(screen.screen().contents().contains("Image attachment"));
    }

    #[test]
    fn frame_diff_erases_old_text_when_a_row_shrinks_or_becomes_empty() {
        let mut state = State::new(12, 40, &split_ui());
        let mut before = state.frame(Instant::now());
        before.rows[0] = "\x1b[31mold text including a wide 界 suffix".into();
        before.rows[1] = "a row that disappears".into();
        let mut screen = physical(&before);
        let mut after = before.clone();
        after.rows[0] = "new".into();
        after.rows[1].clear();
        screen.process(&after.diff(Some(&before)));
        let rows: Vec<_> = screen.screen().rows(0, 40).collect();
        assert_eq!(rows[0], "new");
        assert_eq!(rows[1], "");
        assert_eq!(screen.screen().cell(0, 8).unwrap().contents(), "");
        assert!(after.diff(Some(&after)).is_empty());
    }

    #[test]
    fn escape_followed_by_an_arrow_remains_two_keys_at_every_read_boundary() {
        let bytes = b"\x1b\x1b[D";
        for split in 0..=bytes.len() {
            let mut keyboard = Keyboard::default();
            let now = Instant::now();
            let (mut output, mut keys) = keyboard.feed(&bytes[..split], now, false, true, false);
            let (rest, more) = keyboard.feed(&bytes[split..], now, false, true, false);
            output.extend(rest);
            keys.extend(more);
            assert_eq!(output, b"\x1b");
            assert!(matches!(keys.as_slice(), [Key::Back]));
            assert!(keyboard.pending.is_empty());
        }
    }

    #[test]
    fn geometry_reserves_real_panes_and_survives_narrow_resize() {
        let mut state = State::new(58, 106, &split_ui());
        assert_eq!(state.geometry.left, 75);
        assert_eq!(state.geometry.pty_rows, 23);
        assert_eq!(state.geometry.log_rows, 33);
        state.resize(30, 80);
        assert_eq!(state.geometry.left, 80);
        state.resize(58, 160);
        assert_eq!(state.geometry.left, 129);
        assert_eq!(state.pty.screen().size(), (nonzero(23), nonzero(129)));
        state.resize(18, 96);
        assert_eq!(state.geometry.pty_rows, 7);
        state.resize(1, 1);
        assert_eq!(state.geometry.pty_rows, 1);
        assert_eq!(state.geometry.log_rows, 0);
        assert_eq!(state.frame(Instant::now()).rows.len(), 1);
    }
    #[test]
    fn appearance_resizes_preserve_history_and_render_rgb_defaults() {
        let ui = Ui::parse("[ui]\nlayout='split'\nlog_height_percent=80\nsidebar=false\n[ui.colors]\nbackground='#000000'\nforeground='#abcdef'").unwrap();
        let mut state = State::new(1000, 120, &ui);
        assert_eq!(state.geometry.log_rows, 798);
        state.pty_session("main", b"retained shell");
        state.log(b"retained log");
        state.ui.layout = Layout::Audit;
        state.resize(36, 120);
        assert_eq!(state.geometry.log_rows, 0);
        assert!(state.pty.screen().contents().contains("retained shell"));
        let frame = physical(&state.frame(Instant::now()));
        assert_eq!(
            frame.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Rgb(171, 205, 239)
        );
        assert_eq!(
            frame.screen().cell(0, 0).unwrap().bgcolor(),
            vt100::Color::Rgb(0, 0, 0)
        );
        state.ui.layout = Layout::Split;
        state.resize(36, 120);
        assert!(state.logs.screen().contents().contains("retained log"));
    }

    #[test]
    fn terminal_switches_keep_independent_screens_cursors_and_input_modes() {
        let mut state = State::new(30, 110, &split_ui());
        state.register_session("pty-0001", "build");
        state.pty_session("main", b"MAIN\x1b[2;4H");
        state.pty_session("pty-0001", b"\x1b[?1049h\x1b[?1h\x1b[?2004hBUILD\x1b[4;7H");
        assert!(state.pty.screen().contents().contains("MAIN"));
        assert!(!state.pty.screen().contents().contains("BUILD"));
        assert_eq!(state.pty.screen().cursor_position(), (1, 3));
        state.select_session("pty-0001");
        assert!(state.pty.screen().contents().contains("BUILD"));
        assert_eq!(state.pty.screen().cursor_position(), (3, 6));
        assert!(state.pty.screen().application_cursor());
        assert!(state.pty.screen().bracketed_paste());
        let display = physical(&state.frame(Instant::now()));
        assert!(display.screen().contents().contains("Code mode"));
        assert!(!display.screen().contents().contains("BUILD"));
        assert!(!display.screen().contents().contains("MAIN"));
        assert!(display.screen().hide_cursor());
        assert!(!display.screen().application_cursor());
        state.pty_session("main", b"\x1b[3;1HLATER");
        assert!(!state.pty.screen().contents().contains("LATER"));
        state.select_session("main");
        assert!(state.pty.screen().contents().contains("MAIN"));
        assert!(state.pty.screen().contents().contains("LATER"));
        assert!(!state.pty.screen().application_cursor());
        assert!(!state.pty.screen().bracketed_paste());
    }
    #[test]
    fn device_replies_remain_with_the_requesting_terminal_across_selection_and_resize() {
        let mut state = State::new(30, 110, &split_ui());
        state.register_session("pty-0001", "queries");
        state.pty_session("main", b"\x1b[2;3H\x1b[6n");
        state.pty_session("pty-0001", b"\x1b[4;5H\x1b[6n");
        state.select_session("pty-0001");
        let replies: BTreeMap<_, _> = state.take_session_replies().into_iter().collect();
        assert_eq!(replies["main"], b"\x1b[2;3R");
        assert_eq!(replies["pty-0001"], b"\x1b[4;5R");
        assert!(state.take_session_replies().is_empty());
        state.resize(45, 140);
        let expected = (
            nonzero(state.geometry.pty_rows),
            nonzero(state.geometry.left),
        );
        assert_eq!(state.pty.screen().size(), expected);
        assert_eq!(state.inactive["main"].screen().size(), expected);
        state.pty_session("main", b"\x1b[18t");
        let replies = state.take_session_replies();
        assert_eq!(
            replies,
            vec![(
                "main".into(),
                format!("\x1b[8;{};{}t", expected.0, expected.1).into_bytes()
            )]
        );
    }
    #[test]
    fn closing_selected_terminal_restores_main_and_ignores_late_output() {
        let mut state = State::new(30, 110, &split_ui());
        state.register_session("pty-0001", "worker");
        state.pty_session("main", b"main remains");
        state.select_session("pty-0001");
        state.close_session("pty-0001");
        state.pty_session("pty-0001", b"late bytes\x1b[6n");
        assert_eq!(state.active_session, "main");
        assert_eq!(state.session_titles.len(), 1);
        assert!(state.inactive.is_empty());
        assert!(state.pty.screen().contents().contains("main remains"));
        assert!(!state.pty.screen().contents().contains("late bytes"));
        assert!(state.take_session_replies().is_empty());
        state.close_session("main");
        assert_eq!(state.session_titles.len(), 1);
        for index in 1..32 {
            state.register_session(&format!("pty-{index:04}"), "bounded");
        }
        assert_eq!(state.session_titles.len(), MAX_SESSIONS);
        assert_eq!(state.inactive.len(), MAX_SESSIONS - 1);
    }
    #[test]
    fn hidden_pty_escape_sequences_cannot_change_audit_layout() {
        let mut state = State::new(30, 110, &split_ui());
        state.log(b"retained audit\n");
        state.pty.process(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[31m");
        state.pty.process("界 e\u{301}".as_bytes());
        state.pty.process(b"\x1b[999;999H!");
        let frame = state.frame(Instant::now());
        let view = physical(&frame);
        assert!(view.screen().contents().contains("retained audit"));
        assert!(view.screen().contents().contains("Code mode"));
        assert!(!view.screen().contents().contains("界"));
        assert_eq!(state.pty.screen().cell(0, 0).unwrap().contents(), "界");
        assert!(
            state
                .pty
                .screen()
                .cell(0, 1)
                .unwrap()
                .is_wide_continuation()
        );
        assert_eq!(
            state.pty.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        for row in 0..state.geometry.rows - 1 {
            assert_eq!(
                view.screen()
                    .cell(row, state.geometry.left)
                    .unwrap()
                    .contents(),
                "│"
            );
        }
        assert!(view.screen().hide_cursor());
        state.pty.process(b"\x1b[?1049l");
        assert!(state.logs.screen().contents().contains("retained audit"));
    }
    #[test]
    fn native_queries_reply_with_virtual_coordinates_and_a_bounded_queue() {
        let mut state = State::new(58, 106, &split_ui());
        state
            .pty
            .process(b"\x1b[3;5H\x1b[6n\x1b[5n\x1b[c\x1b[>c\x1b[18t");
        let replies =
            String::from_utf8(std::mem::take(&mut state.pty.callbacks_mut().bytes)).unwrap();
        assert!(replies.contains("\x1b[3;5R"), "{replies:?}");
        assert!(replies.contains("\x1b[0n"), "{replies:?}");
        assert!(replies.contains("\x1b[?1;2c"), "{replies:?}");
        assert!(replies.contains("\x1b[>0;0;0c"), "{replies:?}");
        assert!(replies.contains("\x1b[8;23;75t"), "{replies:?}");
        for _ in 0..2000 {
            state.pty.process(b"\x1b[6n");
        }
        assert!(state.pty.callbacks().bytes.len() <= MAX_REPLIES);
        assert!(state.pty.callbacks().dropped > 0);
    }
    #[test]
    fn visible_log_diff_keeps_hidden_terminal_modes_out_of_the_client() {
        let mut state = State::new(30, 110, &split_ui());
        let a = state.frame(Instant::now());
        assert!(a.diff(Some(&a)).is_empty());
        state
            .pty
            .process(b"\x1b[?1h\x1b=\x1b[?2004hhidden command output");
        state.log(b"visible log update");
        let b = state.frame(Instant::now());
        let diff = b.diff(Some(&a));
        assert!(!diff.windows(4).any(|bytes| bytes == b"\x1b[2J"));
        assert!(diff.len() < b.diff(None).len() / 2);
        let mut parser = physical(&a);
        parser.process(&diff);
        assert_eq!(parser.screen().cursor_position(), b.cursor);
        assert!(!parser.screen().application_cursor());
        assert!(!parser.screen().application_keypad());
        assert!(parser.screen().bracketed_paste());
        assert!(parser.screen().hide_cursor());
        assert!(parser.screen().contents().contains("visible log update"));
        assert!(!parser.screen().contents().contains("hidden command output"));
        assert!(state.pty.screen().application_cursor());
        assert!(state.pty.screen().application_keypad());
    }
    #[test]
    fn no_color_does_not_disable_layout_and_log_lf_is_normalized() {
        let ui = Ui {
            color: false,
            ..split_ui()
        };
        let mut state = State::new(30, 110, &ui);
        state.log(b"a\n");
        state.log(b"b\r");
        state.log(b"\nc\n");
        state.pty.process(b"\x1b[31mred");
        let frame = state.frame(Instant::now());
        let parser = physical(&frame);
        assert_eq!(parser.screen().cell(0, 0).unwrap().contents(), "a");
        assert_eq!(parser.screen().cell(1, 0).unwrap().contents(), "b");
        assert_eq!(parser.screen().cell(2, 0).unwrap().contents(), "c");
        assert_eq!(
            parser
                .screen()
                .cell(state.geometry.pty_top, 0)
                .unwrap()
                .fgcolor(),
            vt100::Color::Default
        );
        assert_eq!(
            parser
                .screen()
                .cell(0, state.geometry.left)
                .unwrap()
                .contents(),
            "│"
        );
    }
    #[test]
    fn synchronized_frame_includes_initial_clear_rows_and_native_images() {
        let mut state = State::new(30, 110, &split_ui());
        state.log(b"complete frame\n");
        let frame = state.frame(Instant::now());
        let before = "\x1b_Ga=d,d=I,i=42\x1b\\";
        let after = "\x1b_Ga=p,i=43,p=1\x1b\\";
        let mut bytes = Vec::new();
        append_frame(&mut bytes, &frame, None, before, after);
        assert!(bytes.starts_with(SYNC_BEGIN));
        assert!(bytes.ends_with(SYNC_END));
        let initial_end = SYNC_BEGIN.len() + ENTER.len();
        assert_eq!(&bytes[SYNC_BEGIN.len()..initial_end], ENTER);
        assert!(bytes[initial_end..].starts_with(before.as_bytes()));
        assert!(bytes[..bytes.len() - SYNC_END.len()].ends_with(after.as_bytes()));
        assert_eq!(
            bytes
                .windows(SYNC_BEGIN.len())
                .filter(|part| *part == SYNC_BEGIN)
                .count(),
            1
        );
        let mut screen = vt100::Parser::new(nonzero(30), nonzero(110), 0);
        screen.process(&bytes);
        assert!(screen.screen().contents().contains("complete frame"));
        let mut unchanged = Vec::new();
        append_frame(&mut unchanged, &frame, Some(&frame), "", "");
        assert!(unchanged.is_empty());
        assert!(LEAVE.starts_with(SYNC_END));
    }

    #[test]
    fn synchronized_frame_remains_open_until_a_split_scroll_write_completes() {
        let mut state = State::new(30, 110, &split_ui());
        for index in 0..80 {
            state.log(format!("scroll row {index:03}\n").as_bytes());
        }
        let first = state.frame(Instant::now());
        state.keyboard(b"\x1b[5~", Instant::now(), false);
        let next = state.frame(Instant::now());
        let mut bytes = Vec::new();
        append_frame(&mut bytes, &next, Some(&first), "", "");
        assert!(bytes.starts_with(SYNC_BEGIN));
        assert!(bytes.ends_with(SYNC_END));
        assert!(!bytes.windows(ENTER.len()).any(|part| part == ENTER));

        struct Backpressure {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl Write for Backpressure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                let count = bytes.len().min(self.limit);
                if count == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                self.bytes.extend_from_slice(&bytes[..count]);
                self.limit -= count;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Backpressure {
            bytes: Vec::new(),
            limit: bytes.len() / 2,
        };
        let mut offset = 0;
        drain(&mut output, &bytes, &mut offset).unwrap();
        assert!(offset > SYNC_BEGIN.len() && offset < bytes.len() - SYNC_END.len());
        assert!(
            !output
                .bytes
                .windows(SYNC_END.len())
                .any(|part| part == SYNC_END)
        );
        output.limit = bytes.len();
        drain(&mut output, &bytes, &mut offset).unwrap();
        assert_eq!(offset, bytes.len());
        assert_eq!(output.bytes, bytes);
        let mut screen = physical(&first);
        screen.process(&output.bytes);
        assert_eq!(
            screen.screen().contents(),
            physical(&next).screen().contents()
        );
    }

    #[test]
    fn nonblocking_output_keeps_partial_bytes_for_the_next_frame() {
        struct Partial {
            bytes: Vec<u8>,
            first: bool,
        }
        impl Write for Partial {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.first {
                    self.first = false;
                    self.bytes.extend_from_slice(&bytes[..2]);
                    Ok(2)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = Partial {
            bytes: Vec::new(),
            first: true,
        };
        let mut offset = 0;
        drain(&mut output, b"abcdef", &mut offset).unwrap();
        assert_eq!(offset, 2);
        assert_eq!(output.bytes, b"ab");
        let mut completed = Vec::new();
        drain(&mut completed, b"abcdef", &mut offset).unwrap();
        assert_eq!(completed, b"cdef");
        assert_eq!(offset, 6);
    }
    #[test]
    fn wheel_and_page_keys_scroll_logs_without_a_focus_shortcut() {
        let mut state = State::new(58, 106, &split_ui());
        for index in 0..200 {
            state.log(format!("audit record {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        for byte in b"\x1b[<64;5;5M" {
            assert!(state.keyboard(&[*byte], now, false).is_empty());
        }
        assert_eq!(state.logs.screen().scrollback(), 3);
        assert!(state.keyboard(b"\x1b[<65;5;5M", now, false).is_empty());
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(state.keyboard(b"\x1b[5~", now, false).is_empty());
        assert!(state.logs.screen().scrollback() > 0);
        state.keyboard(b"\x1b[H", now, false);
        assert_eq!(state.logs.screen().scrollback(), SCROLLBACK_ROWS);
        assert!(state.logs.screen().contents().contains("audit record"));
        assert!(!state.logs.screen().contents().contains("audit record 000"));
        state.keyboard(b"\x1b[F", now, false);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(state.logs.screen().contents().contains("audit record 199"));
        let sidebar_mouse = format!("\x1b[<64;{};5M", state.geometry.left + 2);
        state.keyboard(sidebar_mouse.as_bytes(), now, false);
        assert_eq!(state.logs.screen().scrollback(), 0);
        for id in ["first", "second"] {
            state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","run_id":id,"phase":"started","code":"return 1;"}));
        }
        let audit_mouse = format!("\x1b[<65;2;{}M", state.geometry.pty_top + 2);
        state.keyboard(audit_mouse.as_bytes(), now, false);
        assert_eq!(state.audit.snapshot()["selected_index"], 1);
        assert_eq!(state.logs.screen().scrollback(), 0);
    }
    #[test]
    fn full_screen_logs_remain_scrollable_and_never_forward_input() {
        let ui = Ui {
            layout: Layout::Logs,
            ..split_ui()
        };
        let mut state = State::new(30, 110, &ui);
        for index in 0..200 {
            state.log(format!("audit record {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        assert!(state.keyboard(b"\x1b[5~", now, false).is_empty());
        assert!(state.logs.screen().scrollback() > 0);
        state.keyboard(b"\x1b[F", now, false);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(
            state
                .keyboard(b"ignored text\x1b[5~", now, false)
                .is_empty()
        );
        assert!(state.logs.screen().scrollback() > 0);
        state.register_session("pty-next", "next");
        state.select_session("pty-next");
        assert!(state.keyboard(b"\x1b[5~", now, false).is_empty());
        assert!(state.logs.screen().scrollback() > 0);
        assert!(state.keyboard(b"echo hidden\r", now, false).is_empty());
        assert!(!state.pty.screen().contents().contains("echo hidden"));
    }
    #[test]
    fn escape_closes_help_opened_from_scrolled_logs() {
        let mut state = State::new(30, 110, &split_ui());
        for index in 0..100 {
            state.log(format!("audit record {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        state.keyboard(b"\x1b[5~", now, false);
        let scrollback = state.logs.screen().scrollback();
        assert!(scrollback > 0);
        state.keyboard(b"\x1bOP", now, false);
        assert!(state.help);
        assert!(state.keyboard(b"\x1b", now, false).is_empty());
        assert!(
            state
                .keyboard(&[], now + Duration::from_millis(100), true)
                .is_empty()
        );
        assert!(!state.help);
        assert_eq!(state.logs.screen().scrollback(), scrollback);
    }

    #[test]
    fn layout_shortcut_never_forwards_coalesced_text_to_a_hidden_shell() {
        let ui = Ui {
            layout: Layout::Audit,
            ..split_ui()
        };
        let mut state = State::new(30, 110, &ui);
        let now = Instant::now();
        assert!(
            state
                .keyboard(b"\x1bOQecho hidden\n", now, false)
                .is_empty()
        );
        assert_eq!(state.actions, vec!["layout.next"]);
        state.actions.clear();
        assert!(
            state
                .keyboard(b"\x1b[15~echo hidden\n", now, false)
                .is_empty()
        );
        assert_eq!(state.actions, vec!["reload"]);
        assert!(state.keyboard(b"ordinary text", now, false).is_empty());
    }

    #[test]
    fn pasted_and_incomplete_keys_never_reach_a_hidden_shell() {
        let mut state = State::new(58, 106, &split_ui());
        let now = Instant::now();
        let pasted = b"\x1b[200~literal \x1b[14~ \x1b[5~ \x1b[201~";
        for byte in pasted {
            assert!(state.keyboard(&[*byte], now, false).is_empty());
        }
        assert!(state.actions.is_empty());
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(!state.keyboard.pasting);
        assert!(state.keyboard(b"\x1b[17", now, false).is_empty());
        assert!(
            state
                .keyboard(&[], now + Duration::from_millis(100), true)
                .is_empty()
        );
        assert!(
            state
                .keyboard(b"~", now + Duration::from_millis(101), false)
                .is_empty()
        );
        assert!(
            state
                .keyboard(b"\x1b[1;5Cecho hidden", now, false)
                .is_empty()
        );
        assert!(!state.pty.screen().contents().contains("echo hidden"));
    }
    #[test]
    fn split_unicode_is_preserved_in_logs_and_hidden_native_terminal() {
        let mut state = State::new(30, 110, &split_ui());
        for byte in "界e\u{301}".as_bytes() {
            state.pty.process(&[*byte]);
            state.log(&[*byte]);
        }
        let screen = physical(&state.frame(Instant::now()));
        assert_eq!(state.pty.screen().cell(0, 0).unwrap().contents(), "界");
        assert_eq!(screen.screen().cell(0, 0).unwrap().contents(), "界");
        assert_eq!(screen.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
        assert_ne!(
            screen
                .screen()
                .cell(state.geometry.pty_top, 0)
                .unwrap()
                .contents(),
            "界"
        );
    }
    #[test]
    fn jsonl_and_disabled_ui_never_initialize_a_physical_display() {
        let mut ui = Ui {
            log_format: LogFormat::Jsonl,
            ..split_ui()
        };
        assert!(
            Session::start(&ui, "/workspace/project", None, None)
                .unwrap()
                .is_none()
        );
        ui.log_format = LogFormat::Pretty;
        ui.dashboard = false;
        assert!(
            Session::start(&ui, "/workspace/project", None, None)
                .unwrap()
                .is_none()
        );
    }
    #[cfg(unix)]
    #[test]
    fn worker_exit_restores_alternate_screen_and_joins_within_a_deadline() {
        use std::{
            io::Read,
            os::fd::{AsRawFd, FromRawFd},
        };
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 30,
            ws_col: 110,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: this fixture creates its own PTY; no user terminal is touched.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::addr_of_mut!(size),
                )
            },
            0
        );
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) },
                0
            );
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
        // Keep the fixture's terminal open until its output has been consumed;
        // closing the last slave can discard unread bytes on Darwin.
        let keep_slave = slave.try_clone().unwrap();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(30, 110, &split_ui())),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });
        let worker = shared.clone();
        let thread = std::thread::spawn(move || draw_loop(&worker, PhysicalOutput { file: slave }));
        let session = Session {
            shared: shared.clone(),
            thread: Some(thread),
            guide_probe: None,
            account_probe: None,
        };
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 65536];
        let until = Instant::now() + Duration::from_secs(1);
        while !bytes.windows(8).any(|part| part == b"\x1b[?1049h") && Instant::now() < until {
            match master.read(&mut buffer) {
                Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(error) => panic!("fixture output: {error}"),
            }
        }
        let (done, complete) = std::sync::mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            drop(session);
            let _ = done.send(());
        });
        let until = Instant::now() + Duration::from_secs(1);
        let mut stopped = false;
        loop {
            // Drain while Drop joins the worker, as a real terminal emulator
            // does; a full PTY output queue must not starve bounded restoration.
            let drained = match master.read(&mut buffer) {
                Ok(0) => true,
                Ok(n) => {
                    bytes.extend_from_slice(&buffer[..n]);
                    false
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => true,
                Err(error) => panic!("fixture output: {error}"),
            };
            if !stopped {
                match complete.try_recv() {
                    Ok(()) => stopped = true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        panic!("display cleanup thread exited before confirming shutdown")
                    }
                }
            }
            assert!(
                Instant::now() < until,
                "display did not stop and drain within deadline"
            );
            if stopped && drained {
                break;
            }
            if drained {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        cleanup.join().unwrap();
        drop(keep_slave);
        assert!(!shared.alive.load(Ordering::Acquire));
        assert!(bytes.windows(8).any(|part| part == b"\x1b[?1049h"));
        assert!(bytes.windows(8).any(|part| part == b"\x1b[?1049l"));
        let mut view = vt100::Parser::new(nonzero(30), nonzero(110), 0);
        view.process(b"original shell prompt");
        view.process(&bytes);
        assert!(!view.screen().alternate_screen());
        assert!(!view.screen().hide_cursor());
        assert!(view.screen().contents().contains("original shell prompt"));
    }

    #[test]
    fn typing_keeps_log_scrollback_and_compact_audit_never_exposes_shell() {
        let mut state = State::new(58, 106, &split_ui());
        for index in 0..100 {
            state.log(format!("combined audit {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        assert!(state.keyboard(b"\x1b[5~x", now, false).is_empty());
        let position = state.logs.screen().scrollback();
        assert!(position > 0);
        assert!(state.keyboard(b"ordinary text", now, false).is_empty());
        assert_eq!(state.logs.screen().scrollback(), position);
        state.resize(4, 106);
        let compact = state.frame(now);
        assert_eq!(state.geometry.log_rows, 0);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(compact.hidden);
        assert!(!compact.rows.last().unwrap().contains("F6"));
        assert!(!compact.rows.last().unwrap().contains("Shell"));
        state.resize(58, 106);
        assert!(state.frame(now).hidden);
        assert_eq!(state.logs.screen().scrollback(), 0);
    }

    #[test]
    fn text_filter_keeps_matched_command_output_and_completion() {
        let mut state = State::new(58, 106, &split_ui());
        state.log(b"[RUN ] shell #41 cargo test\n[OUT ] shell #41\n  unit tests passed\n[OK  ] shell #41 completed exit=0\n[READY] Standalone observer status\n  unrelated background activity\n[RUN ] shell #42 npm run lint\n[OUT ] shell #42\n  unrelated lint output\n[OK  ] shell #42 completed exit=0\n");
        let now = Instant::now();
        assert!(state.keyboard(b"/cargo test\r", now, false).is_empty());
        assert_eq!(state.log_query, "cargo test");
        assert!(!state.filter_editing);
        let text = physical(&state.frame(now)).screen().contents();
        assert!(text.contains("cargo test"), "{text}");
        assert!(text.contains("unit tests passed"), "{text}");
        assert!(text.contains("#41 completed exit=0"), "{text}");
        assert!(!text.contains("npm run lint"), "{text}");
        assert!(!text.contains("unrelated lint output"), "{text}");
        assert!(!text.contains("Standalone observer status"), "{text}");
        assert!(!text.contains("unrelated background activity"), "{text}");
        state.log_query = "standalone".into();
        let standalone = physical(&state.frame(now)).screen().contents();
        assert!(
            standalone.contains("Standalone observer status"),
            "{standalone}"
        );
        assert!(
            standalone.contains("unrelated background activity"),
            "{standalone}"
        );
        assert!(!standalone.contains("cargo test"), "{standalone}");
        assert!(!standalone.contains("unit tests passed"), "{standalone}");
        assert!(!standalone.contains("npm run lint"), "{standalone}");
        state.keyboard(b"\x1b", now, false);
        state.keyboard(&[], now + Duration::from_millis(100), true);
        assert!(state.log_query.is_empty());
        assert!(
            physical(&state.frame(now))
                .screen()
                .contents()
                .contains("npm run lint")
        );
    }

    #[test]
    fn audit_navigation_opens_highlighted_source_and_keeps_final_result() {
        let ui = Ui {
            sidebar: false,
            ..split_ui()
        };
        let mut state = State::new(40, 100, &ui);
        state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","run_id":"source-1","phase":"started","code":"const total = 42;\nreturn total;","timestamp_ms":0}));
        state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","run_id":"source-1","phase":"finished","success":false,"error":"sample failure","duration_ms":12}));
        state.audit.ingest(&serde_json::json!({"type":"ssmcp.code_mode","run_id":"source-2","phase":"started","code":"return 'next';","timestamp_ms":1}));
        let now = Instant::now();
        assert!(state.keyboard(b"\x1b[B\r", now, false).is_empty());
        assert_eq!(state.audit.snapshot()["selected"]["run_id"], "source-1");
        assert_eq!(state.audit.snapshot()["detail"], true);
        let frame = state.frame(now);
        let text = physical(&frame).screen().contents();
        assert!(text.contains("const total = 42;"), "{text}");
        assert!(text.contains("return total;"), "{text}");
        assert!(text.contains("sample failure"), "{text}");
        assert!(
            frame
                .rows
                .iter()
                .any(|row| row.contains(&state.ui.color("accent")) && row.contains("const"))
        );
        assert!(state.keyboard(b"\x1b[D", now, false).is_empty());
        assert_eq!(state.audit.snapshot()["detail"], false);
        assert!(state.keyboard(b"\x1b[A\x1b[C", now, false).is_empty());
        assert_eq!(state.audit.snapshot()["selected"]["run_id"], "source-2");
        assert_eq!(state.audit.snapshot()["detail"], true);
    }
    #[test]
    fn interrupt_is_decoded_only_outside_paste_at_every_input_boundary() {
        let png = crate::client::input::paste::fixture_png();
        let pasted = [b"\x1b[200~".as_slice(), &png, b"\x1b[201~"].concat();
        let bytes = [pasted.as_slice(), b"\x03"].concat();
        for split in 0..=bytes.len() {
            let mut keyboard = Keyboard::default();
            let now = Instant::now();
            let (mut output, mut actions) = keyboard.feed(&bytes[..split], now, false, true, false);
            let (tail, extra) = keyboard.feed(&bytes[split..], now, false, true, false);
            output.extend(tail);
            actions.extend(extra);
            assert_eq!(output, pasted, "split={split}");
            assert_eq!(
                actions
                    .iter()
                    .filter(|key| matches!(key, Key::Interrupt))
                    .count(),
                1
            );
            assert!(!keyboard.pasting);
            assert!(keyboard.pending.is_empty());
        }
    }
    #[test]
    fn image_idle_recovery_releases_real_keys_even_without_an_outer_end() {
        use crate::client::input::paste::{IMAGE_IDLE_TIMEOUT, fixture_png};
        let png = fixture_png();
        let malformed = b"\x89PNG\r\n\x1a\n\xff\xff\xff\xffIHDR".to_vec();
        for payload in [&png[..1], &png[..png.len() - 1], png.as_slice(), &malformed] {
            let now = Instant::now();
            let mut keyboard = Keyboard::default();
            let source = [b"\x1b[200~".as_slice(), payload].concat();
            let (output, actions) = keyboard.feed(&source, now, false, true, false);
            assert_eq!(output, source);
            assert!(actions.is_empty());
            assert!(keyboard.pasting);
            let (_, actions) = keyboard.feed(
                &[],
                now + IMAGE_IDLE_TIMEOUT - Duration::from_millis(1),
                true,
                true,
                false,
            );
            assert!(actions.is_empty());
            assert!(keyboard.pasting);
            let (output, actions) =
                keyboard.feed(b"\x03", now + IMAGE_IDLE_TIMEOUT, false, true, false);
            assert!(output.is_empty());
            assert!(matches!(actions.as_slice(), [Key::Interrupt]));
            assert!(!keyboard.pasting);
        }
    }

    #[test]
    fn connection_footer_uses_the_same_last_row_in_every_layout() {
        for layout in [Layout::Chat, Layout::Logs, Layout::Audit, Layout::Split] {
            let ui = Ui {
                layout,
                color: false,
                sidebar: false,
                ..Ui::default()
            };
            let mut state = preview_state(12, 100, &ui);
            let before = state.geometry;
            state.notice = Some("/exit must be used without arguments".into());
            let frame = state.frame(Instant::now());
            assert_eq!(state.geometry, before);
            assert_eq!(frame.rows.len(), 12);
            let bottom = frame.rows.last().unwrap();
            assert!(bottom.contains("without arguments"), "{layout:?}: {bottom}");
            assert!(
                bottom.contains("https://ssmcp.lmm.best"),
                "{layout:?}: {bottom}"
            );
            assert!(bottom.contains("preview"), "{layout:?}: {bottom}");
            assert_eq!(bottom.width(), 100, "{layout:?}: {bottom:?}");
        }
    }
}
