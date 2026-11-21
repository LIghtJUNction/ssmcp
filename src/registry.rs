//! The single authority for callable capabilities on one live client connection.
use crate::catalog::{self, ToolDescriptor};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, btree_map::Entry},
    sync::{Arc, RwLock},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Client,
    Terminal,
    Ui,
    Exec,
    Time,
    Computer,
    Harnesses,
    Plugins,
    Workspace,
    Context,
    Rsync,
    Output,
    Reload,
    Sessions,
    Audit,
    Codex,
    Claude,
    Pi,
    OpenCode,
    #[serde(rename = "ssmcp")]
    Peer,
}

pub(crate) const OPTIONAL_PROVIDERS: [(Provider, &str); 5] = [
    (Provider::Codex, "codex"),
    (Provider::Claude, "claude"),
    (Provider::Pi, "pi"),
    (Provider::OpenCode, "opencode"),
    (Provider::Peer, "ssmcp"),
];
impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Client => "client",
            Self::Terminal => "terminal",
            Self::Ui => "ui",
            Self::Exec => "exec",
            Self::Time => "time",
            Self::Computer => "computer",
            Self::Harnesses => "harnesses",
            Self::Plugins => "plugins",
            Self::Workspace => "workspace",
            Self::Context => "context",
            Self::Rsync => "rsync",
            Self::Output => "output",
            Self::Reload => "reload",
            Self::Sessions => "sessions",
            Self::Audit => "audit",
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::OpenCode => "opencode",
            Self::Peer => "ssmcp",
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
struct StoredTool {
    provider: Provider,
    native_name: String,
    generation: u64,
    descriptor: ToolDescriptor,
}

impl StoredTool {
    fn materialize(&self, id: &str) -> Result<RegisteredTool, String> {
        let descriptor = self.descriptor.value()?;
        Ok(RegisteredTool {
            id: id.into(),
            provider: self.provider,
            native_name: self.native_name.clone(),
            description: descriptor["description"].as_str().map(str::to_owned),
            input_schema: descriptor["inputSchema"].clone(),
            output_schema: descriptor.get("outputSchema").cloned(),
            approval: if self.provider.is_harness() || self.provider == Provider::Peer {
                ApprovalPolicy::Native
            } else {
                ApprovalPolicy::AuthorizedConnection
            },
            availability: ToolAvailability::Ready,
            generation: self.generation,
            descriptor,
        })
    }
}

#[derive(Default)]
struct State {
    tools: BTreeMap<String, StoredTool>,
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
        self.replace_catalog(provider, catalog::compact(tools)?)
    }

    pub(crate) fn replace_catalog(
        &self,
        provider: Provider,
        tools: Vec<ToolDescriptor>,
    ) -> Result<u64, String> {
        self.replace_catalog_inner(provider, tools, true)
    }

    /// Transactional extension updates must not revoke previously accepted tools.
    pub(crate) fn replace_preserving(
        &self,
        provider: Provider,
        tools: Vec<Value>,
    ) -> Result<u64, String> {
        self.replace_catalog_inner(provider, catalog::compact(tools)?, false)
    }

    fn replace_catalog_inner(
        &self,
        provider: Provider,
        tools: Vec<ToolDescriptor>,
        invalidate_on_error: bool,
    ) -> Result<u64, String> {
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        let prepared = prepare(provider, tools);
        let entries = match prepared {
            Ok(entries) => entries,
            Err(error) => {
                if invalidate_on_error {
                    state.tools.retain(|_, tool| tool.provider != provider);
                    state.generation += 1;
                }
                return Err(error);
            }
        };
        if entries.keys().any(|id| {
            state
                .tools
                .get(id)
                .is_some_and(|tool| tool.provider != provider)
        }) {
            if invalidate_on_error {
                state.tools.retain(|_, tool| tool.provider != provider);
                state.generation += 1;
            }
            return Err(format!(
                "{} tried to register a name owned by another provider",
                provider.label()
            ));
        }
        let old_count = state
            .tools
            .values()
            .filter(|t| t.provider == provider)
            .count();
        if old_count == entries.len()
            && entries.iter().all(|(id, next)| {
                state.tools.get(id).is_some_and(|old| {
                    old.native_name == next.native_name && old.descriptor == next.descriptor
                })
            })
        {
            return Ok(state.generation);
        }
        if state.tools.len().saturating_sub(old_count) + entries.len() > 8192 {
            if invalidate_on_error {
                state.tools.retain(|_, tool| tool.provider != provider);
                state.generation += 1;
            }
            return Err("combined tool registry exceeds 8192 entries".into());
        }
        state.generation += 1;
        let generation = state.generation;
        state
            .tools
            .retain(|id, tool| tool.provider != provider || entries.contains_key(id));
        for (id, mut entry) in entries {
            entry.generation = generation;
            match state.tools.entry(id) {
                Entry::Occupied(mut current) => {
                    let previous = current.get_mut();
                    if previous.native_name == entry.native_name
                        && previous.descriptor == entry.descriptor
                    {
                        // Keep the existing allocations, but revoke handles from
                        // the old provider generation even for unchanged tools.
                        previous.generation = generation;
                    } else {
                        *previous = entry;
                    }
                }
                Entry::Vacant(current) => {
                    current.insert(entry);
                }
            }
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
        self.snapshot()?.iter().map(ToolDescriptor::value).collect()
    }
    pub(crate) fn snapshot(&self) -> Result<Vec<ToolDescriptor>, String> {
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
            .ok_or_else(|| format!("tool is unavailable or not registered: {id}; refresh tools"))?
            .materialize(id)
    }
    pub fn validate(&self, tool: &RegisteredTool) -> Result<(), String> {
        self.validate_route(&tool.id, tool.provider, tool.generation, &tool.native_name)
    }
    /// Validate a queued route without retaining a second copy of its schemas.
    pub(crate) fn validate_route(
        &self,
        id: &str,
        provider: Provider,
        generation: u64,
        native_name: &str,
    ) -> Result<(), String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        if !state.tools.get(id).is_some_and(|current| {
            current.provider == provider
                && current.generation == generation
                && current.native_name == native_name
        }) {
            return Err("tool registration changed before dispatch; refresh tools".into());
        }
        Ok(())
    }
    /// Bounded cached route names, without materializing or changing native schemas.
    pub(crate) fn provider_catalog_summary(&self, provider: Provider) -> Result<Value, String> {
        let state = self.state.read().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        let mut names = Vec::new();
        let mut count = 0usize;
        for (name, tool) in &state.tools {
            if tool.provider == provider {
                count += 1;
                if names.len() < 32 {
                    names.push(name.clone());
                }
            }
        }
        Ok(serde_json::json!({
            "provider":provider.label(), "generation":state.generation,
            "tool_count":count, "truncated":count>names.len(), "tool_names":names
        }))
    }

    /// Small, atomic capability summary; native schemas remain on the client.
    pub fn capabilities(&self) -> Result<Value, String> {
        let state = self.state.read().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        let mut providers = BTreeMap::<&str, usize>::new();
        for tool in state.tools.values() {
            *providers.entry(tool.provider.label()).or_default() += 1;
        }
        Ok(serde_json::json!({
            "generation": state.generation,
            "tool_count": state.tools.len(),
            "providers": providers,
        }))
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
    tools: Vec<ToolDescriptor>,
) -> Result<BTreeMap<String, StoredTool>, String> {
    let mut result = BTreeMap::new();
    for mut descriptor in tools {
        let (native_name, has_input_schema) = descriptor.header()?;
        if native_name.is_empty() {
            return Err("native tool has no name".into());
        }
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
            Provider::Client => {
                id == "ssmcp.client.capabilities" || id.starts_with("ssmcp.resources.")
            }
            Provider::Terminal => id.starts_with("terminal."),
            Provider::Ui => id.starts_with("ssmcp.ui."),
            Provider::Exec => id.starts_with("ssmcp.exec."),
            Provider::Time => id.starts_with("ssmcp.time."),
            Provider::Computer => matches!(
                id.as_str(),
                "ssmcp.computer.version"
                    | "ssmcp.computer.download"
                    | "ssmcp.computer.install"
                    | "ssmcp.computer.uninstall"
            ),
            Provider::Harnesses => {
                matches!(id.as_str(), "ssmcp.harnesses.list" | "ssmcp.harnesses.load")
            }
            Provider::Plugins => {
                matches!(
                    id.as_str(),
                    "ssmcp.plugins.list" | "ssmcp.plugins.load" | "ssmcp.plugins.unload"
                ) || id
                    .strip_prefix("plugins.")
                    .and_then(|name| name.split_once('.'))
                    .is_some_and(|(namespace, name)| {
                        !namespace.is_empty()
                            && namespace.len() <= 32
                            && namespace
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                            && !name.is_empty()
                    })
            }
            Provider::Workspace => id.starts_with("ssmcp.fs.") || id.starts_with("ssmcp.net."),
            Provider::Context => ["ssmcp.skills.", "ssmcp.agents.", "ssmcp.context."]
                .iter()
                .any(|p| id.starts_with(p)),
            Provider::Rsync => id.starts_with("ssmcp.rsync."),
            Provider::Output => id.starts_with("ssmcp.output."),
            Provider::Reload => id == "ssmcp.client.reload",
            Provider::Sessions => id.starts_with("ssmcp.session."),
            Provider::Audit => id == "ssmcp.audit.events",
            Provider::Codex => id.starts_with("codex.") || id.starts_with("ssmcp.codex."),
            Provider::Claude => true,
            Provider::Pi => id.starts_with("pi.") || id.starts_with("ssmcp.pi."),
            Provider::OpenCode => id.starts_with("opencode."),
            Provider::Peer => id.starts_with("peer."),
        };
        if !allowed
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(format!("invalid {} route: {id}", provider.label()));
        }
        if descriptor.len() > 896 * 1024 {
            return Err(format!(
                "native tool {native_name} exceeds the transport schema limit"
            ));
        }
        if !has_input_schema {
            return Err(format!(
                "native tool {native_name} has no enumerable input schema"
            ));
        }
        if id != native_name {
            let mut value = descriptor.value()?;
            value["name"] = id.clone().into();
            descriptor = ToolDescriptor::from_value(value)?;
        }
        let tool = StoredTool {
            provider,
            native_name,
            generation: 0,
            descriptor,
        };
        if result.insert(id.clone(), tool).is_some() {
            return Err(format!("duplicate provider route: {id}"));
        }
    }
    Ok(result)
}
pub(crate) fn component(name: &str) -> String {
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
        format!("_h{}", crate::hex::encode(Sha256::digest(name.as_bytes())))
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
    fn partial_catalog_updates_reuse_allocations_and_revoke_old_handles() {
        let registry = Registry::new();
        registry
            .replace(Provider::Exec, vec![tool("ssmcp.exec.start")])
            .unwrap();
        let other = registry.lookup("ssmcp.exec.start").unwrap();
        let before = registry
            .replace(
                Provider::Codex,
                vec![
                    tool("codex.keep"),
                    tool("codex.change"),
                    tool("codex.remove"),
                ],
            )
            .unwrap();
        let old = registry.lookup("codex.keep").unwrap();
        let removed = registry.lookup("codex.remove").unwrap();
        let (key_pointer, native_pointer, descriptor) = {
            let state = registry.state.read().unwrap();
            let (key, stored) = state.tools.get_key_value("codex.keep").unwrap();
            (
                key.as_ptr(),
                stored.native_name.as_ptr(),
                stored.descriptor.clone(),
            )
        };

        let mut changed = tool("codex.change");
        changed["inputSchema"]["additionalProperties"] = false.into();
        let generation = registry
            .replace(
                Provider::Codex,
                vec![
                    tool("codex.keep"),
                    changed.clone(),
                    tool("codex.new1"),
                    tool("codex.new2"),
                ],
            )
            .unwrap();
        assert!(generation > before);
        {
            let state = registry.state.read().unwrap();
            let (key, stored) = state.tools.get_key_value("codex.keep").unwrap();
            assert_eq!(key.as_ptr(), key_pointer);
            assert_eq!(stored.native_name.as_ptr(), native_pointer);
            assert!(stored.descriptor.shares_storage(&descriptor));
            assert!(
                state
                    .tools
                    .values()
                    .filter(|tool| tool.provider == Provider::Codex)
                    .all(|tool| tool.generation == generation)
            );
        }
        assert!(registry.validate(&old).is_err());
        assert!(registry.validate(&removed).is_err());
        assert!(registry.lookup("codex.remove").is_err());
        assert!(
            registry
                .validate(&registry.lookup("codex.keep").unwrap())
                .is_ok()
        );
        assert_eq!(registry.lookup("codex.change").unwrap().descriptor, changed);
        assert!(registry.lookup("codex.new1").is_ok());
        assert!(registry.lookup("codex.new2").is_ok());
        assert!(registry.validate(&other).is_ok());
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
