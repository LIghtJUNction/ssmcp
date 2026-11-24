//! Optional, observation-only relay presentation. Neither capability is inferred
//! from a version string. The parent must offer it and accept the Ready echo.
//! Old or self-only peers receive no display frames and advertise no time tools.
use super::ControlAttribution;
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "wait_display_v3";
pub const DURABLE_CAPABILITY: &str = "wait_display_v4";
pub const MAX_LIFETIME_MS: u64 = 86_400_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Version {
    V3,
    V4,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DurableState {
    Waiting,
    Cached,
    Acknowledged,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DurableStatus {
    Ready,
    TimedOut,
    Cancelled,
    Unauthorized,
    UnknownTarget,
    Unavailable,
    Busy,
}

/// Metadata only. A retrieval timestamp describes a server-side attempt, not
/// evidence that a remote client received a response. Only cursor ACK is explicit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DurableObservation {
    pub state: DurableState,
    pub lifetime_ms: u64,
    pub read_window_ms: u64,
    pub started_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_read_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledged_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retained_until_ms: Option<u64>,
    pub elapsed_total_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_lag_ms: Option<u64>,
    pub read_windows: u64,
    pub replay_count: u64,
    pub cursor: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_status: Option<DurableStatus>,
}

impl DurableStatus {
    pub(crate) fn outcome(self) -> Outcome {
        match self {
            Self::Ready => Outcome::Triggered,
            Self::TimedOut => Outcome::WindowExpired,
            Self::Cancelled => Outcome::ObservationStopped,
            _ => Outcome::Unknown,
        }
    }
}

impl DurableObservation {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let terminal = self.state != DurableState::Waiting;
        if !(1_000..=MAX_LIFETIME_MS).contains(&self.lifetime_ms)
            || !(1_000..=MAX_WAIT_MS).contains(&self.read_window_ms)
            || self.cursor != u64::from(terminal)
            || terminal != self.completed_at_ms.is_some()
            || terminal != self.result_status.is_some()
            || terminal != self.retained_until_ms.is_some()
            || (self.state == DurableState::Acknowledged) != self.acknowledged_at_ms.is_some()
            || self.first_read_at_ms.is_some() != self.last_read_at_ms.is_some()
            || self.first_read_at_ms.is_some() != self.delivery_lag_ms.is_some()
            || (!terminal && self.first_read_at_ms.is_some())
            || self.replay_count > self.read_windows
        {
            return Err("invalid durable wait metadata".into());
        }
        Ok(())
    }
}
/// The relay has actual time-only delivery guards, including internal probes.
pub const TIME_CAPABILITY: &str = "time_observation_v2";
pub const MAX_ACTIVE: usize = 32;
pub const MAX_TARGETS: usize = 8;
pub const MAX_WAIT_MS: u64 = super::MAX_OBSERVATION_WAIT_MS;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Exec,
    Job,
    AgentStatus,
    AgentMessages,
    Event,
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
            Self::Event => "events",
            Self::UserInput => "user input",
            Self::Finish => "finish decision",
        }
    }
}

/// The condition and outcome describe only this observation window.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Any,
    All,
    Single,
    Change,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    Triggered,
    WindowExpired,
    ObservationStopped,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub wait_id: String,
    pub episode: u64,
    pub attribution: ControlAttribution,
    pub operation: String,
    pub targets: Vec<Kind>,
    pub target_labels: Vec<String>,
    pub target_details: Vec<String>,
    pub mode: Mode,
    pub outcome: Outcome,
    /// This call's observation budget, never an execution deadline.
    pub window_ms: u64,
    pub elapsed_ms: u64,
    /// The current public-await observation budget, not a predicted job finish.
    pub remaining_ms: u64,
    /// Present only after explicit wait_display_v4 negotiation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durable: Option<DurableObservation>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub revision: u64,
    pub active: Vec<Observation>,
    #[serde(default)]
    pub recent: Vec<Observation>,
}

impl Snapshot {
    #[cfg(test)]
    pub(crate) fn validate(&self) -> Result<(), String> {
        self.validate_for(Version::V3)
    }

    pub(crate) fn validate_for(&self, version: Version) -> Result<(), String> {
        if self.revision == 0 || self.active.len() > MAX_ACTIVE || self.recent.len() > MAX_ACTIVE {
            return Err("invalid wait display revision or capacity".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut active_ids = std::collections::BTreeSet::new();
        for (active, entry) in self
            .active
            .iter()
            .map(|entry| (true, entry))
            .chain(self.recent.iter().map(|entry| (false, entry)))
        {
            let bound = if let Some(durable) = &entry.durable {
                if version != Version::V4 {
                    return Err("durable wait display was not negotiated".into());
                }
                durable.validate()?;
                if entry.window_ms != durable.lifetime_ms
                    || entry.elapsed_ms != durable.elapsed_total_ms
                    || active != (durable.state == DurableState::Waiting)
                    || entry.outcome
                        != durable
                            .result_status
                            .map_or(Outcome::Pending, DurableStatus::outcome)
                {
                    return Err("inconsistent durable wait observation".into());
                }
                MAX_LIFETIME_MS
            } else {
                MAX_WAIT_MS
            };
            if entry.wait_id.is_empty()
                || entry.wait_id.len() > 128
                || !entry
                    .wait_id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
                || entry.episode == 0
                || !matches!(entry.operation.as_str(), "await" | "events.wait")
                || entry.targets.is_empty()
                || entry.targets.len() > MAX_TARGETS
                || (entry.durable.is_none() && entry.elapsed_ms > MAX_WAIT_MS)
                || entry.remaining_ms > bound
                || entry.window_ms == 0
                || entry.window_ms > bound
                || entry.remaining_ms > entry.window_ms
                || entry.target_labels.len() != entry.targets.len()
                || entry.target_details.len() != entry.targets.len()
                || entry
                    .target_details
                    .iter()
                    .any(|detail| detail.len() > 4096 || detail.chars().any(char::is_control))
                || entry.target_labels.iter().any(|label| {
                    label.is_empty() || label.len() > 256 || label.chars().any(char::is_control)
                })
                || active != (entry.outcome == Outcome::Pending)
                || !seen.insert((&entry.wait_id, entry.episode))
                || (active && !active_ids.insert(entry.wait_id.as_str()))
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
            operation: "await".into(),
            targets: vec![Kind::AgentStatus],
            target_labels: vec!["Member reaches the requested status".into()],
            target_details: vec!["Requested member status must be ready".into()],
            mode: Mode::Any,
            outcome: Outcome::Pending,
            window_ms: 20_000,
            elapsed_ms: 0,
            remaining_ms: 20_000,
            durable: None,
        }
    }

    #[test]
    fn display_wire_is_bounded_typed_and_contains_no_result_or_authority_fields() {
        let valid = Snapshot {
            revision: 1,
            active: vec![observation()],
            recent: vec![],
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
        let mut short = valid.clone();
        short.active[0].window_ms = 1;
        short.active[0].remaining_ms = 1;
        short.validate().unwrap();
        short.active[0].window_ms = 0;
        short.active[0].remaining_ms = 0;
        assert!(short.validate().is_err());
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
    #[test]
    fn outcomes_are_typed_and_terminal_receipts_cannot_masquerade_as_active_waits() {
        let mut receipt = observation();
        receipt.mode = Mode::All;
        receipt.outcome = Outcome::WindowExpired;
        receipt.elapsed_ms = 20_000;
        receipt.remaining_ms = 0;
        let valid = Snapshot {
            revision: 2,
            active: vec![],
            recent: vec![receipt.clone()],
        };
        valid.validate().unwrap();
        let restored: Snapshot =
            serde_json::from_value(serde_json::to_value(&valid).unwrap()).unwrap();
        assert_eq!(restored.recent[0].mode, Mode::All);
        assert_eq!(restored.recent[0].outcome, Outcome::WindowExpired);
        let invalid = Snapshot {
            revision: 3,
            active: vec![receipt],
            recent: vec![],
        };
        assert!(invalid.validate().is_err());
        let mut invalid = Snapshot {
            revision: 4,
            active: vec![observation()],
            recent: vec![],
        };
        invalid.active[0].target_labels.clear();
        assert!(invalid.validate().is_err());
        let mut bad = serde_json::to_value(valid).unwrap();
        bad["recent"][0]["message_body"] = serde_json::json!("private text");
        assert!(serde_json::from_value::<Snapshot>(bad).is_err());
        assert_eq!(CAPABILITY, "wait_display_v3");
    }

    #[test]
    fn durable_metadata_requires_v4_and_keeps_real_elapsed_and_read_receipts_distinct() {
        let mut entry = observation();
        assert!(
            serde_json::to_value(&entry)
                .unwrap()
                .get("durable")
                .is_none()
        );
        entry.window_ms = 600_000;
        entry.elapsed_ms = 95_000;
        entry.remaining_ms = 505_000;
        entry.durable = Some(DurableObservation {
            state: DurableState::Waiting,
            lifetime_ms: 600_000,
            read_window_ms: 1000,
            started_at_ms: 1_000_000,
            completed_at_ms: None,
            first_read_at_ms: None,
            last_read_at_ms: None,
            acknowledged_at_ms: None,
            retained_until_ms: None,
            elapsed_total_ms: 95_000,
            delivery_lag_ms: None,
            read_windows: 3,
            replay_count: 0,
            cursor: 0,
            result_status: None,
        });
        let mut snapshot = Snapshot {
            revision: 1,
            active: vec![entry],
            recent: vec![],
        };
        assert!(snapshot.validate().is_err());
        snapshot.validate_for(Version::V4).unwrap();
        let mut finished = snapshot.active.pop().unwrap();
        finished.outcome = Outcome::Triggered;
        let durable = finished.durable.as_mut().unwrap();
        durable.state = DurableState::Cached;
        durable.completed_at_ms = Some(1_095_000);
        durable.retained_until_ms = Some(2_895_000);
        durable.result_status = Some(DurableStatus::Ready);
        durable.cursor = 1;
        snapshot.recent.push(finished);
        snapshot.validate_for(Version::V4).unwrap();
        let durable = snapshot.recent[0].durable.as_mut().unwrap();
        durable.first_read_at_ms = Some(1_097_000);
        durable.last_read_at_ms = durable.first_read_at_ms;
        durable.delivery_lag_ms = Some(2000);
        durable.state = DurableState::Acknowledged;
        durable.acknowledged_at_ms = Some(1_098_000);
        snapshot.validate_for(Version::V4).unwrap();
        assert_eq!(snapshot.recent[0].elapsed_ms, 95_000);
        let mut bad = serde_json::to_value(&snapshot).unwrap();
        bad["recent"][0]["durable"]["body"] = serde_json::json!("private");
        assert!(serde_json::from_value::<Snapshot>(bad).is_err());
        snapshot.recent[0].durable.as_mut().unwrap().cursor = 0;
        assert!(snapshot.validate_for(Version::V4).is_err());
    }
}
