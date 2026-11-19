use crate::messaging;
use serde_json::{Map, Value, json};

pub(crate) fn tool() -> Value {
    let client = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    let session = json!({"type":"string","pattern":"^session_[0-9a-f]{32}$"});
    let thread = json!({"type":"string","pattern":"^agent_[0-9a-f]{32}$"});
    let target = json!({"type":"object","properties":{"client_id":client,"session_id":session,"thread_id":thread},
        "required":["client_id","session_id"],"additionalProperties":false});
    let process = json!({"type":"string","pattern":"^process_[0-9a-f]{32}$"});
    let selector = json!({"oneOf":[
        {"type":"object","properties":{"scope":{"const":"session"},"client_id":client,"session_id":session,"thread_id":thread},"required":["scope","client_id","session_id"],"additionalProperties":false},
        {"type":"object","properties":{"scope":{"const":"targets"},"targets":{"type":"array","items":target,"minItems":1,"maxItems":messaging::MAX_BATCH}},"required":["scope","targets"],"additionalProperties":false},
        {"type":"object","properties":{"scope":{"const":"process"},"process_id":process},"required":["scope","process_id"],"additionalProperties":false},
        {"type":"object","properties":{"scope":{"const":"global"}},"required":["scope"],"additionalProperties":false}
    ]});
    let proof = json!({"type":"object","properties":{"client_id":client,"id":thread,"token":{"type":"string","minLength":64,"maxLength":64,"writeOnly":true}},
        "required":["client_id","id","token"],"additionalProperties":false});
    let fields = json!({
        "action":{"type":"string","enum":["key","list","send","read","receive","wait","ack","status"]},
        "client_id":client,"process_id":process,"cursor":client,
        "include_threads":{"type":"boolean","default":false,"description":"Requires client_id. Only currently registered collaboration participants are listed."},
        "limit":{"type":"integer","minimum":1,"maximum":messaging::MAX_READ_MESSAGES,"default":messaging::MAX_READ_MESSAGES},
        "to":selector,"target":target,"text":{"type":"string","minLength":1,"maxLength":messaging::MAX_TEXT_BYTES},
        "topic":{"type":"string","maxLength":128},"reply_to":{"type":"string","maxLength":80},
        "idempotency_key":{"type":"string","maxLength":160,"description":"Obtain from messages.key; expires after five minutes or relay restart. Keep the same key to inspect the same frozen batch. Never automatically resend unknown outcomes."},
        "ttl_ms":{"type":"integer","minimum":1,"maximum":messaging::MAX_TTL_MS,"default":messaging::DEFAULT_TTL_MS},
        "from_client_id":client,"agent":proof,
        "after":{"type":"integer","minimum":0,"default":0},
        "wait_ms":{"type":"integer","minimum":0,"maximum":messaging::MAX_WAIT_MS,"default":1000},
        "ids":{"type":"array","items":{"type":"string","pattern":"^delivery_[0-9a-f]{64}$"},"minItems":1,"maxItems":messaging::MAX_BATCH},
        "through":{"type":"integer","minimum":0},
        "message_id":{"type":"string","pattern":"^peer_[0-9a-f]{32}$"},
        "refresh":{"type":"boolean","default":false,"description":"Refresh only these original recipients' native receipt states; never redeliver."}
    });
    let actions: [(&str, &[&str], &[&str]); 8] = [
        ("key", &[], &[]),
        (
            "list",
            &[],
            &[
                "client_id",
                "process_id",
                "cursor",
                "limit",
                "include_threads",
            ],
        ),
        (
            "send",
            &["to", "text", "idempotency_key"],
            &[
                "to",
                "text",
                "idempotency_key",
                "topic",
                "reply_to",
                "ttl_ms",
                "from_client_id",
                "agent",
            ],
        ),
        ("read", &["target"], &["target", "after", "limit", "agent"]),
        (
            "receive",
            &["target"],
            &["target", "after", "limit", "agent"],
        ),
        (
            "wait",
            &["target"],
            &["target", "after", "limit", "wait_ms", "agent"],
        ),
        ("ack", &["target"], &["target", "ids", "through", "agent"]),
        ("status", &["message_id"], &["message_id", "refresh"]),
    ];
    let variants=actions.into_iter().map(|(action,required,allowed)|{
        let mut properties=Map::new();properties.insert("action".into(),json!({"const":action}));
        for name in allowed {properties.insert((*name).into(),fields[*name].clone());}
        let mut required_fields=vec!["action"];required_fields.extend_from_slice(required);
        let mut variant=json!({"type":"object","properties":properties,"required":required_fields,"additionalProperties":false});
        if action=="ack" {variant["oneOf"]=json!([{"required":["ids"],"not":{"required":["through"]}},{"required":["through"],"not":{"required":["ids"]}}]);}
        variant
    }).collect::<Vec<_>>();
    json!({"name":"messages",
        "description":"Native cross-session candidate messages: discover exact live sessions/process generations and registered participants, then send to one session, explicit targets, one process, or the current authorized global snapshot. Source identity is relay-derived. Read/receive/wait do not consume; ack explicitly handles candidates. No model or command starts. Participant read/ack needs its private binding. Delivery ACK is storage, not human reading. Use key before send; status never resends. Details: ssmcp://messages.",
        "inputSchema":{"type":"object","properties":fields,"required":["action"],"oneOf":variants,"additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":false}})
}
