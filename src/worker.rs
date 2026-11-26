//! Bounded worker task service. Remote slots are local coordination capacity;
//! explicit local executors retain their separate owned processes and limits.
mod config;
pub(crate) use config::Config;
mod activity;
pub(crate) use activity::Activity;
#[cfg(target_os = "linux")]
mod child;
#[cfg(target_os = "linux")]
mod executor;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod limits;
mod pool;
pub(crate) mod productivity;
mod queue;
#[cfg(target_os = "linux")]
mod registry;
mod remote;
pub(crate) use pool::Pool;
#[cfg(target_os = "linux")]
pub(crate) use pool::TaskContext;
pub(crate) use remote::CommandLog;

/// Maximum time spent observing one worker request; queued work outlives the call.
pub(crate) const MAX_OBSERVATION_WAIT_MS: u64 = crate::wire::MAX_OBSERVATION_WAIT_MS;

/// Shared by the coordinator's generated message and the terminal history decoder.
pub(crate) const IDLE_STATION_OBSERVATION: &str = " registered station(s) were idle when observed. Workers may be between assignment waits. Use ssmcp.worker.submit for independent work.";

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

/// Verified participant activity shared by the display and its current queue.
type MemberActivity = BTreeMap<String, crate::wire::WorkerMemberActivity>;

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SlotLog {
    pub(crate) sequence: u64,
    pub(crate) at_ms: u64,
    pub(crate) level: String,
    pub(crate) text: String,
    /// Local display purpose from the observed native job, tied to its original claim.
    #[serde(skip)]
    pub(crate) activity_title: Option<String>,
    pub(crate) task_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct SlotView {
    pub(crate) slot: usize,
    pub(crate) epoch: u64,
    pub(crate) state: &'static str,
    /// Display-only relay observation. An idle station stays available for work.
    pub(crate) dreaming: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_calls_10m: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activity_status: Option<&'static str>,
    pub(crate) task_id: Option<String>,
    pub(crate) reason: Option<String>,
    pub(crate) logs: Vec<SlotLog>,
    pub(crate) next_log_sequence: u64,
    #[serde(default)]
    pub(crate) registered: bool,
    #[serde(default)]
    pub(crate) retire_requested: bool,
    #[serde(default)]
    pub(crate) task_summary: Option<String>,
    /// Full accepted task text stays in-process for the worker detail view.
    #[serde(skip)]
    pub(crate) task_text: Option<String>,
    #[serde(default)]
    pub(crate) last_command: Option<String>,
    #[serde(default)]
    pub(crate) progress: Option<String>,
    #[serde(default)]
    pub(crate) total_work_ms: u64,
    #[serde(default)]
    pub(crate) current_task_elapsed_ms: Option<u64>,
    #[serde(default)]
    pub(crate) task_started_at_ms: Option<u64>,
    #[serde(default)]
    pub(crate) recent_task_durations_ms: Vec<u64>,
}

impl SlotView {
    pub(crate) fn display_state(&self) -> &'static str {
        if self.state == "idle" && self.dreaming {
            "dreaming"
        } else if self.activity_status == Some("offline") {
            "offline"
        } else {
            self.state
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagerView {
    pub(crate) client_id: String,
    pub(crate) thread_id: String,
    pub(crate) participant_id: String,
    pub(crate) connection: u64,
    pub(crate) attached_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagerTargetView {
    pub(crate) kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) slot: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) revision: Option<u64>,
    /// Bounded, redacted text from the already-authorized task, never its result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ManagerOperationView {
    pub(crate) operation_id: u64,
    pub(crate) kind: &'static str,
    pub(crate) state: &'static str,
    pub(crate) mode: Option<&'static str>,
    pub(crate) targets: Vec<ManagerTargetView>,
    pub(crate) task_id: Option<String>,
    pub(crate) slot: Option<usize>,
    pub(crate) epoch: Option<u64>,
    pub(crate) summary: Option<String>,
    pub(crate) started_at_ms: u64,
    pub(crate) elapsed_ms: u64,
    pub(crate) wait_ms: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Snapshot {
    pub(crate) process_id: u32,
    pub(crate) generation: String,
    pub(crate) manager: Option<ManagerView>,
    pub(crate) total: usize,
    pub(crate) idle: usize,
    pub(crate) working: usize,
    pub(crate) offline: usize,
    #[serde(default)]
    pub(crate) retiring: usize,
    #[serde(default)]
    pub(crate) registered_workers: usize,
    #[serde(default)]
    pub(crate) desired_total: usize,
    pub(crate) queued: usize,
    pub(crate) closed: bool,
    pub(crate) stopping: bool,
    pub(crate) slots: Vec<SlotView>,
    #[serde(default)]
    pub(crate) manager_operations: Vec<ManagerOperationView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) productivity: Option<productivity::Productivity>,
}

#[derive(Default)]
struct Display {
    pending_dreaming_members: Vec<String>,
    member_activity: Arc<MemberActivity>,
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
        process_id: std::process::id(),
        generation: String::new(),
        manager: None,
        total: config.count,
        idle: 0,
        working: 0,
        offline: config.count,
        retiring: 0,
        registered_workers: 0,
        desired_total: config.count,
        queued: 0,
        closed: false,
        stopping: false,
        manager_operations: Vec::new(),
        productivity: None,
        slots: (0..config.count)
            .map(|slot| SlotView {
                slot,
                epoch: 0,
                state: "offline",
                dreaming: false,
                activity_status: None,
                tool_calls_10m: None,
                task_id: None,
                reason: Some("waiting for the authenticated worker connection".into()),
                logs: Vec::new(),
                next_log_sequence: 1,
                registered: false,
                retire_requested: false,
                task_summary: None,
                task_text: None,
                last_command: None,
                progress: None,
                total_work_ms: 0,
                current_task_elapsed_ms: None,
                task_started_at_ms: None,
                recent_task_durations_ms: Vec::new(),
            })
            .collect(),
    });
    view.pool = None;
    view.pending_dreaming_members.clear();
    view.member_activity = Arc::default();
}

pub(crate) fn set_dreaming_members(members: Vec<String>) {
    {
        let mut view = view().lock().unwrap_or_else(|error| error.into_inner());
        if let Some(pool) = view.pool.as_ref().and_then(std::sync::Weak::upgrade) {
            pool.queue.set_dreaming_members(members);
        } else {
            // The relay can send its roster before native pool initialization.
            view.pending_dreaming_members = members;
        }
    }
    crate::client::terminal::invalidate();
}

pub(crate) fn set_member_activity(members: MemberActivity) {
    let mut display = view().lock().unwrap_or_else(|error| error.into_inner());
    display.member_activity = Arc::new(members);
    if let Some(pool) = display.pool.as_ref().and_then(std::sync::Weak::upgrade) {
        pool.queue
            .set_member_activity(display.member_activity.clone());
    }
    drop(display);
    crate::client::terminal::invalidate();
}

pub(crate) fn display() -> Option<Snapshot> {
    let view = view().lock().unwrap_or_else(|error| error.into_inner());
    if let Some(pool) = view.pool.as_ref().and_then(std::sync::Weak::upgrade) {
        return Some(pool.queue.snapshot());
    }
    view.pending.clone()
}

/// Counts and task activity must describe the same queue revision. The parent
/// window receives this bounded projection rather than copying worker logs.
pub(crate) fn display_with_activity() -> Option<(Snapshot, Activity)> {
    let view = view().lock().unwrap_or_else(|error| error.into_inner());
    if let Some(pool) = view.pool.as_ref().and_then(std::sync::Weak::upgrade) {
        return Some(pool.queue.snapshot_with_activity());
    }
    view.pending
        .clone()
        .map(|snapshot| (snapshot, Activity::default()))
}

/// Kept by the real native job, not by a later read/wait caller. Holding the
/// original pool also prevents a reconnect from redirecting its end event.
pub(crate) struct NativeCommandLog {
    pool: std::sync::Arc<Pool>,
    command: CommandLog,
}

impl NativeCommandLog {
    pub(crate) fn finish(self, detail: &str) {
        // Activity logging must not alter the actual command's exit result.
        let _ = self.pool.command_finished(&self.command, detail);
    }
}

/// Called only after a native command has successfully started. Display state
/// finds a candidate pool; verified client/connection and exact claim identity
/// are still required before any command can be attributed to it.
pub(crate) fn native_command_started(
    source: &crate::runtime::call_scope::Source,
    command_id: &str,
    argv: &[String],
    title: Option<&str>,
) -> Option<NativeCommandLog> {
    let call = source.call.as_ref()?;
    let client_id = call.client_id()?;
    let pool = view()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .pool
        .as_ref()
        .and_then(std::sync::Weak::upgrade)?;
    let manager = pool.queue.snapshot().manager?;
    if manager.client_id != client_id || manager.connection != call.connection() {
        return None;
    }
    // JSON preserves argv boundaries; this string is display data, never shell
    // input. Pool applies its bounded, control-character-safe display policy.
    let command = serde_json::to_string(argv).ok()?;
    let command = pool
        .command_started(source, command_id, &command, title)
        .ok()??;
    Some(NativeCommandLog { pool, command })
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
