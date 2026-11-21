//! Native tools for the runtime's already-authorized workspace root.
use super::CleanupConfig;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::path::Path;

const DEFAULT_ENTRIES: usize = 64;
const MAX_ENTRIES: usize = 128;

pub fn tools() -> Vec<Value> {
    let defaults = CleanupConfig::default();
    let config_schema = json!({
        "type":"object","additionalProperties":false,
        "properties":{
            "enabled":{"type":"boolean","default":defaults.enabled},
            "max_age_seconds":{"type":"integer","minimum":0,"default":defaults.max_age_seconds},
            "max_total_bytes":{"type":"integer","minimum":0,"default":defaults.max_total_bytes},
            "rules":{"type":"array","maxItems":64,"default":[],"items":{
                "type":"object","additionalProperties":false,
                "properties":{
                    "relative_prefix":{"type":"string","minLength":1,"maxLength":4096,
                        "description":"Canonical relative prefix inside an unprotected .ssmcp directory; only explicitly registered files are eligible."},
                    "category":{"type":"string","enum":["temporary"]},
                    "max_age_seconds":{"type":["integer","null"],"minimum":0}
                },
                "required":["relative_prefix","category"]
            }}
        }
    });
    vec![
        tool(
            "ssmcp.files.list",
            "List explicitly registered managed files, including retained removal records. Offset pagination reads the current sorted view on every call, not a stable snapshot; concurrent changes can shift pages. Does not scan workspace contents or register unknown files.",
            json!({
                "offset":{"type":"integer","minimum":0,"default":0},
                "max_entries":{"type":"integer","minimum":1,"maximum":MAX_ENTRIES,"default":DEFAULT_ENTRIES}
            }),
            &[], true, false, true,
        ),
        tool(
            "ssmcp.files.cleanup",
            "Preview cleanup by default. Set dry_run=false to remove only registered, eligible temporary files under the current configuration. Protected, changed, unknown and active files are retained. Return the actual cleanup summary, including any errors.",
            json!({"dry_run":{"type":"boolean","default":true}}),
            &[], false, true, false,
        ),
        tool(
            "ssmcp.files.config",
            "Read the current cleanup configuration, or defaults when none exists, without creating managed state.",
            json!({}),
            &[], true, false, true,
        ),
        tool(
            "ssmcp.files.configure",
            "Validate and replace the cleanup configuration (at most 65536 serialized bytes). Omitted configuration fields use their documented defaults. This operation only sets configuration and does not run cleanup.",
            json!({"config":config_schema}),
            &["config"], false, false, true,
        ),
    ]
}

/// Synchronous filesystem work; the runtime dispatches this on its blocking pool.
pub fn perform(root: &Path, name: &str, args: Value) -> Result<Value, String> {
    // Validate the complete request before opening any workspace state.
    match request(name, args)? {
        Request::List(args) => {
            let records = super::list(root).map_err(error)?;
            let total = records.len();
            let end = args.offset.saturating_add(args.max_entries).min(total);
            let next_offset = (end < total).then_some(end);
            let records: Vec<_> = records
                .into_iter()
                .skip(args.offset)
                .take(args.max_entries)
                .collect();
            Ok(json!({"records":records,"total":total,"next_offset":next_offset}))
        }
        Request::Cleanup(args) => {
            let summary = super::cleanup(root, args.dry_run, "tool").map_err(error)?;
            serde_json::to_value(summary).map_err(error)
        }
        Request::Config => serde_json::to_value(super::config(root).map_err(error)?).map_err(error),
        Request::Configure(config) => {
            super::set_config(root, &config).map_err(error)?;
            Ok(json!({"configured":true,"config":config}))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_entries")]
    max_entries: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CleanupArgs {
    #[serde(default = "default_dry_run")]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigureArgs {
    config: Value,
}

#[derive(Debug)]
enum Request {
    List(ListArgs),
    Cleanup(CleanupArgs),
    Config,
    Configure(CleanupConfig),
}

fn request(name: &str, args: Value) -> Result<Request, String> {
    match name {
        "ssmcp.files.list" => {
            let args: ListArgs = parse(name, args)?;
            if !(1..=MAX_ENTRIES).contains(&args.max_entries) {
                return Err(format!("max_entries must be 1..={MAX_ENTRIES}"));
            }
            Ok(Request::List(args))
        }
        "ssmcp.files.cleanup" => Ok(Request::Cleanup(parse(name, args)?)),
        "ssmcp.files.config" => {
            let _: Empty = parse(name, args)?;
            Ok(Request::Config)
        }
        "ssmcp.files.configure" => {
            let args: ConfigureArgs = parse(name, args)?;
            check_config_size(serde_json::to_vec(&args.config).map_err(error)?.len())?;
            if let Some(rules) = args.config.get("rules").and_then(Value::as_array)
                && rules.iter().any(|rule| !rule.is_object())
            {
                return Err("cleanup rules must be objects".into());
            }
            let config: CleanupConfig = parse(name, args.config)?;
            config.validate().map_err(error)?;
            // The stored form is pretty JSON followed by one newline. Check it
            // before set_config can create an otherwise absent registry.
            check_config_size(
                serde_json::to_vec_pretty(&config)
                    .map_err(error)?
                    .len()
                    .saturating_add(1),
            )?;
            Ok(Request::Configure(config))
        }
        _ => Err(format!("unknown managed-files capability: {name}")),
    }
}

fn check_config_size(bytes: usize) -> Result<(), String> {
    if bytes > super::CONFIG_BYTES_LIMIT {
        Err(format!("cleanup configuration exceeds {} bytes", super::CONFIG_BYTES_LIMIT))
    } else {
        Ok(())
    }
}

fn parse<T: DeserializeOwned>(name: &str, args: Value) -> Result<T, String> {
    if !args.is_object() {
        return Err(format!("invalid {name} arguments: expected an object"));
    }
    serde_json::from_value(args).map_err(|error| format!("invalid {name} arguments: {error}"))
}

fn default_entries() -> usize { DEFAULT_ENTRIES }
fn default_dry_run() -> bool { true }
fn error(error: impl std::fmt::Display) -> String { error.to_string() }

fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    readonly: bool,
    destructive: bool,
    idempotent: bool,
) -> Value {
    json!({
        "name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":readonly,"destructiveHint":destructive,
            "idempotentHint":idempotent,"openWorldHint":false}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_defaults_preview_cleanup_and_bound_list_pages() {
        assert!(matches!(request("ssmcp.files.cleanup", json!({})).unwrap(),
            Request::Cleanup(CleanupArgs { dry_run: true })));
        assert!(matches!(request("ssmcp.files.cleanup", json!({"dry_run":false})).unwrap(),
            Request::Cleanup(CleanupArgs { dry_run: false })));
        assert!(matches!(request("ssmcp.files.list", json!({})).unwrap(),
            Request::List(ListArgs { offset: 0, max_entries: DEFAULT_ENTRIES })));
        assert!(matches!(request("ssmcp.files.list", json!({"offset":9,"max_entries":128})).unwrap(),
            Request::List(ListArgs { offset: 9, max_entries: MAX_ENTRIES })));
    }

    #[test]
    fn malformed_native_requests_fail_before_workspace_io() {
        let root = Path::new("\0managed-files-no-io");
        for (name, args, expected) in [
            ("ssmcp.files.list", json!({"max_entries":0}), "max_entries must be"),
            ("ssmcp.files.list", json!({"max_entries":129}), "max_entries must be"),
            ("ssmcp.files.list", json!({"offset":-1}), "invalid ssmcp.files.list arguments"),
            ("ssmcp.files.list", json!({"root":"elsewhere"}), "unknown field"),
            ("ssmcp.files.cleanup", json!({"dry_run":"false"}), "invalid ssmcp.files.cleanup arguments"),
            ("ssmcp.files.cleanup", json!({"trigger":"reload"}), "unknown field"),
            ("ssmcp.files.config", json!({"path":"elsewhere"}), "unknown field"),
            ("ssmcp.files.config", json!([]), "expected an object"),
            ("ssmcp.files.configure", json!({}), "missing field"),
            ("ssmcp.files.configure", json!({"config":{},"root":"elsewhere"}), "unknown field"),
            ("ssmcp.files.configure", json!({"config":{"unknown":true}}), "unknown field"),
            ("ssmcp.files.configure", json!({"config":[]}), "expected an object"),
            ("ssmcp.files.configure", json!({"config":{"rules":[[]]}}), "rules must be objects"),
            ("ssmcp.files.unknown", json!({}), "unknown managed-files capability"),
        ] {
            let error = perform(root, name, args).unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
    }

    #[test]
    fn native_configuration_uses_core_rules_and_storage_size_limit() {
        let Request::Configure(config) = request("ssmcp.files.configure", json!({"config":{}})).unwrap() else {
            panic!("expected a configuration request");
        };
        assert_eq!(config, CleanupConfig::default());
        let root = Path::new("\0managed-files-no-io");
        let invalid_rule = json!({"config":{"rules":[{
            "relative_prefix":".ssmcp/inputs","category":"temporary"
        }]}});
        assert!(perform(root, "ssmcp.files.configure", invalid_rule).unwrap_err()
            .contains("unprotected path"));
        let oversized = json!({"config":{"rules":[{
            "relative_prefix":"x".repeat(super::super::CONFIG_BYTES_LIMIT),"category":"temporary"
        }]}});
        assert!(perform(root, "ssmcp.files.configure", oversized).unwrap_err()
            .contains("exceeds 65536 bytes"));
    }
}
