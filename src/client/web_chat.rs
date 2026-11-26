//! A real relay client for the module workbench. This process has no native execution backend.
use super::*;
use crate::wire::Operation;
use serde_json::{Value, json};
use std::sync::OnceLock;

pub(crate) struct Identity {
    pub client_id: String,
    pub generation: u64,
}

static IDENTITY: OnceLock<Identity> = OnceLock::new();

pub(crate) fn identity() -> Option<&'static Identity> {
    IDENTITY.get()
}

const TOOLS: [&str; 4] = [
    "ssmcp.ui.message",
    "ssmcp.ui.reply",
    "ssmcp.ui.wait",
    "ssmcp.ui.get",
];

fn tool_page(cursor: Option<&str>) -> Result<Value, String> {
    let mut tools = interface::tools_for_chat(true);
    tools.retain(|tool| {
        tool["name"]
            .as_str()
            .is_some_and(|name| TOOLS.contains(&name))
    });
    for tool in &mut tools {
        if tool["name"] == "ssmcp.ui.get" {
            tool["description"] = json!("Read this chat session's confirmed relay identity.");
            tool["inputSchema"] = json!({"type":"object","additionalProperties":false,
                "required":["path"],"properties":{"path":{"const":"/identity"}}});
        }
    }
    let descriptors = crate::catalog::compact(tools)?;
    crate::catalog::page(
        &descriptors,
        cursor,
        &crate::catalog::fingerprint(&descriptors),
    )
}

async fn perform(operation: Operation, identity: &Identity) -> Result<Value, String> {
    operation.validate()?;
    let (name, arguments) = match operation {
        Operation::ListTools { cursor } => return tool_page(cursor.as_deref()),
        Operation::CallTool { name, arguments } => (name, arguments),
        _ => return Err("device operations are unavailable in a chat session".into()),
    };
    let value = match name.as_str() {
        "ssmcp.ui.message" => {
            let source = crate::runtime::call_scope::Source::capture();
            tokio::task::spawn_blocking(move || interface::publish_message(arguments, source))
                .await
                .map_err(|_| "message publisher stopped")??
        }
        "ssmcp.ui.reply" => interface::reply::call(arguments)?,
        "ssmcp.ui.wait" => interface::user_wait::call(arguments).await?,
        "ssmcp.ui.get" if arguments == json!({"path":"/identity"}) => {
            json!({"schema_version":1, "path":"/identity", "value":{
                "client_id":identity.client_id, "process_id":std::process::id()}})
        }
        _ => return Err("this chat session only accepts conversation operations".into()),
    };
    Ok(crate::backend::result(value))
}

async fn write_reply(
    writer: &Arc<Mutex<tokio::process::ChildStdin>>,
    frame: Frame,
) -> crate::Result<()> {
    timeout(Duration::from_secs(10), async {
        write_frame(&mut *writer.lock().await, &frame).await
    })
    .await
    .map_err(|_| "chat relay write timed out")??;
    Ok(())
}

async fn connected(
    server: &str,
    identity: &'static Identity,
    incoming: &mut Incoming,
    writer: Arc<Mutex<tokio::process::ChildStdin>>,
) -> crate::Result<()> {
    let mut commands = steering::connect_with_chat(&identity.client_id, VERSION, true);
    let mut poll = tokio::time::interval(Duration::from_secs(1));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tasks = JoinSet::<crate::Result<()>>::new();
    loop {
        tokio::select! {
            _ = poll.tick() => { steering::poll()?; }
            Some(command) = commands.recv() => {
                let steering::Transport::Wire { id, .. } = command else { continue };
                let Some(mut command) = steering::pending_command(id) else { continue };
                if let crate::wire::SteeringCommand::Publish { text, .. } = &command
                    && steering::needs_preparation(id)
                {
                    // This workspace is text-only. A mention is never a local file read.
                    let prepared = super::references::PreparedReferences { text: text.clone(), warnings: vec![] };
                    let Some(plain) = steering::prepare_publish(id, prepared)? else { continue };
                    command = plain;
                }
                if steering::sent(id) {
                    write_reply(&writer, Frame::Steering { id, command }).await?;
                }
            }
            Some(result) = tasks.join_next(), if !tasks.is_empty() => { result??; }
            frame = incoming.frames.recv() => match frame.transpose()?.flatten() {
                None => return Ok(()),
                Some(Frame::Reject { message }) => return Err(message.into()),
                Some(Frame::CodeModeFinished { .. }) => {},
                Some(Frame::SteeringReply { id, result }) => steering::reply(id, result)?,
                Some(Frame::Call { id, operation, attribution, code_context, .. }) => {
                    let scope = (|| -> Result<_, String> {
                        let source = attribution.as_deref().ok_or("missing authenticated controller")?;
                        source.validate()?;
                        if !controller_accounts::observe_verified(server, &source.account, &identity.client_id)
                            .map_err(|e| e.to_string())?.is_allowed()
                        {
                            return Err("controller not admitted to this chat session".into());
                        }
                        crate::runtime::call_scope::CallScope::from_relay_for_client(
                            identity.generation, Some(&identity.client_id), id,
                            Some(source), code_context.as_ref())
                    })();
                    let scope = match scope {
                        Ok(scope) if tasks.len() < MAX_INFLIGHT => scope,
                        Ok(_) => {
                            write_reply(&writer, Frame::Reply { id, result: Err("chat session is busy".into()) }).await?;
                            continue;
                        }
                        Err(error) => {
                            write_reply(&writer, Frame::Reply { id, result: Err(error) }).await?;
                            continue;
                        }
                    };
                    let output = writer.clone();
                    tasks.spawn(async move {
                        if let Operation::CallTool { name, .. } = &operation {
                            scope.set_native_tool(name);
                        }
                        let result = scope.scope(perform(operation, identity)).await;
                        write_reply(&output, Frame::Reply { id, result }).await
                    });
                }
                Some(Frame::RelayMaintenance { id, .. } | Frame::LocalTaskCall { id, .. }) =>
                    write_reply(&writer, Frame::Reply { id, result: Err("unavailable in a chat session".into()) }).await?,
                Some(_) => return Err("unexpected frame on the chat relay".into()),
            }
        }
    }
}

pub(super) async fn run(destination: &str, instance: String) -> crate::Result<()> {
    let (name, server) = crate::server_endpoint::parse_target(destination)?;
    let principal = crate::server_endpoint::allow_principal(&name, &server)?;
    let metadata = crate::wire::ClientMetadata {
        name: "ssmcp-web".into(),
        title: Some("Browser chat".into()),
        cwd: "/".into(),
        hostname: "ssmcp-web".into(),
        allow: vec![principal],
    };
    metadata.validate()?;
    let executable = std::env::current_exe()?;
    let mut registered = false;
    let mut transport = register_transport(
        TransportKind::Relay,
        Some(&server),
        &metadata,
        &mut registered,
        || {
            let mut command = relay::command(&executable, &server);
            command.env(
                "SSMCP_RELAY_CAPABILITIES",
                format!(
                    "{},{}",
                    crate::wire::SESSION_ACCESS_CAPABILITY,
                    crate::wire::CONTROL_ATTRIBUTION_CAPABILITY
                ),
            );
            command
        },
        std::future::pending(),
        Duration::from_secs(20),
    )
    .await?
    .ok_or("chat relay registration cancelled")?;
    transport.require_allow_access(&metadata.allow).await?;
    let client_id = transport.ready.3.clone();
    crate::wire::device_address::decode::<32>(&client_id)?;
    IDENTITY
        .set(Identity {
            client_id,
            generation: 1,
        })
        .map_err(|_| "chat identity already bound")?;
    let identity = IDENTITY.get().expect("chat identity set");
    // Register the local attach endpoint only after the real relay has returned Ready.
    let registration =
        crate::session::overview::Registration::open_with_instance(metadata, Some(instance))
            .await?;
    let mut incoming = Incoming::start(transport.reader, TransportKind::Relay);
    let writer = Arc::new(Mutex::new(transport.writer));
    let outcome = connected(&server, identity, &mut incoming, writer).await;
    steering::disconnect();
    incoming.stop().await;
    registration.shutdown().await;
    let _ = stop_registration(
        &mut transport.process,
        &mut transport.stderr_pump,
        TransportKind::Relay,
    )
    .await;
    outcome
}
