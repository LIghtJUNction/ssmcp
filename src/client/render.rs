//! Human transcript only; never changes events retained for MCP consumers.
use super::ui::Ui;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    sync::{Mutex, OnceLock},
    time::Instant,
};

static DISPLAY: OnceLock<Mutex<Renderer>> = OnceLock::new();

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

#[cfg(unix)]
fn terminal_width() -> usize {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl writes a winsize into valid, aligned storage.
    if unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0 {
        // SAFETY: successful TIOCGWINSZ initialized the structure.
        let columns = unsafe { size.assume_init() }.ws_col;
        if columns > 8 {
            return usize::from(columns);
        }
    }
    100
}

#[cfg(not(unix))]
fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 8)
        .unwrap_or(100)
}

fn color_enabled(tty: bool, no_color: bool) -> bool {
    tty && !no_color
}

pub(crate) fn event(label: &str, value: &Value) {
    let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    renderer.width = terminal_width();
    let _ = renderer.event(&mut io::stderr().lock(), label, value);
}

pub(crate) fn finish(label: &str) {
    let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    let _ = renderer.finish_group(&mut io::stderr().lock(), label);
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
}

struct Renderer {
    raw: bool,
    verbose: bool,
    color: bool,
    width: usize,
    ui: Ui,
    streams: HashMap<String, Lines>,
    requests: HashMap<String, (String, Instant)>,
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
        }
    }

    fn event(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<()> {
        if self.raw {
            writeln!(out, "{value}")?;
        } else if !self.activity(out, label, value)? && !self.rpc(out, label, value)? {
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
                    serde_json::to_string_pretty(value).unwrap_or_default()
                ),
            )?;
        }
        out.flush()
    }

    fn activity(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<bool> {
        let kind = value["type"].as_str().unwrap_or("");
        let id = value["job_id"].as_str().unwrap_or("");
        match kind {
            "ssmcp.exec.started" => {
                let command = command_label(&value["command"]);
                self.summary(out, label, "Running", &format!("{id}  {command}"))?;
                if let Some(cwd) = value["cwd"].as_str() {
                    painted(
                        out,
                        self.color,
                        self.ui.theme.color("gray"),
                        &format!("       {}\n", clean(cwd)),
                    )?;
                }
            }
            "ssmcp.exec.finished" => {
                let failed = value["exit_code"].as_i64().is_some_and(|v| v != 0)
                    || !value["error"].is_null();
                let title = if value["timed_out"] == true {
                    "Timeout"
                } else if value["cancelled"] == true {
                    "Warning"
                } else if failed {
                    "Error"
                } else {
                    "Success"
                };
                let state = value["state"].as_str().unwrap_or("completed");
                let elapsed = value["duration_ms"].as_u64().unwrap_or(0) as f64 / 1000.0;
                let exit = value["exit_code"]
                    .as_i64()
                    .map(|v| format!("  exit={v}"))
                    .unwrap_or_default();
                self.summary(
                    out,
                    label,
                    title,
                    &format!("{id}  {state}{exit}  {elapsed:.2}s"),
                )?;
                if let Some(error) = value["error"].as_str() {
                    self.block(out, label, "Error", error)?;
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
            "ssmcp.fileread" => {
                self.summary(out, label, "Read", value["path"].as_str().unwrap_or(""))?
            }
            "ssmcp.filechange" => {
                self.summary(out, label, "Edit", value["path"].as_str().unwrap_or(""))?;
                if let Some(diff) = value["diff"].as_str().filter(|v| !v.is_empty()) {
                    self.text(out, label, diff)?;
                    self.finish(out, label)?;
                } else if value["preview_available"] == false {
                    self.text(
                        out,
                        label,
                        "Binary or large file updated; preview unavailable\n",
                    )?;
                }
            }
            "ssmcp.skill_use" => self.summary(
                out,
                label,
                "Skill",
                &format!(
                    "{}  {}",
                    value["tool"].as_str().unwrap_or(""),
                    value["skill"].as_str().unwrap_or("")
                ),
            )?,
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
            format!("  id={id}")
        } else {
            String::new()
        };
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            if self.requests.len() >= 2048 {
                self.requests.clear();
            }
            self.requests.insert(key, (method.into(), Instant::now()));
            self.summary(out, label, "Request", &format!("{method}{id_text}"))?;
            let params = &v["params"];
            let params = params.get("arguments").unwrap_or(params);
            if let Some(command) = params.get("command").or_else(|| params.get("cmd")) {
                let command = if let Some(args) = command.as_array() {
                    args.iter()
                        .map(|a| {
                            let a = a.as_str().unwrap_or("");
                            if !a.is_empty()
                                && a.chars()
                                    .all(|c| c.is_ascii_alphanumeric() || "_./:-".contains(c))
                            {
                                a.to_owned()
                            } else {
                                format!("{a:?}")
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    command.as_str().unwrap_or("").to_owned()
                };
                self.text(out, label, &format!("cmd={command}\n"))?;
            }
            for field in ["cwd", "name", "tool", "processId"] {
                if let Some(v) = params.get(field).filter(|v| !v.is_null()) {
                    self.text(out, label, &format!("{field}={}\n", compact(v, 0)))?;
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
            if let Some(error) = v.get("error").filter(|e| !e.is_null()) {
                self.value(out, label, error, "Error", 0)?;
            } else {
                self.value(out, label, result, "Result", 0)?;
            }
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
        self.finish_group(out, label)?;
        self.prefix(out, title)?;
        // Include the backend label once; continuation lines are indented.
        let text = clean(&format!("{label}  {text}"));
        let available = self
            .width
            .saturating_sub(if self.ui.timestamps { 24 } else { 7 })
            .max(8);
        let mut col = 0;
        for c in text.chars() {
            let cells = if c as u32 >= 0x1100 { 2 } else { 1 };
            if c == '\n' || col + cells > available {
                write!(out, "\n       ")?;
                col = 0;
            }
            if c != '\n' {
                write!(out, "{c}")?;
                col += cells;
            }
        }
        writeln!(out)
    }

    fn heading(&mut self, out: &mut impl Write, label: &str, title: &str) -> io::Result<()> {
        self.finish_group(out, label)?;
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
        } else if lower.contains("diff") || lower.contains("filechange") || lower.contains("patch")
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
            let others: Vec<_> = self
                .streams
                .keys()
                .filter(|k| k.as_str() != key)
                .cloned()
                .collect();
            for other in others {
                self.finish(out, &other)?;
            }
            if !self.streams.contains_key(&key) {
                self.heading(
                    out,
                    &format!("{label} · {process}"),
                    if stream == "stderr" {
                        "stderr"
                    } else {
                        "stdout"
                    },
                )?;
                let tone = if stream == "stderr" {
                    self.ui.theme.color("yellow")
                } else {
                    "0"
                };
                self.streams.entry(key.clone()).or_default().tone = tone;
            }
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
        if !lines.pending.is_empty() && !lines.hunk && !diff_prefix {
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
            if lines.emitted > 0 {
                render_live(out, self.color, self.width, lines, line, true)?;
            } else {
                render_line(out, self.color, self.width, lines, line)?;
            }
        }
        // Bound buffering even for tools that never emit a newline.
        if lines.pending.len() > 64 * 1024 {
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
        if let Some(mut lines) = self.streams.remove(label)
            && !lines.pending.is_empty()
        {
            let line = std::mem::take(&mut lines.pending);
            if lines.emitted > 0 {
                render_live(out, self.color, self.width, &mut lines, &line, true)?;
            } else {
                render_line(out, self.color, self.width, &mut lines, &line)?;
            }
        }
        out.flush()
    }
}

/// A bounded description, never a giant JSON dump in the default transcript.
fn command_label(command: &Value) -> String {
    command
        .as_array()
        .map(|args| {
            args.iter()
                .map(|arg| {
                    let arg = arg.as_str().unwrap_or("");
                    if !arg.is_empty()
                        && arg
                            .chars()
                            .all(|c| c.is_alphanumeric() || "_./:-=".contains(c))
                    {
                        arg.to_owned()
                    } else {
                        format!("{arg:?}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
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
    let renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = io::stdout().lock();
    if !out.is_terminal() || renderer.raw {
        writeln!(out, "{output}")?;
    } else {
        writeln!(out)?;
        painted(
            &mut out,
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
            &mut out,
            renderer.color,
            renderer.ui.theme.color("gray"),
            "\n  Commands run here. Parallel jobs keep separate output.\n  Ctrl-C disconnects and stops owned processes.\n\n",
        )?;
    }
    out.flush()
}

pub(crate) fn terminal_output(bytes: &[u8]) {
    let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    let mut out = io::stderr().lock();
    if renderer.raw {
        let _=renderer.event(&mut out,"shell",&json!({"type":"ssmcp.terminal_output","data_base64":STANDARD.encode(bytes),"text":String::from_utf8_lossy(bytes)}));
    } else if renderer.color {
        let _ = out.write_all(bytes);
        let _ = out.flush();
    } else {
        let _ = out.write_all(clean(&String::from_utf8_lossy(bytes)).as_bytes());
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
    let mut prefix = String::new();
    let lower = line.to_ascii_lowercase();
    let progress = lower.contains("downloading")
        || lower.contains("download ")
        || lower.starts_with("mise ") && lower.contains("installing");
    let failure = lower.starts_with("error")
        || lower.starts_with("fatal")
        || lower.contains(" failed")
        || lower.contains("timed out");
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
    } else if lower.starts_with("warning") || lower.contains("retrying") {
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
    fn raw_is_exact_original_serialization_without_added_fields() {
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
        let event: Value = serde_json::from_str(
            r#"{"result":{"content":[{"type":"text","text":"first\nsecond"}]}}"#,
        )
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
    fn buffered_command_reply_keeps_both_streams_and_request_context() {
        let text = render(
            &[
                json!({"type":"ssmcp.request","request":{"id":"r1","method":"command/exec","params":{"command":["sh","-c","printf hello"],"cwd":"/project","timeoutMs":1000}}}),
                json!({"id":"r1","result":{"exitCode":0,"stdout":"hello\nsecond\n","stderr":"warning\n"}}),
            ],
            false,
            true,
        );
        for expected in [
            "command/exec  id=r1",
            "cmd=sh -c",
            "cwd=/project",
            "timeout=1.000s",
            "[STDOUT]",
            "[STDERR]",
            "hello\n  second",
            "warning",
            "\x1b[34;1m",
            "\x1b[32;1m",
        ] {
            assert!(
                text.contains(expected) || clean(&text).contains(expected),
                "missing {expected}: {text}"
            );
        }
    }

    #[test]
    fn timeout_still_displays_partial_stdout_and_stderr() {
        let text = render(
            &[
                json!({"id":"r2","result":{"exitCode":124,"stdout":"partial output","stderr":"installing tool"}}),
            ],
            false,
            true,
        );
        for expected in [
            "command timeout",
            "partial output",
            "installing tool",
            "exitCode=124",
        ] {
            assert!(text.contains(expected), "{text}");
        }
    }

    #[test]
    fn rpc_protocol_errors_are_red_and_raw_stays_unmodified() {
        let event = json!({"id":"r3","error":{"code":-32601,"message":"unsupported method"}});
        let text = render(std::slice::from_ref(&event), false, true);
        assert!(text.contains("\x1b[31;1m"));
        assert!(text.contains("unsupported method"));
        assert_eq!(
            render(std::slice::from_ref(&event), true, true),
            format!("{event}\n")
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
        assert_eq!(String::from_utf8_lossy(&out), "[STDOUT] codex\n  live");
        renderer
            .event(&mut out, "codex", &delta(" progress"))
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[STDOUT] codex\n  live progress"
        );
        renderer.event(&mut out, "codex", &delta("\nnext")).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out),
            "[STDOUT] codex\n  live progress\n  next"
        );
        renderer.finish_group(&mut out, "codex").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "[STDOUT] codex\n  live progress\n  next\n"
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
            "[STDOUT] codex\n  1234567890\n  ab"
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
            text.starts_with("[STDOUT] codex\n  1234567890\n  ab\n[OK  ]"),
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
    fn base64_partial_output_is_visible_before_completion() {
        let mut renderer = Renderer::new(false, false, 100);
        let mut output = Vec::new();
        renderer.event(&mut output, "codex", &json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode("still running")}})).unwrap();
        assert!(String::from_utf8(output).unwrap().contains("still running"));
    }
    #[test]
    fn pretty_folds_output_keeps_tail_and_verbose_reveals_json() {
        let stdout = (0..40).map(|n| format!("line-{n}\n")).collect::<String>();
        let event = json!({"id":"fold","result":{"exitCode":1,"stdout":stdout,"stderr":"fatal: stopped","huge":{"blob":"x".repeat(10000)}}});
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
                json!({"type":"ssmcp.exec.started","job_id":"42-1","command":["cargo","test"],"cwd":"/work"}),
                json!({"method":"command/exec/outputDelta","params":{"processId":"42-1","stream":"stdout","deltaBase64":STANDARD.encode("ok\n")}}),
                json!({"type":"ssmcp.exec.finished","job_id":"42-1","state":"completed","exit_code":0,"duration_ms":1234}),
                json!({"type":"ssmcp.fileread","path":"src/main.rs"}),
                json!({"type":"ssmcp.skill_use","tool":"ssmcp.skills.read","skill":"skills/review/SKILL.md"}),
            ],
            false,
            false,
        );
        assert_eq!(output.matches("cargo test").count(), 1);
        assert!(output.contains("[RUN ] codex  42-1"));
        assert!(output.contains("codex · 42-1"));
        assert!(output.contains("completed  exit=0  1.23s"));
        assert!(output.contains("[READ] codex  src/main.rs"));
        assert!(output.contains("[SKIL] codex  ssmcp.skills.read"));
        assert!(!output.contains("{\"type\""));
    }

    #[test]
    fn terminal_preview_fixture() {
        let Some(path) = std::env::var_os("SSMCP_RENDER_PREVIEW") else {
            return;
        };
        let events = [
            json!({"type":"ssmcp.status","level":"Connected","message":"ssmcp · improve command execution"}),
            json!({"type":"ssmcp.exec.started","job_id":"1842-1","command":["cargo","test","--locked"],"cwd":"~/projects/ssmcp"}),
            json!({"type":"ssmcp.exec.started","job_id":"1842-2","command":["cargo","clippy","--locked"],"cwd":"~/projects/ssmcp"}),
            json!({"method":"command/exec/outputDelta","params":{"processId":"1842-1","stream":"stdout","deltaBase64":STANDARD.encode("running 7 command lifecycle tests\ntest parallel_commands ... ok\ntest wait_keeps_command_running ... ok\n")}}),
            json!({"type":"ssmcp.exec.finished","job_id":"1842-2","state":"completed","exit_code":0,"duration_ms":890}),
            json!({"type":"ssmcp.exec.finished","job_id":"1842-1","state":"completed","exit_code":0,"duration_ms":1280}),
            json!({"type":"ssmcp.fileread","path":"src/backend/shell.rs"}),
            json!({"type":"ssmcp.filechange","path":"src/backend/shell.rs","diff":"--- a/src/backend/shell.rs\n+++ b/src/backend/shell.rs\n@@ -18,1 +18,1 @@\n-const MAX_WAIT: u64 = 5_000;\n+const MAX_WAIT: u64 = 60_000;\n"}),
            json!({"type":"ssmcp.skill_use","tool":"ssmcp.skills.read","skill":"skills/ssmcp/SKILL.md"}),
            json!({"type":"ssmcp.output_archived","job_id":"1842-3","stream":"stdout","artifact":{"path":"/tmp/ssmcp/1000/command-output.log","bytes":126720,"lines":2400,"complete":true}}),
            json!({"type":"ssmcp.status","level":"Progress","message":"Reload acknowledged · reconnecting with updated client"}),
        ];
        let mut renderer = Renderer::new(false, true, 100);
        renderer.ui.theme = super::super::ui::Theme::Minimal;
        let mut output = Vec::new();
        for event in &events {
            renderer.event(&mut output, "dev", event).unwrap();
        }
        renderer.finish_group(&mut output, "dev").unwrap();
        std::fs::write(path, output).unwrap();
    }
}
