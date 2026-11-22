//! Compatibility error for removed saved-conversation descriptions.
use serde_json::Value;

pub(crate) fn call(_arguments: Value) -> Result<Value, String> {
    Err(super::HISTORY_RETIRED.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn saved_description_updates_are_retired() {
        assert!(super::call(serde_json::json!({"id":"conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","title":"old session"})).unwrap_err().starts_with("tool_retired:"));
    }
}
