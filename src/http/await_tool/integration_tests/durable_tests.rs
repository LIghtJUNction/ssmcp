use super::*;

fn request(fixture: &Fixture, request_id: &str) -> Value {
    json!({"request_id":request_id,"lifetime_ms":600_000,"window_ms":55_000,
        "targets":[{"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["idle"]}]})
}

async fn created(fixture: &Fixture, request_id: &str) -> Arc<Record> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = {
                fixture
                    .broker
                    .awaiting
                    .records
                    .lock()
                    .unwrap()
                    .values()
                    .find(|record| {
                        record
                            .durable
                            .as_ref()
                            .is_some_and(|value| value.request_id == request_id)
                    })
                    .cloned()
            };
            if let Some(record) = record {
                return record;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("durable creation did not reach the registry")
}

async fn settled(record: &Record) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !record.terminal.load(Ordering::Acquire) || record.active.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("native notification did not settle the original observer");
}

#[tokio::test]
async fn dropped_http_create_completes_in_background_and_reconnect_reuses_its_snapshot() {
    let fixture = Arc::new(Fixture::new().await);
    fixture.update(&fixture.peer, "working").await;
    let worker = fixture.clone();
    let first = tokio::spawn(async move {
        worker
            .public(&worker.member, request(&worker, "lost_create"))
            .await
    });
    let record = created(&fixture, "lost_create").await;
    assert!(record.active.load(Ordering::Acquire));
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    // No await read is alive when the real collaboration notification arrives.
    fixture.update(&fixture.peer, "idle").await;
    settled(&record).await;
    let status = fixture
        .public(&fixture.member, json!({"op":"status","wait_id":record.id}))
        .await;
    assert_eq!(status["status"], "ready");
    assert_eq!(status["cursor"], 1);
    assert!(status["metadata"].get("first_read_at_ms").is_none());
    assert!(status["results"].as_array().unwrap().is_empty());
    let mut retry = request(&fixture, "lost_create");
    retry["window_ms"] = json!(1000);
    retry["timeout_ms"] = json!(2000);
    let ready = fixture.public(&fixture.member, retry.clone()).await;
    assert_eq!(ready["wait_id"], record.id);
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["results"][0]["data"]["matched"], true);
    assert_eq!(ready["metadata"]["state"], "cached");
    assert!(ready["metadata"]["first_read_at_ms"].is_u64());
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":record.id,"after":0}))
        .await;
    assert_eq!(replay["results"], ready["results"]);
    assert_eq!(
        replay["metadata"]["completed_at_ms"],
        ready["metadata"]["completed_at_ms"]
    );
    assert_eq!(
        replay["metadata"]["elapsed_total_ms"],
        ready["metadata"]["elapsed_total_ms"]
    );
    assert_eq!(replay["metadata"]["replay_count"], 1);
    assert_eq!(fixture.broker.awaiting.records.lock().unwrap().len(), 1);
    let other = fixture.public(&fixture.peer, json!({"op":"list"})).await;
    assert!(other["waits"].as_array().unwrap().is_empty());
    let listed = fixture.public(&fixture.member, json!({"op":"list"})).await;
    assert_eq!(listed["waits"][0]["wait_id"], record.id);
    let ack = fixture
        .public(&fixture.member, json!({"wait_id":record.id,"after":1}))
        .await;
    assert_eq!(ack["acknowledged"], true);
    let retried_creation = fixture.public(&fixture.member, retry).await;
    assert_eq!(retried_creation["wait_id"], record.id);
    assert_eq!(retried_creation["metadata"]["state"], "acknowledged");
}

#[tokio::test]
async fn dropping_the_code_mode_helper_future_does_not_own_the_durable_observer() {
    let fixture = Fixture::new().await;
    fixture.update(&fixture.peer, "working").await;
    let run = code_mode::AuditRun::new("fixture helper", None).unwrap();
    let report = fixture.report(run.clone());
    let mut future = Box::pin(fixture.raw(run, &fixture.member, request(&fixture, "helper_drop")));
    let record = tokio::select! {
        result = &mut future => panic!("unexpected immediate completion: {result}"),
        record = created(&fixture, "helper_drop") => record,
    };
    drop(future);
    drop(report);
    assert!(record.active.load(Ordering::Acquire));
    fixture.update(&fixture.peer, "idle").await;
    settled(&record).await;
    assert_eq!(
        fixture
            .public(&fixture.member, json!({"wait_id":record.id,"after":0}))
            .await["status"],
        "ready"
    );
}

#[tokio::test]
async fn cancel_deadline_and_unknown_target_are_cached_without_restarting_targets() {
    let fixture = Arc::new(Fixture::new().await);
    fixture.update(&fixture.peer, "working").await;
    let worker = fixture.clone();
    let pending = tokio::spawn(async move {
        worker
            .public(&worker.member, request(&worker, "cancel_me"))
            .await
    });
    let record = created(&fixture, "cancel_me").await;
    let foreign = fixture
        .public(&fixture.peer, json!({"op":"cancel","wait_id":record.id}))
        .await;
    assert_eq!(foreign["status"], "unauthorized");
    assert!(!record.terminal.load(Ordering::Acquire));
    let cancel = fixture
        .public(&fixture.member, json!({"op":"cancel","wait_id":record.id}))
        .await;
    assert_eq!(cancel["status"], "cancelled");
    assert_eq!(cancel["cursor"], 1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap()["status"],
        "cancelled"
    );
    settled(&record).await;
    fixture.update(&fixture.peer, "working").await;
    let cancelled = fixture
        .public(&fixture.member, json!({"wait_id":record.id,"after":0}))
        .await;
    assert_eq!(cancelled["status"], "cancelled");
    let unknown_request = json!({"request_id":"missing_job","lifetime_ms":60_000,"window_ms":1000,
        "targets":[{"type":"job","id":"absent_job"}]});
    let unknown = fixture
        .public(&fixture.member, unknown_request.clone())
        .await;
    assert_eq!(unknown["status"], "unknown_target");
    assert_eq!(unknown["cursor"], 1);
    let replay = fixture.public(&fixture.member, unknown_request).await;
    assert_eq!(replay["wait_id"], unknown["wait_id"]);
    assert_eq!(replay["results"], unknown["results"]);
    let mut deadline_request = request(&fixture, "deadline");
    deadline_request["lifetime_ms"] = json!(1000);
    deadline_request["mode"] = json!("all");
    deadline_request["targets"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]
        }));
    let deadline = fixture.public(&fixture.member, deadline_request).await;
    assert_eq!(deadline["status"], "timed_out");
    assert_eq!(deadline["cursor"], 1);
    assert_eq!(deadline["continue_wait"], false);
    assert_eq!(deadline["results"][0]["status"], "timed_out");
    assert_eq!(deadline["results"][1]["status"], "ready");
    let ack = fixture
        .public(
            &fixture.member,
            json!({"wait_id":deadline["wait_id"],"after":1}),
        )
        .await;
    assert_eq!(ack["status"], "timed_out");
    assert_eq!(ack["acknowledged"], true);
}

#[tokio::test]
async fn durable_capacity_counts_background_observers_after_caller_disconnect() {
    let fixture = Arc::new(Fixture::new().await);
    fixture.update(&fixture.peer, "working").await;
    let mut records = Vec::new();
    for index in 0..MAX_ACTIVE_PER_MEMBER {
        let worker = fixture.clone();
        let id = format!("active_{index}");
        let value = request(&worker, &id);
        let caller = tokio::spawn(async move { worker.public(&worker.member, value).await });
        let record = created(&fixture, &id).await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        records.push(record);
    }
    let busy = fixture
        .public(&fixture.member, request(&fixture, "overflow"))
        .await;
    assert_eq!(busy["status"], "busy");
    assert_eq!(busy["cursor"], 1);
    for record in records {
        fixture
            .public(&fixture.member, json!({"op":"cancel","wait_id":record.id}))
            .await;
        settled(&record).await;
    }
    // Admission failure itself is frozen; retry cannot silently start later.
    let replay = fixture
        .public(&fixture.member, request(&fixture, "overflow"))
        .await;
    assert_eq!(replay["wait_id"], busy["wait_id"]);
    assert_eq!(replay["status"], "busy");
}

#[tokio::test]
async fn durable_snapshot_ack_keeps_inbox_events_and_user_input_unconsumed() {
    let fixture = Fixture::new().await;
    fixture.update(&fixture.peer, "working").await;
    fixture.publish("private_requirement");
    let subscription = fixture
        .events(
            &fixture.member,
            json!({"action":"subscribe","request_id":"subscribe",
        "filter":{"kinds":["custom.done"]}}),
        )
        .await
        .unwrap();
    fixture
        .events(
            &fixture.member,
            json!({"action":"publish","request_id":"publish",
        "name":"custom.done","data":{"phase":"ready"}}),
        )
        .await
        .unwrap();
    fixture
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "send",
            json!({"agent_id":fixture.peer["agent_id"],
        "token":fixture.peer["token"],"to":fixture.member["agent_id"],"payload":{"phase":"ready"}}),
        )
        .await
        .unwrap();
    let ready = fixture
        .public(
            &fixture.member,
            json!({"request_id":"passive_all","lifetime_ms":60_000,
        "window_ms":1000,"mode":"all","targets":[{"type":"agent_messages"},
            {"type":"event","subscription_id":subscription["subscription_id"]}]}),
        )
        .await;
    assert_eq!(ready["status"], "ready");
    let ack = fixture
        .public(
            &fixture.member,
            json!({"wait_id":ready["wait_id"],"after":1}),
        )
        .await;
    assert_eq!(ack["acknowledged"], true);
    let inbox = fixture
        .session
        .collaboration
        .receive(
            &fixture.owner,
            fixture.member["agent_id"].as_str().unwrap(),
            fixture.member["token"].as_str().unwrap(),
            None,
            32,
            0,
        )
        .await
        .unwrap();
    assert_eq!(inbox["messages"], ready["results"][0]["data"]["messages"]);
    let events = fixture
        .events(
            &fixture.member,
            json!({"action":"read","subscription_id":subscription["subscription_id"]}),
        )
        .await
        .unwrap();
    assert_eq!(events["events"], ready["results"][1]["data"]["events"]);
    assert_eq!(fixture.state("private_requirement"), SteeringState::Queued);
}

#[tokio::test]
async fn cached_durable_event_bodies_and_ack_recheck_source_authorization() {
    let fixture = Fixture::new().await;
    fixture.update(&fixture.peer, "working").await;
    let source = fixture
        .broker
        .test_register_identity(
            "fixture-pool",
            Identity {
                provider: "github".into(),
                subject: "42".into(),
                login: "alice".into(),
            },
        )
        .await;
    let member = source
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"source"}))
        .await
        .unwrap();
    let subscription = fixture.events(&fixture.member, json!({"action":"subscribe","request_id":"subscribe_pool",
        "filter":{"client_ids":[source.client_id()],"kinds":["custom.done"]},
        "source_agents":[{"client_id":source.client_id(),"agent_id":member["agent_id"],"token":member["token"]}]})).await.unwrap();
    super::super::super::events_tool::dispatch(fixture.broker.clone(), "mcp".into(), fixture.token.clone(),
        json!({"action":"publish","client_id":source.client_id(),"agent_id":member["agent_id"],"token":member["token"],
            "request_id":"published","name":"custom.done","data":{"private":"retained"}})).await.unwrap();
    let run = code_mode::AuditRun::new("durable source fixture", None).unwrap();
    let ready = fixture.raw(run.clone(), &fixture.member, json!({"request_id":"durable_event","lifetime_ms":60_000,
        "window_ms":1000,"targets":[{"type":"event","subscription_id":subscription["subscription_id"]}]})).await;
    assert_eq!(ready["status"], "ready");
    let id = ready["wait_id"].as_str().unwrap();
    source
        .collaboration
        .dispatch(
            &fixture.owner,
            "leave",
            json!({"agent_id":member["agent_id"],"token":member["token"]}),
        )
        .await
        .unwrap();
    let final_response = finish_passive_response(
        &fixture.report(run),
        fixture.session.client_id(),
        ready.clone(),
    )
    .await;
    assert_eq!(
        final_response["structuredContent"]["status"],
        "unauthorized"
    );
    assert!(
        final_response["structuredContent"]["results"][0]
            .get("data")
            .is_none()
    );
    for after in [0, 1] {
        let denied = fixture
            .public(&fixture.member, json!({"wait_id":id,"after":after}))
            .await;
        assert_eq!(denied["status"], "unauthorized");
        assert!(denied["results"][0].get("data").is_none());
    }
    assert_eq!(
        fixture
            .record(id)
            .durable
            .as_ref()
            .unwrap()
            .metadata()
            .state,
        crate::wire::wait_display::DurableState::Cached
    );
    let listed = fixture.public(&fixture.member, json!({"op":"list"})).await;
    assert!(listed["waits"].as_array().unwrap().is_empty());
}
