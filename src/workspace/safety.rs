//! Built-in preflight at the common native workspace mutation boundary.
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    path::{Component, Path},
};
use zharness::zguard::{
    ActionKind, Context, Decision, Guard, Limits, NoAudit, Policy, Request, Rule, Verdict,
};

const OPERATIONS: [&str; 4] = [
    "ssmcp.fs.write",
    "ssmcp.fs.write_text",
    "ssmcp.fs.edit_text",
    "ssmcp.fs.mkdir",
];

pub(super) fn operation(name: &str) -> Option<&'static str> {
    OPERATIONS.into_iter().find(|operation| *operation == name)
}

pub(super) fn authorize(operation: &'static str, args: &Value) -> Verdict {
    let relative = args.get("path").and_then(Value::as_str);
    let decision = match relative {
        Some(path) if valid_relative_path(path) && reserved_policy_path(path) => {
            Decision::RequireApproval
        }
        Some(path) if valid_relative_path(path) => Decision::Allow,
        _ => Decision::Deny,
    };
    let rules = [Rule::new(1, ActionKind::Tool, operation, decision)];
    let policy = Policy::new(1, &rules).expect("built-in workspace operation");
    // The workspace executor still validates and confines the real request.
    // This preflight has no approval input or independent invocation quota.
    let mut guard = Guard::new(
        policy,
        Context {
            session: 0,
            capabilities: 0,
        },
        Limits {
            max_calls: 1,
            max_payload_bytes: usize::MAX,
        },
    );
    guard.authorize(
        Request::tool("workspace-preflight", operation, b""),
        0,
        None,
        &mut NoAudit,
    )
}

fn valid_relative_path(path: &str) -> bool {
    for component in Path::new(path).components() {
        match component {
            Component::Normal(_) | Component::CurDir => (),
            _ => return false,
        }
    }
    true
}

// This reserves the proposed project policy namespace from direct native tool
// mutations. This is lexical classification, not a filesystem access control.
fn reserved_policy_path(path: &str) -> bool {
    let mut parts = Path::new(path).components().filter_map(|part| match part {
        Component::Normal(name) => name.to_str(),
        _ => None,
    });
    parts
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case(".ssmcp"))
        && parts
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case("safety"))
}

pub(super) fn event(operation: &'static str, verdict: Verdict, outcome: &'static str) -> Value {
    json!({
        "type": "ssmcp.safety.decision",
        "tool": operation,
        "policy": "native_workspace_path_v1",
        "decision": match verdict.decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::RequireApproval => "require_approval",
        },
        "outcome": outcome,
    })
}

pub(crate) fn run(args: &[OsString]) -> crate::Result<()> {
    match args {
        [] => (),
        [status] if status == "status" => (),
        [inspect, _] if inspect == "inspect" => {
            return Err(
                "live event inspection requires the owning session's ssmcp.fs.events tool".into(),
            );
        }
        _ => return Err("usage: ssmcp safety [status | inspect EVENT_ID]".into()),
    }
    println!(
        "{}",
        json!({
            "covered": OPERATIONS,
            "evidence": "ssmcp.safety.decision events via ssmcp.fs.events with stream=safety",
            "approval": "unavailable",
            "custom_rules": "unavailable",
            "uncovered": ["external processes", "transfers", "deletion", "secret egress"],
        })
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_mutation_preflight_is_path_scoped_and_redacted() {
        for operation in OPERATIONS {
            let input = json!({"path":"../synthetic-secret-name","text":"synthetic-secret-value"});
            let verdict = authorize(operation, &input);
            assert!(!verdict.is_allowed());
            assert!(
                !event(operation, verdict, "blocked")
                    .to_string()
                    .contains("synthetic-secret")
            );
            assert!(authorize(operation, &json!({"path":"fixture/file"})).is_allowed());
        }
    }
}
