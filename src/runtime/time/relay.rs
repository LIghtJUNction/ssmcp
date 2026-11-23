//! Read-only projection of negotiated server waits. This module never captures
//! a Source/HistoryOwner, calls a tool, restores a task or changes any queue.
use super::{Entry, Status, clean_label, millis};
use crate::wire::wait_display;
use std::time::{Duration, Instant};

struct Observation {
    wire: wait_display::Observation,
    received: Instant,
    deadline: Instant,
}

#[derive(Default)]
pub(super) struct State {
    enabled: bool,
    revision: u64,
    active: Vec<Observation>,
}

impl State {
    pub(super) fn configure(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.revision = 0;
        self.active.clear();
    }

    pub(super) fn apply(
        &mut self,
        snapshot: wait_display::Snapshot,
        now: Instant,
    ) -> Result<bool, String> {
        if !self.enabled {
            return Err("wait display capability was not negotiated".into());
        }
        snapshot.validate()?;
        if snapshot.revision <= self.revision {
            return Ok(false);
        }
        self.revision = snapshot.revision;
        self.active = snapshot
            .active
            .into_iter()
            .map(|wire| Observation {
                deadline: now + Duration::from_millis(wire.remaining_ms),
                received: now,
                wire,
            })
            .collect();
        Ok(true)
    }

    pub(super) fn entries(&mut self, now: Instant) -> Vec<Entry> {
        // A delayed/dropped final snapshot cannot leave a zombie countdown.
        self.active.retain(|observation| now < observation.deadline);
        self.active
            .iter()
            .map(|observation| {
                let wire = &observation.wire;
                let actor = wire
                    .attribution
                    .agent
                    .as_ref()
                    .expect("validated participant");
                let kind = wire.targets.first().expect("validated target").label();
                let label = format!(
                    "Relay {} · {}@{}{}",
                    kind,
                    actor.name,
                    wire.attribution.account.login,
                    if wire.targets.len() == 1 {
                        String::new()
                    } else {
                        format!(" · +{} targets", wire.targets.len() - 1)
                    }
                );
                // The display label is bounded independently from validated wire
                // identity; account/agent provenance remains structured metadata.
                let label: String = label.chars().take(80).collect();
                let label = clean_label(Some(label))
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "Relay wait".into());
                Entry {
                    id: wire.wait_id.clone(),
                    kind: "relay_wait",
                    label,
                    status: Status::Running,
                    elapsed_ms: wire.elapsed_ms.saturating_add(millis(
                        now.saturating_duration_since(observation.received),
                    )),
                    remaining_ms: Some(super::remaining_millis(observation.deadline, now)),
                    deadline_unix_ms: None,
                    finished_age_ms: None,
                    cancellable: false,
                    episode: Some(wire.episode),
                    attribution: Some(wire.attribution.clone()),
                }
            })
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
                targets: vec![wait_display::Kind::AgentStatus],
                elapsed_ms: 100,
                remaining_ms: 2000,
            }],
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
                        active: vec![]
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
        snapshot.active[0].remaining_ms = crate::wire::MAX_OBSERVATION_WAIT_MS;
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
}
