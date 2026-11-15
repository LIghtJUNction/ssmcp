//! OpenCode v2's native HTTP API, discovered from the owned server's OpenAPI document.
use super::{events::EventLog, process::Managed, result};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, Response, Url};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    io::{self, Read},
    process::Stdio,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, BufReader},
    process::Command,
    sync::Mutex,
    task::JoinHandle,
    time::{sleep, timeout},
};

const SPEC_LIMIT: usize = 4 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 1024 * 1024;
const LINE_LIMIT: usize = 1024 * 1024;
const STREAM_LIMIT: usize = 32;

#[derive(Clone)]
struct Operation {
    id: String,
    method: Method,
    path: String,
    definition: Value,
    parameters: Vec<Value>,
    content_type: Option<String>,
}

pub struct OpenCode {
    client: Client,
    base: Url,
    password: String,
    process: Mutex<Option<Managed>>,
    document: Value,
    operations: BTreeMap<String, Operation>,
    catalog: Vec<Value>,
    events: Arc<EventLog>,
    readers: StdMutex<Vec<JoinHandle<()>>>,
    streams: StdMutex<BTreeMap<u64, JoinHandle<()>>>,
    next_stream: AtomicU64,
    closed: AtomicBool,
}

impl OpenCode {
    pub async fn start(program: &OsStr, args: &[OsString]) -> crate::Result<Arc<Self>> {
        // These flags define the private transport, not user-facing agent configuration.
        for arg in args {
            if matches!(
                arg.to_str().and_then(|s| s.split('=').next()),
                Some("--hostname" | "--port" | "--service" | "--stdio")
            ) {
                return Err("ssmcp manages OpenCode's --hostname, --port and --stdio".into());
            }
        }
        let mut random = [0_u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let password = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        let events = EventLog::new("opencode");
        let mut command = Command::new(program);
        command
            .arg("serve")
            .args(args)
            .args(["--stdio", "--hostname", "127.0.0.1", "--port", "0"])
            .env("OPENCODE_PASSWORD", &password)
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .env("PWD", std::env::current_dir()?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process = Managed::spawn(&mut command)
            .map_err(|e| format!("could not start OpenCode v2: {e}"))?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or("OpenCode stdout unavailable")?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or("OpenCode stderr unavailable")?;
        let stderr_task = log_lines(
            BufReader::new(stderr),
            events.clone(),
            password.clone(),
            "stderr",
        );
        let mut stdout = BufReader::new(stdout);
        let ready = timeout(Duration::from_secs(30), bounded_line(&mut stdout))
            .await
            .map_err(|_| "OpenCode startup timed out")??
            .ok_or("OpenCode exited before its ready message; OpenCode v2 is required")?;
        let ready: Value = serde_json::from_slice(&ready)
            .map_err(|_| "OpenCode did not emit the v2 serve --stdio ready message")?;
        let base = private_url(
            ready
                .get("url")
                .and_then(Value::as_str)
                .ok_or("OpenCode ready message has no URL")?,
        )?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        let response = timeout(
            Duration::from_secs(15),
            client
                .get(base.join("/openapi.json")?)
                .basic_auth("opencode", Some(&password))
                .send(),
        )
        .await
        .map_err(|_| "OpenCode schema request timed out")??;
        if !response.status().is_success() {
            return Err(format!("OpenCode /openapi.json returned {}", response.status()).into());
        }
        let (bytes, truncated) = timeout(Duration::from_secs(15), read_bytes(response, SPEC_LIMIT))
            .await
            .map_err(|_| "OpenCode schema read timed out")??;
        if truncated {
            return Err("OpenCode schema exceeds 4 MiB".into());
        }
        let document: Value = serde_json::from_slice(&bytes)?;
        let (operations, catalog) = catalog(&document)?;
        let stdout_task = log_lines(stdout, events.clone(), password.clone(), "stdout");
        let backend = Arc::new(Self {
            client,
            base,
            password,
            process: Mutex::new(Some(process)),
            document,
            operations,
            catalog,
            events,
            readers: StdMutex::new(vec![stderr_task, stdout_task]),
            streams: StdMutex::new(BTreeMap::new()),
            next_stream: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        // Establish the event subscription before publishing the remote MCP endpoint.
        let response = backend
            .send(Method::GET, "/api/event", &json!({}), None)
            .await?;
        if !response.status().is_success() {
            return Err(
                format!("OpenCode event subscription returned {}", response.status()).into(),
            );
        }
        backend.start_stream(0, response, "/api/event".into(), true)?;
        backend
            .events
            .push(json!({"type":"ssmcp.ready","backend":"opencode","tools":backend.catalog.len()}));
        Ok(backend)
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = self.catalog.clone();
        tools.extend(extra_tools());
        Ok(tools)
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        let outcome = self.call_inner(name, arguments).await;
        if let Err(error) = &outcome {
            self.events.push(redact(
                json!({"type":"ssmcp.error","tool":name,"error":error}),
                &self.password,
            ));
        }
        outcome
    }

    async fn call_inner(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("OpenCode has stopped".into());
        }
        let arguments = if arguments.is_null() {
            json!({})
        } else {
            arguments
        };
        if !arguments.is_object() {
            return Err("arguments must be an object".into());
        }
        match name {
            "opencode.events" => return self.events.read(arguments).await.map(result),
            "opencode.schema" => return self.schema(&arguments).map(result),
            "opencode.stream_close" => {
                let id = arguments
                    .get("stream_id")
                    .and_then(Value::as_u64)
                    .filter(|id| *id > 0)
                    .ok_or(
                        "stream_id must be a positive integer; stream 0 is the local live log",
                    )?;
                let removed = self.streams.lock().unwrap().remove(&id);
                let existed = removed.is_some();
                if let Some(task) = removed {
                    task.abort();
                }
                return Ok(result(json!({"stream_id":id,"closed":existed})));
            }
            _ => {}
        }
        let operation = if name == "opencode.api" {
            arguments
                .get("operation_id")
                .and_then(Value::as_str)
                .map(|id| self.find_operation(id))
                .transpose()?
        } else {
            Some(self.operations.get(name).ok_or("unknown OpenCode tool")?)
        };
        let (method, path) = if let Some(operation) = operation {
            (
                operation.method.clone(),
                render_path(&operation.path, arguments.get("path"))?,
            )
        } else {
            let method = arguments
                .get("method")
                .and_then(Value::as_str)
                .ok_or("method or operation_id is required")?;
            let method = http_method(method)?;
            let path = arguments
                .get("url_path")
                .and_then(Value::as_str)
                .ok_or("url_path is required with method")?;
            (method, path.to_owned())
        };
        let max_bytes = integer_option(
            &arguments,
            "response_max_bytes",
            256 * 1024,
            1,
            RESPONSE_LIMIT as u64,
        )? as usize;
        self.events.push(redact(json!({"type":"ssmcp.request","tool":name,"method":method.as_str(),"path":path,"arguments":arguments}), &self.password));
        let response = self.send(method, &path, &arguments, operation).await?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let payload = if content_type.split(';').next() == Some("text/event-stream") && status < 400
        {
            let id = self.next_stream.fetch_add(1, Ordering::Relaxed);
            self.start_stream(id, response, path.clone(), false)?;
            json!({"status":status,"stream_id":id,"events_tool":"opencode.events","close_tool":"opencode.stream_close"})
        } else {
            let headers: BTreeMap<String, String> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_owned())))
                .collect();
            let (bytes, truncated) =
                timeout(Duration::from_secs(60), read_bytes(response, max_bytes))
                    .await
                    .map_err(|_| "OpenCode response body timed out")??;
            let mut payload = json!({"status":status,"content_type":content_type,"headers":headers,"truncated":truncated});
            if bytes.is_empty() {
                payload["body"] = Value::Null;
            } else if !truncated && (content_type.contains("json")) {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(value) => payload["body"] = value,
                    Err(_) => {
                        payload["body_text"] = String::from_utf8_lossy(&bytes).into_owned().into()
                    }
                }
            } else if content_type.starts_with("text/") || content_type.contains("json") {
                payload["body_text"] = String::from_utf8_lossy(&bytes).into_owned().into();
            } else {
                payload["body_base64"] = STANDARD.encode(&bytes).into();
            }
            payload
        };
        let payload = redact(payload, &self.password);
        self.events
            .push(json!({"type":"ssmcp.response","tool":name,"path":path,"result":payload}));
        let mut output = result(payload);
        if status >= 400 {
            output["isError"] = true.into();
        }
        if output.to_string().len() > crate::wire::MAX_FRAME.saturating_sub(64 * 1024) {
            return Err("encoded OpenCode response exceeds the transport limit; retry with a smaller response_max_bytes or a Range header".into());
        }
        Ok(output)
    }

    fn find_operation(&self, id: &str) -> Result<&Operation, String> {
        self.operations
            .get(id)
            .or_else(|| self.operations.values().find(|op| op.id == id))
            .ok_or_else(|| format!("unknown OpenCode operation: {id}; use opencode.schema"))
    }

    fn schema(&self, arguments: &Value) -> Result<Value, String> {
        if let Some(id) = arguments.get("operation_id").and_then(Value::as_str) {
            let op = self.find_operation(id)?;
            let defs = referenced_schemas(&op.definition, &self.document)?;
            return Ok(
                json!({"operation_id":op.id,"method":op.method.as_str(),"path":op.path,
                "operation":op.definition,"components":{"schemas":defs}}),
            );
        }
        Ok(
            json!({"operations":self.operations.iter().map(|(name,op)| json!({
            "tool":name,"operation_id":op.id,"method":op.method.as_str(),"path":op.path,
            "summary":op.definition.get("summary")
        })).collect::<Vec<_>>()}),
        )
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        args: &Value,
        operation: Option<&Operation>,
    ) -> Result<Response, String> {
        let mut url = request_url(&self.base, path)?;
        if let Some(query) = args.get("query") {
            let query = query.as_object().ok_or("query must be an object")?;
            let pairs = query_pairs(
                query,
                operation.map(|o| o.parameters.as_slice()).unwrap_or(&[]),
            )?;
            url.query_pairs_mut().extend_pairs(pairs);
        }
        let mut request = self
            .client
            .request(method, url)
            .basic_auth("opencode", Some(&self.password));
        if let Some(headers) = args.get("headers") {
            let headers = headers.as_object().ok_or("headers must be an object")?;
            if headers.len() > 64 {
                return Err("too many request headers".into());
            }
            for (name, value) in headers {
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "authorization"
                        | "proxy-authorization"
                        | "host"
                        | "connection"
                        | "content-length"
                        | "transfer-encoding"
                ) {
                    return Err(format!("ssmcp manages the {name} header"));
                }
                request =
                    request.header(name, value.as_str().ok_or("header values must be strings")?);
            }
        }
        if args.get("body").is_some() && args.get("body_base64").is_some() {
            return Err("use either body or body_base64".into());
        }
        if let Some(body) = args.get("body_base64") {
            let bytes = STANDARD
                .decode(body.as_str().ok_or("body_base64 must be a string")?)
                .map_err(|_| "invalid body_base64")?;
            if bytes.len() > RESPONSE_LIMIT {
                return Err("request body exceeds 1 MiB".into());
            }
            let content_type = args
                .get("content_type")
                .and_then(Value::as_str)
                .or_else(|| operation.and_then(|o| o.content_type.as_deref()))
                .unwrap_or("application/octet-stream");
            request = request
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(bytes);
        } else if let Some(body) = args.get("body") {
            let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
            if bytes.len() > RESPONSE_LIMIT {
                return Err("request body exceeds 1 MiB".into());
            }
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(bytes);
        }
        let wait = integer_option(args, "timeout_ms", 30_000, 1, 120_000)?;
        timeout(Duration::from_millis(wait), request.send())
            .await
            .map_err(|_| {
                "OpenCode request timed out; a mutating request may still have run".to_owned()
            })?
            .map_err(|e| e.to_string())
    }

    fn start_stream(
        &self,
        id: u64,
        response: Response,
        path: String,
        reconnect: bool,
    ) -> Result<(), String> {
        let mut streams = self.streams.lock().unwrap();
        streams.retain(|_, task| !task.is_finished());
        if streams.len() >= STREAM_LIMIT {
            return Err("too many OpenCode streams; close an existing stream".into());
        }
        let events = self.events.clone();
        let client = self.client.clone();
        let base = self.base.clone();
        let password = self.password.clone();
        streams.insert(id, tokio::spawn(async move {
            let mut response = response;
            loop {
                let outcome = consume_sse(response, &events, id, &path, &password).await;
                if !reconnect {
                    events.push(json!({"type":"ssmcp.stream_closed","stream_id":id,"error":outcome.err()}));
                    return;
                }
                events.push(json!({"type":"ssmcp.stream_gap","stream_id":id,"error":outcome.err(),
                    "message":"OpenCode live events disconnected; events during the gap cannot be replayed"}));
                loop {
                    sleep(Duration::from_secs(1)).await;
                    let request = client.get(base.join("/api/event").unwrap()).basic_auth("opencode", Some(&password)).send();
                    if let Ok(Ok(next)) = timeout(Duration::from_secs(15), request).await
                        && next.status().is_success() {
                        response = next; break;
                    }
                }
            }
        }));
        Ok(())
    }

    pub async fn shutdown(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        for (_, task) in std::mem::take(&mut *self.streams.lock().unwrap()) {
            task.abort();
        }
        if let Some(mut process) = self.process.lock().await.take() {
            drop(process.child.stdin.take());
            process.shutdown().await;
        }
        for task in self.readers.lock().unwrap().drain(..) {
            task.abort();
        }
        self.events.close();
    }
}

impl Drop for OpenCode {
    fn drop(&mut self) {
        for (_, task) in std::mem::take(self.streams.get_mut().unwrap()) {
            task.abort();
        }
        for task in self.readers.get_mut().unwrap().drain(..) {
            task.abort();
        }
        self.events.close();
    }
}

fn private_url(text: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|_| "invalid OpenCode ready URL")?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|p| p == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("OpenCode must listen on an explicit HTTP port at 127.0.0.1".into());
    }
    Ok(url)
}

fn request_url(base: &Url, path: &str) -> Result<Url, String> {
    if !path.starts_with('/') || path.starts_with("//") || path.contains(['?', '#', '\\']) {
        return Err(
            "url_path must be a local absolute path without query or fragment; use query".into(),
        );
    }
    let url = base.join(path).map_err(|e| e.to_string())?;
    if url.origin() != base.origin() {
        return Err("OpenCode requests must stay on the owned server".into());
    }
    Ok(url)
}

fn http_method(text: &str) -> Result<Method, String> {
    match text.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::GET),
        "POST" => Ok(Method::POST),
        "PUT" => Ok(Method::PUT),
        "PATCH" => Ok(Method::PATCH),
        "DELETE" => Ok(Method::DELETE),
        "HEAD" => Ok(Method::HEAD),
        "OPTIONS" => Ok(Method::OPTIONS),
        _ => Err("unsupported HTTP method".into()),
    }
}

fn integer_option(
    args: &Value,
    key: &str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, String> {
    match args.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|n| *n >= min && *n <= max)
            .ok_or_else(|| format!("{key} must be an integer in {min}..={max}")),
    }
}

fn dereference<'a>(value: &'a Value, document: &'a Value) -> Result<&'a Value, String> {
    let mut current = value;
    for _ in 0..32 {
        let Some(reference) = current.get("$ref").and_then(Value::as_str) else {
            return Ok(current);
        };
        let pointer = reference
            .strip_prefix('#')
            .ok_or("external OpenAPI references are unsupported")?;
        current = document
            .pointer(pointer)
            .ok_or_else(|| format!("unresolved OpenAPI reference {reference}"))?;
    }
    Err("cyclic OpenAPI parameter reference".into())
}

fn catalog(document: &Value) -> Result<(BTreeMap<String, Operation>, Vec<Value>), String> {
    let paths = document
        .get("paths")
        .and_then(Value::as_object)
        .ok_or("OpenAPI document has no paths")?;
    let mut operations = BTreeMap::new();
    let mut tools = Vec::new();
    for (path, item) in paths {
        let item = dereference(item, document)?;
        let Some(methods) = item.as_object() else {
            continue;
        };
        for (method, definition) in methods {
            let Ok(method) = http_method(method) else {
                continue;
            };
            let Some(id) = definition.get("operationId").and_then(Value::as_str) else {
                continue;
            };
            let name = format!("opencode.{id}");
            if name.len() > 128
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
            {
                return Err(format!(
                    "OpenAPI operation ID is not a valid MCP tool name: {id}"
                ));
            }
            if operations.contains_key(&name) {
                return Err(format!("duplicate OpenAPI operation: {id}"));
            }
            let mut parameters = BTreeMap::new();
            for parameter in item
                .get("parameters")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    definition
                        .get("parameters")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            {
                let parameter = dereference(parameter, document)?.clone();
                let key = (
                    parameter
                        .get("in")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    parameter
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                );
                parameters.insert(key, parameter);
            }
            let parameters: Vec<_> = parameters.into_values().collect();
            let mut properties = Map::new();
            let mut required = Vec::new();
            for (location, group) in [("path", "path"), ("query", "query"), ("header", "headers")] {
                let mut fields = Map::new();
                let mut mandatory = Vec::new();
                for parameter in &parameters {
                    if parameter.get("in").and_then(Value::as_str) != Some(location) {
                        continue;
                    }
                    let Some(key) = parameter.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    fields.insert(
                        key.into(),
                        parameter
                            .get("schema")
                            .cloned()
                            .unwrap_or(json!({"type":"string"})),
                    );
                    if parameter.get("required").and_then(Value::as_bool) == Some(true) {
                        mandatory.push(key.to_owned());
                    }
                }
                if location == "path" && path.contains('*') {
                    fields.insert("wildcard".into(), json!({"type":"string","description":"Path matched by *, with / separating segments. Dot traversal segments are rejected."}));
                    mandatory.push("wildcard".into());
                }
                if !fields.is_empty() {
                    if !mandatory.is_empty() {
                        required.push(group.to_owned());
                    }
                    properties.insert(group.into(), json!({"type":"object","properties":fields,"required":mandatory,"additionalProperties":false}));
                }
            }
            let mut content_type = None;
            if let Some(body) = definition.get("requestBody") {
                let body = dereference(body, document)?;
                if let Some(content) = body.get("content").and_then(Value::as_object) {
                    let entry = content
                        .get_key_value("application/json")
                        .or_else(|| content.iter().next());
                    if let Some((kind, media)) = entry {
                        content_type = Some(kind.to_owned());
                        let key = if kind.contains("json") {
                            "body"
                        } else {
                            "body_base64"
                        };
                        let schema = if key == "body" {
                            media.get("schema").cloned().unwrap_or(json!({}))
                        } else {
                            json!({"type":"string","contentEncoding":"base64","description":"Raw request body encoded as base64"})
                        };
                        properties.insert(key.into(), schema);
                        if body.get("required").and_then(Value::as_bool) == Some(true) {
                            required.push(key.into());
                        }
                    }
                }
            }
            properties.insert(
                "response_max_bytes".into(),
                json!({"type":"integer","minimum":1,"maximum":RESPONSE_LIMIT,"default":262144}),
            );
            properties.insert(
                "timeout_ms".into(),
                json!({"type":"integer","minimum":1,"maximum":120000,"default":30000}),
            );
            let mut schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
            let mut defs = Value::Object(referenced_schemas(&schema, document)?);
            rewrite_refs(&mut schema)?;
            rewrite_refs(&mut defs)?;
            if !defs.as_object().unwrap().is_empty() {
                schema["$defs"] = defs;
            }
            let description = definition
                .get("description")
                .or_else(|| definition.get("summary"))
                .and_then(Value::as_str)
                .unwrap_or(id);
            tools.push(json!({"name":name,"description":format!("{} {}. {}", method.as_str(), path, description),"inputSchema":schema}));
            operations.insert(
                name,
                Operation {
                    id: id.into(),
                    method,
                    path: path.into(),
                    definition: definition.clone(),
                    parameters,
                    content_type,
                },
            );
        }
    }
    if operations.is_empty() {
        return Err("OpenCode OpenAPI document contains no named operations".into());
    }
    Ok((operations, tools))
}

fn collect_refs(value: &Value, output: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "$ref" {
                    if let Some(reference) = value.as_str() {
                        output.insert(reference.to_owned());
                    }
                } else {
                    collect_refs(value, output);
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                collect_refs(value, output);
            }
        }
        _ => {}
    }
}

fn referenced_schemas(value: &Value, document: &Value) -> Result<Map<String, Value>, String> {
    let mut pending = BTreeSet::new();
    collect_refs(value, &mut pending);
    let mut seen = BTreeSet::new();
    let mut defs = Map::new();
    while let Some(reference) = pending.pop_first() {
        if !seen.insert(reference.clone()) {
            continue;
        }
        let key = reference
            .strip_prefix("#/components/schemas/")
            .ok_or_else(|| format!("unsupported OpenAPI schema reference: {reference}"))?;
        let schema = document
            .pointer(&reference[1..])
            .ok_or_else(|| format!("unresolved schema: {reference}"))?;
        collect_refs(schema, &mut pending);
        defs.insert(key.replace("~1", "/").replace("~0", "~"), schema.clone());
    }
    Ok(defs)
}

fn rewrite_refs(value: &mut Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "$ref" {
                    let reference = value.as_str().ok_or("schema $ref must be a string")?;
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .ok_or("unsupported schema reference")?;
                    *value = format!("#/$defs/{name}").into();
                } else {
                    rewrite_refs(value)?;
                }
            }
        }
        Value::Array(items) => {
            for value in items {
                rewrite_refs(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn encode_segment(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn render_path(template: &str, arguments: Option<&Value>) -> Result<String, String> {
    let empty = Map::new();
    let args = match arguments {
        Some(value) => value.as_object().ok_or("path must be an object")?,
        None => &empty,
    };
    let mut rendered = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        rendered.push_str(&rest[..start]);
        let end = rest[start..].find('}').ok_or("invalid OpenAPI path")? + start;
        let name = &rest[start + 1..end];
        let value = args
            .get(name)
            .ok_or_else(|| format!("missing path.{name}"))?;
        let value = scalar(value)?;
        if matches!(value.as_str(), "." | "..") {
            return Err("dot path segments are not allowed".into());
        }
        rendered.push_str(&encode_segment(&value));
        rest = &rest[end + 1..];
    }
    rendered.push_str(rest);
    if rendered.contains('*') {
        let wildcard = args
            .get("wildcard")
            .and_then(Value::as_str)
            .ok_or("path.wildcard is required")?;
        let mut segments = Vec::new();
        for segment in wildcard.split('/') {
            if matches!(segment, "." | "..") {
                return Err("dot wildcard segments are not allowed".into());
            }
            segments.push(encode_segment(segment));
        }
        rendered = rendered.replace('*', &segments.join("/"));
    }
    Ok(rendered)
}

fn scalar(value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(_) | Value::Number(_) | Value::Null => Ok(value.to_string()),
        _ => Err("expected a scalar parameter".into()),
    }
}

fn query_pairs(
    query: &Map<String, Value>,
    parameters: &[Value],
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (name, value) in query {
        let parameter = parameters.iter().find(|p| {
            p.get("in").and_then(Value::as_str) == Some("query")
                && p.get("name").and_then(Value::as_str) == Some(name)
        });
        let style = parameter
            .and_then(|p| p.get("style"))
            .and_then(Value::as_str)
            .unwrap_or(if parameter.is_none() && value.is_object() {
                "deepObject"
            } else {
                "form"
            });
        let explode = parameter
            .and_then(|p| p.get("explode"))
            .and_then(Value::as_bool)
            .unwrap_or(style == "form");
        match value {
            Value::Object(map) if style == "deepObject" => {
                for (key, item) in map {
                    deep_query(&format!("{name}[{key}]"), item, &mut out)?;
                }
            }
            Value::Array(items) => {
                let values = items.iter().map(scalar).collect::<Result<Vec<_>, _>>()?;
                if explode {
                    for item in values {
                        out.push((name.clone(), item));
                    }
                } else {
                    out.push((
                        name.clone(),
                        values.join(match style {
                            "spaceDelimited" => " ",
                            "pipeDelimited" => "|",
                            _ => ",",
                        }),
                    ));
                }
            }
            Value::Object(map) => {
                if explode {
                    for (key, item) in map {
                        out.push((key.clone(), scalar(item)?));
                    }
                } else {
                    let mut values = Vec::new();
                    for (key, item) in map {
                        values.extend([key.clone(), scalar(item)?]);
                    }
                    out.push((name.clone(), values.join(",")));
                }
            }
            _ => out.push((name.clone(), scalar(value)?)),
        }
    }
    Ok(out)
}

fn deep_query(name: &str, value: &Value, out: &mut Vec<(String, String)>) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                deep_query(&format!("{name}[{key}]"), value, out)?;
            }
        }
        Value::Array(items) => {
            for (i, value) in items.iter().enumerate() {
                deep_query(&format!("{name}[{i}]"), value, out)?;
            }
        }
        _ => out.push((name.to_owned(), scalar(value)?)),
    }
    Ok(())
}

async fn read_bytes(mut response: Response, limit: usize) -> Result<(Vec<u8>, bool), String> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok((bytes, true));
        }
    }
    Ok((bytes, false))
}

async fn bounded_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let data = reader.fill_buf().await?;
        if data.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let count = data
            .iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |n| n + 1);
        if line.len() + count > LINE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "OpenCode output line exceeds 1 MiB",
            ));
        }
        let done = data[count - 1] == b'\n';
        line.extend_from_slice(&data[..count]);
        reader.consume(count);
        if done {
            return Ok(Some(line));
        }
    }
}

fn log_lines<R: AsyncBufRead + Unpin + Send + 'static>(
    mut reader: R,
    events: Arc<EventLog>,
    password: String,
    stream: &'static str,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match bounded_line(&mut reader).await {
                Ok(Some(line)) => events.push(redact(json!({"type":"ssmcp.process_output","stream":stream,"text":String::from_utf8_lossy(&line)}),&password)),
                Ok(None) => break,
                Err(error) => { events.push(json!({"type":"ssmcp.process_output_error","stream":stream,"error":error.to_string()})); break; }
            }
        }
    })
}

#[derive(Default)]
struct Sse {
    line: Vec<u8>,
    data: Vec<u8>,
}

impl Sse {
    fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(Value)) -> Result<(), String> {
        for &byte in chunk {
            if byte != b'\n' {
                if self.line.len() + self.data.len() >= LINE_LIMIT {
                    return Err("OpenCode SSE frame exceeds 1 MiB".into());
                }
                self.line.push(byte);
                continue;
            }
            if self.line.last() == Some(&b'\r') {
                self.line.pop();
            }
            if self.line.is_empty() {
                if !self.data.is_empty() {
                    self.data.pop(); // Strip the final newline added for a data field.
                    let value = serde_json::from_slice(&self.data).unwrap_or_else(
                        |_| json!({"data_text":String::from_utf8_lossy(&self.data)}),
                    );
                    emit(value);
                    self.data.clear();
                }
            } else if let Some(data) = self.line.strip_prefix(b"data:") {
                self.data
                    .extend_from_slice(data.strip_prefix(b" ").unwrap_or(data));
                self.data.push(b'\n');
            }
            self.line.clear();
        }
        Ok(())
    }
}

async fn consume_sse(
    mut response: Response,
    events: &EventLog,
    id: u64,
    path: &str,
    password: &str,
) -> Result<(), String> {
    let mut parser = Sse::default();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        parser.push(&chunk, |event| {
            events.push(redact(
                json!({"type":"opencode.event","stream_id":id,"path":path,"event":event}),
                password,
            ))
        })?;
    }
    Ok(())
}

fn redact(value: Value, password: &str) -> Value {
    // Internal HTTP credentials never enter terminal logs or MCP results, even if a child echoes them.
    let basic = STANDARD.encode(format!("opencode:{password}"));
    fn visit(value: Value, password: &str, basic: &str) -> Value {
        match value {
            Value::String(s) => s
                .replace(password, "[redacted]")
                .replace(basic, "[redacted]")
                .into(),
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|v| visit(v, password, basic))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, visit(v, password, basic)))
                    .collect(),
            ),
            value => value,
        }
    }
    visit(value, password, &basic)
}

fn extra_tools() -> Vec<Value> {
    vec![
        json!({"name":"opencode.api","description":"Call any native OpenCode HTTP operation by operation_id, or method plus url_path on the private server. Use opencode.schema to discover input/output schemas. JSON uses body; raw bytes use body_base64. SSE starts a stream read with opencode.events. The native permission policy remains in effect.","inputSchema":{"type":"object","properties":{
            "operation_id":{"type":"string"},"method":{"type":"string","enum":["GET","POST","PUT","PATCH","DELETE","HEAD","OPTIONS"]},"url_path":{"type":"string"},"path":{"type":"object"},"query":{"type":"object"},"headers":{"type":"object","additionalProperties":{"type":"string"}},"body":{},"body_base64":{"type":"string"},"content_type":{"type":"string"},"response_max_bytes":{"type":"integer","minimum":1,"maximum":RESPONSE_LIMIT},"timeout_ms":{"type":"integer","minimum":1,"maximum":120000}},"additionalProperties":false}}),
        json!({"name":"opencode.schema","description":"List installed OpenCode operations, or return a native operation and its referenced schemas by operation_id. This discovers the installed API instead of assuming a fixed OpenCode version.","inputSchema":{"type":"object","properties":{"operation_id":{"type":"string"}},"additionalProperties":false}}),
        json!({"name":"opencode.events","description":"Read bounded cursor-addressed OpenCode live events, tool calls, results, permission requests and adapter logs. Also mirrored live in the local terminal. If truncated=true, advance to start_cursor; upstream stream_gap events mean native events were missed.","inputSchema":{"type":"object","properties":{"cursor":{"type":"integer","minimum":0},"max_events":{"type":"integer","minimum":1,"maximum":256,"default":50},"wait_ms":{"type":"integer","minimum":0,"maximum":5000}},"additionalProperties":false}}),
        json!({"name":"opencode.stream_close","description":"Close an additional SSE stream opened by a native operation. Stream 0 is the always-on local live log.","inputSchema":{"type":"object","properties":{"stream_id":{"type":"integer","minimum":1}},"required":["stream_id"],"additionalProperties":false}}),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_refs_are_transitive_and_exclude_unrelated_components() {
        let document = json!({"paths":{"/api/thing/{id}":{"post":{"operationId":"thing.create","parameters":[{"name":"id","in":"path","required":true,"schema":{"type":"string"}}],"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/A"}}}}}}},"components":{"schemas":{"A":{"type":"object","properties":{"child":{"$ref":"#/components/schemas/B"}}},"B":{"type":"string"},"Unused":{"type":"number"}}}});
        let (_, tools) = catalog(&document).unwrap();
        let input = &tools[0]["inputSchema"];
        assert_eq!(input["properties"]["body"]["$ref"], "#/$defs/A");
        assert_eq!(
            input["$defs"]["A"]["properties"]["child"]["$ref"],
            "#/$defs/B"
        );
        assert_eq!(input["$defs"].as_object().unwrap().len(), 2);
        assert_eq!(input["required"], json!(["path", "body"]));
    }

    #[test]
    fn query_serialization_honors_deep_objects_and_repeated_arrays() {
        let query = json!({"location":{"directory":"/tmp/a b"},"types":["session.tool.called","permission.asked"]});
        let pairs = query_pairs(query.as_object().unwrap(), &[]).unwrap();
        assert_eq!(
            pairs,
            vec![
                ("location[directory]".into(), "/tmp/a b".into()),
                ("types".into(), "session.tool.called".into()),
                ("types".into(), "permission.asked".into())
            ]
        );
        let pairs = query_pairs(
            json!({"tags":["a","b"]}).as_object().unwrap(),
            &[json!({"in":"query","name":"tags","style":"form","explode":false})],
        )
        .unwrap();
        assert_eq!(pairs, vec![("tags".into(), "a,b".into())]);
    }

    #[test]
    fn paths_cannot_change_the_private_http_origin() {
        let base = private_url("http://127.0.0.1:12345").unwrap();
        for path in [
            "https://evil.example/",
            "//evil.example/",
            "/\\evil.example/",
            "/api/x?x=1",
        ] {
            assert!(request_url(&base, path).is_err());
        }
        assert!(private_url("http://0.0.0.0:12345").is_err());
        assert!(private_url("http://127.0.0.1:12345@evil.example/").is_err());
        assert_eq!(
            render_path("/api/session/{id}", Some(&json!({"id":"a/b ?"}))).unwrap(),
            "/api/session/a%2Fb%20%3F"
        );
        assert_eq!(
            render_path("/api/fs/read/*", Some(&json!({"wildcard":"a b/c.txt"}))).unwrap(),
            "/api/fs/read/a%20b/c.txt"
        );
        assert!(render_path("/api/fs/read/*", Some(&json!({"wildcard":"../secret"}))).is_err());
    }

    #[test]
    fn sse_handles_chunk_boundaries_crlf_comments_and_multiline_data() {
        let mut parser = Sse::default();
        let mut values = Vec::new();
        for chunk in [
            b": heartbeat\r\n\r\nda".as_slice(),
            b"ta: {\"a\":\r\ndata: 1}\r",
            b"\n\r\n",
        ] {
            parser.push(chunk, |value| values.push(value)).unwrap();
        }
        assert_eq!(values, vec![json!({"a":1})]);
        assert!(
            Sse::default()
                .push(&vec![b'x'; LINE_LIMIT + 1], |_| {})
                .is_err()
        );
    }

    #[test]
    fn internal_password_is_redacted_recursively() {
        let password = "test-secret-password";
        let value = json!({"nested":[password,STANDARD.encode(format!("opencode:{password}"))]});
        assert_eq!(
            redact(value, password),
            json!({"nested":["[redacted]","[redacted]"]})
        );
    }

    #[tokio::test]
    #[ignore = "requires an explicitly selected OpenCode v2 binary and isolated XDG directories; no model calls"]
    async fn real_opencode_without_model_calls() {
        let program = std::env::var_os("SSMCP_TEST_OPENCODE").expect("set SSMCP_TEST_OPENCODE");
        let directory =
            std::env::var_os("SSMCP_TEST_DIRECTORY").expect("set an isolated SSMCP_TEST_DIRECTORY");
        let directory = std::path::PathBuf::from(directory);
        std::fs::create_dir_all(&directory).unwrap();
        let backend = OpenCode::start(&program, &[]).await.unwrap();
        let attempt: Result<(), String> = async {
            let tools = backend.tools().await?;
            assert!(tools.len() >= 100);
            eprintln!("OpenCode native catalog: {} tools, {} bytes",tools.len(),tools.iter().map(|t|t.to_string().len()).sum::<usize>());
            let response = backend.call("opencode.server.info", json!({})).await?;
            assert_eq!(response["isError"], false);
            let response = backend
                .call(
                    "opencode.api",
                    json!({"method":"GET","url_path":"/api/info"}),
                )
                .await?;
            assert_ne!(response["isError"], true);
            let schema = backend
                .call("opencode.schema", json!({"operation_id":"session.create"}))
                .await?;
            assert!(schema.to_string().contains("session.create"));
            let created = backend.call("opencode.api", json!({"operation_id":"session.create","body":{
                "title":"ssmcp no-model protocol smoke","location":{"directory":directory},
                "permissions":[{"action":"ssmcp.probe","resource":"*","effect":"ask"}]
            }})).await?;
            assert_eq!(created["isError"],false,"{created}");
            let session = created["structuredContent"]["body"]["data"]["id"].as_str().ok_or("missing session id")?;
            let permission = backend.call("opencode.session.permission.create",json!({
                "path":{"sessionID":session},"body":{"action":"ssmcp.probe","resources":["probe"]}
            })).await?;
            assert_eq!(permission["isError"],false,"{permission}");
            assert_eq!(permission["structuredContent"]["body"]["data"]["effect"],"ask");
            let request_id = permission["structuredContent"]["body"]["data"]["id"].as_str().ok_or("missing permission id")?;
            let pending = backend.call("opencode.session.permission.list",json!({"path":{"sessionID":session}})).await?;
            assert!(pending.to_string().contains(request_id));
            let rejected = backend.call("opencode.session.permission.reply",json!({
                "path":{"sessionID":session,"requestID":request_id},"body":{"decision":"reject"}
            })).await?;
            assert_eq!(rejected["structuredContent"]["status"],204);
            let binary = STANDARD.encode([0_u8,1,255,13,10,128]);
            let written = backend.call("opencode.experimental.fs.write",json!({
                "query":{"location":{"directory":directory},"path":"probe.bin"},"body_base64":binary
            })).await?;
            assert_eq!(written["isError"],false,"{written}");
            let listing = backend.call("opencode.fs.list",json!({"query":{"location":{"directory":directory}}})).await?;
            assert!(listing.to_string().contains("probe.bin"));
            let read = backend.call("opencode.fs.read",json!({
                "path":{"wildcard":"probe.bin"},"query":{"location":{"directory":directory}}
            })).await?;
            assert_eq!(read["structuredContent"]["body_base64"],binary);
            let partial = backend.call("opencode.api",json!({"operation_id":"fs.read",
                "path":{"wildcard":"probe.bin"},"query":{"location":{"directory":directory}},"headers":{"Range":"bytes=1-2"}
            })).await?;
            assert_eq!(partial["structuredContent"]["status"],206);
            assert_eq!(partial["structuredContent"]["body_base64"],STANDARD.encode([1_u8,255]));
            let shell = backend.call("opencode.shell.create",json!({"query":{"location":{"directory":directory}},
                "body":{"command":"printf 'ssmcp-opencode-smoke\\n'"}
            })).await?;
            assert_eq!(shell["isError"],false,"{shell}");
            let shell_id = shell["structuredContent"]["body"]["data"]["id"].as_str().ok_or("missing shell id")?;
            let mut seen_output = false;
            for _ in 0..30 {
                let output = backend.call("opencode.shell.output",json!({"path":{"id":shell_id},"query":{"location":{"directory":directory}}})).await?;
                if output["structuredContent"]["body"]["data"]["output"].as_str().is_some_and(|s|s.contains("ssmcp-opencode-smoke")) {
                    seen_output = true; break;
                }
                sleep(Duration::from_millis(100)).await;
            }
            assert!(seen_output,"native shell output was not captured");
            let stream = backend.call("opencode.event.subscribe",json!({"query":{"types":["permission.asked","permission.replied"]}})).await?;
            let stream_id = stream["structuredContent"]["stream_id"].as_u64().ok_or("missing stream id")?;
            backend.call("opencode.stream_close",json!({"stream_id":stream_id})).await?;
            let events = backend.call("opencode.events",json!({"max_events":256})).await?;
            assert!(events.to_string().contains("permission.asked"));
            backend.call("opencode.shell.remove",json!({"path":{"id":shell_id},"query":{"location":{"directory":directory}}})).await?;
            let deleted = backend.call("opencode.session.remove",json!({"path":{"sessionID":session}})).await?;
            assert_eq!(deleted["structuredContent"]["status"],204);
            std::fs::remove_file(directory.join("probe.bin")).map_err(|e|e.to_string())?;
            Ok(())
        }
        .await;
        backend.shutdown().await;
        attempt.unwrap();
    }
}
