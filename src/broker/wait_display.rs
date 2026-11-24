//! Coalesced presentation for already-admitted public awaits. No Call, native
//! permit, tool audit, user queue or completion ledger is touched here.
use super::{Broker, Session};
use crate::{
    code_mode::RunBinding,
    wire::wait_display::{
        self, DurableObservation, Kind, Mode, Observation, Outcome, Snapshot, Version,
    },
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};
use tokio::sync::watch;

pub(crate) struct Request<'a> {
    pub user: &'a str,
    pub token: &'a str,
    pub binding: &'a RunBinding,
    pub wait_id: &'a str,
    pub operation: &'static str,
    pub targets: Vec<Kind>,
    pub target_labels: Vec<String>,
    pub target_details: Vec<String>,
    pub mode: Mode,
    pub timeout_ms: u64,
}

// Private credentials never derive Debug/Serialize and never enter a frame.
struct Authority {
    user: String,
    token: String,
    binding: RunBinding,
    connection: Weak<Session>,
}

impl Authority {
    fn valid(&self, broker: &Broker, session: &Arc<Session>) -> bool {
        if !session.is_alive()
            || self
                .connection
                .upgrade()
                .is_none_or(|original| !Arc::ptr_eq(&original, session))
        {
            return false;
        }
        let Some(authorization) = broker.authenticate_now(&self.user, &self.token) else {
            return false;
        };
        authorization.is_live()
            && authorization.permits(session)
            && crate::bridge::owner(&self.user, &authorization).as_deref()
                == Ok(self.binding.owner.as_str())
            && session.validate_binding(Some(&self.binding)).is_ok()
    }
}

struct Record {
    authority: Authority,
    observation: Observation,
    started: Instant,
    deadline: Instant,
    ended: Option<Instant>,
}

#[derive(Default)]
struct State {
    revision: u64,
    episode: u64,
    closed: bool,
    active: BTreeMap<String, Record>,
    recent: VecDeque<Record>,
}

pub(crate) struct Hub {
    version: Option<Version>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Hub {
    #[cfg(test)]
    pub(super) fn new(supported: bool) -> Arc<Self> {
        Self::new_version(supported.then_some(Version::V3))
    }

    pub(super) fn new_version(version: Option<Version>) -> Arc<Self> {
        Arc::new(Self {
            version,
            state: Mutex::new(State::default()),
            changed: watch::channel(0).0,
        })
    }

    pub(super) fn subscribe(&self) -> watch::Receiver<u64> {
        let mut receiver = self.changed.subscribe();
        // The session is visible just before Ready is written. A caller may
        // admit an await before the socket writer subscribes; replay only the
        // current presentation snapshot, never the underlying operation.
        receiver.mark_changed();
        receiver
    }

    fn changed(&self, state: &mut State) {
        state.revision = state.revision.saturating_add(1);
        self.changed.send_replace(state.revision);
    }

    fn begin(
        self: &Arc<Self>,
        broker: &Broker,
        session: &Arc<Session>,
        request: Request<'_>,
        durable: Option<DurableObservation>,
    ) -> Option<Lease> {
        let version = self.version?;
        let bound = if durable.is_some() {
            if version != Version::V4 {
                return None;
            }
            wait_display::MAX_LIFETIME_MS
        } else {
            wait_display::MAX_WAIT_MS
        };
        if !(1..=bound).contains(&request.timeout_ms) {
            return None;
        }
        let authority = Authority {
            user: request.user.into(),
            token: request.token.into(),
            binding: request.binding.clone(),
            connection: Arc::downgrade(session),
        };
        if !authority.valid(broker, session) {
            return None;
        }
        let authorization = broker.authenticate_now(request.user, request.token)?;
        let attribution = session
            .control_attribution(Some(&authorization), Some(request.binding))
            .ok()??;
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.active.len() >= wait_display::MAX_ACTIVE {
            return None;
        }
        let episode = state.episode.checked_add(1)?;
        let elapsed_ms = durable.as_ref().map_or(0, |value| value.elapsed_total_ms);
        let observation = Observation {
            wait_id: request.wait_id.into(),
            episode,
            attribution,
            operation: request.operation.into(),
            targets: request.targets,
            target_labels: request.target_labels,
            target_details: request.target_details,
            mode: request.mode,
            outcome: Outcome::Pending,
            window_ms: request.timeout_ms,
            elapsed_ms,
            remaining_ms: request.timeout_ms.saturating_sub(elapsed_ms),
            durable,
        };
        Snapshot {
            revision: 1,
            active: vec![observation.clone()],
            recent: vec![],
        }
        .validate_for(version)
        .ok()?;
        // A superseding episode replaces only the same server-issued wait ID.
        // An older guard can no longer remove its replacement.
        state.episode = episode;
        state.active.insert(
            observation.wait_id.clone(),
            Record {
                authority,
                observation,
                started: now,
                deadline: now
                    + Duration::from_millis(request.timeout_ms.saturating_sub(elapsed_ms)),
                ended: None,
            },
        );
        self.changed(&mut state);
        Some(Lease {
            hub: Arc::downgrade(self),
            wait_id: request.wait_id.into(),
            episode,
        })
    }

    pub(super) fn refresh(&self, broker: &Broker, session: &Arc<Session>) {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let before = (state.active.len(), state.recent.len());
        // A deadline is not evidence that the awaited operation returned.
        // The owning future records its actual outcome; a receiver may show
        // the expired observation budget while that return is still pending.
        state
            .active
            .retain(|_, record| record.authority.valid(broker, session));
        state.recent.retain(|record| {
            record.authority.valid(broker, session)
                && record.ended.is_some_and(|ended| {
                    now.saturating_duration_since(ended)
                        < Duration::from_secs(if record.observation.durable.is_some() {
                            1800
                        } else {
                            300
                        })
                })
        });
        if before != (state.active.len(), state.recent.len()) {
            self.changed(&mut state);
        }
    }

    /// Revalidate at the socket writer's last synchronous decision point.
    pub(super) fn snapshot(&self, broker: &Broker, session: &Arc<Session>) -> Option<Snapshot> {
        self.version?;
        self.refresh(broker, session);
        let now = Instant::now();
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.revision == 0 {
            return None;
        }
        let active = state
            .active
            .values()
            .map(|record| {
                let mut item = record.observation.clone();
                let elapsed = now
                    .saturating_duration_since(record.started)
                    .as_millis()
                    .min(u128::from(u64::MAX)) as u64;
                if let Some(durable) = &mut item.durable {
                    item.elapsed_ms = durable.elapsed_total_ms.saturating_add(elapsed);
                    durable.elapsed_total_ms = item.elapsed_ms;
                } else {
                    item.elapsed_ms = elapsed.min(wait_display::MAX_WAIT_MS);
                }
                item.remaining_ms = record
                    .deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .min(u128::from(item.window_ms)) as u64;
                item
            })
            .collect();
        Some(Snapshot {
            revision: state.revision,
            active,
            recent: state
                .recent
                .iter()
                .map(|record| record.observation.clone())
                .collect(),
        })
    }

    fn finish(&self, wait_id: &str, episode: u64, outcome: Outcome) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state
            .active
            .get(wait_id)
            .is_some_and(|record| record.observation.episode == episode)
        {
            let mut record = state.active.remove(wait_id).expect("checked exact episode");
            let now = Instant::now();
            record.ended = Some(now);
            record.observation.outcome = outcome;
            record.observation.elapsed_ms =
                now.saturating_duration_since(record.started)
                    .as_millis()
                    .min(u128::from(wait_display::MAX_WAIT_MS)) as u64;
            record.observation.remaining_ms = record
                .deadline
                .saturating_duration_since(now)
                .as_millis()
                .min(u128::from(wait_display::MAX_WAIT_MS))
                as u64;
            if outcome == Outcome::WindowExpired {
                record.observation.remaining_ms = 0;
            }
            state.recent.push_back(record);
            while state.recent.len() > wait_display::MAX_ACTIVE {
                state.recent.pop_front();
            }
            self.changed(&mut state);
        }
    }

    fn update_durable(&self, wait_id: &str, episode: u64, metadata: DurableObservation) {
        if self.version != Some(Version::V4) || metadata.validate().is_err() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return;
        }
        let now = Instant::now();
        if state
            .active
            .get(wait_id)
            .is_some_and(|record| record.observation.episode == episode)
        {
            let mut record = state.active.remove(wait_id).expect("exact episode");
            record.observation.elapsed_ms = metadata.elapsed_total_ms;
            record.observation.remaining_ms = metadata
                .lifetime_ms
                .saturating_sub(metadata.elapsed_total_ms);
            record.observation.outcome = metadata
                .result_status
                .map_or(Outcome::Pending, |status| status.outcome());
            record.started = now;
            record.deadline = now + Duration::from_millis(record.observation.remaining_ms);
            let terminal = metadata.result_status.is_some();
            record.observation.durable = Some(metadata);
            if terminal {
                record.ended = Some(now);
                state.recent.push_back(record);
                while state.recent.len() > wait_display::MAX_ACTIVE {
                    state.recent.pop_front();
                }
            } else {
                state.active.insert(wait_id.into(), record);
            }
            self.changed(&mut state);
        } else if let Some(record) = state.recent.iter_mut().find(|record| {
            record.observation.wait_id == wait_id && record.observation.episode == episode
        }) {
            // Read/ACK updates preserve the original terminal episode and time.
            if metadata.result_status
                != record
                    .observation
                    .durable
                    .as_ref()
                    .and_then(|value| value.result_status)
            {
                return;
            }
            record.observation.durable = Some(metadata);
            self.changed(&mut state);
        }
    }

    pub(super) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        state.active.clear();
        state.recent.clear();
        self.changed(&mut state);
    }
}

pub(crate) struct Lease {
    hub: Weak<Hub>,
    wait_id: String,
    episode: u64,
}

impl Lease {
    pub(crate) fn update_durable(&self, metadata: DurableObservation) {
        if let Some(hub) = self.hub.upgrade() {
            hub.update_durable(&self.wait_id, self.episode, metadata);
        }
    }
    pub(crate) fn finish(self, outcome: Outcome) {
        if let Some(hub) = self.hub.upgrade() {
            hub.finish(&self.wait_id, self.episode, outcome);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(hub) = self.hub.upgrade() {
            hub.finish(&self.wait_id, self.episode, Outcome::ObservationStopped);
        }
    }
}

impl Session {
    pub(crate) fn show_wait(
        self: &Arc<Self>,
        broker: &Broker,
        request: Request<'_>,
    ) -> Option<Lease> {
        self.wait_display.begin(broker, self, request, None)
    }

    pub(crate) fn show_durable_wait(
        self: &Arc<Self>,
        broker: &Broker,
        request: Request<'_>,
        metadata: DurableObservation,
    ) -> Option<Lease> {
        self.wait_display
            .begin(broker, self, request, Some(metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{ControlAccount, ControlAgent, ControlAttribution};

    #[tokio::test]
    async fn late_display_guard_cannot_remove_new_episode_and_late_subscriber_sees_current_state() {
        // This private-state test isolates the presentation guard's ABA rule;
        // HTTP Session/Frame tests exercise actual admission and authentication.
        let hub = Hub::new(true);
        let wait_id = "await_owned";
        let now = Instant::now();
        {
            let mut state = hub.state.lock().unwrap();
            state.episode = 2;
            state.active.insert(
                wait_id.into(),
                Record {
                    authority: Authority {
                        user: "fixture".into(),
                        token: "private fixture".into(),
                        binding: RunBinding {
                            owner: "fixture".into(),
                            agent_id: "fixture".into(),
                            token: "fixture".into(),
                        },
                        connection: Weak::new(),
                    },
                    observation: Observation {
                        wait_id: wait_id.into(),
                        episode: 2,
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
                        targets: vec![Kind::Job],
                        target_labels: vec!["Background job returns".into()],
                        target_details: vec!["Requested job ID fixture".into()],
                        mode: Mode::Any,
                        outcome: Outcome::Pending,
                        window_ms: 1000,
                        elapsed_ms: 0,
                        remaining_ms: 1000,
                        durable: None,
                    },
                    started: now,
                    deadline: now + Duration::from_secs(1),
                    ended: None,
                },
            );
            hub.changed(&mut state);
        }
        let mut late = hub.subscribe();
        tokio::time::timeout(Duration::from_millis(100), late.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*late.borrow_and_update(), 1);
        drop(Lease {
            hub: Arc::downgrade(&hub),
            wait_id: wait_id.into(),
            episode: 1,
        });
        assert_eq!(
            hub.state.lock().unwrap().active[wait_id]
                .observation
                .episode,
            2
        );
        assert!(!late.has_changed().unwrap());
        drop(Lease {
            hub: Arc::downgrade(&hub),
            wait_id: wait_id.into(),
            episode: 2,
        });
        assert!(hub.state.lock().unwrap().active.is_empty());
        assert!(late.has_changed().unwrap());
        hub.close();
        assert!(hub.state.lock().unwrap().closed);
    }
}
