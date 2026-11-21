//! Shared Computer Use switch for this machine's OS user.
mod storage;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use storage::Store;
use tokio::sync::Notify;

pub(super) use storage::InputLease;
pub(super) use storage::WorkerTicket;

const POLL_INTERVAL: Duration = Duration::from_millis(25);
pub(crate) const LOCKED: &str = "Computer Use is Off; use /computer on or Super+Alt+C";
pub(crate) const ENDED: &str = "Computer Use was revoked; effects already dispatched may have completed. Inspect state before retrying";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct State {
    schema_version: u8,
    enabled: bool,
    generation: [u8; 16],
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: 1,
            enabled: false,
            generation: [0; 16],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Ticket {
    generation: [u8; 16],
}

pub(crate) struct HostControl {
    store: Result<Arc<Store>, String>,
    stopped: AtomicBool,
    changed: Notify,
}

impl Default for HostControl {
    fn default() -> Self {
        #[cfg(not(test))]
        let store = {
            // Initialize a fresh store once per process. Reconnection must not
            // recreate an on setting after a live store disappears or fails.
            static STORE: OnceLock<Result<Arc<Store>, String>> = OnceLock::new();
            STORE.get_or_init(|| Store::shared().map(Arc::new)).clone()
        };
        // Unit tests must never observe or change the operator's real switch.
        #[cfg(test)]
        let store = Store::temporary().map(Arc::new);
        Self::with_store(store)
    }
}

pub(super) fn local() -> &'static HostControl {
    static CONTROL: OnceLock<HostControl> = OnceLock::new();
    CONTROL.get_or_init(HostControl::default)
}

pub(super) fn input_tool(name: &str) -> bool {
    matches!(
        name,
        "ssmcp.computer.focus"
            | "ssmcp.computer.move"
            | "ssmcp.computer.move_path"
            | "ssmcp.computer.click"
            | "ssmcp.computer.drag"
            | "ssmcp.computer.key"
            | "ssmcp.computer.num_lock"
            | "ssmcp.computer.watch_until_click"
            | "ssmcp.computer.type"
            | "ssmcp.computer.scroll"
    )
}

pub(super) fn mutation_tool(name: &str) -> bool {
    input_tool(name)
        || matches!(
            name,
            "ssmcp.computer.desktop_start" | "ssmcp.computer.app_launch"
        )
}

tokio::task_local! {
    static PERMIT: Permit;
}

/// Bound to the generation accepted at ingress, including after a queue wait.
#[derive(Clone)]
pub(crate) struct Permit {
    control: Arc<HostControl>,
    ticket: Ticket,
    // A detached CDP press/release keeps this lease through its final release.
    lease: Option<Arc<InputLease>>,
}

impl Permit {
    pub(crate) fn current() -> Option<Self> {
        PERMIT.try_with(Clone::clone).ok()
    }

    pub(crate) fn check_current() -> Result<(), String> {
        PERMIT.try_with(Self::check).unwrap_or(Ok(()))
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if self.control.valid(self.ticket) {
            Ok(())
        } else {
            Err(ENDED.into())
        }
    }

    pub(crate) fn same_generation(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.control, &other.control) && self.ticket == other.ticket
    }

    pub(crate) fn without_lease(&self) -> Self {
        Self {
            lease: None,
            ..self.clone()
        }
    }

    /// Publish a newly acquired handle while the persisted generation is locked.
    pub(crate) fn commit<T>(&self, publish: impl FnOnce() -> T) -> Result<T, String> {
        self.control.store()?.inspect(|state| {
            if !self.control.stopped.load(Ordering::Acquire)
                && state.enabled
                && state.generation == self.ticket.generation
            {
                Ok(publish())
            } else {
                Err(ENDED.into())
            }
        })
    }

    pub(crate) async fn serialized(mut self) -> Result<Self, String> {
        self.lease = Some(Arc::new(self.control.acquire(self.ticket).await?));
        Ok(self)
    }

    pub(crate) async fn cancelled(&self) {
        self.control.cancelled(self.ticket).await;
    }

    pub(crate) async fn scope<T>(&self, future: impl Future<Output = T>) -> T {
        PERMIT.scope(self.clone(), future).await
    }

    pub(crate) async fn run<T>(
        &self,
        future: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        self.control.run(self.ticket, self.scope(future)).await
    }

    pub(super) fn ticket(&self) -> Ticket {
        self.ticket
    }

    pub(super) fn worker_ticket(&self) -> Result<WorkerTicket, String> {
        self.control.worker_ticket(self.ticket)
    }
}

fn generation() -> Result<[u8; 16], String> {
    let mut bytes = [0; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|error| error.to_string())?;
    if bytes == [0; 16] {
        return Err("could not create a computer-use generation".into());
    }
    Ok(bytes)
}

fn generation_text(bytes: [u8; 16]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl HostControl {
    fn with_store(store: Result<Arc<Store>, String>) -> Self {
        Self {
            store,
            stopped: AtomicBool::new(false),
            changed: Notify::new(),
        }
    }

    fn store(&self) -> Result<&Arc<Store>, String> {
        self.store.as_ref().map_err(Clone::clone)
    }

    #[cfg(test)]
    fn at(path: std::path::PathBuf) -> Self {
        Self::with_store(Ok(Arc::new(Store::at(path))))
    }

    pub(crate) fn permit(self: &Arc<Self>) -> Result<Permit, String> {
        Ok(Permit {
            control: Arc::clone(self),
            ticket: self.ticket()?,
            lease: None,
        })
    }

    /// Called by local UI and CLI entry points, never a native tool.
    pub(crate) fn set_local(&self, enabled: bool) -> Result<Value, String> {
        self.update_local(Some(enabled))
    }

    pub(super) fn toggle_local(&self) -> Result<Value, String> {
        self.update_local(None)
    }

    fn update_local(&self, enabled: Option<bool>) -> Result<Value, String> {
        if self.stopped.load(Ordering::Acquire) && enabled != Some(false) {
            return Err("computer connection is closed".into());
        }
        let state = self.store()?.update(enabled)?;
        self.changed.notify_waiters();
        Ok(status_value(state, None))
    }

    /// Closing one connection cancels only its own tickets, not the user's
    /// persistent setting or other live ssmcp sessions.
    pub(crate) fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(super) fn ticket(&self) -> Result<Ticket, String> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(ENDED.into());
        }
        let state = self.store()?.read()?;
        if !state.enabled {
            return Err(LOCKED.into());
        }
        Ok(Ticket {
            generation: state.generation,
        })
    }

    pub(super) fn worker_ticket(&self, ticket: Ticket) -> Result<WorkerTicket, String> {
        if !self.valid(ticket) {
            return Err(ENDED.into());
        }
        Ok(self.store()?.worker_ticket(ticket.generation))
    }

    fn valid(&self, ticket: Ticket) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && self
                .store()
                .and_then(|store| store.read())
                .is_ok_and(|state| state.enabled && state.generation == ticket.generation)
    }

    pub(super) async fn cancelled(&self, ticket: Ticket) {
        loop {
            // Register first so a local off/close cannot be lost between
            // checking the file and waiting. Polling observes other processes.
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.valid(ticket) {
                return;
            }
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    pub(super) async fn acquire(&self, ticket: Ticket) -> Result<InputLease, String> {
        loop {
            if !self.valid(ticket) {
                return Err(ENDED.into());
            }
            if let Some(lease) = self.store()?.try_input()? {
                // Off or off->on while queued must not rearm this old call.
                if !self.valid(ticket) {
                    return Err(ENDED.into());
                }
                return Ok(lease);
            }
            tokio::select! {
                biased;
                _ = self.cancelled(ticket) => return Err(ENDED.into()),
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
    }

    pub(super) async fn run<T>(
        &self,
        ticket: Ticket,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::select! {
            biased;
            _ = self.cancelled(ticket) => Err(ENDED.into()),
            result = operation => {
                if self.valid(ticket) { result } else { Err(ENDED.into()) }
            }
        }
    }

    pub(crate) fn status(&self) -> Value {
        match self.store().and_then(|store| store.read()) {
            Ok(state) => status_value(state, None),
            Err(error) => status_value(State::default(), Some(error)),
        }
    }
}

fn status_value(state: State, error: Option<String>) -> Value {
    let mut status = json!({
        "enabled":state.enabled,
        "mode":if state.enabled {"on"} else {"off"},
        "background_enabled":state.enabled,
        "enforcement_scope":"SSMCP computer, browser, Omarchy and Playwright mutations",
        "os_permission_boundary":false,
        "scope":"machine_os_user",
        "generation":generation_text(state.generation),
        "local_on":"/computer on",
        "local_off":"/computer off",
        "local_toggle":"Super+Alt+C",
        "revoked_by":"local off or Super+Alt+C; closing a connection cancels only its calls",
        "all_application_input_detection":false,
        "serialization_scope":"this machine and OS user"
    });
    if let Some(error) = error {
        status["error"] = json!(error);
    }
    status
}

#[cfg(test)]
mod tests;
