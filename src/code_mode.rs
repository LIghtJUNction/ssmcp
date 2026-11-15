//! JavaScript orchestration through system Node.js inside mandatory bubblewrap isolation.
use crate::backend::process::Managed;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    future::Future,
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout, Command},
    sync::Semaphore,
    task::JoinSet,
};

pub const MAX_CODE_BYTES: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_BYTES: usize = 64 * 1024;
pub const MAX_CALLS: usize = 128;
pub const MAX_NATIVE_INFLIGHT: usize = 16;
pub const MAX_CONCURRENT: usize = 4;
const MAX_ERROR_BYTES: usize = 4096;
static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT);

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostRequest {
    Call {
        client_id: String,
        name: String,
        arguments: Value,
    },
    Tools {
        client_id: String,
        #[serde(default)]
        cursor: Option<String>,
    },
}
pub type Host = Arc<
    dyn Fn(HostRequest) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerFrame {
    Call { id: u64, request: HostRequest },
    Log { line: String },
    Result { value: Value },
    Error { message: String },
}

struct Worker {
    process: Managed,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Signal the owned process tree before closing a pipe can trigger an early exit.
        self.process.terminate();
    }
}

fn bounded(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str(" [truncated]");
    }
    text
}

fn executable(name: &str) -> Result<PathBuf, String> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join(name))
        .find(|path| std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| format!("code_mode requires system {name}; install nodejs and bubblewrap with your system package manager"))
}

fn runner_path() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("SSMCP_CODE_MODE_RUNNER") {
        let path = PathBuf::from(path);
        if !path.is_absolute() || !path.is_file() {
            return Err("SSMCP_CODE_MODE_RUNNER must name an existing absolute runner file".into());
        }
        return path.canonicalize().map_err(|e| e.to_string());
    }
    if let Some(prefix) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent()?.parent().map(Path::to_path_buf))
    {
        let path = prefix.join("libexec/ssmcp/code-mode.js");
        if path.is_file() {
            return path.canonicalize().map_err(|e| e.to_string());
        }
    }
    #[cfg(debug_assertions)]
    {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/code_mode.js");
        if path.is_file() {
            return Ok(path);
        }
    }
    Err("code_mode runner is missing; run make install-server or set SSMCP_CODE_MODE_RUNNER to its installed absolute path".into())
}

fn node_arguments(command: &mut Command) {
    command.args([
        "--jitless",
        "--no-addons",
        "--max-old-space-size=32",
        "--max-semi-space-size=1",
        "--stack-size=256",
    ]);
}

fn sandbox_command() -> Result<Command, String> {
    let node = executable("node")?;
    let bwrap = executable("bwrap")?;
    let runner = runner_path()?;
    let mut command = Command::new(bwrap);
    command.env_clear().args([
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--uid",
        "65534",
        "--gid",
        "65534",
        "--cap-drop",
        "ALL",
        "--new-session",
        "--die-with-parent",
        "--clearenv",
        "--setenv",
        "HOME",
        "/tmp",
        "--setenv",
        "LANG",
        "C.UTF-8",
        "--setenv",
        "PATH",
        "/runtime",
    ]);
    // Only runtime libraries are visible. The worker needs no procfs; omitting it
    // also preserves systemd's protected /proc mounts without relaxing the service.
    for path in ["/usr/lib", "/lib", "/lib64", "/usr/share/nodejs"] {
        command.args(["--ro-bind-try", path, path]);
    }
    command
        .arg("--ro-bind")
        .arg(node)
        .arg("/runtime/node")
        .arg("--ro-bind")
        .arg(runner)
        .arg("/runner.js")
        .args([
            "--dev",
            "/dev",
            "--remount-ro",
            "/dev",
            "--size",
            "16777216",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/work",
            "--chdir",
            "/work",
            "--remount-ro",
            "/",
            "--",
            "/runtime/node",
        ]);
    node_arguments(&mut command);
    command.arg("/runner.js");
    Ok(command)
}

fn limit_process(command: &mut Command) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: after fork this closure only calls async-signal-safe setrlimit.
    unsafe {
        command.pre_exec(|| {
            for (resource, value) in [
                (libc::RLIMIT_CPU, 2),
                (libc::RLIMIT_DATA, 256 * 1024 * 1024),
                (libc::RLIMIT_AS, 2 * 1024 * 1024 * 1024),
                (libc::RLIMIT_NOFILE, 64),
                (libc::RLIMIT_NPROC, 128),
                (libc::RLIMIT_CORE, 0),
            ] {
                let limits = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &limits) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Run an async JavaScript body with the installed system runtime.
/// Bubblewrap is mandatory: an unavailable sandbox fails closed.
pub async fn execute(
    code: String,
    timeout_ms: u64,
    clients: Value,
    host: Host,
) -> Result<Value, String> {
    execute_process(sandbox_command()?, code, timeout_ms, clients, host).await
}

async fn execute_process(
    mut command: Command,
    code: String,
    timeout_ms: u64,
    clients: Value,
    host: Host,
) -> Result<Value, String> {
    if code.trim().is_empty() || code.len() > MAX_CODE_BYTES {
        return Err(format!(
            "code must contain 1..={MAX_CODE_BYTES} UTF-8 bytes"
        ));
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(format!("timeout_ms must be 1..={MAX_TIMEOUT_MS}"));
    }
    if !clients.is_array() || clients.to_string().len() > MAX_RESULT_BYTES {
        return Err("clients must be an array within the code_mode metadata limit".into());
    }
    let _permit = SLOTS
        .try_acquire()
        .map_err(|_| "code_mode is busy; retry later")?;
    limit_process(&mut command);
    let mut process = Managed::spawn(&mut command)
        .map_err(|e| format!("cannot start isolated code_mode runtime: {e}"))?;
    let stdin = process
        .child
        .stdin
        .take()
        .ok_or("runtime stdin unavailable")?;
    let stdout = process
        .child
        .stdout
        .take()
        .ok_or("runtime stdout unavailable")?;
    let stderr = process
        .child
        .stderr
        .take()
        .ok_or("runtime stderr unavailable")?;
    let diagnostics = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr
            .take(MAX_ERROR_BYTES as u64)
            .read_to_end(&mut bytes)
            .await;
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let mut worker = Worker {
        process,
        stdin,
        stdout: BufReader::new(stdout),
    };
    let mut logs = Vec::new();
    let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        send_frame(&mut worker.stdin, &json!({"type":"run", "code":code, "clients":clients})).await?;
        bridge(&mut worker.stdout, &mut worker.stdin, host, &mut logs).await
    }).await.unwrap_or_else(|_| Err("code_mode timed out; an already submitted native operation may continue and its outcome may be unknown. Inspect the client before retrying a mutating call.".into()));
    // Managed also terminates the owned process tree when this future is cancelled.
    worker.process.shutdown().await;
    let diagnostics = diagnostics.await.unwrap_or_default();
    match result {
        Ok(value) => Ok(json!({"result":value, "logs":logs})),
        Err(mut error) => {
            if !diagnostics.trim().is_empty() {
                error.push_str("\nRuntime: ");
                error.push_str(diagnostics.trim());
            }
            if !logs.is_empty() {
                error.push_str("\nCaptured logs:\n");
                error.push_str(&logs.join("\n"));
            }
            Err(error)
        }
    }
}

fn validate_request(request: &HostRequest) -> Result<(), String> {
    let client_id = match request {
        HostRequest::Call {
            client_id,
            name,
            arguments,
        } => {
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                || !arguments.is_object()
                || arguments.to_string().len() > crate::wire::MAX_FRAME / 2
            {
                return Err("invalid native tool name or arguments".into());
            }
            client_id
        }
        HostRequest::Tools { client_id, cursor } => {
            if cursor.as_ref().is_some_and(|c| c.len() > 128) {
                return Err("tool cursor is too long".into());
            }
            client_id
        }
    };
    if client_id.is_empty() || client_id.len() > 128 {
        return Err("invalid client ID".into());
    }
    Ok(())
}

async fn bridge<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    writer: &mut W,
    host: Host,
    logs: &mut Vec<String>,
) -> Result<Value, String> {
    let mut partial = Vec::new();
    let mut pending = HashSet::new();
    let mut calls = JoinSet::new();
    let mut count = 0u64;
    let mut log_bytes = 0;
    let result = async {
        loop {
            tokio::select! {
                frame = read_frame(&mut reader, &mut partial) => match frame? {
                    Some(WorkerFrame::Call {id, request}) => {
                        count += 1;
                        if count > MAX_CALLS as u64 { return Err(format!("code_mode exceeds {MAX_CALLS} native calls")); }
                        if id != count { return Err("runtime sent an invalid or reused native call ID".into()); }
                        let validation = if pending.len() >= MAX_NATIVE_INFLIGHT {
                            Err(format!("code_mode allows at most {MAX_NATIVE_INFLIGHT} simultaneous native calls; await a smaller batch"))
                        } else { validate_request(&request) };
                        if let Err(error) = validation { send_reply(writer, id, Err(error)).await?; continue; }
                        pending.insert(id);
                        let host = host.clone();
                        calls.spawn(async move { (id, host(request).await) });
                    },
                    Some(WorkerFrame::Log {line}) => {
                        if log_bytes + line.len() + 1 > MAX_LOG_BYTES { return Err(format!("code_mode log output exceeds {MAX_LOG_BYTES} bytes")); }
                        log_bytes += line.len() + 1;
                        logs.push(line);
                    },
                    Some(WorkerFrame::Result {value}) => {
                        if !pending.is_empty() { return Err("code_mode returned with pending native calls; await every call".into()); }
                        if value.to_string().len() > MAX_RESULT_BYTES { return Err(format!("code_mode result exceeds {MAX_RESULT_BYTES} bytes; return a smaller summary")); }
                        return Ok(value);
                    },
                    Some(WorkerFrame::Error {message}) => return Err(bounded(message, MAX_ERROR_BYTES)),
                    None => return Err("isolated code_mode runtime exited before returning a result; check system Node.js, bubblewrap namespace support and process resource limits".into()),
                },
                completed = calls.join_next(), if !calls.is_empty() => {
                    let (id, result) = completed.ok_or("native call task disappeared")?.map_err(|_| "native call task failed")?;
                    pending.remove(&id);
                    send_reply(writer, id, result).await?;
                },
            }
        }
    }.await;
    calls.abort_all();
    while calls.join_next().await.is_some() {}
    result.map_err(|error: String| {
        if !pending.is_empty() { format!("{error}\nSubmitted native operations may continue and their outcome may be unknown; inspect client state before retrying.") } else { error }
    })
}

async fn send_reply<W: AsyncWrite + Unpin>(
    writer: &mut W,
    id: u64,
    result: Result<Value, String>,
) -> Result<(), String> {
    let mut reply = match result {
        Ok(value) => json!({"type":"reply","id":id,"ok":true,"value":value}),
        Err(error) => {
            json!({"type":"reply","id":id,"ok":false,"error":bounded(error, MAX_ERROR_BYTES)})
        }
    };
    if reply.to_string().len() + 1 > crate::wire::MAX_FRAME {
        reply = json!({"type":"reply","id":id,"ok":false,"error":"native operation completed but its response exceeds the code_mode limit; inspect state before retrying a mutating call"});
    }
    send_frame(writer, &reply).await
}

async fn send_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > crate::wire::MAX_FRAME {
        return Err("code_mode protocol frame exceeds its byte limit".into());
    }
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("runtime input failed: {e}"))?;
    writer.flush().await.map_err(|e| e.to_string())
}

async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    partial: &mut Vec<u8>,
) -> Result<Option<WorkerFrame>, String> {
    // Keep partial bytes outside this future: host completions can cancel a read mid-frame.
    loop {
        let bytes = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if bytes.is_empty() {
            return if partial.is_empty() {
                Ok(None)
            } else {
                Err("truncated runtime frame".into())
            };
        }
        let length = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1);
        if partial.len() + length > crate::wire::MAX_FRAME {
            return Err("runtime frame exceeds its byte limit".into());
        }
        let complete = bytes[length - 1] == b'\n';
        partial.extend_from_slice(&bytes[..length]);
        reader.consume(length);
        if complete {
            let frame =
                serde_json::from_slice(partial).map_err(|e| format!("invalid runtime frame: {e}"));
            partial.clear();
            return frame.map(Some);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Instant;
    use tokio::sync::Notify;

    static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    // Fixed test programs verify the runner/pipe protocol; execute() always requires bubblewrap.
    async fn trusted_execute(
        code: String,
        timeout_ms: u64,
        clients: Value,
        host: Host,
    ) -> Result<Value, String> {
        let mut command = Command::new(executable("node")?);
        command.env_clear();
        node_arguments(&mut command);
        command.arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/code_mode.js"));
        execute_process(command, code, timeout_ms, clients, host).await
    }

    fn host() -> Host {
        Arc::new(|request| {
            Box::pin(async move {
                match request {
                    HostRequest::Call {
                        client_id,
                        name,
                        arguments,
                    } => {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                        if name == "fail" {
                            return Err("native failure".into());
                        }
                        Ok(
                            json!({"content":[],"isError":name == "native_error","structuredContent":{"id":client_id,"arguments":arguments},"_meta":{"native":true}}),
                        )
                    }
                    HostRequest::Tools { client_id, cursor } => Ok(
                        json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}],"id":client_id,"cursor":cursor}),
                    ),
                }
            })
        })
    }

    async fn run_code(code: &str) -> Result<Value, String> {
        trusted_execute(
            code.into(),
            2_000,
            json!([{"id":"one","name":"workstation","cwd":"/work","type":"shell"}]),
            host(),
        )
        .await
    }

    #[tokio::test]
    async fn real_javascript_awaits_parallel_calls_and_preserves_native_results() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const results = await Promise.all([1,2,3].map(n => call(clients[0].id, "echo", {n})));
            let sum = 0;
            for (const item of results) { if (!item.isError) sum += item.structuredContent.arguments.n; }
            console.log("sum", sum);
            const catalog = await tools(clients[0].id);
            return {sum, native:results[0], tool:catalog.tools[0].name};
        "#).await.unwrap();
        assert_eq!(output["result"]["sum"], 6);
        assert_eq!(output["result"]["native"]["_meta"]["native"], true);
        assert_eq!(output["result"]["tool"], "echo");
        assert_eq!(output["logs"], json!(["sum 6"]));
        assert_eq!(
            run_code("print('done');").await.unwrap()["result"],
            Value::Null
        );
    }

    #[tokio::test]
    async fn sandbox_has_no_os_capabilities_or_mutable_client_metadata() {
        let _guard = TESTS.lock().await;
        let output = run_code(r#"
            const absent = [typeof process, typeof require, typeof fetch, typeof std, typeof os, typeof __ssmcp_host];
            let frozen = false;
            try { clients[0].cwd = "/changed"; } catch (e) { frozen = true; }
            return {absent,frozen,cwd:clients[0].cwd};
        "#).await.unwrap();
        assert_eq!(output["result"]["absent"], json!(vec!["undefined"; 6]));
        assert_eq!(output["result"]["frozen"], true);
        assert_eq!(output["result"]["cwd"], "/work");
        assert!(run_code("return await import('fs');").await.is_err());
    }

    #[tokio::test]
    async fn errors_keep_logs_and_native_errors_remain_observable() {
        let _guard = TESTS.lock().await;
        let error = run_code("print('before'); throw new Error('broken');")
            .await
            .unwrap_err();
        assert!(error.contains("broken"));
        assert!(error.contains("before"));
        assert!(
            run_code("return await call('one','fail',{});")
                .await
                .unwrap_err()
                .contains("native failure")
        );
        assert_eq!(
            run_code("return await call('one','native_error',{});")
                .await
                .unwrap()["result"]["isError"],
            true
        );
        assert!(
            run_code("Promise.reject('unobserved'); return 1;")
                .await
                .unwrap_err()
                .contains("unobserved")
        );
        assert!(
            run_code("call('one','echo',{}); return 1;")
                .await
                .unwrap_err()
                .contains("pending native calls")
        );
        assert!(run_code("return {;").await.is_err());
    }

    #[tokio::test]
    async fn runaway_cpu_and_waiting_promises_stop_at_the_deadline() {
        let _guard = TESTS.lock().await;
        for code in [
            "while (true) {}",
            "while (true) { await Promise.resolve(); }",
            "await new Promise(() => {});",
        ] {
            let start = Instant::now();
            let error = trusted_execute(code.into(), 40, json!([]), host())
                .await
                .unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        let pending: Host = Arc::new(|_| Box::pin(std::future::pending()));
        let error = trusted_execute(
            "await call('one','wait',{});".into(),
            40,
            json!([]),
            pending,
        )
        .await
        .unwrap_err();
        assert!(error.contains("outcome may be unknown"));
        assert_eq!(run_code("return 7;").await.unwrap()["result"], 7);
    }

    #[tokio::test]
    async fn memory_output_and_call_budgets_are_enforced() {
        let _guard = TESTS.lock().await;
        assert!(
            run_code("return 'x'.repeat(1048577);")
                .await
                .unwrap_err()
                .contains("result exceeds")
        );
        assert!(
            run_code("print('x'.repeat(65536));")
                .await
                .unwrap_err()
                .contains("log output exceeds")
        );
        assert!(
            run_code("const a=[]; while(true) a.push(new Uint8Array(1024*1024));")
                .await
                .is_err()
        );
        let error = run_code("for (let i=0;i<129;i++) await call('one','echo',{});")
            .await
            .unwrap_err();
        assert!(error.contains("128 native calls"), "{error}");
        assert_eq!(run_code("return 9;").await.unwrap()["result"], 9);
    }

    #[tokio::test]
    async fn cancelling_the_request_releases_worker_and_host_future() {
        let _guard = TESTS.lock().await;
        let entered = Arc::new(Notify::new());
        let released = Arc::new(AtomicBool::new(false));
        struct Released(Arc<AtomicBool>);
        impl Drop for Released {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let notice = entered.clone();
        let flag = released.clone();
        let host: Host = Arc::new(move |_| {
            let notice = notice.clone();
            let flag = flag.clone();
            Box::pin(async move {
                let _released = Released(flag);
                notice.notify_one();
                std::future::pending().await
            })
        });
        let task = tokio::spawn(trusted_execute(
            "await call('one','wait',{});".into(),
            MAX_TIMEOUT_MS,
            json!([]),
            host,
        ));
        entered.notified().await;
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !released.load(Ordering::Acquire) || SLOTS.available_permits() != MAX_CONCURRENT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    #[ignore = "requires system Node.js and bubblewrap namespace support; CI runs --include-ignored"]
    async fn real_system_runtime_uses_mandatory_os_isolation() {
        let _guard = TESTS.lock().await;
        let result = execute(
            "return await call('one','echo',{value:42});".into(),
            10_000,
            json!([]),
            host(),
        )
        .await
        .unwrap();
        assert_eq!(
            result["result"]["structuredContent"]["arguments"]["value"],
            42
        );
        // Probe the OS profile directly, independently of the VM's API restrictions.
        let base = sandbox_command().unwrap();
        let mut arguments: Vec<_> = base.as_std().get_args().map(|a| a.to_owned()).collect();
        assert_eq!(arguments.pop().unwrap(), "/runner.js");
        let mut probe = Command::new(base.as_std().get_program());
        probe.env_clear().args(arguments).args(["--eval", r#"
            const fs = require('fs');
            const denied = ['/etc/passwd','/proc/1/root/etc/passwd','/home','/run'].map(p => { try { fs.readFileSync(p); return false; } catch (_) { return true; } });
            let readonly = false;
            try { fs.writeFileSync('/work/probe','x'); } catch (_) { readonly = true; }
            fs.writeFileSync('/tmp/probe','x');
            const interfaces = Object.values(require('os').networkInterfaces()).flat();
            const value = {denied,readonly,env:process.env,uid:process.getuid(),pid:process.pid,proc:fs.existsSync('/proc'),loopbackOnly:interfaces.every(address => address.internal)};
            process.stdout.write(JSON.stringify({type:'result',value})+'\n');
        "#]);
        let result = execute_process(probe, "return null;".into(), 10_000, json!([]), host())
            .await
            .unwrap();
        assert_eq!(result["result"]["denied"], json!([true, true, true, true]));
        assert_eq!(result["result"]["readonly"], true);
        assert_eq!(result["result"]["uid"], 65534);
        assert_eq!(result["result"]["proc"], false);
        assert_eq!(result["result"]["loopbackOnly"], true);
        assert!((1..16).contains(&result["result"]["pid"].as_u64().unwrap()));
        assert!(
            result["result"]["env"]
                .as_object()
                .unwrap()
                .keys()
                .all(|k| matches!(k.as_str(), "HOME" | "LANG" | "PATH" | "PWD"))
        );
    }

    #[test]
    fn sandbox_profile_has_no_host_accounts_or_network_access() {
        let command = sandbox_command().unwrap();
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|v| v == "--unshare-all"));
        assert!(args.iter().any(|v| v == "--unshare-user"));
        assert!(args.iter().any(|v| v == "--disable-userns"));
        assert!(!args.iter().any(|v| matches!(
            v.as_str(),
            "--share-net" | "--proc" | "/proc" | "/etc" | "/home" | "/run" | "/usr"
        )));
        assert_eq!(command.as_std().get_envs().count(), 0);
    }
}
