use super::*;

fn remote(subject: &str, thread: Option<&str>) -> Caller {
    Caller {
        generation: 7,
        session: thread.map(|_| "scope_fixture".into()),
        owner: Owner::Remote {
            provider: "github".into(),
            subject: subject.into(),
            thread: thread.map(str::to_owned),
        },
    }
}

fn fixture() -> Arc<Hub> {
    let hub = Arc::new(Hub::default());
    hub.reset_for(Some("scope_fixture"), 7);
    hub
}

fn lease(hub: &Hub, caller: &Caller, key: Option<&str>) -> (String, String) {
    let result = hub
        .open_for("Program".into(), caller, key.map(str::to_owned))
        .unwrap();
    (
        result["listener_id"].as_str().unwrap().into(),
        result["token"].as_str().unwrap().into(),
    )
}

#[tokio::test]
async fn listener_credentials_do_not_cross_accounts_or_registered_threads() {
    let hub = fixture();
    let alice = remote("123", Some("agent_one"));
    let (id, token) = lease(&hub, &alice, None);
    for other in [
        remote("456", Some("agent_one")),
        remote("123", None),
        remote("123", Some("agent_two")),
    ] {
        assert!(
            hub.read_for(id.clone(), token.clone(), 0, other.clone())
                .await
                .is_err()
        );
        assert!(hub.close_for(&id, &token, &other).is_err());
    }
    assert!(hub.target_for(None).is_err());
    assert!(hub.target_for(Some("agent_two")).is_err());
    let ticket = hub
        .submit(
            hub.target_for(Some("agent_one")).unwrap(),
            "exact thread".into(),
            "exact thread".into(),
        )
        .unwrap();
    let read = hub
        .read_for(id.clone(), token.clone(), 0, alice.clone())
        .await
        .unwrap();
    assert_eq!(read["delivery"]["text"], "exact thread");
    hub.ack_for(
        &id,
        &token,
        &ticket.id,
        Ack {
            status: AckStatus::Accepted,
            reason: None,
        },
        &alice,
    )
    .unwrap();
    assert_eq!(ticket.wait().await.unwrap().status, Status::Accepted);
}

#[tokio::test]
async fn listener_open_idempotency_is_caller_scoped_and_does_not_resume_closed_leases() {
    let hub = fixture();
    let alice = remote("123", None);
    let bob = remote("456", None);
    let (id, token) = lease(&hub, &alice, Some("request_1"));
    let same = lease(&hub, &alice, Some("request_1"));
    assert_eq!(same, (id.clone(), token.clone()));
    assert!(
        hub.open_for("Program".into(), &bob, Some("request_1".into()))
            .unwrap_err()
            .contains("listener_busy")
    );
    assert!(
        hub.open_for("Different".into(), &alice, Some("request_1".into()))
            .unwrap_err()
            .contains("idempotency_conflict")
    );
    hub.close_for(&id, &token, &alice).unwrap();
    assert!(
        hub.open_for("Program".into(), &alice, Some("request_1".into()))
            .unwrap_err()
            .contains("idempotency_expired")
    );
    let (new_id, _) = lease(&hub, &bob, Some("request_1"));
    assert_ne!(id, new_id);
    assert!(!hub.snapshot().to_string().contains(&token));
    assert!(
        hub.call_for(
            alice,
            json!({"op":"open","label":"spoof","thread_id":"agent_one"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn listener_wait_rechecks_generation_and_session_after_connection_change() {
    let hub = fixture();
    let alice = remote("123", None);
    let (id, token) = lease(&hub, &alice, None);
    let task = tokio::spawn({
        let hub = hub.clone();
        let alice = alice.clone();
        async move { hub.read_for(id, token, 5000, alice).await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while hub.snapshot()["listener"]["reading"] != true {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    hub.reset_for(Some("scope_fixture_new"), 8);
    assert!(
        task.await
            .unwrap()
            .unwrap_err()
            .contains("listener_scope_expired")
    );
    assert!(hub.open_for("old generation".into(), &alice, None).is_err());
    let mut other_session = remote("123", Some("agent_one"));
    other_session.generation = 8;
    assert!(
        hub.open_for("wrong session".into(), &other_session, None)
            .is_err()
    );
}

#[tokio::test]
async fn listener_local_delegation_shares_cursors_ack_and_unknown_state_without_replay() {
    let hub = fixture();
    let alice = remote("123", None);
    let local = Caller {
        generation: 7,
        session: None,
        owner: Owner::Local {
            uid: 1,
            pid: 2,
            started: 3,
        },
    };
    let (id, token) = lease(&hub, &alice, None);
    let first = hub
        .submit(hub.target_for(None).unwrap(), "one".into(), "one".into())
        .unwrap();
    let from_remote = hub
        .read_for(id.clone(), token.clone(), 0, alice.clone())
        .await
        .unwrap();
    let from_local = hub
        .read_for(id.clone(), token.clone(), 0, local.clone())
        .await
        .unwrap();
    assert_eq!(from_remote, from_local);
    assert_eq!(from_remote["delivery"]["cursor"], 1);
    hub.ack_for(
        &id,
        &token,
        &first.id,
        Ack {
            status: AckStatus::Accepted,
            reason: None,
        },
        &local,
    )
    .unwrap();
    assert_eq!(first.wait().await.unwrap().cursor, 1);
    assert!(
        hub.read_for(id.clone(), token.clone(), 0, alice.clone())
            .await
            .unwrap()["delivery"]
            .is_null()
    );
    let second = hub
        .submit(hub.target_for(None).unwrap(), "two".into(), "two".into())
        .unwrap();
    let offered = hub
        .read_for(id.clone(), token.clone(), 0, local)
        .await
        .unwrap();
    assert_eq!(offered["delivery"]["cursor"], 2);
    hub.close_for(&id, &token, &alice).unwrap();
    assert_eq!(second.wait().await.unwrap().status, Status::Unknown);
    let (next, next_token) = lease(&hub, &alice, None);
    assert!(hub.read_for(next, next_token, 0, alice).await.unwrap()["delivery"].is_null());
}

#[tokio::test]
async fn listener_status_is_atomic_with_connection_generation() {
    let hub = fixture();
    let caller = remote("123", None);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let switching = {
        let hub = hub.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..4096 {
                hub.reset_for(Some("new_scope"), 8);
                std::thread::yield_now();
                hub.reset_for(Some("scope_fixture"), 7);
            }
        })
    };
    barrier.wait();
    for _ in 0..4096 {
        if let Ok(status) = hub.call_for(caller.clone(), json!({"op":"status"})).await {
            assert_eq!(status["connection_id"], "scope_fixture");
        }
        std::thread::yield_now();
    }
    switching.join().unwrap();
    assert_eq!(
        hub.call_for(caller.clone(), json!({"op":"status"}))
            .await
            .unwrap()["connection_id"],
        "scope_fixture"
    );
    hub.reset_for(Some("new_scope"), 8);
    assert!(
        hub.call_for(caller, json!({"op":"status"}))
            .await
            .unwrap_err()
            .contains("listener_scope_expired")
    );
    assert_eq!(hub.snapshot()["connection_id"], "new_scope");
}
