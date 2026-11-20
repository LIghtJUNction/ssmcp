//! Bearer-account ingress for exact chat I/O recipients; no agent is impersonated.
use super::*;
use crate::broker::session_io::{OutputQuery, Recipient, Target};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum IoRequest {
    Send {
        request_id: String,
        recipients: Vec<Recipient>,
        text: String,
    },
    Outputs {
        client_id: String,
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        main: bool,
        #[serde(default)]
        cursor: u64,
        #[serde(default)]
        generation: Option<String>,
        #[serde(default = "page_limit")]
        limit: usize,
    },
    Poll {
        request_id: String,
        #[serde(default)]
        after: u64,
        #[serde(default = "page_limit")]
        limit: usize,
        #[serde(default)]
        wait_ms: u64,
    },
}
fn page_limit() -> usize {
    64
}
fn rejected(message: String) -> ApiError {
    let code = if message.contains("io_reply_unsupported") {
        "io_reply_unsupported"
    } else if message.contains("already identifies") {
        "io_request_conflict"
    } else if message.contains("full") || message.contains("capacity") {
        "io_capacity"
    } else if message.contains("unavailable")
        || message.contains("expired")
        || message.contains("no longer")
        || message.contains("access changed")
    {
        "io_unavailable"
    } else {
        "io_invalid_request"
    };
    fail(StatusCode::CONFLICT, code)
}
pub(super) async fn request(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let registration = authenticate(&broker, request.headers(), true).await?;
    let input: IoRequest = body(request, &broker).await?;
    let actor = registration.identity.clone();
    let auth = Authorization::Identity(actor.clone());
    let mut result = match input {
        IoRequest::Outputs {
            client_id,
            thread_id,
            main,
            cursor,
            generation,
            limit,
        } => {
            let session = broker
                .client(IDENTITY_RESOURCE, &auth, &client_id)
                .await
                .map_err(|_| fail(StatusCode::NOT_FOUND, "io_unavailable"))?;
            if !registration.is_live(&broker).await {
                return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
            }
            broker
                .session_io
                .outputs(
                    &broker,
                    &session,
                    &actor,
                    OutputQuery {
                        thread_id: thread_id.as_deref(),
                        main,
                        cursor,
                        generation: generation.as_deref(),
                        limit,
                    },
                )
                .map_err(rejected)?
        }
        IoRequest::Send {
            request_id,
            recipients,
            text,
        } => {
            if recipients.is_empty() || recipients.len() > 32 {
                return Err(fail(StatusCode::BAD_REQUEST, "io_invalid_request"));
            }
            let sessions = broker
                .clients(IDENTITY_RESOURCE, &auth)
                .await
                .map_err(|_| fail(StatusCode::FORBIDDEN, "access_denied"))?;
            let mut targets = Vec::with_capacity(recipients.len());
            let mut checked = std::collections::HashSet::new();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            for recipient in recipients {
                let session = sessions
                    .iter()
                    .find(|s| s.client_id() == recipient.client_id)
                    .cloned()
                    .ok_or_else(|| fail(StatusCode::NOT_FOUND, "io_unavailable"))?;
                if checked.insert(session.client_id().to_owned()) && !session.io_reply_advertised()
                {
                    tokio::select! {
                        result=tokio::time::timeout_at(deadline,advertise(&session,&auth))=>result.map_err(|_|fail(StatusCode::GATEWAY_TIMEOUT,"io_capability_unavailable"))??,
                        _=registration.wait_revoked(&broker)=>return Err(fail(StatusCode::UNAUTHORIZED,"invalid_token")),
                    }
                }
                targets
                    .push(Target::resolve(&broker, session, &actor, recipient).map_err(rejected)?);
            }
            // All awaits precede this final authorization and atomic enqueue.
            if !registration.is_live(&broker).await {
                return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
            }
            broker
                .session_io
                .enqueue(&broker, &actor, &request_id, targets, &text)
                .map_err(rejected)?
        }
        IoRequest::Poll {
            request_id,
            after,
            limit,
            wait_ms,
        } => {
            if wait_ms > 1000 || !(1..=64).contains(&limit) {
                return Err(fail(StatusCode::BAD_REQUEST, "io_invalid_request"));
            }
            if !registration.is_live(&broker).await {
                return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
            }
            let mut page = broker
                .session_io
                .poll(&broker, &actor, &request_id, after, limit)
                .map_err(rejected)?;
            let pending = page["recipients"].as_array().is_some_and(|entries| {
                entries
                    .iter()
                    .any(|e| matches!(e["state"].as_str(), Some("queued" | "included")))
            });
            if wait_ms > 0 && pending && page["outputs"].as_array().is_some_and(Vec::is_empty) {
                tokio::select! {
                    _=broker.session_io.wait(Duration::from_millis(wait_ms))=>{},
                    _=registration.wait_revoked(&broker)=>return Err(fail(StatusCode::UNAUTHORIZED,"invalid_token")),
                }
                if !registration.is_live(&broker).await {
                    return Err(fail(StatusCode::UNAUTHORIZED, "invalid_token"));
                }
                page = broker
                    .session_io
                    .poll(&broker, &actor, &request_id, after, limit)
                    .map_err(rejected)?;
            }
            page
        }
    };
    // No queued input is consumed by polling. This is an account-owned receipt.
    result["account"] = json!({"provider":actor.provider,"subject":actor.subject});
    Ok(Json(result))
}

pub(super) fn directory(
    broker: &Broker,
    session: &Arc<crate::broker::Session>,
    actor: &crate::identity::Identity,
) -> Value {
    crate::broker::session_io::process_directory(broker, session, actor)
}

async fn advertise(
    session: &Arc<crate::broker::Session>,
    authorization: &Authorization,
) -> Result<(), ApiError> {
    let discovery = async {
        let mut cursor = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..32 {
            let page = session
                .call_with_authorization(Operation::ListTools { cursor }, None, authorization)
                .await
                .map_err(|_| fail(StatusCode::BAD_GATEWAY, "io_capability_unavailable"))?;
            if session.io_reply_advertised() {
                return Ok(());
            }
            cursor = match page.get("nextCursor") {
                None | Some(Value::Null) => None,
                Some(Value::String(value))
                    if !value.is_empty() && value.len() <= 128 && seen.insert(value.clone()) =>
                {
                    Some(value.clone())
                }
                _ => return Err(fail(StatusCode::BAD_GATEWAY, "io_capability_unavailable")),
            };
            if cursor.is_none() {
                break;
            }
        }
        Err(fail(StatusCode::CONFLICT, "io_reply_unsupported"))
    };
    tokio::time::timeout(Duration::from_secs(3), discovery)
        .await
        .map_err(|_| fail(StatusCode::GATEWAY_TIMEOUT, "io_capability_unavailable"))?
}
