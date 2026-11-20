//! Transport-independent user-message wait coordinator. The MCP host may run
//! locally; no remote URL, OS notification helper, or model is selected here.
use super::*;
use crate::code_mode::user_wait::{Arguments, Outcome, Recipient};
use tokio::time::{Instant, MissedTickBehavior};

pub(super) async fn authorized(
    broker: &Broker,
    user: &str,
    token: &str,
    client_id: &str,
    session: &Arc<Session>,
    recipient: &Recipient,
) -> Result<Authorization, Outcome> {
    if !session.is_alive() {
        return Err(Outcome::Disconnected);
    }
    let authorization = broker
        .authenticate(user, token)
        .await
        .ok_or(Outcome::AuthorizationRevoked)?;
    let current = broker
        .client(user, &authorization, client_id)
        .await
        .map_err(|_| {
            if session.is_alive() {
                Outcome::AuthorizationRevoked
            } else {
                Outcome::Disconnected
            }
        })?;
    if !Arc::ptr_eq(session, &current) {
        return Err(Outcome::Disconnected);
    }
    let owner =
        crate::bridge::owner(user, &authorization).map_err(|_| Outcome::AuthorizationRevoked)?;
    if owner != recipient.owner {
        return Err(Outcome::AuthorizationRevoked);
    }
    if let Some(binding) = &recipient.binding
        && (binding.owner != owner
            || session
                .collaboration
                .bind(&owner, &binding.agent_id, &binding.token)
                .is_err())
    {
        return Err(Outcome::BindingInvalid);
    }
    Ok(authorization)
}

fn native_data(result: &Result<Value, String>, wait_id: &str) -> Option<Value> {
    let value = result.as_ref().ok()?;
    if value
        .get("isError")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return None;
    }
    let data = &value["structuredContent"];
    (data["wait_id"] == wait_id).then(|| data.clone())
}

pub(super) async fn wait(
    broker: Arc<Broker>,
    user: String,
    token: String,
    client_id: String,
    arguments: Arguments,
    controller: Option<Controller>,
) -> Result<Value, String> {
    wait_observed(broker, user, token, client_id, arguments, controller, None).await
}

/// Admission belongs to this invocation only. A rejected duplicate must never
/// clean up another observer's waiting state or uncommitted reservation.
pub(super) async fn wait_observed(
    broker: Arc<Broker>,
    user: String,
    token: String,
    client_id: String,
    arguments: Arguments,
    controller: Option<Controller>,
    admitted: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Value, String> {
    arguments.validate()?;
    let run = code_mode::AUDIT_RUN
        .try_with(Clone::clone)
        .map_err(|_| "user wait requires a code_mode run")?;
    let authorization = broker
        .authenticate(&user, &token)
        .await
        .ok_or("authorization expired or was revoked; refresh the MCP connection")?;
    let session = broker.client(&user, &authorization, &client_id).await?;
    let recipient = Recipient {
        owner: crate::bridge::owner(&user, &authorization)?,
        binding: run.binding(&client_id),
        connection: Arc::downgrade(&session),
    };
    authorized(&broker, &user, &token, &client_id, &session, &recipient)
        .await
        .map_err(|_| "the user wait recipient no longer has access")?;
    let wait_id = run.begin_user_wait(&client_id, recipient.clone(), arguments.intent)?;
    if let Some(admitted) = admitted {
        admitted.store(true, std::sync::atomic::Ordering::Release);
    }
    let _permit = match session.steering.wait_permit() {
        Ok(permit) => permit,
        Err(_) => {
            run.finish_wait_state(&client_id, Outcome::Busy, "four_waiters_already_active");
            return Ok(run.user_wait_receipt(&client_id));
        }
    };
    // watch retains a change that happens before changed() is first polled.
    let mut changed = session.steering.subscribe();
    match session
        .steering
        .reserve(&client_id, &run.context.run_id, recipient.agent_id())
    {
        Ok(Some(reservation)) => {
            run.reserve_user_wait(&client_id, reservation);
            return Ok(run.user_wait_receipt(&client_id));
        }
        Err(_) => {
            run.finish_wait_state(&client_id, Outcome::Disconnected, "connection_closed");
            return Ok(run.user_wait_receipt(&client_id));
        }
        Ok(None) => {}
    }
    let episode_id = session
        .steering
        .episode(&client_id, &recipient.owner, recipient.agent_id());
    let operation = Operation::CallTool {
        name: "ssmcp.ui.wait".into(),
        arguments: json!({
            "op":"wait","wait_id":wait_id,"episode_id":episode_id,
            "recipient":recipient.agent_id().unwrap_or("next"),"intent":arguments.intent,
            "reason":arguments.reason,"timeout_ms":arguments.timeout_ms,"notify":arguments.notify
        }),
    };
    let native = session.call_with_authorization(operation, controller.clone(), &authorization);
    tokio::pin!(native);
    let deadline = Instant::now() + Duration::from_millis(arguments.timeout_ms);
    // Messages/disconnect/ACL changes wake via watch. A sleeping heartbeat only
    // revalidates OAuth and collaboration leases, which have no queue notifier.
    let mut authority = tokio::time::interval(Duration::from_secs(1));
    authority.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut native_done = false;
    let outcome = loop {
        tokio::select! {
            biased;
            result = &mut native => {
                native_done = true;
                if let Err(outcome) = authorized(&broker, &user, &token, &client_id, &session, &recipient).await {
                    break outcome;
                }
                let Some(data) = native_data(&result, &wait_id) else { break Outcome::Unavailable };
                run.wait_notification(&client_id, &data);
                break match data["status"].as_str() {
                    Some("cancelled") => Outcome::Cancelled,
                    Some("timed_out") => Outcome::TimedOut,
                    Some("busy") => Outcome::Busy,
                    Some("disconnected") => Outcome::Disconnected,
                    Some("authorization_revoked") => Outcome::AuthorizationRevoked,
                    Some("binding_invalid") => Outcome::BindingInvalid,
                    Some("superseded") => Outcome::Superseded,
                    // A native caller can finish UI state. It cannot reserve or
                    // deliver an authenticated user's message on the server.
                    _ => Outcome::Cancelled,
                };
            }
            _ = tokio::time::sleep_until(deadline) => break Outcome::TimedOut,
            change = changed.changed() => {
                if change.is_err() { break Outcome::Disconnected }
            }
            _ = authority.tick() => {}
        }
        if Instant::now() >= deadline {
            break Outcome::TimedOut;
        }
        if let Err(outcome) =
            authorized(&broker, &user, &token, &client_id, &session, &recipient).await
        {
            break outcome;
        }
        if Instant::now() >= deadline {
            break Outcome::TimedOut;
        }
        match session
            .steering
            .reserve(&client_id, &run.context.run_id, recipient.agent_id())
        {
            Ok(Some(reservation)) => {
                run.reserve_user_wait(&client_id, reservation);
                break Outcome::Ready;
            }
            Err(_) => break Outcome::Disconnected,
            Ok(None) => {}
        }
    };
    if outcome != Outcome::Ready {
        run.finish_wait_state(&client_id, outcome, "wait_finished_without_delivery");
    }
    // Closing this UI is best effort, not a delivery acknowledgement. Keep
    // polling the first call while finish is dispatched so a local cancellation
    // already in flight can win before the final HTTP queue commit.
    if !native_done
        && let Ok(current) =
            authorized(&broker, &user, &token, &client_id, &session, &recipient).await
    {
        let finish = session.call_with_authorization(
            Operation::CallTool {
                name: "ssmcp.ui.wait".into(),
                arguments: json!({"op":"finish","wait_id":wait_id,"outcome":outcome}),
            },
            controller,
            &current,
        );
        tokio::pin!(finish);
        let result = tokio::select! {
            biased;
            result = &mut native => Some(result),
            result = &mut finish => Some(result),
            _ = tokio::time::sleep(Duration::from_secs(1)) => None,
        };
        let data = result
            .as_ref()
            .and_then(|result| native_data(result, &wait_id));
        if let Some(data) = &data {
            run.wait_notification(&client_id, data);
        }
        if outcome == Outcome::Ready {
            let status = data.as_ref().and_then(|data| data["status"].as_str());
            let negative = match status {
                Some("ready") => None,
                Some("cancelled") => Some(Outcome::Cancelled),
                Some("timed_out") => Some(Outcome::TimedOut),
                Some("disconnected") => Some(Outcome::Disconnected),
                Some("authorization_revoked") => Some(Outcome::AuthorizationRevoked),
                Some("binding_invalid") => Some(Outcome::BindingInvalid),
                _ => Some(Outcome::Superseded),
            };
            if let Some(negative) = negative {
                run.finish_wait_state(
                    &client_id,
                    negative,
                    "native_completion_not_ready_before_delivery",
                );
            }
        }
    }
    Ok(run.user_wait_receipt(&client_id))
}

pub(super) async fn cancel_native(
    broker: &Broker,
    user: &str,
    token: &str,
    waits: Vec<(String, String)>,
) {
    if waits.is_empty() {
        return;
    }
    // One deadline bounds the entire best-effort cleanup, not N deadlines for
    // N clients. Queue reservations were synchronously released before this.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        for (client_id, wait_id) in waits {
            let Some(authorization) = broker.authenticate(user, token).await else {
                break;
            };
            let Ok(session) = broker.client(user, &authorization, &client_id).await else {
                continue;
            };
            let _ = session
                .call_with_authorization(
                    Operation::CallTool {
                        name: "ssmcp.ui.wait".into(),
                        arguments: json!({"op":"finish","wait_id":wait_id,"outcome":"cancelled"}),
                    },
                    None,
                    &authorization,
                )
                .await;
        }
    })
    .await;
}

pub(super) fn append(mut response: Value, run: &code_mode::AuditRun) -> Value {
    let addition = run.user_wait_addition();
    let Some(fields) = addition["structuredContent"]
        .as_object()
        .filter(|fields| !fields.is_empty())
    else {
        return response;
    };
    let Some(root) = response.as_object_mut() else {
        return response;
    };
    let structured = root.entry("structuredContent").or_insert_with(|| json!({}));
    let Some(structured) = structured.as_object_mut() else {
        return response;
    };
    for (key, value) in fields {
        structured.insert(key.clone(), value.clone());
    }
    if let Some(content) = response["content"].as_array_mut() {
        content.extend(
            addition["content"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        code_mode::AuditRun,
        identity::Identity,
        wire::{self, Frame, SteeringCommand, SteeringState},
    };
    use tokio::{
        io::BufReader,
        net::{
            TcpListener, UnixStream,
            unix::{OwnedReadHalf, OwnedWriteHalf},
        },
        task::JoinHandle,
    };

    struct Fixture {
        broker: Arc<Broker>,
        user: String,
        token: String,
        session: Arc<Session>,
        reader: Option<BufReader<OwnedReadHalf>>,
        writer: Option<OwnedWriteHalf>,
        attached: Option<JoinHandle<()>>,
        http: JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let routes = router(broker.clone());
            let http = tokio::spawn(async move { axum::serve(listener, routes).await.unwrap() });
            let (server, client) = UnixStream::pair().unwrap();
            let task_broker = broker.clone();
            let attached = tokio::spawn(async move { task_broker.attach(server).await.unwrap() });
            let (reader, mut writer) = client.into_split();
            let mut reader = BufReader::new(reader);
            wire::write_frame(
                &mut writer,
                &Frame::Hello {
                    version: wire::VERSION,
                    mode: wire::Mode::Shell,
                    metadata: wire::ClientMetadata {
                        name: "user-wait-fixture".into(),
                        title: None,
                        cwd: "/tmp".into(),
                        hostname: "fixture".into(),
                        allow: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();
            let Some(Frame::Ready {
                url,
                login_code,
                client_id,
                ..
            }) = wire::read_frame(&mut reader).await.unwrap()
            else {
                panic!("missing real local-credential ready frame")
            };
            let user = url.rsplit('/').next().unwrap().to_owned();
            let token =
                super::super::tests::cancellation_test_token(&base, &url, &login_code).await;
            let authorization = broker.authenticate(&user, &token).await.unwrap();
            let session = broker
                .client(&user, &authorization, &client_id)
                .await
                .unwrap();
            Self {
                broker,
                user,
                token,
                session,
                reader: Some(reader),
                writer: Some(writer),
                attached: Some(attached),
                http,
            }
        }

        fn report(&self, run: AuditRun) -> CodeRunReport {
            CodeRunReport {
                broker: self.broker.clone(),
                user: self.user.clone(),
                access_token: self.token.clone(),
                run,
                delivered: true,
            }
        }

        fn host(&self, run: AuditRun) -> Host {
            code_host(
                self.broker.clone(),
                self.user.clone(),
                self.token.clone(),
                run,
                None,
                "user-wait-fixture".into(),
            )
        }

        fn start_wait(&self, timeout_ms: u64) -> (AuditRun, JoinHandle<Result<Value, String>>) {
            let run = AuditRun::new("return await ui.wait(client);", None).unwrap();
            let host = self.host(run.clone());
            let request = HostRequest::WaitUser {
                client_id: self.session.client_id().into(),
                arguments: serde_json::from_value(json!({"timeout_ms":timeout_ms,"notify":false}))
                    .unwrap(),
            };
            (run, tokio::spawn(host(request)))
        }

        async fn next_call(&mut self) -> (u64, Value) {
            loop {
                match tokio::time::timeout(
                    Duration::from_secs(3),
                    wire::read_frame(self.reader.as_mut().unwrap()),
                )
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                {
                    Frame::Call {
                        id,
                        operation: Operation::CallTool { name, arguments },
                        ..
                    } => {
                        assert!(matches!(
                            name.as_str(),
                            "ssmcp.ui.wait" | "ssmcp.ui.message"
                        ));
                        return (id, arguments);
                    }
                    Frame::CodeModeFinished { .. } => {}
                    _ => panic!("unexpected fixture frame"),
                }
            }
        }

        async fn reply(&mut self, id: u64, value: Value) {
            wire::write_frame(
                self.writer.as_mut().unwrap(),
                &Frame::Reply {
                    id,
                    result: Ok(value),
                },
            )
            .await
            .unwrap();
        }

        async fn publish(&mut self, message: &str) {
            wire::write_frame(
                self.writer.as_mut().unwrap(),
                &Frame::Steering {
                    id: 800,
                    command: SteeringCommand::Publish {
                        command_id: format!("publish-{message}"),
                        message_id: message.into(),
                        expected_revision: 0,
                        text: format!("local input {message}"),
                        target: "next".into(),
                    },
                },
            )
            .await
            .unwrap();
            let Some(Frame::SteeringReply { id: 800, result }) =
                wire::read_frame(self.reader.as_mut().unwrap())
                    .await
                    .unwrap()
            else {
                panic!("missing publish acknowledgement")
            };
            assert!(result.is_ok());
        }

        fn state(&self) -> SteeringState {
            self.session
                .steering
                .command(
                    self.session.client_id(),
                    vec![],
                    SteeringCommand::Snapshot {},
                )
                .unwrap()
                .messages[0]
                .state
        }

        async fn close(mut self) {
            drop(self.reader.take());
            drop(self.writer.take());
            tokio::time::timeout(Duration::from_secs(3), self.attached.take().unwrap())
                .await
                .unwrap()
                .unwrap();
            self.http.abort();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(attached) = self.attached.take() {
                attached.abort()
            }
            self.http.abort();
        }
    }

    fn original() -> Value {
        json!({"content":[],"structuredContent":{"result":"kept"},"isError":false})
    }

    #[tokio::test]
    async fn user_wait_real_host_blocks_and_only_final_http_response_commits() {
        let mut fixture = Fixture::new().await;
        let (run, task) = fixture.start_wait(2_000);
        let (call, arguments) = fixture.next_call().await;
        assert_eq!(arguments["op"], "wait");
        assert_eq!(arguments["recipient"], "next");
        assert!(!task.is_finished());
        let id = arguments["wait_id"].clone();
        fixture.publish("requested").await;
        let (finish, arguments) = fixture.next_call().await;
        assert_eq!(arguments["op"], "finish");
        assert_eq!(arguments["outcome"], "ready");
        fixture
            .reply(
                call,
                json!({"structuredContent":{"wait_id":id,"status":"ready"}}),
            )
            .await;
        fixture
            .reply(
                finish,
                json!({"structuredContent":{"wait_id":id,"finished":true,"status":"ready"}}),
            )
            .await;
        let result = task.await.unwrap().unwrap();
        assert_eq!(result["status"], "ready");
        assert_eq!(fixture.state(), SteeringState::Queued);
        let report = fixture.report(run);
        let response = append(finish_user_response(&report, original()).await, &report.run);
        let body = &response["structuredContent"];
        assert_eq!(body["user_wait"][0]["status"], "received");
        assert_eq!(
            body["user_wait"][0]["messages"][0]["message_id"],
            body["user_messages"][0]["message_id"]
        );
        assert_eq!(body["user_messages"][0]["text"], "local input requested");
        assert_eq!(fixture.state(), SteeringState::Delivered);
        fixture.close().await;
    }

    #[tokio::test]
    async fn user_wait_real_host_cancel_wins_after_reservation_without_losing_user_input() {
        let mut fixture = Fixture::new().await;
        let (run, task) = fixture.start_wait(2_000);
        let (call, arguments) = fixture.next_call().await;
        let id = arguments["wait_id"].clone();
        fixture.publish("cancel-race").await;
        let (finish, arguments) = fixture.next_call().await;
        assert_eq!(arguments["outcome"], "ready");
        // The finish ACK reaches the relay before the original cancelled wait
        // reply. Its known terminal status must still defeat delivery.
        fixture
            .reply(
                finish,
                json!({"structuredContent":{"wait_id":id,"finished":false,"status":"cancelled"}}),
            )
            .await;
        fixture
            .reply(
                call,
                json!({"structuredContent":{"wait_id":id,"status":"cancelled"}}),
            )
            .await;
        assert_eq!(task.await.unwrap().unwrap()["status"], "cancelled");
        let report = fixture.report(run);
        let response = append(finish_user_response(&report, original()).await, &report.run);
        assert_eq!(
            response["structuredContent"]["user_wait"][0]["status"],
            "cancelled"
        );
        assert!(response["structuredContent"].get("user_messages").is_none());
        assert_eq!(fixture.state(), SteeringState::Queued);
        assert_eq!(
            fixture
                .session
                .steering
                .claim(
                    fixture.session.client_id(),
                    "retry",
                    None,
                    8,
                    crate::steering::MAX_DELIVERY_BYTES
                )
                .len(),
            1
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn user_wait_real_host_rejects_native_forged_ready_and_does_not_claim_after_timeout() {
        let mut fixture = Fixture::new().await;
        for (index, native_status) in ["ready", "timed_out", "disconnected"]
            .into_iter()
            .enumerate()
        {
            let (run, task) = fixture.start_wait(1_000);
            let (call, arguments) = fixture.next_call().await;
            let id = arguments["wait_id"].clone();
            if native_status == "timed_out" {
                let (finish, arguments) = fixture.next_call().await;
                assert_eq!(arguments["outcome"], "timed_out");
                fixture.reply(finish, json!({"structuredContent":{"wait_id":id,"finished":true,"status":"timed_out"}})).await;
            }
            fixture
                .reply(
                    call,
                    json!({"structuredContent":{"wait_id":id,"status":native_status}}),
                )
                .await;
            let expected = if native_status == "ready" {
                "cancelled"
            } else {
                native_status
            };
            assert_eq!(task.await.unwrap().unwrap()["status"], expected);
            fixture.publish(&format!("late-{index}")).await;
            let report = fixture.report(run);
            let response = append(finish_user_response(&report, original()).await, &report.run);
            assert_eq!(
                response["structuredContent"]["user_wait"][0]["status"],
                expected
            );
            assert!(response["structuredContent"].get("user_messages").is_none());
            if native_status == "timed_out" {
                assert_eq!(
                    response["structuredContent"]["user_wait"][0]["continue_wait"],
                    true
                );
                assert!(
                    response["structuredContent"]
                        .get("continuation_reminder")
                        .is_none()
                );
            }
            assert_eq!(
                fixture
                    .session
                    .steering
                    .claim(
                        fixture.session.client_id(),
                        "later-response",
                        None,
                        8,
                        crate::steering::MAX_DELIVERY_BYTES
                    )
                    .len(),
                1
            );
        }
        fixture.close().await;
    }

    #[tokio::test]
    async fn user_wait_answer_hook_uses_real_reauthorized_host_replies() {
        let mut fixture = Fixture::new().await;
        for (kind, duplicate, error, forged, expected) in [
            ("progress", false, false, false, false),
            ("answer", false, false, false, true),
            ("answer", true, false, false, false),
            ("answer", false, true, false, false),
            ("answer", false, false, true, false),
        ] {
            let run = AuditRun::new(
                "return await say(client,{kind:'answer',text:'fixture'});",
                None,
            )
            .unwrap();
            let host = fixture.host(run.clone());
            let task = tokio::spawn(host(HostRequest::Call {
                client_id: fixture.session.client_id().into(),
                name: "ssmcp.ui.message".into(),
                arguments: json!({"kind":kind,"text":"fixture reply"}),
            }));
            let (call, arguments) = fixture.next_call().await;
            assert_eq!(arguments["kind"], kind);
            let value = if forged {
                json!({"kind":"answer","accepted":true})
            } else {
                json!({"isError":error,"structuredContent":{"accepted":true,"duplicate":duplicate,"message_id":"native_reply"}})
            };
            fixture.reply(call, value).await;
            task.await.unwrap().unwrap();
            let response = append(original(), &run);
            assert_eq!(
                response["structuredContent"]
                    .get("continuation_reminder")
                    .is_some(),
                expected
            );
        }
        fixture.close().await;
    }

    async fn identity_report(bound: bool) -> (CodeRunReport, Arc<Session>) {
        let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
        let identity = Identity {
            provider: "github".into(),
            subject: "42".into(),
            login: "alice".into(),
        };
        let session = broker
            .test_register_identity("fixture-owner", identity)
            .await;
        let tokens = broker.oauth.test_identity_tokens("mcp", "42");
        let token = tokens["access_token"].as_str().unwrap().to_owned();
        let authorization = broker.authenticate("mcp", &token).await.unwrap();
        let owner = crate::bridge::owner("mcp", &authorization).unwrap();
        let run = AuditRun::new("return await ui.wait(client);", None).unwrap();
        if bound {
            let agent = session
                .collaboration
                .dispatch(&owner, "join", json!({"name":"fixture agent"}))
                .await
                .unwrap();
            run.bind(
                session.client_id(),
                &owner,
                agent["agent_id"].as_str().unwrap(),
                agent["token"].as_str().unwrap(),
            )
            .unwrap();
        }
        session
            .steering
            .command(
                session.client_id(),
                vec![],
                SteeringCommand::Publish {
                    command_id: "publish".into(),
                    message_id: "request".into(),
                    expected_revision: 0,
                    text: "retained request".into(),
                    target: "next".into(),
                },
            )
            .unwrap();
        let recipient = Recipient {
            owner,
            binding: run.binding(session.client_id()),
            connection: Arc::downgrade(&session),
        };
        run.begin_user_wait(
            session.client_id(),
            recipient.clone(),
            crate::code_mode::user_wait::Intent::NeedUser,
        )
        .unwrap();
        run.reserve_user_wait(
            session.client_id(),
            session
                .steering
                .reserve(
                    session.client_id(),
                    &run.context.run_id,
                    recipient.agent_id(),
                )
                .unwrap()
                .unwrap(),
        );
        (
            CodeRunReport {
                broker,
                user: "mcp".into(),
                access_token: token,
                run,
                delivered: true,
            },
            session,
        )
    }

    #[tokio::test]
    async fn user_wait_final_decision_rechecks_revocation_ban_and_agent_after_eligible_collection()
    {
        for failure in ["oauth", "ban", "agent"] {
            let (report, session) = identity_report(failure == "agent").await;
            let eligible = eligible_user_response(&report).await.unwrap();
            assert_eq!(eligible.len(), 1);
            // This mutation is deliberately after every async eligibility
            // lookup and before the synchronous queue-delivery decision.
            match failure {
                "oauth" => report.broker.oauth.revoke_subject("42").unwrap(),
                "ban" => {
                    report.broker.user_policies.set("42", true, None);
                }
                "agent" => {
                    let recipient = report.run.wait_recipient(session.client_id()).unwrap();
                    let binding = recipient.binding.unwrap();
                    session
                        .collaboration
                        .dispatch(
                            &binding.owner,
                            "leave",
                            json!({"agent_id":binding.agent_id,"token":binding.token}),
                        )
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            let response = append(
                commit_user_response(&report, original(), eligible),
                &report.run,
            );
            let expected = if failure == "agent" {
                "binding_invalid"
            } else {
                "authorization_revoked"
            };
            assert_eq!(
                response["structuredContent"]["user_wait"][0]["status"],
                expected
            );
            assert!(response["structuredContent"].get("user_messages").is_none());
            let queued = session
                .steering
                .command(session.client_id(), vec![], SteeringCommand::Snapshot {})
                .unwrap();
            assert_eq!(queued.messages[0].state, SteeringState::Queued);
            assert_eq!(queued.messages[0].text, "retained request");
            assert_eq!(
                session
                    .steering
                    .claim(
                        session.client_id(),
                        "another-authorized-response",
                        None,
                        8,
                        crate::steering::MAX_DELIVERY_BYTES
                    )
                    .len(),
                1
            );
        }
    }
}
