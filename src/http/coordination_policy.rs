//! Per-participant preferences for advisory coordinator waiting hints.
//! These settings never change authorization, worker ownership, or delivery.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_POLICIES: usize = 256;
const RETAIN_FOR: Duration = Duration::from_secs(15 * 60);
const MAX_KINDS: usize = 16;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Scope {
    owner: String,
    client_id: String,
    agent_id: String,
}

/// A single explicitly requested advisory maintenance round, not an execution grant.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct Dreaming {
    pub round_id: String,
    pub repository: String,
    pub budget_ms: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(super) struct Policy {
    pub enabled: bool,
    pub window_ms: u64,
    pub kinds: Vec<String>,
    pub strategy: Option<String>,
    pub dreaming: Option<Dreaming>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: true,
            window_ms: crate::wire::MAX_OBSERVATION_WAIT_MS,
            kinds: [
                "user.input_available",
                "worker.completed",
                "worker.idle",
                "worker.progress",
            ]
            .map(str::to_owned)
            .to_vec(),
            strategy: None,
            dreaming: None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Patch {
    enabled: Option<bool>,
    window_ms: Option<u64>,
    kinds: Option<Vec<String>>,
    strategy: Option<String>,
    dreaming: Option<Dreaming>,
}

impl Patch {
    fn parse(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or("coordination policy patch must be an object")?;
        if ["enabled", "window_ms", "kinds"]
            .iter()
            .any(|field| object.get(*field).is_some_and(Value::is_null))
        {
            return Err("coordination policy enabled, window_ms, and kinds cannot be null".into());
        }
        let patch: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("invalid coordination policy patch: {error}"))?;
        if patch
            .window_ms
            .is_some_and(|ms| !(1_000..=crate::wire::MAX_OBSERVATION_WAIT_MS).contains(&ms))
        {
            return Err(format!(
                "coordination policy window_ms must be 1000..={}",
                crate::wire::MAX_OBSERVATION_WAIT_MS
            ));
        }
        if let Some(kinds) = &patch.kinds {
            if kinds.is_empty() || kinds.len() > MAX_KINDS {
                return Err("coordination policy kinds must contain 1..=16 event kinds".into());
            }
            for (index, kind) in kinds.iter().enumerate() {
                let prefix = kind.strip_suffix(".*").unwrap_or(kind);
                if prefix.is_empty()
                    || prefix.len() > 94
                    || !prefix.split('.').all(|part| {
                        !part.is_empty()
                            && part
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
                    })
                    || kinds[..index].contains(kind)
                {
                    return Err("coordination policy kinds must be distinct event names or trailing .* prefixes".into());
                }
                // The hook must not recommend waiting on its own lifecycle.
                // Explicit event subscriptions have their own matching rules.
                if prefix == "wait" || prefix.starts_with("wait.") {
                    return Err(
                        "coordination policy kinds cannot include wait lifecycle events".into(),
                    );
                }
            }
        }
        if patch.strategy.as_ref().is_some_and(|strategy| {
            strategy.chars().count() > 512 || strategy.chars().any(char::is_control)
        }) {
            return Err(
                "coordination policy strategy must contain at most 512 characters without controls"
                    .into(),
            );
        }
        if let Some(round) = &patch.dreaming {
            if round.round_id.is_empty()
                || round.round_id.len() > 128
                || !round
                    .round_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err("Dreaming round_id must contain 1..=128 ASCII letters, digits, underscores or hyphens".into());
            }
            if !matches!(round.repository.as_str(), "project" | "global") {
                return Err("Dreaming repository must be project or global".into());
            }
            if !(1_000..=crate::wire::MAX_OBSERVATION_WAIT_MS).contains(&round.budget_ms) {
                return Err(format!(
                    "Dreaming budget_ms must be 1000..={}",
                    crate::wire::MAX_OBSERVATION_WAIT_MS
                ));
            }
        }
        Ok(patch)
    }

    fn apply(self, policy: &mut Policy, clear_strategy: bool, clear_dreaming: bool) {
        if let Some(enabled) = self.enabled {
            policy.enabled = enabled;
        }
        if let Some(window_ms) = self.window_ms {
            policy.window_ms = window_ms;
        }
        if let Some(mut kinds) = self.kinds {
            kinds.sort();
            policy.kinds = kinds;
        }
        if clear_strategy {
            policy.strategy = None;
        } else if let Some(strategy) = self.strategy {
            let strategy = strategy.trim();
            policy.strategy = (!strategy.is_empty()).then(|| strategy.to_owned());
        }
        if clear_dreaming {
            policy.dreaming = None;
        } else if let Some(round) = self.dreaming {
            policy.dreaming = Some(round);
        }
    }
}

#[derive(Default)]
struct Registry {
    entries: HashMap<Scope, (Policy, Instant)>,
}

impl Registry {
    fn access(
        &mut self,
        scope: Scope,
        patch: Option<&Value>,
        now: Instant,
    ) -> Result<Policy, String> {
        // Validate before touching an existing preference or allocating state.
        let parsed = patch.map(Patch::parse).transpose()?;
        self.entries
            .retain(|_, (_, touched)| now.saturating_duration_since(*touched) < RETAIN_FOR);
        let mut policy = self
            .entries
            .get(&scope)
            .map(|(policy, _)| policy.clone())
            .unwrap_or_default();
        if let Some(parsed) = parsed {
            if !self.entries.contains_key(&scope) && self.entries.len() >= MAX_POLICIES {
                return Err(
                    "coordination_policy_capacity: retry after an inactive policy expires".into(),
                );
            }
            let clear_strategy = patch
                .and_then(|value| value.get("strategy"))
                .is_some_and(Value::is_null);
            let clear_dreaming = patch
                .and_then(|value| value.get("dreaming"))
                .is_some_and(Value::is_null);
            parsed.apply(&mut policy, clear_strategy, clear_dreaming);
            self.entries.insert(scope, (policy.clone(), now));
        } else if let Some((_, touched)) = self.entries.get_mut(&scope) {
            *touched = now;
        }
        Ok(policy)
    }
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

pub(super) fn effective(
    owner: &str,
    client_id: &str,
    agent_id: &str,
    patch: Option<&Value>,
) -> Result<Policy, String> {
    if [owner, client_id, agent_id]
        .iter()
        .any(|part| part.is_empty() || part.len() > 1024)
    {
        return Err("coordination policy requires an authenticated participant scope".into());
    }
    registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .access(
            Scope {
                owner: owner.into(),
                client_id: client_id.into(),
                agent_id: agent_id.into(),
            },
            patch,
            Instant::now(),
        )
}

pub(crate) fn policy(
    owner: &str,
    client_id: &str,
    agent_id: &str,
    patch: Option<&Value>,
) -> Result<Value, String> {
    let policy = effective(owner, client_id, agent_id, patch)?;
    Ok(json!({
        "schema_version": 1,
        "client_id": client_id,
        "agent_id": agent_id,
        "enabled": policy.enabled,
        "window_ms": policy.window_ms,
        "kinds": policy.kinds,
        "strategy": policy.strategy,
        "dreaming": policy.dreaming,
        "advisory": true,
        "agent_lifecycle_control":"collab.update with current expected_revision",
        "dreaming_execution":"Only the live agent lifecycle round can request maintenance; this policy is advisory.",
        "retention_ms": RETAIN_FOR.as_millis(),
        "max_policies": MAX_POLICIES,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(agent: &str) -> Scope {
        Scope {
            owner: "owner".into(),
            client_id: "client".into(),
            agent_id: agent.into(),
        }
    }

    #[test]
    fn defaults_do_not_allocate_and_partial_updates_keep_other_preferences() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let defaults = registry.access(scope("main"), None, now).unwrap();
        assert!(registry.entries.is_empty());
        assert!(defaults.enabled);
        assert_eq!(defaults.window_ms, crate::wire::MAX_OBSERVATION_WAIT_MS);
        let edited = registry
            .access(
                scope("main"),
                Some(&json!({"window_ms":4000,"strategy":"Review completed work first"})),
                now,
            )
            .unwrap();
        let disabled = registry
            .access(scope("main"), Some(&json!({"enabled":false})), now)
            .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.window_ms, 4000);
        assert_eq!(disabled.strategy, edited.strategy);
        assert_eq!(disabled.kinds, defaults.kinds);
        let restored = registry
            .access(
                scope("main"),
                Some(&json!({"enabled":true,"strategy":null})),
                now,
            )
            .unwrap();
        assert!(restored.enabled);
        assert_eq!(restored.strategy, None);
    }

    #[test]
    fn dreaming_round_is_explicit_and_partial_updates_preserve_it() {
        let mut registry = Registry::default();
        let now = Instant::now();
        assert!(
            registry
                .access(scope("main"), None, now)
                .unwrap()
                .dreaming
                .is_none()
        );
        let patch = json!({"dreaming":{
            "round_id":"tools-round-1","repository":"project","budget_ms":1000
        }});
        let first = registry.access(scope("main"), Some(&patch), now).unwrap();
        assert_eq!(first.dreaming.as_ref().unwrap().budget_ms, 1000);
        assert!(
            registry
                .access(scope("worker"), None, now)
                .unwrap()
                .dreaming
                .is_none()
        );
        let edited = registry
            .access(scope("main"), Some(&json!({"window_ms":4000})), now)
            .unwrap();
        assert_eq!(edited.dreaming, first.dreaming);
        let repeated = registry.access(scope("main"), Some(&patch), now).unwrap();
        assert_eq!(repeated, edited);
        let cleared = registry
            .access(scope("main"), Some(&json!({"dreaming":null})), now)
            .unwrap();
        assert!(cleared.dreaming.is_none());
        assert_eq!(cleared.window_ms, 4000);
        let global = registry
            .access(
                scope("main"),
                Some(&json!({"dreaming":{
                    "round_id":"tools-round-2","repository":"global",
                    "budget_ms":crate::wire::MAX_OBSERVATION_WAIT_MS
                }})),
                now,
            )
            .unwrap();
        assert_eq!(global.dreaming.as_ref().unwrap().repository, "global");
    }

    #[test]
    fn invalid_dreaming_rounds_preserve_the_previous_policy() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let round = json!({"round_id":"round-1","repository":"project","budget_ms":1000});
        let previous = registry
            .access(scope("main"), Some(&json!({"dreaming":round})), now)
            .unwrap();
        let mut invalid = vec![json!({}), json!(false)];
        for (field, value) in [
            ("round_id", json!("")),
            ("round_id", json!("r".repeat(129))),
            ("round_id", json!("round 1")),
            ("round_id", json!("维护")),
            ("repository", json!("official")),
            ("repository", json!("/arbitrary/path")),
            ("budget_ms", json!(999)),
            ("budget_ms", json!(crate::wire::MAX_OBSERVATION_WAIT_MS + 1)),
            ("budget_ms", json!(-1)),
            ("budget_ms", json!("1000")),
            ("extra", json!(true)),
        ] {
            let mut candidate = round.clone();
            candidate[field] = value;
            invalid.push(candidate);
        }
        for candidate in invalid {
            assert!(
                registry
                    .access(
                        scope("main"),
                        Some(&json!({
                            "window_ms":2000,"dreaming":candidate
                        })),
                        now
                    )
                    .is_err()
            );
            assert_eq!(registry.access(scope("main"), None, now).unwrap(), previous);
        }
    }

    #[test]
    fn scope_isolates_accounts_clients_and_agents() {
        let mut registry = Registry::default();
        let now = Instant::now();
        registry
            .access(scope("main"), Some(&json!({"enabled":false})), now)
            .unwrap();
        let mut other_owner = scope("main");
        other_owner.owner = "other".into();
        let mut other_client = scope("main");
        other_client.client_id = "other".into();
        for other in [other_owner, other_client, scope("worker")] {
            assert!(registry.access(other, None, now).unwrap().enabled);
        }
    }

    #[test]
    fn invalid_patches_leave_the_previous_policy_intact() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let previous = registry
            .access(scope("main"), Some(&json!({"enabled":false})), now)
            .unwrap();
        for patch in [
            json!({"enabled":null}),
            json!({"window_ms":55001}),
            json!({"window_ms":999}),
            json!({"kinds":[]}),
            json!({"kinds":["*"]}),
            json!({"kinds":["worker.progress","worker.progress"]}),
            json!({"kinds":["wait.*"]}),
            json!({"strategy":"\n"}),
            json!({"strategy":"x".repeat(513)}),
            json!({"token":"private"}),
            json!([]),
        ] {
            assert!(
                registry.access(scope("main"), Some(&patch), now).is_err(),
                "accepted {patch}"
            );
            assert_eq!(registry.access(scope("main"), None, now).unwrap(), previous);
        }
    }

    #[test]
    fn live_policies_are_not_evicted_to_make_capacity() {
        let mut registry = Registry::default();
        let now = Instant::now();
        for index in 0..MAX_POLICIES {
            registry
                .access(
                    scope(&index.to_string()),
                    Some(&json!({"enabled":false})),
                    now,
                )
                .unwrap();
        }
        assert!(
            registry
                .access(scope("extra"), Some(&json!({})), now)
                .unwrap_err()
                .starts_with("coordination_policy_capacity")
        );
        assert_eq!(registry.entries.len(), MAX_POLICIES);
        assert!(!registry.access(scope("0"), None, now).unwrap().enabled);
        registry
            .access(scope("extra"), Some(&json!({})), now + RETAIN_FOR)
            .unwrap();
        assert_eq!(registry.entries.len(), 1);
    }

    #[test]
    fn identical_updates_are_stable_and_kinds_are_normalized() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let patch = json!({"kinds":["worker.*","custom.done","user.input_available"],"strategy":"  Inspect receipts  "});
        let first = registry.access(scope("main"), Some(&patch), now).unwrap();
        let second = registry
            .access(scope("main"), Some(&patch), now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.strategy.as_deref(), Some("Inspect receipts"));
        assert_eq!(
            first.kinds,
            ["custom.done", "user.input_available", "worker.*"]
        );
    }
}
