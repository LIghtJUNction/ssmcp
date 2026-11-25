//! The dedicated worker connection owns a separate registry and has human chat
//! disabled. Only its own solicited, complete native response can end a claim.
use crate::wire::{CHAT_DISABLED, SteeringAgent, SteeringSnapshot, WORKER_ROSTER_REQUEST};
use std::time::{Duration, Instant};

pub(super) struct Poll {
    enabled: bool,
    id: u64,
    last_request: Option<Instant>,
    pending: bool,
}

impl Poll {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            id: WORKER_ROSTER_REQUEST,
            last_request: None,
            pending: false,
        }
    }

    pub(super) fn request(
        &mut self,
        now: Instant,
        registered: impl FnOnce() -> bool,
    ) -> Option<u64> {
        if !self.enabled
            || self.last_request.is_some_and(|sent| {
                let interval = if self.pending { 30 } else { 5 };
                now.saturating_duration_since(sent) < Duration::from_secs(interval)
            })
            || !registered()
        {
            return None;
        }
        self.id = self.id.checked_add(1)?;
        self.last_request = Some(now);
        self.pending = true;
        Some(self.id)
    }

    pub(super) fn reply(
        &mut self,
        id: u64,
        result: Result<SteeringSnapshot, String>,
        client_id: &str,
    ) -> Option<(Instant, Vec<SteeringAgent>)> {
        if !self.pending || self.id != id {
            return None;
        }
        self.pending = false;
        let snapshot = match result {
            Err(error) => {
                if error == CHAT_DISABLED {
                    // An older v7 broker declines this read. Silence cannot
                    // prove that the participant or an external process ended.
                    self.enabled = false;
                }
                return None;
            }
            Ok(snapshot) => snapshot,
        };
        if snapshot.schema_version != 1
            || snapshot.client_id != client_id
            || snapshot.revision != 0
            || !snapshot.messages.is_empty()
            || snapshot.agents.len() > 64
            || snapshot.agents.iter().any(|a| {
                !super::steering::valid_id(&a.agent_id)
                    || a.name.len() > 128
                    || a.role.as_ref().is_some_and(|r| r.len() > 128)
            })
        {
            return None;
        }
        Some((self.last_request?, snapshot.agents))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty(room: &str) -> SteeringSnapshot {
        SteeringSnapshot {
            schema_version: 1,
            client_id: room.into(),
            revision: 0,
            agents: vec![],
            messages: vec![],
        }
    }
    #[test]
    fn dedicated_roster_correlates_its_own_room_and_ignores_late_or_refused_reads() {
        let now = Instant::now();
        let mut poll = Poll::new(true);
        assert!(poll.request(now, || false).is_none());
        let old = poll.request(now, || true).unwrap();
        assert!(
            poll.request(now + Duration::from_secs(1), || true)
                .is_none()
        );
        let sent = now + Duration::from_secs(30);
        let id = poll.request(sent, || true).unwrap();
        assert!(poll.reply(old, Ok(empty("room")), "room").is_none());
        assert!(poll.reply(id, Ok(empty("another")), "room").is_none());
        let next = sent + Duration::from_secs(5);
        let id = poll.request(next, || true).unwrap();
        assert_eq!(poll.reply(id, Ok(empty("room")), "room").unwrap().0, next);
        assert!(poll.reply(id, Ok(empty("room")), "room").is_none());
        let id = poll
            .request(next + Duration::from_secs(5), || true)
            .unwrap();
        assert!(poll.reply(id, Err(CHAT_DISABLED.into()), "room").is_none());
        assert!(
            poll.request(next + Duration::from_secs(1800), || true)
                .is_none()
        );
        assert!(Poll::new(false).request(now, || true).is_none());
    }
}
