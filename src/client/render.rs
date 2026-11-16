//! Human transcript only; never changes events retained for MCP consumers.
use super::{
    git_status::GitSnapshot,
    panel::{ControllerView, PanelSnapshot, ProgressView, TaskView},
    ui::Ui,
};
#[path = "activity.rs"]
mod activity;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize)]
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

pub(crate) fn event(label: &str, value: &Value) {
    let _emission = EMISSION.lock().unwrap_or_else(|e| e.into_inner());
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
    tone: &'static str,
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
    requests: HashMap<String, (String, Instant)>,
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
        }
    }

    fn event(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<()> {
        let safe = crate::audit::event(value.clone());
        let value = &safe;
        let controller_event = self.controller(out, value)?;
        if self.raw {
            writeln!(out, "{value}")?;
        } else if !controller_event
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
                self.ui.theme.color("gray"),
                &format!(
                    "[RAW] {}\n",
                    serde_json::to_string_pretty(&debug_metadata(value)).unwrap_or_default()
                ),
            )?;
        }
        out.flush()
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
                        self.ui.theme.color("gray"),
                        &format!("       cwd {}\n", clean(&self.path_label(cwd))),
                    )?;
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
                    .unwrap_or_default();
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
                if let Some(cwd) = job.as_ref().and_then(|job| job.cwd.as_deref()) {
                    painted(
                        out,
                        self.color,
                        self.ui.theme.color("gray"),
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
                self.ui.theme.color("gray"),
                &format!("       {}\n", clean(path)),
            )?;
        }
        if let Some(id) = artifact["id"].as_str() {
            painted(
                out,
                self.color,
                self.ui.theme.color("gray"),
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
            if self.requests.len() >= 2048 {
                self.requests.clear();
            }
            self.requests.insert(key, (method.into(), Instant::now()));
            if value["internal_discovery"] == true
                && !self.verbose
                && self
                    .discovery_success
                    .contains_key(&format!("{label}\0{method}"))
            {
                return Ok(true);
            }
            self.summary(out, label, "Request", &format!("{method}{id_text}"))?;
            let params = &v["params"];
            let params = params.get("arguments").unwrap_or(params);
            if let Some(command) = params.get("command").or_else(|| params.get("cmd")) {
                let command = command_label(command);
                self.text(out, label, &format!("cmd={command}\n"))?;
            }
            for field in ["cwd", "name", "tool", "processId"] {
                if let Some(v) = params.get(field).filter(|v| !v.is_null()) {
                    let text = match (field, v.as_str()) {
                        ("cwd", Some(path)) => self.path_label(path),
                        ("processId", Some(id)) => short_id(id),
                        _ => compact(v, 0),
                    };
                    self.text(out, label, &format!("{field}={text}\n"))?;
                }
            }
            if let Some(ms) = params
                .get("timeoutMs")
                .or_else(|| params.get("timeout_ms"))
                .and_then(Value::as_u64)
            {
                self.text(out, label, &format!("timeout={:.3}s\n", ms as f64 / 1000.0))?;
            }
            self.finish(out, label)?;
            return Ok(true);
        }
        if v.get("result").is_some() || v.get("error").is_some() {
            let (method, elapsed) = self
                .requests
                .remove(&key)
                .map(|(m, t)| (m, format!("  {:.2}s", t.elapsed().as_secs_f64())))
                .unwrap_or_default();
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
            if let Some(pid) = result.pointer("/process/id").or_else(|| result.get("pid")) {
                meta += &format!("  process.id={}", compact(pid, 0));
            }
            if let Some(rss) = result
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
            self.summary(
                out,
                label,
                title,
                &format!("{method}{id_text}{elapsed}{meta}"),
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
                self.ui.theme.color("gray"),
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
            "Edit" => ("EDIT", "purple"),
            "Diff" => ("DIFF", "purple"),
            "Skill" => ("SKIL", "purple"),
            "Output" => ("SAVE", "gray"),
            "Success" | "Connected" => ("OK  ", "green"),
            "Error" | "Timeout" => ("ERR ", "red"),
            "Warning" => ("WARN", "yellow"),
            "Progress" => ("PROG", "yellow"),
            "stdout" => ("STDOUT", "purple"),
            "stderr" => ("STDERR", "yellow"),
            "stderr-error" => ("STDERR", "red"),
            "Tool" | "Command" | "Files" => ("OUT ", "purple"),
            "Usage" => ("META", "gray"),
            _ => ("RES ", "cyan"),
        };
        painted(
            out,
            self.color,
            self.ui.theme.color(color),
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
        let text = clean(&format!("{label}  {text}"));
        let available = self
            .width
            .saturating_sub(if self.ui.timestamps { 24 } else { 7 })
            .max(8);
        writeln!(out, "{}", activity::single_line(&text, available))
    }

    fn heading(&mut self, out: &mut impl Write, label: &str, title: &str) -> io::Result<()> {
        self.finish_visible(out)?;
        self.prefix(out, title)?;
        writeln!(out, "{}", clean(label))
    }

    fn block(
        &mut self,
        out: &mut impl Write,
        label: &str,
        title: &str,
        text: &str,
    ) -> io::Result<()> {
        self.heading(out, label, title)?;
        let stream = matches!(title, "stdout" | "stderr" | "stderr-error");
        let limit = if self.verbose || !stream {
            usize::MAX
        } else {
            self.ui.max_inline_output
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
            self.ui.theme.color("red")
        } else if title == "stderr" {
            self.ui.theme.color("yellow")
        } else {
            "0"
        };
        self.streams.entry(label.into()).or_default().tone = tone;
        for (i, line) in lines.iter().enumerate() {
            if i >= head && i < lines.len().saturating_sub(tail) && lines.len() > limit {
                if i == head {
                    self.text(
                        out,
                        label,
                        &format!(
                            "... {} lines folded; use --verbose or --raw-events\n",
                            lines.len() - shown
                        ),
                    )?;
                }
                continue;
            }
            self.text(out, label, &format!("{line}\n"))?;
        }
        self.finish(out, label)
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
            return self.block(out, label, title, text);
        }
        if let Some(array) = v.as_array() {
            for item in array
                .iter()
                .take(if self.verbose { usize::MAX } else { 12 })
            {
                self.value(out, label, item, title, depth + 1)?;
            }
            return Ok(());
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
                self.heading(
                    out,
                    &format!("{label} · {}", short_id(process)),
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
                self.ui.theme.color("yellow")
            } else {
                "0"
            };
            lines.summarize_json = !self.verbose;
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
                for (key, value) in object
                    .iter()
                    .take(if self.verbose { usize::MAX } else { 12 })
                {
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
                        self.block(out, label, title, &format!("{key}: {}", compact(value, 0)))?;
                    }
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
                )?;
            } else if lines.emitted > 0 {
                render_live(out, self.color, self.width, lines, line, true)?;
            } else {
                render_line(out, self.color, self.width, lines, line)?;
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
                render_line(out, self.color, self.width, lines, &line)?;
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
                )?;
            } else if !lines.pending.is_empty() {
                let line = std::mem::take(&mut lines.pending);
                if lines.emitted > 0 {
                    render_live(out, self.color, self.width, &mut lines, &line, true)?;
                } else {
                    render_line(out, self.color, self.width, &mut lines, &line)?;
                }
            }
        }
        out.flush()
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
    let Some(args) = command.as_array() else {
        return activity::single_line(command.as_str().unwrap_or("command"), 140);
    };
    let mut shown = Vec::new();
    let mut previous = "";
    for (index, value) in args.iter().enumerate() {
        let arg = value.as_str().unwrap_or("");
        if index == 0 {
            shown.push(arg.rsplit(['/', '\\']).next().unwrap_or(arg).to_owned());
        } else if matches!(
            previous,
            "-c" | "-e" | "--eval" | "-Command" | "-EncodedCommand" | "--command"
        ) && (arg.contains('\n') || arg.chars().count() > 100)
        {
            shown.push(format!(
                "[inline script · {} lines]",
                arg.lines().count().max(1)
            ));
        } else {
            let arg = activity::single_line(arg, 72);
            shown.push(if arg.chars().any(char::is_whitespace) {
                format!("'{arg}'")
            } else {
                arg
            });
        }
        previous = value.as_str().unwrap_or("");
    }
    activity::single_line(&shown.join(" "), 160)
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

fn json_outline(value: &Value) -> String {
    fn item(value: &Value) -> String {
        match value {
            Value::String(text) if text.contains('\n') || text.len() > 80 => {
                format!("text ({} lines)", text.lines().count().max(1))
            }
            Value::String(text) => activity::single_line(text, 80),
            Value::Array(items) => format!("{} items", items.len()),
            Value::Object(fields) => format!("{} fields", fields.len()),
            _ => value.to_string(),
        }
    }
    match value {
        Value::Object(fields) => {
            let mut detail = fields
                .iter()
                .take(4)
                .map(|(key, value)| format!("{}={}", activity::single_line(key, 32), item(value)))
                .collect::<Vec<_>>();
            if fields.len() > 4 {
                detail.push(format!("+{} fields", fields.len() - 4));
            }
            format!(
                "JSON output · {} fields · {}",
                fields.len(),
                detail.join(" · ")
            )
        }
        Value::Array(items) => format!("JSON output · {} items", items.len()),
        _ => "JSON output".into(),
    }
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
                renderer.ui.theme.color("blue"),
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
                renderer.ui.theme.color("gray"),
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
            lines.tone
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
) -> io::Result<()> {
    let outline = if lines.summarize_json && json_record(line) {
        serde_json::from_str::<Value>(line)
            .ok()
            .filter(|value| value.is_object() || value.is_array())
            .map(|value| json_outline(&value))
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
        "1"
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
        "36"
    } else if lines.hunk && line.starts_with('+') {
        prefix = format!("      {:>5} │ ", lines.new);
        lines.new += 1;
        "32"
    } else if lines.hunk && line.starts_with('-') {
        prefix = format!("{:>5}       │ ", lines.old);
        lines.old += 1;
        "31"
    } else if lines.hunk && line.starts_with(' ') {
        prefix = format!("{:>5} {:>5} │ ", lines.old, lines.new);
        lines.old += 1;
        lines.new += 1;
        "90"
    } else if failure {
        prefix = "[ERR ] ".into();
        "31;1"
    } else if progress {
        prefix = "[PROG] ".into();
        "33;1"
    } else if outline.is_none() && (lower.starts_with("warning") || lower.contains("retrying")) {
        prefix = "[WARN] ".into();
        "33;1"
    } else {
        if lines.tone.is_empty() {
            "0"
        } else {
            lines.tone
        }
    };
    // Keep diff/source columns intact. Prose wraps without dropping characters.
    let text = format!("{prefix}{line}");
    if lines.hunk || code == "1" {
        return painted(out, color, code, &format!("{text}\n"));
    }
    let mut column = 0;
    let mut wrapped = String::new();
    for c in text.chars() {
        let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
        if column + cells > width.saturating_sub(2) {
            wrapped.push_str("\n  ");
            column = 0;
        }
        wrapped.push(c);
        column += cells;
    }
    painted(out, color, code, &format!("  {wrapped}\n"))
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

    #[test]
    fn raw_keeps_nonsecret_notification_shape() {
        let event = json!({"method":"command/exec/outputDelta","params":{"deltaBase64":"aGkK"},"unknown":{"x":"a\\nb"}});
        assert_eq!(
            render(std::slice::from_ref(&event), true, true),
            format!("{event}\n")
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
        assert!(whole.contains("\x1b[36m@@"));
        assert!(whole.contains("\x1b[31m    2       │ -old"));
        assert!(whole.contains("\x1b[32m          2 │ +new"));
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
        let text = render(
            &[
                json!({"type":"ssmcp.request","request":{"id":"r1","method":"command/exec","params":{"command":["sh","-c","printf hello"],"cwd":"/project","timeoutMs":1000}}}),
                json!({"id":"r1","result":{"exitCode":0,"stdout":"hello\nsecond\n","stderr":"warning\n"}}),
            ],
            false,
            true,
        );
        for expected in [
            "command/exec id=r1",
            "cmd=sh -c",
            "cwd=/project",
            "timeout=1.000s",
            "\x1b[34;1m",
            "\x1b[32;1m",
        ] {
            assert!(
                text.contains(expected) || clean(&text).contains(expected),
                "missing {expected}: {text}"
            );
        }
        assert!(!text.contains("hello\n  second"));
        assert!(!text.contains("warning"));
        assert!(!text.contains("[STDOUT]"));
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
        assert!(text.contains("\x1b[31;1m"));
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
        assert!(whole.contains("\x1b[31m    2       │ -old"));
        assert!(whole.contains("\x1b[32m          2 │ +new"));
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
        for expected in ["[PROG]", "[WARN]", "[ERR ]", "\x1b[31;1m"] {
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
        assert!(text.contains("38;5;167"));
        assert!(clean(&text).contains("Z ["));
        let mut r = Renderer::new(true, true, 100);
        r.ui.timestamps = true;
        let mut out = Vec::new();
        let event = json!({"type":"ssmcp.status","level":"Error","message":"x\ny"});
        r.event(&mut out, "codex", &event).unwrap();
        assert_eq!(out, format!("{event}\n").as_bytes());
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
    fn pretty_bounds_inline_scripts_and_long_ids_without_changing_jsonl() {
        let script = "script_statement_not_for_pretty();\n".repeat(200);
        let id = format!("request-{}-17", "a".repeat(128));
        let events = [
            json!({"type":"exec.started","job_id":"81","command":["python3","-c",script],"cwd":"/work"}),
            json!({"id":id,"method":"command/exec","params":{"command":["python3","-c",script]}}),
        ];
        let pretty = render(&events, false, false);
        assert!(pretty.contains("python3 -c [inline script"), "{pretty}");
        assert!(pretty.contains("200 lines"), "{pretty}");
        assert!(!pretty.contains("script_statement_not_for_pretty"));
        assert!(!pretty.contains(&"a".repeat(64)));
        assert!(pretty.len() < 600, "{pretty}");
        let raw = render(&events, true, false);
        assert!(raw.contains("script_statement_not_for_pretty") && raw.contains(&id));
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
            text.contains("JSON output") && text.contains("exit_code=0"),
            "{text}"
        );
        assert!(!text.contains("{\"") && !text.contains("\\n"), "{text}");
        assert!(!text.contains("[OK  ]"));
        assert_eq!(text.matches("[ERR ]").count(), 1, "{text}");
        assert!(text.contains("exit=9"), "{text}");
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
    }
}
