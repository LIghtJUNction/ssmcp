//! Explicit compatibility switch for older SSMCP mutation providers.
//!
//! This module only reads and writes the legacy same-user state protocol.
//! Current Zuse lifecycle tools, general exec and the standalone Zuse CLI do
//! not consult it. No desktop engine, worker, input dispatch or shortcut lives
//! here; only an explicit local on/off/toggle may write the shared state.
mod storage;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use storage::Store;

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

/// Observe the old setting without creating a directory or initializing it.
pub(crate) fn status() -> Value {
    match Store::shared() {
        Ok(store) => read_status(&store),
        Err(error) => status_value(None, Some(error)),
    }
}

/// An explicit local command, never a remote computer tool.
pub(crate) fn set_local(enabled: bool) -> Result<Value, String> {
    update_status(&Store::shared()?, Some(enabled))
}

/// An explicit local command, never a default keyboard binding.
pub(crate) fn toggle_local() -> Result<Value, String> {
    update_status(&Store::shared()?, None)
}

fn read_status(store: &Store) -> Value {
    match store.read() {
        Ok(state) => status_value(state, None),
        Err(error) => status_value(None, Some(error)),
    }
}

fn update_status(store: &Store, enabled: Option<bool>) -> Result<Value, String> {
    store
        .update(enabled)
        .map(|state| status_value(Some(state), None))
}

fn status_value(state: Option<State>, error: Option<String>) -> Value {
    let available = state.is_some() && error.is_none();
    let enabled = available && state.is_some_and(|state| state.enabled);
    let mode = if error.is_some() {
        "unavailable"
    } else if !available {
        "unset"
    } else if enabled {
        "on"
    } else {
        "off"
    };
    let mut status = json!({
        "schema_version": 1,
        "scope": "machine_os_user",
        "compatibility_only": true,
        "enforcement_scope": "legacy SSMCP mutation providers",
        "controls_this_version": false,
        "standalone_zuse_controlled": false,
        "os_permission_boundary": false,
        "available": available,
        "enabled": enabled,
        "mode": mode,
        "generation": state.map(|state| generation_text(state.generation)),
        "local_on": "/computer on",
        "local_off": "/computer off",
        "local_toggle": "/computer toggle"
    });
    if let Some(error) = error {
        status["error"] = json!(error);
    }
    status
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
