//! Operator-selected chat ingress and dispatch for one live connection.
mod options;
mod process;
mod transport;

pub(crate) use options::{Agent, Flags, Input, Settings, takes_value};

use super::{
    interface::{history, questions},
    messaging, steering,
};
use crate::runtime::Runtime;
use serde::Serialize;
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

const ACK_LIMIT: Duration = Duration::from_secs(16);
type Outcome = Result<Receipt, String>;
pub(crate) type Ticket = oneshot::Receiver<Outcome>;

#[derive(Debug, Serialize)]
pub(crate) struct Receipt {
    pub(crate) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) warnings: Vec<String>,
    #[serde(skip)]
    pub(crate) display: Option<ProgramDisplay>,
}

#[derive(Debug)]
pub(crate) struct ProgramDisplay {
    pub(crate) history: history::HistoryId,
    pub(crate) request_message: String,
    pub(crate) reply_message: String,
}

impl Receipt {
    fn new(status: &str, request_id: Option<String>) -> Self {
        Self {
            status: status.into(),
            request_id,
            output: None,
            warnings: Vec::new(),
            display: None,
        }
    }
}

struct Request {
    text: String,
    reply: oneshot::Sender<Outcome>,
    authorization: Option<transport::Authorization>,
    owner: Option<history::HistoryOwner>,
}

fn private_file(path: &Path) -> Result<Vec<u8>, String> {
    if !std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()) {
        return Err("chat input configuration requires a private regular file".into());
    }
    super::account::read_private(path)
        .ok()
        .flatten()
        .ok_or_else(|| "could not read the private chat input configuration file".into())
}

#[derive(Clone)]
struct Sender {
    queue: mpsc::Sender<Request>,
    save_exchange: bool,
}

impl Sender {
    fn start(
        &self,
        text: &str,
        authorization: Option<transport::Authorization>,
    ) -> Result<Ticket, String> {
        steering::validate_text(text)?;
        let (reply, ticket) = oneshot::channel();
        let owner = self
            .save_exchange
            .then(|| history::capture(None))
            .transpose()?;
        self.queue
            .try_send(Request {
                text: text.into(),
                reply,
                authorization,
                owner,
            })
            .map_err(|_| "chat input is busy or disconnected; no request was admitted")?;
        Ok(ticket)
    }

    async fn dispatch(
        &self,
        text: &str,
        authorization: Option<transport::Authorization>,
    ) -> Outcome {
        tokio::time::timeout(Duration::from_secs(140), self.start(text, authorization)?)
            .await.map_err(|_| "chat input confirmation timed out; check existing receipts before retrying")?
            .map_err(|_| {
            "chat input disconnected before confirmation; inspect existing receipts before retrying"
        })?
    }
}

static TERMINAL_ROUTE: Mutex<Option<(&'static str, Sender)>> = Mutex::new(None);

pub(crate) fn terminal_submit(text: &str) -> Result<Option<Ticket>, String> {
    let sender = TERMINAL_ROUTE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|(_, sender)| sender.clone());
    sender.map(|sender| sender.start(text, None)).transpose()
}

pub(crate) fn terminal_agent_label() -> Option<&'static str> {
    TERMINAL_ROUTE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .map(|(label, _)| *label)
}

pub(super) struct Session {
    worker: JoinHandle<()>,
    transport: Option<JoinHandle<Result<(), String>>>,
    stop: watch::Sender<bool>,
    owns_terminal: bool,
    graceful: bool,
}

impl Session {
    pub(super) async fn start(settings: &Settings, runtime: Arc<Runtime>) -> Result<Self, String> {
        let agent = process::Target::resolve(&settings.agent)?;
        let transport = transport::Endpoint::open(&settings.input).await?;
        let graceful = matches!(settings.input, Input::Web(_) | Input::McpHttp(_));
        let (tx, mut rx) = mpsc::channel::<Request>(4);
        let sender = Sender {
            queue: tx,
            save_exchange: matches!(&agent, process::Target::Program(_)),
        };
        let (stop, mut cancelled) = watch::channel(false);
        let worker = tokio::spawn(async move {
            loop {
                let Some(mut request) = (tokio::select! {
                    biased;
                    _ = cancelled.changed() => None,
                    request = rx.recv() => request,
                }) else {
                    break;
                };
                if request.reply.is_closed() {
                    continue;
                }
                let result = tokio::select! {
                    biased;
                    _ = cancelled.changed() => break,
                    _ = request.reply.closed() => continue,
                    _ = transport::revoked(request.authorization.as_ref()) => Err("chat input authorization was revoked; no automatic retry".into()),
                    result = route(&agent, &runtime, &request.text, request.owner.take(), request.authorization.as_ref()) => result,
                };
                let _ = request.reply.send(result);
                super::terminal::invalidate();
            }
        });
        let terminal_agent = (settings.input == Input::Terminal)
            .then(|| settings.agent.label())
            .flatten();
        if let Some(label) = terminal_agent {
            *TERMINAL_ROUTE
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some((label, sender.clone()));
        }
        Ok(Self {
            worker,
            transport: transport
                .map(|transport| tokio::spawn(transport.serve(sender, stop.subscribe()))),
            stop,
            owns_terminal: terminal_agent.is_some(),
            graceful,
        })
    }

    pub(super) async fn input_ended(&mut self) -> Result<(), String> {
        let Some(task) = &mut self.transport else {
            return std::future::pending().await;
        };
        let result = task.await;
        self.transport = None;
        result.map_err(|_| "chat input worker stopped")?
    }

    pub(super) async fn shutdown(mut self) {
        self.stop();
        let _ = (&mut self.worker).await;
        if let Some(mut task) = self.transport.take()
            && (!self.graceful
                || tokio::time::timeout(Duration::from_secs(11), &mut task)
                    .await
                    .is_err())
        {
            task.abort();
            let _ = task.await;
        }
    }

    pub(super) fn stop(&self) {
        let _ = self.stop.send(true);
    }
}

pub(super) async fn input_ended(session: &mut Option<Session>) -> Result<(), String> {
    match session {
        Some(session) => session.input_ended().await,
        None => std::future::pending().await,
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.owns_terminal {
            *TERMINAL_ROUTE
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
        let _ = self.stop.send(true);
        if let Some(task) = self.transport.take() {
            task.abort();
        }
        self.worker.abort();
    }
}

async fn route(
    agent: &process::Target,
    runtime: &Runtime,
    text: &str,
    owner: Option<history::HistoryOwner>,
    authorization: Option<&transport::Authorization>,
) -> Outcome {
    admit_dispatch(authorization)?;
    let prepared = if matches!(agent, process::Target::Ssmcp { .. }) {
        // Native inboxes carry the exact literal body, even local reference syntax.
        None
    } else {
        Some(
            tokio::time::timeout(Duration::from_secs(5), runtime.prepare_references(text))
                .await
                .map_err(|_| "chat reference preparation timed out")??,
        )
    };
    // Workspace preparation can outlive the connection or token that admitted it.
    admit_dispatch(authorization)?;
    match agent {
        process::Target::NextCaller => {
            let prepared = prepared.expect("next-caller uses prepared input");
            let warnings = prepared.warnings.clone();
            let id = steering::submit_chat_prepared(prepared)?;
            let deadline = tokio::time::Instant::now() + ACK_LIMIT;
            loop {
                let (state, error) = steering::chat_receipt(&id);
                match state {
                    Some(state @ ("queued" | "delivered")) => {
                        let mut receipt = Receipt::new(state, Some(id));
                        receipt.warnings = warnings;
                        return Ok(receipt);
                    }
                    Some("editing" | "cancelled") => {
                        return Err(error.unwrap_or_else(|| {
                            "request was not published; review its queue entry".into()
                        }));
                    }
                    Some("uncertain" | "offline" | "retarget-required") | None => break,
                    _ if tokio::time::Instant::now() >= deadline => break,
                    _ => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
            let mut receipt = Receipt::new("unconfirmed", Some(id));
            receipt.warnings = warnings;
            Ok(receipt)
        }
        process::Target::Ssmcp { id, account } => {
            let mut bytes = [0; 16];
            crate::platform::random_bytes(&mut bytes).map_err(|error| error.to_string())?;
            let request_id = format!("chat-{}", crate::hex::encode(&bytes));
            // Inboxes keep their existing literal body. No local file contents
            // or skill declarations are sent as implicit attachments.
            messaging::send(id, account, text, &request_id)?;
            questions::user_input(None);
            let deadline = tokio::time::Instant::now() + ACK_LIMIT;
            loop {
                match messaging::chat_receipt(&request_id) {
                    Some((state, error)) => match state.as_str() {
                        "queued" | "included" | "acknowledged" => {
                            return Ok(Receipt::new(&state, Some(request_id)));
                        }
                        "failed" | "expired" => {
                            return Err(error.unwrap_or_else(|| format!("native message {state}")));
                        }
                        _ => {}
                    },
                    None => break,
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok(Receipt::new("unconfirmed", Some(request_id)))
        }
        process::Target::Program(command) => {
            let prepared = prepared.expect("explicit program uses prepared input");
            let owner = owner.ok_or("no original conversation was captured for the chat agent")?;
            let mut random = [0; 16];
            crate::platform::random_bytes(&mut random).map_err(|error| error.to_string())?;
            let request_id = format!("chat-{}", crate::hex::encode(&random));
            let request_message = history::request_identity(&request_id, text)?;
            let history = owner.display_id();
            let user_owner = owner.clone();
            let user_id = request_id.clone();
            let user_text = text.to_owned();
            tokio::task::spawn_blocking(move || {
                history::record_owned_request(&user_owner, &user_id, &user_text)
            })
            .await
            .map_err(|_| "chat request history worker stopped")??;

            // The original request is now retained. Recheck permissions before
            // starting the program in case saving took longer than the lease.
            admit_dispatch(authorization)?;
            questions::user_input(None);
            let mut receipt = Receipt::new("completed", Some(request_id.clone()));
            receipt.warnings = prepared.warnings;
            let output = match command.run(&prepared.text).await {
                Ok(output) => output,
                Err(error) => {
                    receipt.status = "failed".into();
                    receipt.warnings.push(error);
                    return Ok(receipt);
                }
            };
            let retained = retained_reply(&output);
            if retained.trim().is_empty() {
                receipt.status = "unconfirmed".into();
                receipt
                    .warnings
                    .push("the chat agent exited without returning a reply".into());
                return Ok(receipt);
            }
            admit_dispatch(authorization)?;
            let identity = request_id.clone();
            let saved = tokio::task::spawn_blocking(move || {
                history::record_owned_program_reply(&owner, &identity, &retained)
            })
            .await;
            match saved {
                Ok(Ok(reply_message)) => {
                    receipt.display = Some(ProgramDisplay {
                        history,
                        request_message,
                        reply_message,
                    });
                    receipt.output = Some(output);
                }
                _ => {
                    receipt.status = "unconfirmed".into();
                    receipt.warnings.push("the chat agent returned a reply, but the original conversation could not be saved; check history before sending again".into());
                }
            }
            Ok(receipt)
        }
    }
}

fn admit_dispatch(authorization: Option<&transport::Authorization>) -> Result<(), String> {
    steering::require_chat_connection()?;
    if authorization.is_some_and(|authorization| !authorization.current()) {
        return Err("chat input authorization was revoked".into());
    }
    Ok(())
}

fn retained_reply(output: &str) -> String {
    let mut text: String = output
        .chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .collect();
    const MARKER: &str = "\n[Reply shortened for conversation history]";
    const LIMIT: usize = 16 * 1024;
    if text.len() > LIMIT {
        text.truncate(text.floor_char_boundary(LIMIT - MARKER.len()));
        text.push_str(MARKER);
    }
    text
}
