//! Built-in preflight at the common native workspace mutation boundary.
#[cfg(target_os = "linux")]
mod linux;
mod rules;

use rules::Snapshot;
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

pub(super) fn authorize(operation: &'static str, args: &Value, snapshot: Snapshot) -> Verdict {
    let relative = args.get("path").and_then(Value::as_str);
    let decision = match relative {
        Some(path) if valid_relative_path(path) => Decision::Allow,
        _ => Decision::Deny,
    };
    let baseline = if snapshot.deny_all {
        Decision::Deny
    } else {
        Decision::Allow
    };
    let [user, project] = snapshot.rules.decisions(operation);
    let rules = [
        Rule::new(1, ActionKind::Tool, operation, decision),
        Rule::new(2, ActionKind::Tool, operation, baseline),
        Rule::new(
            3,
            ActionKind::Tool,
            operation,
            user.unwrap_or(Decision::Allow),
        ),
        Rule::new(
            4,
            ActionKind::Tool,
            operation,
            project.unwrap_or(Decision::Allow),
        ),
    ];
    let policy = Policy::new(snapshot.revision, &rules).expect("built-in workspace operation");
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

#[cfg(target_os = "linux")]
pub(super) use rules::Store;

impl super::Root {
    pub(super) fn safety_snapshot(&self) -> Snapshot {
        #[cfg(target_os = "linux")]
        {
            self.safety_rules
                .refresh(std::time::Instant::now(), || linux::load(&self.directory))
        }
        #[cfg(not(target_os = "linux"))]
        {
            Snapshot::default()
        }
    }
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

pub(super) fn event(
    operation: &'static str,
    verdict: Verdict,
    outcome: &'static str,
    snapshot: Snapshot,
) -> Value {
    json!({
        "type": "ssmcp.safety.decision",
        "tool": operation,
        "policy": "native_workspace_path_v1",
        "rules_revision": snapshot.revision,
        "rules_state": snapshot.state,
        "decision": match verdict.decision {
            Decision::Allow => "allow",
            Decision::Deny => "deny",
            Decision::RequireApproval => "require_approval",
        },
        "outcome": outcome,
    })
}

pub(crate) async fn run(args: &[OsString]) -> crate::Result<()> {
    let (event, session) = match args {
        [] => (None, None),
        [status] if status == "status" => (None, None),
        [local, session] if local == "--local" => (None, session.to_str()),
        [inspect, event, local, session] if inspect == "inspect" && local == "--local" => (
            Some(event.to_str().ok_or("invalid safety event ID")?),
            session.to_str(),
        ),
        _ => {
            return Err(
                "usage: ssmcp safety [status | --local NAME | inspect EVENT_ID --local NAME]"
                    .into(),
            );
        }
    };
    if args.len() > 1 {
        let name = session.ok_or("invalid local session name")?;
        let result = crate::session::attach::safety(name, event).await?;
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    println!(
        "{}",
        json!({
            "covered": OPERATIONS,
            "evidence": "ssmcp.safety.decision events via ssmcp.fs.events with stream=safety",
            "approval": "unavailable",
            "custom_rules": if cfg!(target_os = "linux") { "linux_private_baseline_and_opt_in_project" } else { "unavailable" },
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
            let verdict = authorize(operation, &input, Snapshot::default());
            assert!(!verdict.is_allowed());
            assert!(
                !event(operation, verdict, "blocked", Snapshot::default())
                    .to_string()
                    .contains("synthetic-secret")
            );
            for path in [
                "fixture/file",
                ".ssmcp/safety/rules.toml",
                "./.ssmcp/safety",
                ".SSMCP/SAFETY/rules.toml",
            ] {
                assert!(
                    authorize(operation, &json!({"path":path}), Snapshot::default()).is_allowed()
                );
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod rule_tests;
