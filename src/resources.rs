//! Read-only MCP resources, filtered through the same live authorization as tools.
//! These are snapshots; the JSON-only transport does not advertise subscriptions.
mod client;
use crate::{
    broker::{Authorization, Broker, Session},
    code_mode,
    wire::{self, Operation},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Arc};

pub const PAGE_SIZE: usize = 32;
pub const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
pub const MAX_URI_BYTES: usize = 8192;

const GUIDE: &str = include_str!("../docs/code-mode.md");

#[derive(Debug, PartialEq, Eq)]
pub enum ResourceError {
    InvalidParams(&'static str),
    NotFound,
    Unavailable,
    TooLarge,
}

impl ResourceError {
    pub fn message(&self) -> &'static str {
        match self {
            Self::InvalidParams(message) => message,
            Self::NotFound => "Resource not found",
            Self::Unavailable => "Resource is unavailable; refresh the client list and retry",
            Self::TooLarge => "Resource exceeds the 1 MiB content limit; use a smaller native page",
        }
    }
}

fn descriptor(uri: String, name: String, description: &str, mime: &str) -> Value {
    json!({
        "uri":uri, "name":name, "description":description, "mimeType":mime,
        "annotations":{"audience":["assistant","user"]}
    })
}

fn client_info(session: &Session) -> Value {
    let mut value = json!(session.info());
    value["cwd_kind"] = json!("startup");
    value["controller_metadata"] = json!(session.supports_controller());
    value
}

fn snapshot<T: serde::Serialize>(value: &T) -> String {
    let bytes = serde_json::to_vec(value).expect("resource metadata is serializable");
    let digest = Sha256::digest(bytes);
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn page_offset(
    cursor: Option<&str>,
    fingerprint: &str,
    length: usize,
) -> Result<usize, ResourceError> {
    let Some(cursor) = cursor else { return Ok(0) };
    if cursor.len() > 128 {
        return Err(ResourceError::InvalidParams("resource cursor is too long"));
    }
    let mut parts = cursor.split(':');
    let version = parts.next();
    let digest = parts.next();
    let offset = parts.next().and_then(|value| value.parse::<usize>().ok());
    if version != Some("v1")
        || digest != Some(fingerprint)
        || offset.is_none_or(|offset| offset == 0 || offset >= length || offset % PAGE_SIZE != 0)
        || parts.next().is_some()
    {
        return Err(ResourceError::InvalidParams(
            "resource cursor is invalid or stale; restart listing without a cursor",
        ));
    }
    Ok(offset.unwrap())
}

fn next_cursor(fingerprint: &str, end: usize, length: usize) -> Option<String> {
    (end < length).then(|| format!("v1:{fingerprint}:{end}"))
}

async fn sessions(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
) -> Result<Vec<Arc<Session>>, ResourceError> {
    broker
        .clients(user, authorization)
        .await
        .map_err(|_| ResourceError::Unavailable)
}

pub async fn list(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
    cursor: Option<&str>,
) -> Result<Value, ResourceError> {
    let sessions = sessions(broker, user, authorization).await?;
    let mut entries = vec![
        descriptor(
            "ssmcp://guide".into(),
            "ssmcp usage guide".into(),
            "Read this first: discover clients, read schemas and dispatch bounded code_mode calls.",
            "text/markdown",
        ),
        descriptor(
            "ssmcp://limits".into(),
            "Runtime limits".into(),
            "Current code_mode and resource budgets, supported backends and observation capabilities.",
            "application/json",
        ),
        descriptor(
            "ssmcp://collaboration".into(),
            "Session agent collaboration".into(),
            "Discover peers, exchange private or broadcast messages, acknowledge delivery and manage participant leases.",
            "text/markdown",
        ),
        descriptor(
            "ssmcp://skill".into(),
            "ssmcp skill".into(),
            "Relay discovery guidance; each client publishes its current usage skill through its resource directory.",
            "text/markdown",
        ),
        descriptor(
            "ssmcp://clients".into(),
            "Online clients".into(),
            "A paged snapshot of live clients allowed by your identity and this endpoint.",
            "application/json",
        ),
    ];
    for session in sessions {
        let id = session.client_id();
        let info = session.info();
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/info"),
            format!("{}: connection", info.name),
            "Live connection metadata. cwd is the startup directory, not an agent's current working directory.",
            "application/json",
        ));
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/tools"),
            format!("{}: native tool schemas", info.name),
            "One page of live native tools and input schemas. Follow nextUri for remaining pages.",
            "application/json",
        ));
        for (kind, name, description) in [
            (
                "capabilities",
                "client capabilities",
                "Versioned atomic capability manifest reported by the client; schemas stay on the client.",
            ),
            (
                "resources",
                "client resource directory",
                "Paged resources owned by the client, forwarded without a relay provider catalog.",
            ),
            (
                "resource-templates",
                "client resource templates",
                "Paged native resource templates owned by the client.",
            ),
        ] {
            entries.push(descriptor(
                format!("ssmcp://clients/{id}/{kind}"),
                format!("{}: {name}", info.name),
                description,
                "application/json",
            ));
        }
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/context"),
            format!("{}: context roots", info.name),
            "Read-only context and skill sources. Installed global skills use exact skill-directory or package roots; ordinary filesystem access stays inside the workspace.",
            "application/json",
        ));
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/skills"),
            format!("{}: skills", info.name),
            "One page of discovered skill metadata in the workspace root. Follow nextUri for remaining pages.",
            "application/json",
        ));
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/skills/view"),
            format!("{}: effective skill view", info.name),
            "Live Markdown skill index. Project skills override same-named skills in explicitly granted global roots.",
            "text/markdown",
        ));
        entries.push(descriptor(
            format!("ssmcp://clients/{id}/agents"),
            format!("{}: workspace instructions", info.name),
            "The applicable AGENTS.md instruction chain for the workspace root.",
            "application/json",
        ));
    }
    let fingerprint = snapshot(&entries);
    let offset = page_offset(cursor, &fingerprint, entries.len())?;
    let end = (offset + PAGE_SIZE).min(entries.len());
    let mut result = json!({"resources":entries[offset..end]});
    if let Some(next) = next_cursor(&fingerprint, end, entries.len()) {
        result["nextCursor"] = json!(next);
    }
    Ok(result)
}

pub fn templates() -> Value {
    json!({"resourceTemplates":[
        {
            "uriTemplate":"ssmcp://clients/{client_id}/capabilities",
            "name":"Client capability manifest",
            "description":"Live client version, registry generation and native resource entrypoints.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/resources{?cursor}",
            "name":"Client-owned resource directory",
            "description":"Generic paged forwarding of resources reported by this client.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/resource-templates{?cursor}",
            "name":"Client-owned resource templates",
            "description":"Generic paged forwarding of native resource templates.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/resource{?uri}",
            "name":"Client-owned resource contents",
            "description":"Read an exact URI from that client's resource directory or resolved native template."
        },
        {
            "uriTemplate":"ssmcp://clients{?cursor}",
            "name":"Online client inventory",
            "description":"Read live authorized client metadata; follow the exact nextUri from the prior page.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/info",
            "name":"Client metadata",
            "description":"Read a currently authorized live connection's metadata, using its discovered id.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/tools{?cursor}",
            "name":"Client native tool schemas",
            "description":"Read native schemas for a discovered client; follow nextUri without altering its opaque cursor.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/context",
            "name":"Client context roots",
            "description":"Read configured context roots and exact installed or embedded skill sources without starting a model.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/skills{?root,scope,cursor}",
            "name":"Client skills",
            "description":"Read live skill metadata in a discovered root (default workspace), or scope=effective for project-over-global resolution; follow nextUri for further pages.",
            "mimeType":"application/json"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/skills/view{?root,scope,cursor}",
            "name":"Live skill view",
            "description":"Read a Markdown skill index. Defaults to project-over-global resolution; an explicit root selects only that granted root.",
            "mimeType":"text/markdown"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/skills/read{?root,path}",
            "name":"Live skill content",
            "description":"Read exact current SKILL.md text from a discovered root and relative path, using a metadata page's readUri.",
            "mimeType":"text/markdown"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/agents/read{?root,path}",
            "name":"Live instruction content",
            "description":"Read exact current AGENTS.md, AGENTS.override.md or agents.md text from a discovered root and relative path.",
            "mimeType":"text/markdown"
        },
        {
            "uriTemplate":"ssmcp://clients/{client_id}/agents{?root,path}",
            "name":"Client instruction chain",
            "description":"Resolve AGENTS.md files for a relative target path (default .) in a discovered root (default workspace).",
            "mimeType":"application/json"
        }
    ]})
}

fn limits() -> Value {
    json!({
        "server":{"name":"ssmcpd","version":env!("CARGO_PKG_VERSION")},
        "code_mode":{
            "max_code_bytes":code_mode::MAX_CODE_BYTES,
            "default_timeout_ms":code_mode::DEFAULT_TIMEOUT_MS,
            "max_timeout_ms":code_mode::MAX_TIMEOUT_MS,
            "max_result_bytes":code_mode::MAX_RESULT_BYTES,
            "max_log_bytes":code_mode::MAX_LOG_BYTES,
            "max_images":code_mode::MAX_IMAGES,
            "max_image_encoded_bytes":code_mode::MAX_IMAGE_ENCODED_BYTES,
            "image_output":"image(nativeResult) emits actual MCP image blocks; metadata stays separate",
            "max_native_calls":code_mode::MAX_CALLS,
            "max_native_inflight":code_mode::MAX_NATIVE_INFLIGHT,
            "max_concurrent_runs":code_mode::MAX_CONCURRENT,
            "execution_location":"ssmcp server isolated process",
            "caller_workspace_access":"requires an explicit connected bridge"
        },
        "collaboration":crate::collab::limits(),
        "await":crate::http::await_tool::limits(),
        "question_answers":{"origin":"local human; historical actor origins remain distinguishable","delivery":"next eligible verified bound call; native nonconsuming probe has a 1s total deadline","read_consumes":false,"restart_persistence":false},
        "managed_workspaces":{
            "max_active_syncs":crate::bridge::MAX_ACTIVE_SYNCS,
            "max_active_ports":crate::bridge::MAX_ACTIVE_PORTS,
            "max_forwarded_connections":crate::bridge::MAX_CONNECTIONS,
            "sync_defaults":{"dry_run":true,"delete":false},
            "port_scope":"loopback only",
            "code_mode_helpers":[
                "native","capabilities","resources.forClient","tools.search","fs","fs.readText","fs.writeText","fs.editText",
                "context.roots","skills.list","skills.read","skills.view",
                "agents.list","agents.read","agents.resolve",
                "collab.join","collab.list","collab.update","collab.send","collab.receive","collab.ack","collab.leave",
                "messages.list","messages.send","messages.broadcast","messages.inbox","messages.ack","messages.status",
                "rsync","sync","jobs","ports","resources","reload","outputs",
                "computer.version","computer.download","computer.install","computer.uninstall",
                "terminal.open","terminal.list","terminal.select","terminal.close",
                "terminal.read","terminal.write","terminal.status","terminal.resize",
                "exec.start","exec.list","exec.status","exec.wait","exec.read","exec.write",
                "exec.cancel","exec.forget","exec.events","exec.run","exec.parallel",
                "rtk.status","rtk.run","rtk.start"
            ],
            "text_files":{
                "max_write_bytes":262144,
                "max_read_bytes":262144,
                "min_read_bytes":4,
                "offset_unit":"UTF-8 bytes",
                "expected_sha256":"guards cooperating native writes, not arbitrary external programs"
            },
            "context":{
                "model_invocation":false,
                "additional_roots":"explicit --context-root directories plus exact global skill directories and linked packages; skill-only grants do not extend ordinary files or instructions",
                "skill_overlay":"project skills override same-named skills in explicitly granted roots; fresh scan per read"
            }
        },
        "client_capabilities":{
            "origin":"client",
            "manifest":"ssmcp://clients/{client_id}/capabilities",
            "resource_directory":"ssmcp://clients/{client_id}/resources",
            "native_limits":"ssmcp-client://limits",
            "cache":"none; each native resource read fetches the current client"
        },
        "wire":{
            "version":wire::VERSION,
            "max_frame_bytes":wire::MAX_FRAME,
            "max_inflight_per_client":wire::MAX_INFLIGHT
        },
        "resources":{
            "page_size":PAGE_SIZE,
            "max_content_bytes":MAX_RESOURCE_BYTES,
            "max_uri_bytes":MAX_URI_BYTES,
            "max_cursor_bytes":128,
            "subscribe":false,
            "listChanged":false,
            "observation":"fresh snapshot per read"
        },
        "public_tools":["list", "code_mode", "collab", "await", "messages"]
    })
}

fn content(uri: &str, mime: &str, text: String) -> Result<Value, ResourceError> {
    if text.len() > MAX_RESOURCE_BYTES {
        return Err(ResourceError::TooLarge);
    }
    Ok(json!({"contents":[{"uri":uri,"mimeType":mime,"text":text}]}))
}

fn json_content(uri: &str, value: Value) -> Result<Value, ResourceError> {
    content(
        uri,
        "application/json",
        serde_json::to_string(&value).map_err(|_| ResourceError::Unavailable)?,
    )
}

fn with_cursor(base: &str, cursor: &str) -> String {
    let query =
        serde_urlencoded::to_string([("cursor", cursor)]).expect("cursor pairs are serializable");
    format!("{base}?{query}")
}

type Query = BTreeMap<String, String>;

fn query_encoding_valid(query: &str) -> bool {
    let mut decoded = Vec::with_capacity(query.len());
    let mut bytes = query.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(high) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let Some(low) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            decoded.push((high * 16 + low) as u8);
        } else {
            decoded.push(byte);
        }
    }
    std::str::from_utf8(&decoded).is_ok()
}

fn parse_uri(uri: &str) -> Result<(&str, Query), ResourceError> {
    if uri.is_empty() || uri.len() > MAX_URI_BYTES || uri.chars().any(char::is_control) {
        return Err(ResourceError::InvalidParams(
            "resource uri must be 1..=8192 bytes without control characters",
        ));
    }
    let (base, query) = uri
        .split_once('?')
        .map_or((uri, None), |(base, query)| (base, Some(query)));
    if base.contains('#') || query.is_some_and(|q| q.contains('#')) {
        return Err(ResourceError::NotFound);
    }
    let mut values = Query::new();
    let Some(query) = query else {
        return Ok((base, values));
    };
    let allowed: &[&str] = if base == "ssmcp://clients" {
        &["cursor"]
    } else {
        match base
            .strip_prefix("ssmcp://clients/")
            .and_then(|path| path.split_once('/'))
            .map(|(_, kind)| kind)
        {
            Some("tools" | "resources" | "resource-templates") => &["cursor"],
            Some("resource") => &["uri"],
            Some("skills" | "skills/view") => &["root", "scope", "cursor"],
            Some("agents" | "agents/read" | "skills/read") => &["root", "path"],
            Some("info" | "context" | "capabilities") => &[],
            _ => return Err(ResourceError::NotFound),
        }
    };
    if query.is_empty() || !query_encoding_valid(query) {
        return Err(ResourceError::InvalidParams("invalid resource query"));
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(query)
        .map_err(|_| ResourceError::InvalidParams("invalid resource query"))?;
    for (key, value) in pairs {
        if !allowed.contains(&key.as_str()) {
            return Err(ResourceError::InvalidParams(
                "resource query contains an unsupported parameter",
            ));
        }
        if value.is_empty()
            || value.chars().any(char::is_control)
            || (key == "cursor" && value.len() > 128)
        {
            return Err(ResourceError::InvalidParams(
                "resource query values must be nonempty without controls; cursor is at most 128 bytes",
            ));
        }
        if values.insert(key, value).is_some() {
            return Err(ResourceError::InvalidParams(
                "resource query parameters must not be repeated",
            ));
        }
    }
    if values.is_empty() {
        return Err(ResourceError::InvalidParams("invalid resource query"));
    }
    if values
        .get("scope")
        .is_some_and(|scope| !matches!(scope.as_str(), "root" | "effective"))
    {
        return Err(ResourceError::InvalidParams(
            "skill scope must be root or effective",
        ));
    }
    Ok((base, values))
}

async fn native_view(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
    session: &Session,
    kind: &str,
    query: &Query,
) -> Result<Value, ResourceError> {
    let (name, field) = match kind {
        "context" => ("ssmcp.context.roots", "roots"),
        "skills" => ("ssmcp.skills.list", "skills"),
        "skills/view" => ("ssmcp.skills.view", "markdown"),
        "skills/read" => ("ssmcp.skills.read", "content"),
        "agents" => ("ssmcp.agents.resolve", "files"),
        "agents/read" => ("ssmcp.agents.read", "content"),
        _ => return Err(ResourceError::NotFound),
    };
    let mut arguments = json!({});
    if let Some(root) = query.get("root") {
        arguments["root"] = json!(root);
    }
    if matches!(kind, "skills" | "skills/view") {
        if let Some(cursor) = query.get("cursor") {
            arguments["cursor"] = json!(cursor);
        }
        if let Some(scope) = query.get("scope") {
            arguments["scope"] = json!(scope);
        } else if kind == "skills/view" && !query.contains_key("root") {
            // Declare the requested semantics so older clients fail explicitly
            // instead of returning their single-root default as an effective view.
            arguments["scope"] = json!("effective");
        }
    } else if kind == "agents" {
        arguments["path"] = json!(query.get("path").map(String::as_str).unwrap_or("."));
    } else if matches!(kind, "skills/read" | "agents/read") {
        arguments["path"] = json!(query.get("path").ok_or(ResourceError::InvalidParams(
            "instruction content reads require a relative path"
        ),)?);
    }
    let response = session
        .call_with_authorization(
            Operation::CallTool {
                name: name.into(),
                arguments,
            },
            None,
            authorization,
        )
        .await;
    // A native read may outlive its request's connection-bound authorization.
    broker
        .client(user, authorization, session.client_id())
        .await
        .map_err(|_| ResourceError::NotFound)?;
    let response = response.map_err(|_| ResourceError::Unavailable)?;
    if response
        .get("isError")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(ResourceError::Unavailable);
    }
    let mut view = response
        .get("structuredContent")
        .filter(|value| {
            value.is_object()
                && if matches!(field, "markdown" | "content") {
                    value[field].is_string()
                } else {
                    value[field].is_array()
                }
        })
        .cloned()
        .ok_or(ResourceError::Unavailable)?;
    view["client"] = client_info(session);
    Ok(view)
}

fn add_read_uris(page: &mut Value, client_id: &str) -> Result<(), ResourceError> {
    let default_root = page["root"]["id"]
        .as_str()
        .unwrap_or("workspace")
        .to_owned();
    for (field, kind) in [("skills", "skills"), ("files", "agents")] {
        if let Some(entries) = page.get_mut(field).and_then(Value::as_array_mut) {
            for entry in entries {
                let Some(path) = entry["path"].as_str() else {
                    continue;
                };
                let root = entry["root"]["id"].as_str().unwrap_or(&default_root);
                let query = serde_urlencoded::to_string([("root", root), ("path", path)])
                    .map_err(|_| ResourceError::Unavailable)?;
                let uri = format!("ssmcp://clients/{client_id}/{kind}/read?{query}");
                if uri.len() > MAX_URI_BYTES {
                    return Err(ResourceError::TooLarge);
                }
                entry["readUri"] = json!(uri);
            }
        }
    }
    Ok(())
}

pub async fn read(
    broker: &Broker,
    user: &str,
    authorization: &Authorization,
    uri: &str,
) -> Result<Value, ResourceError> {
    let (base, query) = parse_uri(uri)?;
    let cursor = query.get("cursor").cloned();
    // Validate endpoint scope and a connection-bound grant even for static guidance.
    let current = sessions(broker, user, authorization).await?;
    if base == "ssmcp://clients" {
        let clients: Vec<_> = current.iter().map(|session| client_info(session)).collect();
        let fingerprint = snapshot(&clients);
        let offset = page_offset(cursor.as_deref(), &fingerprint, clients.len())?;
        let end = (offset + PAGE_SIZE).min(clients.len());
        let mut value =
            json!({"clients":clients[offset..end],"snapshot":fingerprint,"cwd_kind":"startup"});
        if let Some(next) = next_cursor(&fingerprint, end, clients.len()) {
            value["nextUri"] = json!(with_cursor(base, &next));
            value["nextCursor"] = json!(next);
        }
        return json_content(uri, value);
    }
    if let Some(path) = base.strip_prefix("ssmcp://clients/") {
        let (id, kind) = path.split_once('/').ok_or(ResourceError::NotFound)?;
        if id.len() != 64
            || !id
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ResourceError::NotFound);
        }
        let session = broker
            .client(user, authorization, id)
            .await
            .map_err(|_| ResourceError::NotFound)?;
        return match kind {
            "capabilities" | "resources" | "resource-templates" | "resource" => {
                client::read(
                    broker,
                    user,
                    authorization,
                    &session,
                    kind,
                    (uri, base),
                    &query,
                )
                .await
            }
            "info" => json_content(uri, client_info(&session)),
            "tools" => {
                let page = session
                    .call_with_authorization(Operation::ListTools { cursor }, None, authorization)
                    .await;
                // Reading a slow native schema does not preserve authority after disconnect.
                broker
                    .client(user, authorization, id)
                    .await
                    .map_err(|_| ResourceError::NotFound)?;
                let mut page = page.map_err(|_| ResourceError::Unavailable)?;
                if !page.is_object() || !page["tools"].is_array() {
                    return Err(ResourceError::Unavailable);
                }
                page["client"] = client_info(&session);
                if let Some(next) = page.get("nextCursor") {
                    let next = next.as_str().ok_or(ResourceError::Unavailable)?;
                    if next.len() > 128 || next.is_empty() || next.chars().any(char::is_control) {
                        return Err(ResourceError::Unavailable);
                    }
                    page["nextUri"] = json!(with_cursor(base, next));
                }
                json_content(uri, page)
            }
            "context" | "skills" | "skills/view" | "skills/read" | "agents" | "agents/read" => {
                let mut page =
                    native_view(broker, user, authorization, &session, kind, &query).await?;
                if matches!(kind, "skills" | "skills/view")
                    && let Some(next) = page.get("next_cursor").filter(|value| !value.is_null())
                {
                    let next = next.as_str().ok_or(ResourceError::Unavailable)?;
                    if next.len() > 128 || next.is_empty() || next.chars().any(char::is_control) {
                        return Err(ResourceError::Unavailable);
                    }
                    let mut continuation = query.clone();
                    continuation.insert("cursor".into(), next.into());
                    let encoded = serde_urlencoded::to_string(continuation)
                        .map_err(|_| ResourceError::Unavailable)?;
                    let next_uri = format!("{base}?{encoded}");
                    if next_uri.len() > MAX_URI_BYTES {
                        return Err(ResourceError::Unavailable);
                    }
                    page["nextUri"] = json!(next_uri);
                }
                if matches!(kind, "skills/read" | "agents/read") {
                    return content(
                        uri,
                        "text/markdown",
                        page["content"].as_str().unwrap().to_owned(),
                    );
                }
                add_read_uris(&mut page, id)?;
                if kind == "skills/view" {
                    let mut markdown = page["markdown"].as_str().unwrap().to_owned();
                    if page["truncated"] == true {
                        markdown.push_str("\n\nSkill discovery is incomplete; inspect the metadata diagnostics and narrow the scanned paths.\n");
                    }
                    let mut metadata_query = query.clone();
                    if !metadata_query.contains_key("root") && !metadata_query.contains_key("scope")
                    {
                        metadata_query.insert("scope".into(), "effective".into());
                    }
                    let metadata_query = serde_urlencoded::to_string(metadata_query)
                        .map_err(|_| ResourceError::Unavailable)?;
                    let metadata_uri = format!("ssmcp://clients/{id}/skills?{metadata_query}");
                    markdown.push_str(&format!(
                        "\n\n[Skill metadata and full-content links]({metadata_uri})\n"
                    ));
                    if let Some(next) = page["nextUri"].as_str() {
                        markdown.push_str(&format!("\n\n[Next skill page]({next})\n"));
                    }
                    return content(uri, "text/markdown", markdown);
                }
                json_content(uri, page)
            }
            _ => Err(ResourceError::NotFound),
        };
    }
    if cursor.is_some() {
        return Err(ResourceError::NotFound);
    }
    match base {
        "ssmcp://guide" => content(uri, "text/markdown", GUIDE.into()),
        "ssmcp://limits" => json_content(uri, limits()),
        "ssmcp://collaboration" => content(
            uri,
            "text/markdown",
            include_str!("../docs/agent-collaboration.md").into(),
        ),
        "ssmcp://skill" => content(uri, "text/markdown", GUIDE.into()),
        _ => Err(ResourceError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture(broker: &Broker, user: &str) -> (Arc<Session>, Authorization) {
        let session = broker.test_register(user, &"a".repeat(64)).await;
        let auth = Authorization::Legacy {
            user: user.into(),
            anchor: Arc::downgrade(&session),
        };
        (session, auth)
    }

    fn decode(value: Value) -> Value {
        serde_json::from_str(value["contents"][0]["text"].as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn every_resource_is_filtered_by_endpoint_and_connection_lifetime() {
        let broker = Broker::new("https://ssmcp.example".into());
        let (alice, auth) = fixture(&broker, "alice").await;
        let (bob, _) = fixture(&broker, "bob").await;
        let page = list(&broker, "alice", &auth, None).await.unwrap();
        let text = page.to_string();
        assert!(text.contains(alice.client_id()));
        assert!(!text.contains(bob.client_id()));
        assert!(list(&broker, "bob", &auth, None).await.is_err());
        for kind in [
            "capabilities",
            "resources",
            "resource-templates",
            "resource",
            "info",
            "tools",
            "context",
            "skills",
            "skills/view",
            "skills/read",
            "agents",
            "agents/read",
        ] {
            assert_eq!(
                read(
                    &broker,
                    "alice",
                    &auth,
                    &format!("ssmcp://clients/{}/{kind}", bob.client_id())
                )
                .await
                .unwrap_err(),
                ResourceError::NotFound,
            );
        }
        let inventory = decode(
            read(&broker, "alice", &auth, "ssmcp://clients")
                .await
                .unwrap(),
        );
        assert_eq!(inventory["clients"].as_array().unwrap().len(), 1);
        broker.disconnect_all().await;
        for uri in [
            "ssmcp://guide",
            "ssmcp://limits",
            "ssmcp://collaboration",
            "ssmcp://skill",
            "ssmcp://clients",
        ] {
            assert!(read(&broker, "alice", &auth, uri).await.is_err());
        }
        assert!(list(&broker, "alice", &auth, None).await.is_err());
    }

    #[tokio::test]
    async fn list_pagination_is_bounded_and_rejects_changed_snapshots() {
        let broker = Broker::new("https://ssmcp.example".into());
        let (_anchor, auth) = fixture(&broker, "alice").await;
        for _ in 0..20 {
            broker.test_register("alice", &"a".repeat(64)).await;
        }
        let first = list(&broker, "alice", &auth, None).await.unwrap();
        assert_eq!(first["resources"].as_array().unwrap().len(), PAGE_SIZE);
        let cursor = first["nextCursor"].as_str().unwrap().to_owned();
        let mut pages = vec![first];
        while let Some(next) = pages.last().unwrap()["nextCursor"].as_str() {
            pages.push(list(&broker, "alice", &auth, Some(next)).await.unwrap());
        }
        assert_eq!(
            pages
                .iter()
                .map(|page| page["resources"].as_array().unwrap().len())
                .collect::<Vec<_>>(),
            [32, 32, 32, 32, 32, 32, 2]
        );
        let all: Vec<_> = pages
            .iter()
            .flat_map(|page| page["resources"].as_array().unwrap())
            .map(|r| r["uri"].as_str().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = all.iter().collect();
        assert_eq!(all.len(), 194);
        assert_eq!(all.len(), unique.len());
        broker.test_register("alice", &"a".repeat(64)).await;
        assert!(matches!(
            list(&broker, "alice", &auth, Some(&cursor)).await,
            Err(ResourceError::InvalidParams(_))
        ));
    }

    #[tokio::test]
    async fn inventory_pagination_and_metadata_are_live_without_credentials() {
        let broker = Broker::new("https://ssmcp.example".into());
        let (anchor, auth) = fixture(&broker, "alice").await;
        for _ in 0..33 {
            broker.test_register("alice", &"a".repeat(64)).await;
        }
        let first = decode(
            read(&broker, "alice", &auth, "ssmcp://clients")
                .await
                .unwrap(),
        );
        assert_eq!(first["clients"].as_array().unwrap().len(), PAGE_SIZE);
        let second = decode(
            read(&broker, "alice", &auth, first["nextUri"].as_str().unwrap())
                .await
                .unwrap(),
        );
        assert_eq!(second["clients"].as_array().unwrap().len(), 2);
        let info = decode(
            read(
                &broker,
                "alice",
                &auth,
                &format!("ssmcp://clients/{}/info", anchor.client_id()),
            )
            .await
            .unwrap(),
        );
        assert_eq!(info["cwd_kind"], "startup");
        for key in [
            "login",
            "login_code",
            "access_token",
            "refresh_token",
            "allow",
            "environment",
        ] {
            assert!(info.get(key).is_none());
        }
    }

    #[test]
    fn unknown_and_ambiguous_uri_queries_are_rejected() {
        for uri in [
            "",
            "ssmcp://clients?cursor=",
            "ssmcp://clients?cursor=a&cursor=b",
            "ssmcp://clients?unknown=a",
            "ssmcp://clients?cursor=%0A",
            "ssmcp://clients?cursor=%GG",
            "ssmcp://clients?cursor=%C3",
            "ssmcp://clients/abc/context?root=workspace",
            "ssmcp://clients/abc/skills?root=one&root=two",
            "ssmcp://clients/abc/skills?root=one&ro%6Ft=two",
            "ssmcp://clients/abc/skills?path=.",
            "ssmcp://clients/abc/skills?scope=unknown",
            "ssmcp://clients/abc/skills/view?scope=effective&scope=root",
            "ssmcp://clients/abc/skills/read?cursor=next",
            "ssmcp://clients/abc/agents/read?scope=effective",
            "ssmcp://clients/abc/agents?cursor=next",
            "ssmcp://clients/abc/agents?path=%00",
            "ssmcp://guide#fragment",
            "ssmcp://clients\n",
        ] {
            assert!(parse_uri(uri).is_err(), "{uri}");
        }
        let value = "cursor /+=?&:";
        let uri = with_cursor("ssmcp://clients/abc/tools", value);
        assert_eq!(
            parse_uri(&uri).unwrap().1.get("cursor").map(String::as_str),
            Some(value)
        );
        let query =
            serde_urlencoded::to_string([("root", "context +&/?:"), ("path", "目录/a +&?=b")])
                .unwrap();
        let uri = format!("ssmcp://clients/abc/agents?{query}");
        let (_, values) = parse_uri(&uri).unwrap();
        assert_eq!(
            values.get("root").map(String::as_str),
            Some("context +&/?:")
        );
        assert_eq!(values.get("path").map(String::as_str), Some("目录/a +&?=b"));
        assert!(parse_uri(&format!("ssmcp://clients?cursor={}", "x".repeat(129))).is_err());
        assert!(
            parse_uri(&format!(
                "ssmcp://clients/abc/agents?path={}",
                "x".repeat(MAX_URI_BYTES)
            ))
            .is_err()
        );
        assert!(
            content(
                "ssmcp://guide",
                "text/plain",
                "x".repeat(MAX_RESOURCE_BYTES + 1)
            )
            .is_err()
        );
        assert_eq!(limits()["resources"]["subscribe"], false);
        assert_eq!(
            limits()["public_tools"],
            json!(["list", "code_mode", "collab", "await", "messages"])
        );
    }
}
