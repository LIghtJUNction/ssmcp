//! Real in-memory broker identities, memberships and queues. No external model,
//! installed client, or other person's inbox participates in these fixtures.
use super::*;
mod durable_tests;
use crate::{
    identity::Identity,
    wire::{SteeringCommand, SteeringState},
};

struct Fixture {
    broker: Arc<Broker>,
    session: Arc<Session>,
    token: String,
    owner: String,
    member: Value,
    peer: Value,
}

impl Fixture {
    async fn new() -> Self {
        let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
        let session = broker
            .test_register_identity(
                "fixture-owner",
                Identity {
                    provider: "github".into(),
                    subject: "42".into(),
                    login: "alice".into(),
                },
            )
            .await;
        let tokens = broker.oauth.test_identity_tokens("mcp", "42");
        let token = tokens["access_token"].as_str().unwrap().to_owned();
        let auth = broker.authenticate("mcp", &token).await.unwrap();
        let owner = crate::bridge::owner("mcp", &auth).unwrap();
        let member = session
            .collaboration
            .dispatch(&owner, "join", json!({"name":"observer"}))
            .await
            .unwrap();
        let peer = session
            .collaboration
            .dispatch(&owner, "join", json!({"name":"worker"}))
            .await
            .unwrap();
        Self {
            broker,
            session,
            token,
            owner,
            member,
            peer,
        }
    }

    fn args(&self, member: &Value, mut value: Value) -> Arguments {
        value["client_id"] = json!(self.session.client_id());
        value["agent"] = json!({"id":member["agent_id"],"token":member["token"]});
        Arguments::parse(value).unwrap()
    }

    async fn public(&self, member: &Value, value: Value) -> Value {
        execute(
            self.broker.clone(),
            "mcp".into(),
            self.token.clone(),
            self.args(member, value),
        )
        .await
        .unwrap()["structuredContent"]
            .clone()
    }

    async fn events(&self, member: &Value, mut value: Value) -> Result<Value, String> {
        value["client_id"] = json!(self.session.client_id());
        value["agent_id"] = member["agent_id"].clone();
        value["token"] = member["token"].clone();
        super::super::events_tool::dispatch(
            self.broker.clone(),
            "mcp".into(),
            self.token.clone(),
            value,
        )
        .await
    }

    async fn raw(&self, run: code_mode::AuditRun, member: &Value, value: Value) -> Value {
        code_mode::AUDIT_RUN
            .scope(
                run,
                wait(
                    self.broker.clone(),
                    "mcp".into(),
                    self.token.clone(),
                    self.args(member, value),
                ),
            )
            .await
            .unwrap()
    }

    fn report(&self, run: code_mode::AuditRun) -> CodeRunReport {
        CodeRunReport {
            broker: self.broker.clone(),
            user: "mcp".into(),
            access_token: self.token.clone(),
            run,
            delivered: true,
        }
    }

    fn record(&self, id: &str) -> Arc<Record> {
        self.broker
            .awaiting
            .records
            .lock()
            .unwrap()
            .get(id)
            .unwrap()
            .clone()
    }

    fn publish(&self, id: &str) {
        self.session
            .steering
            .command(
                self.session.client_id(),
                self.session.collaboration.presence().unwrap(),
                SteeringCommand::Publish {
                    command_id: format!("publish_{id}"),
                    message_id: id.into(),
                    expected_revision: 0,
                    text: format!("local requirement {id}"),
                    target: self.member["agent_id"].as_str().unwrap().into(),
                },
            )
            .unwrap();
    }

    fn state(&self, id: &str) -> SteeringState {
        self.session
            .steering
            .command(
                self.session.client_id(),
                vec![],
                SteeringCommand::Snapshot {},
            )
            .unwrap()
            .messages
            .into_iter()
            .find(|message| message.id == id)
            .unwrap()
            .state
    }

    async fn update(&self, member: &Value, status: &str) {
        let current = self
            .session
            .collaboration
            .lifecycle_snapshot(
                &self.owner,
                member["agent_id"].as_str().unwrap(),
                member["token"].as_str().unwrap(),
            )
            .unwrap();
        let revision = current["lifecycle"]["revision"].as_u64().unwrap();
        self.session
            .collaboration
            .dispatch(
                &self.owner,
                "update",
                json!({"agent_id":member["agent_id"],
            "token":member["token"],"status":status,"expected_revision":revision}),
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "real loopback MCP transport with three 20-second observation windows"]
async fn windowed_await_over_http_retains_one_id_past_sixty_seconds() {
    let fixture = Fixture::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let routes = crate::http::router(fixture.broker.clone());
    let server = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
    let client = crate::http_client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(25))
        .build()
        .unwrap();
    let call = |mut arguments: Value| {
        arguments["client_id"] = json!(fixture.session.client_id());
        arguments["agent"] =
            json!({"id":fixture.member["agent_id"],"token":fixture.member["token"]});
        let request = client
            .post(&endpoint)
            .bearer_auth(&fixture.token)
            .header("MCP-Protocol-Version", "2025-06-18")
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
                          "params":{"name":"await","arguments":arguments}}));
        async move {
            let body: Value = request
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert!(body.get("error").is_none(), "{body}");
            body["result"]["structuredContent"].clone()
        }
    };
    let started = std::time::Instant::now();
    let first =
        call(json!({"window_ms":20_000,"targets":[{"type":"user_input","notify":false}]})).await;
    assert_eq!(first["status"], "pending");
    let id = first["wait_id"].as_str().unwrap();
    for _ in 0..2 {
        let next = call(json!({"wait_id":id,"window_ms":20_000})).await;
        assert_eq!(next["wait_id"], id);
        assert_eq!(next["status"], "pending");
    }
    assert!(started.elapsed() >= Duration::from_secs(60));
    fixture.publish("between_http_windows");
    let ready = call(json!({"wait_id":id,"window_ms":20_000})).await;
    assert_eq!(ready["wait_id"], id);
    assert_eq!(ready["status"], "ready");
    assert_eq!(
        ready["user_messages"][0]["message_id"],
        "between_http_windows"
    );
    assert_eq!(ready["user_messages"].as_array().unwrap().len(), 1);
    assert_eq!(ready["cursor"], 1);
    // Simulate a lost response by resuming from the last application cursor.
    fixture.publish("second_http_input");
    let replay = call(json!({"wait_id":id,"after":0})).await;
    assert_eq!(replay["cursor"], 1);
    assert_eq!(replay["user_messages"], ready["user_messages"]);
    assert_eq!(replay["user_wait"][0]["status"], "received");
    assert_eq!(fixture.state("second_http_input"), SteeringState::Queued);
    for _ in 0..2 {
        let ack = call(json!({"wait_id":id,"after":1})).await;
        assert_eq!(ack["acknowledged"], true);
        assert!(ack.get("user_messages").is_none());
    }
    server.abort();
}

#[tokio::test]
async fn windowed_await_replays_one_completed_snapshot_until_acknowledged() {
    let fixture = Fixture::new().await;
    let first = fixture
        .public(
            &fixture.member,
            json!({"window_ms":1000,"targets":[
                {"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]}
            ]}),
        )
        .await;
    assert_eq!(first["status"], "pending");
    assert_eq!(first["cursor"], 0);
    let id = first["wait_id"].as_str().unwrap();
    let error = execute(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.args(&fixture.member, json!({"wait_id":id,"after":1})),
    )
    .await;
    assert!(error.unwrap_err().contains("ahead"));
    fixture.update(&fixture.peer, "working").await;
    let ready = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["cursor"], 1);
    assert_eq!(ready["wait_id"], id);
    fixture.update(&fixture.peer, "idle").await;
    let other = fixture
        .public(&fixture.peer, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(other["status"], "unauthorized");
    let retry = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(retry["results"], ready["results"]);
    for _ in 0..2 {
        let ack = fixture
            .public(&fixture.member, json!({"wait_id":id,"after":1}))
            .await;
        assert_eq!(ack["acknowledged"], true);
        assert_eq!(ack["results"], json!([]));
    }
    let error = execute(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        fixture.args(&fixture.member, json!({"wait_id":id,"after":0})),
    )
    .await;
    assert!(error.unwrap_err().contains("expired"));
}

#[tokio::test]
async fn windowed_all_keeps_ready_results_and_messages_published_between_windows() {
    let fixture = Fixture::new().await;
    fixture.update(&fixture.peer, "working").await;
    let first = fixture
        .public(
            &fixture.member,
            json!({"mode":"all","window_ms":1000,"targets":[
                {"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]},
                {"type":"agent_messages"},
            ]}),
        )
        .await;
    assert_eq!(first["status"], "pending");
    assert_eq!(first["cursor"], 0);
    assert_eq!(first["results"][0]["status"], "ready");
    assert_eq!(first["results"][1]["status"], "pending");
    let id = first["wait_id"].as_str().unwrap();
    assert_eq!(
        *fixture.record(id).observation_state.lock().unwrap(),
        ObservationState::Resumed
    );
    fixture.update(&fixture.peer, "idle").await;
    fixture
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "send",
            json!({"agent_id":fixture.peer["agent_id"],"token":fixture.peer["token"],
                "to":fixture.member["agent_id"],"payload":{"task":"gap_result"}}),
        )
        .await
        .unwrap();
    let ready = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.public(
            &fixture.member,
            json!({"wait_id":id,"window_ms":MAX_OBSERVATION_WAIT_MS,"after":0}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["wait_id"], id);
    assert_eq!(ready["cursor"], 1);
    assert_eq!(ready["results"][0], first["results"][0]);
    assert_eq!(
        ready["results"][1]["data"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        *fixture.record(id).observation_state.lock().unwrap(),
        ObservationState::Completed
    );
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(replay["results"], ready["results"]);
    let acknowledged = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":1}))
        .await;
    assert_eq!(acknowledged["acknowledged"], true);
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
    assert_eq!(inbox["acked_cursor"], 0);
    assert_eq!(inbox["messages"], ready["results"][1]["data"]["messages"]);
}

#[tokio::test]
async fn event_targets_keep_gap_events_and_await_ack_does_not_ack_subscriptions() {
    let fixture = Fixture::new().await;
    let mut subscriptions = Vec::new();
    for name in ["first", "second"] {
        let subscription = fixture
            .events(
                &fixture.member,
                json!({"action":"subscribe","request_id":format!("subscribe_{name}"),
                    "filter":{"kinds":[format!("custom.{name}")]}}),
            )
            .await
            .unwrap();
        subscriptions.push(subscription["subscription_id"].as_str().unwrap().to_owned());
    }
    fixture
        .events(
            &fixture.member,
            json!({"action":"publish","request_id":"publish_first",
                "name":"custom.first","data":{"phase":"before_window"}}),
        )
        .await
        .unwrap();
    let first = fixture
        .public(
            &fixture.member,
            json!({"mode":"all","window_ms":1000,"targets":[
                {"type":"event","subscription_id":subscriptions[0]},
                {"type":"event","subscription_id":subscriptions[1]},
            ]}),
        )
        .await;
    assert_eq!(first["status"], "pending");
    assert_eq!(first["results"][0]["status"], "ready");
    assert_eq!(first["results"][1]["status"], "pending");
    let id = first["wait_id"].as_str().unwrap();
    fixture
        .events(
            &fixture.member,
            json!({"action":"publish","request_id":"publish_second",
                "name":"custom.second","data":{"phase":"between_windows"}}),
        )
        .await
        .unwrap();
    let ready = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.public(
            &fixture.member,
            json!({"wait_id":id,"window_ms":MAX_OBSERVATION_WAIT_MS,"after":0}),
        ),
    )
    .await
    .unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["wait_id"], id);
    assert_eq!(ready["cursor"], 1);
    assert_eq!(ready["results"][0], first["results"][0]);
    for index in 0..2 {
        assert_eq!(
            ready["results"][index]["data"]["events"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(replay["results"], ready["results"]);
    let acknowledged = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":1}))
        .await;
    assert_eq!(acknowledged["acknowledged"], true);
    for (index, subscription_id) in subscriptions.iter().enumerate() {
        let unacknowledged = fixture
            .events(
                &fixture.member,
                json!({"action":"read","subscription_id":subscription_id}),
            )
            .await
            .unwrap();
        assert_eq!(
            unacknowledged["events"],
            ready["results"][index]["data"]["events"]
        );
    }
}

#[tokio::test]
async fn event_source_binding_revocation_withholds_final_delivery_and_cached_replays() {
    let fixture = Fixture::new().await;
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
    let source_member = source
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"pool-observer"}))
        .await
        .unwrap();
    let subscription = fixture
        .events(
            &fixture.member,
            json!({"action":"subscribe","request_id":"subscribe_pool",
                "filter":{"client_ids":[source.client_id()],"kinds":["custom.done"]},
                "source_agents":[{"client_id":source.client_id(),
                    "agent_id":source_member["agent_id"],"token":source_member["token"]}]}),
        )
        .await
        .unwrap();
    let subscription_id = subscription["subscription_id"].as_str().unwrap();
    super::super::events_tool::dispatch(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        json!({"action":"publish","client_id":source.client_id(),
            "agent_id":source_member["agent_id"],"token":source_member["token"],
            "request_id":"publish_pool","name":"custom.done","data":{"task":"pool_task"}}),
    )
    .await
    .unwrap();
    let run = code_mode::AuditRun::new("await pool events", None).unwrap();
    let ready = fixture
        .raw(
            run.clone(),
            &fixture.member,
            json!({"window_ms":1000,"targets":[{"type":"event","subscription_id":subscription_id}]}),
        )
        .await;
    assert_eq!(ready["status"], "ready");
    let id = ready["wait_id"].as_str().unwrap();
    source
        .collaboration
        .dispatch(
            &fixture.owner,
            "leave",
            json!({"agent_id":source_member["agent_id"],"token":source_member["token"]}),
        )
        .await
        .unwrap();
    let response = finish_passive_response(
        &fixture.report(run),
        fixture.session.client_id(),
        ready.clone(),
    )
    .await;
    assert_eq!(response["structuredContent"]["status"], "unauthorized");
    assert!(
        response["structuredContent"]["results"][0]
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
    assert!(!fixture.record(id).acknowledged.load(Ordering::Acquire));
}

#[tokio::test]
async fn event_target_does_not_wake_on_its_own_wait_lifecycle() {
    let fixture = Fixture::new().await;
    let subscription = fixture
        .events(
            &fixture.member,
            json!({"action":"subscribe","request_id":"subscribe_waits", "filter":{
                "kinds":["wait.suspended","wait.resumed"],"include_own_waits":true}}),
        )
        .await
        .unwrap();
    let subscription_id = subscription["subscription_id"].as_str().unwrap();
    let started = std::time::Instant::now();
    let first = fixture
        .public(
            &fixture.member,
            json!({"window_ms":1000,"targets":[{"type":"event","subscription_id":subscription_id}]}),
        )
        .await;
    assert_eq!(first["status"], "pending");
    let id = first["wait_id"].as_str().unwrap();
    let second = fixture
        .public(&fixture.member, json!({"wait_id":id,"window_ms":1000}))
        .await;
    assert_eq!(second["status"], "pending");
    assert_eq!(second["wait_id"], id);
    assert!(started.elapsed() >= Duration::from_millis(1800));
    let lifecycle = fixture
        .events(
            &fixture.member,
            json!({"action":"read","subscription_id":subscription_id}),
        )
        .await
        .unwrap();
    let events = lifecycle["events"].as_array().unwrap();
    assert_eq!(events.len(), 4);
    assert!(
        events
            .iter()
            .all(|event| event["data"]["origin_wait_id"] == id)
    );
}

#[tokio::test]
#[ignore = "real 55-second observation window"]
async fn fifty_five_second_window_returns_pending_and_resumes_the_original_wait() {
    let fixture = Fixture::new().await;
    let started = std::time::Instant::now();
    let first = tokio::time::timeout(
        Duration::from_secs(58),
        fixture.public(
            &fixture.member,
            json!({"window_ms":55_000,"targets":[
                {"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]},
            ]}),
        ),
    )
    .await
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(54));
    assert!(started.elapsed() < Duration::from_secs(56));
    assert_eq!(first["status"], "pending");
    assert_eq!(first["cursor"], 0);
    let id = first["wait_id"].as_str().unwrap();
    fixture.update(&fixture.peer, "working").await;
    let ready = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.public(&fixture.member, json!({"wait_id":id,"window_ms":55_000})),
    )
    .await
    .unwrap();
    assert_eq!(ready["wait_id"], id);
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["cursor"], 1);
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(replay["results"], ready["results"]);
}

#[tokio::test]
async fn windowed_user_input_delivers_input_published_between_requests_once() {
    let fixture = Fixture::new().await;
    let first = fixture
        .public(
            &fixture.member,
            json!({"window_ms":1000,
        "targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    assert_eq!(first["status"], "pending");
    let id = first["wait_id"].as_str().unwrap();
    fixture.publish("between_windows");
    let ready = fixture
        .public(&fixture.member, json!({"wait_id":id,"window_ms":1000}))
        .await;
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["user_messages"][0]["message_id"], "between_windows");
    assert_eq!(ready["cursor"], 1);
    let denied = fixture
        .public(&fixture.peer, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(denied["status"], "unauthorized");
    assert!(denied.get("user_messages").is_none());
    let retry = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":0}))
        .await;
    assert_eq!(retry["user_messages"], ready["user_messages"]);
    let ack = fixture
        .public(&fixture.member, json!({"wait_id":id,"after":1}))
        .await;
    assert_eq!(ack["acknowledged"], true);
    assert!(ack.get("user_messages").is_none());
}

#[tokio::test]
async fn user_delivery_replay_overflow_is_explicit_and_keeps_no_partial_copy() {
    let fixture = Fixture::new().await;
    let first = fixture
        .public(
            &fixture.member,
            json!({"window_ms":1000,
        "targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    let record = fixture.record(first["wait_id"].as_str().unwrap());
    let receipt = json!({"status":"received","messages":[{"message_id":"big","revision":1}]});
    let mut response = json!({"structuredContent":{"user_messages":[{
        "client_id":fixture.session.client_id(),"agent_id":fixture.member["agent_id"],
        "message_id":"big","revision":1,"text":"x".repeat(MAX_SNAPSHOT_BYTES),
    }]}});
    retain_user_delivery(&record, receipt, &mut response);
    record.terminal.store(true, Ordering::Release);
    assert_eq!(
        response["structuredContent"]["replay_error"],
        "await_snapshot_overflow"
    );
    assert!(record.snapshot.lock().unwrap().is_none());
    assert!(
        read_window_snapshot(&record, Some(0))
            .unwrap_err()
            .contains("await_snapshot_overflow")
    );
}

#[tokio::test]
async fn timeout_resumes_same_server_record_and_another_member_cannot_cancel_it() {
    let fixture = Fixture::new().await;
    let first = fixture
        .public(
            &fixture.member,
            json!({"targets":[{"type":"agent_status",
        "agent_id":fixture.peer["agent_id"],"statuses":["working"]}],"timeout_ms":1000}),
        )
        .await;
    assert_eq!(first["status"], "timed_out");
    let id = first["wait_id"].as_str().unwrap();
    let denied = fixture
        .public(&fixture.peer, json!({"op":"cancel","wait_id":id}))
        .await;
    assert_eq!(denied["status"], "unauthorized");
    assert!(!*fixture.record(id).cancelled.borrow());
    fixture.update(&fixture.peer, "working").await;
    let resumed = fixture
        .public(&fixture.member, json!({"wait_id":id,"timeout_ms":1000}))
        .await;
    assert_eq!(resumed["status"], "ready");
    assert_eq!(resumed["wait_id"], id);
    assert_eq!(resumed["results"][0]["data"]["agent"]["status"], "working");
    assert!(resumed["completion_permission"].is_null());
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":id,"timeout_ms":1000}))
        .await;
    assert_eq!(
        replay["results"][0]["error"]["code"],
        "await_already_resolved"
    );
}

#[tokio::test]
async fn two_members_observe_independently_even_when_user_wait_slots_are_full() {
    let fixture = Fixture::new().await;
    let _permits: Vec<_> = (0..4)
        .map(|_| fixture.session.steering.wait_permit().unwrap())
        .collect();
    let one = fixture.public(&fixture.member, json!({"targets":[{"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]}],"timeout_ms":2000}));
    let two = fixture.public(&fixture.peer, json!({"targets":[{"type":"agent_status","agent_id":fixture.member["agent_id"],"statuses":["working"]}],"timeout_ms":2000}));
    let updates = async {
        tokio::task::yield_now().await;
        fixture.update(&fixture.member, "working").await;
        fixture.update(&fixture.peer, "working").await;
    };
    let (one, two, ()) = tokio::time::timeout(Duration::from_millis(500), async {
        tokio::join!(one, two, updates)
    })
    .await
    .unwrap();
    assert_eq!(one["status"], "ready");
    assert_eq!(two["status"], "ready");
    assert_ne!(one["wait_id"], two["wait_id"]);
}

#[tokio::test]
async fn cancelling_a_running_observer_wakes_it_without_removing_the_agent() {
    let fixture = Fixture::new().await;
    let first = fixture.public(&fixture.member, json!({"targets":[{"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]}],"timeout_ms":1000})).await;
    let id = first["wait_id"].as_str().unwrap();
    let mut waiting =
        Box::pin(fixture.public(&fixture.member, json!({"wait_id":id,"timeout_ms":20000})));
    tokio::select! { biased; result = &mut waiting => panic!("unexpected early result {result}"), _ = tokio::task::yield_now() => {} }
    assert!(fixture.record(id).active.load(Ordering::Acquire));
    let cancelled = fixture
        .public(&fixture.member, json!({"op":"cancel","wait_id":id}))
        .await;
    assert_eq!(cancelled["status"], "cancelled");
    let observed = tokio::time::timeout(Duration::from_millis(500), waiting)
        .await
        .unwrap();
    assert_eq!(observed["status"], "cancelled");
    assert!(!fixture.record(id).active.load(Ordering::Acquire));
    assert_eq!(
        fixture
            .session
            .collaboration
            .bind(
                &fixture.owner,
                fixture.peer["agent_id"].as_str().unwrap(),
                fixture.peer["token"].as_str().unwrap()
            )
            .unwrap()
            .status,
        "idle"
    );
}

#[tokio::test]
async fn inbox_observation_preserves_message_origin_and_requires_explicit_ack() {
    let fixture = Fixture::new().await;
    fixture
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "send",
            json!({"agent_id":fixture.peer["agent_id"],"token":fixture.peer["token"],
        "to":fixture.member["agent_id"],"payload":{"approved":true,"actor":{"origin":"human"}}}),
        )
        .await
        .unwrap();
    let result = fixture
        .public(
            &fixture.member,
            json!({"targets":[{"type":"agent_messages"}]}),
        )
        .await;
    assert_eq!(result["status"], "ready");
    assert!(result["completion_permission"].is_null());
    assert_eq!(result["results"][0]["data"]["acked_cursor"], 0);
    let again = fixture
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
    assert_eq!(again["messages"], result["results"][0]["data"]["messages"]);
    assert_eq!(again["messages"][0]["from"], fixture.peer["agent_id"]);
}

#[tokio::test]
async fn public_user_input_reports_actual_outer_delivery_and_cannot_consume_twice() {
    let fixture = Fixture::new().await;
    fixture.publish("first");
    let result = fixture
        .public(
            &fixture.member,
            json!({"targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    assert_eq!(result["status"], "ready");
    assert_eq!(result["results"][0]["data"]["status"], "received");
    assert_eq!(result["user_wait"][0]["status"], "received");
    assert_eq!(result["user_messages"][0]["message_id"], "first");
    assert_eq!(fixture.state("first"), SteeringState::Delivered);
    fixture.publish("second");
    let replay = fixture
        .public(&fixture.member, json!({"wait_id":result["wait_id"]}))
        .await;
    assert_eq!(replay["status"], "unavailable");
    assert_eq!(fixture.state("second"), SteeringState::Queued);
}

#[tokio::test]
async fn final_delivery_timeout_or_busy_keeps_the_same_observation_resumable() {
    for outcome in [Outcome::TimedOut, Outcome::Busy] {
        let fixture = Fixture::new().await;
        fixture.publish("reserved");
        let run = code_mode::AuditRun::new("waitFor user input", None).unwrap();
        let ready = fixture
            .raw(
                run.clone(),
                &fixture.member,
                json!({"targets":[{"type":"user_input","notify":false}]}),
            )
            .await;
        assert_eq!(ready["status"], "ready");
        let id = ready["wait_id"].as_str().unwrap();
        assert!(fixture.record(id).terminal.load(Ordering::Acquire));
        assert_eq!(
            *fixture.record(id).observation_state.lock().unwrap(),
            ObservationState::Resumed
        );
        let report = fixture.report(run.clone());
        let eligible = eligible_user_response(&report).await.unwrap();
        // This is the genuine post-observation/pre-commit window. A late local
        // wait result can lower readiness without having delivered any input.
        run.finish_wait_state(fixture.session.client_id(), outcome, "late_wait_result");
        commit_user_response(&report, tool_result(Ok(ready.clone())), eligible);
        assert!(!fixture.record(id).terminal.load(Ordering::Acquire));
        assert_eq!(
            *fixture.record(id).observation_state.lock().unwrap(),
            ObservationState::Resumed
        );
        assert_eq!(fixture.state("reserved"), SteeringState::Queued);
        let resumed = fixture.public(&fixture.member, json!({"wait_id":id})).await;
        assert_eq!(resumed["status"], "ready");
        assert_eq!(resumed["wait_id"], id);
        assert_eq!(resumed["user_wait"][0]["status"], "received");
        assert_eq!(
            *fixture.record(id).observation_state.lock().unwrap(),
            ObservationState::Completed
        );
    }
}

#[tokio::test]
async fn observer_cancel_after_ready_wins_before_user_input_commit() {
    let fixture = Fixture::new().await;
    fixture.publish("reserved");
    let run = code_mode::AuditRun::new("waitFor user input", None).unwrap();
    let ready = fixture
        .raw(
            run.clone(),
            &fixture.member,
            json!({"targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    let report = fixture.report(run.clone());
    let eligible = eligible_user_response(&report).await.unwrap();
    let cancelled = fixture
        .public(
            &fixture.member,
            json!({"op":"cancel","wait_id":ready["wait_id"]}),
        )
        .await;
    assert_eq!(cancelled["status"], "cancelled");
    let response = commit_user_response(&report, tool_result(Ok(ready)), eligible);
    assert!(response["structuredContent"].get("user_messages").is_none());
    assert_eq!(
        run.user_wait_receipt(fixture.session.client_id())["status"],
        "cancelled"
    );
    assert_eq!(fixture.state("reserved"), SteeringState::Queued);
}

#[tokio::test]
async fn same_run_rejected_user_wait_does_not_release_ready_reservation_or_own_its_cancel() {
    for cancel_original in [false, true] {
        let fixture = Fixture::new().await;
        fixture.publish("reserved-original");
        let run = code_mode::AuditRun::new("two waitFor calls in one run", None).unwrap();
        let args = json!({"targets":[{"type":"user_input","notify":false}]});
        let first = fixture
            .raw(run.clone(), &fixture.member, args.clone())
            .await;
        assert_eq!(first["status"], "ready");
        let original_id = first["wait_id"].as_str().unwrap();
        let receipt = run.user_wait_receipt(fixture.session.client_id());
        assert_eq!(receipt["status"], "ready");

        let rejected = fixture.raw(run.clone(), &fixture.member, args).await;
        assert_eq!(rejected["status"], "unavailable");
        assert!(
            rejected["results"][0]["error"]["message"]
                .as_str()
                .unwrap()
                .contains("one user wait per client")
        );
        assert_ne!(rejected["wait_id"], first["wait_id"]);
        assert_eq!(run.user_wait_receipt(fixture.session.client_id()), receipt);
        assert!(fixture.record(original_id).awaiting_delivery());

        // Cancel the rejected observer in the SAME run. It never acquired the
        // first observer's input reservation or its final-delivery record.
        let cancelled = fixture
            .raw(
                run.clone(),
                &fixture.member,
                json!({"op":"cancel","wait_id":rejected["wait_id"]}),
            )
            .await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(run.user_wait_receipt(fixture.session.client_id()), receipt);
        assert!(!*fixture.record(original_id).cancelled.borrow());

        let report = fixture.report(run.clone());
        let eligible = eligible_user_response(&report).await.unwrap();
        if cancel_original {
            let cancelled = fixture
                .raw(
                    run.clone(),
                    &fixture.member,
                    json!({"op":"cancel","wait_id":original_id}),
                )
                .await;
            assert_eq!(cancelled["status"], "cancelled");
        }
        let response = commit_user_response(
            &report,
            tool_result(Ok(json!({"first":first,"second":rejected}))),
            eligible,
        );
        if cancel_original {
            assert!(response["structuredContent"].get("user_messages").is_none());
            assert_eq!(fixture.state("reserved-original"), SteeringState::Queued);
            assert_eq!(
                run.user_wait_receipt(fixture.session.client_id())["status"],
                "cancelled"
            );
        } else {
            assert_eq!(
                response["structuredContent"]["user_messages"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                response["structuredContent"]["user_messages"][0]["message_id"],
                "reserved-original"
            );
            assert_eq!(fixture.state("reserved-original"), SteeringState::Delivered);
            assert_eq!(
                run.user_wait_receipt(fixture.session.client_id())["status"],
                "received"
            );
            let repeated = commit_user_response(
                &report,
                tool_result(Ok(json!({}))),
                eligible_user_response(&report).await.unwrap(),
            );
            assert!(repeated["structuredContent"].get("user_messages").is_none());
        }
    }
}

#[tokio::test]
async fn concurrent_same_run_user_waits_commit_only_the_admitted_observers_input() {
    let fixture = Fixture::new().await;
    fixture.publish("concurrent-original");
    let run = code_mode::AuditRun::new("Promise.all waitFor input", None).unwrap();
    let start = tokio::sync::Barrier::new(2);
    let observe = || async {
        start.wait().await;
        fixture
            .raw(
                run.clone(),
                &fixture.member,
                json!({"targets":[{"type":"user_input","notify":false}]}),
            )
            .await
    };
    let (one, two) = tokio::join!(observe(), observe());
    let (ready, rejected) = if one["status"] == "ready" {
        (one, two)
    } else {
        (two, one)
    };
    assert_eq!(ready["status"], "ready");
    assert_eq!(rejected["status"], "unavailable");
    assert!(
        rejected["results"][0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("one user wait per client")
    );
    assert_eq!(
        run.user_wait_receipt(fixture.session.client_id())["status"],
        "ready"
    );
    let original = fixture.record(ready["wait_id"].as_str().unwrap());
    assert!(original.awaiting_delivery());
    assert!(
        !fixture
            .record(rejected["wait_id"].as_str().unwrap())
            .awaiting_delivery()
    );
    let report = fixture.report(run.clone());
    let eligible = eligible_user_response(&report).await.unwrap();
    let response =
        commit_user_response(&report, tool_result(Ok(json!([ready, rejected]))), eligible);
    assert_eq!(
        response["structuredContent"]["user_messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        response["structuredContent"]["user_messages"][0]["message_id"],
        "concurrent-original"
    );
    assert_eq!(
        fixture.state("concurrent-original"),
        SteeringState::Delivered
    );
    assert!(!original.awaiting_delivery());
    assert_eq!(
        run.user_wait_receipt(fixture.session.client_id())["status"],
        "received"
    );
}

#[tokio::test]
async fn unknown_objects_and_revoked_membership_have_distinct_typed_outcomes() {
    let fixture = Fixture::new().await;
    let unknown = fixture
        .public(
            &fixture.member,
            json!({"targets":[{"type":"agent_status","agent_id":"missing","statuses":["working"]}]}),
        )
        .await;
    assert_eq!(unknown["status"], "unknown_target");
    assert!(unknown["results"][0]["error"]["message"].is_string());
    fixture
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "leave",
            json!({"agent_id":fixture.member["agent_id"],"token":fixture.member["token"]}),
        )
        .await
        .unwrap();
    let denied = fixture
        .public(
            &fixture.member,
            json!({"targets":[{"type":"job","id":"missing"}]}),
        )
        .await;
    assert_eq!(denied["status"], "unauthorized");
    assert!(denied["results"][0].get("data").is_none());
}

#[tokio::test]
async fn pending_final_delivery_survives_retention_and_capacity_until_it_settles() {
    let fixture = Fixture::new().await;
    fixture.publish("protected");
    let run = code_mode::AuditRun::new("waitFor protected input", None).unwrap();
    let ready = fixture
        .raw(
            run.clone(),
            &fixture.member,
            json!({"targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    let id = ready["wait_id"].as_str().unwrap();
    let protected = fixture.record(id);
    *protected.touched.lock().unwrap() = Instant::now() - RETAIN_FOR - Duration::from_secs(1);
    for index in 0..MAX_ACTOR_RECORDS + 2 {
        let args = fixture.args(
            &fixture.member,
            json!({"targets":[{"type":"job","id":format!("finished_{index}")}]}),
        );
        let record = fixture
            .broker
            .awaiting
            .record(protected.actor.clone(), &args)
            .unwrap();
        record.terminal.store(true, Ordering::Release);
    }
    assert!(Arc::ptr_eq(&fixture.record(id), &protected));
    let report = fixture.report(run.clone());
    let eligible = eligible_user_response(&report).await.unwrap();
    run.finish_wait_state(
        fixture.session.client_id(),
        Outcome::TimedOut,
        "late_timeout",
    );
    commit_user_response(&report, tool_result(Ok(ready.clone())), eligible);
    assert!(protected.delivery_run.lock().unwrap().is_none());
    assert!(!protected.terminal.load(Ordering::Acquire));
    // Settling refreshes the retention point before the original ID is resumed.
    let resumed = fixture.public(&fixture.member, json!({"wait_id":id})).await;
    assert_eq!(resumed["status"], "ready");
    assert_eq!(resumed["wait_id"], id);
}

#[tokio::test]
async fn dropping_the_outer_response_releases_protected_delivery_without_consuming_input() {
    let fixture = Fixture::new().await;
    fixture.publish("abandoned");
    let run = code_mode::AuditRun::new("waitFor abandoned input", None).unwrap();
    let ready = fixture
        .raw(
            run.clone(),
            &fixture.member,
            json!({"targets":[{"type":"user_input","notify":false}]}),
        )
        .await;
    let record = fixture.record(ready["wait_id"].as_str().unwrap());
    assert!(record.awaiting_delivery());
    drop(fixture.report(run));
    assert!(record.delivery_run.lock().unwrap().is_none());
    assert!(*record.cancelled.borrow());
    assert!(record.terminal.load(Ordering::Acquire));
    assert_eq!(fixture.state("abandoned"), SteeringState::Queued);
}

#[tokio::test]
async fn a_real_membership_lease_expiring_during_observation_revokes_the_result() {
    let fixture = Fixture::new().await;
    let args = json!({"targets":[{"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["working"]}],"timeout_ms":5000});
    let mut waiting = Box::pin(fixture.public(&fixture.member, args));
    tokio::select! { biased; result = &mut waiting => panic!("unexpected early result {result}"), _ = tokio::task::yield_now() => {} }
    fixture
        .session
        .collaboration
        .expire_lease_for_test(&fixture.owner, fixture.member["agent_id"].as_str().unwrap());
    let revoked = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap();
    assert_eq!(revoked["status"], "unauthorized");
    assert!(revoked["results"][0].get("data").is_none());
    assert_eq!(
        fixture
            .session
            .collaboration
            .bind(
                &fixture.owner,
                fixture.peer["agent_id"].as_str().unwrap(),
                fixture.peer["token"].as_str().unwrap()
            )
            .unwrap()
            .status,
        "idle"
    );
}

#[tokio::test]
async fn activation_rechecks_completion_after_an_older_lease_is_released() {
    let fixture = Fixture::new().await;
    let args = fixture.args(
        &fixture.member,
        json!({"targets":[{"type":"job","id":"job_one"}]}),
    );
    let run = code_mode::AuditRun::new("registry activation", None).unwrap();
    let scope = code_mode::AUDIT_RUN
        .scope(
            run,
            Scope::create(
                fixture.broker.clone(),
                "mcp".into(),
                fixture.token.clone(),
                &args,
            ),
        )
        .await
        .unwrap();
    let record = fixture
        .broker
        .awaiting
        .record(scope.actor(), &args)
        .unwrap();
    let previous = fixture.broker.awaiting.activate(&record).unwrap();
    assert!(!record.terminal.load(Ordering::Acquire)); // A contender's stale pre-acquisition view.
    record.terminal.store(true, Ordering::Release);
    drop(previous);
    let error = fixture.broker.awaiting.activate(&record).err().unwrap();
    assert_eq!(error, (Status::Unavailable, "await_already_resolved"));
    assert!(!record.active.load(Ordering::Acquire));
}

#[tokio::test]
async fn public_member_tool_window_counts_direct_inbox_and_multi_target_await_once() {
    let fixture = Fixture::new().await;
    let id = fixture.session.client_id();
    let send = json!({"client_id":id, "action":"send", "agent_id":fixture.peer["agent_id"],
        "token":fixture.peer["token"], "to":fixture.member["agent_id"], "payload":"for observer"});
    let result = crate::http::execute_server_tool(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        "collab",
        send,
    )
    .await
    .unwrap();
    assert_ne!(result["isError"], true, "{result}");

    let result = fixture
        .public(
            &fixture.member,
            json!({"mode":"all","timeout_ms":1000,
        "targets":[{"type":"agent_status","agent_id":fixture.peer["agent_id"],"statuses":["idle"]},
                   {"type":"agent_messages"}]}),
        )
        .await;
    assert_eq!(result["status"], "ready", "{result}");
    let receive = json!({"client_id":id,"action":"receive","agent_id":fixture.member["agent_id"],
        "token":fixture.member["token"],"wait_ms":0});
    let result = crate::http::execute_server_tool(
        fixture.broker.clone(),
        "mcp".into(),
        fixture.token.clone(),
        "collab",
        receive.clone(),
    )
    .await
    .unwrap();
    assert_ne!(result["isError"], true, "{result}");

    let mut invalid = receive;
    invalid["token"] = fixture.peer["token"].clone();
    assert!(
        crate::http::execute_server_tool(
            fixture.broker.clone(),
            "mcp".into(),
            fixture.token.clone(),
            "collab",
            invalid
        )
        .await
        .is_err()
    );
    let members = fixture.session.collaboration.worker_activity().unwrap();
    let count = |who: &Value| {
        members
            .iter()
            .find(|m| Some(m.agent_id.as_str()) == who["agent_id"].as_str())
            .unwrap()
            .tool_calls_10m
    };
    assert_eq!(count(&fixture.peer), 1);
    assert_eq!(count(&fixture.member), 2);
}
