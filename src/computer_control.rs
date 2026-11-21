//! Local same-user switch for legacy mutations and SSMCP's scoped CLI observations.
//! Standalone programs launched independently retain their OS permissions.
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

/// Only an explicitly enabled generation may produce a scoped observation.
pub(crate) struct ObservationPermit {
    store: Store,
    generation: [u8; 16],
}

impl ObservationPermit {
    pub(crate) fn local() -> Result<Self, String> {
        Self::acquire(Store::shared()?.observations())
    }

    fn acquire(store: Store) -> Result<Self, String> {
        let state = store.read()?.filter(|s| s.enabled).ok_or(
            "Scoped observation is off; enable it locally with ssmcp computer observation on",
        )?;
        Ok(Self {
            store,
            generation: state.generation,
        })
    }

    pub(crate) fn check(&self) -> Result<(), String> {
        if self
            .store
            .read()?
            .is_some_and(|s| s.enabled && s.generation == self.generation)
        {
            Ok(())
        } else {
            Err("Scoped observation was revoked".into())
        }
    }

    pub(crate) async fn revoked(&self) -> String {
        loop {
            if let Err(error) = self.check() {
                return error;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

pub(crate) fn observation_state(enabled: Option<bool>) -> Result<Value, String> {
    let store = Store::shared()?.observations();
    let state = match enabled {
        None => store.read()?,
        Some(value) => Some(store.update(Some(value))?),
    };
    Ok(json!({"scope":"local_zuse_observations", "enabled":state.is_some_and(|s| s.enabled)}))
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
    let legacy = Store::shared()?;
    if enabled {
        return update_status(&legacy, Some(true));
    }
    disable(&legacy)
}

fn disable(legacy: &Store) -> Result<Value, String> {
    // Attempt both even when one store is corrupt or cannot be locked.
    let previous = update_status(legacy, Some(false));
    let observation = legacy.observations().update(Some(false));
    match (observation, previous) {
        (Ok(_), Ok(mut state)) => {
            state["scoped_observations_disabled"] = json!(true);
            Ok(state)
        }
        (Err(a), Err(b)) => Err(format!(
            "Scoped observation Off failed: {a}; legacy Off failed: {b}"
        )),
        (Err(e), _) => Err(format!(
            "Legacy Off published; scoped observation Off failed: {e}"
        )),
        (_, Err(e)) => Err(format!(
            "Scoped observation Off published; legacy Off failed: {e}"
        )),
    }
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
