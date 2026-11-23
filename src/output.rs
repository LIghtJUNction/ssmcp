//! Bounded, connection-owned output held only in this process's memory.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    sync::{Arc, Mutex, OnceLock, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const INLINE_BYTES: usize = 32 * 1024;
pub const MAX_IMAGE_BYTES: usize = 400 * 1024;
pub const MAX_INLINE_IMAGE_RESULT_BYTES: usize = 1024 * 1024;
pub const INLINE_LINES: u64 = 500;
pub const RETENTION_SECS: u64 = 15 * 60;
pub const RETAINED_BYTES: u64 = 32 * 1024 * 1024;
pub const ARTIFACT_BYTES: u64 = 8 * 1024 * 1024;
pub const READ_BYTES: usize = 32 * 1024;
const MAX_ARTIFACTS: usize = 512;
const MAX_CAPTURES: usize = 512;
const SCHEMA: &str = "ssmcp.output.v1";

#[derive(Clone)]
struct Policy {
    inline_bytes: usize,
    inline_lines: u64,
    retention_secs: u64,
    retained_bytes: u64,
    artifact_bytes: u64,
    max_artifacts: usize,
    max_captures: usize,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            inline_bytes: INLINE_BYTES,
            inline_lines: INLINE_LINES,
            retention_secs: RETENTION_SECS,
            retained_bytes: RETAINED_BYTES,
            artifact_bytes: ARTIFACT_BYTES,
            max_artifacts: MAX_ARTIFACTS,
            max_captures: MAX_CAPTURES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub schema: String,
    pub id: String,
    pub path: Option<String>,
    pub storage: String,
    pub scope: String,
    /// Digest of exactly the retained prefix described by `bytes`.
    pub sha256: String,
    pub sha256_scope: String,
    pub bytes: u64,
    pub lines: u64,
    pub observed_bytes: u64,
    pub complete: bool,
    pub expires_at_unix: u64,
    pub retention_secs: u64,
    pub max_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Entry {
    data: Vec<u8>,
    metadata: ArtifactMetadata,
    active: bool,
    order: u64,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    // Includes small inline captures as well as entries exposed through IDs.
    used_bytes: u64,
    captures: usize,
    sequence: u64,
}

impl State {
    fn remove(&mut self, id: &str) -> Option<Entry> {
        let entry = self.entries.remove(id)?;
        self.used_bytes = self.used_bytes.saturating_sub(entry.data.len() as u64);
        Some(entry)
    }

    fn expire(&mut self, current: u64) -> (usize, u64) {
        let ids: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.active && entry.metadata.expires_at_unix <= current)
            .map(|(id, _)| id.clone())
            .collect();
        let mut bytes = 0;
        for id in &ids {
            if let Some(entry) = self.remove(id) {
                bytes += entry.data.len() as u64;
            }
        }
        (ids.len(), bytes)
    }

    fn evict_oldest(&mut self) -> bool {
        let id = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.active)
            .min_by_key(|(_, entry)| entry.order)
            .map(|(id, _)| id.clone());
        id.is_some_and(|id| self.remove(&id).is_some())
    }

    fn room(&mut self, requested: usize, policy: &Policy) -> usize {
        self.expire(now());
        while requested as u64 > policy.retained_bytes.saturating_sub(self.used_bytes)
            && self.evict_oldest()
        {}
        requested.min(policy.retained_bytes.saturating_sub(self.used_bytes) as usize)
    }
}

pub struct OutputStore {
    policy: Policy,
    state: Mutex<State>,
    observer: Mutex<Option<Weak<crate::observer::Pool>>>,
}

impl OutputStore {
    pub(crate) fn set_observer(&self, observer: &Arc<crate::observer::Pool>) {
        *self
            .observer
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::downgrade(observer));
    }

    pub(crate) fn observer(&self) -> Option<Arc<crate::observer::Pool>> {
        self.observer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(Weak::upgrade)
    }

    pub fn global() -> Result<Arc<Self>, String> {
        static STORE: OnceLock<Arc<OutputStore>> = OnceLock::new();
        Ok(Arc::clone(
            STORE.get_or_init(|| Self::with_policy(Policy::default())),
        ))
    }

    /// A new connection always starts empty. No disk state or reload grants are read.
    pub fn new_connection() -> Result<Arc<Self>, String> {
        Ok(Self::with_policy(Policy::default()))
    }

    pub(crate) fn start_connection() -> crate::Result<Arc<Self>> {
        Ok(Self::with_policy(Policy::default()))
    }

    fn with_policy(policy: Policy) -> Arc<Self> {
        let store = Arc::new(Self {
            policy,
            state: Mutex::new(State::default()),
            observer: Mutex::new(None),
        });
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&store);
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let Some(store) = weak.upgrade() else {
                        break;
                    };
                    let _ = store.cleanup();
                }
            });
        }
        store
    }

    #[cfg(test)]
    pub(crate) fn at_for_test(_directory: std::path::PathBuf) -> Result<Arc<Self>, String> {
        Self::new_connection()
    }

    /// Hold the real memory-store lock inside an owned fixture thread.
    #[cfg(all(test, unix))]
    pub(crate) fn with_state_lock_for_test(&self, work: impl FnOnce()) {
        let _state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        work();
    }

    pub fn capture(self: &Arc<Self>, scope: impl Into<String>) -> Capture {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let registered = state.captures < self.policy.max_captures;
        if registered {
            state.captures += 1;
        }
        Capture {
            store: Arc::clone(self),
            scope: bounded(scope.into(), 1024),
            inline: Vec::new(),
            artifact: None,
            digest: Sha256::new(),
            bytes: 0,
            newlines: 0,
            last: None,
            observed_bytes: 0,
            finished: false,
            failure: (!registered).then(|| "too many retained output capture handles".into()),
            modified: now(),
            registered,
        }
    }

    pub fn cleanup(&self) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let (removed, bytes) = state.expire(now());
        Ok(
            json!({"storage":"memory","removed_artifacts":removed,"removed_bytes":bytes,
            "retained_bytes":state.used_bytes,"retained_artifacts":state.entries.len(),
            "active_artifacts":state.entries.values().filter(|entry| entry.active).count(),
            "retention_secs":self.policy.retention_secs,"max_retained_bytes":self.policy.retained_bytes,
            "max_artifacts":self.policy.max_artifacts}),
        )
    }

    pub fn tools() -> Vec<Value> {
        vec![
            json!({"name":"ssmcp.output.read","description":"Read a bounded chunk of this connection's in-memory output by ID. Continue with next_offset; data_base64 preserves binary bytes. Memory outputs expire and are cleared by exit or reload.","inputSchema":{"type":"object","properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"},"offset":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":READ_BYTES,"default":16384}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
            json!({"name":"ssmcp.output.stat","description":"Inspect this connection's retained in-memory byte snapshot, digest, completion and truncation state. path is null; use output.read with its ID.","inputSchema":{"type":"object","properties":{"id":{"type":"string","pattern":"^[a-f0-9]{32}$"}},"required":["id"],"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false}}),
            json!({"name":"ssmcp.output.cleanup","description":"Release expired completed in-memory outputs. Active captures are preserved within the shared hard memory limit. This operation never reads or deletes files.","inputSchema":{"type":"object","properties":{},"additionalProperties":false},"annotations":{"readOnlyHint":false,"destructiveHint":true,"idempotentHint":true}}),
        ]
    }

    pub async fn perform(self: &Arc<Self>, name: &str, arguments: Value) -> Result<Value, String> {
        self.perform_sync(name, arguments)
    }

    fn perform_sync(&self, name: &str, arguments: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Id {
            id: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ReadArgs {
            id: String,
            #[serde(default)]
            offset: u64,
            #[serde(default = "default_read")]
            max_bytes: usize,
        }
        fn default_read() -> usize {
            16 * 1024
        }
        if name == "ssmcp.output.cleanup" {
            if arguments.as_object().is_none_or(|args| !args.is_empty()) {
                return Err("cleanup accepts an empty arguments object".into());
            }
            return self.cleanup();
        }
        match name {
            "ssmcp.output.read" => {
                let args: ReadArgs = serde_json::from_value(arguments).map_err(error)?;
                validate_id(&args.id)?;
                if !(1..=READ_BYTES).contains(&args.max_bytes) {
                    return Err(format!("max_bytes must be 1..={READ_BYTES}"));
                }
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.expire(now());
                let entry = state.entries.get(&args.id).ok_or(
                    "output is unavailable in this connection; it may have expired or been evicted",
                )?;
                let size = entry.data.len() as u64;
                if args.offset > size {
                    return Err("offset is beyond the retained output".into());
                }
                let start = args.offset as usize;
                let length = args.max_bytes.min(entry.data.len() - start);
                let data = &entry.data[start..start + length];
                let next = args.offset + length as u64;
                Ok(
                    json!({"id":args.id,"path":null,"storage":"memory","offset":args.offset,
                    "next_offset":next,"bytes":length,"size":size,"eof":next == size,
                    "complete":entry.metadata.complete,"observed_bytes":entry.metadata.observed_bytes,
                    "truncated":entry.metadata.observed_bytes > size || entry.metadata.error.is_some(),
                    "data_base64":STANDARD.encode(data),"text":String::from_utf8_lossy(data),
                    "sha256":crate::hex::encode(Sha256::digest(data)),"sha256_scope":"returned_chunk"}),
                )
            }
            "ssmcp.output.stat" => {
                let args: Id = serde_json::from_value(arguments).map_err(error)?;
                validate_id(&args.id)?;
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.expire(now());
                let entry = state.entries.get(&args.id).ok_or(
                    "output is unavailable in this connection; it may have expired or been evicted",
                )?;
                serde_json::to_value(&entry.metadata).map_err(error)
            }
            _ => Err("unknown output operation".into()),
        }
    }

    pub fn guard_result(self: &Arc<Self>, value: Value) -> Value {
        if inline_image_result(&value, &self.policy) {
            return value;
        }
        let mut capture = self.capture("native_tool_result_json");
        if has_long_lines(&value, self.policy.inline_lines) {
            let _ = capture.spill();
        }
        let encoded = serde_json::to_writer(CaptureWriter(&mut capture), &value);
        if encoded.is_ok() && !capture.is_spilled() {
            return value;
        }
        let _ = capture.finish();
        let mut summary = json!({"spilled":capture.is_spilled(),"format":"mcp_tool_result_json","artifact":capture.metadata()});
        if let Some(error) = capture.error() {
            summary["output_error"] = json!(error);
        }
        if let Some(source) = value.get("structuredContent").and_then(Value::as_object) {
            for key in [
                "exit_code",
                "exitCode",
                "status",
                "running",
                "job_id",
                "jobId",
                "session_id",
                "sessionId",
                "id",
                "timed_out",
                "cancelled",
                "next_cursor",
                "output_eof",
            ] {
                if let Some(field) = source.get(key)
                    && (field.is_null()
                        || field.is_boolean()
                        || field.is_number()
                        || field.as_str().is_some_and(|s| s.len() <= 256))
                {
                    summary[key] = field.clone();
                }
            }
        }
        json!({"isError":value.get("isError").and_then(Value::as_bool).unwrap_or(false) || capture.error().is_some(),
            "content":[{"type":"text","text":summary.to_string()}],"structuredContent":summary})
    }
}

/// Identify supported raster headers; this does not decode the image.
pub(crate) fn image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

/// Validate bounded canonical base64 and matching raster headers before forwarding.
pub(crate) fn validate_image_content(value: &Value) -> Result<usize, String> {
    if value["type"] != "image" {
        return Err("image content must have type=image".into());
    }
    let mime = value["mimeType"]
        .as_str()
        .filter(|mime| {
            matches!(
                *mime,
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        })
        .ok_or("image MIME type must be image/png, image/jpeg, image/gif or image/webp")?;
    let data = value["data"].as_str().ok_or("image data must be base64")?;
    if data.is_empty() || data.len() > MAX_IMAGE_BYTES.div_ceil(3) * 4 {
        return Err("image data exceeds 400 KiB or is empty".into());
    }
    let decoded = STANDARD
        .decode(data)
        .map_err(|_| "image data must be canonical base64")?;
    if decoded.len() > MAX_IMAGE_BYTES || STANDARD.encode(&decoded) != data {
        return Err("image data must be canonical base64 within 400 KiB".into());
    }
    if image_mime_type(&decoded) != Some(mime) {
        return Err("image bytes do not match the declared raster MIME type".into());
    }
    Ok(decoded.len())
}

fn inline_image_result(value: &Value, policy: &Policy) -> bool {
    let Some(content) = value.get("content").and_then(Value::as_array) else {
        return false;
    };
    let images: Vec<_> = content
        .iter()
        .filter(|item| item["type"] == "image")
        .collect();
    if images.is_empty()
        || images.len() > 4
        || images
            .iter()
            .any(|image| validate_image_content(image).is_err())
        || value.to_string().len() > MAX_INLINE_IMAGE_RESULT_BYTES
    {
        return false;
    }
    // Only binary image payloads get a larger allowance. Large text, duplicate
    // base64 in structuredContent and oversized metadata still use artifacts.
    let mut metadata = value.clone();
    for item in metadata["content"].as_array_mut().unwrap() {
        if item["type"] == "image" {
            item["data"] = Value::Null;
        }
    }
    metadata.to_string().len() <= policy.inline_bytes
        && !has_long_lines(&metadata, policy.inline_lines)
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(error)?;
    Ok(crate::hex::encode(bytes))
}

pub struct Capture {
    store: Arc<OutputStore>,
    scope: String,
    inline: Vec<u8>,
    artifact: Option<String>,
    digest: Sha256,
    bytes: u64,
    newlines: u64,
    last: Option<u8>,
    observed_bytes: u64,
    finished: bool,
    failure: Option<String>,
    modified: u64,
    registered: bool,
}

impl Capture {
    pub fn is_spilled(&self) -> bool {
        self.artifact.is_some()
    }
    pub fn inline_bytes(&self) -> &[u8] {
        &self.inline
    }
    pub fn error(&self) -> Option<&str> {
        self.failure.as_deref()
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn lines(&self) -> u64 {
        self.newlines + u64::from(self.last.is_some_and(|byte| byte != b'\n'))
    }

    pub(crate) fn read_range(&self, offset: u64, limit: usize) -> Result<Vec<u8>, String> {
        if !(1..=READ_BYTES).contains(&limit) {
            return Err(format!("max_bytes must be 1..={READ_BYTES}"));
        }
        if offset > self.bytes {
            return Err("output cursor is ahead of the captured stream".into());
        }
        let length = limit.min((self.bytes - offset) as usize);
        let start = offset as usize;
        if let Some(id) = &self.artifact {
            let mut state = self
                .store
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.expire(now());
            let entry = state
                .entries
                .get(id)
                .ok_or("captured output is no longer retained in memory")?;
            entry
                .data
                .get(start..start + length)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "retained output is shorter than its recorded length".into())
        } else {
            self.inline
                .get(start..start + length)
                .map(<[u8]>::to_vec)
                .ok_or_else(|| "captured output is shorter than its recorded length".into())
        }
    }

    fn snapshot(&self) -> Option<ArtifactMetadata> {
        Some(ArtifactMetadata {
            schema: SCHEMA.into(),
            id: self.artifact.clone()?,
            path: None,
            storage: "memory".into(),
            scope: self.scope.clone(),
            sha256: crate::hex::encode(self.digest.clone().finalize()),
            sha256_scope: "first_bytes_snapshot".into(),
            bytes: self.bytes,
            lines: self.lines(),
            observed_bytes: self.observed_bytes,
            complete: self.finished && self.failure.is_none(),
            expires_at_unix: self
                .modified
                .saturating_add(self.store.policy.retention_secs),
            retention_secs: self.store.policy.retention_secs,
            max_bytes: self.store.policy.artifact_bytes,
            error: self.failure.clone(),
        })
    }

    pub fn metadata(&self) -> Option<ArtifactMetadata> {
        let mut metadata = self.snapshot()?;
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.expire(now());
        if !state.entries.contains_key(&metadata.id) {
            metadata.complete = false;
            metadata.error = Some("output is no longer retained in memory".into());
        }
        Some(metadata)
    }

    fn sync_locked(&self, state: &mut State) {
        if let Some(metadata) = self.snapshot()
            && let Some(entry) = state.entries.get_mut(&metadata.id)
        {
            entry.metadata = metadata;
            entry.active = !self.finished;
        }
    }

    fn sync(&self) {
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.sync_locked(&mut state);
    }

    fn set_failure(&mut self, reason: impl Into<String>) {
        if self.failure.is_none() {
            self.failure = Some(bounded(reason.into(), 1024));
        }
    }

    pub fn fail(&mut self, reason: impl Into<String>) {
        self.set_failure(reason);
        self.sync();
    }

    pub fn spill(&mut self) -> Result<(), String> {
        if self.artifact.is_some() {
            return Ok(());
        }
        if self.finished {
            return Err("output capture is already complete".into());
        }
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let store = Arc::clone(&self.store);
        let mut state = store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.expire(now());
        while state.entries.len() >= store.policy.max_artifacts && state.evict_oldest() {}
        if state.entries.len() >= store.policy.max_artifacts {
            self.set_failure("in-memory output entry limit reached; active captures are preserved");
            return Err(self.failure.clone().unwrap());
        }
        let id = (0..8)
            .map(|_| random_id())
            .find_map(|id| match id {
                Ok(id) if !state.entries.contains_key(&id) => Some(Ok(id)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .unwrap_or_else(|| Err("could not allocate a unique output ID".into()));
        let id = match id {
            Ok(id) => id,
            Err(error) => {
                self.set_failure(error.clone());
                return Err(error);
            }
        };
        self.artifact = Some(id.clone());
        state.sequence = state.sequence.wrapping_add(1);
        let order = state.sequence;
        state.entries.insert(
            id,
            Entry {
                data: std::mem::take(&mut self.inline),
                metadata: self.snapshot().expect("assigned output ID"),
                active: true,
                order,
            },
        );
        Ok(())
    }

    pub fn push(&mut self, data: &[u8]) -> Result<(), String> {
        if self.finished {
            return Err("output capture is already complete".into());
        }
        self.observed_bytes = self.observed_bytes.saturating_add(data.len() as u64);
        if let Some(error) = &self.failure {
            let error = error.clone();
            self.sync();
            return Err(error);
        }
        let newlines = self
            .newlines
            .saturating_add(data.iter().filter(|byte| **byte == b'\n').count() as u64);
        let last = data.last().copied().or(self.last);
        let lines = newlines.saturating_add(u64::from(last.is_some_and(|byte| byte != b'\n')));
        if !self.is_spilled()
            && (self.bytes.saturating_add(data.len() as u64)
                > self.store.policy.inline_bytes as u64
                || lines > self.store.policy.inline_lines)
        {
            self.spill()?;
        }
        self.append(data)
    }

    fn append(&mut self, data: &[u8]) -> Result<(), String> {
        let store = Arc::clone(&self.store);
        let mut state = store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let requested = data
            .len()
            .min(store.policy.artifact_bytes.saturating_sub(self.bytes) as usize);
        let permitted = state.room(requested, &store.policy);
        let target = if let Some(id) = &self.artifact {
            &mut state
                .entries
                .get_mut(id)
                .expect("active capture stays retained")
                .data
        } else {
            &mut self.inline
        };
        if let Err(error) = target.try_reserve_exact(permitted) {
            self.set_failure(format!("could not allocate bounded output memory: {error}"));
            self.sync_locked(&mut state);
            return Err(self.failure.clone().unwrap());
        }
        target.extend_from_slice(&data[..permitted]);
        state.used_bytes += permitted as u64;
        self.digest.update(&data[..permitted]);
        self.bytes += permitted as u64;
        self.newlines += data[..permitted]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count() as u64;
        self.last = data.get(permitted.wrapping_sub(1)).copied().or(self.last);
        self.modified = now();
        if permitted < data.len() {
            self.set_failure(if requested < data.len() && permitted == requested {
                format!(
                    "in-memory output reached its {} byte limit; retained prefix is partial",
                    store.policy.artifact_bytes
                )
            } else {
                format!(
                    "shared output memory reached its {} byte limit; retained prefix is partial",
                    store.policy.retained_bytes
                )
            });
        }
        self.sync_locked(&mut state);
        self.failure.clone().map_or(Ok(()), Err)
    }

    pub fn finish(&mut self) -> Result<(), String> {
        if self.finished {
            return self.failure.clone().map_or(Ok(()), Err);
        }
        self.inline.shrink_to_fit();
        self.modified = now();
        self.finished = true;
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.sequence = state.sequence.wrapping_add(1);
        let order = state.sequence;
        if let Some(id) = &self.artifact
            && let Some(entry) = state.entries.get_mut(id)
        {
            entry.data.shrink_to_fit();
            entry.order = order;
        }
        self.sync_locked(&mut state);
        self.failure.clone().map_or(Ok(()), Err)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.finished && self.is_spilled() {
            self.set_failure("capture ended before completion");
            let _ = self.finish();
        }
        let mut state = self
            .store
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.artifact.is_none() {
            state.used_bytes = state.used_bytes.saturating_sub(self.inline.len() as u64);
        }
        if self.registered {
            state.captures = state.captures.saturating_sub(1);
        }
    }
}

struct CaptureWriter<'a>(&'a mut Capture);
impl Write for CaptureWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.push(bytes).map_err(io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn has_long_lines(value: &Value, limit: u64) -> bool {
    match value {
        Value::String(text) => {
            text.bytes()
                .filter(|byte| *byte == b'\n')
                .take(limit as usize + 1)
                .count() as u64
                + u64::from(!text.is_empty() && !text.ends_with('\n'))
                > limit
        }
        Value::Array(values) => values.iter().any(|value| has_long_lines(value, limit)),
        Value::Object(values) => values.values().any(|value| has_long_lines(value, limit)),
        _ => false,
    }
}
fn validate_id(id: &str) -> Result<(), String> {
    if id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("output id must be 32 lowercase hexadecimal characters".into())
    }
}

fn bounded(mut text: String, length: usize) -> String {
    if text.len() > length {
        let mut end = length;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        store: Arc<OutputStore>,
    }
    impl Fixture {
        fn new(policy: Policy) -> Self {
            Self {
                store: OutputStore::with_policy(policy),
            }
        }
    }
    fn tiny() -> Policy {
        Policy {
            inline_bytes: 4,
            inline_lines: 2,
            retained_bytes: 24,
            artifact_bytes: 16,
            max_artifacts: 4,
            max_captures: 8,
            ..Policy::default()
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires two live dropped-UID processes driven by test_multi_user_runtime.py"]
    fn per_user_output_fixture() {
        use std::io::{BufRead, Read, Write};
        use std::os::unix::fs::MetadataExt;

        fn command() -> Value {
            let mut line = String::new();
            let bytes = io::stdin().lock().take(1025).read_line(&mut line).unwrap();
            assert!(bytes > 0 && bytes <= 1024 && line.ends_with('\n'));
            serde_json::from_str(&line).unwrap()
        }
        fn report(frame: Value) {
            println!("\nSSMCP_MULTI_USER_OUTPUT {frame}");
            io::stdout().flush().unwrap();
        }

        assert_eq!(
            std::env::var("SSMCP_TEST_OUTPUT_PROTOCOL").expect("run test_multi_user_runtime.py"),
            "1"
        );
        let uid = unsafe { libc::geteuid() };
        let pid = std::process::id();
        assert_ne!(uid, 0, "fixture must drop privileges");
        if let Some(path) = std::env::var_os("SSMCP_TEST_OUTPUT_FOREIGN_ROOT") {
            let path = std::path::PathBuf::from(path);
            assert_eq!(path, std::env::temp_dir().join(format!("ssmcp-{uid}")));
            let metadata = std::fs::metadata(&path).unwrap();
            assert!(metadata.is_dir());
            assert_ne!(metadata.uid(), uid);
            assert_eq!(metadata.mode() & 0o7777, 0o700);
            assert_eq!(
                std::fs::read_dir(&path).err().unwrap().kind(),
                io::ErrorKind::PermissionDenied
            );
        }

        let store = OutputStore::new_connection().unwrap();
        let original: Vec<u8> = (0..READ_BYTES * 2 + 137)
            .map(|index| (index as u8).wrapping_add(uid as u8))
            .collect();
        let mut capture = store.capture(format!("multi-user-output:{uid}"));
        capture.push(&original).unwrap();
        capture.finish().unwrap();
        let metadata = capture.metadata().unwrap();
        let id = metadata.id.as_str();
        assert_eq!(metadata.storage, "memory");
        assert!(metadata.path.is_none() && metadata.error.is_none());
        assert!(metadata.complete);
        assert_eq!(metadata.bytes, original.len() as u64);
        assert_eq!(metadata.observed_bytes, metadata.bytes);
        let stat = store
            .perform_sync("ssmcp.output.stat", json!({"id":id}))
            .unwrap();
        assert_eq!(stat, serde_json::to_value(&metadata).unwrap());
        let mut restored = Vec::new();
        let mut offset = 0;
        while offset < metadata.bytes {
            let page = store
                .perform_sync(
                    "ssmcp.output.read",
                    json!({"id":id,"offset":offset,"max_bytes":READ_BYTES}),
                )
                .unwrap();
            let bytes = STANDARD
                .decode(page["data_base64"].as_str().unwrap())
                .unwrap();
            let next = page["next_offset"].as_u64().unwrap();
            assert_eq!(page["id"], id);
            assert_eq!(page["storage"], "memory");
            assert!(page["path"].is_null());
            assert_eq!(page["offset"], offset);
            assert_eq!(page["bytes"], bytes.len());
            assert_eq!(page["size"], metadata.bytes);
            assert_eq!(page["observed_bytes"], metadata.bytes);
            assert_eq!(page["complete"], true);
            assert_eq!(page["truncated"], false);
            assert!(!bytes.is_empty() && bytes.len() <= READ_BYTES);
            assert_eq!(next, offset + bytes.len() as u64);
            assert!(next <= metadata.bytes);
            assert_eq!(page["eof"], next == metadata.bytes);
            assert_eq!(page["sha256"], crate::hex::encode(Sha256::digest(&bytes)));
            restored.extend_from_slice(&bytes);
            offset = next;
        }
        assert_eq!(restored, original);
        assert_eq!(
            crate::hex::encode(Sha256::digest(&restored)),
            metadata.sha256
        );
        let replacement = OutputStore::new_connection().unwrap();
        for name in ["ssmcp.output.read", "ssmcp.output.stat"] {
            assert!(
                replacement
                    .perform_sync(name, json!({"id":id}))
                    .unwrap_err()
                    .contains("output is unavailable in this connection")
            );
        }
        report(json!({"phase":"ready","uid":uid,"pid":pid,"artifact":&metadata}));

        let request = command();
        let foreign = request["foreign_id"].as_str().unwrap();
        assert_eq!(
            request,
            json!({"phase":"verify","id":id,"foreign_id":foreign})
        );
        validate_id(foreign).unwrap();
        assert_ne!(foreign, id);
        for name in ["ssmcp.output.read", "ssmcp.output.stat"] {
            assert!(
                store
                    .perform_sync(name, json!({"id":foreign}))
                    .unwrap_err()
                    .contains("output is unavailable in this connection")
            );
        }
        report(json!({"phase":"verified","uid":uid,"pid":pid,"id":id,"foreign_id":foreign}));

        // Neither process releases its real store until both peers have verified.
        assert_eq!(command(), json!({"phase":"release","id":id}));
        assert_eq!(
            store
                .perform_sync("ssmcp.output.stat", json!({"id":id}))
                .unwrap(),
            stat
        );
        assert_eq!(capture.metadata().unwrap().id, id);
        report(json!({"phase":"complete","uid":uid,"pid":pid,"id":id,"foreign_id":foreign}));
    }

    #[test]
    fn short_binary_output_stays_inline_and_releases_its_memory() {
        let fixture = Fixture::new(tiny());
        let mut capture = fixture.store.capture("inline");
        capture.push(b"a\0\n").unwrap();
        assert_eq!(capture.bytes(), 3);
        assert_eq!(capture.lines(), 1);
        assert_eq!(capture.read_range(0, 10).unwrap(), b"a\0\n");
        assert!(!capture.is_spilled());
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 3);
        capture.finish().unwrap();
        drop(capture);
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 0);
    }

    #[test]
    fn spill_moves_prefix_without_double_charging_and_hashes_exact_bytes() {
        let fixture = Fixture::new(tiny());
        let mut capture = fixture.store.capture("bytes");
        capture.push(b"abc").unwrap();
        capture.push(b"\0\xff\nxyz").unwrap();
        capture.finish().unwrap();
        assert!(capture.inline_bytes().is_empty());
        let metadata = capture.metadata().unwrap();
        assert!(metadata.path.is_none());
        assert_eq!(metadata.storage, "memory");
        assert_eq!(metadata.bytes, 9);
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 9);
        assert_eq!(
            metadata.sha256,
            crate::hex::encode(Sha256::digest(b"abc\0\xff\nxyz"))
        );
        assert_eq!(capture.read_range(2, 5).unwrap(), b"c\0\xff\nx");
        assert!(metadata.complete);
    }

    #[test]
    fn line_limit_retains_small_results_by_id() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 100,
            inline_lines: 2,
            ..tiny()
        });
        let mut capture = fixture.store.capture("lines");
        capture.push(b"a\nb\nc").unwrap();
        assert!(capture.is_spilled());
        assert_eq!(capture.lines(), 3);
    }

    #[test]
    fn hard_cap_preserves_prefix_and_keeps_observed_bytes_truthful() {
        let fixture = Fixture::new(tiny());
        let mut capture = fixture.store.capture("limited");
        assert!(capture.push(b"0123456789abcdefghij").is_err());
        assert!(capture.push(b"more").is_err());
        assert!(capture.finish().is_err());
        let metadata = capture.metadata().unwrap();
        assert_eq!(metadata.bytes, 16);
        assert_eq!(metadata.observed_bytes, 24);
        assert!(!metadata.complete);
        assert_eq!(capture.read_range(0, 32).unwrap(), b"0123456789abcdef");
        assert_eq!(
            metadata.sha256,
            crate::hex::encode(Sha256::digest(b"0123456789abcdef"))
        );
        let read = fixture
            .store
            .perform_sync("ssmcp.output.read", json!({"id":metadata.id}))
            .unwrap();
        assert_eq!(read["next_offset"], 16);
        assert_eq!(read["eof"], true);
        assert_eq!(read["truncated"], true);
    }

    #[test]
    fn active_captures_share_a_hard_budget_including_inline_bytes() {
        let fixture = Fixture::new(Policy {
            retained_bytes: 10,
            ..tiny()
        });
        let mut first = fixture.store.capture("first");
        first.push(b"abc").unwrap();
        let mut second = fixture.store.capture("second");
        assert!(second.push(b"123456789").is_err());
        assert_eq!(second.bytes(), 7);
        assert_eq!(first.read_range(0, 10).unwrap(), b"abc");
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 10);
        first.spill().unwrap();
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 10);
        fixture.store.cleanup().unwrap();
        assert_eq!(first.read_range(0, 10).unwrap(), b"abc");
    }

    #[test]
    fn completed_entries_are_evicted_before_live_output_is_truncated() {
        let fixture = Fixture::new(Policy {
            retained_bytes: 10,
            ..tiny()
        });
        let mut old = fixture.store.capture("old");
        old.push(b"12345678").unwrap();
        old.finish().unwrap();
        let id = old.metadata().unwrap().id;
        let mut live = fixture.store.capture("live");
        live.push(b"abcdefghi").unwrap();
        assert_eq!(fixture.store.state.lock().unwrap().used_bytes, 9);
        assert!(
            fixture
                .store
                .perform_sync("ssmcp.output.stat", json!({"id":id}))
                .is_err()
        );
        assert!(old.read_range(0, 1).is_err());
        assert!(!old.metadata().unwrap().complete);
        assert_eq!(live.read_range(0, 32).unwrap(), b"abcdefghi");
    }

    #[test]
    fn entry_and_capture_counts_are_bounded_even_for_empty_streams() {
        let fixture = Fixture::new(Policy {
            max_artifacts: 1,
            max_captures: 2,
            ..tiny()
        });
        let mut first = fixture.store.capture("first");
        first.spill().unwrap();
        let mut second = fixture.store.capture("second");
        assert!(second.spill().is_err());
        let rejected = fixture.store.capture("third");
        assert!(rejected.error().is_some());
        assert_eq!(fixture.store.state.lock().unwrap().entries.len(), 1);
        drop(rejected);
        drop(second);
        drop(first);
        let mut next = fixture.store.capture("next");
        next.spill().unwrap();
        assert_eq!(fixture.store.state.lock().unwrap().entries.len(), 1);
    }

    #[test]
    fn expired_outputs_fail_instead_of_returning_a_successful_eof() {
        let fixture = Fixture::new(tiny());
        let mut capture = fixture.store.capture("expiry");
        capture.push(b"content").unwrap();
        let id = capture.metadata().unwrap().id;
        fixture
            .store
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&id)
            .unwrap()
            .metadata
            .expires_at_unix = 0;
        assert_eq!(fixture.store.cleanup().unwrap()["removed_artifacts"], 0);
        capture.finish().unwrap();
        fixture
            .store
            .state
            .lock()
            .unwrap()
            .entries
            .get_mut(&id)
            .unwrap()
            .metadata
            .expires_at_unix = 0;
        assert_eq!(fixture.store.cleanup().unwrap()["removed_artifacts"], 1);
        assert!(capture.read_range(capture.bytes(), 1).is_err());
        assert!(
            fixture
                .store
                .perform_sync("ssmcp.output.read", json!({"id":id}))
                .is_err()
        );
    }

    #[test]
    fn connections_and_replacements_cannot_read_previous_outputs() {
        let first = Fixture::new(tiny());
        let replacement = Fixture::new(tiny());
        let mut capture = first.store.capture("private");
        capture.push(b"private output").unwrap();
        capture.finish().unwrap();
        let id = capture.metadata().unwrap().id;
        assert!(
            replacement
                .store
                .perform_sync("ssmcp.output.stat", json!({"id":id}))
                .is_err()
        );
        replacement.store.cleanup().unwrap();
        assert_eq!(capture.read_range(0, 32).unwrap(), b"private output");
        assert!(
            first
                .store
                .perform_sync("ssmcp.output.read", json!({"id":"../path"}))
                .is_err()
        );
    }

    #[test]
    fn paginated_binary_reads_reconstruct_exact_bytes_and_validate_cursors() {
        let fixture = Fixture::new(tiny());
        let data = b"a\0\xffb\nc\nend";
        let mut capture = fixture.store.capture("binary");
        capture.push(data).unwrap();
        capture.finish().unwrap();
        let id = capture.metadata().unwrap().id;
        let mut offset = 0;
        let mut restored = Vec::new();
        loop {
            let chunk = fixture
                .store
                .perform_sync(
                    "ssmcp.output.read",
                    json!({"id":id,"offset":offset,"max_bytes":3}),
                )
                .unwrap();
            let bytes = STANDARD
                .decode(chunk["data_base64"].as_str().unwrap())
                .unwrap();
            assert_eq!(chunk["sha256"], crate::hex::encode(Sha256::digest(&bytes)));
            restored.extend(bytes);
            offset = chunk["next_offset"].as_u64().unwrap();
            if chunk["eof"] == true {
                break;
            }
        }
        assert_eq!(restored, data);
        assert_eq!(offset, data.len() as u64);
        assert!(
            fixture
                .store
                .perform_sync("ssmcp.output.read", json!({"id":id,"offset":offset+1}))
                .is_err()
        );
        assert!(
            fixture
                .store
                .perform_sync(
                    "ssmcp.output.read",
                    json!({"id":id,"max_bytes":READ_BYTES+1})
                )
                .is_err()
        );
    }

    #[test]
    fn output_store_never_creates_a_supplied_filesystem_location() {
        let location =
            std::env::temp_dir().join(format!("ssmcp-ram-output-{}", random_id().unwrap()));
        assert!(!location.exists());
        let store = OutputStore::at_for_test(location.clone()).unwrap();
        let mut capture = store.capture("no disk");
        capture.spill().unwrap();
        capture.push(b"live data").unwrap();
        capture.finish().unwrap();
        let metadata = capture.metadata().unwrap();
        assert!(metadata.path.is_none());
        store.cleanup().unwrap();
        drop(capture);
        drop(store);
        assert!(!location.exists());
    }

    #[test]
    fn dropped_writers_release_active_status_and_keep_incomplete_metadata() {
        let fixture = Fixture::new(tiny());
        let mut capture = fixture.store.capture("abandoned");
        capture.push(b"unfinished").unwrap();
        let id = capture.metadata().unwrap().id;
        drop(capture);
        let stat = fixture
            .store
            .perform_sync("ssmcp.output.stat", json!({"id":id}))
            .unwrap();
        assert_eq!(stat["complete"], false);
        assert!(
            stat["error"]
                .as_str()
                .unwrap()
                .contains("before completion")
        );
        assert_eq!(fixture.store.cleanup().unwrap()["active_artifacts"], 0);
    }

    #[test]
    fn screenshots_keep_binary_content_above_the_text_budget() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 192,
            ..Policy::default()
        });
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII=";
        let native = json!({
            "isError":false,
            "content":[{"type":"image","mimeType":"image/png","data":png}],
            "structuredContent":{"width":1,"height":1}
        });
        assert!(native.to_string().len() > 192);
        assert_eq!(fixture.store.guard_result(native.clone()), native);
        let mut invalid = native.clone();
        invalid["content"][0]["mimeType"] = "image/jpeg".into();
        assert!(validate_image_content(&invalid["content"][0]).is_err());
        assert_eq!(
            fixture.store.guard_result(invalid)["structuredContent"]["spilled"],
            true
        );
        let mut large_text = native;
        large_text["structuredContent"]["text"] = "x".repeat(256).into();
        assert_eq!(
            fixture.store.guard_result(large_text)["structuredContent"]["spilled"],
            true
        );
    }

    #[test]
    fn gif_and_webp_images_remain_inline_above_the_text_budget() {
        let fixture = Fixture::new(Policy::default());
        // Header fixtures exercise the envelope, not complete codec decoding.
        for (mime, header) in [
            ("image/gif", b"GIF87a".as_slice()),
            ("image/webp", b"RIFF\0\0\0\0WEBP".as_slice()),
        ] {
            let mut bytes = vec![0; INLINE_BYTES + 1];
            bytes[..header.len()].copy_from_slice(header);
            let native = json!({
                "isError":false,
                "content":[{"type":"image","mimeType":mime,"data":STANDARD.encode(&bytes)}],
                "structuredContent":{"size":bytes.len(),"mime_type":mime}
            });
            assert_eq!(
                validate_image_content(&native["content"][0]).unwrap(),
                bytes.len()
            );
            assert_eq!(fixture.store.guard_result(native.clone()), native);
        }
    }

    #[test]
    fn image_payload_validation_bounds_decode_and_rejects_mismatched_types() {
        for image in [
            json!({"type":"image","mimeType":"image/png","data":"not base64"}),
            json!({"type":"image","mimeType":"image/gif","data":STANDARD.encode(b"GIF88a")}),
            json!({"type":"image","mimeType":"image/webp","data":STANDARD.encode(b"RIFF\0\0\0\0WAVE")}),
            json!({"type":"image","mimeType":"image/webp","data":STANDARD.encode(b"RIFF\0\0\0\0WEB")}),
            json!({"type":"image","mimeType":"image/png","data":STANDARD.encode(b"GIF89a")}),
            json!({"type":"image","mimeType":"image/jpeg","data":"YWJj"}),
            json!({"type":"image","mimeType":"image/svg+xml","data":"PHN2Zy8+"}),
            json!({"type":"image","mimeType":"image/png","data":""}),
            json!({"type":"image","mimeType":"image/png","data":"A".repeat(MAX_IMAGE_BYTES.div_ceil(3)*4+4)}),
        ] {
            assert!(validate_image_content(&image).is_err());
        }
    }

    #[test]
    fn native_result_guard_preserves_errors_and_execution_status() {
        let fixture = Fixture::new(Policy {
            inline_bytes: 100,
            ..Policy::default()
        });
        let native = json!({"isError":true,"content":[{"type":"text","text":"x".repeat(1024)}],"structuredContent":{"exit_code":17,"job_id":"job-1","stderr":"y".repeat(1024)}});
        let guarded = fixture.store.guard_result(native.clone());
        assert_eq!(guarded["isError"], true);
        assert_eq!(guarded["structuredContent"]["exit_code"], 17);
        assert_eq!(guarded["structuredContent"]["job_id"], "job-1");
        assert!(guarded["structuredContent"]["artifact"]["path"].is_null());
        let id = guarded["structuredContent"]["artifact"]["id"]
            .as_str()
            .unwrap();
        let chunk = fixture
            .store
            .perform_sync("ssmcp.output.read", json!({"id":id}))
            .unwrap();
        let bytes = STANDARD
            .decode(chunk["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), native);
    }
}
