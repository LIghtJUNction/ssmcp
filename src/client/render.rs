//! Human transcript only; never changes events retained for MCP consumers.
use super::{
    git_status::GitSnapshot,
    panel::{ControllerView, PanelSnapshot, ProgressView, TaskView},
    ui::Ui,
};
#[path = "activity.rs"]
mod activity;
#[path = "command.rs"]
mod command;
#[cfg(test)]
#[path = "command_process_tests.rs"]
mod command_process_tests;
#[cfg(test)]
#[path = "readable_log_fixture.rs"]
mod readable_log_fixture;
use activity::{Activities, key as activity_key, short_id};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

static DISPLAY: OnceLock<Mutex<Renderer>> = OnceLock::new();
static ACTIVITY_TICKER: OnceLock<()> = OnceLock::new();
// Serialize transcript emission and snapshots without holding DISPLAY while
// the compositor locks or writes the physical terminal.
static EMISSION: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum LogFormat {
    Pretty,
    PrettyVerbose,
    Jsonl,
}

impl LogFormat {
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pretty" => Ok(Self::Pretty),
            "pretty-verbose" => Ok(Self::PrettyVerbose),
            "jsonl" => Ok(Self::Jsonl),
            _ => Err(format!(
                "unsupported log format {value:?}; expected pretty, pretty-verbose, or jsonl"
            )),
        }
    }
}

pub(crate) fn configure(ui: Ui) {
    let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    renderer.raw = ui.log_format == LogFormat::Jsonl;
    renderer.verbose = ui.log_format == LogFormat::PrettyVerbose || ui.show_raw_json;
    renderer.color = ui.color
        && !renderer.raw
        && color_enabled(
            io::stderr().is_terminal(),
            std::env::var_os("NO_COLOR").is_some()
                || std::env::var("TERM").is_ok_and(|t| t == "dumb"),
        );
    renderer.ui = ui;
    drop(renderer);
    ACTIVITY_TICKER.get_or_init(|| {
        let started = std::thread::Builder::new()
            .name("ssmcp-activity".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let Some(display) = DISPLAY.get() else {
                        continue;
                    };
                    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
                    let dashboard = super::terminal::active();
                    let width = terminal_width();
                    let now = Instant::now();
                    let mut bytes = Vec::new();
                    let snapshot = {
                        let mut renderer = display.lock().unwrap_or_else(|e| e.into_inner());
                        renderer.width = width;
                        renderer.dashboard_active = dashboard && !renderer.raw;
                        let _ = renderer.flush_diagnostics_due(&mut bytes, now);
                        if renderer.dashboard_active {
                            Some(renderer.panel_snapshot(now))
                        } else {
                            let _ = renderer.heartbeat(&mut bytes, now, io::stderr().is_terminal());
                            None
                        }
                    };
                    publish(&bytes, snapshot);
                }
            });
        if started.is_err() {
            status(
                "client",
                "Warning",
                "periodic activity display is unavailable",
            );
        }
    });
}

pub(crate) fn current_ui() -> Ui {
    display()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .ui
        .clone()
}

pub(crate) fn apply_ui(ui: Ui) {
    configure(ui);
}

/// Observational data for alternate frontends. Original request payloads and
/// terminal input are absent; execution output has its own cursor-based interfaces.
pub(crate) fn snapshot() -> Value {
    let renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    renderer.snapshot(Instant::now())
}

fn display() -> &'static Mutex<Renderer> {
    DISPLAY.get_or_init(|| {
        Mutex::new(Renderer::new(
            false,
            color_enabled(
                io::stderr().is_terminal(),
                std::env::var_os("NO_COLOR").is_some()
                    || std::env::var("TERM").is_ok_and(|term| term == "dumb"),
            ),
            terminal_width(),
        ))
    })
}

fn terminal_width() -> usize {
    if let Some(columns) = super::terminal::log_width() {
        return columns;
    }
    #[cfg(unix)]
    {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        // SAFETY: ioctl writes into valid aligned winsize storage.
        if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0 {
            let columns = unsafe { size.assume_init() }.ws_col;
            if columns > 8 {
                return usize::from(columns);
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 8 && *n <= 1000)
        .unwrap_or(100)
}

fn color_enabled(tty: bool, no_color: bool) -> bool {
    tty && !no_color
}

pub(crate) fn message(text: &str) {
    if let Err(error) = super::interface::history::record("assistant", text) {
        super::terminal::notice(&format!("Message history could not be saved: {error}"));
    }
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    let event = json!({"type":"ssmcp.message","format":"markdown","text":text});
    crate::session::attach::record_event("agent", &event);
    let width = terminal_width().min(1000) as u16;
    let (raw, mut ui) = {
        let renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        let mut ui = renderer.ui.clone();
        ui.color = renderer.color;
        (renderer.raw, ui)
    };
    let mut bytes = Vec::new();
    if raw {
        let safe = crate::audit::event(event);
        let _ = writeln!(bytes, "{safe}");
    } else {
        if !super::terminal::active() {
            ui.color &= io::stderr().is_terminal();
        }
        let _ = writeln!(bytes);
        let _ = painted(
            &mut bytes,
            ui.color,
            &format!("1;{}", ui.color("accent")),
            "[AGENT]\n",
        );
        for row in super::terminal::markdown::rows(text, width, &ui) {
            let _ = writeln!(bytes, "{row}");
        }
        let _ = writeln!(bytes);
    }
    publish(&bytes, None);
}

pub(crate) fn event(label: &str, value: &Value) {
    if value["type"] == "ssmcp.code_mode" {
        super::terminal::audit(value);
    }
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    crate::session::attach::record_event(label, value);
    let dashboard = super::terminal::active();
    let width = terminal_width();
    let mut bytes = Vec::new();
    let snapshot = {
        let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        renderer.width = width;
        renderer.dashboard_active = dashboard && !renderer.raw;
        let _ = renderer.event(&mut bytes, label, value);
        renderer
            .dashboard_active
            .then(|| renderer.panel_snapshot(Instant::now()))
    };
    publish(&bytes, snapshot);
}

/// Held by the individual native request, including while it awaits I/O.
/// Dropping a cancelled request clears only that request's control indicator.
pub(crate) struct ControllerGuard {
    id: u64,
}

impl Drop for ControllerGuard {
    fn drop(&mut self) {
        controller_finished(self.id);
    }
}

pub(crate) fn controller_started(
    id: u64,
    controller: Option<&crate::wire::Controller>,
) -> ControllerGuard {
    event(
        "controller",
        &json!({"type":"ssmcp.controller.started","request_id":id,"controller":controller}),
    );
    ControllerGuard { id }
}

pub(crate) fn controller_finished(id: u64) {
    event(
        "controller",
        &json!({"type":"ssmcp.controller.finished","request_id":id}),
    );
}

pub(crate) fn finish(label: &str) {
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    crate::session::attach::record_finish(label);
    let mut bytes = Vec::new();
    {
        let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        let _ = renderer.finish_group(&mut bytes, label);
    }
    publish(&bytes, None);
}

/// Git polling has no dependency on renderer or terminal locks.
pub(super) fn set_git_status(snapshot: Arc<GitSnapshot>) {
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    let dashboard = super::terminal::active();
    let panel = {
        let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        renderer.git = Some(snapshot);
        (dashboard && !renderer.raw).then(|| renderer.panel_snapshot(Instant::now()))
    };
    if let Some(panel) = panel {
        super::terminal::set_panel(panel);
    }
}

fn publish(bytes: &[u8], snapshot: Option<PanelSnapshot>) {
    if !bytes.is_empty() && !super::terminal::log(bytes) {
        let mut out = io::stderr().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }
    if let Some(snapshot) = snapshot {
        super::terminal::set_panel(snapshot);
    }
}

#[derive(Default)]
struct Lines {
    pending: String,
    emitted: usize,
    column: usize,
    old: usize,
    new: usize,
    hunk: bool,
    tone: String,
    summarize_json: bool,
    folded_bytes: usize,
}

struct Renderer {
    raw: bool,
    verbose: bool,
    color: bool,
    width: usize,
    ui: Ui,
    streams: HashMap<String, Lines>,
    requests: HashMap<String, Request>,
    deferred_commands: HashMap<String, Option<String>>,
    activities: Activities,
    last_activity_summary: Instant,
    last_pty_output: Option<Instant>,
    current_stream: Option<String>,
    stream_offsets: HashMap<String, u64>,
    workspace_root: Option<std::path::PathBuf>,
    dashboard_active: bool,
    git: Option<Arc<GitSnapshot>>,
    native_progress: HashMap<String, ProgressView>,
    discovery_success: HashMap<String, Value>,
    controllers: HashMap<String, ControllerView>,
    last_controller: Option<ControllerView>,
    diagnostics: HashMap<String, Diagnostic>,
}

struct Diagnostic {
    pending: String,
    key: String,
    occurrences: u64,
    unreported: u64,
    updated: Instant,
    reported: Instant,
}

impl Diagnostic {
    fn new(now: Instant) -> Self {
        Self {
            pending: String::new(),
            key: String::new(),
            occurrences: 0,
            unreported: 0,
            updated: now,
            reported: now,
        }
    }
}

struct Request {
    method: String,
    started: Instant,
    command: Option<String>,
    process_id: Option<String>,
}

impl Renderer {
    fn new(raw: bool, color: bool, width: usize) -> Self {
        Self {
            raw,
            verbose: false,
            color,
            width: width.max(12),
            ui: Ui::default(),
            streams: HashMap::new(),
            requests: HashMap::new(),
            deferred_commands: HashMap::new(),
            activities: Activities::default(),
            last_activity_summary: Instant::now(),
            last_pty_output: None,
            current_stream: None,
            stream_offsets: HashMap::new(),
            workspace_root: std::env::current_dir().ok(),
            dashboard_active: false,
            git: None,
            native_progress: HashMap::new(),
            discovery_success: HashMap::new(),
            controllers: HashMap::new(),
            last_controller: None,
            diagnostics: HashMap::new(),
        }
    }

    fn event(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<()> {
        let mut safe = crate::audit::event(value.clone());
        omit_image_bytes(&mut safe);
        let value = &safe;
        let controller_event = self.controller(out, value)?;
        if self.raw {
            writeln!(out, "{value}")?;
        } else if value["type"] != "ssmcp.code_mode"
            && !controller_event
            && !self.backend_diagnostic(out, label, value)?
            && !self.activity(out, label, value)?
            && !self.rpc(out, label, value)?
        {
            self.value(out, label, value, "Event", 0)?;
        }
        if self.verbose && !self.raw {
            self.finish(out, label)?;
            painted(
                out,
                self.color,
                &self.ui.color("gray"),
                &format!(
                    "[RAW] {}\n",
                    serde_json::to_string_pretty(&debug_metadata(value)).unwrap_or_default()
                ),
            )?;
        }
        out.flush()
    }

    fn snapshot(&self, now: Instant) -> Value {
        let panel = self.panel_snapshot(now);
        let controller = |value: &ControllerView| {
            json!({
                "request_id":value.request_id,
                "identity":value.identity,
                "elapsed_ms":now.saturating_duration_since(value.started).as_millis().min(u128::from(u64::MAX)) as u64
            })
        };
        let git = panel.git.as_ref().map(|git| {
            json!({
                "state":match git.status {
                    super::git_status::GitState::Checking => "checking",
                    super::git_status::GitState::Ready => "ready",
                    super::git_status::GitState::NotRepository => "not-repository",
                    super::git_status::GitState::Unavailable => "unavailable",
                    super::git_status::GitState::Error => "error",
                },
                "branch":git.branch,"head":git.head,"upstream":git.upstream,
                "ahead":git.ahead,"behind":git.behind,"detached":git.detached,"unborn":git.unborn,
                "staged":git.staged,"unstaged":git.unstaged,"untracked":git.untracked,"conflicts":git.conflicts,
                "stale":git.stale,
                "changed_files":git.changed_files.iter().map(|entry|json!({"status":entry.status,"path":entry.path})).collect::<Vec<_>>(),
                "history":git.history.commits.iter().map(|commit|json!({"hash":commit.full_hash,"subject":commit.subject,"committed_at_unix":commit.committed_at_unix})).collect::<Vec<_>>(),
                "history_stale":git.history.stale,"error":git.last_error
            })
        });
        crate::audit::event(json!({
            "schema_version":1,
            "controllers":panel.controllers.iter().map(controller).collect::<Vec<_>>(),
            "last_controller":panel.last_controller.as_ref().map(controller),
            "tasks":self.activities.snapshot().iter().map(|task|json!({
                "id":task.id,"display_id":short_id(&task.id),"description":task.description,"cwd":task.cwd.as_deref().map(|cwd|self.path_label(cwd)),
                "elapsed_ms":now.saturating_duration_since(task.started).as_millis().min(u128::from(u64::MAX)) as u64
            })).collect::<Vec<_>>(),
            "progress":panel.progress.iter().map(|progress|json!({
                "source":progress.source,"message":progress.message,"value":progress.value,"total":progress.total,
                "age_ms":now.saturating_duration_since(progress.updated).as_millis().min(u128::from(u64::MAX)) as u64
            })).collect::<Vec<_>>(),
            "git":git,
            "diagnostics":self.diagnostics.iter().filter(|(_, item)|item.occurrences > 0).map(|(source,item)|json!({
                "source":source,"occurrences":item.occurrences,"folded":item.occurrences.saturating_sub(1),
                "message":diagnostic_summary(&item.key),
                "age_ms":now.saturating_duration_since(item.updated).as_millis().min(u128::from(u64::MAX)) as u64
            })).collect::<Vec<_>>()
        }))
    }

    fn backend_diagnostic(
        &mut self,
        out: &mut impl Write,
        label: &str,
        value: &Value,
    ) -> io::Result<bool> {
        if self.verbose || !matches!(value["type"].as_str(), Some("stderr" | "ssmcp.stderr")) {
            return Ok(false);
        }
        let Some(text) = value["text"].as_str() else {
            return Ok(false);
        };
        let now = Instant::now();
        if self.diagnostics.len() >= 64
            && !self.diagnostics.contains_key(label)
            && let Some(oldest) = self
                .diagnostics
                .iter()
                .min_by_key(|(_, item)| item.updated)
                .map(|(source, _)| source.clone())
        {
            self.finish_diagnostic(out, &oldest)?;
            self.diagnostics.remove(&oldest);
        }
        let mut diagnostic = self
            .diagnostics
            .remove(label)
            .unwrap_or_else(|| Diagnostic::new(now));
        diagnostic.pending.push_str(text);
        while let Some(end) = diagnostic.pending.find(['\n', '\r']) {
            let line = diagnostic.pending[..end].to_owned();
            diagnostic.pending.drain(..=end);
            self.diagnostic_line(out, label, &mut diagnostic, &line, now)?;
        }
        if diagnostic.pending.len() > 64 * 1024 {
            let line = std::mem::take(&mut diagnostic.pending);
            self.diagnostic_line(out, label, &mut diagnostic, &line, now)?;
        }
        diagnostic.updated = now;
        self.diagnostics.insert(label.into(), diagnostic);
        Ok(true)
    }

    fn diagnostic_line(
        &mut self,
        out: &mut impl Write,
        label: &str,
        diagnostic: &mut Diagnostic,
        text: &str,
        now: Instant,
    ) -> io::Result<()> {
        let text = clean(text);
        if text.trim().is_empty() {
            return Ok(());
        }
        let normalized = diagnostic_key(&text);
        if normalized == diagnostic.key && diagnostic.occurrences > 0 {
            diagnostic.occurrences = diagnostic.occurrences.saturating_add(1);
            diagnostic.unreported = diagnostic.unreported.saturating_add(1);
            diagnostic.updated = now;
            return Ok(());
        }
        self.report_diagnostic_repeats(out, label, diagnostic, now)?;
        diagnostic.key = normalized;
        diagnostic.occurrences = 1;
        diagnostic.updated = now;
        diagnostic.reported = now;
        if diagnostic.key.contains("AuthRequired") || diagnostic.key.contains("auth_required") {
            self.block(out, label, "Warning", &diagnostic_summary(&diagnostic.key))
        } else {
            self.block(out, label, "stderr", &text)
        }
    }

    fn report_diagnostic_repeats(
        &mut self,
        out: &mut impl Write,
        label: &str,
        diagnostic: &mut Diagnostic,
        now: Instant,
    ) -> io::Result<bool> {
        if diagnostic.unreported == 0 {
            return Ok(false);
        }
        self.summary(
            out,
            label,
            "Warning",
            &format!(
                "same diagnostic ×{} · {} repeats folded · --verbose for full text",
                diagnostic.occurrences, diagnostic.unreported
            ),
        )?;
        diagnostic.unreported = 0;
        diagnostic.reported = now;
        Ok(true)
    }

    fn flush_diagnostics_due(&mut self, out: &mut impl Write, now: Instant) -> io::Result<()> {
        if self.raw || self.verbose {
            return Ok(());
        }
        let labels = self
            .diagnostics
            .iter()
            .filter(|(_, diagnostic)| {
                (diagnostic.unreported > 0
                    && now.saturating_duration_since(diagnostic.reported)
                        >= Duration::from_secs(30))
                    || (!diagnostic.pending.is_empty()
                        && now.saturating_duration_since(diagnostic.updated)
                            >= Duration::from_secs(1))
            })
            .map(|(label, _)| label.clone())
            .collect::<Vec<_>>();
        for label in labels {
            if let Some(mut diagnostic) = self.diagnostics.remove(&label) {
                if !diagnostic.pending.is_empty()
                    && now.saturating_duration_since(diagnostic.updated) >= Duration::from_secs(1)
                {
                    let pending = std::mem::take(&mut diagnostic.pending);
                    self.diagnostic_line(out, &label, &mut diagnostic, &pending, now)?;
                }
                self.report_diagnostic_repeats(out, &label, &mut diagnostic, now)?;
                self.diagnostics.insert(label, diagnostic);
            }
        }
        Ok(())
    }

    fn finish_diagnostic(&mut self, out: &mut impl Write, label: &str) -> io::Result<()> {
        if let Some(mut diagnostic) = self.diagnostics.remove(label) {
            let now = Instant::now();
            let pending = std::mem::take(&mut diagnostic.pending);
            self.diagnostic_line(out, label, &mut diagnostic, &pending, now)?;
            self.report_diagnostic_repeats(out, label, &mut diagnostic, now)?;
            self.diagnostics.insert(label.into(), diagnostic);
        }
        Ok(())
    }

    fn panel_snapshot(&self, now: Instant) -> PanelSnapshot {
        let tasks = self
            .activities
            .snapshot()
            .into_iter()
            .map(|task| TaskView {
                id: short_id(&task.id),
                description: task.description,
                cwd: task.cwd.as_deref().map(|cwd| self.path_label(cwd)),
                started: task.started,
            })
            .collect();
        let mut progress = self
            .native_progress
            .values()
            .filter(|value| now.saturating_duration_since(value.updated) < Duration::from_secs(300))
            .cloned()
            .collect::<Vec<_>>();
        progress.sort_by_key(|value| std::cmp::Reverse(value.updated));
        let mut controllers = self.controllers.values().cloned().collect::<Vec<_>>();
        controllers.sort_by_key(|controller| controller.started);
        PanelSnapshot {
            controllers,
            last_controller: self.last_controller.clone(),
            tasks,
            progress,
            git: self.git.clone(),
        }
    }

    fn controller(&mut self, out: &mut impl Write, value: &Value) -> io::Result<bool> {
        let kind = value["type"].as_str().unwrap_or("");
        if !matches!(
            kind,
            "ssmcp.controller.started" | "ssmcp.controller.finished"
        ) {
            return Ok(false);
        }
        let Some(request_id) = value["request_id"].as_u64().map(|id| id.to_string()) else {
            return Ok(true);
        };
        if kind == "ssmcp.controller.finished" {
            if let Some(controller) = self.controllers.remove(&request_id) {
                self.last_controller = Some(controller);
            }
            return Ok(true);
        }
        let identity =
            serde_json::from_value::<crate::wire::Controller>(value["controller"].clone())
                .ok()
                .filter(|controller| controller.validate().is_ok());
        let changed = self
            .last_controller
            .as_ref()
            .is_none_or(|last| last.identity != identity);
        let controller = ControllerView {
            request_id: request_id.clone(),
            identity,
            started: Instant::now(),
        };
        // The real transport allows only 16 requests. Keep malformed/replayed
        // display events bounded as well, without evicting an active identity.
        if self.controllers.len() < crate::wire::MAX_INFLIGHT
            || self.controllers.contains_key(&request_id)
        {
            self.controllers
                .insert(request_id.clone(), controller.clone());
        }
        self.last_controller = Some(controller.clone());
        if changed && !self.raw {
            let identity = controller.identity.as_ref();
            let name = identity
                .and_then(|value| value.name.as_deref())
                .unwrap_or("Client unknown");
            let model = identity
                .and_then(|value| value.model.as_deref())
                .unwrap_or("Model unknown");
            let source = match identity.map(|value| value.source) {
                Some(crate::wire::ControllerSource::CallerReported) => "caller-reported",
                Some(crate::wire::ControllerSource::McpClientInfo) => "MCP clientInfo",
                None => "metadata missing",
            };
            self.summary(
                out,
                "controller",
                "Usage",
                &format!("#{request_id} · {model} · {name} · {source}"),
            )?;
        }
        Ok(true)
    }

    fn path_label(&self, path: &str) -> String {
        self.workspace_root
            .as_ref()
            .and_then(|root| std::path::Path::new(path).strip_prefix(root).ok())
            .map(|relative| {
                if relative.as_os_str().is_empty() {
                    ".".into()
                } else {
                    relative.display().to_string()
                }
            })
            .unwrap_or_else(|| path.into())
    }

    fn active_summary(&mut self, out: &mut impl Write, now: Instant) -> io::Result<()> {
        if self.dashboard_active {
            self.last_activity_summary = now;
            return Ok(());
        }
        if let Some(summary) = self.activities.summary(now) {
            self.summary(out, "tasks", "Progress", &summary)?;
        }
        self.last_activity_summary = now;
        Ok(())
    }

    fn heartbeat(&mut self, out: &mut impl Write, now: Instant, tty: bool) -> io::Result<bool> {
        let interval = self.ui.activity_interval_secs;
        if self.raw || interval == 0 || self.activities.len() == 0 {
            return Ok(false);
        }
        let interval = Duration::from_secs(if tty { interval } else { interval.max(30) });
        if now.saturating_duration_since(self.last_activity_summary) < interval
            || self
                .last_pty_output
                .is_some_and(|last| now.saturating_duration_since(last) < Duration::from_secs(2))
        {
            return Ok(false);
        }
        // Append a normal transcript line. Never clear/redraw the user's PTY.
        self.active_summary(out, now)?;
        out.flush()?;
        Ok(true)
    }

    fn finish_visible(&mut self, out: &mut impl Write) -> io::Result<()> {
        let keys = self
            .streams
            .iter()
            .filter(|(_, lines)| {
                !(lines.summarize_json
                    && lines.emitted == 0
                    && (lines.folded_bytes > 0 || json_record(&lines.pending)))
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        for key in keys {
            self.finish(out, &key)?;
        }
        self.current_stream = None;
        Ok(())
    }

    fn finish_process(&mut self, out: &mut impl Write, label: &str, id: &str) -> io::Result<()> {
        let prefix = format!("{label}\0{id}\0");
        let keys = self
            .streams
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            if self.current_stream.as_deref() != Some(&key)
                && self
                    .streams
                    .get(&key)
                    .is_some_and(|lines| !lines.pending.is_empty() || lines.folded_bytes > 0)
            {
                self.heading(
                    out,
                    &format!("{label} · {}", short_id(id)),
                    if key.ends_with("\0stderr") {
                        "stderr"
                    } else {
                        "stdout"
                    },
                )?;
            }
            self.finish(out, &key)?;
        }
        self.current_stream = None;
        Ok(())
    }

    fn activity(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<bool> {
        if value["method"] == "notifications/progress" {
            let params = &value["params"];
            let token = params
                .get("progressToken")
                .map(|v| compact(v, 0))
                .unwrap_or_default();
            let message = params["message"]
                .as_str()
                .map(|s| activity::single_line(s, 100))
                .unwrap_or_default();
            let now = Instant::now();
            self.native_progress.retain(|_, update| {
                now.saturating_duration_since(update.updated) < Duration::from_secs(300)
            });
            if self.native_progress.len() >= 64
                && let Some(oldest) = self
                    .native_progress
                    .iter()
                    .min_by_key(|(_, update)| update.updated)
                    .map(|(key, _)| key.clone())
            {
                self.native_progress.remove(&oldest);
            }
            let progress_key = format!(
                "{label}\0{}",
                params
                    .get("progressToken")
                    .map(Value::to_string)
                    .unwrap_or_default()
            );
            self.native_progress.insert(
                progress_key,
                ProgressView {
                    source: if token.is_empty() {
                        label.into()
                    } else {
                        format!("{label}:{}", short_id(&token))
                    },
                    message: (!message.is_empty()).then_some(message.clone()),
                    value: params["progress"].as_f64(),
                    total: params["total"].as_f64(),
                    updated: now,
                },
            );
            if self.dashboard_active {
                return Ok(true);
            }
            self.summary(
                out,
                label,
                "Progress",
                &format!(
                    "{} {}  {}",
                    short_id(&token),
                    message,
                    activity::progress(params)
                ),
            )?;
            return Ok(true);
        }
        let kind = value["type"].as_str().unwrap_or("");
        let id = value["job_id"]
            .as_str()
            .or_else(|| value["id"].as_str())
            .unwrap_or("");
        match kind {
            "exec.started" | "ssmcp.exec.started" => {
                let command = command_label(&value["command"]);
                self.activities.start(
                    activity_key("job", label, id),
                    id.into(),
                    command.clone(),
                    value["cwd"].as_str().map(str::to_owned),
                    Instant::now(),
                );
                self.summary(
                    out,
                    label,
                    "Running",
                    &format!("{}  {command}  started", short_id(id)),
                )?;
                if let Some(cwd) = value["cwd"].as_str() {
                    painted(
                        out,
                        self.color,
                        &self.ui.color("gray"),
                        &format!("       cwd {}\n", clean(&self.path_label(cwd))),
                    )?;
                }
                let details = command::Command::new(&value["command"]);
                if details.script.is_some()
                    || details.invocation.chars().count() > self.width.saturating_sub(25)
                {
                    self.command_details(out, label, &value["command"])?;
                }
                self.last_activity_summary = Instant::now();
                if self.activities.len() > 1 {
                    self.active_summary(out, Instant::now())?;
                }
            }
            "exec.stdout" | "exec.stderr" | "ssmcp.exec.stdout" | "ssmcp.exec.stderr"
            | "process_output" => {
                // Existing Codex-compatible delta events already take the single
                // streaming path below. Do not render their duplicate text field.
                if value.pointer("/params/deltaBase64").is_some() {
                    return Ok(false);
                }
                let stream = value["stream"]
                    .as_str()
                    .unwrap_or(if kind.ends_with("stderr") {
                        "stderr"
                    } else {
                        "stdout"
                    });
                let process = value["job_id"]
                    .as_str()
                    .or_else(|| value["process_id"].as_str())
                    .or_else(|| value["id"].as_str())
                    .unwrap_or("command");
                let data = value["data_base64"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| value["text"].as_str().map(|text| STANDARD.encode(text)));
                if let Some(data) = data {
                    self.value(
                        out,
                        label,
                        &json!({
                            "method":"command/exec/outputDelta",
                            "params":{"processId":process,"stream":stream,"deltaBase64":data}
                        }),
                        "Command",
                        0,
                    )?;
                }
            }
            "exec.exited" | "ssmcp.exec.exited" | "ssmcp.exec.finished" => {
                self.finish_process(out, label, id)?;
                let job = self.activities.finish(&activity_key("job", label, id));
                let failed = value["exit_code"].as_i64().is_some_and(|v| v != 0)
                    || value["exit_signal"].as_i64().is_some()
                    || !value["error"].is_null();
                let title = if value["timed_out"] == true {
                    "Timeout"
                } else if value["cancelled"] == true {
                    "Warning"
                } else if failed {
                    "Error"
                } else if value["exit_code"].as_i64() == Some(0) {
                    "Success"
                } else {
                    "Warning"
                };
                let state = value["state"].as_str().unwrap_or("completed");
                let elapsed = value["duration_ms"]
                    .as_u64()
                    .map(|ms| ms as f64 / 1000.0)
                    .or_else(|| job.as_ref().map(|job| job.started.elapsed().as_secs_f64()))
                    .map(|seconds| format!("  {seconds:.2}s"))
                    .unwrap_or_else(|| "  duration unknown".into());
                let exit = value["exit_code"]
                    .as_i64()
                    .map(|v| format!("  exit={v}"))
                    .or_else(|| {
                        value["exit_signal"]
                            .as_i64()
                            .map(|signal| format!("  signal={signal}"))
                    })
                    .unwrap_or_else(|| "  exit unknown".into());
                self.summary(
                    out,
                    label,
                    title,
                    &format!(
                        "{}  {state}{exit}{elapsed} · {}",
                        short_id(id),
                        job.as_ref()
                            .map(|job| job.description.as_str())
                            .unwrap_or("command")
                    ),
                )?;
                if self.verbose
                    && let Some(cwd) = job.as_ref().and_then(|job| job.cwd.as_deref())
                {
                    painted(
                        out,
                        self.color,
                        &self.ui.color("gray"),
                        &format!("       cwd {}\n", clean(&self.path_label(cwd))),
                    )?;
                }
                if let Some(error) = value["error"].as_str() {
                    self.block(out, label, "Error", error)?;
                }
                if self.activities.len() > 0 {
                    self.active_summary(out, Instant::now())?;
                }
                for stream in ["stdout", "stderr"] {
                    if let Some(artifact) = value[stream].get("artifact").filter(|v| !v.is_null()) {
                        self.artifact(out, label, id, stream, artifact)?;
                    } else if let Some(error) = value[stream]["error"].as_str() {
                        self.summary(out, label, "Warning", &format!("{id}  {stream}: {error}"))?;
                    }
                }
            }
            "ssmcp.output_archived" => self.artifact(
                out,
                label,
                id,
                value["stream"].as_str().unwrap_or("output"),
                &value["artifact"],
            )?,
            "file.read" | "ssmcp.file.read" | "ssmcp.fileread" => {
                self.summary(
                    out,
                    label,
                    "Read",
                    &self.path_label(value["path"].as_str().unwrap_or("")),
                )?;
            }
            "file.write" | "ssmcp.file.write" => {
                let path = value["path"].as_str().unwrap_or("");
                let bytes = value["written"]
                    .as_u64()
                    .map(|n| format!("  {n} bytes"))
                    .unwrap_or_default();
                self.summary(
                    out,
                    label,
                    "Edit",
                    &format!("{}{bytes}", self.path_label(path)),
                )?;
            }
            "file.diff" | "ssmcp.file.diff" => {
                let path = value["path"].as_str().unwrap_or("");
                let counts = value["added_lines"]
                    .as_u64()
                    .zip(value["removed_lines"].as_u64())
                    .map(|(added, removed)| format!("  +{added} / -{removed}"))
                    .unwrap_or_else(|| "  line counts unknown".into());
                self.summary(
                    out,
                    label,
                    "Diff",
                    &format!("{}  {}", counts.trim(), self.path_label(path)),
                )?;
                // A preview and the complete inline diff may both be present.
                // Select one; querying fs.events/output.read does not replay it.
                if let Some(preview) = value["preview"]
                    .as_str()
                    .or_else(|| value["diff"].as_str())
                    .filter(|v| !v.is_empty())
                {
                    self.text(out, label, preview)?;
                    self.finish(out, label)?;
                }
                if let Some(artifact) = value.get("diff_artifact").filter(|v| !v.is_null()) {
                    self.artifact(out, label, "", "diff", artifact)?;
                }
                if value["diff_complete"] == false {
                    let reason = value["diff_unavailable_reason"]
                        .as_str()
                        .or_else(|| value["output_error"].as_str())
                        .unwrap_or("snapshot unavailable");
                    self.summary(
                        out,
                        label,
                        "Warning",
                        &format!("diff unavailable: {reason}"),
                    )?;
                } else if value["preview_truncated"] == true {
                    let hint = value
                        .get("diff_artifact")
                        .and_then(|a| a["id"].as_str())
                        .map(|id| format!("diff preview folded; full diff: outputs.read · {id}"))
                        .or_else(|| {
                            value.get("cursor").and_then(Value::as_u64).map(|cursor| {
                                format!("diff preview folded; fs.events cursor {cursor}")
                            })
                        })
                        .unwrap_or_else(|| "diff preview folded".into());
                    self.summary(out, label, "Output", &hint)?;
                }
            }
            "ssmcp.filechange" => {
                // Older/native reports are observations, not evidence that this
                // broker independently audited an external write.
                self.summary(
                    out,
                    label,
                    "Files",
                    &format!(
                        "reported change  {}",
                        self.path_label(value["path"].as_str().unwrap_or(""))
                    ),
                )?;
                if let Some(diff) = value["diff"].as_str().filter(|v| !v.is_empty()) {
                    self.text(out, label, diff)?;
                    self.finish(out, label)?;
                }
            }
            "skill.discovery.completed" | "ssmcp.skill.discovery.completed" => {
                let detail = value["shown"]
                    .as_u64()
                    .map(|shown| format!("discovery completed  {shown} summaries"))
                    .unwrap_or_else(|| "discovery completed".into());
                self.summary(out, label, "Skill", &detail)?;
            }
            "skill.discovered"
            | "ssmcp.skill.discovered"
            | "skill.read"
            | "ssmcp.skill.read"
            | "skill.invoked"
            | "ssmcp.skill.invoked"
            | "ssmcp.skill_use" => {
                let state = if kind.ends_with(".discovered") {
                    "discovered"
                } else if kind.ends_with(".read") {
                    "read"
                } else if kind.ends_with(".invoked") {
                    "invoked"
                } else {
                    "activity reported; invocation unconfirmed"
                };
                let subject = value["path"]
                    .as_str()
                    .or_else(|| value["name"].as_str())
                    .or_else(|| value["skill"].as_str())
                    .or_else(|| value["tool"].as_str())
                    .unwrap_or("");
                self.summary(
                    out,
                    label,
                    "Skill",
                    &format!("{state}  {}", self.path_label(subject)),
                )?;
            }
            "harness.tool.started"
            | "ssmcp.harness.tool.started"
            | "harness.tool.completed"
            | "ssmcp.harness.tool.completed" => {
                let provider = value["provider"].as_str().unwrap_or(label);
                let tool = value["tool"].as_str().unwrap_or("");
                let tool_name = if tool.is_empty() {
                    provider.to_owned()
                } else if tool.starts_with(&format!("{provider}.")) {
                    tool.to_owned()
                } else {
                    format!("{provider}.{tool}")
                };
                let call = value
                    .get("call_id")
                    .map(|v| format!("  call={}", short_id(&compact(v, 0))))
                    .unwrap_or_default();
                let call_id = value
                    .get("call_id")
                    .map(|v| compact(v, 0))
                    .unwrap_or_default();
                let key = activity_key("call", provider, &call_id);
                if value["rpc_correlation"] == true && value["command_rpc_expected"] == true {
                    if kind.ends_with(".started") {
                        // The exact native request supplies the useful command.
                        // Keep a pending activity so a blocked dispatch remains visible.
                        if self.deferred_commands.len() < 2048 {
                            self.deferred_commands.insert(key.clone(), None);
                            self.activities.start(
                                key,
                                format!("{provider}:{}", short_id(&call_id)),
                                format!("{tool_name} · awaiting command"),
                                None,
                                Instant::now(),
                            );
                            return Ok(true);
                        }
                    } else {
                        self.activities.finish(&key);
                        if let Some(Some(request_key)) = self.deferred_commands.remove(&key) {
                            self.activities.finish(&request_key);
                            if value["command_rpc_completed"] != true
                                && let Some(key) = request_key.strip_prefix("rpc\0")
                                && let Some(request) = self.requests.remove(key)
                                && let Some(process) = request.process_id
                            {
                                self.finish_process(out, provider, &process)?;
                            }
                        }
                        if value["command_rpc_completed"] == true {
                            // Native status (including nonzero exit) is authoritative.
                            // Verbose/JSONL still retain the wrapper audit event.
                            return Ok(true);
                        }
                    }
                }
                if kind.ends_with(".started") {
                    if !call_id.is_empty() {
                        self.activities.start(
                            key,
                            format!("{provider}:{}", short_id(&call_id)),
                            tool_name.clone(),
                            None,
                            Instant::now(),
                        );
                    }
                    self.summary(out, "", "Request", &format!("{tool_name}{call}  started"))?;
                    self.last_activity_summary = Instant::now();
                    if self.activities.len() > 1 {
                        self.active_summary(out, Instant::now())?;
                    }
                } else {
                    self.activities.finish(&key);
                    let success = value["success"] == true;
                    let state = if success { "completed" } else { "failed" };
                    let elapsed = value["duration_ms"]
                        .as_u64()
                        .map(|v| format!("  {:.2}s", v as f64 / 1000.0))
                        .unwrap_or_default();
                    self.summary(
                        out,
                        "",
                        if success { "Success" } else { "Error" },
                        &format!("{tool_name}{call}  {state}{elapsed}"),
                    )?;
                    if self.activities.len() > 0 {
                        self.active_summary(out, Instant::now())?;
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn artifact(
        &mut self,
        out: &mut impl Write,
        label: &str,
        id: &str,
        stream: &str,
        artifact: &Value,
    ) -> io::Result<()> {
        let bytes = artifact["bytes"].as_u64().unwrap_or(0);
        let lines = artifact["lines"].as_u64().unwrap_or(0);
        let state = if artifact["complete"] == true {
            "saved"
        } else if artifact["error"].is_string() {
            "partial"
        } else {
            "saving"
        };
        self.summary(
            out,
            label,
            "Output",
            &format!("{id}  {stream} {state}  {bytes} bytes · {lines} lines"),
        )?;
        if let Some(path) = artifact["path"].as_str() {
            painted(
                out,
                self.color,
                &self.ui.color("gray"),
                &format!("       {}\n", clean(path)),
            )?;
        }
        if let Some(id) = artifact["id"].as_str() {
            painted(
                out,
                self.color,
                &self.ui.color("gray"),
                &format!("       artifact {} · outputs.read\n", clean(id)),
            )?;
        }
        if let Some(error) = artifact["error"].as_str() {
            self.summary(out, label, "Warning", error)?;
        }
        Ok(())
    }

    // Correlate at the RPC boundary, scoped by backend so identical IDs cannot collide.
    fn rpc(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<bool> {
        let v = value
            .get("request")
            .or_else(|| value.get("response"))
            .unwrap_or(value);
        let Some(id) = v.get("id").filter(|id| !id.is_null()) else {
            return Ok(false);
        };
        let id = id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string());
        let key = format!("{label}\0{id}");
        let id_text = if self.ui.show_request_id {
            format!("  id={}", short_id(&id))
        } else {
            String::new()
        };
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            let params = &v["params"];
            let params = params.get("arguments").unwrap_or(params);
            let command = params.get("command").or_else(|| params.get("cmd"));
            let command_description = (method == "command/exec")
                .then(|| command.map(command_label))
                .flatten();
            if self.requests.len() >= 2048 {
                for request_key in self.requests.keys() {
                    self.activities.finish(&format!("rpc\0{request_key}"));
                }
                self.requests.clear();
            }
            self.requests.insert(
                key,
                Request {
                    method: method.into(),
                    started: Instant::now(),
                    command: command_description.clone(),
                    process_id: params
                        .get("processId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            );
            if value["internal_discovery"] == true
                && !self.verbose
                && self
                    .discovery_success
                    .contains_key(&format!("{label}\0{method}"))
            {
                return Ok(true);
            }
            if let Some(description) = &command_description {
                let request_key = activity_key("rpc", label, &id);
                if let Some(call_id) = value.get("harness_call_id") {
                    let call_key = activity_key("call", label, &compact(call_id, 0));
                    if let Some(request) = self.deferred_commands.get_mut(&call_key) {
                        self.activities.finish(&call_key);
                        *request = Some(request_key.clone());
                    }
                }
                self.activities.start(
                    request_key,
                    format!("{label}:{}", short_id(&id)),
                    description.clone(),
                    params["cwd"].as_str().map(str::to_owned),
                    Instant::now(),
                );
                self.summary(
                    out,
                    label,
                    "Running",
                    &format!(
                        "{}{description}",
                        if id_text.is_empty() {
                            String::new()
                        } else {
                            format!("{}  ", id_text.trim())
                        }
                    ),
                )?;
                self.last_activity_summary = Instant::now();
            } else {
                self.summary(out, label, "Request", &format!("{method}{id_text}"))?;
            }
            let mut context = Vec::new();
            for field in ["cwd", "name", "tool", "processId"] {
                if !self.verbose && field == "processId" {
                    continue;
                }
                if let Some(v) = params.get(field).filter(|v| !v.is_null()) {
                    let text = match (field, v.as_str()) {
                        ("cwd", Some(path)) => self.path_label(path),
                        ("processId", Some(id)) => short_id(id),
                        _ => compact(v, 0),
                    };
                    if self.verbose || field != "cwd" || text != "." {
                        context.push(format!("{field}={text}"));
                    }
                }
            }
            if self.verbose
                && let Some(ms) = params
                    .get("timeoutMs")
                    .or_else(|| params.get("timeout_ms"))
                    .and_then(Value::as_u64)
            {
                context.push(format!("timeout={:.3}s", ms as f64 / 1000.0));
            }
            if !context.is_empty() {
                self.text(out, label, &format!("{}\n", context.join("  ·  ")))?;
            }
            if let Some(command) = command
                && (self.verbose || command_description.is_none())
            {
                self.command_details(out, label, command)?;
            }
            self.finish(out, label)?;
            return Ok(true);
        }
        if v.get("result").is_some() || v.get("error").is_some() {
            let request = self.requests.remove(&key);
            let method = request.as_ref().map(|r| r.method.as_str()).unwrap_or("");
            let command = request.as_ref().and_then(|r| r.command.as_deref());
            let elapsed = request
                .as_ref()
                .map(|r| format!("  {:.2}s", r.started.elapsed().as_secs_f64()))
                .unwrap_or_default();
            if command.is_some() {
                let process = request
                    .as_ref()
                    .and_then(|r| r.process_id.as_deref())
                    .unwrap_or(&id);
                self.finish_process(out, label, process)?;
                self.activities.finish(&activity_key("rpc", label, &id));
            }
            let result = v["result"].get("structuredContent").unwrap_or(&v["result"]);
            let exit = result
                .get("exitCode")
                .or_else(|| result.get("exit_code"))
                .and_then(Value::as_i64);
            let timed_out =
                exit == Some(124) || result["timedOut"] == true || result["timed_out"] == true;
            let failed = timed_out
                || v.get("error").is_some_and(|e| !e.is_null())
                || v["result"]["isError"] == true
                || result["isError"] == true
                || result["success"] == false
                || exit.is_some_and(|c| c != 0);
            if value["internal_discovery"] == true && !self.verbose && !method.is_empty() {
                let key = format!("{label}\0{method}");
                if failed {
                    // Keep failures visible, including repeated native errors.
                    // Clearing success makes the eventual recovery visible too.
                    self.discovery_success.remove(&key);
                } else {
                    let signature = json!({
                        "status":result["status"],"exit":exit,"success":result["success"],
                        "in_flight":result["in_flight"],"timed_out":timed_out
                    });
                    if self.discovery_success.get(&key) == Some(&signature) {
                        return Ok(true);
                    }
                    if self.discovery_success.len() >= 256 {
                        self.discovery_success.clear();
                    }
                    self.discovery_success.insert(key, signature);
                }
            }
            let title = if timed_out {
                "Timeout"
            } else if failed {
                "Error"
            } else if method == "command/exec" && exit.is_none() {
                "Warning"
            } else {
                "Success"
            };
            let mut meta = String::new();
            for field in [
                "exitCode",
                "exit_code",
                "timeout",
                "timedOut",
                "in_flight",
                "tool",
                "toolName",
                "status",
            ] {
                if let Some(v) = result.get(field).filter(|v| !v.is_null()) {
                    meta += &format!("  {field}={}", compact(v, 0));
                }
            }
            if self.verbose
                && let Some(pid) = result.pointer("/process/id").or_else(|| result.get("pid"))
            {
                meta += &format!("  process.id={}", compact(pid, 0));
            }
            if self.verbose
                && let Some(rss) = result
                    .pointer("/process/rssBytes")
                    .or_else(|| result.pointer("/memory/rss"))
                    .or_else(|| result.get("rssBytes"))
                    .and_then(Value::as_u64)
            {
                meta += &format!("  rss={:.1}MB", rss as f64 / 1_000_000.0);
            }
            if timed_out {
                meta += "  command timeout";
            }
            if method == "command/exec" && exit.is_none() && !failed {
                meta += "  exit unknown";
            }
            self.summary(
                out,
                label,
                title,
                &if let Some(command) = command {
                    format!("{}{meta}{elapsed} · {command}", id_text.trim())
                } else {
                    format!("{method}{id_text}{elapsed}{meta}")
                },
            )?;
            if let Some(code) = v.pointer("/error/code") {
                self.summary(
                    out,
                    label,
                    "Error",
                    &format!("native error code={}", compact(code, 0)),
                )?;
            }
            // Response payloads are available to the caller. The audit copy
            // contains metadata only and must not replay wait/read output.
            return Ok(true);
        }
        Ok(false)
    }

    fn finish_group(&mut self, out: &mut impl Write, label: &str) -> io::Result<()> {
        self.finish_diagnostic(out, label)?;
        let prefix = format!("{label}\0");
        let keys: Vec<_> = self
            .streams
            .keys()
            .filter(|k| k.as_str() == label || k.starts_with(&prefix))
            .cloned()
            .collect();
        for key in keys {
            self.finish(out, &key)?;
        }
        Ok(())
    }

    fn prefix(&self, out: &mut impl Write, title: &str) -> io::Result<()> {
        if self.ui.timestamps {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let sec = now.as_secs() % 86400;
            painted(
                out,
                self.color,
                &self.ui.color("gray"),
                &format!(
                    "{:02}:{:02}:{:02}.{:03}Z ",
                    sec / 3600,
                    (sec / 60) % 60,
                    sec % 60,
                    now.subsec_millis()
                ),
            )?;
        }
        let (tag, color) = match title {
            "Request" | "User" => ("REQ ", "blue"),
            "Running" => ("RUN ", "blue"),
            "Read" => ("READ", "cyan"),
            "Image" => ("IMG ", "cyan"),
            "Edit" => ("EDIT", "purple"),
            "Diff" => ("DIFF", "purple"),
            "Skill" => ("SKIL", "purple"),
            "Output" => ("SAVE", "gray"),
            "Success" | "Connected" => ("OK  ", "green"),
            "Error" | "Timeout" => ("ERR ", "red"),
            "Warning" => ("WARN", "yellow"),
            "Progress" => ("PROG", "yellow"),
            "stdout" => ("STDOUT", "gray"),
            "stderr" => ("STDERR", "yellow"),
            "stderr-error" => ("STDERR", "red"),
            "Tool" | "Command" | "Files" => ("OUT ", "purple"),
            "Usage" => ("META", "gray"),
            _ => ("RES ", "cyan"),
        };
        painted(
            out,
            self.color,
            &format!("1;{}", self.ui.color(color)),
            &format!("[{tag}] "),
        )
    }

    fn summary(
        &mut self,
        out: &mut impl Write,
        label: &str,
        title: &str,
        text: &str,
    ) -> io::Result<()> {
        self.finish_visible(out)?;
        self.prefix(out, title)?;
        // Summaries are one line. Full identifiers, paths and command arguments
        // remain in the event/output record; the interactive transcript is bounded.
        let available = self
            .width
            .saturating_sub(if self.ui.timestamps { 24 } else { 7 })
            .max(8);
        let label = activity::single_line(&clean(label), (available / 3).clamp(4, 24));
        let label_width = unicode_width::UnicodeWidthStr::width(label.as_str());
        let separator = usize::from(!label.is_empty());
        let text = activity::single_line(
            &clean(text),
            available.saturating_sub(label_width + separator),
        );
        if !label.is_empty() {
            painted(out, self.color, &self.ui.color("muted"), &label)?;
            if !text.is_empty() {
                write!(out, " ")?;
            }
        }
        let tone = match title {
            "Error" | "Timeout" => "error",
            "Warning" => "warning",
            _ => "foreground",
        };
        painted(out, self.color, &self.ui.color(tone), &text)?;
        writeln!(out)
    }

    fn heading(&mut self, out: &mut impl Write, label: &str, title: &str) -> io::Result<()> {
        self.finish_visible(out)?;
        self.prefix(out, title)?;
        painted(
            out,
            self.color,
            &self.ui.color("muted"),
            &format!("{}\n", clean(label)),
        )
    }

    fn block(
        &mut self,
        out: &mut impl Write,
        label: &str,
        title: &str,
        text: &str,
    ) -> io::Result<()> {
        self.heading(out, label, title)?;
        // Human conversation remains complete. Tool data, file reads and
        // generic result values all share the same bounded default preview.
        let conversation = matches!(title, "Assistant" | "User" | "Answer");
        let limit = if self.verbose || conversation {
            usize::MAX
        } else {
            self.ui.max_inline_output.max(1)
        };
        let cleaned = clean(text);
        let lines: Vec<_> = cleaned.lines().collect();
        let shown = lines.len().min(limit);
        // Keep the tail (usually the actual failure) when folding a buffered stream.
        let tail = if lines.len() > limit {
            limit.min(3) / 2 + usize::from(limit >= 3)
        } else {
            0
        };
        let head = shown - tail;
        let tone = if title == "stderr-error" {
            self.ui.color("red")
        } else if title == "stderr" {
            self.ui.color("yellow")
        } else {
            self.ui.color("foreground")
        };
        self.streams.entry(label.into()).or_default().tone = tone;
        let mut shortened = false;
        for (i, line) in lines.iter().enumerate() {
            if i >= head && i < lines.len().saturating_sub(tail) && lines.len() > limit {
                if i == head {
                    self.text(
                        out,
                        label,
                        &format!(
                            "... {} lines folded; original output: archive or --verbose\n",
                            lines.len() - shown
                        ),
                    )?;
                }
                continue;
            }
            let visible = if self.verbose || conversation {
                (*line).to_owned()
            } else {
                super::panel::fit(line, self.width.saturating_sub(2).max(12) * 2)
            };
            shortened |= visible != *line;
            self.text(out, label, &format!("{visible}\n"))?;
        }
        if shortened {
            self.text(
                out,
                label,
                "... long lines shortened; original output: archive or --verbose\n",
            )?;
        }
        self.finish(out, label)
    }

    fn command_details(
        &mut self,
        out: &mut impl Write,
        label: &str,
        value: &Value,
    ) -> io::Result<()> {
        let command = command::Command::new(value);
        // Normal logs show the command lifecycle and its actual output. Source
        // browsing belongs to the audit pane; verbose output keeps the script.
        if command.script.is_some() && !self.verbose {
            return Ok(());
        }
        self.text(
            out,
            label,
            &format!("cmd={}\n", self.source_excerpt(&command.invocation)),
        )?;
        let Some(script) = command.script else {
            return self.finish(out, label);
        };
        let total = script.lines().count();
        self.text(out, label, &format!("script · {total} lines\n"))?;
        self.finish(out, label)?;
        let limit = if self.verbose {
            usize::MAX
        } else {
            self.ui.max_inline_output
        };
        let shown = total.min(limit);
        let tail = if total > limit { limit / 3 } else { 0 };
        let head = shown - tail;
        let digits = total.max(1).to_string().len();
        for (index, line) in script.lines().enumerate() {
            if index >= head && index < total - tail && total > limit {
                if index == head {
                    self.text(out, label, &format!(
                        "... lines {}–{} folded ({} lines); --verbose or --max-inline-output {}\n",
                        head + 1, total - tail, total - shown, total,
                    ))?;
                    self.finish(out, label)?;
                }
                continue;
            }
            self.source_line(out, index + 1, digits, &self.source_excerpt(line))?;
        }
        Ok(())
    }

    fn source_excerpt(&self, text: &str) -> String {
        if self.verbose || text.chars().count() <= 512 {
            text.into()
        } else {
            format!(
                "{} … ({} chars folded; --verbose)",
                text.chars().take(512).collect::<String>(),
                text.chars().count() - 512
            )
        }
    }

    fn source_line(
        &self,
        out: &mut impl Write,
        number: usize,
        digits: usize,
        text: &str,
    ) -> io::Result<()> {
        let prefix = format!("  {number:>digits$} │ ");
        let continuation = format!("  {:>digits$} │ ", "");
        let available = self
            .width
            .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()))
            .max(2);
        painted(out, self.color, &self.ui.color("gray"), &prefix)?;
        let mut column = 0;
        let mut fragment = String::new();
        for c in clean(text).chars() {
            // Tabs in source are expanded at tab stops so both the terminal and
            // plain transcript agree on the continuation column.
            let expanded = if c == '\t' {
                " ".repeat(4 - column % 4)
            } else {
                c.to_string()
            };
            for c in expanded.chars() {
                let cells = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if c == '\n' || column + cells > available {
                    painted(out, self.color, "0", &fragment)?;
                    fragment.clear();
                    writeln!(out)?;
                    painted(out, self.color, &self.ui.color("gray"), &continuation)?;
                    column = 0;
                    if c == '\n' {
                        continue;
                    }
                }
                fragment.push(c);
                column += cells;
            }
        }
        painted(out, self.color, "0", &fragment)?;
        writeln!(out)
    }

    fn value(
        &mut self,
        out: &mut impl Write,
        label: &str,
        v: &Value,
        title: &str,
        depth: usize,
    ) -> io::Result<()> {
        if depth > 24 {
            return self.block(out, label, title, &compact(v, 0));
        }
        if let Some(text) = v.as_str() {
            // Decode JSON envelopes once per layer, never replace literal source-code \\n.
            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                return self.value(out, label, &inner, title, depth + 1);
            }
            if title == "Event" {
                // Unwrapped text events are transcript bodies. Retain every
                // character for EventView to fold/open; only buffered tool
                // results use the bounded block preview below.
                self.heading(out, label, title)?;
                self.streams.entry(label.into()).or_default().tone = self.ui.color("foreground");
                self.text(out, label, text)?;
                return self.finish(out, label);
            }
            return self.block(out, label, title, text);
        }
        if let Some(array) = v.as_array() {
            let limit = if self.verbose {
                usize::MAX
            } else {
                self.ui.max_inline_output.max(1)
            };
            for item in array.iter().take(limit) {
                self.value(out, label, item, title, depth + 1)?;
            }
            if array.len() > limit {
                self.summary(
                    out,
                    label,
                    "Usage",
                    &format!(
                        "{} additional items folded; original output: archive or --verbose",
                        array.len() - limit
                    ),
                )?;
            }
            return Ok(());
        }
        if v["type"] == "image" {
            let mime = v["mimeType"]
                .as_str()
                .or_else(|| v["mime_type"].as_str())
                .unwrap_or("unknown image type");
            let dimensions = v["width"]
                .as_u64()
                .zip(v["height"].as_u64())
                .map(|(width, height)| format!(" · {width} × {height}"))
                .unwrap_or_default();
            return self.summary(
                out,
                label,
                "Image",
                &format!("{mime}{dimensions} · image available in the tool result"),
            );
        }
        if v["method"] == "notifications/progress" {
            self.activity(out, label, v)?;
            return Ok(());
        }
        if v["type"] == "ssmcp.status" {
            return self.summary(
                out,
                label,
                v["level"].as_str().unwrap_or("Result"),
                v["message"].as_str().unwrap_or(""),
            );
        }
        let kind = v["method"]
            .as_str()
            .or_else(|| v["type"].as_str())
            .unwrap_or("");
        let lower = kind.to_ascii_lowercase();
        let title = if v["isError"] == true
            || v["success"] == false
            || v.get("error").is_some_and(|e| !e.is_null())
            || lower.contains("error")
            || lower.contains("failed")
        {
            "Error"
        } else if lower == "stderr" || lower == "ssmcp.stderr" {
            "stderr"
        } else if lower.contains("token") || lower.contains("usage") {
            "Usage"
        } else if lower.contains("diff")
            || lower.contains("filechange")
            || lower.contains("patch")
            || lower.contains("file")
            || lower.contains("directory")
        {
            "Files"
        } else if lower.contains("command") || lower.contains("bash") || lower.contains("output") {
            "Command"
        } else if lower.contains("tool") || lower.contains("request") {
            "Tool"
        } else if v["role"] == "user" {
            "User"
        } else if lower.contains("text") || lower.contains("message") || v["role"] == "assistant" {
            "Assistant"
        } else if lower.contains("completed") || lower.contains("end") || v["success"] == true {
            "Success"
        } else {
            title
        };
        if let Some(encoded) = v.pointer("/params/deltaBase64").and_then(Value::as_str)
            && let Ok(bytes) = STANDARD.decode(encoded)
        {
            let stream = v
                .pointer("/params/stream")
                .and_then(Value::as_str)
                .unwrap_or("stdout");
            let process = v
                .pointer("/params/processId")
                .and_then(Value::as_str)
                .unwrap_or("command");
            let key = format!("{label}\0{process}\0{stream}");
            let mut bytes = bytes;
            if let Some((offset, end)) = v
                .pointer("/params/offset")
                .and_then(Value::as_u64)
                .zip(v.pointer("/params/next_offset").and_then(Value::as_u64))
                .filter(|(offset, end)| end >= offset)
            {
                if let Some(previous) = self.stream_offsets.get(&key).copied() {
                    if end <= previous {
                        return Ok(());
                    }
                    // Audit redaction can change byte length. Only slice an
                    // overlapping chunk when its native offsets still match.
                    if offset < previous && end - offset == bytes.len() as u64 {
                        bytes.drain(..(previous - offset) as usize);
                    }
                }
                if self.stream_offsets.len() >= 4096 {
                    self.stream_offsets.clear();
                }
                self.stream_offsets.insert(key.clone(), end);
            }
            // Fresh output already tells the user that work is advancing.
            self.last_activity_summary = Instant::now();
            if self.current_stream.as_deref() != Some(&key) {
                let prefix = format!("{label}\0");
                let request_id = self.requests.iter().find_map(|(key, request)| {
                    (request.process_id.as_deref() == Some(process))
                        .then(|| key.strip_prefix(&prefix))
                        .flatten()
                });
                let process_label = request_id
                    .filter(|id| *id != process)
                    .map(|id| format!("{} · {}", short_id(id), short_id(process)))
                    .unwrap_or_else(|| short_id(process));
                self.heading(
                    out,
                    &format!("{label} · {process_label}"),
                    if stream == "stderr" {
                        "stderr"
                    } else {
                        "stdout"
                    },
                )?;
                self.current_stream = Some(key.clone());
            }
            let lines = self.streams.entry(key.clone()).or_default();
            lines.tone = if stream == "stderr" {
                self.ui.color("yellow")
            } else {
                "0".into()
            };
            // The interactive EventView owns folding and needs the full stream
            // body to reopen it. Plain terminal logs retain their compact outline.
            lines.summarize_json = !self.verbose && !self.dashboard_active;
            return self.command_text(out, &key, &String::from_utf8_lossy(&bytes));
        }
        // Native transports wrap useful payloads in these fields.
        for field in [
            "event",
            "request",
            "response",
            "params",
            "assistantMessageEvent",
            "item",
            "part",
            "message",
            "result",
            "data",
            "properties",
        ] {
            if let Some(inner) = v.get(field).filter(|v| !v.is_null()) {
                if title == "Error" {
                    self.heading(out, label, title)?;
                }
                self.value(out, label, inner, title, depth + 1)?;
                // Error/status/usage can sit alongside the wrapped payload.
                self.details(out, label, v, title != "Result")?;
                return Ok(());
            }
        }
        let mut shown = false;
        for field in [
            "delta",
            "text",
            "output",
            "aggregatedOutput",
            "diff",
            "patch",
            "content",
        ] {
            if let Some(inner) = v.get(field).filter(|v| !v.is_null()) {
                if let Some(text) = inner.as_str() {
                    if field == "delta"
                        || lower.contains("delta")
                        || lower.ends_with("update")
                        || lower == "background_output"
                    {
                        if !self.streams.contains_key(label) {
                            self.heading(out, label, title)?;
                        }
                        self.text(out, label, text)?;
                    } else {
                        self.value(out, label, inner, title, depth + 1)?;
                    }
                } else {
                    self.value(out, label, inner, title, depth + 1)?;
                }
                shown = true;
            }
        }
        for field in [
            "command",
            "tool",
            "toolName",
            "name",
            "path",
            "file_path",
            "arguments",
            "input",
            "changes",
        ] {
            if let Some(inner) = v.get(field) {
                self.value(out, label, inner, title, depth + 1)?;
                shown = true;
            }
        }
        shown |= self.details(out, label, v, title != "Result")?;
        if !shown && title == "Error" {
            self.block(out, label, title, &compact(v, 0))?;
        } else if !shown && kind.is_empty() {
            if let Some(object) = v.as_object() {
                let mut fields = Vec::new();
                for (key, value) in object.iter() {
                    if title == "Result"
                        && matches!(
                            key.as_str(),
                            "process" | "in_flight" | "pid" | "rssBytes" | "timeout" | "timedOut"
                        )
                    {
                        continue;
                    }

                    if !matches!(
                        key.as_str(),
                        "id" | "jsonrpc"
                            | "threadId"
                            | "turnId"
                            | "itemId"
                            | "type"
                            | "role"
                            | "exitCode"
                            | "exit_code"
                            | "stdout"
                            | "stderr"
                            | "status"
                    ) {
                        fields.push(format!("{key}: {}", compact(value, 0)));
                    }
                }
                if !fields.is_empty() {
                    self.block(out, label, title, &fields.join("\n"))?;
                }
            } else if !v.is_null() {
                self.block(out, label, title, &compact(v, 0))?;
            }
        } else if !shown
            && !kind.is_empty()
            && !["heartbeat", "ping", "keepalive"]
                .iter()
                .any(|s| lower.contains(s))
            && !lower.starts_with("ssmcp.")
        {
            self.block(out, label, title, kind)?;
        }
        Ok(())
    }

    fn details(
        &mut self,
        out: &mut impl Write,
        label: &str,
        v: &Value,
        include_status: bool,
    ) -> io::Result<bool> {
        let mut shown = false;
        // Buffered command/exec replies carry output beside exitCode. Render it
        // explicitly; a displayed status must never suppress either stream.
        for field in ["stdout", "stderr"] {
            if let Some(text) = v
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                let failed = v
                    .get("exitCode")
                    .or_else(|| v.get("exit_code"))
                    .and_then(Value::as_i64)
                    .is_some_and(|v| v != 0)
                    || v["isError"] == true;
                let title = if field == "stderr" && failed {
                    "stderr-error"
                } else {
                    field
                };
                self.block(out, label, title, text)?;
                shown = true;
            }
        }
        for (field, title) in [
            ("error", "Error"),
            ("usage", "Usage"),
            ("tokenUsage", "Usage"),
            ("status", "Status"),
            ("exitCode", "Status"),
            ("exit_code", "Status"),
            ("duration_ms", "Status"),
            ("durationMs", "Status"),
        ] {
            if !include_status && !matches!(field, "error" | "usage" | "tokenUsage") {
                continue;
            }
            if let Some(inner) = v.get(field).filter(|v| !v.is_null()) {
                let title = if field.starts_with("exit") && inner.as_i64().is_some_and(|n| n != 0) {
                    "Error"
                } else if field.starts_with("exit") && inner.as_i64() == Some(0) {
                    "Success"
                } else {
                    title
                };
                self.block(
                    out,
                    label,
                    title,
                    &format!(
                        "{field}: {}",
                        if let Some(s) = inner.as_str() {
                            s.to_owned()
                        } else {
                            compact(inner, 0)
                        }
                    ),
                )?;
                shown = true;
            }
        }
        if include_status && v["success"] == true {
            self.block(out, label, "Success", "Completed")?;
            shown = true;
        }
        Ok(shown)
    }

    fn command_text(&mut self, out: &mut impl Write, label: &str, text: &str) -> io::Result<()> {
        self.text(out, label, text)?;
        let lines = self.streams.get_mut(label).unwrap();
        // Command progress is useful before a newline or final RPC reply arrives.
        // Keep possible diff headers and hunk lines intact for their parser.
        let diff_prefix = ["diff --git", "--- ", "+++ ", "index ", "@@ "]
            .iter()
            .any(|prefix| prefix.starts_with(&lines.pending) || lines.pending.starts_with(prefix));
        if !lines.pending.is_empty()
            && !lines.hunk
            && !diff_prefix
            && !(lines.summarize_json && (lines.folded_bytes > 0 || json_record(&lines.pending)))
        {
            let pending = std::mem::take(&mut lines.pending);
            render_live(out, self.color, self.width, lines, &pending, false)?;
            lines.pending = pending;
        }
        Ok(())
    }

    fn text(&mut self, out: &mut impl Write, label: &str, text: &str) -> io::Result<()> {
        let lines = self.streams.entry(label.into()).or_default();
        // Strip terminal control sequences supplied by tools, including in redirected logs.
        lines.pending.push_str(&clean(text));
        while let Some(end) = lines.pending.find('\n') {
            let line: String = lines.pending.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if lines.folded_bytes > 0 {
                lines.folded_bytes = 0;
                render_line(
                    out,
                    self.color,
                    self.width,
                    lines,
                    "JSON output · large record folded; inspect original output",
                    &self.ui,
                )?;
            } else if lines.emitted > 0 {
                render_live(out, self.color, self.width, lines, line, true)?;
            } else {
                render_line(out, self.color, self.width, lines, line, &self.ui)?;
            }
        }
        // JSON can span many chunks. Keep the display buffer bounded without
        // exposing partial JSON or discarding bytes from the execution capture.
        if lines.summarize_json
            && (lines.folded_bytes > 0
                || (lines.pending.len() > 16 * 1024 && json_record(&lines.pending)))
        {
            lines.folded_bytes = lines.folded_bytes.saturating_add(lines.pending.len());
            lines.pending.clear();
        } else if lines.pending.len() > 64 * 1024 {
            let line = std::mem::take(&mut lines.pending);
            if lines.emitted > 0 {
                render_live(out, self.color, self.width, lines, &line, true)?;
            } else {
                render_line(out, self.color, self.width, lines, &line, &self.ui)?;
            }
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl Write, label: &str) -> io::Result<()> {
        if let Some(mut lines) = self.streams.remove(label) {
            if lines.folded_bytes > 0 {
                render_line(
                    out,
                    self.color,
                    self.width,
                    &mut lines,
                    "JSON output · large record folded; inspect original output",
                    &self.ui,
                )?;
            } else if !lines.pending.is_empty() {
                let line = std::mem::take(&mut lines.pending);
                if lines.emitted > 0 {
                    render_live(out, self.color, self.width, &mut lines, &line, true)?;
                } else {
                    render_line(out, self.color, self.width, &mut lines, &line, &self.ui)?;
                }
            }
        }
        out.flush()
    }
}

/// Most backend loggers prefix ISO-8601 timestamps. Ignore only that prefix;
/// changing server names, error details, and all other text remain distinct.
fn diagnostic_key(text: &str) -> String {
    let text = text.trim();
    let bytes = text.as_bytes();
    let timestamp = bytes.len() >= 20
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':';
    if timestamp && let Some((_, rest)) = text.split_once(char::is_whitespace) {
        rest.trim_start().into()
    } else {
        text.into()
    }
}

fn diagnostic_summary(text: &str) -> String {
    if text.contains("AuthRequired") || text.contains("auth_required") {
        let origin = text
            .find("https://")
            .or_else(|| text.find("http://"))
            .and_then(|start| {
                let candidate = text[start..].split(['"', '\\', ' ', ')', '}']).next()?;
                let url = reqwest::Url::parse(candidate).ok()?;
                Some(url.origin().ascii_serialization())
            });
        let server = origin
            .map(|origin| format!(" ({origin})"))
            .unwrap_or_default();
        format!(
            "Backend MCP authentication required{server}.\nSign in to that MCP server in this backend, or disable its connector.\nOriginal diagnostics: --verbose or --raw-events."
        )
    } else {
        activity::single_line(text, 240)
    }
}

// Images remain unchanged in native MCP responses. Their local transcript copy
// contains metadata only, including in verbose/JSONL logs.
fn omit_image_bytes(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if fields.get("type").and_then(Value::as_str) == Some("image") {
                let removed = fields.remove("data").is_some();
                let removed = fields.remove("data_base64").is_some() || removed;
                if removed {
                    fields.insert("data_omitted".into(), json!(true));
                }
            }
            for inner in fields.values_mut() {
                omit_image_bytes(inner);
            }
        }
        Value::Array(items) => {
            for inner in items {
                omit_image_bytes(inner);
            }
        }
        _ => {}
    }
}

// Verbose mode adds routing metadata, not a second copy of already-rendered
// output/diffs. Raw mode still emits one sanitized event per input event.
fn debug_metadata(value: &Value) -> Value {
    let kind = value["type"].as_str().unwrap_or("");
    let stream = value.pointer("/params/deltaBase64").is_some()
        || matches!(
            kind,
            "exec.stdout"
                | "exec.stderr"
                | "ssmcp.exec.stdout"
                | "ssmcp.exec.stderr"
                | "process_output"
                | "ssmcp.terminal_output"
        );
    let diff = matches!(kind, "file.diff" | "ssmcp.file.diff");
    let mut copy = value.clone();
    if let Some(object) = copy.as_object_mut() {
        if stream {
            for key in [
                "text",
                "data_base64",
                "ssmcp_output_text",
                "delta",
                "stdout",
                "stderr",
            ] {
                object.remove(key);
            }
            if let Some(params) = object.get_mut("params").and_then(Value::as_object_mut) {
                params.remove("deltaBase64");
                params.remove("text");
            }
            object.insert("output_already_rendered".into(), json!(true));
        }
        if diff {
            object.remove("diff");
            object.remove("preview");
            object.insert("diff_already_rendered".into(), json!(true));
        }
    }
    copy
}

/// A bounded description, never a giant JSON dump in the default transcript.
fn command_label(command: &Value) -> String {
    command::Command::new(command).summary()
}

fn json_record(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with('{')
        || text.strip_prefix('[').is_some_and(|rest| {
            rest.trim_start()
                .chars()
                .next()
                .is_none_or(|c| matches!(c, '{' | '[' | '"' | ']'))
        })
}

fn json_outline(value: &Value, limit: usize) -> String {
    fn item(value: &Value) -> String {
        match value {
            Value::String(text) => clean(text),
            _ => serde_json::to_string_pretty(value).unwrap_or_default(),
        }
    }
    let body = match value {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| format!("{}: {}", clean(key), item(value)))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => item(value),
    };
    let rows = body.lines().collect::<Vec<_>>();
    let limit = limit.max(1);
    let tail = if rows.len() > limit { limit / 3 } else { 0 };
    let head = rows.len().min(limit) - tail;
    let mut shown = vec!["JSON output".to_owned()];
    for (index, row) in rows.iter().enumerate() {
        if index >= head && index < rows.len() - tail {
            if index == head {
                shown.push(format!(
                    "… {} lines folded; --verbose shows the original JSON",
                    rows.len() - limit
                ));
            }
            continue;
        }
        if row.chars().count() > 512 {
            shown.push(format!(
                "{} … ({} chars folded; --verbose)",
                row.chars().take(512).collect::<String>(),
                row.chars().count() - 512
            ));
        } else {
            shown.push((*row).to_owned());
        }
    }
    shown.join("\n")
}

fn compact(value: &Value, depth: usize) -> String {
    if depth > 2 {
        return "… (--verbose for JSON)".into();
    }
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Array(v) => format!("[{} items; --verbose for JSON]", v.len()),
        Value::Object(o) => {
            let mut fields = o
                .iter()
                .take(6)
                .map(|(k, v)| format!("{k}={}", compact(v, depth + 1)))
                .collect::<Vec<_>>();
            if o.len() > 6 {
                fields.push(format!("… {} more fields", o.len() - 6));
            }
            fields.join("  ")
        }
        _ => value.to_string(),
    };
    if text.chars().count() > 256 {
        format!(
            "{}… (--verbose for JSON)",
            text.chars().take(256).collect::<String>()
        )
    } else {
        text
    }
}

pub(crate) fn status(label: &str, level: &str, message: &str) {
    event(
        label,
        &json!({"type":"ssmcp.status","level":level,"message":message}),
    );
}

pub(crate) fn connection(output: &Value) -> io::Result<()> {
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    let tty = io::stdout().is_terminal();
    let mut bytes = Vec::new();
    let raw;
    {
        let renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        raw = renderer.raw;
        let out = &mut bytes;
        if !tty || raw {
            // This explicitly requested connection result retains the human login
            // code. It is not an audit copy and is never published to native peers.
            writeln!(out, "{output}")?;
        } else {
            writeln!(out)?;
            painted(
                out,
                renderer.color,
                &renderer.ui.color("blue"),
                concat!("  ssmcp  ", env!("CARGO_PKG_VERSION"), "\n"),
            )?;
            if let Some(title) = output["title"].as_str() {
                writeln!(out, "  {}", clean(title))?;
            }
            writeln!(out)?;
            writeln!(
                out,
                "  MCP    {}",
                clean(output["url"].as_str().unwrap_or(""))
            )?;
            if let Some(cwd) = output["cwd"].as_str() {
                writeln!(out, "  Workspace  {}", clean(cwd))?;
            }
            if let Some(code) = output["login_code"].as_str() {
                writeln!(
                    out,
                    "  Login  {}  ·  expires in {}s",
                    clean(code),
                    output["login_expires_in"]
                )?;
            } else {
                writeln!(out, "  Login  GitHub account")?;
            }
            painted(
                out,
                renderer.color,
                &renderer.ui.color("gray"),
                "\n  Commands run here. Parallel jobs keep separate output.\n  Ctrl-C disconnects and stops owned processes.\n\n",
            )?;
        }
    }
    if tty && !raw && super::terminal::log(&bytes) {
        return Ok(());
    }
    let mut out = io::stdout().lock();
    out.write_all(&bytes)?;
    out.flush()
}

pub(crate) fn terminal_output_for(terminal_id: &str, bytes: &[u8]) {
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
    crate::session::attach::record_terminal(terminal_id, bytes);
    let tty = io::stderr().is_terminal();
    let mut rendered = Vec::new();
    let raw;
    {
        let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
        renderer.last_pty_output = Some(Instant::now());
        renderer.current_stream = None;
        raw = renderer.raw;
        if raw {
            let _ = renderer.event(
                &mut rendered,
                "shell",
                &json!({
                    "type":"ssmcp.terminal_output","terminal_id":terminal_id,
                    "data_base64":STANDARD.encode(bytes),"text":String::from_utf8_lossy(bytes)
                }),
            );
        }
    }
    if raw {
        publish(&rendered, None);
    } else if tty {
        // Preserve the user's original PTY bytes; only the compositor interprets
        // cursor control when panes are active. NO_COLOR does not change transport.
        if !super::terminal::pty_session(terminal_id, bytes) {
            let mut out = io::stderr().lock();
            let _ = out.write_all(bytes);
            let _ = out.flush();
        }
    } else {
        let safe = crate::audit::event(json!({
            "type":"ssmcp.terminal_output","terminal_id":terminal_id,"text":String::from_utf8_lossy(bytes)
        }));
        let text = safe["text"].as_str().unwrap_or("");
        let mut out = io::stderr().lock();
        let _ = out.write_all(clean(text).as_bytes());
        let _ = out.flush();
    }
}

fn render_live(
    out: &mut impl Write,
    color: bool,
    width: usize,
    lines: &mut Lines,
    text: &str,
    complete: bool,
) -> io::Result<()> {
    let mut fragment = String::new();
    if lines.emitted == 0 {
        fragment.push_str("  ");
    }
    for c in text[lines.emitted..].chars() {
        let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
        if lines.column + cells > width.saturating_sub(2) {
            fragment.push_str("\n  ");
            lines.column = 0;
        }
        fragment.push(c);
        lines.column += cells;
    }
    lines.emitted = text.len();
    if complete {
        fragment.push('\n');
        lines.emitted = 0;
        lines.column = 0;
    }
    painted(
        out,
        color,
        if lines.tone.is_empty() {
            "0"
        } else {
            &lines.tone
        },
        &fragment,
    )
}

fn painted(out: &mut impl Write, color: bool, code: &str, text: &str) -> io::Result<()> {
    if color {
        write!(out, "\x1b[{code}m{text}\x1b[0m")
    } else {
        write!(out, "{text}")
    }
}

fn render_line(
    out: &mut impl Write,
    color: bool,
    width: usize,
    lines: &mut Lines,
    line: &str,
    ui: &Ui,
) -> io::Result<()> {
    let outline = if lines.summarize_json && json_record(line) && line.len() > 16 * 1024 {
        Some("JSON output · large record folded; inspect original output".into())
    } else if lines.summarize_json && json_record(line) {
        serde_json::from_str::<Value>(line)
            .ok()
            .filter(|value| value.is_object() || value.is_array())
            .map(|value| json_outline(&value, ui.max_inline_output))
    } else {
        None
    };
    let line = outline.as_deref().unwrap_or(line);
    let mut prefix = String::new();
    let lower = line.to_ascii_lowercase();
    let progress = outline.is_none()
        && (lower.contains("downloading")
            || lower.contains("download ")
            || lower.starts_with("mise ") && lower.contains("installing"));
    let failure = outline.is_none()
        && (lower.starts_with("error")
            || lower.starts_with("fatal")
            || lower.contains(" failed")
            || lower.contains("timed out"));
    let code = if line.starts_with("diff --git")
        || line.starts_with("--- ")
        || line.starts_with("+++ ")
        || line.starts_with("index ")
    {
        lines.hunk = false;
        "1".to_owned()
    } else if line.starts_with("@@ ") {
        let mut fields = line.split_whitespace().skip(1);
        lines.old = fields
            .next()
            .and_then(|s| s.trim_start_matches('-').split(',').next()?.parse().ok())
            .unwrap_or(0);
        lines.new = fields
            .next()
            .and_then(|s| s.trim_start_matches('+').split(',').next()?.parse().ok())
            .unwrap_or(0);
        lines.hunk = true;
        ui.color("cyan")
    } else if lines.hunk && line.starts_with('+') {
        prefix = format!("      {:>5} │ ", lines.new);
        lines.new += 1;
        ui.color("green")
    } else if lines.hunk && line.starts_with('-') {
        prefix = format!("{:>5}       │ ", lines.old);
        lines.old += 1;
        ui.color("red")
    } else if lines.hunk && line.starts_with(' ') {
        prefix = format!("{:>5} {:>5} │ ", lines.old, lines.new);
        lines.old += 1;
        lines.new += 1;
        ui.color("gray")
    } else if failure {
        prefix = "[ERR ] ".into();
        ui.color("red")
    } else if progress {
        prefix = "[PROG] ".into();
        ui.color("yellow")
    } else if outline.is_none() && (lower.starts_with("warning") || lower.contains("retrying")) {
        prefix = "[WARN] ".into();
        ui.color("yellow")
    } else {
        if lines.tone.is_empty() {
            ui.color("foreground")
        } else {
            lines.tone.clone()
        }
    };
    // Keep diff/source columns intact. Prose wraps without dropping characters.
    let text = format!("{prefix}{line}");
    if lines.hunk || code == "1" {
        return painted(out, color, &code, &format!("{text}\n"));
    }
    let mut column = 0;
    let mut wrapped = String::new();
    for c in text.chars() {
        if c == '\n' {
            wrapped.push_str("\n  ");
            column = 0;
            continue;
        }
        let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
        if column + cells > width.saturating_sub(2) {
            wrapped.push_str("\n  ");
            column = 0;
        }
        wrapped.push(c);
        column += cells;
    }
    painted(out, color, &code, &format!("  {wrapped}\n"))
}

fn clean(text: &str) -> String {
    let mut chars = text.chars().peekable();
    let mut output = String::new();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\r' {
            if chars.peek() != Some(&'\n') {
                output.push('\n');
            }
        } else if !c.is_control() || matches!(c, '\n' | '\t') {
            output.push(c);
        }
    }
    output
}

#[cfg(test)]
mod tests {

    #[test]
    fn images_show_metadata_without_base64_in_any_local_log_mode() {
        let data = STANDARD.encode([19u8; 4096]);
        let image =
            json!({"type":"image","mimeType":"image/png","width":20,"height":30,"data":data});
        for (raw, verbose) in [(false, false), (false, true), (true, false)] {
            let mut renderer = Renderer::new(raw, false, 120);
            renderer.verbose = verbose;
            let mut output = Vec::new();
            renderer.event(&mut output, "tool", &image).unwrap();
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains("image/png"));
            assert!(!text.contains(&data));
            assert!(text.len() < 1000);
            if !raw {
                assert!(text.contains("20 × 30"));
            }
        }
        assert_eq!(image["data"], data, "native image content is never mutated");
    }

    #[test]
    fn tool_text_and_metadata_share_bounded_previews_without_truncating_conversation() {
        let body = (0..40)
            .map(|line| format!("content-{line}\n"))
            .collect::<String>();
        let mut renderer = Renderer::new(false, false, 60);
        renderer.ui.max_inline_output = 4;
        let mut output = Vec::new();
        renderer
            .value(&mut output, "tool", &json!(body), "Result", 0)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("content-0"));
        assert!(text.contains("content-39"));
        assert!(!text.contains("content-20"));
        assert!(text.contains("lines folded"));

        let mut output = Vec::new();
        renderer
            .value(
                &mut output,
                "tool",
                &json!("界".repeat(10_000)),
                "Result",
                0,
            )
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.len() < 1200);
        assert!(text.contains("long lines shortened"));

        let mut output = Vec::new();
        renderer
            .value(
                &mut output,
                "agent",
                &json!({"role":"assistant","text":body}),
                "Event",
                0,
            )
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("content-20"),
            "conversation messages stay complete"
        );
        assert!(!text.contains("folded"));
    }

    #[test]
    fn default_metadata_has_one_heading_and_verbose_and_raw_keep_the_source() {
        let fields = (0..40)
            .map(|i| (format!("field_{i:02}"), json!(format!("value-{i}"))))
            .collect::<serde_json::Map<_, _>>();
        let value = Value::Object(fields);
        let mut renderer = Renderer::new(false, false, 80);
        renderer.ui.max_inline_output = 4;
        let mut output = Vec::new();
        renderer
            .value(&mut output, "tool", &value, "Result", 0)
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches("[RES ").count(), 1);
        assert!(text.contains("field_00"));
        assert!(text.contains("field_39"));
        assert!(!text.contains("field_20"));

        renderer.verbose = true;
        let mut verbose = Vec::new();
        renderer
            .value(&mut verbose, "tool", &value, "Result", 0)
            .unwrap();
        assert!(String::from_utf8(verbose).unwrap().contains("field_20"));
        let mut raw = Renderer::new(true, false, 80);
        let mut output = Vec::new();
        raw.event(&mut output, "tool", &value).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), value);
    }

    #[test]
    fn ordinary_command_metadata_does_not_repeat_its_invocation() {
        let mut renderer = Renderer::new(false, false, 100);
        renderer.workspace_root = Some("/fixture".into());
        let request = json!({"id":"once","method":"command/exec","params":{
            "command":["cargo","test"],"cwd":"/fixture","timeoutMs":2000,"processId":"process"}});
        let mut output = Vec::new();
        renderer.event(&mut output, "tool", &request).unwrap();
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.matches("cargo test").count(), 1);
        assert!(!text.contains("cwd=."));
        assert!(!text.contains("processId="));
        assert!(!text.contains("timeout="));
        renderer.verbose = true;
        let mut verbose = Vec::new();
        renderer.event(&mut verbose, "tool", &request).unwrap();
        let text = String::from_utf8(verbose).unwrap();
        assert!(text.contains("timeout=2.000s"));
        assert!(text.contains("cmd=cargo test"));
    }

    use super::*;

    #[test]
    fn concurrent_controller_requests_keep_their_own_identity_until_finished() {
        let mut renderer = Renderer::new(false, false, 140);
        let mut out = Vec::new();
        for (id, model) in [(1, "model-a"), (2, "model-b")] {
            renderer
                .event(
                    &mut out,
                    "controller",
                    &json!({
                        "type":"ssmcp.controller.started","request_id":id,
                        "controller":{"name":"ChatGPT","model":model,"source":"caller-reported"}
                    }),
                )
                .unwrap();
        }
        let panel = renderer.panel_snapshot(Instant::now());
        assert_eq!(panel.controllers.len(), 2);
        assert_eq!(
            panel.controllers[0]
                .identity
                .as_ref()
                .unwrap()
                .model
                .as_deref(),
            Some("model-a")
        );
        assert_eq!(
            panel.controllers[1]
                .identity
                .as_ref()
                .unwrap()
                .model
                .as_deref(),
            Some("model-b")
        );
        renderer
            .event(
                &mut out,
                "controller",
                &json!({"type":"ssmcp.controller.finished","request_id":2}),
            )
            .unwrap();
        let panel = renderer.panel_snapshot(Instant::now());
        assert_eq!(panel.controllers.len(), 1);
        assert_eq!(panel.controllers[0].request_id, "1");
        renderer
            .event(
                &mut out,
                "controller",
                &json!({"type":"ssmcp.controller.finished","request_id":1}),
            )
            .unwrap();
        let panel = renderer.panel_snapshot(Instant::now());
        assert!(panel.controllers.is_empty());
        assert_eq!(panel.last_controller.unwrap().request_id, "1");
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("model-a")
                && text.contains("model-b")
                && text.contains("caller-reported")
        );
    }

    #[test]
    fn missing_controller_metadata_is_not_replaced_by_a_previous_model() {
        let mut renderer = Renderer::new(false, false, 140);
        let mut out = Vec::new();
        renderer
            .event(
                &mut out,
                "controller",
                &json!({
                    "type":"ssmcp.controller.started","request_id":1,
                    "controller":{"name":"ChatGPT","model":"model-a","source":"caller-reported"}
                }),
            )
            .unwrap();
        renderer
            .event(
                &mut out,
                "controller",
                &json!({"type":"ssmcp.controller.finished","request_id":1}),
            )
            .unwrap();
        renderer
            .event(
                &mut out,
                "controller",
                &json!({"type":"ssmcp.controller.started","request_id":2,"controller":null}),
            )
            .unwrap();
        let panel = renderer.panel_snapshot(Instant::now());
        assert!(panel.controllers[0].identity.is_none());
        assert!(String::from_utf8(out).unwrap().contains("Model unknown"));
    }

    use serde_json::json;

    fn render(events: &[Value], raw: bool, color: bool) -> String {
        let mut renderer = Renderer::new(raw, color, 100);
        let mut out = Vec::new();
        for event in events {
            renderer.event(&mut out, "codex", event).unwrap();
        }
        renderer.finish_group(&mut out, "codex").unwrap();
        String::from_utf8(out).unwrap()
    }

    fn terminal_screen(text: &str) -> vt100::Screen {
        use std::num::NonZeroU16;
        let mut parser = vt100::Parser::new(
            NonZeroU16::new(64).unwrap(),
            NonZeroU16::new(160).unwrap(),
            0,
        );
        // A real PTY applies ONLCR to renderer line feeds.
        parser.process(text.replace('\n', "\r\n").as_bytes());
        parser.screen().clone()
    }

    fn assert_text_color(
        screen: &vt100::Screen,
        context: &str,
        fragment: &str,
        expected: vt100::Color,
    ) {
        let (row, line) = screen
            .rows(0, screen.size().1.get())
            .enumerate()
            .find(|(_, line)| line.contains(context))
            .unwrap_or_else(|| panic!("missing {context}: {}", screen.contents()));
        let start = line
            .find(fragment)
            .expect("colored fragment in matched row");
        let column = unicode_width::UnicodeWidthStr::width(&line[..start]);
        let cells = unicode_width::UnicodeWidthStr::width(fragment);
        for column in column..column + cells {
            assert_eq!(
                screen.cell(row as u16, column as u16).unwrap().fgcolor(),
                expected,
                "{context}: {fragment}"
            );
        }
    }

    #[test]
    fn raw_keeps_nonsecret_notification_shape() {
        let event = json!({"method":"command/exec/outputDelta","params":{"deltaBase64":"aGkK"},"unknown":{"x":"a\\nb"}});
        assert_eq!(
            render(std::slice::from_ref(&event), true, true),
            format!("{event}\n")
        );
    }

    #[test]
    fn repeated_backend_auth_errors_fold_across_chunks_and_timestamps() {
        let mut renderer = Renderer::new(false, false, 120);
        let mut out = Vec::new();
        for second in 0..8 {
            let line = format!(
                "2026-11-17T17:26:{second:02}.123456Z ERROR rmcp::transport::worker: Worker quit with fatal: Transport channel closed, when AuthRequired(AuthRequiredError {{ www_authenticate_header: resource_metadata=\\\"https://api.x.com/.well-known/oauth-protected-resource\\\" }})\n"
            );
            let mid = line.len() / 2;
            for chunk in [&line[..mid], &line[mid..]] {
                renderer
                    .event(&mut out, "codex", &json!({"type":"stderr","text":chunk}))
                    .unwrap();
            }
        }
        renderer.finish_group(&mut out, "codex").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text.matches("Backend MCP authentication required").count(),
            1,
            "{text}"
        );
        assert!(text.contains("https://api.x.com"), "{text}");
        assert!(
            text.contains("same diagnostic ×8") && text.contains("7 repeats folded"),
            "{text}"
        );
        assert!(
            !text.contains("Worker quit") && !text.contains('\x1b'),
            "{text}"
        );
        let snapshot = renderer.snapshot(Instant::now());
        assert_eq!(snapshot["schema_version"], 1);
        assert_eq!(snapshot["diagnostics"][0]["occurrences"], 8);
    }

    #[test]
    fn diagnostic_folding_preserves_distinct_failures_and_source_boundaries() {
        let mut renderer = Renderer::new(false, false, 120);
        let mut out = Vec::new();
        for (source, text) in [
            (
                "codex",
                "2026-11-17T17:26:00Z error: server one unavailable\n",
            ),
            (
                "codex",
                "2026-11-17T17:26:01Z error: server one unavailable\n",
            ),
            (
                "claude",
                "2026-11-17T17:26:01Z error: server one unavailable\n",
            ),
            (
                "codex",
                "2026-11-17T17:26:02Z error: server two unavailable\n",
            ),
        ] {
            renderer
                .event(&mut out, source, &json!({"type":"stderr","text":text}))
                .unwrap();
        }
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("server one unavailable").count(), 2, "{text}");
        assert_eq!(text.matches("server two unavailable").count(), 1, "{text}");
        assert!(text.contains("same diagnostic ×2"), "{text}");
        assert!(text.contains("claude"), "{text}");
    }

    #[test]
    fn repeated_backend_diagnostics_remain_complete_in_jsonl_and_verbose() {
        let events = [
            json!({"type":"stderr","text":"2026-11-17T17:26:00Z error: sample\n"}),
            json!({"type":"stderr","text":"2026-11-17T17:26:01Z error: sample\n"}),
        ];
        let raw = render(&events, true, false);
        let decoded = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(decoded, events);
        let mut renderer = Renderer::new(false, false, 120);
        renderer.verbose = true;
        let mut out = Vec::new();
        for event in &events {
            renderer.event(&mut out, "codex", event).unwrap();
        }
        assert!(renderer.diagnostics.is_empty());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("17:26:00Z") && text.contains("17:26:01Z"));
        assert!(!text.contains("repeats folded"));
    }

    #[test]
    fn diagnostic_repeat_notice_has_bounded_cadence_and_snapshot_redacts_secrets() {
        let mut renderer = Renderer::new(false, false, 140);
        let mut out = Vec::new();
        for _ in 0..3 {
            renderer
                .event(
                    &mut out,
                    "codex",
                    &json!({"type":"stderr","text":"warning: password=fixture-secret\n"}),
                )
                .unwrap();
        }
        let reported = renderer.diagnostics["codex"].reported;
        let before = out.len();
        renderer
            .flush_diagnostics_due(&mut out, reported + Duration::from_secs(29))
            .unwrap();
        assert_eq!(out.len(), before);
        renderer
            .flush_diagnostics_due(&mut out, reported + Duration::from_secs(30))
            .unwrap();
        assert!(out.len() > before);
        let after = out.len();
        renderer
            .flush_diagnostics_due(&mut out, reported + Duration::from_secs(60))
            .unwrap();
        assert_eq!(out.len(), after);
        assert!(
            !renderer
                .snapshot(Instant::now())
                .to_string()
                .contains("fixture-secret")
        );
    }

    #[test]
    fn tty_and_no_color_policy() {
        assert!(color_enabled(true, false));
        assert!(!color_enabled(true, true));
        assert!(!color_enabled(false, false));
        assert!(!color_enabled(false, true));
        let text = render(
            &[json!({"text":"\u{001b}[31mhello\u{001b}[0m"})],
            false,
            false,
        );
        assert!(!text.contains('\u{001b}'));
        assert!(text.contains("hello"));
    }

    #[test]
    fn envelopes_newlines_plain_text_and_source_escapes() {
        let event: Value =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"first\nsecond"}]}"#)
                .unwrap();
        let text = render(
            &[
                event,
                json!("plain"),
                json!({"text":"let s = \\\"\\n\\\";"}),
            ],
            false,
            false,
        );
        assert!(text.contains("first\n  second"), "{text}");
        assert!(text.contains("plain"));
        assert!(!text.contains("content"));
        assert!(text.contains("\\n"));
    }

    #[test]
    fn tools_commands_errors_status_and_usage() {
        let text = render(
            &[
                json!({"type":"ssmcp.request","request":{"method":"tools/call","params":{"name":"bash","arguments":{"command":"cargo test"}}}}),
                json!({"type":"command_execution_end","exitCode":1,"error":"first\nsecond","duration_ms":24}),
                json!({"type":"response","success":true,"usage":{"input_tokens":12,"output_tokens":4}}),
            ],
            false,
            false,
        );
        for expected in [
            "[OUT ]",
            "bash",
            "cargo test",
            "[ERR ]",
            "first\n  second",
            "duration_ms: 24",
            "[OK  ]",
            "[META]",
            "input_tokens",
        ] {
            assert!(
                text.contains(expected) || clean(&text).contains(expected),
                "missing {expected}: {text}"
            );
        }
    }

    #[test]
    fn unified_diff_categories_and_every_chunk_boundary() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,2 +2,2 @@\n-old\n+new\n same\n";
        let whole = render(&[json!({"delta":diff})], false, true);
        assert!(whole.contains("\x1b[1mdiff --git"));
        assert!(whole.contains(&format!("\x1b[{}m@@", Ui::default().color("cyan"))));
        assert!(whole.contains(&format!(
            "\x1b[{}m    2       │ -old",
            Ui::default().color("red")
        )));
        assert!(whole.contains(&format!(
            "\x1b[{}m          2 │ +new",
            Ui::default().color("green")
        )));
        assert!(whole.contains("    3     3 │  same"));
        for split in 1..diff.len() {
            let chunks = [
                json!({"delta":&diff[..split]}),
                json!({"delta":&diff[split..]}),
            ];
            assert_eq!(render(&chunks, false, true), whole, "split {split}");
        }
    }

    #[test]
    fn final_partial_line_and_base64_output_are_not_lost() {
        let text = render(
            &[
                json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode("partial")}}),
            ],
            false,
            false,
        );
        assert!(text.contains("partial"));
        let long = "x".repeat(1000);
        let wrapped = render(&[json!(long)], false, false);
        assert_eq!(wrapped.chars().filter(|c| *c == 'x').count(), 1001); // label codex includes x
    }
    #[test]
    fn rpc_reply_keeps_status_and_request_context_without_replaying_streams() {
        let events = [
            json!({"type":"ssmcp.request","request":{"id":"r1","method":"command/exec","params":{"command":["sh","-c","printf hello"],"cwd":"/project","timeoutMs":1000}}}),
            json!({"id":"r1","result":{"exitCode":0,"stdout":"hello\\nsecond\\n","stderr":"warning\\n"}}),
        ];
        let output = render(&events, false, true);
        let text = terminal_screen(&output).contents();
        for expected in [
            "[RUN ] codex id=r1 sh -c 'printf hello'",
            "[OK  ] codex id=r1 exitCode=0",
            "cwd=/project",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("hello\\n  second"));
        assert!(!text.contains("warning"));
        assert!(!text.contains("[STDOUT]"));

        // The concise view keeps the command and outcome; routing options and
        // the separately formatted command remain available in verbose output.
        let mut renderer = Renderer::new(false, true, 100);
        renderer.verbose = true;
        let mut output = Vec::new();
        for event in &events {
            renderer.event(&mut output, "codex", event).unwrap();
        }
        let text = terminal_screen(&String::from_utf8(output).unwrap()).contents();
        assert!(text.contains("cmd=sh -c"), "{text}");
        assert!(text.contains("timeout=1.000s"), "{text}");
    }

    #[test]
    fn timeout_reply_shows_status_without_replaying_streams() {
        let text = render(
            &[
                json!({"id":"r2","result":{"exitCode":124,"stdout":"partial output","stderr":"installing tool"}}),
            ],
            false,
            true,
        );
        for expected in ["command timeout", "exitCode=124"] {
            assert!(text.contains(expected), "{text}");
        }
        assert!(!text.contains("partial output"));
        assert!(!text.contains("installing tool"));
    }

    #[test]
    fn rpc_protocol_errors_keep_codes_while_raw_logs_omit_arbitrary_messages() {
        let event = json!({"id":"r3","error":{"code":-32601,"message":"unsupported method"}});
        let text = render(std::slice::from_ref(&event), false, true);
        assert!(text.contains(&format!("\x1b[{}m", Ui::default().color("red"))));
        assert!(text.contains("-32601"));
        assert!(!text.contains("unsupported method"));
        assert_eq!(
            render(std::slice::from_ref(&event), true, true),
            format!("{}\n", crate::audit::event(event.clone()))
        );
    }
    #[test]
    fn command_progress_is_visible_before_newline_and_completion() {
        let mut renderer = Renderer::new(false, false, 100);
        let mut out = Vec::new();
        let delta = |text: &str| {
            json!({
                "method":"command/exec/outputDelta",
                "params":{"processId":"running","deltaBase64":STANDARD.encode(text)}
            })
        };
        renderer.event(&mut out, "codex", &delta("live")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[STDOUT] codex · running\n  live"
        );
        renderer
            .event(&mut out, "codex", &delta(" progress"))
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[STDOUT] codex · running\n  live progress"
        );
        renderer.event(&mut out, "codex", &delta("\nnext")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[STDOUT] codex · running\n  live progress\n  next"
        );
        renderer.finish_group(&mut out, "codex").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[STDOUT] codex · running\n  live progress\n  next\n"
        );
    }

    #[test]
    fn command_progress_keeps_wrapping_state_and_finishes_once_before_headings() {
        let mut renderer = Renderer::new(false, true, 12);
        let mut out = Vec::new();
        for text in ["12345", "67890", "ab"] {
            renderer
                .event(
                    &mut out,
                    "codex",
                    &json!({
                        "method":"command/exec/outputDelta",
                        "params":{"deltaBase64":STANDARD.encode(text)}
                    }),
                )
                .unwrap();
        }
        assert_eq!(
            clean(&String::from_utf8_lossy(&out)),
            "[STDOUT] codex · command\n  1234567890\n  ab"
        );
        renderer
            .event(
                &mut out,
                "codex",
                &json!({"id":"done","result":{"exitCode":0}}),
            )
            .unwrap();
        let text = clean(&String::from_utf8(out).unwrap());
        assert!(
            text.starts_with("[STDOUT] codex · command\n  1234567890\n  ab\n[OK  ]"),
            "{text}"
        );
        assert_eq!(text.matches("1234567890").count(), 1);
        assert_eq!(text.matches("  ab").count(), 1);
    }

    #[test]
    fn command_output_preserves_diff_rendering_at_every_chunk_boundary() {
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,2 +2,2 @@\n-old\n+new\n same\n";
        let delta = |text: &str| {
            json!({
                "method":"command/exec/outputDelta",
                "params":{"deltaBase64":STANDARD.encode(text)}
            })
        };
        let whole = render(&[delta(diff)], false, true);
        assert!(whole.contains(&format!(
            "\x1b[{}m    2       │ -old",
            Ui::default().color("red")
        )));
        assert!(whole.contains(&format!(
            "\x1b[{}m          2 │ +new",
            Ui::default().color("green")
        )));
        for split in 1..diff.len() {
            assert_eq!(
                render(&[delta(&diff[..split]), delta(&diff[split..])], false, true),
                whole,
                "split {split}"
            );
        }
    }

    #[test]
    fn narrow_exec_stdout_keeps_unicode_chunks_and_redacts_secrets() {
        let mut renderer = Renderer::new(false, false, 12);
        let mut output = Vec::new();
        // These are complete UTF-8 chunks, not arbitrary byte fragments.
        let chunks = [
            format!("{}\u{3001}exit=0", "a".repeat(51)),
            "\u{1f980}\n".to_owned(),
            "token_test\u{3001}API_TOKEN=renderer-only-secret\n".to_owned(),
        ];
        let mut offset = 0;
        for chunk in &chunks {
            let next = offset + chunk.len();
            renderer
                .event(
                    &mut output,
                    "test",
                    &json!({
                        "type":"exec.stdout",
                        "job_id":"unicode",
                        "method":"command/exec/outputDelta",
                        "params":{
                            "processId":"unicode","stream":"stdout",
                            "offset":offset,"next_offset":next,
                            "deltaBase64":STANDARD.encode(chunk)
                        }
                    }),
                )
                .unwrap();
            offset = next;
        }
        renderer.finish_group(&mut output, "test").unwrap();
        let text = String::from_utf8(output).unwrap();
        let mut rows = text.lines();
        assert_eq!(rows.next(), Some("[STDOUT] test \u{b7} unicode"));
        let body = rows
            .map(|line| {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line) <= 12,
                    "{line:?}"
                );
                line.strip_prefix("  ").expect("stdout line indentation")
            })
            .collect::<String>();
        assert_eq!(
            body,
            format!(
                "{}\u{3001}exit=0\u{1f980}token_test\u{3001}API_TOKEN=[redacted]",
                "a".repeat(51)
            )
        );
        assert_eq!(text.matches("[STDOUT]").count(), 1);
        assert!(!body.contains("renderer-only-secret"));
        assert!(!text.contains("[OK"));
    }

    #[test]
    fn base64_partial_output_is_visible_before_completion() {
        let mut renderer = Renderer::new(false, false, 100);
        let mut output = Vec::new();
        renderer.event(&mut output, "codex", &json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode("still running")}})).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("still running"));
    }
    #[test]
    fn pretty_folds_output_keeps_tail_and_verbose_reveals_json() {
        let stdout = (0..40).map(|n| format!("line-{n}\n")).collect::<String>();
        let event = json!({"type":"command_output","exitCode":1,"stdout":stdout,"stderr":"fatal: stopped","huge":{"blob":"x".repeat(10000)}});
        let text = render(std::slice::from_ref(&event), false, false);
        assert!(text.contains("lines folded"));
        assert!(text.contains("line-39"));
        assert!(text.contains("fatal: stopped"));
        assert!(text.len() < 2500, "{}", text.len());
        assert!(!text.contains(&"x".repeat(1000)));
        let mut r = Renderer::new(false, false, 100);
        r.verbose = true;
        let mut out = Vec::new();
        r.event(&mut out, "codex", &event).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[RAW]"));
        assert!(text.contains("line-20"));
        assert!(text.contains(&"x".repeat(1000)));
    }

    #[test]
    fn progress_warnings_failure_and_cr_are_distinct() {
        let text = render(
            &[
                json!({"stderr":"mise downloading codex@0.169.0 16.3/192.3 MB\rwarning: retrying\rfatal: download failed\n"}),
            ],
            false,
            true,
        );
        for expected in [
            "[PROG]",
            "[WARN]",
            "[ERR ]",
            &format!("\x1b[{}m", Ui::default().color("red")),
        ] {
            assert!(text.contains(expected), "{text}");
        }
        assert_eq!(clean("a\rb\r\nc"), "a\nb\nc");
    }

    #[test]
    fn request_ids_are_backend_scoped_and_can_be_hidden() {
        let mut r = Renderer::new(false, false, 100);
        let mut out = Vec::new();
        for label in ["one", "two"] {
            r.event(&mut out, label, &json!({"id":1,"method":label,"params":{}}))
                .unwrap();
        }
        r.event(&mut out, "one", &json!({"id":1,"result":{}}))
            .unwrap();
        assert_eq!(r.requests.len(), 1);
        assert!(r.requests.contains_key("two\u{0}1"));
        r.ui.show_request_id = false;
        out.clear();
        r.event(&mut out, "two", &json!({"id":1,"result":{}}))
            .unwrap();
        assert!(!String::from_utf8(out).unwrap().contains("id="));
    }

    #[test]
    fn interleaved_processes_and_streams_never_join_partial_lines() {
        let mut r = Renderer::new(false, false, 100);
        let mut out = Vec::new();
        for (pid, stream, text) in [
            ("a", "stdout", "first"),
            ("b", "stderr", "second"),
            ("a", "stdout", "tail\n"),
        ] {
            r.event(&mut out,"codex",&json!({"method":"command/exec/outputDelta","params":{"processId":pid,"stream":stream,"deltaBase64":STANDARD.encode(text)}})).unwrap();
        }
        r.finish_group(&mut out, "codex").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("firstsecond"));
        assert_eq!(text.matches("first").count(), 1);
        assert!(text.contains("[STDERR]"));
        assert!(text.contains("tail"));
    }

    #[test]
    fn theme_and_timestamp_are_display_only() {
        let mut r = Renderer::new(false, true, 100);
        r.ui.theme = super::super::ui::Theme::Nord;
        r.ui.timestamps = true;
        let mut out = Vec::new();
        r.event(&mut out, "codex", &json!({"id":"t","error":"failed"}))
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains(super::super::ui::Theme::Nord.color("red")));
        assert!(clean(&text).contains("Z ["));
        let mut r = Renderer::new(true, true, 100);
        r.ui.timestamps = true;
        let mut out = Vec::new();
        let event = json!({"type":"ssmcp.status","level":"Error","message":"x\ny"});
        r.event(&mut out, "codex", &event).unwrap();
        assert_eq!(out, format!("{event}\n").as_bytes());
    }

    #[test]
    fn custom_palette_applies_to_diagnostic_bodies_and_status_headings() {
        let mut renderer = Renderer::new(false, true, 120);
        renderer.ui.colors.error = "#123456".into();
        renderer.ui.colors.warning = "#987654".into();
        renderer.ui.colors.muted = "#536475".into();
        let mut out = Vec::new();
        renderer
            .event(
                &mut out,
                "codex",
                &json!({"type":"stderr","text":"fatal: unavailable\n"}),
            )
            .unwrap();
        renderer
            .event(
                &mut out,
                "client",
                &json!({"type":"ssmcp.status","level":"Error","message":"failed"}),
            )
            .unwrap();
        let screen = terminal_screen(&String::from_utf8(out).unwrap());
        assert_text_color(
            &screen,
            "fatal: unavailable",
            "[ERR ] fatal: unavailable",
            vt100::Color::Rgb(18, 52, 86),
        );
        assert_text_color(
            &screen,
            "client failed",
            "[ERR ]",
            vt100::Color::Rgb(18, 52, 86),
        );
        assert_text_color(
            &screen,
            "client failed",
            "failed",
            vt100::Color::Rgb(18, 52, 86),
        );
        assert_text_color(
            &screen,
            "[STDERR]",
            "[STDERR]",
            vt100::Color::Rgb(152, 118, 84),
        );
        assert_text_color(
            &screen,
            "client failed",
            "client",
            vt100::Color::Rgb(83, 100, 117),
        );
    }

    #[test]
    fn job_lifecycle_has_commands_status_and_distinct_stream_labels() {
        let output = render(
            &[
                json!({"type":"exec.started","job_id":"42-1","command":["cargo","test"],"cwd":"/work"}),
                json!({"method":"command/exec/outputDelta","params":{"processId":"42-1","stream":"stdout","deltaBase64":STANDARD.encode("ok\n")}}),
                json!({"type":"exec.exited","job_id":"42-1","state":"completed","exit_code":0,"duration_ms":1234}),
                json!({"type":"ssmcp.file.read","path":"src/main.rs"}),
                json!({"type":"ssmcp.skill.read","tool":"ssmcp.skills.read","skill":"skills/review/SKILL.md"}),
            ],
            false,
            false,
        );
        assert_eq!(output.matches("cargo test").count(), 2);
        assert!(output.contains("[RUN ] codex 42-1"));
        assert!(output.contains("codex · 42-1"));
        assert!(output.contains("completed exit=0 1.23s"));
        assert!(output.contains("[READ] codex src/main.rs"));
        assert!(output.contains("[SKIL] codex read"));
        assert!(!output.contains("{\"type\""));
    }

    #[test]
    fn internal_discovery_folds_only_repeated_success_and_keeps_explicit_calls() {
        let exchange = |id: &str, internal: bool, failed: bool| {
            let mut request = json!({"type":"ssmcp.request","request":{
                "id":id,"method":"mcpServerStatus/list","params":{}}});
            let mut response = if failed {
                json!({"id":id,"error":{"code":-32000,"message":"native discovery failed"}})
            } else {
                json!({"id":id,"result":{"status":"ready"}})
            };
            if internal {
                request["internal_discovery"] = json!(true);
                response["internal_discovery"] = json!(true);
            }
            [request, response]
        };
        let events = [
            exchange("first", true, false),
            exchange("again", true, false),
            exchange("failed", true, true),
            exchange("still-failed", true, true),
            exchange("recovered", true, false),
            exchange("explicit", false, false),
        ]
        .concat();
        let pretty = render(&events, false, false);
        assert!(pretty.contains("id=first"), "{pretty}");
        assert!(!pretty.contains("id=again"), "{pretty}");
        for id in ["failed", "still-failed", "recovered", "explicit"] {
            assert!(pretty.contains(&format!("id={id}")), "{pretty}");
        }
        assert_eq!(pretty.matches("id=explicit").count(), 2, "{pretty}");
        let mut renderer = Renderer::new(false, false, 100);
        renderer.verbose = true;
        let mut output = Vec::new();
        for event in &events {
            renderer.event(&mut output, "codex", event).unwrap();
        }
        assert!(String::from_utf8(output).unwrap().contains("id=again"));
        let raw = render(&events, true, false);
        assert!(raw.contains("again") && raw.contains("internal_discovery"));
    }

    #[test]
    fn harness_display_has_one_namespace_and_distinguishes_equal_call_ids() {
        let mut renderer = Renderer::new(false, false, 100);
        let mut output = Vec::new();
        for provider in ["codex", "claude"] {
            renderer
                .event(
                    &mut output,
                    provider,
                    &json!({
                        "type":"ssmcp.harness.tool.started","provider":provider,
                        "tool":format!("{provider}.lookup"),"call_id":7
                    }),
                )
                .unwrap();
        }
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("[REQ ] codex.lookup call=7 started"),
            "{text}"
        );
        assert!(!text.contains("codex codex"), "{text}");
        assert!(
            text.contains("codex:7") && text.contains("claude:7"),
            "{text}"
        );
        assert_eq!(renderer.activities.len(), 2);
    }

    #[test]
    fn dashboard_snapshot_keeps_native_progress_separate_from_jobs() {
        let mut renderer = Renderer::new(false, false, 77);
        renderer.dashboard_active = true;
        renderer.workspace_root = Some(std::path::PathBuf::from("/workspace/project"));
        let mut output = Vec::new();
        let nonce = "1234567890abcdef1234567890abcdef";
        for suffix in ["a", "b"] {
            renderer
                .event(
                    &mut output,
                    "shell",
                    &json!({
                        "type":"exec.started","job_id":format!("{nonce}-{suffix}"),
                        "command":["cargo","test"],"cwd":"/workspace/project"
                    }),
                )
                .unwrap();
        }
        for provider in ["codex", "claude"] {
            renderer
                .event(
                    &mut output,
                    provider,
                    &json!({
                        "method":"notifications/progress","params":{
                            "progressToken":7,"progress":3,"message":"Indexing sources"
                        }
                    }),
                )
                .unwrap();
        }
        let snapshot = renderer.panel_snapshot(Instant::now());
        assert_eq!(snapshot.tasks.len(), 2);
        assert_eq!(snapshot.tasks[0].id, "#a");
        assert_eq!(snapshot.tasks[1].cwd.as_deref(), Some("."));
        assert_eq!(snapshot.progress.len(), 2);
        assert!(
            snapshot
                .progress
                .iter()
                .all(|progress| progress.total.is_none())
        );
        assert!(
            snapshot
                .progress
                .iter()
                .any(|progress| progress.source == "codex:7")
        );
        assert!(
            snapshot
                .progress
                .iter()
                .any(|progress| progress.source == "claude:7")
        );
        let transcript = String::from_utf8(output).unwrap();
        assert!(!transcript.contains("[PROG]"), "{transcript}");
        let rows = super::super::panel::render(&snapshot, 28, 58, Instant::now());
        assert!(!rows.iter().any(|line| line.text.contains('%')));
        let mut output = Vec::new();
        renderer.event(&mut output, "shell", &json!({
            "type":"exec.exited","job_id":format!("{nonce}-a"),"exit_code":0,"state":"completed"
        })).unwrap();
        let snapshot = renderer.panel_snapshot(Instant::now());
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, "#b");
        assert_eq!(
            snapshot.progress.len(),
            2,
            "a progress token is not assumed to belong to a shell job"
        );
    }

    #[test]
    fn terminal_preview_fixture() {
        let Some(path) = std::env::var_os("SSMCP_RENDER_PREVIEW") else {
            return;
        };
        // This is a labeled renderer fixture. Its long-diff reference is backed
        // by a real capture, not an invented path or artifact identifier.
        let store = crate::output::OutputStore::new_connection().unwrap();
        let diff = format!(
            "--- a/generated/report.txt\n+++ b/generated/report.txt\n@@ -0,0 +1,600 @@\n{}",
            (1..=600)
                .map(|line| format!("+diagnostic row {line}\n"))
                .collect::<String>()
        );
        let mut capture = store.capture("preview:diff".to_owned());
        capture.push(diff.as_bytes()).unwrap();
        capture.finish().unwrap();
        let artifact = capture.metadata().expect("the real diff exceeds 500 lines");
        let preview = diff.lines().take(7).collect::<Vec<_>>().join("\n") + "\n";
        let nonce = "1234567890abcdef1234567890abcdef";
        let job_a = format!("{nonce}-81");
        let job_b = format!("{nonce}-82");
        let mut renderer = Renderer::new(false, true, 100);
        renderer.ui.theme = super::super::ui::Theme::Minimal;
        renderer.workspace_root = Some("/workspace/ssmcp".into());
        let mut output = Vec::new();
        let now = Instant::now();
        for event in [
            json!({"type":"ssmcp.status","level":"Connected","message":"workstation · unified activity preview"}),
            json!({"type":"exec.started","job_id":job_a,"command":["cargo","test","--locked"],"cwd":"/workspace/ssmcp"}),
            json!({"type":"exec.started","job_id":job_b,"command":["cargo","clippy","--locked"],"cwd":"/workspace/ssmcp"}),
        ] {
            renderer.event(&mut output, "dev", &event).unwrap();
        }
        // Age only the fixture's entries; subsequent real Instants cannot make
        // the preview's elapsed time run backwards.
        for id in [&job_a, &job_b] {
            let key = activity_key("job", "dev", id);
            let entry = renderer.activities.finish(&key).unwrap();
            renderer.activities.start(
                key,
                entry.id,
                entry.description,
                entry.cwd,
                now - Duration::from_secs(6),
            );
        }
        renderer.last_activity_summary = now - Duration::from_secs(6);
        renderer
            .heartbeat(&mut output, Instant::now(), true)
            .unwrap();
        for event in [
            json!({"type":"exec.stdout","job_id":job_a,"method":"command/exec/outputDelta",
                "params":{"processId":job_a,"stream":"stdout","deltaBase64":STANDARD.encode("running command lifecycle tests\nparallel_commands ... ok\n")}}),
            json!({"method":"notifications/progress","params":{"progressToken":"index","progress":3,"message":"Indexing sources"}}),
            json!({"method":"notifications/progress","params":{"progressToken":"index","progress":3,"total":12,"message":"Native progress"}}),
            json!({"type":"ssmcp.file.read","path":"/workspace/ssmcp/src/client/ui.rs"}),
            json!({"type":"ssmcp.file.write","path":"/workspace/ssmcp/src/client/ui.rs","written":182}),
            json!({"type":"ssmcp.file.diff","path":"/workspace/ssmcp/src/client/ui.rs",
                "added_lines":1,"removed_lines":0,"diff_complete":true,
                "diff":"--- a/src/client/ui.rs\n+++ b/src/client/ui.rs\n@@ -5,1 +5,2 @@\n max_inline_output: 12,\n+activity_interval_secs: 5,\n"}),
            json!({"type":"ssmcp.file.diff","path":"/workspace/ssmcp/generated/report.txt",
                "added_lines":600,"removed_lines":0,"diff_complete":true,
                "preview":preview,"preview_truncated":true,"diff_artifact":artifact}),
            json!({"type":"ssmcp.skill.read","path":"skills/ssmcp/SKILL.md","invoked":false}),
            json!({"type":"ssmcp.harness.tool.started","call_id":7,"provider":"codex","tool":"codex.mcp.example.lookup"}),
            json!({"type":"ssmcp.harness.tool.completed","call_id":7,"provider":"codex","tool":"codex.mcp.example.lookup","success":true,"duration_ms":140}),
            json!({"type":"exec.exited","job_id":job_b,"state":"completed","exit_code":0,"duration_ms":6800}),
            json!({"type":"exec.exited","job_id":job_a,"state":"completed","exit_code":0,"duration_ms":7100}),
        ] {
            renderer.event(&mut output, "dev", &event).unwrap();
        }
        renderer.finish_group(&mut output, "dev").unwrap();
        std::fs::write(path, output).unwrap();
    }
    #[test]
    fn every_log_mode_redacts_secrets_and_preserves_native_values() {
        let events = [
            json!({"type":"note","text":"visible progress","env":{"PATH":"/private-path-value"},"api_key":"private-token-value"}),
            json!({"id":"result","result":{"exit_code":0,"text":"private-file-value","stdout":"replayed-value"}}),
            json!({"type":"ssmcp.file.read","path":".env","text":"private-env-file-value"}),
        ];
        for (raw, verbose) in [(false, false), (true, false), (false, true)] {
            let mut renderer = Renderer::new(raw, false, 100);
            renderer.verbose = verbose;
            let mut output = Vec::new();
            for event in &events {
                renderer.event(&mut output, "test", event).unwrap();
            }
            renderer.finish_group(&mut output, "test").unwrap();
            let text = String::from_utf8(output).unwrap();
            for secret in [
                "/private-path-value",
                "private-token-value",
                "private-file-value",
                "replayed-value",
                "private-env-file-value",
            ] {
                assert!(
                    !text.contains(secret),
                    "raw={raw} verbose={verbose}: {text}"
                );
            }
            assert!(text.contains("visible progress"), "{text}");
        }
        assert_eq!(events[1]["result"]["text"], "private-file-value");
    }

    #[test]
    fn live_stdout_and_stderr_are_not_replayed_by_wait_or_read_responses() {
        let events = [
            json!({"type":"exec.started","job_id":"81","command":["cargo","test"]}),
            json!({"type":"exec.stdout","job_id":"81","text":"unique-out\n",
                "method":"command/exec/outputDelta","params":{"processId":"81","stream":"stdout",
                    "offset":0,"next_offset":11,"deltaBase64":STANDARD.encode("unique-out\n")}}),
            json!({"type":"exec.stderr","job_id":"81","text":"unique-err\n",
                "method":"command/exec/outputDelta","params":{"processId":"81","stream":"stderr",
                    "offset":0,"next_offset":11,"deltaBase64":STANDARD.encode("unique-err\n")}}),
            json!({"id":"wait","method":"tools/call","params":{"name":"ssmcp.exec.wait","arguments":{"id":"81"}}}),
            json!({"id":"wait","result":{"structuredContent":{"id":"81","exit_code":0,"stdout":"unique-out\n","stderr":"unique-err\n"}}}),
            json!({"id":"read","result":{"content":[{"type":"text","text":"unique-out\nunique-err\n"}]}}),
            json!({"type":"exec.exited","job_id":"81","state":"completed","exit_code":0,"duration_ms":25}),
        ];
        for verbose in [false, true] {
            let mut renderer = Renderer::new(false, false, 100);
            renderer.verbose = verbose;
            let mut output = Vec::new();
            for event in &events {
                renderer.event(&mut output, "codex", event).unwrap();
            }
            renderer.finish_group(&mut output, "codex").unwrap();
            let text = String::from_utf8(output).unwrap();
            assert_eq!(text.matches("unique-out").count(), 1, "{text}");
            assert_eq!(text.matches("unique-err").count(), 1, "{text}");
            assert!(text.contains("[STDOUT] codex · 81"));
            assert!(text.contains("[STDERR] codex · 81"));
            assert!(text.contains("completed exit=0"));
        }
    }

    #[test]
    fn file_skill_and_harness_events_show_only_observed_states() {
        let diff = "--- a/file\n+++ b/file\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let text = render(
            &[
                json!({"type":"ssmcp.file.write","path":"file","written":4}),
                json!({"type":"ssmcp.file.diff","path":"file","added_lines":1,"removed_lines":1,
                "diff":diff,"preview":diff,"diff_complete":true}),
                json!({"type":"ssmcp.file.diff","path":"other","added_lines":null,"removed_lines":null,
                "diff_complete":false,"diff_unavailable_reason":"snapshot_unknown"}),
                json!({"type":"ssmcp.skill.discovery.completed","shown":0,"invoked":false}),
                json!({"type":"ssmcp.skill.discovered","path":"skills/review/SKILL.md","invoked":false}),
                json!({"type":"ssmcp.skill.read","path":"skills/review/SKILL.md","invoked":false}),
                json!({"type":"ssmcp.skill.invoked","name":"explicit-native-invocation"}),
                json!({"type":"ssmcp.harness.tool.started","call_id":7,"provider":"codex","tool":"codex.mcp.graph.lookup","generation":2}),
                json!({"type":"ssmcp.harness.tool.completed","call_id":7,"provider":"codex","tool":"codex.mcp.graph.lookup","success":true,"duration_ms":12,"file_audit":"unknown"}),
            ],
            false,
            false,
        );
        assert_eq!(text.matches("-old").count(), 1, "{text}");
        assert_eq!(text.matches("+new").count(), 1, "{text}");
        for expected in [
            "+1 / -1",
            "line counts unknown",
            "diff unavailable: snapshot_unknown",
            "discovery completed 0 summaries",
            "discovered",
            "read skills/review",
            "invoked explicit-native-invocation",
            "codex.mcp.graph.lookup",
            "completed",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("+0 / -0"));
        assert!(!text.contains("skill used"));
    }

    #[test]
    fn activity_snapshot_tracks_parallel_jobs_and_relative_paths() {
        let nonce = "1234567890abcdef1234567890abcdef";
        let mut renderer = Renderer::new(false, false, 80);
        renderer.workspace_root = Some("/workspace".into());
        let mut out = Vec::new();
        for (suffix, command) in [(81, "build"), (82, "test")] {
            renderer
                .event(
                    &mut out,
                    "shell",
                    &json!({
                        "type":"exec.started","job_id":format!("{nonce}-{suffix}"),
                        "command":["cargo",command],"cwd":"/workspace/project"
                    }),
                )
                .unwrap();
        }
        renderer
            .event(
                &mut out,
                "files",
                &json!({"type":"ssmcp.file.read","path":"/workspace/src/lib.rs"}),
            )
            .unwrap();
        renderer
            .event(
                &mut out,
                "shell",
                &json!({
                    "type":"exec.exited","job_id":format!("{nonce}-81"),"state":"completed",
                    "exit_code":0,"duration_ms":1250
                }),
            )
            .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("[RUN ] shell #81 cargo build started"),
            "{text}"
        );
        assert!(text.contains("cwd project"), "{text}");
        assert!(text.contains("2 running"), "{text}");
        assert!(text.contains("[READ] files src/lib.rs"), "{text}");
        assert!(text.contains("#81 completed exit=0 1.25s"), "{text}");
        assert!(text.contains("1 running"), "{text}");
        assert!(
            !text.contains(nonce) && !text.contains("/workspace"),
            "{text}"
        );
        assert_eq!(renderer.activities.len(), 1);
    }

    #[test]
    fn quiet_activity_uses_elapsed_and_yields_to_the_interactive_pty() {
        let mut renderer = Renderer::new(false, false, 100);
        let now = Instant::now();
        renderer.activities.start(
            activity_key("job", "shell", "81"),
            "81".into(),
            "cargo test".into(),
            None,
            now,
        );
        renderer.last_activity_summary = now;
        let mut out = Vec::new();
        assert!(
            !renderer
                .heartbeat(&mut out, now + Duration::from_secs(4), true)
                .unwrap()
        );
        assert!(
            renderer
                .heartbeat(&mut out, now + Duration::from_secs(6), true)
                .unwrap()
        );
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(
            text.contains("1 running") && text.contains("6.0s"),
            "{text}"
        );
        assert!(!text.contains('%') && !text.contains('\x1b'));
        renderer.last_pty_output = Some(now + Duration::from_secs(12));
        assert!(
            !renderer
                .heartbeat(&mut out, now + Duration::from_secs(13), true)
                .unwrap()
        );
        renderer.raw = true;
        assert!(
            !renderer
                .heartbeat(&mut out, now + Duration::from_secs(30), true)
                .unwrap()
        );
        assert_eq!(String::from_utf8(out).unwrap(), text);
    }

    #[test]
    fn native_progress_needs_a_real_positive_total() {
        let without = render(
            &[json!({
                "method":"notifications/progress",
                "params":{"progressToken":"p1","progress":3,"message":"Indexing"}
            })],
            false,
            false,
        );
        assert!(
            without.contains("progress 3") && without.contains("total unknown"),
            "{without}"
        );
        assert!(!without.contains('%'));
        let measured = render(
            &[json!({
                "method":"notifications/progress",
                "params":{"progressToken":"p1","progress":3,"total":12,"message":"Indexing"}
            })],
            false,
            false,
        );
        assert!(measured.contains("25% (3/12)"), "{measured}");
        assert!(measured.contains("━━────────"), "{measured}");
        for invalid in [
            json!({"progress":1,"total":0}),
            json!({"progress":9,"total":3}),
        ] {
            assert!(!activity::progress(&invalid).contains('%'));
        }
    }

    #[test]
    fn pretty_labels_inline_scripts_and_verbose_or_jsonl_keep_source() {
        let script = "script_statement_visible();\n".repeat(200);
        let id = format!("request-{}-17", "a".repeat(128));
        let events = [
            json!({"type":"exec.started","job_id":"81","command":["python3","-c",script],"cwd":"/work"}),
            json!({"id":id,"method":"command/exec","params":{"command":["python3","-c",script]}}),
        ];
        let pretty = render(&events, false, false);
        assert!(pretty.contains("python3 -c · 200 lines"), "{pretty}");
        assert!(pretty.contains("200 lines"), "{pretty}");
        assert!(!pretty.contains("script_statement_visible"), "{pretty}");
        assert!(
            !pretty.contains("cmd=") && !pretty.contains("folded"),
            "{pretty}"
        );
        assert!(!pretty.contains(&"a".repeat(64)));
        assert!(pretty.len() < 1000, "{pretty}");
        let mut renderer = Renderer::new(false, false, 100);
        renderer.verbose = true;
        let mut output = Vec::new();
        for event in &events {
            renderer.event(&mut output, "codex", event).unwrap();
        }
        let verbose = String::from_utf8(output).unwrap();
        assert!(verbose.contains("python3 -c <script>"), "{verbose}");
        assert!(verbose.contains("200 │ script_statement_visible();"));
        let raw = render(&events, true, false);
        assert!(raw.contains("script_statement_visible") && raw.contains(&id));
    }

    #[test]
    fn json_stdout_is_labeled_and_cannot_claim_command_success() {
        let line = serde_json::to_string(&json!({
            "exit_code":0,"status":"completed","message":"a failed program phase",
            "text":"first\nsecond\n"
        }))
        .unwrap()
            + "\n";
        let events = [
            json!({"type":"exec.started","job_id":"81","command":["fixture"],"cwd":"/work"}),
            json!({"type":"exec.stdout","job_id":"81","method":"command/exec/outputDelta",
                "params":{"processId":"81","stream":"stdout","deltaBase64":STANDARD.encode(line)}}),
            json!({"type":"exec.exited","job_id":"81","state":"completed","exit_code":9,"duration_ms":20}),
        ];
        let text = render(&events, false, false);
        assert!(text.contains("[STDOUT] codex · 81"), "{text}");
        assert!(
            text.contains("JSON output") && text.contains("exit_code: 0"),
            "{text}"
        );
        assert!(text.contains("message: a failed program phase"), "{text}");
        assert!(text.contains("text: first\n  second"), "{text}");
        assert!(!text.contains("{\"") && !text.contains("\\n"), "{text}");
        assert!(!text.contains("[OK  ]"));
        assert_eq!(text.matches("[ERR ]").count(), 1, "{text}");
        assert!(text.contains("exit=9"), "{text}");
    }

    #[test]
    fn dashboard_keeps_large_chunked_json_streams_available_for_expansion() {
        let body = json!({"data":"x".repeat(20_000),"tail":"STREAM_END"}).to_string();
        let mut renderer = Renderer::new(false, false, 80);
        renderer.dashboard_active = true;
        renderer.ui.max_inline_output = 2;
        let mut output = Vec::new();
        for chunk in body.as_bytes().chunks(1024) {
            renderer
                .event(
                    &mut output,
                    "codex",
                    &json!({
                        "method":"command/exec/outputDelta",
                        "params":{"processId":"json-fixture","stream":"stdout",
                            "deltaBase64":STANDARD.encode(chunk)}
                    }),
                )
                .unwrap();
        }
        renderer.finish_group(&mut output, "codex").unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.starts_with("[STDOUT] codex · json-fixture\n"),
            "{text}"
        );
        let restored = text
            .lines()
            .skip(1)
            .map(|line| line.strip_prefix("  ").unwrap_or(line))
            .collect::<String>();
        assert_eq!(restored, body, "folding must not discard the stream body");
    }

    #[test]
    fn parallel_command_results_keep_stream_ids_exit_codes_and_durations() {
        let text = render(
            &[
                json!({"type":"exec.started","job_id":"81","command":["cargo","test"],"cwd":"/work/test"}),
                json!({"type":"exec.started","job_id":"82","command":["cargo","check"],"cwd":"/work/check"}),
                json!({"type":"exec.stdout","job_id":"81","text":"tests passed\n"}),
                json!({"type":"exec.stderr","job_id":"82","text":"compiler detail\n"}),
                json!({"type":"exec.exited","job_id":"82","state":"completed","exit_code":2,"duration_ms":120}),
                json!({"type":"exec.exited","job_id":"81","state":"completed","exit_code":0,"duration_ms":500}),
            ],
            false,
            false,
        );
        for expected in [
            "[STDOUT] codex · 81",
            "tests passed",
            "[STDERR] codex · 82",
            "compiler detail",
            "[ERR ] codex 82 completed exit=2 0.12s",
            "[OK  ] codex 81 completed exit=0 0.50s",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert_eq!(text.matches("cwd /work/test").count(), 1, "{text}");
        assert_eq!(text.matches("cwd /work/check").count(), 1, "{text}");
    }

    #[test]
    fn large_json_is_folded_across_chunks_and_kept_in_raw_events() {
        let line =
            serde_json::to_string(&json!({"result":"not_for_pretty".repeat(4000)})).unwrap() + "\n";
        let mut renderer = Renderer::new(false, false, 100);
        let mut out = Vec::new();
        let mut offset = 0;
        for bytes in line.as_bytes().chunks(8192) {
            let event = json!({"type":"exec.stdout","job_id":"81","method":"command/exec/outputDelta",
                "params":{"processId":"81","stream":"stdout","offset":offset,"next_offset":offset+bytes.len(),
                    "deltaBase64":STANDARD.encode(bytes)}});
            renderer.event(&mut out, "shell", &event).unwrap();
            offset += bytes.len();
        }
        renderer.finish_group(&mut out, "shell").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[STDOUT] shell · 81"), "{text}");
        assert!(text.contains("large record folded"), "{text}");
        assert!(
            !text.contains("not_for_pretty") && text.len() < 300,
            "{text}"
        );
        let one_chunk = render(
            &[json!({"method":"command/exec/outputDelta","params":{
                "processId":"81","stream":"stdout","deltaBase64":STANDARD.encode(&line)
            }})],
            false,
            false,
        );
        assert!(one_chunk.contains("large record folded"));
        assert!(!one_chunk.contains("not_for_pretty") && one_chunk.len() < 300);
        let event = json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode(&line)}});
        assert_eq!(
            render(std::slice::from_ref(&event), true, false),
            format!("{}\n", crate::audit::event(event))
        );
    }

    #[test]
    fn duplicate_live_offsets_do_not_repeat_either_stream() {
        let mut renderer = Renderer::new(false, false, 100);
        let mut out = Vec::new();
        for stream in ["stdout", "stderr"] {
            let text = format!("unique-{stream}\n");
            let event = json!({"type":format!("exec.{stream}"),"job_id":"81",
                "method":"command/exec/outputDelta","params":{"processId":"81","stream":stream,
                "offset":0,"next_offset":text.len(),"deltaBase64":STANDARD.encode(text)}});
            renderer.event(&mut out, "shell", &event).unwrap();
            renderer.event(&mut out, "shell", &event).unwrap();
        }
        renderer.finish_group(&mut out, "shell").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches("unique-stdout").count(), 1, "{text}");
        assert_eq!(text.matches("unique-stderr").count(), 1, "{text}");
    }

    #[test]
    fn diff_query_hint_requires_a_real_artifact_or_cursor() {
        let without = render(
            &[json!({"type":"ssmcp.file.diff","path":"a.rs",
            "added_lines":1,"removed_lines":1,"diff_complete":true,"preview_truncated":true})],
            false,
            false,
        );
        assert!(without.contains("+1 / -1") && without.contains("a.rs"));
        assert!(!without.contains("outputs.read") && !without.contains("queryable"));
        let actual = render(
            &[json!({"type":"ssmcp.file.diff","path":"a.rs",
            "added_lines":1,"removed_lines":1,"diff_complete":true,"preview_truncated":true,
            "diff_artifact":{"id":"actual-diff-id","path":"/tmp/ssmcp/1000/output-actual.log",
                "bytes":40000,"lines":1000,"complete":true}})],
            false,
            false,
        );
        assert!(
            actual.contains("outputs.read") && actual.contains("actual-diff-id"),
            "{actual}"
        );
    }

    #[test]
    fn absent_exit_status_is_not_reported_as_success() {
        let text = render(
            &[json!({"type":"exec.exited","job_id":"81","state":"finished"})],
            false,
            false,
        );
        assert!(!text.contains("[OK  ]"), "{text}");
        assert!(
            text.contains("exit unknown") && text.contains("[WARN]"),
            "{text}"
        );
        assert!(text.contains("duration unknown"), "{text}");
    }
}
