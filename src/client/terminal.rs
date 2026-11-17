//! Compose independent log/PTY VT screens with an observational sidebar.
//! Only this module writes the physical terminal while a Session is active.
use super::{
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

const FRAME_INTERVAL: Duration = Duration::from_millis(34);
const MAX_REPLIES: usize = 8192;
const MAX_SESSIONS: usize = 16;
const ENTER: &[u8] = b"\x1b[?1;6;25;66;2004s\x1b[?1049h\x1b[?6l\x1b[r\x1b[?25l\x1b[0m\x1b[H\x1b[2J";
const LEAVE: &[u8] = b"\x1b[0m\x1b[?1l\x1b>\x1b[?2004l\x1b[?25h\x1b[?1049l\x1b[?1;6;25;66;2004r";
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
}

impl Session {
    pub(crate) fn start(ui: &Ui) -> io::Result<Option<Self>> {
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
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(rows, cols, &options)),
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
            Ok(thread) => Ok(Some(Self {
                shared,
                thread: Some(thread),
            })),
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

pub(super) fn active() -> bool {
    shared().is_some_and(|shared| shared.alive.load(Ordering::Acquire))
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
        if !state.preview { state.panel = snapshot; }
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
    state.ui.color &= std::env::var_os("NO_COLOR").is_none();
    let (rows, cols) = (state.geometry.rows, state.geometry.cols);
    state.resize(rows, cols);
    state.keyboard.logs = ui.layout == Layout::Logs;
    state.notice = None;
    state.dirty = true;
    Some((state.geometry.pty_rows, state.geometry.left))
}

pub(crate) fn take_actions() -> Vec<&'static str> {
    let Some(shared) = shared() else { return Vec::new(); };
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
        state.logs = vt100::Parser::new(nonzero(state.geometry.log_rows), nonzero(state.geometry.left), 512);
        state.previous_cr = false;
        state.dirty = true;
    }
}

pub(crate) fn snapshot() -> serde_json::Value {
    let Some(shared) = shared() else { return serde_json::json!({"active": false}); };
    let state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    let g = state.geometry;
    serde_json::json!({
        "active": shared.alive.load(Ordering::Acquire), "rows":g.rows, "cols":g.cols,
        "panes":{"logs":{"row":0,"col":0,"rows":g.log_rows,"cols":g.left},
            "shell":{"row":g.pty_top,"col":0,"rows":g.pty_rows,"cols":g.left,"visible":state.ui.layout != Layout::Logs}},
        "focus":if state.help {"help"} else if state.keyboard.logs || state.ui.layout == Layout::Logs {"logs"} else {"shell"},
        "active_terminal":state.active_session,
        "terminals":state.session_titles.iter().map(|(id,title)| serde_json::json!({"id":id,"title":panel::safe_text(title)})).collect::<Vec<_>>(),
        "help":state.help,"log_scrollback":state.logs.screen().scrollback(),"log_history":state.logs.screen().scrollback_len(),
    })
}

/// A fixture rendered through the production compositor; no commands execute.
fn preview_state(rows: u16, cols: u16, ui: &Ui) -> State {
    let mut state = State::new(rows, cols, ui);
    state.preview = true;
    state.log(b"SSMCP / APPEARANCE PREVIEW\n\nThis view uses example data. No device is connected.\n\n[READY] terminal and task interfaces\n[RESULT] theme loaded from local configuration\n");
    state.pty.process(b"\x1b[2;3H$ ssmcp\r\n\r\n  Your shell has room to work.\r\n\r\n  Press F1 to explore appearance and keyboard controls.\r\n  Edit a TOML file, then press F5 to reload it.\r\n\r\n  Preview only: keyboard text is never executed.\r\n  Press q or Ctrl-] to leave.\r\n");
    state.panel.tasks.push(panel::TaskView { id:"example".into(), description:"Example task · custom frontend".into(), cwd:Some("~/project".into()), started:Instant::now() });
    state.panel.progress.push(panel::ProgressView { source:"Example".into(), message:Some("Preparing workspace".into()), value:Some(3.0), total:Some(4.0), updated:Instant::now() });
    state
}

pub(crate) fn preview_bytes(ui: &Ui, rows: u16, cols: u16) -> Vec<u8> {
    preview_state(rows, cols, ui).frame(Instant::now()).diff(None)
}

pub(crate) fn seed_preview() {
    if let Some(shared) = shared() {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        *state = preview_state(state.geometry.rows, state.geometry.cols, &state.ui);
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

/// F6 selects the scrollable audit pane. All other shell input, including
/// fragmented escape sequences and bracketed-paste payloads, stays native.
pub(crate) fn keyboard(text: &str) -> String {
    let Some(shared) = shared() else {
        return text.into();
    };
    if !shared.alive.load(Ordering::Acquire) {
        return text.into();
    }
    let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&state.keyboard(text.as_bytes(), Instant::now(), false)).into_owned()
}

/// Called by the input pump so a lone Escape never waits indefinitely for a
/// possible shortcut suffix. Escape in log focus returns to the shell.
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
        let layout = PanelLayout::configured(cols, rows, ui.sidebar && !ui.panels.is_empty(), ui.sidebar_width);
        let available = if rows >= 3 { rows - 1 } else { rows };
        let log_rows = match ui.layout {
            Layout::Logs => available,
            Layout::Split if rows >= 5 => ((available - 1) * ui.log_height_percent / 100).clamp(1, available - 2),
            _ => 0,
        };
        let pty_top = if ui.layout == Layout::Split && log_rows > 0 { log_rows + 1 } else { 0 };
        let pty_rows = available.saturating_sub(pty_top).max(1);
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

#[derive(Clone, Copy)]
enum Key {
    Help,
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
    PasteStart,
    PasteEnd,
}
const KEYS: &[(&[u8], Key)] = &[
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
}
impl Keyboard {
    fn accepts(&self, key: Key, allow_logs: bool) -> bool {
        if self.pasting {
            return matches!(key, Key::PasteEnd);
        }
        match key {
            Key::Help | Key::Layout | Key::Sidebar | Key::Theme | Key::Reload | Key::Save => true,
            Key::Toggle => allow_logs,
            Key::Up | Key::Down | Key::Home | Key::End => self.logs,
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
        self.logs |= force_logs;
        let mut output = Vec::new();
        let mut actions = Vec::new();
        for &byte in bytes {
            if self.pending.is_empty() && byte != 0x1b {
                if !self.pasting {
                    self.logs = force_logs;
                }
                output.push(byte);
                continue;
            }
            self.pending.push(byte);
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
                        output.extend_from_slice(sequence);
                    }
                    Key::PasteEnd => {
                        self.pasting = false;
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
                output.append(&mut self.pending);
                self.since = None;
            }
        }
        if flush
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
    geometry: Geometry,
    logs: vt100::Parser,
    pty: vt100::Parser<Replies>,
    active_session: String,
    inactive: BTreeMap<String, vt100::Parser<Replies>>,
    session_titles: BTreeMap<String, String>,
    panel: PanelSnapshot,
    ui: Ui,
    help: bool,
    preview: bool,
    notice: Option<String>,
    actions: Vec<&'static str>,
    previous_cr: bool,
    dirty: bool,
    keyboard: Keyboard,
}
impl State {
    fn new(rows: u16, cols: u16, ui: &Ui) -> Self {
        let geometry = Geometry::new(rows, cols, ui);
        Self {
            logs: vt100::Parser::new(nonzero(geometry.log_rows), nonzero(geometry.left), 512),
            pty: vt100::Parser::new_with_callbacks(
                nonzero(geometry.pty_rows),
                nonzero(geometry.left),
                512,
                Replies::default(),
            ),
            active_session: "main".into(),
            inactive: BTreeMap::new(),
            session_titles: BTreeMap::from([("main".into(), "main".into())]),
            geometry,
            panel: PanelSnapshot::default(),
            ui: ui.clone(),
            help: false,
            preview: false,
            notice: None,
            actions: Vec::new(),
            previous_cr: false,
            dirty: true,
            keyboard: Keyboard { logs: ui.layout == Layout::Logs, ..Keyboard::default() },
        }
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
                    512,
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
    fn session_label(&self) -> String {
        let index = self
            .session_titles
            .keys()
            .position(|id| id == &self.active_session)
            .unwrap_or(0)
            + 1;
        let title = self
            .session_titles
            .get(&self.active_session)
            .map_or(self.active_session.as_str(), String::as_str);
        format!("{index}/{} · {title}", self.session_titles.len())
    }
    fn log(&mut self, bytes: &[u8]) {
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
    fn keyboard(&mut self, bytes: &[u8], now: Instant, flush: bool) -> Vec<u8> {
        let old_focus = self.keyboard.logs;
        let showing_help = self.help;
        let (output, actions) = self
            .keyboard
            .feed(bytes, now, flush, self.geometry.log_rows > 0, self.ui.layout == Layout::Logs);
        let mut layout_changed = false;
        for action in actions {
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
                Key::Layout => Some("layout.next"), Key::Sidebar => Some("sidebar.toggle"),
                Key::Theme => Some("theme.next"), Key::Reload => Some("reload"), Key::Save => Some("save"),
                _ => None,
            };
            if let Some(action) = ui_action {
                // Display actions are applied after this input read. Never send
                // coalesced text to a shell that may have just become hidden.
                layout_changed |= matches!(action, "layout.next" | "reload");
                if self.actions.len() < 64 { self.actions.push(action); }
                continue;
            }
            let screen = self.logs.screen_mut();
            let current = screen.scrollback();
            let page = usize::from(self.geometry.log_rows).saturating_sub(1).max(1);
            let position = match action {
                Key::Up => current.saturating_add(page),
                Key::Down => current.saturating_sub(page),
                Key::Home => screen.scrollback_len(),
                Key::End => 0,
                _ => current,
            };
            screen.set_scrollback(position);
            self.dirty = true;
        }
        if !self.keyboard.logs && self.logs.screen().scrollback() > 0 {
            self.logs.screen_mut().set_scrollback(0);
            self.dirty = true;
        }
        if old_focus != self.keyboard.logs {
            self.dirty = true;
        }
        if showing_help && output == b"\x1b" { self.help = false; self.dirty = true; }
        if showing_help || self.help || self.ui.layout == Layout::Logs || layout_changed { Vec::new() } else { output }
    }
    fn frame(&self, now: Instant) -> Frame {
        let g = self.geometry;
        let layout = PanelLayout::configured(g.cols, g.rows, self.ui.sidebar && !self.ui.panels.is_empty(), self.ui.sidebar_width);
        let sidebar = layout.sidebar.map(|rect| {
            let height = rect.height.saturating_sub(u16::from(g.rows >= 3));
            (rect, panel::render_configured(&self.panel, rect.width, height, now, &self.ui))
        });
        let mut rows = Vec::with_capacity(usize::from(g.rows));
        for row in 0..g.rows {
            if g.rows >= 3 && row == g.rows - 1 {
                let hint = if let Some(notice) = &self.notice {
                    format!(" {notice} · F1 Help ")
                } else if self.keyboard.logs || self.ui.layout == Layout::Logs {
                    format!(" Logs · PgUp/PgDn · Home/End · F2 View · F1 Help · history {}/{} ", self.logs.screen().scrollback(), self.logs.screen().scrollback_len())
                } else if g.cols < 100 {
                    format!(" Shell · {} · F1 Help · F2 View · F6 Logs · F7 Next ", self.session_label())
                } else {
                    " Shell · F1 Help · F2 View · F3 Sidebar · F4 Theme · F5 Reload · F6 Logs · F7 Next · F8 Save ".into()
                };
                let text = fit(&hint, g.cols);
                let padded = format!(
                    "{text}{}",
                    " ".repeat(usize::from(g.cols).saturating_sub(text.width()))
                );
                rows.push(self.tone(
                    if self.keyboard.logs {
                        PanelTone::Accent
                    } else {
                        PanelTone::Muted
                    },
                    &padded,
                ));
                continue;
            }
            let mut line = if row < g.log_rows {
                screen_row(self.logs.screen(), row, g.left, &self.ui)
            } else if row == g.log_rows && g.log_rows > 0 {
                let title = format!(
                    " Shell · {} · F7 Next · {}×{} ",
                    self.session_label(),
                    g.left,
                    g.pty_rows
                );
                let text = fit(&title, g.left);
                let padding = usize::from(g.left).saturating_sub(text.width());
                self.tone(PanelTone::Muted, &format!("{text}{}", "─".repeat(padding)))
            } else {
                screen_row(
                    self.pty.screen(),
                    row.saturating_sub(g.pty_top),
                    g.left,
                    &self.ui,
                )
            };
            if let Some((rect, panel)) = &sidebar {
                line.push_str(&self.tone(PanelTone::Muted, "│"));
                let item = &panel[usize::from(row - rect.row)];
                let text = fit(&item.text, rect.width);
                let padded = format!(
                    "{text}{}",
                    " ".repeat(usize::from(rect.width).saturating_sub(text.width()))
                );
                line.push_str(&self.tone(item.tone, &padded));
            }
            rows.push(line);
        }
        if self.help {
            let lines = [
                "SSMCP  /  Help & appearance".to_string(),
                String::new(),
                format!("Theme  {}     View  {}", self.ui.theme.name(), self.ui.layout.name()),
                format!("Logs  {}%     Sidebar  {} · {} columns", self.ui.log_height_percent, if self.ui.sidebar { "shown" } else { "hidden" }, self.ui.sidebar_width),
                String::new(),
                "F1 / Esc   Close this view".into(),
                "F2         Switch split / shell / logs".into(),
                "F3         Show or hide the sidebar".into(),
                "F4         Cycle color themes".into(),
                "F5         Reload your configuration".into(),
                "F6         Focus logs; PgUp / PgDn scroll".into(),
                "F7         Select the next terminal".into(),
                "F8         Save the current appearance".into(),
                String::new(),
                "Edit colors, panel order and sizes in your TOML file.".into(),
                "ssmcp ui show     ssmcp ui check     ssmcp ui preview".into(),
                String::new(),
                self.notice.clone().unwrap_or_else(|| "Appearance changes keep running tasks and shell output.".into()),
            ];
            for (row, line) in rows.iter_mut().enumerate() {
                let content = lines.get(row.saturating_sub(1)).filter(|_| row > 0).map(String::as_str).unwrap_or("");
                let text = fit(&format!("  {content}"), g.cols);
                let padded = format!("{text}{}", " ".repeat(usize::from(g.cols).saturating_sub(text.width())));
                *line = self.tone(if row == 1 { PanelTone::Heading } else { PanelTone::Normal }, &padded);
            }
        }
        let screen = self.pty.screen();
        let (row, col) = screen.cursor_position();
        Frame {
            geometry: g,
            rows,
            cursor: (g.pty_top + row.min(g.pty_rows - 1), col.min(g.left - 1)),
            hidden: screen.hide_cursor() || self.keyboard.logs || self.help || self.ui.layout == Layout::Logs,
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
            bracketed_paste: screen.bracketed_paste(),
        }
    }
    fn tone(&self, tone: PanelTone, text: &str) -> String {
        if !self.ui.color { return text.into(); }
        let semantic = match tone {
            PanelTone::Normal => "foreground", PanelTone::Muted => "muted",
            PanelTone::Heading => "heading", PanelTone::Accent => "accent",
            PanelTone::Success => "success", PanelTone::Warning => "warning", PanelTone::Error => "error",
        };
        format!("\x1b[{};{}m{text}\x1b[0m", self.ui.background(), self.ui.color(semantic))
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
    if ui.color { let _ = write!(text, "\x1b[{};{}m", ui.background(), ui.color("foreground")); }
    let mut style = Style::default();
    let mut col = 0;
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
                if next_style.fg == vt100::Color::Default { let _ = write!(text, "\x1b[{}m", ui.color("foreground")); }
                if next_style.bg == vt100::Color::Default { let _ = write!(text, "\x1b[{}m", ui.background()); }
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
}
impl Frame {
    fn diff(&self, previous: Option<&Self>) -> Vec<u8> {
        if previous == Some(self) {
            return Vec::new();
        }
        let mut output = String::from("\x1b[?25l");
        let resized = previous.is_none_or(|old| old.geometry != self.geometry);
        for (row, line) in self.rows.iter().enumerate() {
            if resized || previous.and_then(|old| old.rows.get(row)) != Some(line) {
                let _ = write!(output, "\x1b[{};1H\x1b[0m{line}", row + 1);
            }
        }
        if previous.is_none_or(|old| old.application_cursor != self.application_cursor) {
            output.push_str(if self.application_cursor {
                "\x1b[?1h"
            } else {
                "\x1b[?1l"
            });
        }
        if previous.is_none_or(|old| old.application_keypad != self.application_keypad) {
            output.push_str(if self.application_keypad {
                "\x1b="
            } else {
                "\x1b>"
            });
        }
        if previous.is_none_or(|old| old.bracketed_paste != self.bracketed_paste) {
            output.push_str(if self.bracketed_paste {
                "\x1b[?2004h"
            } else {
                "\x1b[?2004l"
            });
        }
        let _ = write!(
            output,
            "\x1b[0m\x1b[{};{}H\x1b[?25{}",
            self.cursor.0 + 1,
            self.cursor.1 + 1,
            if self.hidden { 'l' } else { 'h' }
        );
        output.into_bytes()
    }
}

struct RestoringOutput<'a> {
    shared: &'a Shared,
    output: PhysicalOutput,
    wrote: bool,
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
        let mut offset = 0;
        while self.wrote && offset < LEAVE.len() && Instant::now() < until {
            if drain(&mut self.output, LEAVE, &mut offset).is_err() {
                break;
            }
            if offset < LEAVE.len() {
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
    };
    let mut pending = ENTER.to_vec();
    let mut offset = 0;
    let mut previous = None;
    let mut last_panel = Instant::now();
    while !shared.stop.load(Ordering::Acquire) {
        if offset >= pending.len() {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if last_panel.elapsed() >= Duration::from_secs(1) {
                state.dirty = true;
                last_panel = Instant::now();
            }
            if state.dirty {
                let frame = state.frame(Instant::now());
                pending = frame.diff(previous.as_ref());
                previous = Some(frame);
                offset = 0;
                state.dirty = false;
            }
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
    fn geometry_reserves_real_panes_and_survives_narrow_resize() {
        let mut state = State::new(58, 106, &Ui::default());
        assert_eq!(state.geometry.left, 77);
        assert_eq!(state.geometry.pty_rows, 18);
        assert_eq!(state.geometry.log_rows, 38);
        state.resize(30, 80);
        assert_eq!(state.geometry.left, 80);
        state.resize(58, 160);
        assert_eq!(state.geometry.left, 123);
        assert_eq!(state.pty.screen().size(), (nonzero(18), nonzero(123)));
        state.resize(18, 96);
        assert_eq!(state.geometry.pty_rows, 8);
        state.resize(1, 1);
        assert_eq!(state.geometry.pty_rows, 1);
        assert_eq!(state.geometry.log_rows, 0);
        assert_eq!(state.frame(Instant::now()).rows.len(), 1);
    }
    #[test]
    fn terminal_switches_keep_independent_screens_cursors_and_input_modes() {
        let mut state = State::new(30, 110, &Ui::default());
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
        assert!(
            physical(&state.frame(Instant::now()))
                .screen()
                .contents()
                .contains("2/2 · build · F7")
        );
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
        let mut state = State::new(30, 110, &Ui::default());
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
        let mut state = State::new(30, 110, &Ui::default());
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
    fn pty_clear_absolute_cursor_alternate_and_wide_cells_cannot_escape() {
        let mut state = State::new(30, 110, &Ui::default());
        state.log(b"retained audit\n");
        state.pty.process(b"\x1b[?1049h\x1b[2J\x1b[H\x1b[31m");
        state.pty.process("界 e\u{301}".as_bytes());
        state.pty.process(b"\x1b[999;999H!");
        let frame = state.frame(Instant::now());
        let view = physical(&frame);
        assert!(view.screen().contents().contains("retained audit"));
        let top = state.geometry.pty_top;
        assert_eq!(view.screen().cell(top, 0).unwrap().contents(), "界");
        assert!(view.screen().cell(top, 1).unwrap().is_wide_continuation());
        assert_eq!(
            view.screen().cell(top, 0).unwrap().fgcolor(),
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
        assert_eq!(view.screen().cursor_position(), frame.cursor);
        state.pty.process(b"\x1b[?1049l");
        assert!(state.logs.screen().contents().contains("retained audit"));
    }
    #[test]
    fn native_queries_reply_with_virtual_coordinates_and_a_bounded_queue() {
        let mut state = State::new(58, 106, &Ui::default());
        state
            .pty
            .process(b"\x1b[3;5H\x1b[6n\x1b[5n\x1b[c\x1b[>c\x1b[18t");
        let replies =
            String::from_utf8(std::mem::take(&mut state.pty.callbacks_mut().bytes)).unwrap();
        assert!(replies.contains("\x1b[3;5R"), "{replies:?}");
        assert!(replies.contains("\x1b[0n"), "{replies:?}");
        assert!(replies.contains("\x1b[?1;2c"), "{replies:?}");
        assert!(replies.contains("\x1b[>0;0;0c"), "{replies:?}");
        assert!(replies.contains("\x1b[8;18;77t"), "{replies:?}");
        for _ in 0..2000 {
            state.pty.process(b"\x1b[6n");
        }
        assert!(state.pty.callbacks().bytes.len() <= MAX_REPLIES);
        assert!(state.pty.callbacks().dropped > 0);
    }
    #[test]
    fn line_diff_and_input_modes_preserve_the_pty_cursor() {
        let mut state = State::new(30, 110, &Ui::default());
        let a = state.frame(Instant::now());
        assert!(a.diff(Some(&a)).is_empty());
        state.pty.process(b"\x1b[?1h\x1b=\x1b[?2004hhello");
        let b = state.frame(Instant::now());
        let diff = b.diff(Some(&a));
        assert!(!diff.windows(4).any(|bytes| bytes == b"\x1b[2J"));
        assert!(diff.len() < b.diff(None).len() / 2);
        let mut parser = physical(&a);
        parser.process(&diff);
        assert_eq!(parser.screen().cursor_position(), b.cursor);
        assert!(parser.screen().application_cursor());
        assert!(parser.screen().application_keypad());
        assert!(parser.screen().bracketed_paste());
        assert!(parser.screen().contents().contains("hello"));
    }
    #[test]
    fn no_color_does_not_disable_layout_and_log_lf_is_normalized() {
        let ui = Ui {
            color: false,
            ..Ui::default()
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
    fn log_focus_pages_history_and_escape_returns_without_losing_native_keys() {
        let mut state = State::new(58, 106, &Ui::default());
        for index in 0..200 {
            state.log(format!("audit record {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        for byte in b"\x1b[17~" {
            assert!(state.keyboard(&[*byte], now, false).is_empty());
        }
        assert!(state.keyboard.logs);
        assert!(state.keyboard(b"\x1b[5~", now, false).is_empty());
        assert!(state.logs.screen().scrollback() > 0);
        state.keyboard(b"\x1b[H", now, false);
        assert!(state.logs.screen().contents().contains("audit record 000"));
        state.keyboard(b"\x1b[F", now, false);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(state.keyboard(b"\x1b", now, false).is_empty());
        assert!(
            state
                .keyboard(&[], now + Duration::from_millis(100), true)
                .is_empty()
        );
        assert!(!state.keyboard.logs);
        let mut forwarded = Vec::new();
        for byte in b"\x1b[A\x1b[5~\x1bOP\x1b[1;5C" {
            forwarded.extend(state.keyboard(&[*byte], now, false));
        }
        assert_eq!(forwarded, b"\x1b[A\x1b[5~\x1bOP\x1b[1;5C");
        state.keyboard(b"\x1b", now, false);
        assert_eq!(
            state.keyboard(&[], now + Duration::from_millis(100), true),
            b"\x1b"
        );
    }
    #[test]
    fn bracketed_paste_and_incomplete_shortcuts_round_trip_byte_for_byte() {
        let mut state = State::new(58, 106, &Ui::default());
        let now = Instant::now();
        let pasted = b"\x1b[200~literal \x1b[17~ \x1b[5~ \x1b[201~";
        let mut forwarded = Vec::new();
        for byte in pasted {
            forwarded.extend(state.keyboard(&[*byte], now, false));
        }
        assert_eq!(forwarded, pasted);
        assert!(!state.keyboard.logs);
        assert!(!state.keyboard.pasting);
        assert!(state.keyboard(b"\x1b[17", now, false).is_empty());
        assert_eq!(
            state.keyboard(&[], now + Duration::from_millis(100), true),
            b"\x1b[17"
        );
        assert_eq!(
            state.keyboard(b"~", now + Duration::from_millis(101), false),
            b"~"
        );
    }
    #[test]
    fn split_unicode_in_pty_and_log_views_uses_streaming_parser() {
        let mut state = State::new(30, 110, &Ui::default());
        for byte in "界e\u{301}".as_bytes() {
            state.pty.process(&[*byte]);
            state.log(&[*byte]);
        }
        let screen = physical(&state.frame(Instant::now()));
        assert_eq!(
            screen
                .screen()
                .cell(state.geometry.pty_top, 0)
                .unwrap()
                .contents(),
            "界"
        );
        assert_eq!(screen.screen().cell(0, 2).unwrap().contents(), "e\u{301}");
    }
    #[test]
    fn jsonl_and_disabled_ui_never_initialize_a_physical_display() {
        let mut ui = Ui {
            log_format: LogFormat::Jsonl,
            ..Ui::default()
        };
        assert!(Session::start(&ui).unwrap().is_none());
        ui.log_format = LogFormat::Pretty;
        ui.dashboard = false;
        assert!(Session::start(&ui).unwrap().is_none());
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
            state: Mutex::new(State::new(30, 110, &Ui::default())),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
        });
        let worker = shared.clone();
        let thread = std::thread::spawn(move || draw_loop(&worker, PhysicalOutput { file: slave }));
        let session = Session {
            shared: shared.clone(),
            thread: Some(thread),
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
    fn combined_keys_and_small_resize_return_visible_shell_to_live_logs() {
        let mut state = State::new(58, 106, &Ui::default());
        for index in 0..100 {
            state.log(format!("combined audit {index:03}\n").as_bytes());
        }
        let now = Instant::now();
        assert_eq!(state.keyboard(b"\x1b[17~\x1b[5~x", now, false), b"x");
        assert!(!state.keyboard.logs);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(
            state
                .logs
                .screen()
                .contents()
                .contains("combined audit 099")
        );
        assert!(state.keyboard(b"\x1b[17~\x1b[5~", now, false).is_empty());
        assert!(state.keyboard.logs);
        assert!(state.logs.screen().scrollback() > 0);
        state.resize(4, 106);
        let compact = state.frame(now);
        assert_eq!(state.geometry.log_rows, 0);
        assert!(!state.keyboard.logs);
        assert_eq!(state.logs.screen().scrollback(), 0);
        assert!(!compact.hidden);
        assert!(compact.rows.last().unwrap().contains("compact view"));
        assert!(!compact.rows.last().unwrap().contains("F6"));
        state.resize(58, 106);
        assert!(!state.frame(now).hidden);
        assert_eq!(state.logs.screen().scrollback(), 0);
    }
}
