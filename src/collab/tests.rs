use super::*;

async fn join(hub: &Hub, owner: &str, name: &str) -> Value {
    hub.dispatch(owner, "join", json!({"name":name}))
        .await
        .unwrap()
}

async fn action(
    hub: &Hub,
    owner: &str,
    name: &str,
    agent: &Value,
    mut arguments: Value,
) -> Result<Value, String> {
    arguments["agent_id"] = agent["agent_id"].clone();
    arguments["token"] = agent["token"].clone();
    hub.dispatch(owner, name, arguments).await
}

async fn send(hub: &Hub, from: &Value, to: &Value, payload: Value) -> Value {
    action(
        hub,
        "owner",
        "send",
        from,
        json!({"to":to["agent_id"],"payload":payload}),
    )
    .await
    .unwrap()
}

async fn receive(hub: &Hub, agent: &Value) -> Value {
    action(hub, "owner", "receive", agent, json!({}))
        .await
        .unwrap()
}

async fn wait_started(hub: &Hub, agent: &Value) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let waiting = hub.state.lock().unwrap().agents[agent["agent_id"].as_str().unwrap()]
                .waiters
                .available_permits()
                == 0;
            if waiting {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receive started waiting");
}

fn spawn_receive(hub: &Arc<Hub>, agent: &Value) -> tokio::task::JoinHandle<Result<Value, String>> {
    let hub = hub.clone();
    let agent = agent.clone();
    tokio::spawn(async move {
        action(&hub, "owner", "receive", &agent, json!({"wait_ms":30_000})).await
    })
}

#[tokio::test]
async fn discovery_is_public_but_mailbox_credentials_bind_owner_and_agent() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "same name").await;
    let b = join(&hub, "owner", "same name").await;
    assert_ne!(a["agent_id"], b["agent_id"]);
    assert_ne!(a["token"], b["token"]);
    let list = hub.dispatch("owner", "list", json!({})).await.unwrap();
    let agents = list["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2);
    for agent in agents {
        assert!(agent.get("token").is_none());
        assert!(agent.get("owner").is_none());
        assert_eq!(agent["status"], "available");
    }
    assert!(
        action(&hub, "another-owner", "receive", &a, json!({}))
            .await
            .is_err()
    );
    let mut forged = b.clone();
    forged["token"] = a["token"].clone();
    assert!(
        action(&hub, "owner", "receive", &forged, json!({}))
            .await
            .is_err()
    );
    assert!(
        action(
            &Hub::default(),
            "owner",
            "update",
            &a,
            json!({"name":"other"})
        )
        .await
        .is_err()
    );
    let updated = action(
        &hub,
        "owner",
        "update",
        &a,
        json!({"name":"planner","role":"review","status":"busy","capabilities":["rust"]}),
    )
    .await
    .unwrap();
    assert_eq!(updated["agent"]["name"], "planner");
    assert_eq!(updated["agent"]["role"], "review");
    assert_eq!(updated["agent"]["capabilities"], json!(["rust"]));
    assert!(updated["agent"].get("token").is_none());
    let cleared = action(&hub, "owner", "update", &a, json!({"role":""}))
        .await
        .unwrap();
    assert!(cleared["agent"]["role"].is_null());
}

#[tokio::test]
async fn local_presence_and_run_binding_expose_no_participant_credentials() {
    let hub = Hub::default();
    let member = join(&hub, "owner", "reviewer").await;
    let id = member["agent_id"].as_str().unwrap();
    let token = member["token"].as_str().unwrap();
    assert!(hub.bind("another-owner", id, token).is_err());
    assert!(hub.bind("owner", id, &"0".repeat(64)).is_err());
    action(
        &hub,
        "owner",
        "update",
        &member,
        json!({"status":"reviewing"}),
    )
    .await
    .unwrap();
    let bound = hub.bind("owner", id, token).unwrap();
    assert_eq!(bound.agent_id, id);
    assert_eq!(bound.name, "reviewer");
    assert_eq!(bound.status, "reviewing");
    let presence = serde_json::to_value(hub.presence().unwrap()).unwrap();
    assert_eq!(presence[0]["agent_id"], id);
    assert_eq!(presence[0]["status"], "reviewing");
    assert!(!presence.to_string().contains(token));
    assert!(presence[0].get("owner").is_none());
    assert!(presence[0].get("inbox").is_none());
    action(&hub, "owner", "leave", &member, json!({}))
        .await
        .unwrap();
    assert!(hub.bind("owner", id, token).is_err());
    assert!(hub.presence().unwrap().is_empty());
}

#[tokio::test]
async fn private_messages_replay_until_ack_and_reject_skipping_unread_data() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let c = join(&hub, "owner", "c").await;
    let receipt = send(&hub, &a, &b, json!({"text":"hello","nested":[null,3,true]})).await;
    let seq = receipt["seq"].as_u64().unwrap();
    assert!(
        action(&hub, "owner", "ack", &b, json!({"through":seq}))
            .await
            .is_err()
    );
    assert!(
        action(&hub, "owner", "receive", &b, json!({"after":seq}))
            .await
            .is_err()
    );
    assert_eq!(receive(&hub, &a).await["messages"], json!([]));
    assert_eq!(receive(&hub, &c).await["messages"], json!([]));
    let first = receive(&hub, &b).await;
    let retry = receive(&hub, &b).await;
    assert_eq!(first["messages"], retry["messages"]);
    assert_eq!(first["messages"][0]["message_id"], receipt["message_id"]);
    assert_eq!(first["messages"][0]["from"], a["agent_id"]);
    assert_eq!(first["messages"][0]["payload"]["text"], "hello");
    assert_eq!(first["next_cursor"], seq);
    let ack = action(&hub, "owner", "ack", &b, json!({"through":seq}))
        .await
        .unwrap();
    assert_eq!(ack["removed"], 1);
    assert_eq!(ack["pending"], 0);
    assert_eq!(receive(&hub, &b).await["messages"], json!([]));
    assert!(
        action(&hub, "owner", "receive", &b, json!({"after":0}))
            .await
            .is_err()
    );
    let repeated = action(&hub, "owner", "ack", &b, json!({"through":0}))
        .await
        .unwrap();
    assert_eq!(repeated["acked_cursor"], seq);
    assert_eq!(repeated["removed"], 0);
    assert_eq!(hub.state.lock().unwrap().queued_bytes, 0);
}

#[tokio::test]
async fn broadcast_uses_current_other_members_and_independent_acknowledgements() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let c = join(&hub, "owner", "c").await;
    let receipt = action(
        &hub,
        "owner",
        "send",
        &a,
        json!({"to":"*","payload":{"commit":"abc"},"topic":"result"}),
    )
    .await
    .unwrap();
    assert_eq!(receipt["recipients"].as_array().unwrap().len(), 2);
    assert!(
        !receipt["recipients"]
            .as_array()
            .unwrap()
            .contains(&a["agent_id"])
    );
    let late = join(&hub, "owner", "late").await;
    assert_eq!(receive(&hub, &late).await["messages"], json!([]));
    assert_eq!(receive(&hub, &a).await["messages"], json!([]));
    let b_batch = receive(&hub, &b).await;
    action(
        &hub,
        "owner",
        "ack",
        &b,
        json!({"through":b_batch["next_cursor"]}),
    )
    .await
    .unwrap();
    let c_batch = receive(&hub, &c).await;
    assert_eq!(b_batch["messages"], c_batch["messages"]);
    assert_eq!(c_batch["messages"][0]["to"], "*");
    assert_eq!(c_batch["messages"][0]["topic"], "result");
}

#[tokio::test]
async fn retry_is_deduplicated_even_after_ack_and_recipient_departure() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let arguments = json!({
        "to":b["agent_id"],"payload":{"answer":42},"idempotency_key":"operation-1"
    });
    let sent = action(&hub, "owner", "send", &a, arguments.clone())
        .await
        .unwrap();
    let retry = action(&hub, "owner", "send", &a, arguments.clone())
        .await
        .unwrap();
    assert_eq!(sent["message_id"], retry["message_id"]);
    assert_eq!(retry["duplicate"], true);
    assert_eq!(
        receive(&hub, &b).await["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut different = arguments.clone();
    different["payload"] = json!({"answer":41});
    assert!(action(&hub, "owner", "send", &a, different).await.is_err());
    action(&hub, "owner", "ack", &b, json!({"through":sent["seq"]}))
        .await
        .unwrap();
    action(&hub, "owner", "leave", &b, json!({})).await.unwrap();
    let retry = action(&hub, "owner", "send", &a, arguments).await.unwrap();
    assert_eq!(retry["duplicate"], true);
    assert_eq!(retry["recipients"], sent["recipients"]);
    assert_eq!(hub.state.lock().unwrap().sequence, 1);
}

#[tokio::test]
async fn idempotency_window_counts_successful_new_sends() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let arguments = json!({"to":"*","payload":null,"idempotency_key":"first"});
    let first = action(&hub, "owner", "send", &a, arguments.clone())
        .await
        .unwrap();
    for _ in 0..MAX_DEDUP {
        action(&hub, "owner", "send", &a, json!({"to":"*","payload":null}))
            .await
            .unwrap();
    }
    let again = action(&hub, "owner", "send", &a, arguments).await.unwrap();
    assert_eq!(again["duplicate"], false);
    assert_ne!(again["message_id"], first["message_id"]);
    assert_eq!(
        hub.state.lock().unwrap().agents[a["agent_id"].as_str().unwrap()]
            .dedup
            .len(),
        MAX_DEDUP
    );
}

#[tokio::test]
async fn full_inbox_rejects_broadcast_without_partial_delivery() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let c = join(&hub, "owner", "c").await;
    for index in 0..MAX_INBOX_MESSAGES {
        send(&hub, &a, &b, json!(index)).await;
    }
    let before = hub.state.lock().unwrap().sequence;
    assert!(
        action(
            &hub,
            "owner",
            "send",
            &a,
            json!({"to":"*","payload":"blocked"})
        )
        .await
        .is_err()
    );
    assert_eq!(hub.state.lock().unwrap().sequence, before);
    assert_eq!(receive(&hub, &c).await["messages"], json!([]));
    let page = action(&hub, "owner", "receive", &b, json!({"limit":1}))
        .await
        .unwrap();
    action(
        &hub,
        "owner",
        "ack",
        &b,
        json!({"through":page["next_cursor"]}),
    )
    .await
    .unwrap();
    action(
        &hub,
        "owner",
        "send",
        &a,
        json!({"to":"*","payload":"accepted"}),
    )
    .await
    .unwrap();
    assert_eq!(
        receive(&hub, &c).await["messages"][0]["payload"],
        "accepted"
    );
    let state = hub.state.lock().unwrap();
    assert_eq!(
        state.agents[b["agent_id"].as_str().unwrap()].inbox.len(),
        MAX_INBOX_MESSAGES
    );
}

#[tokio::test]
async fn payload_inbox_bytes_and_receive_bytes_are_bounded() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    assert!(
        action(
            &hub,
            "owner",
            "send",
            &a,
            json!({"to":b["agent_id"],"payload":"x".repeat(MAX_PAYLOAD-1)})
        )
        .await
        .is_err()
    );
    let payload = json!("x".repeat(MAX_PAYLOAD - 2));
    let mut accepted = 0;
    for _ in 0..MAX_INBOX_MESSAGES {
        if action(
            &hub,
            "owner",
            "send",
            &a,
            json!({"to":b["agent_id"],"payload":payload}),
        )
        .await
        .is_err()
        {
            break;
        }
        accepted += 1;
    }
    assert!(accepted > 8 && accepted < MAX_INBOX_MESSAGES);
    let page = action(&hub, "owner", "receive", &b, json!({"limit":64}))
        .await
        .unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_RECEIVE_BYTES);
    assert_eq!(page["has_more"], true);
    let second = action(
        &hub,
        "owner",
        "receive",
        &b,
        json!({"after":page["next_cursor"],"limit":64}),
    )
    .await
    .unwrap();
    assert!(serde_json::to_vec(&second).unwrap().len() <= MAX_RECEIVE_BYTES);
    assert_ne!(
        page["messages"][0]["message_id"],
        second["messages"][0]["message_id"]
    );
    let state = hub.state.lock().unwrap();
    let inbox = &state.agents[b["agent_id"].as_str().unwrap()];
    assert!(inbox.inbox_bytes <= MAX_INBOX_BYTES);
}

#[tokio::test]
async fn aggregate_capacity_and_membership_limits_are_enforced() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    for _ in 1..MAX_AGENTS {
        join(&hub, "owner", "peer").await;
    }
    assert!(
        hub.dispatch("owner", "join", json!({"name":"overflow"}))
            .await
            .is_err()
    );
    let payload = json!("x".repeat(MAX_PAYLOAD - 2));
    let mut accepted = 0;
    for _ in 0..8 {
        if action(
            &hub,
            "owner",
            "send",
            &a,
            json!({"to":"*","payload":payload}),
        )
        .await
        .is_err()
        {
            break;
        }
        accepted += 1;
    }
    assert!(accepted > 0 && accepted < 8);
    let state = hub.state.lock().unwrap();
    assert_eq!(state.sequence, accepted);
    assert!(state.queued_bytes <= MAX_QUEUED_BYTES);
    for peer in state
        .agents
        .values()
        .filter(|peer| peer.id != a["agent_id"].as_str().unwrap())
    {
        assert_eq!(peer.inbox.len() as u64, accepted);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_receive_releases_waiter_and_delivery_wakes_next_waiter() {
    let hub = Arc::new(Hub::default());
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let waiting = spawn_receive(&hub, &b);
    wait_started(&hub, &b).await;
    let second = action(&hub, "owner", "receive", &b, json!({"wait_ms":100})).await;
    assert!(second.unwrap_err().contains("already has a waiting"));
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    let waiting = spawn_receive(&hub, &b);
    wait_started(&hub, &b).await;
    send(&hub, &a, &b, json!("wake")).await;
    let response = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response["messages"][0]["payload"], "wake");
    assert_eq!(
        hub.state.lock().unwrap().agents[b["agent_id"].as_str().unwrap()]
            .waiters
            .available_permits(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_than_four_waiters_can_receive_one_broadcast() {
    let hub = Arc::new(Hub::default());
    let sender = join(&hub, "owner", "sender").await;
    let mut waiters = Vec::new();
    for _ in 0..6 {
        let peer = join(&hub, "owner", "peer").await;
        let waiting = spawn_receive(&hub, &peer);
        wait_started(&hub, &peer).await;
        waiters.push(waiting);
    }
    let sent = action(
        &hub,
        "owner",
        "send",
        &sender,
        json!({"to":"*","payload":"ready"}),
    )
    .await
    .unwrap();
    for waiting in waiters {
        let response = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response["messages"][0]["message_id"], sent["message_id"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_senders_preserve_a_single_delivery_order() {
    let hub = Arc::new(Hub::default());
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    let mut sends = Vec::new();
    for index in 0..16 {
        let hub = hub.clone();
        let a = a.clone();
        let b = b.clone();
        sends.push(tokio::spawn(async move {
            send(&hub, &a, &b, json!(index)).await
        }));
    }
    for sent in sends {
        sent.await.unwrap();
    }
    let batch = receive(&hub, &b).await;
    let messages = batch["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 16);
    for (index, message) in messages.iter().enumerate() {
        assert_eq!(message["seq"], index + 1);
    }
}

#[tokio::test]
async fn leaving_expiry_and_close_end_waits_and_release_queued_storage() {
    for mode in ["leave", "expiry", "close"] {
        let hub = Arc::new(Hub::default());
        let a = join(&hub, "owner", "a").await;
        let b = join(&hub, "owner", "b").await;
        let waiting = spawn_receive(&hub, &b);
        wait_started(&hub, &b).await;
        // A separate queue verifies cleanup without making B's wait ready.
        send(&hub, &b, &a, json!("pending")).await;
        match mode {
            "leave" => {
                action(&hub, "owner", "leave", &b, json!({})).await.unwrap();
            }
            "expiry" => {
                hub.state
                    .lock()
                    .unwrap()
                    .agents
                    .get_mut(b["agent_id"].as_str().unwrap())
                    .unwrap()
                    .expires = Instant::now() - Duration::from_secs(1);
                let list = hub.dispatch("owner", "list", json!({})).await.unwrap();
                assert_eq!(list["agents"].as_array().unwrap().len(), 1);
            }
            "close" => hub.close(),
            _ => unreachable!(),
        }
        let ended = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(ended.is_err(), "{mode}");
        assert!(
            action(&hub, "owner", "update", &b, json!({}))
                .await
                .is_err()
        );
        if mode == "close" {
            assert!(hub.dispatch("owner", "list", json!({})).await.is_err());
            hub.close();
        } else {
            action(&hub, "owner", "leave", &a, json!({})).await.unwrap();
        }
        assert_eq!(hub.state.lock().unwrap().queued_bytes, 0);
    }
}

#[tokio::test]
async fn short_receive_deadline_returns_empty_and_renews_membership() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let old_expiry = {
        let mut state = hub.state.lock().unwrap();
        let agent = state
            .agents
            .get_mut(a["agent_id"].as_str().unwrap())
            .unwrap();
        agent.expires = Instant::now() + Duration::from_secs(1);
        agent.expires_at_ms = now_ms() + 1000;
        agent.expires_at_ms
    };
    let response = action(&hub, "owner", "receive", &a, json!({"wait_ms":5}))
        .await
        .unwrap();
    assert_eq!(response["messages"], json!([]));
    assert!(response["expires_at_ms"].as_u64().unwrap() > old_expiry);
}

#[tokio::test]
async fn invalid_inputs_do_not_create_members_or_enqueue_messages() {
    let hub = Hub::default();
    for arguments in [
        json!({"name":""}),
        json!({"name":"測".repeat(43)}),
        json!({"name":"a\nb"}),
        json!({"name":"a","capabilities":vec!["x";17]}),
        json!({"name":"a","capabilities":["x".repeat(65)]}),
        json!({"name":"a","unexpected":true}),
    ] {
        assert!(hub.dispatch("owner", "join", arguments).await.is_err());
    }
    assert!(hub.dispatch("", "join", json!({"name":"a"})).await.is_err());
    let a = join(&hub, "owner", "a").await;
    for arguments in [
        json!({"to":"*","payload":null,"topic":""}),
        json!({"to":"*","payload":null,"reply_to":"\u{0000}"}),
        json!({"to":"*","payload":null,"idempotency_key":""}),
        json!({"to":"missing","payload":null}),
    ] {
        assert!(action(&hub, "owner", "send", &a, arguments).await.is_err());
    }
    for arguments in [
        json!({"limit":0}),
        json!({"limit":65}),
        json!({"wait_ms":30_001}),
        json!({"after":1}),
    ] {
        assert!(
            action(&hub, "owner", "receive", &a, arguments)
                .await
                .is_err()
        );
    }
    let state = hub.state.lock().unwrap();
    assert_eq!(state.agents.len(), 1);
    assert_eq!(state.sequence, 0);
    assert_eq!(state.queued_bytes, 0);
}

#[tokio::test]
async fn sequence_exhaustion_never_emits_imprecise_javascript_numbers() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    hub.state.lock().unwrap().sequence = MAX_SEQUENCE;
    assert!(
        action(&hub, "owner", "send", &a, json!({"to":"*","payload":null}))
            .await
            .is_err()
    );
    assert_eq!(hub.state.lock().unwrap().sequence, MAX_SEQUENCE);
}

#[tokio::test]
async fn explicit_null_payload_is_valid_but_missing_payload_is_rejected() {
    let hub = Hub::default();
    let a = join(&hub, "owner", "a").await;
    let b = join(&hub, "owner", "b").await;
    assert!(
        action(&hub, "owner", "send", &a, json!({"to":b["agent_id"]}))
            .await
            .is_err()
    );
    let sent = send(&hub, &a, &b, Value::Null).await;
    let received = receive(&hub, &b).await;
    assert!(received["messages"][0]["payload"].is_null());
    assert_eq!(received["messages"][0]["message_id"], sent["message_id"]);
}

#[tokio::test]
async fn logical_ids_and_thread_alias_preserve_private_mailboxes() {
    let hub = Hub::for_session("session_one");
    let one = hub
        .dispatch("owner", "join", json!({"name":"one"}))
        .await
        .unwrap();
    let two = hub
        .dispatch("owner", "join", json!({"name":"two"}))
        .await
        .unwrap();
    assert_eq!(one["session_id"], "session_one");
    assert_eq!(one["thread_id"], one["agent_id"]);
    let list = hub.dispatch("owner", "list", json!({})).await.unwrap();
    assert_eq!(list["session_id"], "session_one");
    assert!(
        list["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["session_id"] == "session_one"
                && agent["thread_id"] == agent["agent_id"]
                && agent.get("token").is_none())
    );
    let sent = hub
        .dispatch(
            "owner",
            "send",
            json!({
                "thread_id":one["thread_id"],"token":one["token"],"to":two["thread_id"],
                "payload":{"text":"private handoff"},"idempotency_key":"once"
            }),
        )
        .await
        .unwrap();
    assert_eq!(sent["delivery"], "queued");
    assert!(
        hub.dispatch(
            "owner",
            "receive",
            json!({
                "thread_id":two["thread_id"],"token":one["token"]
            })
        )
        .await
        .is_err()
    );
    let own = hub
        .dispatch(
            "owner",
            "receive",
            json!({
                "thread_id":one["thread_id"],"token":one["token"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(own["messages"], json!([]));
    let received = hub
        .dispatch(
            "owner",
            "receive",
            json!({
                "thread_id":two["thread_id"],"token":two["token"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(received["messages"][0]["from_thread_id"], one["thread_id"]);
    assert_eq!(received["thread_id"], two["thread_id"]);
    assert_eq!(received["session_id"], "session_one");
    assert!(
        hub.dispatch(
            "different_owner",
            "receive",
            json!({
                "thread_id":two["thread_id"],"token":two["token"]
            })
        )
        .await
        .is_err()
    );
}
