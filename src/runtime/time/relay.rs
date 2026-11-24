//! Read-only projection of negotiated server waits. This module never captures
//! a Source/HistoryOwner, calls a tool, restores a task or changes any queue.
use super::{Entry, WaitView, clean_label, millis};
use crate::wire::wait_display::{self, Outcome, Version};
use std::time::{Duration, Instant};

struct Observation {
    wire: wait_display::Observation,
    received: Instant,
    deadline: Instant,
    ended: Option<Instant>,
}

impl Observation {
    fn entry(&self, now: Instant) -> Entry {
        let wire = &self.wire;
        let actor = wire
            .attribution
            .agent
            .as_ref()
            .expect("validated participant");
        let kind = wire.targets.first().expect("validated target").label();
        let label = format!(
            "Relay {kind} · {}@{}{}",
            actor.name,
            wire.attribution.account.login,
            if wire.targets.len() == 1 {
                String::new()
            } else {
                format!(" · +{} targets", wire.targets.len() - 1)
            }
        );
        let label: String = label.chars().take(80).collect();
        let label = clean_label(Some(label))
            .ok()
            .flatten()
            .unwrap_or_else(|| "Relay wait".into());
        let view = WaitView {
            operation: wire.operation.clone(),
            mode: wire.mode,
            targets: wire.target_labels.clone(),
            // Only bounded declared references and authenticated public
            // metadata arrive here, never result/private-message bodies.
            references: wire
                .target_details
                .iter()
                .filter(|text| !text.is_empty())
                .cloned()
                .collect(),
            outcome: wire.outcome,
            window_ms: Some(wire.window_ms),
        };
        let active = self.ended.is_none();
        Entry {
            id: wire.wait_id.clone(),
            kind: "relay_wait",
            label,
            status: view.status(),
            elapsed_ms: if active {
                wire.elapsed_ms
                    .saturating_add(millis(now.saturating_duration_since(self.received)))
            } else {
                wire.elapsed_ms
            },
            remaining_ms: Some(if active {
                super::remaining_millis(self.deadline, now)
            } else {
                wire.remaining_ms
            }),
            deadline_unix_ms: None,
            finished_age_ms: self
                .ended
                .map(|ended| millis(now.saturating_duration_since(ended))),
            cancellable: false,
            episode: Some(wire.episode),
            attribution: Some(wire.attribution.clone()),
            wait: Some(view),
            durable: wire.durable.clone(),
        }
    }

    fn same(&self, wire: &wait_display::Observation) -> bool {
        self.wire.wait_id == wire.wait_id && self.wire.episode == wire.episode
    }
}

#[derive(Default)]
pub(super) struct State {
    version: Option<Version>,
    revision: u64,
    active: Vec<Observation>,
    recent: Vec<Observation>,
}

impl State {
    pub(super) fn configure(&mut self, enabled: bool) {
        self.configure_version(enabled.then_some(Version::V3));
    }

    pub(super) fn configure_version(&mut self, version: Option<Version>) {
        self.version = version;
        self.revision = 0;
        self.active.clear();
        self.recent.clear();
    }

    fn retain(&mut self, mut observation: Observation, now: Instant) {
        if let Some(index) = self
            .recent
            .iter()
            .position(|prior| prior.same(&observation.wire))
        {
            observation.ended = self.recent[index].ended;
            self.recent.remove(index);
        }
        observation.ended.get_or_insert(now);
        self.recent.push(observation);
        if self.recent.len() > wait_display::MAX_ACTIVE {
            self.recent.remove(0);
        }
    }

    pub(super) fn apply(
        &mut self,
        snapshot: wait_display::Snapshot,
        now: Instant,
    ) -> Result<bool, String> {
        let Some(version) = self.version else {
            return Err("wait display capability was not negotiated".into());
        };
        snapshot.validate_for(version)?;
        if snapshot.revision <= self.revision {
            return Ok(false);
        }
        self.revision = snapshot.revision;
        for mut old in std::mem::take(&mut self.active) {
            if !snapshot.active.iter().any(|wire| old.same(wire))
                && !snapshot.recent.iter().any(|wire| old.same(wire))
            {
                // Disappearance is not evidence of success or cancellation.
                old.wire.outcome = Outcome::Unknown;
                old.wire.elapsed_ms = old.entry(now).elapsed_ms;
                self.retain(old, now);
            }
        }
        self.active = snapshot
            .active
            .into_iter()
            .map(|wire| Observation {
                deadline: now + Duration::from_millis(wire.remaining_ms),
                received: now,
                wire,
                ended: None,
            })
            .collect();
        for wire in snapshot.recent {
            let observation = Observation {
                deadline: now,
                received: now,
                wire,
                ended: Some(now),
            };
            self.retain(observation, now);
        }
        Ok(true)
    }

    fn expire(&mut self, now: Instant) {
        let mut active = Vec::new();
        for mut observation in std::mem::take(&mut self.active) {
            if now < observation.deadline {
                active.push(observation);
            } else {
                // A lost final frame cannot leave a zombie countdown.
                // The local budget ending is not evidence of the server's
                // actual return. Only its typed receipt can establish expiry.
                observation.wire.elapsed_ms = observation.wire.elapsed_ms.saturating_add(millis(
                    observation
                        .deadline
                        .saturating_duration_since(observation.received),
                ));
                observation.wire.remaining_ms = 0;
                observation.wire.outcome = Outcome::Unknown;
                observation.ended = Some(observation.deadline);
                self.retain(observation, now);
            }
        }
        self.active = active;
        self.recent.retain(|observation| {
            observation.ended.is_some_and(|ended| {
                // This is a bounded local display window, not a claim about
                // the server cache deadline or synchronized wall clocks.
                let seconds = if observation.wire.durable.is_some() {
                    1800
                } else {
                    300
                };
                now.saturating_duration_since(ended) < Duration::from_secs(seconds)
            })
        });
    }

    pub(super) fn entries(&mut self, now: Instant) -> Vec<Entry> {
        self.expire(now);
        self.active
            .iter()
            .map(|observation| observation.entry(now))
            .collect()
    }

    pub(super) fn recent_entries(&mut self, now: Instant) -> Vec<Entry> {
        self.expire(now);
        self.recent
            .iter()
            .map(|observation| observation.entry(now))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::interface::history;
    use crate::wire::{ControlAccount, ControlAgent, ControlAttribution};

    fn frame(revision: u64, episode: u64) -> wait_display::Snapshot {
        wait_display::Snapshot {
            revision,
            active: vec![wait_display::Observation {
                wait_id: "await_owned".into(),
                episode,
                attribution: ControlAttribution {
                    account: ControlAccount {
                        provider: "github".into(),
                        subject: "42".into(),
                        login: "fixture".into(),
                    },
                    owner: None,
                    agent: Some(ControlAgent {
                        id: format!("agent_{}", "a".repeat(32)),
                        name: "Owned observer".into(),
                    }),
                },
                operation: "await".into(),
                targets: vec![wait_display::Kind::AgentStatus],
                target_labels: vec!["Member reaches the requested status".into()],
                target_details: vec!["Requested member status must be ready".into()],
                mode: wait_display::Mode::Any,
                outcome: wait_display::Outcome::Pending,
                window_ms: 2100,
                elapsed_ms: 100,
                remaining_ms: 2000,
                durable: None,
            }],
            recent: vec![],
        }
    }

    #[test]
    fn only_negotiated_fresh_snapshots_change_local_monotonic_display() {
        let mut state = State::default();
        let now = Instant::now();
        assert!(state.apply(frame(1, 1), now).is_err());
        state.configure(true);
        assert!(state.apply(frame(1, 1), now).unwrap());
        let entry = state.entries(now + Duration::from_millis(750)).remove(0);
        assert_eq!(
            (entry.elapsed_ms, entry.remaining_ms, entry.episode),
            (850, Some(1250), Some(1))
        );
        assert!(!entry.cancellable);
        assert_eq!(entry.kind, "relay_wait");
        assert!(state.apply(frame(3, 2), now).unwrap());
        assert!(
            !state
                .apply(
                    wait_display::Snapshot {
                        revision: 2,
                        active: vec![],
                        recent: vec![],
                    },
                    now
                )
                .unwrap()
        );
        assert_eq!(state.entries(now)[0].episode, Some(2));
        assert!(state.entries(now + Duration::from_secs(2)).is_empty());
        assert!(!state.apply(frame(3, 2), now).unwrap());
        assert!(state.entries(now).is_empty());
        state.configure(false);
        assert!(state.entries(now).is_empty());
        assert!(state.apply(frame(4, 3), now).is_err());
    }

    #[test]
    fn negotiated_event_wait_accepts_fifty_five_seconds_and_expires_locally() {
        let mut state = State::default();
        state.configure(true);
        let now = Instant::now();
        let mut snapshot = frame(1, 1);
        snapshot.active[0].targets = vec![wait_display::Kind::Event];
        snapshot.active[0].target_labels = vec!["Events in the authorized subscription".into()];
        snapshot.active[0].remaining_ms = crate::wire::MAX_OBSERVATION_WAIT_MS;
        snapshot.active[0].window_ms = crate::wire::MAX_OBSERVATION_WAIT_MS;
        assert!(state.apply(snapshot, now).unwrap());
        let entry = state.entries(now).remove(0);
        assert_eq!(
            entry.remaining_ms,
            Some(crate::wire::MAX_OBSERVATION_WAIT_MS)
        );
        assert!(entry.label.starts_with("Relay events"));
        assert!(
            state
                .entries(now + Duration::from_millis(crate::wire::MAX_OBSERVATION_WAIT_MS))
                .is_empty()
        );
        let mut invalid = frame(2, 2);
        invalid.active[0].remaining_ms = crate::wire::MAX_OBSERVATION_WAIT_MS + 1;
        assert!(state.apply(invalid, now).is_err());
    }

    #[tokio::test]
    async fn relay_projection_does_not_create_or_bind_a_conversation_and_disconnect_clears_it() {
        let clock = super::super::Clock::new().unwrap();
        let directory = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "ssmcp-relay-display-{}-{}",
            std::process::id(),
            clock.id
        ));
        crate::platform::private_directory(&directory).unwrap();
        history::with_test_history(directory.clone(), async {
            assert!(history::current_owner().is_none());
            clock.configure(false, true);
            clock.relay_display(frame(1, 1)).unwrap();
            let snapshot = clock.snapshot(&history::current_owner(), Instant::now());
            assert_eq!(snapshot.active.len(), 1);
            assert_eq!(snapshot.active[0].kind, "relay_wait");
            assert_eq!(
                snapshot.active[0]
                    .attribution
                    .as_ref()
                    .unwrap()
                    .account
                    .subject,
                "42"
            );
            assert!(
                history::current_owner().is_none(),
                "display ingress must not capture a HistoryOwner"
            );
            clock.disconnect();
            assert!(clock.snapshot(&None, Instant::now()).active.is_empty());
            assert!(clock.relay_display(frame(2, 2)).is_err());
            assert!(history::current_owner().is_none());
        })
        .await;
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn missing_receipt_trigger_and_disappearance_preserve_distinct_observation_outcomes() {
        let mut state = State::default();
        state.configure(true);
        let now = Instant::now();
        state.apply(frame(1, 1), now).unwrap();
        assert!(state.entries(now + Duration::from_secs(2)).is_empty());
        let expired = state.recent_entries(now + Duration::from_secs(2));
        assert_eq!(expired[0].wait.as_ref().unwrap().outcome, Outcome::Unknown);
        assert_eq!(expired[0].status, super::super::Status::Unknown);

        let mut receipt = frame(2, 1).active.remove(0);
        receipt.outcome = Outcome::Triggered;
        receipt.elapsed_ms = 1900;
        receipt.remaining_ms = 100;
        state
            .apply(
                wait_display::Snapshot {
                    revision: 2,
                    active: vec![],
                    recent: vec![receipt],
                },
                now + Duration::from_secs(2),
            )
            .unwrap();
        let actual = state.recent_entries(now + Duration::from_secs(2));
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].wait.as_ref().unwrap().outcome, Outcome::Triggered);
        assert_eq!(actual[0].status, super::super::Status::Returned);

        state
            .apply(frame(3, 2), now + Duration::from_secs(3))
            .unwrap();
        state
            .apply(
                wait_display::Snapshot {
                    revision: 4,
                    active: vec![],
                    recent: vec![],
                },
                now + Duration::from_secs(3),
            )
            .unwrap();
        assert!(
            state
                .recent_entries(now + Duration::from_secs(3))
                .iter()
                .any(|entry| entry.episode == Some(2)
                    && entry.wait.as_ref().unwrap().outcome == Outcome::Unknown)
        );
    }

    fn durable_frame(revision: u64) -> wait_display::Snapshot {
        use wait_display::{DurableObservation, DurableState};
        let mut snapshot = frame(revision, 1);
        let wire = &mut snapshot.active[0];
        wire.window_ms = 3_600_000;
        wire.remaining_ms = 3_599_900;
        wire.durable = Some(DurableObservation {
            state: DurableState::Waiting,
            result_status: None,
            started_at_ms: 1_795_000_000_000,
            completed_at_ms: None,
            first_read_at_ms: None,
            last_read_at_ms: None,
            acknowledged_at_ms: None,
            delivery_lag_ms: None,
            retained_until_ms: None,
            elapsed_total_ms: 100,
            lifetime_ms: 3_600_000,
            read_window_ms: 55_000,
            read_windows: 1,
            replay_count: 0,
            cursor: 0,
        });
        snapshot
    }

    #[test]
    fn durable_display_requires_v4_and_outlives_the_read_window_without_faking_a_result() {
        let mut state = State::default();
        let now = Instant::now();
        state.configure(true);
        assert!(state.apply(durable_frame(1), now).is_err());
        state.configure_version(Some(Version::V4));
        assert!(state.apply(durable_frame(1), now).unwrap());
        let entry = state.entries(now + Duration::from_secs(60)).remove(0);
        assert_eq!(entry.elapsed_ms, 60_100);
        assert_eq!(entry.remaining_ms, Some(3_539_900));
        assert_eq!(entry.durable.as_ref().unwrap().read_window_ms, 55_000);
        assert_eq!(
            entry.durable.as_ref().unwrap().elapsed_total_ms,
            100,
            "do not rewrite authoritative server measurements during local display projection"
        );
        state
            .apply(
                wait_display::Snapshot {
                    revision: 2,
                    active: vec![],
                    recent: vec![],
                },
                now + Duration::from_secs(61),
            )
            .unwrap();
        let missing = state
            .recent_entries(now + Duration::from_secs(61))
            .remove(0);
        assert_eq!(missing.status, super::super::Status::Unknown);
        assert_eq!(
            missing.durable.unwrap().state,
            wait_display::DurableState::Waiting
        );
    }

    #[test]
    fn terminal_durable_metadata_and_ack_update_keep_actual_duration_and_bounded_retention() {
        use wait_display::{DurableState, DurableStatus};
        let mut state = State::default();
        state.configure_version(Some(Version::V4));
        let now = Instant::now();
        let mut wire = durable_frame(1).active.remove(0);
        wire.outcome = Outcome::Triggered;
        wire.elapsed_ms = 91_000;
        wire.remaining_ms = 3_509_000;
        let metadata = wire.durable.as_mut().unwrap();
        metadata.state = DurableState::Cached;
        metadata.result_status = Some(DurableStatus::Ready);
        metadata.elapsed_total_ms = 91_000;
        metadata.completed_at_ms = Some(1_795_000_091_000);
        metadata.retained_until_ms = Some(1_795_001_891_000);
        metadata.cursor = 1;
        state
            .apply(
                wait_display::Snapshot {
                    revision: 1,
                    active: vec![],
                    recent: vec![wire.clone()],
                },
                now,
            )
            .unwrap();
        assert_eq!(
            state.recent_entries(now + Duration::from_secs(301))[0].elapsed_ms,
            91_000
        );
        let metadata = wire.durable.as_mut().unwrap();
        metadata.state = DurableState::Acknowledged;
        metadata.first_read_at_ms = Some(1_795_000_092_000);
        metadata.last_read_at_ms = metadata.first_read_at_ms;
        metadata.acknowledged_at_ms = Some(1_795_000_093_000);
        metadata.delivery_lag_ms = Some(1000);
        state
            .apply(
                wait_display::Snapshot {
                    revision: 2,
                    active: vec![],
                    recent: vec![wire.clone()],
                },
                now + Duration::from_secs(302),
            )
            .unwrap();
        assert_eq!(
            state.recent_entries(now + Duration::from_secs(303))[0].durable,
            wire.durable
        );
        assert!(
            state
                .recent_entries(now + Duration::from_secs(1800))
                .is_empty()
        );
    }
}
