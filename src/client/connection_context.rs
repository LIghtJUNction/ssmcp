//! Named connection targets. Contexts never contain credentials or session state.
use super::{account, config};
use crate::Result;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    path::Path,
};
use toml_edit::{DocumentMut, Item, Table, value};

const HELP: &str = "\
ssmcp context create NAME --local       Save a local Linux connection
ssmcp context create NAME --server URL  Save an HTTPS relay origin
ssmcp context list [--json]             List saved connection targets
ssmcp context show [--json]             Show the effective target and its source
ssmcp context inspect [NAME] [--json]   Inspect a named or effective target
ssmcp context use NAME                 Select the default for new connections
ssmcp context use --default            Restore the existing server defaults
ssmcp context rm NAME                  Remove an unused context

Select once: ssmcp --context NAME [connection options]
Priority: --context > SSMCP_CONTEXT > context use > existing server defaults.
Explicit --server, local or username@server overrides environment and saved defaults.
Do not combine --context with another explicit connection target.
Contexts contain only local or HTTPS endpoints. Account credentials remain origin-scoped.
Switching contexts does not move running jobs or retarget saved sessions.";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    endpoint: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Store {
    current: Option<String>,
    entries: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Endpoint {
    Local,
    Https(String),
}

impl Endpoint {
    fn parse(raw: &str) -> Result<Self> {
        if raw == "local" {
            return Ok(Self::Local);
        }
        let origin = account::normalize_server(raw)?;
        if !origin.starts_with("https://") {
            return Err("a connection context requires local or an HTTPS origin".into());
        }
        Ok(Self::Https(origin))
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Local => "local",
            Self::Https(origin) => origin,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Selection {
    pub name: String,
    pub endpoint: Endpoint,
    pub source: &'static str,
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
    {
        return Err("context names must start with a letter or digit and contain 1..64 ASCII letters, digits, dots, underscores or hyphens".into());
    }
    Ok(())
}

pub(super) fn set_name(slot: &mut Option<String>, name: &str) -> Result<()> {
    if slot.is_some() {
        return Err("--context may be supplied only once".into());
    }
    validate_name(name)?;
    *slot = Some(name.to_owned());
    Ok(())
}

impl Store {
    pub(super) fn validate(&self) -> Result<()> {
        for (name, entry) in &self.entries {
            validate_name(name)?;
            Endpoint::parse(&entry.endpoint)?;
        }
        if let Some(name) = &self.current {
            self.named(name, "current")?;
        }
        Ok(())
    }

    fn named(&self, name: &str, source: &'static str) -> Result<Selection> {
        validate_name(name)?;
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| format!("unknown connection context: {name}"))?;
        Ok(Selection {
            name: name.to_owned(),
            endpoint: Endpoint::parse(&entry.endpoint)?,
            source,
        })
    }

    fn select(&self, cli: Option<&str>, environment: Option<&OsStr>) -> Result<Option<Selection>> {
        if let Some(name) = cli {
            return self.named(name, "command_line").map(Some);
        }
        if let Some(name) = environment {
            return self
                .named(
                    name.to_str().ok_or("SSMCP_CONTEXT must be UTF-8")?,
                    "environment",
                )
                .map(Some);
        }
        self.current
            .as_deref()
            .map(|name| self.named(name, "current"))
            .transpose()
    }
}

/// Resolve once, at a new connection or account command boundary. Never call this
/// from a reconnect, saved-session parser or an already running job.
pub(super) fn resolve(cli: Option<&str>, explicit_target: bool) -> Result<Option<Selection>> {
    if explicit_target {
        if cli.is_some() {
            return Err(
                "--context cannot be combined with --server, local, a destination or SSH options"
                    .into(),
            );
        }
        return Ok(None);
    }
    config::contexts()?.select(cli, std::env::var_os("SSMCP_CONTEXT").as_deref())
}

pub(super) fn remote_server(cli: Option<&str>, explicit: Option<&str>) -> Result<Option<String>> {
    match resolve(cli, explicit.is_some())? {
        Some(Selection {
            endpoint: Endpoint::Https(origin),
            ..
        }) => Ok(Some(origin)),
        Some(Selection {
            endpoint: Endpoint::Local,
            ..
        }) => Err("this command requires an HTTPS account; the selected context is local".into()),
        None => Ok(explicit.map(str::to_owned)),
    }
}

/// Only move a leading selector prefix across a known command boundary. Values
/// such as --title "--context" are never searched for or rewritten.
pub(super) fn normalize_args(args: &mut Vec<OsString>) -> Result<()> {
    let mut end = 0;
    let mut has_context = false;
    while let Some(arg) = args.get(end).and_then(|arg| arg.to_str()) {
        if matches!(arg, "--context" | "--server") {
            has_context |= arg == "--context";
            let value = args
                .get(end + 1)
                .ok_or("connection selector requires a value")?;
            if value.is_empty() || value.to_string_lossy().starts_with('-') {
                return Err("connection selector requires a value".into());
            }
            end += 2;
        } else if arg.starts_with("--context=") {
            has_context = true;
            end += 1;
        } else {
            break;
        }
    }
    if !has_context {
        return Ok(());
    }
    let command = args.get(end).and_then(|arg| arg.to_str());
    let command_len = match command {
        Some("login" | "logout" | "whoami" | "list" | "ls" | "invite" | "join" | "setup") => 1,
        Some("context" | "trust") => usize::from(args.get(end + 1).is_some()) + 1,
        Some("sessions") => match args.get(end + 1).and_then(|arg| arg.to_str()) {
            Some("create") => {
                if args.get(end + 2).is_none() { return Err("sessions create requires a name".into()); }
                3
            }
            Some("list") if args.iter().skip(end + 2).any(|arg| arg == "--remote" || arg == "-i" || arg == "--interactive") => 2,
            Some("--remote" | "-i" | "--interactive") => 1,
            _ => return Err("--context selects new connections; saved-session operations use their recorded target".into()),
        },
        Some("config" | "ui" | "update" | "007" | "service" | "attach" | "__session-run") => {
            return Err("--context is not supported by this local management command".into());
        }
        _ => 0,
    };
    if command_len != 0 {
        args[..end + command_len].rotate_left(end);
    }
    Ok(())
}

enum Edit<'a> {
    Create(&'a str, Endpoint),
    Use(Option<&'a str>),
    Remove(&'a str),
}

fn edit_at(path: &Path, action: Edit<'_>) -> Result<()> {
    let text = config::read_config(path, false)?;
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct ContextDocument {
        contexts: Store,
    }
    let old: ContextDocument =
        toml::from_str(&text).map_err(|_| "invalid ssmcp client TOML configuration")?;
    old.contexts.validate()?;
    match &action {
        Edit::Create(name, _) => {
            validate_name(name)?;
            if old.contexts.entries.contains_key(*name) {
                return Err("context already exists".into());
            }
        }
        Edit::Use(Some(name)) | Edit::Remove(name) => {
            old.contexts.named(name, "argument")?;
        }
        Edit::Use(None) => {}
    }
    if let Edit::Remove(name) = &action
        && old.contexts.current.as_deref() == Some(*name)
    {
        return Err(
            "cannot remove the current context; select another or run context use --default first"
                .into(),
        );
    }
    let mut document: DocumentMut = text
        .parse()
        .map_err(|_| "invalid ssmcp client TOML configuration")?;
    if !document.contains_key("contexts") {
        document["contexts"] = Item::Table(Table::new());
    }
    let contexts = document["contexts"]
        .as_table_like_mut()
        .ok_or("contexts must be a TOML table")?;
    match action {
        Edit::Create(name, endpoint) => {
            if !contexts.contains_key("entries") {
                contexts.insert("entries", Item::Table(Table::new()));
            }
            let entries = contexts
                .get_mut("entries")
                .and_then(Item::as_table_like_mut)
                .ok_or("contexts.entries must be a TOML table")?;
            let mut entry = Table::new();
            entry.insert("endpoint", value(endpoint.as_str()));
            entries.insert(name, Item::Table(entry));
        }
        Edit::Use(Some(name)) => {
            contexts.insert("current", value(name));
        }
        Edit::Use(None) => {
            contexts.remove("current");
        }
        Edit::Remove(name) => {
            contexts
                .get_mut("entries")
                .and_then(Item::as_table_like_mut)
                .ok_or("contexts.entries must be a TOML table")?
                .remove(name);
        }
    }
    config::save_document(path, &text, document)
}

fn selection_value(selection: Option<Selection>) -> Result<Value> {
    Ok(match selection {
        Some(selected) => {
            json!({"name":selected.name, "endpoint":selected.endpoint.as_str(), "selection_source":selected.source})
        }
        None => {
            json!({"name":null, "endpoint":account::resolve_server(None)?, "selection_source":"legacy"})
        }
    })
}

pub(super) fn dispatch(args: &[OsString]) -> Result<bool> {
    if !args.first().is_some_and(|arg| arg == "context") {
        return Ok(false);
    }
    let args = args
        .iter()
        .skip(1)
        .map(|arg| arg.to_str().ok_or("context arguments must be UTF-8"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if args.is_empty() || matches!(args.as_slice(), ["--help" | "-h"]) {
        println!("{HELP}");
        return Ok(true);
    }
    let path = config::config_path()?;
    match args.as_slice() {
        ["create", name, "--local"] => edit_at(&path, Edit::Create(name, Endpoint::Local))?,
        ["create", name, "--server", server] => {
            let endpoint = Endpoint::parse(server)?;
            if endpoint == Endpoint::Local {
                return Err(
                    "--server requires an HTTPS origin; use --local for a local context".into(),
                );
            }
            edit_at(&path, Edit::Create(name, endpoint))?;
        }
        ["use", "--default"] => edit_at(&path, Edit::Use(None))?,
        ["use", name] => edit_at(&path, Edit::Use(Some(name)))?,
        ["rm" | "remove", name] => edit_at(&path, Edit::Remove(name))?,
        [action @ ("list" | "ls" | "show" | "inspect"), rest @ ..] => {
            let mut json_output = false;
            let mut cli = None;
            let mut name = None;
            let mut rest = rest.iter();
            while let Some(arg) = rest.next() {
                match *arg {
                    "--json" if !json_output => json_output = true,
                    "--context" => {
                        set_name(&mut cli, rest.next().ok_or("--context requires a name")?)?
                    }
                    value if value.starts_with("--context=") => {
                        set_name(&mut cli, &value["--context=".len()..])?
                    }
                    value if *action == "inspect" && name.is_none() && !value.starts_with('-') => {
                        validate_name(value)?;
                        name = Some(value);
                    }
                    _ => {
                        return Err(
                            "unknown or repeated context option; see ssmcp context --help".into(),
                        );
                    }
                }
            }
            if name.is_some() && cli.is_some() {
                return Err("inspect takes either a name or --context, not both".into());
            }
            let store = config::contexts()?;
            let selected = if let Some(name) = name {
                Some(store.named(name, "argument")?)
            } else {
                store.select(cli.as_deref(), std::env::var_os("SSMCP_CONTEXT").as_deref())?
            };
            let output = if matches!(*action, "list" | "ls") {
                Value::Array(store.entries.iter().map(|(name, entry)| {
                    json!({"name":name, "endpoint":entry.endpoint,
                        "current":store.current.as_deref() == Some(name),
                        "selected":selected.as_ref().is_some_and(|s| &s.name == name),
                        "selection_source":selected.as_ref().filter(|s| &s.name == name).map(|s| s.source)})
                }).collect())
            } else {
                selection_value(selected)?
            };
            if json_output || *action == "inspect" {
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else if let Some(entries) = output.as_array() {
                for entry in entries {
                    println!(
                        "{}\t{}\t{}",
                        entry["name"].as_str().unwrap_or_default(),
                        entry["endpoint"].as_str().unwrap_or_default(),
                        entry["selection_source"].as_str().unwrap_or("")
                    );
                }
                if entries.is_empty() {
                    println!("No connection contexts saved.");
                }
            } else {
                println!(
                    "{}\t{}\t{}",
                    output["name"].as_str().unwrap_or("default"),
                    output["endpoint"].as_str().unwrap_or_default(),
                    output["selection_source"].as_str().unwrap_or_default()
                );
            }
            return Ok(true);
        }
        _ => return Err("unknown context command or options; see ssmcp context --help".into()),
    }
    println!("Connection contexts saved. Existing sessions keep their recorded target.");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn store() -> Store {
        toml::from_str("current = 'work'\n[entries.work]\nendpoint = 'https://WORK.example:443/'\n[entries.other]\nendpoint = 'https://other.example:8443'\n[entries.here]\nendpoint = 'local'\n").unwrap()
    }

    #[test]
    fn context_precedence_is_explicit_environment_then_current() {
        let store = store();
        let selected = store
            .select(Some("here"), Some(OsStr::new("missing")))
            .unwrap()
            .unwrap();
        assert_eq!(selected.endpoint, Endpoint::Local);
        assert_eq!(selected.source, "command_line");
        let selected = store
            .select(None, Some(OsStr::new("other")))
            .unwrap()
            .unwrap();
        assert_eq!(
            selected.endpoint,
            Endpoint::Https("https://other.example:8443".into())
        );
        assert_eq!(selected.source, "environment");
        let selected = store.select(None, None).unwrap().unwrap();
        assert_eq!(
            selected.endpoint,
            Endpoint::Https("https://work.example".into())
        );
        assert_eq!(selected.source, "current");
        assert!(Store::default().select(None, None).unwrap().is_none());
    }

    #[test]
    fn context_unknown_selected_names_never_fall_back() {
        let store = store();
        assert!(
            store
                .select(Some("missing"), Some(OsStr::new("work")))
                .is_err()
        );
        for name in ["missing", "", "--help", "a/b", "../work", "work\n"] {
            assert!(
                store.select(None, Some(OsStr::new(name))).is_err(),
                "{name:?}"
            );
        }
        let mut invalid = store;
        invalid.current = Some("missing".into());
        assert!(invalid.validate().is_err());
        assert!(invalid.select(None, None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn context_non_unicode_environment_is_rejected_only_when_selected() {
        use std::os::unix::ffi::OsStrExt;
        let invalid = OsStr::from_bytes(&[0xff]);
        assert!(store().select(None, Some(invalid)).is_err());
        assert_eq!(
            store()
                .select(Some("work"), Some(invalid))
                .unwrap()
                .unwrap()
                .source,
            "command_line"
        );
    }

    #[test]
    fn context_store_accepts_only_named_local_or_credential_free_https() {
        for endpoint in [
            "ssh://host",
            "host",
            "http://127.0.0.1:9000",
            "https://user:secret@host",
            "https://host/path",
            "https://host?token=secret",
            "https://host#secret",
        ] {
            assert!(Endpoint::parse(endpoint).is_err(), "{endpoint}");
        }
        for text in [
            "[entries.work]\nendpoint = 'https://work.example'\ntoken = 'secret'",
            "[entries.work]\nendpoint = 'local'\nssh = 'host'",
            "[entries.work]\nserver = 'https://work.example'",
        ] {
            assert!(toml::from_str::<Store>(text).is_err());
        }
        assert!(store().validate().is_ok());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn context_prefix_normalization_preserves_value_and_command_boundaries() {
        for (input, expected) in [
            (
                vec!["--context", "work", "whoami", "--json"],
                vec!["whoami", "--context", "work", "--json"],
            ),
            (
                vec!["--context=work", "trust", "add", "--pin", "PIN"],
                vec!["trust", "add", "--context=work", "--pin", "PIN"],
            ),
            (
                vec![
                    "--context",
                    "work",
                    "sessions",
                    "create",
                    "build",
                    "--title",
                    "--context",
                ],
                vec![
                    "sessions",
                    "create",
                    "build",
                    "--context",
                    "work",
                    "--title",
                    "--context",
                ],
            ),
            (
                vec!["--context", "work", "context", "show"],
                vec!["context", "show", "--context", "work"],
            ),
            (
                vec!["--title", "--context", "--name", "whoami"],
                vec!["--title", "--context", "--name", "whoami"],
            ),
            (
                vec!["--context", "whoami", "--title", "login"],
                vec!["--context", "whoami", "--title", "login"],
            ),
        ] {
            let mut args: Vec<_> = input.iter().map(OsString::from).collect();
            normalize_args(&mut args).unwrap();
            assert_eq!(
                args,
                expected.iter().map(OsString::from).collect::<Vec<_>>()
            );
        }
        for command in ["config", "service", "attach", "__session-run"] {
            let mut args = ["--context", "work", command].map(OsString::from).to_vec();
            assert!(normalize_args(&mut args).is_err());
        }
        let mut args = ["--context", "work", "sessions", "restart", "saved"]
            .map(OsString::from)
            .to_vec();
        assert!(normalize_args(&mut args).is_err());
    }

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssmcp-context-config-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn context_edits_preserve_comments_settings_and_private_file_mode() {
        let scratch = Scratch::new();
        let path = scratch.0.join("config.toml");
        let original = "# keep user preferences\n[ui]\ntheme = 'nord' # color\n[browser]\nbackground = false\n[git]\nfetch_interval_seconds = 600\n[client]\nserver = 'https://fallback.example'\n";
        fs::write(&path, original).unwrap();
        edit_at(&path, Edit::Create("here", Endpoint::Local)).unwrap();
        edit_at(&path, Edit::Use(Some("here"))).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with(original));
        assert!(edit_at(&path, Edit::Remove("here")).is_err());
        assert!(edit_at(&path, Edit::Create("here", Endpoint::Local)).is_err());
        edit_at(&path, Edit::Use(None)).unwrap();
        edit_at(&path, Edit::Remove("here")).unwrap();
        assert!(fs::read_to_string(&path).unwrap().starts_with(original));
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn context_oversized_write_is_rejected_without_changing_the_original() {
        let scratch = Scratch::new();
        let path = scratch.0.join("config.toml");
        let original = format!("#{}\n", "x".repeat(64 * 1024 - 30));
        fs::write(&path, &original).unwrap();
        assert!(
            edit_at(
                &path,
                Edit::Create("remote", Endpoint::Https("https://work.example".into()))
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(path).unwrap(), original);
        assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 1);
    }

    #[test]
    fn context_switch_keeps_existing_options_and_saved_endpoint_concrete() {
        let mut store = store();
        let Endpoint::Https(origin) = store.select(None, None).unwrap().unwrap().endpoint else {
            panic!("HTTPS fixture");
        };
        let options = super::super::Options::parse(
            [
                "--server",
                &origin,
                "--name",
                "pinned",
                "--title",
                "--context",
            ]
            .map(OsString::from),
        )
        .unwrap()
        .unwrap();
        let spec = options.session_spec().unwrap();
        store.current = Some("here".into());
        assert_eq!(
            store.select(None, None).unwrap().unwrap().endpoint,
            Endpoint::Local
        );
        assert_eq!(options.server.as_deref(), Some("https://work.example"));
        let restored = super::super::Options::parse_saved(&spec).unwrap().unwrap();
        assert_eq!(restored.server, options.server);
        assert_eq!(restored.title.as_deref(), Some("--context"));
        let mut invalid = spec;
        invalid.args.extend(["--context".into(), "here".into()]);
        assert!(super::super::Options::parse_saved(&invalid).is_err());
    }
}
