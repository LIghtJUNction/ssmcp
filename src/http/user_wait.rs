//! Transport-independent user-message wait coordinator. The MCP host may run
//! locally; no remote URL, OS notification helper, or model is selected here.
use super::*;
use crate::code_mode::user_wait::{Arguments, Outcome, Recipient};
use tokio::time::{Instant, MissedTickBehavior};

async fn authorized(
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
        if let Some(data) = result
            .as_ref()
            .and_then(|result| native_data(result, &wait_id))
        {
            run.wait_notification(&client_id, &data);
            if outcome == Outcome::Ready && data["status"] == "cancelled" {
                run.finish_wait_state(
                    &client_id,
                    Outcome::Cancelled,
                    "local_cancel_won_before_delivery",
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
