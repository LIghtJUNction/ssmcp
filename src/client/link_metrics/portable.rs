use super::{ENV, ID_ENV, Status};
use tokio::{process::Command, sync::watch, task::JoinHandle};

pub(in crate::client) struct Prepared;

pub(super) fn prepare(command: &mut Command) -> Option<Prepared> {
    command.env_remove(ENV).env_remove(ID_ENV);
    None
}

pub(super) enum Reporter {}

pub(super) fn reporter() -> Option<Reporter> {
    None
}

impl Reporter {
    pub(super) fn send(&self, _bytes: &[u8; 32]) -> std::io::Result<()> {
        match *self {}
    }
}

pub(super) fn observe(
    _prepared: Option<Prepared>,
    closed: watch::Receiver<Option<String>>,
    mut report: impl FnMut(Status) + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let connected = closed.borrow().is_none();
        if connected {
            report(Status::Unavailable);
            super::wait_closed(closed).await;
        }
        report(Status::Disconnected);
    })
}
