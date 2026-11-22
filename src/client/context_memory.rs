//! Retired persistent context memory. Live request ownership stays in the history module.
#[cfg(test)]
use super::interface::history;
use serde_json::{Value, json};

const RETIRED: &str =
    "tool_retired: context memory has been removed; only live process logs are available";
#[cfg(test)]
const COMMAND_RETIRED: &str =
    "command_retired: context memory has been removed; only live process logs are available";

/// Startup keeps no memory store and never reads or creates a checkpoint.
pub(crate) fn initialize() -> Result<(), String> {
    Ok(())
}

/// Retained for callers publishing live records; no second copy is kept.
#[cfg(test)]
pub(crate) fn record_history(
    _workspace: &str,
    _conversation_id: &str,
    _message: &history::Message,
) -> Result<(), String> {
    Ok(())
}

/// Delivery ownership and acknowledgements remain with the request machinery.
#[cfg(test)]
pub(crate) fn record_delivered_history(
    _workspace: &str,
    _conversation_id: &str,
    _message: &history::Message,
    _request_id: &str,
    _revision: u64,
    _target: &str,
) -> Result<(), String> {
    Ok(())
}

/// Responses no longer carry remembered conversation text or checkpoints.
#[cfg(test)]
pub(crate) fn snapshot_for_tool() -> Option<Value> {
    None
}

/// Existing UI projections can report retirement without opening old state.
pub(crate) fn ui_snapshot() -> Result<Value, String> {
    Ok(json!({
        "active": false,
        "retired": true,
        "storage": "none",
        "status": {
            "schema_version": 1,
            "active": false,
            "retired": true,
            "storage": "none",
            "restored_from_disk": false,
        },
        "read": {
            "schema_version": 1,
            "active": false,
            "retired": true,
            "summary": null,
            "recent": [],
        },
        "list": {
            "schema_version": 1,
            "active": false,
            "retired": true,
            "branches": [],
        },
    }))
}

pub(crate) fn call(name: &str, _arguments: Value) -> Result<Value, String> {
    match name {
        "ssmcp.context.status"
        | "ssmcp.context.list"
        | "ssmcp.context.read"
        | "ssmcp.context.compact"
        | "ssmcp.context.fork"
        | "ssmcp.context.switch" => Err(RETIRED.into()),
        _ => Err("unknown context memory operation".into()),
    }
}

#[cfg(test)]
pub(crate) fn command(name: &str, _arguments: &str) -> Result<String, String> {
    match name {
        "context" | "memory" | "compact" | "fork" | "switch" => Err(COMMAND_RETIRED.into()),
        _ => Err("unknown local context command".into()),
    }
}

pub(crate) fn tools() -> Vec<Value> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

    struct Temporary(PathBuf);

    impl Temporary {
        fn new() -> Self {
            let id = FIXTURE_ID.fetch_add(1, Ordering::Relaxed);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ssmcp-retired-memory-{}-{now}-{id}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Temporary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn message(role: &str) -> history::Message {
        history::Message {
            id: "message_00000000000000000000000000000001".into(),
            role: role.into(),
            content: "PRIVATE_LIVE_RECORD_DO_NOT_REMEMBER".into(),
            kind: "fixture".into(),
            created_at_ms: 1,
            truncated: false,
        }
    }

    #[test]
    fn startup_and_delivery_keep_no_memory_and_leave_existing_files_untouched() {
        let fixture = Temporary::new();
        let legacy = fixture.0.join("state.json");
        let original = b"legacy checkpoint retained for user-controlled cleanup";
        fs::write(&legacy, original).unwrap();
        let workspace = fixture.0.to_string_lossy();

        initialize().unwrap();
        record_history(&workspace, "conversation_fixture", &message("assistant")).unwrap();
        record_delivered_history(
            &workspace,
            "conversation_fixture",
            &message("user"),
            "request_fixture",
            1,
            "next",
        )
        .unwrap();
        initialize().unwrap();

        assert_eq!(fs::read(&legacy).unwrap(), original.to_vec());
        assert_eq!(fs::read_dir(&fixture.0).unwrap().count(), 1);
        assert!(snapshot_for_tool().is_none());
        let ui = ui_snapshot().unwrap();
        assert_eq!(ui["active"], false);
        assert_eq!(ui["retired"], true);
        assert_eq!(ui["status"]["restored_from_disk"], false);
        assert_eq!(ui["read"]["recent"], json!([]));
        assert_eq!(ui["list"]["branches"], json!([]));
        assert!(
            !ui.to_string()
                .contains("PRIVATE_LIVE_RECORD_DO_NOT_REMEMBER")
        );
        assert!(!ui.to_string().contains("legacy checkpoint"));
    }

    #[test]
    fn context_tools_are_unpublished_and_direct_calls_are_retired() {
        assert!(tools().is_empty());
        for operation in ["status", "list", "read", "compact", "fork", "switch"] {
            assert_eq!(
                call(
                    &format!("ssmcp.context.{operation}"),
                    json!({"summary": "must not be saved", "id": "old_branch"})
                )
                .unwrap_err(),
                RETIRED
            );
        }
        assert!(snapshot_for_tool().is_none());
    }

    #[test]
    fn previous_context_commands_do_not_restore_or_save_state() {
        for name in ["context", "memory", "compact", "fork", "switch"] {
            assert_eq!(
                command(name, "old checkpoint").unwrap_err(),
                COMMAND_RETIRED
            );
        }
        assert_eq!(ui_snapshot().unwrap()["storage"], "none");
    }

    #[test]
    fn unrelated_operations_are_not_reported_as_retired_context_tools() {
        assert_eq!(
            call("ssmcp.other.tool", json!({})).unwrap_err(),
            "unknown context memory operation"
        );
        assert_eq!(
            command("other", "").unwrap_err(),
            "unknown local context command"
        );
    }
}
