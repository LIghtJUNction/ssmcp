//! Bounded, cursor-addressed native events, also visible on the local terminal.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::Notify,
    time::{Instant, timeout_at},
};

const CAPACITY: usize = 8 * 1024 * 1024;
const MAX_EVENT: usize = 256 * 1024;
const MAX_PAGE: usize = 512 * 1024;

/// Applies only to the audit copy, before any terminal, buffer or archive sink.
#[derive(Clone, Copy, Default)]
pub enum AuditMode {
    #[default]
    Redacted,
    MetadataOnly,
}

impl AuditMode {
    fn event(self, event: Value) -> Value {
        let event = match self {
            Self::Redacted => event,
            Self::MetadataOnly => {
                let mut metadata = protocol_metadata(&event, 0);
                metadata["audit_mode"] = json!("metadata_only");
                metadata["original_bytes"] = json!(event.to_string().len());
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    metadata["text_bytes"] = json!(text.len());
                }
                metadata
            }
        };
        crate::audit::event(event)
    }
}

// No arbitrary message, arguments, content, log data or nested result objects.
// Keep protocol envelopes so request/response correlation and native progress
// still work. Text identifiers are bounded at UTF-8 boundaries.
fn protocol_metadata(value: &Value, depth: usize) -> Value {
    let mut clean = scalar_metadata(value);
    for key in ["type", "method", "jsonrpc", "id", "harness_call_id"] {
        if let Some(value) = value.get(key).and_then(identifier) {
            clean.insert(key.into(), value);
        }
    }
    if depth < 2 {
        for key in ["request", "response"] {
            if let Some(value) = value.get(key).filter(|v| v.is_object()) {
                clean.insert(key.into(), protocol_metadata(value, depth + 1));
            }
        }
    }
    if let Some(params) = value.get("params") {
        let mut fields = Map::new();
        if value["method"] == "tools/call"
            && let Some(name) = params.get("name").and_then(identifier)
        {
            fields.insert("name".into(), name);
        }
        if value["method"] == "notifications/progress" {
            for key in ["progress", "total"] {
                if let Some(value) = params.get(key).filter(|v| v.is_number()) {
                    fields.insert(key.into(), value.clone());
                }
            }
            if let Some(id) = params.get("progressToken").and_then(identifier) {
                fields.insert("progressToken".into(), id);
            }
        }
        if let Some(id) = params.get("requestId").and_then(identifier) {
            fields.insert("requestId".into(), id);
        }
        if value["method"] == "notifications/message"
            && let Some(level) = params.get("level").and_then(Value::as_str).filter(|level| {
                matches!(
                    *level,
                    "debug"
                        | "info"
                        | "notice"
                        | "warning"
                        | "error"
                        | "critical"
                        | "alert"
                        | "emergency"
                )
            })
        {
            fields.insert("level".into(), json!(level));
        }
        fields.insert("payload_omitted".into(), json!(true));
        clean.insert("params".into(), Value::Object(fields));
    }
    if let Some(result) = value.get("result") {
        clean.insert("result".into(), Value::Object(scalar_metadata(result)));
    }
    if let Some(error) = value.get("error") {
        let mut fields = Map::new();
        if let Some(code) = error.get("code").filter(|v| v.is_number()) {
            fields.insert("code".into(), code.clone());
        }
        fields.insert("message_omitted".into(), json!(true));
        clean.insert("error".into(), Value::Object(fields));
    }
    Value::Object(clean)
}

fn identifier(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => Some(json!(text.chars().take(256).collect::<String>())),
        Value::Number(_) => Some(value.clone()),
        _ => None,
    }
}

fn scalar_metadata(value: &Value) -> Map<String, Value> {
    let mut clean = Map::new();
    for key in [
        "isError",
        "success",
        "running",
        "cancelled",
        "timed_out",
        "timedOut",
        "complete",
        "internal_discovery",
    ] {
        if let Some(value) = value.get(key).filter(|v| v.is_boolean()) {
            clean.insert(key.into(), value.clone());
        }
    }
    for key in [
        "bytes",
        "size",
        "duration_ms",
        "durationMs",
        "exit_code",
        "exitCode",
    ] {
        if let Some(value) = value.get(key).filter(|v| v.is_number()) {
            clean.insert(key.into(), value.clone());
        }
    }
    for key in ["status", "state", "approval", "approval_status"] {
        if let Some(state) = value.get(key).and_then(Value::as_str).filter(|state| {
            matches!(
                *state,
                "pending"
                    | "running"
                    | "completed"
                    | "failed"
                    | "cancelled"
                    | "approved"
                    | "denied"
                    | "rejected"
                    | "accepted"
                    | "declined"
            )
        }) {
            clean.insert(key.into(), json!(state));
        }
    }
    clean.insert("payload_omitted".into(), json!(true));
    clean
}

#[derive(Default)]
struct Buffer {
    entries: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    next: u64,
    closed: bool,
}

pub struct EventLog {
    label: String,
    audit_mode: AuditMode,
    buffer: Mutex<Buffer>,
    changed: Notify,
    archive: Mutex<Option<crate::output::Capture>>,
}

impl EventLog {
    pub fn new(label: &str) -> Arc<Self> {
        Self::create(
            label,
            crate::output::OutputStore::global().ok(),
            AuditMode::default(),
        )
    }

    pub fn with_output(label: &str, output: Arc<crate::output::OutputStore>) -> Arc<Self> {
        Self::with_output_and_audit(label, output, AuditMode::default())
    }

    pub fn with_output_and_audit(
        label: &str,
        output: Arc<crate::output::OutputStore>,
        audit_mode: AuditMode,
    ) -> Arc<Self> {
        Self::create(label, Some(output), audit_mode)
    }

    fn create(
        label: &str,
        output: Option<Arc<crate::output::OutputStore>>,
        audit_mode: AuditMode,
    ) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            audit_mode,
            buffer: Mutex::new(Buffer::default()),
            changed: Notify::new(),
            archive: Mutex::new(output.map(|store| store.capture(format!("events:{label}:jsonl")))),
        })
    }

    pub fn push(&self, event: Value) {
        let mut event = self.audit_mode.event(event);
        // Serialise persistence, display and sequence assignment for this stream.
        let mut archive = self.archive.lock().unwrap_or_else(|e| e.into_inner());
        crate::client::render::event(&self.label, &event);
        // Codex command streaming carries bytes as base64. Keep the original event
        // and add readable text so both the local transcript and MCP consumers can inspect it.
        if event["method"] == "command/exec/outputDelta"
            && let Some(encoded) = event["params"]["deltaBase64"].as_str()
            && let Ok(bytes) = STANDARD.decode(encoded)
        {
            event["ssmcp_output_text"] = json!(String::from_utf8_lossy(&bytes));
        }
        let encoded = event.to_string();
        if let Some(capture) = archive.as_mut() {
            let _ = capture.push(encoded.as_bytes());
            let _ = capture.push(b"\n");
        }
        let artifact = archive.as_ref().and_then(|capture| capture.metadata());

        if encoded.len() > MAX_EVENT {
            let mut end = MAX_EVENT / 2;
            while !encoded.is_char_boundary(end) {
                end -= 1;
            }
            event = json!({"type":"ssmcp.event_truncated", "original_bytes":encoded.len(), "preview":&encoded[..end], "artifact":artifact, "format":"jsonl"});
        }
        let bytes = event.to_string().len();
        let mut buffer = self.buffer.lock().unwrap();
        while buffer.bytes + bytes > CAPACITY {
            let Some((_, _, size)) = buffer.entries.pop_front() else {
                break;
            };
            buffer.bytes -= size;
        }
        let cursor = buffer.next;
        buffer.next += 1;
        buffer.bytes += bytes;
        buffer.entries.push_back((cursor, event, bytes));
        drop(buffer);
        drop(archive);
        self.changed.notify_waiters();
    }

    pub fn close(&self) {
        if let Some(capture) = self
            .archive
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            let _ = capture.finish();
        }
        crate::client::render::finish(&self.label);
        self.buffer.lock().unwrap().closed = true;
        self.changed.notify_waiters();
    }

    pub async fn read(&self, arguments: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Read {
            #[serde(default)]
            cursor: u64,
            #[serde(default = "default_count")]
            max_events: usize,
            #[serde(default)]
            wait_ms: u64,
        }
        fn default_count() -> usize {
            50
        }
        let args: Read = serde_json::from_value(arguments).map_err(|e| e.to_string())?;
        if !(1..=256).contains(&args.max_events) || args.wait_ms > 5000 {
            return Err("max_events must be 1..=256 and wait_ms must be 0..=5000".into());
        }
        let deadline = Instant::now() + Duration::from_millis(args.wait_ms);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let buffer = self.buffer.lock().unwrap();
                if args.cursor > buffer.next {
                    return Err("cursor is ahead of the event stream".into());
                }
                if args.cursor < buffer.next || buffer.closed || Instant::now() >= deadline {
                    let start = buffer.entries.front().map_or(buffer.next, |e| e.0);
                    let mut next = args.cursor.max(start);
                    let mut bytes = 0;
                    let mut events = Vec::new();
                    for (cursor, event, size) in &buffer.entries {
                        if *cursor < next {
                            continue;
                        }
                        if events.len() >= args.max_events || bytes + size > MAX_PAGE {
                            break;
                        }
                        events.push(json!({"cursor":cursor,"event":event}));
                        bytes += size;
                        next = cursor + 1;
                    }
                    let closed = buffer.closed;
                    drop(buffer);
                    let archive = self.archive.lock().unwrap_or_else(|e| e.into_inner());
                    let artifact = archive.as_ref().and_then(|capture| capture.metadata());
                    return Ok(
                        json!({"events":events,"next_cursor":next,"start_cursor":start,"truncated":args.cursor < start,"closed":closed,"artifact":artifact,"archive_format":"jsonl","archive_error":archive.as_ref().and_then(|capture|capture.error())}),
                    );
                }
            }
            let _ = timeout_at(deadline, changed).await;
        }
    }
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "cursor":{"type":"integer","minimum":0,"default":0},
        "max_events":{"type":"integer","minimum":1,"maximum":256,"default":50},
        "wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":0}
    },"additionalProperties":false})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metadata_only_is_applied_before_terminal_buffer_and_archive_sinks() {
        // This marker deliberately has no sensitive key or redaction pattern.
        const MARKER: &str = "vH8nP4cM9qR2yT6s";
        let output = crate::output::OutputStore::new_connection().unwrap();
        let log = EventLog::with_output_and_audit(
            "metadata-audit-fixture",
            output.clone(),
            AuditMode::MetadataOnly,
        );
        // Exercise the existing artifact writer without emitting hundreds of
        // terminal events. Production spill thresholds and quotas are unchanged.
        log.archive
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .spill()
            .unwrap();
        let values = [
            json!({"type":"ssmcp.request","request":{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"browser_fill_form","arguments":{"fields":[{"value":MARKER}],"text":MARKER}}}}),
            json!({"jsonrpc":"2.0","id":"call-1","result":{"isError":false,"content":[{"type":"text","text":MARKER}],"structuredContent":{"status":MARKER}}}),
            json!({"method":"notifications/progress","params":{"progressToken":"call-1","progress":1.5,"total":4,"message":MARKER}}),
            json!({"method":"notifications/progress","params":{"progressToken":"call-2","progress":MARKER,"total":MARKER,"message":"working"}}),
            json!({"method":"notifications/message","params":{"level":"info","data":{"value":MARKER},"message":MARKER}}),
            json!({"type":"stderr","text":MARKER}),
            json!({"id":"call-1","error":{"code":-32000,"message":MARKER,"data":MARKER}}),
            json!({"id":"review-1","method":"elicitation/create","params":{"message":MARKER,"requestedSchema":{"properties":{"value":{"default":MARKER}}}}}),
        ];
        for value in &values {
            // This is exactly the object passed to the terminal renderer.
            let terminal_event = log.audit_mode.event(value.clone());
            assert!(!terminal_event.to_string().contains(MARKER));
            assert_eq!(terminal_event["original_bytes"], value.to_string().len());
            log.push(value.clone());
        }
        log.close();
        let page = log.read(json!({"max_events":64})).await.unwrap();
        assert!(!page.to_string().contains(MARKER));
        let events = page["events"].as_array().unwrap();
        assert_eq!(events.len(), values.len());
        for (event, source) in events.iter().zip(&values) {
            assert_eq!(event["event"], log.audit_mode.event(source.clone()));
        }
        assert_eq!(
            events[0]["event"]["request"]["params"]["name"],
            "browser_fill_form"
        );
        assert_eq!(events[1]["event"]["result"]["isError"], false);
        assert_eq!(events[2]["event"]["params"]["progress"], 1.5);
        assert_eq!(events[2]["event"]["params"]["total"], 4);
        assert!(events[3]["event"]["params"].get("progress").is_none());
        assert!(events[3]["event"]["params"].get("total").is_none());
        assert_eq!(events[4]["event"]["params"]["level"], "info");
        assert_eq!(events[5]["event"]["text_bytes"], MARKER.len());
        assert_eq!(events[6]["event"]["error"]["code"], -32000);
        let artifact = output
            .perform(
                "ssmcp.output.read",
                json!({
                    "id":page["artifact"]["id"], "max_bytes":16384,
                }),
            )
            .await
            .unwrap();
        assert_eq!(artifact["eof"], true);
        assert_eq!(artifact["complete"], true);
        let text = artifact["text"].as_str().unwrap();
        assert!(!text.contains(MARKER));
        let archived: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(archived.len(), values.len());
        for (event, source) in archived.iter().zip(&values) {
            assert_eq!(*event, log.audit_mode.event(source.clone()));
        }
        // The default policy is not silently changed for other providers.
        assert_eq!(
            AuditMode::default().event(json!({"type":"stderr","text":MARKER}))["text"],
            MARKER
        );
    }

    #[tokio::test]
    async fn cursors_waiters_and_close() {
        let log = EventLog::new("test");
        let waiting = log.clone();
        let task =
            tokio::spawn(async move { waiting.read(json!({"wait_ms":5000})).await.unwrap() });
        tokio::task::yield_now().await;
        log.push(json!({"delta":"hello"}));
        let page = task.await.unwrap();
        assert_eq!(page["next_cursor"], 1);
        assert_eq!(page["events"][0]["event"]["delta"], "hello");
        assert!(log.read(json!({"cursor":2})).await.is_err());
        log.close();
        assert_eq!(
            log.read(json!({"cursor":1,"wait_ms":5000})).await.unwrap()["closed"],
            true
        );
    }
}
