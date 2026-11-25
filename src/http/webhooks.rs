//! Accept external notifications only for a live mailbox owned by the bearer identity.
use super::{
    Authorization, Broker, Json, Request, Response, State, StatusCode, bearer, valid_origin,
};
use axum::{extract::Query, response::IntoResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Destination {
    recipient_id: String,
    resource: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Payload {
    schema_version: u32,
    id: String,
    event: String,
    text: String,
    data: Option<Value>,
    #[serde(default, rename = "created_at_ms")]
    _created_at_ms: Option<u64>,
}

pub(super) async fn receive(
    State(broker): State<Arc<Broker>>,
    Query(destination): Query<Destination>,
    request: Request,
) -> Response {
    let headers = request.headers();
    if !valid_origin(headers, &broker.public_url) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let user = destination
        .resource
        .as_deref()
        .unwrap_or(crate::broker::IDENTITY_RESOURCE);
    let Some(token) = bearer(headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    match broker.authenticate_now(user, token) {
        Some(Authorization::Identity(_)) => {}
        Some(_) => return StatusCode::FORBIDDEN.into_response(),
        None => return StatusCode::UNAUTHORIZED.into_response(),
    }
    let token = token.to_owned();
    if !super::single_header(headers, "content-type")
        .ok()
        .flatten()
        .is_some_and(|v| {
            v.split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    if crate::wire::messages::parse_recipient(&destination.recipient_id).is_err() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let bytes = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        axum::body::to_bytes(request.into_body(), 64 * 1024),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
        Err(_) => return StatusCode::REQUEST_TIMEOUT.into_response(),
    };
    let Ok(payload) = serde_json::from_slice::<Payload>(&bytes) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if payload.schema_version != 1
        || crate::wire::messages::validate_request_id(&payload.id).is_err()
        || crate::wire::messages::validate_request_id(&payload.event).is_err()
        || payload.text.trim().is_empty()
        || payload.text.contains('\0')
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    // The queue owns receipt time. Sender timestamps can differ on a retry.
    let text = json!({"event":payload.event,"text":payload.text,"data":payload.data}).to_string();
    if text.len() > crate::wire::MAX_USER_MESSAGE_BYTES {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    match broker
        .queue_webhook(user, &token, &destination.recipient_id, text, payload.id)
        .await
    {
        Ok(receipt) => {
            let status = match receipt.status {
                crate::wire::messages::Status::Expired => StatusCode::GONE,
                _ => StatusCode::ACCEPTED,
            };
            (status, Json(receipt)).into_response()
        }
        Err(_) => (
            StatusCode::CONFLICT,
            "webhook could not be queued for the selected mailbox",
        )
            .into_response(),
    }
}
