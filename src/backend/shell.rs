//! Independent command jobs with bounded live output and durable output artifacts.
use super::{events::EventLog, process::Managed};
use crate::output::{Capture, OutputStore};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Notify, Semaphore, watch},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};

const MAX_JOBS: usize = 32;
const MAX_RETAINED: usize = 128;
const MAX_OUTPUT: usize = 64 * 1024;
const MAX_COMMAND_ARGS: usize = 256;
const MAX_WAIT: u64 = 60_000;
const MAX_TIMEOUT: u64 = 900_000;
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
    eof: bool,
    capture: Capture,
}
impl Output {
    fn new(store: &Arc<OutputStore>, scope: String) -> Self {
        Self {
            bytes: VecDeque::new(),
            start: 0,
            end: 0,
            eof: false,
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
        let _ = self.capture.finish();
        self.eof = true;
    }
}
struct State {
    running: bool,
    exit_code: Option<i32>,
    cancelled: bool,
    timed_out: bool,
    error: Option<String>,
    finished: Option<Instant>,
    stdout: Output,
    stderr: Output,
}
struct Job {
    id: String,
    pid: u32,
    command: Vec<String>,
    cwd: PathBuf,
    started: Instant,
    state: Mutex<State>,
    changed: Notify,
    cancel: watch::Sender<bool>,
}
impl Job {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn snapshot_locked(&self, state: &State) -> Value {
        json!({"job_id":self.id,"id":self.id,"pid":self.pid,"command":self.command,"cwd":self.cwd,
            "state":if state.running {"running"} else if state.cancelled {"cancelled"} else if state.timed_out {"timed_out"} else if state.error.is_some() {"failed"} else {"completed"},
            "running":state.running,"exit_code":state.exit_code,"cancelled":state.cancelled,
            "timed_out":state.timed_out,"error":state.error,
            "duration_ms":state.finished.unwrap_or_else(Instant::now).duration_since(self.started).as_millis() as u64,
            "stdout":state.stdout.metadata(),"stderr":state.stderr.metadata()})
    }
    fn snapshot(&self) -> Value {
        self.snapshot_locked(&self.state())
    }
}
#[derive(Default)]
struct Registry {
    jobs: HashMap<String, Arc<Job>>,
    closed: bool,
}
pub struct Shell {
    registry: Mutex<Registry>,
    slots: Arc<Semaphore>,
    events: Arc<EventLog>,
}
impl Shell {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            registry: Mutex::new(Registry::default()),
            slots: Arc::new(Semaphore::new(MAX_JOBS)),
            events: EventLog::new("shell"),
        })
    }
    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn audit(&self, event: Value) {
        self.events.push(event);
    }

    pub fn tools() -> Vec<Value> {
        let id = json!({"job_id":{"type":"string","minLength":1,"maxLength":128}});
        vec![
            tool(
                "ssmcp.exec.start",
                "Start an independent argv command immediately and return job_id. Commands run concurrently, survive code_mode completion, and stop when this client disconnects or reloads. No model is invoked.",
                json!({"command":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_COMMAND_ARGS},
                    "cwd":{"type":"string","description":"Existing absolute directory; defaults to the client process working directory."},
                    "env":{"type":"object","additionalProperties":{"type":"string"}},
                    "timeout_ms":{"type":"integer","minimum":0,"maximum":MAX_TIMEOUT,"default":0,"description":"0 means no execution deadline. A nonzero deadline terminates the command."}}),
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
                "Wait for completion for up to 60 seconds. A wait deadline leaves the command running; reuse job_id to wait again.",
                json!({"job_id":{"type":"string"},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT,"default":5000}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.read",
                "Read incremental stdout/stderr and an atomic status snapshot. Each stream has its own byte cursor; base64 preserves exact bytes. Large output is retained in artifacts accessible through outputs.read.",
                json!({"job_id":{"type":"string"},"stdout_cursor":{"type":"integer","minimum":0,"default":0},
                    "stderr_cursor":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":32768,"default":16384},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT,"default":0}}),
                &["job_id"],
                true,
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
        match name {
            "ssmcp.exec.start" => self.start(args),
            "ssmcp.exec.list" => {
                empty(args)?;
                let jobs: Vec<_> = self.registry().jobs.values().cloned().collect();
                let mut values: Vec<_> = jobs.iter().map(|job| job.snapshot()).collect();
                values.sort_by(|a, b| a["job_id"].as_str().cmp(&b["job_id"].as_str()));
                Ok(json!({"jobs":values,"max_running":MAX_JOBS,"max_retained":MAX_RETAINED}))
            }
            "ssmcp.exec.status" => {
                let args: Id = parse(args)?;
                Ok(self.job(&args.job_id)?.snapshot())
            }
            "ssmcp.exec.wait" => {
                let args: Wait = parse(args)?;
                check_wait(args.wait_ms)?;
                let job = self.job(&args.job_id)?;
                wait_finished(&job, args.wait_ms).await;
                let mut result = job.snapshot();
                result["wait_timed_out"] = result["running"].clone();
                Ok(result)
            }
            "ssmcp.exec.read" => {
                let args: Read = parse(args)?;
                check_wait(args.wait_ms)?;
                if !(1..=32768).contains(&args.max_bytes) {
                    return Err("max_bytes must be 1..=32768".into());
                }
                let job = self.job(&args.job_id)?;
                wait_output(&job, args.stdout_cursor, args.stderr_cursor, args.wait_ms).await?;
                let state = job.state();
                let (out, on, ot) = state.stdout.read(args.stdout_cursor, args.max_bytes)?;
                let (err, en, et) = state.stderr.read(args.stderr_cursor, args.max_bytes)?;
                Ok(
                    json!({"job_id":job.id,"snapshot":job.snapshot_locked(&state),"running":state.running,
                    "exit_code":state.exit_code,"stdout":String::from_utf8_lossy(&out),"stderr":String::from_utf8_lossy(&err),
                    "stdout_base64":STANDARD.encode(&out),"stderr_base64":STANDARD.encode(&err),
                    "stdout_cursor":on,"stderr_cursor":en,"stdout_truncated":ot,"stderr_truncated":et,
                    "stdout_eof":state.stdout.eof && on==state.stdout.end,"stderr_eof":state.stderr.eof && en==state.stderr.end}),
                )
            }
            "ssmcp.exec.cancel" => {
                let args: Id = parse(args)?;
                let job = self.job(&args.job_id)?;
                if job.state().running {
                    job.cancel.send_replace(true);
                }
                wait_finished(&job, 8000).await;
                Ok(job.snapshot())
            }
            "ssmcp.exec.forget" => {
                let args: Id = parse(args)?;
                let mut registry = self.registry();
                if registry
                    .jobs
                    .get(&args.job_id)
                    .is_some_and(|job| job.state().running)
                {
                    return Err("job is running; cancel it before forgetting".into());
                }
                Ok(
                    json!({"job_id":args.job_id,"forgotten":registry.jobs.remove(&args.job_id).is_some()}),
                )
            }
            "ssmcp.exec.events" => self.events.read(args).await,
            _ => Err(format!("unknown shell execution capability: {name}")),
        }
    }
    fn job(&self, id: &str) -> Result<Arc<Job>, String> {
        if id.is_empty() || id.len() > 128 {
            return Err("invalid job_id".into());
        }
        self.registry()
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown or expired background job; use exec.list(clientId)".into())
    }
    fn start(&self, args: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Start {
            command: Vec<String>,
            cwd: Option<PathBuf>,
            env: Option<HashMap<String, String>>,
            #[serde(default)]
            timeout_ms: u64,
        }
        let args: Start = parse(args)?;
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
        if args.timeout_ms > MAX_TIMEOUT {
            return Err(format!("timeout_ms must be 0..={MAX_TIMEOUT}"));
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
        let cwd = args
            .cwd
            .unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
        if !cwd.is_absolute() || !cwd.is_dir() {
            return Err("cwd must be an existing absolute directory".into());
        }
        let store = OutputStore::global()?;
        // Reserve capacity before spawning: rejected requests must never run commands.
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            format!("at most {MAX_JOBS} commands may run at once; wait or cancel an existing job")
        })?;
        let mut registry = self.registry();
        if registry.closed {
            return Err("shell command manager is closed".into());
        }
        let evicted = if registry.jobs.len() >= MAX_RETAINED {
            registry
                .jobs
                .values()
                .filter(|job| !job.state().running)
                .min_by_key(|job| job.started)
                .map(|job| job.id.clone())
        } else {
            None
        };
        let mut command = Command::new(&args.command[0]);
        command
            .args(&args.command[1..])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(env) = args.env {
            command.envs(env);
        }
        let mut process =
            Managed::spawn(&mut command).map_err(|e| format!("could not start command: {e}"))?;
        let pid = process.child.id().ok_or("command has no process ID")?;
        let stdout = process
            .child
            .stdout
            .take()
            .ok_or("command stdout unavailable")?;
        let stderr = process
            .child
            .stderr
            .take()
            .ok_or("command stderr unavailable")?;
        let id = format!(
            "{}-{:x}",
            std::process::id(),
            NEXT_JOB.fetch_add(1, Ordering::Relaxed)
        );
        let (cancel, cancel_rx) = watch::channel(false);
        let job = Arc::new(Job {
            id: id.clone(),
            pid,
            command: args.command,
            cwd,
            started: Instant::now(),
            state: Mutex::new(State {
                running: true,
                exit_code: None,
                cancelled: false,
                timed_out: false,
                error: None,
                finished: None,
                stdout: Output::new(&store, format!("job:{id}:stdout")),
                stderr: Output::new(&store, format!("job:{id}:stderr")),
            }),
            changed: Notify::new(),
            cancel,
        });
        // A failed spawn must leave the retained history untouched.
        if let Some(id) = &evicted {
            registry.jobs.remove(id);
        }
        registry.jobs.insert(id.clone(), job.clone());
        drop(registry);
        self.events.push(json!({"type":"ssmcp.exec.started","job_id":id,"pid":pid,"command":job.command,"cwd":job.cwd}));
        let events = self.events.clone();
        let worker = job.clone();
        tokio::spawn(async move {
            let mut out = tokio::spawn(capture(worker.clone(), stdout, "stdout", events.clone()));
            let mut err = tokio::spawn(capture(worker.clone(), stderr, "stderr", events.clone()));
            let mut cancel_rx = cancel_rx;
            let outcome = tokio::select! {
                biased;
                _=cancel_rx.changed() => (None,true,false,None),
                result=async {
                    if args.timeout_ms==0 { Ok(process.wait().await) }
                    else { timeout(Duration::from_millis(args.timeout_ms),process.wait()).await }
                } => match result {
                    Ok(Ok(status)) => (status.code(),false,false,None),
                    Ok(Err(error)) => (None,false,false,Some(error.to_string())),
                    Err(_) => (None,false,true,None),
                }
            };
            process.shutdown().await;
            tokio::join!(
                drain(&mut out, &worker, "stdout"),
                drain(&mut err, &worker, "stderr")
            );
            {
                let mut state = worker.state();
                state.running = false;
                state.exit_code = outcome.0;
                state.cancelled = outcome.1;
                state.timed_out = outcome.2;
                state.error = outcome.3;
                state.finished = Some(Instant::now());
            }
            // Release the running slot before publishing completion to waiters.
            drop(permit);
            worker.changed.notify_waiters();
            let mut snapshot = worker.snapshot();
            snapshot["type"] = json!("ssmcp.exec.finished");
            events.push(snapshot);
        });
        let mut result = job.snapshot();
        result["timeout_ms"] = json!(args.timeout_ms);
        result["evicted_job_id"] = json!(evicted);
        Ok(result)
    }
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = {
            let mut registry = self.registry();
            registry.closed = true;
            registry.jobs.values().cloned().collect()
        };
        for job in &jobs {
            job.cancel.send_replace(true);
        }
        let waits = jobs.iter().map(|job| wait_finished(job, 8000));
        futures_util::future::join_all(waits).await;
        self.events.close();
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
async fn wait_finished(job: &Job, wait_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !job.state().running || wait_ms == 0 {
            return;
        }
        if timeout_at(deadline, notified).await.is_err() {
            return;
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
                let (inline, notice) = {
                    let mut state = job.state();
                    let output = if stream == "stdout" {
                        &mut state.stdout
                    } else {
                        &mut state.stderr
                    };
                    let was_spilled = output.capture.is_spilled();
                    output.append(&buffer[..count]);
                    (
                        !output.capture.is_spilled(),
                        (!was_spilled && output.capture.is_spilled())
                            .then(|| output.capture.metadata()),
                    )
                };
                if inline {
                    events.push(json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode(&buffer[..count]),"stream":stream,"processId":job.id}}));
                } else if let Some(artifact) = notice {
                    events.push(json!({"type":"ssmcp.output_archived","job_id":job.id,"stream":stream,"artifact":artifact}));
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
fn empty(args: Value) -> Result<(), String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    let _: Empty = parse(args)?;
    Ok(())
}
fn check_wait(ms: u64) -> Result<(), String> {
    if ms > MAX_WAIT {
        Err(format!("wait_ms must be 0..={MAX_WAIT}"))
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
struct Wait {
    job_id: String,
    #[serde(default = "default_wait")]
    wait_ms: u64,
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

    fn completed_job(store: &Arc<OutputStore>) -> Arc<Job> {
        let id = format!("test-{}", NEXT_JOB.fetch_add(1, Ordering::Relaxed));
        let (cancel, _) = watch::channel(false);
        let mut stdout = Output::new(store, format!("job:{id}:stdout"));
        let mut stderr = Output::new(store, format!("job:{id}:stderr"));
        stdout.finish(None);
        stderr.finish(None);
        Arc::new(Job {
            id,
            pid: 0,
            command: vec!["completed fixture".into()],
            cwd: std::env::current_dir().unwrap(),
            started: Instant::now(),
            state: Mutex::new(State {
                running: false,
                exit_code: Some(0),
                cancelled: false,
                timed_out: false,
                error: None,
                finished: Some(Instant::now()),
                stdout,
                stderr,
            }),
            changed: Notify::new(),
            cancel,
        })
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

    #[cfg(unix)]
    #[tokio::test]
    async fn independent_jobs_run_concurrently_and_keep_streams_separate() {
        let shell = Shell::new();
        let a = start(
            &shell,
            &["sh", "-c", "sleep 0.25; printf alpha; printf alpha-err >&2"],
            5000,
        )
        .await;
        let b = start(
            &shell,
            &["sh", "-c", "sleep 0.25; printf beta; printf beta-err >&2"],
            5000,
        )
        .await;
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
        assert_eq!(first["stdout"], "alpha");
        assert_eq!(first["stderr"], "alpha-err");
        assert_eq!(second["stdout"], "beta");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn jobs_support_timeout_cancellation_and_client_isolation() {
        let one = Shell::new();
        let two = Shell::new();
        let timeout_job = start(&one, &["sh", "-c", "sleep 5"], 100).await;
        let timed = one
            .call(
                "ssmcp.exec.wait",
                json!({"job_id":timeout_job,"wait_ms":5000}),
            )
            .await
            .unwrap();
        assert_eq!(timed["timed_out"], true);
        let cancel_job = start(&one, &["sh", "-c", "sleep 5"], 5000).await;
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
    async fn large_output_keeps_the_full_artifact_and_the_bounded_tail() {
        let shell = Shell::new();
        let id = py(&shell, "import sys; sys.stdout.buffer.write(b'x'*100000)").await;
        shell
            .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
            .await
            .unwrap();
        let page = shell
            .call("ssmcp.exec.read", json!({"job_id":id}))
            .await
            .unwrap();
        assert_eq!(page["stdout_truncated"], true);
        let artifact = &page["snapshot"]["stdout"]["artifact"];
        assert_eq!(artifact["bytes"], 100000);
        assert_eq!(artifact["complete"], true);
        let store = OutputStore::global().unwrap();
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
}
