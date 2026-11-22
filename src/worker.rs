//! Bounded worker task service. Remote slots are local coordination capacity;
//! explicit local executors retain their separate owned processes and limits.
mod config;
pub(crate) use config::Config;
#[cfg(target_os = "linux")]
mod child;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(any(target_os = "linux", test))]
mod limits;
mod pool;
mod queue;
#[cfg(target_os = "linux")]
mod registry;
mod remote;
pub(crate) use pool::Pool;
#[cfg(target_os = "linux")]
pub(crate) use pool::TaskContext;

use serde::Serialize;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SlotLog {
    pub(crate) sequence: u64,
    pub(crate) at_ms: u64,
    pub(crate) level: String,
    pub(crate) text: String,
    pub(crate) task_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SlotView {
    pub(crate) slot: usize,
    pub(crate) epoch: u64,
    pub(crate) state: &'static str,
    pub(crate) task_id: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) logs: Vec<SlotLog>,
    pub(crate) next_log_sequence: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) generation: String,
    pub(crate) total: usize,
    pub(crate) idle: usize,
    pub(crate) working: usize,
    pub(crate) offline: usize,
    pub(crate) queued: usize,
    pub(crate) closed: bool,
    pub(crate) slots: Vec<SlotView>,
}

#[derive(Default)]
struct Display {
    pending: Option<Snapshot>,
    pool: Option<std::sync::Weak<Pool>>,
}

fn view() -> &'static Mutex<Display> {
    static DISPLAY: OnceLock<Mutex<Display>> = OnceLock::new();
    DISPLAY.get_or_init(Default::default)
}

/// The first monitor frame is already a worker page, before backend discovery.
/// This only publishes in-process display data; it does not initialize limits.
pub(crate) fn prepare_display(config: Config) {
    let mut view = view().lock().unwrap_or_else(|error| error.into_inner());
    view.pending = Some(Snapshot {
        generation: String::new(),
        total: config.count,
        idle: 0,
        working: 0,
        offline: config.count,
        queued: 0,
        closed: false,
        slots: (0..config.count)
            .map(|slot| SlotView {
                slot,
                epoch: 0,
                state: "offline",
                task_id: None,
                reason: Some("waiting for the authenticated worker connection".into()),
                logs: Vec::new(),
                next_log_sequence: 1,
            })
            .collect(),
    });
    view.pool = None;
}

pub(crate) fn display() -> Option<Snapshot> {
    let view = view().lock().unwrap_or_else(|error| error.into_inner());
    if let Some(pool) = view.pool.as_ref().and_then(std::sync::Weak::upgrade) {
        return Some(pool.queue.snapshot());
    }
    view.pending.clone()
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

/// Checked before ordinary CLI initialization; the private child never starts
/// another SSMCP connection, general shell backend, installer or model by default.
pub(crate) async fn child_entry() -> Option<crate::Result<()>> {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--ssmcp-worker-child")) {
        return None;
    }
    if std::env::args_os().count() != 2 {
        return Some(Err(
            "private worker entry accepts no command-line payload".into()
        ));
    }
    #[cfg(target_os = "linux")]
    {
        Some(child::run().await.map_err(Into::into))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(Err(
            "worker hard limits currently require Linux cgroup v2 and the user's systemd manager"
                .into(),
        ))
    }
}
