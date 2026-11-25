//! Explicit reuse for validated, noninteractive pipe jobs. Each Shell owns one cache.
//! Entries index existing jobs; output and process lifetime remain owned by the registry.
use super::{InputMode, Job, MAX_RETAINED, Output};
use crate::runtime::call_scope::Source;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::sync::watch;

pub(super) const MAX_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Key([u8; 32]);

impl Key {
    /// Use only after normal authorization and argument validation. The shell
    /// has already resolved cwd and restricted the request to untracked pipes
    /// with closed stdin. scope_key is the caller's explicit input revision.
    pub(super) fn new(
        creator: &Source,
        cwd: &Path,
        argv: &[String],
        env: Option<&HashMap<String, String>>,
        timeout_ms: u64,
        title: Option<&str>,
        scope_key: &str,
    ) -> Result<Self, String> {
        let call = creator
            .call
            .as_ref()
            .ok_or("exec reuse requires an authenticated call")?;
        let identity = call
            .attribution()
            .ok_or("exec reuse requires a verified account")?;
        let thread = call
            .thread()
            .ok_or("exec reuse requires a verified thread")?;
        // Lengths delimit every variable-length field, including argv boundaries.
        // Store a fixed-size digest instead of retaining raw arguments and env.
        let mut digest = Sha256::new();
        let mut field = |bytes: &[u8]| {
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        };
        field(&call.connection().to_le_bytes());
        field(call.client_id().unwrap_or_default().as_bytes());
        field(identity.account.provider.as_bytes());
        field(identity.account.subject.as_bytes());
        field(
            identity
                .owner
                .as_ref()
                .map_or("", |a| &a.provider)
                .as_bytes(),
        );
        field(
            identity
                .owner
                .as_ref()
                .map_or("", |a| &a.subject)
                .as_bytes(),
        );
        field(thread.session_id().as_bytes());
        field(thread.thread_id().as_bytes());
        field(cwd.as_os_str().as_encoded_bytes());
        field(&timeout_ms.to_le_bytes());
        field(title.unwrap_or_default().as_bytes());
        field(scope_key.as_bytes());
        field(&(argv.len() as u64).to_le_bytes());
        for arg in argv {
            field(arg.as_bytes());
        }
        let mut inherited: Vec<_> = std::env::vars_os().collect();
        inherited.sort_unstable();
        field(&(inherited.len() as u64).to_le_bytes());
        for (key, value) in inherited {
            field(key.as_encoded_bytes());
            field(value.as_encoded_bytes());
        }
        let mut overrides: Vec<_> = env.into_iter().flatten().collect();
        // Windows folds case when applying overrides. Preserve insertion order
        // for ambiguous names, since the last spelling in the actual command wins.
        #[cfg(windows)]
        let order_matters = {
            let mut names = std::collections::HashSet::new();
            overrides
                .iter()
                .any(|(name, _)| !name.is_ascii() || !names.insert(name.to_ascii_uppercase()))
        };
        #[cfg(not(windows))]
        let order_matters = false;
        if !order_matters {
            overrides.sort_unstable();
        }
        field(&(overrides.len() as u64).to_le_bytes());
        for (key, value) in overrides {
            field(key.as_bytes());
            field(value.as_bytes());
        }
        Ok(Self(digest.finalize().into()))
    }
}

struct Entry {
    key: Key,
    ttl: Duration,
    job_id: watch::Receiver<Option<String>>,
}

pub(super) enum Decision<'a> {
    Start(Reservation<'a>),
    Joined(Arc<Job>),
    Cached(Arc<Job>),
}

pub(super) struct Cache {
    entries: Mutex<VecDeque<Arc<Entry>>>,
    capacity: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            entries: Mutex::default(),
            capacity: MAX_RETAINED,
        }
    }
}

impl Cache {
    fn entries(&self) -> MutexGuard<'_, VecDeque<Arc<Entry>>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn remove(&self, entry: &Arc<Entry>) {
        self.entries()
            .retain(|current| !Arc::ptr_eq(current, entry));
    }

    /// lookup must retrieve the currently retained job in this shell. Never
    /// hold either the cache lock or the registry lock across process startup.
    pub(super) async fn acquire(
        &self,
        key: Key,
        ttl: Duration,
        lookup: impl Fn(&str) -> Option<Arc<Job>>,
    ) -> Result<Decision<'_>, String> {
        if ttl > MAX_TTL {
            return Err("exec reuse TTL exceeds the cache limit".into());
        }
        loop {
            let entry = {
                let mut entries = self.entries();
                if let Some(found) = entries.iter().find(|e| e.key == key) {
                    Some(found.clone())
                } else if entries.len() < self.capacity {
                    let (sender, job_id) = watch::channel(None);
                    let entry = Arc::new(Entry { key, ttl, job_id });
                    entries.push_back(entry.clone());
                    return Ok(Decision::Start(Reservation {
                        cache: self,
                        entry,
                        sender: Some(sender),
                    }));
                } else {
                    None
                }
            };
            let Some(entry) = entry else {
                // Under pressure evict an old finished/forgotten job. Never
                // evict a live job or a reserved start and launch a duplicate.
                let candidates = self.entries().clone();
                if let Some(old) = candidates.iter().find(|old| {
                    let Some(id) = old.job_id.borrow().clone() else {
                        return false;
                    };
                    lookup(&id).is_none_or(|job| !job.state().running)
                }) {
                    self.remove(old);
                    continue;
                }
                return Err("exec reuse index is full of pending or running jobs".into());
            };

            let mut receiver = entry.job_id.clone();
            let published = receiver.borrow_and_update().clone();
            if let Some(id) = published {
                if let Some(job) = lookup(&id) {
                    let result = classify(&job, ttl.min(entry.ttl));
                    match result {
                        Some(Hit::Running) => return Ok(Decision::Joined(job)),
                        Some(Hit::Success) => return Ok(Decision::Cached(job)),
                        None => {}
                    }
                }
                self.remove(&entry);
            } else {
                // This caller joined before startup finished. Return the one
                // actual job, including a fast failure, or report lost startup.
                // In particular, never rerun this participant's operation here.
                receiver
                    .changed()
                    .await
                    .map_err(|_| "shared exec start ended without publishing a job")?;
                let id = receiver
                    .borrow()
                    .clone()
                    .ok_or("shared exec start has no job")?;
                let job = lookup(&id).ok_or("shared exec job is no longer retained")?;
                return Ok(Decision::Joined(job));
            }
        }
    }
}

pub(super) struct Reservation<'a> {
    cache: &'a Cache,
    entry: Arc<Entry>,
    sender: Option<watch::Sender<Option<String>>>,
}

impl Reservation<'_> {
    /// Publish only after the job is visible in the registry. Returning an
    /// error or dropping the start future abandons the reservation via Drop.
    pub(super) fn publish(mut self, job_id: &str) {
        self.sender
            .take()
            .expect("unpublished reservation")
            .send_replace(Some(job_id.to_owned()));
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.sender.is_some() {
            self.cache.remove(&self.entry);
        }
    }
}

enum Hit {
    Running,
    Success,
}

fn classify(job: &Job, ttl: Duration) -> Option<Hit> {
    if job.terminal.is_some() || job.input_mode != InputMode::Null || job.output_mode != "pipes" {
        return None;
    }
    #[cfg(target_os = "linux")]
    if job.artifacts.is_some() {
        return None;
    }
    let state = job.state();
    if state.running {
        return Some(Hit::Running);
    }
    if state.exit_code != Some(0)
        || state.exit_signal.is_some()
        || state.error.is_some()
        || state.cancelled
        || state.timed_out
        || state.finished?.elapsed() >= ttl
        || !complete(&state.stdout)
        || !complete(&state.stderr)
    {
        return None;
    }
    Some(Hit::Success)
}

fn complete(output: &Output) -> bool {
    output.eof && output.start == 0 && output.read_from_capture
        && output.capture.error().is_none() && output.capture.bytes() == output.end
        // An archived capture may expire or be evicted independently of the job.
        && output.capture.read_range(output.end, 1).is_ok()
}

#[cfg(test)]
mod tests;
