//! Station registration and cooperative retirement. Removed indices remain
//! bounded tombstones so a resize never changes an outstanding claim's slot.
use super::*;

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
            let worker = &mut state.slots[index];
            if retirement.is_none() {
                worker.enabled = false;
                continue;
            }
            let task_id = match &worker.state {
                SlotState::Working(id) => Some(id.clone()),
                _ => None,
            };
            worker.retirement = retirement;
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
                self.record_task_event(&mut state, "worker.cancelling", &id, None, now_ms);
            } else {
                self.record_slot_event(&mut state, "worker.retiring", index, now_ms);
            }
        }
        state.desired_total = total;
        self.changed(&mut state);
        Ok(())
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
        let worker = &mut state.slots[retirement.slot];
        worker.registered = None;
        worker.retirement = None;
        worker.enabled = false;
        self.changed(&mut state);
        Ok(())
    }
}

pub(super) fn same_account(left: &Actor, right: &Actor) -> bool {
    left.provider == right.provider
        && left.subject == right.subject
        && left.connection == right.connection
}
