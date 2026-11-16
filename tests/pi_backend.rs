#![cfg(target_os = "linux")]

use std::{ffi::OsStr, path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use ssmcp::backend::pi::Pi;
use tokio::time::timeout;

// Persistent real Unix transport; it never launches a Harness, model or package
// manager. Unknown model/startup commands deliberately fail the fixture.
struct SocketFixture {
    root: PathBuf,
    path: PathBuf,
    requests: Arc<std::sync::Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl SocketFixture {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("ssmcp-pi-socket-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.join("native.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = requests.clone();
        let task = tokio::spawn(async move {
            let mut connection = 0;
            let mut previous_state = None;
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                connection += 1;
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                let mut pending: Option<Value> = None;
                let mut first = true;
                while let Some(line) = lines.next_line().await.unwrap() {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    observed.lock().unwrap().push(request.clone());
                    let response = match request["type"].as_str().unwrap() {
                        "fixture_approval" => {
                            pending = Some(request.clone());
                            json!({"type":"extension_ui_request","id":format!("native-{}",request["id"].as_str().unwrap()),"method":"confirm","title":"Native fixture approval","message":"Explicit answer required"})
                        }
                        "extension_ui_response" => {
                            let original = pending.take().unwrap();
                            json!({"type":"response","id":original["id"],"command":original["type"],"success":true,"data":{"answer":request}})
                        }
                        "fixture_failure" => {
                            json!({"type":"response","id":request["id"],"command":request["type"],"success":false,"error":"native fixture refusal","nativeExtra":{"opaque":[1,true]}})
                        }
                        "get_state" => {
                            if first && let Some(stale) = &previous_state {
                                let encoded = format!("{stale}\n");
                                if writer.write_all(encoded.as_bytes()).await.is_err() {
                                    break;
                                }
                            }
                            let current = json!({"type":"response","id":request["id"],"command":"get_state","success":true,"data":{"isStreaming":false,"connection":connection}});
                            previous_state = Some(current.clone());
                            current
                        }
                        other => panic!("unexpected startup/model command {other}"),
                    };
                    first = false;
                    let encoded = response.to_string() + "\n";
                    if writer.write_all(encoded.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        });
        Self {
            root,
            path,
            requests,
            task,
        }
    }
    async fn attach(&self) -> Arc<Pi> {
        Pi::attach_with_output(
            &self.path,
            ssmcp::output::OutputStore::new_connection().unwrap(),
        )
        .await
        .unwrap()
    }
}
impl Drop for SocketFixture {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn existing_socket_preserves_native_approvals_errors_and_service_lifetime() {
    let fixture = SocketFixture::new();
    let pi = fixture.attach().await;
    assert_eq!(pi.tools().await.unwrap().len(), 4);
    assert_eq!(pi.diagnostics().await["transport"], "existing_unix_socket");
    let first_id = fixture.requests.lock().unwrap()[0]["id"].clone();
    let failed = call(&pi, "pi.rpc", json!({"type":"fixture_failure"})).await;
    assert_eq!(failed["isError"], true);
    assert_eq!(payload(&failed)["nativeExtra"], json!({"opaque":[1,true]}));
    assert_eq!(payload(&failed)["error"], "native fixture refusal");

    let waiting = pi.clone();
    let approval = tokio::spawn(async move {
        waiting
            .call("pi.rpc", json!({"type":"fixture_approval"}))
            .await
    });
    let dialog = wait_event(&pi, "extension_ui_request").await;
    let pending = call(&pi, "ssmcp.pi.requests", json!({})).await;
    assert_eq!(payload(&pending)["requests"][0]["id"], dialog["id"]);
    call(&pi, "pi.get_state", json!({})).await;
    assert!(!approval.is_finished());
    assert!(
        !fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request["type"] == "extension_ui_response")
    );
    call(
        &pi,
        "ssmcp.pi.respond",
        json!({"id":dialog["id"],"confirmed":false}),
    )
    .await;
    let refused = approval.await.unwrap().unwrap();
    assert_eq!(payload(&refused)["data"]["answer"]["confirmed"], false);

    let waiting = pi.clone();
    let abandoned = tokio::spawn(async move {
        waiting
            .call("pi.rpc", json!({"type":"fixture_approval"}))
            .await
    });
    timeout(Duration::from_secs(5), async {
        while payload(&call(&pi, "ssmcp.pi.requests", json!({})).await)["requests"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    pi.shutdown().await;
    assert!(abandoned.await.unwrap().is_err());
    assert!(
        !fixture.task.is_finished(),
        "disconnect must not terminate an attached native service"
    );
    let next_index = fixture.requests.lock().unwrap().len();
    let reconnected = fixture.attach().await;
    let requests = fixture.requests.lock().unwrap().clone();
    assert_ne!(
        requests[next_index]["id"], first_id,
        "connections must use different request ID namespaces"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request["type"] == "extension_ui_response")
            .count(),
        1,
        "disconnect must not fabricate another native approval response"
    );
    reconnected.shutdown().await;
}

#[tokio::test]
async fn native_socket_reconnect_cannot_match_a_previous_connection_response() {
    use ssmcp::backend::rpc::{Dialect, Peer};
    let fixture = SocketFixture::new();
    for expected in [1, 2] {
        let peer = Peer::connect_unix_with_output(
            &fixture.path,
            "pi-nonce",
            Dialect::Pi,
            ssmcp::output::OutputStore::new_connection().unwrap(),
        )
        .await
        .unwrap();
        let result = timeout(Duration::from_secs(5), peer.request("get_state", json!({})))
            .await
            .unwrap()
            .unwrap();
        // On reconnect the fixture deliberately sends the previous connection's
        // response first. Reusing ssmcp-1 would resolve this call with stale data.
        assert_eq!(result["data"]["connection"], expected);
        peer.shutdown().await;
    }
}

#[tokio::test]
async fn existing_socket_rejects_public_permissions_symlinks_and_non_socket_paths() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = SocketFixture::new();
    let output = ssmcp::output::OutputStore::new_connection().unwrap();
    assert!(
        Pi::attach_with_output(std::path::Path::new("relative.sock"), output.clone())
            .await
            .is_err()
    );
    let file = fixture.root.join("ordinary-file");
    std::fs::write(&file, b"not a socket").unwrap();
    assert!(Pi::attach_with_output(&file, output.clone()).await.is_err());
    let link = fixture.root.join("symlink.sock");
    symlink(&fixture.path, &link).unwrap();
    assert!(Pi::attach_with_output(&link, output.clone()).await.is_err());
    std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(
        Pi::attach_with_output(&fixture.path, output.clone())
            .await
            .is_err()
    );
    std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Pi::attach_with_output(&fixture.path, output).await.is_err());
    assert!(fixture.requests.lock().unwrap().is_empty());
    std::fs::set_permissions(&fixture.root, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture.attach().await.shutdown().await;
}

async fn start() -> Arc<Pi> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi_rpc.py");
    Pi::start(fixture.as_os_str(), &[]).await.unwrap()
}

async fn call(pi: &Pi, name: &str, args: Value) -> Value {
    // Exercise the published control wrapper while retaining readable native
    // command names in the protocol fixtures below.
    let (name, args) = match name.strip_prefix("pi.") {
        Some(command) if command != "rpc" => ("pi.rpc", json!({"type":command,"args":args})),
        _ => (name, args),
    };
    timeout(Duration::from_secs(5), pi.call(name, args))
        .await
        .expect("Pi call timed out")
        .expect("Pi call failed")
}

fn payload(result: &Value) -> &Value {
    &result["structuredContent"]
}

async fn wait_event(pi: &Pi, wanted: &str) -> Value {
    timeout(Duration::from_secs(5), async {
        let mut cursor = 0;
        loop {
            let page = call(
                pi,
                "ssmcp.pi.events",
                json!({"cursor":cursor,"max_events":256,"wait_ms":1000}),
            )
            .await;
            for event in payload(&page)["events"].as_array().unwrap() {
                if event["event"]["type"] == wanted {
                    return event["event"].clone();
                }
            }
            cursor = payload(&page)["next_cursor"].as_u64().unwrap();
        }
    })
    .await
    .expect("Expected Pi event did not arrive")
}

#[tokio::test]
async fn streams_before_completion_and_correlates_out_of_order_responses() {
    let pi = start().await;
    let backend = pi.clone();
    let bash = tokio::spawn(async move {
        call(
            &backend,
            "pi.bash",
            json!({"command":"fixture holds this request"}),
        )
        .await
    });
    let event = wait_event(&pi, "bash_execution_update").await;
    assert_eq!(event["delta"], "first\nsecond\u{2028}line\u{2029}\n");
    assert!(
        !bash.is_finished(),
        "Streaming output must be available before command completion"
    );

    let abort = call(&pi, "pi.abort_bash", json!({})).await;
    assert_eq!(payload(&abort)["command"], "abort_bash");
    let completed = timeout(Duration::from_secs(5), bash)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload(&completed)["command"], "bash");
    assert_eq!(payload(&completed)["id"], event["id"]);
    assert_ne!(payload(&completed)["id"], payload(&abort)["id"]);
    assert_eq!(payload(&completed)["data"]["cancelled"], true);
    assert_eq!(payload(&completed)["data"]["exitCode"], 130);
    pi.shutdown().await;
}

#[tokio::test]
async fn exposes_future_commands_errors_and_extension_vetoes_without_data_loss() {
    let pi = start().await;
    let args = json!({"newOption":{"nested":[1,true,"value"]}});
    let forwarded = call(&pi, "pi.rpc", json!({"type":"future_control","args":args})).await;
    assert_eq!(payload(&forwarded)["data"], args);
    assert_eq!(payload(&forwarded)["command"], "future_control");

    let rejected = call(&pi, "pi.rpc", json!({"type":"future_failure"})).await;
    assert_eq!(rejected["isError"], true);
    assert_eq!(payload(&rejected)["success"], false);
    assert_eq!(payload(&rejected)["error"], "native fixture rejection");
    let veto = call(&pi, "pi.new_session", json!({})).await;
    assert_eq!(veto["isError"], false);
    assert_eq!(payload(&veto)["data"]["cancelled"], true);

    let commands = call(&pi, "pi.get_commands", json!({})).await;
    assert_eq!(
        payload(&commands)["data"]["commands"][0]["name"],
        "fixture-command"
    );
    pi.shutdown().await;
}

#[tokio::test]
async fn dialogs_require_explicit_answers_and_informational_events_are_not_pending() {
    for method in ["confirm", "select", "input", "editor"] {
        let pi = start().await;
        let backend = pi.clone();
        let prompt = tokio::spawn(async move {
            call(
                &backend,
                "pi.prompt",
                json!({"message":format!("/dialog {method}")}),
            )
            .await
        });
        wait_event(&pi, "extension_ui_request").await;
        // This round trip establishes that the fixture has processed its input
        // while the prompt is blocked; no timing-based approval assumption.
        let state = call(&pi, "pi.get_state", json!({})).await;
        assert_eq!(payload(&state)["data"]["answers"], 0);
        assert!(!prompt.is_finished());
        let requests = call(&pi, "ssmcp.pi.requests", json!({})).await;
        let requests = payload(&requests)["requests"].as_array().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "notify must not appear as a pending dialog"
        );
        assert_eq!(requests[0]["method"], method);
        let id = requests[0]["id"].clone();

        assert!(
            pi.call("ssmcp.pi.respond", json!({"id":"unknown","cancelled":true}))
                .await
                .is_err()
        );
        assert!(
            pi.call(
                "ssmcp.pi.respond",
                json!({"id":id,"value":"deny","confirmed":false})
            )
            .await
            .is_err()
        );
        assert!(
            pi.call(
                "pi.rpc",
                json!({"type":"extension_ui_response","args":{"id":id,"confirmed":true}})
            )
            .await
            .is_err()
        );
        let answer = if method == "confirm" {
            assert!(
                pi.call("ssmcp.pi.respond", json!({"id":id,"value":"allow"}))
                    .await
                    .is_err()
            );
            json!({"id":id,"confirmed":false})
        } else {
            assert!(
                pi.call("ssmcp.pi.respond", json!({"id":id,"confirmed":true}))
                    .await
                    .is_err()
            );
            json!({"id":id,"value":"deny"})
        };
        assert_eq!(
            payload(&call(&pi, "ssmcp.pi.respond", answer.clone()).await)["sent"],
            true
        );
        assert!(
            pi.call("ssmcp.pi.respond", answer.clone()).await.is_err(),
            "A dialog must not be answered twice"
        );
        let response = timeout(Duration::from_secs(5), prompt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payload(&response)["data"]["disposition"], "handled");
        for (key, value) in answer.as_object().unwrap() {
            assert_eq!(payload(&response)["data"]["answer"][key], *value);
        }
        assert!(
            payload(&call(&pi, "ssmcp.pi.requests", json!({})).await)["requests"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        pi.shutdown().await;
    }
}

#[tokio::test]
async fn rejects_protocol_spoofing_before_writing_and_reaps_the_child_on_shutdown() {
    let pi = start().await;
    let before = call(&pi, "pi.get_state", json!({})).await;
    let pid = payload(&before)["data"]["pid"].as_i64().unwrap() as libc::pid_t;
    for (name, arguments) in [
        ("pi.get_state", json!({"id":"fake"})),
        ("pi.get_state", json!({"type":"bash"})),
        ("pi.rpc", json!({"type":"get_state","args":{"id":"fake"}})),
        ("pi.rpc", json!({"type":"get_state","args":{"type":"bash"}})),
        ("pi.rpc", json!({"type":"get_state","args":null})),
    ] {
        assert!(pi.call(name, arguments).await.is_err());
    }
    let after = call(&pi, "pi.get_state", json!({})).await;
    assert_eq!(
        payload(&after)["data"]["calls"].as_u64().unwrap(),
        payload(&before)["data"]["calls"].as_u64().unwrap() + 1
    );
    pi.shutdown().await;
    // SAFETY: signal 0 only checks whether this already-reaped child PID exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert!(pi.call("pi.get_state", json!({})).await.is_err());
    let events = call(&pi, "ssmcp.pi.events", json!({})).await;
    assert_eq!(payload(&events)["closed"], true);
    assert_eq!(
        payload(&call(&pi, "ssmcp.pi.requests", json!({})).await)["connected"],
        false
    );
}

#[tokio::test]
async fn missing_executable_fails_before_creating_a_backend() {
    assert!(
        Pi::start(OsStr::new("/nonexistent-ssmcp-pi-test"), &[])
            .await
            .is_err()
    );
    let exited = timeout(
        Duration::from_secs(5),
        Pi::start(OsStr::new("/bin/false"), &[]),
    )
    .await
    .expect("An exited CLI must not wait for the startup deadline");
    assert!(exited.is_err());
}

/// Run explicitly with an installed CLI, for example:
/// SSMCP_PI_BIN=/path/to/pi cargo test --test pi_backend real_pi_cli_smoke -- --ignored
/// The launcher strips inherited credentials and uses a disposable configuration.
#[tokio::test]
#[ignore = "requires SSMCP_PI_BIN; starts a real local Pi CLI without model calls"]
async fn real_pi_cli_smoke() {
    use std::os::unix::fs::PermissionsExt;

    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    let executable = std::fs::canonicalize(
        std::env::var_os("SSMCP_PI_BIN").expect("Set SSMCP_PI_BIN to the installed Pi executable"),
    )
    .expect("Cannot resolve SSMCP_PI_BIN");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let scratch =
        Scratch(std::env::temp_dir().join(format!("ssmcp-real-pi-{}-{nonce}", std::process::id())));
    std::fs::create_dir(&scratch.0).unwrap();
    std::fs::set_permissions(&scratch.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = scratch.0.join("config");
    let work = scratch.0.join("work");
    std::fs::create_dir(&config).unwrap();
    std::fs::create_dir(&work).unwrap();
    let extension = scratch.0.join("probe.mjs");
    std::fs::write(
        &extension,
        r#"export default function(pi) {
            pi.registerCommand("ssmcp-real-confirm", {
                description: "Confirm a local test without model work",
                handler: async (_args, ctx) => {
                    const confirmed = await ctx.ui.confirm("ssmcp test", "Local test only");
                    ctx.ui.notify(JSON.stringify({confirmed}), "info");
                }
            });
        }"#,
    )
    .unwrap();
    let launcher = scratch.0.join("launch-pi");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\ncd {} || exit 1\nexec env -i PATH={} LANG=C.UTF-8 PI_CODING_AGENT_DIR={} PI_OFFLINE=1 PI_SKIP_VERSION_CHECK=1 PI_TELEMETRY=0 {} \"$@\"\n",
            quote(work.to_str().unwrap()),
            quote(&std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())),
            quote(config.to_str().unwrap()),
            quote(executable.to_str().unwrap()),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut args: Vec<std::ffi::OsString> = [
        "--no-session",
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--no-approve",
        "--offline",
        "--extension",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    args.push(extension.into_os_string());
    let pi = timeout(
        Duration::from_secs(20),
        Pi::start(launcher.as_os_str(), &args),
    )
    .await
    .expect("Real Pi startup timed out")
    .expect("Real Pi startup failed");

    let state = call(&pi, "pi.get_state", json!({})).await;
    assert_eq!(payload(&state)["success"], true);
    assert_eq!(payload(&state)["data"]["isStreaming"], false);
    let commands = call(&pi, "pi.get_commands", json!({})).await;
    assert!(
        payload(&commands)["data"]["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command["name"] == "ssmcp-real-confirm"),
        "Discover the extension before prompting so a missing slash command cannot reach a model"
    );

    let bash = call(
        &pi,
        "pi.bash",
        json!({"command":"printf 'ssmcp-real-one\\n'; sleep 0.1; printf 'ssmcp-real-two\\n'","excludeFromContext":true}),
    )
    .await;
    assert_eq!(payload(&bash)["success"], true);
    assert_eq!(payload(&bash)["data"]["exitCode"], 0);
    assert_eq!(
        payload(&bash)["data"]["output"],
        "ssmcp-real-one\nssmcp-real-two\n"
    );
    let streamed = wait_event(&pi, "bash_execution_update").await;
    assert_eq!(streamed["id"], payload(&bash)["id"]);

    let backend = pi.clone();
    let prompt = tokio::spawn(async move {
        call(
            &backend,
            "pi.prompt",
            json!({"message":"/ssmcp-real-confirm"}),
        )
        .await
    });
    let dialog = wait_event(&pi, "extension_ui_request").await;
    assert_eq!(dialog["method"], "confirm");
    assert!(!prompt.is_finished());
    let requests = call(&pi, "ssmcp.pi.requests", json!({})).await;
    assert_eq!(payload(&requests)["requests"][0]["id"], dialog["id"]);
    let reply = call(
        &pi,
        "ssmcp.pi.respond",
        json!({"id":dialog["id"],"confirmed":false}),
    )
    .await;
    assert_eq!(payload(&reply)["sent"], true);
    let accepted = timeout(Duration::from_secs(5), prompt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload(&accepted)["data"]["disposition"], "handled");
    let history = call(&pi, "ssmcp.pi.events", json!({"max_events":256})).await;
    let events = payload(&history)["events"].as_array().unwrap();
    assert!(events.iter().any(|entry| {
        entry["event"]["method"] == "notify" && entry["event"]["message"] == "{\"confirmed\":false}"
    }));
    assert!(
        !events
            .iter()
            .any(|entry| entry["event"]["type"] == "agent_start")
    );
    let native_error = call(&pi, "pi.rpc", json!({"type":"ssmcp_unknown_probe_command"})).await;
    assert_eq!(native_error["isError"], true);
    assert_eq!(payload(&native_error)["success"], false);
    pi.shutdown().await;
    assert_eq!(
        payload(&call(&pi, "ssmcp.pi.events", json!({})).await)["closed"],
        true
    );
}

#[tokio::test]
async fn discovery_reports_control_scope_without_inventing_native_tool_schemas() {
    let pi = start().await;
    let before = call(&pi, "pi.get_state", json!({})).await;
    assert_eq!(payload(&before)["data"]["pi_offline"], "1");
    let args = payload(&before)["data"]["startup_args"].as_array().unwrap();
    assert!(args.iter().any(|arg| arg == "--offline"));
    assert!(
        !args.iter().any(|arg| arg == "--no-extensions"),
        "native approval extensions must not be disabled"
    );
    let tools = pi.tools().await.unwrap();
    let names: std::collections::BTreeSet<_> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        std::collections::BTreeSet::from([
            "pi.rpc",
            "ssmcp.pi.events",
            "ssmcp.pi.requests",
            "ssmcp.pi.respond",
        ])
    );
    let diagnostic = pi.diagnostics().await;
    assert_eq!(diagnostic["status"], "ready");
    assert_eq!(diagnostic["tools"], 4);
    assert_eq!(diagnostic["nativeToolEnumeration"], "unavailable");
    assert!(
        diagnostic["scope"]
            .as_str()
            .unwrap()
            .contains("not enumerable")
    );
    assert_eq!(pi.tools().await.unwrap(), tools);
    let after = call(&pi, "pi.get_state", json!({})).await;
    assert_eq!(
        payload(&after)["data"]["calls"].as_u64().unwrap(),
        payload(&before)["data"]["calls"].as_u64().unwrap() + 1,
        "catalog discovery must not issue prompts or probe unadvertised commands"
    );
    pi.shutdown().await;
    assert!(pi.tools().await.is_err());
    let diagnostic = pi.diagnostics().await;
    assert_eq!(diagnostic["status"], "unavailable");
    assert_eq!(diagnostic["tools"], 0);
}
