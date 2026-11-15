//! The adapter's protocol semantics are tested without asking a model to run.
use serde_json::{Value, json};
use ssmcp::backend::codex::Codex;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::time::timeout;

async fn start() -> Arc<Codex> {
    Codex::start(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/codex_rpc.py")
            .as_os_str(),
        &[],
    )
    .await
    .unwrap()
}

async fn call(codex: &Codex, name: &str, args: Value) -> Value {
    timeout(Duration::from_secs(5), codex.call(name, args))
        .await
        .unwrap()
        .unwrap()["structuredContent"]
        .clone()
}

async fn event(codex: &Codex, method: &str) -> Value {
    timeout(Duration::from_secs(5), async {
        let mut cursor = 0;
        loop {
            let page = call(
                codex,
                "ssmcp.codex.events",
                json!({"cursor":cursor,"wait_ms":1000}),
            )
            .await;
            for item in page["events"].as_array().unwrap() {
                if item["event"]["method"] == method {
                    return item["event"].clone();
                }
            }
            cursor = page["next_cursor"].as_u64().unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn generated_catalog_keeps_native_shapes_null_parameters_and_future_methods() {
    let codex = start().await;
    let tools = codex.tools().await.unwrap();
    assert_eq!(tools.len(), 11);
    assert!(!tools.iter().any(|t| t["name"] == "codex.initialize"));
    let thread = tools
        .iter()
        .find(|t| t["name"] == "codex.thread_start")
        .unwrap();
    assert!(thread["inputSchema"]["definitions"]["Node"].is_object());
    let native = call(
        &codex,
        "codex.thread_start",
        json!({"ephemeral":true,"newOption":{"x":1}}),
    )
    .await;
    assert_eq!(native["params"]["newOption"]["x"], 1);
    assert!(
        call(&codex, "codex.configRequirements_read", json!({})).await["requirements"].is_null()
    );
    assert!(
        codex
            .call("codex.configRequirements_read", json!({"extra":true}))
            .await
            .is_err()
    );
    let native = call(
        &codex,
        "codex.rpc",
        json!({"method":"future/control","params":{"x":2}}),
    )
    .await;
    assert_eq!(native["native_method"], "future/control");
    assert_eq!(native["params"]["x"], 2);
    let schema = call(
        &codex,
        "ssmcp.codex.schema",
        json!({"name":"ApprovalResponse.json"}),
    )
    .await;
    assert_eq!(schema["properties"]["decision"]["enum"][1], "decline");
    codex.shutdown().await;
}

#[tokio::test]
async fn native_approvals_are_visible_and_require_an_explicit_response() {
    let codex = start().await;
    call(
        &codex,
        "codex.turn_start",
        json!({"threadId":"thread-1","input":[]}),
    )
    .await;
    let pending = event(&codex, "item/commandExecution/requestApproval").await;
    let status = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(status["answers"].as_array().unwrap().is_empty());
    let requests = call(&codex, "ssmcp.codex.requests", json!({})).await;
    assert_eq!(requests["requests"][0], pending);
    assert!(
        codex
            .call(
                "ssmcp.codex.respond",
                json!({"id":"spoof","result":{"decision":"accept"}})
            )
            .await
            .is_err()
    );
    assert!(
        codex
            .call(
                "ssmcp.codex.respond",
                json!({"id":"approval-1","result":{},"error":{"code":1,"message":"both"}})
            )
            .await
            .is_err()
    );
    call(
        &codex,
        "ssmcp.codex.respond",
        json!({"id":"approval-1","result":{"decision":"decline"}}),
    )
    .await;
    let completed = event(&codex, "turn/completed").await;
    assert_eq!(completed["params"]["decision"]["decision"], "decline");
    assert!(
        call(&codex, "ssmcp.codex.requests", json!({})).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        codex
            .call(
                "ssmcp.codex.respond",
                json!({"id":"approval-1","result":{}})
            )
            .await
            .is_err()
    );
    codex.shutdown().await;
}

#[tokio::test]
async fn native_cancellation_removes_stale_approval_requests() {
    let codex = start().await;
    call(&codex, "codex.turn_start", json!({})).await;
    event(&codex, "item/commandExecution/requestApproval").await;
    call(&codex, "codex.rpc", json!({"method":"fixture/cancel"})).await;
    assert!(
        call(&codex, "ssmcp.codex.requests", json!({})).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        codex
            .call(
                "ssmcp.codex.respond",
                json!({"id":"approval-1","result":{}})
            )
            .await
            .is_err()
    );
    codex.shutdown().await;
}

#[tokio::test]
async fn long_running_native_calls_leave_control_and_live_events_available() {
    let codex = start().await;
    let running = codex.clone();
    let task = tokio::spawn(async move {
        call(&running, "codex.command_exec", json!({"command":["hold"]})).await
    });
    event(&codex, "command/exec/outputDelta").await;
    assert!(!task.is_finished());
    call(
        &codex,
        "codex.command_exec_terminate",
        json!({"processId":"held"}),
    )
    .await;
    assert_eq!(task.await.unwrap()["exitCode"], 137);
    let pid = call(&codex, "codex.server_diagnostics", json!({})).await["pid"]
        .as_u64()
        .unwrap();
    codex.shutdown().await;
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
}

#[tokio::test]
#[ignore = "requires SSMCP_CODEX_BIN and isolated HOME/CODEX_HOME; no model calls"]
async fn real_codex_backend_smoke() {
    let binary =
        std::env::var_os("SSMCP_CODEX_BIN").expect("set an explicitly selected Codex binary");
    assert_eq!(std::env::var("SSMCP_SMOKE_ISOLATED").as_deref(), Ok("1"));
    let codex = timeout(Duration::from_secs(90), Codex::start(&binary, &[]))
        .await
        .unwrap()
        .unwrap();
    let tools = codex.tools().await.unwrap();
    assert!(tools.iter().any(|t| t["name"] == "codex.thread_start"));
    assert!(tools.iter().any(|t| t["name"] == "codex.turn_start"));
    eprintln!("Real Codex discovered {} MCP tools", tools.len());
    let requirements = call(&codex, "codex.configRequirements_read", json!({})).await;
    assert!(requirements.is_object());
    let diagnostics = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(diagnostics.is_object());
    let command = codex
        .call(
            "codex.command_exec",
            json!({"command":["/bin/sh","-c","printf SSMCP_CODEX_REAL"],"timeoutMs":5000}),
        )
        .await
        .unwrap();
    assert_eq!(command["structuredContent"]["exitCode"], 0);
    assert!(
        command["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .contains("SSMCP_CODEX_REAL")
    );
    let command=codex.call("codex.command_exec",json!({"command":["/bin/sh","-c","printf SSMCP_CODEX_STREAM"],"timeoutMs":5000,"processId":"ssmcp-smoke","streamStdoutStderr":true})).await.unwrap();
    assert_eq!(command["structuredContent"]["exitCode"], 0);
    event(&codex, "command/exec/outputDelta").await;
    assert!(
        call(&codex, "ssmcp.codex.requests", json!({})).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    codex.shutdown().await;
}
