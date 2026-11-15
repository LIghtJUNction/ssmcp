#![cfg(target_os = "linux")]

use std::{ffi::OsStr, path::PathBuf, sync::Arc, time::Duration};

use serde_json::{Value, json};
use ssmcp::backend::pi::Pi;
use tokio::time::timeout;

async fn start() -> Arc<Pi> {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi_rpc.py");
    Pi::start(fixture.as_os_str(), &[]).await.unwrap()
}

async fn call(pi: &Pi, name: &str, args: Value) -> Value {
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
