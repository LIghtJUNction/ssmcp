//! Version-matched Codex App Server: every generated client method, native events,
//! and server-initiated approvals/tool calls. No embedded copy of Codex's tool set.
use super::{
    result,
    rpc::{Dialect, Peer},
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{process::Command, time::timeout};

type Methods = BTreeMap<String, (String, bool)>;

pub struct Codex {
    peer: Arc<Peer>,
    tools: Vec<Value>,
    methods: Methods,
    schemas: BTreeMap<String, Value>,
    mcp_thread_id: Option<String>,
    mcp_tools: BTreeMap<String, (String, String)>,
}

impl Codex {
    pub async fn start(program: &OsStr, args: &[OsString]) -> crate::Result<Arc<Self>> {
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
        let schemas = load_schemas(&directory.0)?;
        let request_schema = schemas
            .get("ClientRequest.json")
            .ok_or("Codex generated no ClientRequest.json")?;
        let (tools, methods) = catalog(request_schema)?;
        let mut command = Command::new(program);
        command
            .args(["app-server", "--listen", "stdio://"])
            .args(args);
        let peer = Peer::spawn(command, "codex", Dialect::JsonRpc).await?;
        let initialized = async {
            peer.request("initialize",json!({
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
        let (mcp_thread_id, mcp_tools, dynamic_tools) = discover_mcp_tools(&peer).await;
        let mut tools = tools;
        let mut registered: BTreeSet<String> = tools
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
        for tool in dynamic_tools {
            if let Some(name) = tool["name"].as_str() {
                if registered.insert(name.to_owned()) {
                    tools.push(tool);
                }
            }
        }
        eprintln!(
            "[ssmcp harness] Codex MCP tools registered: {}",
            mcp_tools.len()
        );
        Ok(Arc::new(Self {
            peer,
            tools,
            methods,
            schemas,
            mcp_thread_id,
            mcp_tools,
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        Ok(self.tools.clone())
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "ssmcp.codex.events" => Ok(result(self.peer.events().read(args).await?)),
            "ssmcp.codex.requests" => Ok(result(self.peer.pending().await)),
            "ssmcp.codex.respond" => Ok(result(self.peer.respond(args).await?)),
            "ssmcp.codex.schema" => {
                if let Some(name) = args.get("name") {
                    let name = name.as_str().ok_or("schema name must be a string")?;
                    Ok(result(
                        self.schemas
                            .get(name)
                            .cloned()
                            .ok_or("unknown schema; omit name to list schemas")?,
                    ))
                } else {
                    Ok(result(
                        json!({"schemas":self.schemas.keys().collect::<Vec<_>>()}),
                    ))
                }
            }
            "codex.rpc" => {
                let method = args
                    .get("method")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .ok_or("method is required")?;
                if method == "initialize" {
                    return Err("ssmcp already initialized this Codex connection".into());
                }
                Ok(result(
                    self.peer
                        .request(
                            method,
                            args.get("params").cloned().unwrap_or_else(|| json!({})),
                        )
                        .await?,
                ))
            }
            _ => {
                if let Some((server, tool)) = self.mcp_tools.get(name) {
                    let thread_id = self
                        .mcp_thread_id
                        .as_deref()
                        .ok_or("Codex MCP thread is unavailable")?;
                    return Ok(result(self.peer.request("mcpServer/tool/call", json!({"server":server,"threadId":thread_id,"tool":tool,"arguments":args})).await?));
                }
                let (method, null_params) = self
                    .methods
                    .get(name)
                    .ok_or("unknown Codex tool; call tools/list or codex.rpc")?;
                if *null_params && args.as_object().is_none_or(|a| !a.is_empty()) {
                    return Err("this native method takes no arguments".into());
                }
                Ok(result(
                    self.peer
                        .request(method, if *null_params { Value::Null } else { args })
                        .await?,
                ))
            }
        }
    }

    pub async fn shutdown(&self) {
        self.peer.shutdown().await;
    }
}

async fn discover_mcp_tools(
    peer: &Arc<Peer>,
) -> (
    Option<String>,
    BTreeMap<String, (String, String)>,
    Vec<Value>,
) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let thread = match timeout(
        Duration::from_secs(15),
        peer.request("thread/start", json!({"cwd":cwd,"ephemeral":true})),
    )
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            eprintln!("[ssmcp harness] Codex MCP thread unavailable: {error}");
            return (None, BTreeMap::new(), Vec::new());
        }
        Err(_) => {
            eprintln!("[ssmcp harness] Codex MCP thread startup timed out");
            return (None, BTreeMap::new(), Vec::new());
        }
    };
    let Some(thread_id) = thread
        .pointer("/thread/id")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        eprintln!("[ssmcp harness] Codex MCP thread response omitted thread.id");
        return (None, BTreeMap::new(), Vec::new());
    };
    let mut cursor: Option<String> = None;
    let mut methods = BTreeMap::new();
    let mut definitions = Vec::new();
    for _ in 0..128 {
        let mut params = json!({"detail":"toolsAndAuthOnly","limit":1});
        if let Some(value) = &cursor {
            params["cursor"] = json!(value);
        }
        let page = match timeout(
            Duration::from_secs(5),
            peer.request("mcpServerStatus/list", params),
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                eprintln!("[ssmcp harness] Codex MCP discovery stopped: {error}");
                break;
            }
            Err(_) => {
                eprintln!("[ssmcp harness] Codex MCP discovery page timed out");
                break;
            }
        };
        if let Some(servers) = page.get("data").and_then(Value::as_array) {
            for server in servers {
                let Some(server_name) = server.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let Some(tools) = server.get("tools").and_then(Value::as_object) else {
                    continue;
                };
                for (tool_name, spec) in tools {
                    let Some(schema) = spec.get("inputSchema").filter(|v| v.is_object()) else {
                        continue;
                    };
                    let server_part = safe_tool_segment(server_name);
                    let tool_part = safe_tool_segment(tool_name);
                    let name = format!("codex.mcp.{server_part}.{tool_part}");
                    if name.len() > 128 || methods.contains_key(&name) {
                        continue;
                    }
                    let description = spec
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("Native MCP server tool exposed by Codex");
                    let mut definition = json!({"name":name,"description":description,"inputSchema":schema,"annotations":spec.get("annotations").cloned().unwrap_or_else(||json!({"readOnlyHint":false,"destructiveHint":false,"openWorldHint":true}))});
                    if let Some(meta) = spec.get("_meta") {
                        definition["_meta"] = meta.clone();
                    }
                    methods.insert(name, (server_name.to_owned(), tool_name.to_owned()));
                    definitions.push(definition);
                }
            }
        }
        let next = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if next.is_none() || next == cursor {
            break;
        }
        cursor = next;
    }
    (Some(thread_id), methods, definitions)
}

fn safe_tool_segment(value: &str) -> String {
    let value: String = value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"_.-".contains(&b) {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    if value.is_empty() {
        "unknown".into()
    } else {
        value
    }
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
        fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("ssmcp-codex-{suffix}"));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}
impl Drop for SchemaDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn load_schemas(root: &Path) -> crate::Result<BTreeMap<String, Value>> {
    let mut schemas = BTreeMap::new();
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
                if total > 32 * 1024 * 1024 || schemas.len() >= 2048 {
                    return Err("Codex schema bundle exceeds limits".into());
                }
                let name = entry
                    .path()
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .into_owned();
                schemas.insert(name, serde_json::from_slice(&fs::read(entry.path())?)?);
            }
        }
    }
    Ok(schemas)
}

#[cfg(test)]
mod tests {
    use super::*;
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
