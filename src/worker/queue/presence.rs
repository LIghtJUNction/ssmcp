//! A station is bound to a specific collaboration participant for this pool
//! connection. Silence is display evidence only. Only a later, complete native
//! roster can establish that this exact participant no longer holds membership.
use super::*;
use std::time::Duration;

pub(in crate::worker) const MEMBERSHIP_ENDED: &str = "Participant membership ended";
pub(super) const LOST: &str =
    "Authenticated participant left or expired; external execution is unconfirmed";

pub(in crate::worker) const HUMAN_FORCE: &str = "Human force-retired station";
pub(super) fn reason(source: &str) -> &'static str {
    if source == HUMAN_FORCE {
        "Station force-retired by human; external execution is unconfirmed"
    } else {
        LOST
    }
}

#[derive(Clone, Copy)]
pub(super) struct Contact {
    at: Instant,
    direct: Instant,
    offline: bool,
}

impl Contact {
    pub(super) fn authenticated(at: Instant) -> Self {
        Self {
            at,
            direct: at,
            offline: false,
        }
    }

    pub(super) fn label(self, now: Instant) -> &'static str {
        if now.saturating_duration_since(self.at)
            >= Duration::from_millis(crate::wire::COLLAB_LEASE_MS)
        {
            "unverified"
        } else if self.offline {
            "declared_offline"
        } else if now.saturating_duration_since(self.direct)
            >= Duration::from_millis(2 * crate::worker::MAX_OBSERVATION_WAIT_MS)
        {
            "membership_only"
        } else {
            "confirmed"
        }
    }
}

impl Slot {
    pub(super) fn vacate_departed(&mut self) {
        self.registered = None;
        self.contact = None;
        // Resolve only the capacity requested by the human/manager. There is
        // no worker release receipt and no claim about an external process.
        if self.retirement.is_some() {
            self.last_retirement = self.retirement.take();
            self.enabled = false;
        }
    }
}

impl<T: Clone> Queue<T> {
    pub(crate) fn has_registered_workers(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .iter()
            .any(|s| s.registered.is_some())
    }

    pub(in crate::worker) fn authenticated_contact(&self, actor: &Actor, at: Instant) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(worker) = state
            .slots
            .iter_mut()
            .find(|s| s.registered.as_ref() == Some(actor))
        {
            worker.contact = Some(Contact::authenticated(at));
        }
    }

    pub(in crate::worker) fn reconcile_roster(
        &self,
        connection: u64,
        sent: Instant,
        members: &BTreeMap<String, bool>,
        active: impl Fn(&Actor) -> Option<Lease<T>>,
        now_ms: u64,
    ) -> Vec<Actor> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut lost = Vec::new();
        for i in 0..state.slots.len() {
            let slot = &state.slots[i];
            let Some(actor) = slot.registered.as_ref() else {
                continue;
            };
            if !slot.enabled
                || actor.connection != connection
                || slot.contact.is_none_or(|contact| contact.at >= sent)
            {
                continue;
            }
            let Some(id) = &actor.participant else {
                continue;
            };
            if let Some(&offline) = members.get(id) {
                state.slots[i].contact = slot.contact.map(|previous| Contact {
                    at: sent,
                    offline,
                    ..previous
                });
                continue;
            }

            let actor = actor.clone();
            if self
                .abandon(&mut state, i, active(&actor), MEMBERSHIP_ENDED, now_ms)
                .is_err()
            {
                continue;
            }
            lost.push(actor);
        }
        lost
    }
    fn abandon(
        &self,
        state: &mut State<T>,
        i: usize,
        lease: Option<Lease<T>>,
        source: &'static str,
        now_ms: u64,
    ) -> Result<(), String> {
        if matches!(state.slots[i].state, SlotState::Working(_)) {
            let lease = lease
                .filter(|l| l.slot == i && valid(state, &self.generation, l))
                .ok_or("could not establish the active claim; preserved station")?;
            self.finish_locked(
                state,
                &lease,
                Outcome::Unknown(reason(source).into()),
                false,
                now_ms,
                source,
            )?;
        } else {
            state.slots[i].state = SlotState::Offline(reason(source).into());
            log(&mut state.slots[i], now_ms, "warning", reason(source), None);
            self.record_slot_event(state, "worker.offline", i, now_ms);
            state.slots[i].vacate_departed();
            self.changed(state);
        }
        Ok(())
    }

    pub(in crate::worker) fn force_retirement(
        &self,
        target: &Retirement,
        active: impl Fn(&Actor) -> Option<Lease<T>>,
        now_ms: u64,
    ) -> Result<Option<Actor>, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || target.generation != self.generation {
            return Err("retirement generation changed".into());
        }
        let slot = state
            .slots
            .get(target.slot)
            .filter(|s| s.epoch == target.epoch)
            .ok_or("retirement slot or epoch changed")?;
        if !slot.enabled && slot.last_retirement.as_ref() == Some(target) {
            return Ok(None);
        }
        let actor = slot
            .registered
            .as_ref()
            .filter(|_| slot.enabled && slot.retirement.as_ref() == Some(target))
            .ok_or("the human-approved retirement target changed")?
            .clone();
        self.abandon(&mut state, target.slot, active(&actor), HUMAN_FORCE, now_ms)?;
        Ok(Some(actor))
    }
}
