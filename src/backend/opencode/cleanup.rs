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
        if let Self::Thread(task) = self {
            let task = task.take().unwrap();
            *self = Self::Runtime(tokio::task::spawn_blocking(move || {
                let _ = task.join();
            }));
        }
        if let Self::Runtime(task) = self {
            let _ = task.await;
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

    pub(super) fn run(&self, work: impl FnOnce() + Send + 'static) {
        let task = match tokio::runtime::Handle::try_current() {
            Ok(runtime) => Finalizer::Runtime(runtime.spawn_blocking(work)),
            // Destruction outside a runtime must still retain the partial capture.
            // It must not perform disk I/O on the thread dropping the transport.
            Err(_) => Finalizer::Thread(Some(std::thread::spawn(work))),
        };
        self.insert(task);
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
        // Cancelled stream destructors have now published all their finalizers.
        // Close the event archive only after their partial-artifact records exist.
        let events = self.events.take().unwrap();
        let _ = tokio::task::spawn_blocking(move || events.close()).await;
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
