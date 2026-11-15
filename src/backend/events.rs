//! Bounded, cursor-addressed native events, also visible on the local terminal.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
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

#[derive(Default)]
struct Buffer {
    entries: VecDeque<(u64, Value, usize)>,
    bytes: usize,
    next: u64,
    closed: bool,
}

pub struct EventLog {
    label: String,
    buffer: Mutex<Buffer>,
    changed: Notify,
}

impl EventLog {
    pub fn new(label: &str) -> Arc<Self> {
        Arc::new(Self {
            label: label.into(),
            buffer: Mutex::new(Buffer::default()),
            changed: Notify::new(),
        })
    }

    pub fn push(&self, mut event: Value) {
        // Codex command streaming carries bytes as base64. Keep the original event
        // and add readable text so both the local transcript and MCP consumers can inspect it.
        if event["method"] == "command/exec/outputDelta"
            && let Some(encoded) = event["params"]["deltaBase64"].as_str()
            && let Ok(bytes) = STANDARD.decode(encoded)
        {
            event["ssmcp_output_text"] = json!(String::from_utf8_lossy(&bytes));
        }
        let encoded = event.to_string();
        // stderr is the human transcript; stdout is reserved for the endpoint credentials.
        eprintln!("[ssmcp {}] {encoded}", self.label);
        if encoded.len() > MAX_EVENT {
            let mut end = MAX_EVENT / 2;
            while !encoded.is_char_boundary(end) {
                end -= 1;
            }
            event = json!({"type":"ssmcp.event_truncated", "original_bytes":encoded.len(), "preview":&encoded[..end]});
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
        self.changed.notify_waiters();
    }

    pub fn close(&self) {
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
                    return Ok(
                        json!({"events":events,"next_cursor":next,"start_cursor":start,"truncated":args.cursor < start,"closed":buffer.closed}),
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
