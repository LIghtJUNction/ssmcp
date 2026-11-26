use super::*;
#[cfg(unix)]
use zbridge::Effect;
use zbridge::Operation;

fn req(operation: Operation) -> Request {
    Request {
        version: zbridge::VERSION,
        id: id("fixture-request"),
        operation,
    }
}

fn host() -> (Arc<Capture>, super::super::Handle) {
    let capture = Arc::new(Capture::new().unwrap());
    let handle = super::super::Handle {
        task: None,
        capture: capture.clone(),
    };
    (capture, handle)
}

async fn generation(frontend: &Frontend) -> Id {
    let response = frontend.request(req(Operation::Capabilities {})).await;
    assert!(matches!(response.body, ResponseBody::Capabilities(_)));
    response.generation.unwrap()
}

fn error(response: Response) -> BridgeError {
    let ResponseBody::Error(error) = response.body else {
        panic!("expected bridge error")
    };
    error
}

#[tokio::test]
async fn pending_attach_observation_never_takes_input_or_consumes_events() {
    let (capture, handle) = host();
    let frontend = handle.frontend(true).unwrap();
    let generation = generation(&frontend).await;
    capture.push(json!({"kind":"event","label":"fixture","value":{"type":"ssmcp.status","message":"live event"}}));
    let live = frontend
        .request(req(Operation::Events {
            generation: generation.clone(),
            cursor: None,
            limit: 16,
            wait_ms: 0,
        }))
        .await;
    let ResponseBody::Events(live) = live.body else {
        panic!("missing event page")
    };
    assert!(live.events.is_empty());
    assert_eq!(live.next_cursor, Counter(1));
    let read = req(Operation::Events {
        generation: generation.clone(),
        cursor: Some(Counter(0)),
        limit: 16,
        wait_ms: 0,
    });
    for _ in 0..2 {
        let response = frontend.request(read.clone()).await;
        response.validate_for(&read).unwrap();
        let ResponseBody::Events(page) = response.body else {
            panic!("missing event page")
        };
        assert_eq!(page.events.len(), 1);
        assert_eq!(page.events[0].value["value"]["message"], "live event");
    }
    assert_eq!(capture.ring.lock().unwrap().next, 1);
    assert_eq!(capture.ring.lock().unwrap().events.len(), 1);
    let writer = handle.frontend(false).unwrap();
    assert!(handle.frontend(false).is_err());
    let response = frontend
        .request(req(Operation::Command {
            generation,
            command: id("terminal.write"),
            arguments: json!({"terminal_id":"main","data":"not delivered"}),
        }))
        .await;
    assert_eq!(error(response).code, ErrorCode::Denied);
    drop(writer);
    assert!(handle.frontend(false).is_ok());
}

#[tokio::test]
async fn real_attach_envelope_and_embedded_frontend_share_host_leases() {
    let (capture, handle) = host();
    let opened = super::super::handle_request(&capture, json!({"op":"open","read_only":true}))
        .await
        .unwrap();
    let lease = opened["lease"].as_str().unwrap();
    let request = req(Operation::Capabilities {});
    let raw = super::super::handle_request(
        &capture,
        json!({"op":"bridge","lease":lease,"request":request.clone()}),
    )
    .await
    .unwrap();
    let response: zbridge::Response = serde_json::from_value(raw).unwrap();
    response.validate_for(&request).unwrap();
    let ResponseBody::Capabilities(capabilities) = response.body else {
        panic!("missing capabilities")
    };
    assert_eq!(capabilities.mode, Mode::Observe);
    assert!(capabilities.commands.is_empty());
    assert_eq!(capabilities.resources.len(), 1);
    let forged = super::super::handle_request(
        &capture,
        json!({"op":"bridge","lease":lease,
        "request":{"version":1,"id":"forged","operation":{"op":"capabilities","owner":"human"}}}),
    )
    .await;
    assert!(forged.is_err());
    super::super::handle_request(&capture, json!({"op":"detach","lease":lease}))
        .await
        .unwrap();
    let denied = super::request(&capture, lease, req(Operation::Capabilities {})).await;
    assert_eq!(error(denied).code, ErrorCode::Denied);
    let frontend = handle.frontend(true).unwrap();
    drop(handle);
    assert_eq!(
        error(frontend.request(req(Operation::Capabilities {})).await).code,
        ErrorCode::Closed
    );
}

#[tokio::test]
async fn expired_lease_is_rechecked_after_a_real_event_wait() {
    let (capture, handle) = host();
    let frontend = handle.frontend(true).unwrap();
    let generation = generation(&frontend).await;
    let wait = frontend.request(req(Operation::Events {
        generation,
        cursor: None,
        limit: 16,
        wait_ms: 1000,
    }));
    tokio::pin!(wait);
    tokio::select! {
        biased;
        value = &mut wait => panic!("wait returned before an event: {value:?}"),
        _ = tokio::task::yield_now() => {},
    }
    capture.leases.lock().unwrap().remove(&frontend.lease);
    capture.push(json!({"kind":"event","value":{"message":"private after revocation"}}));
    let response = wait.await;
    assert!(
        !String::from_utf8(zbridge::encode_response(&response).unwrap())
            .unwrap()
            .contains("private after revocation")
    );
    assert_eq!(error(response).code, ErrorCode::Denied);
}

#[tokio::test]
async fn bridge_pages_report_real_retention_gaps_and_reject_old_backend_cursors() {
    let (capture, handle) = host();
    let frontend = handle.frontend(true).unwrap();
    let old_generation = generation(&frontend).await;
    for index in 0..100 {
        capture.push(json!({"kind":"event","value":{"index":index,"text":"x".repeat(40_000)}}));
    }
    let query = req(Operation::Events {
        generation: old_generation.clone(),
        cursor: Some(Counter(0)),
        limit: 128,
        wait_ms: 0,
    });
    let response = frontend.request(query.clone()).await;
    response.validate_for(&query).unwrap();
    assert!(zbridge::encode_response(&response).unwrap().len() < zbridge::MAX_FRAME_BYTES);
    let ResponseBody::Events(page) = response.body else {
        panic!("missing page")
    };
    assert!(page.retention_gap);
    assert!(page.start_cursor.0 > 0);
    assert!(page.next_cursor > page.start_cursor);
    capture.runtime.lock().unwrap().generation += 1;
    let stale = frontend
        .request(req(Operation::Events {
            generation: old_generation,
            cursor: Some(page.next_cursor),
            limit: 16,
            wait_ms: 0,
        }))
        .await;
    assert_eq!(error(stale).code, ErrorCode::StaleGeneration);
}

#[cfg(unix)]
#[tokio::test]
async fn real_runtime_worker_resize_requires_writer_and_exact_pool_without_manager_impersonation() {
    let backend = crate::backend::Backend::start(std::path::Path::new("/bin/sh"))
        .await
        .unwrap();
    let runtime = Runtime::new(backend, std::env::current_dir().unwrap()).unwrap();
    runtime
        .configure_workers(
            crate::worker::Config {
                count: 2,
                factor: 1.0,
            },
            &crate::client::chat_io::Agent::Remote,
        )
        .unwrap();
    runtime.start_workers();
    let (capture, handle) = host();
    let binding = handle.bind_runtime(runtime.clone()).unwrap();
    let reader = handle.frontend(true).unwrap();
    let writer = handle.frontend(false).unwrap();
    let generation = generation(&writer).await;
    let state = writer
        .request(req(Operation::State {
            generation: generation.clone(),
            resource: id("workers"),
            query: json!({}),
        }))
        .await;
    let ResponseBody::State(state) = state.body else {
        panic!("missing worker state")
    };
    assert_eq!(state.value["offline"], 2);
    assert_eq!(state.value["idle"], 0);
    assert_eq!(state.value["dreaming"], 0);
    assert_eq!(state.value["slots"][0]["display_state"], "offline");
    assert_eq!(state.value["registered_workers"], 0);
    assert!(state.value["manager"].is_null());
    let pool_generation = state.value["pool_generation"].as_str().unwrap();
    let command = req(Operation::Command {
        generation: generation.clone(),
        command: id("workers.resize"),
        arguments: json!({"pool_generation":pool_generation,"total":0}),
    });
    let denied = reader.request(command.clone()).await;
    assert_eq!(error(denied).code, ErrorCode::Denied);
    assert_eq!(runtime.frontend_workers().unwrap().total, 2);
    let response = writer.request(command.clone()).await;
    response.validate_for(&command).unwrap();
    let ResponseBody::Command(receipt) = response.body else {
        panic!("missing native resize receipt")
    };
    assert_eq!(receipt.value["resize_pending"], false);
    assert_eq!(receipt.value["worker"]["total"], 0);
    assert!(receipt.value["worker"]["manager"].is_null());
    assert!(runtime.frontend_workers().unwrap().manager.is_none());
    let stale_pool = writer
        .request(req(Operation::Command {
            generation: generation.clone(),
            command: id("workers.resize"),
            arguments: json!({"pool_generation":"replaced","total":2}),
        }))
        .await;
    let stale_pool = error(stale_pool);
    assert_eq!(stale_pool.effect, Effect::None);
    assert_eq!(runtime.frontend_workers().unwrap().total, 0);
    drop(binding);
    let replaced = writer.request(command).await;
    assert_eq!(error(replaced).code, ErrorCode::StaleGeneration);
    assert_ne!(self::generation(&writer).await, generation);
    assert_eq!(
        capture
            .ring
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|(_, raw, _, _)| raw.get().contains("bridge"))
            .count(),
        0
    );
    runtime.shutdown().await;
}

#[tokio::test]
async fn listeners_sharing_one_process_journal_have_distinct_bridge_generations() {
    let journal = Capture::new().unwrap();
    let first = Arc::new(Capture::with_journal(&journal));
    let second = Arc::new(Capture::with_journal(&journal));
    assert_eq!(first.generation, second.generation);
    assert!(Arc::ptr_eq(&first.ring, &second.ring));
    let first_handle = super::super::Handle {
        task: None,
        capture: first,
    };
    let second_handle = super::super::Handle {
        task: None,
        capture: second,
    };
    let first = first_handle.frontend(true).unwrap();
    let second = second_handle.frontend(true).unwrap();
    let first_generation = generation(&first).await;
    assert_ne!(first_generation, generation(&second).await);
    let rejected = second
        .request(req(Operation::State {
            generation: first_generation,
            resource: id("runtime"),
            query: json!({}),
        }))
        .await;
    assert_eq!(error(rejected).code, ErrorCode::StaleGeneration);
}

#[cfg(unix)]
#[tokio::test]
async fn remote_frontend_uses_the_real_private_ipc_and_explicit_detach() {
    use std::os::unix::fs::PermissionsExt;
    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let directory = Directory(std::env::temp_dir().join(format!(
        "ssmcp-zbridge-{}",
        crate::session::random_suffix().unwrap()
    )));
    std::fs::create_dir(&directory.0).unwrap();
    std::fs::set_permissions(&directory.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = crate::session::ipc::Server::bind(&directory.0, super::super::ENDPOINT)
        .await
        .unwrap();
    let (capture, _handle) = host();
    let served = capture.clone();
    let service = tokio::spawn(async move {
        loop {
            let mut peer = server.accept().await.unwrap();
            let request = peer.read().await.unwrap();
            let result = super::super::handle_request(&served, request)
                .await
                .map_err(|error| error.to_string());
            let _ = peer.reply(result).await;
        }
    });
    let mut frontend = RemoteFrontend::connect(directory.0.clone(), true)
        .await
        .unwrap();
    let capabilities = frontend
        .request(req(Operation::Capabilities {}))
        .await
        .unwrap();
    let generation = capabilities.generation.unwrap();
    let ResponseBody::Capabilities(capabilities) = capabilities.body else {
        panic!("missing real IPC capabilities")
    };
    assert_eq!(capabilities.mode, Mode::Observe);
    assert!(capabilities.commands.is_empty());
    capture.push(json!({"kind":"event","value":{"message":"from the actual IPC source"}}));
    let query = req(Operation::Events {
        generation: generation.clone(),
        cursor: Some(Counter(0)),
        limit: 16,
        wait_ms: 0,
    });
    let events = frontend.request(query.clone()).await.unwrap();
    events.validate_for(&query).unwrap();
    let ResponseBody::Events(events) = events.body else {
        panic!("missing real IPC events")
    };
    assert_eq!(
        events.events[0].value["value"]["message"],
        "from the actual IPC source"
    );
    assert_eq!(capture.leases.lock().unwrap().len(), 1);
    let denied = frontend
        .request(req(Operation::Command {
            generation,
            command: id("terminal.write"),
            arguments: json!({"terminal_id":"main","data":"never sent"}),
        }))
        .await
        .unwrap_err();
    assert_eq!(denied.effect, Effect::None);
    frontend.close().await.unwrap();
    assert!(capture.leases.lock().unwrap().is_empty());
    assert_eq!(
        frontend
            .request(req(Operation::Capabilities {}))
            .await
            .unwrap_err()
            .code,
        ErrorCode::Closed
    );
    service.abort();
    let _ = service.await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn unified_pty_bridge_keeps_writer_lease_generation_and_native_hooks() {
    use crate::runtime::hooks::{ToolCall, ToolCallHook};
    use crate::wire::Operation as NativeOperation;
    use std::time::Duration;
    struct DenyInput;
    impl ToolCallHook for DenyInput {
        fn before_call(&self, call: &ToolCall) -> Result<(), String> {
            if matches!(
                call.native_name.as_str(),
                "terminal.write" | "ssmcp.exec.write" | "ssmcp.exec.resize"
            ) {
                Err("fixture denies native PTY control".into())
            } else {
                Ok(())
            }
        }
    }
    let backend = crate::backend::Backend::start(std::path::Path::new("/bin/sh"))
        .await
        .unwrap();
    let runtime = Runtime::new(backend, std::env::current_dir().unwrap()).unwrap();
    let jobs = runtime.terminals();
    let started=jobs.call("ssmcp.exec.start",json!({"mode":"pty","command":["python3","-c",
        "import os,tty; tty.setraw(0); os.write(1,b'READY'); data=os.read(0,1); os.write(1,b'HANDLED:'+data)"]})).await.unwrap();
    let job_id = started["job_id"].as_str().unwrap();
    let mut cursor = 0;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let page = jobs
                .call(
                    "ssmcp.exec.read",
                    json!({"job_id":job_id,"stdout_cursor":cursor,"wait_ms":100}),
                )
                .await
                .unwrap();
            cursor = page["stdout_cursor"].as_u64().unwrap();
            if page["stdout"].as_str().unwrap().contains("READY") {
                break;
            }
        }
    })
    .await
    .unwrap();
    runtime.set_tool_call_hook(Some(Arc::new(DenyInput)));
    for (name, arguments) in [
        ("ssmcp.exec.write", json!({"job_id":job_id,"data":"x"})),
        (
            "ssmcp.exec.resize",
            json!({"job_id":job_id,"rows":44,"cols":120}),
        ),
        ("terminal.write", json!({"terminal_id":job_id,"data":"x"})),
    ] {
        let rejected = runtime
            .perform(NativeOperation::CallTool {
                name: name.into(),
                arguments,
            })
            .await
            .unwrap_err();
        assert!(rejected.contains("fixture denies native PTY control"));
    }
    runtime.set_tool_call_hook(None);
    assert_eq!(
        jobs.call("ssmcp.exec.status", json!({"job_id":job_id}))
            .await
            .unwrap()["stdin"]["bytes_written"],
        0
    );

    let (_capture, handle) = host();
    let binding = handle.bind_runtime(runtime.clone()).unwrap();
    let reader = handle.frontend(true).unwrap();
    let writer = handle.frontend(false).unwrap();
    assert!(handle.frontend(false).is_err());
    let generation = generation(&writer).await;
    let state = reader
        .request(req(Operation::State {
            generation: generation.clone(),
            resource: id("terminals"),
            query: json!({}),
        }))
        .await;
    let ResponseBody::State(state) = state.body else {
        panic!("missing terminals")
    };
    let terminal = state.value["terminals"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["job_id"] == job_id)
        .expect("exec PTY appears in existing bridge resource");
    assert_eq!(terminal["pid"], started["pid"]);
    let write = req(Operation::Command {
        generation: generation.clone(),
        command: id("terminal.write"),
        arguments: json!({"terminal_id":job_id,"data":"q"}),
    });
    assert_eq!(
        error(reader.request(write.clone()).await).code,
        ErrorCode::Denied
    );
    let resize = req(Operation::Command {
        generation: generation.clone(),
        command: id("terminal.resize"),
        arguments: json!({"rows":39,"cols":117}),
    });
    assert_eq!(
        error(reader.request(resize.clone()).await).code,
        ErrorCode::Denied
    );
    let resized = writer.request(resize).await;
    assert!(matches!(resized.body, ResponseBody::Command(_)));
    assert_eq!(
        jobs.call("ssmcp.exec.status", json!({"job_id":job_id}))
            .await
            .unwrap()["stdin"]["bytes_written"],
        0
    );
    let accepted = writer.request(write.clone()).await;
    let ResponseBody::Command(receipt) = accepted.body else {
        panic!("missing write receipt")
    };
    assert_eq!(receipt.value["written"], 1);
    let done = jobs
        .call("ssmcp.exec.wait", json!({"job_id":job_id,"wait_ms":3000}))
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    let page = jobs
        .call(
            "ssmcp.exec.read",
            json!({"job_id":job_id,"stdout_cursor":cursor}),
        )
        .await
        .unwrap();
    assert_eq!(page["stdout"], "HANDLED:q");
    drop(binding);
    assert_eq!(
        error(writer.request(write).await).code,
        ErrorCode::StaleGeneration
    );
    assert_eq!(
        jobs.call("ssmcp.exec.status", json!({"job_id":job_id}))
            .await
            .unwrap()["stdin"]["bytes_written"],
        1
    );
    runtime.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn session_directory_through_bridge_changes_exec_and_keeps_workspace_boundary() {
    let root = std::env::current_dir().unwrap().canonicalize().unwrap();
    let runtime = Runtime::new(
        crate::backend::Backend::start(std::path::Path::new("/bin/sh"))
            .await
            .unwrap(),
        root.clone(),
    )
    .unwrap();
    runtime
        .local_shell()
        .bind_workbench(Arc::new(
            crate::session::workbench::Workbench::new(root.clone(), Some("demo@relay.test".into()))
                .unwrap(),
        ))
        .unwrap();
    let (_, handle) = host();
    let _binding = handle.bind_runtime(runtime.clone()).unwrap();
    let reader = handle.frontend(true).unwrap();
    let writer = handle.frontend(false).unwrap();
    let generation = generation(&writer).await;
    let command = |name: &str, arguments: Value| {
        req(Operation::Command {
            command: id(name),
            generation: generation.clone(),
            arguments,
        })
    };
    assert_eq!(
        error(reader.request(command("session.new", json!({}))).await).code,
        ErrorCode::Denied
    );
    let cd = command("session.cd", json!({"path":"src"}));
    assert_eq!(
        error(reader.request(cd.clone()).await).code,
        ErrorCode::Denied
    );
    let response = writer.request(cd).await;
    let ResponseBody::Command(receipt) = response.body else {
        panic!("{response:?}")
    };
    assert_eq!(receipt.value["cwd"], json!(root.join("src")));
    let response = writer
        .request(command("session.cd", json!({"path":"../.."})))
        .await;
    assert_eq!(error(response).code, ErrorCode::InvalidRequest);
    let status = runtime
        .local_shell()
        .call(
            "ssmcp.exec.start",
            json!({"command":["pwd"],"mode":"pipes"}),
        )
        .await
        .unwrap();
    let id = status["job_id"].as_str().unwrap();
    runtime
        .local_shell()
        .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
        .await
        .unwrap();
    let page = runtime
        .local_shell()
        .call("ssmcp.exec.read", json!({"job_id":id,"wait_ms":5000}))
        .await
        .unwrap();
    assert_eq!(page["exit_code"], 0);
    assert_eq!(
        page["stdout"].as_str().unwrap().trim_end(),
        root.join("src").to_str().unwrap()
    );
    let result = writer
        .request(req(Operation::State {
            generation,
            resource: self::id("runtime"),
            query: json!({}),
        }))
        .await;
    let ResponseBody::State(state) = result.body else {
        panic!("missing runtime state")
    };
    assert_eq!(state.value["cwd"], json!(root.join("src")));
    runtime.shutdown().await;
}
