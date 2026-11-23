//! Independent command jobs with bounded live output retained only in memory.
#[cfg(all(test, target_os = "linux"))]
mod artifact_tests;
#[cfg(target_os = "linux")]
mod artifacts;
#[cfg(all(test, target_os = "linux"))]
mod pty_tests;

use super::{events::EventLog, process::Managed};
use crate::output::{Capture, OutputStore};
use crate::runtime::time::ExecutionTiming;
use crate::{pty::Terminal, wire::Operation};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{ChildStdin, Command},
    sync::{Mutex as AsyncMutex, Notify, Semaphore, watch},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};

const DEFAULT_ARTIFACT_BUDGET_BYTES: u64 = 20_000_000_000;
const MAX_JOBS: usize = 32;
const MAX_RETAINED: usize = 128;
const MAX_OUTPUT: usize = 64 * 1024;
const COMPLETED_INLINE_BYTES: usize = 4 * 1024;
const MAX_COMMAND_ARGS: usize = 256;
const MAX_TITLE_CHARS: usize = 160;
const MAX_READ_WAIT: u64 = crate::wire::MAX_OBSERVATION_WAIT_MS;
const MAX_INPUT: usize = 32 * 1024;
const MAX_INPUT_WAIT: u64 = 5_000;
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);
const PID_ROLE: &str = if cfg!(target_os = "linux") {
    "supervisor"
} else {
    "command"
};

struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    eof: bool,
    read_from_capture: bool,
    capture: Capture,
}
impl Output {
    fn new(store: &Arc<OutputStore>, scope: String) -> Self {
        Self {
            bytes: VecDeque::new(),
            start: 0,
            end: 0,
            eof: false,
            read_from_capture: false,
            capture: store.capture(scope),
        }
    }
    fn append(&mut self, data: &[u8]) {
        let _ = self.capture.push(data);
        let retained = &data[data.len().saturating_sub(MAX_OUTPUT)..];
        let excess = (self.bytes.len() + retained.len()).saturating_sub(MAX_OUTPUT);
        self.bytes.drain(..excess);
        self.bytes.extend(retained);
        self.end = self.end.saturating_add(data.len() as u64);
        self.start = self.end.saturating_sub(self.bytes.len() as u64);
    }
    fn metadata(&self) -> Value {
        json!({"start_cursor":self.start,"next_cursor":self.end,"truncated":self.start>0,
            "eof":self.eof,"artifact":self.capture.metadata(),"error":self.capture.error()})
    }
    fn read(&self, cursor: u64, limit: usize) -> Result<(Vec<u8>, u64, bool), String> {
        if cursor > self.end {
            return Err("output cursor is ahead of the stream".into());
        }
        if self.read_from_capture {
            let data = self.capture.read_range(cursor, limit)?;
            let next = cursor + data.len() as u64;
            return Ok((data, next, false));
        }
        let next = cursor.max(self.start);
        let offset = (next - self.start) as usize;
        let data: Vec<u8> = self
            .bytes
            .iter()
            .skip(offset)
            .take(limit)
            .copied()
            .collect();
        let next = next + data.len() as u64;
        Ok((data, next, cursor < self.start))
    }
    fn finish(&mut self, error: Option<String>) {
        if let Some(error) = error {
            self.capture.fail(error);
        }
        // Successful completed jobs can read their full capture directly. Keep
        // tiny output inline; larger output uses the shared bounded memory store.
        // A failed capture keeps the live tail and its explicit error metadata.
        if self.capture.error().is_none() && self.capture.bytes() > COMPLETED_INLINE_BYTES as u64 {
            let _ = self.capture.spill();
        }
        let archived = self.capture.finish().is_ok() && self.capture.bytes() == self.end;
        if archived {
            self.read_from_capture = true;
            self.start = 0;
            self.bytes = VecDeque::new();
        }
        self.eof = true;
    }
}
#[derive(Clone, Copy, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum InputMode {
    #[default]
    Null,
    Pipe,
}
impl InputMode {
    fn label(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Pipe => "pipe",
        }
    }
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Pipes,
    Pty,
}

struct PtyStart {
    id: String,
    command: Vec<String>,
    title: Option<String>,
    cwd: PathBuf,
    env: HashMap<String, String>,
    timeout_ms: u64,
    size: Option<(u16, u16)>,
    live: bool,
    compatibility_limit: bool,
}

struct State {
    running: bool,
    #[cfg(target_os = "linux")]
    tree_reaped: bool,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    cancelled: bool,
    timed_out: bool,
    error: Option<String>,
    finished: Option<Instant>,
    stdin_open: bool,
    stdin_written: u64,
    stdout: Output,
    stderr: Output,
}
struct Job {
    /// Bound once after successful native spawn; observers cannot retag it.
    creator: crate::runtime::call_scope::Source,
    id: String,
    pid: u32,
    command: Vec<String>,
    title: Option<String>,
    cwd: PathBuf,
    timing: ExecutionTiming,
    state: Mutex<State>,
    changed: Arc<Notify>,
    cancel: watch::Sender<bool>,
    input_mode: InputMode,
    stdin: AsyncMutex<Option<ChildStdin>>,
    input_stopped: watch::Sender<bool>,
    observation: Mutex<Option<crate::observer::Lease>>,
    output_mode: &'static str,
    tty_slot: Option<usize>,
    terminal: Option<Arc<Terminal>>,
    #[cfg(target_os = "linux")]
    artifacts: Option<Mutex<artifacts::Artifacts>>,
    #[cfg(target_os = "linux")]
    temporary_artifacts: bool,
}
impl Job {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn snapshot_locked(&self, state: &State) -> Value {
        let through = state.finished.unwrap_or_else(Instant::now).into_std();
        json!({"job_id":self.id,"id":self.id,"pid":self.pid,"pid_role":if self.terminal.is_some() {"command"} else {PID_ROLE},"command":self.command,"title":self.title,"cwd":self.cwd,
            "state":if state.running {"running"} else if state.cancelled {"cancelled"} else if state.timed_out {"timed_out"} else if state.error.is_some() {"failed"} else {"completed"},
            "running":state.running,"exit_code":state.exit_code,"exit_signal":state.exit_signal,"cancelled":state.cancelled,
            "timed_out":state.timed_out,"error":state.error,
            "duration_ms":self.timing.elapsed_ms(through),
            "timeout_ms":self.timing.timeout_ms(),
            "remaining_ms":if state.running {self.timing.remaining_ms(through)} else {None},
            "deadline_clock":"monotonic",
            "mode":if self.terminal.is_some() {"pty"} else {"pipes"},
            "output_mode":self.output_mode,"tty_slot":self.tty_slot,
            "stdin":self.input_snapshot(state),
            "stdout":self.terminal.as_ref().map_or_else(|| state.stdout.metadata(), |terminal| terminal.output_snapshot()),
            "stderr":state.stderr.metadata()})
    }
    fn snapshot(&self) -> Value {
        self.snapshot_locked(&self.state())
    }
    fn add_artifact_result(&self, result: &mut Value, caller: &crate::runtime::call_scope::Source) {
        #[cfg(target_os = "linux")]
        if let Some(artifacts) = &self.artifacts
            && same_artifact_owner(&self.creator, caller)
        {
            result["artifacts"] = artifacts.lock().unwrap_or_else(|e| e.into_inner()).sample();
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (result, caller);
    }

    async fn result_snapshot(
        self: &Arc<Self>,
        caller: crate::runtime::call_scope::Source,
    ) -> Result<Value, String> {
        #[cfg(target_os = "linux")]
        if self.artifacts.is_some() && same_artifact_owner(&self.creator, &caller) {
            let job = self.clone();
            return tokio::task::spawn_blocking(move || {
                let mut result = job.snapshot();
                job.add_artifact_result(&mut result, &caller);
                result
            })
            .await
            .map_err(|err| format!("could not sample task artifacts: {err}"));
        }
        #[cfg(not(target_os = "linux"))]
        let _ = caller;
        Ok(self.snapshot())
    }

    fn input_snapshot(&self, state: &State) -> Value {
        json!({"mode":if self.terminal.is_some() {"pty"} else {self.input_mode.label()},
            "open":state.stdin_open,
            "bytes_written":self.terminal.as_ref().map_or(state.stdin_written, |terminal| terminal.input_bytes())})
    }
    fn stop_input(&self) {
        self.state().stdin_open = false;
        self.input_stopped.send_replace(true);
        self.changed.notify_waiters();
    }
    fn write_result(
        &self,
        written: usize,
        requested: usize,
        timed_out: bool,
        error: Option<String>,
    ) -> Value {
        let state = self.state();
        json!({"job_id":self.id,"written":written,"requested_bytes":requested,
            "remaining_bytes":requested-written,"timed_out":timed_out,"error":error,
            "closed":!state.stdin_open,"stdin":self.input_snapshot(&state)})
    }
}
#[cfg(target_os = "linux")]
fn same_artifact_owner(
    owner: &crate::runtime::call_scope::Source,
    caller: &crate::runtime::call_scope::Source,
) -> bool {
    let (Some(owner), Some(caller)) = (&owner.call, &caller.call) else {
        return false;
    };
    owner.thread().is_some()
        && owner.thread() == caller.thread()
        && owner.connection() == caller.connection()
        && matches!((owner.attribution(), caller.attribution()), (Some(a), Some(b))
            if a.account.subject == b.account.subject && a.account.provider == b.account.provider)
}

#[derive(Default)]
struct Registry {
    jobs: HashMap<String, Arc<Job>>,
    selected: String,
    closed: bool,
    #[cfg(target_os = "linux")]
    retired_temporary: VecDeque<Arc<Job>>,
}

impl Registry {
    fn forget(&mut self, id: &str) -> bool {
        let Some(_job) = self.jobs.remove(id) else {
            return false;
        };
        if _job.terminal.is_some() {
            if self.selected == id {
                self.selected = "main".into();
                crate::client::terminal::select_session("main");
            }
            crate::client::terminal::close_session(id);
        }
        #[cfg(target_os = "linux")]
        if _job.temporary_artifacts {
            if self.retired_temporary.len() == MAX_RETAINED
                && let Some(old) = self.retired_temporary.pop_front()
            {
                eprintln!(
                    "ssmcp: preserving temporary artifacts for job {} (retired queue full)",
                    old.id
                );
            }
            self.retired_temporary.push_back(_job);
        }
        true
    }
}

pub struct Shell {
    root: PathBuf,
    output: Arc<OutputStore>,
    namespace: String,
    registry: Mutex<Registry>,
    slots: Arc<Semaphore>,
    events: Arc<EventLog>,
    observer: Option<Arc<crate::observer::Pool>>,
    interactive_shell: Option<PathBuf>,
}
impl Shell {
    pub fn new() -> Arc<Self> {
        Self::with_workspace(
            std::env::current_dir().expect("shell startup directory is unavailable"),
            OutputStore::new_connection().expect("shell output store is unavailable"),
        )
        .expect("shell workspace is unavailable")
    }

    pub fn with_workspace(root: PathBuf, output: Arc<OutputStore>) -> Result<Arc<Self>, String> {
        let root = root
            .canonicalize()
            .map_err(|error| format!("invalid execution workspace: {error}"))?;
        if !root.is_dir() {
            return Err("execution workspace must be a directory".into());
        }
        let mut nonce = [0u8; 16];
        crate::platform::random_bytes(&mut nonce).map_err(|error| error.to_string())?;
        let namespace = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let events = EventLog::with_output("shell", output.clone());
        Ok(Arc::new(Self {
            root,
            observer: output.observer(),
            interactive_shell: None,
            output,
            namespace,
            registry: Mutex::new(Registry::default()),
            slots: Arc::new(Semaphore::new(MAX_JOBS)),
            events,
        }))
    }
    pub fn with_terminal(
        shell: &Path,
        root: PathBuf,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        if !shell.is_absolute() {
            return Err("shell must be an absolute path".into());
        }
        let mut manager = Self::with_workspace(root, output)?;
        Arc::get_mut(&mut manager).unwrap().interactive_shell = Some(shell.to_path_buf());
        let job = manager.spawn_pty(
            PtyStart {
                id: "main".into(),
                command: interactive_command(shell),
                title: Some("main".into()),
                cwd: manager.root.clone(),
                env: HashMap::new(),
                timeout_ms: 0,
                size: None,
                live: true,
                compatibility_limit: false,
            },
            crate::runtime::call_scope::Source::capture(),
        )?;
        manager.registry().selected = job.id.clone();
        Ok(manager)
    }

    fn next_job_id(&self) -> String {
        format!(
            "{}-{:x}",
            self.namespace,
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn spawn_pty(
        &self,
        args: PtyStart,
        creator: crate::runtime::call_scope::Source,
    ) -> Result<Arc<Job>, String> {
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            format!("at most {MAX_JOBS} commands may run at once; wait or cancel an existing job")
        })?;
        let started = std::time::Instant::now();
        let deadline = (args.timeout_ms != 0)
            .then(|| {
                started
                    .checked_add(Duration::from_millis(args.timeout_ms))
                    .ok_or("timeout_ms cannot be represented by the execution clock")
            })
            .transpose()?;
        let timing = ExecutionTiming { started, deadline };
        let mut registry = self.registry();
        if registry.closed {
            return Err("shell command manager is closed".into());
        }
        if args.compatibility_limit
            && registry
                .jobs
                .values()
                .filter(|job| job.terminal.is_some())
                .count()
                >= crate::terminals::MAX_TERMINALS
        {
            return Err(format!(
                "at most {} terminal jobs may be retained; close an existing terminal",
                crate::terminals::MAX_TERMINALS
            ));
        }
        if registry.jobs.contains_key(&args.id) {
            return Err("job ID already exists".into());
        }
        let evicted = if registry.jobs.len() >= MAX_RETAINED {
            registry
                .jobs
                .values()
                .filter(|job| job.id != "main" && !job.state().running)
                .min_by_key(|job| job.timing.started)
                .map(|job| job.id.clone())
        } else {
            None
        };
        let events = self.events.clone();
        let source = creator.clone();
        let event_id = args.id.clone();
        let output_event: crate::pty::OutputEvent = Arc::new(move |offset, bytes| {
            events.push_scoped(json!({"type":"exec.stdout","job_id":event_id,
                "method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode(bytes),
                    "stream":"stdout","processId":event_id,"offset":offset,"next_offset":offset+bytes.len() as u64}}),
                source.clone());
        });
        crate::client::terminal::register_session(
            &args.id,
            args.title.as_deref().unwrap_or(&args.id),
        );
        let terminal = match Terminal::spawn_job(
            crate::pty::JobConfig {
                command: &args.command,
                cwd: &args.cwd,
                env: &args.env,
                id: &args.id,
                size: args.size,
                live: args.live,
                output_event: Some(output_event),
            },
            self.output.clone(),
        ) {
            Ok(terminal) => terminal,
            Err(error) => {
                crate::client::terminal::close_session(&args.id);
                return Err(format!("could not start PTY command: {error}"));
            }
        };
        let (cancel, mut cancel_rx) = watch::channel(false);
        let (input_stopped, _) = watch::channel(false);
        let mut stderr = Output::new(&self.output, format!("job:{}:stderr", args.id));
        stderr.finish(None);
        let job = Arc::new(Job {
            creator,
            id: args.id.clone(),
            pid: terminal.pid(),
            command: args.command,
            title: args.title,
            cwd: args.cwd,
            timing,
            state: Mutex::new(State {
                running: true,
                #[cfg(target_os = "linux")]
                tree_reaped: false,
                exit_code: None,
                exit_signal: None,
                cancelled: false,
                timed_out: false,
                error: None,
                finished: None,
                stdin_open: true,
                stdin_written: 0,
                stdout: Output::new(&self.output, format!("job:{}:unused", args.id)),
                stderr,
            }),
            changed: terminal.changed(),
            cancel,
            input_mode: InputMode::Null,
            stdin: AsyncMutex::new(None),
            input_stopped,
            observation: Mutex::new(None),
            output_mode: "pty",
            tty_slot: None,
            terminal: Some(terminal.clone()),
            #[cfg(target_os = "linux")]
            artifacts: None,
            #[cfg(target_os = "linux")]
            temporary_artifacts: false,
        });
        if let Some(id) = evicted {
            registry.forget(&id);
        }
        registry.jobs.insert(job.id.clone(), job.clone());
        drop(registry);
        let worker_command =
            crate::worker::native_command_started(&job.creator, &job.id, &job.command);
        let mut started = job.snapshot();
        started["type"] = json!("exec.started");
        self.events
            .push_execution(started, job.creator.clone(), timing);
        let events = self.events.clone();
        let worker = job.clone();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                _ = cancel_rx.changed() => (None, true, false, None, None),
                result = async {
                    if let Some(deadline) = timing.deadline {
                        timeout_at(deadline.into(), terminal.wait_exit()).await
                    } else { Ok(terminal.wait_exit().await) }
                } => match result {
                    Ok(Ok((code, signal))) => (code, false, false, None, signal),
                    Ok(Err(error)) => (None, false, false, Some(error.to_string()), None),
                    Err(_) => (None, false, true, None, None),
                }
            };
            worker.stop_input();
            terminal.shutdown().await;
            {
                let mut state = worker.state();
                drop(permit);
                state.running = false;
                state.exit_code = outcome.0;
                state.cancelled = outcome.1;
                state.timed_out = outcome.2;
                state.error = outcome.3;
                state.exit_signal = outcome.4;
                state.finished = Some(Instant::now());
            }
            let status = if outcome.1 {
                "cancelled".to_owned()
            } else if outcome.2 {
                "timed out".to_owned()
            } else if let Some(error) = &worker.state().error {
                format!("failed: {error}")
            } else if let Some(code) = outcome.0 {
                format!("exit {code}")
            } else if let Some(signal) = outcome.4 {
                format!("signal {signal}")
            } else {
                "finished without a reported exit status".into()
            };
            if let Some(command) = worker_command {
                command.finish(&status);
            }
            let mut snapshot = worker.snapshot();
            snapshot["type"] = json!("exec.exited");
            events.push_scoped(snapshot, worker.creator.clone());
            worker.changed.notify_waiters();
        });
        Ok(job)
    }

    pub fn get(&self, id: &str) -> Result<Arc<Terminal>, String> {
        self.job(id)?
            .terminal
            .clone()
            .ok_or_else(|| "job is not a PTY".into())
    }

    pub fn primary(&self) -> Arc<Terminal> {
        self.get("main")
            .expect("main terminal exists until shutdown")
    }

    pub fn selected(&self) -> Arc<Terminal> {
        let registry = self.registry();
        registry
            .jobs
            .get(&registry.selected)
            .and_then(|job| job.terminal.clone())
            .expect("selected terminal exists")
    }

    pub fn cycle(&self) -> Result<(), String> {
        let mut registry = self.registry();
        if registry.closed {
            return Err("shell command manager is closed".into());
        }
        let mut ids: Vec<_> = registry
            .jobs
            .values()
            .filter(|job| job.terminal.is_some())
            .map(|job| job.id.clone())
            .collect();
        ids.sort();
        if ids.is_empty() {
            return Err("no terminal jobs".into());
        }
        let current = ids
            .iter()
            .position(|id| id == &registry.selected)
            .unwrap_or(0);
        registry.selected = ids[(current + 1) % ids.len()].clone();
        crate::client::terminal::select_session(&registry.selected);
        Ok(())
    }

    pub async fn resize_all(&self, rows: u16, cols: u16) -> Result<(), String> {
        let jobs: Vec<_> = self
            .registry()
            .jobs
            .values()
            .filter(|job| job.terminal.is_some())
            .cloned()
            .collect();
        for job in jobs {
            let terminal = job.terminal.as_ref().unwrap();
            let running = job.state().running;
            if running
                && let Err(error) = terminal.perform(Operation::Resize { rows, cols }).await
                && terminal.perform(Operation::Status {}).await?["running"] == true
            {
                return Err(error);
            }
        }
        Ok(())
    }

    async fn terminal_snapshot(&self, job: &Job) -> Result<Value, String> {
        let terminal = job.terminal.as_ref().ok_or("job is not a PTY")?;
        let mut result = terminal.perform(Operation::Status {}).await?;
        result["running"] = json!(job.state().running);
        result["job_id"] = json!(job.id);
        result["terminal_id"] = json!(job.id);
        result["name"] = json!(job.title.as_deref().unwrap_or(&job.id));
        result["cwd"] = json!(job.cwd);
        result["cwd_kind"] = json!("startup");
        result["selected"] = json!(self.registry().selected == job.id);
        Ok(result)
    }

    async fn terminal_call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "terminal.open" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Open {
                    name: Option<String>,
                    cwd: Option<PathBuf>,
                    #[serde(default)]
                    select: bool,
                }
                let args: Open = parse(args)?;
                if args.name.as_ref().is_some_and(|name| {
                    name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control)
                }) {
                    return Err(
                        "terminal name must contain 1..=128 UTF-8 bytes and no control characters"
                            .into(),
                    );
                }
                let shell = self
                    .interactive_shell
                    .as_ref()
                    .ok_or("connection has no interactive shell")?;
                let requested = args.cwd.unwrap_or_else(|| self.root.clone());
                let cwd = if requested.is_absolute() {
                    requested
                } else {
                    self.root.join(requested)
                }
                .canonicalize()
                .map_err(|error| format!("invalid terminal cwd: {error}"))?;
                if !cwd.is_dir() || !cwd.starts_with(&self.root) {
                    return Err("terminal cwd must be an existing directory inside the authorized workspace".into());
                }
                if self
                    .registry()
                    .jobs
                    .values()
                    .filter(|job| job.terminal.is_some())
                    .count()
                    >= crate::terminals::MAX_TERMINALS
                {
                    return Err(format!(
                        "at most {} terminal jobs may be retained; close an existing terminal",
                        crate::terminals::MAX_TERMINALS
                    ));
                }
                let job = self.spawn_pty(
                    PtyStart {
                        id: self.next_job_id(),
                        command: interactive_command(shell),
                        title: args.name,
                        cwd,
                        env: HashMap::new(),
                        timeout_ms: 0,
                        size: None,
                        live: true,
                        compatibility_limit: true,
                    },
                    crate::runtime::call_scope::Source::capture(),
                )?;
                if args.select {
                    let mut registry = self.registry();
                    if registry.closed || !registry.jobs.contains_key(&job.id) {
                        return Err("terminal was closed before selection".into());
                    }
                    registry.selected = job.id.clone();
                    crate::client::terminal::select_session(&job.id);
                }
                self.terminal_snapshot(&job).await
            }
            "terminal.list" => {
                empty(args)?;
                let (mut jobs, selected) = {
                    let registry = self.registry();
                    (
                        registry
                            .jobs
                            .values()
                            .filter(|job| job.terminal.is_some())
                            .cloned()
                            .collect::<Vec<_>>(),
                        registry.selected.clone(),
                    )
                };
                jobs.sort_by(|a, b| a.id.cmp(&b.id));
                let mut terminals = Vec::with_capacity(jobs.len());
                for job in jobs {
                    terminals.push(self.terminal_snapshot(&job).await?);
                }
                Ok(
                    json!({"terminals":terminals,"selected":selected,"default_terminal_id":"main",
                    "max_terminals":crate::terminals::MAX_TERMINALS}),
                )
            }
            "terminal.select" => {
                let id = terminal_id(args)?;
                let job = self.job(&id)?;
                if job.terminal.is_none() {
                    return Err("job is not a PTY".into());
                }
                {
                    let mut registry = self.registry();
                    if registry.closed || !registry.jobs.contains_key(&id) {
                        return Err("terminal was closed before selection".into());
                    }
                    registry.selected = id.clone();
                    crate::client::terminal::select_session(&id);
                }
                self.terminal_snapshot(&job).await
            }
            "terminal.close" => {
                let id = terminal_id(args)?;
                protect_main(&id)?;
                let job = self.registry().jobs.get(&id).cloned();
                if let Some(job) = &job {
                    if job.terminal.is_none() {
                        return Err("job is not a PTY".into());
                    }
                    job.cancel.send_replace(true);
                    wait_finished(job, Some(Instant::now() + Duration::from_secs(8))).await;
                    if job.state().running {
                        return Err(
                            "terminal cleanup is still pending; observe the original job_id".into(),
                        );
                    }
                    self.registry().forget(&id);
                }
                Ok(
                    json!({"job_id":id,"terminal_id":id,"closed":job.is_some(),"selected":self.registry().selected}),
                )
            }
            _ => {
                let mut args = args
                    .as_object()
                    .cloned()
                    .ok_or("terminal arguments must be an object")?;
                let id = match args.remove("terminal_id") {
                    None => "main".to_owned(),
                    Some(Value::String(id)) if valid_terminal_id(&id) => id,
                    _ => {
                        return Err(
                            "terminal_id must be a nonempty string of at most 128 bytes".into()
                        );
                    }
                };
                let job = self.job(&id)?;
                let terminal = job.terminal.as_ref().ok_or("job is not a PTY")?;
                let operation = crate::backend::terminal_operation(name, Value::Object(args))?;
                let mut result = terminal.perform(operation).await?;
                if result.get("running").is_some() {
                    result["running"] = json!(job.state().running);
                }
                result["job_id"] = json!(id);
                result["terminal_id"] = json!(id);
                Ok(result)
            }
        }
    }

    pub fn management_tools() -> Vec<Value> {
        let id = json!({"terminal_id":{"type":"string","minLength":1,"maxLength":128}});
        vec![
            tool(
                "terminal.open",
                "Compatibility entry point. On Linux prefer ssmcp.exec.start with mode:'pty' and explicit interactive-shell argv. This creates one job in the shared exec registry; terminal_id and job_id identify the same physical PTY. Existing portable terminals remain supported here.",
                json!({"name":{"type":"string","minLength":1,"maxLength":128},
                    "cwd":{"type":"string","description":"Existing directory inside this client's workspace; defaults to the workspace root."},
                    "select":{"type":"boolean","default":false,"description":"Mark this terminal as selected for interactive frontends. The built-in dashboard remains an audit interface and keeps keyboard input for navigation."}}),
                &[],
                false,
            ),
            tool(
                "terminal.list",
                "Compatibility terminal-only view of ssmcp.exec.list, including selection for existing interactive frontends. terminal_id equals job_id; calls without terminal_id target the protected main job.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "terminal.select",
                "Compatibility selection control for existing interactive frontends. This changes the selected PTY job in the shared exec registry without starting another process; exec.write with the job_id provides explicit input.",
                id.clone(),
                &["terminal_id"],
                false,
            ),
            tool(
                "terminal.close",
                "Compatibility combined cancel-and-forget for one additional PTY job. Prefer exec.cancel then exec.forget. Repeated closes are harmless; the main job ends only with the client connection.",
                id,
                &["terminal_id"],
                false,
            ),
        ]
    }
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }
    #[cfg(all(test, unix))]
    pub(crate) fn observe_projection(
        &self,
    ) -> std::sync::mpsc::Receiver<super::events::ProjectedEvent> {
        self.events.observe_projection()
    }

    pub fn audit(&self, event: Value) {
        self.events.push(event);
    }

    pub fn tools() -> Vec<Value> {
        let id = json!({"job_id":{"type":"string","minLength":1,"maxLength":128}});
        vec![
            tool(
                "ssmcp.exec.start",
                "Start an independent argv command immediately and return job_id. Supply a short title describing the purpose of complex inline scripts; it labels the command without changing execution. Commands run concurrently, survive code_mode completion, and stop when this client disconnects or reloads. No model is invoked. mode:'pty' starts a real Linux controlling terminal for stdin/stdout/stderr with merged output; mode:'pipes' forces pipes. Omitted mode preserves legacy --tty output observation. Native macOS/Windows terminals remain available through the compatibility interface; explicit exec PTY currently requires Linux.",
                json!({"command":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_COMMAND_ARGS},
                    "title":{"type":"string","minLength":1,"maxLength":MAX_TITLE_CHARS,"description":"Optional display-only purpose, such as Check deployment status. At most 160 Unicode characters; surrounding whitespace is trimmed, blank titles and control or line-separator characters are rejected. Does not change argv, cwd, permissions or job identity."},
                    "cwd":{"type":"string","description":"Existing directory inside the authorized workspace; relative paths and the default resolve from that workspace."},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "track_artifacts":{"type":"boolean","default":false,"description":format!("Enable Linux task artifact tracking for a verified Agent. Sets the job's TMPDIR and SSMCP_ARTIFACT_DIR. Default soft limit when enabled: {DEFAULT_ARTIFACT_BUDGET_BYTES} logical bytes.")},
                    "artifact_budget_bytes":{"type":"integer","minimum":1,"description":"Custom logical-byte soft limit. Providing a positive value also enables tracking for compatibility."},
                    "temporary_artifacts":{"type":"boolean","default":false,"description":"Enable tracking for disposable, task-exclusive artifacts eligible for bounded shutdown cleanup. Omit to keep artifacts after close or reload."},
                    "mode":{"type":"string","enum":["pipes","pty"],"description":"Explicit pipes or full controlling PTY. Omission preserves legacy observation. PTY currently requires Linux."},
                    "rows":{"type":"integer","minimum":1,"maximum":1000,"description":"Initial PTY rows; only with mode:pty."},
                    "cols":{"type":"integer","minimum":1,"maximum":1000,"description":"Initial PTY columns; only with mode:pty."},
                    "stdin":{"enum":["null","pipe"],"type":"string","default":"null","description":"Pipe jobs only: null delivers EOF immediately; pipe enables exec.write and close_stdin. Omit stdin entirely for mode:pty."},
                    "timeout_ms":{"type":"integer","minimum":0,"default":0,"description":"0 means no execution deadline. A nonzero deadline terminates the command."}}),
                &["command"],
                false,
            ),
            tool(
                "ssmcp.exec.list",
                "List running and recently completed jobs, their commands, elapsed time and exit codes. Keeps the latest 128 jobs; running jobs are never evicted.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "ssmcp.exec.status",
                "Read a job state, exit code, output cursors and full-output artifact metadata.",
                id.clone(),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.wait",
                "Observe this command for at most 55000 ms. Omitted or null wait_ms uses a 55000 ms window; 0 returns an immediate snapshot. A pending receipt keeps the original job_id and leaves the command running. Continue observing that job after the window expires; this does not change its execution timeout.",
                json!({"job_id":{"type":"string"},"wait_ms":{"type":["integer","null"],"minimum":0,"maximum":MAX_READ_WAIT,"default":MAX_READ_WAIT}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.read",
                "Read incremental stdout/stderr and an atomic status snapshot. Each stream has its own byte cursor; base64 preserves exact bytes. Live streams use bounded tails. Completed pipe streams read their saved capture until retention expires; PTY pages always read their bounded raw-byte buffer without the legacy spill cursor jump. PTY stdout contains merged output and stderr_cursor must stay 0. Artifacts are also accessible through outputs.read.",
                json!({"job_id":{"type":"string"},"stdout_cursor":{"type":"integer","minimum":0,"default":0},
                    "stderr_cursor":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":32768,"default":16384},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_READ_WAIT,"default":0}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.write",
                "Write UTF-8 data or base64 bytes to a pipe or Linux PTY job. PTYs reject close_stdin before writing any bytes. Returns exact written byte count; retry only the remaining bytes after a timeout. close_stdin sends EOF only after all supplied bytes are written. Writes are serialized per job; cancellation and deadlines interrupt blocked writers.",
                json!({"job_id":{"type":"string","minLength":1,"maxLength":128},
                    "data":{"type":"string","maxLength":MAX_INPUT},
                    "data_base64":{"type":"string","maxLength":MAX_INPUT.div_ceil(3)*4},
                    "close_stdin":{"type":"boolean","default":false},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_INPUT_WAIT,"default":MAX_INPUT_WAIT}}),
                &["job_id"],
                false,
            ),
            tool(
                "ssmcp.exec.resize",
                "Resize a running PTY job, updating its native terminal dimensions and foreground SIGWINCH on Unix.",
                json!({"job_id":{"type":"string","minLength":1,"maxLength":128},
                    "rows":{"type":"integer","minimum":1,"maximum":1000},
                    "cols":{"type":"integer","minimum":1,"maximum":1000}}),
                &["job_id", "rows", "cols"],
                false,
            ),
            tool(
                "ssmcp.exec.cancel",
                "Terminate a job and its owned process tree; return its final state. Safe to repeat for a completed job.",
                id.clone(),
                &["job_id"],
                false,
            ),
            tool(
                "ssmcp.exec.forget",
                "Remove a completed job from the list. Running jobs must be cancelled first; output artifacts retain their normal cleanup policy.",
                id,
                &["job_id"],
                false,
            ),
            tool(
                "ssmcp.exec.events",
                "Read bounded execution events also shown on the client terminal.",
                super::events::schema()["properties"].clone(),
                &[],
                true,
            ),
        ]
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        self.call_scoped(name, args, crate::runtime::call_scope::Source::capture())
            .await
    }

    /// Only the native host's validated Exec route supplies a creator. The
    /// retained job, rather than a later result payload, owns that identity.
    pub(crate) async fn call_scoped(
        &self,
        name: &str,
        args: Value,
        creator: crate::runtime::call_scope::Source,
    ) -> Result<Value, String> {
        if name.starts_with("terminal.") {
            return self.terminal_call(name, args).await;
        }
        match name {
            "ssmcp.exec.start" => self.start(args, creator).await,
            "ssmcp.exec.write" => self.write_input(args).await,
            "ssmcp.exec.list" => {
                empty(args)?;
                let jobs: Vec<_> = self.registry().jobs.values().cloned().collect();
                let mut values: Vec<_> = jobs.iter().map(|job| job.snapshot()).collect();
                values.sort_by(|a, b| a["job_id"].as_str().cmp(&b["job_id"].as_str()));
                Ok(json!({"jobs":values,"max_running":MAX_JOBS,"max_retained":MAX_RETAINED}))
            }
            "ssmcp.exec.status" => {
                let args: Id = parse(args)?;
                self.job(&args.job_id)?.result_snapshot(creator).await
            }
            "ssmcp.exec.wait" => {
                let args: Wait = parse(args)?;
                let deadline = args.deadline()?;
                let job = self.job(&args.job_id)?;
                wait_finished(&job, Some(deadline)).await;
                let mut result = job.snapshot();
                let pending = result["running"] == true;
                result["wait_timed_out"] = json!(pending);
                result["wait_status"] = json!(if pending { "pending" } else { "ready" });
                Ok(result)
            }
            "ssmcp.exec.read" => {
                let args: Read = parse(args)?;
                check_wait(args.wait_ms)?;
                if !(1..=32768).contains(&args.max_bytes) {
                    return Err("max_bytes must be 1..=32768".into());
                }
                let job = self.job(&args.job_id)?;
                if let Some(terminal) = &job.terminal {
                    if args.stderr_cursor != 0 {
                        return Err(
                            "PTY stderr is merged into stdout; stderr_cursor must be 0".into()
                        );
                    }
                    let page = terminal
                        .read_page(args.stdout_cursor, args.max_bytes, args.wait_ms)
                        .await?;
                    let snapshot = job.snapshot();
                    return Ok(
                        json!({"job_id":job.id,"running":snapshot["running"],"exit_code":snapshot["exit_code"],
                        "stdout":page["text"],"stdout_base64":page["data_base64"],
                        "stdout_cursor":page["next_cursor"],"stdout_truncated":page["truncated"],
                        "stdout_eof":page["output_eof"] == true && page["next_cursor"] == snapshot["stdout"]["next_cursor"],
                        "stderr":"","stderr_base64":"","stderr_cursor":0,"stderr_truncated":false,"stderr_eof":true,
                        "snapshot":snapshot}),
                    );
                }
                wait_output(&job, args.stdout_cursor, args.stderr_cursor, args.wait_ms).await?;
                // Archive reads can touch disk. Keep their atomic output/status
                // snapshot on a blocking worker without holding a guard across await.
                tokio::task::spawn_blocking(move || -> Result<Value, String> {
                    let state = job.state();
                    let (out, on, ot) = state.stdout.read(args.stdout_cursor, args.max_bytes)?;
                    let (err, en, et) = state.stderr.read(args.stderr_cursor, args.max_bytes)?;
                    let mut result = json!({"job_id":job.id,"snapshot":job.snapshot_locked(&state),"running":state.running,
                        "exit_code":state.exit_code,"stdout":String::from_utf8_lossy(&out),"stderr":String::from_utf8_lossy(&err),
                        "stdout_base64":STANDARD.encode(&out),"stderr_base64":STANDARD.encode(&err),
                        "stdout_cursor":on,"stderr_cursor":en,"stdout_truncated":ot,"stderr_truncated":et,
                        "stdout_eof":state.stdout.eof && on==state.stdout.end,"stderr_eof":state.stderr.eof && en==state.stderr.end});
                    drop(state);
                    job.add_artifact_result(&mut result["snapshot"], &creator);
                    Ok(result)
                })
                .await
                .map_err(|error| format!("could not read command output: {error}"))?
            }
            "ssmcp.exec.resize" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Resize {
                    job_id: String,
                    rows: u16,
                    cols: u16,
                }
                let args: Resize = parse(args)?;
                let job = self.job(&args.job_id)?;
                let terminal = job.terminal.as_ref().ok_or("job is not a PTY")?;
                let mut result = terminal
                    .perform(Operation::Resize {
                        rows: args.rows,
                        cols: args.cols,
                    })
                    .await?;
                result["job_id"] = json!(job.id);
                Ok(result)
            }
            "ssmcp.exec.cancel" => {
                let args: Id = parse(args)?;
                protect_main(&args.job_id)?;
                let job = self.job(&args.job_id)?;
                if job.state().running {
                    job.cancel.send_replace(true);
                }
                wait_finished(&job, Some(Instant::now() + Duration::from_secs(8))).await;
                Ok(job.snapshot())
            }
            "ssmcp.exec.forget" => {
                let args: Id = parse(args)?;
                protect_main(&args.job_id)?;
                let mut registry = self.registry();
                if registry.closed {
                    return Err("shell command manager is closed".into());
                }
                if registry
                    .jobs
                    .get(&args.job_id)
                    .is_some_and(|job| job.state().running)
                {
                    return Err("job is running; cancel it before forgetting".into());
                }
                Ok(json!({"job_id":args.job_id,"forgotten":registry.forget(&args.job_id)}))
            }
            "ssmcp.exec.events" => self.events.read(args).await,
            _ => Err(format!("unknown shell execution capability: {name}")),
        }
    }
    fn job(&self, id: &str) -> Result<Arc<Job>, String> {
        if id.is_empty() || id.len() > 128 {
            return Err("invalid job_id".into());
        }
        let registry = self.registry();
        if registry.closed {
            return Err("shell command manager is closed".into());
        }
        registry
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown or expired background job; use exec.list(clientId)".into())
    }
    async fn write_input(&self, args: Value) -> Result<Value, String> {
        let args: Input = parse(args)?;
        if args.wait_ms > MAX_INPUT_WAIT {
            return Err(format!("stdin wait_ms must be 0..={MAX_INPUT_WAIT}"));
        }
        let data = match (args.data, args.data_base64) {
            (Some(_), Some(_)) => return Err("provide data or data_base64, not both".into()),
            (Some(data), None) => data.into_bytes(),
            (None, Some(data)) => {
                if data.len() > MAX_INPUT.div_ceil(3) * 4 {
                    return Err(format!("stdin is limited to {MAX_INPUT} bytes per write"));
                }
                STANDARD
                    .decode(data)
                    .map_err(|error| format!("invalid stdin base64: {error}"))?
            }
            (None, None) if args.close_stdin => Vec::new(),
            (None, None) => return Err("provide data, data_base64 or close_stdin:true".into()),
        };
        if data.len() > MAX_INPUT {
            return Err(format!("stdin is limited to {MAX_INPUT} bytes per write"));
        }
        let job = self.job(&args.job_id)?;
        if let Some(terminal) = &job.terminal {
            if args.close_stdin {
                return Err("close_stdin is not supported for a PTY; no input was written".into());
            }
            let (written, timed_out, error) = terminal.write_counted(&data, args.wait_ms).await;
            let result = job.write_result(written, data.len(), timed_out, error);
            self.events.push_scoped(
                json!({"type":"exec.stdin","job_id":job.id,
                "written":written,"closed":result["closed"],"timed_out":timed_out}),
                job.creator.clone(),
            );
            return Ok(result);
        }
        if job.input_mode != InputMode::Pipe {
            return Err("stdin is disabled; start the job with stdin:'pipe'".into());
        }
        let mut stopped = job.input_stopped.subscribe();
        if *stopped.borrow() {
            return if data.is_empty() && args.close_stdin {
                Ok(job.write_result(0, 0, false, None))
            } else {
                Err("job stdin is closed".into())
            };
        }
        let deadline = Instant::now() + Duration::from_millis(args.wait_ms);
        let mut input = tokio::select! {
            biased;
            _ = stopped.changed() => return Ok(job.write_result(0,data.len(),false,Some("job stdin closed before the write".into()))),
            result = timeout_at(deadline, job.stdin.lock()) => match result {
                Ok(input) => input,
                Err(_) => return Ok(job.write_result(0,data.len(),true,None)),
            }
        };
        let mut written = 0;
        let mut timed_out = false;
        let mut error = None;
        while written < data.len() {
            if *stopped.borrow() {
                error = Some("job stdin closed during the write".into());
                break;
            }
            let Some(pipe) = input.as_mut() else {
                error = Some("job stdin is closed".into());
                break;
            };
            // One write is cancellation-safe. write_all would hide partially
            // accepted bytes when a deadline or command exit interrupts it.
            let result = tokio::select! {
                biased;
                _ = stopped.changed() => {
                    error = Some("job stdin closed during the write".into());
                    break;
                },
                result = timeout_at(deadline, pipe.write(&data[written..])) => result,
            };
            match result {
                Ok(Ok(0)) => {
                    error = Some("job stdin accepted zero bytes".into());
                    break;
                }
                Ok(Ok(count)) => {
                    written += count;
                    job.state().stdin_written += count as u64;
                }
                Ok(Err(cause)) => {
                    error = Some(format!("stdin write failed: {cause}"));
                    break;
                }
                Err(_) => {
                    timed_out = true;
                    break;
                }
            }
        }
        if error.is_some() || (written == data.len() && args.close_stdin) {
            input.take();
            job.stop_input();
        }
        drop(input);
        let result = job.write_result(written, data.len(), timed_out, error);
        self.events.push_scoped(
            json!({"type":"exec.stdin","job_id":job.id,
            "written":written,"closed":result["closed"],"timed_out":timed_out}),
            job.creator.clone(),
        );
        Ok(result)
    }

    async fn start(
        &self,
        args: Value,
        creator: crate::runtime::call_scope::Source,
    ) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Start {
            command: Vec<String>,
            title: Option<String>,
            cwd: Option<PathBuf>,
            env: Option<HashMap<String, String>>,
            artifact_budget_bytes: Option<u64>,
            #[serde(default)]
            track_artifacts: bool,
            #[serde(default)]
            temporary_artifacts: bool,
            stdin: Option<InputMode>,
            mode: Option<Mode>,
            rows: Option<u16>,
            cols: Option<u16>,
            #[serde(default)]
            timeout_ms: u64,
        }
        let args: Start = parse(args)?;
        let stdin = args.stdin.unwrap_or_default();
        if args.mode == Some(Mode::Pty) {
            if !cfg!(target_os = "linux") {
                return Err("explicit exec PTY mode requires Linux exact-count input; native portable terminals remain available through terminal.open".into());
            }
            if args.stdin.is_some() {
                return Err("PTY mode owns stdin; omit the pipe/null stdin option".into());
            }
            if args.artifact_budget_bytes.is_some()
                || args.track_artifacts
                || args.temporary_artifacts
            {
                return Err("task artifact supervision currently requires pipes mode".into());
            }
            if args.rows.is_some_and(|n| n == 0 || n > 1000)
                || args.cols.is_some_and(|n| n == 0 || n > 1000)
            {
                return Err("PTY rows and cols must be 1..=1000".into());
            }
        } else if args.rows.is_some() || args.cols.is_some() {
            return Err("rows and cols require mode:'pty'".into());
        }
        let title = job_title(args.title)?;
        let artifact_budget = args.artifact_budget_bytes.or_else(|| {
            (args.track_artifacts || args.temporary_artifacts)
                .then_some(DEFAULT_ARTIFACT_BUDGET_BYTES)
        });
        if let Some(budget) = artifact_budget {
            if budget == 0 {
                return Err("artifact_budget_bytes must be positive".into());
            }
            if !cfg!(target_os = "linux") {
                return Err("task artifact budgets are available on Linux".into());
            }
            if creator
                .call
                .as_ref()
                .and_then(|scope| scope.thread())
                .is_none()
            {
                return Err("task artifact budgets require a verified Agent thread".into());
            }
        }
        if args.command.is_empty()
            || args.command.len() > MAX_COMMAND_ARGS
            || args.command[0].is_empty()
            || args
                .command
                .iter()
                .any(|s| s.len() > 8192 || s.contains('\0'))
            || args.command.iter().map(String::len).sum::<usize>() > 65536
        {
            return Err("command must contain 1..=256 argv strings, at most 8192 bytes each and 65536 bytes total".into());
        }
        if let Some(env) = &args.env
            && (env.len() > 128
                || env.iter().any(|(k, v)| {
                    k.is_empty()
                        || k.len() > 128
                        || k.contains(['=', '\0'])
                        || v.len() > 8192
                        || v.contains('\0')
                })
                || env.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>() > 65536)
        {
            return Err("invalid or oversized environment override".into());
        }
        let requested = args.cwd.unwrap_or_else(|| self.root.clone());
        let cwd = if requested.is_absolute() {
            requested
        } else {
            self.root.join(requested)
        }
        .canonicalize()
        .map_err(|error| format!("invalid execution cwd: {error}"))?;
        if !cwd.is_dir() || !cwd.starts_with(&self.root) {
            return Err("cwd must be an existing directory inside the authorized workspace".into());
        }
        if args.mode == Some(Mode::Pty) {
            let id = self.next_job_id();
            if let Some(pool) = &self.observer {
                pool.terminal_begin(&id, &cwd).await?;
            }
            let started = self.spawn_pty(
                PtyStart {
                    id: id.clone(),
                    command: args.command,
                    title,
                    cwd,
                    env: args.env.unwrap_or_default(),
                    timeout_ms: args.timeout_ms,
                    size: Some((args.rows.unwrap_or(24), args.cols.unwrap_or(80))),
                    live: true,
                    compatibility_limit: false,
                },
                creator,
            );
            if started.is_err()
                && let Some(pool) = &self.observer
            {
                pool.terminal_closed(&id);
            }
            let job = started?;
            return Ok(job.snapshot());
        }
        let store = &self.output;
        // Reserve capacity before spawning: rejected requests must never run commands.
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            format!("at most {MAX_JOBS} commands may run at once; wait or cancel an existing job")
        })?;
        let id = format!(
            "{}-{:x}",
            self.namespace,
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        );
        let observation = match &self.observer {
            Some(pool) => Some(pool.begin(id.clone(), &json!(args.command), &cwd).await?),
            None => None,
        };
        let tty_slot = observation.as_ref().map(crate::observer::Lease::slot_id);
        let mut registry = self.registry();
        if registry.closed {
            return Err("shell command manager is closed".into());
        }
        let evicted = if registry.jobs.len() >= MAX_RETAINED {
            registry
                .jobs
                .values()
                .filter(|job| job.id != "main" && !job.state().running)
                .min_by_key(|job| job.timing.started)
                .map(|job| job.id.clone())
        } else {
            None
        };
        let mut command = Command::new(&args.command[0]);
        command
            .args(&args.command[1..])
            .current_dir(&cwd)
            .stdin(if stdin == InputMode::Pipe {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let pty_reader: Option<Box<dyn AsyncRead + Unpin + Send>> = {
            #[cfg(unix)]
            {
                if args.mode.is_none()
                    && let Some(lease) = &observation
                {
                    let (reader, stdout, stderr) = crate::observer::pty::Output::open(lease)
                        .map_err(|error| format!("could not open command PTY: {error}"))?;
                    command
                        .stdout(stdout)
                        .stderr(stderr)
                        .env("TERM", "xterm-256color");
                    Some(Box::new(reader))
                } else {
                    None
                }
            }
            #[cfg(not(unix))]
            {
                None
            }
        };
        let output_mode = if pty_reader.is_some() { "pty" } else { "pipes" };
        if let Some(env) = args.env {
            command.envs(env);
        }
        let started = std::time::Instant::now();
        let deadline = (args.timeout_ms != 0)
            .then(|| {
                started
                    .checked_add(Duration::from_millis(args.timeout_ms))
                    .ok_or("timeout_ms cannot be represented by the execution clock")
            })
            .transpose()?;
        let timing = ExecutionTiming { started, deadline };
        #[cfg(target_os = "linux")]
        let mut artifacts = artifact_budget
            .map(|limit| artifacts::Artifacts::new(&self.root, limit))
            .transpose()
            .map_err(|err| format!("could not prepare task artifact directory: {err}"))?;
        #[cfg(target_os = "linux")]
        if let Some(artifacts) = &artifacts {
            command
                .env("TMPDIR", &artifacts.path)
                .env("SSMCP_ARTIFACT_DIR", &artifacts.path);
        }
        let mut process =
            Managed::spawn(&mut command).map_err(|e| format!("could not start command: {e}"))?;
        #[cfg(target_os = "linux")]
        if let Some(artifacts) = &mut artifacts {
            artifacts.mark_started();
        }
        let pid = process.child.id().ok_or("command has no process ID")?;
        let stdout: Box<dyn AsyncRead + Unpin + Send> = match pty_reader {
            Some(reader) => reader,
            None => Box::new(
                process
                    .child
                    .stdout
                    .take()
                    .ok_or("command stdout unavailable")?,
            ),
        };
        let stderr: Box<dyn AsyncRead + Unpin + Send> = if output_mode == "pty" {
            Box::new(tokio::io::empty())
        } else {
            Box::new(
                process
                    .child
                    .stderr
                    .take()
                    .ok_or("command stderr unavailable")?,
            )
        };
        let (cancel, cancel_rx) = watch::channel(false);
        let (input_stopped, _) = watch::channel(stdin != InputMode::Pipe);
        let job = Arc::new(Job {
            creator,
            id: id.clone(),
            pid,
            command: args.command,
            title,
            cwd,
            timing,
            state: Mutex::new(State {
                running: true,
                #[cfg(target_os = "linux")]
                tree_reaped: false,
                exit_code: None,
                exit_signal: None,
                cancelled: false,
                timed_out: false,
                error: None,
                finished: None,
                stdin_open: stdin == InputMode::Pipe,
                stdin_written: 0,
                stdout: Output::new(store, format!("job:{id}:stdout")),
                stderr: Output::new(store, format!("job:{id}:stderr")),
            }),
            changed: Arc::new(Notify::new()),
            cancel,
            input_mode: stdin,
            stdin: AsyncMutex::new(process.child.stdin.take()),
            input_stopped,
            observation: Mutex::new(observation),
            output_mode,
            tty_slot,
            terminal: None,
            #[cfg(target_os = "linux")]
            artifacts: artifacts.map(Mutex::new),
            #[cfg(target_os = "linux")]
            temporary_artifacts: args.temporary_artifacts,
        });
        // A failed spawn must leave the retained history untouched.
        if let Some(id) = &evicted {
            registry.forget(id);
        }
        registry.jobs.insert(id.clone(), job.clone());
        drop(registry);
        let worker_command =
            crate::worker::native_command_started(&job.creator, &job.id, &job.command);
        let mut started = job.snapshot();
        started["type"] = json!("exec.started");
        self.events
            .push_execution(started, job.creator.clone(), job.timing);
        let events = self.events.clone();
        let worker = job.clone();
        tokio::spawn(async move {
            let mut out = tokio::spawn(capture(worker.clone(), stdout, "stdout", events.clone()));
            let mut err = tokio::spawn(capture(worker.clone(), stderr, "stderr", events.clone()));
            let mut cancel_rx = cancel_rx;
            let outcome = tokio::select! {
                biased;
                _=cancel_rx.changed() => (None,true,false,None,None),
                result=async {
                    if let Some(deadline) = worker.timing.deadline {
                        timeout_at(deadline.into(), process.wait()).await
                    } else { Ok(process.wait().await) }
                } => match result {
                    Ok(Ok(status)) => (status.code(),false,false,None,exit_signal(&status)),
                    Ok(Err(error)) => (None,false,false,Some(error.to_string()),None),
                    Err(_) => (None,false,true,None,None),
                }
            };
            // Wake blocked/queued writers before waiting for their pipe lock.
            // Command cancellation and execution deadlines must not wait for
            // a reader that has stopped consuming stdin.
            worker.stop_input();
            worker.stdin.lock().await.take();
            process.shutdown().await;
            tokio::join!(
                drain(&mut out, &worker, "stdout"),
                drain(&mut err, &worker, "stderr")
            );
            {
                let mut state = worker.state();
                // Status reads can observe completion without a notification.
                // Release capacity before publishing it under the same lock.
                drop(permit);
                state.running = false;
                // The Linux supervisor can report a normal zero exit only
                // after reaping the complete owned tree. A signal or other
                // status cannot exclude supervisor escalation/failure.
                #[cfg(target_os = "linux")]
                {
                    state.tree_reaped = outcome.0 == Some(0);
                }
                state.exit_code = outcome.0;
                state.cancelled = outcome.1;
                state.timed_out = outcome.2;
                state.error = outcome.3;
                state.exit_signal = outcome.4;
                state.finished = Some(Instant::now());
            }
            let status = {
                let state = worker.state();
                if state.cancelled {
                    "cancelled".into()
                } else if state.timed_out {
                    "timed out".into()
                } else if let Some(error) = &state.error {
                    format!("failed: {error}")
                } else if let Some(code) = state.exit_code {
                    format!("exit {code}")
                } else if let Some(signal) = state.exit_signal {
                    format!("signal {signal}")
                } else {
                    "finished without a reported exit status".into()
                }
            };
            if let Some(command) = worker_command {
                command.finish(&status);
            }
            if let Some(lease) = worker.observation.lock().unwrap().take() {
                lease.finish(&status);
            }
            let mut snapshot = worker.snapshot();
            snapshot["type"] = json!("exec.exited");
            events.push_scoped(snapshot, worker.creator.clone());
            let changed = worker.changed.clone();
            drop(worker);
            changed.notify_waiters();
        });
        let mut result = job.snapshot();
        #[cfg(target_os = "linux")]
        if let Some(artifacts) = &job.artifacts {
            result["artifacts"] = artifacts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .description();
        }
        result["evicted_job_id"] = json!(evicted);
        Ok(result)
    }
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = {
            let mut registry = self.registry();
            if registry.closed {
                return;
            }
            registry.closed = true;
            registry.jobs.values().cloned().collect()
        };
        for job in &jobs {
            job.cancel.send_replace(true);
            if job.terminal.is_some() {
                crate::client::terminal::close_session(&job.id);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(8);
        let waits = jobs.iter().map(|job| wait_finished(job, Some(deadline)));
        futures_util::future::join_all(waits).await;
        drop(jobs);
        #[cfg(target_os = "linux")]
        self.reclaim_temporary().await;
        self.events.close();
    }

    #[cfg(target_os = "linux")]
    async fn reclaim_temporary(&self) {
        let jobs = {
            let mut registry = self.registry();
            let mut jobs = std::mem::take(&mut registry.retired_temporary);
            jobs.extend(
                registry
                    .jobs
                    .drain()
                    .map(|(_, job)| job)
                    .filter(|job| job.temporary_artifacts),
            );
            jobs
        };
        if jobs.is_empty() {
            return;
        }
        let deadline = std::time::Instant::now() + artifacts::CLEANUP_TIME;
        let (done, result) = tokio::sync::oneshot::channel();
        // The async runtime may be about to shut down. A stuck filesystem
        // syscall cannot hold the runtime or extend its async wait.
        let thread = std::thread::Builder::new().name("ssmcp-artifact-reclaim".into()).spawn(move || {
            for job in jobs {
                let job = match Arc::try_unwrap(job) {
                    Ok(job) => job,
                    Err(job) => {
                        eprintln!("ssmcp: preserving temporary artifacts for job {} (active job references)", job.id);
                        continue;
                    }
                };
                if !job.state().tree_reaped {
                    eprintln!("ssmcp: preserving temporary artifacts for job {} (owned tree exit unverified)", job.id);
                    continue;
                }
                if let Some(artifacts) = job.artifacts {
                    artifacts.into_inner().unwrap_or_else(|e| e.into_inner()).reclaim(deadline);
                }
            }
            let _ = done.send(());
        });
        match thread {
            Err(error) => {
                eprintln!("ssmcp: could not start temporary artifact reclamation: {error}")
            }
            Ok(_) => match timeout_at(deadline.into(), result).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    eprintln!("ssmcp: temporary artifact reclamation stopped unexpectedly")
                }
                Err(_) => eprintln!("ssmcp: temporary artifact reclamation reached its time limit"),
            },
        }
    }
}

fn protect_main(id: &str) -> Result<(), String> {
    if id == "main" {
        Err("main is the connection terminal; disconnect the client to close it".into())
    } else {
        Ok(())
    }
}

fn valid_terminal_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}

fn terminal_id(args: Value) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Id {
        terminal_id: String,
    }
    let args: Id = parse(args)?;
    if !valid_terminal_id(&args.terminal_id) {
        return Err("invalid terminal_id".into());
    }
    Ok(args.terminal_id)
}

fn interactive_command(shell: &Path) -> Vec<String> {
    let mut command = vec![shell.to_string_lossy().into_owned()];
    #[cfg(unix)]
    command.push("-i".into());
    #[cfg(windows)]
    {
        let name = shell
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(name.as_str(), "bash" | "zsh" | "sh" | "fish") {
            command.push("-i".into());
        }
        if matches!(name.as_str(), "powershell" | "pwsh") {
            command.push("-NoLogo".into());
        }
    }
    command
}

fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        let registry = self.registry.get_mut().unwrap_or_else(|e| e.into_inner());
        for job in registry.jobs.values() {
            job.cancel.send_replace(true);
        }
    }
}
fn wait_deadline(ms: u64) -> Result<Instant, String> {
    check_wait(ms)?;
    Instant::now()
        .checked_add(Duration::from_millis(ms))
        .ok_or_else(|| "wait_ms cannot be represented by the execution clock".into())
}

async fn wait_finished(job: &Job, deadline: Option<Instant>) {
    loop {
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !job.state().running {
            return;
        }
        match deadline {
            Some(deadline) => {
                if deadline <= Instant::now() || timeout_at(deadline, notified).await.is_err() {
                    return;
                }
            }
            None => notified.await,
        }
    }
}
async fn wait_output(job: &Job, out: u64, err: u64, wait_ms: u64) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        {
            let state = job.state();
            if out > state.stdout.end || err > state.stderr.end {
                return Err("output cursor is ahead of the stream".into());
            }
            if state.stdout.end > out || state.stderr.end > err || !state.running || wait_ms == 0 {
                return Ok(());
            }
        }
        if timeout_at(deadline, notified).await.is_err() {
            return Ok(());
        }
    }
}
async fn capture<R: AsyncRead + Unpin>(
    job: Arc<Job>,
    mut reader: R,
    stream: &'static str,
    events: Arc<EventLog>,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                finish_output(&job, stream, None);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                finish_output(
                    &job,
                    stream,
                    Some(format!("could not read {stream}: {error}")),
                );
                break;
            }
            Ok(count) => {
                if let Some(lease) = job.observation.lock().unwrap().as_ref() {
                    lease.append(&buffer[..count]);
                }
                let (offset, notice) = {
                    let mut state = job.state();
                    let output = if stream == "stdout" {
                        &mut state.stdout
                    } else {
                        &mut state.stderr
                    };
                    let was_spilled = output.capture.is_spilled();
                    let offset = output.end;
                    output.append(&buffer[..count]);
                    (
                        offset,
                        (!was_spilled && output.capture.is_spilled())
                            .then(|| output.capture.metadata()),
                    )
                };
                // Retention and live observation are independent: spilling never
                // stops new output events or repeats bytes already displayed.
                events.push_scoped(json!({"type":format!("exec.{stream}"),"job_id":job.id,
                    "method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode(&buffer[..count]),
                    "stream":stream,"processId":job.id,"offset":offset,"next_offset":offset+count as u64}}), job.creator.clone());
                if let Some(artifact) = notice {
                    events.push_scoped(json!({"type":"ssmcp.output_archived","job_id":job.id,"stream":stream,"artifact":artifact}), job.creator.clone());
                }
                job.changed.notify_waiters();
            }
        }
    }
}
fn finish_output(job: &Job, stream: &str, error: Option<String>) {
    let mut state = job.state();
    let output = if stream == "stdout" {
        &mut state.stdout
    } else {
        &mut state.stderr
    };
    output.finish(error);
    drop(state);
    job.changed.notify_waiters();
}
async fn drain(handle: &mut JoinHandle<()>, job: &Job, stream: &str) {
    match timeout(Duration::from_secs(2), &mut *handle).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => finish_output(
            job,
            stream,
            Some(format!("{stream} capture task failed: {error}")),
        ),
        Err(_) => {
            handle.abort();
            let _ = handle.await;
            finish_output(
                job,
                stream,
                Some("output pipe remained open after command exit; capture stopped".into()),
            );
        }
    }
}
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    readonly: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":readonly,"destructiveHint":!readonly,"openWorldHint":!readonly}})
}
fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid execution arguments: {e}"))
}
fn job_title(title: Option<String>) -> Result<Option<String>, String> {
    title
        .map(|title| {
            if title.chars().count() > MAX_TITLE_CHARS
                || title
                    .chars()
                    .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
                || title.trim().is_empty()
            {
                return Err(format!(
                    "title must contain 1..={MAX_TITLE_CHARS} Unicode characters, with non-whitespace text and no control or line-separator characters"
                ));
            }
            Ok(title.trim().to_owned())
        })
        .transpose()
}
fn empty(args: Value) -> Result<(), String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    let _: Empty = parse(args)?;
    Ok(())
}
fn check_wait(ms: u64) -> Result<(), String> {
    if ms > MAX_READ_WAIT {
        Err(format!("wait_ms must be 0..={MAX_READ_WAIT}"))
    } else {
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    job_id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    job_id: String,
    data: Option<String>,
    data_base64: Option<String>,
    #[serde(default)]
    close_stdin: bool,
    #[serde(default = "default_wait")]
    wait_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    job_id: String,
    wait_ms: Option<u64>,
}
impl Wait {
    fn deadline(&self) -> Result<Instant, String> {
        wait_deadline(self.wait_ms.unwrap_or(MAX_READ_WAIT))
    }
}
fn default_wait() -> u64 {
    5000
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    job_id: String,
    #[serde(default)]
    stdout_cursor: u64,
    #[serde(default)]
    stderr_cursor: u64,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
    #[serde(default)]
    wait_ms: u64,
}
fn default_bytes() -> usize {
    16384
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn isolated_output() -> Arc<OutputStore> {
        OutputStore::new_connection().unwrap()
    }

    fn completed_job(store: &Arc<OutputStore>) -> Arc<Job> {
        let id = format!("test-{}", NEXT_JOB.fetch_add(1, Ordering::Relaxed));
        let (cancel, _) = watch::channel(false);
        let (input_stopped, _) = watch::channel(true);
        let mut stdout = Output::new(store, format!("job:{id}:stdout"));
        let mut stderr = Output::new(store, format!("job:{id}:stderr"));
        stdout.finish(None);
        stderr.finish(None);
        Arc::new(Job {
            creator: crate::runtime::call_scope::Source::default(),
            id,
            pid: 0,
            command: vec!["completed fixture".into()],
            title: None,
            cwd: std::env::current_dir().unwrap(),
            timing: ExecutionTiming::new(Instant::now().into_std(), 0),
            state: Mutex::new(State {
                running: false,
                #[cfg(target_os = "linux")]
                tree_reaped: false,
                exit_code: Some(0),
                exit_signal: None,
                cancelled: false,
                timed_out: false,
                error: None,
                finished: Some(Instant::now()),
                stdin_open: false,
                stdin_written: 0,
                stdout,
                stderr,
            }),
            changed: Arc::new(Notify::new()),
            cancel,
            input_mode: InputMode::Null,
            stdin: AsyncMutex::new(None),
            input_stopped,
            observation: Mutex::new(None),
            output_mode: "pipes",
            tty_slot: None,
            terminal: None,
            #[cfg(target_os = "linux")]
            artifacts: None,
            #[cfg(target_os = "linux")]
            temporary_artifacts: false,
        })
    }

    #[test]
    fn completion_wait_defaults_to_a_bounded_observation_deadline() {
        for args in [
            json!({"job_id":"fixture"}),
            json!({"job_id":"fixture","wait_ms":null}),
        ] {
            let before = Instant::now();
            let wait: Wait = parse(args).unwrap();
            let deadline = wait.deadline().unwrap();
            let after = Instant::now();
            let window = Duration::from_millis(crate::wire::MAX_OBSERVATION_WAIT_MS);
            assert!(deadline >= before + window && deadline <= after + window);
        }
        let zero: Wait = parse(json!({"job_id":"fixture","wait_ms":0})).unwrap();
        assert!(zero.deadline().unwrap() <= Instant::now());
        assert!(wait_deadline(crate::wire::MAX_OBSERVATION_WAIT_MS).is_ok());
        assert!(wait_deadline(crate::wire::MAX_OBSERVATION_WAIT_MS + 1).is_err());
    }

    #[tokio::test]
    async fn observation_limits_reject_excess_waits_without_changing_command_deadlines() {
        let shell = Shell::new();
        let id = start_fixture(&shell, "wait", 900_001).await;
        for name in ["ssmcp.exec.wait", "ssmcp.exec.read"] {
            assert!(
                shell
                    .call(
                        name,
                        json!({"job_id":id,"wait_ms":crate::wire::MAX_OBSERVATION_WAIT_MS+1})
                    )
                    .await
                    .unwrap_err()
                    .starts_with("wait_ms must be")
            );
        }
        let snapshot = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":0}))
            .await
            .unwrap();
        assert_eq!(snapshot["running"], true);
        assert_eq!(snapshot["wait_status"], "pending");
        assert_eq!(snapshot["timeout_ms"], 900_001);
        assert_eq!(snapshot["timed_out"], false);
        assert_eq!(snapshot["cancelled"], false);
        shell
            .call("ssmcp.exec.cancel", json!({"job_id":id}))
            .await
            .unwrap();
        shell.shutdown().await;
    }

    #[test]
    fn discovery_matches_observation_limits_and_keeps_execution_timeout_independent() {
        let tools = Shell::tools();
        for name in ["ssmcp.exec.wait", "ssmcp.exec.read"] {
            let descriptor = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(
                descriptor["inputSchema"]["properties"]["wait_ms"]["maximum"],
                crate::wire::MAX_OBSERVATION_WAIT_MS
            );
        }
        let start = tools
            .iter()
            .find(|tool| tool["name"] == "ssmcp.exec.start")
            .unwrap();
        assert_eq!(
            start["inputSchema"]["properties"]["timeout_ms"]["default"],
            0
        );
        assert!(
            start["inputSchema"]["properties"]["timeout_ms"]
                .get("maximum")
                .is_none()
        );
    }

    #[tokio::test]
    async fn exec_title_rejection_precedes_job_creation() {
        let shell = Shell::new();
        for title in [
            json!(""),
            json!("   "),
            json!(" leading newline\n"),
            json!("title\twith tab"),
            json!("title\u{1b}[2J"),
            json!("title\u{7f}"),
            json!("title\u{2028}second line"),
            json!("title\u{2029}second paragraph"),
            json!("x".repeat(MAX_TITLE_CHARS + 1)),
            json!("界".repeat(MAX_TITLE_CHARS + 1)),
        ] {
            let error = shell
                .call(
                    "ssmcp.exec.start",
                    json!({"command":[std::env::current_exe().unwrap(),"--exact",
                        "backend::shell::tests::fixture_process","--nocapture"],
                        "env":{"SSMCP_SHELL_TEST_FIXTURE":"next"},
                        "title":title,"timeout_ms":1000}),
                )
                .await
                .unwrap_err();
            assert!(error.starts_with("title must contain"), "{error}");
            assert!(shell.registry().jobs.is_empty());
            assert_eq!(shell.slots.available_permits(), MAX_JOBS);
        }
        let events = shell
            .call("ssmcp.exec.events", json!({"wait_ms":0}))
            .await
            .unwrap();
        assert!(events["events"].as_array().unwrap().is_empty());
        assert_eq!(job_title(None).unwrap(), None);
        assert_eq!(
            job_title(Some("界".repeat(MAX_TITLE_CHARS))).unwrap(),
            Some("界".repeat(MAX_TITLE_CHARS))
        );
    }

    #[tokio::test]
    async fn exec_title_is_metadata_through_the_job_lifecycle() {
        let shell = Shell::new();
        let command = json!([
            std::env::current_exe().unwrap(),
            "--exact",
            "backend::shell::tests::fixture_process",
            "--nocapture"
        ]);
        let started = shell
            .call(
                "ssmcp.exec.start",
                json!({"command":command,"title":"  Read fixture output 中文  ",
                    "env":{"SSMCP_SHELL_TEST_FIXTURE":"next"},"timeout_ms":5000}),
            )
            .await
            .unwrap();
        let id = started["job_id"].as_str().unwrap();
        let finished = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        let read = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        let listed = shell.call("ssmcp.exec.list", json!({})).await.unwrap();
        let listed = listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["job_id"] == id)
            .unwrap();
        for snapshot in [&started, &finished, &read["snapshot"], listed] {
            assert_eq!(snapshot["title"], "Read fixture output 中文");
            assert_eq!(snapshot["command"], command);
            assert_eq!(snapshot["job_id"], id);
            assert_eq!(snapshot["cwd"], started["cwd"]);
        }
        assert_eq!(finished["exit_code"], 0);
        assert_eq!(output(&read), "next");
        let events = shell
            .call("ssmcp.exec.events", json!({"max_events":256,"wait_ms":0}))
            .await
            .unwrap();
        let event = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|record| &record["event"])
            .find(|event| event["type"] == "exec.started")
            .unwrap();
        assert_eq!(event["title"], "Read fixture output 中文");
        assert_eq!(event["job_id"], id);
        assert_eq!(event["command"], command);
    }

    #[tokio::test]
    async fn failed_start_preserves_the_full_retained_history() {
        let shell = Shell::new();
        let store = OutputStore::global().unwrap();
        let jobs: Vec<_> = (0..MAX_RETAINED).map(|_| completed_job(&store)).collect();
        for job in &jobs {
            shell.registry().jobs.insert(job.id.clone(), job.clone());
        }
        let missing = std::env::temp_dir().join(format!(
            "ssmcp-missing-executable-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        assert!(
            shell
                .call("ssmcp.exec.start", json!({"command":[missing]}))
                .await
                .is_err()
        );
        let registry = shell.registry();
        assert_eq!(registry.jobs.len(), MAX_RETAINED);
        assert!(jobs.iter().all(|job| registry.jobs.contains_key(&job.id)));
        assert_eq!(shell.slots.available_permits(), MAX_JOBS);
    }

    #[tokio::test]
    async fn retained_completed_output_is_bounded_and_reads_exact_memory_ranges() {
        let store = isolated_output();
        let shell = Shell::with_workspace(std::env::current_dir().unwrap(), store).unwrap();
        let payload: Vec<u8> = (0..MAX_OUTPUT + 137).map(|i| (i % 251) as u8).collect();
        let reversed: Vec<u8> = payload.iter().rev().copied().collect();
        let mut jobs = Vec::new();
        for _ in 0..MAX_RETAINED {
            let job = completed_job(&shell.output);
            {
                let mut state = job.state();
                state.stdout = Output::new(&shell.output, format!("job:{}:stdout", job.id));
                state.stderr = Output::new(&shell.output, format!("job:{}:stderr", job.id));
                state.stdout.append(&payload);
                assert_eq!(state.stdout.bytes.len(), MAX_OUTPUT);
                state.stdout.finish(None);
                state.stderr.append(&reversed);
                state.stderr.finish(None);
                for output in [&state.stdout, &state.stderr] {
                    assert_eq!(output.bytes.capacity(), 0);
                    assert!(output.capture.inline_bytes().is_empty());
                    assert!(output.read_from_capture);
                    assert_eq!(output.metadata()["start_cursor"], 0);
                    assert_eq!(output.metadata()["truncated"], false);
                    assert!(output.capture.metadata().unwrap().complete);
                }
            }
            shell.registry().jobs.insert(job.id.clone(), job.clone());
            jobs.push(job);
        }
        assert_eq!(shell.registry().jobs.len(), MAX_RETAINED);

        let id = &jobs[0].id;
        let mut cursor = 0;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let page = shell
                .call(
                    "ssmcp.exec.read",
                    json!({
                        "job_id":id,"stdout_cursor":cursor,"stderr_cursor":cursor,"max_bytes":32768
                    }),
                )
                .await
                .unwrap();
            stdout.extend(
                STANDARD
                    .decode(page["stdout_base64"].as_str().unwrap())
                    .unwrap(),
            );
            stderr.extend(
                STANDARD
                    .decode(page["stderr_base64"].as_str().unwrap())
                    .unwrap(),
            );
            assert_eq!(page["stdout_truncated"], false);
            assert_eq!(page["stderr_truncated"], false);
            assert_eq!(page["stdout_cursor"], page["stderr_cursor"]);
            cursor = page["stdout_cursor"].as_u64().unwrap();
            if page["stdout_eof"] == true {
                assert_eq!(page["stderr_eof"], true);
                break;
            }
            assert!(cursor < payload.len() as u64);
        }
        assert_eq!(stdout, payload);
        assert_eq!(stderr, reversed);
        for (offset, limit) in [(17, 29), (payload.len() - 11, 127), (payload.len(), 127)] {
            let page = shell
                .call(
                    "ssmcp.exec.read",
                    json!({
                        "job_id":id,"stdout_cursor":offset,"stderr_cursor":offset,"max_bytes":limit
                    }),
                )
                .await
                .unwrap();
            let end = payload.len().min(offset + limit);
            assert_eq!(
                STANDARD
                    .decode(page["stdout_base64"].as_str().unwrap())
                    .unwrap(),
                payload[offset..end]
            );
            assert_eq!(page["stdout_cursor"], end);
            assert_eq!(page["stdout_eof"], end == payload.len());
        }
        shell.shutdown().await;
    }

    #[test]
    fn completed_small_output_keeps_only_one_inline_copy() {
        let store = isolated_output();
        let mut output = Output::new(&store, "small-completed-fixture".into());
        let data = vec![b'x'; COMPLETED_INLINE_BYTES];
        output.append(&data);
        output.finish(None);
        assert_eq!(output.bytes.capacity(), 0);
        assert_eq!(output.capture.inline_bytes(), data);
        assert!(!output.capture.is_spilled());
        assert_eq!(
            output.read(0, 32768).unwrap(),
            (data, COMPLETED_INLINE_BYTES as u64, false)
        );
    }

    #[test]
    fn failed_capture_preserves_the_tail_and_evicted_memory_output_returns_errors() {
        let store = isolated_output();
        let data = vec![b'x'; MAX_OUTPUT + 137];
        let mut failed = Output::new(&store, "failed-completed-fixture".into());
        failed.append(&data);
        failed.finish(Some("fixture capture failure".into()));
        assert!(!failed.read_from_capture);
        assert_eq!(failed.bytes.len(), MAX_OUTPUT);
        assert_eq!(failed.read(0, 127).unwrap(), (vec![b'x'; 127], 264, true));
        assert_eq!(failed.metadata()["error"], "fixture capture failure");
        assert!(!failed.capture.metadata().unwrap().complete);

        let mut retained = Output::new(&store, "evicted-completed-fixture".into());
        retained.append(&data);
        retained.finish(None);
        assert!(retained.read_from_capture);
        assert!(retained.capture.metadata().unwrap().path.is_none());
        assert_eq!(
            retained.read(0, 127).unwrap(),
            (vec![b'x'; 127], 127, false)
        );
        // Fill the real shared memory budget so completed output is evicted by
        // the same policy used in production, without modifying private state.
        let pressure = vec![b'p'; crate::output::ARTIFACT_BYTES as usize];
        for index in 0..crate::output::RETAINED_BYTES.div_ceil(crate::output::ARTIFACT_BYTES) {
            let mut capture = store.capture(format!("quota-pressure-{index}"));
            capture.push(&pressure).unwrap();
            capture.finish().unwrap();
        }
        assert!(!retained.capture.metadata().unwrap().complete);
        assert!(
            retained
                .read(0, 127)
                .unwrap_err()
                .contains("no longer retained")
        );
        assert!(
            retained
                .read(data.len() as u64, 127)
                .unwrap_err()
                .contains("no longer retained")
        );
    }

    #[tokio::test]
    async fn aborted_capture_is_finalized_as_incomplete() {
        let store = OutputStore::global().unwrap();
        let job = completed_job(&store);
        {
            let mut state = job.state();
            state.stdout = Output::new(&store, "aborted-capture-test".into());
            state
                .stdout
                .append(&vec![b'x'; crate::output::INLINE_BYTES + 1]);
        }
        let mut collector = tokio::spawn(std::future::pending::<()>());
        collector.abort();
        drain(&mut collector, &job, "stdout").await;
        let snapshot = job.snapshot();
        assert_eq!(snapshot["stdout"]["eof"], true);
        assert_eq!(snapshot["stdout"]["artifact"]["complete"], false);
        assert!(
            snapshot["stdout"]["error"]
                .as_str()
                .unwrap()
                .contains("capture task failed")
        );
    }

    async fn start(shell: &Shell, command: &[&str], timeout_ms: u64) -> String {
        shell
            .call(
                "ssmcp.exec.start",
                json!({
                    "command":command,"timeout_ms":timeout_ms
                }),
            )
            .await
            .unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    const OUTPUT_START: &str = "SSMCP_SHELL_FIXTURE_OUTPUT\n";

    async fn start_fixture(shell: &Shell, fixture: &str, timeout_ms: u64) -> String {
        start_fixture_with_input(shell, fixture, timeout_ms, "null").await
    }

    async fn start_fixture_with_input(
        shell: &Shell,
        fixture: &str,
        timeout_ms: u64,
        stdin: &str,
    ) -> String {
        shell
            .call(
                "ssmcp.exec.start",
                json!({
                    "command":[std::env::current_exe().unwrap(),"--exact",
                        "backend::shell::tests::fixture_process","--nocapture"],
                    "env":{"SSMCP_SHELL_TEST_FIXTURE":fixture},
                    "timeout_ms":timeout_ms,"stdin":stdin
                }),
            )
            .await
            .unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn output(value: &Value) -> &str {
        value["stdout"]
            .as_str()
            .unwrap()
            .split_once(OUTPUT_START)
            .expect("the native command did not emit its output boundary")
            .1
    }

    // Re-exec the Rust test binary so the same process lifecycle regressions run
    // on Windows without relying on sh, sleep, printf or an external interpreter.
    #[test]
    fn fixture_process() {
        let Ok(fixture) = std::env::var("SSMCP_SHELL_TEST_FIXTURE") else {
            return;
        };
        if fixture == "stdin" {
            use std::io::Read;
            print!("{OUTPUT_START}");
            std::io::stdout().flush().unwrap();
            let mut bytes = [0u8; 4096];
            loop {
                let size = std::io::stdin().read(&mut bytes).unwrap();
                if size == 0 {
                    break;
                }
                std::io::stdout().write_all(&bytes[..size]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            std::process::exit(0);
        }
        let (stdout, stderr) = match fixture.as_str() {
            "alpha" | "beta" => {
                std::thread::sleep(Duration::from_millis(250));
                (fixture.clone(), format!("{fixture}-err"))
            }
            "retained" | "next" => (fixture, String::new()),
            "wait" => {
                std::thread::sleep(Duration::from_secs(60));
                (String::new(), String::new())
            }
            _ => panic!("unknown native command fixture"),
        };
        print!("{OUTPUT_START}{stdout}");
        eprint!("{stderr}");
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        // Avoid libtest's trailing report so bytes after the boundary are exact.
        std::process::exit(0);
    }

    #[tokio::test]
    async fn piped_stdin_preserves_unicode_binary_bytes_and_independent_eof() {
        let shell = Shell::new();
        let one = start_fixture_with_input(&shell, "stdin", 5000, "pipe").await;
        let two = start_fixture_with_input(&shell, "stdin", 5000, "pipe").await;
        let text = "你好，shell 🦀\n";
        let first = shell
            .call("ssmcp.exec.write", json!({"job_id":one,"data":text}))
            .await
            .unwrap();
        assert_eq!(first["written"], text.len());
        assert_eq!(first["closed"], false);
        let binary = [0, 255, 13, 10, 128];
        let second = shell
            .call(
                "ssmcp.exec.write",
                json!({"job_id":one,"data_base64":STANDARD.encode(binary),"close_stdin":true}),
            )
            .await
            .unwrap();
        assert_eq!(second["written"], binary.len());
        assert_eq!(second["closed"], true);
        assert_eq!(second["stdin"]["bytes_written"], text.len() + binary.len());
        assert_eq!(
            shell
                .call("ssmcp.exec.status", json!({"job_id":two}))
                .await
                .unwrap()["stdin"]["open"],
            true
        );
        shell
            .call(
                "ssmcp.exec.write",
                json!({"job_id":two,"data":"second","close_stdin":true}),
            )
            .await
            .unwrap();
        for job in [&one, &two] {
            let status = shell
                .call("ssmcp.exec.wait", json!({"job_id":job,"wait_ms":5000}))
                .await
                .unwrap();
            assert_eq!(status["exit_code"], 0, "{status}");
        }
        let out = shell
            .call("ssmcp.exec.read", json!({"job_id":one}))
            .await
            .unwrap();
        let out = STANDARD
            .decode(out["stdout_base64"].as_str().unwrap())
            .unwrap();
        let position = out
            .windows(OUTPUT_START.len())
            .position(|part| part == OUTPUT_START.as_bytes())
            .unwrap()
            + OUTPUT_START.len();
        let expected = [text.as_bytes(), &binary].concat();
        assert_eq!(&out[position..], expected.as_slice());
        let other = shell
            .call("ssmcp.exec.read", json!({"job_id":two}))
            .await
            .unwrap();
        assert_eq!(output(&other), "second");
        let again = shell
            .call("ssmcp.exec.write", json!({"job_id":one,"close_stdin":true}))
            .await
            .unwrap();
        assert_eq!(again["written"], 0);
        assert_eq!(again["closed"], true);
        assert!(
            shell
                .call("ssmcp.exec.write", json!({"job_id":one,"data":"late"}))
                .await
                .is_err()
        );
        shell.shutdown().await;
    }

    #[tokio::test]
    async fn default_stdin_delivers_eof_and_invalid_input_does_not_write() {
        let shell = Shell::new();
        let job = shell.call("ssmcp.exec.start",json!({
            "command":[std::env::current_exe().unwrap(),"--exact","backend::shell::tests::fixture_process","--nocapture"],
            "env":{"SSMCP_SHELL_TEST_FIXTURE":"stdin"},"timeout_ms":5000
        })).await.unwrap()["job_id"].as_str().unwrap().to_owned();
        let ended = shell
            .call("ssmcp.exec.wait", json!({"job_id":job,"wait_ms":5000}))
            .await
            .unwrap();
        assert_eq!(ended["exit_code"], 0);
        assert_eq!(
            ended["stdin"],
            json!({"mode":"null","open":false,"bytes_written":0})
        );
        assert!(
            shell
                .call("ssmcp.exec.write", json!({"job_id":job,"data":"disabled"}))
                .await
                .is_err()
        );

        let job = start_fixture_with_input(&shell, "stdin", 5000, "pipe").await;
        for mut args in [
            json!({"data":"x","data_base64":"eA=="}),
            json!({"data_base64":"@@@="}),
            json!({"data":"界".repeat(MAX_INPUT / 3 + 1)}),
            json!({"data_base64":STANDARD.encode(vec![0;MAX_INPUT+1])}),
            json!({"data":"x","wait_ms":MAX_INPUT_WAIT+1}),
            json!({}),
        ] {
            args["job_id"] = json!(job);
            assert!(shell.call("ssmcp.exec.write", args).await.is_err());
        }
        assert_eq!(
            shell
                .call("ssmcp.exec.status", json!({"job_id":job}))
                .await
                .unwrap()["stdin"]["bytes_written"],
            0
        );
        shell
            .call("ssmcp.exec.write", json!({"job_id":job,"close_stdin":true}))
            .await
            .unwrap();
        assert_eq!(
            shell
                .call("ssmcp.exec.wait", json!({"job_id":job,"wait_ms":5000}))
                .await
                .unwrap()["exit_code"],
            0
        );
        shell.shutdown().await;
    }

    type StdinWriter<'a> =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, String>> + 'a>>;

    async fn poll_once<F: std::future::Future + ?Sized>(
        mut future: std::pin::Pin<&mut F>,
    ) -> std::task::Poll<F::Output> {
        std::future::poll_fn(|cx| std::task::Poll::Ready(future.as_mut().poll(cx))).await
    }

    async fn pending_stdin_writer<'a>(shell: &'a Shell, id: &str) -> StdinWriter<'a> {
        let job = shell.job(id).unwrap();
        for _ in 0..64 {
            let mut writer: StdinWriter<'a> = Box::pin(shell.call(
                "ssmcp.exec.write",
                json!({"job_id":id,"data":"x".repeat(MAX_INPUT),"wait_ms":5000}),
            ));
            loop {
                match poll_once(writer.as_mut()).await {
                    std::task::Poll::Ready(result) => {
                        let result = result.unwrap();
                        assert_eq!(result["closed"], false);
                        assert_eq!(result["timed_out"], false);
                        assert_eq!(result["written"], MAX_INPUT);
                        assert_eq!(result["remaining_bytes"], 0);
                        break;
                    }
                    std::task::Poll::Pending => {
                        assert!(job.state().running);
                        assert!(!*job.input_stopped.borrow());
                        // Pending can mean cooperative scheduling, or Windows'
                        // buffered background write. Only retain a request that
                        // has acquired stdin, and keep that exact future alive:
                        // a timed-out request says nothing about the next one.
                        if job.stdin.try_lock().is_err() {
                            return writer;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        panic!("the non-reading fixture must eventually suspend a stdin writer");
    }

    async fn wait_for_input_stop(job: &Job) {
        let mut stopped = job.input_stopped.subscribe();
        timeout(Duration::from_secs(3), stopped.wait_for(|stopped| *stopped))
            .await
            .expect("the supervisor must stop input before awaiting the held stdin lock")
            .unwrap();
    }

    #[tokio::test]
    async fn cancelling_a_job_interrupts_blocked_and_queued_stdin_writers() {
        let shell = Shell::new();
        let job = start_fixture_with_input(&shell, "wait", 10000, "pipe").await;
        let state = shell.job(&job).unwrap();
        let blocked = pending_stdin_writer(&shell, &job).await;
        let queued = shell.call(
            "ssmcp.exec.write",
            json!({"job_id":job,"data":"queued","wait_ms":5000}),
        );
        tokio::pin!(queued);
        assert!(poll_once(queued.as_mut()).await.is_pending());
        assert!(state.stdin.try_lock().is_err());

        let cancelled = shell.call("ssmcp.exec.cancel", json!({"job_id":job}));
        tokio::pin!(cancelled);
        assert!(poll_once(cancelled.as_mut()).await.is_pending());
        // Retain both pending futures and the writer's lock until the
        // supervisor publishes the stop. This exercises the cancellation
        // ordering without relying on pipe capacity or a millisecond race.
        wait_for_input_stop(&state).await;
        let (cancelled, blocked, queued) = timeout(Duration::from_secs(3), async {
            tokio::join!(cancelled, blocked, queued)
        })
        .await
        .expect("stdin backpressure must not stall job cancellation");
        assert_eq!(cancelled.unwrap()["cancelled"], true);
        assert_eq!(blocked.unwrap()["closed"], true);
        assert_eq!(queued.unwrap()["closed"], true);
        shell.shutdown().await;
    }

    #[tokio::test]
    async fn an_execution_deadline_interrupts_a_blocked_stdin_writer() {
        let shell = Shell::new();
        let job = start_fixture_with_input(&shell, "wait", 500, "pipe").await;
        let writer = pending_stdin_writer(&shell, &job).await;
        // Keep the actual pending request alive across the execution deadline.
        // In particular, do not discard a short write timeout and assume that
        // a subsequent write must block on Windows' asynchronous pipe buffer.
        wait_for_input_stop(&shell.job(&job).unwrap()).await;
        let result = timeout(Duration::from_secs(3), writer)
            .await
            .expect("execution deadline must wake stdin writer")
            .unwrap();
        assert_eq!(result["closed"], true);
        let status = shell
            .call("ssmcp.exec.wait", json!({"job_id":job,"wait_ms":3000}))
            .await
            .unwrap();
        assert_eq!(status["timed_out"], true);
        assert_eq!(status["stdin"]["open"], false);
        shell.shutdown().await;
    }

    #[tokio::test]
    async fn independent_jobs_run_concurrently_and_keep_streams_separate() {
        let shell = Shell::new();
        let a = start_fixture(&shell, "alpha", 5000).await;
        let b = start_fixture(&shell, "beta", 5000).await;
        let status = shell
            .call("ssmcp.exec.status", json!({"job_id":a}))
            .await
            .unwrap();
        assert_eq!(status["running"], true);
        shell
            .call("ssmcp.exec.wait", json!({"job_id":a,"wait_ms":5000}))
            .await
            .unwrap();
        shell
            .call("ssmcp.exec.wait", json!({"job_id":b,"wait_ms":5000}))
            .await
            .unwrap();
        let first = shell
            .call("ssmcp.exec.read", json!({"job_id":a}))
            .await
            .unwrap();
        let second = shell
            .call("ssmcp.exec.read", json!({"job_id":b}))
            .await
            .unwrap();
        assert_eq!(output(&first), "alpha");
        assert_eq!(first["stderr"], "alpha-err");
        assert_eq!(output(&second), "beta");
        assert_eq!(second["stderr"], "beta-err");
        assert_eq!(
            shell
                .call("ssmcp.exec.status", json!({"job_id":a}))
                .await
                .unwrap()["exit_code"],
            0
        );
        shell.shutdown().await;
    }

    #[tokio::test]
    async fn jobs_support_timeout_cancellation_and_client_isolation() {
        let one = Shell::new();
        let two = Shell::new();
        let timeout_job = start_fixture(&one, "wait", 100).await;
        let timed = one
            .call(
                "ssmcp.exec.wait",
                json!({"job_id":timeout_job,"wait_ms":5000}),
            )
            .await
            .unwrap();
        assert_eq!(timed["timed_out"], true);
        let cancel_job = start_fixture(&one, "wait", 5000).await;
        assert!(
            two.call("ssmcp.exec.status", json!({"job_id":cancel_job}))
                .await
                .is_err()
        );
        let cancelled = one
            .call("ssmcp.exec.cancel", json!({"job_id":cancel_job}))
            .await
            .unwrap();
        assert_eq!(cancelled["cancelled"], true);
        one.shutdown().await;
        two.shutdown().await;
    }
    fn python() -> &'static str {
        if cfg!(windows) { "python" } else { "python3" }
    }
    fn newline() -> &'static str {
        if cfg!(windows) { "\r\n" } else { "\n" }
    }
    async fn py(shell: &Shell, code: &str) -> String {
        start(shell, &[python(), "-u", "-c", code], 0).await
    }
    #[tokio::test]
    async fn native_completion_wait_is_targeted_and_droppable() {
        let shell = Shell::new();
        let other_connection = Shell::new();
        let job_id = start(
            &shell,
            &[python(), "-u", "-c", "import time; time.sleep(10)"],
            900_001,
        )
        .await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                shell.call("ssmcp.exec.wait", json!({"job_id":job_id})),
            )
            .await
            .is_err()
        );
        let wait = shell.call("ssmcp.exec.wait", json!({"job_id":job_id}));
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        assert!(
            other_connection
                .call("ssmcp.exec.wait", json!({"job_id":job_id}))
                .await
                .is_err()
        );
        let sibling = py(&shell, "print('unrelated')").await;
        assert_eq!(
            shell
                .call("ssmcp.exec.wait", json!({"job_id":sibling}))
                .await
                .unwrap()["exit_code"],
            0
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        let zero = shell
            .call("ssmcp.exec.wait", json!({"job_id":job_id,"wait_ms":0}))
            .await
            .unwrap();
        assert_eq!(zero["wait_timed_out"], true);
        shell
            .call("ssmcp.exec.cancel", json!({"job_id":job_id}))
            .await
            .unwrap();
        let result = wait.await.unwrap();
        assert_eq!(result["cancelled"], true);
        assert_eq!(result["wait_timed_out"], false);
        let already = shell
            .call("ssmcp.exec.wait", json!({"job_id":job_id}))
            .await
            .unwrap();
        assert_eq!(already["cancelled"], true);
    }

    #[tokio::test]
    #[ignore = "real 61-second or 305-second command observed through windows of at most 55 seconds"]
    async fn native_completion_observations_stay_bounded_beyond_sixty_seconds() {
        let shell = Shell::new();
        let seconds = match std::env::var("SSMCP_TEST_NATIVE_WAIT_SECONDS").as_deref() {
            Ok("305") => 305,
            Err(std::env::VarError::NotPresent) => 61,
            _ => panic!("set SSMCP_TEST_NATIVE_WAIT_SECONDS to 305 or leave it unset"),
        };
        let code = format!("import time; time.sleep({seconds}); print('long wait complete')");
        let id = py(&shell, &code).await;
        let started = Instant::now();
        let mut windows = 0;
        let result = loop {
            windows += 1;
            let result = tokio::time::timeout(
                Duration::from_millis(MAX_READ_WAIT + 5000),
                shell.call("ssmcp.exec.wait", json!({"job_id":id})),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(result["job_id"], id);
            if result["running"] == false {
                break result;
            }
            assert_eq!(result["wait_status"], "pending");
            assert_eq!(result["wait_timed_out"], true);
            assert_eq!(result["timeout_ms"], 0);
        };
        assert!(windows >= 2);
        assert!(started.elapsed() > Duration::from_secs(seconds - 1));
        assert_eq!(result["running"], false);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["wait_timed_out"], false);
    }

    #[tokio::test]
    #[ignore = "real 61-second command with multiple short observation windows"]
    async fn native_completion_windows_resume_the_same_job() {
        let shell = Shell::new();
        let id = py(&shell, "import time; time.sleep(61); print('once')").await;
        let started = Instant::now();
        for _ in 0..3 {
            let pending = shell
                .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":20_000}))
                .await
                .unwrap();
            assert_eq!(pending["job_id"], id);
            assert_eq!(pending["wait_status"], "pending");
            assert_eq!(pending["wait_timed_out"], true);
            assert_eq!(pending["running"], true);
        }
        // Deliberately let the process complete while there is no outstanding wait.
        tokio::time::sleep_until(started + Duration::from_secs(62)).await;
        let next = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":20_000}))
            .await
            .unwrap();
        assert_eq!(next["job_id"], id);
        assert_eq!(next["wait_status"], "ready");
        assert_eq!(next["wait_timed_out"], false);
        assert_eq!(next["exit_code"], 0);
        let output = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(output["stdout"], format!("once{}", newline()));
        let repeated = shell
            .call("ssmcp.exec.wait", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(repeated["job_id"], id);
        assert_eq!(repeated["exit_code"], 0);
        assert!(started.elapsed() > Duration::from_secs(60));
    }

    #[tokio::test]
    async fn waits_keep_work_running_and_completed_jobs_remain_available() {
        let shell = Shell::new();
        let id = py(&shell, "import time; time.sleep(.3); print('retained')").await;
        let wait = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":1}))
            .await
            .unwrap();
        assert_eq!(wait["running"], true);
        assert_eq!(wait["wait_timed_out"], true);
        assert!(
            shell
                .call("ssmcp.exec.forget", json!({"job_id":id}))
                .await
                .is_err()
        );
        shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        let next = py(&shell, "print('next')").await;
        let first = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(first["stdout"], format!("retained{}", newline()));
        assert_eq!(first["exit_code"], 0);
        assert_eq!(first["snapshot"]["state"], "completed");
        assert_eq!(
            shell
                .call("ssmcp.exec.forget", json!({"job_id":id}))
                .await
                .unwrap()["forgotten"],
            true
        );
        assert!(
            shell
                .call("ssmcp.exec.status", json!({"job_id":id}))
                .await
                .is_err()
        );
        shell
            .call("ssmcp.exec.wait", json!({"job_id":next,"wait_ms":5000}))
            .await
            .unwrap();
        shell.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_job_releases_capacity_before_reporting_finished() {
        let store = isolated_output();
        let shell = Shell::with_workspace(std::env::current_dir().unwrap(), store).unwrap();
        let occupied = shell
            .slots
            .clone()
            .acquire_many_owned((MAX_JOBS - 1) as u32)
            .await
            .unwrap();
        let id = start_fixture_with_input(&shell, "stdin", 15000, "pipe").await;
        // Consume the fixture's initial output before taking the observation
        // lock. Closing its stdin then exits without emitting any further bytes.
        timeout(Duration::from_secs(5), async {
            let mut stdout = String::new();
            let mut cursor = 0;
            loop {
                let page = shell
                    .call(
                        "ssmcp.exec.read",
                        json!({"job_id":id,"stdout_cursor":cursor,"wait_ms":100}),
                    )
                    .await
                    .unwrap();
                stdout.push_str(page["stdout"].as_str().unwrap());
                cursor = page["stdout_cursor"].as_u64().unwrap();
                if stdout.contains(OUTPUT_START) {
                    break;
                }
            }
        })
        .await
        .expect("stdin fixture must be ready before holding its observation lock");

        let job = shell.job(&id).unwrap();
        let slots = shell.slots.clone();
        let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
        let (locked, acquired) = tokio::sync::oneshot::channel();
        let (finished, completion) = tokio::sync::oneshot::channel();
        // This OS thread observes and releases the gate itself. It never needs
        // a blocked Tokio worker or timer to advance before dropping the mutex.
        let observer = std::thread::spawn(move || {
            let result = {
                let _observation = job.observation.lock().unwrap();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                let _ = locked.send(());
                loop {
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    if remaining.is_zero() {
                        break Err("observation gate reached its independent cleanup deadline");
                    }
                    // snapshot() is also used by the public exec.status path.
                    let state = job.snapshot();
                    if state["running"] == false {
                        break Ok((state, slots.available_permits()));
                    }
                    // Dropping the test future closes this channel and releases
                    // the guard even when the async path panics or is cancelled.
                    match cancelled.recv_timeout(remaining.min(Duration::from_millis(1))) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            break Err("observation gate was cancelled");
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            };
            // Publish only after the observation mutex guard has been dropped.
            let _ = finished.send(());
            result
        });
        timeout(Duration::from_secs(5), acquired)
            .await
            .expect("observation gate must acquire the existing mutex")
            .unwrap();
        shell
            .call("ssmcp.exec.write", json!({"job_id":id,"close_stdin":true}))
            .await
            .unwrap();
        completion.await.expect("observation gate thread stopped");
        let observed = observer.join().expect("observation gate thread failed");
        drop(cancel);

        // The gate is now open. Public status and a replacement native process
        // run without deliberately blocking any runtime worker.
        let public_state = shell.call("ssmcp.exec.status", json!({"job_id":id})).await;
        let replacement = if observed.is_ok() {
            Some(
                shell
                    .call(
                        "ssmcp.exec.start",
                        json!({
                            "command":[std::env::current_exe().unwrap(),"--exact",
                                "backend::shell::tests::fixture_process","--nocapture"],
                            "env":{"SSMCP_SHELL_TEST_FIXTURE":"next"},
                            "timeout_ms":5000
                        }),
                    )
                    .await,
            )
        } else {
            None
        };
        // Clean up every owned process and permit before checking the results.
        drop(occupied);
        shell.shutdown().await;

        let (completed, available_permits) = observed.expect("observation gate failed");
        assert_eq!(completed["exit_code"], 0);
        assert!(completed["error"].is_null(), "{completed}");
        assert_eq!(public_state.unwrap()["running"], false);
        assert_eq!(
            available_permits, 1,
            "a job reported complete while its execution permit was still held"
        );
        let replacement = replacement.unwrap();
        assert!(
            replacement.is_ok(),
            "the released execution slot must accept a new command: {replacement:?}"
        );
    }

    #[tokio::test]
    async fn rejected_capacity_never_executes_the_command() {
        let shell = Shell::new();
        let occupied = shell
            .slots
            .clone()
            .acquire_many_owned(MAX_JOBS as u32)
            .await
            .unwrap();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-reject-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        let command = vec![
            python().to_owned(),
            "-c".into(),
            "import pathlib,sys; pathlib.Path(sys.argv[1]).write_text('ran')".into(),
            path.to_string_lossy().into_owned(),
        ];
        assert!(
            shell
                .call("ssmcp.exec.start", json!({"command":command}))
                .await
                .is_err()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !path.exists(),
            "a rejected start must have no command side effects"
        );
        drop(occupied);
        let id = py(&shell, "print('slot released')").await;
        shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        shell.shutdown().await;
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_read_status_wait_never_deadlock_and_output_is_exact() {
        let shell = Shell::new();
        let id = py(&shell, "import sys,time\nfor i in range(40):\n print(i); print(i,file=sys.stderr); time.sleep(.002)").await;
        timeout(Duration::from_secs(10), async {
            let reader = async {
                let (mut out,mut err)=(0,0);
                loop {
                    let page=shell.call("ssmcp.exec.read",json!({"job_id":id,"stdout_cursor":out,"stderr_cursor":err,"wait_ms":20})).await.unwrap();
                    out=page["stdout_cursor"].as_u64().unwrap(); err=page["stderr_cursor"].as_u64().unwrap();
                    if page["stdout_eof"]==true && page["stderr_eof"]==true { break; }
                }
            };
            let states=async { for _ in 0..100 { shell.call("ssmcp.exec.status",json!({"job_id":id})).await.unwrap(); tokio::task::yield_now().await; } };
            let waiter=shell.call("ssmcp.exec.wait",json!({"job_id":id,"wait_ms":5000}));
            let (_,_,done)=tokio::join!(reader,states,waiter);
            assert_eq!(done.unwrap()["exit_code"],0);
        }).await.expect("read/status/wait must make progress concurrently");
        let all = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        let expected = (0..40)
            .map(|n| format!("{n}{}", newline()))
            .collect::<String>();
        assert_eq!(all["stdout"], expected);
        assert_eq!(all["stderr"], expected);
        shell.shutdown().await;
    }
    #[tokio::test]
    async fn spilled_output_remains_live_before_completion_and_is_not_replayed() {
        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let shell = Shell::new();
        let script = format!(
            r#"import socket, sys
with socket.create_connection(("127.0.0.1", {port}), timeout=5) as gate:
    gate.settimeout(None)
    sys.stdout.write("line\n" * 600)
    sys.stdout.flush()
    gate.recv(1)
    print("AFTER_SPILL_MARKER", flush=True)
    gate.recv(1)
"#
        );
        let id = py(&shell, &script).await;
        let (mut gate, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("output fixture did not connect to its parent")
            .unwrap();
        drop(listener);
        // Observe the spill before permitting the marker, so pipe coalescing
        // cannot accidentally turn this into a pre-spill delivery test.
        timeout(Duration::from_secs(5), async {
            loop {
                let state = shell
                    .call("ssmcp.exec.status", json!({"job_id":id}))
                    .await
                    .unwrap();
                if state["stdout"]["artifact"].is_object() {
                    assert_eq!(state["running"], true);
                    assert!(state["stdout"]["artifact"]["lines"].as_u64().unwrap() >= 600);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("output fixture did not spill before its marker");
        timeout(Duration::from_secs(5), gate.write_all(b"\x01"))
            .await
            .expect("parent could not permit the post-spill marker")
            .unwrap();
        // Keep the child alive until the parent has observed live output.
        // Dropping this owned socket on failure also releases the child's recv.
        let mut cursor = 0;
        let mut markers = 0;
        timeout(Duration::from_secs(5), async {
            loop {
                let page = shell
                    .call(
                        "ssmcp.exec.events",
                        json!({"cursor":cursor,"max_events":256,"wait_ms":100}),
                    )
                    .await
                    .unwrap();
                cursor = page["next_cursor"].as_u64().unwrap();
                for record in page["events"].as_array().unwrap() {
                    let event = &record["event"];
                    if event["method"] == "command/exec/outputDelta"
                        && let Some(encoded) = event["params"]["deltaBase64"].as_str()
                    {
                        let bytes = STANDARD.decode(encoded).unwrap();
                        markers += String::from_utf8_lossy(&bytes)
                            .matches("AFTER_SPILL_MARKER")
                            .count();
                    }
                }
                if markers != 0 {
                    break;
                }
            }
        })
        .await
        .expect("post-spill output was not delivered live");
        assert_eq!(markers, 1);
        assert_eq!(
            shell
                .call("ssmcp.exec.status", json!({"job_id":id}))
                .await
                .unwrap()["running"],
            true
        );
        timeout(Duration::from_secs(5), gate.write_all(b"\x01"))
            .await
            .expect("parent could not release the output fixture")
            .unwrap();
        drop(gate);
        let finished = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        assert_eq!(finished["running"], false);
        assert_eq!(finished["exit_code"], 0);
        shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        let remaining = shell
            .call(
                "ssmcp.exec.events",
                json!({"cursor":cursor,"max_events":256}),
            )
            .await
            .unwrap();
        for record in remaining["events"].as_array().unwrap() {
            if let Some(encoded) = record["event"]["params"]["deltaBase64"].as_str() {
                assert!(
                    !String::from_utf8_lossy(&STANDARD.decode(encoded).unwrap())
                        .contains("AFTER_SPILL_MARKER")
                );
            }
        }
        shell.shutdown().await;
    }

    #[tokio::test]
    async fn execution_cwd_is_bound_to_the_authorized_workspace() {
        let base = std::env::temp_dir().join(format!(
            "ssmcp-exec-root-{}-{}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let shell =
            Shell::with_workspace(root.clone(), OutputStore::new_connection().unwrap()).unwrap();
        let command = json!([python(), "-u", "-c", "import os; print(os.getcwd())"]);
        for cwd in [outside.to_string_lossy().into_owned(), "../outside".into()] {
            assert!(
                shell
                    .call("ssmcp.exec.start", json!({"command":command,"cwd":cwd}))
                    .await
                    .is_err()
            );
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            assert!(
                shell
                    .call(
                        "ssmcp.exec.start",
                        json!({"command":command,"cwd":"escape"})
                    )
                    .await
                    .is_err()
            );
        }
        let job = shell
            .call(
                "ssmcp.exec.start",
                json!({"command":command,"cwd":"nested"}),
            )
            .await
            .unwrap();
        let id = job["job_id"].as_str().unwrap();
        assert_eq!(
            job["cwd"],
            json!(root.join("nested").canonicalize().unwrap())
        );
        shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        let result = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(
            std::path::Path::new(result["stdout"].as_str().unwrap().trim())
                .canonicalize()
                .unwrap(),
            root.join("nested").canonicalize().unwrap()
        );
        shell.shutdown().await;
        std::fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn large_output_keeps_the_full_artifact_after_releasing_the_live_tail() {
        let shell = Shell::new();
        let id = py(&shell, "import sys; sys.stdout.buffer.write(b'x'*100000)").await;
        // wait_ms is an observation window, not a guarantee of command completion.
        // Stop at the terminal job state; never poll a failed artifact until it passes.
        let mut finished = Value::Null;
        let observed = timeout(Duration::from_secs(15), async {
            loop {
                finished = shell
                    .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":250}))
                    .await
                    .unwrap();
                if finished["running"] == false {
                    break;
                }
            }
        })
        .await;
        if observed.is_err() {
            shell.shutdown().await;
            panic!("large-output fixture did not finish; last observation: {finished}");
        }
        assert_eq!(finished["wait_timed_out"], false, "{finished}");
        assert_eq!(finished["exit_code"], 0, "{finished}");
        assert!(finished["error"].is_null(), "{finished}");
        assert!(finished["stdout"]["error"].is_null(), "{finished}");
        assert_eq!(finished["stdout"]["eof"], true, "{finished}");
        let page = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(page["stdout_truncated"], false);
        assert_eq!(page["stdout_cursor"], 16384);
        assert_eq!(page["stdout_eof"], false);
        assert_eq!(page["stdout"], "x".repeat(16384));
        assert_eq!(shell.job(&id).unwrap().state().stdout.bytes.capacity(), 0);
        let artifact = &page["snapshot"]["stdout"]["artifact"];
        assert_eq!(artifact["bytes"], 100000);
        assert_eq!(artifact["complete"], true, "snapshot: {}", page["snapshot"]);
        let store = shell.output.clone();
        let saved = store
            .perform(
                "ssmcp.output.read",
                json!({"id":artifact["id"],"offset":0,"max_bytes":32768}),
            )
            .await
            .unwrap();
        let bytes = STANDARD
            .decode(saved["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, vec![b'x'; 32768]);
        assert!(
            shell
                .call(
                    "ssmcp.exec.read",
                    json!({"job_id":id,"stdout_cursor":100001})
                )
                .await
                .is_err()
        );
        shell.shutdown().await;
    }
    #[tokio::test]
    async fn parallel_commands_finish_with_distinct_streams_and_cancel_is_idempotent() {
        let shell = Shell::new();
        let (a, b) = tokio::join!(
            py(&shell, "import time; time.sleep(.15); print('alpha')"),
            py(&shell, "import time; time.sleep(.15); print('beta')")
        );
        assert_ne!(a, b);
        let (left, right) = tokio::join!(
            shell.call("ssmcp.exec.wait", json!({"job_id":a,"wait_ms":5000})),
            shell.call("ssmcp.exec.wait", json!({"job_id":b,"wait_ms":5000}))
        );
        assert_eq!(left.unwrap()["exit_code"], 0);
        assert_eq!(right.unwrap()["exit_code"], 0);
        assert_eq!(
            shell
                .call("ssmcp.exec.read", json!({"job_id":a}))
                .await
                .unwrap()["stdout"],
            format!("alpha{}", newline())
        );
        assert_eq!(
            shell
                .call("ssmcp.exec.read", json!({"job_id":b}))
                .await
                .unwrap()["stdout"],
            format!("beta{}", newline())
        );
        let slow = py(&shell, "import time; time.sleep(10)").await;
        let one = shell
            .call("ssmcp.exec.cancel", json!({"job_id":slow}))
            .await
            .unwrap();
        let two = shell
            .call("ssmcp.exec.cancel", json!({"job_id":slow}))
            .await
            .unwrap();
        assert_eq!(one["cancelled"], true);
        assert_eq!(two["running"], false);
        assert!(
            shell
                .call("ssmcp.exec.list", json!({"unexpected":true}))
                .await
                .is_err()
        );
        shell.shutdown().await;
        assert!(
            shell
                .call(
                    "ssmcp.exec.start",
                    json!({"command":[python(),"-c","print(1)"]})
                )
                .await
                .is_err()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn native_signal_and_process_identity_are_explicit_in_status() {
        let shell = Shell::new();
        let id = py(
            &shell,
            "import os,signal; os.kill(os.getpid(),signal.SIGTERM)",
        )
        .await;
        let done = shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        assert_eq!(done["running"], false);
        assert!(done["exit_code"].is_null());
        assert_eq!(done["exit_signal"], libc::SIGTERM);
        assert_eq!(done["pid_role"], PID_ROLE);
        shell.shutdown().await;
    }
}
