//! One local backend per SSH registration; the daemon only forwards MCP messages.
pub mod cc;
pub mod codex;
pub mod events;
pub mod opencode;
pub mod pi;
pub mod process;
pub mod rpc;

use crate::{
    pty::Terminal,
    wire::{Mode, Operation},
};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    path::Path,
    sync::Arc,
};

pub enum Backend {
    Shell(Arc<Terminal>),
    Codex(Arc<codex::Codex>),
    Claude(Arc<cc::Claude>),
    Pi(Arc<pi::Pi>),
    OpenCode(Arc<opencode::OpenCode>),
}

impl Backend {
    pub async fn start(
        mode: Mode,
        shell: &Path,
        program: Option<&OsStr>,
        args: &[OsString],
    ) -> crate::Result<Arc<Self>> {
        let backend = match mode {
            Mode::Shell => Self::Shell(Terminal::spawn_live(shell)?),
            Mode::Codex => Self::Codex(
                codex::Codex::start(program.unwrap_or_else(|| OsStr::new("codex")), args).await?,
            ),
            Mode::Cc => Self::Claude(
                cc::Claude::start(program.unwrap_or_else(|| OsStr::new("claude")), args).await?,
            ),
            Mode::Pi => {
                Self::Pi(pi::Pi::start(program.unwrap_or_else(|| OsStr::new("pi")), args).await?)
            }
            Mode::Opencode => Self::OpenCode(
                opencode::OpenCode::start(program.unwrap_or_else(|| OsStr::new("opencode")), args)
                    .await?,
            ),
        };
        Ok(Arc::new(backend))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let tools = match self {
            Self::Shell(_) => Ok(crate::http::tool_definitions().as_array().unwrap().clone()),
            Self::Codex(driver) => driver.tools().await,
            Self::Claude(driver) => driver.tools().await,
            Self::Pi(driver) => driver.tools().await,
            Self::OpenCode(driver) => driver.tools().await,
        }?;
        if tools.len() > 4096 {
            return Err("native tool catalog exceeds 4096 tools".into());
        }
        let mut names = std::collections::HashSet::new();
        for tool in &tools {
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
                return Err(format!("duplicate native tool name: {name}"));
            }
            if !tool.get("inputSchema").is_some_and(Value::is_object) {
                return Err(format!("native tool {name} has no input schema"));
            }
        }
        Ok(tools)
    }

    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        operation.validate()?;
        match operation {
            Operation::ListTools { cursor } => {
                let tools = self.tools().await?;
                page(tools, cursor.as_deref())
            }
            Operation::CallTool { name, arguments } => match self {
                Self::Shell(_) => Err("unknown shell tool".into()),
                Self::Codex(driver) => driver.call(&name, arguments).await,
                Self::Claude(driver) => driver.call(&name, arguments).await,
                Self::Pi(driver) => driver.call(&name, arguments).await,
                Self::OpenCode(driver) => driver.call(&name, arguments).await,
            },
            operation => match self {
                Self::Shell(terminal) => Ok(result(terminal.perform(operation).await?)),
                _ => {
                    Err("terminal.* tools are only available in shell mode; call tools/list".into())
                }
            },
        }
    }

    pub async fn shutdown(&self) {
        match self {
            Self::Shell(terminal) => terminal.shutdown().await,
            Self::Codex(driver) => driver.shutdown().await,
            Self::Claude(driver) => driver.shutdown().await,
            Self::Pi(driver) => driver.shutdown().await,
            Self::OpenCode(driver) => driver.shutdown().await,
        }
    }
}

// The cursor binds to catalog names/schema content, so list changes cannot silently skip tools.
fn page(tools: Vec<Value>, cursor: Option<&str>) -> Result<Value, String> {
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
