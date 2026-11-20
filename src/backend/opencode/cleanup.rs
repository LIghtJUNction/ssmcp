//! Keep cancelled stream transport teardown independent of blocking archival I/O.
use super::EventLog;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

#[derive(Default)]
pub(super) struct Finalizers(Mutex<Vec<Finalizer>>);

enum Finalizer {
    Runtime(JoinHandle<()>),
    Thread(Option<std::thread::JoinHandle<()>>),
}

impl Finalizer {
    fn is_finished(&self) -> bool {
        match self {
            Self::Runtime(task) => task.is_finished(),
            Self::Thread(task) => task.as_ref().is_none_or(|task| task.is_finished()),
        }
    }

    async fn join(&mut self) {
        match self {
            Self::Runtime(task) => {
                let _ = task.await;
            }
            Self::Thread(task) => {
                while task.as_ref().is_some_and(|task| !task.is_finished()) {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
                if let Some(task) = task.take() {
                    let _ = task.join();
                }
            }
        }
    }

    fn join_blocking(self) {
        match self {
            Self::Runtime(task) => {
                while !task.is_finished() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            Self::Thread(Some(task)) => {
                let _ = task.join();
            }
            Self::Thread(None) => {}
        }
    }
}

impl Finalizers {
    fn insert(&self, task: Finalizer) {
        let mut pending = self.0.lock().unwrap_or_else(|error| error.into_inner());
        pending.retain(|task| !task.is_finished());
        pending.push(task);
    }

    fn take(&self) -> Vec<Finalizer> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|error| error.into_inner()))
    }

    pub(super) fn retire(&self, task: JoinHandle<()>) {
        task.abort();
        self.insert(Finalizer::Runtime(task));
    }

    pub(super) fn run(self: &Arc<Self>, work: impl FnOnce() + Send + 'static) {
        let mut work = DeferredWork {
            action: Some(Box::new(work)),
            finalizers: self.clone(),
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            self.insert(Finalizer::Runtime(runtime.spawn_blocking(move || {
                work.action.take().unwrap()();
            })));
        }
        // Without a runtime, Drop sends the work directly to its fallback.
    }
}

struct DeferredWork {
    action: Option<Box<dyn FnOnce() + Send>>,
    finalizers: Arc<Finalizers>,
}

impl Drop for DeferredWork {
    fn drop(&mut self) {
        if let Some(work) = self.action.take() {
            // A runtime may discard a queued blocking closure during shutdown.
            // Keep its capture and completion ownership instead of losing bytes
            // or falling back to disk I/O in the transport's destructor.
            self.finalizers
                .insert(Finalizer::Thread(Some(std::thread::spawn(work))));
        }
    }
}

pub(super) struct Shutdown {
    streams: Vec<JoinHandle<()>>,
    finalizers: Arc<Finalizers>,
    pending: Vec<Finalizer>,
    events: Option<Arc<EventLog>>,
}

impl Shutdown {
    pub(super) fn new(
        streams: impl IntoIterator<Item = JoinHandle<()>>,
        finalizers: Arc<Finalizers>,
        events: Arc<EventLog>,
    ) -> Self {
        let streams: Vec<_> = streams.into_iter().collect();
        for task in &streams {
            task.abort();
        }
        Self {
            streams,
            finalizers,
            pending: Vec::new(),
            events: Some(events),
        }
    }

    pub(super) async fn finish(mut self) {
        // Retain each handle until its await completes. If this future is itself
        // cancelled, Drop transfers the remaining cleanup to an owned thread.
        for task in &mut self.streams {
            let _ = task.await;
        }
        self.streams.clear();
        self.drain_finalizers().await;
        // Cancelled stream destructors have now published all their finalizers.
        // Close the event archive only after their partial-artifact records exist.
        let events = self.events.take().unwrap();
        self.finalizers.run(move || events.close());
        self.drain_finalizers().await;
    }

    async fn drain_finalizers(&mut self) {
        loop {
            self.pending = self.finalizers.take();
            if self.pending.is_empty() {
                break;
            }
            for task in &mut self.pending {
                task.join().await;
            }
            self.pending.clear();
        }
    }

    pub(super) fn dispatch(self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(self.finish());
        }
        // Without a live runtime, or if it rejects the spawned future during
        // shutdown, this owner's Drop continues cleanup off the calling thread.
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        let Some(events) = self.events.take() else {
            return;
        };
        let streams = std::mem::take(&mut self.streams);
        let mut pending = std::mem::take(&mut self.pending);
        let finalizers = self.finalizers.clone();
        std::thread::spawn(move || {
            for task in streams {
                task.abort();
                Finalizer::Runtime(task).join_blocking();
            }
            loop {
                pending.extend(finalizers.take());
                if pending.is_empty() {
                    break;
                }
                for task in pending.drain(..) {
                    task.join_blocking();
                }
            }
            events.close();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn queued_archive_survives_runtime_shutdown_exactly_once() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (started, running) = std::sync::mpsc::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        runtime.spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = waiting.recv_timeout(std::time::Duration::from_secs(10));
        });
        running
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let finalizers = Arc::new(Finalizers::default());
        let count = Arc::new(AtomicUsize::new(0));
        let actual = count.clone();
        let (finished, completed) = std::sync::mpsc::channel();
        {
            let _entered = runtime.enter();
            finalizers.run(move || {
                actual.fetch_add(1, Ordering::AcqRel);
                finished.send(()).unwrap();
            });
        }
        runtime.shutdown_background();
        completed
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("queued archive must survive cancellation of the blocking pool");
        assert_eq!(count.load(Ordering::Acquire), 1);
        release.send(()).unwrap();
        for task in finalizers.take() {
            task.join_blocking();
        }
        assert_eq!(count.load(Ordering::Acquire), 1);
    }
}
