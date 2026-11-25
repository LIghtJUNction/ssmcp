//! Bounded, native structured presentations in the client's conversation.
//! The display is data, not executable markup. Plain text remains complete for copy/history.
use super::{interface::messages, panel, ui::Ui};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_BLOCKS: usize = 16;
const MAX_DOCUMENT: usize = 24 * 1024;
const MAX_PLAIN: usize = 14 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Presentation {
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    blocks: Vec<Block>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Block {
    Text {
        text: String,
    },
    Card {
        title: String,
        body: String,
        #[serde(default)]
        tone: Tone,
    },
    Metrics {
        items: Vec<Metric>,
    },
    Table {
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Progress {
        label: String,
        value: Number,
        #[serde(default)]
        detail: Option<String>,
    },
    Steps {
        items: Vec<Step>,
    },
    BarChart {
        title: String,
        items: Vec<Bar>,
        #[serde(default)]
        unit: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Tone {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

impl Tone {
    fn color(self) -> &'static str {
        match self {
            Self::Info => "blue",
            Self::Success => "success",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Success => "SUCCESS",
            Self::Warning => "WARNING",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Metric {
    label: String,
    value: String,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Step {
    label: String,
    status: StepStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum StepStatus {
    Pending,
    Running,
    Done,
    Failed,
}

impl StepStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
    fn color(self) -> &'static str {
        match self {
            Self::Pending => "muted",
            Self::Running => "blue",
            Self::Done => "success",
            Self::Failed => "error",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Bar {
    label: String,
    value: Number,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    title: String,
    #[serde(default)]
    subtitle: Option<String>,
    blocks: Vec<Block>,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    idempotency_key: Option<String>,
}
fn default_kind() -> String {
    "progress".into()
}

fn field(text: &str, label: &str, limit: usize, multiline: bool) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > limit {
        return Err(format!("{label} must contain 1–{limit} UTF-8 bytes"));
    }
    if text
        .chars()
        .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\r' | '\t')))
    {
        return Err(format!(
            "{label} must not contain terminal control characters"
        ));
    }
    Ok(())
}
fn optional(value: &Option<String>, label: &str, limit: usize) -> Result<(), String> {
    if let Some(text) = value {
        field(text, label, limit, false)?;
    }
    Ok(())
}
fn count(n: usize, max: usize, label: &str) -> Result<(), String> {
    if n == 0 || n > max {
        return Err(format!("{label} must contain 1–{max} items"));
    }
    Ok(())
}
fn number(value: &Number) -> String {
    value.to_string()
}
fn magnitude(value: &Number) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

impl Presentation {
    fn validate(&self) -> Result<(), String> {
        field(&self.title, "title", 160, false)?;
        optional(&self.subtitle, "subtitle", 320)?;
        count(self.blocks.len(), MAX_BLOCKS, "blocks")?;
        for block in &self.blocks {
            match block {
                Block::Text { text } => field(text, "text", 4096, true)?,
                Block::Card { title, body, .. } => {
                    field(title, "card title", 160, false)?;
                    field(body, "card body", 4096, true)?;
                }
                Block::Metrics { items } => {
                    count(items.len(), 8, "metrics")?;
                    for m in items {
                        field(&m.label, "metric label", 80, false)?;
                        field(&m.value, "metric value", 80, false)?;
                        optional(&m.detail, "metric detail", 160)?;
                    }
                }
                Block::Table { columns, rows } => {
                    count(columns.len(), 8, "table columns")?;
                    count(rows.len(), 30, "table rows")?;
                    for column in columns {
                        field(column, "table column", 80, false)?;
                    }
                    for row in rows {
                        if row.len() != columns.len() {
                            return Err("every table row must match the column count".into());
                        }
                        for cell in row {
                            field(cell, "table cell", 320, false)?;
                        }
                    }
                }
                Block::Progress {
                    label,
                    value,
                    detail,
                } => {
                    field(label, "progress label", 160, false)?;
                    optional(detail, "progress detail", 320)?;
                    if !magnitude(value).is_finite() || !(0.0..=100.0).contains(&magnitude(value)) {
                        return Err(
                            "progress value must be a finite percentage from 0 to 100".into()
                        );
                    }
                }
                Block::Steps { items } => {
                    count(items.len(), 24, "steps")?;
                    for step in items {
                        field(&step.label, "step label", 320, false)?;
                    }
                }
                Block::BarChart { title, items, unit } => {
                    field(title, "chart title", 160, false)?;
                    optional(unit, "chart unit", 24)?;
                    count(items.len(), 24, "chart bars")?;
                    for bar in items {
                        field(&bar.label, "bar label", 80, false)?;
                        if !magnitude(&bar.value).is_finite() || magnitude(&bar.value) < 0.0 {
                            return Err("bar values must be finite nonnegative numbers".into());
                        }
                    }
                }
            }
        }
        if self.plain_text().len() > MAX_PLAIN {
            return Err(
                "presentation plain text exceeds 14 KiB; use separate presentations".into(),
            );
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid presentation")?
            .len()
            > MAX_DOCUMENT
        {
            return Err("presentation exceeds 24 KiB".into());
        }
        Ok(())
    }

    pub(crate) fn plain_text(&self) -> String {
        let mut lines = vec![self.title.clone()];
        if let Some(subtitle) = &self.subtitle {
            lines.push(subtitle.clone());
        }
        for block in &self.blocks {
            lines.push(String::new());
            match block {
                Block::Text { text } => lines.push(text.clone()),
                Block::Card { title, body, tone } => {
                    lines.push(format!("[{}] {title}", tone.label()));
                    lines.push(body.clone());
                }
                Block::Metrics { items } => {
                    for item in items {
                        let detail = item
                            .detail
                            .as_ref()
                            .map(|s| format!(" ({s})"))
                            .unwrap_or_default();
                        lines.push(format!("{}: {}{detail}", item.label, item.value));
                    }
                }
                Block::Table { columns, rows } => {
                    lines.push(columns.join(" | "));
                    for row in rows {
                        lines.push(row.join(" | "));
                    }
                }
                Block::Progress {
                    label,
                    value,
                    detail,
                } => {
                    lines.push(format!("{label}: {}%", number(value)));
                    if let Some(detail) = detail {
                        lines.push(detail.clone());
                    }
                }
                Block::Steps { items } => {
                    for step in items {
                        lines.push(format!("[{}] {}", step.status.label(), step.label));
                    }
                }
                Block::BarChart { title, items, unit } => {
                    lines.push(title.clone());
                    let unit = unit.as_ref().map(|u| format!(" {u}")).unwrap_or_default();
                    for bar in items {
                        lines.push(format!("{}: {}{unit}", bar.label, number(&bar.value)));
                    }
                }
            }
        }
        lines.join("\n")
    }

    pub(crate) fn render(&self, width: usize, ui: &Ui, color: bool) -> String {
        let width = width.clamp(1, 160);
        if width < 16 {
            return wrap(&self.plain_text(), width).join("\n") + "\n";
        }
        let mut lines = Vec::new();
        push_wrapped(&mut lines, &self.title, width, "accent", true, ui, color);
        if let Some(subtitle) = &self.subtitle {
            push_wrapped(&mut lines, subtitle, width, "muted", false, ui, color);
        }
        for block in &self.blocks {
            lines.push(String::new());
            match block {
                Block::Text { text } => {
                    push_wrapped(&mut lines, text, width, "foreground", false, ui, color)
                }
                Block::Card { title, body, tone } => {
                    let inner = width - 4;
                    let caption = clip(&format!("{} · {title}", tone.label()), inner);
                    lines.push(paint(
                        &format!(
                            "╭─ {}{}╮",
                            caption,
                            "─".repeat(inner.saturating_sub(caption.width()))
                        ),
                        tone.color(),
                        false,
                        ui,
                        color,
                    ));
                    for line in wrap(body, inner) {
                        lines.push(format!(
                            "{} {} {}",
                            paint("│", tone.color(), false, ui, color),
                            pad(&line, inner),
                            paint("│", tone.color(), false, ui, color)
                        ));
                    }
                    lines.push(paint(
                        &format!("╰{}╯", "─".repeat(width - 2)),
                        tone.color(),
                        false,
                        ui,
                        color,
                    ));
                }
                Block::Metrics { items } => {
                    let columns = (width / 24).clamp(1, items.len());
                    let cell = (width - 3 * (columns - 1)) / columns;
                    for chunk in items.chunks(columns) {
                        for row in 0..3 {
                            let cells: Vec<String> = chunk
                                .iter()
                                .map(|item| {
                                    let (text, tone, bold) = match row {
                                        0 => (item.value.as_str(), "accent", true),
                                        1 => (item.label.as_str(), "foreground", false),
                                        _ => (item.detail.as_deref().unwrap_or(""), "muted", false),
                                    };
                                    paint(&pad(text, cell), tone, bold, ui, color)
                                })
                                .collect();
                            lines.push(cells.join("   "));
                        }
                        lines.push(String::new());
                    }
                }
                Block::Table { columns, rows } => {
                    render_table(&mut lines, columns, rows, width, ui, color)
                }
                Block::Progress {
                    label,
                    value,
                    detail,
                } => {
                    push_wrapped(&mut lines, label, width, "foreground", true, ui, color);
                    let numeric = format!("{}%", number(value));
                    let separate = numeric.width() + 3 > width;
                    let cells = if separate {
                        width.min(60)
                    } else {
                        width.saturating_sub(numeric.width() + 3).clamp(1, 60)
                    };
                    let filled = ((magnitude(value) / 100.0) * cells as f64).round() as usize;
                    let bar = format!("{}{}", "━".repeat(filled), "─".repeat(cells - filled));
                    let tone = if magnitude(value) == 100.0 {
                        "success"
                    } else {
                        "blue"
                    };
                    if separate {
                        lines.push(paint(&bar, tone, false, ui, color));
                        push_wrapped(&mut lines, &numeric, width, tone, false, ui, color);
                    } else {
                        lines.push(format!(
                            "{}  {numeric}",
                            paint(&bar, tone, false, ui, color)
                        ));
                    }
                    if let Some(detail) = detail {
                        push_wrapped(&mut lines, detail, width, "muted", false, ui, color);
                    }
                }
                Block::Steps { items } => {
                    for step in items {
                        let text = format!("[{}] {}", step.status.label(), step.label);
                        push_wrapped(
                            &mut lines,
                            &text,
                            width,
                            step.status.color(),
                            false,
                            ui,
                            color,
                        );
                    }
                }
                Block::BarChart { title, items, unit } => {
                    push_wrapped(&mut lines, title, width, "foreground", true, ui, color);
                    let maximum = items
                        .iter()
                        .map(|bar| magnitude(&bar.value))
                        .fold(0.0, f64::max);
                    let label_width = items
                        .iter()
                        .map(|b| b.label.width())
                        .max()
                        .unwrap_or(0)
                        .min(width / 3);
                    let suffix = unit
                        .as_ref()
                        .map(|u| format!(" {}", panel::safe_text(u)))
                        .unwrap_or_default();
                    let values: Vec<String> = items
                        .iter()
                        .map(|b| format!("{}{suffix}", number(&b.value)))
                        .collect();
                    let value_width = values.iter().map(|v| v.width()).max().unwrap_or(0);
                    let available = width.saturating_sub(label_width + value_width + 4);
                    if available < 4 {
                        for (bar, value) in items.iter().zip(&values) {
                            push_wrapped(
                                &mut lines,
                                &format!("{}: {value}", bar.label),
                                width,
                                "blue",
                                false,
                                ui,
                                color,
                            );
                        }
                    } else {
                        let bar_width = available.min(60);
                        for (bar, value) in items.iter().zip(&values) {
                            let filled = if maximum == 0.0 {
                                0
                            } else {
                                ((magnitude(&bar.value) / maximum) * bar_width as f64).round()
                                    as usize
                            };
                            let visual =
                                format!("{}{}", "█".repeat(filled), " ".repeat(bar_width - filled));
                            lines.push(format!(
                                "{}  {}  {value}",
                                pad(&bar.label, label_width),
                                paint(&visual, "blue", false, ui, color)
                            ));
                        }
                    }
                }
            }
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n") + "\n"
    }
}

fn paint(text: &str, tone: &str, bold: bool, ui: &Ui, color: bool) -> String {
    if color {
        let weight = if bold { "1;" } else { "" };
        format!("\x1b[{weight}{}m{text}\x1b[0m", ui.color(tone))
    } else {
        text.to_owned()
    }
}
fn push_wrapped(
    out: &mut Vec<String>,
    text: &str,
    width: usize,
    tone: &str,
    bold: bool,
    ui: &Ui,
    color: bool,
) {
    out.extend(
        wrap(text, width)
            .into_iter()
            .map(|line| paint(&line, tone, bold, ui, color)),
    );
}
fn clip(text: &str, width: usize) -> String {
    let text = panel::safe_text(text);
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let size = c.width().unwrap_or(0);
        if used + size > width - 1 {
            break;
        }
        out.push(c);
        used += size;
    }
    out.push('…');
    out
}
fn pad(text: &str, width: usize) -> String {
    let text = clip(text, width);
    format!("{text}{}", " ".repeat(width.saturating_sub(text.width())))
}
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let normalized = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ");
    let mut result = Vec::new();
    for source in normalized.split('\n') {
        let source = panel::safe_text(source);
        let mut line = String::new();
        let mut used = 0;
        for c in source.chars() {
            let size = c.width().unwrap_or(0);
            if size > width {
                if !line.is_empty() {
                    result.push(std::mem::take(&mut line));
                }
                result.push("…".into());
                used = 0;
                continue;
            }
            if used + size > width {
                result.push(std::mem::take(&mut line));
                used = 0;
            }
            line.push(c);
            used += size;
        }
        if !line.is_empty() || source.is_empty() {
            result.push(line);
        }
    }
    result
}
fn render_table(
    out: &mut Vec<String>,
    columns: &[String],
    rows: &[Vec<String>],
    width: usize,
    ui: &Ui,
    color: bool,
) {
    let cell = width.saturating_sub(3 * (columns.len() - 1)) / columns.len();
    if cell < 6 {
        for (index, row) in rows.iter().enumerate() {
            if index != 0 {
                out.push(String::new());
            }
            for (column, value) in columns.iter().zip(row) {
                push_wrapped(
                    out,
                    &format!("{column}: {value}"),
                    width,
                    "foreground",
                    false,
                    ui,
                    color,
                );
            }
        }
        return;
    }
    let header = columns
        .iter()
        .map(|s| pad(s, cell))
        .collect::<Vec<_>>()
        .join(" │ ");
    out.push(paint(&header, "blue", true, ui, color));
    out.push(paint(
        &"─".repeat(cell * columns.len() + 3 * (columns.len() - 1)),
        "muted",
        false,
        ui,
        color,
    ));
    for row in rows {
        out.push(
            row.iter()
                .map(|s| pad(s, cell))
                .collect::<Vec<_>>()
                .join(" │ "),
        );
    }
}

pub(crate) fn send(arguments: Value) -> Result<Value, String> {
    if serde_json::to_vec(&arguments)
        .map_err(|_| "invalid presentation")?
        .len()
        > MAX_DOCUMENT + 1024
    {
        return Err("presentation request exceeds 25 KiB".into());
    }
    let request: Request =
        serde_json::from_value(arguments).map_err(|e| format!("invalid presentation: {e}"))?;
    let presentation = Presentation {
        title: request.title,
        subtitle: request.subtitle,
        blocks: request.blocks,
    };
    presentation.validate()?;
    let blocks = presentation.blocks.len();
    let text = presentation.plain_text();
    let mut answer = messages::send_presentation(
        json!({
            "text": text, "kind": request.kind,
            "idempotency_key": request.idempotency_key,
        }),
        presentation,
    )?;
    if let Some(object) = answer.as_object_mut() {
        object.insert("format".into(), json!("intelligence_ui.v1"));
        object.insert("block_count".into(), json!(blocks));
    }
    Ok(answer)
}

fn text_schema(max: usize) -> Value {
    json!({"type":"string","minLength":1,"maxLength":max,"description":format!("At most {max} UTF-8 bytes; terminal control characters are rejected.")})
}
fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
}
fn array_schema(items: Value, max: usize) -> Value {
    json!({"type":"array","minItems":1,"maxItems":max,"items":items})
}
pub(crate) fn tool() -> Value {
    let metric = object_schema(
        json!({"label":text_schema(80),"value":text_schema(80),"detail":text_schema(160)}),
        &["label", "value"],
    );
    let step = object_schema(
        json!({"label":text_schema(320),"status":{"type":"string","enum":["pending","running","done","failed"]}}),
        &["label", "status"],
    );
    let bar = object_schema(
        json!({"label":text_schema(80),"value":{"type":"number","minimum":0}}),
        &["label", "value"],
    );
    let blocks = json!({"oneOf":[
        object_schema(json!({"type":{"const":"text"},"text":text_schema(4096)}), &["type","text"]),
        object_schema(json!({"type":{"const":"card"},"title":text_schema(160),"body":text_schema(4096),"tone":{"type":"string","enum":["info","success","warning","error"],"default":"info"}}), &["type","title","body"]),
        object_schema(json!({"type":{"const":"metrics"},"items":array_schema(metric,8)}), &["type","items"]),
        object_schema(json!({"type":{"const":"table"},"columns":array_schema(text_schema(80),8),"rows":array_schema(array_schema(text_schema(320),8),30)}), &["type","columns","rows"]),
        object_schema(json!({"type":{"const":"progress"},"label":text_schema(160),"value":{"type":"number","minimum":0,"maximum":100},"detail":text_schema(320)}), &["type","label","value"]),
        object_schema(json!({"type":{"const":"steps"},"items":array_schema(step,24)}), &["type","items"]),
        object_schema(json!({"type":{"const":"bar_chart"},"title":text_schema(160),"items":array_schema(bar,24),"unit":text_schema(24)}), &["type","title","items"])
    ]});
    json!({
        "name":"ssmcp.ui.present",
        "description":"Render native Intelligence UI in this client's conversation: semantic cards, metric grids, responsive tables, progress bars, task steps and numeric bar charts. Supply actual data in typed blocks. This is a presentation, not executable HTML and not a new model. Theme, narrow terminals and NO_COLOR are respected. Complete plain text is retained for /copy and history; visual table cells may be shortened. Use ui.ask for interactive questions. Supports the same kind, bounded retry idempotency and ACK contract as ui.message. Sender identity comes from the controller context; caller-supplied author fields are rejected. A successful display can receive newly queued user requirements.",
        "inputSchema":object_schema(json!({
            "title":text_schema(160),"subtitle":text_schema(320),"blocks":array_schema(blocks,MAX_BLOCKS),
            "kind":{"type":"string","enum":["progress","answer"],"default":"progress"},
            "idempotency_key":{"type":"string","minLength":1,"maxLength":96,"pattern":"^[A-Za-z0-9_-]+$"}
        }), &["title","blocks"]),
        "annotations":{"readOnlyHint":false,"destructiveHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Presentation {
        serde_json::from_value(json!({
            "title":"项目进度 / Project",
            "subtitle":"Native structured conversation",
            "blocks":[
                {"type":"card","title":"Ready","body":"All checks complete.\nUnicode: 界面 👩‍💻","tone":"success"},
                {"type":"metrics","items":[{"label":"Tests","value":"814","detail":"passed"},{"label":"Queued","value":"2"},{"label":"Version","value":"0.9.0"}]},
                {"type":"table","columns":["Project","Status","Details"],"rows":[["ssmcp","ready","完整的说明文字，不因显示截断而丢失"],["client","active","long-context-value-that-does-not-fit-a-small-cell"]]},
                {"type":"progress","label":"Deployment","value":62.5,"detail":"Source verified"},
                {"type":"steps","items":[{"label":"Build","status":"done"},{"label":"Install","status":"running"},{"label":"Probe","status":"pending"}]},
                {"type":"bar_chart","title":"Checks","items":[{"label":"Rust","value":814},{"label":"Terminal","value":380}],"unit":"checks"}
            ]
        })).unwrap()
    }
    #[test]
    fn rich_layout_respects_terminal_cell_width_and_retains_copy_data() {
        let presentation = sample();
        presentation.validate().unwrap();
        let ui = Ui::default();
        for width in [1, 8, 16, 24, 40, 80, 120] {
            let output = presentation.render(width, &ui, false);
            assert!(!output.contains('\x1b'));
            for line in output.lines() {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
        let text = presentation.plain_text();
        assert!(text.contains("long-context-value-that-does-not-fit-a-small-cell"));
        assert!(text.contains("完整的说明文字，不因显示截断而丢失"));
        assert!(text.contains("62.5%"));
        let colored = presentation.render(80, &ui, true);
        assert!(colored.contains('\x1b'));
        assert!(colored.contains("814"));
        assert!(colored.contains("running"));
    }
    #[test]
    fn structured_display_uses_shared_redaction_and_terminal_sanitizing() {
        let document: Presentation = serde_json::from_value(json!({
            "title":"Visible \u{202e}title",
            "blocks":[
                {"type":"text","text":"{\"api_key\":\"private-secret-123\"}\nsecond\tline"},
                {"type":"bar_chart","title":"Units","unit":"\u{202e}x","items":[{"label":"Count","value":1}]}
            ]
        })).unwrap();
        document.validate().unwrap();
        for width in [8, 24, 80] {
            let output = document.render(width, &Ui::default(), false);
            assert!(!output.contains('\u{202e}'));
            assert!(!output.contains("private-secret-123"));
            assert!(!output.contains('\x1b'));
            for line in output.lines() {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }

    #[test]
    fn precise_numbers_remain_complete_and_high_precision_progress_wraps() {
        let document: Presentation = serde_json::from_value(json!({
            "title":"Exact values",
            "blocks":[
                {"type":"progress","label":"Partial","value":12.345678901234567},
                {"type":"bar_chart","title":"Counter","items":[{"label":"count","value":9007199254740993_u64}]}
            ]
        })).unwrap();
        document.validate().unwrap();
        assert!(document.plain_text().contains("9007199254740993"));
        for width in [16, 24, 80] {
            for line in document.render(width, &Ui::default(), false).lines() {
                assert!(line.width() <= width, "{width}: {line:?}");
            }
        }
    }
    #[test]
    fn malformed_or_executable_input_is_rejected_before_publishing() {
        let mut value = serde_json::to_value(sample()).unwrap();
        value["blocks"][0]["body"] = json!("\u{1b}[2J");
        let document: Presentation = serde_json::from_value(value).unwrap();
        assert!(document.validate().is_err());
        for arguments in [
            json!({"title":"spoof","author":"User","blocks":[{"type":"text","text":"body"}]}),
            json!({"title":"unknown","blocks":[{"type":"text","text":"body","extra":true}]}),
        ] {
            assert!(send(arguments).is_err());
        }
        let invalid =
            json!({"title":"bad","blocks":[{"type":"html","html":"<script>run()</script>"}]});
        assert!(serde_json::from_value::<Presentation>(invalid).is_err());
        let invalid = json!({"title":"bad","blocks":[{"type":"table","columns":["a","b"],"rows":[["only one"]]}]});
        assert!(
            serde_json::from_value::<Presentation>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        for value in [-1.0, 100.1] {
            let invalid =
                json!({"title":"bad","blocks":[{"type":"progress","label":"step","value":value}]});
            assert!(
                serde_json::from_value::<Presentation>(invalid)
                    .unwrap()
                    .validate()
                    .is_err()
            );
        }
    }
    #[test]
    fn empty_zero_and_complete_values_render_without_division_or_overflow() {
        let document: Presentation = serde_json::from_value(json!({
            "title":"Complete",
            "blocks":[
                {"type":"progress","label":"Done","value":100.0},
                {"type":"bar_chart","title":"Zero","items":[{"label":"a","value":0.0},{"label":"b","value":0.0}]}
            ]
        })).unwrap();
        document.validate().unwrap();
        let output = document.render(40, &Ui::default(), false);
        assert!(output.contains("100.0%"));
        assert!(!output.contains("NaN"));
        assert!(!output.contains("inf"));
        for line in output.lines() {
            assert!(line.width() <= 40);
        }
    }
    #[test]
    fn document_and_collection_budgets_apply_to_complete_content() {
        let mut document = sample();
        document.blocks = vec![
            Block::Text {
                text: "a".repeat(4096)
            };
            MAX_BLOCKS
        ];
        assert!(document.validate().is_err());
        document.blocks = vec![Block::Text { text: "a".into() }; MAX_BLOCKS + 1];
        assert!(document.validate().is_err());
        let schema = tool();
        assert_eq!(
            schema["inputSchema"]["properties"]["blocks"]["maxItems"],
            MAX_BLOCKS
        );
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
    }
}
