use super::*;
use crate::broker::{IDENTITY_RESOURCE, session_io::TestFixture};
use crate::wire::{SteeringCommand, SteeringState};

struct Fixture {
    native: TestFixture,
    pool: Arc<Session>,
    main: Value,
    pool_member: Value,
    owner: String,
    run: code_mode::AuditRun,
    main_bound: bool,
}

impl Fixture {
    async fn new(bind_pool: bool) -> Self {
        let native = TestFixture::new().await;
        let authorization = native
            .broker
            .authenticate(IDENTITY_RESOURCE, &native.access_token)
            .await
            .unwrap();
        let owner = crate::bridge::owner(IDENTITY_RESOURCE, &authorization).unwrap();
        let main = native
            .session
            .collaboration
            .dispatch(&owner, "join", json!({"name":"main","role":"main"}))
            .await
            .unwrap();
        let pool = native
            .broker
            .test_register_identity("automatic-worker-pool", native.actor.clone())
            .await;
        assert_ne!(pool.client_id(), native.session.client_id());
        let pool_member = pool
            .collaboration
            .dispatch(&owner, "join", json!({"name":"main","role":"worker"}))
            .await
            .unwrap();
        let run = code_mode::AuditRun::new("automatic worker subscription fixture", None).unwrap();
        if bind_pool {
            let receipt = code_mode::AUDIT_RUN
                .scope(
                    run.clone(),
                    crate::bridge::dispatch(
                        native.broker.clone(),
                        IDENTITY_RESOURCE.into(),
                        native.access_token.clone(),
                        "collab.bind".into(),
                        json!({
                            "client_id":pool.client_id(),
                            "agent_id":pool_member["agent_id"],"token":pool_member["token"],
                        }),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(receipt["bound"], true);
            assert!(
                receipt.get("worker_subscriptions").is_none(),
                "a display name of main does not turn a worker into a main participant"
            );
        }
        Self {
            native,
            pool,
            main,
            pool_member,
            owner,
            run,
            main_bound: false,
        }
    }

    fn link(&self) -> Value {
        json!({"isError":false,"structuredContent":{"path":"/worker_pool","value":{
            "client_id":self.pool.client_id(),"process_id":42,
            "connected":true,"closed":false
        }}})
    }

    async fn bind_main(&mut self) -> Value {
        let action = code_mode::AUDIT_RUN.scope(
            self.run.clone(),
            crate::bridge::dispatch(
                self.native.broker.clone(),
                IDENTITY_RESOURCE.into(),
                self.native.access_token.clone(),
                "collab.bind".into(),
                json!({
                    "client_id":self.native.session.client_id(),
                    "agent_id":self.main["agent_id"],"token":self.main["token"],
                }),
            ),
        );
        let link = self.link();
        let first = !self.main_bound;
        let replies = async {
            if first {
                let operation = self
                    .native
                    .native_reply(json!({"isError":false,"structuredContent":{"thread_scope":1}}))
                    .await;
                assert_eq!(operation["name"], "ssmcp.client.capabilities");
            }
            let operation = self.native.native_reply(link).await;
            assert_eq!(operation["name"], "ssmcp.ui.get");
            assert_eq!(operation["arguments"], json!({"path":"/worker_pool"}));
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(action, replies)
        })
        .await
        .unwrap();
        self.main_bound = true;
        result.unwrap()
    }

    async fn context(&self) -> Context {
        Context::create(
            &self.native.broker,
            IDENTITY_RESOURCE,
            &self.native.access_token,
            self.native.session.client_id(),
            Some((
                self.main["agent_id"].as_str().unwrap(),
                self.main["token"].as_str().unwrap(),
            )),
        )
        .await
        .unwrap()
    }

    async fn control(&self, mut args: Value) -> Result<Value, String> {
        args["client_id"] = json!(self.native.session.client_id());
        args["agent_id"] = self.main["agent_id"].clone();
        args["token"] = self.main["token"].clone();
        super::super::dispatch(
            self.native.broker.clone(),
            IDENTITY_RESOURCE.into(),
            self.native.access_token.clone(),
            args,
        )
        .await
    }

    fn emit(&self, kind: &str) -> Value {
        let receipt = self
            .native
            .broker
            .events
            .publish_system(
                &self.owner,
                self.pool.client_id(),
                self.pool_member["agent_id"].as_str(),
                kind,
                json!({"task_id":"task-fixture","task_status":"completed"}),
                (None, None),
            )
            .unwrap();
        let binding = self.run.binding(self.pool.client_id()).unwrap();
        published(
            &self.native.broker,
            IDENTITY_RESOURCE,
            &self.native.access_token,
            &self.pool,
            &binding,
            kind,
            receipt["event_id"].as_str().unwrap(),
        );
        receipt
    }

    fn queue_input(&self, id: &str) {
        self.native
            .session
            .steering
            .command(
                self.native.session.client_id(),
                self.native.session.collaboration.presence().unwrap(),
                SteeringCommand::Publish {
                    command_id: format!("publish_{id}"),
                    message_id: id.into(),
                    expected_revision: 0,
                    text: format!("retained requirement {id}"),
                    target: self.main["agent_id"].as_str().unwrap().into(),
                },
            )
            .unwrap();
    }

    fn input_state(&self, id: &str) -> SteeringState {
        self.native
            .session
            .steering
            .command(
                self.native.session.client_id(),
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

    async fn await_call(&self, mut args: Value) -> Value {
        args["client_id"] = json!(self.native.session.client_id());
        args["agent"] = json!({"id":self.main["agent_id"],"token":self.main["token"]});
        let action = crate::http::await_tool::execute(
            self.native.broker.clone(),
            IDENTITY_RESOURCE.into(),
            self.native.access_token.clone(),
            crate::http::await_tool::Arguments::parse(args).unwrap(),
        );
        tokio::pin!(action);
        loop {
            tokio::select! {
                result = &mut action => return result.unwrap()["structuredContent"].clone(),
                operation = self.native.native_reply_with(TestFixture::input_receipt) => {
                    assert_eq!(operation["arguments"]["input_origin"], "worker_event");
                }
            }
        }
    }

    fn lifecycle(&self) -> Value {
        self.native
            .session
            .collaboration
            .lifecycle_snapshot(
                &self.owner,
                self.main["agent_id"].as_str().unwrap(),
                self.main["token"].as_str().unwrap(),
            )
            .unwrap()
    }
}

#[tokio::test]
async fn automatic_worker_subscription_requires_real_separate_pool_binding() {
    let mut missing = Fixture::new(false).await;
    let bound = missing.bind_main().await;
    assert!(
        bound["worker_subscriptions"][0]["error"]
            .as_str()
            .unwrap()
            .contains("needs_binding")
    );
    assert_eq!(
        missing
            .control(json!({"action":"subscriptions"}))
            .await
            .unwrap()["subscriptions"],
        json!([])
    );

    let mut fixture = Fixture::new(true).await;
    let first = fixture.bind_main().await;
    let id = first["worker_subscriptions"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let duplicate = fixture.bind_main().await;
    assert_eq!(duplicate["worker_subscriptions"][0]["subscription_id"], id);
    let listed = fixture
        .control(json!({"action":"subscriptions"}))
        .await
        .unwrap();
    assert_eq!(listed["subscriptions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["subscriptions"][0]["management"], "automatic");
    assert_eq!(
        listed["subscriptions"][0]["filter"]["client_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let page = fixture
        .control(json!({"action":"read","subscription_id":id}))
        .await
        .unwrap();
    assert!(
        page["native_sources"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |source| source["client_id"] == fixture.native.session.client_id()
                    && source["status"] == "server_hub"
            )
    );
}

#[tokio::test]
async fn configured_worker_selection_and_explicit_ack_control_real_wake_and_replay() {
    let mut fixture = Fixture::new(true).await;
    let created = fixture.bind_main().await;
    let id = created["worker_subscriptions"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let changed = fixture
        .control(json!({"action":"policy","patch":{
            "kinds":["worker.completed"]
        }}))
        .await
        .unwrap();
    assert_eq!(changed["worker_subscription"]["management"], "configured");
    assert_eq!(changed["worker_subscription"]["subscription_id"], id);
    assert_eq!(fixture.lifecycle()["status"], "idle");
    fixture.emit("worker.progress");
    assert_eq!(fixture.lifecycle()["status"], "idle");
    fixture.emit("worker.completed");
    assert_eq!(fixture.lifecycle()["status"], "working");
    let page = fixture
        .control(json!({"action":"read","subscription_id":id}))
        .await
        .unwrap();
    assert_eq!(page["events"].as_array().unwrap().len(), 1);
    assert_eq!(page["events"][0]["kind"], "worker.completed");
    let replay = fixture
        .control(json!({"action":"read","subscription_id":id}))
        .await
        .unwrap();
    assert_eq!(replay["events"], page["events"]);
    assert_eq!(replay["cursor"], page["cursor"]);
    fixture
        .control(json!({"action":"ack","subscription_id":id,"through":page["next_cursor"]}))
        .await
        .unwrap();
    assert_eq!(
        fixture
            .control(json!({"action":"read","subscription_id":id}))
            .await
            .unwrap()["events"],
        json!([])
    );
}

#[tokio::test]
async fn advancing_retention_boundary_keeps_one_visible_gap_and_the_latest_receipt() {
    let mut fixture = Fixture::new(true).await;
    fixture.bind_main().await;
    let context = fixture.context().await;
    let entry = fixture
        .native
        .broker
        .event_subscriptions
        .workers
        .get(&context)
        .unwrap();
    let report = CodeRunReport {
        broker: fixture.native.broker.clone(),
        user: IDENTITY_RESOURCE.into(),
        access_token: fixture.native.access_token.clone(),
        run: fixture.run.clone(),
        delivered: true,
    };
    let target = EligibleUserResponse {
        session: context.session.clone(),
        owner: fixture.owner.clone(),
        binding: Some(context.binding.clone()),
        may_claim: false,
        waiting: false,
    };
    let mut attempted = 0;
    for (cursor, oldest, expected_writes) in [(28, 303, 1), (28, 450, 1), (32, 480, 2)] {
        let notice = json!({"status":"unavailable","events":[],"history_complete":false,
            "error": format!("events_gap: cursor {cursor} precedes retained cursor {oldest}; latest cursor {}", oldest + 1024)});
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let result = {
            let observed = visible_notice(
                &report,
                &target,
                &context,
                &entry,
                notice.clone(),
                deadline,
                &mut attempted,
            );
            tokio::pin!(observed);
            loop {
                tokio::select! {
                    result = &mut observed => break result,
                    _ = fixture.native.native_reply_with(TestFixture::input_receipt) => {}
                }
            }
        };
        assert_eq!(result, Some(notice));
        assert_eq!(attempted, expected_writes);
    }
}

#[tokio::test]
async fn worker_notice_sleeps_without_events_and_wakes_without_consuming_input_or_cursor() {
    let mut fixture = Fixture::new(true).await;
    fixture.bind_main().await;
    let context = fixture.context().await;
    let no_event = wait_for_notice(
        &fixture.native.broker,
        IDENTITY_RESOURCE,
        &fixture.native.access_token,
        &context.session,
        &context.binding,
        1_000,
        Some("fixture-wait"),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), no_event)
            .await
            .is_err()
    );
    assert_eq!(fixture.lifecycle()["status"], "idle");
    fixture.queue_input("queued_input");
    let event_wait = wait_for_notice(
        &fixture.native.broker,
        IDENTITY_RESOURCE,
        &fixture.native.access_token,
        &context.session,
        &context.binding,
        1_000,
        Some("fixture-wait"),
    );
    let publish = async {
        tokio::task::yield_now().await;
        fixture.emit("worker.completed");
    };
    let (notice, ()) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(event_wait, publish)
    })
    .await
    .unwrap();
    let notice = notice.unwrap();
    assert_eq!(notice["status"], "ready");
    assert_eq!(notice["events"][0]["kind"], "worker.completed");
    let replay = fixture
        .control(json!({
            "action":"read","subscription_id":notice["subscription_id"]
        }))
        .await
        .unwrap();
    assert_eq!(replay["cursor"], notice["cursor"]);
    assert_eq!(replay["events"], notice["events"]);
    assert_eq!(fixture.input_state("queued_input"), SteeringState::Queued);
}

#[tokio::test]
async fn unsubscribe_stays_disabled_and_source_authority_is_never_a_display_name() {
    let mut fixture = Fixture::new(true).await;
    let initial = fixture.bind_main().await;
    let id = initial["worker_subscriptions"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let peer = fixture
        .native
        .session
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"main","role":"main"}))
        .await
        .unwrap();
    let denied = super::super::dispatch(
        fixture.native.broker.clone(),
        IDENTITY_RESOURCE.into(),
        fixture.native.access_token.clone(),
        json!({
            "client_id":fixture.native.session.client_id(),
            "agent_id":peer["agent_id"],"token":peer["token"],
            "action":"read","subscription_id":id
        }),
    )
    .await;
    assert!(denied.is_err());
    fixture.native.session.collaboration.dispatch(&fixture.owner, "update", json!({
        "agent_id":fixture.main["agent_id"],"token":fixture.main["token"],"name":"renamed main"
    })).await.unwrap();
    assert_eq!(
        fixture
            .control(json!({"action":"read","subscription_id":id}))
            .await
            .unwrap()["subscription_id"],
        id
    );
    fixture
        .control(json!({"action":"unsubscribe","subscription_id":id}))
        .await
        .unwrap();
    let rebound = code_mode::AUDIT_RUN
        .scope(
            fixture.run.clone(),
            crate::bridge::dispatch(
                fixture.native.broker.clone(),
                IDENTITY_RESOURCE.into(),
                fixture.native.access_token.clone(),
                "collab.bind".into(),
                json!({
                    "client_id":fixture.native.session.client_id(),
                    "agent_id":fixture.main["agent_id"],"token":fixture.main["token"],
                }),
            ),
        )
        .await
        .unwrap();
    assert_eq!(rebound["worker_subscriptions"][0]["enabled"], false);
    assert_eq!(
        fixture
            .control(json!({"action":"subscriptions"}))
            .await
            .unwrap()["subscriptions"],
        json!([])
    );
}

#[tokio::test]
async fn retiring_main_and_revoked_main_cannot_be_woken_or_read() {
    let mut fixture = Fixture::new(true).await;
    let initial = fixture.bind_main().await;
    let id = initial["worker_subscriptions"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let revision = fixture.lifecycle()["lifecycle"]["revision"].clone();
    fixture
        .native
        .session
        .collaboration
        .dispatch(
            &fixture.owner,
            "update",
            json!({
                "agent_id":fixture.main["agent_id"],"token":fixture.main["token"],
                "status":"retiring","expected_revision":revision
            }),
        )
        .await
        .unwrap();
    assert_eq!(fixture.lifecycle()["status"], "offline");
    fixture.emit("worker.completed");
    assert_eq!(fixture.lifecycle()["status"], "offline");
    let context = fixture.context().await;
    assert!(
        wait_for_notice(
            &fixture.native.broker,
            IDENTITY_RESOURCE,
            &fixture.native.access_token,
            &context.session,
            &context.binding,
            0,
            None,
        )
        .await
        .is_none()
    );
    fixture.native.revoke_access();
    assert!(
        fixture
            .control(json!({"action":"read","subscription_id":id}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn both_ordinary_and_durable_await_wake_without_completing_the_original_target() {
    for durable in [false, true] {
        let mut fixture = Fixture::new(true).await;
        fixture.bind_main().await;
        let peer = fixture
            .native
            .session
            .collaboration
            .dispatch(
                &fixture.owner,
                "join",
                json!({"name":"pending peer","role":"worker"}),
            )
            .await
            .unwrap();
        fixture.queue_input("still_queued");
        let mut request = json!({
            "timeout_ms":1000,
            "targets":[{"type":"agent_status","agent_id":peer["agent_id"],"statuses":["working"]}]
        });
        if durable {
            request["request_id"] = json!("durable_subscription_wake");
            request["lifetime_ms"] = json!(60_000);
            request["window_ms"] = json!(1000);
        }
        let caller = fixture.await_call(request);
        let publish = async {
            tokio::task::yield_now().await;
            fixture.emit("worker.completed");
        };
        let (pending, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(caller, publish)
        })
        .await
        .unwrap();
        assert_eq!(pending["status"], "pending", "{pending}");
        assert_eq!(pending["continue_wait"], true);
        assert_eq!(pending["wake_reason"], "subscription_event");
        if durable {
            assert_eq!(pending["results"], json!([]));
            assert_eq!(pending["metadata_only"], true);
            assert_eq!(pending["cursor"], 0);
            assert_eq!(pending["targets"][0]["agent_id"], peer["agent_id"]);
            assert!(pending["metadata"]["completed_at_ms"].is_null());
        } else {
            assert_eq!(pending["results"][0]["status"], "pending");
        }
        let notice = &pending["worker_events"][0];
        assert_eq!(notice["events"][0]["kind"], "worker.completed");
        assert_eq!(fixture.input_state("still_queued"), SteeringState::Queued);
        let replay = fixture
            .control(json!({
                "action":"read","subscription_id":notice["subscription_id"]
            }))
            .await
            .unwrap();
        assert_eq!(replay["events"], notice["events"]);
        assert_eq!(replay["cursor"], notice["cursor"]);
        fixture
            .control(json!({
                "action":"ack","subscription_id":notice["subscription_id"],
                "through":notice["next_cursor"]
            }))
            .await
            .unwrap();

        let revision = fixture
            .native
            .session
            .collaboration
            .lifecycle_snapshot(
                &fixture.owner,
                peer["agent_id"].as_str().unwrap(),
                peer["token"].as_str().unwrap(),
            )
            .unwrap()["lifecycle"]["revision"]
            .clone();
        fixture
            .native
            .session
            .collaboration
            .dispatch(
                &fixture.owner,
                "update",
                json!({
                    "agent_id":peer["agent_id"],"token":peer["token"],
                    "status":"working","expected_revision":revision
                }),
            )
            .await
            .unwrap();
        if durable {
            // No read window is active while the detached original observer
            // reacts to its target's actual state transition.
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let status = fixture
                        .await_call(json!({
                            "op":"status","wait_id":pending["wait_id"]
                        }))
                        .await;
                    if status["status"] == "ready" {
                        assert_eq!(status["cursor"], 1);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }
        let ready = fixture
            .await_call(json!({
                "wait_id":pending["wait_id"],"timeout_ms":1000
            }))
            .await;
        assert_eq!(ready["wait_id"], pending["wait_id"]);
        assert_eq!(ready["status"], "ready", "{ready}");
        assert_eq!(fixture.input_state("still_queued"), SteeringState::Queued);
    }
}

#[tokio::test]
async fn live_managed_choices_survive_advisory_expiry_and_partial_patches() {
    let mut fixture = Fixture::new(true).await;
    fixture.bind_main().await;
    fixture
        .control(json!({"action":"policy","patch":{
            "enabled":false,"kinds":["worker.completed"]
        }}))
        .await
        .unwrap();
    crate::http::coordination_policy::test_expire_entry(
        &fixture.owner,
        fixture.native.session.client_id(),
        fixture.main["agent_id"].as_str().unwrap(),
    );
    let expired = fixture.control(json!({"action":"policy"})).await.unwrap();
    assert_eq!(expired["enabled"], false);
    assert_eq!(expired["kinds"], json!(["worker.completed"]));
    assert_eq!(
        expired["enabled"],
        expired["worker_subscription"]["enabled"]
    );
    assert_eq!(expired["kinds"], expired["worker_subscription"]["kinds"]);
    let partial = fixture
        .control(json!({"action":"policy","patch":{
            "window_ms":4000
        }}))
        .await
        .unwrap();
    assert_eq!(partial["window_ms"], 4000);
    assert_eq!(partial["enabled"], false);
    assert_eq!(partial["kinds"], json!(["worker.completed"]));
    let changed = fixture
        .control(json!({"action":"policy","patch":{
            "kinds":["worker.idle"]
        }}))
        .await
        .unwrap();
    assert_eq!(changed["enabled"], false);
    assert_eq!(changed["kinds"], json!(["worker.idle"]));
    fixture.emit("worker.idle");
    assert_eq!(fixture.lifecycle()["status"], "idle");
}

#[tokio::test]
async fn revoked_pool_binding_can_only_be_replaced_after_explicit_managed_revocation() {
    let mut fixture = Fixture::new(true).await;
    let first = fixture.bind_main().await;
    let id = first["worker_subscriptions"][0]["subscription_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture
        .pool
        .collaboration
        .dispatch(
            &fixture.owner,
            "leave",
            json!({
                "agent_id":fixture.pool_member["agent_id"],"token":fixture.pool_member["token"]
            }),
        )
        .await
        .unwrap();
    assert!(
        fixture
            .control(json!({"action":"read","subscription_id":id}))
            .await
            .is_err()
    );
    fixture.pool_member = fixture
        .pool
        .collaboration
        .dispatch(
            &fixture.owner,
            "join",
            json!({"name":"replacement pool member","role":"worker"}),
        )
        .await
        .unwrap();
    fixture.run = code_mode::AuditRun::new("replacement authenticated pool binding", None).unwrap();
    code_mode::AUDIT_RUN
        .scope(
            fixture.run.clone(),
            crate::bridge::dispatch(
                fixture.native.broker.clone(),
                IDENTITY_RESOURCE.into(),
                fixture.native.access_token.clone(),
                "collab.bind".into(),
                json!({"client_id":fixture.pool.client_id(),
            "agent_id":fixture.pool_member["agent_id"],"token":fixture.pool_member["token"]}),
            ),
        )
        .await
        .unwrap();
    let changed = fixture.bind_main().await;
    assert_eq!(changed["worker_subscriptions"][0]["subscription_id"], id);
    assert!(
        changed["worker_subscriptions"][0]["error"]
            .as_str()
            .unwrap()
            .contains("source_changed")
    );
    let revoked = fixture
        .control(json!({"action":"unsubscribe","subscription_id":id}))
        .await
        .unwrap();
    assert_eq!(revoked["status"], "unsubscribed");
    fixture
        .control(json!({"action":"policy","patch":{"enabled":true}}))
        .await
        .unwrap();
    let restored = fixture.bind_main().await;
    assert_ne!(restored["worker_subscriptions"][0]["subscription_id"], id);
    assert_eq!(restored["worker_subscriptions"][0]["status"], "subscribed");
    assert!(
        fixture
            .control(json!({"action":"read","subscription_id":id}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn another_original_session_cannot_read_a_same_account_main_subscription() {
    let mut fixture = Fixture::new(true).await;
    let first = fixture.bind_main().await;
    let id = &first["worker_subscriptions"][0]["subscription_id"];
    let other = fixture
        .native
        .broker
        .test_register_identity("other main session", fixture.native.actor.clone())
        .await;
    let member = other
        .collaboration
        .dispatch(&fixture.owner, "join", json!({"name":"main","role":"main"}))
        .await
        .unwrap();
    let denied = super::super::dispatch(
        fixture.native.broker.clone(),
        IDENTITY_RESOURCE.into(),
        fixture.native.access_token.clone(),
        json!({
            "client_id":other.client_id(),"agent_id":member["agent_id"],
            "token":member["token"],"action":"read","subscription_id":id
        }),
    )
    .await;
    assert!(denied.is_err());
}

#[test]
fn notice_budget_delivers_a_precise_prefix_or_explicit_read_fallback() {
    let response = json!({"isError":false,"content":[],"structuredContent":{}});
    let notice = json!({
        "status":"ready","client_id":"a".repeat(64),
        "subscription_id":"subscription_fixture","cursor":0,"next_cursor":9,"has_more":false,
        "events":(1..=4).map(|seq| json!({
            "seq":seq,"id":format!("event_{seq}"),"kind":"custom.progress",
            "data":{"body":"x".repeat(2500)}
        })).collect::<Vec<_>>()
    });
    let (response_with_events, prefix) =
        fit_notice(&response, &[], notice.clone(), MAX_NOTICE_BYTES).unwrap();
    let events = prefix["events"].as_array().unwrap();
    assert!(!events.is_empty() && events.len() < 4);
    assert_eq!(prefix["next_cursor"], events.last().unwrap()["seq"]);
    assert_eq!(prefix["cursor"], 0);
    assert_eq!(prefix["has_more"], true);
    assert!(
        response_with_events.to_string().len() - response.to_string().len() <= MAX_NOTICE_BYTES
    );
    let (small_response, fallback) = fit_notice(&response, &[], notice, 1400).unwrap();
    assert_eq!(fallback["status"], "read_required");
    assert_eq!(fallback["read_limit"], 1);
    assert_eq!(fallback["events"], json!([]));
    assert!(fallback.get("next_cursor").is_none());
    assert!(small_response.to_string().len() - response.to_string().len() <= 1400);
}

#[test]
fn automatic_repeat_projection_keeps_material_events_and_explicit_recovery_available() {
    let now = std::time::Instant::now();
    let mut reminders = ModelReminder::default();
    let page = json!({"status":"ready","client_id":"control","subscription_id":"sub",
        "cursor":10,"next_cursor":11,"has_more":false,"events":[
        {"seq":11,"kind":"worker.progress","data":{"task_status":"working","details":"full details"}}]});
    let first = reminders.project(0, page.clone(), now).unwrap();
    assert_eq!(first, page);
    let repeat = reminders.project(0, page.clone(), now).unwrap();
    assert_eq!(repeat["status"], "read_required");
    assert_eq!(repeat["subscription_id"], "sub");
    assert!(repeat.get("next_cursor").is_none());
    let gap = json!({"status":"unavailable","subscription_id":"sub",
        "error":"events_gap: cursor 28 precedes retained cursor 303; latest cursor 1327","events":[]});
    assert_eq!(reminders.project(0, gap.clone(), now), Some(gap.clone()));
    let mut moving_gap = gap.clone();
    moving_gap["error"] =
        json!("events_gap: cursor 28 precedes retained cursor 450; latest cursor 1474");
    assert_eq!(reminders.project(0, moving_gap.clone(), now), None);
    assert_eq!(
        reminders.project(1, moving_gap.clone(), now),
        Some(moving_gap.clone())
    );
    assert_eq!(
        reminders.project(1, moving_gap.clone(), now + Duration::from_secs(60)),
        Some(moving_gap)
    );
    for (kind, status) in [
        ("worker.unknown", "unknown"),
        ("worker.completed", "failed"),
        ("user.input_available", ""),
    ] {
        let mut important = page.clone();
        important["events"][0]["kind"] = json!(kind);
        important["events"][0]["data"]["task_status"] = json!(status);
        for _ in 0..2 {
            let delivered = reminders.project(1, important.clone(), now).unwrap();
            assert_eq!(delivered, important);
            let summary = notice_summary(&delivered);
            assert!(summary.contains(kind));
            assert!(summary.contains(status));
            assert!(!summary.contains("full details"));
        }
    }
}

#[test]
fn text_summary_keeps_authoritative_machine_receipts_and_user_content() {
    let original = json!({"isError":true,"content":[{"type":"text","text":"existing error"}],
        "structuredContent":{"user_messages":[{"text":"actual user instruction"}]}});
    let notice = json!({"subscription_id":"sub","status":"ready","cursor":10,"next_cursor":11,
        "has_more":false,"events":[{"seq":11,"kind":"worker.completed","data":{"task_id":"original",
        "task_status":"failed","details":"fixture event detail covered by structured access"}}]});
    let result = append_values(&original, std::slice::from_ref(&notice), MAX_NOTICE_BYTES).unwrap();
    assert_eq!(result["worker_events"], json!([notice]));
    assert_eq!(
        result["structuredContent"]["worker_events"],
        result["worker_events"]
    );
    assert_eq!(
        result["structuredContent"]["user_messages"],
        original["structuredContent"]["user_messages"]
    );
    assert_eq!(result["content"][0], original["content"][0]);
    assert_eq!(result["isError"], true);
    let text = result["content"][1]["text"].as_str().unwrap();
    assert!(text.contains("11 worker.completed (failed)"));
    assert!(!text.contains("fixture event detail"));
}
