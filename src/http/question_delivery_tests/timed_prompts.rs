//! Timed hook delivery uses the real authenticated native websocket fixture.
use super::*;

#[tokio::test]
async fn passive_await_delivers_retained_timer_hooks_without_ack_or_user_consumption() {
    let fixture = Fixture::with_timing(true).await;
    let event = timer_event();
    fixture.native.lock().unwrap().prompt_reply = Some(json!({
        "isError":false,"structuredContent":{
            "schema_version":1,"scope":"native_connection","provenance":"agent_timer",
            "completion_permission":false,"ack_required":true,"events":[event]
        }
    }));
    // Support is established by an actual bound native receipt, not test JSON
    // returned from a program or a write to the Session capability cache.
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    assert!(fixture.session.prompt_hooks_advertised());
    fixture.publish_summary_requirement();
    let mailbox_id = fixture.send_message(0).await;

    let ordinary = fixture
        .public_await(json!({
            "targets":[{"type":"job","id":"missing_prompt_fixture_job"}]
        }))
        .await;
    let durable = fixture
        .public_await(json!({
            "request_id":"prompt_fixture_wait","lifetime_ms":60_000,"window_ms":1000,
            "targets":[{"type":"job","id":"missing_prompt_fixture_job"}]
        }))
        .await;
    let acknowledged = fixture
        .public_await(json!({
            "wait_id":durable["structuredContent"]["wait_id"],"after":1
        }))
        .await;
    assert_eq!(acknowledged["structuredContent"]["acknowledged"], true);

    for response in [&ordinary, &durable, &acknowledged] {
        let value = &response["structuredContent"];
        assert_eq!(value["status"], "unknown_target");
        assert_eq!(value["prompt_hooks"][0]["provenance"], "agent_timer");
        assert_eq!(
            value["prompt_hooks"][0]["client_id"],
            fixture.session.client_id()
        );
        assert_eq!(value["prompt_hooks"][0]["agent_id"], fixture.agent_id);
        assert_eq!(value["prompt_hooks"][0]["event"], event);
        assert!(value.get("user_messages").is_none());
        assert!(value.get("question_answers").is_none());
        assert!(value.get("native_messages").is_none());
    }
    // ACKing the durable observation only releases that observation's body.
    // All four timer operations, including bootstrap, were exact reads.
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 4);
    assert_eq!(fixture.message_status(&mailbox_id).await, "queued");
    fixture.assert_summary_observation_did_not_consume_questions();
}

pub(super) fn timer_event() -> Value {
    json!({
        "event_id":"prompt_fixture_event_1","revision":1,"timer_id":"prompt_fixture",
        "provenance":"agent_timer","completion_permission":false,
        "scheduled_at_unix_ms":1000,"fired_at_unix_ms":1250,"delay_ms":250,
        "tick":1,"missed":0,"body":"Inspect the original job; never replay an unknown operation."
    })
}

#[tokio::test]
async fn idle_member_avoids_empty_probes_and_lost_schedule_receipt_keeps_delivery_live() {
    let fixture = Fixture::with_timing(true).await;
    let empty = json!({"isError":false,"structuredContent":{
        "schema_version":1,"scope":"native_connection","provenance":"agent_timer",
        "completion_permission":false,"ack_required":true,"more":false,"visibility_deferred":0,
        "member_has_no_prompt_timers":true,"events":[]
    }});
    fixture.native.lock().unwrap().prompt_reply = Some(empty.clone());
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    let observe =
        || fixture.public_await(json!({"targets":[{"type":"job","id":"missing_idle_probe"}]}));
    // First eligible response reconciles a fresh connection with the actual native clock.
    observe().await;
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 2);
    for _ in 0..32 {
        let run = fixture.run();
        fixture
            .native_call(run.clone(), "ssmcp.fs.stat")
            .await
            .unwrap();
        let response = finish_user_response(&fixture.report(run), original()).await;
        assert!(response["structuredContent"].get("result").is_some());
        assert!(response["structuredContent"].get("prompt_hooks").is_none());
    }
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 2);
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    assert_eq!(
        fixture.native.lock().unwrap().prompt_reads,
        3,
        "explicit read remains available"
    );

    // Simulate a dispatched schedule whose native receipt cannot confirm its outcome.
    // An empty read before its delayed commit must not disarm the next probe.
    let host = code_host(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.run(),
        None,
        "lost schedule receipt".into(),
        Default::default(),
    );
    let scheduled = tokio::spawn(host(HostRequest::Call {
        client_id: fixture.session.client_id().into(),
        name: prompt_hooks::TOOL.into(),
        arguments: json!({"action":"schedule","request_id":"late",
        "after_ms":1,"template":"inspect"}),
    }));
    timeout(Duration::from_secs(3), async {
        while fixture.native.lock().unwrap().held_replies.is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    observe().await;
    scheduled.abort();
    assert!(scheduled.await.unwrap_err().is_cancelled());
    fixture.release_held.send(()).unwrap();
    let mut due = empty;
    let body = due["structuredContent"].as_object_mut().unwrap();
    body.remove("member_has_no_prompt_timers");
    body.insert("events".into(), json!([timer_event()]));
    fixture.native.lock().unwrap().prompt_reply = Some(due);
    for _ in 0..2 {
        assert_eq!(
            observe().await["structuredContent"]["prompt_hooks"][0]["event"],
            timer_event()
        );
    }
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 6);
}

#[tokio::test]
async fn unhinted_legacy_empty_reads_are_reconciled_on_each_response() {
    let fixture = Fixture::with_timing(true).await;
    fixture.native.lock().unwrap().prompt_reply = Some(
        json!({"isError":false,"structuredContent":{
            "schema_version":1,"scope":"native_connection","provenance":"agent_timer",
            "completion_permission":false,"ack_required":true,"more":false,"visibility_deferred":0,"events":[]
        }}),
    );
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    fixture
        .public_await(json!({"targets":[{"type":"job","id":"missing_legacy_prompt"}]}))
        .await;
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 2);
    for _ in 0..32 {
        let run = fixture.run();
        fixture
            .native_call(run.clone(), "ssmcp.fs.stat")
            .await
            .unwrap();
        finish_user_response(&fixture.report(run), original()).await;
    }
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 34);
}

pub(super) fn clock_reply(mut payload: Value) -> Value {
    let obj = payload.as_object_mut().unwrap();
    obj.insert("schema_version".into(), json!(1));
    obj.insert("scope".into(), json!("native_connection"));
    obj.insert("provenance".into(), json!("agent_timer"));
    obj.insert("completion_permission".into(), json!(false));
    json!({"isError":false,"structuredContent":payload})
}

pub(super) fn read_reply(events: Value, idle: bool) -> Value {
    let mut payload =
        json!({"events":events,"more":false,"ack_required":true,"visibility_deferred":0});
    if idle {
        payload["member_prompt_delivery_idle"] = json!(true);
    }
    clock_reply(payload)
}

#[tokio::test]
async fn confirmed_schedule_and_ack_return_member_to_quiet_delivery() {
    let fixture = Fixture::with_timing(true).await;
    fixture.native.lock().unwrap().prompt_reply = Some(read_reply(json!([]), true));
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    let observe =
        || fixture.public_await(json!({"targets":[{"type":"job","id":"missing_cycle_job"}]}));
    observe().await;
    let host = code_host(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.run(),
        None,
        "confirmed schedule cycle".into(),
        Default::default(),
    );
    let action = |arguments| {
        host(HostRequest::Call {
            client_id: fixture.session.client_id().into(),
            name: prompt_hooks::TOOL.into(),
            arguments,
        })
    };
    action(json!({"action":"schedule","request_id":"cycle","after_ms":0,"template":"inspect"}))
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            observe().await["structuredContent"]["prompt_hooks"][0]["event"],
            timer_event()
        );
    }
    action(json!({"action":"ack","timer_id":"prompt_fixture","event_id":"prompt_fixture_event_1","revision":1})).await.unwrap();
    observe().await;
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 5);
    for _ in 0..32 {
        let run = fixture.run();
        fixture
            .native_call(run.clone(), "ssmcp.fs.stat")
            .await
            .unwrap();
        finish_user_response(&fixture.report(run), original()).await;
    }
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 5);
}

#[tokio::test]
async fn a_rejected_clock_lookup_settles_only_its_own_pending_mutation() {
    let fixture = Fixture::with_timing(true).await;
    fixture.native.lock().unwrap().prompt_reply = Some(read_reply(json!([]), true));
    fixture
        .native_call(fixture.run(), prompt_hooks::TOOL)
        .await
        .unwrap();
    let observe =
        || fixture.public_await(json!({"targets":[{"type":"job","id":"lookup_failure_case"}]}));
    observe().await;
    let host = code_host(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.run(),
        None,
        "rejected prompt lookup".into(),
        Default::default(),
    );
    let cancel = |id: &str| {
        host(HostRequest::Call {
            client_id: fixture.session.client_id().into(),
            name: prompt_hooks::TOOL.into(),
            arguments: json!({"action":"cancel","timer_id":id}),
        })
    };
    let missing = cancel("missing").await.unwrap_err();
    assert_eq!(missing, crate::runtime::time::UNKNOWN_PROMPT_TIMER);
    let error = code_mode::tool_result(Err(missing));
    assert_eq!(error["isError"], true);
    assert_eq!(
        error["content"][0]["text"],
        crate::runtime::time::UNKNOWN_PROMPT_TIMER
    );
    for _ in 0..8 {
        observe().await;
    }
    // Bootstrap and initial reconciliation (2), one reconciliation after the
    // rejection, and no empty reads on the following seven outer responses.
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 3);

    assert_eq!(
        cancel("unknown_outcome").await.unwrap_err(),
        "prompt worker stopped; outcome unknown"
    );
    assert_eq!(
        cancel("missing").await.unwrap_err(),
        crate::runtime::time::UNKNOWN_PROMPT_TIMER
    );
    for _ in 0..3 {
        observe().await;
    }
    // A second definitive rejection cannot settle a different unknown dispatch.
    assert_eq!(fixture.native.lock().unwrap().prompt_reads, 6);
}
