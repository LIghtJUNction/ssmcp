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
pub(super) struct Baseline {
    #[serde(default)]
    project_rules: bool,
    #[serde(default)]
    operations: BTreeMap<String, Action>,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Project {
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

#[derive(Clone, Copy, Debug)]
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
    active: Option<Rules>,
    revision: u64,
    next_check: Option<Instant>,
    failed: bool,
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
        load: impl FnOnce() -> Result<Option<Rules>, ()>,
    ) -> Snapshot {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.next_check.is_none_or(|check| now >= check) {
            state.next_check = Some(now + Duration::from_secs(1));
            match load() {
                Ok(Some(rules)) => {
                    if state.active != Some(rules) {
                        state.revision = state.revision.saturating_add(1);
                        state.active = Some(rules);
                    }
                    state.failed = false;
                }
                Ok(None) if state.active.is_none() && !state.failed => (),
                Ok(None) | Err(()) => state.failed = true,
            }
        }
        let status = match (state.active, state.failed) {
            (Some(_), false) => "active",
            (Some(_), true) => "retained_previous",
            (None, false) => "builtin",
            (None, true) => "configured_unavailable",
        };
        Snapshot {
            rules: state.active.unwrap_or_default(),
            revision: state.revision,
            state: status,
            deny_all: state.active.is_none() && state.failed,
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn parse(
    baseline: &str,
    project: impl FnOnce() -> Result<String, ()>,
) -> Result<Rules, ()> {
    let baseline: Baseline = toml::from_str(baseline).map_err(|_| ())?;
    let mut rules = Rules {
        baseline: decisions(baseline.operations)?,
        ..Rules::default()
    };
    if baseline.project_rules {
        let project: Project = toml::from_str(&project()?).map_err(|_| ())?;
        rules.project = decisions(project.operations)?;
    }
    Ok(rules)
}
