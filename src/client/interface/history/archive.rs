//! Compatibility errors for removed archive operations.
use super::{HISTORY_RETIRED, SessionSummary};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    #[default]
    Active,
    Archived,
    All,
}

pub(crate) fn change(_id: Option<&str>, _archived: bool) -> Result<Value, String> {
    Err(HISTORY_RETIRED.into())
}

pub(crate) fn sessions(_filter: Filter) -> Result<Vec<SessionSummary>, String> {
    Err(HISTORY_RETIRED.into())
}

pub(crate) fn call(_arguments: Value) -> Result<Value, String> {
    Err(HISTORY_RETIRED.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn archive_and_restore_are_retired_without_reading_a_store() {
        for action in ["archive", "restore"] {
            assert!(
                call(json!({"action":action,"id":"conversation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}))
                    .unwrap_err()
                    .starts_with("tool_retired:")
            );
        }
        assert!(change(None, true).is_err());
        assert!(sessions(Filter::All).is_err());
    }
}
