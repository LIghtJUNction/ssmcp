//! Display primitives shared by the built-in terminal and alternate frontends.
use super::{render, terminal, ui::Ui};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

static CONFIG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
static UPDATES: Mutex<()> = Mutex::new(());

pub(crate) fn initialize_path(explicit: Option<&Path>) {
    let path = Ui::config_path(explicit).map(|path| {
        if path.is_absolute() { path } else {
            std::env::current_dir().unwrap_or_default().join(path)
        }
    });
    *CONFIG_PATH.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner()) = path;
}

fn config_path() -> Option<PathBuf> {
    CONFIG_PATH.get().and_then(|path| path.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .or_else(|| Ui::config_path(None))
}

pub(crate) fn get() -> Value {
    json!({
        "schema_version": 1,
        "config": render::current_ui(),
        "config_path": config_path(),
        "display": terminal::snapshot(),
        "activity": render::snapshot(),
    })
}

fn save(ui: &Ui) -> Result<(), String> {
    let path = config_path().ok_or("UI configuration path is unavailable; use --ui-config FILE")?;
    // A display command must not replace an unrelated or malformed document.
    if path.exists() { Ui::load(Some(&path))?; }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).map_err(|_| "cannot create UI configuration directory")?;
    let data = ui.to_toml()?;
    let temporary = parent.join(format!(".ssmcp-ui-{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|_| "cannot create temporary UI configuration")?;
    let result = (|| {
        file.write_all(data.as_bytes()).map_err(|_| "cannot write UI configuration")?;
        file.sync_all().map_err(|_| "cannot flush UI configuration")?;
        drop(file);
        std::fs::rename(&temporary, &path).map_err(|_| "cannot replace UI configuration")
    })();
    if result.is_err() { let _ = std::fs::remove_file(&temporary); }
    result.map_err(str::to_owned)
}

fn apply(ui: Ui, persist: bool) -> Result<Option<(u16, u16)>, String> {
    ui.validate()?;
    let current = render::current_ui();
    if terminal::active() && (ui.dashboard != current.dashboard || ui.log_format != current.log_format) {
        return Err("dashboard and log_format are startup options; change the file and restart the client".into());
    }
    if persist { save(&ui)?; }
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
        "save" => {
            save(&ui)?;
            terminal::notice("Appearance saved · F5 Reload");
            return Ok(None);
        }
        "logs.clear" => { terminal::clear_logs(); return Ok(None); }
        _ => return Err("unknown UI action; inspect ssmcp.ui.action for supported actions".into()),
    }
    apply(ui, false)
}

/// Called outside the terminal lock by each keyboard frontend.
pub(crate) fn apply_pending() -> Result<Option<(u16, u16)>, String> {
    let actions = terminal::take_actions();
    if actions.is_empty() { return Ok(None); }
    let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
    let mut geometry = None;
    for action in actions {
        match action_inner(action) {
            Ok(size) => { if size.is_some() { geometry = size; } }
            Err(error) => { terminal::notice(&error); }
        }
    }
    Ok(geometry)
}

pub(crate) async fn call(name: &str, arguments: Value, terminals: &crate::terminals::Terminals) -> Result<Value, String> {
    let size = {
        let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
        match name {
            "ssmcp.ui.get" => {
                if !arguments.as_object().is_some_and(|args| args.is_empty()) {
                    return Err("ssmcp.ui.get takes no arguments".into());
                }
                return Ok(get());
            }
            "ssmcp.ui.set" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Set { patch: Value, #[serde(default)] persist: bool }
                let args: Set = serde_json::from_value(arguments).map_err(|_| "expected patch object and optional persist boolean")?;
                let ui = render::current_ui().merge_json(args.patch)?;
                apply(ui, args.persist)?
            }
            "ssmcp.ui.action" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Action { action: String }
                let args: Action = serde_json::from_value(arguments).map_err(|_| "expected one UI action string")?;
                action_inner(&args.action)?
            }
            _ => return Err("unknown UI primitive".into()),
        }
    };
    if let Some((rows, cols)) = size { terminals.resize_all(rows, cols).await?; }
    Ok(get())
}

pub(crate) fn tools() -> Vec<Value> {
    let color = json!({"type":"string","description":"#RRGGBB, ANSI color name, default (terminal color), or empty (inherit theme)."});
    let properties = json!({
        "theme":{"type":"string","enum":["minimal","codex-dark","nord","solarized","light"]},
        "layout":{"type":"string","enum":["split","shell","logs"]},
        "log_height_percent":{"type":"integer","minimum":10,"maximum":80},
        "sidebar":{"type":"boolean"}, "sidebar_width":{"type":"integer","minimum":20,"maximum":60},
        "panels":{"type":"array","uniqueItems":true,"items":{"type":"string","enum":["controller","tasks","progress","git","history"]}},
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
        json!({"name":"ssmcp.ui.get","description":"Read versioned display configuration, layout and sanitized controller/task/progress/Git state. Works with or without the built-in TUI. Use terminal.*, ssmcp.exec.* and ssmcp.audit.events for independent I/O and events.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.set","description":"Validate and apply a partial display configuration immediately. Colors merge by key; panels replace the ordered list. Set persist:true to save to this client's UI config. Running jobs and terminal contents are retained.","inputSchema":{"type":"object","required":["patch"],"additionalProperties":false,"properties":{"patch":{"type":"object","properties":properties,"additionalProperties":false},"persist":{"type":"boolean","default":false}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.action","description":"Perform one display action. reload validates the saved configuration before applying it; save persists current appearance. logs.clear clears only the visible log history, leaving audit events intact.","inputSchema":{"type":"object","required":["action"],"additionalProperties":false,"properties":{"action":{"type":"string","enum":["layout.next","sidebar.toggle","theme.next","reload","save","logs.clear"]}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
    ]
}
