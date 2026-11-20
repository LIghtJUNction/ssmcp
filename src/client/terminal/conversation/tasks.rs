//! A shared cache of native command jobs with independent, lightweight view positions.
use crate::{
    client::{interface::harness, terminal, ui::Ui},
    runtime::Runtime,
    wire::Operation,
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TaskPosition {
    selected_id: Option<String>,
    detail: bool,
    list_scroll: usize,
    detail_scroll: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Job {
    id: String,
    data: Value,
}

impl Job {
    fn parse(value: &Value) -> Result<Self, String> {
        let id = value["job_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Task snapshot is missing a job_id")?;
        if value["command"]
            .as_array()
            .is_none_or(|args| args.iter().any(|argument| !argument.is_string()))
            || !value["cwd"].is_string()
            || !value["state"].is_string()
            || !value["running"].is_boolean()
            || !value["duration_ms"].is_u64()
        {
            return Err(format!("Incomplete native task snapshot for {id}"));
        }
        Ok(Self {
            id: id.into(),
            data: value.clone(),
        })
    }

    fn command(&self) -> String {
        self.data["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| {
                let arg = arg.as_str().unwrap();
                if !arg.is_empty()
                    && arg
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || "-_/.:@+=,%".contains(ch))
                {
                    arg.to_owned()
                } else {
                    serde_json::to_string(arg).unwrap()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn state(&self) -> &str {
        self.data["state"].as_str().unwrap()
    }

    fn cwd(&self) -> &str {
        self.data["cwd"].as_str().unwrap()
    }

    fn duration(&self) -> String {
        let millis = self.data["duration_ms"].as_u64().unwrap();
        format!("{}.{:03}s", millis / 1000, millis % 1000)
    }

    fn exit(&self) -> String {
        if let Some(code) = self.data["exit_code"].as_i64() {
            code.to_string()
        } else if let Some(signal) = self.data["exit_signal"].as_i64() {
            format!("signal {signal}")
        } else {
            "—".into()
        }
    }
}

#[derive(Debug)]
struct RefreshResult {
    list: Result<Value, String>,
    detail: Option<(String, Result<Value, String>)>,
}

struct Pending {
    receiver: mpsc::Receiver<RefreshResult>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Only the read worker/timer belongs to this panel. Running jobs do not.
        self.task.abort();
    }
}

#[derive(Default)]
pub(crate) struct TaskPanel {
    jobs: Vec<Job>,
    position: TaskPosition,
    pending: Option<Pending>,
    loading: bool,
    loaded: bool,
    preview: bool,
    next_refresh: Option<Instant>,
    error: Option<String>,
    detail_error: Option<(String, String)>,
    mouse_rows: Vec<Option<String>>,
    list_capacity: usize,
}

impl TaskPanel {
    /// Start at most one read. The visible panel can call this on every frame.
    pub(crate) fn refresh(&mut self, preview: bool) -> Result<(), String> {
        if preview {
            self.pending = None;
            self.loading = false;
            self.next_refresh = None;
            if !self.preview || !self.loaded {
                self.preview = true;
                self.jobs.clear();
                self.position = TaskPosition::default();
                self.ingest(&preview_fixture())?;
            }
            return Ok(());
        }
        if self.preview {
            self.preview = false;
            self.loaded = false;
            self.jobs.clear();
            self.position = TaskPosition::default();
        }
        if self.loading || self.next_refresh.is_some_and(|at| Instant::now() < at) {
            return Ok(());
        }
        self.pending = None;
        self.next_refresh = Some(Instant::now() + REFRESH_INTERVAL);
        let (runtime, handle) = match harness::local_runtime() {
            Ok(binding) => binding,
            Err(error) => {
                self.error = Some(error.clone());
                return Err(error);
            }
        };
        let selected = self
            .position
            .detail
            .then(|| self.position.selected_id.clone())
            .flatten();
        let (sender, receiver) = mpsc::channel();
        let task = handle.spawn(async move {
            let list = read(&runtime, "ssmcp.exec.list", json!({})).await;
            let detail = if let Some(id) = selected.filter(|id| {
                list.as_ref()
                    .ok()
                    .and_then(|value| native_data(value).get("jobs"))
                    .and_then(Value::as_array)
                    .is_some_and(|jobs| {
                        jobs.iter()
                            .any(|job| job["job_id"].as_str() == Some(id.as_str()))
                    })
            }) {
                let result = read(&runtime, "ssmcp.exec.status", json!({"job_id": id})).await;
                Some((id, result))
            } else {
                None
            };
            if sender.send(RefreshResult { list, detail }).is_ok() {
                terminal::invalidate();
                // One wake schedules the next visible refresh; hidden columns stop here.
                tokio::time::sleep(REFRESH_INTERVAL).await;
                terminal::invalidate();
            }
        });
        self.pending = Some(Pending { receiver, task });
        self.loading = true;
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        let result = match pending.receiver.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                if !self.loading {
                    return false;
                }
                let visible_loading = self.loading && !self.loaded && self.error.is_none();
                self.loading = false;
                let error = "Task refresh ended before returning a snapshot";
                let changed = self.error.as_deref() != Some(error);
                self.error = Some(error.into());
                if changed {
                    self.mouse_rows.clear();
                }
                return visible_loading || changed;
            }
        };
        let visible_loading = self.loading && !self.loaded && self.error.is_none();
        self.loading = false;
        // Keep the deadline set when the read started. Moving it here could put
        // it after the worker's only wake and leave an idle panel stale.
        self.apply_refresh(result) || visible_loading
    }

    fn apply_refresh(&mut self, result: RefreshResult) -> bool {
        let old_mouse_rows = std::mem::take(&mut self.mouse_rows);
        let old_jobs = self.jobs.clone();
        let old_loaded = self.loaded;
        let old_error = self.error.clone();
        let old_detail_error = self.detail_error.clone();
        if let Err(error) = result.list.and_then(|value| self.ingest(&value)) {
            self.error = Some(error);
        }
        if let Some((id, result)) = result.detail {
            match result.and_then(|value| Job::parse(native_data(&value))) {
                Ok(job) if job.id == id => {
                    if let Some(old) = self.jobs.iter_mut().find(|old| old.id == id) {
                        *old = job;
                    }
                    if self
                        .detail_error
                        .as_ref()
                        .is_some_and(|(old, _)| old == &id)
                    {
                        self.detail_error = None;
                    }
                }
                Ok(_) => {
                    self.detail_error = Some((id, "Task status returned a different job ID".into()))
                }
                Err(error) => self.detail_error = Some((id, error)),
            }
        }
        let changed = self.jobs != old_jobs
            || self.loaded != old_loaded
            || self.error != old_error
            || self.detail_error != old_detail_error;
        if changed {
            self.mouse_rows.clear();
        } else {
            self.mouse_rows = old_mouse_rows;
        }
        changed
    }

    /// Accept the actual exec.list shape, directly or inside its MCP result envelope.
    /// This also allows deterministic snapshots without a runtime or process.
    pub(crate) fn ingest(&mut self, value: &Value) -> Result<(), String> {
        let result: Result<Vec<Job>, String> = (|| {
            check_result(value)?;
            let values = native_data(value)["jobs"]
                .as_array()
                .ok_or("Task list did not return a jobs array")?;
            let mut jobs = values
                .iter()
                .map(Job::parse)
                .collect::<Result<Vec<_>, _>>()?;
            jobs.sort_by(|a, b| {
                b.data["running"]
                    .as_bool()
                    .cmp(&a.data["running"].as_bool())
                    .then_with(|| a.id.cmp(&b.id))
            });
            if jobs
                .iter()
                .map(|job| &job.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != jobs.len()
            {
                return Err("Task list contains duplicate job IDs".into());
            }
            Ok(jobs)
        })();
        match result {
            Ok(jobs) => {
                let changed = self.jobs != jobs || !self.loaded || self.error.is_some();
                let anchor = self.jobs.get(self.position.list_scroll).map(|job| &job.id);
                if let Some(index) = anchor.and_then(|id| jobs.iter().position(|job| &job.id == id))
                {
                    self.position.list_scroll = index;
                }
                self.jobs = jobs;
                self.loaded = true;
                self.error = None;
                if self.position.selected_id.is_none() {
                    self.position.selected_id = self.jobs.first().map(|job| job.id.clone());
                }
                if changed {
                    self.mouse_rows.clear();
                }
                Ok(())
            }
            Err(error) => {
                if self.error.as_ref() != Some(&error) {
                    self.mouse_rows.clear();
                }
                self.error = Some(error.clone());
                Err(error)
            }
        }
    }

    pub(crate) fn position(&self) -> TaskPosition {
        self.position.clone()
    }

    pub(crate) fn restore_position(&mut self, position: &TaskPosition) {
        self.position = position.clone();
        if self.position.selected_id.is_none() {
            self.position.selected_id = self.jobs.first().map(|job| job.id.clone());
        }
        self.mouse_rows.clear();
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.position.detail {
            self.scroll_by(delta);
            return;
        }
        if self.jobs.is_empty() || delta == 0 {
            return;
        }
        let index = match self.selected_index() {
            Some(index) => index.saturating_add_signed(delta).min(self.jobs.len() - 1),
            None if delta < 0 => self.jobs.len() - 1,
            None => 0,
        };
        self.position.selected_id = Some(self.jobs[index].id.clone());
        self.position.detail_scroll = 0;
        let capacity = self.list_capacity.max(1);
        if index < self.position.list_scroll {
            self.position.list_scroll = index;
        } else if index >= self.position.list_scroll.saturating_add(capacity) {
            self.position.list_scroll = index.saturating_sub(capacity - 1);
        }
        self.mouse_rows.clear();
    }

    pub(crate) fn scroll_by(&mut self, delta: isize) {
        if self.position.detail {
            self.position.detail_scroll = self.position.detail_scroll.saturating_add_signed(delta);
        } else {
            self.position.list_scroll = self
                .position
                .list_scroll
                .saturating_add_signed(delta)
                .min(self.jobs.len().saturating_sub(1));
        }
        self.mouse_rows.clear();
    }

    pub(crate) fn open(&mut self) -> bool {
        if self.selected_index().is_none() {
            return false;
        }
        if !self.position.detail {
            self.position.detail_scroll = 0;
        }
        self.position.detail = true;
        self.next_refresh = None;
        self.mouse_rows.clear();
        true
    }

    pub(crate) fn back(&mut self) -> bool {
        if !self.position.detail {
            return false;
        }
        self.position.detail = false;
        self.mouse_rows.clear();
        true
    }

    pub(crate) fn mouse_select(&mut self, row: u16) -> bool {
        if self.position.detail {
            return false;
        }
        let Some(Some(id)) = self.mouse_rows.get(usize::from(row)) else {
            return false;
        };
        self.position.selected_id = Some(id.clone());
        self.position.detail_scroll = 0;
        true
    }

    fn selected_index(&self) -> Option<usize> {
        let id = self.position.selected_id.as_ref()?;
        self.jobs.iter().position(|job| &job.id == id)
    }

    pub(crate) fn snapshot(&self) -> Value {
        json!({
            "source": if self.preview { "preview_fixture" } else { "native" },
            "jobs": self.jobs.iter().map(|job| &job.data).collect::<Vec<_>>(),
            "selected_id": self.position.selected_id,
            "selected_job": self.selected_index().map(|index| &self.jobs[index].data),
            "detail": self.position.detail,
            "list_scroll": self.position.list_scroll,
            "detail_scroll": self.position.detail_scroll,
            "loading": self.loading,
            "loaded": self.loaded,
            "error": self.error,
            "detail_error": self.detail_error.as_ref().filter(|(id, _)| Some(id) == self.position.selected_id.as_ref()).map(|(_, error)| error),
        })
    }

    pub(crate) fn render(&mut self, width: u16, height: u16, ui: &Ui) -> Vec<String> {
        let width = usize::from(width);
        let height = usize::from(height);
        self.mouse_rows.clear();
        if height == 0 {
            return Vec::new();
        }
        let mut rows: Vec<(String, &str, Option<String>)> = Vec::new();
        let title = if self.preview {
            " Tasks · Preview · fictional task data".into()
        } else {
            format!(
                " Tasks · {} jobs{}",
                self.jobs.len(),
                if self.loading && !self.loaded && self.error.is_none() {
                    " · loading…"
                } else {
                    ""
                }
            )
        };
        rows.push((title, "heading", None));
        rows.push((
            if self.position.detail {
                " Esc list · ↑/↓ scroll · /tasks cancel JOB_ID"
            } else {
                " ↑/↓ select · Enter details · PgUp/PgDn scroll"
            }
            .into(),
            "muted",
            None,
        ));
        if let Some(error) = &self.error {
            rows.push((format!(" Refresh failed: {error}"), "error", None));
        }
        if self.position.detail {
            let indent = if width > 1 { " " } else { "" };
            let detail = self.detail_lines(width.saturating_sub(indent.len()).max(1));
            let capacity = height.saturating_sub(rows.len());
            self.position.detail_scroll = self
                .position
                .detail_scroll
                .min(detail.len().saturating_sub(capacity.max(1)));
            for text in detail
                .into_iter()
                .skip(self.position.detail_scroll)
                .take(capacity)
            {
                rows.push((format!("{indent}{text}"), "foreground", None));
            }
        } else {
            if let Some(id) = &self.position.selected_id
                && self.selected_index().is_none()
            {
                rows.push((
                    format!(" Selected job {id} is no longer retained"),
                    "warning",
                    None,
                ));
            }
            if self.jobs.is_empty() {
                rows.push((
                    if self.loaded {
                        " No local command jobs."
                    } else if self.loading && self.error.is_none() {
                        " Loading local command jobs…"
                    } else {
                        " No task snapshot is available."
                    }
                    .into(),
                    "muted",
                    None,
                ));
            } else {
                let available = height.saturating_sub(rows.len());
                let per_job = if available >= 3 { 3 } else { 1 };
                self.list_capacity = (available / per_job).max(1);
                self.position.list_scroll = self
                    .position
                    .list_scroll
                    .min(self.jobs.len().saturating_sub(self.list_capacity));
                for job in self
                    .jobs
                    .iter()
                    .skip(self.position.list_scroll)
                    .take(self.list_capacity)
                {
                    let selected = self.position.selected_id.as_deref() == Some(job.id.as_str());
                    let tone = if selected { "accent" } else { "foreground" };
                    rows.push((
                        format!(
                            "{} {} · {} · exit {} · {}",
                            if selected { "›" } else { " " },
                            job.id,
                            job.state(),
                            job.exit(),
                            job.duration()
                        ),
                        tone,
                        Some(job.id.clone()),
                    ));
                    if per_job == 3 {
                        rows.push((format!("  {}", job.command()), tone, Some(job.id.clone())));
                        rows.push((
                            format!("  cwd: {}", job.cwd()),
                            "muted",
                            Some(job.id.clone()),
                        ));
                    }
                }
            }
        }
        rows.truncate(height);
        self.mouse_rows = rows.iter().map(|(_, _, id)| id.clone()).collect();
        let mut rendered = rows
            .into_iter()
            .map(|(text, tone, _)| row(&text, tone, width, ui))
            .collect::<Vec<_>>();
        rendered.resize_with(height, || row("", "foreground", width, ui));
        rendered
    }

    fn detail_lines(&self, width: usize) -> Vec<String> {
        let Some(index) = self.selected_index() else {
            return wrap(
                &format!(
                    "Job {} is no longer retained. Press Esc to return to the task list.",
                    self.position.selected_id.as_deref().unwrap_or("—")
                ),
                width,
            );
        };
        let job = &self.jobs[index];
        let mut lines = vec![
            format!("Job: {}", job.id),
            format!("State: {}", job.state()),
            format!("Exit: {}", job.exit()),
            format!("Duration: {}", job.duration()),
            format!("Command: {}", job.command()),
            format!("cwd: {}", job.cwd()),
        ];
        if let Some((id, error)) = &self.detail_error
            && id == &job.id
        {
            lines.push(format!("Status refresh failed: {error}"));
        }
        if let Some(error) = job.data["error"].as_str() {
            lines.push(format!("Error: {error}"));
        }
        for (key, label) in [
            ("stdin", "Stdin"),
            ("stdout", "Stdout"),
            ("stderr", "Stderr"),
        ] {
            if let Some(metadata) = job.data.get(key) {
                lines.push(format!("{label} metadata: {metadata}"));
            }
        }
        lines.push(format!("Explicit cancellation: /tasks cancel {}", job.id));
        lines
            .into_iter()
            .flat_map(|line| wrap(&line, width))
            .collect()
    }
}

async fn read(runtime: &Arc<Runtime>, name: &str, arguments: Value) -> Result<Value, String> {
    let value = tokio::time::timeout(
        READ_TIMEOUT,
        runtime.perform(Operation::CallTool {
            name: name.into(),
            arguments,
        }),
    )
    .await
    .map_err(|_| format!("{name} timed out while reading task metadata"))??;
    check_result(&value)?;
    Ok(value)
}

fn native_data(value: &Value) -> &Value {
    value.get("structuredContent").unwrap_or(value)
}

fn check_result(value: &Value) -> Result<(), String> {
    if value["isError"] != true {
        return Ok(());
    }
    let error = value["content"]
        .as_array()
        .map(|content| {
            content
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| "Native task read failed".into());
    Err(error)
}

fn safe(text: &str) -> String {
    text.chars()
        .flat_map(|ch| match ch {
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect(),
            '\t' => "    ".chars().collect(),
            _ if ch.is_control() => vec!['�'],
            _ => vec![ch],
        })
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for mut ch in safe(text).chars() {
        let mut cells = ch.width().unwrap_or(0);
        if cells > width {
            ch = '�';
            cells = 1;
        }
        if used + cells > width {
            lines.push(std::mem::take(&mut line));
            used = 0;
        }
        line.push(ch);
        used += cells;
    }
    lines.push(line);
    lines
}

fn row(text: &str, tone: &str, width: usize, ui: &Ui) -> String {
    if width == 0 {
        return String::new();
    }
    let mut clipped = String::new();
    let mut used = 0;
    for ch in safe(text).chars() {
        let cells = ch.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        clipped.push(ch);
        used += cells;
    }
    clipped.push_str(&" ".repeat(width.saturating_sub(clipped.width())));
    if ui.color {
        format!(
            "\x1b[{};{}m{}\x1b[0m",
            ui.background(),
            ui.color(tone),
            clipped
        )
    } else {
        clipped
    }
}

fn preview_fixture() -> Value {
    // Explicitly fictional. No process is started and no runtime is consulted.
    json!({"jobs":[
        {"job_id":"preview-task-build","id":"preview-task-build","command":["cargo","check","--workspace"],
         "cwd":"/preview/fictional-project","state":"running","running":true,
         "exit_code":null,"exit_signal":null,"duration_ms":1250,"error":null,
         "stdin":{"mode":"null","open":false,"bytes_written":0},
         "stdout":{"start_cursor":0,"next_cursor":64,"truncated":false,"eof":false,"artifact":null,"error":null},
         "stderr":{"start_cursor":0,"next_cursor":0,"truncated":false,"eof":false,"artifact":null,"error":null}},
        {"job_id":"preview-task-test","id":"preview-task-test","command":["cargo","test","--lib"],
         "cwd":"/preview/fictional-project","state":"completed","running":false,
         "exit_code":0,"exit_signal":null,"duration_ms":3200,"error":null,
         "stdin":{"mode":"null","open":false,"bytes_written":0},
         "stdout":{"start_cursor":0,"next_cursor":128,"truncated":false,"eof":true,"artifact":null,"error":null},
         "stderr":{"start_cursor":0,"next_cursor":0,"truncated":false,"eof":true,"artifact":null,"error":null}}
    ],"max_running":32,"max_retained":128})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, running: bool) -> Value {
        json!({"job_id":id,"id":id,"command":["printf","中文 with space"],
            "cwd":"/tmp/tasks","state":if running {"running"} else {"completed"},
            "running":running,"exit_code":if running {None} else {Some(0)},
            "exit_signal":null,"duration_ms":1234,"error":null,
            "stdin":{"mode":"null","open":false,"bytes_written":0},
            "stdout":{"start_cursor":0,"next_cursor":0,"truncated":false,"eof":!running,"artifact":null,"error":null},
            "stderr":{"start_cursor":0,"next_cursor":0,"truncated":false,"eof":!running,"artifact":null,"error":null}})
    }

    fn ui() -> Ui {
        Ui {
            color: false,
            ..Ui::default()
        }
    }

    #[test]
    fn background_refresh_keeps_unchanged_rows_and_mouse_targets() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let mut panel = TaskPanel::default();
        let snapshot = json!({"jobs":[job("a", false), job("b", false)]});
        panel.ingest(&snapshot).unwrap();
        let rows = panel.render(80, 10, &ui());
        let mouse_rows = panel.mouse_rows.clone();
        let position = panel.position();
        let (sender, receiver) = mpsc::channel();
        panel.pending = Some(Pending {
            receiver,
            task: runtime.spawn(std::future::pending::<()>()),
        });
        panel.loading = true;
        assert_eq!(panel.render(80, 10, &ui()), rows);
        sender
            .send(RefreshResult {
                list: Ok(snapshot),
                detail: None,
            })
            .unwrap();
        drop(sender);
        assert!(!panel.poll());
        assert!(!panel.loading);
        assert_eq!(panel.position(), position);
        assert_eq!(panel.mouse_rows, mouse_rows);
        assert!(
            panel.mouse_select(5),
            "unchanged refresh must keep live click targets"
        );
        assert_eq!(panel.snapshot()["selected_id"], "b");
    }

    #[test]
    fn background_refresh_repeated_errors_do_not_flash_loading() {
        let mut panel = TaskPanel::default();
        assert!(panel.apply_refresh(RefreshResult {
            list: Err("local task source is unavailable".into()),
            detail: None,
        }));
        let rows = panel.render(80, 10, &ui());
        panel.loading = true;
        assert_eq!(panel.render(80, 10, &ui()), rows);
        assert!(!panel.apply_refresh(RefreshResult {
            list: Err("local task source is unavailable".into()),
            detail: None,
        }));
        panel.loading = false;
        assert_eq!(panel.render(80, 10, &ui()), rows);
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[]})),
            detail: None,
        }));
        assert!(panel.loaded);
        assert!(panel.error.is_none());
        assert_ne!(panel.render(80, 10, &ui()), rows);
    }

    #[test]
    fn background_refresh_reports_real_duration_state_and_output_changes() {
        let mut panel = TaskPanel::default();
        let mut current = job("a", true);
        panel.ingest(&json!({"jobs":[current.clone()]})).unwrap();
        current["duration_ms"] = json!(2000);
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[current.clone()]})),
            detail: None,
        }));
        assert!(panel.render(80, 10, &ui()).join("\n").contains("2.000s"));
        assert!(!panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[current.clone()]})),
            detail: None,
        }));
        let mut detail = current.clone();
        detail["stdout"]["next_cursor"] = json!(42);
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[current]})),
            detail: Some(("a".into(), Ok(detail.clone()))),
        }));
        assert_eq!(
            panel.snapshot()["selected_job"]["stdout"]["next_cursor"],
            42
        );
        assert!(!panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[detail.clone()]})),
            detail: Some(("a".into(), Ok(detail))),
        }));
        let mut completed = job("a", false);
        completed["exit_code"] = json!(7);
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[completed]})),
            detail: None,
        }));
        let rows = panel.render(80, 10, &ui()).join("\n");
        assert!(rows.contains("completed") && rows.contains("exit 7"));
    }

    #[test]
    fn background_refresh_preserves_visible_anchor_and_selection_when_jobs_reorder() {
        let mut panel = TaskPanel::default();
        let jobs: Vec<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|id| job(id, false))
            .collect();
        panel.ingest(&json!({"jobs":jobs})).unwrap();
        panel.position.list_scroll = 2;
        panel.position.selected_id = Some("d".into());
        panel.position.detail_scroll = 7;
        panel.render(80, 8, &ui());
        assert_eq!(panel.mouse_rows[2].as_deref(), Some("c"));
        let jobs: Vec<_> = ["a", "b", "c", "d", "e", "f", "g", "h"]
            .into_iter()
            .map(|id| job(id, id == "h"))
            .collect();
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":jobs})),
            detail: None,
        }));
        panel.render(80, 8, &ui());
        assert_eq!(panel.mouse_rows[2].as_deref(), Some("c"));
        assert_eq!(panel.position.list_scroll, 3);
        assert_eq!(panel.position.selected_id.as_deref(), Some("d"));
        assert_eq!(panel.position.detail_scroll, 7);
    }

    #[test]
    fn background_refresh_equal_final_detail_preserves_mouse_targets() {
        let mut panel = TaskPanel::default();
        let listed = job("a", true);
        let mut detailed = listed.clone();
        detailed["stdout"]["next_cursor"] = json!(42);
        assert!(panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[listed.clone()]})),
            detail: Some(("a".into(), Ok(detailed.clone()))),
        }));
        panel.render(80, 10, &ui());
        let mouse_rows = panel.mouse_rows.clone();
        assert!(!panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[listed]})),
            detail: Some(("a".into(), Ok(detailed))),
        }));
        assert_eq!(panel.mouse_rows, mouse_rows);
        assert!(panel.mouse_select(2));
    }

    #[test]
    fn native_snapshots_keep_real_fields_and_selection_across_reordering() {
        let mut panel = TaskPanel::default();
        panel
            .ingest(&json!({"structuredContent":{"jobs":[job("a", true), job("b", false)]}}))
            .unwrap();
        panel.move_selection(1);
        panel
            .ingest(&json!({"jobs":[job("a", false), job("b", true)]}))
            .unwrap();
        let snapshot = panel.snapshot();
        assert_eq!(snapshot["selected_id"], "b");
        assert_eq!(snapshot["jobs"][0]["job_id"], "b");
        assert_eq!(snapshot["selected_job"]["duration_ms"], 1234);
        assert_eq!(snapshot["selected_job"]["cwd"], "/tmp/tasks");
        assert_eq!(snapshot["selected_job"]["command"][1], "中文 with space");
    }

    #[test]
    fn a_retired_selected_job_does_not_retarget_details() {
        let mut panel = TaskPanel::default();
        panel
            .ingest(&json!({"jobs":[job("a", true), job("b", false)]}))
            .unwrap();
        assert!(panel.open());
        panel.ingest(&json!({"jobs":[job("b", false)]})).unwrap();
        assert_eq!(panel.snapshot()["selected_id"], "a");
        assert!(panel.snapshot()["selected_job"].is_null());
        assert!(
            panel
                .render(80, 12, &ui())
                .join("\n")
                .contains("no longer retained")
        );
        assert!(panel.back());
        assert!(!panel.open());
        panel.move_selection(1); // Explicit navigation can choose another task.
        assert_eq!(panel.snapshot()["selected_id"], "b");
    }

    #[test]
    fn positions_restore_views_without_copying_or_rolling_back_task_data() {
        let mut panel = TaskPanel::default();
        panel
            .ingest(&json!({"jobs":[job("a", true), job("b", false)]}))
            .unwrap();
        panel.open();
        panel.scroll_by(2);
        let a = panel.position();
        panel.back();
        panel.move_selection(1);
        let b = panel.position();
        let mut updated = job("a", false);
        updated["duration_ms"] = json!(9999);
        panel
            .ingest(&json!({"jobs":[updated, job("b", false)]}))
            .unwrap();
        panel.restore_position(&a);
        assert_eq!(panel.snapshot()["selected_id"], "a");
        assert_eq!(panel.snapshot()["detail_scroll"], 2);
        assert_eq!(panel.snapshot()["selected_job"]["duration_ms"], 9999);
        panel.restore_position(&b);
        assert_eq!(panel.snapshot()["selected_id"], "b");
        assert_eq!(panel.snapshot()["detail"], false);
    }

    #[test]
    fn invalid_snapshots_keep_cached_jobs_and_distinguish_errors_from_empty() {
        let mut panel = TaskPanel::default();
        panel.ingest(&json!({"jobs":[job("a", true)]})).unwrap();
        assert!(panel.ingest(&json!({"unexpected":[]})).is_err());
        assert_eq!(panel.snapshot()["jobs"].as_array().unwrap().len(), 1);
        assert!(
            panel
                .render(80, 8, &ui())
                .join("\n")
                .contains("Refresh failed")
        );
        panel.ingest(&json!({"jobs":[]})).unwrap();
        assert!(panel.snapshot()["error"].is_null());
        assert!(
            panel
                .render(80, 8, &ui())
                .join("\n")
                .contains("No local command jobs.")
        );
    }

    #[test]
    fn detail_status_updates_its_captured_id_after_selection_changes() {
        let mut panel = TaskPanel::default();
        panel
            .ingest(&json!({"jobs":[job("a", true), job("b", false)]}))
            .unwrap();
        panel.move_selection(1);
        let mut a = job("a", false);
        a["exit_code"] = json!(7);
        panel.apply_refresh(RefreshResult {
            list: Ok(json!({"jobs":[job("a", true), job("b", false)]})),
            detail: Some(("a".into(), Ok(json!({"structuredContent":a})))),
        });
        assert_eq!(panel.snapshot()["selected_id"], "b");
        assert_eq!(panel.snapshot()["jobs"][0]["exit_code"], 7);
    }

    #[test]
    fn preview_is_explicit_offline_and_supports_selection_details_and_mouse() {
        let mut panel = TaskPanel::default();
        panel.refresh(true).unwrap();
        assert!(panel.pending.is_none());
        assert!(!panel.poll());
        let rows = panel.render(90, 12, &ui());
        assert!(rows[0].contains("fictional task data"));
        assert_eq!(panel.snapshot()["source"], "preview_fixture");
        assert_eq!(panel.snapshot()["selected_id"], "preview-task-build");
        assert!(!panel.mouse_select(0));
        assert!(panel.mouse_select(5));
        assert_eq!(panel.snapshot()["selected_id"], "preview-task-test");
        assert!(panel.open());
        panel.move_selection(2);
        assert_eq!(panel.snapshot()["detail_scroll"], 2);
        assert!(panel.back());
        panel.refresh(true).unwrap();
        assert_eq!(panel.snapshot()["selected_id"], "preview-task-test");
    }

    #[test]
    fn pending_reads_are_deduplicated_and_poll_updates_without_waiting() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let deadline = Instant::now() + REFRESH_INTERVAL;
        let mut panel = TaskPanel {
            next_refresh: Some(deadline),
            pending: Some(Pending {
                receiver,
                task: runtime.spawn(std::future::pending::<()>()),
            }),
            loading: true,
            ..TaskPanel::default()
        };
        // Reuse the pending read without consulting the shared harness binding.
        panel.refresh(false).unwrap();
        assert!(!panel.poll());
        sender
            .send(RefreshResult {
                list: Ok(json!({"jobs":[job("a", true)]})),
                detail: None,
            })
            .unwrap();
        drop(sender);
        assert!(panel.poll());
        assert!(!panel.loading);
        assert_eq!(panel.next_refresh, Some(deadline));
        assert_eq!(panel.snapshot()["selected_id"], "a");
        assert!(!panel.poll());
        assert!(panel.snapshot()["error"].is_null());
    }

    #[tokio::test]
    async fn native_exec_snapshots_refresh_and_render_real_jobs() {
        use crate::{backend::shell::Shell, output::OutputStore};
        use std::{
            path::PathBuf,
            time::{SystemTime, UNIX_EPOCH},
        };

        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        // Exercise the real completion channel and parser with native manager
        // results. A Backend/Runtime constructor would also discover providers.
        async fn refresh_native(panel: &mut TaskPanel, shell: &Arc<Shell>) -> bool {
            let shell = shell.clone();
            let selected = panel
                .position
                .detail
                .then(|| panel.position.selected_id.clone())
                .flatten();
            let (sender, receiver) = mpsc::channel();
            let task = tokio::spawn(async move {
                let list = shell
                    .call("ssmcp.exec.list", json!({}))
                    .await
                    .map(crate::backend::result);
                let detail = if let Some(id) = selected {
                    let result = shell
                        .call("ssmcp.exec.status", json!({"job_id":id}))
                        .await
                        .map(crate::backend::result);
                    Some((id, result))
                } else {
                    None
                };
                sender.send(RefreshResult { list, detail }).unwrap();
            });
            panel.pending = Some(Pending { receiver, task });
            panel.loading = true;
            tokio::time::timeout(
                Duration::from_secs(5),
                &mut panel.pending.as_mut().unwrap().task,
            )
            .await
            .expect("native metadata refresh timed out")
            .expect("native metadata worker failed");
            let changed = panel.poll();
            assert!(!panel.loading);
            assert!(panel.snapshot()["error"].is_null());
            assert!(panel.snapshot()["detail_error"].is_null());
            changed
        }

        let root = Root(std::env::temp_dir().join(format!(
            "ssmcp-task-panel-native-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        )));
        let workspace = root.0.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let output = OutputStore::at_for_test(root.0.join("output")).unwrap();
        let shell = Shell::with_workspace(workspace.clone(), output).unwrap();
        let command = json!([
            std::env::current_exe().unwrap(),
            "--exact",
            "backend::shell::tests::fixture_process",
            "--nocapture"
        ]);
        let started = shell
            .call(
                "ssmcp.exec.start",
                json!({
                    "command": command,
                    "env": {"SSMCP_SHELL_TEST_FIXTURE":"stdin"},
                    "stdin":"pipe",
                    "timeout_ms":10000
                }),
            )
            .await
            .unwrap();
        let id = started["job_id"].as_str().unwrap().to_owned();

        let mut panel = TaskPanel::default();
        assert!(refresh_native(&mut panel, &shell).await);
        let snapshot = panel.snapshot();
        assert_eq!(snapshot["source"], "native");
        assert_eq!(snapshot["jobs"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["selected_id"], id);
        assert_eq!(snapshot["selected_job"]["state"], "running");
        assert_eq!(snapshot["selected_job"]["running"], true);
        assert_eq!(snapshot["selected_job"]["command"], command);
        assert_eq!(snapshot["selected_job"]["cwd"], json!(workspace));
        assert!(snapshot["selected_job"]["duration_ms"].is_u64());
        let list = panel.render(512, 12, &ui()).join("\n");
        assert!(list.contains(&id), "{list}");
        assert!(list.contains("running"), "{list}");
        assert!(list.contains("fixture_process"), "{list}");
        assert!(list.contains(workspace.to_str().unwrap()), "{list}");
        assert!(!list.contains("fictional"), "{list}");

        assert!(panel.open());
        // Opening details may read the same native snapshot within one clock
        // tick. Completion need not request another redraw.
        refresh_native(&mut panel, &shell).await;
        assert_eq!(panel.snapshot()["selected_job"]["stdin"]["open"], true);

        // EOF ends only this fixture; no cancellation or shared registry is used.
        shell
            .call("ssmcp.exec.write", json!({"job_id":id,"close_stdin":true}))
            .await
            .unwrap();
        let done = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        assert_eq!(done["running"], false, "{done}");
        assert_eq!(done["exit_code"], 0, "{done}");
        assert!(refresh_native(&mut panel, &shell).await);
        let snapshot = panel.snapshot();
        assert_eq!(snapshot["selected_id"], id);
        assert_eq!(snapshot["detail"], true);
        assert_eq!(snapshot["selected_job"]["state"], "completed");
        assert_eq!(snapshot["selected_job"]["exit_code"], 0);
        assert_eq!(snapshot["selected_job"]["duration_ms"], done["duration_ms"]);
        assert_eq!(snapshot["selected_job"]["command"], command);
        assert_eq!(snapshot["selected_job"]["cwd"], json!(workspace));
        assert_eq!(snapshot["selected_job"]["stdout"]["eof"], true);
        let detail = panel.render(512, 32, &ui()).join("\n");
        assert!(detail.contains(&format!("Job: {id}")), "{detail}");
        assert!(detail.contains("State: completed"), "{detail}");
        assert!(detail.contains("Exit: 0"), "{detail}");
        assert!(detail.contains("Duration: "), "{detail}");
        assert!(detail.contains("fixture_process"), "{detail}");
        assert!(detail.contains(workspace.to_str().unwrap()), "{detail}");
        shell.shutdown().await;
    }

    #[test]
    fn narrow_unicode_details_are_padded_and_controls_cannot_escape() {
        let mut panel = TaskPanel::default();
        let mut value = job("a", true);
        value["cwd"] = json!("/tmp/中文\u{1b}[2J");
        panel.ingest(&json!({"jobs":[value]})).unwrap();
        panel.open();
        for width in [1, 7, 20, 60] {
            let rows = panel.render(width, 20, &ui());
            assert_eq!(rows.len(), 20);
            assert!(
                rows.iter()
                    .all(|row| row.width() == usize::from(width) && !row.contains('\u{1b}'))
            );
        }
        let text = wrap("中文abcdef", 4).concat();
        assert_eq!(text, "中文abcdef");
    }
}
