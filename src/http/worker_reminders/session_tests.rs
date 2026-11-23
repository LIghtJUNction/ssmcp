//! Exercise the actual broker/session channel and response authority boundary.
//! This reuses the local authenticated fixture; no JS runner or model is started.
use super::*;
use crate::{broker::session_io::TestFixture, wire};

fn original() -> Value {
    json!({"isError":true,"content":[{"type":"text","text":"original native error"}],
        "structuredContent":{"result":{"exit_code":9},"user_messages":[{"id":"existing","text":"original input"}]}})
}

fn without_input() -> Value {
    let mut response = original();
    response["structuredContent"]
        .as_object_mut()
        .unwrap()
        .remove("user_messages");
    response
}

fn successful() -> Value {
    json!({"isError":false,"content":[{"type":"text","text":"observed fixture result"}],
        "structuredContent":{"result":{"ready":true}}})
}

async fn configure_dreaming(target: &EligibleUserResponse) -> String {
    let binding = target.binding.as_ref().unwrap();
    let current = target
        .session
        .collaboration
        .lifecycle_snapshot(&binding.owner, &binding.agent_id, &binding.token)
        .unwrap();
    let updated = target
        .session
        .collaboration
        .dispatch(
            &binding.owner,
            "update",
            json!({
                "agent_id":binding.agent_id,"token":binding.token,
                "expected_revision":current["lifecycle"]["revision"],
                "status":"dreaming","maintenance_budget_ms":5000,
            }),
        )
        .await
        .unwrap();
    updated["agent"]["lifecycle"]["maintenance"]["round_id"]
        .as_str()
        .unwrap()
        .into()
}

async fn quiet_pool_reply(fixture: &TestFixture) -> Value {
    let pool = fixture
        .broker
        .test_register_identity(crate::broker::IDENTITY_RESOURCE, fixture.actor.clone())
        .await;
    let mut reply = pool_reply();
    reply["structuredContent"]["value"]["client_id"] = json!(pool.client_id());
    reply["structuredContent"]["value"]["idle"] = json!(3);
    reply["structuredContent"]["value"]["working"] = json!(0);
    reply
}

fn pool_reply() -> Value {
    json!({"isError":false,"structuredContent":{"path":"/worker_pool","value":{
        "client_id":"b".repeat(64),"process_id":42,"total":3,"desired_total":3,
        "idle":2,"working":1,"offline":0,"retiring":0,"queued":0,"registered_workers":3,
        "connected":true,"closed":false,"stopping":false}}})
}

fn assert_probe(operation: Value) {
    assert_eq!(
        operation,
        serde_json::to_value(Operation::CallTool {
            name: "ssmcp.ui.get".into(),
            arguments: json!({"path":"/worker_pool"}),
        })
        .unwrap()
    );
}

async fn prepared(role: &str) -> (TestFixture, CodeRunReport, Vec<EligibleUserResponse>) {
    let mut fixture = TestFixture::new().await;
    let user = crate::broker::IDENTITY_RESOURCE;
    let authorization = fixture
        .broker
        .authenticate(user, &fixture.access_token)
        .await
        .unwrap();
    let owner = crate::bridge::owner(user, &authorization).unwrap();
    let joined = fixture
        .session
        .collaboration
        .dispatch(
            &owner,
            "join",
            json!({
                "name":"main", "role":role,
            }),
        )
        .await
        .unwrap();
    let run = code_mode::AuditRun::new("return null;", None).unwrap();
    run.record_connection(&fixture.session).unwrap();
    run.bind(
        fixture.session.client_id(),
        &owner,
        joined["agent_id"].as_str().unwrap(),
        joined["token"].as_str().unwrap(),
    )
    .unwrap();
    let report = CodeRunReport {
        broker: fixture.broker.clone(),
        user: user.into(),
        access_token: fixture.access_token.clone(),
        run,
        delivered: true,
    };
    // Establish a real ordinary native call in this run. The reminder must not
    // be tested by fabricating program JSON or by treating a time-only read as work.
    let session = fixture.session.clone();
    let actual = code_mode::AUDIT_RUN.scope(
        report.run.clone(),
        session.call_with_authorization(
            Operation::CallTool {
                name: "ssmcp.fs.stat".into(),
                arguments: json!({"path":"."}),
            },
            None,
            &authorization,
        ),
    );
    let reply = async {
        // The first authenticated bound call negotiates native thread scope
        // before dispatching fs.stat. Reply to both distinct protocol requests.
        let capability = fixture
            .native_reply(json!({"isError":false,"structuredContent":{"thread_scope":1}}))
            .await;
        assert_eq!(
            capability,
            serde_json::to_value(Operation::CallTool {
                name: "ssmcp.client.capabilities".into(),
                arguments: json!({}),
            })
            .unwrap()
        );
        fixture
            .native_reply(
                json!({"isError":false,"structuredContent":{"path":".","kind":"directory"}}),
            )
            .await
    };
    let (result, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(actual, reply)
    })
    .await
    .unwrap();
    result.unwrap();
    assert_eq!(
        operation,
        serde_json::to_value(Operation::CallTool {
            name: "ssmcp.fs.stat".into(),
            arguments: json!({"path":"."}),
        })
        .unwrap()
    );
    assert!(!report.run.time_observation_only(session.client_id()));
    let eligible = super::super::eligible_user_response(&report).await.unwrap();
    assert_eq!(eligible.len(), 1);
    assert!(Arc::ptr_eq(
        &report.run.connection(session.client_id()).unwrap(),
        &eligible[0].session
    ));
    (fixture, report, eligible)
}

fn publish_pending(target: &EligibleUserResponse) {
    target
        .session
        .steering_command(wire::SteeringCommand::Publish {
            command_id: "publish-worker-hint-fixture".into(),
            message_id: "worker-hint-fixture".into(),
            expected_revision: 0,
            text: "Keep this genuine input queued during the passive worker probe.".into(),
            target: target.binding.as_ref().unwrap().agent_id.clone(),
        })
        .unwrap();
}

fn assert_pending(target: &EligibleUserResponse) {
    let snapshot = target
        .session
        .steering
        .command(
            target.session.client_id(),
            vec![],
            wire::SteeringCommand::Snapshot {},
        )
        .unwrap();
    let message = snapshot
        .messages
        .iter()
        .find(|message| message.id == "worker-hint-fixture")
        .unwrap();
    assert_eq!(message.state, wire::SteeringState::Queued);
}

#[tokio::test]
async fn real_session_probe_preserves_errors_and_does_not_consume_user_input() {
    let (mut fixture, report, eligible) = prepared("main").await;
    assert!(is_coordinator(&report, &eligible[0]));
    publish_pending(&eligible[0]);

    // The real target binding remains valid, but an unrelated current-run token
    // for the same public owner/agent IDs is not authority to append a reminder.
    let mismatch = CodeRunReport {
        broker: report.broker.clone(),
        user: report.user.clone(),
        access_token: report.access_token.clone(),
        run: code_mode::AuditRun::new("return null;", None).unwrap(),
        delivered: true,
    };
    let binding = eligible[0].binding.as_ref().unwrap();
    mismatch.run.record_connection(&fixture.session).unwrap();
    mismatch
        .run
        .bind(
            fixture.session.client_id(),
            &binding.owner,
            &binding.agent_id,
            &"0".repeat(64),
        )
        .unwrap();
    assert!(!is_coordinator(&mismatch, &eligible[0]));

    let before = original();
    let (appended, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            append(&report, before.clone(), &eligible),
            fixture.native_reply(pool_reply())
        )
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert_eq!(appended["isError"], before["isError"]);
    assert_eq!(appended["content"][0], before["content"][0]);
    assert_eq!(
        appended["structuredContent"]["result"],
        before["structuredContent"]["result"]
    );
    assert_eq!(
        appended["structuredContent"]["user_messages"],
        before["structuredContent"]["user_messages"]
    );
    let notice = &appended["structuredContent"]["worker_reminders"][0];
    assert_eq!(notice["client_id"], fixture.session.client_id());
    assert_eq!(notice["agent_id"], binding.agent_id);
    assert_eq!(notice["worker_client_id"], "b".repeat(64));
    assert_eq!(notice["standby"], 2);
    assert!(
        appended["structuredContent"]
            .get("coordination_hints")
            .is_none()
    );
    assert!(appended.to_string().len() - before.to_string().len() <= MAX_APPEND_BYTES);
    assert_pending(&eligible[0]);
}

#[tokio::test]
async fn real_probe_error_and_revocation_leave_the_original_response_intact() {
    let (mut fixture, report, eligible) = prepared("coordinator").await;
    let before = without_input();
    let (failed, operation) =
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(append(&report, before.clone(), &eligible), fixture.native_reply(json!({
            "isError":true,"content":[{"type":"text","text":"unknown UI path: /worker_pool"}]
        })))
        })
        .await
        .unwrap();
    assert_probe(operation);
    assert_policy_only(&failed, &before);
    let (mut fixture, report, eligible) = prepared("lead").await;
    publish_pending(&eligible[0]);
    let reply = async {
        let operation = fixture.native_reply(pool_reply()).await;
        // Same poll as sending the native response, before append can resume.
        fixture.revoke_access();
        operation
    };
    let (revoked, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(append(&report, before.clone(), &eligible), reply)
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert_eq!(revoked, before);
    assert_pending(&eligible[0]);
}

#[tokio::test]
async fn actual_role_and_total_probe_deadline_gate_session_requests() {
    let (mut fixture, report, eligible) = prepared("worker").await;
    assert!(
        !is_coordinator(&report, &eligible[0]),
        "a display name of main is not a coordinator role"
    );
    let before = original();
    assert_eq!(append(&report, before.clone(), &eligible).await, before);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(25),
            fixture.native_reply(pool_reply())
        )
        .await
        .is_err(),
        "non-coordinators must not send a worker-pool probe"
    );

    let (mut fixture, report, eligible) = prepared("controller").await;
    publish_pending(&eligible[0]);
    let timed_out = tokio::time::timeout(
        Duration::from_secs(2),
        append(&report, before.clone(), &eligible),
    )
    .await
    .unwrap();
    assert_eq!(timed_out, before);
    // The responder intentionally stayed silent. Seeing the actual queued call
    // distinguishes a bounded timeout from accidentally skipping the probe.
    let operation =
        tokio::time::timeout(Duration::from_secs(1), fixture.native_reply(pool_reply()))
            .await
            .unwrap();
    assert_probe(operation);
    assert_pending(&eligible[0]);
}

#[tokio::test]
async fn input_arriving_during_probe_preempts_dreaming_without_consuming_it() {
    let (mut fixture, report, eligible) = prepared("main").await;
    let round_id = configure_dreaming(&eligible[0]).await;
    let quiet = quiet_pool_reply(&fixture).await;
    let before = successful();
    let reply = async {
        let operation = fixture.native_reply(quiet.clone()).await;
        publish_pending(&eligible[0]);
        operation
    };
    let (blocked, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(append(&report, before.clone(), &eligible), reply)
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert!(
        blocked["structuredContent"]
            .get("coordination_hints")
            .is_none()
    );
    assert_pending(&eligible[0]);
    assert!(!blocked.to_string().contains("Keep this genuine input"));

    // Handling input does not resurrect a cancelled maintenance request.
    // A subsequent explicit authorized request receives a different round ID.
    let target = &eligible[0];
    let delivered = target.session.steering.claim(
        target.session.client_id(),
        "fixture-input-delivery",
        Some(&target.binding.as_ref().unwrap().agent_id),
        8,
        65536,
    );
    assert_eq!(delivered.len(), 1);
    let binding = target.binding.as_ref().unwrap();
    let preempted = target
        .session
        .collaboration
        .lifecycle_snapshot(&binding.owner, &binding.agent_id, &binding.token)
        .unwrap();
    assert_eq!(preempted["status"], "working");
    assert_eq!(preempted["lifecycle"]["maintenance"]["status"], "cancelled");
    let fresh_round_id = configure_dreaming(target).await;
    assert_ne!(fresh_round_id, round_id);
    let (ready, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            append(&report, before.clone(), &eligible),
            fixture.native_reply(quiet.clone())
        )
    })
    .await
    .unwrap();
    assert_probe(operation);
    let hint = &ready["structuredContent"]["coordination_hints"][0];
    assert_eq!(hint["dreaming_round"]["round_id"], fresh_round_id);
    assert_eq!(hint["dreaming_round"]["budget_ms"], 5000);
    assert_eq!(hint["provenance"], "agent_lifecycle");
    assert_eq!(hint["completion_permission"], false);
    assert!(ready.to_string().len() - before.to_string().len() <= MAX_APPEND_BYTES);

    let (repeated, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            append(&report, before.clone(), &eligible),
            fixture.native_reply(quiet)
        )
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert_eq!(repeated, before);
}

#[tokio::test]
async fn lifecycle_request_can_be_observed_without_standby_workers() {
    let (mut fixture, report, eligible) = prepared("main").await;
    let round_id = configure_dreaming(&eligible[0]).await;
    let mut available = quiet_pool_reply(&fixture).await;
    available["structuredContent"]["value"]["idle"] = json!(2);
    available["structuredContent"]["value"]["working"] = json!(1);
    let mut busy = available.clone();
    busy["structuredContent"]["value"]["idle"] = json!(0);
    busy["structuredContent"]["value"]["working"] = json!(3);
    let before = successful();
    let (blocked, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            append(&report, before.clone(), &eligible),
            fixture.native_reply(busy)
        )
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert_eq!(
        blocked["structuredContent"]["coordination_hints"][0]["dreaming_round"]["round_id"],
        round_id
    );
    assert!(
        blocked["structuredContent"]
            .get("worker_reminders")
            .is_none()
    );

    // A later capacity notice does not reissue the already delivered request.
    let (ready, operation) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            append(&report, before.clone(), &eligible),
            fixture.native_reply(available)
        )
    })
    .await
    .unwrap();
    assert_probe(operation);
    assert!(
        ready["structuredContent"]
            .get("coordination_hints")
            .is_none()
    );
    assert!(
        ready["structuredContent"]["worker_reminders"]
            .as_array()
            .is_some_and(|items| items.len() == 1)
    );
    assert!(ready.to_string().len() - before.to_string().len() <= MAX_APPEND_BYTES);
}

#[tokio::test]
async fn maintenance_uses_only_main_authority_and_yields_to_errors_or_input() {
    for failure in [
        "unavailable",
        "unauthorized",
        "native_error",
        "delivered_input",
    ] {
        let (mut fixture, report, eligible) = prepared("main").await;
        let round_id = configure_dreaming(&eligible[0]).await;
        let mut reply = quiet_pool_reply(&fixture).await;
        let mut before = successful();
        match failure {
            "unavailable" => reply = json!({"isError":true,"content":[]}),
            "unauthorized" => {
                reply["structuredContent"]["value"]["client_id"] = json!("f".repeat(64))
            }
            "native_error" => before["isError"] = json!(true),
            "delivered_input" => {
                before["structuredContent"]["user_messages"] =
                    original()["structuredContent"]["user_messages"].clone();
            }
            _ => unreachable!(),
        }
        let (after, operation) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                append(&report, before.clone(), &eligible),
                fixture.native_reply(reply)
            )
        })
        .await
        .unwrap();
        assert_probe(operation);
        let hint = &after["structuredContent"]["coordination_hints"][0];
        if matches!(failure, "unavailable" | "unauthorized") {
            assert_eq!(hint["dreaming_round"]["round_id"], round_id);
            assert_eq!(
                hint["subscription_filter"]["client_ids"],
                json!([eligible[0].session.client_id()])
            );
            assert!(after["structuredContent"].get("worker_reminders").is_none());
        } else {
            assert!(hint["dreaming_round"].is_null(), "{failure}");
        }
        assert_eq!(after["isError"], before["isError"]);
        assert_eq!(
            after["structuredContent"]["user_messages"],
            before["structuredContent"]["user_messages"]
        );
    }
}

#[tokio::test]
async fn pending_input_observation_is_recipient_scoped_and_never_claims() {
    let (fixture, _report, eligible) = prepared("main").await;
    let target = &eligible[0];
    let binding = target.binding.as_ref().unwrap();
    let other = fixture
        .session
        .collaboration
        .dispatch(
            &binding.owner,
            "join",
            json!({"name":"other","role":"worker"}),
        )
        .await
        .unwrap();
    let mut snapshot = target
        .session
        .steering
        .command(
            target.session.client_id(),
            target.session.collaboration.presence().unwrap(),
            wire::SteeringCommand::Publish {
                command_id: "other-input-publish".into(),
                message_id: "other-input".into(),
                expected_revision: 0,
                text: "private other participant input".into(),
                target: other["agent_id"].as_str().unwrap().into(),
            },
        )
        .unwrap();
    assert!(!input_pending(target, &successful()));
    assert_eq!(snapshot.messages[0].state, wire::SteeringState::Queued);
    publish_pending(target);
    assert!(input_pending(target, &successful()));
    assert_pending(target);
    snapshot = target
        .session
        .steering
        .command(
            target.session.client_id(),
            vec![],
            wire::SteeringCommand::Snapshot {},
        )
        .unwrap();
    assert_eq!(snapshot.messages.len(), 2);
    assert!(
        snapshot
            .messages
            .iter()
            .all(|message| message.state == wire::SteeringState::Queued)
    );
}

// A verified coordinator can receive generic wait policy even when a worker
// probe is unavailable. The hint never manufactures a pool or changes the
// original error/input; revoked identities above still receive no addition.
fn assert_policy_only(after: &Value, before: &Value) {
    let hints = after["structuredContent"]["coordination_hints"]
        .as_array()
        .unwrap();
    assert_eq!(hints.len(), 1);
    assert!(after["structuredContent"].get("worker_reminders").is_none());
    assert!(
        hints[0]["subscription_filter"]["client_ids"]
            .as_array()
            .unwrap()
            .len()
            == 1
    );
    let mut preserved = after.clone();
    preserved["structuredContent"]
        .as_object_mut()
        .unwrap()
        .remove("coordination_hints");
    let original_content = before["content"].as_array().unwrap();
    let content = preserved["content"].as_array_mut().unwrap();
    assert_eq!(&content[..original_content.len()], original_content);
    assert_eq!(content.len(), original_content.len() + 1);
    assert!(
        content.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("COORDINATOR WAIT POLICY")
    );
    content.truncate(original_content.len());
    assert_eq!(&preserved, before);
    assert!(after.to_string().len() - before.to_string().len() <= MAX_APPEND_BYTES);
}
