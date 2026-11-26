//! The local frontend projects the existing request queue. It cannot manufacture
//! an agent, retarget an earlier request, or replace another frontend's draft.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const RECEIPT_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_RECEIPTS: usize = 256;

struct Receipt {
    fingerprint: Vec<u8>,
    value: Value,
    created: Instant,
}

#[derive(Default)]
pub(super) struct Receipts {
    entries: Mutex<BTreeMap<(u64, u64, String), Receipt>>,
}

impl Receipts {
    fn admit(
        &self,
        backend: u64,
        connection: u64,
        args: &Send,
        submit: impl FnOnce() -> Result<Value, BridgeError>,
    ) -> Result<Value, BridgeError> {
        let key = (backend, connection, args.request_id.clone());
        let fingerprint = Sha256::digest(
            serde_json::to_vec(&json!({
                "text":args.text,"target":args.target,
            }))
            .expect("bounded chat arguments serialize"),
        )
        .to_vec();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        entries.retain(|_, value| value.created.elapsed() < RECEIPT_TTL);
        if let Some(receipt) = entries.get(&key) {
            if receipt.fingerprint != fingerprint {
                return Err(BridgeError::invalid(
                    "chat request_id was already used with different text or target",
                ));
            }
            let mut value = receipt.value.clone();
            value["duplicate"] = json!(true);
            return Ok(value);
        }
        if entries.len() >= MAX_RECEIPTS {
            return Err(BridgeError::new(
                ErrorCode::Unavailable,
                "chat receipt capacity reached; no new input was admitted",
            ));
        }
        // The actual queue admission and its receipt are synchronous. Cancellation
        // cannot land between them, and no network request is repeated here.
        let mut value = submit()?;
        value["request_id"] = json!(args.request_id);
        value["duplicate"] = json!(false);
        value["deduplication_ms"] = json!(RECEIPT_TTL.as_millis() as u64);
        entries.insert(
            key,
            Receipt {
                fingerprint,
                value: value.clone(),
                created: Instant::now(),
            },
        );
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Send {
    request_id: String,
    text: String,
    #[serde(default = "next_target")]
    target: String,
}
fn next_target() -> String {
    "next".into()
}

pub(super) fn command() -> Command {
    Command { id:id("chat.send"),
        description:"Submit explicit human text to this connection's existing request queue. A receipt confirms local admission, not model reading. Same request_id and contents return the retained receipt for 15 minutes; never automatically retry an unknown outcome.".into(),
        input_schema:json!({"type":"object","additionalProperties":false,
            "properties":{"request_id":{"type":"string","minLength":1,"maxLength":128},
            "text":{"type":"string","minLength":1,"maxLength":crate::wire::MAX_USER_MESSAGE_BYTES},
            "target":{"type":"string","minLength":1,"maxLength":128,"default":"next"}},
            "required":["request_id","text"]}) }
}

pub(super) fn send(
    host: &BoundHost<'_>,
    runtime: &Runtime,
    arguments: Value,
) -> Result<Value, BridgeError> {
    host.runtime()?;
    let client_id = runtime.frontend_client_id().ok_or_else(|| {
        BridgeError::new(
            ErrorCode::Unavailable,
            "this runtime has no registered chat connection",
        )
    })?;
    send_to(host, client_id, runtime.connection_generation(), arguments)
}

pub(super) fn send_to(
    host: &BoundHost<'_>,
    client_id: &str,
    generation: u64,
    arguments: Value,
) -> Result<Value, BridgeError> {
    let args: Send = serde_json::from_value(arguments)
        .map_err(|_| BridgeError::invalid("invalid chat.send arguments"))?;
    Id::new(&args.request_id)?;
    if args.text.trim().is_empty()
        || args.text.len() > crate::wire::MAX_USER_MESSAGE_BYTES
        || args.target.is_empty()
        || args.target.len() > 128
    {
        return Err(BridgeError::invalid(
            "chat text must be 1..16384 UTF-8 bytes with a valid target",
        ));
    }
    host.authorize(Access::Control)?;
    let value = host.capture.bridge.chat_receipts.admit(
        host.backend_generation,
        generation,
        &args,
        || {
            crate::client::steering::frontend_submit(client_id, &args.text, &args.target)
                .map_err(native)
        },
    )?;
    if value["duplicate"] == false {
        let scope = super::super::thread_history::RecordScope::frontend_input(
            client_id,
            &args.target,
            generation,
            &args.request_id,
        );
        host.capture.push_scoped(json!({"kind":"event","label":"user","value":crate::audit::event(json!({"type":"ssmcp.user_input",
            "text":args.text,"request_id":args.request_id,"message_id":value["message_id"],
            "target":args.target,"status":value["status"],"origin":"local_frontend"}))}), Some(scope));
    }
    Ok(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    #[serde(default)]
    offset: u16,
    #[serde(default = "default_worker_limit")]
    limit: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationQuery {
    cursor: Option<Counter>,
    #[serde(default = "conversation_limit")]
    limit: u16,
    thread_id: Option<String>,
    #[serde(default)]
    main: bool,
}
fn conversation_limit() -> u16 {
    32
}

pub(super) fn state(
    host: &BoundHost<'_>,
    resource: &str,
    query: Value,
) -> Result<Value, BridgeError> {
    let runtime = host.runtime()?;
    let client_id = runtime.frontend_client_id().ok_or_else(|| {
        BridgeError::new(
            ErrorCode::Unavailable,
            "this runtime has no registered chat connection",
        )
    })?;
    state_for(host, client_id, resource, query)
}

pub(super) fn state_for(
    host: &BoundHost<'_>,
    client_id: &str,
    resource: &str,
    query: Value,
) -> Result<Value, BridgeError> {
    if resource == "conversation" {
        let query: ConversationQuery = serde_json::from_value(query)
            .map_err(|_| BridgeError::invalid("invalid conversation query"))?;
        let selector = super::super::thread_history::Selector {
            client_id: client_id.into(),
            agent_id: query.thread_id,
            main: query.main,
        };
        let page = host
            .capture
            .thread_page(
                &selector,
                query.cursor.map(|cursor| cursor.0),
                Some(&host.capture.generation),
                usize::from(query.limit),
            )
            .map_err(BridgeError::invalid)?;
        return Ok(portable_page(page));
    }
    let query: PageQuery = serde_json::from_value(query)
        .map_err(|_| BridgeError::invalid("invalid chat state page query"))?;
    match resource {
        "threads" => crate::client::steering::frontend_threads(
            client_id,
            usize::from(query.offset),
            usize::from(query.limit),
        ),
        "chat.queue" => crate::client::steering::frontend_queue(
            client_id,
            usize::from(query.offset),
            usize::from(query.limit),
        ),
        _ => unreachable!("validated chat resource"),
    }
    .map_err(native)
}

fn portable_page(mut page: Value) -> Value {
    for key in ["next_cursor", "start_cursor", "end_cursor"] {
        if let Some(counter) = page[key].as_u64() {
            page[key] = json!(Counter(counter));
        }
    }
    page["gap"] = page["retention_gap_possible"].clone();
    if let Some(events) = page["events"].as_array_mut() {
        for event in events {
            if let Some(counter) = event["sequence"].as_u64() {
                event["sequence"] = json!(Counter(counter));
            }
            for key in ["connection", "request_id"] {
                if let Some(counter) = event["source"][key].as_u64() {
                    event["source"][key] = json!(Counter(counter));
                }
            }
        }
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn duplicate_chat_admission_returns_original_receipt_without_running_again() {
        let receipts = Receipts::default();
        let calls = Cell::new(0);
        let args = Send {
            request_id: "send-one".into(),
            text: "你好".into(),
            target: "agent_one".into(),
        };
        let first = receipts
            .admit(1, 2, &args, || {
                calls.set(calls.get() + 1);
                Ok(json!({"message_id":"real-one","status":"sending"}))
            })
            .unwrap();
        let duplicate = receipts
            .admit(1, 2, &args, || panic!("duplicate input was replayed"))
            .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(first["duplicate"], false);
        assert_eq!(duplicate["duplicate"], true);
        assert_eq!(duplicate["message_id"], "real-one");
        let changed = Send {
            text: "different".into(),
            ..args
        };
        assert!(
            receipts
                .admit(1, 2, &changed, || panic!("conflicting input was admitted"))
                .is_err()
        );
    }

    #[test]
    fn receipt_capacity_rejects_before_admission_and_new_generation_has_separate_authority() {
        let receipts = Receipts::default();
        for index in 0..MAX_RECEIPTS {
            let args = Send {
                request_id: format!("r{index}"),
                text: "x".into(),
                target: "next".into(),
            };
            receipts
                .admit(1, 2, &args, || Ok(json!({"message_id":index.to_string()})))
                .unwrap();
        }
        let args = Send {
            request_id: "new".into(),
            text: "x".into(),
            target: "next".into(),
        };
        assert!(
            receipts
                .admit(1, 2, &args, || panic!("over-capacity input admitted"))
                .is_err()
        );
        let receipts = Receipts::default();
        receipts
            .admit(1, 2, &args, || Ok(json!({"message_id":"old"})))
            .unwrap();
        assert_eq!(
            receipts
                .admit(2, 3, &args, || Ok(json!({"message_id":"new"})))
                .unwrap()["message_id"],
            "new"
        );
    }

    #[test]
    fn portable_conversation_counters_keep_all_u64_bits() {
        let page = portable_page(
            json!({"next_cursor":u64::MAX,"start_cursor":1,"end_cursor":u64::MAX,
            "retention_gap_possible":true,"events":[{"sequence":u64::MAX-1,
                "source":{"connection":u64::MAX,"request_id":9007199254740993u64}}]}),
        );
        assert_eq!(page["next_cursor"], u64::MAX.to_string());
        assert_eq!(page["events"][0]["sequence"], (u64::MAX - 1).to_string());
        assert_eq!(
            page["events"][0]["source"]["request_id"],
            "9007199254740993"
        );
        assert_eq!(page["gap"], true);
    }
}
