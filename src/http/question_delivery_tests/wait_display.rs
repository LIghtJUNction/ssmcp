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
    json!({"type":"agent_status","agent_id":fixture.agent_id,"statuses":["display-fixture-ready"]})
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
        json!({"targets":[target(&fixture)],"timeout_ms":5000}),
    );
    let frame = snapshot(&fixture, |frame| frame.active.len() == 1).await;
    assert!(
        !waiting.is_finished(),
        "the target must actually be waiting when its clock appears"
    );
    let display = &frame.active[0];
    assert_eq!(display.targets, vec![wire::wait_display::Kind::AgentStatus]);
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
    fixture.session.collaboration.dispatch(&fixture.owner, "update", json!({
        "agent_id":fixture.agent_id,"token":fixture.agent_token,"status":"display-fixture-ready"
    })).await.unwrap();
    let response = waiting.await.unwrap();
    assert_eq!(response["structuredContent"]["status"], "ready");
    assert_eq!(response["structuredContent"]["wait_id"], display.wait_id);
    passive(&response);
    snapshot(&fixture, |ended| {
        ended.revision > frame.revision && ended.active.is_empty()
    })
    .await;
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
