//! Bounded policy snapshots. Publication and reload are serialized per workspace.
use super::OPERATIONS;
#[cfg(target_os = "linux")]
use serde::Deserialize;
#[cfg(target_os = "linux")]
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use zharness::zguard::Decision;

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Allow,
    RequireApproval,
    Deny,
}

#[cfg(target_os = "linux")]
impl From<Action> for Decision {
    fn from(value: Action) -> Self {
        match value {
            Action::Allow => Self::Allow,
            Action::RequireApproval => Self::RequireApproval,
            Action::Deny => Self::Deny,
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Baseline {
    #[serde(default)]
    project_rules: bool,
    #[serde(default)]
    operations: BTreeMap<String, Action>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Project {
    #[serde(default)]
    operations: BTreeMap<String, Action>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Rules {
    baseline: [Option<Decision>; OPERATIONS.len()],
    project: [Option<Decision>; OPERATIONS.len()],
}

impl Rules {
    pub(super) fn decisions(self, operation: &str) -> [Option<Decision>; 2] {
        let index = OPERATIONS
            .iter()
            .position(|name| *name == operation)
            .expect("native mutation");
        [self.baseline[index], self.project[index]]
    }

    #[cfg(target_os = "linux")]
    pub(super) fn baseline_only(self) -> Self {
        Self {
            project: [None; OPERATIONS.len()],
            ..self
        }
    }
}

#[cfg(target_os = "linux")]
fn decisions(input: BTreeMap<String, Action>) -> Result<[Option<Decision>; OPERATIONS.len()], ()> {
    let mut result = [None; OPERATIONS.len()];
    for (name, action) in input {
        let index = OPERATIONS.iter().position(|op| *op == name).ok_or(())?;
        result[index] = Some(action.into());
    }
    Ok(result)
}

/// A valid user baseline is still evidence when a subordinate project fails.
/// This never authorizes a mutation: every load error publishes deny_all.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LoadError {
    pub(super) baseline: Option<Rules>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::workspace) struct Snapshot {
    pub(super) rules: Rules,
    pub(super) revision: u64,
    pub(super) state: &'static str,
    pub(super) deny_all: bool,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            rules: Rules::default(),
            revision: 0,
            state: "builtin",
            deny_all: false,
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct State {
    // Only a complete, successful load latches activation. A missing baseline
    // after activation cannot silently remove a user's previous restrictions.
    last_valid: Option<Rules>,
    snapshot: Snapshot,
    next_check: Option<Instant>,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
pub(in crate::workspace) struct Store {
    state: Mutex<State>,
}

#[cfg(target_os = "linux")]
impl Store {
    pub(super) fn refresh(
        &self,
        now: Instant,
        load: impl FnOnce() -> Result<Option<Rules>, LoadError>,
    ) -> Snapshot {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.next_check.is_none_or(|check| now >= check) {
            state.next_check = Some(now + Duration::from_secs(1));
            let mut next = match load() {
                Ok(Some(rules)) => {
                    state.last_valid = Some(rules);
                    Snapshot {
                        rules,
                        state: "active",
                        ..Snapshot::default()
                    }
                }
                Ok(None) if state.last_valid.is_none() => Snapshot::default(),
                Ok(None) => Snapshot {
                    rules: state.last_valid.unwrap_or_default(),
                    state: "active_unavailable",
                    deny_all: true,
                    ..Snapshot::default()
                },
                Err(error) => Snapshot {
                    rules: error.baseline.or(state.last_valid).unwrap_or_default(),
                    state: if state.last_valid.is_some() {
                        "active_unavailable"
                    } else {
                        "configured_unavailable"
                    },
                    deny_all: true,
                    ..Snapshot::default()
                },
            };
            next.revision = state.snapshot.revision;
            if next != state.snapshot {
                next.revision = next.revision.saturating_add(1);
            }
            state.snapshot = next;
        }
        state.snapshot
    }
}

#[cfg(target_os = "linux")]
pub(super) fn parse(
    baseline: &str,
    project: impl FnOnce() -> Result<String, ()>,
) -> Result<Rules, LoadError> {
    let baseline: Baseline = toml::from_str(baseline).map_err(|_| LoadError::default())?;
    let mut rules = Rules {
        baseline: decisions(baseline.operations).map_err(|_| LoadError::default())?,
        ..Rules::default()
    };
    if baseline.project_rules {
        let unavailable = LoadError {
            baseline: Some(rules),
        };
        let project: Project =
            toml::from_str(&project().map_err(|_| unavailable)?).map_err(|_| unavailable)?;
        rules.project = decisions(project.operations).map_err(|_| unavailable)?;
    }
    Ok(rules)
}
