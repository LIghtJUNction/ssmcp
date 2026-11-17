//! Explicitly granted management of this OS user's saved sessions.
//!
//! Creation inherits the management connection's SSH destination and allow-list;
//! native callers cannot supply arbitrary command lines, identities or roots.
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};

use super::SessionSpec;

pub struct Control {
    root: PathBuf,
    template: Option<SessionSpec>,
    current_session: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Named {
    name: String,
    #[serde(default)]
    expected_generation: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    name: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    watchdog: Option<bool>,
    #[serde(default)]
    reconnect_delay_secs: Option<u64>,
}

impl Control {
    pub fn new(
        root: PathBuf,
        template: Option<SessionSpec>,
        current_session: Option<String>,
    ) -> Result<Self, String> {
        let root = root.canonicalize().map_err(|e| e.to_string())?;
        if !root.is_dir() {
            return Err("session management workspace is not a directory".into());
        }
        Ok(Self {
            root,
            template,
            current_session,
        })
    }

    pub fn tools() -> Vec<Value> {
        let name = json!({"type":"string","minLength":1,"maxLength":128});
        let generation = json!({"type":"string","minLength":1,"maxLength":128,
            "description":"Optional generation from status; reject stale lifecycle requests."});
        let named = json!({"type":"object","additionalProperties":false,
            "properties":{"name":name,"expected_generation":generation},"required":["name"]});
        [
            ("list", "List the current OS user's service-managed sessions. Available only with --session-control. No credentials or startup arguments are returned.",
                json!({"type":"object","additionalProperties":false,"properties":{}}), true),
            ("status", "Inspect one saved session. Manager process state and Broker connection state are separate; generation identifies the current worker instance.", named.clone(), true),
            ("create", "Create and start one new service-managed session. Inherits this connection's SSH settings and GitHub allow-list; cwd must remain inside this connection's workspace. Does not grant session-control to the child. Requires the local ssmcp service. A start acknowledgement does not prove Broker connection readiness.",
                json!({"type":"object","additionalProperties":false,"properties":{
                    "name":name,"cwd":{"type":"string","maxLength":4096,"description":"Existing workspace directory; defaults to this client's startup root."},
                    "title":{"type":"string","maxLength":512},
                    "watchdog":{"type":"boolean","description":"Defaults to the management connection's retry policy."},
                    "reconnect_delay_secs":{"type":"integer","minimum":1,"maximum":3600}
                },"required":["name"]}), false),
            ("restart", "Stop the named session, confirm its owned worker tree exited, then start a new generation using saved configuration. Existing connection jobs end. Use another control session to restart this management connection itself.", named.clone(), false),
            ("close", "Persist stopped intent and confirm the named session's owned worker tree is stopped. Watchdog and service restart will not resurrect it. Retains saved configuration. Use another control session to close this management connection itself.", named, false),
        ]
        .into_iter()
        .map(|(operation, description, schema, read_only)| json!({
            "name":format!("ssmcp.session.{operation}"),"description":description,"inputSchema":schema,
            "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,
                "idempotentHint":read_only || operation == "close","openWorldHint":false}
        }))
        .collect()
    }

    pub async fn perform(&self, tool: &str, arguments: Value) -> Result<Value, String> {
        let request = match tool {
            "ssmcp.session.list" => {
                serde_json::from_value::<Empty>(arguments).map_err(|e| e.to_string())?;
                json!({"op":"list"})
            }
            "ssmcp.session.create" => {
                let input: Create = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                super::validate_name(&input.name).map_err(|e| e.to_string())?;
                let cwd = resolve_cwd(&self.root, input.cwd.as_deref())?;
                let template = self.template.as_ref().ok_or(
                    "session creation requires a live management connection with saved SSH settings",
                )?;
                let spec = crate::client::child_session_spec(
                    template,
                    input.name,
                    cwd,
                    input.title,
                    input.watchdog,
                    input.reconnect_delay_secs,
                )
                .map_err(|e| e.to_string())?;
                json!({"op":"create","spec":spec})
            }
            "ssmcp.session.status" | "ssmcp.session.restart" | "ssmcp.session.close" => {
                let input: Named = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
                super::validate_name(&input.name).map_err(|e| e.to_string())?;
                if tool != "ssmcp.session.status"
                    && self.current_session.as_deref() == Some(input.name.as_str())
                {
                    return Err("use a different management connection or the local CLI to restart/close this control session; its own acknowledgement must remain deliverable".into());
                }
                if input.expected_generation.as_ref().is_some_and(|v| {
                    v.is_empty() || v.len() > 128 || v.chars().any(char::is_control)
                }) {
                    return Err(
                        "expected_generation must be 1–128 bytes without control characters".into(),
                    );
                }
                let mut request =
                    json!({"op":tool.trim_start_matches("ssmcp.session."),"name":input.name});
                if let Some(generation) = input.expected_generation {
                    request["expected_generation"] = generation.into();
                }
                request
            }
            _ => return Err("unknown session control operation".into()),
        };
        let value = super::service::request(request)
            .await
            .map_err(|e| e.to_string())?;
        // MCP structuredContent is an object; preserve lists inside a named field.
        Ok(if tool == "ssmcp.session.list" {
            json!({"sessions": value})
        } else {
            value
        })
    }
}

fn resolve_cwd(root: &Path, value: Option<&str>) -> Result<PathBuf, String> {
    let value = value.unwrap_or(".");
    if value.is_empty() || value.len() > 4096 || value.chars().any(char::is_control) {
        return Err("session cwd must be a nonempty path without control characters".into());
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err("session cwd must not contain parent traversal".into());
    }
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|e| format!("session cwd: {e}"))?;
    if !resolved.starts_with(root) || !resolved.is_dir() {
        return Err(
            "session cwd must be an existing directory inside this connection's workspace".into(),
        );
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creation_schema_cannot_change_identity_or_grant_control() {
        for extra in [
            "args",
            "allow",
            "destination",
            "identity",
            "session_control",
        ] {
            let mut value = json!({"name":"worker"});
            value[extra] = json!("unexpected");
            assert!(serde_json::from_value::<Create>(value).is_err(), "{extra}");
        }
    }

    #[test]
    fn create_cwd_is_confined_and_rejects_escape_symlinks() {
        let parent = std::env::temp_dir().join(format!(
            "ssmcp-control-{}-{}",
            std::process::id(),
            super::super::random_suffix().unwrap()
        ));
        let root = parent.join("project");
        let outside = parent.join("outside");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root = root.canonicalize().unwrap();
        assert_eq!(resolve_cwd(&root, None).unwrap(), root);
        assert_eq!(
            resolve_cwd(&root, Some("child")).unwrap(),
            root.join("child")
        );
        assert!(resolve_cwd(&root, Some("../outside")).is_err());
        assert!(resolve_cwd(&root, outside.to_str()).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            assert!(resolve_cwd(&root, Some("escape")).is_err());
        }
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[tokio::test]
    async fn self_restart_and_close_fail_before_service_contact() {
        let control = Control::new(
            std::env::current_dir().unwrap(),
            None,
            Some("manager".into()),
        )
        .unwrap();
        for tool in ["ssmcp.session.restart", "ssmcp.session.close"] {
            let error = control
                .perform(tool, json!({"name":"manager"}))
                .await
                .unwrap_err();
            assert!(error.contains("different management connection"));
        }
    }
}
