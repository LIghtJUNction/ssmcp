//! Human transcript only; never changes events retained for MCP consumers.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self, IsTerminal, Write},
    sync::{Mutex, OnceLock},
    time::Instant,
};

static DISPLAY: OnceLock<Mutex<Renderer>> = OnceLock::new();

pub(crate) fn configure(raw: bool) {
    let mut renderer = display().lock().unwrap_or_else(|e| e.into_inner());
    renderer.raw = raw;
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
    let _ = renderer.finish(&mut io::stderr().lock(), label);
}

#[derive(Default)]
struct Lines {
    pending: String,
    old: usize,
    new: usize,
    hunk: bool,
}

struct Renderer {
    raw: bool,
    color: bool,
    width: usize,
    streams: HashMap<String, Lines>,
    requests: HashMap<String, (String, Instant)>,
}

impl Renderer {
    fn new(raw: bool, color: bool, width: usize) -> Self {
        Self {
            raw,
            color,
            width: width.max(12),
            streams: HashMap::new(),
            requests: HashMap::new(),
        }
    }

    fn event(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<()> {
        if self.raw {
            writeln!(out, "{value}")?;
        } else if !self.rpc(out, label, value)? {
            self.value(out, label, value, "Event", 0)?;
        }
        out.flush()
    }

    // Keep IDs and method names at the RPC boundary: descending directly into
    // params/result otherwise loses the context that makes a transcript readable.
    fn rpc(&mut self, out: &mut impl Write, label: &str, value: &Value) -> io::Result<bool> {
        let v = value
            .get("request")
            .or_else(|| value.get("response"))
            .unwrap_or(value);
        let Some(id) = v.get("id").filter(|id| !id.is_null()) else {
            return Ok(false);
        };
        let key = id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string());
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            // An unresponsive backend must not grow display-only bookkeeping forever.
            if self.requests.len() >= 2048 {
                self.requests.clear();
            }
            self.requests
                .insert(key.clone(), (method.into(), Instant::now()));
            self.block(out, label, "Request", &format!("{key}  {method}"))?;
            let params = &v["params"];
            if let Some(argv) = params.get("command").and_then(Value::as_array) {
                let command = argv
                    .iter()
                    .map(|arg| {
                        let arg = arg.as_str().unwrap_or("");
                        if !arg.is_empty()
                            && arg
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || "_./:-".contains(c))
                        {
                            arg.to_owned()
                        } else {
                            format!("{arg:?}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.block(out, label, "Command", &format!("$ {command}"))?;
                for field in ["cwd", "timeoutMs"] {
                    if let Some(v) = params.get(field).filter(|v| !v.is_null()) {
                        let text = v
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string());
                        self.text(out, label, &format!("{field}: {text}\n"))?;
                    }
                }
            } else {
                self.value(out, label, params, "Tool", 0)?;
            }
            return Ok(true);
        }
        if v.get("result").is_some() || v.get("error").is_some() {
            let (method, elapsed) = self
                .requests
                .remove(&key)
                .map(|(method, started)| (method, Some(started.elapsed())))
                .unwrap_or_else(|| (String::new(), None));
            let result = &v["result"];
            let exit = result
                .get("exitCode")
                .or_else(|| result.get("exit_code"))
                .and_then(Value::as_i64);
            let error = v.get("error").is_some_and(|v| !v.is_null())
                || result["isError"] == true
                || result["success"] == false
                || exit.is_some_and(|code| code != 0);
            let title = if exit == Some(124) {
                "Timeout"
            } else if error {
                "Error"
            } else {
                "Success"
            };
            let duration = elapsed
                .map(|d| format!("  {:.2}s", d.as_secs_f64()))
                .unwrap_or_default();
            self.block(out, label, title, &format!("{key}  {method}{duration}"))?;
            if let Some(error) = v.get("error").filter(|v| !v.is_null()) {
                self.value(out, label, error, "Error", 0)?;
            } else {
                self.value(out, label, result, title, 0)?;
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn heading(&mut self, out: &mut impl Write, label: &str, title: &str) -> io::Result<()> {
        self.finish(out, label)?;
        let color = match title {
            "Error" => "31;1",
            "Success" => "32;1",
            "Assistant" => "36;1",
            "User" | "Request" => "34;1",
            "stdout" => "35;1",
            "stderr" | "Timeout" => "33;1",
            "Usage" => "90",
            _ => "33;1",
        };
        painted(out, self.color, color, &format!("\n[{label} · {title}]\n"))
    }

    fn block(
        &mut self,
        out: &mut impl Write,
        label: &str,
        title: &str,
        text: &str,
    ) -> io::Result<()> {
        self.heading(out, label, title)?;
        self.text(out, label, text)?;
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
            return self.block(out, label, title, &v.to_string());
        }
        if let Some(text) = v.as_str() {
            // Decode JSON envelopes once per layer, never replace literal source-code \\n.
            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                return self.value(out, label, &inner, title, depth + 1);
            }
            return self.block(out, label, title, text);
        }
        if let Some(array) = v.as_array() {
            for item in array {
                self.value(out, label, item, title, depth + 1)?;
            }
            return Ok(());
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
            return self.text(out, label, &String::from_utf8_lossy(&bytes));
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
                self.details(out, label, v)?;
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
        shown |= self.details(out, label, v)?;
        if !shown && title == "Error" {
            self.block(out, label, title, &v.to_string())?;
        } else if !shown && kind.is_empty() {
            if let Some(object) = v.as_object() {
                for (key, value) in object {
                    if !matches!(
                        key.as_str(),
                        "id" | "jsonrpc" | "threadId" | "turnId" | "itemId" | "type" | "role"
                    ) {
                        self.block(
                            out,
                            label,
                            title,
                            &format!(
                                "{key}: {}",
                                value
                                    .as_str()
                                    .map(str::to_owned)
                                    .unwrap_or_else(|| value.to_string())
                            ),
                        )?;
                    }
                }
            } else if !v.is_null() {
                self.block(out, label, title, &v.to_string())?;
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

    fn details(&mut self, out: &mut impl Write, label: &str, v: &Value) -> io::Result<bool> {
        let mut shown = false;
        // Buffered command/exec replies carry output beside exitCode. Render it
        // explicitly; a displayed status must never suppress either stream.
        for field in ["stdout", "stderr"] {
            if let Some(text) = v
                .get(field)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                self.block(out, label, field, text)?;
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
                            serde_json::to_string_pretty(inner).unwrap_or_default()
                        }
                    ),
                )?;
                shown = true;
            }
        }
        if v["success"] == true {
            self.block(out, label, "Success", "Completed")?;
            shown = true;
        }
        Ok(shown)
    }

    fn text(&mut self, out: &mut impl Write, label: &str, text: &str) -> io::Result<()> {
        let lines = self.streams.entry(label.into()).or_default();
        // Strip terminal control sequences supplied by tools, including in redirected logs.
        lines.pending.push_str(&clean(text));
        while let Some(end) = lines.pending.find('\n') {
            let line: String = lines.pending.drain(..=end).collect();
            render_line(
                out,
                self.color,
                self.width,
                lines,
                line.trim_end_matches(['\n', '\r']),
            )?;
        }
        // Bound buffering even for tools that never emit a newline.
        if lines.pending.len() > 64 * 1024 {
            let line = std::mem::take(&mut lines.pending);
            render_line(out, self.color, self.width, lines, &line)?;
        }
        Ok(())
    }

    fn finish(&mut self, out: &mut impl Write, label: &str) -> io::Result<()> {
        if let Some(mut lines) = self.streams.remove(label)
            && !lines.pending.is_empty()
        {
            let line = std::mem::take(&mut lines.pending);
            render_line(out, self.color, self.width, &mut lines, &line)?;
        }
        out.flush()
    }
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
    } else {
        "0"
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
        renderer.finish(&mut out, "codex").unwrap();
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
            "Tool",
            "bash",
            "cargo test",
            "Error",
            "first\n  second",
            "duration_ms: 24",
            "Success",
            "Usage",
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
            "r1  command/exec",
            "$ sh -c",
            "cwd: /project",
            "timeoutMs: 1000",
            "stdout",
            "stderr",
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
            "Timeout",
            "partial output",
            "installing tool",
            "exitCode: 124",
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
}
