//! A replaceable public notebook for one authenticated station occupant.
//! Task results, claims, command logs and station lifetimes remain independent.
use super::*;

const MAX_READ_PATHS: usize = 24;
const MAX_DONE: usize = 12;

#[derive(Serialize)]
struct ReadPath {
    path: String,
    source: &'static str,
    task_id: String,
    at_ms: u64,
}

#[derive(Serialize)]
struct Finished {
    task_id: String,
    summary: String,
    status: TaskStatus,
    at_ms: u64,
}

#[derive(Default)]
pub(super) struct Notebook {
    reset_revision: u64,
    reads: VecDeque<ReadPath>,
    finished: VecDeque<Finished>,
}

pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.chars().any(char::is_control)
        && !path.starts_with(['/', '\\'])
        && !path.contains("://")
        && path.as_bytes().get(1) != Some(&b':')
        && !path.split(['/', '\\']).any(|part| part == "..")
        && path
            .split(['/', '\\'])
            .any(|part| !part.is_empty() && part != ".")
}

impl Notebook {
    pub(super) fn read(&mut self, path: &str, source: &'static str, task_id: &str, now_ms: u64) {
        if !valid_path(path) {
            return;
        }
        self.reads.retain(|entry| {
            entry.path != path || entry.source != source || entry.task_id != task_id
        });
        self.reads.push_front(ReadPath {
            path: path.into(),
            source,
            task_id: task_id.into(),
            at_ms: now_ms,
        });
        self.reads.truncate(MAX_READ_PATHS);
    }

    pub(super) fn finish(&mut self, task: &TaskView, text: &str) {
        if task.status == TaskStatus::Unknown {
            return;
        }
        self.finished.push_front(Finished {
            task_id: task.task_id.clone(),
            summary: display_text(text, 256),
            status: task.status,
            at_ms: task.updated_ms,
        });
        self.finished.truncate(MAX_DONE);
    }
}

pub(crate) struct ReadTicket {
    slot: usize,
    epoch: u64,
    task_id: String,
    attempt: u64,
    reset_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::worker) struct ReadState {
    pub(in crate::worker) slot: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::worker) struct Reset {
    generation: String,
    slot: usize,
    epoch: u64,
    expected_reset_revision: u64,
}

impl<T: Clone> Queue<T> {
    pub(in crate::worker) fn station_registration(&self, actor: &Actor) -> Option<Value> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (index, slot) = state
            .slots
            .iter()
            .enumerate()
            .find(|(_, slot)| slot.enabled && slot.registered.as_ref() == Some(actor))?;
        Some(
            json!({"generation":self.generation,"slot":index,"epoch":slot.epoch,"reset_revision":slot.notebook.reset_revision}),
        )
    }

    pub(in crate::worker) fn station_state(
        &self,
        actor: &Actor,
        client_id: &str,
        index: usize,
    ) -> Result<Value, String> {
        let _ = Manager::new(actor, client_id, 0)?;
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.manager.as_ref().is_some_and(|manager| {
            manager.view.client_id != client_id || !lifecycle::same_account(&manager.actor, actor)
        }) || state
            .slots
            .iter()
            .filter_map(|s| s.registered.as_ref())
            .any(|owner| !lifecycle::same_account(owner, actor))
        {
            return Err(
                "station state belongs to a different authenticated connection or account".into(),
            );
        }
        let slot = state
            .slots
            .get(index)
            .filter(|s| s.enabled)
            .ok_or("station does not exist")?;
        let participant_id = slot
            .registered
            .as_ref()
            .and_then(|a| a.participant.as_deref());
        let current = match (&slot.state, participant_id) {
            (SlotState::Working(id), Some(_)) => {
                Some(json!({"task_id":id,"summary":slot.task_summary,"progress":slot.progress}))
            }
            _ => None,
        };
        let empty = Notebook::default();
        let notebook = if participant_id.is_some() {
            &slot.notebook
        } else {
            &empty
        };
        Ok(
            json!({"generation":self.generation,"slot":index,"epoch":slot.epoch,"participant_id":participant_id,
            "current":current,"recent_done":notebook.finished,"read_paths":notebook.reads,"reset_revision":notebook.reset_revision}),
        )
    }

    pub(in crate::worker) fn station_reset(
        &self,
        actor: &Actor,
        client_id: &str,
        req: Reset,
    ) -> Result<Value, String> {
        let _ = Manager::new(actor, client_id, 0)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if req.generation != self.generation
            || state
                .manager
                .as_ref()
                .is_some_and(|m| m.view.client_id != client_id)
        {
            return Err("station reset belongs to a different pool".into());
        }
        let slot = state
            .slots
            .get_mut(req.slot)
            .filter(|s| s.enabled && s.epoch == req.epoch && s.registered.as_ref() == Some(actor))
            .ok_or("only the current authenticated occupant can reset this station epoch")?;
        let next = req
            .expected_reset_revision
            .checked_add(1)
            .ok_or("station reset revision exhausted")?;
        let duplicate = slot.notebook.reset_revision == next;
        if !duplicate {
            if slot.notebook.reset_revision != req.expected_reset_revision {
                return Err("station reset revision changed".into());
            }
            slot.notebook = Notebook {
                reset_revision: next,
                ..Notebook::default()
            };
            slot.progress = None;
            self.changed(&mut state);
        }
        Ok(
            json!({"status":"reset","generation":self.generation,"slot":req.slot,"epoch":req.epoch,"reset_revision":next,"duplicate":duplicate}),
        )
    }

    pub(in crate::worker) fn file_read_ticket(
        &self,
        actor: &Actor,
        client_id: &str,
    ) -> Option<ReadTicket> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let manager = state.manager.as_ref()?;
        if manager.view.client_id != client_id || !lifecycle::same_account(&manager.actor, actor) {
            return None;
        }
        let (index, slot) = state
            .slots
            .iter()
            .enumerate()
            .find(|(_, s)| s.enabled && s.registered.as_ref() == Some(actor))?;
        let SlotState::Working(id) = &slot.state else {
            return None;
        };
        let task = state.tasks.get(id)?;
        Some(ReadTicket {
            slot: index,
            epoch: slot.epoch,
            task_id: id.clone(),
            attempt: task.view.attempt,
            reset_revision: slot.notebook.reset_revision,
        })
    }

    pub(crate) fn finish_file_read(&self, ticket: ReadTicket, path: &str, now_ms: u64) {
        if !valid_path(path) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if !state
            .tasks
            .get(&ticket.task_id)
            .is_some_and(|t| t.view.attempt == ticket.attempt)
        {
            return;
        }
        let Some(slot) = state.slots.get_mut(ticket.slot).filter(|s| {
            s.enabled
                && s.registered.is_some()
                && s.epoch == ticket.epoch
                && s.notebook.reset_revision == ticket.reset_revision
                && matches!(&s.state,SlotState::Working(id) if id==&ticket.task_id)
        }) else {
            return;
        };
        slot.notebook.read(path, "native", &ticket.task_id, now_ms);
        self.changed(&mut state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn actor(id: &str) -> Actor {
        Actor {
            provider: "fixture".into(),
            subject: "owner".into(),
            connection: 1,
            participant: Some(id.into()),
        }
    }

    fn reset(epoch: u64, revision: u64) -> Reset {
        Reset {
            generation: "pool".into(),
            slot: 0,
            epoch,
            expected_reset_revision: revision,
        }
    }

    #[test]
    fn notebook_is_bound_to_the_claim_occupant_and_reset_revision() {
        let queue = Queue::new("pool".into(), 1).unwrap();
        let manager = actor("manager");
        let self_worker = actor("worker");
        let peer = actor("peer");
        queue.attach(&manager, CLIENT, 1).unwrap();
        queue.remote_ready().unwrap();
        let task = queue
            .submit_managed(manager.clone(), CLIENT, "task", "Inspect fixtures", (), 2)
            .unwrap();
        let lease = queue.claim_remote(&self_worker, 3).unwrap().unwrap();
        assert!(queue.file_read_ticket(&peer, CLIENT).is_none());
        assert!(
            queue
                .file_read_ticket(&self_worker, &"b".repeat(64))
                .is_none()
        );
        let capture = || queue.file_read_ticket(&self_worker, CLIENT).unwrap();
        queue.finish_file_read(capture(), "src/lib.rs", 4);
        let pending = capture();
        queue
            .report(
                &lease,
                &ReportUpdate {
                    event: ReportEvent::FileRead,
                    command: None,
                    progress: None,
                    detail: Some("docs/workers.md".into()),
                },
                5,
            )
            .unwrap();
        let state = queue.station_state(&peer, CLIENT, 0).unwrap();
        assert_eq!(state["current"]["task_id"], task.task_id);
        assert_eq!(state["read_paths"][0]["source"], "self_reported");
        assert_eq!(state["read_paths"][1]["source"], "native");
        assert!(
            queue
                .station_reset(&manager, CLIENT, reset(lease.epoch, 0))
                .is_err()
        );
        assert!(
            queue
                .station_reset(&self_worker, CLIENT, reset(lease.epoch + 1, 0))
                .is_err()
        );
        assert!(
            queue
                .station_state(
                    &Actor {
                        subject: "foreign".into(),
                        ..peer.clone()
                    },
                    CLIENT,
                    0
                )
                .is_err()
        );
        assert!(
            queue
                .station_state(
                    &Actor {
                        connection: 2,
                        ..peer.clone()
                    },
                    CLIENT,
                    0
                )
                .is_err()
        );
        assert_eq!(
            queue
                .station_reset(&self_worker, CLIENT, reset(lease.epoch, 0))
                .unwrap()["duplicate"],
            false
        );
        queue.finish_file_read(pending, "before-reset", 7);
        let late = capture();
        queue.finish_file_read(capture(), "after-reset", 8);
        assert_eq!(
            queue
                .station_reset(&self_worker, CLIENT, reset(lease.epoch, 0))
                .unwrap()["duplicate"],
            true
        );
        assert_eq!(
            queue.station_registration(&self_worker).unwrap()["reset_revision"],
            1
        );
        let after = queue.station_state(&peer, CLIENT, 0).unwrap();
        assert_eq!(after["read_paths"].as_array().unwrap().len(), 1);
        assert_eq!(after["read_paths"][0]["path"], "after-reset");
        assert_eq!(after["current"]["task_id"], task.task_id);
        queue
            .finish_remote(&lease, Outcome::Completed("private body".into()), true, 9)
            .unwrap();
        queue.finish_file_read(late, "after-completion", 10);
        let finished = queue.station_state(&peer, CLIENT, 0).unwrap();
        assert!(finished["current"].is_null());
        assert_eq!(finished["recent_done"][0]["summary"], "Inspect fixtures");
        assert_eq!(finished["recent_done"][0]["status"], "completed");
        assert_eq!(finished["read_paths"].as_array().unwrap().len(), 1);
        assert!(!finished.to_string().contains("private body"));
        queue
            .station_reset(&self_worker, CLIENT, reset(lease.epoch, 1))
            .unwrap();
        assert!(
            queue.station_state(&peer, CLIENT, 0).unwrap()["recent_done"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            queue
                .result(&manager, &task.task_id)
                .unwrap()
                .output
                .as_deref(),
            Some("private body")
        );
    }

    #[test]
    fn replacement_occupant_starts_fresh_and_cannot_use_the_old_epoch() {
        let queue = Queue::new("pool".into(), 1).unwrap();
        let manager = actor("manager");
        let first = actor("first");
        let replacement = actor("next");
        queue.attach(&manager, CLIENT, 1).unwrap();
        queue.remote_ready().unwrap();
        let task = queue
            .submit_managed(manager.clone(), CLIENT, "original", "Old task", (), 2)
            .unwrap();
        let lease = queue.claim_remote(&first, 3).unwrap().unwrap();
        queue.finish_file_read(
            queue.file_read_ticket(&first, CLIENT).unwrap(),
            "old.txt",
            4,
        );
        queue
            .finish_remote(&lease, Outcome::Completed("intact".into()), true, 5)
            .unwrap();
        queue.resize(0).unwrap();
        let retirement = queue.retirement(&first).unwrap();
        queue.release_remote(&first, &retirement, 6).unwrap();
        queue.resize(1).unwrap();
        assert!(queue.claim_remote(&replacement, 7).unwrap().is_none());
        let now = queue.station_state(&manager, CLIENT, 0).unwrap();
        assert_eq!(now["participant_id"], "next");
        let epoch = now["epoch"].as_u64().unwrap();
        assert!(epoch > lease.epoch);
        assert_eq!(now["read_paths"], json!([]));
        assert_eq!(now["recent_done"], json!([]));
        assert!(
            queue
                .station_reset(&first, CLIENT, reset(lease.epoch, 0))
                .is_err()
        );
        assert!(
            queue
                .station_reset(&replacement, CLIENT, reset(lease.epoch, 0))
                .is_err()
        );
        queue
            .station_reset(&replacement, CLIENT, reset(epoch, 0))
            .unwrap();
        assert_eq!(
            queue
                .result(&manager, &task.task_id)
                .unwrap()
                .output
                .as_deref(),
            Some("intact")
        );
    }

    #[test]
    fn reads_are_bounded_and_only_workspace_relative_paths_are_accepted() {
        let mut notebook = Notebook::default();
        for path in [
            "",
            "..",
            "/etc/passwd",
            "../secrets",
            "a/../private",
            "C:\\profile",
            "https://example",
            "a\nb",
            "..\\secret",
        ] {
            assert!(!valid_path(path));
            notebook.read(path, "self_reported", "t", 0);
        }
        assert!(notebook.reads.is_empty());
        for i in 0..MAX_READ_PATHS + 6 {
            notebook.read(&format!("dir/{i}"), "native", "t", i as u64);
        }
        assert_eq!(notebook.reads.len(), MAX_READ_PATHS);
        notebook.read("dir/6", "native", "t", 99);
        assert_eq!(notebook.reads.len(), MAX_READ_PATHS);
        assert_eq!(notebook.reads[0].path, "dir/6");
    }
}
