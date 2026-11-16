//! Local shell and installed native harness tools on one authenticated connection.
pub mod cc;
pub mod codex;
pub mod events;
pub mod opencode;
pub mod pi;
pub mod process;
pub mod rpc;
pub mod shell;

use crate::{
    pty::Terminal,
    wire::{MAX_READ, MAX_WAIT_MS, MAX_WRITE, Operation},
};
use serde_json::{Value, json};
use std::{ffi::OsString, future::Future, path::Path, sync::Arc, time::Duration};

pub struct Backend {
    terminal: Arc<Terminal>,
    shell: Arc<shell::Shell>,
    codex: Option<Arc<codex::Codex>>,
    claude: Option<Arc<cc::Claude>>,
    pi: Option<Arc<pi::Pi>>,
    opencode: Option<Arc<opencode::OpenCode>>,
}

async fn discover<T, F: Future<Output = crate::Result<T>>>(
    name: &str,
    seconds: u64,
    start: impl FnOnce(OsString) -> F,
) -> Option<T> {
    let program = crate::platform::executable(name).ok()?.into_os_string();
    match tokio::time::timeout(Duration::from_secs(seconds), start(program)).await {
        Ok(Ok(driver)) => {
            crate::client::render::status("harness", "Result", &format!("{name} tools registered"));
            Some(driver)
        }
        Ok(Err(error)) => {
            crate::client::render::status(
                "harness",
                "Warning",
                &format!("{name} unavailable: {error}"),
            );
            None
        }
        Err(_) => {
            crate::client::render::status(
                "harness",
                "Warning",
                &format!("{name} discovery timed out; shell remains available"),
            );
            None
        }
    }
}

impl Backend {
    pub async fn start(shell_path: &Path) -> crate::Result<Arc<Self>> {
        let terminal = Terminal::spawn_live(shell_path)?;
        let shell = shell::Shell::new();
        let (codex, claude, pi, opencode) = tokio::join!(
            discover("codex", 90, |program| async move {
                codex::Codex::start(&program, &[]).await
            }),
            discover("claude", 45, |program| async move {
                cc::Claude::start(&program, &[]).await
            }),
            discover("pi", 30, |program| async move {
                pi::Pi::start(&program, &[]).await
            }),
            discover("opencode", 45, |program| async move {
                opencode::OpenCode::start(&program, &[]).await
            }),
        );
        Ok(Arc::new(Self {
            terminal,
            shell,
            codex,
            claude,
            pi,
            opencode,
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = terminal_tools().as_array().unwrap().clone();
        tools.extend(shell::Shell::tools());
        tools.push(json!({
            "name":"ssmcp.harnesses.list",
            "description":"List coding harnesses detected on this client and the number of native tools registered for each.",
            "inputSchema":{"type":"object","additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        }));
        if let Some(driver) = &self.codex {
            append_harness_tools(&mut tools, "Codex", driver.tools()).await;
        }
        if let Some(driver) = &self.claude {
            append_harness_tools(&mut tools, "Claude Code", driver.tools()).await;
        }
        if let Some(driver) = &self.pi {
            append_harness_tools(&mut tools, "Pi", driver.tools()).await;
        }
        if let Some(driver) = &self.opencode {
            append_harness_tools(&mut tools, "OpenCode", driver.tools()).await;
        }
        validate_catalog(&tools)?;
        Ok(tools)
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::ListTools { cursor } => page(self.tools().await?, cursor.as_deref()),
            Operation::CallTool { name, arguments } => {
                if name.starts_with("terminal.") {
                    return self
                        .terminal
                        .perform(terminal_operation(&name, arguments)?)
                        .await
                        .map(result);
                }
                if name.starts_with("ssmcp.exec.") {
                    return self.shell.call(&name, arguments).await.map(result);
                }
                if name == "ssmcp.harnesses.list" {
                    if !arguments.as_object().is_some_and(|args| args.is_empty()) {
                        return Err("ssmcp.harnesses.list takes no arguments".into());
                    }
                    let tools = self.tools().await?;
                    let counts = ["codex.", "cc.", "pi.", "opencode."]
                        .iter()
                        .map(|prefix| {
                            let count = tools
                                .iter()
                                .filter(|tool| {
                                    tool["name"].as_str().is_some_and(|n| n.starts_with(prefix))
                                })
                                .count();
                            (prefix.trim_end_matches('.').to_owned(), json!(count))
                        })
                        .collect::<serde_json::Map<String, Value>>();
                    return Ok(result(json!({"harnesses":counts})));
                }
                if name.starts_with("codex.") || name.starts_with("ssmcp.codex.") {
                    self.audit_harness("codex", &name, &arguments);
                    return self
                        .codex
                        .as_ref()
                        .ok_or("Codex is not installed or failed native startup")?
                        .call(&name, arguments)
                        .await;
                }
                if name.starts_with("cc.") || name.starts_with("ssmcp.cc.") {
                    self.audit_harness("claude-code", &name, &arguments);
                    return self
                        .claude
                        .as_ref()
                        .ok_or("Claude Code is not installed or failed native startup")?
                        .call(&name, arguments)
                        .await;
                }
                if name.starts_with("pi.") || name.starts_with("ssmcp.pi.") {
                    self.audit_harness("pi", &name, &arguments);
                    return self
                        .pi
                        .as_ref()
                        .ok_or("Pi is not installed or failed native startup")?
                        .call(&name, arguments)
                        .await;
                }
                if name.starts_with("opencode.") {
                    self.audit_harness("opencode", &name, &arguments);
                    return self
                        .opencode
                        .as_ref()
                        .ok_or("OpenCode is not installed or failed native startup")?
                        .call(&name, arguments)
                        .await;
                }
                Err(format!("unknown registered tool: {name}"))
            }
            operation => self.terminal.perform(operation).await.map(result),
        }
    }

    fn audit_harness(&self, harness: &str, tool: &str, arguments: &Value) {
        let lower = tool.to_ascii_lowercase();
        let path = ["file_path", "filePath", "path", "file"]
            .iter()
            .find_map(|key| arguments.get(*key).and_then(Value::as_str));
        let skill = ["skill", "skill_name", "skillName", "name"]
            .iter()
            .find_map(|key| arguments.get(*key).and_then(Value::as_str));
        let kind = if lower.contains("skill") {
            "skill_used"
        } else if path.is_some()
            && (lower.contains("write") || lower.contains("edit") || lower.contains("patch"))
        {
            "file_change"
        } else if path.is_some()
            && (lower.contains("read") || lower.ends_with(".get") || lower.contains("file"))
        {
            "file_read"
        } else {
            "harness_tool_call"
        };
        self.shell.audit(json!({"type":kind,"harness":harness,"tool":tool,"path":path,"skill":if lower.contains("skill"){skill}else{None}}));
    }

    pub async fn shutdown(&self) {
        tokio::join!(
            self.terminal.shutdown(),
            self.shell.shutdown(),
            async {
                if let Some(driver) = &self.codex {
                    driver.shutdown().await;
                }
            },
            async {
                if let Some(driver) = &self.claude {
                    driver.shutdown().await;
                }
            },
            async {
                if let Some(driver) = &self.pi {
                    driver.shutdown().await;
                }
            },
            async {
                if let Some(driver) = &self.opencode {
                    driver.shutdown().await;
                }
            },
        );
    }
}

fn validate_catalog(tools: &[Value]) -> Result<(), String> {
    if tools.len() > 8192 {
        return Err("combined native tool catalog exceeds 8192 tools".into());
    }
    let mut names = std::collections::HashSet::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .ok_or("native tool has no name")?;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(format!("native tool name is not MCP compatible: {name:?}"));
        }
        if !names.insert(name) {
            return Err(format!(
                "installed harnesses expose a duplicate tool name: {name}"
            ));
        }
        if !tool.get("inputSchema").is_some_and(Value::is_object) {
            return Err(format!("native tool {name} has no input schema"));
        }
        if tool.to_string().len() > crate::wire::MAX_FRAME / 2 {
            return Err(format!(
                "native tool {name} exceeds the transport schema limit"
            ));
        }
    }
    Ok(())
}

async fn append_harness_tools(
    tools: &mut Vec<Value>,
    harness: &str,
    catalog: impl std::future::Future<Output = Result<Vec<Value>, String>>,
) {
    let result = match tokio::time::timeout(std::time::Duration::from_secs(15), catalog).await {
        Ok(result) => result,
        Err(_) => Err("tool discovery timed out".into()),
    };
    let old_len = tools.len();
    let failure = match result {
        Ok(catalog) => {
            tools.extend(catalog);
            validate_catalog(tools).err()
        }
        Err(error) => Some(error),
    };
    if let Some(error) = failure {
        tools.truncate(old_len);
        harness_status(
            "Warning",
            &format!("{harness} tools unavailable: {error}; shell remains available"),
        );
    }
}

pub(crate) fn harness_status(level: &str, message: &str) {
    crate::client::render::status("harness", level, message);
}

/// Convert a shell capability into a validated private wire operation.
fn terminal_operation(name: &str, arguments: Value) -> Result<Operation, String> {
    let op = match name {
        "terminal.read" => "read",
        "terminal.write" => "write",
        "terminal.resize" => "resize",
        "terminal.status" => "status",
        _ => return Err(format!("unknown shell capability: {name}")),
    };
    let mut arguments = arguments
        .as_object()
        .cloned()
        .ok_or("tool arguments must be an object")?;
    if arguments.contains_key("op") {
        return Err("unexpected tool argument: op".into());
    }
    arguments.insert("op".into(), Value::String(op.into()));
    let operation: Operation = serde_json::from_value(Value::Object(arguments))
        .map_err(|e| format!("invalid shell arguments: {e}"))?;
    operation.validate()?;
    Ok(operation)
}

fn terminal_tools() -> Value {
    json!([
        {
            "name":"terminal.read",
            "description":"Read terminal output from a byte cursor. Reuse the returned cursor for the next read; output is retained in a bounded buffer.",
            "inputSchema":{"type":"object","properties":{
                "cursor":{"type":"integer","minimum":0,"default":0},
                "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_READ,"default":16384},
                "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":0}
            },"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        },
        {
            "name":"terminal.write",
            "description":"Write text or control characters to the terminal. Append a newline to run a command; use \\u0003 for Ctrl-C. The limit is 16384 UTF-8 bytes.",
            "inputSchema":{"type":"object","properties":{"data":{"type":"string","maxLength":MAX_WRITE}},"required":["data"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":false,"openWorldHint":true}
        },
        {
            "name":"terminal.resize",
            "description":"Set terminal rows and columns.",
            "inputSchema":{"type":"object","properties":{
                "rows":{"type":"integer","minimum":1,"maximum":1000},
                "cols":{"type":"integer","minimum":1,"maximum":1000}
            },"required":["rows","cols"],"additionalProperties":false},
            "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}
        },
        {
            "name":"terminal.status",
            "description":"Inspect terminal process state and output cursor.",
            "inputSchema":{"type":"object","additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }
    ])
}

// The cursor binds to catalog names/schema content, so list changes cannot silently skip tools.
pub(crate) fn page(tools: Vec<Value>, cursor: Option<&str>) -> Result<Value, String> {
    let encoded = serde_json::to_vec(&tools).map_err(|e| e.to_string())?;
    // A non-cryptographic fingerprint is sufficient: this is stale-page detection, not auth.
    let hash = encoded.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    let start = match cursor {
        None => 0,
        Some(cursor) => {
            let (fingerprint, index) = cursor.split_once(':').ok_or("invalid tool cursor")?;
            if fingerprint != format!("{hash:016x}") {
                return Err("tool catalog changed; restart tools/list without a cursor".into());
            }
            index.parse::<usize>().map_err(|_| "invalid tool cursor")?
        }
    };
    if start > tools.len() {
        return Err("tool cursor is beyond the catalog".into());
    }
    let mut end = start;
    let mut bytes = 0;
    while end < tools.len() && end - start < 64 {
        let length = tools[end].to_string().len();
        if length > crate::wire::MAX_FRAME / 2 {
            return Err("one tool schema exceeds the transport limit".into());
        }
        if bytes + length > crate::wire::MAX_FRAME / 2 {
            break;
        }
        bytes += length;
        end += 1;
    }
    let mut page = json!({"tools":&tools[start..end]});
    if end < tools.len() {
        page["nextCursor"] = json!(format!("{hash:016x}:{end}"));
    }
    Ok(page)
}

pub fn result(value: Value) -> Value {
    let mut output = json!({"content":[{"type":"text","text":value.to_string()}],"isError":false});
    if value.is_object() {
        output["structuredContent"] = value;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn unavailable_harness_preserves_shell_catalog() {
        let mut tools = terminal_tools().as_array().unwrap().clone();
        let original = tools.clone();
        append_harness_tools(&mut tools, "fixture", async { Err("peer closed".into()) }).await;
        assert_eq!(tools, original);
        append_harness_tools(&mut tools, "fixture", async {
            Ok(vec![
                json!({"name":"terminal.read","inputSchema":{"type":"object"}}),
            ])
        })
        .await;
        assert_eq!(
            tools, original,
            "a collision must reject only the optional catalogue"
        );
        append_harness_tools(&mut tools, "fixture", async {
            Ok(vec![
                json!({"name":"fixture.read","inputSchema":{"type":"object"}}),
            ])
        })
        .await;
        assert_eq!(tools.len(), original.len() + 1);
    }
    #[test]
    fn shell_capabilities_use_the_same_validated_dispatch_inside_code_mode() {
        assert!(matches!(
            terminal_operation("terminal.read", json!({})),
            Ok(Operation::Read {
                cursor: 0,
                max_bytes: 16384,
                wait_ms: 0
            })
        ));
        for arguments in [
            json!({"max_bytes":MAX_READ+1}),
            json!({"wait_ms":MAX_WAIT_MS+1}),
            json!({"cursor":-1}),
            json!({"op":"status"}),
            json!({"extra":true}),
        ] {
            assert!(terminal_operation("terminal.read", arguments).is_err());
        }
        assert!(
            terminal_operation("terminal.write", json!({"data":"界".repeat(MAX_WRITE/3+1)}))
                .is_err()
        );
        assert!(terminal_operation("terminal.resize", json!({"rows":0,"cols":80})).is_err());
        assert!(terminal_operation("unknown", json!({})).is_err());
    }

    #[tokio::test]
    async fn shell_calls_reach_the_terminal_through_generic_native_dispatch() {
        let terminal = Terminal::spawn(&crate::platform::default_shell()).unwrap();
        let backend = Backend {
            terminal: terminal.clone(),
            shell: shell::Shell::new(),
            codex: None,
            claude: None,
            pi: None,
            opencode: None,
        };
        let result = backend
            .perform(Operation::CallTool {
                name: "terminal.status".into(),
                arguments: json!({}),
            })
            .await
            .unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(result["structuredContent"]["running"], true);
        terminal.shutdown().await;
    }

    #[test]
    fn tool_pagination_detects_catalog_changes() {
        let tools: Vec<_> = (0..150)
            .map(|n| json!({"name":format!("tool{n}")}))
            .collect();
        let first = page(tools.clone(), None).unwrap();
        assert_eq!(first["tools"].as_array().unwrap().len(), 64);
        let second = page(tools.clone(), first["nextCursor"].as_str()).unwrap();
        assert_eq!(second["tools"][0]["name"], "tool64");
        let third = page(tools.clone(), second["nextCursor"].as_str()).unwrap();
        assert_eq!(third["tools"].as_array().unwrap().len(), 22);
        assert!(third.get("nextCursor").is_none());
        assert!(page(tools[1..].to_vec(), first["nextCursor"].as_str()).is_err());
    }
}
