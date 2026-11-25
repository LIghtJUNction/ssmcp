use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Handoff {
    pub(crate) id: String,
    pub(crate) workspace: String,
    pub(crate) control: String,
    pub(crate) pool: String,
    pub(crate) generation: String,
    pub(crate) predecessor: String,
    pub(crate) attached_at_ms: u64,
    pub(crate) successor: String,
    pub(crate) note: String,
}

impl<T: Clone> Queue<T> {
    pub(crate) fn offer_handoff(&self, offer: Handoff) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.manager.as_ref().ok_or("no attached manager")?;
        if state.closed
            || state.stopping
            || self.generation != offer.generation
            || current.view.client_id != offer.pool
            || current.view.thread_id != offer.predecessor
            || current.view.attached_at_ms != offer.attached_at_ms
            || offer.successor == offer.predecessor
        {
            return Err("local handoff no longer matches this manager and pool generation".into());
        }
        state.pending_handoff = Some((offer, Instant::now()));
        Ok(())
    }

    pub(crate) fn accept_handoff(
        &self,
        actor: &Actor,
        client_id: &str,
        id: &str,
        now: u64,
    ) -> Result<Value, String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let current = state.manager.as_ref().ok_or("no attached manager")?;
        let successor = actor
            .participant
            .as_deref()
            .ok_or("bind the successor first")?;
        if state.closed
            || state.stopping
            || current.view.client_id != client_id
            || !lifecycle::same_account(&current.actor, actor)
        {
            return Err("handoff requires the live owner account and pool connection".into());
        }
        if let Some(done) = &state.accepted_handoff
            && done.id == id
            && current.matches(actor, client_id)
            && done.successor == successor
        {
            return Ok(
                json!({"handoff":done,"manager":current.view,"duplicate":true,"live_activity":projection::managed_activity(&state,&self.generation)}),
            );
        }
        let offer = state
            .pending_handoff
            .as_ref()
            .filter(|(_, offered)| offered.elapsed() < std::time::Duration::from_secs(600))
            .map(|(offer, _)| offer)
            .filter(|offer| {
                offer.id == id
                    && offer.successor == successor
                    && offer.generation == self.generation
                    && offer.predecessor == current.view.thread_id
                    && offer.attached_at_ms == current.view.attached_at_ms
            })
            .ok_or("no matching local handoff for this bound participant")?;
        if state
            .slots
            .iter()
            .any(|slot| slot.registered.as_ref() == Some(actor))
        {
            return Err("an occupied worker station cannot become coordinator".into());
        }
        let at = now.max(
            current
                .view
                .attached_at_ms
                .checked_add(1)
                .ok_or("manager tenure exhausted")?,
        );
        let manager = Manager::new(actor, client_id, at)?;
        let receipt = json!({"handoff":offer,"manager":manager.view,"duplicate":false,"live_activity":projection::managed_activity(&state,&self.generation)});
        state.manager = Some(manager);
        state.accepted_handoff = state.pending_handoff.take().map(|(offer, _)| offer);
        self.changed(&mut state);
        self.capacity_changed.send_replace(());
        Ok(receipt)
    }
}
