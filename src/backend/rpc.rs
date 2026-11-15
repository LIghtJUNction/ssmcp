//! A small bidirectional JSON-lines peer for native MCP, Codex App Server and Pi.
use super::{events::EventLog, process::Managed};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    process::Stdio,
    sync::{
        Arc, Mutex as SyncMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, Command},
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::timeout,
};

const MAX_NATIVE_FRAME: usize = 16 * 1024 * 1024;
const MAX_CALLS: usize = 8; // Leave bridge slots free for approvals, interrupt and event reads.
const MAX_REQUESTS: usize = 64;
type Response = Result<Value, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    JsonRpc,
    Pi,
}

struct Waiting {
    method: String,
    reply: oneshot::Sender<Response>,
}

pub struct Peer {
    dialect: Dialect,
    writer: Mutex<Option<ChildStdin>>,
    process: Mutex<Managed>,
    reader: SyncMutex<Option<JoinHandle<()>>>,
    calls: SyncMutex<HashMap<String, Waiting>>,
    requests: SyncMutex<HashMap<String, Value>>,
    next: AtomicU64,
    alive: AtomicBool,
    events: Arc<EventLog>,
}

impl Peer {
    pub async fn spawn(
        mut command: Command,
        label: &str,
        dialect: Dialect,
    ) -> crate::Result<Arc<Self>> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut process = Managed::spawn(&mut command)?;
        let writer = process
            .child
            .stdin
            .take()
            .ok_or("backend stdin unavailable")?;
        let mut stdout = BufReader::new(
            process
                .child
                .stdout
                .take()
                .ok_or("backend stdout unavailable")?,
        );
        let peer = Arc::new(Self {
            dialect,
            writer: Mutex::new(Some(writer)),
            process: Mutex::new(process),
            reader: SyncMutex::new(None),
            calls: SyncMutex::new(HashMap::new()),
            requests: SyncMutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            events: EventLog::new(label),
        });
        let weak = Arc::downgrade(&peer);
        let task = tokio::spawn(async move {
            loop {
                let message = read_json(&mut stdout).await;
                let Some(peer) = weak.upgrade() else {
                    break;
                };
                match message {
                    Ok(Some(message)) => {
                        if let Err(error) = peer.receive(message) {
                            peer.fail(&error);
                            break;
                        }
                    }
                    Ok(None) => {
                        peer.fail("native process closed stdout");
                        break;
                    }
                    Err(error) => {
                        peer.fail(&format!("native JSON stream failed: {error}"));
                        break;
                    }
                }
            }
        });
        *peer.reader.lock().unwrap() = Some(task);
        Ok(peer)
    }

    pub fn events(&self) -> Arc<EventLog> {
        self.events.clone()
    }

    pub async fn send(&self, value: Value) -> Result<(), String> {
        if !self.alive.load(Ordering::Acquire) {
            return Err("native process disconnected".into());
        }
        let mut encoded = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        encoded.push(b'\n');
        if encoded.len() > MAX_NATIVE_FRAME {
            return Err("native request exceeds 16 MiB".into());
        }
        timeout(Duration::from_secs(10), async {
            let mut guard = self.writer.lock().await;
            let writer = guard
                .as_mut()
                .ok_or_else(|| io::Error::other("native stdin closed"))?;
            writer.write_all(&encoded).await?;
            writer.flush().await
        })
        .await
        .map_err(|_| "native stdin write timed out".to_string())?
        .map_err(|e| e.to_string())
    }

    pub async fn request(&self, method: &str, params: Value) -> Response {
        let id = format!("ssmcp-{}", self.next.fetch_add(1, Ordering::Relaxed));
        let key = json!(id).to_string();
        let (reply, receiver) = oneshot::channel();
        {
            let mut calls = self.calls.lock().unwrap();
            if calls.len() >= MAX_CALLS {
                return Err("native backend busy; at most 8 outstanding native calls".into());
            }
            calls.insert(
                key.clone(),
                Waiting {
                    method: method.into(),
                    reply,
                },
            );
        }
        struct Remove<'a> {
            calls: &'a SyncMutex<HashMap<String, Waiting>>,
            key: String,
        }
        impl Drop for Remove<'_> {
            fn drop(&mut self) {
                self.calls.lock().unwrap().remove(&self.key);
            }
        }
        let _remove = Remove {
            calls: &self.calls,
            key,
        };
        let message = match self.dialect {
            Dialect::JsonRpc => json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
            Dialect::Pi => {
                let mut arguments = params
                    .as_object()
                    .cloned()
                    .ok_or("Pi arguments must be an object")?;
                if arguments.contains_key("id") || arguments.contains_key("type") {
                    return Err("Pi arguments may not override id or type".into());
                }
                arguments.insert("id".into(), json!(id));
                arguments.insert("type".into(), json!(method));
                Value::Object(arguments)
            }
        };
        if !matches!(method, "initialize" | "tools/list") {
            self.events
                .push(json!({"type":"ssmcp.request","request":message}));
        }
        self.send(message).await?;
        timeout(Duration::from_secs(900), receiver)
            .await
            .map_err(|_| {
                "native request timed out after 15 minutes; its outcome may be unknown".to_string()
            })?
            .map_err(|_| "native response channel closed".to_string())?
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        if self.dialect == Dialect::Pi {
            return Err(
                "use Pi request or respond; arbitrary notifications are not defined".into(),
            );
        }
        self.send(json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    pub async fn pending(&self) -> Value {
        let mut requests: Vec<_> = self.requests.lock().unwrap().values().cloned().collect();
        requests.sort_by_key(|request| request["id"].to_string());
        json!({"requests":requests,"connected":self.alive.load(Ordering::Acquire)})
    }

    pub async fn respond(&self, args: Value) -> Response {
        let object = args.as_object().ok_or("response must be an object")?;
        let id = object
            .get("id")
            .filter(|id| id.is_string() || id.is_i64() || id.is_u64())
            .ok_or("id must identify a pending native request")?;
        let key = id.to_string();
        let request = self
            .requests
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or("no pending native request with this id")?;
        let message = match self.dialect {
            Dialect::JsonRpc => {
                if object
                    .keys()
                    .any(|k| !matches!(k.as_str(), "id" | "result" | "error"))
                    || object.contains_key("result") == object.contains_key("error")
                {
                    return Err("provide id and exactly one of result or error".into());
                }
                if let Some(error) = object.get("error")
                    && (error.get("code").and_then(Value::as_i64).is_none()
                        || error.get("message").and_then(Value::as_str).is_none())
                {
                    return Err("error requires an integer code and string message".into());
                }
                let mut message = object.clone();
                message.insert("jsonrpc".into(), json!("2.0"));
                Value::Object(message)
            }
            Dialect::Pi => {
                let field_count = ["value", "confirmed", "cancelled"]
                    .iter()
                    .filter(|field| object.contains_key(**field))
                    .count();
                if field_count != 1
                    || object
                        .keys()
                        .any(|k| !matches!(k.as_str(), "id" | "value" | "confirmed" | "cancelled"))
                {
                    return Err(
                        "provide id and exactly one of value, confirmed, or cancelled".into(),
                    );
                }
                if object.get("value").is_some_and(|v| !v.is_string())
                    || object.get("confirmed").is_some_and(|v| !v.is_boolean())
                    || object.get("cancelled").is_some_and(|v| v != &json!(true))
                {
                    return Err(
                        "value must be a string, confirmed a boolean, cancelled must be true"
                            .into(),
                    );
                }
                if !object.contains_key("cancelled")
                    && ((request["method"] == "confirm") != object.contains_key("confirmed"))
                {
                    return Err(
                        "confirm requests require confirmed; other dialogs require value".into(),
                    );
                }
                let mut message = object.clone();
                message.insert("type".into(), json!("extension_ui_response"));
                Value::Object(message)
            }
        };
        // Remove before awaiting: simultaneous callers cannot answer the same request twice.
        if self.requests.lock().unwrap().remove(&key).is_none() {
            return Err("native request already answered".into());
        }
        if let Err(error) = self.send(message.clone()).await {
            // Do not resend automatically: a partial write can have an uncertain outcome.
            return Err(format!(
                "native response write failed; outcome may be unknown: {error}"
            ));
        }
        self.events
            .push(json!({"type":"ssmcp.response","response":message}));
        Ok(json!({"sent":true,"id":id}))
    }

    fn receive(&self, message: Value) -> Result<(), String> {
        if !message.is_object() {
            return Err("native message must be an object".into());
        }
        let response = match self.dialect {
            Dialect::JsonRpc => {
                message.get("id").is_some()
                    && (message.get("result").is_some() || message.get("error").is_some())
            }
            Dialect::Pi => message["type"] == "response",
        };
        if response {
            let key = message
                .get("id")
                .ok_or("native response has no id")?
                .to_string();
            let waiting = self.calls.lock().unwrap().remove(&key);
            if let Some(waiting) = waiting {
                if !matches!(waiting.method.as_str(), "initialize" | "tools/list") {
                    self.events.push(message.clone());
                }
                let result = if self.dialect == Dialect::Pi {
                    Ok(message)
                } else if let Some(error) = message.get("error") {
                    Err(format!("native RPC error: {error}"))
                } else {
                    Ok(message["result"].clone())
                };
                let _ = waiting.reply.send(result);
            } else {
                self.events
                    .push(json!({"type":"ssmcp.late_response","response":message}));
            }
            return Ok(());
        }
        let is_request = match self.dialect {
            Dialect::JsonRpc => message.get("id").is_some() && message["method"].is_string(),
            Dialect::Pi => {
                message["type"] == "extension_ui_request"
                    && matches!(
                        message["method"].as_str(),
                        Some("select" | "confirm" | "input" | "editor")
                    )
            }
        };
        if is_request {
            let id = message.get("id").ok_or("native request has no id")?;
            let mut requests = self.requests.lock().unwrap();
            if requests.len() >= MAX_REQUESTS {
                return Err(
                    "too many pending native requests; no request was automatically approved"
                        .into(),
                );
            }
            if requests.insert(id.to_string(), message.clone()).is_some() {
                return Err("duplicate pending native request id".into());
            }
        }
        // Codex explicitly resolves approval/input requests on turn cancellation as well.
        if message["method"] == "serverRequest/resolved"
            && let Some(id) = message["params"].get("requestId")
        {
            self.requests.lock().unwrap().remove(&id.to_string());
        }
        self.events.push(message);
        Ok(())
    }

    fn fail(&self, error: &str) {
        if self.alive.swap(false, Ordering::AcqRel) {
            self.events
                .push(json!({"type":"ssmcp.disconnected","message":error}));
            for (_, waiting) in self.calls.lock().unwrap().drain() {
                let _ = waiting.reply.send(Err(error.into()));
            }
            self.requests.lock().unwrap().clear();
            self.events.close();
        }
    }

    pub async fn shutdown(&self) {
        self.fail("ssmcp disconnected");
        self.writer.lock().await.take();
        self.process.lock().await.shutdown().await;
        let task = self.reader.lock().unwrap().take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = self.reader.get_mut().unwrap().take() {
            task.abort();
        }
    }
}

async fn read_json<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Value>> {
    let mut line = Vec::new();
    loop {
        let bytes = reader.fill_buf().await?;
        if bytes.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete native JSON line",
                ))
            };
        }
        let n = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if line.len() + n > MAX_NATIVE_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "native frame exceeds 16 MiB",
            ));
        }
        let complete = bytes[n - 1] == b'\n';
        line.extend_from_slice(&bytes[..n]);
        reader.consume(n);
        if complete {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn json_lines_preserve_fragmentation_and_reject_truncation() {
        let (mut tx, rx) = tokio::io::duplex(64);
        tokio::spawn(async move {
            tx.write_all(b"{\"id\":1,").await.unwrap();
            tokio::task::yield_now().await;
            tx.write_all(b"\"result\":{}}\n").await.unwrap();
        });
        assert_eq!(
            read_json(&mut BufReader::new(rx)).await.unwrap().unwrap()["id"],
            1
        );
        assert!(read_json(&mut &b"{}"[..]).await.is_err());
    }
}
