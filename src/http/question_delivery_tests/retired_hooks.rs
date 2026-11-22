//! Retired entry points cannot reach an authenticated native peer.
use super::*;

#[test]
fn public_finish_is_absent_and_direct_calls_return_retired() {
    assert!(
        !tool_definitions()
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "request_finish")
    );
    for arguments in [
        json!({}),
        json!({"client_id":"fixture","reason":"done","approved":true}),
        json!({"client_id":"fixture","request_id":"old","reason":"done"}),
    ] {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"request_finish","arguments":arguments}});
        let request =
            RpcRequest::parse(&request).unwrap_or_else(|fault| panic!("{}", fault.message));
        let Action::Reply(response) =
            operation(&request).unwrap_or_else(|fault| panic!("{}", fault.message))
        else {
            panic!("retired tool must return immediately without native work");
        };
        assert_eq!(response["isError"], true);
        assert!(
            response["content"][0]["text"]
                .as_str()
                .unwrap()
                .starts_with("tool_retired:")
        );
    }
}

#[tokio::test]
async fn retired_native_tools_never_dispatch_or_consume_human_answers() {
    let fixture = Fixture::new().await;
    for name in ["ssmcp.ui.answer", "ssmcp.ui.finish"] {
        let error = fixture.native_call(fixture.run(), name).await.unwrap_err();
        assert!(error.starts_with("tool_retired:"), "{name}: {error}");
    }
    let native = fixture.native.lock().unwrap();
    assert!(native.calls.is_empty());
    assert!(native.finish_operations.is_empty());
    assert!(native.acknowledgements.is_empty());
    assert!(native.pending);
    assert!(!fixture.peer.is_finished());
}

#[tokio::test]
async fn retired_finish_wait_never_starts_a_native_observation() {
    let fixture = Fixture::new().await;
    let host = code_host(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.run(),
        None,
        "retired wait".into(),
        Default::default(),
    );
    let error = host(HostRequest::Builtin {
        name: "await".into(),
        arguments: json!({"client_id":fixture.session.client_id(),
            "targets":[{"type":"finish","request_id":"old"}],"timeout_ms":1000}),
    })
    .await
    .unwrap_err();
    assert!(error.starts_with("tool_retired:"), "{error}");
    let native = fixture.native.lock().unwrap();
    assert!(native.calls.is_empty());
    assert!(native.finish_operations.is_empty());
    assert!(native.acknowledgements.is_empty());
}

#[test]
fn public_discovery_has_no_forced_continuation_or_finish_instructions() {
    let definitions = tool_definitions();
    let description = definitions
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "code_mode")
        .unwrap()["description"]
        .as_str()
        .unwrap();
    let helper_usage = usage().to_string();
    for text in [INSTRUCTIONS, description, helper_usage.as_str()] {
        for retired in [
            "CONTINUATION REMINDER",
            "CONVERSATION REMINDER",
            "continuation_reminder",
            "conversation_reminders",
            "request_finish",
            "ssmcp.ui.finish",
            "unless the user",
            "repeat after timed_out",
            "Repeat after timed_out",
            "If it times out, wait again",
        ] {
            assert!(!text.contains(retired), "retired instruction: {retired}");
        }
    }
}
