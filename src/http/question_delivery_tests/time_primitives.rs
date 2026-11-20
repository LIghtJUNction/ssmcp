//! Real authenticated Session/Frame delivery boundaries for time-only runs.
use super::*;

fn run(fixture: &Fixture, bound: bool) -> code_mode::AuditRun {
    if bound {
        return fixture.run();
    }
    let run =
        code_mode::AuditRun::new("return await call(id, 'ssmcp.time.now', {});", None).unwrap();
    run.record_connection(&fixture.session).unwrap();
    run
}

fn publish(fixture: &Fixture, bound: bool) {
    fixture
        .session
        .steering
        .command(
            fixture.session.client_id(),
            fixture.session.collaboration.presence().unwrap(),
            wire::SteeringCommand::Publish {
                command_id: "publish-time-requirement".into(),
                message_id: "time-requirement".into(),
                expected_revision: 0,
                text: "Keep this exact user text queued during time-only work.".into(),
                target: if bound {
                    fixture.agent_id.clone()
                } else {
                    "next".into()
                },
            },
        )
        .unwrap();
}

fn requirement(fixture: &Fixture) -> wire::SteeringMessage {
    fixture
        .session
        .steering
        .command(
            fixture.session.client_id(),
            vec![],
            wire::SteeringCommand::Snapshot {},
        )
        .unwrap()
        .messages
        .into_iter()
        .find(|message| message.id == "time-requirement")
        .unwrap()
}

async fn inbox(fixture: &Fixture, bound: bool) -> String {
    let recipient = if bound {
        format!("agent:{}:{}", fixture.session.client_id(), fixture.agent_id)
    } else {
        format!("client:{}", fixture.session.client_id())
    };
    let result = fixture.broker.message_request("mcp", &fixture.token, json!({
        "client_id":fixture.session.client_id(),"action":"send","recipient_id":recipient,
        "request_id":"time-only-inbox","text":"Do not consume this native inbox item while timing."
    })).await.unwrap();
    result["receipts"][0]["message_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn time_only_authenticated_frames_preserve_bound_and_unbound_messages_answers_and_finish() {
    for bound in [false, true] {
        let fixture = Fixture::new().await;
        publish(&fixture, bound);
        let queued = requirement(&fixture);
        let message_id = inbox(&fixture, bound).await;
        let question = fixture.native.lock().unwrap().view.clone();
        let audit = run(&fixture, bound);
        for tool in ["ssmcp.time.now", "ssmcp.time.wait"] {
            let native = fixture.native_call(audit.clone(), tool).await.unwrap();
            assert_eq!(native["structuredContent"]["schema_version"], 1);
        }
        assert!(audit.time_observation_only(fixture.session.client_id()));
        let response = finish_user_response(&fixture.report(audit), original()).await;
        for key in ["user_messages", "native_messages", "question_answers"] {
            assert!(
                response["structuredContent"].get(key).is_none(),
                "{bound}/{key}: {response}"
            );
        }
        assert_eq!(requirement(&fixture), queued);
        assert_eq!(fixture.message_status(&message_id).await, "queued");
        {
            let state = fixture.native.lock().unwrap();
            assert!(state.pending);
            assert_eq!(state.view, question);
            assert!(state.acknowledgements.is_empty());
            assert!(state.finish_operations.is_empty());
            assert!(!state.calls.iter().any(|name| matches!(
                name.as_str(),
                "ssmcp.ui.question_delivery" | "ssmcp.ui.question_ack"
            )));
        }

        // A retained finish request is a different object; no clock result may
        // turn its pending state into a decision or call the finish endpoint.
        let finish = json!({"id":"retained_finish","kind":"finish","state":"pending","answer":null,
            "answer_actor":null,"answer_revision":0,"finish":{"status":"pending","decision":null,"approved":null}});
        fixture.native.lock().unwrap().view = finish.clone();
        let audit = run(&fixture, bound);
        fixture
            .native_call(audit.clone(), "ssmcp.time.wait")
            .await
            .unwrap();
        finish_user_response(&fixture.report(audit), original()).await;
        assert_eq!(fixture.native.lock().unwrap().view, finish);
        assert!(fixture.native.lock().unwrap().finish_operations.is_empty());
        assert_eq!(requirement(&fixture), queued);
        assert_eq!(fixture.message_status(&message_id).await, "queued");
    }
}

#[tokio::test]
async fn failed_time_calls_and_their_real_summary_probe_keep_all_delivery_passive() {
    for bound in [false, true] {
        let fixture = Fixture::new().await;
        publish(&fixture, bound);
        let queued = requirement(&fixture);
        let message_id = inbox(&fixture, bound).await;
        let question = fixture.native.lock().unwrap().view.clone();
        let audit = run(&fixture, bound);
        let host = code_host(
            fixture.broker.clone(),
            "mcp".into(),
            fixture.token.clone(),
            audit.clone(),
            None,
            "time error fixture".into(),
        );
        let failure = host(HostRequest::Call {
            client_id: fixture.session.client_id().into(),
            name: "ssmcp.time.wait".into(),
            arguments: json!({"op":"status","timer_id":"missing_timer"}),
        })
        .await
        .unwrap_err();
        assert_eq!(
            failure,
            "time timer is unknown, expired, or belongs to another caller"
        );
        assert!(audit.time_observation_only(fixture.session.client_id()));
        let original = code_mode::tool_result(Err(failure));
        let report = fixture.report(audit.clone());
        let response = finish_user_response(&report, original.clone()).await;
        assert_eq!(response["isError"], true);
        assert_eq!(response["content"][0], original["content"][0]);
        for key in ["user_messages", "native_messages", "question_answers"] {
            assert!(
                response["structuredContent"].get(key).is_none(),
                "{bound}/{key}: {response}"
            );
        }
        assert!(audit.time_observation_only(fixture.session.client_id()));
        assert!(audit.steering_targets().is_empty());
        assert!(fixture.session.tool_summary_advertised());
        assert_eq!(requirement(&fixture), queued);
        assert_eq!(fixture.message_status(&message_id).await, "queued");
        // Finalizing again neither probes twice nor replays the failed action.
        finish_user_response(&report, original).await;
        assert_eq!(requirement(&fixture), queued);
        assert_eq!(fixture.message_status(&message_id).await, "queued");
        let state = fixture.native.lock().unwrap();
        assert_eq!(
            state
                .calls
                .iter()
                .filter(|name| *name == "ssmcp.time.wait")
                .count(),
            1
        );
        assert_eq!(
            state
                .calls
                .iter()
                .filter(|name| *name == "ssmcp.ui.tool_summary")
                .count(),
            1
        );
        assert!(state.pending);
        assert_eq!(state.view, question);
        assert!(state.acknowledgements.is_empty());
        assert!(state.finish_operations.is_empty());
        assert!(!state.calls.iter().any(|name| matches!(
            name.as_str(),
            "ssmcp.ui.question_delivery" | "ssmcp.ui.question_ack"
        )));
        assert!(!fixture.peer.is_finished());
    }
}

#[tokio::test]
async fn mixed_time_and_real_native_work_deliver_normally_in_both_execution_orders() {
    for time_first in [false, true] {
        let fixture = Fixture::new().await;
        publish(&fixture, true);
        let message_id = inbox(&fixture, true).await;
        let audit = fixture.run();
        if time_first {
            fixture
                .native_call(audit.clone(), "ssmcp.time.wait")
                .await
                .unwrap();
        }
        fixture
            .native_call(audit.clone(), "fixture.actual")
            .await
            .unwrap();
        fixture
            .native_call(audit.clone(), "ssmcp.time.now")
            .await
            .unwrap();
        assert!(!audit.time_observation_only(fixture.session.client_id()));
        let response = finish_user_response(&fixture.report(audit), original()).await;
        assert_eq!(requirement(&fixture).state, wire::SteeringState::Delivered);
        assert_eq!(
            response["structuredContent"]["user_messages"][0]["message_id"],
            "time-requirement"
        );
        assert_eq!(
            response["structuredContent"]["question_answers"][0]["id"],
            "retained_question"
        );
        assert!(
            response["structuredContent"]["native_messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty())
        );
        assert_eq!(fixture.message_status(&message_id).await, "included");
        fixture.wait_for_ack().await;
        assert_eq!(fixture.native.lock().unwrap().acknowledgements.len(), 1);
        assert!(fixture.native.lock().unwrap().finish_operations.is_empty());
    }
}
