//! Native transport capacity must not turn a passive observation into a final result.
use super::*;

async fn observe(fixture: &Fixture, mut arguments: Value) -> Value {
    arguments["client_id"] = json!(fixture.session.client_id());
    arguments["agent"] = json!({"id":fixture.agent_id,"token":fixture.agent_token});
    let arguments = await_tool::Arguments::parse(arguments).unwrap();
    timeout(
        Duration::from_secs(5),
        await_tool::execute(
            fixture.broker.clone(),
            "mcp".into(),
            fixture.token.clone(),
            arguments,
        ),
    )
    .await
    .expect("the authenticated capacity observation must return")
    .unwrap()
}

#[tokio::test]
async fn native_capacity_busy_resumes_the_same_wait_after_authenticated_frames_are_released() {
    let fixture = Fixture::new().await;
    fixture.native.lock().unwrap().pending = false;
    // Resolve the real connection's optional thread scope before occupying
    // every native slot. This does not use the process-global UI or a JS worker.
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
        loop {
            if fixture.native.lock().unwrap().held_replies.len() == wire::MAX_INFLIGHT {
                break;
            }
            assert!(
                !fixture.peer.is_finished(),
                "the authenticated native peer must remain alive"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("all native slots must have received a real authenticated Call frame");
    assert_eq!(held.len(), wire::MAX_INFLIGHT);

    let target = json!({"type":"exec","job_id":"exec_capacity_fixture"});
    let first = observe(&fixture, json!({"targets":[target],"timeout_ms":1000})).await;
    let data = &first["structuredContent"];
    assert_eq!(data["status"], "busy");
    assert_eq!(data["results"][0]["status"], "busy");
    assert_eq!(data["results"][0]["target"], target);
    assert_eq!(
        data["results"][0]["error"]["message"],
        "client busy; retry later"
    );
    assert!(data["completion_permission"].is_null());
    assert!(first["user_messages"].is_null());
    let wait_id = data["wait_id"]
        .as_str()
        .expect("a retryable server-issued wait ID")
        .to_owned();
    assert!(wait_id.starts_with("await_"));
    {
        let state = fixture.native.lock().unwrap();
        assert_eq!(state.held_replies.len(), wire::MAX_INFLIGHT);
        assert!(
            !state.calls.iter().any(|name| name == "ssmcp.exec.wait"),
            "capacity rejection occurs before the observed operation is dispatched"
        );
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
    .expect("all held calls must complete and release their actual Session permits");
    assert!(fixture.native.lock().unwrap().held_replies.is_empty());

    let resumed = observe(&fixture, json!({"wait_id":wait_id,"timeout_ms":1000})).await;
    let data = &resumed["structuredContent"];
    assert_eq!(data["wait_id"], wait_id);
    assert_eq!(data["status"], "ready");
    assert_eq!(data["results"][0]["status"], "ready");
    assert_eq!(data["results"][0]["target"], target);
    assert_eq!(
        data["results"][0]["data"]["job_id"],
        "exec_capacity_fixture"
    );
    assert_eq!(data["results"][0]["data"]["running"], false);
    assert!(data["completion_permission"].is_null());
    assert!(resumed["user_messages"].is_null());

    let completed = observe(&fixture, json!({"wait_id":wait_id,"timeout_ms":1000})).await;
    let data = &completed["structuredContent"];
    assert_eq!(data["wait_id"], wait_id);
    assert_eq!(data["status"], "unavailable");
    assert_eq!(
        data["results"][0]["error"]["code"],
        "await_already_resolved"
    );
    let state = fixture.native.lock().unwrap();
    assert_eq!(
        state
            .calls
            .iter()
            .filter(|name| *name == "ssmcp.exec.wait")
            .count(),
        1
    );
    assert!(state.acknowledgements.is_empty());
    assert!(state.finish_operations.is_empty());
    assert!(!fixture.peer.is_finished());
}
