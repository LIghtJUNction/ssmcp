//! Authorized, bounded, one-use downloads. Payloads never touch the filesystem.
//!
//! Callers must derive `owner` from the authenticated MCP grant and pass the
//! broker's validated public URL. Neither value comes from tool arguments or Host.
use crate::{broker::Broker, wire::Operation};
use axum::{
    Router,
    body::{Body, Bytes},
    extract::Path,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Notify;

pub(super) const ROUTE: &str = "/_ssmcp/temporary/{id}";
pub(super) const DEFAULT_TTL_SECS: u64 = 120;
pub(super) const MAX_TTL_SECS: u64 = 900;
pub(super) const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
const MAX_LINKS: usize = 128;

#[derive(Clone, Serialize)]
pub(super) struct Published {
    pub(super) id: String,
    pub(super) url: String,
    pub(super) name: String,
    pub(super) mime_type: String,
    pub(super) bytes: usize,
    pub(super) expires_at_ms: u64,
    pub(super) single_use: bool,
}

impl Published {
    pub(super) fn content(&self) -> Value {
        json!({
            "type":"resource_link", "uri":self.url, "name":self.name,
            "mimeType":self.mime_type, "size":self.bytes,
            "description":format!("Single-use temporary download; expires at {} UTC milliseconds. HEAD does not consume it.", self.expires_at_ms),
        })
    }
}

/// Tool arguments never contain an owner, an OAuth token or an arbitrary URL.
/// The HTTP endpoint supplies the grant, and the native client resolves the path.
#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Arguments {
    Create {
        client_id: String,
        path: String,
        #[serde(default)]
        ttl_secs: Option<u64>,
    },
    Cleanup {
        id: String,
    },
}

impl Arguments {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Create {
                client_id,
                path,
                ttl_secs,
            } => {
                if !valid_id(client_id) {
                    return Err(
                        "client_id must be an exact 64-character connected client id".into(),
                    );
                }
                if path.is_empty() || path.len() > 4096 || path.chars().any(char::is_control) {
                    return Err("path must be 1..=4096 UTF-8 bytes without controls".into());
                }
                if ttl_secs.is_some_and(|ttl| !(1..=MAX_TTL_SECS).contains(&ttl)) {
                    return Err(format!("ttl_secs must be 1..={MAX_TTL_SECS}"));
                }
            }
            Self::Cleanup { id } => {
                if !valid_id(id) {
                    return Err("id must be the exact 64-character temporary link id".into());
                }
            }
        }
        Ok(())
    }
}

pub(super) fn parse(value: Value) -> Result<Arguments, String> {
    if value
        .get("ttl_secs")
        .is_some_and(|ttl| ttl.as_u64().is_none())
    {
        return Err("ttl_secs must be an integer".into());
    }
    let arguments: Arguments = serde_json::from_value(value)
        .map_err(|error| format!("invalid link arguments: {error}"))?;
    arguments.validate()?;
    Ok(arguments)
}

pub(super) fn tool_definition() -> Value {
    json!({
        "name":"link",
        "description":"Create a short-lived, single-use image URL on this server without inline base64, or clean it up. The selected client's native filesystem permissions apply. Supports PNG, JPEG, GIF and WebP up to the native 400 KiB limit. Anyone holding the URL can GET once; HEAD does not consume it. Unused links expire automatically.",
        "inputSchema":{
            "type":"object",
            "oneOf":[
                {
                    "type":"object", "additionalProperties":false,
                    "required":["action","client_id","path"],
                    "properties":{
                        "action":{"const":"create"},
                        "client_id":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"Exact authorized connected client id returned by list."},
                        "path":{"type":"string","minLength":1,"maxLength":4096,"description":"Image path resolved by the client's authorized native filesystem, not a URL."},
                        "ttl_secs":{"type":"integer","minimum":1,"maximum":MAX_TTL_SECS,"default":DEFAULT_TTL_SECS}
                    }
                },
                {
                    "type":"object", "additionalProperties":false,
                    "required":["action","id"],
                    "properties":{
                        "action":{"const":"cleanup"},
                        "id":{"type":"string","pattern":"^[0-9a-f]{64}$","description":"Temporary link id returned by create. Only its authenticated creator may clean it up."}
                    }
                }
            ]
        }
    })
}

/// Called by the authenticated MCP endpoint, using its verified account and
/// bearer grant. Source reads never bypass the broker or the native permissions.
pub(super) async fn execute(
    broker: Arc<Broker>,
    user: String,
    access_token: String,
    arguments: Arguments,
) -> Result<Value, String> {
    arguments.validate()?;
    let authorization = broker
        .authenticate(&user, &access_token)
        .await
        .ok_or("connection authorization ended; reconnect before using link")?;
    let owner = crate::bridge::owner(&user, &authorization)?;
    match arguments {
        Arguments::Cleanup { id } => {
            let removed = cleanup(&owner, &id)?;
            Ok(json!({
                "isError":false,
                "content":[{"type":"text","text":if removed {
                    "Temporary link removed."
                } else {
                    "Temporary link is already absent, consumed or expired."
                }}],
                "structuredContent":{"action":"cleanup","id":id,"removed":removed}
            }))
        }
        Arguments::Create {
            client_id,
            path,
            ttl_secs,
        } => {
            // Validate configuration before asking the native client to read.
            link_url(&broker.public_url, &"0".repeat(64))?;
            let session = broker.client(&user, &authorization, &client_id).await?;
            let native = session
                .call_with_authorization(
                    Operation::CallTool {
                        name: "ssmcp.fs.read_image".into(),
                        arguments: json!({"path":path}),
                    },
                    None,
                    &authorization,
                )
                .await?;

            // The native request may have waited for permission. Recheck the
            // current grant and exact connection before exposing its result.
            let current_authorization = broker
                .authenticate(&user, &access_token)
                .await
                .ok_or("connection authorization ended while reading the image")?;
            if crate::bridge::owner(&user, &current_authorization)? != owner {
                return Err("connection authorization changed while reading the image".into());
            }
            let current = broker
                .client(&user, &current_authorization, &client_id)
                .await?;
            if !Arc::ptr_eq(&session, &current) {
                return Err(
                    "client connection changed while reading the image; retry with its current id"
                        .into(),
                );
            }
            let published =
                publish_native_content(&owner, &broker.public_url, &native, Some(&path), ttl_secs)
                    .await?;
            let still_authorized =
                broker
                    .authenticate_now(&user, &access_token)
                    .is_some_and(|grant| {
                        grant.is_live()
                            && session.is_alive()
                            && grant.permits(&session)
                            && crate::bridge::owner(&user, &grant).is_ok_and(|now| now == owner)
                    });
            if !still_authorized {
                let _ = cleanup(&owner, &published.id);
                return Err(
                    "connection authorization ended before the image link was returned".into(),
                );
            }
            Ok(json!({
                "isError":false,
                "content":[
                    published.content(),
                    {"type":"text","text":"One GET consumes this image link. HEAD is non-consuming. To remove an unused link sooner, call link with action=cleanup and the returned id."}
                ],
                "structuredContent":{
                    "action":"create", "client_id":client_id,
                    "id":published.id, "url":published.url, "name":published.name,
                    "mime_type":published.mime_type, "bytes":published.bytes,
                    "expires_at_ms":published.expires_at_ms, "single_use":published.single_use,
                }
            }))
        }
    }
}

#[derive(Default)]
struct Usage {
    objects: usize,
    bytes: usize,
}

struct Quota {
    usage: Mutex<Usage>,
    max_objects: usize,
    max_bytes: usize,
}

impl Quota {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Reservation, String> {
        let mut usage = self.usage.lock().unwrap_or_else(|error| error.into_inner());
        if usage.objects >= self.max_objects || bytes > self.max_bytes.saturating_sub(usage.bytes) {
            return Err(
                "temporary download capacity is full; consume or clean up existing links".into(),
            );
        }
        usage.objects += 1;
        usage.bytes += bytes;
        Ok(Reservation {
            quota: self.clone(),
            bytes,
        })
    }
}

struct Reservation {
    quota: Arc<Quota>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut usage = self
            .quota
            .usage
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        usage.objects -= 1;
        usage.bytes -= self.bytes;
    }
}

struct Payload {
    data: Vec<u8>,
    _reservation: Reservation,
}

impl AsRef<[u8]> for Payload {
    fn as_ref(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Clone)]
struct Metadata {
    mime: HeaderValue,
    disposition: HeaderValue,
    length: usize,
}

struct Entry {
    owner: String,
    bytes: Bytes,
    metadata: Metadata,
    expires: Instant,
}

struct Registry {
    entries: Mutex<HashMap<String, Entry>>,
    quota: Arc<Quota>,
    changed: Notify,
    reaper_running: AtomicBool,
}

impl Registry {
    fn new(max_objects: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            quota: Arc::new(Quota {
                usage: Mutex::new(Usage::default()),
                max_objects,
                max_bytes,
            }),
            changed: Notify::new(),
            reaper_running: AtomicBool::new(false),
        }
    }

    fn next_expiry(&self, now: Instant) -> Option<Instant> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, entry| entry.expires > now);
        entries.values().map(|entry| entry.expires).min()
    }

    fn publish(
        &self,
        owner: &str,
        public_base: &str,
        bytes: Vec<u8>,
        mime: &str,
        filename: Option<&str>,
        ttl_secs: u64,
    ) -> Result<Published, String> {
        // bridge::owner is an opaque internal key with NUL separators. It is
        // compared only in memory and never copied into a URL or HTTP header.
        if owner.is_empty() || owner.len() > 512 {
            return Err("temporary downloads require an authenticated owner".into());
        }
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!(
                "temporary files are limited to {MAX_FILE_BYTES} bytes"
            ));
        }
        if !(1..=MAX_TTL_SECS).contains(&ttl_secs) {
            return Err(format!("ttl_secs must be 1..={MAX_TTL_SECS}"));
        }
        let (mime_type, mime_header, inline) = content_type(mime, &bytes)?;
        let name = filename
            .and_then(|name| name.rsplit(['/', '\\']).next())
            .filter(|name| !name.is_empty())
            .unwrap_or("download");
        if name.len() > 255 || name.chars().any(char::is_control) {
            return Err(
                "temporary filename must be at most 255 UTF-8 bytes without controls".into(),
            );
        }
        let disposition = disposition(name, inline)?;
        let now = Instant::now();
        self.next_expiry(now);
        let ttl = Duration::from_secs(ttl_secs);
        let expires_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "system clock is before the Unix epoch")?
            .as_millis()
            .saturating_add(u128::from(ttl_secs) * 1000)
            .min(u128::from(u64::MAX)) as u64;
        let length = bytes.len();
        let reservation = self.quota.reserve(length)?;
        let mut random = [0u8; 32];
        crate::platform::random_bytes(&mut random)
            .map_err(|_| "cannot create a secure temporary download identifier")?;
        let id = crate::hex::encode(random);
        let url = link_url(public_base, &id)?;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries.len() >= self.quota.max_objects {
            return Err(
                "temporary download capacity is full; consume or clean up existing links".into(),
            );
        }
        if entries.contains_key(&id) {
            return Err("temporary download identifier collision; retry publication".into());
        }
        // The quota remains owned by the Bytes allocation after GET removes the
        // entry, including while an HTTP response or body clone is still alive.
        entries.insert(
            id.clone(),
            Entry {
                owner: owner.into(),
                bytes: Bytes::from_owner(Payload {
                    data: bytes,
                    _reservation: reservation,
                }),
                metadata: Metadata {
                    mime: mime_header,
                    disposition,
                    length,
                },
                expires: now + ttl,
            },
        );
        drop(entries);
        self.changed.notify_one();
        Ok(Published {
            id,
            url,
            name: name.into(),
            mime_type,
            bytes: length,
            expires_at_ms,
            single_use: true,
        })
    }

    fn head(&self, id: &str, now: Instant) -> Option<Metadata> {
        if !valid_id(id) {
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries.get(id).is_some_and(|entry| entry.expires <= now) {
            entries.remove(id);
        }
        entries.get(id).map(|entry| entry.metadata.clone())
    }

    fn take(&self, id: &str, now: Instant) -> Option<Entry> {
        if !valid_id(id) {
            return None;
        }
        let entry = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(id)?;
        (entry.expires > now).then_some(entry)
    }

    fn cleanup(&self, owner: &str, id: &str) -> Result<bool, String> {
        if owner.is_empty() {
            return Err("temporary cleanup requires an authenticated owner".into());
        }
        if !valid_id(id) {
            return Ok(false);
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = entries.get(id) else {
            return Ok(false);
        };
        if entry.owner != owner {
            return Err("temporary download does not belong to the authenticated owner".into());
        }
        entries.remove(id);
        Ok(true)
    }

    fn start_reaper(self: &Arc<Self>) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| "temporary downloads require the server runtime")?;
        if self.reaper_running.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let registry = self.clone();
        let guard = ReaperGuard(registry.clone());
        runtime.spawn(async move {
            let _guard = guard;
            loop {
                let changed = registry.changed.notified();
                match registry.next_expiry(Instant::now()) {
                    Some(expires) => tokio::select! {
                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(expires)) => {},
                        _ = changed => {},
                    },
                    None => changed.await,
                }
            }
        });
        Ok(())
    }
}

struct ReaperGuard(Arc<Registry>);
impl Drop for ReaperGuard {
    fn drop(&mut self) {
        self.0.reaper_running.store(false, Ordering::Release);
    }
}

fn registry() -> &'static Arc<Registry> {
    static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Arc::new(Registry::new(MAX_LINKS, MAX_TOTAL_BYTES)))
}

fn valid_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn link_url(public_base: &str, id: &str) -> Result<String, String> {
    let mut url = Url::parse(public_base).map_err(|_| "invalid configured public URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "configured public URL must be an HTTP(S) origin without credentials or a path".into(),
        );
    }
    url.set_path(&format!("/_ssmcp/temporary/{id}"));
    Ok(url.into())
}

fn content_type(raw: &str, bytes: &[u8]) -> Result<(String, HeaderValue, bool), String> {
    if raw.is_empty() || raw.len() > 127 || !raw.is_ascii() || raw.chars().any(char::is_control) {
        return Err("invalid temporary file MIME type".into());
    }
    let essence = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let token = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&byte))
    };
    let Some((kind, subtype)) = essence.split_once('/') else {
        return Err("invalid temporary file MIME type".into());
    };
    if !token(kind) || !token(subtype) {
        return Err("invalid temporary file MIME type".into());
    }
    let inline = matches!(
        essence.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    );
    if inline && crate::output::image_mime_type(bytes) != Some(essence.as_str()) {
        return Err("temporary image bytes do not match their MIME type".into());
    }
    let header = HeaderValue::from_str(raw).map_err(|_| "invalid temporary file MIME header")?;
    Ok((raw.into(), header, inline))
}

fn disposition(name: &str, inline: bool) -> Result<HeaderValue, String> {
    let fallback: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::new();
    for byte in name.as_bytes() {
        if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(byte) {
            encoded.push(char::from(*byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("string write");
        }
    }
    HeaderValue::from_str(&format!(
        "{}; filename=\"{}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        fallback,
        encoded,
    ))
    .map_err(|_| "invalid temporary file disposition".into())
}

/// Publish bytes already obtained through an authorized native operation.
/// This API never opens a path, fetches a URL, or reads a directory.
pub(super) async fn publish_bytes(
    owner: &str,
    public_base: &str,
    bytes: Vec<u8>,
    mime: &str,
    filename: Option<&str>,
    ttl_secs: Option<u64>,
) -> Result<Published, String> {
    let registry = registry();
    registry.start_reaper()?;
    registry.publish(
        owner,
        public_base,
        bytes,
        mime,
        filename,
        ttl_secs.unwrap_or(DEFAULT_TTL_SECS),
    )
}

/// Convert an existing MCP image/resource or a complete native fs/output read.
/// Incomplete chunks must first be assembled by the authenticated caller.
pub(super) async fn publish_native_content(
    owner: &str,
    public_base: &str,
    native: &Value,
    filename: Option<&str>,
    ttl_secs: Option<u64>,
) -> Result<Published, String> {
    let (bytes, mime) = native_payload(native)?;
    publish_bytes(owner, public_base, bytes, &mime, filename, ttl_secs).await
}

fn native_payload(native: &Value) -> Result<(Vec<u8>, String), String> {
    if native["isError"] == true {
        return Err("cannot publish a failed native result".into());
    }
    let content = if native["type"] == "image" || native["type"] == "resource" {
        Some(native)
    } else {
        let binary: Vec<_> = native
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "image" || item["type"] == "resource")
            .collect();
        match binary.as_slice() {
            [] => None,
            [one] => Some(*one),
            _ => return Err("select one native image or resource to publish".into()),
        }
    };
    if let Some(content) = content {
        let (encoded, mime) = if content["type"] == "image" {
            let mime = content["mimeType"]
                .as_str()
                .ok_or("native image has no MIME type")?;
            (
                content["data"]
                    .as_str()
                    .ok_or("native image has no base64 data")?,
                mime,
            )
        } else {
            let resource = content
                .get("resource")
                .ok_or("native resource is missing its contents")?;
            (
                resource["blob"]
                    .as_str()
                    .ok_or("native resource has no binary blob")?,
                resource["mimeType"]
                    .as_str()
                    .unwrap_or("application/octet-stream"),
            )
        };
        return Ok((decode(encoded)?, mime.into()));
    }
    let value = native.get("structuredContent").unwrap_or(native);
    if value["offset"].as_u64() != Some(0)
        || value["eof"].as_bool() != Some(true)
        || value
            .get("complete")
            .is_some_and(|complete| complete.as_bool() != Some(true))
        || value
            .get("truncated")
            .is_some_and(|truncated| truncated.as_bool() != Some(false))
    {
        return Err("publish a complete, untruncated native read from offset zero".into());
    }
    let bytes = decode(
        value["data_base64"]
            .as_str()
            .ok_or("native read has no data_base64")?,
    )?;
    if value["size"].as_u64() != Some(bytes.len() as u64)
        || value["next_offset"].as_u64() != Some(bytes.len() as u64)
    {
        return Err("native read byte count does not match its complete payload".into());
    }
    let mime = value
        .get("mime_type")
        .or_else(|| value.get("mimeType"))
        .and_then(Value::as_str)
        .or_else(|| crate::output::image_mime_type(&bytes))
        .unwrap_or("application/octet-stream")
        .to_owned();
    Ok((bytes, mime))
}

fn decode(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.len() > MAX_FILE_BYTES.div_ceil(3) * 4 {
        return Err("native payload exceeds the temporary file limit".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "native payload is invalid base64")?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err("native payload exceeds the temporary file limit".into());
    }
    Ok(bytes)
}

pub(super) fn cleanup(owner: &str, id: &str) -> Result<bool, String> {
    registry().cleanup(owner, id)
}

/// Merge into the public router before `.with_state(broker)`.
/// Explicit HEAD dispatch prevents a metadata request from consuming a link.
pub(super) fn routes<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route(ROUTE, get(download).head(head))
}

async fn download(Path(id): Path<String>, headers: HeaderMap) -> Response {
    if headers.contains_key(header::RANGE) {
        return empty_response(StatusCode::RANGE_NOT_SATISFIABLE);
    }
    match registry().take(&id, Instant::now()) {
        Some(entry) => file_response(entry.metadata, Body::from(entry.bytes)),
        None => empty_response(StatusCode::NOT_FOUND),
    }
}

async fn head(Path(id): Path<String>) -> Response {
    match registry().head(&id, Instant::now()) {
        Some(metadata) => file_response(metadata, Body::empty()),
        None => empty_response(StatusCode::NOT_FOUND),
    }
}

fn safe_headers(headers: &mut HeaderMap) {
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store, no-transform"),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'",
        ),
    );
}

fn empty_response(status: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    safe_headers(response.headers_mut());
    response
}

fn file_response(metadata: Metadata, body: Body) -> Response {
    let mut response = Response::new(body);
    let headers = response.headers_mut();
    safe_headers(headers);
    headers.insert(header::CONTENT_TYPE, metadata.mime);
    headers.insert(header::CONTENT_DISPOSITION, metadata.disposition);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.length.to_string()).expect("byte count header"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "github:fixture-owner";
    const ORIGIN: &str = "https://ssmcp.example";

    fn publish(registry: &Registry, data: &[u8]) -> Published {
        registry
            .publish(
                OWNER,
                ORIGIN,
                data.into(),
                "application/octet-stream",
                Some("file.bin"),
                120,
            )
            .unwrap()
    }

    #[test]
    fn urls_use_configured_origin_and_unpredictable_distinct_identifiers() {
        let registry = Registry::new(4, 1024);
        let one = publish(&registry, b"first");
        let two = publish(&registry, b"second");
        assert!(valid_id(&one.id));
        assert_ne!(one.id, two.id);
        assert_eq!(one.url, format!("{ORIGIN}/_ssmcp/temporary/{}", one.id));
        assert!(one.single_use);
        assert!(one.content().get("data").is_none());
        for invalid in [
            "file:///tmp",
            "https://user:pass@example.com",
            "https://example.com/path",
            "https://example.com?host=evil",
        ] {
            assert!(link_url(invalid, &one.id).is_err());
        }
    }

    #[test]
    fn head_is_repeatable_and_first_get_consumes_atomically() {
        let registry = Registry::new(4, 1024);
        let link = publish(&registry, b"payload");
        for _ in 0..3 {
            assert_eq!(registry.head(&link.id, Instant::now()).unwrap().length, 7);
        }
        let entry = registry.take(&link.id, Instant::now()).unwrap();
        assert_eq!(&entry.bytes[..], b"payload");
        assert!(registry.head(&link.id, Instant::now()).is_none());
        assert!(registry.take(&link.id, Instant::now()).is_none());
    }

    #[test]
    fn cleanup_requires_owner_and_expiry_removes_payloads() {
        let registry = Registry::new(4, 1024);
        let link = publish(&registry, b"payload");
        assert!(registry.cleanup("github:other", &link.id).is_err());
        assert!(registry.head(&link.id, Instant::now()).is_some());
        assert!(registry.cleanup(OWNER, &link.id).unwrap());
        assert!(!registry.cleanup(OWNER, &link.id).unwrap());
        let expired = publish(&registry, b"expiry");
        registry.next_expiry(Instant::now() + Duration::from_secs(121));
        assert!(registry.head(&expired.id, Instant::now()).is_none());
        assert_eq!(registry.quota.usage.lock().unwrap().bytes, 0);
    }

    #[test]
    fn authenticated_owner_keys_preserve_the_bridge_namespace() {
        let registry = Registry::new(4, 1024);
        let owner = concat!("identity\0github\0", "42");
        let link = registry
            .publish(owner, ORIGIN, b"image".to_vec(), "text/plain", None, 1)
            .unwrap();
        assert!(
            registry
                .cleanup(concat!("identity\0github\0", "43"), &link.id)
                .is_err()
        );
        assert!(!link.url.contains(owner));
        assert!(registry.cleanup(owner, &link.id).unwrap());
    }

    #[test]
    fn in_flight_body_and_its_clones_keep_the_memory_budget_reserved() {
        let registry = Registry::new(1, 7);
        let link = publish(&registry, b"payload");
        let entry = registry.take(&link.id, Instant::now()).unwrap();
        let retained = entry.bytes.clone();
        drop(entry);
        assert!(
            registry
                .publish(OWNER, ORIGIN, b"next".to_vec(), "text/plain", None, 1)
                .is_err()
        );
        drop(retained);
        assert_eq!(registry.quota.usage.lock().unwrap().bytes, 0);
        assert!(
            registry
                .publish(OWNER, ORIGIN, b"next".to_vec(), "text/plain", None, 1)
                .is_ok()
        );
    }

    #[test]
    fn global_object_and_byte_limits_and_validation_fail_without_residue() {
        let registry = Registry::new(2, 8);
        let one = publish(&registry, b"12345");
        assert!(
            registry
                .publish(OWNER, ORIGIN, b"6789".to_vec(), "text/plain", None, 1)
                .is_err()
        );
        assert!(
            registry
                .publish("", ORIGIN, vec![1], "text/plain", None, 1)
                .is_err()
        );
        assert!(
            registry
                .publish(OWNER, ORIGIN, vec![1], "text/plain", None, 0)
                .is_err()
        );
        assert!(
            registry
                .publish(OWNER, ORIGIN, vec![1], "text/plain", None, MAX_TTL_SECS + 1)
                .is_err()
        );
        assert!(
            registry
                .publish(
                    OWNER,
                    "https://user@example.com",
                    vec![1],
                    "text/plain",
                    None,
                    1
                )
                .is_err()
        );
        assert_eq!(registry.quota.usage.lock().unwrap().objects, 1);
        registry.cleanup(OWNER, &one.id).unwrap();
        let zero_one = publish(&registry, b"");
        let _zero_two = publish(&registry, b"");
        assert!(
            registry
                .publish(OWNER, ORIGIN, vec![], "text/plain", None, 1)
                .is_err()
        );
        registry.cleanup(OWNER, &zero_one.id).unwrap();
    }

    #[test]
    fn image_mime_and_attachment_headers_are_safe() {
        let registry = Registry::new(4, 1024);
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let link = registry
            .publish(
                OWNER,
                ORIGIN,
                png.to_vec(),
                "image/png",
                Some("../图像.png"),
                1,
            )
            .unwrap();
        let metadata = registry.head(&link.id, Instant::now()).unwrap();
        assert_eq!(metadata.mime, "image/png");
        assert!(
            metadata
                .disposition
                .to_str()
                .unwrap()
                .starts_with("inline;")
        );
        assert!(
            metadata
                .disposition
                .to_str()
                .unwrap()
                .contains("filename*=UTF-8''%E5%9B%BE%E5%83%8F.png")
        );
        let response = file_response(metadata, Body::empty());
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
            png.len().to_string()
        );
        assert!(
            registry
                .publish(OWNER, ORIGIN, b"<html>".to_vec(), "image/png", None, 1)
                .is_err()
        );
        assert!(
            registry
                .publish(OWNER, ORIGIN, vec![], "text/plain\r\nX-Test: yes", None, 1)
                .is_err()
        );
        assert!(
            registry
                .publish(OWNER, ORIGIN, vec![], "text/plain", Some("bad\nname"), 1)
                .is_err()
        );
        let html = registry
            .publish(OWNER, ORIGIN, b"<script/>".to_vec(), "text/html", None, 1)
            .unwrap();
        assert!(
            registry
                .head(&html.id, Instant::now())
                .unwrap()
                .disposition
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
    }

    #[test]
    fn native_images_resources_and_complete_artifacts_decode_without_inline_output() {
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let image = json!({"content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(png)}]});
        let (data, mime) = native_payload(&image).unwrap();
        assert_eq!(data, png.to_vec());
        assert_eq!(mime, "image/png");
        let resource = json!({"type":"resource","resource":{"mimeType":"application/pdf","blob":STANDARD.encode(b"%PDF-fixture")}});
        assert_eq!(native_payload(&resource).unwrap().1, "application/pdf");
        let artifact = json!({"structuredContent":{"offset":0,"next_offset":3,"size":3,"eof":true,"complete":true,"truncated":false,"data_base64":STANDARD.encode(b"abc")}});
        assert_eq!(native_payload(&artifact).unwrap().0, b"abc".to_vec());
        for (field, value) in [
            ("offset", json!(1)),
            ("eof", json!(false)),
            ("complete", json!(false)),
            ("truncated", json!(true)),
            ("next_offset", json!(2)),
            ("size", json!(4)),
        ] {
            let mut invalid = artifact.clone();
            invalid["structuredContent"][field] = value;
            assert!(native_payload(&invalid).is_err(), "accepted {field}");
        }
    }

    fn image_arguments(id: &str) -> Arguments {
        parse(json!({"action":"create","client_id":id,"path":"private/fixture.png"})).unwrap()
    }

    #[test]
    fn tool_schema_and_arguments_expose_only_authorized_image_and_cleanup_operations() {
        let id = "a".repeat(64);
        let valid = json!({"action":"create","client_id":id,"path":"image.png","ttl_secs":30});
        assert!(parse(valid.clone()).is_ok());
        assert!(parse(json!({"action":"cleanup","id":id})).is_ok());
        for (key, value) in [
            ("owner", json!("another-owner")),
            ("access_token", json!("untrusted")),
            ("url", json!("https://elsewhere.example/image.png")),
            ("client_id", json!("fixture")),
            ("path", json!("")),
            ("path", json!("bad\npath")),
            ("ttl_secs", json!(0)),
            ("ttl_secs", json!(MAX_TTL_SECS + 1)),
            ("ttl_secs", json!(null)),
            ("ttl_secs", json!("30")),
            ("ttl_secs", json!(1.5)),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            assert!(parse(invalid).is_err(), "accepted {key}");
        }
        assert!(parse(json!({"action":"cleanup","id":id,"path":"image.png"})).is_err());
        assert!(parse(json!({"action":"cleanup","id":"missing"})).is_err());
        assert!(parse(json!({"action":"fetch","url":"https://elsewhere.example"})).is_err());
        let schema = tool_definition();
        assert_eq!(schema["name"], "link");
        assert_eq!(schema["inputSchema"]["oneOf"].as_array().unwrap().len(), 2);
        for branch in schema["inputSchema"]["oneOf"].as_array().unwrap() {
            assert_eq!(branch["additionalProperties"], false);
            assert!(branch["properties"].get("owner").is_none());
            assert!(branch["properties"].get("access_token").is_none());
            assert!(branch["properties"].get("url").is_none());
        }
    }

    #[tokio::test]
    async fn tool_requires_a_live_authenticated_grant_and_an_authorized_client() {
        let fixture = crate::broker::session_io::TestFixture::new().await;
        let user = crate::broker::IDENTITY_RESOURCE;
        let denied = execute(
            fixture.broker.clone(),
            user.into(),
            "invalid-credential".into(),
            image_arguments(fixture.session.client_id()),
        )
        .await;
        assert!(denied.unwrap_err().contains("authorization"));
        let unavailable = execute(
            fixture.broker.clone(),
            user.into(),
            fixture.access_token.clone(),
            image_arguments(&"b".repeat(64)),
        )
        .await;
        assert!(unavailable.unwrap_err().contains("unavailable"));
    }

    #[tokio::test]
    async fn authenticated_tool_reads_the_native_image_and_returns_a_link_without_base64() {
        let mut fixture = crate::broker::session_io::TestFixture::new().await;
        let user = crate::broker::IDENTITY_RESOURCE;
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let encoded = STANDARD.encode(png);
        let call = execute(
            fixture.broker.clone(),
            user.into(),
            fixture.access_token.clone(),
            image_arguments(fixture.session.client_id()),
        );
        let reply = fixture.native_reply(json!({
            "isError":false,
            "content":[{"type":"image","mimeType":"image/png","data":encoded}],
            "structuredContent":{"path":"private/fixture.png","size":png.len(),"mime_type":"image/png"}
        }));
        let (result, operation) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(call, reply) })
                .await
                .unwrap();
        assert_eq!(
            operation,
            serde_json::to_value(Operation::CallTool {
                name: "ssmcp.fs.read_image".into(),
                arguments: json!({"path":"private/fixture.png"}),
            })
            .unwrap()
        );
        let result = result.unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "resource_link");
        assert_eq!(
            result["structuredContent"]["client_id"],
            fixture.session.client_id()
        );
        assert_eq!(result["structuredContent"]["mime_type"], "image/png");
        assert_eq!(result["structuredContent"]["bytes"], png.len());
        assert!(
            result["structuredContent"]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://ssmcp.test/_ssmcp/temporary/")
        );
        let serialized = result.to_string();
        assert!(!serialized.contains(&encoded));
        assert!(!serialized.contains("private/fixture.png"));
        assert!(!serialized.contains("data_base64"));
        assert!(!serialized.contains(&fixture.access_token));
        let id = result["structuredContent"]["id"].as_str().unwrap();
        assert_eq!(
            registry().head(id, Instant::now()).unwrap().length,
            png.len()
        );
        let removed = execute(
            fixture.broker.clone(),
            user.into(),
            fixture.access_token.clone(),
            parse(json!({"action":"cleanup","id":id})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(removed["structuredContent"]["removed"], true);
        assert!(registry().head(id, Instant::now()).is_none());
        let repeated = execute(
            fixture.broker.clone(),
            user.into(),
            fixture.access_token.clone(),
            parse(json!({"action":"cleanup","id":id})).unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(repeated["structuredContent"]["removed"], false);
    }

    #[tokio::test]
    async fn image_read_cannot_publish_after_its_client_permission_is_revoked() {
        let mut fixture = crate::broker::session_io::TestFixture::new().await;
        let call = execute(
            fixture.broker.clone(),
            crate::broker::IDENTITY_RESOURCE.into(),
            fixture.access_token.clone(),
            image_arguments(fixture.session.client_id()),
        );
        let reply = async {
            let operation = fixture.native_reply(json!({
                "content":[{"type":"image","mimeType":"image/png","data":STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture")}]
            })).await;
            // This branch clears the ACL in the same poll that sends the native
            // response, before the waiting execute future can resume.
            fixture.revoke_access();
            operation
        };
        let (result, _) =
            tokio::time::timeout(Duration::from_secs(2), async { tokio::join!(call, reply) })
                .await
                .unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ttl_worker_releases_expired_entries_without_http_traffic() {
        let registry = Arc::new(Registry::new(4, 1024));
        let link = publish(&registry, b"expires");
        registry
            .entries
            .lock()
            .unwrap()
            .get_mut(&link.id)
            .unwrap()
            .expires = Instant::now();
        registry.start_reaper().unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !registry.entries.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(registry.quota.usage.lock().unwrap().bytes, 0);
    }

    #[tokio::test]
    async fn real_http_head_range_and_competing_gets_preserve_single_use_contract() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, routes()).await.unwrap();
        });
        let link = publish_bytes(
            "github:http-fixture",
            &origin,
            b"real-http-payload".to_vec(),
            "application/octet-stream",
            Some("fixture.bin"),
            None,
        )
        .await
        .unwrap();
        let client = crate::http_client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        for _ in 0..2 {
            let response = client.head(&link.url).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_LENGTH], "17");
            assert!(response.bytes().await.unwrap().is_empty());
        }
        let range = client
            .get(&link.url)
            .header(header::RANGE, "bytes=0-3")
            .send()
            .await
            .unwrap();
        assert_eq!(range.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        let (first, second) =
            tokio::join!(client.get(&link.url).send(), client.get(&link.url).send(),);
        let first = first.unwrap();
        let second = second.unwrap();
        let (success, missing) = if first.status() == StatusCode::OK {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(success.status(), StatusCode::OK);
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(&success.bytes().await.unwrap()[..], b"real-http-payload");
        assert_eq!(
            client.head(&link.url).send().await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
        server.abort();
        let _ = server.await;
    }
}
