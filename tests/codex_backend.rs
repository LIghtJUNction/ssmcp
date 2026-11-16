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
    assert_eq!(
        tools
            .iter()
            .filter(|tool| !tool["name"].as_str().unwrap().starts_with("codex.mcp."))
            .count(),
        13
    );
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
    let observed = event(&codex, "item/commandExecution/requestApproval").await;
    let status = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(status["answers"].as_array().unwrap().is_empty());
    let requests = call(&codex, "ssmcp.codex.requests", json!({})).await;
    let pending = &requests["requests"][0];
    assert_eq!(
        pending,
        &json!({
            "id":"approval-1", "method":"item/commandExecution/requestApproval",
            "params":{"command":"fixture-only","threadId":"thread-1","turnId":"turn-1"}
        })
    );
    // Audit events identify the request without persisting its full payload.
    // The pending approval interface must retain that payload unchanged.
    assert_eq!(observed["id"], pending["id"]);
    assert_eq!(observed["method"], pending["method"]);
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
    event(&codex, "turn/completed").await;
    let status = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(status["answers"][0]["id"], "approval-1");
    assert_eq!(
        status["answers"][0]["result"],
        json!({"decision":"decline"})
    );
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

fn mcp_tools(tools: &[Value]) -> Vec<&Value> {
    tools
        .iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| name.starts_with("codex.mcp."))
        })
        .collect()
}

fn named_tool(tools: &[Value], title: &str) -> String {
    tools.iter().find(|tool| tool["title"] == title).unwrap()["name"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn mcp_mode(codex: &Codex, mode: &str) {
    call(
        codex,
        "codex.rpc",
        json!({"method":"fixture/mcp_mode","params":{"mode":mode}}),
    )
    .await;
}

#[tokio::test]
async fn mcp_catalog_preserves_full_native_tools_and_stable_names_without_model_calls() {
    let codex = start().await;
    let tools = codex.tools().await.unwrap();
    let discovered = mcp_tools(&tools);
    assert_eq!(discovered.len(), 8);
    let original = call(&codex, "codex.rpc", json!({"method":"fixture/mcp_specs"})).await;
    let mut names = std::collections::BTreeSet::new();
    for tool in discovered {
        let name = tool["name"].as_str().unwrap();
        assert!(name.len() <= 128);
        assert!(
            name.bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        );
        assert!(names.insert(name));
        let native = original["servers"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|server| server["tools"].as_object().unwrap().values())
            .find(|native| native["title"] == tool["title"])
            .unwrap();
        let mut unrouted = tool.clone();
        unrouted["name"] = native["name"].clone();
        assert_eq!(&unrouted, native, "only the routing name may change");
    }
    assert_eq!(
        named_tool(&tools, "native-read"),
        "codex.mcp.code_2Egraph.read_2Ffile"
    );
    assert_ne!(
        named_tool(&tools, "native-read"),
        named_tool(&tools, "native-underscore")
    );
    assert_ne!(
        named_tool(&tools, "native-dot-server"),
        named_tool(&tools, "native-boundary")
    );
    assert!(named_tool(&tools, "native-long").starts_with("codex.mcp._h"));
    let no_annotations = tools
        .iter()
        .find(|tool| tool["title"] == "native-no-annotations")
        .unwrap();
    assert!(no_annotations.get("annotations").is_none());
    let diagnostic = codex.diagnostics().await;
    assert_eq!(diagnostic["status"], "ready");
    assert_eq!(diagnostic["omitted"][0]["tool"], "missing-schema");
    assert!(
        diagnostic["omitted"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("not enumerable")
    );
    mcp_mode(&codex, "reordered").await;
    assert_eq!(
        codex.tools().await.unwrap(),
        tools,
        "native pagination order must not change routing or directory order"
    );
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(
        stats["thread_starts"], 1,
        "refresh must reuse its ephemeral thread"
    );
    assert_eq!(stats["model_requests"], 0);
    codex.shutdown().await;
}

#[tokio::test]
async fn background_catalog_reads_coalesce_native_enumeration_but_explicit_refresh_still_works() {
    let codex = start().await;
    let listed = |stats: &Value| {
        stats["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|method| **method == "mcpServerStatus/list")
            .count()
    };
    let before = call(&codex, "codex.server_diagnostics", json!({})).await;
    for _ in 0..4 {
        assert_eq!(mcp_tools(&codex.background_tools().await.unwrap()).len(), 8);
    }
    let cached = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(listed(&cached), listed(&before));
    mcp_mode(&codex, "updated").await;
    let changed = codex.tools().await.unwrap();
    assert!(!mcp_tools(&changed).is_empty());
    let refreshed = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(listed(&refreshed) > listed(&cached));
    assert_eq!(refreshed["model_requests"], 0);
    codex.shutdown().await;
}

#[tokio::test]
async fn mcp_calls_keep_original_arguments_results_and_native_error_flag() {
    let codex = start().await;
    let tools = codex.tools().await.unwrap();
    let name = named_tool(&tools, "native-read");
    for failed in [false, true] {
        let arguments =
            json!({"path":"src/lib.rs","choice":7,"fail":failed,"nativeExtra":{"preserved":true}});
        let result = codex.call(&name, arguments.clone()).await.unwrap();
        assert_eq!(result["isError"], failed);
        assert_eq!(result["content"][0]["text"], "native-result");
        assert_eq!(result["content"][1]["type"], "image");
        assert_eq!(
            result["structuredContent"],
            json!({"server":"code.graph","threadId":"thread-1","tool":"read/file","arguments":arguments})
        );
        assert_eq!(result["_meta"]["fixture/result"], "preserved");
        assert_eq!(result["x-native-result"]["value"], 42);
    }
    let params =
        json!({"server":"other","threadId":"thread-1","tool":"echo","arguments":{"fail":true}});
    let direct = codex
        .call("codex.mcpServer_tool_call", params.clone())
        .await
        .unwrap();
    let raw = codex
        .call(
            "codex.rpc",
            json!({"method":"mcpServer/tool/call","params":params}),
        )
        .await
        .unwrap();
    assert_eq!(
        raw, direct,
        "generic native entry points must preserve the same MCP result shape"
    );
    assert_eq!(direct["isError"], true);
    assert_eq!(
        codex.diagnostics().await["tools"],
        8,
        "a native MCP business error is not a stale transport/catalog"
    );
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(stats["model_requests"], 0);
    codex.shutdown().await;
}

#[tokio::test]
async fn mcp_refresh_replaces_changed_schemas_and_rejects_removed_tools() {
    let codex = start().await;
    let initial = codex.tools().await.unwrap();
    let removed = named_tool(&initial, "native-underscore");
    let unchanged = named_tool(&initial, "native-read");
    mcp_mode(&codex, "updated").await;
    let updated = codex.tools().await.unwrap();
    assert!(!updated.iter().any(|tool| tool["name"] == removed));
    assert_eq!(named_tool(&updated, "native-read"), unchanged);
    assert_eq!(
        updated
            .iter()
            .find(|tool| tool["name"] == unchanged)
            .unwrap()["inputSchema"]["properties"]["revision"]["const"],
        2
    );
    assert!(
        codex
            .call(&removed, json!({}))
            .await
            .unwrap_err()
            .contains("current discovered catalog")
    );
    let added = named_tool(&updated, "native-new");
    assert_eq!(
        codex.call(&added, json!({})).await.unwrap()["structuredContent"]["tool"],
        "new-tool"
    );
    mcp_mode(&codex, "down").await;
    assert!(mcp_tools(&codex.tools().await.unwrap()).is_empty());
    assert!(codex.call(&unchanged, json!({})).await.is_err());
    mcp_mode(&codex, "healthy").await;
    assert_eq!(
        named_tool(&codex.tools().await.unwrap(), "native-read"),
        unchanged
    );
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(stats["mcp_calls"].as_array().unwrap().len(), 1);
    assert_eq!(stats["thread_starts"], 1);
    assert_eq!(stats["model_requests"], 0);
    codex.shutdown().await;
}

#[tokio::test]
async fn failed_or_cyclic_mcp_pages_revoke_the_previous_complete_snapshot() {
    let codex = start().await;
    let old_name = named_tool(&codex.tools().await.unwrap(), "native-read");
    for mode in [
        "page_error",
        "malformed",
        "invalid_cursor",
        "loop",
        "endless",
    ] {
        mcp_mode(&codex, mode).await;
        let tools = codex.tools().await.unwrap();
        assert!(
            mcp_tools(&tools).is_empty(),
            "{mode} must not publish a partial catalog"
        );
        assert!(
            tools
                .iter()
                .any(|tool| tool["name"] == "codex.server_diagnostics")
        );
        let diagnostic = codex.diagnostics().await;
        assert_eq!(diagnostic["status"], "unavailable");
        assert_eq!(diagnostic["tools"], 0);
        assert!(diagnostic["reason"].is_string());
        assert!(codex.call(&old_name, json!({})).await.is_err());
        mcp_mode(&codex, "healthy").await;
        assert_eq!(mcp_tools(&codex.tools().await.unwrap()).len(), 8);
    }
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(stats["mcp_calls"].as_array().unwrap().is_empty());
    assert_eq!(stats["model_requests"], 0);
    codex.shutdown().await;
}

#[tokio::test]
async fn stalled_mcp_page_has_a_deadline_and_keeps_native_controls_available() {
    let codex = start().await;
    mcp_mode(&codex, "timeout").await;
    let refreshing = codex.clone();
    let task = tokio::spawn(async move { refreshing.tools().await });
    event(&codex, "fixture/mcpPageWaiting").await;
    // The native control channel remains usable while MCP discovery is pending.
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert_eq!(stats["model_requests"], 0);
    let tools = timeout(Duration::from_secs(26), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(mcp_tools(&tools).is_empty());
    assert!(
        codex.diagnostics().await["reason"]
            .as_str()
            .unwrap()
            .contains("timed out")
    );
    let before = call(&codex, "codex.server_diagnostics", json!({})).await;
    let retry = timeout(Duration::from_secs(2), codex.tools())
        .await
        .unwrap()
        .unwrap();
    assert!(mcp_tools(&retry).is_empty());
    let after = call(&codex, "codex.server_diagnostics", json!({})).await;
    let pages = |stats: &Value| {
        stats["methods"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|name| *name == "mcpServerStatus/list")
            .count()
    };
    assert_eq!(
        pages(&before),
        pages(&after),
        "a timed-out native page must not accumulate another request"
    );
    mcp_mode(&codex, "healthy").await;
    assert_eq!(mcp_tools(&codex.tools().await.unwrap()).len(), 8);
    codex.shutdown().await;
}

#[tokio::test]
async fn cancelled_discovery_does_not_leave_old_mcp_routes_registered() {
    let codex = start().await;
    let initial = codex.tools().await.unwrap();
    let name = named_tool(&initial, "native-read");
    assert_eq!(mcp_tools(&initial).len(), 8);
    mcp_mode(&codex, "timeout").await;
    let refreshing = codex.clone();
    let task = tokio::spawn(async move { refreshing.tools().await });
    event(&codex, "fixture/mcpPageWaiting").await;
    let during = timeout(Duration::from_secs(2), codex.call(&name, json!({})))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(during["content"][0]["text"], "native-result");
    let concurrent = timeout(Duration::from_secs(2), codex.tools())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        mcp_tools(&concurrent).len(),
        8,
        "a concurrent listing uses the last complete snapshot"
    );
    assert_eq!(codex.diagnostics().await["refreshing"], true);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let diagnostic = codex.diagnostics().await;
    assert_eq!(diagnostic["tools"], 0);
    assert_eq!(diagnostic["status"], "unavailable");
    mcp_mode(&codex, "healthy").await;
    assert_eq!(mcp_tools(&codex.tools().await.unwrap()).len(), 8);
    codex.shutdown().await;
}

#[tokio::test]
async fn mcp_approval_is_forwarded_without_auto_answer_or_blocking_refresh() {
    let codex = start().await;
    let name = named_tool(&codex.tools().await.unwrap(), "native-approval");
    let caller = codex.clone();
    let task =
        tokio::spawn(async move { caller.call(&name, json!({"path":"fixture-only"})).await });
    let observed = event(&codex, "mcpServer/elicitation/request").await;
    assert_eq!(observed["id"], "mcp-approval-1");
    assert!(!task.is_finished());
    let stats = call(&codex, "codex.server_diagnostics", json!({})).await;
    assert!(stats["answers"].as_array().unwrap().is_empty());
    assert_eq!(stats["model_requests"], 0);
    let listed = timeout(Duration::from_secs(2), codex.tools())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mcp_tools(&listed).len(), 8);
    assert!(!task.is_finished());
    let requests = call(&codex, "ssmcp.codex.requests", json!({})).await;
    let pending = &requests["requests"][0];
    assert_eq!(
        pending,
        &json!({
            "jsonrpc":"2.0", "id":"mcp-approval-1", "method":"mcpServer/elicitation/request",
            "params":{"serverName":"code.graph","message":"Fixture approval",
                "requestedSchema":{"type":"object","properties":{}}}
        })
    );
    assert_eq!(observed["id"], pending["id"]);
    assert_eq!(observed["method"], pending["method"]);
    let decline = json!({"action":"decline","content":null});
    call(
        &codex,
        "ssmcp.codex.respond",
        json!({"id":"mcp-approval-1","result":decline}),
    )
    .await;
    let native = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(native["isError"], true);
    assert_eq!(native["structuredContent"]["approval"], decline);
    assert_eq!(native["content"][1]["type"], "image");
    assert!(
        call(&codex, "ssmcp.codex.requests", json!({})).await["requests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    codex.shutdown().await;
}

#[tokio::test]
async fn native_mcp_rpc_error_is_preserved_and_blocks_stale_refresh_publication() {
    let codex = start().await;
    let name = named_tool(&codex.tools().await.unwrap(), "native-read");
    mcp_mode(&codex, "timeout").await;
    let refreshing = codex.clone();
    let task = tokio::spawn(async move { refreshing.tools().await });
    event(&codex, "fixture/mcpPageWaiting").await;
    let error = codex
        .call(&name, json!({"nativeRpcError":true}))
        .await
        .unwrap_err();
    let native: Value =
        serde_json::from_str(error.strip_prefix("native RPC error: ").unwrap()).unwrap();
    assert_eq!(
        native,
        json!({"code":-32042,"message":"Fixture native routing failed",
        "data":{"server":"code.graph","preserved":[1,{"opaque":true}]}})
    );
    assert_eq!(codex.diagnostics().await["tools"], 0);
    mcp_mode(&codex, "healthy").await;
    let obsolete = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        mcp_tools(&obsolete).is_empty(),
        "an older successful discovery must not republish after native invalidation"
    );
    assert!(codex.call(&name, json!({})).await.is_err());
    assert_eq!(mcp_tools(&codex.tools().await.unwrap()).len(), 8);
    assert_eq!(
        call(&codex, "codex.server_diagnostics", json!({})).await["model_requests"],
        0
    );
    codex.shutdown().await;
}

#[tokio::test]
async fn native_peer_exit_invalidates_mcp_routes_and_catalog() {
    let codex = start().await;
    let old_name = named_tool(&codex.tools().await.unwrap(), "native-read");
    call(&codex, "codex.rpc", json!({"method":"fixture/disconnect"})).await;
    timeout(Duration::from_secs(2), async {
        loop {
            let state = call(&codex, "ssmcp.codex.requests", json!({})).await;
            if state["connected"] == false {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(codex.background_tools().await.is_err());
    assert!(codex.tools().await.is_err());
    assert!(codex.call(&old_name, json!({})).await.is_err());
    let diagnostic = codex.diagnostics().await;
    assert_eq!(diagnostic["connected"], false);
    assert_eq!(diagnostic["tools"], 0);
    assert_eq!(diagnostic["status"], "unavailable");
    codex.shutdown().await;
}

#[tokio::test]
#[ignore = "requires isolated SSMCP_CODEX_BIN, SSMCP_CODEX_MCP_TOOL and SSMCP_CODEX_MCP_ARGUMENTS; no model calls"]
async fn real_codex_mcp_tool_smoke() {
    assert_eq!(std::env::var("SSMCP_SMOKE_ISOLATED").as_deref(), Ok("1"));
    let binary = std::env::var_os("SSMCP_CODEX_BIN").expect("select an installed Codex binary");
    let name = std::env::var("SSMCP_CODEX_MCP_TOOL")
        .expect("select an actually discovered, harmless MCP tool");
    assert!(name.starts_with("codex.mcp."));
    let arguments: Value = serde_json::from_str(
        &std::env::var("SSMCP_CODEX_MCP_ARGUMENTS")
            .expect("supply the selected tool's native arguments"),
    )
    .unwrap();
    let codex = timeout(Duration::from_secs(90), Codex::start(&binary, &[]))
        .await
        .unwrap()
        .unwrap();
    let result = async {
        let discovered = wait_for_real_mcp_tool(&codex, &name).await?;
        eprintln!("Native MCP tool definition: {discovered}");
        timeout(Duration::from_secs(30), codex.call(&name, arguments))
            .await
            .map_err(|_| "selected native MCP tool call timed out after 30 seconds".to_owned())?
    }
    .await;
    codex.shutdown().await;
    let native = result.unwrap();
    assert!(native["content"].is_array());
    assert_ne!(native.get("isError"), Some(&Value::Bool(true)));
    eprintln!("Native MCP tool result: {native}");
}

async fn wait_for_real_mcp_tool(codex: &Codex, name: &str) -> Result<Value, String> {
    // Native MCP servers initialize independently after the app-server handshake.
    // Poll only discovery; never start a model turn to make a server become ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut last_error = None;
    let mut total = 0;
    let mut names = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, codex.tools()).await {
            Ok(Ok(tools)) => {
                if let Some(discovered) = tools.iter().find(|tool| tool["name"] == name) {
                    return Ok(discovered.clone());
                }
                total = tools.len();
                names = mcp_tools(&tools)
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                    .collect();
                last_error = None;
            }
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => break,
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep_until(
            (tokio::time::Instant::now() + Duration::from_millis(500)).min(deadline),
        )
        .await;
    }
    let diagnostic = json!({
        "selectedTool":name, "catalogTools":total, "mcpTools":names.len(),
        "mcpNamesSample":names.iter().take(32).collect::<Vec<_>>(),
        "lastDiscoveryError":last_error, "provider":codex.diagnostics().await
    });
    Err(format!(
        "selected MCP tool was not discovered within 45 seconds: {diagnostic}"
    ))
}
