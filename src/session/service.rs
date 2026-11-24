//! One foreground, same-user owner for saved session processes.
//!
//! IPC authenticates callers before they enter a bounded, serialized operation
//! queue. Process ownership lives here, never in a PID file or attached client.
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    process::Command,
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout, timeout_at},
};

use super::{Manifest, NativeStatus, RunGeneration, SessionSpec, ipc};
use crate::{Result, backend::process::Managed};

const ENDPOINT: &str = "service.json";
const CAPACITY: usize = 32;
const OPERATION_TIMEOUT: Duration = Duration::from_secs(35);
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(250);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceEpoch {
    epoch: String,
}

/// Cooperative owner-loss detection complements Linux guardians/Windows Jobs.
/// In particular, macOS launchd does not own a worker's separate process group.
pub(super) struct OwnerLease {
    stop: Arc<AtomicBool>,
    lost: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OwnerLease {
    pub(super) fn start(root: PathBuf, epoch: String) -> Result<Self> {
        if !owner_active(&root, &epoch)? {
            return Err("session supervisor lease is no longer current".into());
        }
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let cancelled = stop.clone();
        let owner_lost = lost.clone();
        let thread = std::thread::Builder::new()
            .name("ssmcp-session-owner".into())
            .spawn(move || {
                loop {
                    std::thread::park_timeout(TICK);
                    if cancelled.load(Ordering::Acquire) {
                        return;
                    }
                    if !owner_active(&root, &epoch).unwrap_or(false) {
                        owner_lost.store(true, Ordering::Release);
                        // Signal only this live process, never a PID read from disk.
                        // Once initialized, the client's existing TERM handler
                        // shuts down Runtime and every owned child before exiting.
                        #[cfg(unix)]
                        if !cancelled.load(Ordering::Acquire) {
                            unsafe {
                                libc::kill(libc::getpid(), libc::SIGTERM);
                            }
                        }
                        return;
                    }
                }
            })?;
        Ok(Self {
            stop,
            lost,
            thread: Some(thread),
        })
    }

    pub(super) fn lost(&self) -> bool {
        self.lost.load(Ordering::Acquire)
    }
}

impl Drop for OwnerLease {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn owner_active(root: &Path, expected: &str) -> Result<bool> {
    if expected.len() != 32 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let actual: ServiceEpoch = super::read_json(&root.join("service-epoch.json"))?;
    if actual.epoch != expected {
        return Ok(false);
    }
    let file = super::private_open(&root.join("service.lock"), true, false, false)?;
    match crate::platform::lock_file(&file, true) {
        Ok(()) => Ok(false), // Acquiring it proves the supervisor no longer holds it.
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn runner_active(directory: &Path) -> Result<bool> {
    let file = super::private_open(&directory.join("run.lock"), true, true, false)?;
    match crate::platform::lock_file(&file, true) {
        Ok(()) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Ping,
    List,
    Status {
        name: String,
        expected_generation: Option<String>,
    },
    Create {
        spec: SessionSpec,
    },
    Start {
        name: String,
    },
    Recover {
        name: String,
        expected_definition_id: String,
    },
    Restart {
        name: String,
        expected_generation: Option<String>,
    },
    Close {
        name: String,
        expected_generation: Option<String>,
    },
    Remove {
        name: String,
    },
}

struct Pending {
    request: Request,
    reply: oneshot::Sender<std::result::Result<Value, String>>,
    deadline: Instant,
}

struct Worker {
    generation: String,
    started_at: u64,
    // On Linux this is the owned guardian, not the runner observed in state.json.
    pid: u32,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<io::Result<ExitStatus>>,
    stopping: bool,
}

impl Worker {
    fn spawn(command: &mut Command, generation: RunGeneration) -> Result<Self> {
        let mut process = Managed::spawn(command)?;
        let pid = process
            .child
            .id()
            .ok_or("session process has no owned identity")?;
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::select! {
                status = process.wait() => status,
                _ = stopped => {
                    process.shutdown().await;
                    process.child.wait().await
                }
            }
        });
        Ok(Self {
            generation: generation.generation,
            started_at: generation.started_at,
            pid,
            stop: Some(stop),
            task,
            stopping: false,
        })
    }

    fn stop(&mut self) {
        self.stopping = true;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[derive(Default)]
struct Outcome {
    failed: bool,
    exit_code: Option<i64>,
}

struct Supervisor {
    root: PathBuf,
    epoch: String,
    workers: BTreeMap<String, Worker>,
    retries: BTreeMap<String, Instant>,
    recoveries: BTreeMap<String, Instant>,
    outcomes: BTreeMap<String, Outcome>,
}

impl Supervisor {
    fn new(root: PathBuf, epoch: String) -> Self {
        Self {
            root,
            epoch,
            workers: BTreeMap::new(),
            retries: BTreeMap::new(),
            recoveries: BTreeMap::new(),
            outcomes: BTreeMap::new(),
        }
    }

    fn saved(&self) -> Result<Vec<(PathBuf, Manifest)>> {
        let mut saved = Vec::new();
        let mut count = 0;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with("session-")
                || !entry.file_type()?.is_dir()
                || entry.file_type()?.is_symlink()
            {
                continue;
            }
            count += 1;
            if count > 1024 {
                return Err("too many saved sessions to enumerate safely".into());
            }
            let definition = (|| -> Result<_> {
                let manifest: Manifest = super::read_json(&entry.path().join("manifest.json"))?;
                super::load(&self.root, &manifest.spec.name)
            })();
            match definition {
                Ok((directory, manifest)) if manifest.supervised => {
                    saved.push((directory, manifest))
                }
                Ok(_) => {}
                Err(error) => report("load saved definition", &error.to_string()),
            }
        }
        saved.sort_by(|a, b| a.1.spec.name.cmp(&b.1.spec.name));
        Ok(saved)
    }

    fn load(&self, name: &str) -> Result<(PathBuf, Manifest)> {
        let (directory, manifest) = super::load(&self.root, name)?;
        if !manifest.supervised {
            return Err(
                "session belongs to its legacy native manager; use sessions start/stop/restart"
                    .into(),
            );
        }
        Ok((directory, manifest))
    }

    fn start_worker(&mut self, directory: &Path, manifest: &Manifest) -> Result<()> {
        let name = &manifest.spec.name;
        if self.workers.contains_key(name) {
            return Err("session is already running or stopping".into());
        }
        // The ownership lock, not a stored PID, detects a surviving old runner.
        let guard = super::lock(&directory.join("run.lock"), true)?;
        super::verify_snapshot(directory, manifest)?;
        crate::client::validate_session_spec(&manifest.spec)?;
        if !desired_running(directory)? {
            return Err("session is deliberately closed; start it explicitly".into());
        }
        super::write_management_state(directory, "starting", None)?;
        let generation = super::RunGeneration {
            generation: super::random_suffix()?,
            started_at: super::now(),
            supervisor_pid: std::process::id(),
            service_epoch: self.epoch.clone(),
        };
        super::write_json(&directory.join("generation.json"), &generation)?;
        let mut command =
            super::limits::managed_command(directory, manifest, &generation.generation)?;
        command
            .current_dir(&manifest.spec.cwd)
            .envs(&manifest.environment)
            .env("SSMCP_SESSION_GENERATION", &generation.generation)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        // A runner must take this same lock. Drop immediately before spawning;
        // the serialized actor remains the sole publisher of generations.
        drop(guard);
        self.workers
            .insert(name.clone(), Worker::spawn(&mut command, generation)?);
        self.retries.remove(name);
        self.recoveries.remove(name);
        self.outcomes.remove(name);
        Ok(())
    }

    async fn reap(&mut self, name: &str) -> Result<()> {
        if !self
            .workers
            .get(name)
            .is_some_and(|worker| worker.task.is_finished())
        {
            return Ok(());
        }
        let result = (&mut self.workers.get_mut(name).expect("worker exists").task).await;
        let worker = self.workers.remove(name).expect("worker exists");
        self.finished(name, worker.stopping, result)
    }

    fn finished(
        &mut self,
        name: &str,
        stopping: bool,
        result: std::result::Result<io::Result<ExitStatus>, tokio::task::JoinError>,
    ) -> Result<()> {
        let (failed, exit_code, error) = match result {
            Ok(Ok(status)) => (
                !status.success(),
                status.code().map(i64::from),
                (!status.success() && !stopping)
                    .then(|| format!("session process exited with {status}")),
            ),
            Ok(Err(error)) => (
                true,
                None,
                Some(format!("session process wait failed: {error}")),
            ),
            Err(error) => (
                true,
                None,
                Some(format!("session process owner failed: {error}")),
            ),
        };
        self.outcomes.insert(
            name.into(),
            Outcome {
                failed: failed && !stopping,
                exit_code,
            },
        );
        let (directory, manifest) = self.load(name)?;
        let desired = desired_running(&directory)?;
        super::write_management_state(
            &directory,
            if desired { "disconnected" } else { "stopped" },
            error.as_deref(),
        )?;
        if restart_after_exit(desired, manifest.spec.watchdog, failed, stopping) {
            self.retries.insert(
                name.into(),
                Instant::now() + Duration::from_secs(manifest.spec.reconnect_delay_secs),
            );
        }
        Ok(())
    }

    async fn stop_worker(&mut self, name: &str, directory: &Path) -> Result<()> {
        self.retries.remove(name);
        self.recoveries.remove(name);
        if let Some(worker) = self.workers.get_mut(name) {
            worker.stop();
            // Keep the handle in the map across await: cancellation or service
            // shutdown must retain ownership of a child still being reaped.
            let result = timeout(STOP_TIMEOUT, &mut worker.task).await.map_err(
                |_| "session termination is not confirmed; inspect status before retrying",
            )?;
            let confirmed = matches!(result, Ok(Ok(_)));
            let worker = self.workers.remove(name).expect("worker exists");
            self.finished(name, worker.stopping, result)?;
            if !confirmed {
                return Err("owned process wait failed; termination is not confirmed".into());
            }
        }
        // A prior service may still be cleaning up its owned tree. We never
        // signal the PID in a generation/observation file to hurry it along.
        drop(super::lock(&directory.join("run.lock"), true)?);
        Ok(())
    }

    fn status(&self, directory: &Path, manifest: &Manifest) -> Result<Value> {
        let name = &manifest.spec.name;
        let generation = generation(directory)?;
        let native = if let Some(worker) = self.workers.get(name) {
            NativeStatus {
                state: if worker.stopping {
                    "stopping"
                } else {
                    "running"
                }
                .into(),
                active: true,
                pid: Some(worker.pid),
                exit_code: None,
                started_at: Some(worker.started_at),
                generation: Some(worker.generation.clone()),
            }
        } else {
            let retrying = self.retries.contains_key(name);
            let previous_runner = runner_active(directory)?;
            let closing = !desired_running(directory)?;
            let outcome = self.outcomes.get(name);
            NativeStatus {
                state: if previous_runner {
                    if closing { "stopping" } else { "recovering" }
                } else if retrying {
                    "retrying"
                } else if outcome.is_some_and(|v| v.failed) {
                    "failed"
                } else {
                    "inactive"
                }
                .into(),
                active: previous_runner || retrying,
                pid: None,
                exit_code: outcome.and_then(|v| v.exit_code),
                started_at: generation.as_ref().map(|v| v.started_at),
                generation: generation.map(|v| v.generation),
            }
        };
        super::status_observed(directory, manifest, native, None)
    }

    fn check_generation(&self, directory: &Path, expected: Option<&str>) -> Result<()> {
        if let Some(expected) = expected {
            let actual = generation(directory)?;
            if actual.as_ref().map(|v| v.generation.as_str()) != Some(expected) {
                return Err(
                    "stale session generation; refresh status before controlling this session"
                        .into(),
                );
            }
        }
        Ok(())
    }

    async fn dispatch(&mut self, request: Request) -> Result<Value> {
        match request {
            Request::Ping => Ok(pong()),
            Request::List => {
                let mut values = Vec::new();
                for (directory, manifest) in self.saved()? {
                    self.reap(&manifest.spec.name).await?;
                    values.push(self.status(&directory, &manifest)?);
                }
                Ok(Value::Array(values))
            }
            Request::Status {
                name,
                expected_generation,
            } => {
                let (directory, manifest) = self.load(&name)?;
                self.check_generation(&directory, expected_generation.as_deref())?;
                self.reap(&name).await?;
                self.status(&directory, &manifest)
            }
            Request::Create { spec } => {
                super::validate_spec(&spec)?;
                crate::client::validate_session_spec(&spec)?;
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                let (directory, manifest) = super::prepare(&self.root, spec, true)?;
                if let Err(error) = self.start_worker(&directory, &manifest) {
                    // No process was published when spawn fails. Remove the
                    // newly committed definition only after confirming its lock.
                    let cleanup =
                        super::lock(&directory.join("run.lock"), true).and_then(|guard| {
                            drop(guard);
                            Ok(fs::remove_dir_all(&directory)?)
                        });
                    if let Err(cleanup) = cleanup {
                        let _ = super::write_json(
                            &directory.join("stop.requested"),
                            &json!({"requested_at":super::now()}),
                        );
                        return Err(format!(
                            "session creation failed ({error}); cleanup is unconfirmed ({cleanup})"
                        )
                        .into());
                    }
                    return Err(error);
                }
                self.status(&directory, &manifest)
            }
            Request::Start { name } => {
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                self.reap(&name).await?;
                let (directory, manifest) = self.load(&name)?;
                if self.workers.contains_key(&name) {
                    return Err("session is already running or stopping".into());
                }
                super::verify_snapshot(&directory, &manifest)?;
                super::remove_file(&directory.join("stop.requested"))?;
                self.start_worker(&directory, &manifest)?;
                self.status(&directory, &manifest)
            }
            Request::Recover {
                name,
                expected_definition_id,
            } => {
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                let (directory, manifest) = self.load(&name)?;
                super::check_recovery_definition(&directory, &expected_definition_id)?;
                self.reap(&name).await?;
                let mut status = self.status(&directory, &manifest)?;
                if status["manager_active"] == true {
                    status["recovery_state"] = json!("already_active");
                    return Ok(status);
                }
                super::verify_snapshot(&directory, &manifest)?;
                self.start_worker(&directory, &manifest)?;
                let mut status = self.status(&directory, &manifest)?;
                status["recovery_state"] = json!("start_requested");
                Ok(status)
            }
            Request::Restart {
                name,
                expected_generation,
            } => {
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                let (directory, manifest) = self.load(&name)?;
                self.check_generation(&directory, expected_generation.as_deref())?;
                self.reap(&name).await?;
                super::verify_snapshot(&directory, &manifest)?;
                crate::client::validate_session_spec(&manifest.spec)?;
                // Preserve desired-running intent if the service is stopped
                // between generations. Only an explicit close persists a stop.
                self.stop_worker(&name, &directory).await?;
                super::remove_file(&directory.join("stop.requested"))?;
                self.start_worker(&directory, &manifest)?;
                self.status(&directory, &manifest)
            }
            Request::Close {
                name,
                expected_generation,
            } => {
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                let (directory, manifest) = self.load(&name)?;
                self.check_generation(&directory, expected_generation.as_deref())?;
                super::write_json(
                    &directory.join("stop.requested"),
                    &json!({"requested_at":super::now()}),
                )?;
                super::clear_auth(&directory)?;
                self.stop_worker(&name, &directory).await?;
                super::write_management_state(&directory, "stopped", None)?;
                self.status(&directory, &manifest)
            }
            Request::Remove { name } => {
                let _guard = super::lock(&self.root.join("manage.lock"), true)?;
                let (directory, _) = self.load(&name)?;
                super::write_json(
                    &directory.join("stop.requested"),
                    &json!({"requested_at":super::now()}),
                )?;
                super::clear_auth(&directory)?;
                self.stop_worker(&name, &directory).await?;
                fs::remove_dir_all(directory)?;
                self.outcomes.remove(&name);
                Ok(
                    json!({"name":name,"manager_state":"removed","connection_state":"disconnected","removed":true}),
                )
            }
        }
    }

    async fn restore(&mut self) -> Result<()> {
        for (directory, manifest) in self.saved()? {
            tokio::task::yield_now().await;
            if !desired_running(&directory).unwrap_or(false) {
                continue;
            }
            self.recoveries
                .insert(manifest.spec.name.clone(), Instant::now() + STOP_TIMEOUT);
            let result = super::lock(&self.root.join("manage.lock"), true)
                .and_then(|_guard| self.restore_worker(&directory, &manifest));
            if let Err(error) = result {
                self.failed_start(&directory, &manifest, &error.to_string());
            }
        }
        Ok(())
    }

    fn restore_worker(&mut self, directory: &Path, manifest: &Manifest) -> Result<()> {
        let name = &manifest.spec.name;
        if self
            .recoveries
            .get(name)
            .is_some_and(|deadline| *deadline > Instant::now())
            && runner_active(directory)?
        {
            // A replaced owner has just invalidated its old epoch. Allow the
            // runner's <=250ms monitor and bounded graceful cleanup to finish,
            // including when the session's crash watchdog is disabled.
            self.retries.insert(name.clone(), Instant::now() + TICK);
            return Ok(());
        }
        self.start_worker(directory, manifest)
    }

    fn failed_start(&mut self, directory: &Path, manifest: &Manifest, error: &str) {
        self.recoveries.remove(&manifest.spec.name);
        self.outcomes.insert(
            manifest.spec.name.clone(),
            Outcome {
                failed: true,
                exit_code: None,
            },
        );
        let _ = super::write_management_state(directory, "disconnected", Some(error));
        if manifest.spec.watchdog && desired_running(directory).unwrap_or(false) {
            self.retries.insert(
                manifest.spec.name.clone(),
                Instant::now() + Duration::from_secs(manifest.spec.reconnect_delay_secs),
            );
        }
    }

    async fn tick(&mut self) {
        let finished: Vec<_> = self
            .workers
            .iter()
            .filter(|(_, worker)| worker.task.is_finished())
            .map(|(name, _)| name.clone())
            .collect();
        for name in finished {
            if let Err(error) = self.reap(&name).await {
                report("observe", &error.to_string());
            }
        }
        let due: Vec<_> = self
            .retries
            .iter()
            .filter(|(_, at)| **at <= Instant::now())
            .map(|(name, _)| name.clone())
            .collect();
        for name in due {
            self.retries.remove(&name);
            let (directory, manifest) = match self.load(&name) {
                Ok(value) => value,
                Err(error) => {
                    report("reload", &error.to_string());
                    continue;
                }
            };
            if !desired_running(&directory).unwrap_or(false) {
                continue;
            }
            let result = super::lock(&self.root.join("manage.lock"), true)
                .and_then(|_guard| self.restore_worker(&directory, &manifest));
            if let Err(error) = result {
                self.failed_start(&directory, &manifest, &error.to_string());
            }
        }
    }

    async fn shutdown(&mut self) -> Result<()> {
        self.retries.clear();
        self.recoveries.clear();
        for worker in self.workers.values_mut() {
            worker.stop();
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        let names: Vec<_> = self.workers.keys().cloned().collect();
        let mut unconfirmed = Vec::new();
        for name in names {
            let result = timeout_at(
                deadline,
                &mut self.workers.get_mut(&name).expect("worker exists").task,
            )
            .await;
            match result {
                Ok(result) => {
                    if !matches!(result, Ok(Ok(_))) {
                        unconfirmed.push(name.clone());
                    }
                    let worker = self.workers.remove(&name).expect("worker exists");
                    if let Err(error) = self.finished(&name, worker.stopping, result) {
                        report("shutdown state", &error.to_string());
                    }
                }
                Err(_) => unconfirmed.push(name),
            }
        }
        if !unconfirmed.is_empty() {
            return Err(format!(
                "service shutdown could not confirm {} owned session termination(s)",
                unconfirmed.len()
            )
            .into());
        }
        Ok(())
    }
}

fn desired_running(directory: &Path) -> Result<bool> {
    // A marker of any file type means stop. Filesystem errors fail closed.
    match fs::symlink_metadata(directory.join("stop.requested")) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

fn restart_after_exit(desired: bool, watchdog: bool, failed: bool, stopping: bool) -> bool {
    desired && watchdog && failed && !stopping
}

fn generation(directory: &Path) -> Result<Option<RunGeneration>> {
    match super::read_json::<RunGeneration>(&directory.join("generation.json")) {
        Ok(value)
            if value.generation.len() == 32
                && value.generation.bytes().all(|c| c.is_ascii_hexdigit()) =>
        {
            Ok(Some(value))
        }
        Ok(_) => Err("invalid saved session generation".into()),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn pong() -> Value {
    json!({"service":"ssmcp","protocol":1})
}

fn report(operation: &str, error: &str) {
    eprintln!(
        "ssmcp: service {}",
        crate::audit::event(json!({"operation":operation,"message":error}))
    );
}

async fn handle(mut peer: ipc::Peer, queue: mpsc::Sender<Pending>) -> Result<()> {
    let value = peer.read().await?;
    let request = match serde_json::from_value::<Request>(value) {
        Ok(request) => request,
        Err(error) => {
            return peer
                .reply(Err(format!("invalid service request: {error}")))
                .await;
        }
    };
    if matches!(request, Request::Ping) {
        return peer.reply(Ok(pong())).await;
    }
    let (reply, response) = oneshot::channel();
    let deadline = Instant::now() + OPERATION_TIMEOUT;
    if queue
        .try_send(Pending {
            request,
            reply,
            deadline,
        })
        .is_err()
    {
        return peer
            .reply(Err(
                "service operation queue is busy; retry after inspecting status".into(),
            ))
            .await;
    }
    let result = match timeout_at(deadline, response).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("service is shutting down".into()),
        Err(_) => {
            Err("service operation timed out; inspect status before repeating a mutation".into())
        }
    };
    peer.reply(result).await
}

async fn shutdown_signal(stop: Arc<AtomicBool>) {
    let external = async {
        let mut tick = tokio::time::interval(TICK);
        loop {
            if stop.load(Ordering::Acquire) {
                return;
            }
            tick.tick().await;
        }
    };
    #[cfg(unix)]
    let native = async {
        use tokio::signal::unix::{SignalKind, signal};
        let term = signal(SignalKind::terminate());
        let interrupt = signal(SignalKind::interrupt());
        if let (Ok(mut term), Ok(mut interrupt)) = (term, interrupt) {
            tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} }
        } else {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(windows)]
    let native = async {
        let interrupted = async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let broken = async {
            if let Ok(mut signal) = tokio::signal::windows::ctrl_break() {
                signal.recv().await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! { _ = interrupted => {}, _ = broken => {} }
    };
    #[cfg(not(any(unix, windows)))]
    let native = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    tokio::select! { _ = external => {}, _ = native => {} }
}

pub async fn run() -> Result<()> {
    run_with_shutdown(Arc::new(AtomicBool::new(false))).await
}

/// SCM and native service hosts may request shutdown without a console signal.
pub async fn run_with_shutdown(stop: Arc<AtomicBool>) -> Result<()> {
    #[cfg(unix)]
    if unsafe { libc::geteuid() } == 0 {
        return Err("ssmcp service must run as its non-root session owner; configure the service User first".into());
    }
    let root = super::session_root(true)?;
    let _guard = super::lock(&root.join("service.lock"), true)?;
    let epoch = super::random_suffix()?;
    super::write_json(
        &root.join("service-epoch.json"),
        &ServiceEpoch {
            epoch: epoch.clone(),
        },
    )?;
    let server = ipc::Server::bind(&root, ENDPOINT).await?;
    let mut supervisor = Supervisor::new(root, epoch);
    let shutdown = shutdown_signal(stop);
    tokio::pin!(shutdown);
    tokio::select! {
        _ = &mut shutdown => { return supervisor.shutdown().await; },
        result = supervisor.restore() => result?,
    }
    let (queue, mut incoming) = mpsc::channel::<Pending>(CAPACITY);
    let mut handlers = JoinSet::new();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            peer = server.accept(), if handlers.len() < CAPACITY => {
                match peer { Ok(peer) => { handlers.spawn(handle(peer, queue.clone())); }, Err(error) => report("accept", &error.to_string()) }
            }
            _ = handlers.join_next(), if !handlers.is_empty() => {}
            pending = incoming.recv() => {
                if let Some(pending) = pending {
                    if pending.reply.is_closed() || pending.deadline <= Instant::now() { continue; }
                    let result = tokio::select! {
                        _ = &mut shutdown => { let _ = pending.reply.send(Err("service is shutting down".into())); break; }
                        result = timeout_at(pending.deadline, supervisor.dispatch(pending.request)) => match result {
                            Ok(result) => result.map_err(|error| error.to_string()),
                            Err(_) => Err("service operation timed out; inspect status before repeating a mutation".into()),
                        },
                    };
                    let _ = pending.reply.send(result);
                }
            }
            _ = tick.tick() => {
                tokio::select! { _ = &mut shutdown => break, _ = supervisor.tick() => {} }
            }
        }
    }
    drop(server);
    incoming.close();
    while let Ok(pending) = incoming.try_recv() {
        let _ = pending.reply.send(Err("service is shutting down".into()));
    }
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    supervisor.shutdown().await
}

pub async fn request(request: Value) -> Result<Value> {
    let root = super::session_root(false)?;
    ipc::request(&root, ENDPOINT, request)
        .await
        .map_err(|error| format!("ssmcp service: {error}").into())
}

pub async fn available() -> bool {
    let Ok(root) = super::session_root(false) else {
        return false;
    };
    ipc::request_with_timeout(
        &root,
        ENDPOINT,
        json!({"op":"ping"}),
        Duration::from_millis(750),
    )
    .await
    .is_ok_and(|value| value == pong())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_rejects_foreground_chat_modes_before_saving_or_spawning() {
        let directory = Directory::new();
        let mut supervisor = Supervisor::new(directory.0.clone(), "a".repeat(32));
        let mut cases = vec![vec!["--input", "terminal", "--agent", "codex"]];
        if cfg!(unix) {
            cases.extend([vec!["--input", "pipe"], vec!["--input", "mcp-stdio"]]);
        }
        for flags in cases {
            let name = "foreground-chat";
            let args = [
                "--server",
                "https://relay.example",
                "--name",
                name,
                "--no-watchdog",
            ]
            .into_iter()
            .chain(flags)
            .map(str::to_owned)
            .collect();
            let result = supervisor
                .dispatch(Request::Create {
                    spec: SessionSpec {
                        name: name.into(),
                        cwd: directory.0.clone(),
                        args,
                        watchdog: false,
                        reconnect_delay_secs: 5,
                    },
                })
                .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("require the foreground")
            );
            assert!(!super::super::session_directory(&directory.0, name).exists());
        }
    }

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ssmcp-supervisor-test-{}",
                super::super::random_suffix().unwrap()
            ));
            crate::platform::private_directory(&path).unwrap();
            Self(path)
        }

        fn saved(&self, name: &str, supervised: bool) -> PathBuf {
            let directory = super::super::session_directory(&self.0, name);
            crate::platform::private_directory(&directory).unwrap();
            let manifest = Manifest {
                format: super::super::FORMAT,
                supervised,
                spec: SessionSpec {
                    name: name.into(),
                    cwd: self.0.clone(),
                    args: vec!["ssmcp@example".into()],
                    watchdog: false,
                    reconnect_delay_secs: 1,
                },
                executable_sha256: "fixture".into(),
                version: "fixture".into(),
                manager_id: super::super::manager_id(&self.0, name),
                created_at: 0,
                environment: BTreeMap::from([(
                    "SSMCP_SESSION_HOME".into(),
                    self.0.to_str().unwrap().into(),
                )]),
            };
            super::super::write_json(&directory.join("manifest.json"), &manifest).unwrap();
            directory
        }

        #[cfg(unix)]
        fn worker(&self, supervisor: &mut Supervisor, name: &str) -> (PathBuf, String) {
            let directory = self.saved(name, true);
            let active = RunGeneration {
                generation: super::super::random_suffix().unwrap(),
                started_at: super::super::now(),
                supervisor_pid: std::process::id(),
                service_epoch: supervisor.epoch.clone(),
            };
            super::super::write_json(&directory.join("generation.json"), &active).unwrap();
            let generation = active.generation.clone();
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", "sleep 60 & echo \"$!\" > \"$1\"; wait", "worker"])
                .arg(directory.join("descendant.pid"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            supervisor
                .workers
                .insert(name.into(), Worker::spawn(&mut command, active).unwrap());
            (directory, generation)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn owner_lease_requires_both_matching_epoch_and_held_lock() {
        let root = Directory::new();
        let epoch = "a".repeat(32);
        let guard = super::super::lock(&root.0.join("service.lock"), true).unwrap();
        super::super::write_json(
            &root.0.join("service-epoch.json"),
            &ServiceEpoch {
                epoch: epoch.clone(),
            },
        )
        .unwrap();
        assert!(owner_active(&root.0, &epoch).unwrap());
        super::super::write_json(
            &root.0.join("service-epoch.json"),
            &ServiceEpoch {
                epoch: "b".repeat(32),
            },
        )
        .unwrap();
        assert!(
            !owner_active(&root.0, &epoch).unwrap(),
            "a replacement service cannot inherit the old lease"
        );
        super::super::write_json(
            &root.0.join("service-epoch.json"),
            &ServiceEpoch {
                epoch: epoch.clone(),
            },
        )
        .unwrap();
        drop(guard);
        assert!(
            !owner_active(&root.0, &epoch).unwrap(),
            "a stale epoch file cannot keep an orphan running"
        );
    }

    #[tokio::test]
    async fn restore_waits_for_previous_runner_even_without_watchdog() {
        let root = Directory::new();
        let directory = root.saved("recover", true);
        let guard = super::super::lock(&directory.join("run.lock"), true).unwrap();
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        supervisor.restore().await.unwrap();
        assert!(supervisor.retries.contains_key("recover"));
        assert!(supervisor.recoveries.contains_key("recover"));
        assert!(
            supervisor.outcomes.is_empty(),
            "old owner cleanup is not a failed start"
        );
        drop(guard);
        // Another test may fork while the lease is open. Its close-on-exec
        // descriptor can briefly keep the old owner's flock alive.
        for _ in 0..100 {
            supervisor.retries.insert("recover".into(), Instant::now());
            supervisor.tick().await;
            if supervisor.outcomes.contains_key("recover") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The fixture deliberately lacks its executable. Seeing this failure
        // proves restore attempted launch only after the prior lease released.
        assert!(supervisor.outcomes["recover"].failed);
        assert!(supervisor.recoveries.is_empty());
        assert!(
            supervisor.retries.is_empty(),
            "watchdog=false still prevents subsequent crash loops"
        );
    }

    #[tokio::test]
    async fn previous_runner_lock_prevents_a_false_stopped_status() {
        let root = Directory::new();
        let directory = root.saved("previous", true);
        let guard = super::super::lock(&directory.join("run.lock"), true).unwrap();
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        assert!(
            supervisor
                .dispatch(Request::Close {
                    name: "previous".into(),
                    expected_generation: None
                })
                .await
                .is_err()
        );
        let (_, manifest) = supervisor.load("previous").unwrap();
        let pending = supervisor.status(&directory, &manifest).unwrap();
        assert_eq!(pending["manager_state"], "stopping");
        assert_eq!(pending["manager_active"], true);
        assert_eq!(pending["auth_available"], false);
        drop(guard);
        let stopped = supervisor.status(&directory, &manifest).unwrap();
        assert_eq!(stopped["manager_state"], "inactive");
        assert_eq!(stopped["manager_active"], false);
    }

    #[test]
    fn request_shape_rejects_ambiguous_or_unbounded_controls() {
        assert!(
            serde_json::from_value::<Request>(
                json!({"op":"close","name":"a","expected_generation":"old"})
            )
            .is_ok()
        );
        for value in [
            json!({"op":"close"}),
            json!({"op":"start","name":"a","pid":123}),
            json!({"op":"remove","name":"a","force":true}),
            json!({"op":"exec","command":"sh"}),
        ] {
            assert!(serde_json::from_value::<Request>(value).is_err());
        }
    }

    #[test]
    fn legacy_manifest_and_generation_bound_connection_observation() {
        let root = Directory::new();
        let directory = root.saved("observed", true);
        let (_, manifest) = super::super::load(&root.0, "observed").unwrap();
        let mut legacy = serde_json::to_value(&manifest).unwrap();
        legacy.as_object_mut().unwrap().remove("supervised");
        assert!(
            !serde_json::from_value::<Manifest>(legacy)
                .unwrap()
                .supervised
        );
        super::super::write_json(
            &directory.join("state.json"),
            &super::super::Observation {
                phase: "connected".into(),
                pid: 111,
                observed_at: 10,
                generation: Some("a".repeat(32)),
                connection: Some(json!({"client_id":"observed"})),
                ..Default::default()
            },
        )
        .unwrap();
        let native = |generation: &str| NativeStatus {
            state: "running".into(),
            active: true,
            pid: Some(222),
            started_at: Some(9),
            generation: Some(generation.repeat(32)),
            ..Default::default()
        };
        let current =
            super::super::status_observed(&directory, &manifest, native("a"), None).unwrap();
        assert_eq!(
            current["connection_state"], "connected",
            "guardian and runner may have distinct PIDs"
        );
        let stale =
            super::super::status_observed(&directory, &manifest, native("b"), None).unwrap();
        assert_eq!(stale["connection_state"], "unknown");
        assert!(
            stale["connection"].is_null(),
            "old connection cannot survive a generation change"
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_exit_retries_only_while_watchdog_and_running_intent_remain() {
        use std::os::unix::process::ExitStatusExt;
        let root = Directory::new();
        let directory = root.saved("retry", true);
        let (_, mut manifest) = super::super::load(&root.0, "retry").unwrap();
        manifest.spec.watchdog = true;
        super::super::write_json(&directory.join("manifest.json"), &manifest).unwrap();
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        supervisor
            .finished("retry", false, Ok(Ok(ExitStatus::from_raw(17 << 8))))
            .unwrap();
        assert!(supervisor.retries["retry"] > Instant::now());
        supervisor.retries.clear();
        supervisor
            .finished("retry", false, Ok(Ok(ExitStatus::from_raw(0))))
            .unwrap();
        assert!(
            supervisor.retries.is_empty(),
            "successful exit must not cause a crash loop"
        );
        supervisor
            .finished("retry", true, Ok(Ok(ExitStatus::from_raw(17 << 8))))
            .unwrap();
        assert!(
            supervisor.retries.is_empty(),
            "requested teardown must not race a restart"
        );
        super::super::write_json(
            &directory.join("stop.requested"),
            &json!({"requested_at":1}),
        )
        .unwrap();
        supervisor
            .finished("retry", false, Ok(Ok(ExitStatus::from_raw(17 << 8))))
            .unwrap();
        assert!(
            supervisor.retries.is_empty(),
            "closed intent must survive a late failed exit"
        );
    }

    #[tokio::test]
    async fn restore_skips_native_and_deliberately_closed_definitions() {
        let root = Directory::new();
        root.saved("legacy", false);
        let closed = root.saved("closed", true);
        super::super::write_json(&closed.join("stop.requested"), &json!({"requested_at":1}))
            .unwrap();
        root.saved("wanted", true);
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        // All fixtures lack executable snapshots. Only the wanted supervised
        // definition may even attempt a start, so only it records a failure.
        supervisor.restore().await.unwrap();
        assert!(supervisor.workers.is_empty());
        assert_eq!(
            supervisor
                .outcomes
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["wanted"]
        );
        assert!(!closed.join("state.json").exists());
        assert!(!root.0.join("service.json").exists());
    }

    #[test]
    fn closed_intent_and_generation_survive_owner_restart() {
        let root = Directory::new();
        let directory = &root.0;
        let supervisor = Supervisor::new(directory.clone(), "a".repeat(32));
        assert!(desired_running(directory).unwrap());
        assert!(
            supervisor
                .check_generation(directory, Some("missing"))
                .is_err()
        );
        let first = "a".repeat(32);
        super::super::write_json(
            &directory.join("generation.json"),
            &RunGeneration {
                generation: first.clone(),
                started_at: 1,
                supervisor_pid: 123,
                service_epoch: "a".repeat(32),
            },
        )
        .unwrap();
        assert!(supervisor.check_generation(directory, Some(&first)).is_ok());
        super::super::write_json(
            &directory.join("stop.requested"),
            &json!({"requested_at":2}),
        )
        .unwrap();
        drop(supervisor);
        let next = Supervisor::new(directory.clone(), "a".repeat(32));
        assert!(!desired_running(directory).unwrap());
        super::super::write_json(
            &directory.join("generation.json"),
            &RunGeneration {
                generation: "b".repeat(32),
                started_at: 3,
                supervisor_pid: 456,
                service_epoch: "a".repeat(32),
            },
        )
        .unwrap();
        assert!(next.check_generation(directory, Some(&first)).is_err());
        assert!(!desired_running(directory).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_close_is_rejected_before_mutation_and_close_waits_for_owned_tree() {
        if crate::test_process::isolated("session::service::tests::stale_close_is_rejected_before_mutation_and_close_waits_for_owned_tree").await { return; }
        let root = Directory::new();
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        let (directory, generation) = root.worker(&mut supervisor, "owned");
        let mut unrelated = Command::new("/bin/sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let descendant: i32 = loop {
            if let Ok(text) = fs::read_to_string(directory.join("descendant.pid"))
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "worker did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let stale = supervisor
            .dispatch(Request::Close {
                name: "owned".into(),
                expected_generation: Some("stale".into()),
            })
            .await
            .unwrap_err();
        assert!(
            stale.to_string().contains("stale session generation"),
            "{stale}"
        );
        assert!(desired_running(&directory).unwrap());
        assert!(!supervisor.workers["owned"].stopping);
        let result = supervisor
            .dispatch(Request::Close {
                name: "owned".into(),
                expected_generation: Some(generation),
            })
            .await
            .unwrap();
        assert_eq!(result["manager_state"], "inactive");
        assert_eq!(result["stop_requested"], true);
        assert_eq!(result["connection_state"], "disconnected");
        assert!(!supervisor.workers.contains_key("owned"));
        assert!(!desired_running(&directory).unwrap());
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "unrelated process was signalled"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            unsafe { libc::kill(descendant, 0) },
            -1,
            "owned descendant survived confirmed close"
        );
        #[cfg(not(target_os = "linux"))]
        let _ = descendant;
        unrelated.kill().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn service_shutdown_preserves_running_intent_and_never_schedules_watchdog() {
        let root = Directory::new();
        let mut supervisor = Supervisor::new(root.0.clone(), "a".repeat(32));
        let (directory, _) = root.worker(&mut supervisor, "restore-me");
        supervisor.shutdown().await.unwrap();
        assert!(desired_running(&directory).unwrap());
        assert!(supervisor.workers.is_empty());
        assert!(supervisor.retries.is_empty());
        let observed: Value = super::super::read_json(&directory.join("state.json")).unwrap();
        assert_eq!(observed["phase"], "disconnected");
    }
}
