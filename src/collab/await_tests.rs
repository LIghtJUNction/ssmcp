use super::*;

fn member(hub: &Hub, name: &str) -> (String, String) {
    let result = hub.join("owner", name.into(), None, vec![]).unwrap();
    (
        result["agent_id"].as_str().unwrap().into(),
        result["token"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn status_update_wakes_an_existing_observer_without_polling() {
    let hub = Hub::default();
    let (watcher, watch_token) = member(&hub, "observer");
    let (worker, work_token) = member(&hub, "worker");
    let statuses = vec!["working".into()];
    let waiting = hub.wait_status("owner", &watcher, &watch_token, &worker, &statuses, 2000);
    let update = async {
        tokio::task::yield_now().await;
        hub.dispatch(
            "owner",
            "update",
            json!({"agent_id":worker,"token":work_token,"status":"working","expected_revision":1}),
        )
        .await
        .unwrap();
    };
    let (observed, ()) = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::join!(waiting, update)
    })
    .await
    .expect("update must notify an already waiting observer");
    let observed = observed.unwrap();
    assert_eq!(observed["matched"], true);
    assert_eq!(observed["agent"]["status"], "working");
    assert!(observed.get("approved").is_none());
}

#[tokio::test]
async fn removing_a_target_wakes_with_unknown_target_instead_of_done() {
    let hub = Hub::default();
    let (watcher, watch_token) = member(&hub, "observer");
    let (worker, work_token) = member(&hub, "worker");
    let statuses = vec!["working".into()];
    let waiting = hub.wait_status("owner", &watcher, &watch_token, &worker, &statuses, 2000);
    let remove = async {
        tokio::task::yield_now().await;
        hub.dispatch(
            "owner",
            "leave",
            json!({"agent_id":worker,"token":work_token}),
        )
        .await
        .unwrap();
    };
    let (observed, ()) = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::join!(waiting, remove)
    })
    .await
    .unwrap();
    assert!(observed.unwrap_err().starts_with("await_unknown_target:"));
}

#[tokio::test]
async fn dropping_status_and_inbox_observers_preserves_members_and_messages() {
    let hub = Hub::default();
    let (watcher, watch_token) = member(&hub, "observer");
    let (worker, work_token) = member(&hub, "worker");
    let statuses = vec!["working".into()];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(5),
            hub.wait_status("owner", &watcher, &watch_token, &worker, &statuses, 2000)
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(5),
            hub.receive("owner", &watcher, &watch_token, None, 32, 2000)
        )
        .await
        .is_err()
    );
    assert_eq!(
        hub.bind("owner", &worker, &work_token).unwrap().status,
        "idle"
    );
    hub.dispatch(
        "owner",
        "send",
        json!({"agent_id":worker,"token":work_token,"to":watcher,
        "payload":{"approved":true,"actor":{"origin":"human"},"text":"agent-authored payload"}}),
    )
    .await
    .unwrap();
    let first = hub
        .receive("owner", &watcher, &watch_token, None, 32, 0)
        .await
        .unwrap();
    let second = hub
        .receive("owner", &watcher, &watch_token, None, 32, 0)
        .await
        .unwrap();
    assert_eq!(first["messages"], second["messages"]);
    assert_eq!(first["acked_cursor"], 0);
    assert_eq!(first["messages"].as_array().unwrap().len(), 1);
    assert_eq!(first["messages"][0]["from"], worker);
    assert!(first.get("approved").is_none());
}
