//! Optional display metadata from the current MCP request.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CAPABILITY: &str = "openai_context_v1";
pub(crate) const SESSION: &str = "openai/session";
pub(crate) const SUBJECT: &str = "openai/subject";
const MAX_VALUE_BYTES: usize = 512;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subject: Option<String>,
}

pub(crate) fn valid(value: &str) -> bool {
    value.len() <= MAX_VALUE_BYTES
        && !value.trim().is_empty()
        && !value.chars().any(|ch| {
            ch.is_control() || matches!(ch, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
}

impl OpenAiContext {
    pub(crate) fn from_meta(meta: Option<&Value>) -> Option<Self> {
        let meta = meta?.as_object()?;
        let field = |key| {
            meta.get(key)
                .and_then(Value::as_str)
                .filter(|value| valid(value))
                .map(str::to_owned)
        };
        let session = field(SESSION);
        let subject = field(SUBJECT);
        (session.is_some() || subject.is_some()).then_some(Self { session, subject })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_bounded_string_metadata_is_extracted_independently() {
        for meta in [
            None,
            Some(json!(null)),
            Some(json!(["wrong-shape"])),
            Some(json!({"openai/userLocation":{"city":"fixture"},"openai/organization":"x"})),
            Some(json!({"openai/session":{},"openai/subject":null})),
            Some(json!({"openai/session":"x".repeat(513),"openai/subject":"a\u{001b}[2J"})),
        ] {
            assert!(OpenAiContext::from_meta(meta.as_ref()).is_none());
        }
        let meta = json!({
            "openai/session": "correlation-甲",
            "openai/subject": 123,
            "identity": "ignored",
            "openai/userAgent": "ignored",
        });
        let context = OpenAiContext::from_meta(Some(&meta)).unwrap();
        assert_eq!(
            serde_json::to_value(context).unwrap(),
            json!({"session":"correlation-甲"})
        );
    }
}
