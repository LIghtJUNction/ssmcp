//! Display primitives shared by the built-in terminal and alternate frontends.
use super::{
    render, terminal,
    ui::{Theme, Ui},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

pub(crate) mod harness;
pub(crate) mod history;
pub(crate) mod messages;
pub(crate) mod questions;

static CONFIG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static UPDATES: Mutex<()> = Mutex::new(());
static RESIZES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PENDING_THEME: Mutex<Option<Theme>> = Mutex::new(None);

/// Safe while a terminal frame/input lock is held: only enqueue the desired theme.
pub(super) fn queue_theme(theme: Theme) {
    *PENDING_THEME
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(theme);
}

pub(crate) fn initialize_path(explicit: Option<&Path>) {
    let path = Ui::config_path(explicit).map(|path| {
        if path.is_absolute() {
            path
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    *CONFIG_PATH
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = path;
}

fn config_path() -> Option<PathBuf> {
    CONFIG_PATH
        .get()
        .and_then(|path| path.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .or_else(|| Ui::config_path(None))
}

pub(crate) fn get() -> Value {
    json!({
        "schema_version": 1,
        "config": render::current_ui(),
        "config_path": config_path(),
        "display": terminal::snapshot(),
        "activity": render::snapshot(),
        "steering": super::steering::snapshot(),
        "questions": questions::snapshot(),
    })
}

fn document_with_ui(ui: &Ui, existing: Option<&str>) -> Result<String, String> {
    let mut document = if let Some(text) = existing {
        Ui::parse(text)?;
        toml::from_str::<toml::Table>(text).map_err(|_| "invalid client configuration")?
    } else {
        toml::Table::new()
    };
    ui.validate()?;
    document.insert(
        "ui".into(),
        toml::Value::try_from(ui).map_err(|_| "cannot serialize UI configuration")?,
    );
    toml::to_string_pretty(&document).map_err(|_| "cannot serialize client configuration".into())
}

fn save(ui: &Ui) -> Result<(), String> {
    let path = config_path().ok_or("UI configuration path is unavailable; use --ui-config FILE")?;
    // A display command must not replace an unrelated or malformed document.
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("cannot read UI configuration".into()),
    };
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|_| "cannot create UI configuration directory")?;
    let data = document_with_ui(ui, existing.as_deref())?;
    let temporary = parent.join(format!(".ssmcp-ui-{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|_| "cannot create temporary UI configuration")?;
    let result = (|| {
        file.write_all(data.as_bytes())
            .map_err(|_| "cannot write UI configuration")?;
        file.sync_all()
            .map_err(|_| "cannot flush UI configuration")?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(|_| "cannot replace UI configuration")
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.map_err(str::to_owned)
}

fn apply(ui: Ui, persist: bool) -> Result<Option<(u16, u16)>, String> {
    ui.validate()?;
    let current = render::current_ui();
    if terminal::active()
        && (ui.dashboard != current.dashboard || ui.log_format != current.log_format)
    {
        return Err(
            "dashboard and log_format are startup options; change the file and restart the client"
                .into(),
        );
    }
    if persist {
        save(&ui)?;
    }
    render::apply_ui(ui.clone());
    Ok(terminal::apply_ui(&ui))
}

fn action_inner(action: &str) -> Result<Option<(u16, u16)>, String> {
    let mut ui = render::current_ui();
    match action {
        "layout.next" => ui.layout = ui.layout.next(),
        "sidebar.toggle" => ui.sidebar = !ui.sidebar,
        "theme.next" => ui.theme = ui.theme.next(),
        "reload" => ui = Ui::load(config_path().as_deref())?,
        "client.reload" => {
            super::steering::request_reload()?;
            return Ok(None);
        }
        "save" => {
            save(&ui)?;
            terminal::notice("Appearance saved · F5 Reload");
            return Ok(None);
        }
        "logs.clear" => {
            terminal::clear_logs();
            return Ok(None);
        }
        "audit.up" | "audit.down" | "audit.open" | "audit.back" | "logs.page-up"
        | "logs.page-down" | "logs.end" => {
            terminal::browse(action)?;
            return Ok(None);
        }
        _ => return Err("unknown UI action; inspect ssmcp.ui.action for supported actions".into()),
    }
    apply(ui, false)
}

/// Called outside the terminal lock by each keyboard frontend.
pub(crate) fn apply_pending() -> Result<Option<(u16, u16)>, String> {
    let actions = terminal::take_actions();
    let theme = PENDING_THEME
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if actions.is_empty() && theme.is_none() {
        return Ok(None);
    }
    let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
    let mut geometry = None;
    for action in actions {
        match action_inner(action) {
            Ok(size) => {
                if size.is_some() {
                    geometry = size;
                }
            }
            Err(error) => {
                terminal::notice(&error);
            }
        }
    }
    if let Some(theme) = theme {
        let mut ui = render::current_ui();
        ui.theme = theme;
        match apply(ui, false) {
            Ok(Some(size)) => geometry = Some(size),
            Ok(None) => {}
            Err(error) => terminal::notice(&error),
        }
    }
    Ok(geometry)
}

const ACTIONS: &[&str] = &[
    "layout.next",
    "sidebar.toggle",
    "theme.next",
    "reload",
    "save",
    "logs.clear",
    "audit.up",
    "audit.down",
    "audit.open",
    "audit.back",
    "logs.page-up",
    "logs.page-down",
    "logs.end",
];

const MAX_PROJECTION_BYTES: usize = 8 * 1024;
const MAX_PROJECTION_LIMIT: usize = 4096;

#[derive(Debug)]
struct GetProjection {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn parse_get(arguments: &Value) -> Result<Option<GetProjection>, String> {
    let args = arguments
        .as_object()
        .ok_or("expected an empty object or a path with optional offset and limit")?;
    if args.is_empty() {
        return Ok(None);
    }
    if args
        .keys()
        .any(|key| !matches!(key.as_str(), "path" | "offset" | "limit"))
    {
        return Err("ui.get accepts only path, offset and limit".into());
    }
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or("ui.get path must be a nonempty JSON Pointer string")?;
    let mut chars = path.chars();
    if chars.next() != Some('/') {
        return Err("ui.get path must be a nonempty JSON Pointer beginning with /".into());
    }
    while let Some(ch) = chars.next() {
        if ch == '~' && !matches!(chars.next(), Some('0' | '1')) {
            return Err("ui.get path must use ~0 for ~ and ~1 for / in object keys".into());
        }
    }
    let number = |name: &str| {
        args.get(name)
            .map(|value| {
                value
                    .as_u64()
                    .and_then(|value| usize::try_from(value).ok())
                    .ok_or_else(|| format!("ui.get {name} must be a nonnegative integer"))
            })
            .transpose()
    };
    let offset = number("offset")?;
    let limit = number("limit")?;
    if limit.is_some_and(|limit| !(1..=MAX_PROJECTION_LIMIT).contains(&limit)) {
        return Err("ui.get limit must be between 1 and 4096".into());
    }
    Ok(Some(GetProjection {
        path: path.into(),
        offset,
        limit,
    }))
}

fn project_snapshot(snapshot: Value, projection: Option<GetProjection>) -> Result<Value, String> {
    let Some(projection) = projection else {
        return Ok(snapshot);
    };
    let selected = snapshot
        .pointer(&projection.path)
        .ok_or("ui.get path does not exist in the current snapshot")?;
    let mut response = json!({"schema_version":1,"path":projection.path});
    let offset = projection.offset.unwrap_or(0);
    let page = match selected {
        Value::Array(values) => Some((values.len(), projection.limit.unwrap_or(32))),
        Value::String(text) => Some((text.chars().count(), projection.limit.unwrap_or(1024))),
        _ if projection.offset.is_some() || projection.limit.is_some() => {
            return Err("ui.get offset and limit are only valid for arrays and strings".into());
        }
        _ => None,
    };
    if let Some((total, limit)) = page {
        if offset > total {
            return Err("ui.get offset exceeds the selected array or string length".into());
        }
        let end = offset.saturating_add(limit).min(total);
        response["value"] = match selected {
            Value::Array(values) => json!(&values[offset..end]),
            Value::String(text) => json!(text.chars().skip(offset).take(limit).collect::<String>()),
            _ => unreachable!("only arrays and strings are paged"),
        };
        response["total"] = json!(total);
        if end < total {
            response["next_offset"] = json!(end);
        }
    } else {
        response["value"] = selected.clone();
    }
    let bytes = serde_json::to_vec(&response).map_err(|_| "cannot serialize UI projection")?;
    if bytes.len() > MAX_PROJECTION_BYTES {
        return Err(
            "ui.get projection exceeds 8 KiB; choose a narrower path or a smaller page limit"
                .into(),
        );
    }
    Ok(response)
}

fn parse_set(current: &Ui, arguments: Value) -> Result<(Ui, bool), String> {
    if !arguments.is_object() {
        return Err("expected patch object and optional persist boolean".into());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Set {
        patch: Value,
        #[serde(default)]
        persist: bool,
    }
    let args: Set = serde_json::from_value(arguments)
        .map_err(|_| "expected patch object and optional persist boolean")?;
    if args.patch.get("dashboard").is_some() || args.patch.get("log_format").is_some() {
        return Err("dashboard and log_format are startup options; edit the configuration file and restart the client".into());
    }
    Ok((current.merge_json(args.patch)?, args.persist))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    #[serde(default, deserialize_with = "present_string")]
    text: Option<String>,
    #[serde(default, deserialize_with = "present_string")]
    level: Option<String>,
}

// Missing optional keys are allowed; explicit null does not satisfy the schema.
fn present_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn parse_filter(arguments: Value) -> Result<Filter, String> {
    if !arguments.is_object() {
        return Err("expected optional text and level strings".into());
    }
    let args: Filter = serde_json::from_value(arguments)
        .map_err(|_| "expected optional text and level strings")?;
    if args
        .text
        .as_ref()
        .is_some_and(|text| text.len() > 256 || text.chars().any(char::is_control))
    {
        return Err("log filter must contain at most 256 bytes of plain text".into());
    }
    if args
        .level
        .as_deref()
        .is_some_and(|level| !matches!(level, "all" | "commands" | "errors"))
    {
        return Err("log level must be all, commands, or errors".into());
    }
    Ok(args)
}

fn parse_action(arguments: Value) -> Result<String, String> {
    if !arguments.is_object() {
        return Err("expected one UI action string".into());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Action {
        action: String,
    }
    let args: Action =
        serde_json::from_value(arguments).map_err(|_| "expected one UI action string")?;
    if !ACTIONS.contains(&args.action.as_str()) {
        return Err("unknown UI action; inspect ssmcp.ui.action for supported actions".into());
    }
    Ok(args.action)
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum QueueRequest {
    Get {},
    Submit {
        text: String,
        #[serde(default, deserialize_with = "present_string")]
        target: Option<String>,
    },
    Takeback {
        id: String,
    },
    Edit {
        id: String,
        text: String,
        #[serde(default, deserialize_with = "present_string")]
        target: Option<String>,
    },
    Cancel {
        id: String,
    },
    Retarget {
        id: String,
        #[serde(default, deserialize_with = "present_string")]
        target: Option<String>,
    },
}

fn parse_queue(arguments: Value) -> Result<QueueRequest, String> {
    if !arguments.is_object() {
        return Err("expected a request queue action object".into());
    }
    let request: QueueRequest =
        serde_json::from_value(arguments).map_err(|_| "invalid request queue arguments")?;
    let (id, text, target) = match &request {
        QueueRequest::Get {} => (None, None, None),
        QueueRequest::Submit { text, target } => (None, Some(text), target.as_ref()),
        QueueRequest::Takeback { id } | QueueRequest::Cancel { id } => (Some(id), None, None),
        QueueRequest::Edit { id, text, target } => (Some(id), Some(text), target.as_ref()),
        QueueRequest::Retarget { id, target } => (Some(id), None, target.as_ref()),
    };
    for value in [id, target].into_iter().flatten() {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err("request and recipient identifiers must be 1..=128 ASCII letters, digits, underscores or hyphens".into());
        }
    }
    if let Some(text) = text {
        super::steering::validate_text(text)?;
    }
    Ok(request)
}

fn queue_call(request: QueueRequest) -> Result<Value, String> {
    let id = match request {
        QueueRequest::Get {} => return Ok(super::steering::snapshot()),
        QueueRequest::Submit { text, target } => super::steering::submit(&text, target.as_deref())?,
        QueueRequest::Takeback { id } => {
            super::steering::take_back(&id)?;
            id
        }
        QueueRequest::Edit { id, text, target } => {
            super::steering::publish_edit(&id, &text, target.as_deref())?;
            id
        }
        QueueRequest::Cancel { id } => {
            super::steering::cancel(&id)?;
            id
        }
        QueueRequest::Retarget { id, target } => {
            super::steering::retarget(&id, target.as_deref())?;
            id
        }
    };
    Ok(json!({"requested":true,"id":id,"queue":super::steering::snapshot()}))
}

pub(crate) async fn call(
    name: &str,
    arguments: Value,
    terminals: &crate::terminals::Terminals,
) -> Result<Value, String> {
    // User-facing messages commit JSONL and derived memory before their ACK.
    // Keep their existing serial order while all storage/display work runs on
    // a blocking worker; no mutex is held across an await.
    if matches!(name, "ssmcp.ui.message" | "ssmcp.ui.present") {
        let presentation = name == "ssmcp.ui.present";
        return tokio::task::spawn_blocking(move || {
            let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
            if presentation {
                super::presentation::send(arguments)
            } else {
                messages::send(arguments)
            }
        })
        .await
        .map_err(|_| "message storage worker stopped".to_owned())?;
    }
    // Memory reads are a projection of the same document, not a second cache.
    // Finish every disk read before taking the UI configuration/update mutex.
    if name == "ssmcp.ui.get" {
        let projection = parse_get(&arguments)?;
        let context_requested = projection
            .as_ref()
            .is_some_and(|p| p.path == "/context" || p.path.starts_with("/context/"));
        let mut snapshot = get();
        if context_requested || projection.is_none() {
            let context = tokio::task::spawn_blocking(super::context_memory::ui_snapshot)
                .await
                .map_err(|_| "context memory worker stopped".to_owned())?;
            match context {
                Ok(context) => snapshot["context"] = context,
                Err(error) if context_requested => return Err(error),
                Err(error) => snapshot["context"] = json!({"available":false,"error":error}),
            }
        }
        return project_snapshot(snapshot, projection);
    }
    let size = {
        let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
        match name {
            "ssmcp.ui.harness" => return harness::call(arguments),
            "ssmcp.ui.history" => return history::call(arguments),
            "ssmcp.ui.ask" => return questions::ask(arguments),
            "ssmcp.ui.questions" => return questions::read(arguments),
            "ssmcp.ui.queue" => return queue_call(parse_queue(arguments)?),
            "ssmcp.ui.set" => {
                let (ui, persist) = parse_set(&render::current_ui(), arguments)?;
                apply(ui, persist)?
            }
            "ssmcp.ui.filter" => {
                let args = parse_filter(arguments)?;
                terminal::filter_logs(args.text.as_deref(), args.level.as_deref())?;
                None
            }
            "ssmcp.ui.action" => action_inner(&parse_action(arguments)?)?,
            _ => return Err("unknown UI primitive".into()),
        }
    };
    if size.is_some() {
        sync_terminal_sizes(terminals).await?;
    }
    Ok(get())
}

/// Serialize PTY updates and read the latest geometry after acquiring the slot.
pub(crate) async fn sync_terminal_sizes(
    terminals: &crate::terminals::Terminals,
) -> Result<(), String> {
    let _resize = RESIZES.lock().await;
    let (rows, cols) = terminal::pane_size(0, 0);
    if rows > 0 && cols > 0 {
        terminals.resize_all(rows, cols).await?;
    }
    Ok(())
}

pub(crate) fn tools() -> Vec<Value> {
    let color = json!({"type":"string","description":"#RRGGBB, ANSI color name, default (terminal color), or empty (inherit theme)."});
    let properties = json!({
        "pet":super::ui::pet::Pet::schema(),
        "theme":{"type":"string","enum":["dark","minimal","codex-dark","nord","solarized","light"]},
        "layout":{"type":"string","enum":["chat","split","audit","logs"]},
        "log_height_percent":{"type":"integer","minimum":10,"maximum":80},
        "sidebar":{"type":"boolean"}, "sidebar_width":{"type":"integer","minimum":20,"maximum":60},
        "panels":{"type":"array","uniqueItems":true,"items":{"type":"string","enum":["agents","queue","controller","tasks","progress","git","history"]}},
        "hide_empty":{"type":"boolean"}, "color":{"type":"boolean"},
        "timestamps":{"type":"boolean"},"show_request_id":{"type":"boolean"},"show_raw_json":{"type":"boolean"},
        "max_inline_output":{"type":"integer","minimum":1,"maximum":10000},
        "activity_interval_secs":{"type":"integer","minimum":0,"maximum":300},
        "colors":{"type":"object","additionalProperties":false,"properties":{
            "background":color,"foreground":color,"muted":color,"accent":color,
            "success":color,"warning":color,"error":color,"heading":color
        }}
    });
    vec![
        queue_tool(),
        json!({"name":"ssmcp.ui.filter","description":"Filter the retained visible log history by case-insensitive text and all/commands/errors. Matching blocks keep their output. Does not alter audit events or running commands.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"text":{"type":"string","maxLength":256,"description":"Plain text, at most 256 UTF-8 bytes; empty clears the text filter."},"level":{"type":"string","enum":["all","commands","errors"]}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        get_tool(),
        json!({"name":"ssmcp.ui.set","description":"Validate and apply a partial display configuration immediately. Colors merge by key; panels replace the ordered list. Set persist:true to save to this client's UI config. Running jobs and terminal contents are retained.","inputSchema":{"type":"object","required":["patch"],"additionalProperties":false,"properties":{"patch":{"type":"object","properties":properties,"additionalProperties":false},"persist":{"type":"boolean","default":false}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.action","description":"Perform one display action. reload validates and reloads appearance only (F5); use ssmcp.client.reload for a process restart. save persists current appearance. logs.clear clears visible history, leaving audit events intact.","inputSchema":{"type":"object","required":["action"],"additionalProperties":false,"properties":{"action":{"type":"string","enum":ACTIONS}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
    ].into_iter().chain(questions::tools())
    .chain(std::iter::once(history::tool()))
    .chain(std::iter::once(harness::tool()))
    .chain(std::iter::once(super::presentation::tool())).collect()
}

fn get_tool() -> Value {
    json!({
        "name":"ssmcp.ui.get",
        "description":"Read versioned display configuration and sanitized client state with or without the built-in TUI. Empty arguments preserve the full snapshot. For bounded polling, use a nonempty JSON Pointer such as /display/conversation, /config, /steering/queue or /display/audit/entries. The /context/status, /context/read and /context/list projections read the same local memory document off the UI thread; use /context/read/summary or /context/read/recent for bounded paging. Projections return {schema_version:1,path,value}; arrays and strings also return total and next_offset when more remains. Page offsets count array items or Unicode code points. Objects and scalars reject paging. Projected responses are limited to 8 KiB serialized; choose a narrower path or smaller page if rejected. Use terminal.*, ssmcp.exec.* and ssmcp.audit.events for independent I/O and events.",
        "inputSchema":{
            "type":"object",
            "additionalProperties":false,
            "properties":{
                "path":{"type":"string","minLength":1,"pattern":"^/(?:[^~]|~[01])*$","description":"RFC 6901 JSON Pointer to an existing snapshot value. Use ~0 for ~ and ~1 for / inside object keys. Empty root paths are not allowed; omit all arguments for the full snapshot."},
                "offset":{"type":"integer","minimum":0,"default":0,"description":"Array item or Unicode code point offset, up to the selected value's total length. Requires path to an array or string."},
                "limit":{"type":"integer","minimum":1,"maximum":MAX_PROJECTION_LIMIT,"description":"Maximum items or Unicode code points per page. Defaults to 32 for arrays and 1024 for strings. Requires path to an array or string; the serialized response must still fit in 8 KiB."}
            },
            "oneOf":[{"maxProperties":0},{"required":["path"]}]
        },
        "annotations":{"readOnlyHint":true,"destructiveHint":false}
    })
}

fn queue_tool() -> Value {
    let identifier =
        json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"});
    let target = json!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$","description":"next or an exact current agent ID. submit defaults to next; edit keeps its current recipient; retarget defaults explicitly to next."});
    let text = json!({"type":"string","minLength":1,"maxLength":16384,"description":"Request text: at most 16384 UTF-8 bytes. Newline and tab are the only allowed control characters."});
    let variant = |action: &str, required: &[&str], mut properties: Value| {
        properties["action"] = json!({"const":action});
        let required = std::iter::once("action")
            .chain(required.iter().copied())
            .collect::<Vec<_>>();
        json!({"type":"object","required":required,"additionalProperties":false,"properties":properties})
    };
    json!({
        "name":"ssmcp.ui.queue",
        "description":"Read or change this client's local request queue without starting a model. submit and edit request asynchronous publication; read state until acknowledged. takeback reclaims a still-queued request using its current revision before editing the same ID. Delivery can win that race; delivered requests cannot be edited or cancelled. A changed room or uncertain delivery requires explicit retarget, then edit to republish. Agents are the actual public collaboration roster. Requests are included at the next eligible tool response, not evidence that an agent has read or executed them.",
        "inputSchema":{"type":"object","oneOf":[
            variant("get", &[], json!({})),
            variant("submit", &["text"], json!({"text":text,"target":target})),
            variant("takeback", &["id"], json!({"id":identifier})),
            variant("edit", &["id","text"], json!({"id":identifier,"text":text,"target":target})),
            variant("cancel", &["id"], json!({"id":identifier})),
            variant("retarget", &["id"], json!({"id":identifier,"target":target}))
        ]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saving_appearance_preserves_the_configured_relay() {
        let existing = "[client]\nserver='https://relay.example.com'\n[ui]\ntheme='nord'\n";
        let text = document_with_ui(&Ui::default(), Some(existing)).unwrap();
        let doc: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            doc["client"]["server"].as_str(),
            Some("https://relay.example.com")
        );
        assert_eq!(Ui::parse(&text).unwrap(), Ui::default());
    }

    #[test]
    fn get_advertises_empty_snapshot_requests_and_bounded_projections() {
        assert!(parse_get(&json!({})).unwrap().is_none());
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == "ssmcp.ui.get")
            .unwrap();
        assert_eq!(schema["annotations"]["readOnlyHint"], true);
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            schema["inputSchema"]["oneOf"],
            json!([{"maxProperties":0},{"required":["path"]}])
        );
        let properties = &schema["inputSchema"]["properties"];
        assert_eq!(properties["path"]["minLength"], 1);
        assert_eq!(properties["offset"]["minimum"], 0);
        assert_eq!(properties["limit"]["minimum"], 1);
        assert_eq!(properties["limit"]["maximum"], MAX_PROJECTION_LIMIT);
    }

    fn read_projection(snapshot: &Value, arguments: Value) -> Result<Value, String> {
        project_snapshot(snapshot.clone(), parse_get(&arguments)?)
    }

    #[test]
    fn get_projection_excludes_large_unrelated_audit_history() {
        let snapshot = json!({
            "schema_version":1,
            "config":{"theme":"dark"},
            "display":{
                "conversation":{"view":"activity","composer":{"text":"draft","cursor":5}},
                "audit":{"entries":[{"code":"x".repeat(40_000)}]}
            }
        });
        assert!(serde_json::to_vec(&snapshot).unwrap().len() > 32 * 1024);
        assert_eq!(read_projection(&snapshot, json!({})).unwrap(), snapshot);
        let projected =
            read_projection(&snapshot, json!({"path":"/display/conversation"})).unwrap();
        assert_eq!(
            projected,
            json!({
                "schema_version":1,
                "path":"/display/conversation",
                "value":{"view":"activity","composer":{"text":"draft","cursor":5}}
            })
        );
        assert!(serde_json::to_vec(&projected).unwrap().len() < MAX_PROJECTION_BYTES);
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/config"})).unwrap()["value"],
            snapshot["config"]
        );
    }

    #[test]
    fn get_array_pages_report_total_and_only_a_real_next_offset() {
        let snapshot = json!({"queue":(0..40).collect::<Vec<_>>(),"empty":[]});
        let first = read_projection(&snapshot, json!({"path":"/queue"})).unwrap();
        assert_eq!(first["value"], json!((0..32).collect::<Vec<_>>()));
        assert_eq!(first["total"], 40);
        assert_eq!(first["next_offset"], 32);
        let last =
            read_projection(&snapshot, json!({"path":"/queue","offset":32,"limit":4096})).unwrap();
        assert_eq!(last["value"], json!((32..40).collect::<Vec<_>>()));
        assert_eq!(last["total"], 40);
        assert!(last.get("next_offset").is_none());
        let exhausted =
            read_projection(&snapshot, json!({"path":"/queue","offset":40,"limit":1})).unwrap();
        assert_eq!(exhausted["value"], json!([]));
        assert_eq!(exhausted["total"], 40);
        assert!(exhausted.get("next_offset").is_none());
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/empty"})).unwrap(),
            json!({"schema_version":1,"path":"/empty","value":[],"total":0})
        );
    }

    #[test]
    fn get_string_pages_count_unicode_code_points_and_preserve_exact_text() {
        let snapshot = json!({"text":"A界🦀e\u{301}Z","long":"界".repeat(1100)});
        let page =
            read_projection(&snapshot, json!({"path":"/text","offset":1,"limit":3})).unwrap();
        assert_eq!(
            page,
            json!({"schema_version":1,"path":"/text","value":"界🦀e","total":6,"next_offset":4})
        );
        let combining =
            read_projection(&snapshot, json!({"path":"/text","offset":4,"limit":1})).unwrap();
        assert_eq!(combining["value"], "\u{301}");
        assert_eq!(combining["next_offset"], 5);
        let exhausted = read_projection(&snapshot, json!({"path":"/text","offset":6})).unwrap();
        assert_eq!(exhausted["value"], "");
        assert_eq!(exhausted["total"], 6);
        assert!(exhausted.get("next_offset").is_none());
        let default = read_projection(&snapshot, json!({"path":"/long"})).unwrap();
        assert_eq!(default["value"], "界".repeat(1024));
        assert_eq!(default["total"], 1100);
        assert_eq!(default["next_offset"], 1024);
    }

    #[test]
    fn get_rejects_invalid_argument_types_and_pointer_syntax_without_echoing_input() {
        for arguments in [
            Value::Null,
            json!([]),
            json!(["/config", 0, 32]),
            json!("private-fixture"),
            json!({"extra":"private-fixture"}),
            json!({"offset":0}),
            json!({"limit":32}),
            json!({"path":null}),
            json!({"path":17}),
            json!({"path":""}),
            json!({"path":"private-fixture"}),
            json!({"path":"#/config"}),
            json!({"path":"/private-fixture~"}),
            json!({"path":"/~2private-fixture"}),
            json!({"path":"/config","extra":"private-fixture"}),
            json!({"path":"/queue","offset":null}),
            json!({"path":"/queue","offset":-1}),
            json!({"path":"/queue","offset":1.5}),
            json!({"path":"/queue","offset":"private-fixture"}),
            json!({"path":"/queue","limit":null}),
            json!({"path":"/queue","limit":false}),
            json!({"path":"/queue","limit":1.5}),
            json!({"path":"/queue","limit":"private-fixture"}),
            json!({"path":"/queue","limit":0}),
            json!({"path":"/queue","limit":4097}),
        ] {
            let error = parse_get(&arguments).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
    }

    #[test]
    fn get_resolves_pointer_escapes_and_rejects_missing_paths_and_invalid_paging() {
        let snapshot = json!({
            "":{"present":true},
            "a/b":{"~name":7,"~1":true},
            "object":{"present":true},
            "number":7,
            "boolean":false,
            "nothing":null,
            "array":["one"],
            "text":"界"
        });
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/a~1b/~0name"})).unwrap(),
            json!({"schema_version":1,"path":"/a~1b/~0name","value":7})
        );
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/a~1b/~01"})).unwrap()["value"],
            true
        );
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/"})).unwrap()["value"],
            json!({"present":true})
        );
        assert_eq!(
            read_projection(&snapshot, json!({"path":"/nothing"})).unwrap(),
            json!({"schema_version":1,"path":"/nothing","value":null})
        );
        for arguments in [
            json!({"path":"/missing/private-fixture"}),
            json!({"path":"/array/01"}),
            json!({"path":"/array/-"}),
            json!({"path":"/object","offset":0}),
            json!({"path":"/object","limit":1}),
            json!({"path":"/number","offset":0}),
            json!({"path":"/boolean","limit":1}),
            json!({"path":"/nothing","offset":0}),
            json!({"path":"/array","offset":2}),
            json!({"path":"/text","offset":2}),
            json!({"path":"/array","offset":u64::MAX}),
        ] {
            let error = read_projection(&snapshot, arguments).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
    }

    #[test]
    fn get_rejects_oversized_serialized_projections_and_never_truncates_objects() {
        let snapshot = json!({
            "object":{"text":"private-fixture".repeat(1000)},
            "items":[{"text":"x".repeat(5000)},{"text":"y".repeat(5000)}],
            "unicode":"🦀".repeat(2200),
            "escaped":"\0".repeat(1400)
        });
        for arguments in [
            json!({"path":"/object"}),
            json!({"path":"/items"}),
            json!({"path":"/unicode","limit":2200}),
            json!({"path":"/escaped","limit":1400}),
        ] {
            let error = read_projection(&snapshot, arguments).unwrap_err();
            assert!(error.contains("8 KiB"));
            assert!(error.contains("narrower path"));
            assert!(error.contains("smaller page"));
            assert!(!error.contains("private-fixture"));
        }
        let page = read_projection(&snapshot, json!({"path":"/items","limit":1})).unwrap();
        assert_eq!(page["value"], json!([snapshot["items"][0]]));
        assert_eq!(page["total"], 2);
        assert_eq!(page["next_offset"], 1);
        let text = read_projection(&snapshot, json!({"path":"/unicode","limit":1000})).unwrap();
        assert_eq!(text["value"], "🦀".repeat(1000));
        assert_eq!(text["next_offset"], 1000);
        assert!(serde_json::to_vec(&text).unwrap().len() <= MAX_PROJECTION_BYTES);
    }

    #[test]
    fn pet_schema_set_and_saved_configuration_use_the_same_validated_object() {
        use super::super::ui::pet::{Pet, Preset};
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == "ssmcp.ui.set")
            .unwrap();
        assert_eq!(
            schema["inputSchema"]["properties"]["patch"]["properties"]["pet"],
            Pet::schema()
        );
        let drawing = " /\\_/\\\n( o.o )\n > ^ <";
        let (updated, persist) = parse_set(
            &Ui::default(),
            json!({"patch":{"pet":{"preset":"custom","text":drawing}},"persist":true}),
        )
        .unwrap();
        assert!(persist);
        assert_eq!(updated.pet.preset, Preset::Custom);
        assert_eq!(
            serde_json::to_value(&updated).unwrap()["pet"]["text"],
            drawing
        );
        let saved = document_with_ui(
            &updated,
            Some("[client]\nserver = 'https://relay.example'\n"),
        )
        .unwrap();
        assert!(saved.contains("https://relay.example"));
        assert_eq!(Ui::parse(&saved).unwrap().pet, updated.pet);

        let path = std::env::temp_dir().join(format!(
            "ssmcp-pet-config-{}-{}.toml",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, &saved).unwrap();
        let loaded = Ui::load(Some(&path));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.unwrap().pet, updated.pet);
        let (disabled, _) = parse_set(&updated, json!({"patch":{"pet":{"preset":"off"}}})).unwrap();
        assert_eq!(disabled.pet.preset, Preset::Off);
        assert_eq!(disabled.pet.text, drawing);
        assert!(parse_set(&updated, json!({"patch":{"pet":{"text":"\u{1b}[2J"}}}),).is_err());
        assert_eq!(updated.pet.text, drawing);
    }

    #[test]
    fn partial_set_is_validated_before_any_configuration_is_replaced() {
        let current = Ui::default()
            .merge_json(json!({"colors":{"accent":"#123456"},"sidebar_width":36}))
            .unwrap();
        let original = current.clone();
        let (updated, persist) = parse_set(
            &current,
            json!({"patch":{"colors":{"background":"#010203"},"panels":["tasks","git"]}}),
        )
        .unwrap();
        assert!(!persist);
        assert_eq!(updated.colors.accent, "#123456");
        assert_eq!(updated.colors.background, "#010203");
        assert_eq!(updated.sidebar_width, 36);
        assert_eq!(
            updated.panels,
            vec![super::super::ui::Panel::Tasks, super::super::ui::Panel::Git]
        );
        assert_eq!(current, original);
        assert!(
            parse_set(&current, json!({"patch":{},"persist":true}))
                .unwrap()
                .1
        );
        for value in [
            Value::Null,
            json!([]),
            json!([{}, false]),
            json!({}),
            json!({"patch":null}),
            json!({"patch":[]}),
            json!({"patch":{},"persist":null}),
            json!({"patch":{},"persist":"private-fixture"}),
            json!({"patch":{},"extra":true}),
            json!({"patch":{"unknown":"private-fixture"}}),
            json!({"patch":{"colors":{"accent":"\u{1b}[2Jprivate-fixture"}}}),
            json!({"patch":{"sidebar_width":1}}),
            json!({"patch":{"panels":["git","git"]}}),
        ] {
            let error = parse_set(&current, value).unwrap_err();
            assert!(!error.contains("private-fixture"));
            assert_eq!(current, original);
        }
    }

    #[test]
    fn set_rejects_startup_options_even_if_their_values_are_unchanged() {
        let current = Ui::default();
        for patch in [
            json!({"dashboard":true}),
            json!({"dashboard":false}),
            json!({"log_format":"pretty"}),
            json!({"log_format":"jsonl"}),
            json!({"colors":{"accent":"red"},"dashboard":null}),
        ] {
            let error = parse_set(&current, json!({"patch":patch})).unwrap_err();
            assert!(error.contains("startup options"));
        }
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == "ssmcp.ui.set")
            .unwrap();
        let patch = &schema["inputSchema"]["properties"]["patch"];
        assert_eq!(patch["additionalProperties"], false);
        assert!(patch["properties"].get("dashboard").is_none());
        assert!(patch["properties"].get("log_format").is_none());
    }

    #[test]
    fn advertised_appearance_enums_and_ranges_match_runtime_validation() {
        let schema = tools()
            .into_iter()
            .find(|tool| tool["name"] == "ssmcp.ui.set")
            .unwrap();
        let properties = &schema["inputSchema"]["properties"]["patch"]["properties"];
        let current = Ui::default();
        for name in ["theme", "layout"] {
            for value in properties[name]["enum"].as_array().unwrap() {
                let mut patch = serde_json::Map::new();
                patch.insert(name.into(), value.clone());
                assert!(parse_set(&current, json!({"patch":patch})).is_ok());
            }
        }
        for name in [
            "log_height_percent",
            "sidebar_width",
            "max_inline_output",
            "activity_interval_secs",
        ] {
            let minimum = properties[name]["minimum"].as_i64().unwrap();
            let maximum = properties[name]["maximum"].as_i64().unwrap();
            for (value, accepted) in [
                (minimum - 1, false),
                (minimum, true),
                (maximum, true),
                (maximum + 1, false),
            ] {
                let mut patch = serde_json::Map::new();
                patch.insert(name.into(), json!(value));
                assert_eq!(
                    parse_set(&current, json!({"patch":patch})).is_ok(),
                    accepted,
                    "{name}={value}"
                );
            }
        }
        assert_eq!(properties["colors"]["additionalProperties"], false);
    }

    #[test]
    fn filter_arguments_are_plain_bounded_strings_and_explicit_null_is_rejected() {
        let filter = parse_filter(json!({"text":"编译 cargo", "level":"commands"})).unwrap();
        assert_eq!(filter.text.as_deref(), Some("编译 cargo"));
        assert_eq!(filter.level.as_deref(), Some("commands"));
        let clear = parse_filter(json!({"text":"", "level":"all"})).unwrap();
        assert_eq!(clear.text.as_deref(), Some(""));
        assert!(parse_filter(json!({})).is_ok());
        for value in [
            Value::Null,
            json!([]),
            json!(["cargo", "all"]),
            json!({"text":null}),
            json!({"level":null}),
            json!({"text":123}),
            json!({"level":"debug"}),
            json!({"extra":"private-fixture"}),
            json!({"text":"private-fixture\n"}),
            json!({"text":"\u{1b}[2Jprivate-fixture"}),
            json!({"text":"界".repeat(86)}),
        ] {
            let error = parse_filter(value).err().expect("invalid filter accepted");
            assert!(!error.contains("private-fixture"));
        }
    }

    #[test]
    fn action_decoder_validates_the_catalog_without_executing_actions() {
        let catalog = tools();
        let mut names = std::collections::BTreeSet::new();
        for tool in &catalog {
            assert!(names.insert(tool["name"].as_str().unwrap()));
        }
        assert_eq!(names.len(), 11);
        assert!(names.contains("ssmcp.ui.harness"));
        assert!(names.contains("ssmcp.ui.history"));
        for name in [
            "ssmcp.ui.ask",
            "ssmcp.ui.questions",
            "ssmcp.ui.message",
            "ssmcp.ui.present",
        ] {
            assert!(names.contains(name));
        }
        let schema = catalog
            .iter()
            .find(|tool| tool["name"] == "ssmcp.ui.action")
            .unwrap();
        for action in schema["inputSchema"]["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
        {
            assert_eq!(
                parse_action(json!({"action":action})).unwrap(),
                action.as_str().unwrap()
            );
        }
        for value in [
            Value::Null,
            json!([]),
            json!(["save"]),
            json!({}),
            json!({"action":null}),
            json!({"action":"private-fixture"}),
            json!({"action":"save","extra":true}),
        ] {
            let error = parse_action(value).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
    }

    #[test]
    fn queue_arguments_are_atomic_actions_with_bounded_text_and_exact_identifiers() {
        for value in [
            json!({"action":"get"}),
            json!({"action":"submit","text":"改进客户端\n保持草稿"}),
            json!({"action":"takeback","id":"request_1"}),
            json!({"action":"edit","id":"request_1","text":"edited","target":"agent_1"}),
            json!({"action":"cancel","id":"request_1"}),
            json!({"action":"retarget","id":"request_1"}),
        ] {
            assert!(parse_queue(value).is_ok());
        }
        for value in [
            json!([]),
            json!({}),
            json!({"action":"get","id":"private-fixture"}),
            json!({"action":"submit","text":" "}),
            json!({"action":"submit","text":"private-fixture\u{1b}"}),
            json!({"action":"submit","text":"x".repeat(16385)}),
            json!({"action":"submit","text":"valid","target":null}),
            json!({"action":"edit","id":"request_1","text":"valid","target":"bad target"}),
            json!({"action":"cancel","id":""}),
            json!({"action":"retarget","id":1}),
            json!({"action":"retarget","id":"request_1","target":null}),
        ] {
            let error = parse_queue(value).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
        let schema = queue_tool();
        assert_eq!(schema["inputSchema"]["oneOf"].as_array().unwrap().len(), 6);
        for variant in schema["inputSchema"]["oneOf"].as_array().unwrap() {
            assert_eq!(variant["additionalProperties"], false);
        }
    }
}
