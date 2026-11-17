//! Version-matched Codex App Server and its dynamically discovered MCP tools.
//! Native requests, results and approvals remain owned by the installed server.
use super::{
    result,
    rpc::{Dialect, Peer},
};
use crate::{catalog::ToolDescriptor, output::OutputStore};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fmt::Write,
    fs,
    io::{BufReader, Read},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::Mutex, time::timeout};

type Methods = BTreeMap<String, (String, bool)>;
const MCP_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MCP_PAGE_TIMEOUT: Duration = Duration::from_secs(20);
const MCP_BACKGROUND_REFRESH: Duration = Duration::from_secs(30);
const MAX_MCP_PAGES: usize = 128;
const MAX_MCP_TOOLS: usize = 4096;

#[derive(Clone)]
struct McpRoute {
    server: String,
    tool: String,
}

#[derive(Default)]
struct McpCatalog {
    tools: Vec<ToolDescriptor>,
    routes: BTreeMap<String, McpRoute>,
    omitted: Vec<Value>,
}

impl McpCatalog {
    fn reuse_unchanged(&mut self, previous: &Self) {
        // Both maps and descriptor vectors follow the same routing-name order.
        // Reuse payloads before publication so a healthy refresh keeps sharing
        // storage with the registry and any active pagination snapshots.
        let mut old = previous.routes.keys().zip(&previous.tools).peekable();
        for (name, descriptor) in self.routes.keys().zip(&mut self.tools) {
            while old
                .peek()
                .is_some_and(|(old_name, _)| old_name.as_str() < name.as_str())
            {
                old.next();
            }
            if let Some((old_name, old_descriptor)) = old.peek()
                && old_name.as_str() == name.as_str()
                && ToolDescriptor::eq(old_descriptor, descriptor)
            {
                *descriptor = ToolDescriptor::clone(old_descriptor);
            }
        }
    }
}

#[derive(Default)]
struct McpState {
    thread_id: Option<String>,
    catalog: McpCatalog,
    error: Option<String>,
    generation: u64,
    refreshing: bool,
    refreshed: Option<Instant>,
}

pub struct Codex {
    peer: Arc<Peer>,
    tools: Vec<ToolDescriptor>,
    methods: Methods,
    schemas: SchemaStore,
    // A snapshot lock never crosses network I/O. Healthy refreshes retain the
    // last complete snapshot; failure/cancellation revokes it atomically.
    mcp: std::sync::Mutex<McpState>,
    mcp_refresh: Mutex<()>,
    // A native list request may outlive our deadline; retain at most one rather
    // than accumulating uncancellable native requests on every refresh tick.
    mcp_page: Mutex<Option<tokio::task::JoinHandle<Result<Value, String>>>>,
    mcp_supported: bool,
}

impl Codex {
    pub async fn start(program: &OsStr, args: &[OsString]) -> crate::Result<Arc<Self>> {
        Self::start_with_output(program, args, OutputStore::global()?).await
    }

    pub async fn start_with_output(
        program: &OsStr,
        args: &[OsString],
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        for arg in args {
            if arg == "--listen" || arg.to_string_lossy().starts_with("--listen=") {
                return Err(
                    "ssmcp owns Codex's stdio transport; --listen is not a backend argument".into(),
                );
            }
        }
        let directory = SchemaDirectory::new()?;
        let generated = timeout(
            Duration::from_secs(60),
            Command::new(program)
                .args([
                    "app-server",
                    "generate-json-schema",
                    "--experimental",
                    "--out",
                ])
                .arg(&directory.0)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "Codex schema generation timed out")??;
        if !generated.status.success() {
            return Err(format!(
                "Codex must support app-server generate-json-schema --experimental: {}",
                String::from_utf8_lossy(&generated.stderr)
            )
            .into());
        }
        let schemas = load_schemas(directory)?;
        if !schemas.files.contains_key("ClientRequest.json") {
            return Err("Codex generated no ClientRequest.json".into());
        }
        let (tools, methods) = catalog(&schemas.read("ClientRequest.json")?)?;
        let tools = tools
            .into_iter()
            .map(ToolDescriptor::from_value)
            .collect::<Result<Vec<_>, _>>()?;
        let mcp_supported = [
            "thread/start",
            "mcpServerStatus/list",
            "mcpServer/tool/call",
        ]
        .iter()
        .all(|expected| {
            methods
                .values()
                .any(|(method, _)| method.as_str() == *expected)
        });
        let mut command = Command::new(program);
        command
            .args(["app-server", "--listen", "stdio://"])
            .args(args);
        let peer = Peer::spawn_with_output(command, "codex", Dialect::JsonRpc, output).await?;
        let initialized = async {
            peer.request("initialize", json!({
                "clientInfo":{"name":"ssmcp","title":"ssmcp","version":env!("CARGO_PKG_VERSION")},
                "capabilities":{"experimentalApi":true}
            })).await?;
            peer.notify("initialized", json!({})).await
        }
        .await;
        if let Err(error) = initialized {
            peer.shutdown().await;
            return Err(error.into());
        }
        let codex = Arc::new(Self {
            peer,
            tools,
            methods,
            schemas,
            mcp: std::sync::Mutex::new(McpState::default()),
            mcp_refresh: Mutex::new(()),
            mcp_page: Mutex::new(None),
            mcp_supported,
        });
        // MCP discovery is optional. A rejected or absent MCP capability does
        // not suppress the already-negotiated App Server control interface.
        let _ = codex.refresh_mcp().await;
        Ok(codex)
    }

    async fn connected(&self) -> bool {
        self.peer.pending().await["connected"] == true
    }

    async fn invalidate_mcp(&self, error: &str) {
        let mut state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
        state.catalog = McpCatalog::default();
        state.error = Some(error.to_owned());
        state.generation += 1;
        state.refreshed = None;
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        self.catalog_tools(true)
            .await?
            .iter()
            .map(ToolDescriptor::value)
            .collect()
    }

    /// Cheap liveness checks run every broker discovery tick; a healthy native
    /// catalog is polled at most once per interval, independently of user calls.
    pub async fn background_tools(&self) -> Result<Vec<Value>, String> {
        self.background_catalog()
            .await?
            .iter()
            .map(ToolDescriptor::value)
            .collect()
    }

    pub(crate) async fn background_catalog(&self) -> Result<Vec<ToolDescriptor>, String> {
        self.catalog_tools(false).await
    }

    async fn catalog_tools(&self, force_refresh: bool) -> Result<Vec<ToolDescriptor>, String> {
        if !self.connected().await {
            self.invalidate_mcp("Codex native process disconnected")
                .await;
            return Err("Codex native process disconnected".into());
        }
        let due = self
            .mcp
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .refreshed
            .is_none_or(|last| last.elapsed() >= MCP_BACKGROUND_REFRESH);
        if force_refresh || due {
            let _ = self.refresh_mcp().await;
        }
        if !self.connected().await {
            self.invalidate_mcp("Codex native process disconnected")
                .await;
            return Err("Codex native process disconnected".into());
        }
        let mut tools = self.tools.clone();
        tools.extend(
            self.mcp
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .catalog
                .tools
                .clone(),
        );
        Ok(tools)
    }

    /// MCP discovery diagnostics; App Server controls may remain available when
    /// this optional scope is unavailable or a tool has no native schema.
    pub async fn diagnostics(&self) -> Value {
        let connected = self.connected().await;
        if !connected {
            self.invalidate_mcp("Codex native process disconnected")
                .await;
        }
        let state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
        json!({
            "status":if connected && state.error.is_none() {"ready"} else {"unavailable"},
            "reason":state.error,
            "tools":state.catalog.tools.len(),
            "connected":connected,
            "refreshing":state.refreshing,
            "generation":state.generation,
            "scope":"MCP tools enumerated by Codex mcpServerStatus/list and forwarded through mcpServer/tool/call",
            "omitted":state.catalog.omitted
        })
    }

    async fn refresh_mcp(&self) -> Result<(), String> {
        // Concurrent listings share the existing snapshot instead of queuing
        // another potentially slow native network discovery.
        let Ok(_serial) = self.mcp_refresh.try_lock() else {
            return Ok(());
        };
        let (generation, previous_error, thread_id) = {
            let mut state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
            state.refreshing = true;
            (
                state.generation,
                state.error.clone(),
                state.thread_id.clone(),
            )
        };
        struct Refresh<'a> {
            state: &'a std::sync::Mutex<McpState>,
            generation: u64,
            finished: bool,
        }
        impl Drop for Refresh<'_> {
            fn drop(&mut self) {
                if self.finished {
                    return;
                }
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.refreshing = false;
                if state.generation == self.generation {
                    state.refreshed = Some(Instant::now());
                    state.catalog = McpCatalog::default();
                    state.error = Some("Codex MCP discovery was cancelled".into());
                    state.generation += 1;
                }
            }
        }
        let mut refresh = Refresh {
            state: &self.mcp,
            generation,
            finished: false,
        };
        let discovered = timeout(MCP_DISCOVERY_TIMEOUT, async {
            if !self.connected().await {
                return Err("Codex native process disconnected".to_owned());
            }
            if !self.mcp_supported {
                return Err("Installed Codex does not expose MCP discovery and direct calls in its native schema".to_owned());
            }
            if thread_id.is_none() {
                let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
                let response = self.peer.request_discovery(
                    "thread/start", json!({"cwd":cwd,"ephemeral":true})
                ).await?;
                let thread_id = response.pointer("/thread/id")
                    .and_then(Value::as_str).filter(|id| !id.is_empty())
                    .ok_or("Codex MCP thread response omitted thread.id")?.to_owned();
                let mut state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
                if state.generation != generation {
                    return Err("Codex MCP discovery was invalidated while starting its thread".into());
                }
                state.thread_id = Some(thread_id);
            }
            discover_mcp_tools(self).await
        }).await.map_err(|_| "Codex MCP discovery timed out".to_owned()).and_then(|value| value);
        let outcome = {
            let mut state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
            state.refreshing = false;
            if state.generation != generation {
                Err("Codex MCP discovery was invalidated before publication".to_owned())
            } else {
                state.refreshed = Some(Instant::now());
                state.generation += 1;
                match discovered {
                    Ok(mut catalog) => {
                        catalog.reuse_unchanged(&state.catalog);
                        state.catalog = catalog;
                        state.error = None;
                        Ok(())
                    }
                    Err(error) => {
                        state.catalog = McpCatalog::default();
                        state.error = Some(error.clone());
                        Err(error)
                    }
                }
            }
        };
        refresh.finished = true;
        if let Err(error) = &outcome
            && previous_error.as_deref() != Some(error.as_str())
        {
            self.peer.events().push(json!({
                "type":"harness.discovery.failed","provider":"codex",
                "scope":"mcp","error":error
            }));
        }
        outcome
    }

    async fn discovery_page(&self, params: Value) -> Result<Value, String> {
        let mut pending = self.mcp_page.lock().await;
        if let Some(previous) = pending.as_mut() {
            // A timeout/cancellation cannot cancel undocumented native server
            // work. Drain its eventual response before issuing any new list.
            if timeout(Duration::from_millis(25), previous).await.is_err() {
                return Err(
                    "A previous Codex MCP discovery page is still pending; no new request was sent"
                        .into(),
                );
            }
            pending.take();
        }
        let peer = self.peer.clone();
        *pending = Some(tokio::spawn(async move {
            peer.request_discovery("mcpServerStatus/list", params).await
        }));
        match timeout(MCP_PAGE_TIMEOUT, pending.as_mut().unwrap()).await {
            Ok(result) => {
                pending.take();
                result.map_err(|error| format!("Codex MCP discovery request stopped: {error}"))?
            }
            Err(_) => Err("Codex MCP discovery page timed out".into()),
        }
    }

    async fn native_call(&self, method: &str, params: Value) -> Result<Value, String> {
        let response = self.peer.request(method, params).await;
        if method == "mcpServer/tool/call" && response.is_err() {
            self.invalidate_mcp("Codex MCP native request failed; refresh the discovered catalog")
                .await;
        }
        let value = response?;
        if method == "mcpServer/tool/call" {
            // This is already a CallToolResult, including native isError,
            // structuredContent, images, metadata and future result fields.
            Ok(value)
        } else {
            Ok(result(value))
        }
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "ssmcp.codex.events" => Ok(result(self.peer.events().read(args).await?)),
            "ssmcp.codex.requests" => Ok(result(self.peer.pending().await)),
            "ssmcp.codex.respond" => Ok(result(self.peer.respond(args).await?)),
            "ssmcp.codex.schema" => {
                if let Some(name) = args.get("name") {
                    let name = name.as_str().ok_or("schema name must be a string")?;
                    Ok(result(self.schemas.read(name)?))
                } else {
                    Ok(result(
                        json!({"schemas":self.schemas.files.keys().collect::<Vec<_>>()}),
                    ))
                }
            }
            "codex.rpc" => {
                let method = args
                    .get("method")
                    .and_then(Value::as_str)
                    .filter(|method| !method.is_empty())
                    .ok_or("method is required")?;
                if method == "initialize" {
                    return Err("ssmcp already initialized this Codex connection".into());
                }
                self.native_call(
                    method,
                    args.get("params").cloned().unwrap_or_else(|| json!({})),
                )
                .await
            }
            _ if name.starts_with("codex.mcp.") => {
                if !self.connected().await {
                    self.invalidate_mcp("Codex native process disconnected")
                        .await;
                    return Err("Codex native process disconnected".into());
                }
                let (route, thread_id) = {
                    let state = self.mcp.lock().unwrap_or_else(|e| e.into_inner());
                    let route = state
                        .catalog
                        .routes
                        .get(name)
                        .ok_or("Codex MCP tool is not in the current discovered catalog")?
                        .clone();
                    let thread_id = state
                        .thread_id
                        .clone()
                        .ok_or("Codex MCP thread is unavailable")?;
                    (route, thread_id)
                };
                let response = self
                    .native_call(
                        "mcpServer/tool/call",
                        json!({
                            "server":route.server,"threadId":thread_id,
                            "tool":route.tool,"arguments":args
                        }),
                    )
                    .await;
                if !self.connected().await {
                    self.invalidate_mcp("Codex native process disconnected")
                        .await;
                }
                response
            }
            _ => {
                let (method, null_params) = self
                    .methods
                    .get(name)
                    .ok_or("unknown Codex tool; call tools/list or codex.rpc")?;
                if *null_params
                    && args
                        .as_object()
                        .is_none_or(|arguments| !arguments.is_empty())
                {
                    return Err("this native method takes no arguments".into());
                }
                self.native_call(method, if *null_params { Value::Null } else { args })
                    .await
            }
        }
    }

    pub async fn shutdown(&self) {
        self.invalidate_mcp("Codex native process disconnected")
            .await;
        self.peer.shutdown().await;
        let pending = self.mcp_page.lock().await.take();
        if let Some(task) = pending {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Codex {
    fn drop(&mut self) {
        if let Some(task) = self.mcp_page.get_mut().take() {
            task.abort();
        }
    }
}

async fn discover_mcp_tools(codex: &Codex) -> Result<McpCatalog, String> {
    let mut catalog = McpCatalog::default();
    let mut descriptors = BTreeMap::new();
    let mut cursor: Option<String> = None;
    let mut cursors = BTreeSet::new();
    let mut servers_seen = BTreeSet::new();
    for _ in 0..MAX_MCP_PAGES {
        let mut params = json!({"detail":"toolsAndAuthOnly","limit":64});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page = codex.discovery_page(params).await?;
        let servers = page
            .get("data")
            .and_then(Value::as_array)
            .ok_or("Codex MCP discovery returned no data array")?;
        for server in servers {
            if descriptors.len() + catalog.omitted.len() >= MAX_MCP_TOOLS {
                return Err("Codex MCP tool catalog exceeds 4096 entries".into());
            }
            let server_name = server
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or("Codex MCP server has no name")?;
            if !servers_seen.insert(server_name.to_owned()) {
                return Err("Codex MCP discovery repeated a server".into());
            }
            let Some(tools) = server.get("tools").and_then(Value::as_object) else {
                catalog.omitted.push(json!({"server":server_name,"reason":"Native server did not expose a tool catalog"}));
                continue;
            };
            for (key, spec) in tools {
                if descriptors.len() + catalog.omitted.len() >= MAX_MCP_TOOLS {
                    return Err("Codex MCP tool catalog exceeds 4096 entries".into());
                }
                let native_name = spec
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(key.as_str());
                if native_name.is_empty() || !spec.get("inputSchema").is_some_and(Value::is_object)
                {
                    catalog.omitted.push(json!({"server":server_name,"tool":native_name,"reason":"Native tool has no usable name or inputSchema; not enumerable"}));
                    continue;
                }
                let name = mcp_tool_name(server_name, native_name);
                let mut definition = spec.clone();
                // Keep the complete native Tool. Only its bridge routing name
                // changes; never synthesize schemas, annotations or approvals.
                definition["name"] = json!(name);
                descriptors.insert(name.clone(), ToolDescriptor::from_value(definition)?);
                if catalog
                    .routes
                    .insert(
                        name,
                        McpRoute {
                            server: server_name.to_owned(),
                            tool: native_name.to_owned(),
                        },
                    )
                    .is_some()
                {
                    return Err("Codex MCP routing name collision".into());
                }
            }
        }
        match page.get("nextCursor") {
            None | Some(Value::Null) => {
                catalog.tools = descriptors.into_values().collect();
                return Ok(catalog);
            }
            Some(Value::String(next)) if !next.is_empty() => {
                if !cursors.insert(next.clone()) {
                    return Err("Codex MCP discovery repeated a pagination cursor".into());
                }
                cursor = Some(next.clone());
            }
            _ => return Err("Codex MCP discovery returned an invalid pagination cursor".into()),
        }
    }
    Err("Codex MCP discovery exceeded its pagination limit".into())
}

fn mcp_tool_name(server: &str, tool: &str) -> String {
    fn component(value: &str) -> String {
        let mut encoded = String::new();
        for byte in value.bytes() {
            if byte.is_ascii_alphanumeric() || byte == b'-' {
                encoded.push(char::from(byte));
            } else {
                // '_' itself is escaped, as are '.' separators and UTF-8 bytes.
                write!(&mut encoded, "_{byte:02X}").unwrap();
            }
        }
        encoded
    }
    let name = format!("codex.mcp.{}.{}", component(server), component(tool));
    if name.len() <= 128 {
        return name;
    }
    let mut digest = Sha256::new();
    digest.update(b"ssmcp.codex.mcp\0");
    digest.update((server.len() as u64).to_be_bytes());
    digest.update(server.as_bytes());
    digest.update((tool.len() as u64).to_be_bytes());
    digest.update(tool.as_bytes());
    // '_h' cannot occur in an escaped component: an escape always starts with
    // two uppercase hex digits. Long names therefore have a disjoint namespace.
    format!("codex.mcp._h{:x}", digest.finalize())
}

fn catalog(schema: &Value) -> Result<(Vec<Value>, Methods), String> {
    let variants = schema["oneOf"]
        .as_array()
        .ok_or("Codex ClientRequest schema has no oneOf")?;
    let definitions = schema["definitions"]
        .as_object()
        .ok_or("Codex schema has no definitions")?;
    let mut tools = Vec::new();
    let mut methods = BTreeMap::new();
    for variant in variants {
        let properties = &variant["properties"];
        let method = properties["method"]["enum"][0]
            .as_str()
            .ok_or("Codex schema method enum is missing")?;
        if method == "initialize" {
            continue;
        }
        let name = format!("codex.{}", method.replace('/', "_"));
        if methods.contains_key(&name) {
            return Err("Codex tool name collision".into());
        }
        let mut input = properties
            .get("params")
            .cloned()
            .unwrap_or_else(|| json!({"type":"object","additionalProperties":false}));
        if let Some(reference) = input.get("$ref").and_then(Value::as_str) {
            let key = reference
                .strip_prefix("#/definitions/")
                .ok_or("unsupported Codex schema reference")?;
            input = definitions
                .get(key)
                .cloned()
                .ok_or("Codex parameter definition missing")?;
        }
        if input.is_null() {
            input = json!({"type":"object","additionalProperties":false});
        }
        if !input.is_object() {
            return Err(format!(
                "Codex method {method} has an unsupported parameter schema"
            ));
        }
        let null_params = input.get("type").is_some_and(|v| v == "null");
        methods.insert(name.clone(), (method.into(), null_params));
        if null_params {
            input = json!({"type":"object","additionalProperties":false});
        }
        input["type"] = json!("object");
        let mut included = Map::new();
        let mut needed = BTreeSet::new();
        refs(&input, &mut needed)?;
        while let Some(key) = needed.pop_first() {
            if included.contains_key(&key) {
                continue;
            }
            let definition = definitions
                .get(&key)
                .ok_or_else(|| format!("missing Codex definition {key}"))?;
            refs(definition, &mut needed)?;
            included.insert(key, definition.clone());
        }
        if !included.is_empty() {
            input["definitions"] = Value::Object(included);
        }
        let description = variant
            .get("description")
            .and_then(Value::as_str)
            .or_else(|| input.get("description").and_then(Value::as_str))
            .unwrap_or(
                "Forward this native Codex App Server method with its version-matched parameters.",
            );
        tools.push(json!({"name":name,"description":format!("Codex {method}. {description}"),"inputSchema":input}));
    }
    tools.extend([
        json!({"name":"codex.rpc","description":"Forward any native Codex App Server method. The connection is already initialized. Native sandbox, approval and feature policies apply. Agent turns execute all locally available Codex tools; tool progress and approvals appear in ssmcp.codex.events/requests.","inputSchema":{"type":"object","properties":{"method":{"type":"string"},"params":{"type":["object","null"]}},"required":["method"],"additionalProperties":false}}),
        json!({"name":"ssmcp.codex.events","description":"Read native Codex events, command output, tool execution and turn completion from a retained cursor. Read continuously while work runs.","inputSchema":super::events::schema()}),
        json!({"name":"ssmcp.codex.requests","description":"Read pending native approvals, user-input, elicitation and dynamic tool requests. No request is automatically approved or answered.","inputSchema":{"type":"object","additionalProperties":false}}),
        json!({"name":"ssmcp.codex.respond","description":"Answer an existing native server request, preserving its ID and required result shape. Use ssmcp.codex.schema to inspect generated response types. Supply exactly one of result or error.","inputSchema":{"type":"object","properties":{"id":{"type":["string","integer"]},"result":{},"error":{"type":"object","properties":{"code":{"type":"integer"},"message":{"type":"string"},"data":{}},"required":["code","message"]}},"required":["id"],"additionalProperties":false}}),
        json!({"name":"ssmcp.codex.schema","description":"Read version-matched generated Codex schemas, including approval responses and dynamic tool results. Omit name to list all schema filenames.","inputSchema":{"type":"object","properties":{"name":{"type":"string"}},"additionalProperties":false}}),
    ]);
    Ok((tools, methods))
}

fn refs(value: &Value, names: &mut BTreeSet<String>) -> Result<(), String> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "$ref" {
                    let reference = value.as_str().ok_or("invalid Codex reference")?;
                    let name = reference
                        .strip_prefix("#/definitions/")
                        .ok_or("external Codex references are not supported")?;
                    names.insert(name.into());
                } else {
                    refs(value, names)?;
                }
            }
        }
        Value::Array(array) => {
            for value in array {
                refs(value, names)?;
            }
        }
        _ => {}
    }
    Ok(())
}

struct SchemaDirectory(PathBuf);
impl SchemaDirectory {
    fn new() -> crate::Result<Self> {
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("ssmcp-codex-{suffix}"));
        crate::platform::private_directory(&path)?;
        Ok(Self(path))
    }
}
impl Drop for SchemaDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// Generated bundles contain many overlapping response schemas. Keep the private
// files for this provider's lifetime instead of retaining every parsed JSON tree.
struct SchemaStore {
    directory: SchemaDirectory,
    files: BTreeMap<String, u64>,
}

impl SchemaStore {
    fn read(&self, name: &str) -> Result<Value, String> {
        // Resolve only an indexed name: callers cannot traverse outside the
        // generated bundle or select an unvalidated file.
        let length = *self
            .files
            .get(name)
            .ok_or("unknown schema; omit name to list schemas")?;
        let path = self.directory.0.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() != length {
            return Err("generated Codex schema file changed; refresh the provider".into());
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)
            .map_err(|e| e.to_string())?
            .take(length + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() as u64 != length {
            return Err("generated Codex schema file changed; refresh the provider".into());
        }
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }
}

fn load_schemas(directory: SchemaDirectory) -> crate::Result<SchemaStore> {
    let root = &directory.0;
    let mut files = BTreeMap::new();
    let mut directories = vec![root.to_path_buf()];
    let mut total = 0;
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file() && entry.path().extension().is_some_and(|x| x == "json") {
                let length = entry.metadata()?.len();
                total += length;
                if total > 32 * 1024 * 1024 || files.len() >= 2048 {
                    return Err("Codex schema bundle exceeds limits".into());
                }
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                // Preserve startup validation without allocating response trees
                // that no caller has asked to read.
                let reader = BufReader::new(fs::File::open(entry.path())?.take(length + 1));
                serde_json::from_reader::<_, serde::de::IgnoredAny>(reader)?;
                files.insert(name, length);
            }
        }
    }
    Ok(SchemaStore { directory, files })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_refresh_reuses_only_unchanged_tool_storage() {
        fn fixture(entries: &[(&str, u64)]) -> McpCatalog {
            McpCatalog {
                tools: entries
                    .iter()
                    .map(|(name, version)| {
                        ToolDescriptor::from_value(
                            json!({"name":name,"inputSchema":{"type":"object"},"version":version}),
                        )
                        .unwrap()
                    })
                    .collect(),
                routes: entries
                    .iter()
                    .map(|(name, _)| {
                        (
                            (*name).into(),
                            McpRoute {
                                server: "native".into(),
                                tool: (*name).into(),
                            },
                        )
                    })
                    .collect(),
                omitted: Vec::new(),
            }
        }

        let previous = fixture(&[("a", 1), ("b", 1), ("c", 1), ("e", 1)]);
        let mut refreshed = fixture(&[("a", 1), ("c", 2), ("d", 1), ("e", 1)]);
        refreshed.routes.get_mut("a").unwrap().server = "refreshed".into();
        let changed = refreshed.tools[1].clone();
        let added = refreshed.tools[2].clone();
        assert!(!previous.tools[0].shares_storage(&refreshed.tools[0]));
        refreshed.reuse_unchanged(&previous);
        assert!(previous.tools[0].shares_storage(&refreshed.tools[0]));
        assert!(previous.tools[3].shares_storage(&refreshed.tools[3]));
        assert!(refreshed.tools[1].shares_storage(&changed));
        assert!(refreshed.tools[2].shares_storage(&added));
        assert_eq!(refreshed.routes["a"].server, "refreshed");
        assert!(!refreshed.routes.contains_key("b"));
    }

    #[test]
    fn generated_schemas_remain_readable_until_the_provider_drops() {
        let directory = SchemaDirectory::new().unwrap();
        let root = directory.0.clone();
        fs::create_dir(root.join("v2")).unwrap();
        let schema = json!({"type":"object","properties":{"value":{"type":"integer"}}});
        fs::write(root.join("v2/Response.json"), schema.to_string()).unwrap();

        let schemas = load_schemas(directory).unwrap();
        assert_eq!(
            schemas.files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["v2/Response.json"]
        );
        assert_eq!(schemas.read("v2/Response.json").unwrap(), schema);
        assert!(schemas.read("../Response.json").is_err());
        assert!(schemas.read("v2/Unknown.json").is_err());

        fs::write(root.join("v2/Response.json"), "{}").unwrap();
        assert!(
            schemas
                .read("v2/Response.json")
                .unwrap_err()
                .contains("changed")
        );
        drop(schemas);
        assert!(!root.exists());
    }

    #[test]
    fn generated_schema_index_rejects_invalid_and_oversized_files() {
        let invalid = SchemaDirectory::new().unwrap();
        let invalid_root = invalid.0.clone();
        fs::write(invalid_root.join("broken.json"), "{").unwrap();
        assert!(load_schemas(invalid).is_err());
        assert!(!invalid_root.exists());

        let oversized = SchemaDirectory::new().unwrap();
        let oversized_root = oversized.0.clone();
        fs::File::create(oversized_root.join("large.json"))
            .unwrap()
            .set_len(32 * 1024 * 1024 + 1)
            .unwrap();
        assert!(load_schemas(oversized).is_err());
        assert!(!oversized_root.exists());
    }

    #[test]
    fn mcp_routing_names_keep_components_distinct_and_are_bounded() {
        let pairs = [
            ("a.b", "c"),
            ("a", "b.c"),
            ("a/b", "c"),
            ("a_b", "c"),
            ("a_2Fb", "c"),
            ("a", "读取/文件"),
            ("a", "????"),
            ("a", "____"),
            ("_h", "literal"),
        ];
        let names: BTreeSet<_> = pairs
            .iter()
            .map(|(server, tool)| mcp_tool_name(server, tool))
            .collect();
        assert_eq!(names.len(), pairs.len());
        assert_eq!(
            mcp_tool_name("codegraph", "read"),
            "codex.mcp.codegraph.read"
        );
        assert_eq!(mcp_tool_name("a.b", "c/d"), "codex.mcp.a_2Eb.c_2Fd");
        let long = "读取/file_".repeat(100);
        let first = mcp_tool_name("server", &long);
        assert_eq!(first, mcp_tool_name("server", &long));
        assert!(first.starts_with("codex.mcp._h"));
        assert!(first.len() <= 128);
        assert_ne!(first, mcp_tool_name("server", &(long + "x")));
    }

    #[test]
    fn schemas_keep_recursive_refs_and_every_native_method() {
        let schema = json!({"oneOf":[
            {"properties":{"method":{"enum":["initialize"]},"params":{"$ref":"#/definitions/Params"}}},
            {"properties":{"method":{"enum":["thread/start"]},"params":{"$ref":"#/definitions/Params"}}},
            {"properties":{"method":{"enum":["future/method"]},"params":{"type":["object","null"]}}}
        ],"definitions":{"Params":{"type":"object","properties":{"node":{"$ref":"#/definitions/Node"}}},"Node":{"type":"object","properties":{"child":{"$ref":"#/definitions/Node"}}},"Unused":{"type":"string"}}});
        let (tools, methods) = catalog(&schema).unwrap();
        assert_eq!(methods.len(), 2);
        let input = &tools[0]["inputSchema"];
        assert!(input["definitions"].get("Node").is_some());
        assert!(input["definitions"].get("Unused").is_none());
        assert_eq!(methods["codex.future_method"].0, "future/method");
    }
}
