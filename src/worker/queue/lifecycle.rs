//! Station registration and cooperative retirement. Removed indices remain
//! bounded tombstones so a resize never changes an outstanding claim's slot.
use super::*;

pub(super) const FORCE_RETIREMENT: &str =
    "Station force-retired by the operator; external execution is unconfirmed";

impl<T: Clone> Queue<T> {
    pub(in crate::worker) fn remote_ready(&self) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.stopping {
            return Err("remote worker connection is closed or stopped".into());
        }
        state.remote_ready = true;
        self.changed(&mut state);
        Ok(())
    }

    pub(crate) fn resize(&self, total: usize) -> Result<(), String> {
        self.resize_inner(total, None)
    }

    pub(in crate::worker) fn resize_managed(
        &self,
        actor: &Actor,
        client_id: &str,
        total: usize,
    ) -> Result<(), String> {
        self.resize_inner(total, Some((actor, client_id)))
    }

    fn resize_inner(&self, total: usize, manager: Option<(&Actor, &str)>) -> Result<(), String> {
        if total > super::super::config::MAX_WORKERS {
            return Err("worker total must be an integer from 0 through 128".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some((actor, client_id)) = manager {
            require_manager(&state, actor, client_id)?;
        }
        if !state.remote_ready || state.closed || state.stopping {
            return Err("only an active remote worker pool can be resized".into());
        }
        if state
            .slots
            .iter()
            .any(|slot| slot.enabled && slot.retirement.is_some())
        {
            return if total == state.desired_total {
                Ok(())
            } else {
                Err("a worker resize is awaiting external release acknowledgements; retry after it completes".into())
            };
        }
        let current = state.slots.iter().filter(|slot| slot.enabled).count();
        if total == current {
            state.desired_total = total;
            return Ok(());
        }
        if total > current {
            let mut remaining = total - current;
            for slot in state.slots.iter_mut().filter(|slot| !slot.enabled) {
                if remaining == 0 {
                    break;
                }
                // Preserve the epoch across reuse; registration increments it.
                let epoch = slot.epoch;
                *slot = Slot::new();
                slot.epoch = epoch;
                remaining -= 1;
            }
            state.slots.extend((0..remaining).map(|_| Slot::new()));
            state.desired_total = total;
            self.changed(&mut state);
            return Ok(());
        }

        // No mutation until all candidates and all acknowledgement nonces exist.
        // An unregistered offline station is removed before a connected worker
        // is asked to stop; idle workers are always selected before active work.
        let mut candidates: Vec<_> = state
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.enabled)
            .map(|(index, slot)| {
                let priority = match (&slot.state, &slot.registered) {
                    (SlotState::Offline(_), None) => 0,
                    (SlotState::Idle | SlotState::Offline(_), Some(_)) => 1,
                    (SlotState::Working(_), Some(_)) => 2,
                    _ => 3,
                };
                (priority, index)
            })
            .collect();
        candidates.sort_by_key(|(priority, index)| (*priority, std::cmp::Reverse(*index)));
        let mut selected = Vec::with_capacity(current - total);
        for (priority, index) in candidates.into_iter().take(current - total) {
            if priority == 3 {
                return Err(
                    "worker station has no authenticated remote owner; resize was not applied"
                        .into(),
                );
            }
            let retirement = if priority == 0 {
                None
            } else {
                Some(Retirement {
                    id: super::super::remote::nonce()?,
                    generation: self.generation.clone(),
                    slot: index,
                    epoch: state.slots[index].epoch,
                })
            };
            selected.push((index, retirement));
        }
        let now_ms = super::super::now_ms();
        for (index, retirement) in selected {
            if let Some(retirement) = retirement {
                self.mark_retiring(&mut state, index, retirement, now_ms);
            } else {
                state.slots[index].enabled = false;
            }
        }
        self.cancel_unavailable_shares(&mut state, now_ms);
        state.desired_total = total;
        self.changed(&mut state);
        Ok(())
    }

    fn mark_retiring(
        &self,
        state: &mut State<T>,
        index: usize,
        retirement: Retirement,
        now_ms: u64,
    ) {
        let worker = &mut state.slots[index];
        let task_id = match &worker.state {
            SlotState::Working(id) => Some(id.clone()),
            _ => None,
        };
        worker.retirement = Some(retirement);
        if task_id.is_none() {
            worker.state = SlotState::Retiring;
        }
        log(
            worker,
            now_ms,
            "warning",
            "Resize requested shutdown; waiting for this worker's authenticated acknowledgement",
            task_id.as_deref(),
        );
        if let Some(task) = task_id.as_ref().and_then(|id| state.tasks.get_mut(id)) {
            task.view.status = TaskStatus::Cancelling;
            task.view.updated_ms = now_ms;
        }
        if let Some(id) = task_id {
            self.record_task_event(state, "worker.cancelling", &id, None, now_ms);
        } else {
            self.record_slot_event(state, "worker.retiring", index, now_ms);
        }
    }

    pub(in crate::worker) fn prepare_retirement(
        &self,
        index: usize,
    ) -> Result<(Retirement, Option<String>), String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state.remote_ready || state.closed || state.stopping {
            return Err("only an active remote worker pool can retire a station".into());
        }
        let worker = state
            .slots
            .get(index)
            .filter(|slot| slot.enabled && slot.registered.is_some())
            .ok_or("selected worker station has no registered participant")?;
        let task = match &worker.state {
            SlotState::Working(task) => Some(task.clone()),
            _ => None,
        };
        if let Some(ticket) = &worker.retirement {
            return Ok((ticket.clone(), task));
        }
        let ticket = Retirement {
            id: super::super::remote::nonce()?,
            generation: self.generation.clone(),
            slot: index,
            epoch: worker.epoch,
        };
        let now_ms = super::super::now_ms();
        self.mark_retiring(&mut state, index, ticket.clone(), now_ms);
        state.desired_total -= 1;
        self.cancel_unavailable_shares(&mut state, now_ms);
        self.changed(&mut state);
        Ok((ticket, task))
    }

    pub(in crate::worker) fn force_retirement(
        &self,
        target: &Retirement,
        active: impl FnOnce(&Actor) -> Option<Lease<T>>,
        now_ms: u64,
    ) -> Result<Option<Actor>, String> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed || target.generation != self.generation {
            return Err("retirement generation changed".into());
        }
        let worker = state
            .slots
            .get(target.slot)
            .filter(|s| s.epoch == target.epoch)
            .ok_or("retirement station or epoch changed")?;
        if !worker.enabled && worker.last_retirement.as_ref() == Some(target) {
            return Ok(None);
        }
        let actor = worker
            .registered
            .as_ref()
            .filter(|_| worker.enabled && worker.retirement.as_ref() == Some(target))
            .ok_or("selected retirement no longer matches this station")?
            .clone();
        if matches!(worker.state, SlotState::Working(_)) {
            let lease = active(&actor)
                .filter(|lease| lease.slot == target.slot && valid(&state, &self.generation, lease))
                .ok_or("could not verify the active claim; station preserved")?;
            self.finish_locked(
                &mut state,
                &lease,
                Outcome::Unknown(FORCE_RETIREMENT.into()),
                false,
                now_ms,
                FORCE_RETIREMENT,
            )?;
        } else {
            let worker = &mut state.slots[target.slot];
            worker.state = SlotState::Offline(FORCE_RETIREMENT.into());
            log(worker, now_ms, "warning", FORCE_RETIREMENT, None);
            self.record_slot_event(&mut state, "worker.offline", target.slot, now_ms);
            state.slots[target.slot].finish_retirement();
            self.changed(&mut state);
        }
        Ok(Some(actor))
    }

    pub(in crate::worker) fn retirement(&self, actor: &Actor) -> Option<Retirement> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .slots
            .iter()
            .find(|slot| {
                slot.enabled
                    && slot.registered.as_ref() == Some(actor)
                    && matches!(slot.state, SlotState::Retiring)
            })
            .and_then(|slot| slot.retirement.clone())
    }

    pub(in crate::worker) fn retire_requested(&self, lease: &Lease<T>) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        valid(&state, &self.generation, lease) && state.slots[lease.slot].retirement.is_some()
    }

    pub(in crate::worker) fn release_remote(
        &self,
        actor: &Actor,
        retirement: &Retirement,
        now_ms: u64,
    ) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if retirement.generation != self.generation {
            return Err("stale worker retirement generation".into());
        }
        let worker = state
            .slots
            .get_mut(retirement.slot)
            .filter(|slot| {
                slot.enabled
                    && slot.epoch == retirement.epoch
                    && slot.registered.as_ref() == Some(actor)
                    && slot.retirement.as_ref() == Some(retirement)
            })
            .ok_or("worker release does not match this exact authenticated retirement receipt")?;
        if !matches!(worker.state, SlotState::Retiring) {
            return Err(
                "working worker must report its real task result before acknowledging release"
                    .into(),
            );
        }
        log(
            worker,
            now_ms,
            "info",
            "External worker acknowledged shutdown and released its station",
            None,
        );
        worker.state = SlotState::Offline("external worker released its station".into());
        self.record_slot_event(&mut state, "worker.offline", retirement.slot, now_ms);
        state.slots[retirement.slot].finish_retirement();
        self.changed(&mut state);
        Ok(())
    }
}

pub(super) fn same_account(left: &Actor, right: &Actor) -> bool {
    left.provider == right.provider
        && left.subject == right.subject
        && left.connection == right.connection
}
