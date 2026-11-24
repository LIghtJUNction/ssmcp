//! Timed hook delivery uses the real authenticated native websocket fixture.
use super::*;

#[tokio::test]
async fn passive_await_delivers_retained_timer_hooks_without_ack_or_user_consumption() {
    let fixture = Fixture::with_timing(true).await;
    let event = json!({
        "event_id":"prompt_fixture_event_1","revision":1,"timer_id":"prompt_fixture",
        "provenance":"agent_timer","completion_permission":false,
        "scheduled_at_unix_ms":1000,"fired_at_unix_ms":1250,"delay_ms":250,
        "tick":1,"missed":0,"body":"Inspect the original job; never replay an unknown operation."
    });
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
