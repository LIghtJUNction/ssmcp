use super::*;
use zbridge::{Effect, Id, Operation, ResponseBody};

struct PrivateFixture {
    directory: PathBuf,
}
impl Host for PrivateFixture {
    fn sessions(&self) -> CoreFuture<'_, Vec<Value>> {
        Box::pin(async {
            Ok(vec![
                json!({"name":"private-attach-fixture","connection_state":"disconnected"}),
            ])
        })
    }
    fn connect(&self, session: String, read_only: bool) -> CoreFuture<'_, Arc<dyn Connection>> {
        Box::pin(async move {
            if session != "private-attach-fixture" {
                return Err(BridgeError::invalid("unknown fixture"));
            }
            let frontend = RemoteFrontend::connect(self.directory.clone(), read_only)
                .await
                .map_err(native)?;
            Ok(Arc::new(Attached::new(frontend)) as Arc<dyn Connection>)
        })
    }
}

/// Called from the attach module's isolated real IPC fixture, without replacing
/// process-global capture state or starting any Runtime/model/session manager.
pub(crate) async fn exercise_gateway_fixture(
    directory: PathBuf,
    observe_started: Arc<Notify>,
    resume_events: Arc<Notify>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    const TOKEN: &str = "2222222222222222222222222222222222222222222222222222222222222222";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = gateway::router(
        gateway::Config {
            token: TOKEN.into(),
            origin: url.clone(),
            assets: None,
        },
        Arc::new(PrivateFixture {
            directory: directory.clone(),
        }),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    // Exercise the real RemoteFrontend path that used to return a local
    // Denied error and have the HTTP transport incorrectly make it uncertain.
    let readonly: Value = client
        .post(format!("{url}/api/gui/v1/connections"))
        .bearer_auth(TOKEN)
        .json(&json!({
            "request_id":"private-readonly-open",
            "session":"private-attach-fixture",
            "read_only":true
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let readonly_connection = readonly["connection_id"].as_str().unwrap();
    let readonly_command = Request {
        version: zbridge::VERSION,
        id: Id::new("private-readonly-command").unwrap(),
        operation: Operation::Command {
            generation: serde_json::from_value(readonly["capabilities"]["generation"].clone())
                .unwrap(),
            command: Id::new("terminal.write").unwrap(),
            arguments: json!({"terminal_id":"main","data":""}),
        },
    };
    let denied: Response = client
        .post(format!(
            "{url}/api/gui/v1/connections/{readonly_connection}/bridge"
        ))
        .bearer_auth(TOKEN)
        .json(&readonly_command)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    denied.validate_for(&readonly_command).unwrap();
    let ResponseBody::Error(error) = denied.body else {
        panic!("readonly native attachment accepted a command")
    };
    assert_eq!(error.code, zbridge::ErrorCode::Denied);
    assert_eq!(error.effect, Effect::None);
    let detached: Value = client
        .delete(format!(
            "{url}/api/gui/v1/connections/{readonly_connection}"
        ))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detached["closed"], true);
    assert_eq!(detached["session_stopped"], false);
    let open =
        json!({"request_id":"private-open","session":"private-attach-fixture","read_only":false});
    let opened: Value = client
        .post(format!("{url}/api/gui/v1/connections"))
        .bearer_auth(TOKEN)
        .json(&open)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(opened["status"], "connected");
    assert_eq!(opened["capabilities"]["body"]["value"]["mode"], "control");
    assert!(opened.get("lease").is_none());
    assert!(opened.get("token").is_none());
    assert!(opened.get("directory").is_none());
    // A duplicate HTTP open must reuse the actual exclusive input lease.
    let repeated: Value = client
        .post(format!("{url}/api/gui/v1/connections"))
        .bearer_auth(TOKEN)
        .json(&open)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(repeated["connection_id"], opened["connection_id"]);
    assert!(
        RemoteFrontend::connect(directory.clone(), false)
            .await
            .is_err()
    );
    let connection = opened["connection_id"].as_str().unwrap();
    let generation: Id =
        serde_json::from_value(opened["capabilities"]["generation"].clone()).unwrap();
    let request = Request {
        version: zbridge::VERSION,
        id: Id::new("real-state").unwrap(),
        operation: Operation::State {
            generation: generation.clone(),
            resource: Id::new("runtime").unwrap(),
            query: json!({}),
        },
    };
    let reply: Response = client
        .post(format!("{url}/api/gui/v1/connections/{connection}/bridge"))
        .bearer_auth(TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    reply.validate_for(&request).unwrap();
    let ResponseBody::State(state) = reply.body else {
        panic!("native runtime state missing")
    };
    assert_eq!(state.value["backend_active"], false);
    let events: Value = client
        .post(format!("{url}/api/gui/v1/connections/{connection}/events"))
        .bearer_auth(TOKEN)
        .json(&json!({"request_id":"real-events","generation":generation,"cursor":"0","wait_ms":0}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events["schema"], "ssmcp-gui-events-v1");
    assert!(events["page"]["events"].is_array());
    // There is no bound Runtime, so the real host must not invent terminal work.
    let command = Request {
        version: zbridge::VERSION,
        id: Id::new("unavailable-command").unwrap(),
        operation: Operation::Command {
            generation: generation.clone(),
            command: Id::new("terminal.write").unwrap(),
            arguments: json!({"terminal_id":"none","data":"not sent"}),
        },
    };
    let reply: Response = client
        .post(format!("{url}/api/gui/v1/connections/{connection}/bridge"))
        .bearer_auth(TOKEN)
        .json(&command)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let ResponseBody::Error(error) = reply.body else {
        panic!("missing Runtime was presented as completed work")
    };
    assert_eq!(error.effect, Effect::None);
    // The fixture holds a real native event request until explicitly released.
    // State/command exchanges must remain independent; close must cancel the
    // HTTP observation rather than await its 55-second window or stalled IPC.
    let observing = {
        let client = client.clone();
        let url = url.clone();
        let connection = connection.to_owned();
        let generation = generation.clone();
        let cursor = events["page"]["live_cursor"].clone();
        tokio::spawn(async move {
            client.post(format!("{url}/api/gui/v1/connections/{connection}/events")).bearer_auth(TOKEN)
                .json(&json!({"request_id":"pending-observation","generation":generation,"cursor":cursor,"wait_ms":55000}))
                .send().await.unwrap()
        })
    };
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        observe_started.notified(),
    )
    .await
    .unwrap();
    let state = Request {
        version: zbridge::VERSION,
        id: Id::new("state-during-observation").unwrap(),
        operation: Operation::State {
            generation: generation.clone(),
            resource: Id::new("runtime").unwrap(),
            query: json!({}),
        },
    };
    let state = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client
            .post(format!("{url}/api/gui/v1/connections/{connection}/bridge"))
            .bearer_auth(TOKEN)
            .json(&state)
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(state.status().is_success());
    let command = Request {
        version: zbridge::VERSION,
        id: Id::new("command-during-observation").unwrap(),
        operation: Operation::Command {
            generation,
            command: Id::new("terminal.write").unwrap(),
            arguments: json!({"terminal_id":"none","data":"not sent"}),
        },
    };
    let command = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client
            .post(format!("{url}/api/gui/v1/connections/{connection}/bridge"))
            .bearer_auth(TOKEN)
            .json(&command)
            .send(),
    )
    .await
    .unwrap()
    .unwrap();
    let command: Response = command.json().await.unwrap();
    let ResponseBody::Error(error) = command.body else {
        panic!("missing Runtime invented a concurrent command")
    };
    assert_eq!(error.effect, Effect::None);
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client
            .delete(format!("{url}/api/gui/v1/connections/{connection}"))
            .bearer_auth(TOKEN)
            .send(),
    )
    .await
    .unwrap()
    .unwrap()
    .error_for_status()
    .unwrap();
    let closed: Value = closed.json().await.unwrap();
    assert_eq!(closed["session_stopped"], false);
    let observed = tokio::time::timeout(std::time::Duration::from_secs(2), observing)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.status(), axum::http::StatusCode::GONE);
    resume_events.notify_one();
    let mut writer = RemoteFrontend::connect(directory, false).await.unwrap();
    writer.close().await.unwrap();
    server.abort();
    let _ = server.await;
}
