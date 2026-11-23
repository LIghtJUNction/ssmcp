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
        Some(path) if valid_relative_path(path) && reserved_policy_path(path) => {
            Decision::RequireApproval
        }
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
            assert!(
                authorize(
                    operation,
                    &json!({"path":"fixture/file"}),
                    Snapshot::default()
                )
                .is_allowed()
            );
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod rule_tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::symlink,
        path::PathBuf,
        time::{Duration, Instant},
    };

    const PROJECT_MODE: &str = "project_rules = true\n";
    const OP: &str = "ssmcp.fs.write_text";

    fn rule(action: &str) -> String {
        format!("[operations]\n\"{OP}\" = \"{action}\"\n")
    }

    #[test]
    fn actual_guard_matches_independent_decision_matrix() {
        use Decision::{Allow, Deny, RequireApproval};
        let names = ["allow", "require_approval", "deny"];
        let reference = [
            [Allow, RequireApproval, Deny],
            [RequireApproval, RequireApproval, Deny],
            [Deny, Deny, Deny],
        ];
        for (i, baseline) in names.into_iter().enumerate() {
            for (j, project) in names.into_iter().enumerate() {
                let baseline = format!("{PROJECT_MODE}{}", rule(baseline));
                let rules = rules::parse(&baseline, || Ok(rule(project))).unwrap();
                let snapshot = Snapshot {
                    rules,
                    ..Snapshot::default()
                };
                let allowed_path = authorize(OP, &json!({"path":"fixture"}), snapshot);
                assert_eq!(allowed_path.decision, reference[i][j], "{i}, {j}");
                assert_eq!(
                    authorize(OP, &json!({"path":"../fixture"}), snapshot).decision,
                    Deny
                );
            }
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let mut token = [0u8; 8];
            crate::platform::random_bytes(&mut token).unwrap();
            let root = std::env::temp_dir().join(format!(
                "ssmcp-safety-rules-{:016x}",
                u64::from_ne_bytes(token)
            ));
            crate::platform::private_directory(&root).unwrap();
            Self(root)
        }

        fn policy_dir(&self, key: &str) -> PathBuf {
            let state = self.0.join(key);
            let config = state.join(".ssmcp");
            let policy = config.join("safety");
            for dir in [&state, &config, &policy] {
                crate::platform::private_directory(dir).unwrap();
            }
            policy
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    #[ignore = "manual synthetic latency measurement"]
    fn safety_latency_samples() {
        use std::hint::black_box;

        const WARMUP: usize = 1_000;
        const SAMPLES: usize = 10_000;

        fn measure(label: &str, mut attempt: impl FnMut()) {
            for _ in 0..WARMUP {
                attempt();
            }
            let mut times = Vec::with_capacity(SAMPLES);
            for _ in 0..SAMPLES {
                let start = Instant::now();
                attempt();
                times.push(start.elapsed().as_nanos());
            }
            times.sort_unstable();
            println!(
                "{label}: n={SAMPLES} p50={} ns p95={} ns",
                times[SAMPLES / 2 - 1],
                times[SAMPLES * 95 / 100 - 1]
            );
        }

        let scratch = Scratch::new();
        let workspace = super::super::Workspace::new(scratch.0.clone()).unwrap();
        let rules = rules::parse(&rule("deny"), || unreachable!()).unwrap();
        workspace
            .root
            .safety_rules
            .refresh(Instant::now() + Duration::from_secs(60), || Ok(Some(rules)));
        let arguments = json!({"path":"fixture","text":"synthetic"});
        measure(
            "SSMCP blocked native preflight + bounded memory audit",
            || {
                assert!(black_box(workspace.root.perform_audited(OP, arguments.clone())).is_err());
            },
        );

        let rules = [
            Rule::new(1, ActionKind::Tool, OP, Decision::Allow),
            Rule::new(2, ActionKind::Tool, OP, Decision::Deny),
            Rule::new(3, ActionKind::Tool, OP, Decision::Allow),
        ];
        let policy = Policy::new(1, &rules).unwrap();
        let mut guard = Guard::new(
            policy,
            Context {
                session: 1,
                capabilities: 0,
            },
            Limits {
                max_calls: u64::MAX,
                max_payload_bytes: usize::MAX,
            },
        );
        let request = Request::tool("fixture", OP, b"{}");
        measure("zguard three exact rules, NoAudit, deny", || {
            assert!(
                !black_box(guard.authorize(black_box(request), 0, None, &mut NoAudit)).is_allowed()
            );
        });
    }

    #[tokio::test]
    async fn trusted_reload_is_atomic_and_failed_cold_start_never_writes() {
        let scratch = Scratch::new();
        let baseline = scratch.policy_dir("home").join("baseline.toml");
        let project = scratch.policy_dir("workspace").join("rules.toml");
        let home = scratch.0.join("home");
        let workspace_path = scratch.0.join("workspace");
        let workspace = super::super::Workspace::new(workspace_path.clone()).unwrap();
        let now = Instant::now();
        let store = &workspace.root.safety_rules;
        let load = || linux::load_at(&home, &workspace.root.directory);

        fs::write(&project, rule("deny")).unwrap();
        assert!(
            load().unwrap().is_none(),
            "workspace alone cannot activate rules"
        );
        fs::write(&baseline, PROJECT_MODE).unwrap();
        fs::write(&project, "malformed!").unwrap();
        let cold = store.refresh(now, load);
        assert_eq!(cold.state, "configured_unavailable");
        let args = || json!({"path":"nested/fixture", "text":"test", "create_parents":true});
        assert!(workspace.perform(OP, args()).await.is_err());
        assert!(!workspace_path.join("nested").exists());

        fs::write(&project, rule("allow")).unwrap();
        let active = store.refresh(now + Duration::from_secs(2), load);
        assert_eq!((active.state, active.revision), ("active", 1));
        workspace.perform(OP, args()).await.unwrap();
        assert_eq!(
            fs::read_to_string(workspace_path.join("nested/fixture")).unwrap(),
            "test"
        );

        fs::write(&baseline, format!("{PROJECT_MODE}{}", rule("deny"))).unwrap();
        fs::remove_file(&project).unwrap();
        symlink(&baseline, &project).unwrap();
        let retained = store.refresh(now + Duration::from_secs(4), load);
        assert_eq!(
            (retained.state, retained.revision),
            ("retained_previous", 1)
        );
        assert!(authorize(OP, &args(), retained).is_allowed());

        fs::remove_file(&project).unwrap();
        fs::write(&project, rule("allow")).unwrap();
        let denied = store.refresh(now + Duration::from_secs(6), load);
        assert_eq!((denied.state, denied.revision), ("active", 2));
        let untouched = json!({"path":"blocked/data", "text":"secret", "create_parents":true});
        assert!(workspace.perform(OP, untouched).await.is_err());
        assert!(!workspace_path.join("blocked").exists());
    }
}
