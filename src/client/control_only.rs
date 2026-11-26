//! Explicit, unsupervised native control connection. No frontend Runtime,
//! chat ingress, repository watcher, worker pool or lifecycle workflow exists.
use super::*;
use crate::{
    registry::Provider,
    runtime::{
        call_scope::CallScope,
        native_host::{Definition, NativeHost},
    },
};

fn command(executable: &std::path::Path, server: &str) -> Command {
    let mut command = relay::command(executable, server);
    // Replace the usual offers: no lifecycle, messaging, wait-display, peer-port,
    // repository-event or worker activity subscriptions on this connection.
    command.env(
        "SSMCP_RELAY_CAPABILITIES",
        format!(
            "{},{},{}",
            crate::wire::SESSION_ACCESS_CAPABILITY,
            crate::wire::CONTROL_ATTRIBUTION_CAPABILITY,
            crate::wire::CHAT_DISABLED_CAPABILITY
        ),
    );
    // Keep transport diagnostics on stderr directly; the shared registration
    // helper then has no stderr event pump or presentation frontend to start.
    command.stderr(std::process::Stdio::inherit());
    command
}

async fn send(writer: &mut tokio::process::ChildStdin, frame: Frame) -> crate::Result<()> {
    timeout(Duration::from_secs(10), write_frame(writer, &frame))
        .await
        .map_err(|_| "control-only reply timed out; inspect native state before retrying")??;
    Ok(())
}

struct Controls {
    process: Arc<Backend>,
    host: NativeHost,
    generation: u64,
}
impl Controls {
    async fn open() -> crate::Result<Self> {
        // Existing discovery constructor allocates no shell or command process.
        let process = Backend::for_control()?;
        let result = (|| {
            process.configure_chat(false)?;
            let registry = process.registry.clone();
            registry.replace(Provider::Ui, vec![])?;
            registry.replace(Provider::Terminal, vec![])?;
            registry.replace(Provider::Harnesses, vec![])?;
            registry.replace(Provider::Client, vec![json!({
                "name":"ssmcp.client.capabilities", "description":"Read this control-only connection capability manifest.",
                "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
                "annotations":{"readOnlyHint":true}
            })])?;
            // Retain ordinary process state/output, but no event-reading API.
            let tools = crate::backend::shell::Shell::tools()
                .into_iter()
                .filter(|tool| tool["name"] != "ssmcp.exec.events")
                .collect();
            registry.replace(Provider::Exec, tools)?;
            let host = NativeHost::new(registry, vec![Definition::new(process.clone())])?;
            Ok::<_, String>(host)
        })();
        match result {
            Ok(host) => Ok(Self {
                process,
                host,
                generation: crate::runtime::call_scope::connection(),
            }),
            Err(error) => {
                process.shutdown().await;
                Err(error.into())
            }
        }
    }
    fn capabilities(&self) -> Result<serde_json::Value, String> {
        let mut manifest = self.process.registry.capabilities()?;
        manifest["schema"] = json!("ssmcp.client.capabilities.v1");
        manifest["client_version"] = json!(env!("CARGO_PKG_VERSION"));
        manifest["wire_version"] = json!(VERSION);
        manifest["chat_enabled"] = json!(false);
        manifest["control_only"] = json!(true);
        Ok(manifest)
    }
    async fn perform(
        &self,
        operation: crate::wire::Operation,
    ) -> Result<serde_json::Value, String> {
        operation.validate()?;
        let (name, arguments) = match operation {
            crate::wire::Operation::ListTools { cursor } => {
                let tools = self.process.registry.snapshot()?;
                let fingerprint = crate::catalog::fingerprint(&tools);
                return crate::catalog::page(&tools, cursor.as_deref(), &fingerprint);
            }
            crate::wire::Operation::CallTool { name, arguments }
                if name == "ssmcp.client.capabilities" =>
            {
                if !arguments.as_object().is_some_and(|args| args.is_empty()) {
                    return Err("client capabilities takes no arguments".into());
                }
                return self.capabilities();
            }
            crate::wire::Operation::CallTool { name, arguments } => (name, arguments),
            _ => return Err("operation unavailable in control-only mode".into()),
        };
        let tool = self.process.registry.lookup(&name)?;
        if !matches!(tool.provider, Provider::Exec) {
            return Err("tool unavailable in control-only mode".into());
        }
        self.host.call(&tool, arguments).await
    }
    async fn shutdown(&self) {
        self.process.registry.close();
        self.host.shutdown().await;
        self.process.shutdown().await;
    }
}

fn authorize(
    server: &str,
    client: &str,
    attribution: Option<&crate::wire::ControlAttribution>,
) -> Result<(), String> {
    let source = attribution.ok_or("missing authenticated controller identity")?;
    source.validate()?;
    let decision = controller_accounts::observe_verified(server, &source.account, client)
        .map_err(|_| "local controller policy is unavailable")?;
    if !decision.is_allowed() {
        return Err(
            "controller requires local approval; inspect ssmcp controllers on the device".into(),
        );
    }
    Ok(())
}

pub(super) async fn run(options: &Options) -> crate::Result<()> {
    let server = options
        .server
        .as_deref()
        .ok_or("control-only requires a relay server")?;
    let mut interrupt = interrupt_signal()?;
    let mut terminate = terminate_signal()?;
    let cancelled = async {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
    };
    let mut registered = false;
    let executable = reload::installed_program()?;
    let registration = register_transport(
        TransportKind::Relay,
        Some(server),
        &options.metadata()?,
        &mut registered,
        || command(executable, server),
        cancelled,
        Duration::from_secs(120),
    )
    .await?;
    let Some(mut link) = registration else {
        return Ok(());
    };
    let validation = async {
        link.require_allow_access(&options.allow).await?;
        if !link.chat_disabled_acknowledged {
            return Err("control-only requires relay acknowledgement of disabled chat".into());
        }
        Ok(())
    }
    .await;
    if let Err(error) = validation {
        stop_registration(
            &mut link.process,
            &mut link.stderr_pump,
            link.transport_kind,
        )
        .await?;
        return Err(error);
    }
    let RegisteredTransport {
        mut process,
        mut stderr_pump,
        mut writer,
        reader,
        ready,
        transport_kind,
        ..
    } = link;
    let outcome = async {
        let controls = Controls::open().await?;
        println!("{}", json!({"type":"shell", "mode":"control-only", "client_id":ready.3,
            "url":ready.0, "chat_enabled":false}));
        let mut incoming = Incoming::start(reader, transport_kind);
        let mut requests = tokio::task::JoinSet::new();
        let controls = Arc::new(controls);
        // Replies are serialized here, so spawned native calls never borrow transport IO.
        let result: crate::Result<()> = async {
            loop {
                tokio::select! {
                    biased;
                    _ = interrupt.recv() => return Ok(()),
                    _ = terminate.recv() => return Ok(()),
                    complete = requests.join_next(), if !requests.is_empty() => {
                        let (id, result) = complete.ok_or("native request disappeared")??;
                        send(&mut writer, Frame::Reply { id, result }).await?;
                    }
                    frame = incoming.frames.recv() => match frame {
                        Some(Ok(Some(Frame::RelayMaintenance { id, operation: crate::wire::Operation::CallTool { name, arguments } })))
                            if name == "ssmcp.client.capabilities" && arguments.as_object().is_some_and(|args| args.is_empty()) => {
                            send(&mut writer, Frame::Reply { id, result: controls.capabilities() }).await?;
                        }
                        Some(Ok(Some(Frame::Call { id, operation, attribution, code_context, .. }))) => {
                            let admitted = authorize(server, &ready.3, attribution.as_deref())
                                .and_then(|()| CallScope::from_relay_for_client(controls.generation,
                                    Some(&ready.3), id, attribution.as_deref(), code_context.as_ref()));
                            #[cfg(target_os = "linux")]
                            let admitted = admitted.and_then(|scope| {
                                    // Preserve normal Linux resource admission, lazily after
                                    // authorization and before any requested process is created.
                                    if matches!(&operation, crate::wire::Operation::CallTool { name, .. } if name == "ssmcp.exec.start") {
                                        crate::session_budget::enter().map_err(|error| format!("could not admit command memory ceiling: {error}"))?;
                                    }
                                    Ok(scope)
                                });
                            match admitted {
                                Err(error) => send(&mut writer, Frame::Reply { id, result: Err(error) }).await?,
                                Ok(_) if requests.len() >= MAX_INFLIGHT => send(&mut writer,
                                    Frame::Reply { id, result: Err("control-only request limit reached; no request admitted".into()) }).await?,
                                Ok(scope) => {
                                    let controls = controls.clone();
                                    requests.spawn(async move {
                                        let result = scope.scope(controls.perform(operation)).await;
                                        (id, bounded_native_reply(result, |_| {}))
                                    });
                                }
                            }
                        }
                        Some(Ok(Some(Frame::CodeModeFinished { completion }))) => {
                            // Existing transport audit sidecar, not a workflow or event dispatch.
                            completion.validate()?;
                        }
                        // No dispatch of workflow, messaging, or event frames.
                        Some(Ok(Some(Frame::Reject { message }))) => return Err(relay::failure(message)),
                        Some(Err(error)) => return Err(error.into()),
                        None | Some(Ok(None)) => return Err("control-only relay disconnected; no automatic restart or replay".into()),
                        _ => return Err("unexpected frame on control-only connection".into()),
                    }
                }
            }
        }.await;
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        controls.shutdown().await;
        incoming.stop().await;
        result
    }.await;
    let cleanup = stop_registration(&mut process, &mut stderr_pump, transport_kind).await;
    outcome.and(cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn options(flags: &[&str]) -> Result<Options, String> {
        Options::parse(flags.iter().map(OsString::from))?.ok_or("missing options".into())
    }
    #[test]
    fn explicit_mode_preserves_default_and_legacy_daemon() {
        for flags in [
            vec!["alice@relay.example"],
            vec!["alice@relay.example", "--daemon"],
        ] {
            let parsed = options(&flags).unwrap();
            assert!(!parsed.control_only);
            assert!(parsed.watchdog);
        }
        let parsed = options(&["alice@relay.example", "--daemon", "--control-only"]).unwrap();
        assert!(parsed.daemon && parsed.control_only);
        assert!(!parsed.watchdog);
        assert_eq!(parsed.chat.input, chat_io::Input::None);
        assert_eq!(parsed.chat.agent, chat_io::Agent::NextCaller);
        for extra in [
            vec!["--worker", "2"],
            vec!["--input", "none"],
            vec!["--agent", "codex"],
            vec!["--session-control"],
            vec!["--watchdog"],
            vec!["--tty", "kitty"],
            vec!["--shell", "/bin/sh"],
            vec!["--reconnect-delay", "1"],
            vec!["--memory-limit", "512MiB"],
        ] {
            let mut flags = vec!["alice@relay.example", "--daemon", "--control-only"];
            flags.extend(extra);
            assert!(options(&flags).is_err(), "{flags:?}");
        }
        assert!(options(&["alice@relay.example", "--control-only"]).is_err());
    }
    #[test]
    fn transport_has_only_auth_and_chat_disabled_offers() {
        let command = command(
            std::path::Path::new("/fixture/ssmcp"),
            "https://relay.example",
        );
        let offer = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == "SSMCP_RELAY_CAPABILITIES")
            .unwrap()
            .1
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(
            offer.split(',').collect::<Vec<_>>(),
            vec![
                crate::wire::SESSION_ACCESS_CAPABILITY,
                crate::wire::CONTROL_ATTRIBUTION_CAPABILITY,
                crate::wire::CHAT_DISABLED_CAPABILITY
            ]
        );
        assert!(
            authorize("https://relay.example", "fixture", None)
                .unwrap_err()
                .contains("authenticated")
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_catalog_cannot_dispatch_workflow_or_events() {
        let controls = Controls::open().await.unwrap();
        let projection = controls.process.observe_exec_projection();
        let page = controls
            .perform(crate::wire::Operation::ListTools { cursor: None })
            .await
            .unwrap();
        let names: Vec<_> = page["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"ssmcp.exec.start") && !names.contains(&"terminal.status"));
        assert!(names.iter().all(|name| *name == "ssmcp.client.capabilities"
            || (name.starts_with("ssmcp.exec.") && !name.ends_with("events"))));
        for name in [
            "ssmcp.exec.events",
            "ssmcp.worker.submit",
            "ssmcp.ui.wait",
            "ssmcp.harnesses.load",
            "ssmcp.plugins.load",
            "ssmcp.client.reload",
            "ssmcp.webhooks.notify",
        ] {
            assert!(
                controls
                    .perform(crate::wire::Operation::CallTool {
                        name: name.into(),
                        arguments: json!({})
                    })
                    .await
                    .is_err(),
                "{name}"
            );
        }
        assert_eq!(controls.capabilities().unwrap()["chat_enabled"], false);
        let job = controls.perform(crate::wire::Operation::CallTool {
            name: "ssmcp.exec.start".into(), arguments: json!({"command":["/bin/sh","-c","printf CONTROL_ONLY_UNIT"],"timeout_ms":5000})
        }).await.unwrap();
        let job_id = job["structuredContent"]["job_id"].as_str().unwrap();
        controls
            .perform(crate::wire::Operation::CallTool {
                name: "ssmcp.exec.wait".into(),
                arguments: json!({"job_id":job_id,"wait_ms":5000}),
            })
            .await
            .unwrap();
        let output = controls
            .perform(crate::wire::Operation::CallTool {
                name: "ssmcp.exec.read".into(),
                arguments: json!({"job_id":job_id,"max_bytes":4096}),
            })
            .await
            .unwrap();
        assert!(output.to_string().contains("CONTROL_ONLY_UNIT"));
        assert!(
            projection.try_recv().is_err(),
            "native events must not reach a presentation subscriber"
        );
        #[cfg(target_os = "linux")]
        let owned_pid = {
            let job = controls
                .perform(crate::wire::Operation::CallTool {
                    name: "ssmcp.exec.start".into(),
                    arguments: json!({"command":["/bin/sleep","120"]}),
                })
                .await
                .unwrap();
            job["structuredContent"]["pid"].as_u64().unwrap()
        };
        controls.shutdown().await;
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{owned_pid}")).exists());
        assert!(
            controls
                .perform(crate::wire::Operation::Status {})
                .await
                .is_err()
        );
    }
}
