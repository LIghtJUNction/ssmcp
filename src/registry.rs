//! The single authority for callable capabilities on one live client connection.
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Terminal,
    Exec,
    Browser,
    Computer,
    Harnesses,
    Workspace,
    Context,
    Rsync,
    Output,
    Reload,
    Audit,
    Codex,
    Claude,
    Pi,
    OpenCode,
}
impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Exec => "exec",
            Self::Browser => "browser",
            Self::Computer => "computer",
            Self::Harnesses => "harnesses",
            Self::Workspace => "workspace",
            Self::Context => "context",
            Self::Rsync => "rsync",
            Self::Output => "output",
            Self::Reload => "reload",
            Self::Audit => "audit",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
        }
    }
    pub fn is_harness(self) -> bool {
        matches!(self, Self::Codex | Self::Claude | Self::Pi | Self::OpenCode)
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Native,
    AuthorizedConnection,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolAvailability {
    Ready,
}

#[derive(Clone, Debug, Serialize)]
pub struct RegisteredTool {
    pub id: String,
    pub provider: Provider,
    pub native_name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
    pub approval: ApprovalPolicy,
    pub availability: ToolAvailability,
    pub generation: u64,
    // Preserve the complete native object, including extensions, refs and annotations.
    pub descriptor: Value,
}
#[derive(Default)]
struct State {
    tools: BTreeMap<String, RegisteredTool>,
    generation: u64,
    closed: bool,
}
#[derive(Default)]
pub struct Registry {
    state: RwLock<State>,
}
impl Registry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Replace one provider atomically. Failure revokes only that provider's old snapshot.
    pub fn replace(&self, provider: Provider, tools: Vec<Value>) -> Result<u64, String> {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        let prepared = prepare(provider, tools);
        let entries = match prepared {
            Ok(entries) => entries,
            Err(error) => {
                state.tools.retain(|_, tool| tool.provider != provider);
                state.generation += 1;
                return Err(error);
            }
        };
        if entries.keys().any(|id| {
            state
                .tools
                .get(id)
                .is_some_and(|tool| tool.provider != provider)
        }) {
            state.tools.retain(|_, tool| tool.provider != provider);
            state.generation += 1;
            return Err(format!(
                "{} tried to register a name owned by another provider",
                provider.label()
            ));
        }
        let old: BTreeMap<_, _> = state
            .tools
            .iter()
            .filter(|(_, t)| t.provider == provider)
            .map(|(id, t)| (id.clone(), t.descriptor.clone()))
            .collect();
        let next: BTreeMap<_, _> = entries
            .iter()
            .map(|(id, t)| (id.clone(), t.descriptor.clone()))
            .collect();
        if old == next {
            return Ok(state.generation);
        }
        if state.tools.len().saturating_sub(old.len()) + entries.len() > 8192 {
            state.tools.retain(|_, tool| tool.provider != provider);
            state.generation += 1;
            return Err("combined tool registry exceeds 8192 entries".into());
        }
        state.generation += 1;
        let generation = state.generation;
        state.tools.retain(|_, tool| tool.provider != provider);
        for (id, mut entry) in entries {
            entry.generation = generation;
            state.tools.insert(id, entry);
        }
        Ok(generation)
    }
    pub fn invalidate(&self, provider: Provider) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if state.tools.values().any(|t| t.provider == provider) {
            state.tools.retain(|_, t| t.provider != provider);
            state.generation += 1;
        }
    }
    pub fn ensure_open(&self) -> Result<(), String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        Ok(())
    }
    pub fn catalog(&self) -> Result<Vec<Value>, String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        Ok(state.tools.values().map(|t| t.descriptor.clone()).collect())
    }
    pub fn lookup(&self, id: &str) -> Result<RegisteredTool, String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        state
            .tools
            .get(id)
            .cloned()
            .ok_or_else(|| format!("tool is unavailable or not registered: {id}; refresh tools"))
    }
    pub fn validate(&self, tool: &RegisteredTool) -> Result<(), String> {
        let current = self.lookup(&tool.id)?;
        if current.provider != tool.provider
            || current.generation != tool.generation
            || current.native_name != tool.native_name
        {
            return Err("tool registration changed before dispatch; refresh tools".into());
        }
        Ok(())
    }
    pub fn generation(&self) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }
    pub fn close(&self) {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.closed = true;
        state.generation += 1;
        state.tools.clear();
    }
}
fn prepare(
    provider: Provider,
    tools: Vec<Value>,
) -> Result<BTreeMap<String, RegisteredTool>, String> {
    let mut result = BTreeMap::new();
    for mut descriptor in tools {
        let native_name = descriptor["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("native tool has no name")?
            .to_owned();
        let id = if provider == Provider::Claude {
            if let Some(name) = native_name.strip_prefix("cc.") {
                format!("claude.{}", component(name))
            } else if let Some(name) = native_name.strip_prefix("ssmcp.cc.") {
                format!("ssmcp.claude.{name}")
            } else {
                return Err("Claude returned a tool outside its namespace".into());
            }
        } else {
            native_name.clone()
        };
        let allowed = match provider {
            Provider::Terminal => id.starts_with("terminal."),
            Provider::Exec => id.starts_with("ssmcp.exec."),
            Provider::Browser => id.starts_with("ssmcp.browser."),
            Provider::Computer => id.starts_with("ssmcp.computer."),
            Provider::Harnesses => id == "ssmcp.harnesses.list",
            Provider::Workspace => id.starts_with("ssmcp.fs.") || id.starts_with("ssmcp.net."),
            Provider::Context => ["ssmcp.skills.", "ssmcp.agents.", "ssmcp.context."]
                .iter()
                .any(|p| id.starts_with(p)),
            Provider::Rsync => id.starts_with("ssmcp.rsync."),
            Provider::Output => id.starts_with("ssmcp.output."),
            Provider::Reload => id == "ssmcp.client.reload",
            Provider::Audit => id == "ssmcp.audit.events",
            Provider::Codex => id.starts_with("codex.") || id.starts_with("ssmcp.codex."),
            Provider::Claude => true,
            Provider::Pi => id.starts_with("pi.") || id.starts_with("ssmcp.pi."),
            Provider::OpenCode => id.starts_with("opencode."),
        };
        if !allowed
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(format!("invalid {} route: {id}", provider.label()));
        }
        if descriptor.to_string().len() > 896 * 1024 {
            return Err(format!(
                "native tool {native_name} exceeds the transport schema limit"
            ));
        }
        let input_schema = descriptor
            .get("inputSchema")
            .filter(|s| s.is_object())
            .ok_or_else(|| format!("native tool {native_name} has no enumerable input schema"))?
            .clone();
        descriptor["name"] = id.clone().into();
        let tool = RegisteredTool {
            id: id.clone(),
            provider,
            native_name,
            description: descriptor["description"].as_str().map(str::to_owned),
            input_schema,
            output_schema: descriptor.get("outputSchema").cloned(),
            approval: if provider.is_harness() {
                ApprovalPolicy::Native
            } else {
                ApprovalPolicy::AuthorizedConnection
            },
            availability: ToolAvailability::Ready,
            generation: 0,
            descriptor,
        };
        if result.insert(id.clone(), tool).is_some() {
            return Err(format!("duplicate provider route: {id}"));
        }
    }
    Ok(result)
}
fn component(name: &str) -> String {
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("_{b:02X}"));
        }
    }
    if encoded.len() > 96 {
        use sha2::{Digest, Sha256};
        format!("_h{:x}", Sha256::digest(name.as_bytes()))
    } else {
        encoded
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn tool(name: &str) -> Value {
        json!({"name":name,"description":"native","inputSchema":{"$defs":{"value":{"oneOf":[{"const":null},{"type":"string"}]}},"type":"object","properties":{"x":{"$ref":"#/$defs/value"}}},"outputSchema":{"type":"object"},"annotations":{"custom":true},"x-native":{"future":1}})
    }
    #[test]
    fn native_schema_and_extensions_survive_exactly() {
        let registry = Registry::new();
        let native = tool("cc.Read");
        registry
            .replace(Provider::Claude, vec![native.clone()])
            .unwrap();
        let routed = registry.lookup("claude.Read").unwrap();
        let mut expected = native;
        expected["name"] = "claude.Read".into();
        assert_eq!(routed.descriptor, expected);
        assert_eq!(routed.native_name, "cc.Read");
    }
    #[test]
    fn snapshots_isolate_failures_reject_collisions_and_stale_routes() {
        let registry = Registry::new();
        registry
            .replace(Provider::Exec, vec![tool("ssmcp.exec.start")])
            .unwrap();
        registry
            .replace(Provider::Codex, vec![tool("codex.a")])
            .unwrap();
        let old = registry.lookup("codex.a").unwrap();
        let mut changed = tool("codex.a");
        changed["inputSchema"]["additionalProperties"] = false.into();
        registry.replace(Provider::Codex, vec![changed]).unwrap();
        assert!(registry.validate(&old).is_err());
        assert!(
            registry
                .replace(Provider::Codex, vec![tool("ssmcp.exec.start")])
                .is_err()
        );
        assert!(registry.lookup("codex.a").is_err());
        assert!(registry.lookup("ssmcp.exec.start").is_ok());
        registry.close();
        assert!(registry.lookup("ssmcp.exec.start").is_err());
    }
    #[test]
    fn names_are_injective_and_missing_schemas_are_not_invented() {
        assert_ne!(component("a.b"), component("a_b"));
        let registry = Registry::new();
        assert!(
            registry
                .replace(Provider::Pi, vec![json!({"name":"pi.read"})])
                .is_err()
        );
        assert!(registry.catalog().unwrap().is_empty());
    }
}
