use super::*;

fn actor(owner: &str, client: &str, agent: &str) -> Actor {
    Actor {
        owner: owner.into(),
        client_id: client.into(),
        agent_id: agent.into(),
    }
}

fn subscriber(hub: &Hub, actor: &Actor, request: &str, after: Option<u64>) -> String {
    hub.subscribe(actor, request, Filter::default(), after)
        .unwrap()["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn publish(hub: &Hub, owner: &str, client: &str, index: usize) -> Value {
    hub.publish_system(
        owner,
        client,
        Some("worker"),
        "worker.progress",
        json!({"index":index,"task_id":"task-one"}),
        (None, None),
    )
    .unwrap()
}

fn sequence(value: &Value) -> u64 {
    value["next_cursor"].as_u64().unwrap()
}

#[test]
fn accounts_and_subscription_actors_are_isolated() {
    let hub = Hub::default();
    let a = actor("account-a", "client-a", "agent-a");
    let b = actor("account-b", "client-a", "agent-a");
    let peer = actor("account-a", "client-a", "peer");
    let sa = subscriber(&hub, &a, "same-request", None);
    let sb = subscriber(&hub, &b, "same-request", None);
    publish(&hub, &a.owner, &a.client_id, 1);
    assert_eq!(
        hub.read(&a, &sa, 64).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(hub.read(&b, &sb, 64).unwrap()["events"], json!([]));
    for foreign in [&b, &peer] {
        assert!(hub.read(foreign, &sa, 64).is_err());
        assert!(hub.ack(foreign, &sa, 1).is_err());
        assert!(hub.unsubscribe(foreign, &sa).is_err());
        assert!(hub.subscription_clients(foreign, &sa).is_err());
    }
    publish(&hub, &b.owner, &b.client_id, 2);
    let event = &hub.read(&b, &sb, 64).unwrap()["events"][0];
    assert_eq!(event["seq"], 1);
    assert_eq!(event["data"]["index"], 2);
    assert_ne!(
        event["id"],
        hub.read(&a, &sa, 64).unwrap()["events"][0]["id"]
    );
    assert_eq!(
        hub.subscriptions(&peer).unwrap()["subscriptions"],
        json!([])
    );
}

#[test]
fn real_opaque_owner_keys_work_without_weakening_public_identifiers() {
    let hub = Hub::default();
    let a = actor("user\0github\0subject", "client", "agent");
    let b = actor("user\0github\0another-subject", "client", "agent");
    let id = subscriber(&hub, &a, "opaque-owner", Some(0));
    let other = subscriber(&hub, &b, "opaque-owner", Some(0));
    publish(&hub, &a.owner, &a.client_id, 1);
    hub.publish_custom(&a, "custom-owner", "custom.analysis", json!({"ready":true}))
        .unwrap();
    let page = hub.read(&a, &id, 64).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(page["events"][0]["origin"], "system");
    assert_eq!(page["events"][1]["origin"], "agent");
    assert!(page["events"][0].get("owner").is_none());
    assert_eq!(hub.read(&b, &other, 64).unwrap()["events"], json!([]));
    assert!(hub.read(&b, &id, 64).is_err());
    assert_eq!(hub.ack(&a, &id, sequence(&page)).unwrap()["cursor"], 2);
    assert!(
        hub.subscribe(
            &actor(&a.owner, "client\0other", "agent"),
            "bad-client",
            Filter::default(),
            None
        )
        .is_err()
    );
    assert!(
        hub.publish_custom(
            &actor(&a.owner, "client", "agent\0other"),
            "bad-agent",
            "custom.analysis",
            json!({})
        )
        .is_err()
    );
    assert!(
        hub.publish_custom(&a, "request\0other", "custom.analysis", json!({}))
            .is_err()
    );
    for invalid in [String::new(), "x".repeat(513)] {
        assert!(
            hub.subscribe(
                &actor(&invalid, "client", "agent"),
                "bad-owner",
                Filter::default(),
                None
            )
            .is_err()
        );
    }
    assert_eq!(hub.read(&a, &id, 64).unwrap()["latest_cursor"], 2);
}

#[test]
fn private_audience_uses_the_complete_client_and_agent_identity() {
    let hub = Hub::default();
    let a = actor("owner", "client-a", "same-agent");
    let b = actor("owner", "client-b", "same-agent");
    let peer = actor("owner", "client-a", "other-agent");
    let filter = Filter {
        client_ids: vec!["client-a".into(), "client-b".into()],
        ..Filter::default()
    };
    let subscriptions: Vec<_> = [&a, &b, &peer]
        .iter()
        .map(|who| {
            hub.subscribe(who, "private-sub", filter.clone(), None)
                .unwrap()["subscription_id"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    hub.publish_system(
        "owner",
        "client-a",
        Some("sender"),
        "message.received",
        json!({"message_id":"message-one"}),
        (
            Some(vec!["client-a:same-agent".into()]),
            Some("message-one-received"),
        ),
    )
    .unwrap();
    for (index, who) in [&a, &b, &peer].iter().enumerate() {
        let page = hub.read(who, &subscriptions[index], 64).unwrap();
        assert_eq!(
            page["events"].as_array().unwrap().len(),
            usize::from(index == 0)
        );
        assert_eq!(page["next_cursor"], 1);
        if index == 0 {
            assert!(page["events"][0].get("audience").is_none());
            assert_eq!(
                page["events"][0]["data"],
                json!({"message_id":"message-one"})
            );
        }
    }
}

#[test]
fn filters_scan_unmatched_events_without_exposing_or_implicitly_acknowledging_them() {
    let hub = Hub::default();
    let a = actor("owner", "client-a", "reader");
    let filter = Filter {
        kinds: vec!["worker.*".into()],
        client_ids: vec!["client-a".into()],
        actor_id: Some("worker".into()),
        task_id: Some("task-one".into()),
        include_own_waits: false,
    };
    let id = hub.subscribe(&a, "filter", filter, Some(0)).unwrap()["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    hub.publish_system(
        "owner",
        "client-a",
        Some("worker"),
        "message.sent",
        json!({"task_id":"task-one"}),
        (None, None),
    )
    .unwrap();
    publish(&hub, "owner", "client-b", 10);
    hub.publish_system(
        "owner",
        "client-a",
        Some("another-worker"),
        "worker.started",
        json!({"task_id":"task-one"}),
        (None, None),
    )
    .unwrap();
    hub.publish_system(
        "owner",
        "client-a",
        Some("worker"),
        "worker.completed",
        json!({"task_id":"another-task"}),
        (None, None),
    )
    .unwrap();
    publish(&hub, "owner", "client-a", 20);
    let first = hub.read(&a, &id, 64).unwrap();
    assert_eq!(first["events"].as_array().unwrap().len(), 1);
    assert_eq!(first["events"][0]["data"]["index"], 20);
    assert_eq!(first["cursor"], 0);
    assert_eq!(first["next_cursor"], 5);
    assert_eq!(hub.read(&a, &id, 64).unwrap()["events"], first["events"]);
    hub.ack(&a, &id, 5).unwrap();
    assert_eq!(hub.read(&a, &id, 64).unwrap()["events"], json!([]));
}

#[test]
fn publication_and_subscription_requests_are_idempotent_with_content_checks() {
    let hub = Hub::default();
    let a = actor("owner", "client-a", "agent");
    let initial = hub.subscribe(&a, "sub", Filter::default(), None).unwrap();
    let id = initial["subscription_id"].as_str().unwrap();
    let first = hub
        .publish_custom(&a, "publish-one", "custom.analysis", json!({"count":3}))
        .unwrap();
    let retry = hub
        .publish_custom(&a, "publish-one", "custom.analysis", json!({"count":3}))
        .unwrap();
    assert_eq!(first["event_id"], retry["event_id"]);
    assert_eq!(retry["duplicate"], true);
    assert!(
        hub.publish_custom(&a, "publish-one", "custom.analysis", json!({"count":4}))
            .unwrap_err()
            .starts_with("events_request_conflict")
    );
    let duplicate = hub.subscribe(&a, "sub", Filter::default(), None).unwrap();
    assert_eq!(initial["subscription_id"], duplicate["subscription_id"]);
    assert_eq!(duplicate["cursor"], 0);
    assert!(
        hub.subscribe(&a, "sub", Filter::default(), Some(0))
            .is_err()
    );
    let page = hub.read(&a, id, 64).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["origin"], "agent");
    assert_eq!(page["events"][0]["actor_id"], "agent");
    hub.unsubscribe(&a, id).unwrap();
    assert_eq!(hub.unsubscribe(&a, id).unwrap()["duplicate"], true);
    let retry = hub.subscribe(&a, "sub", Filter::default(), None).unwrap();
    assert_eq!(retry["status"], "unsubscribed");
    assert_eq!(retry["subscription_id"], initial["subscription_id"]);
    assert!(hub.read(&a, id, 64).is_err());
}

#[test]
fn custom_events_cannot_impersonate_system_sources_or_supply_authority_fields() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    assert!(
        hub.publish_custom(&a, "forged", "worker.completed", json!({}))
            .is_err()
    );
    assert!(
        hub.publish_custom(&a, "empty", "custom.", json!({}))
            .is_err()
    );
    assert!(
        hub.publish_system(
            "owner",
            "client",
            None,
            "custom.impersonation",
            json!({}),
            (None, None)
        )
        .is_err()
    );
    assert!(serde_json::from_value::<Filter>(json!({"owner":"someone-else"})).is_err());
    assert!(
        hub.publish_system(
            "owner",
            "client",
            None,
            "message.sent",
            json!({}),
            (Some(vec!["agent-without-client".into()]), None)
        )
        .is_err()
    );
    assert!(
        hub.publish_custom(
            &actor("owner", "client:agent", "other"),
            "ambiguous",
            "custom.event",
            json!({})
        )
        .is_err()
    );
}

#[test]
fn cursors_must_be_retained_and_acknowledgements_cannot_skip_undelivered_pages() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "from-start", Some(0));
    for index in 0..3 {
        publish(&hub, "owner", "client", index);
    }
    assert!(
        hub.subscribe(&a, "future", Filter::default(), Some(4))
            .unwrap_err()
            .starts_with("events_cursor_ahead")
    );
    assert!(hub.ack(&a, &id, 1).is_err());
    let first = hub.read(&a, &id, 1).unwrap();
    assert_eq!(first["has_more"], true);
    assert_eq!(sequence(&first), 1);
    assert!(hub.ack(&a, &id, 2).is_err());
    hub.ack(&a, &id, 1).unwrap();
    assert_eq!(hub.ack(&a, &id, 0).unwrap()["cursor"], 1);
    assert_eq!(hub.ack(&a, &id, 1).unwrap()["duplicate"], true);
    let second = hub.read(&a, &id, 1).unwrap();
    assert_eq!(second["events"][0]["seq"], 2);
    assert_eq!(hub.read(&a, &id, 1).unwrap()["events"], second["events"]);
    assert!(hub.read(&a, &id, 0).is_err());
    assert!(hub.read(&a, &id, 65).is_err());
    let from_now = subscriber(&hub, &a, "from-now", None);
    assert_eq!(hub.read(&a, &from_now, 64).unwrap()["events"], json!([]));
}

#[tokio::test]
async fn publications_between_wait_windows_are_retained_until_explicit_ack() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "windows", None);
    let first = hub.wait(&a, &id, 64, 2).await.unwrap();
    assert_eq!(first["status"], "pending");
    assert_eq!(first["cursor"], 0);
    publish(&hub, "owner", "client", 1);
    let next = hub.wait(&a, &id, 64, 55_000).await.unwrap();
    assert_eq!(next["status"], "ready");
    assert_eq!(next["events"][0]["seq"], 1);
    assert_eq!(
        hub.wait(&a, &id, 64, 0).await.unwrap()["events"],
        next["events"]
    );
    hub.ack(&a, &id, sequence(&next)).unwrap();
    publish(&hub, "owner", "client", 2);
    assert_eq!(
        hub.wait(&a, &id, 64, 0).await.unwrap()["events"][0]["seq"],
        2
    );
    assert!(hub.wait(&a, &id, 64, 55_001).await.is_err());
    assert_eq!(hub.locked().waiters, 0);
}

#[tokio::test]
async fn publication_wakes_an_enabled_wait_and_dropping_a_wait_never_acknowledges() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "wake", None);
    let mut pending = Box::pin(hub.wait(&a, &id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut pending => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    assert_eq!(hub.locked().waiters, 1);
    publish(&hub, "owner", "client", 1);
    let page = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page["status"], "ready");
    assert_eq!(hub.locked().waiters, 0);
    assert_eq!(hub.read(&a, &id, 64).unwrap()["cursor"], 0);
    hub.ack(&a, &id, 1).unwrap();
    let mut cancelled = Box::pin(hub.wait(&a, &id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut cancelled => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    drop(cancelled);
    assert_eq!(hub.locked().waiters, 0);
    publish(&hub, "owner", "client", 2);
    let page = hub.read(&a, &id, 64).unwrap();
    assert_eq!(page["cursor"], 1);
    assert_eq!(page["events"][0]["seq"], 2);
}

#[tokio::test]
async fn unsubscribe_and_client_close_wake_waits_without_synthetic_completion() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "close", None);
    let mut pending = Box::pin(hub.wait(&a, &id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut pending => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    hub.close_client("client");
    let error = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.starts_with("events_client_closed"));
    assert_eq!(hub.locked().owners["owner"].sequence, 0);
    assert_eq!(hub.locked().waiters, 0);
    let other = actor("owner", "other-client", "reader");
    let other_id = subscriber(&hub, &other, "unsubscribe", None);
    let mut pending = Box::pin(hub.wait(&other, &other_id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut pending => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    hub.unsubscribe(&other, &other_id).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .is_err()
    );
    assert_eq!(hub.locked().waiters, 0);
}

#[test]
fn overflow_is_explicit_and_recovery_requires_a_retained_cursor() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "overflow", Some(0));
    for index in 0..=MAX_EVENTS {
        publish(&hub, "owner", "client", index);
    }
    assert!(hub.read(&a, &id, 64).unwrap_err().starts_with("events_gap"));
    assert!(
        hub.subscribe(&a, "stale", Filter::default(), Some(0))
            .unwrap_err()
            .starts_with("events_gap")
    );
    assert!(hub.ack(&a, &id, 1).is_err());
    let boundary = hub.locked().owners["owner"].oldest_cursor;
    assert_eq!(boundary, 1);
    let recovery = subscriber(&hub, &a, "reconcile", Some(boundary));
    let page = hub.read(&a, &recovery, 64).unwrap();
    assert_eq!(page["events"][0]["seq"], 2);
    assert_eq!(page["oldest_cursor"], 1);
    let state = hub.locked();
    assert!(state.owners["owner"].events.len() <= MAX_EVENTS);
    assert!(state.owners["owner"].event_bytes <= MAX_JOURNAL_BYTES);
}

#[test]
fn byte_capacity_and_read_page_budget_are_enforced_without_partial_events() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let data = json!({"detail":"x".repeat(3000)});
    for _ in 0..400 {
        hub.publish_system(
            "owner",
            "client",
            None,
            "worker.progress",
            data.clone(),
            (None, None),
        )
        .unwrap();
    }
    let boundary = hub.locked().owners["owner"].oldest_cursor;
    assert!(boundary > 0);
    let id = subscriber(&hub, &a, "byte-page", Some(boundary));
    let page = hub.read(&a, &id, 64).unwrap();
    assert!(encoded_len(&page).unwrap() <= MAX_READ_BYTES);
    assert!(page["events"].as_array().unwrap().len() < 64);
    assert_eq!(page["has_more"], true);
    for event in page["events"].as_array().unwrap() {
        assert_eq!(event["data"], data);
        assert!(encoded_len(event).unwrap() <= MAX_EVENT_BYTES);
    }
    let head = page["latest_cursor"].clone();
    assert!(
        hub.publish_custom(
            &a,
            "too-large",
            "custom.detail",
            json!({"detail":"x".repeat(MAX_EVENT_BYTES)})
        )
        .is_err()
    );
    assert_eq!(hub.read(&a, &id, 64).unwrap()["latest_cursor"], head);
}

#[tokio::test]
async fn system_receipt_capacity_failure_wakes_readers_with_an_explicit_gap() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let first = hub
        .publish_system(
            "owner",
            "client",
            None,
            "worker.completed",
            json!({"index":0}),
            (None, Some("system-0")),
        )
        .unwrap();
    for index in 1..MAX_PUBLISH_RECEIPTS {
        hub.publish_system(
            "owner",
            "client",
            None,
            "worker.completed",
            json!({"index":index}),
            (None, Some(&format!("system-{index}"))),
        )
        .unwrap();
    }
    let id = subscriber(&hub, &a, "at-head", None);
    let before = hub.read(&a, &id, 64).unwrap();
    let head = before["latest_cursor"].as_u64().unwrap();
    let mut pending = Box::pin(hub.wait(&a, &id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut pending => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    let error = hub
        .publish_system(
            "owner",
            "client",
            None,
            "worker.completed",
            json!({"index":"lost"}),
            (None, Some("system-overflow")),
        )
        .unwrap_err();
    assert!(error.starts_with("events_dedupe_capacity"));
    let error = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.starts_with("events_gap"));
    assert_eq!(hub.locked().waiters, 0);
    assert!(hub.read(&a, &id, 64).unwrap_err().starts_with("events_gap"));
    assert!(hub.ack(&a, &id, head + 1).is_err());
    let retry = hub
        .publish_system(
            "owner",
            "client",
            None,
            "worker.completed",
            json!({"index":0}),
            (None, Some("system-0")),
        )
        .unwrap();
    assert_eq!(retry["event_id"], first["event_id"]);
    assert_eq!(retry["duplicate"], true);
    let fresh = subscriber(&hub, &a, "after-reconciliation", Some(head + 1));
    let empty = hub.read(&a, &fresh, 64).unwrap();
    assert_eq!(empty["oldest_cursor"], head + 1);
    assert_eq!(empty["latest_cursor"], head + 1);
    assert_eq!(empty["events"], json!([]));
    publish(&hub, "owner", "client", 42);
    let resumed = hub.read(&a, &fresh, 64).unwrap();
    assert_eq!(resumed["events"][0]["seq"], head + 2);
    assert_eq!(resumed["events"][0]["data"]["index"], 42);
}

#[test]
fn oversized_system_metadata_marks_loss_but_invalid_requests_do_not() {
    let hub = Hub::default();
    let a = actor("owner", "client", "reader");
    let id = subscriber(&hub, &a, "oversize", Some(0));
    publish(&hub, "owner", "client", 1);
    let before = hub.read(&a, &id, 64).unwrap();
    assert!(
        hub.publish_custom(
            &a,
            "too-large",
            "custom.detail",
            json!({"detail":"x".repeat(MAX_EVENT_BYTES)})
        )
        .is_err()
    );
    assert!(
        hub.publish_system(
            "owner",
            "client\0invalid",
            None,
            "worker.completed",
            json!({}),
            (None, None)
        )
        .is_err()
    );
    assert_eq!(hub.read(&a, &id, 64).unwrap(), before);
    assert!(
        hub.publish_system(
            "owner",
            "client",
            None,
            "worker.completed",
            json!({"detail":"x".repeat(MAX_EVENT_BYTES)}),
            (None, None)
        )
        .unwrap_err()
        .starts_with("events_event_too_large")
    );
    assert!(hub.read(&a, &id, 64).unwrap_err().starts_with("events_gap"));
    let fresh = subscriber(&hub, &a, "reconciled", Some(2));
    let page = hub.read(&a, &fresh, 64).unwrap();
    assert_eq!(page["oldest_cursor"], 2);
    assert_eq!(page["latest_cursor"], 2);
    assert_eq!(page["events"], json!([]));
}

#[test]
fn recent_idempotency_receipts_apply_backpressure_instead_of_forgetting_requests() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    let first = hub
        .publish_custom(&a, "first", "custom.event", json!({}))
        .unwrap();
    for index in 1..MAX_PUBLISH_RECEIPTS {
        hub.publish_custom(&a, &format!("request-{index}"), "custom.event", json!({}))
            .unwrap();
    }
    let id = subscriber(&hub, &a, "custom-capacity-head", None);
    let before = hub.read(&a, &id, 64).unwrap();
    assert!(
        hub.publish_custom(&a, "overflow", "custom.event", json!({}))
            .unwrap_err()
            .starts_with("events_dedupe_capacity")
    );
    assert_eq!(hub.read(&a, &id, 64).unwrap(), before);
    let retry = hub
        .publish_custom(&a, "first", "custom.event", json!({}))
        .unwrap();
    assert_eq!(retry["event_id"], first["event_id"]);
    assert_eq!(retry["duplicate"], true);
    assert!(
        hub.publish_custom(&a, "first", "custom.changed", json!({}))
            .is_err()
    );
}

#[test]
fn subscription_and_account_capacities_are_bounded_with_retries_preserved() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    let first = hub.subscribe(&a, "first", Filter::default(), None).unwrap();
    for index in 1..MAX_SUBSCRIPTIONS {
        hub.subscribe(&a, &format!("sub-{index}"), Filter::default(), None)
            .unwrap();
    }
    assert!(
        hub.subscribe(&a, "one-more", Filter::default(), None)
            .is_err()
    );
    assert_eq!(
        hub.subscribe(&a, "first", Filter::default(), None).unwrap()["subscription_id"],
        first["subscription_id"]
    );
    for index in 1..MAX_OWNERS {
        publish(&hub, &format!("owner-{index}"), "client", index);
    }
    assert!(
        hub.publish_system(
            "overflow-owner",
            "client",
            None,
            "worker.idle",
            json!({}),
            (None, None)
        )
        .is_err()
    );
}

#[test]
fn retention_expiry_produces_a_gap_and_monotonic_sequence_survives_empty_storage() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    let id = subscriber(&hub, &a, "old", Some(0));
    publish(&hub, "owner", "client", 1);
    {
        let mut state = hub.locked();
        state.owners.get_mut("owner").unwrap().events[0].at =
            Instant::now() - RETENTION - Duration::from_secs(1);
    }
    assert!(hub.read(&a, &id, 64).unwrap_err().starts_with("events_gap"));
    let first_after_gap = publish(&hub, "owner", "client", 2);
    assert_eq!(first_after_gap["seq"], 2);
    let recovered = subscriber(&hub, &a, "recovered", Some(1));
    assert_eq!(hub.read(&a, &recovered, 64).unwrap()["events"][0]["seq"], 2);
    {
        let mut state = hub.locked();
        state
            .owners
            .get_mut("owner")
            .unwrap()
            .subscriptions
            .get_mut(&recovered)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
    }
    assert!(
        hub.read(&a, &recovered, 64)
            .unwrap_err()
            .starts_with("events_subscription_unavailable")
    );
}

#[tokio::test]
async fn a_subscriber_does_not_wake_itself_with_its_own_wait_lifecycle() {
    let hub = Hub::default();
    let a = actor("owner", "client", "waiting-agent");
    let observer = actor("owner", "client", "observer");
    let id = subscriber(&hub, &a, "self-wait", None);
    let observed = subscriber(&hub, &observer, "watch-peer", None);
    let mut pending = Box::pin(hub.wait(&a, &id, 64, 55_000));
    tokio::select! {
        biased;
        result = &mut pending => panic!("empty wait returned early: {result:?}"),
        _ = tokio::task::yield_now() => {}
    }
    for (index, kind) in [
        "wait.started",
        "wait.suspended",
        "wait.resumed",
        "wait.completed",
    ]
    .iter()
    .enumerate()
    {
        hub.publish_system(
            "owner",
            "client",
            Some("waiting-agent"),
            kind,
            json!({"origin_wait_id":format!("wait-{index}")}),
            (None, None),
        )
        .unwrap();
        tokio::select! {
            biased;
            result = &mut pending => panic!("own wait lifecycle caused self-wakeup: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
    assert_eq!(
        hub.read(&observer, &observed, 64).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(hub.read(&a, &id, 64).unwrap()["cursor"], 0);
    publish(&hub, "owner", "client", 1);
    let page = tokio::time::timeout(Duration::from_secs(1), pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["kind"], "worker.progress");
    assert_eq!(page["cursor"], 0);
    assert_eq!(page["next_cursor"], 5);
}

#[tokio::test]
async fn explicit_own_wait_observation_still_excludes_the_active_origin_id() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    let filter = Filter {
        include_own_waits: true,
        ..Filter::default()
    };
    let id = hub.subscribe(&a, "own-history", filter, None).unwrap()["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    hub.publish_system(
        "owner",
        "client",
        Some("agent"),
        "wait.started",
        json!({"origin_wait_id":"current"}),
        (None, None),
    )
    .unwrap();
    let pending = hub
        .wait_excluding(&a, &id, 64, 0, Some("current"))
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["cursor"], 0);
    assert_eq!(pending["next_cursor"], 1);
    hub.publish_system(
        "owner",
        "client",
        Some("agent"),
        "wait.completed",
        json!({"origin_wait_id":"previous"}),
        (None, None),
    )
    .unwrap();
    let page = hub
        .wait_excluding(&a, &id, 64, 0, Some("current"))
        .await
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["data"]["origin_wait_id"], "previous");
    assert_eq!(page["cursor"], 0);
    // The exclusion is an observation choice, never an implicit acknowledgement.
    assert_eq!(
        hub.read(&a, &id, 64).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(hub.read(&a, &id, 64).unwrap()["cursor"], 0);
}

#[test]
fn closing_a_client_preserves_old_subscription_proofs_and_allows_fresh_requests() {
    let hub = Hub::default();
    let a = actor("owner", "client", "agent");
    let original = hub
        .subscribe(&a, "original", Filter::default(), None)
        .unwrap();
    assert_eq!(original["duplicate"], false);
    let old_id = original["subscription_id"].as_str().unwrap();
    hub.close_client("client");
    let retry = hub
        .subscribe(&a, "original", Filter::default(), None)
        .unwrap();
    assert_eq!(retry["subscription_id"], original["subscription_id"]);
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["status"], "closed");
    assert!(
        hub.read(&a, old_id, 64)
            .unwrap_err()
            .starts_with("events_client_closed")
    );
    // The caller separately verifies a fresh live connection before admission.
    let fresh = hub
        .subscribe(&a, "fresh-connection", Filter::default(), None)
        .unwrap();
    let new_id = fresh["subscription_id"].as_str().unwrap();
    assert_ne!(new_id, old_id);
    assert_eq!(fresh["status"], "subscribed");
    assert_eq!(fresh["duplicate"], false);
    publish(&hub, "owner", "client", 1);
    assert_eq!(
        hub.read(&a, new_id, 64).unwrap()["events"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(hub.read(&a, old_id, 64).is_err());
    assert_eq!(
        hub.subscription_clients(&a, old_id).unwrap(),
        vec!["client"]
    );
}

#[test]
fn managed_selection_preserves_ack_and_invalidates_old_attempted_delivery() {
    let hub = Hub::default();
    let main = actor("owner", "main", "main_agent");
    let id = subscriber(&hub, &main, "managed", None);
    publish(&hub, &main.owner, &main.client_id, 1);
    let first = hub.read(&main, &id, 64).unwrap();
    hub.ack(&main, &id, sequence(&first)).unwrap();
    let progress = publish(&hub, &main.owner, &main.client_id, 2);
    let attempted = hub.read(&main, &id, 64).unwrap();
    assert_eq!(
        hub.contains_pending_event(&main, &id, progress["event_id"].as_str().unwrap()),
        Ok(true)
    );
    let changed = hub
        .update_kinds(&main, &id, vec!["worker.completed".into()])
        .unwrap();
    assert_eq!(changed["cursor"], first["next_cursor"]);
    assert_eq!(
        hub.subscription_clients(&main, &id).unwrap(),
        vec!["main".to_owned()]
    );
    assert!(hub.ack(&main, &id, sequence(&attempted)).is_err());
    assert_eq!(
        hub.contains_pending_event(&main, &id, progress["event_id"].as_str().unwrap()),
        Ok(false)
    );
    let completed = hub
        .publish_system(
            &main.owner,
            &main.client_id,
            Some("worker"),
            "worker.completed",
            json!({"task_status":"completed"}),
            (None, None),
        )
        .unwrap();
    assert_eq!(
        hub.contains_pending_event(&main, &id, completed["event_id"].as_str().unwrap()),
        Ok(true)
    );
    let page = hub.read(&main, &id, 64).unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["id"], completed["event_id"]);
    assert_eq!(hub.read(&main, &id, 64).unwrap()["events"], page["events"]);
    hub.ack(&main, &id, sequence(&page)).unwrap();
    assert_eq!(
        hub.contains_pending_event(&main, &id, completed["event_id"].as_str().unwrap()),
        Ok(false)
    );
    let peer = actor("owner", "main", "other_agent");
    assert!(
        hub.update_kinds(&peer, &id, vec!["worker.progress".into()])
            .is_err()
    );
}

#[test]
fn managed_wake_check_reports_evicted_history_without_claiming_delivery() {
    let hub = Hub::default();
    let main = actor("owner", "main", "main_agent");
    let id = subscriber(&hub, &main, "managed_gap", Some(0));
    let first = publish(&hub, &main.owner, &main.client_id, 0);
    for index in 1..=1100 {
        publish(&hub, &main.owner, &main.client_id, index);
    }
    assert!(
        hub.contains_pending_event(&main, &id, first["event_id"].as_str().unwrap(),)
            .unwrap_err()
            .starts_with("events_gap:")
    );
    assert!(
        hub.read(&main, &id, 1)
            .unwrap_err()
            .starts_with("events_gap:")
    );
}

#[test]
fn unrelated_evictions_do_not_create_a_gap_in_an_existing_subscription() {
    let hub = Hub::default();
    let operator = actor("owner", "device-A", "coordinator");
    let sub = subscriber(&hub, &operator, "watch-A", None);
    for i in 0..=MAX_EVENTS {
        publish(&hub, "owner", "device-B", i);
    }
    let own = publish(&hub, "owner", "device-A", 0);
    let page = hub.read(&operator, &sub, 1).unwrap();
    assert_eq!(page["cursor"], 0);
    assert_eq!(page["events"][0]["id"], own["event_id"]);
    assert_eq!(page["next_cursor"], own["seq"]);
    assert_eq!(
        hub.read(&operator, &sub, 1).unwrap()["events"],
        page["events"]
    );
    hub.ack(&operator, &sub, sequence(&page)).unwrap();
    assert_eq!(hub.read(&operator, &sub, 1).unwrap()["events"], json!([]));
}

#[test]
fn oversized_system_event_does_not_erase_another_devices_pending_receipt() {
    let hub = Hub::default();
    let a = actor("owner", "device-A", "coordinator");
    let b = actor("owner", "device-B", "coordinator");
    let sub_a = subscriber(&hub, &a, "watch-A", None);
    let sub_b = subscriber(&hub, &b, "watch-B", None);
    let kept = publish(&hub, "owner", "device-A", 0);
    let missed = hub.publish_system(
        "owner",
        "device-B",
        Some("worker"),
        "worker.progress",
        json!({"summary":"x".repeat(MAX_EVENT_BYTES)}),
        (None, None),
    );
    assert!(missed.is_err());
    let page = hub.read(&a, &sub_a, 2).unwrap();
    assert_eq!(page["events"][0]["id"], kept["event_id"]);
    assert!(
        hub.read(&b, &sub_b, 2)
            .unwrap_err()
            .starts_with("events_gap:")
    );
}

#[test]
fn narrowing_selected_kinds_after_unrelated_eviction_does_not_invent_loss() {
    let hub = Hub::default();
    let a = actor("owner", "device-A", "coordinator");
    let id = subscriber(&hub, &a, "narrow", None);
    for i in 0..=MAX_EVENTS {
        publish(&hub, "owner", "unrelated", i);
    }
    hub.update_kinds(&a, &id, vec!["worker.*".into()]).unwrap();
    hub.update_kinds(&a, &id, vec!["worker.progress".into()])
        .unwrap();
    let own = publish(&hub, "owner", "device-A", 0);
    assert_eq!(
        hub.read(&a, &id, 2).unwrap()["events"][0]["id"],
        own["event_id"]
    );
}
