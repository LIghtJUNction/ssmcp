//! A proposal for one authenticated, request-correlated output. Only the relay
//! ledger accepts it; this native result never approves an agent's global finish.
use crate::runtime::call_scope::Source;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "ssmcp.ui.reply";

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Proposal {
    request_id: String,
    input_id: String,
    output_id: String,
    kind: String,
    state: String,
    text: String,
}

impl Proposal {
    pub(crate) fn parse(value: Value) -> Result<Self, String> {
        let proposal: Self = serde_json::from_value(value)
            .map_err(|_| "invalid reply fields; use the delivered request_id and input_id")?;
        crate::wire::messages::validate_request_id(&proposal.request_id)?;
        for (id, prefix) in [
            (&proposal.input_id, "input_"),
            (&proposal.output_id, "output_"),
        ] {
            let valid = id.strip_prefix(prefix).is_some_and(|suffix| {
                suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
            });
            if !valid {
                return Err("reply input_id and output_id must have their documented prefixes and 32 hexadecimal characters".into());
            }
        }
        if !matches!(
            (proposal.kind.as_str(), proposal.state.as_str()),
            ("progress", "running") | ("result", "done" | "error")
        ) {
            return Err("reply progress requires running; result requires done or error".into());
        }
        if proposal.text.trim().is_empty()
            || proposal.text.len() > 16 * 1024
            || proposal
                .text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
        {
            return Err("reply text must be nonempty and at most 16384 UTF-8 bytes, with no control characters except newline or tab".into());
        }
        Ok(proposal)
    }

    pub(crate) fn value(&self) -> Value {
        serde_json::to_value(self).expect("validated reply proposal serializes")
    }
}

pub(crate) fn call(arguments: Value) -> Result<Value, String> {
    let proposal = Proposal::parse(arguments)?;
    let source = Source::capture();
    let call = source
        .call
        .as_ref()
        .ok_or("reply requires an authenticated native caller")?;
    let attribution = call
        .attribution()
        .ok_or("reply requires an authenticated account")?;
    if call.client_id().is_none()
        || attribution.agent.as_ref().map(|agent| agent.id.as_str())
            != call.thread().map(|thread| thread.thread_id())
    {
        return Err(
            "reply requires this connection's exact authenticated main or agent scope".into(),
        );
    }
    Ok(json!({"schema_version":1,"proposal":proposal.value()}))
}

pub(super) fn tool() -> Value {
    json!({"name":TOOL,
        "description":"Reply to one delivered thread_inputs request. Copy its request_id and input_id, and choose a new output_<32 hexadecimal characters> id; reuse that output_id only to retry identical text. progress/running streams an update. result/done or result/error completes only this input. The relay verifies the actual authenticated main or agent recipient and returns accepted:true only after recording it. This result keeps its agent origin and completes only the addressed input.",
        "inputSchema":{"type":"object","additionalProperties":false,
            "required":["request_id","input_id","output_id","kind","state","text"],
            "properties":{
                "request_id":{"type":"string","minLength":1,"maxLength":128},
                "input_id":{"type":"string","pattern":"^input_[0-9a-fA-F]{32}$"},
                "output_id":{"type":"string","pattern":"^output_[0-9a-fA-F]{32}$"},
                "kind":{"type":"string","enum":["progress","result"]},
                "state":{"type":"string","enum":["running","done","error"]},
                "text":{"type":"string","minLength":1,"maxLength":16384,
                    "description":"Nonempty text, at most 16384 UTF-8 bytes."}}},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> Value {
        json!({"request_id":"io_fixture","input_id":format!("input_{}","1".repeat(32)),
            "output_id":format!("output_{}","2".repeat(32)),"kind":"result","state":"done","text":"回答\nnext"})
    }

    #[test]
    fn reply_proposals_have_strict_fields_and_request_local_completion() {
        assert_eq!(Proposal::parse(valid()).unwrap().value(), valid());
        for (field, value) in [
            ("agent_id", json!("spoof")),
            ("accepted", json!(true)),
            ("state", json!("acknowledged")),
            ("kind", json!("answer")),
            ("input_id", json!("input_unknown")),
            ("output_id", json!("output_")),
            ("text", json!("\u{1b}[2J")),
            ("text", json!("界".repeat(5462))),
        ] {
            let mut bad = valid();
            bad[field] = value;
            assert!(Proposal::parse(bad).is_err(), "{field}");
        }
        let mut progress = valid();
        progress["kind"] = json!("progress");
        assert!(Proposal::parse(progress.clone()).is_err());
        progress["state"] = json!("running");
        assert!(Proposal::parse(progress).is_ok());
    }

    #[test]
    fn unscoped_reply_cannot_create_a_receipt() {
        assert!(Source::default().blocking(|| call(valid())).is_err());
        assert!(
            tool()["description"]
                .as_str()
                .unwrap()
                .contains("agent origin")
        );
    }
}
