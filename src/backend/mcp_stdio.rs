//! A bounded async transport around zmcpio's actual protocol state machine.
//! The owner starts one explicit server; no model calls, callbacks or automatic retries.
use super::process::Managed;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout},
};
use zmcpio::{
    client::{Client as Protocol, Event, Pending},
    json::{Document, Limits as JsonLimits, Token},
    mcp::Era,
    rpc,
};

const MAX_PENDING: usize = 32;
const MAX_TOKENS: usize = 16384;
const MAX_PAGES: usize = 32;
const MAX_TOOLS: usize = 512;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub frame_bytes: usize,
    pub handshake: Duration,
    pub request: Duration,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: 1024 * 1024,
            handshake: Duration::from_secs(15),
            request: Duration::from_secs(45),
        }
    }
}

struct Call {
    name: String,
    arguments: Vec<u8>,
    deadline: Instant,
    reply: oneshot::Sender<Result<Value, String>>,
}

pub(crate) struct McpClient {
    tools: Vec<Value>,
    names: BTreeSet<String>,
    calls: mpsc::Sender<Call>,
    stop: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    limits: Limits,
}

impl McpClient {
    pub(crate) async fn start(command: &mut Command, limits: Limits) -> Result<Arc<Self>, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child =
            Managed::spawn(command).map_err(|e| format!("Cannot start MCP server: {e}"))?;
        let stdin = child.child.stdin.take().ok_or("MCP stdin is unavailable")?;
        let stdout = child
            .child
            .stdout
            .take()
            .ok_or("MCP stdout is unavailable")?;
        let mut stderr = child
            .child
            .stderr
            .take()
            .ok_or("MCP stderr is unavailable")?;
        // Always drain stderr; retain neither arbitrary logs nor server-provided instructions.
        let drain = tokio::spawn(async move {
            let mut bytes = [0u8; 4096];
            while matches!(stderr.read(&mut bytes).await, Ok(n) if n > 0) {}
        });
        let (calls, receiver) = mpsc::channel(MAX_PENDING);
        let (stop, stopped) = watch::channel(false);
        let (ready, initialized) = oneshot::channel();
        let task = tokio::spawn(async move {
            run(stdin, stdout, receiver, stopped, ready, limits).await;
            child.shutdown().await;
            drain.abort();
            let _ = drain.await;
        });
        let tools = match initialized.await {
            Ok(Ok(tools)) => tools,
            result => {
                stop.send_replace(true);
                let _ = task.await;
                return Err(match result {
                    Ok(Err(e)) => e,
                    _ => "MCP startup ended before initialization".into(),
                });
            }
        };
        let names = tools
            .iter()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect();
        Ok(Arc::new(Self {
            tools,
            names,
            calls,
            stop,
            task: Mutex::new(Some(task)),
            limits,
        }))
    }

    pub(crate) fn tools(&self) -> Vec<Value> {
        self.tools.clone()
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.stop.borrow() || self.calls.is_closed()
    }

    pub(crate) async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if !self.names.contains(name) {
            return Err(format!("MCP tool was not advertised: {name}"));
        }
        if !arguments.is_object() {
            return Err("MCP arguments must be an object".into());
        }
        if *self.stop.borrow() {
            return Err("MCP connection is closed".into());
        }
        let arguments = serde_json::to_vec(&arguments).map_err(|e| e.to_string())?;
        if arguments.len() > self.limits.frame_bytes {
            return Err("MCP arguments exceed the frame limit".into());
        }
        let (reply, result) = oneshot::channel();
        self.calls
            .try_send(Call {
                name: name.into(),
                arguments,
                deadline: Instant::now() + self.limits.request,
                reply,
            })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    "MCP request queue is full; the request was not sent".to_owned()
                }
                mpsc::error::TrySendError::Closed(_) => {
                    "MCP connection is closed; the request was not sent".to_owned()
                }
            })?;
        result.await.map_err(|_| "MCP connection ended; the operation may have executed; inspect its state before retrying".to_owned())?
    }

    pub(crate) async fn shutdown(&self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }
}

enum Message {
    Initialized(i64),
    Tools(i64, Vec<Value>, Option<String>),
    Tool(i64, Value),
    Failure(Option<i64>, i32),
    InputRequired(i64),
    Ignore,
    Callback(Vec<u8>),
}

fn decode(
    client: &mut Protocol<'_>,
    bytes: &[u8],
    tokens: &mut [Token],
) -> Result<Message, String> {
    let event = client
        .receive(bytes, tokens, JsonLimits::default())
        .map_err(|e| format!("Invalid MCP response: {e}"))?;
    match event {
        Event::Initialized { id, .. } => Ok(Message::Initialized(id)),
        Event::Tools { id, page } => {
            let tools = page
                .tools()
                .map(|tool| serde_json::from_str(tool.raw().raw()).map_err(|e| e.to_string()))
                .collect::<Result<Vec<Value>, String>>()?;
            let next = page
                .next_cursor()
                .map(|cursor| {
                    let mut text = [0u8; 4096];
                    cursor
                        .decode_into(&mut text)
                        .map(str::to_owned)
                        .map_err(|e| e.to_string())
                })
                .transpose()?;
            Ok(Message::Tools(id, tools, next))
        }
        Event::ToolResult { id, result } => Ok(Message::Tool(
            id,
            serde_json::from_str(result.raw().raw()).map_err(|e| e.to_string())?,
        )),
        Event::Failure { id, error } => Ok(Message::Failure(id, error.code)),
        Event::InputRequired { id, .. } => Ok(Message::InputRequired(id)),
        Event::Notification(notification) => {
            if notification
                .method
                .matches("notifications/tools/list_changed")
            {
                return Err(
                    "MCP tool catalog changed; reconnect explicitly to discover the new catalog"
                        .into(),
                );
            }
            Ok(Message::Ignore)
        }
        Event::ServerRequest(request) => {
            let mut frame = vec![0; 2048];
            let len = rpc::failure(
                Some(request.id),
                -32601,
                "Client callback not supported",
                None,
                &mut frame,
            )
            .map_err(|e| e.to_string())?;
            frame.truncate(len);
            Ok(Message::Callback(frame))
        }
        _ => Err("Unexpected MCP response kind".into()),
    }
}

// The partial frame lives outside select!, so cancellation never loses bytes.
async fn read_frame(
    reader: &mut BufReader<ChildStdout>,
    partial: &mut Vec<u8>,
    max: usize,
) -> Result<Vec<u8>, String> {
    loop {
        let bytes = reader
            .fill_buf()
            .await
            .map_err(|e| format!("MCP read failed: {e}"))?;
        if bytes.is_empty() {
            return Err("MCP server closed its output".into());
        }
        let newline = bytes.iter().position(|b| *b == b'\n');
        let count = newline.map_or(bytes.len(), |index| index + 1);
        if partial.len().saturating_add(count) > max {
            return Err("MCP response exceeds the frame limit".into());
        }
        partial.extend_from_slice(&bytes[..count]);
        reader.consume(count);
        if newline.is_some() {
            let mut frame = std::mem::take(partial);
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(frame);
        }
    }
}

async fn send(stdin: &mut ChildStdin, bytes: &[u8]) -> Result<(), String> {
    timeout(Duration::from_secs(5), async {
        stdin.write_all(bytes).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| "MCP write timed out; request delivery is uncertain".to_owned())?
    .map_err(|e| format!("MCP write failed; request delivery is uncertain: {e}"))
}

async fn reply(
    client: &mut Protocol<'_>,
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    partial: &mut Vec<u8>,
    tokens: &mut [Token],
    max: usize,
) -> Result<Message, String> {
    for _ in 0..128 {
        let frame = read_frame(reader, partial, max).await?;
        match decode(client, &frame, tokens)? {
            Message::Ignore => {}
            Message::Callback(frame) => send(stdin, &frame).await?,
            message => return Ok(message),
        }
    }
    Err("MCP handshake exceeded its message budget".into())
}

async fn initialize(
    client: &mut Protocol<'_>,
    stdin: &mut ChildStdin,
    reader: &mut BufReader<ChildStdout>,
    partial: &mut Vec<u8>,
    tokens: &mut [Token],
    output: &mut [u8],
) -> Result<Vec<Value>, String> {
    let sent = client.initialize(output).map_err(|e| e.to_string())?;
    send(stdin, &output[..sent.len]).await?;
    if !matches!(reply(client, stdin, reader, partial, tokens, output.len()).await?, Message::Initialized(id) if id == sent.id)
    {
        return Err("MCP initialization did not return the expected response".into());
    }
    let length = client.initialized(output).map_err(|e| e.to_string())?;
    send(stdin, &output[..length]).await?;
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    let mut seen_cursors = BTreeSet::new();
    let mut cursor: Option<String> = None;
    let mut catalog_bytes = 0usize;
    for _ in 0..MAX_PAGES {
        let sent = client
            .list_tools(cursor.as_deref(), output)
            .map_err(|e| e.to_string())?;
        send(stdin, &output[..sent.len]).await?;
        let Message::Tools(id, page, next) =
            reply(client, stdin, reader, partial, tokens, output.len()).await?
        else {
            return Err("MCP discovery did not return a tools page".into());
        };
        if id != sent.id {
            return Err("MCP discovery returned a mismatched ID".into());
        }
        for tool in page {
            let name = tool["name"]
                .as_str()
                .ok_or("MCP tool has no name")?
                .to_owned();
            if !names.insert(name) {
                return Err("MCP catalog contains a duplicate tool".into());
            }
            catalog_bytes = catalog_bytes
                .saturating_add(serde_json::to_vec(&tool).map_err(|e| e.to_string())?.len());
            if tools.len() >= MAX_TOOLS || catalog_bytes > 2 * 1024 * 1024 {
                return Err("MCP catalog exceeds its bounded budget".into());
            }
            tools.push(tool);
        }
        match next {
            None => return Ok(tools),
            Some(next) => {
                if !seen_cursors.insert(next.clone()) {
                    return Err("MCP server repeated its discovery cursor".into());
                }
                cursor = Some(next);
            }
        }
    }
    Err("MCP discovery exceeded its page budget".into())
}

async fn run(
    mut stdin: ChildStdin,
    stdout: ChildStdout,
    mut calls: mpsc::Receiver<Call>,
    mut stop: watch::Receiver<bool>,
    ready: oneshot::Sender<Result<Vec<Value>, String>>,
    limits: Limits,
) {
    let mut slots = [Pending::EMPTY; MAX_PENDING];
    let mut client = Protocol::new("ssmcp", env!("CARGO_PKG_VERSION"), Era::Legacy, &mut slots);
    let mut tokens = vec![Token::EMPTY; MAX_TOKENS];
    let mut argument_tokens = vec![Token::EMPTY; MAX_TOKENS];
    let mut output = vec![0u8; limits.frame_bytes];
    let mut reader = BufReader::new(stdout);
    let mut partial = Vec::new();
    let startup = tokio::select! {
        _ = stop.changed() => Err("MCP startup was cancelled".into()),
        result = timeout(limits.handshake, initialize(&mut client, &mut stdin, &mut reader, &mut partial, &mut tokens, &mut output)) => result.unwrap_or_else(|_| Err("MCP handshake timed out".into())),
    };
    match startup {
        Ok(tools) => {
            if ready.send(Ok(tools)).is_err() {
                return;
            }
        }
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    }
    let mut pending = BTreeMap::<i64, Call>::new();
    let mut clock = tokio::time::interval(Duration::from_millis(100));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let ended = loop {
        tokio::select! {
            _ = stop.changed() => break "MCP connection was closed".to_owned(),
            _ = clock.tick() => {
                if pending.values().any(|call| call.deadline <= Instant::now()) {
                    break "MCP request timed out; the operation may have executed; inspect its state before retrying".into();
                }
            },
            request = calls.recv(), if pending.len() < MAX_PENDING => {
                let Some(call) = request else { break "MCP owner disconnected".into(); };
                if call.reply.is_closed() { continue; }
                if call.deadline <= Instant::now() {
                    let _ = call.reply.send(Err("MCP request expired in the queue; it was not sent".into()));
                    continue;
                }
                let sent = Document::parse(&call.arguments, &mut argument_tokens, JsonLimits::default())
                    .and_then(|arguments| client.call_tool(&call.name, Some(arguments.root()), &mut output));
                match sent {
                    Ok(sent) => {
                        pending.insert(sent.id, call);
                        if let Err(error) = send(&mut stdin, &output[..sent.len]).await { break error; }
                    }
                    Err(error) => { let _ = call.reply.send(Err(format!("MCP request was not sent: {error}"))); }
                }
            },
            result = read_frame(&mut reader, &mut partial, limits.frame_bytes) => {
                let frame = match result { Ok(frame) => frame, Err(error) => break error };
                let message = match decode(&mut client, &frame, &mut tokens) { Ok(message) => message, Err(error) => break error };
                match message {
                    Message::Tool(id, value) => {
                        let Some(call) = pending.remove(&id) else { break "MCP replied to an unknown call".into(); };
                        let _ = call.reply.send(Ok(value));
                    },
                    Message::Failure(Some(id), code) => {
                        let Some(call) = pending.remove(&id) else { break "MCP failed an unknown call".into(); };
                        let _ = call.reply.send(Err(format!("MCP request failed with JSON-RPC code {code}")));
                    },
                    Message::InputRequired(id) => {
                        let Some(call) = pending.remove(&id) else { break "MCP requested input for an unknown call".into(); };
                        let _ = call.reply.send(Err("MCP requested an unsupported interactive continuation; the request was not replayed".into()));
                    },
                    Message::Callback(frame) => if let Err(error) = send(&mut stdin, &frame).await { break error; },
                    Message::Ignore => {},
                    _ => break "MCP returned an unexpected response".into(),
                }
            }
        }
    };
    client.close();
    for (_, call) in pending {
        let _ = call.reply.send(Err(format!(
            "{ended}; accepted requests are never replayed automatically"
        )));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
