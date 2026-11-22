//! Read-only path discovery before account, configuration, or session startup.
use serde::{Serialize, Serializer, ser::SerializeMap};
use std::{ffi::OsString, path::PathBuf};

const HELP: &str = "ssmcp paths — show the paths used by this client\n\n\
Usage: ssmcp paths [NAME ...] [--json | --plain]\n\
       ssmcp paths --list [--json | --plain]\n\
       ssmcp paths --help\n\n\
No NAME selects every path. Names keep their requested order; duplicates are removed.\n\
Default output is NAME<TAB>PATH. --plain prints one path per line.\n\
--json prints an object. --list shows names and descriptions without resolving paths;\n\
with --plain it prints names only. No directories are created or configuration read.\n";

const CATALOG: [(&str, &str); 10] = [
    ("config", "Client configuration file"),
    ("ui-config", "UI configuration file"),
    ("auth", "Account credential directory (path only)"),
    ("trust", "Trusted-server pins file"),
    ("rc", "Tool repository configuration file"),
    ("tools", "Managed tool repository directory"),
    ("history", "Local conversation history directory"),
    ("sessions", "Saved background session directory"),
    ("cwd", "Current working directory"),
    ("executable", "Current executable"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Format {
    Named,
    Plain,
    Json,
}

enum Request {
    Help,
    List(Format),
    Paths(Vec<&'static str>, Format),
}

fn request(args: &[OsString]) -> Result<Option<Request>, String> {
    if !args.first().is_some_and(|arg| arg == "paths") {
        return Ok(None);
    }
    let args = &args[1..];
    if args.len() == 1 && (args[0] == "--help" || args[0] == "-h") {
        return Ok(Some(Request::Help));
    }
    let mut names = Vec::new();
    let mut format = None;
    let mut list = false;
    for arg in args {
        let arg = arg.to_str().ok_or("path names and options must be UTF-8")?;
        match arg {
            "--help" | "-h" => return Err("paths --help does not accept other arguments".into()),
            "--list" => list = true,
            "--json" | "--plain" => {
                let selected = if arg == "--json" {
                    Format::Json
                } else {
                    Format::Plain
                };
                if format.is_some_and(|current| current != selected) {
                    return Err("paths --json and --plain cannot be combined".into());
                }
                format = Some(selected);
            }
            option if option.starts_with('-') => {
                return Err(format!(
                    "unknown paths option {option:?}; see ssmcp paths --help"
                ));
            }
            name => {
                let name = CATALOG
                    .iter()
                    .find(|entry| entry.0 == name)
                    .map(|entry| entry.0)
                    .ok_or_else(|| format!("unknown path name {name:?}; use ssmcp paths --list"))?;
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    let format = format.unwrap_or(Format::Named);
    if list {
        if !names.is_empty() {
            return Err("paths --list cannot be combined with path names".into());
        }
        return Ok(Some(Request::List(format)));
    }
    if names.is_empty() {
        names.extend(CATALOG.iter().map(|entry| entry.0));
    }
    Ok(Some(Request::Paths(names, format)))
}

pub(super) fn output(args: &[OsString]) -> Result<Option<String>, String> {
    output_with(args, resolve)
}

fn output_with(
    args: &[OsString],
    mut resolve: impl FnMut(&str) -> Result<PathBuf, String>,
) -> Result<Option<String>, String> {
    let Some(request) = request(args)? else {
        return Ok(None);
    };
    let output = match request {
        Request::Help => HELP.to_owned(),
        Request::List(Format::Plain) => {
            format!(
                "{}\n",
                CATALOG
                    .iter()
                    .map(|entry| entry.0)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
        Request::List(format) => render(
            &CATALOG
                .iter()
                .map(|&(name, description)| (name, description.to_owned()))
                .collect::<Vec<_>>(),
            format,
        )?,
        Request::Paths(names, format) => {
            let entries = names
                .into_iter()
                .map(|name| {
                    let path = resolve(name).map_err(|error| format!("{name}: {error}"))?;
                    let path = path
                        .to_str()
                        .ok_or_else(|| format!("{name}: path is not valid UTF-8"))?;
                    Ok((name, path.to_owned()))
                })
                .collect::<Result<Vec<_>, String>>()?;
            render(&entries, format)?
        }
    };
    Ok(Some(output))
}

fn resolve(name: &str) -> Result<PathBuf, String> {
    match name {
        "config" => super::config::config_path().map_err(|error| error.to_string()),
        "ui-config" => super::ui::Ui::config_path(None)
            .ok_or_else(|| "cannot determine UI configuration path".into()),
        "auth" => super::account::state_root().map_err(|error| error.to_string()),
        "trust" => super::trust::path().map_err(|error| error.to_string()),
        "rc" | "tools" => {
            let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
            let (rc, tools) = crate::tool_repository::configured_paths(&cwd)?;
            Ok(if name == "rc" { rc } else { tools })
        }
        "history" => super::interface::history::default_directory(),
        "sessions" => crate::session::state_path().map_err(|error| error.to_string()),
        "cwd" => std::env::current_dir().map_err(|error| error.to_string()),
        "executable" => std::env::current_exe().map_err(|error| error.to_string()),
        _ => Err(format!("unknown path name {name:?}")),
    }
}

// Serialize entries directly instead of sorting object keys through serde_json::Map.
struct OrderedEntries<'a>(&'a [(&'static str, String)]);

impl Serialize for OrderedEntries<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

fn render(entries: &[(&'static str, String)], format: Format) -> Result<String, String> {
    if format == Format::Json {
        return serde_json::to_string_pretty(&OrderedEntries(entries))
            .map(|output| format!("{output}\n"))
            .map_err(|error| error.to_string());
    }
    let mut lines = Vec::with_capacity(entries.len());
    for (name, value) in entries {
        if value.chars().any(char::is_control) {
            return Err(format!(
                "{name}: path contains control characters; use --json"
            ));
        }
        lines.push(if format == Format::Plain {
            value.clone()
        } else {
            format!("{name}\t{value}")
        });
    }
    Ok(format!("{}\n", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    #[test]
    fn selected_paths_keep_order_deduplicate_and_do_not_resolve_other_names() {
        let mut requested = Vec::new();
        let result = output_with(
            &args(&["paths", "trust", "cwd", "trust", "--json"]),
            |name| {
                requested.push(name.to_owned());
                Ok(PathBuf::from(format!("planned-{name}")))
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(requested, ["trust", "cwd"]);
        assert!(result.find("\"trust\"").unwrap() < result.find("\"cwd\"").unwrap());
        let value: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["trust"], "planned-trust");
        assert_eq!(value["cwd"], "planned-cwd");
    }

    #[test]
    fn invalid_arguments_are_rejected_before_any_path_resolution() {
        for values in [
            vec!["paths", "cwd", "unknown"],
            vec!["paths", "cwd", "--unknown"],
            vec!["paths", "--json", "--plain"],
            vec!["paths", "cwd", "--list"],
            vec!["paths", "--help", "cwd"],
        ] {
            assert!(
                output_with(&args(&values), |_| panic!(
                    "invalid request resolved a path"
                ))
                .is_err()
            );
        }
        assert!(
            output_with(&args(&["other-command"]), |_| panic!(
                "unrelated command resolved a path"
            ))
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn help_and_catalog_do_not_resolve_environment_or_paths() {
        for flag in ["--help", "-h"] {
            assert!(
                output_with(&args(&["paths", flag]), |_| panic!("help resolved a path"))
                    .unwrap()
                    .unwrap()
                    .contains("Usage: ssmcp paths")
            );
        }
        let names = output_with(&args(&["paths", "--list", "--plain"]), |_| {
            panic!("list resolved a path")
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            names.lines().collect::<Vec<_>>(),
            CATALOG.map(|entry| entry.0)
        );
        let catalog = output_with(&args(&["paths", "--json", "--list"]), |_| {
            panic!("list resolved a path")
        })
        .unwrap()
        .unwrap();
        let catalog: serde_json::Value = serde_json::from_str(&catalog).unwrap();
        assert_eq!(catalog.as_object().unwrap().len(), 10);
        assert_eq!(catalog["auth"], "Account credential directory (path only)");
    }

    #[test]
    fn all_paths_are_default_and_formats_preserve_literal_values() {
        let mut names = Vec::new();
        let named = output_with(&args(&["paths"]), |name| {
            names.push(name.to_owned());
            Ok(PathBuf::from(format!("path with spaces/{name}")))
        })
        .unwrap()
        .unwrap();
        assert_eq!(names, CATALOG.map(|entry| entry.0));
        assert_eq!(named.lines().count(), 10);
        assert!(named.starts_with("config\tpath with spaces/config\n"));
        let plain = output_with(&args(&["paths", "cwd", "--plain"]), |_| {
            Ok(PathBuf::from("$(literal) with spaces"))
        })
        .unwrap()
        .unwrap();
        assert_eq!(plain, "$(literal) with spaces\n");
        let json = output_with(&args(&["paths", "cwd", "--json"]), |_| {
            Ok(PathBuf::from("line\nbreak"))
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&json).unwrap()["cwd"],
            "line\nbreak"
        );
        assert!(
            output_with(&args(&["paths", "cwd", "--plain"]), |_| Ok(PathBuf::from(
                "line\nbreak"
            )))
            .is_err()
        );
    }

    #[test]
    fn a_failed_selected_path_returns_no_partial_output() {
        let mut names = Vec::new();
        let result = output_with(&args(&["paths", "cwd", "auth"]), |name| {
            names.push(name.to_owned());
            if name == "auth" {
                Err("synthetic unavailable root".into())
            } else {
                Ok(PathBuf::from("valid-cwd"))
            }
        });
        assert_eq!(names, ["cwd", "auth"]);
        assert_eq!(result.unwrap_err(), "auth: synthetic unavailable root");
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn real_getters_report_planned_paths_without_reading_config_or_creating_state() {
        const PROBE: &str = "SSMCP_PATHS_READONLY_TEST_ROOT";
        if let Some(root) = std::env::var_os(PROBE) {
            let root = PathBuf::from(root);
            let json = output(&args(&["paths", "--json"])).unwrap().unwrap();
            let paths: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(paths.as_object().unwrap().len(), 10);
            for (name, relative) in [
                ("config", "invalid-config.toml"),
                ("ui-config", "invalid-ui.toml"),
                ("auth", "auth"),
                ("trust", "trusted-servers.toml"),
                ("rc", "config/ssmcp/ssmcprc.toml"),
                ("tools", "data/ssmcp/tools"),
                ("sessions", "sessions"),
            ] {
                assert_eq!(
                    PathBuf::from(paths[name].as_str().unwrap()),
                    root.join(relative)
                );
            }
            #[cfg(windows)]
            let history = root.join("local/ssmcp/conversations");
            #[cfg(not(windows))]
            let history = root.join("state/ssmcp/conversations");
            assert_eq!(PathBuf::from(paths["history"].as_str().unwrap()), history);
            assert_eq!(
                PathBuf::from(paths["cwd"].as_str().unwrap()),
                std::env::current_dir().unwrap()
            );
            assert_eq!(
                PathBuf::from(paths["executable"].as_str().unwrap()),
                std::env::current_exe().unwrap()
            );
            for name in [
                "home", "config", "data", "state", "local", "auth", "sessions",
            ] {
                assert!(!root.join(name).exists(), "path query created {name}");
            }
            return;
        }
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ssmcp-paths-{}-{unique}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let scratch = Scratch(root);
        let root = &scratch.0;
        for file in ["invalid-config.toml", "invalid-ui.toml"] {
            std::fs::write(root.join(file), b"this is deliberately not TOML = [").unwrap();
        }
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "client::paths::tests::real_getters_report_planned_paths_without_reading_config_or_creating_state", "--nocapture"])
            .current_dir(root)
            .env(PROBE, root)
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("LOCALAPPDATA", root.join("local"))
            .env("SSMCP_CLIENT_CONFIG", root.join("invalid-config.toml"))
            .env("SSMCP_UI_CONFIG", root.join("invalid-ui.toml"))
            .env("SSMCP_AUTH_HOME", root.join("auth"))
            .env("SSMCP_SESSION_HOME", root.join("sessions"))
            .output().unwrap();
        assert!(
            result.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            stdout.contains("1 passed; 0 failed"),
            "probe did not execute its exact test: {stdout}"
        );
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 2);
        for file in ["invalid-config.toml", "invalid-ui.toml"] {
            assert_eq!(
                std::fs::read(root.join(file)).unwrap(),
                b"this is deliberately not TOML = ["
            );
        }
    }
}
