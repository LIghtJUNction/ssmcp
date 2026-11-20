//! Optional, observation-only relay presentation. Neither capability is inferred
//! from a version string. The parent must offer it and accept the Ready echo.
//! Old or self-only peers receive no display frames and advertise no time tools.
use super::ControlAttribution;
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "wait_display_v1";
/// The relay has actual time-only delivery guards, including internal probes.
pub const TIME_CAPABILITY: &str = "time_observation_v1";
pub const MAX_ACTIVE: usize = 32;
pub const MAX_TARGETS: usize = 8;
pub const MAX_WAIT_MS: u64 = 25_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Exec,
    Job,
    AgentStatus,
    AgentMessages,
    UserInput,
    Finish,
}

impl Kind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Exec => "execution",
            Self::Job => "job",
            Self::AgentStatus => "agent status",
            Self::AgentMessages => "agent messages",
            Self::UserInput => "user input",
            Self::Finish => "finish decision",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub wait_id: String,
    pub episode: u64,
    pub attribution: ControlAttribution,
    pub targets: Vec<Kind>,
    pub elapsed_ms: u64,
    /// The current public-await observation budget, not a predicted job finish.
    pub remaining_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub active: Vec<Observation>,
}

impl Snapshot {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.revision == 0 || self.active.len() > MAX_ACTIVE {
            return Err("invalid wait display revision or capacity".into());
        }
        for (index, entry) in self.active.iter().enumerate() {
            if entry.wait_id.is_empty()
                || entry.wait_id.len() > 128
                || !entry
                    .wait_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                || entry.episode == 0
                || entry.targets.is_empty()
                || entry.targets.len() > MAX_TARGETS
                || entry.elapsed_ms > MAX_WAIT_MS
                || entry.remaining_ms > MAX_WAIT_MS
                || self.active[..index]
                    .iter()
                    .any(|prior| prior.wait_id == entry.wait_id)
            {
                return Err("invalid bounded wait display observation".into());
            }
            entry.attribution.validate()?;
            if entry.attribution.agent.is_none() {
                return Err("wait display requires an authenticated participant".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ControlAccount, ControlAgent};

    fn observation() -> Observation {
        Observation {
            wait_id: "await_fixture".into(),
            episode: 1,
            attribution: ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: "42".into(),
                    login: "fixture".into(),
                },
                owner: None,
                agent: Some(ControlAgent {
                    id: format!("agent_{}", "a".repeat(32)),
                    name: "fixture".into(),
                }),
            },
            targets: vec![Kind::AgentStatus],
            elapsed_ms: 0,
            remaining_ms: 20_000,
        }
    }

    #[test]
    fn display_wire_is_bounded_typed_and_contains_no_result_or_authority_fields() {
        let valid = Snapshot {
            revision: 1,
            active: vec![observation()],
        };
        valid.validate().unwrap();
        let mut value = serde_json::to_value(&valid).unwrap();
        for key in [
            "token",
            "approved",
            "completion_permission",
            "result",
            "history_id",
        ] {
            value[key] = serde_json::json!(true);
            assert!(serde_json::from_value::<Snapshot>(value.clone()).is_err());
            value.as_object_mut().unwrap().remove(key);
        }
        for change in [0, 1, 2, 3, 4, 5] {
            let mut invalid = valid.clone();
            match change {
                0 => invalid.revision = 0,
                1 => invalid.active[0].episode = 0,
                2 => invalid.active[0].remaining_ms = MAX_WAIT_MS + 1,
                3 => invalid.active[0].wait_id.push('\n'),
                4 => invalid.active[0].attribution.agent = None,
                _ => invalid.active.push(invalid.active[0].clone()),
            }
            assert!(invalid.validate().is_err());
        }
        let mut full = valid;
        full.active = (0..MAX_ACTIVE)
            .map(|index| {
                let mut entry = observation();
                entry.wait_id = format!("await_{index}");
                entry
            })
            .collect();
        full.validate().unwrap();
        full.active.push(observation());
        assert!(full.validate().is_err());
    }
}
