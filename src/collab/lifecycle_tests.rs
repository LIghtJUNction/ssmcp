use super::*;

fn member(hub: &Hub, owner: &str, role: &str) -> (String, String) {
    let joined = hub
        .join(owner, "participant".into(), Some(role.into()), vec![])
        .unwrap();
    (
        joined["agent_id"].as_str().unwrap().into(),
        joined["token"].as_str().unwrap().into(),
    )
}

fn snapshot(hub: &Hub, id: &str, token: &str) -> Value {
    hub.lifecycle_snapshot("owner", id, token).unwrap()
}

async fn update(hub: &Hub, id: &str, token: &str, mut patch: Value) -> Result<Value, String> {
    patch["agent_id"] = json!(id);
    patch["token"] = json!(token);
    patch["expected_revision"] = snapshot(hub, id, token)["lifecycle"]["revision"].clone();
    hub.dispatch("owner", "update", patch).await
}

fn native(tool: &str, arguments: Value) -> crate::wire::Operation {
    crate::wire::Operation::CallTool {
        name: format!("ssmcp.{tool}"),
        arguments,
    }
}

fn receipt(value: Value) -> Result<Value, String> {
    Ok(json!({"isError":false,"structuredContent":value}))
}

fn claim() -> Value {
    json!({"id":"original_claim","generation":"pool_generation","slot":0,
        "epoch":7,"task_id":"original_task","attempt":2})
}

fn assigned(hub: &Hub, id: &str, token: &str) {
    let operation = native("worker.wait", json!({"request_id":"original-wait"}));
    let mut call = hub
        .prepare_native_activity("owner", id, token, &operation)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(json!({"status":"assigned","claim":claim()})));
}

#[tokio::test]
async fn idle_deadline_notifies_without_any_roster_or_timer_poll() {
    let hub = Hub::for_session("deadline-room");
    let (id, token) = member(&hub, "owner", "main");
    update(&hub, &id, &token, json!({"idle_after_ms":1000}))
        .await
        .unwrap();
    // A pending worker assignment wait is not evidence of working. Its wait
    // windows and lease observations must not continually reset the idle clock.
    let operation = native("worker.wait", json!({"request_id":"idle-window"}));
    let mut waiting = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    waiting.dispatched();
    assert_eq!(snapshot(&hub, &id, &token)["status"], "idle");
    let (mut changes, lease) = {
        let state = hub.state.lock().unwrap();
        let agent = &state.agents[&id];
        (agent.changed.subscribe(), agent.expires)
    };
    tokio::time::timeout(Duration::from_secs(3), changes.changed())
        .await
        .expect("the Hub's independent deadline pump must notify")
        .unwrap();
    // Raw state inspection cannot cause a lazy transition and cannot renew lease.
    let state = hub.state.lock().unwrap();
    let agent = &state.agents[&id];
    assert_eq!(agent.lifecycle.status, Status::Dreaming);
    assert_eq!(agent.expires, lease);
    let round = agent.lifecycle.round.as_ref().unwrap();
    assert!(round.started_at_ms.is_none());
    assert!(round.finished_at_ms.is_none());
    assert_eq!(round.public()["status"], "requested");
}

#[tokio::test]
async fn new_input_preempts_running_maintenance_and_rejects_stale_reports() {
    let hub = Hub::for_session("input-room");
    let (id, token) = member(&hub, "owner", "main");
    let requested = update(&hub, &id, &token, json!({"status":"dreaming"}))
        .await
        .unwrap();
    let round = requested["agent"]["lifecycle"]["maintenance"]["round_id"].clone();
    let running = update(
        &hub,
        &id,
        &token,
        json!({"maintenance":{"round_id":round,"action":"start"}}),
    )
    .await
    .unwrap();
    let stale = running["agent"]["lifecycle"]["revision"].clone();
    assert_eq!(hub.presence().unwrap()[0].status, "dreaming");
    hub.user_input_queued("wrong-owner", &id, "input-1", 1);
    assert_eq!(snapshot(&hub, &id, &token)["status"], "dreaming");
    hub.user_input_queued("owner", &id, "input-1", 1);
    let preempted = snapshot(&hub, &id, &token);
    assert_eq!(preempted["status"], "working");
    assert_eq!(hub.presence().unwrap()[0].status, "working");
    assert_eq!(preempted["lifecycle"]["maintenance"]["status"], "unknown");
    assert!(preempted["lifecycle"]["unknown_outcomes"].as_u64().unwrap() > 0);
    assert!(
        hub.dispatch(
            "owner",
            "update",
            json!({"agent_id":id,"token":token,
        "expected_revision":stale,"maintenance":{"round_id":round,"action":"complete"}})
        )
        .await
        .unwrap_err()
        .starts_with("agent_lifecycle_stale_revision:")
    );
    hub.user_input_queued("owner", &id, "input-1", 1);
    assert_eq!(
        snapshot(&hub, &id, &token),
        preempted,
        "same queued input is not another wake"
    );
    hub.user_input_queued("owner", &id, "input-2", 1);
    assert_ne!(
        snapshot(&hub, &id, &token)["lifecycle"]["revision"],
        preempted["lifecycle"]["revision"],
        "fresh input invalidates an old CAS even while already working"
    );
    assert!(
        update(&hub, &id, &token, json!({"status":"idle"}))
            .await
            .is_err()
    );
    let before = snapshot(&hub, &id, &token);
    {
        let mut state = hub.state.lock().unwrap();
        let life = &mut state.agents.get_mut(&id).unwrap().lifecycle;
        assert!(
            life.advance(
                &id,
                Instant::now() + Duration::from_secs(3600),
                now_ms() + 3_600_000
            )
            .is_empty()
        );
    }
    assert_eq!(
        snapshot(&hub, &id, &token),
        before,
        "unknown maintenance never starts another round"
    );
    update(
        &hub,
        &id,
        &token,
        json!({"maintenance":{"round_id":round,"action":"complete"}}),
    )
    .await
    .unwrap();
    update(&hub, &id, &token, json!({"status":"idle"}))
        .await
        .unwrap();
    let next = update(&hub, &id, &token, json!({"status":"dreaming"}))
        .await
        .unwrap();
    assert_ne!(next["agent"]["lifecycle"]["maintenance"]["round_id"], round);
    let original = snapshot(&hub, &id, &token);
    assert!(
        update(
            &hub,
            &id,
            &token,
            json!({"idle_after_ms":2000,
        "maintenance":{"round_id":round,"action":"complete"}})
        )
        .await
        .is_err()
    );
    assert_eq!(
        snapshot(&hub, &id, &token),
        original,
        "stale round cannot partly edit configuration"
    );
}

#[tokio::test]
async fn retirement_drains_exact_claim_and_preserves_unknown_completion() {
    let hub = Hub::for_session("retirement-room");
    let (id, token) = member(&hub, "owner", "worker");
    assigned(&hub, &id, &token);
    assert_eq!(snapshot(&hub, &id, &token)["status"], "working");
    update(&hub, &id, &token, json!({"status":"retiring"}))
        .await
        .unwrap();
    let operation = native(
        "worker.complete",
        json!({"claim":claim(),"status":"completed","output":"actual result"}),
    );
    {
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &operation)
            .unwrap();
        call.dispatched();
        // Simulate a dropped HTTP future after native dispatch; no automatic retry.
    }
    let unknown = snapshot(&hub, &id, &token);
    assert_eq!(unknown["status"], "retiring");
    assert_eq!(unknown["lifecycle"]["active_claims"], 1);
    assert!(unknown["lifecycle"]["unknown_outcomes"].as_u64().unwrap() > 0);
    assert!(
        !hub.wake_for_worker_event("owner", &id, &token, "event-1")
            .unwrap()
    );
    {
        let mut state = hub.state.lock().unwrap();
        let life = &mut state.agents.get_mut(&id).unwrap().lifecycle;
        assert!(
            life.advance(
                &id,
                Instant::now() + Duration::from_secs(3600),
                now_ms() + 3_600_000
            )
            .is_empty()
        );
        assert_eq!(
            serde_json::to_value(&life.claims.values().next().unwrap().claim).unwrap(),
            claim()
        );
    }
    assert!(
        update(&hub, &id, &token, json!({"status":"offline"}))
            .await
            .is_err()
    );
    // Explicitly observing the native service's idempotent exact completion is
    // different from treating a dropped request or a state label as completion.
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(
        json!({"status":"recorded","claim":claim(),"task":{"status":"completed"}}),
    ));
    let drained = snapshot(&hub, &id, &token);
    assert_eq!(drained["status"], "offline");
    assert_eq!(drained["lifecycle"]["active_claims"], 0);
    assert_eq!(drained["lifecycle"]["unknown_outcomes"], 0);
    // Offline is not process exit or credential revocation. The native wait
    // retirement receipt/release protocol remains available to this same member.
    assert!(hub.bind("owner", &id, &token).is_ok());
    let release_wait = native("worker.wait", json!({"request_id":"release-receipt"}));
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &release_wait)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(
        json!({"status":"retiring","retirement":{"id":"actual-retirement"}}),
    ));
    assert_eq!(snapshot(&hub, &id, &token)["status"], "offline");
}

#[tokio::test]
async fn cancelled_wait_retains_request_identity_and_never_invents_a_claim() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "worker");
    let operation = native("worker.wait", json!({"request_id":"same-wait-id"}));
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    call.dispatched();
    drop(call);
    let unknown = snapshot(&hub, &id, &token);
    assert_eq!(unknown["lifecycle"]["active_claims"], 0);
    assert_eq!(unknown["lifecycle"]["unknown_outcomes"], 1);
    assert!(
        update(&hub, &id, &token, json!({"status":"dreaming"}))
            .await
            .is_err()
    );
    let prepared = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    drop(prepared); // Not dispatched: restores the original uncertainty.
    assert_eq!(snapshot(&hub, &id, &token), unknown);
    let mut retried = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    retried.dispatched();
    retried.finish(&receipt(json!({"status":"assigned","claim":claim()})));
    assert_eq!(
        snapshot(&hub, &id, &token)["lifecycle"]["unknown_outcomes"],
        0
    );
    assert_eq!(snapshot(&hub, &id, &token)["lifecycle"]["active_claims"], 1);
}

#[tokio::test]
async fn old_worker_wait_and_answer_receipts_cannot_erase_new_user_instruction() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "main");
    for (index, operation) in [
        native("worker.wait", json!({"request_id":"waiting"})),
        native("ui.message", json!({"kind":"answer","text":"answer"})),
    ]
    .iter()
    .enumerate()
    {
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, operation)
            .unwrap();
        call.dispatched();
        hub.user_input_queued("owner", &id, &format!("new-input-{index}"), 1);
        call.finish(&receipt(json!({"status":"waiting"})));
        assert_eq!(snapshot(&hub, &id, &token)["status"], "working");
    }
    let answer = native("ui.message", json!({"kind":"answer","text":"new answer"}));
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &answer)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(json!({"sent":true})));
    assert_eq!(snapshot(&hub, &id, &token)["status"], "idle");
}

#[tokio::test]
async fn passive_observations_do_not_reset_idle_or_execute_requested_maintenance() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "main");
    let before = snapshot(&hub, &id, &token);
    for tool in [
        "ui.get",
        "ui.wait",
        "time.now",
        "events.read",
        "worker.events",
        "client.capabilities",
    ] {
        let operation = native(tool, json!({}));
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &operation)
            .unwrap();
        call.dispatched();
        call.finish(&receipt(json!({})));
    }
    assert_eq!(snapshot(&hub, &id, &token), before);
    let requested = update(&hub, &id, &token, json!({"status":"dreaming"}))
        .await
        .unwrap();
    let phase = &requested["agent"]["lifecycle"]["maintenance"];
    assert_eq!(phase["status"], "requested");
    assert!(phase["started_at_ms"].is_null());
    // Expiring a request which never started is a cancellation, not completed work.
    let mut state = hub.state.lock().unwrap();
    let life = &mut state.agents.get_mut(&id).unwrap().lifecycle;
    life.advance(
        &id,
        Instant::now() + Duration::from_millis(DEFAULT_MAINTENANCE_BUDGET_MS + 1),
        now_ms(),
    );
    assert_eq!(life.public()["maintenance"]["status"], "cancelled");
    assert_eq!(life.status, Status::Idle);
}

#[tokio::test]
async fn lease_expiry_notifies_offline_and_keeps_token_revocation_authoritative() {
    let hub = Hub::for_session("expiry-room");
    let (id, token) = member(&hub, "owner", "main");
    let mut changes = {
        let mut state = hub.state.lock().unwrap();
        let agent = state.agents.get_mut(&id).unwrap();
        agent.expires = Instant::now() + Duration::from_millis(30);
        agent.changed.subscribe()
    };
    hub.lifecycle_changed.notify_one();
    tokio::time::timeout(Duration::from_secs(2), changes.changed())
        .await
        .unwrap()
        .unwrap();
    {
        let state = hub.state.lock().unwrap();
        assert!(!state.agents.contains_key(&id));
        let offline = state.offline_agent(&id).unwrap();
        assert_eq!(offline["status"], "offline");
        assert_eq!(offline["membership_active"], false);
        assert!(!offline.to_string().contains(&token));
    }
    assert!(hub.bind("owner", &id, &token).is_err());
    assert!(
        hub.wake_for_worker_event("owner", &id, &token, "late-event")
            .is_err()
    );
    let (new_id, new_token) = member(&hub, "owner", "main");
    assert_ne!(new_id, id);
    assert!(
        hub.wake_for_worker_event("owner", &new_id, &token, "forged")
            .is_err()
    );
    assert!(
        hub.wake_for_worker_event("different-owner", &new_id, &new_token, "forged")
            .is_err()
    );
    assert!(
        Hub::for_session("other-room")
            .wake_for_worker_event("owner", &new_id, &new_token, "forged")
            .is_err()
    );
}

#[tokio::test]
async fn five_agent_states_keep_task_results_separate_and_metadata_updates_compatible() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "main");
    for status in ["working", "idle", "offline", "dreaming", "retiring"] {
        assert!(serde_json::from_value::<Status>(json!(status)).is_ok());
    }
    for status in [
        "done",
        "completed",
        "failed",
        "cancelled",
        "exited",
        "available",
        "busy",
    ] {
        assert!(serde_json::from_value::<Status>(json!(status)).is_err());
    }
    let revision = snapshot(&hub, &id, &token)["lifecycle"]["revision"].clone();
    hub.dispatch(
        "owner",
        "update",
        json!({"agent_id":id,"token":token,"name":"Renamed"}),
    )
    .await
    .unwrap();
    assert_eq!(
        snapshot(&hub, &id, &token)["lifecycle"]["revision"],
        revision
    );
    assert!(
        hub.dispatch(
            "owner",
            "update",
            json!({"agent_id":id,"token":token,"status":"working"})
        )
        .await
        .is_err()
    );
    update(&hub, &id, &token, json!({"status":"working"}))
        .await
        .unwrap();
    let before = snapshot(&hub, &id, &token);
    update(&hub, &id, &token, json!({"status":"working"}))
        .await
        .unwrap();
    assert_eq!(snapshot(&hub, &id, &token), before);
    assert!(!before.to_string().contains(&token));
}

#[tokio::test]
async fn bounded_unknown_operations_apply_backpressure_before_native_dispatch() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "worker");
    for index in 0..MAX_TRACKED_WORK {
        let operation = native(
            "worker.wait",
            json!({"request_id":format!("unknown-{index}")}),
        );
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &operation)
            .unwrap();
        call.dispatched();
        drop(call);
    }
    let operation = native("worker.wait", json!({"request_id":"new-request"}));
    assert!(
        hub.prepare_native_activity("owner", &id, &token, &operation)
            .is_err()
    );
    let state = hub.state.lock().unwrap();
    assert_eq!(
        state.agents[&id].lifecycle.operations.len(),
        MAX_TRACKED_WORK
    );
    assert!(state.agents[&id].lifecycle.claims.is_empty());
}

#[tokio::test]
async fn maintenance_events_have_an_exact_member_audience_on_shared_connections() {
    use crate::events::{Actor, Filter};
    let events = Arc::new(crate::events::Hub::default());
    let session = "a".repeat(64);
    let hub = Hub::with_events(&session, events.clone());
    let (one, token) = member(&hub, "owner", "main");
    let (two, _) = member(&hub, "owner", "main");
    let (other, _) = member(&hub, "other-owner", "main");
    let subscriptions: Vec<_> = [
        ("owner", one.as_str()),
        ("owner", two.as_str()),
        ("other-owner", other.as_str()),
    ]
    .into_iter()
    .map(|(owner, id)| {
        let actor = Actor {
            owner: owner.into(),
            client_id: session.clone(),
            agent_id: id.into(),
        };
        let filter: Filter =
            serde_json::from_value(json!({"kinds":["agent.maintenance_requested"]})).unwrap();
        let sub = events
            .subscribe(&actor, "maintenance", filter, None)
            .unwrap();
        (actor, sub["subscription_id"].as_str().unwrap().to_owned())
    })
    .collect();
    let requested = update(&hub, &one, &token, json!({"status":"dreaming"}))
        .await
        .unwrap();
    let pages: Vec<_> = subscriptions
        .iter()
        .map(|(actor, sub)| events.read(actor, sub, 8).unwrap())
        .collect();
    assert_eq!(pages[0]["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        pages[0]["events"][0]["data"]["maintenance"]["round_id"],
        requested["agent"]["lifecycle"]["maintenance"]["round_id"]
    );
    assert!(!pages[0].to_string().contains(&token));
    assert!(pages[1]["events"].as_array().unwrap().is_empty());
    assert!(pages[2]["events"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn nonblocking_claim_alias_tracks_the_same_immutable_worker_claim() {
    let hub = Hub::default();
    let (id, token) = member(&hub, "owner", "worker");
    let operation = native("worker.claim", json!({"request_id":"nonblocking-request"}));
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    call.dispatched();
    assert_eq!(snapshot(&hub, &id, &token)["status"], "idle");
    call.finish(&receipt(json!({"status":"assigned","claim":claim()})));
    let observed = snapshot(&hub, &id, &token);
    assert_eq!(observed["status"], "working");
    assert_eq!(observed["lifecycle"]["active_claims"], 1);
    assert!(
        update(&hub, &id, &token, json!({"status":"dreaming"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn completed_wait_retry_reconciles_only_its_exact_claim() {
    for (lost_completion, unrelated_unknown) in [(false, false), (true, false), (true, true)] {
        let hub = Hub::for_session("completed-wait-room");
        let (id, token) = member(&hub, "owner", "worker");
        assigned(&hub, &id, &token);
        let complete = native(
            "worker.complete",
            json!({"claim":claim(),"status":"completed","output":"actual result"}),
        );
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &complete)
            .unwrap();
        call.dispatched();
        if lost_completion {
            drop(call);
        } else {
            call.finish(&receipt(json!({
                "status":"recorded","claim":claim(),
                "task":{"status":"completed"}
            })));
        }
        if unrelated_unknown {
            let separate = native(
                "worker.wait",
                json!({"request_id":"unrelated-unknown-wait"}),
            );
            let mut call = hub
                .prepare_native_activity("owner", &id, &token, &separate)
                .unwrap();
            call.dispatched();
            drop(call);
        }
        let original = native("worker.wait", json!({"request_id":"original-wait"}));
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &original)
            .unwrap();
        call.dispatched();
        call.finish(&receipt(json!({
            "status":"already_completed","claim":claim(),
            "task":{"status":"completed"}
        })));
        let current = snapshot(&hub, &id, &token);
        assert_eq!(current["lifecycle"]["active_claims"], 0);
        assert_eq!(current["lifecycle"]["in_flight"], 0);
        assert_eq!(
            current["lifecycle"]["unknown_outcomes"],
            usize::from(unrelated_unknown)
        );
        assert_eq!(
            current["status"],
            if unrelated_unknown { "working" } else { "idle" }
        );
    }
}

#[tokio::test]
async fn native_wait_stop_receipts_do_not_invent_unknown_work() {
    for status in ["retiring", "released", "not_accepting"] {
        let hub = Hub::for_session("native-stop-room");
        let (id, token) = member(&hub, "owner", "worker");
        let operation = native("worker.wait", json!({"request_id":"native-stop-receipt"}));
        let mut reply = json!({"status":status,"execution":"remote"});
        if status != "not_accepting" {
            reply["retirement"] = json!({
                "id":"native-retirement","generation":"pool_generation",
                "slot":0,"epoch":7
            });
        }
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &operation)
            .unwrap();
        call.dispatched();
        call.finish(&receipt(reply));
        let current = snapshot(&hub, &id, &token);
        assert_eq!(current["status"], "offline", "{status}");
        assert_eq!(current["lifecycle"]["active_claims"], 0, "{status}");
        assert_eq!(current["lifecycle"]["in_flight"], 0, "{status}");
        assert_eq!(current["lifecycle"]["unknown_outcomes"], 0, "{status}");
    }
}

#[tokio::test]
async fn completed_wait_preserves_unrelated_or_unproven_claims() {
    for case in ["different_generation", "missing_claim", "nonterminal"] {
        let hub = Hub::for_session("unproven-wait-room");
        let (id, token) = member(&hub, "owner", "worker");
        assigned(&hub, &id, &token);
        let complete = native(
            "worker.complete",
            json!({"claim":claim(),"status":"completed","output":"actual result"}),
        );
        let mut lost = hub
            .prepare_native_activity("owner", &id, &token, &complete)
            .unwrap();
        lost.dispatched();
        drop(lost);
        let before = snapshot(&hub, &id, &token);
        let mut reply =
            json!({"status":"already_completed","claim":claim(),"task":{"status":"completed"}});
        match case {
            "different_generation" => reply["claim"]["generation"] = json!("different_pool"),
            "missing_claim" => {
                reply.as_object_mut().unwrap().remove("claim");
            }
            "nonterminal" => reply["task"]["status"] = json!("running"),
            _ => unreachable!(),
        }
        let original = native("worker.wait", json!({"request_id":"original-wait"}));
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &original)
            .unwrap();
        call.dispatched();
        call.finish(&receipt(reply));
        let current = snapshot(&hub, &id, &token);
        assert_eq!(current["lifecycle"]["active_claims"], 1, "{case}");
        assert!(
            current["lifecycle"]["unknown_outcomes"].as_u64().unwrap()
                >= before["lifecycle"]["unknown_outcomes"].as_u64().unwrap(),
            "{case}"
        );
        assert_eq!(current["status"], "working", "{case}");
    }
}

#[tokio::test]
async fn native_stop_receipt_retains_an_unrelated_unknown_wait() {
    for status in ["released", "not_accepting"] {
        let hub = Hub::for_session("unknown-stop-room");
        let (id, token) = member(&hub, "owner", "worker");
        let unknown = native("worker.wait", json!({"request_id":"separate-unobserved"}));
        let mut lost = hub
            .prepare_native_activity("owner", &id, &token, &unknown)
            .unwrap();
        lost.dispatched();
        drop(lost);
        let stop = native("worker.wait", json!({"request_id":"stop"}));
        let mut call = hub
            .prepare_native_activity("owner", &id, &token, &stop)
            .unwrap();
        call.dispatched();
        call.finish(&receipt(
            json!({"status":status,"retirement":{"id":"actual-retirement"}}),
        ));
        let current = snapshot(&hub, &id, &token);
        assert_eq!(current["status"], "retiring");
        assert_eq!(current["lifecycle"]["unknown_outcomes"], 1);
    }
}

#[tokio::test]
async fn visible_generated_input_keeps_requested_maintenance_passive() {
    let hub = Hub::for_session("visible-prompt-room");
    let (id, token) = member(&hub, "owner", "main");
    update(&hub, &id, &token, json!({"status":"dreaming"}))
        .await
        .unwrap();
    let before = snapshot(&hub, &id, &token);
    let operation = native(
        "ui.message",
        json!({
            "kind":"input","input_origin":"maintenance","idempotency_key":"visible_round",
            "text":"One authorized review","finish_task":false
        }),
    );
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &operation)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(
        json!({"kind":"input","accepted":true,"recorded":true,
        "input_origin":"maintenance","completion_permission":false}),
    ));
    let after = snapshot(&hub, &id, &token);
    assert_eq!(after["status"], "dreaming");
    assert_eq!(
        after["lifecycle"]["maintenance"],
        before["lifecycle"]["maintenance"]
    );
    assert_eq!(
        after["lifecycle"]["revision"],
        before["lifecycle"]["revision"]
    );
    assert_eq!(after["lifecycle"]["active_claims"], 0);
}

#[tokio::test]
async fn deadline_releases_only_the_acked_members_empty_inbox_once() {
    let hub = Hub::for_session("housekeeping");
    let (id, token) = member(&hub, "owner", "worker");
    let (other, other_token) = member(&hub, "owner", "worker");
    let send = |to: &str, key: &str| {
        hub.send(
            "owner",
            &other,
            &other_token,
            Send {
                to: to.into(),
                payload: json!("keep until acknowledged"),
                topic: None,
                reply_to: None,
                idempotency_key: Some(key.into()),
            },
        )
        .unwrap()
    };
    let seq = send(&id, "first")["seq"].as_u64().unwrap();
    let pending = send(&other, "other");
    hub.receive("owner", &id, &token, None, 1, 0).await.unwrap();
    hub.ack("owner", &id, &token, seq).unwrap();
    update(&hub, &id, &token, json!({"idle_after_ms":1000}))
        .await
        .unwrap();
    let (mut changed, capacity) = {
        let state = hub.state.lock().unwrap();
        let agent = &state.agents[&id];
        (agent.changed.subscribe(), agent.inbox.capacity())
    };
    assert!(capacity > 0);
    tokio::time::timeout(Duration::from_secs(3), changed.changed())
        .await
        .unwrap()
        .unwrap();
    let first = snapshot(&hub, &id, &token);
    let round = &first["lifecycle"]["maintenance"];
    assert_eq!(round["status"], "requested");
    assert_eq!(round["local_housekeeping"]["released_slots"], capacity);
    assert_eq!(
        round["local_housekeeping"]["released_slot_bytes"],
        capacity * std::mem::size_of::<Arc<Message>>()
    );
    {
        let state = hub.state.lock().unwrap();
        assert_eq!(state.agents[&id].inbox.capacity(), 0);
        assert_eq!(state.agents[&id].acked, seq);
        assert_eq!(state.agents[&other].inbox.len(), 1);
    }
    assert_eq!(send(&id, "first")["duplicate"], true);
    assert_eq!(
        hub.receive("owner", &other, &other_token, None, 1, 0)
            .await
            .unwrap()["messages"][0]["message_id"],
        pending["message_id"]
    );

    // Refill and acknowledge while the same round is open. A duplicate driver
    // notification must keep this new allocation and the original receipt.
    let next = send(&id, "second")["seq"].as_u64().unwrap();
    hub.receive("owner", &id, &token, None, 1, 0).await.unwrap();
    hub.ack("owner", &id, &token, next).unwrap();
    {
        let mut state = hub.state.lock().unwrap();
        advance(&mut state, None, "housekeeping", true);
        assert!(state.agents[&id].inbox.capacity() > 0);
    }
    let again = snapshot(&hub, &id, &token);
    assert_eq!(
        again["lifecycle"]["revision"],
        first["lifecycle"]["revision"]
    );
    assert_eq!(
        again["lifecycle"]["maintenance"],
        first["lifecycle"]["maintenance"]
    );
    assert_eq!(
        again["lifecycle"]["dreaming_since_ms"],
        first["lifecycle"]["dreaming_since_ms"]
    );
    for action in ["start", "complete"] {
        let result = update(
            &hub,
            &id,
            &token,
            json!({"maintenance":{"round_id":round["round_id"],"action":action}}),
        )
        .await
        .unwrap();
        assert_eq!(
            result["agent"]["lifecycle"]["maintenance"]["local_housekeeping"],
            round["local_housekeeping"]
        );
    }
}

#[tokio::test]
async fn housekeeping_preserves_unacknowledged_input_and_yields_to_work() {
    let hub = Hub::for_session("housekeeping");
    let (id, token) = member(&hub, "owner", "worker");
    let receipt = hub
        .send(
            "owner",
            &id,
            &token,
            Send {
                to: id.clone(),
                payload: json!("pending"),
                topic: None,
                reply_to: None,
                idempotency_key: None,
            },
        )
        .unwrap();
    let page = hub.receive("owner", &id, &token, None, 1, 0).await.unwrap();
    let due = |hub: &Hub| {
        let mut state = hub.state.lock().unwrap();
        let life = &mut state.agents.get_mut(&id).unwrap().lifecycle;
        life.idle_since = Some(Instant::now() - Duration::from_millis(life.idle_after_ms));
    };
    due(&hub);
    {
        let mut state = hub.state.lock().unwrap();
        let capacity = state.agents[&id].inbox.capacity();
        advance(&mut state, None, "housekeeping", true);
        assert_eq!(state.agents[&id].inbox.capacity(), capacity);
    }
    assert_eq!(
        snapshot(&hub, &id, &token)["lifecycle"]["maintenance"]["local_housekeeping"]["released_slots"],
        0
    );
    assert_eq!(
        hub.receive("owner", &id, &token, None, 1, 0).await.unwrap()["messages"],
        page["messages"]
    );
    hub.ack("owner", &id, &token, receipt["seq"].as_u64().unwrap())
        .unwrap();
    // An already-due round may be noticed while locking to deliver human input
    // or an assignment. It must not clean the inbox in front of that work.
    for human in [true, false] {
        update(&hub, &id, &token, json!({"status":"idle"}))
            .await
            .unwrap();
        due(&hub);
        if human {
            hub.user_input_queued("owner", &id, "real-input", 1);
        } else {
            assigned(&hub, &id, &token);
        }
        let mut state = hub.state.lock().unwrap();
        advance(&mut state, None, "housekeeping", true);
        let agent = &state.agents[&id];
        assert_eq!(agent.lifecycle.status, Status::Working);
        assert!(agent.inbox.capacity() > 0);
        assert!(
            agent
                .lifecycle
                .round
                .as_ref()
                .unwrap()
                .released_inbox_slots
                .is_none()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn dreaming_episode_outlives_bounded_chores_and_input_resets_its_clock() {
    let now = Instant::now();
    let mut life = Lifecycle::new(now, 1_000);
    let mut inbox = VecDeque::new();
    let idle = Duration::from_millis(DEFAULT_IDLE_AFTER_MS);
    assert!(
        life.advance("member", now + idle - Duration::from_millis(1), 99)
            .is_empty()
    );
    assert!(life.dreaming_since_ms().is_none());
    tokio::time::advance(idle).await;
    let mut changes = life.advance("member", Instant::now(), 301_000);
    assert!(life.dreaming_since_ms().is_none());
    life.release_empty_inbox(&mut inbox, Instant::now(), 301_000, &mut changes);
    assert_eq!(life.dreaming_since_ms(), Some(301_000));
    tokio::time::advance(Duration::from_millis(DEFAULT_MAINTENANCE_BUDGET_MS)).await;
    life.advance("member", Instant::now(), 331_000);
    assert_eq!(life.public()["maintenance"]["status"], "cancelled");
    assert_eq!(life.status, Status::Dreaming);
    assert!(life.next_deadline().is_none());
    tokio::time::advance(Duration::from_secs(1770)).await;
    assert_eq!(life.public()["dreaming_elapsed_ms"], 1_800_000);
    life.wake(
        "human-input".into(),
        "user_input",
        Instant::now(),
        2_101_000,
    );
    assert_eq!(life.status, Status::Working);
    assert!(life.public()["dreaming_elapsed_ms"].is_null());
}

#[tokio::test(start_paused = true)]
async fn main_activity_tracks_dispatch_receipts_and_membership_time_without_lease_progress() {
    use crate::wire::MainPhase;
    let hub = Hub::for_session("main-display");
    let (id, token) = member(&hub, "owner", "main");
    let observation = || {
        hub.display_presence(true)
            .unwrap()
            .into_iter()
            .find(|agent| agent.agent_id == id)
            .unwrap()
            .activity
            .unwrap()
    };
    let start = |name, args| {
        hub.prepare_native_activity("owner", &id, &token, &native(name, args))
            .unwrap()
    };
    assert!(hub.presence().unwrap()[0].activity.is_none());
    let (worker, _) = member(&hub, "owner", "worker");
    assert!(
        hub.display_presence(true)
            .unwrap()
            .iter()
            .find(|a| a.agent_id == worker)
            .unwrap()
            .activity
            .is_none()
    );
    for _ in 0..18 {
        tokio::time::advance(Duration::from_secs(600)).await;
        hub.bind("owner", &id, &token).unwrap();
        for (name, args) in [
            ("ui.get", json!({})),
            ("time.now", json!({})),
            ("exec.status", json!({})),
            ("exec.wait", json!({"wait_ms":0})),
            ("ui.message", json!({"kind":"input"})),
        ] {
            let mut read = start(name, args);
            read.dispatched();
            read.finish(&receipt(json!({})));
        }
    }
    let old = observation();
    assert_eq!(old.connected_ms, 3 * 3_600_000);
    assert_eq!((old.phase, old.observed_age_ms), (MainPhase::Quiet, None));
    let mut work = start("fs.edit_text", json!({}));
    assert_eq!(observation().phase, MainPhase::Quiet);
    work.dispatched();
    let mut wait = start("exec.wait", json!({"wait_ms":45000}));
    wait.dispatched();
    assert_eq!(observation().phase, MainPhase::Work);
    work.finish(&receipt(json!({})));
    assert_eq!(observation().phase, MainPhase::Wait);
    drop(wait);
    assert_eq!(observation().phase, MainPhase::Recent);
    tokio::time::advance(Duration::from_secs(75)).await;
    hub.bind("owner", &id, &token).unwrap();
    assert_eq!(observation().observed_age_ms, Some(75_000));
    let mut lost = start("exec.start", json!({}));
    lost.dispatched();
    drop(lost);
    assert_eq!(observation().phase, MainPhase::Unknown);
    let (new, _) = member(&hub, "owner", "main");
    let second = hub
        .display_presence(true)
        .unwrap()
        .into_iter()
        .find(|agent| agent.agent_id == new)
        .unwrap()
        .activity
        .unwrap();
    assert_eq!((second.connected_ms, second.phase), (0, MainPhase::Quiet));
}

#[tokio::test]
async fn progress_advice_requires_its_original_authenticated_known_claim() {
    let hub = Hub::for_session("progress-test");
    let (id, token) = member(&hub, "owner", "worker");
    let (other, key) = member(&hub, "owner", "worker");
    let known = claim();
    let claim_id = known["id"].as_str().unwrap();
    assert!(!hub.has_confirmed_claim("owner", &id, &token, claim_id));
    assigned(&hub, &id, &token);
    assert!(hub.has_confirmed_claim("owner", &id, &token, claim_id));
    assert!(!hub.has_confirmed_claim("owner", &other, &key, claim_id));
    assert!(!hub.has_confirmed_claim("owner", &id, "bad", claim_id));
    let op = native("worker.complete", json!({"claim":known}));
    let mut call = hub
        .prepare_native_activity("owner", &id, &token, &op)
        .unwrap();
    call.dispatched();
    call.finish(&receipt(
        json!({"status":"recorded","claim":known,"task":{"status":"completed"}}),
    ));
    assert!(!hub.has_confirmed_claim("owner", &id, &token, claim_id));
}
