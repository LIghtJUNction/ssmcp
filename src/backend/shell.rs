//! Interactive shell plus independent, observable background command sessions.
use super::{events::EventLog, process::Managed};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    ffi::OsStr,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{Mutex, Notify, watch},
    time::{Instant, timeout, timeout_at},
};

const MAX_JOBS: usize = 32;
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_COMMAND_ARGS: usize = 256;
#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    start: u64,
    end: u64,
}
impl Output {
    fn append(&mut self, data: &[u8]) {
        let excess = (self.bytes.len() + data.len()).saturating_sub(MAX_OUTPUT);
        self.bytes.drain(..excess);
        self.bytes.extend(data);
        self.end = self.end.saturating_add(data.len() as u64);
        self.start = self.end.saturating_sub(self.bytes.len() as u64);
    }
    fn read(&self, cursor: u64, limit: usize) -> (String, u64, bool) {
        let next = cursor.max(self.start).min(self.end);
        let offset = (next - self.start) as usize;
        let count = limit.min(self.bytes.len().saturating_sub(offset));
        let data: Vec<u8> = self
            .bytes
            .iter()
            .skip(offset)
            .take(count)
            .copied()
            .collect();
        (
            String::from_utf8_lossy(&data).into_owned(),
            next + count as u64,
            cursor < self.start,
        )
    }
}
struct State {
    running: bool,
    exit_code: Option<i32>,
    cancelled: bool,
    timed_out: bool,
    error: Option<String>,
}
struct Job {
    id: String,
    pid: u32,
    state: Mutex<State>,
    changed: Notify,
    stdout: Mutex<Output>,
    stderr: Mutex<Output>,
    cancel: watch::Sender<bool>,
}
impl Job {
    async fn snapshot(&self) -> Value {
        let state = self.state.lock().await;
        let stdout = self.stdout.lock().await;
        let stderr = self.stderr.lock().await;
        json!({"job_id":self.id,"pid":self.pid,"state":if state.running{"running"}else if state.cancelled{"cancelled"}else if state.timed_out{"timed_out"}else{"completed"},"running":state.running,"exit_code":state.exit_code,"cancelled":state.cancelled,"timed_out":state.timed_out,"error":state.error,"stdout":{"start_cursor":stdout.start,"next_cursor":stdout.end,"truncated":stdout.start>0},"stderr":{"start_cursor":stderr.start,"next_cursor":stderr.end,"truncated":stderr.start>0}})
    }
}
pub struct Shell {
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    next: AtomicU64,
    events: Arc<EventLog>,
}
impl Shell {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            jobs: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            events: EventLog::new("shell"),
        })
    }
    pub fn audit(&self, event: Value) {
        self.events.push(event);
    }

    pub fn tools() -> Vec<Value> {
        vec![
            tool(
                "ssmcp.exec.start",
                "Start an independent argv command in the background. stdout and stderr are captured separately; the interactive terminal remains independent.",
                json!({"command":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":MAX_COMMAND_ARGS},"cwd":{"type":"string","description":"Optional existing absolute working directory."},"env":{"type":"object","additionalProperties":{"type":"string"}},"timeout_ms":{"type":"integer","minimum":100,"maximum":3600000,"default":600000}}),
                &["command"],
                false,
            ),
            tool(
                "ssmcp.exec.list",
                "List background command jobs owned by this client.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "ssmcp.exec.status",
                "Read one background job's state, exit code and output cursors.",
                json!({"job_id":{"type":"string"}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.wait",
                "Wait up to 5 seconds for a background job to finish.",
                json!({"job_id":{"type":"string"},"wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":5000}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.read",
                "Read bounded incremental stdout/stderr. Each stream has its own cursor.",
                json!({"job_id":{"type":"string"},"stdout_cursor":{"type":"integer","minimum":0,"default":0},"stderr_cursor":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":32768,"default":16384},"wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":0}}),
                &["job_id"],
                true,
            ),
            tool(
                "ssmcp.exec.cancel",
                "Terminate a background command and its owned child process group.",
                json!({"job_id":{"type":"string"}}),
                &["job_id"],
                false,
            ),
            tool(
                "ssmcp.exec.events",
                "Read local shell execution events mirrored in the connected client's terminal.",
                super::events::schema(),
                &[],
                true,
            ),
        ]
    }
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "ssmcp.exec.start" => self.start(args).await,
            "ssmcp.exec.list" => {
                empty(args)?;
                let jobs = self.jobs.lock().await;
                let mut values = Vec::new();
                for j in jobs.values() {
                    values.push(j.snapshot().await);
                }
                values.sort_by(|a, b| a["job_id"].as_str().cmp(&b["job_id"].as_str()));
                Ok(json!({"jobs":values,"max_jobs":MAX_JOBS}))
            }
            "ssmcp.exec.status" => {
                let a: Id = parse(args)?;
                Ok(self.job(&a.job_id).await?.snapshot().await)
            }
            "ssmcp.exec.wait" => {
                let a: Wait = parse(args)?;
                if a.wait_ms > 5000 {
                    return Err("wait_ms must be 0..=5000".into());
                }
                let j = self.job(&a.job_id).await?;
                if a.wait_ms > 0 {
                    wait_finished(&j, a.wait_ms).await;
                }
                Ok(j.snapshot().await)
            }
            "ssmcp.exec.read" => {
                let a: Read = parse(args)?;
                if a.max_bytes == 0 || a.max_bytes > 32768 || a.wait_ms > 5000 {
                    return Err("max_bytes must be 1..=32768 and wait_ms must be 0..=5000".into());
                }
                let j = self.job(&a.job_id).await?;
                if a.wait_ms > 0 {
                    wait_output(&j, a.stdout_cursor, a.stderr_cursor, a.wait_ms).await;
                }
                let stdout = j.stdout.lock().await;
                let stderr = j.stderr.lock().await;
                let (out, on, ot) = stdout.read(a.stdout_cursor, a.max_bytes);
                let (err, en, et) = stderr.read(a.stderr_cursor, a.max_bytes);
                Ok(
                    json!({"job_id":j.id,"stdout":out,"stderr":err,"stdout_cursor":on,"stderr_cursor":en,"stdout_truncated":ot,"stderr_truncated":et,"running":j.state.lock().await.running}),
                )
            }
            "ssmcp.exec.cancel" => {
                let a: Id = parse(args)?;
                let j = self.job(&a.job_id).await?;
                j.cancel.send_replace(true);
                let _ = timeout(Duration::from_secs(8), wait_finished(&j, 8000)).await;
                Ok(j.snapshot().await)
            }
            "ssmcp.exec.events" => self.events.read(args).await,
            _ => Err(format!("unknown shell execution capability: {name}")),
        }
    }
    async fn job(&self, id: &str) -> Result<Arc<Job>, String> {
        if id.is_empty() || id.len() > 128 {
            return Err("invalid job_id".into());
        }
        self.jobs
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| "unknown background job".into())
    }
    async fn start(&self, args: Value) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Start {
            command: Vec<String>,
            cwd: Option<PathBuf>,
            env: Option<HashMap<String, String>>,
            #[serde(default = "default_timeout")]
            timeout_ms: u64,
        }
        fn default_timeout() -> u64 {
            600000
        }
        let a: Start = parse(args)?;
        if a.command.is_empty()
            || a.command.len() > MAX_COMMAND_ARGS
            || a.command.iter().any(|s| s.len() > 8192 || s.contains('\0'))
        {
            return Err("command must contain 1..=256 bounded argv strings".into());
        }
        if !(100..=3600000).contains(&a.timeout_ms) {
            return Err("timeout_ms must be 100..=3600000".into());
        }
        if let Some(cwd) = &a.cwd {
            if !cwd.is_absolute() || !cwd.is_dir() {
                return Err("cwd must be an existing absolute directory".into());
            }
        }
        let mut command = Command::new(OsStr::new(&a.command[0]));
        command
            .args(&a.command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = a.cwd {
            command.current_dir(cwd);
        }
        if let Some(env) = a.env {
            if env.len() > 128
                || env.iter().any(|(k, v)| {
                    k.is_empty()
                        || k.len() > 128
                        || k.contains('=')
                        || k.contains('\0')
                        || v.len() > 8192
                        || v.contains('\0')
                })
            {
                return Err("invalid or oversized environment override".into());
            }
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
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let (cancel, cancel_rx) = watch::channel(false);
        let job = Arc::new(Job {
            id: id.clone(),
            pid,
            state: Mutex::new(State {
                running: true,
                exit_code: None,
                cancelled: false,
                timed_out: false,
                error: None,
            }),
            changed: Notify::new(),
            stdout: Mutex::new(Output::default()),
            stderr: Mutex::new(Output::default()),
            cancel,
        });
        {
            let mut jobs = self.jobs.lock().await;
            jobs.retain(|_, j| {
                Arc::strong_count(j) > 1 || j.state.try_lock().is_ok_and(|s| s.running)
            });
            if jobs.len() >= MAX_JOBS {
                process.shutdown().await;
                return Err(format!("background job limit is {MAX_JOBS}"));
            }
            jobs.insert(id.clone(), job.clone());
        }
        self.events
            .push(json!({"type":"ssmcp.exec.started","job_id":id,"pid":pid,"command":a.command}));
        let events = self.events.clone();
        let worker = job.clone();
        tokio::spawn(async move {
            let out = tokio::spawn(capture(worker.clone(), stdout, "stdout", events.clone()));
            let err = tokio::spawn(capture(worker.clone(), stderr, "stderr", events.clone()));
            let mut cancel_rx = cancel_rx;
            let outcome = tokio::select! {result=timeout(Duration::from_millis(a.timeout_ms),process.child.wait())=>match result{Ok(Ok(s))=>(s.code(),false,false,None),Ok(Err(e))=>(None,false,false,Some(e.to_string())),Err(_)=>(None,false,true,None)},_ = cancel_rx.changed()=>(None,true,false,None)};
            process.shutdown().await;
            let _ = timeout(Duration::from_secs(2), out).await;
            let _ = timeout(Duration::from_secs(2), err).await;
            {
                let mut state = worker.state.lock().await;
                state.running = false;
                state.exit_code = outcome.0;
                state.cancelled = outcome.1;
                state.timed_out = outcome.2;
                state.error = outcome.3.clone();
            }
            worker.changed.notify_waiters();
            events.push(json!({"type":"ssmcp.exec.finished","job_id":worker.id,"exit_code":outcome.0,"cancelled":outcome.1,"timed_out":outcome.2,"error":outcome.3}));
        });
        Ok(json!({"job_id":id,"pid":pid,"state":"running","timeout_ms":a.timeout_ms}))
    }
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = self.jobs.lock().await.values().cloned().collect();
        for j in &jobs {
            j.cancel.send_replace(true);
        }
        for j in jobs {
            let _ = timeout(Duration::from_secs(8), wait_finished(&j, 8000)).await;
        }
        self.events.close();
    }
}
async fn wait_finished(job: &Job, wait_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !job.state.lock().await.running {
            return;
        }
        if timeout_at(deadline, notified).await.is_err() {
            return;
        }
    }
}
async fn wait_output(job: &Job, stdout_cursor: u64, stderr_cursor: u64, wait_ms: u64) {
    let deadline = Instant::now() + Duration::from_millis(wait_ms);
    loop {
        let notified = job.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let state = job.state.lock().await;
        let has_output = job.stdout.lock().await.end > stdout_cursor
            || job.stderr.lock().await.end > stderr_cursor;
        if has_output || !state.running {
            return;
        }
        drop(state);
        if timeout_at(deadline, notified).await.is_err() {
            return;
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
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stream == "stdout" {
                    job.stdout.lock().await.append(&buffer[..n]);
                } else {
                    job.stderr.lock().await.append(&buffer[..n]);
                }
                events.push(json!({"method":"command/exec/outputDelta","params":{"deltaBase64":STANDARD.encode(&buffer[..n]),"stream":stream,"jobId":job.id}}));
                job.changed.notify_waiters();
            }
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
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":readonly,"destructiveHint":!readonly,"openWorldHint":false}})
}
fn parse<T: for<'de> Deserialize<'de>>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid execution arguments: {e}"))
}
fn empty(args: Value) -> Result<(), String> {
    let _: std::collections::BTreeMap<String, Value> = parse(args)?;
    Ok(())
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
}
