//! Transport-independent user-message wait coordinator. The MCP host may run
//! locally; no remote URL, OS notification helper, or model is selected here.
use super::*;
use crate::code_mode::user_wait::{Arguments, Intent, Outcome, Recipient};
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
    coordinate_wait(
        broker,
        user,
        token,
        client_id,
        WaitRequest {
            arguments,
            message: None,
        },
        controller,
        admitted,
    )
    .await
}

/// The public message flag follows the same coordinator as ui.wait. Register
/// its finish intent before publication so outer cancellation still reports that
/// the task has no approval, even if the message ACK was already committed.
pub(super) async fn message(
    broker: Arc<Broker>,
    user: String,
    token: String,
    client_id: String,
    message: Value,
    controller: Option<Controller>,
) -> Result<Value, String> {
    let arguments = Arguments {
        timeout_ms: crate::code_mode::user_wait::DEFAULT_TIMEOUT_MS,
        intent: Intent::Finish,
        // Keep the native default reason so, after an uncertain ACK/timeout,
        // a new default wait can rediscover the same pending scoped request.
        reason: None,
        notify: true,
        finish_request_id: None,
    };
    let run = code_mode::AUDIT_RUN
        .try_with(Clone::clone)
        .map_err(|_| "finish message requires a code_mode run")?;
    let result = coordinate_wait(
        broker.clone(),
        user.clone(),
        token.clone(),
        client_id.clone(),
        WaitRequest {
            arguments,
            message: Some(message),
        },
        controller,
        None,
    )
    .await?;
    // Cleanup may itself await a native reply. Recheck after every coordinator
    // return, including deadline/busy/early-input paths, before exposing the ACK.
    let recipient = run
        .wait_recipient(&client_id)
        .ok_or("finish message wait state disappeared before delivery")?;
    let authority = match recipient.connection.upgrade() {
        Some(session) => authorized(&broker, &user, &token, &client_id, &session, &recipient).await,
        None => Err(Outcome::Disconnected),
    };
    if let Err(outcome) = authority {
        run.finish_wait_state(
            &client_id,
            outcome,
            "authorization_changed_before_message_delivery",
        );
        return Ok(wait_reply(&run, &client_id, Some(&result)));
    }
    Ok(result)
}

struct WaitRequest {
    arguments: Arguments,
    message: Option<Value>,
}

fn wait_reply(run: &code_mode::AuditRun, client_id: &str, message: Option<&Value>) -> Value {
    let receipt = run.user_wait_receipt(client_id);
    let Some(message) = message else {
        return receipt;
    };
    let inaccessible = matches!(
        receipt["status"].as_str(),
        Some("authorization_revoked" | "binding_invalid" | "disconnected")
    );
    let mut result = if !inaccessible && message.is_object() {
        message.clone()
    } else {
        // Never expose an earlier native payload through a now-revoked wait.
        // Its publication may still have happened, so avoid a blind resend.
        json!({"isError":true,"content":[],
            "structuredContent":{"message_delivery":"may_have_been_saved"}})
    };
    if !result["structuredContent"].is_object() {
        result["structuredContent"] = json!({});
    }
    let data = &mut result["structuredContent"];
    data["finish_task"] = json!(true);
    // The final HTTP delivery rechecks authorization and input epoch. An inner
    // native completion or message ACK never independently grants permission.
    data["may_finish"] = json!(false);
    data["finish_request_id"] = receipt["finish_request_id"].clone();
    data["completion_instruction"] = receipt["completion_instruction"].clone();
    data["finish_wait"] = receipt.clone();
    if !result["content"].is_array() {
        result["content"] = json!([]);
    }
    result["content"].as_array_mut().expect("message content").push(
        json!({"type":"text","text":format!("TASK FINISH WAIT\n{receipt}\nOnly the final outer user_wait receipt may authorize ending. Follow completion_instruction if it does not grant may_finish:true.")})
    );
    result
}

async fn coordinate_wait(
    broker: Arc<Broker>,
    user: String,
    token: String,
    client_id: String,
    request: WaitRequest,
    controller: Option<Controller>,
    admitted: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Value, String> {
    let WaitRequest { arguments, message } = request;
    arguments.validate()?;
    let run = code_mode::AUDIT_RUN
        .try_with(Clone::clone)
        .map_err(|_| "user wait requires a code_mode run")?;
    let authorization = broker
        .authenticate(&user, &token)
        .await
        .ok_or("authorization expired or was revoked; refresh the MCP connection")?;
    let session = broker.client(&user, &authorization, &client_id).await?;
    session.require_chat()?;
    let recipient = Recipient {
        owner: crate::bridge::owner(&user, &authorization)?,
        binding: run.binding(&client_id),
        connection: Arc::downgrade(&session),
    };
    authorized(&broker, &user, &token, &client_id, &session, &recipient)
        .await
        .map_err(|_| "the user wait recipient no longer has access")?;
    if arguments.intent == Intent::Finish && recipient.binding.is_none() {
        return Err("finish requires a verified collab thread binding; bind the asking agent before waiting for an end decision".into());
    }
    let wait_id = run.begin_user_wait(&client_id, recipient.clone(), arguments.intent)?;
    if let Some(admitted) = admitted {
        admitted.store(true, std::sync::atomic::Ordering::Release);
    }
    let mut message_response = None;
    if let Some(arguments) = message {
        run.record_connection(&session)?;
        let mut result = session.call_with_bound_authorization(
            Operation::CallTool { name: "ssmcp.ui.message".into(), arguments },
            controller.clone(), &authorization, recipient.binding.clone(),
        ).await.map_err(|error| {
            run.finish_wait_state(&client_id, Outcome::Unavailable, "message_ack_unavailable");
            format!("message delivery could not be confirmed: {error}; it may already have been saved. Keep the task active and use a new blocking ui.wait with intent finish; do not resend blindly.")
        })?;
        if let Err(outcome) =
            authorized(&broker, &user, &token, &client_id, &session, &recipient).await
        {
            run.finish_wait_state(&client_id, outcome, "authorization_changed_after_message");
            // As with every native answer, do not disclose its content after
            // revocation. The outer wait receipt still retains the no-approval
            // instruction and reports that delivery may already have happened.
            return Err("authorization changed after message dispatch; the message may already have been saved, but no end approval was delivered. Restore the original verified thread and make a new blocking ui.wait call; do not resend blindly.".into());
        }
        run.capture_context(&client_id, &result);
        run.capture_question_answers(&session, recipient.agent_id(), &result);
        code_mode::question_answers::strip_native_metadata(&mut result);
        let accepted = result
            .get("isError")
            .is_none_or(|error| error == &Value::Bool(false))
            && result["structuredContent"]["accepted"] == true
            && result["structuredContent"]["message_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty());
        message_response = Some(result);
        if !accepted {
            run.finish_wait_state(&client_id, Outcome::Unavailable, "message_not_accepted");
            return Ok(wait_reply(&run, &client_id, message_response.as_ref()));
        }
    }
    let _permit = match session.steering.wait_permit() {
        Ok(permit) => permit,
        Err(_) => {
            run.finish_wait_state(&client_id, Outcome::Busy, "four_waiters_already_active");
            return Ok(wait_reply(&run, &client_id, message_response.as_ref()));
        }
    };
    // watch retains a change that happens before changed() is first polled.
    let mut changed = session.steering.subscribe();
    let episode_id = session
        .steering
        .episode(&client_id, &recipient.owner, recipient.agent_id());
    if arguments.intent == Intent::Finish {
        run.begin_finish_wait(
            &client_id,
            arguments.finish_request_id.clone(),
            episode_id.clone(),
        );
    }
    match session
        .steering
        .reserve(&client_id, &run.context.run_id, recipient.agent_id())
    {
        Ok(Some(reservation)) => {
            run.reserve_user_wait(&client_id, reservation);
            if let Some(finish_request_id) = run.finish_request_id(&client_id) {
                // Existing text is delivered through the ordinary reservation;
                // it can never stand in for a Human decision. Invalidate an
                // explicitly resumed request without creating a new one.
                let close = session.call_with_bound_authorization(
                    Operation::CallTool {
                        name: "ssmcp.ui.wait".into(),
                        arguments: json!({"op":"finish","wait_id":wait_id,"outcome":"ready",
                            "finish_request_id":finish_request_id}),
                    },
                    controller.clone(),
                    &authorization,
                    recipient.binding.clone(),
                );
                if let Ok(result) = tokio::time::timeout(Duration::from_secs(1), close).await
                    && let Some(data) = native_data(&result, &wait_id)
                {
                    run.capture_finish_wait(&client_id, &data);
                }
            }
            return Ok(wait_reply(&run, &client_id, message_response.as_ref()));
        }
        Err(_) => {
            run.finish_wait_state(&client_id, Outcome::Disconnected, "connection_closed");
            return Ok(wait_reply(&run, &client_id, message_response.as_ref()));
        }
        Ok(None) => {}
    }
    let operation = Operation::CallTool {
        name: "ssmcp.ui.wait".into(),
        arguments: json!({
            "op":"wait","wait_id":wait_id,"episode_id":episode_id,
            "recipient":recipient.agent_id().unwrap_or("next"),"intent":arguments.intent,
            "reason":arguments.reason,"timeout_ms":arguments.timeout_ms,"notify":arguments.notify,
            "finish_request_id":run.finish_request_id(&client_id)
        }),
    };
    let native = session.call_with_bound_authorization(
        operation,
        controller.clone(),
        &authorization,
        recipient.binding.clone(),
    );
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
                let decision = arguments.intent == Intent::Finish
                    && run.capture_finish_wait(&client_id, &data);
                if decision && data["status"] == "decided" {
                    // A new ordinary input may invalidate the native request
                    // before the queue watch wins this select. It still follows
                    // the original reservation/outer-commit delivery path.
                    match session.steering.reserve(
                        &client_id, &run.context.run_id, recipient.agent_id(),
                    ) {
                        Ok(Some(reservation)) => {
                            run.reserve_user_wait(&client_id, reservation);
                            break Outcome::Ready;
                        }
                        Err(_) => break Outcome::Disconnected,
                        Ok(None) => {}
                    }
                }
                break match data["status"].as_str() {
                    Some("decided") if decision && run.finish_decision_ready(&client_id) => {
                        Outcome::DecisionReady
                    }
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
    if !matches!(outcome, Outcome::Ready | Outcome::DecisionReady) {
        run.finish_wait_state(&client_id, outcome, "wait_finished_without_delivery");
    }
    // Closing this UI is best effort, not a delivery acknowledgement. Keep
    // polling the first call while finish is dispatched so a local cancellation
    // already in flight can win before the final HTTP queue commit.
    if !native_done
        && let Ok(current) =
            authorized(&broker, &user, &token, &client_id, &session, &recipient).await
    {
        let finish = session.call_with_bound_authorization(
            Operation::CallTool {
                name: "ssmcp.ui.wait".into(),
                arguments: json!({"op":"finish","wait_id":wait_id,"outcome":outcome,
                    "finish_request_id":run.finish_request_id(&client_id)}),
            },
            controller,
            &current,
            recipient.binding.clone(),
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
            if arguments.intent == Intent::Finish {
                run.capture_finish_wait(&client_id, data);
            }
        }
        if outcome == Outcome::Ready {
            let status = data.as_ref().and_then(|data| data["status"].as_str());
            let negative = match status {
                Some("ready") => None,
                Some("decided")
                    if arguments.intent == Intent::Finish
                        && data
                            .as_ref()
                            .is_some_and(|data| run.capture_finish_wait(&client_id, data)) =>
                {
                    None
                }
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
    Ok(wait_reply(&run, &client_id, message_response.as_ref()))
}

pub(super) async fn cancel_native(
    broker: &Broker,
    user: &str,
    token: &str,
    run: &code_mode::AuditRun,
    waits: Vec<(String, String, Option<String>)>,
) {
    if waits.is_empty() {
        return;
    }
    // One deadline bounds the entire best-effort cleanup, not N deadlines for
    // N clients. Queue reservations were synchronously released before this.
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        for (client_id, wait_id, finish_request_id) in waits {
            let Some(authorization) = broker.authenticate(user, token).await else {
                break;
            };
            let Ok(session) = broker.client(user, &authorization, &client_id).await else {
                continue;
            };
            let Some(recipient) = run.wait_recipient(&client_id) else {
                continue;
            };
            if recipient
                .connection
                .upgrade()
                .is_none_or(|original| !Arc::ptr_eq(&original, &session))
                || authorized(broker, user, token, &client_id, &session, &recipient)
                    .await
                    .is_err()
            {
                continue;
            }
            let cleanup = session.call_with_bound_authorization(
                Operation::CallTool {
                    name: "ssmcp.ui.wait".into(),
                    arguments: json!({"op":"finish","wait_id":wait_id,"outcome":"cancelled",
                        "finish_request_id":finish_request_id}),
                },
                None,
                &authorization,
                recipient.binding,
            );
            // Cleanup retains the original verified thread's CodeContext;
            // neither controller text nor a later connection can select it.
            let _ = code_mode::AUDIT_RUN.scope(run.clone(), cleanup).await;
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

    #[tokio::test]
    async fn disabled_chat_rejects_before_wait_state_or_native_dispatch() {
        let mut fixture = Fixture::with_disabled_chat(true).await;
        let (run, task) = fixture.start_wait(2000);
        let result = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), wire::CHAT_DISABLED);
        assert!(run.wait_recipient(fixture.session.client_id()).is_none());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                wire::read_frame(fixture.reader.as_mut().unwrap()),
            )
            .await
            .is_err()
        );
    }

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
            Self::with_disabled_chat(false).await
        }

        async fn with_disabled_chat(disabled: bool) -> Self {
            let mut broker = Broker::new("https://ssmcp.example".into());
            broker.local_chat_disabled = disabled;
            let broker = Arc::new(broker);
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
                capabilities,
                ..
            }) = wire::read_frame(&mut reader).await.unwrap()
            else {
                panic!("missing real local-credential ready frame")
            };
            assert_eq!(
                capabilities
                    .iter()
                    .any(|capability| capability == wire::CHAT_DISABLED_CAPABILITY),
                disabled
            );
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
                Default::default(),
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

        async fn bound_run(&self) -> AuditRun {
            let authorization = self
                .broker
                .authenticate(&self.user, &self.token)
                .await
                .unwrap();
            let owner = crate::bridge::owner(&self.user, &authorization).unwrap();
            let member = self
                .session
                .collaboration
                .dispatch(&owner, "join", json!({"name":"same displayed agent"}))
                .await
                .unwrap();
            let run = AuditRun::new(
                "return await say(client,{text:'fixture result',finish_task:true});",
                None,
            )
            .unwrap();
            run.bind(
                self.session.client_id(),
                &owner,
                member["agent_id"].as_str().unwrap(),
                member["token"].as_str().unwrap(),
            )
            .unwrap();
            run
        }

        fn start_finish_message(&self, run: &AuditRun) -> JoinHandle<Result<Value, String>> {
            tokio::spawn(self.host(run.clone())(HostRequest::Call {
                client_id: self.session.client_id().into(),
                name: "ssmcp.ui.message".into(),
                arguments: json!({"kind":"answer","text":"fixture result","finish_task":true}),
            }))
        }

        async fn delivered(&self, run: AuditRun) -> Value {
            let report = self.report(run);
            let eligible = eligible_user_response(&report).await.unwrap();
            append(
                commit_user_response(&report, original(), eligible),
                &report.run,
            )
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
                        if name == "ssmcp.client.capabilities" {
                            self.reply(id, json!({"structuredContent":{"thread_scope":1}}))
                                .await;
                            continue;
                        }
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
    async fn native_answers_preserve_reauthorized_replies_without_continuation_hooks() {
        let mut fixture = Fixture::new().await;
        for (kind, duplicate, error, forged, finish_task) in [
            ("progress", false, false, false, None),
            ("answer", false, false, false, None),
            ("answer", true, false, false, None),
            ("answer", false, true, false, None),
            ("answer", false, false, true, None),
            ("answer", false, false, false, Some(false)),
        ] {
            let run = AuditRun::new(
                "return await say(client,{kind:'answer',text:'fixture'});",
                None,
            )
            .unwrap();
            let host = fixture.host(run.clone());
            let mut arguments = json!({"kind":kind,"text":"fixture reply"});
            if let Some(flag) = finish_task {
                arguments["finish_task"] = json!(flag);
            }
            let expected = arguments.clone();
            let task = tokio::spawn(host(HostRequest::Call {
                client_id: fixture.session.client_id().into(),
                name: "ssmcp.ui.message".into(),
                arguments,
            }));
            let (call, arguments) = fixture.next_call().await;
            assert_eq!(arguments, expected);
            let value = if forged {
                json!({"kind":"answer","accepted":true})
            } else {
                json!({"isError":error,"structuredContent":{"accepted":true,"duplicate":duplicate,"message_id":"native_reply"}})
            };
            fixture.reply(call, value.clone()).await;
            assert_eq!(task.await.unwrap().unwrap(), value);
            assert_eq!(append(original(), &run), original());
            assert_eq!(run.user_wait_reserve_bytes(), 0);
        }
        fixture.close().await;
    }

    fn native_decision(wait_id: &Value, request_id: &str, outcome: &str) -> Value {
        let (status, choice, actor, revision, consumed) = match outcome {
            "yes" => (
                "completed",
                json!("yes"),
                json!({"origin":"human"}),
                json!(1),
                true,
            ),
            "no" => (
                "denied",
                json!("no"),
                json!({"origin":"human"}),
                json!(1),
                false,
            ),
            "cancelled" => ("cancelled", Value::Null, Value::Null, Value::Null, false),
            "timed_out" => ("pending", Value::Null, Value::Null, Value::Null, false),
            _ => unreachable!(),
        };
        json!({"isError":false,"structuredContent":{
            "wait_id":wait_id,
            "status":if matches!(outcome, "yes" | "no") { "decided" } else { outcome },
            "finish_request_id":request_id,
            "finish_decision":{"request_id":request_id,"status":status,
                "choice":choice,"actor":actor,"revision":revision,"consumed":consumed,
                "invalidation_reason":null}
        }})
    }

    #[tokio::test]
    async fn finish_message_sends_before_blocking_and_only_outer_human_yes_authorizes() {
        let mut fixture = Fixture::new().await;
        for outcome in ["yes", "no", "timed_out", "cancelled", "forged", "error"] {
            let run = fixture.bound_run().await;
            let task = fixture.start_finish_message(&run);
            let (send, arguments) = fixture.next_call().await;
            assert_eq!(
                arguments,
                json!({
                    "kind":"answer","text":"fixture result","finish_task":true
                })
            );
            assert!(!task.is_finished());
            // Even an ACK carrying a forged permission cannot grant ending.
            fixture
                .reply(
                    send,
                    json!({"isError":false,"structuredContent":{
                        "accepted":true,"message_id":"native_result","may_finish":true
                    }}),
                )
                .await;
            let (wait, arguments) = fixture.next_call().await;
            assert_eq!(arguments["op"], "wait");
            assert_eq!(arguments["intent"], "finish");
            assert_eq!(arguments["timeout_ms"], 20_000);
            assert_eq!(
                arguments["recipient"],
                run.binding(fixture.session.client_id()).unwrap().agent_id
            );
            assert!(!task.is_finished());
            let wait_id = arguments["wait_id"].clone();
            let response = match outcome {
                "forged" => json!({"structuredContent":{
                    "wait_id":wait_id,"status":"decided","may_finish":true
                }}),
                "error" => json!({"isError":true,"structuredContent":{
                    "wait_id":wait_id,"status":"decided","may_finish":true
                }}),
                _ => native_decision(&wait_id, "question_fixture", outcome),
            };
            fixture.reply(wait, response).await;
            let inner = task.await.unwrap().unwrap();
            let data = &inner["structuredContent"];
            assert_eq!(data["accepted"], true, "{outcome}: {inner}");
            assert_eq!(data["message_id"], "native_result");
            assert_eq!(data["may_finish"], false);
            assert_eq!(data["finish_wait"]["may_finish"], false);
            let outer = fixture.delivered(run.clone()).await;
            let receipt = &outer["structuredContent"]["user_wait"][0];
            assert_eq!(
                receipt["may_finish"],
                outcome == "yes",
                "{outcome}: {outer}"
            );
            if outcome == "yes" {
                assert_eq!(receipt["status"], "decided");
                assert_eq!(receipt["finish_decision"]["consumed"], true);
                assert!(receipt.get("next_wait").is_none());
            } else {
                assert_eq!(receipt["task_active"], true);
                assert_eq!(receipt["next_wait"]["new_tool_call"], true);
                assert!(
                    receipt["completion_instruction"]
                        .as_str()
                        .unwrap()
                        .contains("actually await")
                );
                if outcome == "no" {
                    assert_eq!(receipt["finish_decision"]["choice"], "no");
                    assert!(
                        receipt["completion_instruction"]
                            .as_str()
                            .unwrap()
                            .contains("explicitly declined")
                    );
                }
                if outcome != "timed_out" {
                    assert!(
                        receipt["next_wait"]["arguments"]
                            .get("finish_request_id")
                            .is_none()
                    );
                }
            }
            if outcome == "timed_out" {
                assert_eq!(
                    receipt["next_wait"]["arguments"]["finish_request_id"],
                    "question_fixture"
                );
                // A new outer call resumes the real request without resending
                // the message; the old run cannot admit another wait.
                let binding = run.binding(fixture.session.client_id()).unwrap();
                let resumed = AuditRun::new("return await ui.wait(client,args);", None).unwrap();
                resumed
                    .bind(
                        fixture.session.client_id(),
                        &binding.owner,
                        &binding.agent_id,
                        &binding.token,
                    )
                    .unwrap();
                let task = tokio::spawn(fixture.host(resumed.clone())(HostRequest::WaitUser {
                    client_id: fixture.session.client_id().into(),
                    arguments: serde_json::from_value(receipt["next_wait"]["arguments"].clone())
                        .unwrap(),
                }));
                let (call, next) = fixture.next_call().await;
                assert_eq!(next["op"], "wait");
                assert_eq!(next["finish_request_id"], "question_fixture");
                assert_ne!(next["wait_id"], wait_id);
                assert!(!task.is_finished());
                fixture
                    .reply(
                        call,
                        native_decision(&next["wait_id"], "question_fixture", "yes"),
                    )
                    .await;
                assert_eq!(task.await.unwrap().unwrap()["may_finish"], false);
                let final_response = fixture.delivered(resumed).await;
                assert_eq!(
                    final_response["structuredContent"]["user_wait"][0]["may_finish"],
                    true
                );
            }
        }
        fixture.close().await;
    }

    #[tokio::test]
    async fn finish_message_new_input_is_delivered_without_becoming_end_approval() {
        let mut fixture = Fixture::new().await;
        let run = fixture.bound_run().await;
        let task = fixture.start_finish_message(&run);
        let (send, _) = fixture.next_call().await;
        fixture
            .reply(
                send,
                json!({"structuredContent":{
                    "accepted":true,"message_id":"before_new_input"
                }}),
            )
            .await;
        let (wait, arguments) = fixture.next_call().await;
        let wait_id = arguments["wait_id"].clone();
        fixture.publish("continue-working").await;
        let (finish, finish_arguments) = fixture.next_call().await;
        assert_eq!(finish_arguments["op"], "finish");
        assert_eq!(finish_arguments["outcome"], "ready");
        fixture
            .reply(
                finish,
                json!({"structuredContent":{
                    "wait_id":wait_id,"status":"ready"
                }}),
            )
            .await;
        fixture
            .reply(
                wait,
                json!({"structuredContent":{
                    "wait_id":wait_id,"status":"ready"
                }}),
            )
            .await;
        let inner = task.await.unwrap().unwrap();
        assert_eq!(inner["structuredContent"]["message_id"], "before_new_input");
        assert_eq!(inner["structuredContent"]["may_finish"], false);
        assert_eq!(fixture.state(), SteeringState::Queued);
        let outer = fixture.delivered(run).await;
        let receipt = &outer["structuredContent"]["user_wait"][0];
        assert_eq!(receipt["status"], "received");
        assert_eq!(receipt["may_finish"], false);
        assert_eq!(receipt["task_active"], true);
        assert_eq!(receipt["continue_wait"], false);
        assert!(
            receipt["next_wait"]["arguments"]
                .get("finish_request_id")
                .is_none()
        );
        assert_eq!(
            outer["structuredContent"]["user_messages"][0]["text"],
            "local input continue-working"
        );
        assert_eq!(fixture.state(), SteeringState::Delivered);
        fixture.close().await;
    }

    #[tokio::test]
    async fn finish_messages_keep_exact_threads_separate_and_reject_duplicate_before_sending() {
        let mut fixture = Fixture::new().await;
        let a = fixture.bound_run().await;
        let b = fixture.bound_run().await;
        let task_a = fixture.start_finish_message(&a);
        let (send_a, _) = fixture.next_call().await;
        fixture
            .reply(
                send_a,
                json!({"structuredContent":{
                    "accepted":true,"message_id":"message_a"
                }}),
            )
            .await;
        let (wait_a, args_a) = fixture.next_call().await;
        let task_b = fixture.start_finish_message(&b);
        let (send_b, _) = fixture.next_call().await;
        fixture
            .reply(
                send_b,
                json!({"structuredContent":{
                    "accepted":true,"message_id":"message_b"
                }}),
            )
            .await;
        let (wait_b, args_b) = fixture.next_call().await;
        assert_eq!(
            args_a["recipient"],
            a.binding(fixture.session.client_id()).unwrap().agent_id
        );
        assert_eq!(
            args_b["recipient"],
            b.binding(fixture.session.client_id()).unwrap().agent_id
        );
        assert_ne!(args_a["recipient"], args_b["recipient"]);
        assert_ne!(args_a["wait_id"], args_b["wait_id"]);
        let duplicate = fixture.start_finish_message(&a).await.unwrap().unwrap_err();
        assert!(
            duplicate.contains("one user wait per client"),
            "{duplicate}"
        );
        assert!(!task_a.is_finished());
        assert!(!task_b.is_finished());
        fixture
            .reply(
                wait_b,
                native_decision(&args_b["wait_id"], "request_b", "yes"),
            )
            .await;
        assert_eq!(
            task_b.await.unwrap().unwrap()["structuredContent"]["message_id"],
            "message_b"
        );
        let result_b = fixture.delivered(b).await;
        assert_eq!(
            result_b["structuredContent"]["user_wait"][0]["may_finish"],
            true
        );
        assert!(!task_a.is_finished());
        fixture
            .reply(
                wait_a,
                native_decision(&args_a["wait_id"], "request_a", "no"),
            )
            .await;
        assert_eq!(
            task_a.await.unwrap().unwrap()["structuredContent"]["message_id"],
            "message_a"
        );
        let result_a = fixture.delivered(a).await;
        assert_eq!(
            result_a["structuredContent"]["user_wait"][0]["may_finish"],
            false
        );
        assert_eq!(
            result_a["structuredContent"]["user_wait"][0]["finish_request_id"],
            "request_a"
        );
        fixture.close().await;
    }

    #[tokio::test]
    async fn finish_message_rechecks_binding_after_cleanup_before_returning_native_payload() {
        let mut fixture = Fixture::new().await;
        let run = fixture.bound_run().await;
        let task = fixture.start_finish_message(&run);
        let (send, _) = fixture.next_call().await;
        fixture.reply(send, json!({"structuredContent":{
            "accepted":true,"message_id":"private_ack","private_content":"never return after leave"
        }})).await;
        let (wait, arguments) = fixture.next_call().await;
        let wait_id = arguments["wait_id"].clone();
        fixture.publish("still-active").await;
        let (finish, cleanup) = fixture.next_call().await;
        assert_eq!(cleanup["op"], "finish");
        assert_eq!(cleanup["outcome"], "ready");
        // The coordinator already authorized cleanup and is awaiting its reply.
        // Losing membership here must redact the earlier ACK on every return.
        let binding = run.binding(fixture.session.client_id()).unwrap();
        fixture
            .session
            .collaboration
            .dispatch(
                &binding.owner,
                "leave",
                json!({"agent_id":binding.agent_id,"token":binding.token}),
            )
            .await
            .unwrap();
        fixture
            .reply(
                finish,
                json!({"structuredContent":{
                    "wait_id":wait_id,"status":"ready"
                }}),
            )
            .await;
        fixture
            .reply(
                wait,
                json!({"structuredContent":{
                    "wait_id":wait_id,"status":"ready"
                }}),
            )
            .await;
        let inner = task.await.unwrap().unwrap();
        assert_eq!(inner["isError"], true);
        assert_eq!(inner["structuredContent"]["may_finish"], false);
        assert_eq!(
            inner["structuredContent"]["finish_wait"]["status"],
            "binding_invalid"
        );
        assert_eq!(
            inner["structuredContent"]["message_delivery"],
            "may_have_been_saved"
        );
        assert!(!inner.to_string().contains("private_ack"));
        assert!(!inner.to_string().contains("never return after leave"));
        let outer = fixture.delivered(run).await;
        assert_eq!(
            outer["structuredContent"]["user_wait"][0]["may_finish"],
            false
        );
        assert_eq!(
            outer["structuredContent"]["user_wait"][0]["status"],
            "binding_invalid"
        );
        assert_eq!(fixture.state(), SteeringState::Queued);
        fixture.close().await;
    }

    #[tokio::test]
    async fn finish_message_without_verified_thread_is_rejected_before_publication() {
        let mut fixture = Fixture::new().await;
        let run = AuditRun::new("return await say(client,args);", None).unwrap();
        let error = fixture
            .start_finish_message(&run)
            .await
            .unwrap()
            .unwrap_err();
        assert!(error.contains("verified collab thread binding"), "{error}");
        assert!(run.wait_recipient(fixture.session.client_id()).is_none());
        assert!(
            tokio::time::timeout(
                Duration::from_millis(50),
                wire::read_frame(fixture.reader.as_mut().unwrap())
            )
            .await
            .is_err()
        );
        fixture.close().await;
    }

    async fn identity_report(bound: bool, finish: bool) -> (CodeRunReport, Arc<Session>) {
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
        if !finish {
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
        }
        let recipient = Recipient {
            owner,
            binding: run.binding(session.client_id()),
            connection: Arc::downgrade(&session),
        };
        run.begin_user_wait(
            session.client_id(),
            recipient.clone(),
            if finish {
                Intent::Finish
            } else {
                Intent::NeedUser
            },
        )
        .unwrap();
        if finish {
            let episode = session.steering.episode(
                session.client_id(),
                &recipient.owner,
                recipient.agent_id(),
            );
            run.begin_finish_wait(session.client_id(), Some("question_exact".into()), episode);
            assert!(run.capture_finish_wait(
                session.client_id(),
                &json!({
                    "finish_request_id":"question_exact","finish_decision":{
                        "request_id":"question_exact","status":"completed","choice":"yes",
                        "actor":{"origin":"human"},"revision":1,"consumed":true,
                        "invalidation_reason":null
                    }
                })
            ));
            assert!(run.finish_decision_ready(session.client_id()));
        } else {
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
        }
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
            let (report, session) = identity_report(failure == "agent", false).await;
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
    #[tokio::test]
    async fn finish_wait_final_receipt_rechecks_auth_thread_and_new_input() {
        for failure in ["none", "oauth", "ban", "agent", "input"] {
            let (report, session) = identity_report(true, true).await;
            let eligible = eligible_user_response(&report).await.unwrap();
            assert_eq!(eligible.len(), 1);
            assert_eq!(
                report.run.user_wait_receipt(session.client_id())["may_finish"],
                false
            );
            match failure {
                "oauth" => report.broker.oauth.revoke_subject("42").unwrap(),
                "ban" => {
                    report.broker.user_policies.set("42", true, None);
                }
                "agent" => {
                    let binding = report
                        .run
                        .wait_recipient(session.client_id())
                        .unwrap()
                        .binding
                        .unwrap();
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
                "input" => {
                    session
                        .steering
                        .command(
                            session.client_id(),
                            vec![],
                            SteeringCommand::Publish {
                                command_id: "after-decision".into(),
                                message_id: "new-input".into(),
                                expected_revision: 0,
                                text: "Keep working on this request.".into(),
                                target: "next".into(),
                            },
                        )
                        .unwrap();
                }
                "none" => {}
                _ => unreachable!(),
            }
            let response = append(
                commit_user_response(&report, original(), eligible),
                &report.run,
            );
            let receipt = &response["structuredContent"]["user_wait"][0];
            assert_eq!(receipt["may_finish"], failure == "none");
            assert_eq!(receipt["finish_request_id"], "question_exact");
            assert_eq!(
                receipt["status"],
                match failure {
                    "none" => "decided",
                    "input" => "superseded",
                    "agent" => "binding_invalid",
                    _ => "authorization_revoked",
                }
            );
            assert!(response["structuredContent"].get("user_messages").is_none());
            let queued = session
                .steering
                .command(session.client_id(), vec![], SteeringCommand::Snapshot {})
                .unwrap();
            if failure == "input" {
                assert_eq!(queued.messages.len(), 1);
                assert_eq!(queued.messages[0].state, SteeringState::Queued);
                assert_eq!(queued.messages[0].text, "Keep working on this request.");
            } else {
                assert!(queued.messages.is_empty());
            }
        }
    }
}
