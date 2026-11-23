use super::*;

fn actor() -> Actor {
    Actor {
        user: "fixture".into(),
        owner: "owner".into(),
        client_id: "client".into(),
        agent_id: "agent_fixture".into(),
        connection: Weak::new(),
    }
}

fn create(id: &str) -> Arguments {
    Arguments::parse(
        json!({"client_id":"client","request_id":id,"lifetime_ms":600_000,
        "window_ms":1_000,"targets":[{"type":"exec","job_id":"exec_fixture"}]}),
    )
    .unwrap()
}

fn existing(id: &str) -> Arguments {
    Arguments::parse(json!({"client_id":"client","wait_id":id})).unwrap()
}

#[test]
fn explicit_lifetime_requires_idempotency_and_excludes_consuming_targets() {
    for value in [
        json!({"client_id":"client","lifetime_ms":60_000,"window_ms":1000,"targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","request_id":"same","window_ms":1000,"targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","request_id":"same","lifetime_ms":60_000,"targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","request_id":"same","lifetime_ms":999,"window_ms":1000,"targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","request_id":"same","lifetime_ms":86_400_001,"window_ms":1000,"targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","request_id":"same","lifetime_ms":60_000,"window_ms":1000,"targets":[{"type":"user_input","notify":false}]}),
        json!({"client_id":"client","request_id":"same","lifetime_ms":60_000,"window_ms":1000,"targets":[{"type":"worker_assignment","request_id":"claim"}]}),
        json!({"client_id":"client","op":"list","targets":[{"type":"job","id":"job"}]}),
        json!({"client_id":"client","op":"list","wait_id":"await_other"}),
        json!({"client_id":"client","op":"status"}),
    ] {
        assert!(Arguments::parse(value.clone()).is_err(), "{value}");
    }
    for lifetime_ms in [1000, 86_400_000] {
        assert!(
            Arguments::parse(
                json!({"client_id":"client","request_id":"same","lifetime_ms":lifetime_ms,
            "window_ms":1000,"targets":[{"type":"job","id":"job"}]})
            )
            .is_ok()
        );
    }
    assert!(Arguments::parse(json!({"client_id":"client","op":"list"})).is_ok());
    assert!(
        Arguments::parse(json!({"client_id":"client","op":"status","wait_id":"await_owned"}))
            .is_ok()
    );
    assert_eq!(limits()["durable_retention_ms"], 1_800_000);
    assert_eq!(
        tool()["inputSchema"]["properties"]["lifetime_ms"]["maximum"],
        86_400_000
    );
}

#[test]
fn idempotency_normalizes_definitions_but_read_windows_are_independent() {
    let registry = Registry::default();
    let first = Arguments::parse(json!({"client_id":"client","request_id":"stable","lifetime_ms":60_000,
        "window_ms":1000,"targets":[{"type":"agent_status","agent_id":"agent_peer","statuses":["idle","working"]},
            {"type":"exec","job_id":"exec_1"}]})).unwrap();
    let record = registry.record(actor(), &first).unwrap();
    let retry = Arguments::parse(json!({"client_id":"client","request_id":"stable","lifetime_ms":60_000,
        "window_ms":55_000,"timeout_ms":1000,"mode":"any","targets":[{"type":"exec","job_id":"exec_1"},
            {"type":"agent_status","agent_id":"agent_peer","statuses":["working","idle","idle"]}]})).unwrap();
    assert!(Arc::ptr_eq(
        &record,
        &registry.record(actor(), &retry).unwrap()
    ));
    assert_eq!(registry.records.lock().unwrap().len(), 1);
    let mut changed = retry;
    changed.lifetime_ms = Some(61_000);
    assert_eq!(
        registry.record(actor(), &changed).err().unwrap(),
        (Status::Unavailable, "await_request_conflict")
    );
    let mut other = actor();
    other.agent_id = "agent_other".into();
    assert_ne!(registry.record(other, &first).unwrap().id, record.id);
}

#[tokio::test]
async fn same_creation_id_cannot_retarget_a_replaced_native_session() {
    let broker = Broker::new("https://fixture.example".into());
    let first = broker.test_register("fixture", "first").await;
    let replacement = broker.test_register("fixture", "replacement").await;
    let original = Actor {
        client_id: first.client_id().into(),
        connection: Arc::downgrade(&first),
        ..actor()
    };
    let changed = Actor {
        connection: Arc::downgrade(&replacement),
        ..original.clone()
    };
    let registry = Registry::default();
    let mut args = create("stable");
    args.client_id = original.client_id.clone();
    let record = registry.record(original.clone(), &args).unwrap();
    assert_eq!(
        registry.record(changed, &args).err().unwrap(),
        (Status::Unauthorized, "await_connection_changed")
    );
    assert!(Arc::ptr_eq(
        &record,
        &registry.record(original, &args).unwrap()
    ));
    assert_eq!(registry.records.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn every_terminal_outcome_is_frozen_and_acknowledges_only_its_cache() {
    let broker = Broker::new("https://fixture.example".into());
    for status in [
        Status::Ready,
        Status::TimedOut,
        Status::Cancelled,
        Status::Unauthorized,
        Status::UnknownTarget,
        Status::Unavailable,
        Status::Busy,
    ] {
        let registry = Registry::default();
        let record = registry.record(actor(), &create("stable")).unwrap();
        let result = TargetResult {
            target: record.targets[0].clone(),
            status,
            data: (status == Status::Ready).then(|| json!({"running":false,"exit_code":0})),
            error: None,
        };
        finish(&broker, &record, status, vec![result]);
        let durable = record.durable.as_ref().unwrap();
        durable.note_window(1000);
        let first = snapshot(&record, Some(0)).unwrap();
        assert_eq!(first["status"], json!(status));
        assert_eq!(first["cursor"], 1);
        assert_eq!(first["continue_wait"], false);
        // A late cancel cannot relabel an already-published outcome.
        finish(&broker, &record, Status::Cancelled, Vec::new());
        durable.note_window(2000);
        let replay = snapshot(&record, Some(0)).unwrap();
        assert_eq!(replay["results"], first["results"]);
        assert_eq!(
            replay["metadata"]["completed_at_ms"],
            first["metadata"]["completed_at_ms"]
        );
        assert_eq!(
            replay["metadata"]["elapsed_total_ms"],
            first["metadata"]["elapsed_total_ms"]
        );
        assert_eq!(replay["metadata"]["replay_count"], 1);
        let ack = snapshot(&record, Some(1)).unwrap();
        let again = snapshot(&record, Some(1)).unwrap();
        assert_eq!(ack, again);
        assert_eq!(ack["status"], json!(status));
        assert_eq!(ack["acknowledged"], true);
        assert!(durable.state.lock().unwrap().results.is_none());
        assert!(snapshot(&record, Some(0)).is_err());
        durable.metadata().validate().unwrap();
    }
}

#[tokio::test]
async fn retention_and_capacity_protect_errors_and_acknowledged_metadata_for_thirty_minutes() {
    let broker = Broker::new("https://fixture.example".into());
    let registry = Registry::default();
    let mut records = Vec::new();
    for index in 0..MAX_ACTOR_RECORDS {
        let record = registry
            .record(actor(), &create(&format!("request_{index}")))
            .unwrap();
        finish(
            &broker,
            &record,
            Status::Unavailable,
            failure_results(
                &record,
                Status::Unavailable,
                "fixture_failure",
                "bounded failure",
            ),
        );
        snapshot(&record, Some(1)).unwrap();
        records.push(record);
    }
    assert_eq!(
        registry.record(actor(), &create("extra")).err().unwrap(),
        (Status::Busy, "await_registry_full")
    );
    let protected = &records[0];
    *protected.touched.lock().unwrap() = Instant::now() - Duration::from_secs(3600);
    assert!(registry.record(actor(), &existing(&protected.id)).is_ok());
    let expired = Instant::now() - RETAIN_FOR - Duration::from_secs(1);
    protected
        .durable
        .as_ref()
        .unwrap()
        .state
        .lock()
        .unwrap()
        .completed = Some(expired);
    // Even an expired terminal marker cannot evict a still-owned observer.
    protected.active.store(true, Ordering::Release);
    assert!(registry.record(actor(), &existing(&protected.id)).is_ok());
    protected.active.store(false, Ordering::Release);
    assert_eq!(
        registry
            .record(actor(), &existing(&protected.id))
            .err()
            .unwrap(),
        (Status::UnknownTarget, "await_unknown_wait")
    );
    assert!(registry.record(actor(), &create("extra")).is_ok());
}

#[tokio::test]
async fn oversized_results_become_one_cached_failure_and_elapsed_is_not_a_read_window() {
    let broker = Broker::new("https://fixture.example".into());
    let registry = Registry::default();
    let record = registry.record(actor(), &create("overflow")).unwrap();
    finish(
        &broker,
        &record,
        Status::Ready,
        vec![TargetResult {
            target: record.targets[0].clone(),
            status: Status::Ready,
            data: Some(json!({"body":"x".repeat(MAX_SNAPSHOT_BYTES)})),
            error: None,
        }],
    );
    let cached = snapshot(&record, Some(0)).unwrap();
    assert_eq!(cached["status"], "unavailable");
    assert_eq!(
        cached["results"][0]["error"]["code"],
        "await_snapshot_overflow"
    );
    assert!(cached["results"][0].get("data").is_none());
    assert!(serde_json::to_vec(&cached).unwrap().len() < MAX_SNAPSHOT_BYTES);
    let mut observation = Observation::new(&create("elapsed"));
    observation.started = Instant::now() - Duration::from_secs(95);
    assert!(observation.metadata().elapsed_total_ms >= 95_000);
    assert_eq!(observation.metadata().read_window_ms, 1000);
}
