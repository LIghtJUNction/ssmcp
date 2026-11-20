//! First-class completion requests wait in Rust and do not acquire JS slots.
use super::*;
use crate::code_mode::user_wait::Recipient;
use tokio::time::{Instant, MissedTickBehavior};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    client_id: String,
    reason: String,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default = "default_timeout")]
    timeout_ms: u64,
    #[serde(default)]
    agent: Option<Agent>,
    #[serde(skip)]
    pub(super) controller: Option<Controller>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Agent {
    id: String,
    token: String,
}
fn default_timeout() -> u64 {
    20_000
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

impl Arguments {
    pub(super) fn parse(value: Value) -> Result<Self, String> {
        let arguments: Self = serde_json::from_value(value).map_err(|_|
            "request_finish requires client_id and reason; optional request_id/idempotency_key, timeout_ms, and agent {id,token}; no approval or actor fields")?;
        if arguments.client_id.is_empty() || arguments.client_id.len() > 64 {
            return Err("client_id must come from list".into());
        }
        if arguments.reason.trim().is_empty()
            || arguments.reason.len() > 8192
            || arguments
                .reason
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        {
            return Err(
                "reason must contain 1..=8192 UTF-8 bytes without terminal controls".into(),
            );
        }
        if !(1..=25_000).contains(&arguments.timeout_ms) {
            return Err("timeout_ms must be 1..=25000".into());
        }
        if arguments
            .request_id
            .as_deref()
            .is_some_and(|id| !identifier(id))
            || arguments
                .idempotency_key
                .as_deref()
                .is_some_and(|id| !identifier(id))
            || (arguments.request_id.is_some() && arguments.idempotency_key.is_some())
        {
            return Err(
                "use one valid request_id to resume, or one idempotency_key to create".into(),
            );
        }
        if arguments.agent.as_ref().is_some_and(|agent| {
            !identifier(&agent.id)
                || agent.token.len() != 64
                || !agent.token.bytes().all(|byte| byte.is_ascii_hexdigit())
        }) {
            return Err(
                "agent credentials must be the id and private token returned by collab.join".into(),
            );
        }
        Ok(arguments)
    }
}

struct CancelGuard {
    broker: Arc<Broker>,
    user: String,
    token: String,
    session: Arc<Session>,
    run: code_mode::AuditRun,
    cancel: Value,
    armed: bool,
}
impl Drop for CancelGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (broker, user, token, session, run, cancel) = (
            self.broker.clone(),
            self.user.clone(),
            self.token.clone(),
            self.session.clone(),
            self.run.clone(),
            self.cancel.clone(),
        );
        runtime.spawn(async move {
            // Bounded best effort only. Revoked credentials never gain a cleanup
            // bypass; the next completion still requires live ACL and binding.
            let _ = tokio::time::timeout(Duration::from_secs(1), async {
                let Some(authorization) = broker.authenticate(&user, &token).await else {
                    return;
                };
                let Ok(current) = broker
                    .client(&user, &authorization, session.client_id())
                    .await
                else {
                    return;
                };
                if !Arc::ptr_eq(&current, &session) {
                    return;
                }
                let _ = code_mode::AUDIT_RUN
                    .scope(
                        run,
                        session.call_with_authorization(
                            Operation::CallTool {
                                name: "ssmcp.ui.finish".into(),
                                arguments: cancel,
                            },
                            None,
                            &authorization,
                        ),
                    )
                    .await;
            })
            .await;
        });
    }
}

fn final_authorization(
    broker: &Broker,
    user: &str,
    token: &str,
    session: &Session,
    recipient: &Recipient,
) -> Result<(), String> {
    let authorization = broker
        .authenticate_now(user, token)
        .ok_or("finish_authorization_revoked: approval withheld")?;
    if !session.is_alive()
        || !authorization.is_live()
        || !authorization.permits(session)
        || crate::bridge::owner(user, &authorization).as_deref() != Ok(recipient.owner.as_str())
    {
        return Err("finish_scope_expired: approval withheld".into());
    }
    let binding = recipient.binding.as_ref().ok_or("finish_thread_required")?;
    if binding.owner != recipient.owner
        || session
            .collaboration
            .bind(&binding.owner, &binding.agent_id, &binding.token)
            .is_err()
    {
        return Err("finish_binding_invalid: approval withheld".into());
    }
    Ok(())
}

pub(super) async fn execute(
    broker: Arc<Broker>,
    user: String,
    access_token: String,
    arguments: Arguments,
) -> Result<Value, String> {
    let authorization = broker
        .authenticate(&user, &access_token)
        .await
        .ok_or("authorization expired or revoked")?;
    if !matches!(authorization, Authorization::Identity(_)) {
        return Err(
            "finish_account_required: use an authenticated account and verified collab thread"
                .into(),
        );
    }
    let session = broker
        .client(&user, &authorization, &arguments.client_id)
        .await?;
    let owner = crate::bridge::owner(&user, &authorization)?;
    let inherited = code_mode::AUDIT_RUN
        .try_with(|run| run.binding(&arguments.client_id))
        .ok()
        .flatten();
    let binding = match arguments.agent {
        Some(agent) => code_mode::RunBinding { owner:owner.clone(),agent_id:agent.id,token:agent.token },
        None => inherited.ok_or("finish_thread_required: call collab.join for this client, then pass its {id,token}; this registers an existing participant and starts no model; display names are not identity")?,
    };
    if binding.owner != owner {
        return Err("finish_account_mismatch".into());
    }
    session
        .collaboration
        .bind(&owner, &binding.agent_id, &binding.token)?;
    let run = code_mode::AuditRun::new(
        "request_finish: explicit completion decision",
        arguments.controller.clone(),
    )?;
    run.record_connection(&session)?;
    run.bind(
        &arguments.client_id,
        &owner,
        &binding.agent_id,
        &binding.token,
    )?;
    let recipient = Recipient {
        owner,
        binding: Some(binding.clone()),
        connection: Arc::downgrade(&session),
    };
    let _permit = session
        .steering
        .wait_permit()
        .map_err(|_| "finish_busy: four waits are already active on this client")?;
    let mut report = CodeRunReport {
        broker: broker.clone(),
        user: user.clone(),
        access_token: access_token.clone(),
        run: run.clone(),
        delivered: false,
    };
    let mut native = json!({"op":"request","reason":arguments.reason,"timeout_ms":arguments.timeout_ms.saturating_sub(250).max(1)});
    let mut cancel = json!({"op":"cancel"});
    if let Some(id) = arguments.request_id {
        native["request_id"] = json!(id);
        cancel["request_id"] = native["request_id"].clone();
    } else {
        let key = arguments
            .idempotency_key
            .unwrap_or_else(|| format!("finish_{}", run.context.run_id));
        native["idempotency_key"] = json!(key);
        cancel["idempotency_key"] = native["idempotency_key"].clone();
    }
    let mut guard = CancelGuard {
        broker: broker.clone(),
        user: user.clone(),
        token: access_token.clone(),
        session: session.clone(),
        run: run.clone(),
        cancel,
        armed: true,
    };
    let operation = Operation::CallTool {
        name: "ssmcp.ui.finish".into(),
        arguments: native,
    };
    let waiting = code_mode::AUDIT_RUN.scope(
        run.clone(),
        session.call_with_authorization(operation, arguments.controller, &authorization),
    );
    tokio::pin!(waiting);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let deadline = Instant::now() + Duration::from_millis(arguments.timeout_ms);
    let result = loop {
        tokio::select! {
            biased;
            result = &mut waiting => break result,
            _ = tokio::time::sleep_until(deadline) => break Err("finish_native_wait_timed_out: resume only after inspecting the original request".into()),
            _ = heartbeat.tick() => {
                if user_wait::authorized(&broker, &user, &access_token, &arguments.client_id, &session, &recipient).await.is_err() {
                    break Err("finish_authorization_or_binding_revoked: no approval granted".into());
                }
            }
        }
    };
    report.finish(&result).await;
    let native = result?;
    final_authorization(&broker, &user, &access_token, &session, &recipient)?;
    if native["isError"] == true {
        return Ok(native);
    }
    let data = native
        .get("structuredContent")
        .filter(|data| {
            data["request_id"].is_string()
                && data["default_decision"] == "no"
                && data["request"]["requester"]["thread_id"] == binding.agent_id
        })
        .ok_or("finish_response_invalid: no approval accepted")?
        .clone();
    run.capture_question_answers(&session, Some(&binding.agent_id), &native);
    collect_question_answers(&report).await;
    let eligible = eligible_user_response(&report)
        .await
        .map_err(|_| "finish_authorization_revoked: approval withheld")?;
    // No await after this final decision, matching the common response boundary.
    final_authorization(&broker, &user, &access_token, &session, &recipient)?;
    let response = commit_user_response(&report, tool_result(Ok(data)), eligible);
    guard.armed = false;
    Ok(response)
}

pub(super) fn tool() -> Value {
    json!({"name":"request_finish",
        "description":"Request permission to finish the current work. A nonempty reason is required. This direct Rust tool sleeps up to 25 seconds without occupying a JavaScript worker. Default No is not an invented user decision: pending returns decision null and approved false; resume the same request_id after timeout. The local human or another verified agent in this same client/account may decide; the requester cannot approve itself. Pass your existing collab id/token to bind the exact thread (join starts no model). An approved request permits one explicit native kind answer; new user input, cancellation or connection changes invalidate it. Current authentication and ACL remain required. External assistant final speech cannot be intercepted by SSMCP.",
        "inputSchema":{"type":"object","additionalProperties":false,"required":["client_id","reason"],"properties":{
            "client_id":{"type":"string","minLength":1,"maxLength":64},
            "reason":{"type":"string","minLength":1,"maxLength":8192},
            "request_id":{"type":"string","minLength":1,"maxLength":128},
            "idempotency_key":{"type":"string","minLength":1,"maxLength":128},
            "timeout_ms":{"type":"integer","minimum":1,"maximum":25000,"default":20000},
            "agent":{"type":"object","additionalProperties":false,"required":["id","token"],"properties":{
                "id":{"type":"string","minLength":1,"maxLength":64},"token":{"type":"string","minLength":64,"maxLength":64}}}},
            "not":{"required":["request_id","idempotency_key"]}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"openWorldHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reasoned_finish_requires_bounded_arguments_and_rejects_forged_consent() {
        let valid = json!({"client_id":"one","reason":"Implementation and targeted checks are complete","timeout_ms":1});
        assert!(Arguments::parse(valid.clone()).is_ok());
        for (name, value) in [
            ("approved", json!(true)),
            ("decision", json!("yes")),
            ("actor", json!("human")),
            ("timeout_ms", json!(25001)),
            ("reason", json!(" ")),
        ] {
            let mut request = valid.clone();
            request[name] = value;
            assert!(Arguments::parse(request).is_err());
        }
        let mut both = valid;
        both["request_id"] = json!("old");
        both["idempotency_key"] = json!("new");
        assert!(Arguments::parse(both).is_err());
    }
}
