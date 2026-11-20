//! Explicit local foreground input switch, shared by this machine's OS user.
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

const POLL_INTERVAL: Duration = Duration::from_millis(25);
pub(super) const LOCKED: &str = "host input is off; the local operator can use /computer on or Super+Alt+C. Isolated desktop and background browser control remain enabled";
pub(super) const ENDED: &str =
    "host input was switched off or this connection closed; control returned to the operator";

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

#[derive(Clone, Copy, Debug)]
pub(super) struct Ticket {
    generation: [u8; 16],
}

pub(super) struct HostControl {
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
            | "ssmcp.computer.click"
            | "ssmcp.computer.drag"
            | "ssmcp.computer.key"
            | "ssmcp.computer.type"
            | "ssmcp.computer.scroll"
    )
}

fn generation() -> Result<[u8; 16], String> {
    let mut bytes = [0; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|error| error.to_string())?;
    if bytes == [0; 16] {
        return Err("could not create a foreground input generation".into());
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

    /// Called only by decoded local operator commands, never a native tool.
    pub(super) fn set_local(&self, enabled: bool) -> Result<Value, String> {
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
    pub(super) fn close(&self) {
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

    pub(super) fn status(&self) -> Value {
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
        "background_enabled":true,
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
