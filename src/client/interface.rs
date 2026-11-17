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
static RESIZES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        "audit.up"|"audit.down"|"audit.open"|"audit.back"|"logs.page-up"|"logs.page-down"|"logs.end" => { terminal::browse(action)?; return Ok(None); }
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

const ACTIONS: &[&str] = &[
    "layout.next", "sidebar.toggle", "theme.next", "reload", "save", "logs.clear",
    "audit.up", "audit.down", "audit.open", "audit.back", "logs.page-up", "logs.page-down", "logs.end",
];

fn validate_get(arguments: &Value) -> Result<(), String> {
    if !arguments.as_object().is_some_and(|args| args.is_empty()) {
        return Err("ssmcp.ui.get takes no arguments".into());
    }
    Ok(())
}

fn parse_set(current: &Ui, arguments: Value) -> Result<(Ui, bool), String> {
    if !arguments.is_object() {
        return Err("expected patch object and optional persist boolean".into());
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Set { patch: Value, #[serde(default)] persist: bool }
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
fn present_string<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

fn parse_filter(arguments: Value) -> Result<Filter, String> {
    if !arguments.is_object() {
        return Err("expected optional text and level strings".into());
    }
    let args: Filter = serde_json::from_value(arguments)
        .map_err(|_| "expected optional text and level strings")?;
    if args.text.as_ref().is_some_and(|text| text.len() > 256 || text.chars().any(char::is_control)) {
        return Err("log filter must contain at most 256 bytes of plain text".into());
    }
    if args.level.as_deref().is_some_and(|level| !matches!(level, "all" | "commands" | "errors")) {
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
    struct Action { action: String }
    let args: Action = serde_json::from_value(arguments).map_err(|_| "expected one UI action string")?;
    if !ACTIONS.contains(&args.action.as_str()) {
        return Err("unknown UI action; inspect ssmcp.ui.action for supported actions".into());
    }
    Ok(args.action)
}

pub(crate) async fn call(name: &str, arguments: Value, terminals: &crate::terminals::Terminals) -> Result<Value, String> {
    let size = {
        let _updates = UPDATES.lock().unwrap_or_else(|e| e.into_inner());
        match name {
            "ssmcp.ui.get" => {
                validate_get(&arguments)?;
                return Ok(get());
            }
            "ssmcp.ui.set" => {
                let (ui, persist) = parse_set(&render::current_ui(), arguments)?;
                apply(ui, persist)?
            }
            "ssmcp.ui.filter" => {
                let args = parse_filter(arguments)?;
                terminal::filter_logs(args.text.as_deref(),args.level.as_deref())?;
                None
            }
            "ssmcp.ui.action" => {
                action_inner(&parse_action(arguments)?)?
            }
            _ => return Err("unknown UI primitive".into()),
        }
    };
    if size.is_some() { sync_terminal_sizes(terminals).await?; }
    Ok(get())
}

/// Serialize PTY updates and read the latest geometry after acquiring the slot.
pub(crate) async fn sync_terminal_sizes(terminals: &crate::terminals::Terminals) -> Result<(), String> {
    let _resize = RESIZES.lock().await;
    let (rows, cols) = terminal::pane_size(0, 0);
    if rows > 0 && cols > 0 { terminals.resize_all(rows, cols).await?; }
    Ok(())
}

pub(crate) fn tools() -> Vec<Value> {
    let color = json!({"type":"string","description":"#RRGGBB, ANSI color name, default (terminal color), or empty (inherit theme)."});
    let properties = json!({
        "theme":{"type":"string","enum":["minimal","codex-dark","nord","solarized","light"]},
        "layout":{"type":"string","enum":["split","audit","logs"]},
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
        json!({"name":"ssmcp.ui.filter","description":"Filter the retained visible log history by case-insensitive text and all/commands/errors. Matching blocks keep their output. Does not alter audit events or running commands.","inputSchema":{"type":"object","additionalProperties":false,"properties":{"text":{"type":"string","maxLength":256,"description":"Plain text, at most 256 UTF-8 bytes; empty clears the text filter."},"level":{"type":"string","enum":["all","commands","errors"]}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.get","description":"Read versioned display configuration, layout and sanitized controller/task/progress/Git state. Works with or without the built-in TUI. Use terminal.*, ssmcp.exec.* and ssmcp.audit.events for independent I/O and events.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.set","description":"Validate and apply a partial display configuration immediately. Colors merge by key; panels replace the ordered list. Set persist:true to save to this client's UI config. Running jobs and terminal contents are retained.","inputSchema":{"type":"object","required":["patch"],"additionalProperties":false,"properties":{"patch":{"type":"object","properties":properties,"additionalProperties":false},"persist":{"type":"boolean","default":false}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
        json!({"name":"ssmcp.ui.action","description":"Perform one display action. reload validates the saved configuration before applying it; save persists current appearance. logs.clear clears only the visible log history, leaving audit events intact.","inputSchema":{"type":"object","required":["action"],"additionalProperties":false,"properties":{"action":{"type":"string","enum":ACTIONS}}},"annotations":{"readOnlyHint":false,"destructiveHint":false}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_accepts_only_the_declared_empty_object() {
        assert!(validate_get(&json!({})).is_ok());
        for value in [Value::Null, json!([]), json!(""), json!({"extra":"private-fixture"})] {
            let error = validate_get(&value).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
        let schema = tools().into_iter().find(|tool| tool["name"] == "ssmcp.ui.get").unwrap();
        assert_eq!(schema["annotations"]["readOnlyHint"], true);
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
        assert_eq!(schema["inputSchema"]["properties"], json!({}));
    }

    #[test]
    fn partial_set_is_validated_before_any_configuration_is_replaced() {
        let current = Ui::default().merge_json(json!({"colors":{"accent":"#123456"},"sidebar_width":36})).unwrap();
        let original = current.clone();
        let (updated, persist) = parse_set(&current, json!({"patch":{"colors":{"background":"#010203"},"panels":["tasks","git"]}})).unwrap();
        assert!(!persist);
        assert_eq!(updated.colors.accent, "#123456");
        assert_eq!(updated.colors.background, "#010203");
        assert_eq!(updated.sidebar_width, 36);
        assert_eq!(updated.panels, vec![super::super::ui::Panel::Tasks, super::super::ui::Panel::Git]);
        assert_eq!(current, original);
        assert!(parse_set(&current, json!({"patch":{},"persist":true})).unwrap().1);
        for value in [
            Value::Null, json!([]), json!([{}, false]), json!({}), json!({"patch":null}), json!({"patch":[]}),
            json!({"patch":{},"persist":null}), json!({"patch":{},"persist":"private-fixture"}),
            json!({"patch":{},"extra":true}), json!({"patch":{"unknown":"private-fixture"}}),
            json!({"patch":{"colors":{"accent":"\u{1b}[2Jprivate-fixture"}}}),
            json!({"patch":{"sidebar_width":1}}), json!({"patch":{"panels":["git","git"]}}),
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
            json!({"dashboard":true}), json!({"dashboard":false}),
            json!({"log_format":"pretty"}), json!({"log_format":"jsonl"}),
            json!({"colors":{"accent":"red"},"dashboard":null}),
        ] {
            let error = parse_set(&current, json!({"patch":patch})).unwrap_err();
            assert!(error.contains("startup options"));
        }
        let schema = tools().into_iter().find(|tool| tool["name"] == "ssmcp.ui.set").unwrap();
        let patch = &schema["inputSchema"]["properties"]["patch"];
        assert_eq!(patch["additionalProperties"], false);
        assert!(patch["properties"].get("dashboard").is_none());
        assert!(patch["properties"].get("log_format").is_none());
    }

    #[test]
    fn advertised_appearance_enums_and_ranges_match_runtime_validation() {
        let schema = tools().into_iter().find(|tool| tool["name"] == "ssmcp.ui.set").unwrap();
        let properties = &schema["inputSchema"]["properties"]["patch"]["properties"];
        let current = Ui::default();
        for name in ["theme", "layout"] {
            for value in properties[name]["enum"].as_array().unwrap() {
                let mut patch = serde_json::Map::new();
                patch.insert(name.into(), value.clone());
                assert!(parse_set(&current, json!({"patch":patch})).is_ok());
            }
        }
        for name in ["log_height_percent", "sidebar_width", "max_inline_output", "activity_interval_secs"] {
            let minimum = properties[name]["minimum"].as_i64().unwrap();
            let maximum = properties[name]["maximum"].as_i64().unwrap();
            for (value, accepted) in [(minimum - 1, false), (minimum, true), (maximum, true), (maximum + 1, false)] {
                let mut patch = serde_json::Map::new();
                patch.insert(name.into(), json!(value));
                assert_eq!(parse_set(&current, json!({"patch":patch})).is_ok(), accepted, "{name}={value}");
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
            Value::Null, json!([]), json!(["cargo", "all"]), json!({"text":null}), json!({"level":null}),
            json!({"text":123}), json!({"level":"debug"}), json!({"extra":"private-fixture"}),
            json!({"text":"private-fixture\n"}), json!({"text":"\u{1b}[2Jprivate-fixture"}),
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
        for tool in &catalog { assert!(names.insert(tool["name"].as_str().unwrap())); }
        assert_eq!(names.len(), 4);
        let schema = catalog.iter().find(|tool| tool["name"] == "ssmcp.ui.action").unwrap();
        for action in schema["inputSchema"]["properties"]["action"]["enum"].as_array().unwrap() {
            assert_eq!(parse_action(json!({"action":action})).unwrap(), action.as_str().unwrap());
        }
        for value in [Value::Null, json!([]), json!(["save"]), json!({}), json!({"action":null}), json!({"action":"private-fixture"}), json!({"action":"save","extra":true})] {
            let error = parse_action(value).unwrap_err();
            assert!(!error.contains("private-fixture"));
        }
    }
}
