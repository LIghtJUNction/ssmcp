//! Real authenticated websocket coverage for optional, passive wait display.
use super::*;

fn start(fixture: &Fixture, arguments: Value) -> JoinHandle<Value> {
    let arguments = fixture.await_arguments(arguments);
    let broker = fixture.broker.clone();
    let token = fixture.token.clone();
    tokio::spawn(async move {
        timeout(
            Duration::from_secs(8),
            await_tool::execute(broker, "mcp".into(), token, arguments),
        )
        .await
        .expect("owned await must remain bounded")
        .unwrap()
    })
}

fn target(fixture: &Fixture) -> Value {
    json!({"type":"agent_status","agent_id":fixture.agent_id,"statuses":["retiring"]})
}

async fn snapshot(
    fixture: &Fixture,
    predicate: impl Fn(&wire::wait_display::Snapshot) -> bool,
) -> wire::wait_display::Snapshot {
    timeout(Duration::from_secs(3), async {
        loop {
            if let Some(frame) = fixture
                .native
                .lock()
                .unwrap()
                .display_frames
                .iter()
                .rev()
                .find(|frame| predicate(frame))
                .cloned()
            {
                return frame;
            }
            assert!(
                !fixture.peer.is_finished(),
                "authenticated display peer must remain alive"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the real socket must deliver the current display snapshot")
}

fn passive(response: &Value) {
    let value = &response["structuredContent"];
    assert!(value["completion_permission"].is_null());
    for key in [
        "user_messages",
        "native_messages",
        "question_answers",
        "tool_summary_reminders",
    ] {
        assert!(value.get(key).is_none(), "{key}: {response}");
    }
}

#[tokio::test]
async fn public_wait_display_works_with_every_native_slot_held_without_calls_or_delivery() {
    let fixture = Fixture::with_timing(true).await;
    let peer = fixture
        .session
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"wait-display-peer"}))
        .await
        .unwrap();
    let peer_id = peer["agent_id"].as_str().unwrap();
    let peer_token = peer["token"].as_str().unwrap();
    // Resolve the optional native thread scope before occupying actual permits.
    fixture
        .native_call(fixture.run(), "fixture.actual")
        .await
        .unwrap();
    let authorization = fixture
        .broker
        .authenticate("mcp", &fixture.token)
        .await
        .unwrap();
    let mut held = tokio::task::JoinSet::new();
    for slot in 0..wire::MAX_INFLIGHT {
        let session = fixture.session.clone();
        let authorization = authorization.clone();
        let run = fixture.run();
        held.spawn(async move {
            let result = code_mode::AUDIT_RUN
                .scope(
                    run,
                    session.call_with_authorization(
                        Operation::CallTool {
                            name: "fixture.hold".into(),
                            arguments: json!({"slot":slot}),
                        },
                        None,
                        &authorization,
                    ),
                )
                .await;
            (slot, result)
        });
    }
    timeout(Duration::from_secs(5), async {
        while fixture.native.lock().unwrap().held_replies.len() != wire::MAX_INFLIGHT {
            assert!(!fixture.peer.is_finished());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("all actual native Call frames must hold their permits");
    fixture.publish_summary_requirement();
    let message_id = fixture.send_message(0).await;
    let calls = fixture.native.lock().unwrap().calls.clone();
    let question = fixture.native.lock().unwrap().view.clone();

    let waiting = start(
        &fixture,
        json!({"targets":[{"type":"agent_status","agent_id":peer_id,"statuses":["working"]}],"timeout_ms":5000}),
    );
    let frame = snapshot(&fixture, |frame| frame.active.len() == 1).await;
    assert!(
        !waiting.is_finished(),
        "the target must actually be waiting when its clock appears"
    );
    let display = &frame.active[0];
    assert_eq!(display.targets, vec![wire::wait_display::Kind::AgentStatus]);
    assert_eq!(display.target_labels.len(), 1);
    assert!(display.target_labels[0].contains("requested status"));
    assert!(display.target_details[0].contains(peer_id));
    assert!(display.target_details[0].contains("working"));
    assert_eq!(display.mode, wire::wait_display::Mode::Any);
    assert_eq!(display.outcome, wire::wait_display::Outcome::Pending);
    assert_eq!(display.window_ms, 5000);
    assert_eq!(display.attribution.account.subject, "42");
    assert_eq!(
        display.attribution.agent.as_ref().unwrap().id,
        fixture.agent_id
    );
    assert!((1..=5000).contains(&display.remaining_ms));
    let encoded = serde_json::to_string(&frame).unwrap();
    assert!(!encoded.contains(&fixture.token));
    assert!(!encoded.contains(&fixture.agent_token));
    assert!(!encoded.contains("completion_permission"));
    let lifecycle = fixture
        .session
        .collaboration
        .lifecycle_snapshot(&fixture.owner, peer_id, peer_token)
        .unwrap();
    fixture
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "update",
            json!({
                "agent_id":peer_id,"token":peer_token,"status":"working",
                "expected_revision":lifecycle["lifecycle"]["revision"]
            }),
        )
        .await
        .unwrap();
    let response = waiting.await.unwrap();
    assert_eq!(response["structuredContent"]["status"], "ready");
    assert_eq!(response["structuredContent"]["wait_id"], display.wait_id);
    passive(&response);
    let ended = snapshot(&fixture, |ended| {
        ended.revision > frame.revision && ended.active.is_empty()
    })
    .await;
    assert_eq!(ended.recent.len(), 1);
    assert_eq!(
        ended.recent[0].outcome,
        wire::wait_display::Outcome::Triggered
    );
    fixture.assert_summary_observation_did_not_consume_questions();
    assert_eq!(fixture.message_status(&message_id).await, "queued");
    {
        let state = fixture.native.lock().unwrap();
        assert_eq!(
            state.calls, calls,
            "the entire public await and finalization add zero native calls"
        );
        assert_eq!(state.held_replies.len(), wire::MAX_INFLIGHT);
        assert_eq!(state.summary_calls, 0);
        assert!(!state.summary_reminded);
        assert_eq!(state.view, question);
        assert!(state.finish_operations.is_empty());
    }
    fixture.release_held.send(()).unwrap();
    timeout(Duration::from_secs(5), async {
        let mut completed = std::collections::BTreeSet::new();
        while let Some(result) = held.join_next().await {
            let (slot, reply) = result.unwrap();
            let reply = reply.unwrap();
            assert_eq!(reply["structuredContent"]["released"], true);
            assert_eq!(reply["structuredContent"]["slot"], slot);
            assert!(completed.insert(slot));
        }
        assert_eq!(completed.len(), wire::MAX_INFLIGHT);
    })
    .await
    .expect("every owned held call must release its original permit");
    assert_eq!(fixture.native.lock().unwrap().calls, calls);
}

#[tokio::test]
async fn public_wait_display_timeout_retry_busy_and_cancel_keep_exact_episodes_passive() {
    let fixture = Fixture::with_timing(true).await;
    fixture.publish_summary_requirement();
    let message_id = fixture.send_message(0).await;
    let first = start(
        &fixture,
        json!({"targets":[target(&fixture)],"timeout_ms":1000}),
    );
    let frame = snapshot(&fixture, |frame| frame.active.len() == 1).await;
    let original = frame.active[0].clone();
    let timed_out = first.await.unwrap();
    assert_eq!(timed_out["structuredContent"]["status"], "timed_out");
    assert_eq!(timed_out["structuredContent"]["wait_id"], original.wait_id);
    passive(&timed_out);
    snapshot(&fixture, |frame| {
        frame.revision > 1 && frame.active.is_empty()
    })
    .await;

    let resumed = start(
        &fixture,
        json!({"wait_id":original.wait_id,"timeout_ms":5000}),
    );
    let next = snapshot(&fixture, |frame| {
        frame.active.first().is_some_and(|entry| {
            entry.wait_id == original.wait_id && entry.episode > original.episode
        })
    })
    .await;
    let busy = fixture
        .public_await(json!({"wait_id":original.wait_id,"timeout_ms":1000}))
        .await;
    assert_eq!(busy["structuredContent"]["status"], "busy");
    passive(&busy);
    assert!(!resumed.is_finished());
    assert_eq!(
        fixture.native.lock().unwrap().display_frames.last(),
        Some(&next),
        "a rejected concurrent observer cannot remove or replace the admitted episode"
    );
    let cancelled = fixture
        .public_await(json!({"wait_id":original.wait_id,"op":"cancel"}))
        .await;
    assert_eq!(cancelled["structuredContent"]["status"], "cancelled");
    passive(&cancelled);
    let response = resumed.await.unwrap();
    assert_eq!(response["structuredContent"]["status"], "cancelled");
    passive(&response);
    snapshot(&fixture, |frame| {
        frame.revision > next.revision && frame.active.is_empty()
    })
    .await;
    fixture.assert_summary_observation_did_not_consume_questions();
    assert_eq!(fixture.message_status(&message_id).await, "queued");
    let state = fixture.native.lock().unwrap();
    assert!(state.calls.is_empty());
    assert_eq!(state.summary_calls, 0);
    assert!(state.finish_operations.is_empty());
}

#[tokio::test]
async fn legacy_wait_peer_gets_no_display_frames_and_caller_flags_cannot_enable_them() {
    let fixture = Fixture::with_timing(false).await;
    let mut forged = json!({"client_id":fixture.session.client_id(),
        "agent":{"id":fixture.agent_id,"token":fixture.agent_token},
        "targets":[target(&fixture)],"timeout_ms":1000,"wait_display_v1":true});
    assert!(await_tool::Arguments::parse(forged.clone()).is_err());
    forged.as_object_mut().unwrap().remove("wait_display_v1");
    let response = fixture.public_await(forged).await;
    assert_eq!(response["structuredContent"]["status"], "timed_out");
    passive(&response);
    let state = fixture.native.lock().unwrap();
    assert!(state.display_frames.is_empty());
    assert!(state.calls.is_empty());
    assert!(!fixture.peer.is_finished());
}

#[tokio::test]
async fn display_authority_revocation_and_disconnect_end_only_the_observation() {
    for revoked in ["oauth", "agent", "connection"] {
        let fixture = Fixture::with_timing(true).await;
        let waiting = start(
            &fixture,
            json!({"targets":[target(&fixture)],"timeout_ms":5000}),
        );
        let frame = snapshot(&fixture, |frame| frame.active.len() == 1).await;
        match revoked {
            "oauth" => fixture.broker.oauth.revoke_subject("42").unwrap(),
            "agent" => {
                fixture
                    .session
                    .collaboration
                    .dispatch(
                        &fixture.owner,
                        "leave",
                        json!({"agent_id":fixture.agent_id,"token":fixture.agent_token}),
                    )
                    .await
                    .unwrap();
            }
            "connection" => fixture.broker.disconnect_all().await,
            _ => unreachable!(),
        }
        let result = waiting.await.unwrap();
        assert_eq!(
            result["structuredContent"]["status"], "unauthorized",
            "{revoked}: {result}"
        );
        passive(&result);
        if revoked != "connection" {
            snapshot(&fixture, |after| {
                after.revision > frame.revision && after.active.is_empty()
            })
            .await;
        } else {
            assert!(!fixture.session.is_alive());
        }
        let state = fixture.native.lock().unwrap();
        assert!(state.calls.is_empty(), "{revoked}");
        assert!(state.pending);
        assert!(state.acknowledgements.is_empty());
        assert!(state.finish_operations.is_empty());
    }
}

fn event_arguments(fixture: &Fixture, mut arguments: Value) -> Value {
    arguments["client_id"] = json!(fixture.session.client_id());
    arguments["agent_id"] = json!(fixture.agent_id);
    arguments["token"] = json!(fixture.agent_token);
    arguments
}

async fn event_request(fixture: &Fixture, arguments: Value) -> Value {
    crate::http::events_tool::dispatch(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        event_arguments(fixture, arguments),
    )
    .await
    .unwrap()
}

fn start_event_wait(fixture: &Fixture, arguments: Value) -> JoinHandle<Value> {
    let arguments = event_arguments(fixture, arguments);
    let broker = fixture.broker.clone();
    let token = fixture.token.clone();
    tokio::spawn(async move {
        timeout(
            Duration::from_secs(8),
            crate::http::events_tool::dispatch(broker, "mcp".into(), token, arguments),
        )
        .await
        .expect("owned event wait must remain bounded")
        .unwrap()
    })
}

#[tokio::test]
async fn standalone_event_wait_displays_only_authorized_conditions_and_actual_outcomes() {
    let fixture = Fixture::with_timing(true).await;
    fixture.publish_summary_requirement();
    let message_id = fixture.send_message(0).await;
    let subscription = event_request(
        &fixture,
        json!({"action":"subscribe","request_id":"display_events",
            "filter":{"kinds":["custom.wait_display"]}}),
    )
    .await;
    let subscription_id = subscription["subscription_id"].as_str().unwrap();
    let waiting = start_event_wait(
        &fixture,
        json!({"action":"wait","subscription_id":subscription_id,"wait_ms":500}),
    );
    let first = snapshot(&fixture, |frame| frame.active.len() == 1).await;
    let entry = first.active[0].clone();
    assert_eq!(entry.targets, vec![wire::wait_display::Kind::Event]);
    assert_eq!(entry.mode, wire::wait_display::Mode::Single);
    assert_eq!(entry.operation, "events.wait");
    assert_eq!(entry.window_ms, 500);
    assert!(entry.target_details[0].contains(subscription_id));
    let pending = waiting.await.unwrap();
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["observation_id"], entry.wait_id);
    snapshot(&fixture, |frame| {
        frame.recent.iter().any(|ended| {
            ended.wait_id == entry.wait_id
                && ended.outcome == wire::wait_display::Outcome::WindowExpired
        })
    })
    .await;
    let waiting = start_event_wait(
        &fixture,
        json!({"action":"wait","subscription_id":subscription_id,"wait_ms":5000}),
    );
    let active = snapshot(&fixture, |frame| {
        frame
            .active
            .iter()
            .any(|active| active.wait_id != entry.wait_id)
    })
    .await;
    let next_id = active.active[0].wait_id.clone();
    event_request(
        &fixture,
        json!({"action":"publish","request_id":"display_event_trigger",
            "name":"custom.wait_display","data":{"message":"private event body fixture"}}),
    )
    .await;
    let ready = waiting.await.unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["observation_id"], next_id);
    assert!(!ready["events"].as_array().unwrap().is_empty());
    let ended = snapshot(&fixture, |frame| {
        frame.recent.iter().any(|ended| {
            ended.wait_id == next_id && ended.outcome == wire::wait_display::Outcome::Triggered
        })
    })
    .await;
    let encoded = serde_json::to_string(&ended).unwrap();
    for private in [
        fixture.token.as_str(),
        fixture.agent_token.as_str(),
        "private event body fixture",
    ] {
        assert!(!encoded.contains(private));
    }
    fixture.assert_summary_observation_did_not_consume_questions();
    assert_eq!(fixture.message_status(&message_id).await, "queued");
    assert!(
        fixture
            .native
            .lock()
            .unwrap()
            .calls
            .iter()
            .all(|name| matches!(
                name.as_str(),
                "ssmcp.client.capabilities" | "ssmcp.worker.events"
            ))
    );
    let before = fixture.native.lock().unwrap().display_frames.len();
    let denied = crate::http::events_tool::dispatch(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        event_arguments(
            &fixture,
            json!({"action":"wait","subscription_id":"unowned_subscription","wait_ms":5000}),
        ),
    )
    .await;
    assert!(denied.is_err());
    assert_eq!(fixture.native.lock().unwrap().display_frames.len(), before);
}
