//! Capability-only JavaScript orchestration. Each call gets a fresh, bounded QuickJS runtime.
use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, Function, Promise, function::Async};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    future::Future,
    hash::{Hash, Hasher},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, Semaphore};

pub const MAX_CODE_BYTES: usize = 64 * 1024;
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
pub const MAX_TIMEOUT_MS: u64 = 900_000;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
pub const MAX_LOG_BYTES: usize = 64 * 1024;
pub const MAX_CALLS: usize = 128;
pub const MAX_NATIVE_INFLIGHT: usize = 16;
pub const MAX_CONCURRENT: usize = 4;
const MAX_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_CPU_TIME: Duration = Duration::from_secs(2);
const MAX_ERROR_BYTES: usize = 4096;
static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT);

#[derive(Debug)]
pub enum HostRequest {
    Call {
        client_id: String,
        name: String,
        arguments: Value,
    },
    Tools {
        client_id: String,
        cursor: Option<String>,
    },
}

pub type Host = Arc<
    dyn Fn(HostRequest) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Default)]
struct Stop {
    cancelled: AtomicBool,
    cpu_exhausted: AtomicBool,
    wake: Notify,
}

struct CancelOnDrop(Arc<Stop>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_one();
    }
}

#[derive(Default)]
struct Logs {
    bytes: usize,
    lines: Vec<String>,
}

struct Pending(Arc<AtomicUsize>);
impl Drop for Pending {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
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

fn timeout_message() -> String {
    "code_mode timed out; an already submitted native operation may continue and its outcome may be unknown. Inspect the client before retrying a mutating call.".into()
}

fn thread_cpu_time() -> Duration {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: time points to valid writable storage; CLOCK_THREAD_CPUTIME_ID is a Linux clock.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0 {
        Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
    } else {
        Duration::ZERO
    }
}

/// Run an async JavaScript function body and return its JSON result and captured logs.
/// Dropping this future interrupts its worker; native actions already sent are not rolled back.
pub async fn execute(
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
    if !clients.is_array() {
        return Err("clients must be an array".into());
    }
    let clients = serde_json::to_string(&clients).map_err(|e| e.to_string())?;
    if clients.len() > MAX_RESULT_BYTES {
        return Err("client metadata exceeds the code_mode limit".into());
    }
    let permit = SLOTS
        .try_acquire()
        .map_err(|_| "code_mode is busy; retry later")?;
    let stop = Arc::new(Stop::default());
    let _cancel = CancelOnDrop(stop.clone());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        // A cancelled HTTP request must not free a slot while its worker is still running.
        let _permit = permit;
        handle.block_on(run(code, clients, host, stop, deadline))
    })
    .await
    .map_err(|_| "code_mode worker failed".to_string())?
}

async fn run(
    code: String,
    clients: String,
    host: Host,
    stop: Arc<Stop>,
    deadline: Instant,
) -> Result<Value, String> {
    let runtime = AsyncRuntime::new().map_err(|e| e.to_string())?;
    runtime.set_memory_limit(MAX_MEMORY_BYTES).await;
    runtime.set_max_stack_size(256 * 1024).await;
    let cpu_start = thread_cpu_time();
    let interrupt = stop.clone();
    runtime
        .set_interrupt_handler(Some(Box::new(move || {
            if thread_cpu_time().saturating_sub(cpu_start) >= MAX_CPU_TIME {
                interrupt.cpu_exhausted.store(true, Ordering::Release);
            }
            interrupt.cancelled.load(Ordering::Acquire)
                || interrupt.cpu_exhausted.load(Ordering::Acquire)
                || Instant::now() >= deadline
        })))
        .await;

    // The tracker never inspects user-defined getters or retains JavaScript values.
    let rejections = Arc::new(Mutex::new(HashMap::<u64, String>::new()));
    let rejected = rejections.clone();
    let rejection_overflow = Arc::new(AtomicBool::new(false));
    let overflow = rejection_overflow.clone();
    runtime
        .set_host_promise_rejection_tracker(Some(Box::new(move |_, promise, reason, handled| {
            let mut hash = std::hash::DefaultHasher::new();
            promise.hash(&mut hash);
            let id = hash.finish();
            let mut rejected = rejected.lock().unwrap();
            if handled {
                rejected.remove(&id);
            } else if rejected.len() < MAX_CALLS {
                let message = reason
                    .as_string()
                    .and_then(|s| s.to_string().ok())
                    .unwrap_or_else(|| "unhandled JavaScript promise rejection".into());
                rejected.insert(id, bounded(message, MAX_ERROR_BYTES));
            } else {
                overflow.store(true, Ordering::Release);
            }
        })))
        .await;

    let logs = Arc::new(Mutex::new(Logs::default()));
    let pending = Arc::new(AtomicUsize::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let context = AsyncContext::full(&runtime)
        .await
        .map_err(|e| e.to_string())?;
    let eval = context.async_with(async |ctx| {
        let logging = logs.clone();
        let log = Function::new(ctx.clone(), move |line: String| {
            let mut logs = logging.lock().unwrap();
            // Charge at least one byte per line, including empty lines.
            if logs.bytes + line.len() + 1 > MAX_LOG_BYTES {
                return false;
            }
            logs.bytes += line.len() + 1;
            logs.lines.push(line);
            true
        })
        .map_err(|e| e.to_string())?;
        let active = pending.clone();
        let stopped = stop.clone();
        let callback = Function::new(
            ctx.clone(),
            Async(move |kind: String, client_id: String, name: String, encoded: String| {
                let host = host.clone();
                let stop = stopped.clone();
                let inflight = active.fetch_add(1, Ordering::AcqRel);
                let pending = Pending(active.clone());
                // Validate before creating the future. Otherwise a loop could retain many
                // large Rust argument strings before the async call budget is polled.
                let request = (|| {
                    if count.fetch_add(1, Ordering::AcqRel) >= MAX_CALLS {
                        return Err(format!("code_mode exceeds {MAX_CALLS} native calls"));
                    }
                    if inflight >= MAX_NATIVE_INFLIGHT {
                        return Err(format!("code_mode allows at most {MAX_NATIVE_INFLIGHT} simultaneous native calls; await a smaller batch"));
                    }
                    if client_id.is_empty() || client_id.len() > 128 {
                        return Err("invalid client ID".into());
                    }
                    if encoded.len() > crate::wire::MAX_FRAME / 2 {
                        return Err("native arguments exceed the code_mode limit".into());
                    }
                    let value: Value = serde_json::from_str(&encoded)
                        .map_err(|_| "native arguments must be JSON".to_string())?;
                    match kind.as_str() {
                        "call" if value.is_object() && !name.is_empty() && name.len() <= 128 => {
                            Ok(HostRequest::Call { client_id, name, arguments: value })
                        }
                        "tools" if value.is_null() || value.as_str().is_some_and(|s| s.len() <= 128) => {
                            Ok(HostRequest::Tools { client_id, cursor: value.as_str().map(str::to_owned) })
                        }
                        _ => Err("invalid native call arguments".into()),
                    }
                })();
                async move {
                    let _pending = pending;
                    let response = async {
                        if stop.cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                            return Err(timeout_message());
                        }
                        host(request?).await
                    }.await;
                    let envelope = match response {
                        Ok(value) => json!({"ok":true,"value":value}),
                        Err(error) => json!({"ok":false,"error":bounded(error, MAX_ERROR_BYTES)}),
                    };
                    let encoded = envelope.to_string();
                    Ok::<_, rquickjs::Error>(if encoded.len() <= crate::wire::MAX_FRAME {
                        encoded
                    } else {
                        json!({"ok":false,"error":"native operation completed but its response exceeds the code_mode limit; inspect state before retrying a mutating call"}).to_string()
                    })
                }
            }),
        )
        .map_err(|e| e.to_string())?;
        let globals = ctx.globals();
        globals.set("__ssmcp_host", callback).map_err(|e| e.to_string())?;
        globals.set("__ssmcp_log", log).map_err(|e| e.to_string())?;
        globals.set("__ssmcp_clients", clients).map_err(|e| e.to_string())?;
        ctx.eval::<(), _>(include_str!("code_mode.js"))
            .catch(&ctx)
            .map_err(|e| bounded(e.to_string(), MAX_ERROR_BYTES))?;
        let source = format!("(async () => {{\n\"use strict\";\n{code}\n}})()");
        let promise = ctx.eval::<Promise, _>(source)
            .catch(&ctx)
            .map_err(|e| bounded(e.to_string(), MAX_ERROR_BYTES))?;
        let value = promise.into_future::<rquickjs::Value>().await
            .catch(&ctx)
            .map_err(|e| bounded(e.to_string(), MAX_ERROR_BYTES))?;
        let encoded = ctx.json_stringify(value)
            .catch(&ctx)
            .map_err(|e| bounded(e.to_string(), MAX_ERROR_BYTES))?
            .map(|s| s.to_string())
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| "null".into());
        if encoded.len() > MAX_RESULT_BYTES {
            return Err(format!("code_mode result exceeds {MAX_RESULT_BYTES} bytes; return a smaller summary"));
        }
        if pending.load(Ordering::Acquire) != 0 {
            return Err("code_mode returned with pending native calls; await every call. Already submitted operations may continue; inspect state before retrying.".into());
        }
        if rejection_overflow.load(Ordering::Acquire) {
            return Err("too many unhandled JavaScript promise rejections".into());
        }
        if let Some(error) = rejections.lock().unwrap().values().next() {
            return Err(format!("unhandled promise rejection: {error}"));
        }
        serde_json::from_str::<Value>(&encoded).map_err(|e| format!("result is not supported JSON: {e}"))
    });
    let result = tokio::select! {
        biased;
        _ = stop.wake.notified() => Err("code_mode cancelled; already submitted native operations may continue".into()),
        _ = tokio::time::sleep_until(deadline.into()) => Err(timeout_message()),
        result = eval => result,
    };
    let result = if stop.cpu_exhausted.load(Ordering::Acquire) {
        Err("code_mode exceeded its 2-second CPU budget; already submitted native operations may continue".into())
    } else if Instant::now() >= deadline {
        Err(timeout_message())
    } else {
        result
    };
    let result = result.map_err(|error| {
        if pending.load(Ordering::Acquire) != 0 && !error.contains("may continue") {
            format!("{error}\nSubmitted native operations may continue; inspect client state before retrying.")
        } else {
            error
        }
    });
    let lines = std::mem::take(&mut logs.lock().unwrap().lines);
    match result {
        Ok(result) => Ok(json!({"result":result,"logs":lines})),
        Err(error) if lines.is_empty() => Err(error),
        Err(error) => Err(format!("{error}\nCaptured logs:\n{}", lines.join("\n"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
        execute(
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
            let error = execute(code.into(), 40, json!([]), host())
                .await
                .unwrap_err();
            assert!(error.contains("timed out"), "{error}");
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        let pending: Host = Arc::new(|_| Box::pin(std::future::pending()));
        let error = execute(
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
        let task = tokio::spawn(execute(
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
}
