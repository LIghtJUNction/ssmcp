//! Explicit local commands over the native Runtime.
use crate::{runtime::Runtime, wire::Operation};
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicUsize, Ordering},
};

const MAX_INPUT: usize = 16 * 1024;
const MAX_ACTIVE: usize = 4;

#[derive(Clone)]
struct Binding {
    runtime: Weak<Runtime>,
    handle: Option<tokio::runtime::Handle>,
}

static BINDING: OnceLock<Mutex<Option<Binding>>> = OnceLock::new();
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn bind(runtime: &Arc<Runtime>) {
    let mut binding = BINDING
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    *binding = Some(Binding {
        runtime: Arc::downgrade(runtime),
        handle: tokio::runtime::Handle::try_current().ok(),
    });
}

fn binding() -> Result<Binding, String> {
    BINDING
        .get()
        .and_then(|binding| {
            binding
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        })
        .ok_or_else(|| "local commands require a live client connection".into())
}

pub(crate) fn local_runtime() -> Result<(Arc<Runtime>, tokio::runtime::Handle), String> {
    let binding = binding()?;
    let runtime = binding
        .runtime
        .upgrade()
        .ok_or("The client connection has ended; reconnect before importing input")?;
    let handle = binding
        .handle
        .ok_or("Local input requires the interactive client runtime")?;
    Ok((runtime, handle))
}

struct Slot;
impl Slot {
    fn acquire() -> Result<Self, String> {
        // compare_exchange_weak remains available at the project's Rust 1.89
        // MSRV; try_update is only available on newer toolchains.
        let mut current = ACTIVE.load(Ordering::Acquire);
        loop {
            if current >= MAX_ACTIVE {
                return Err(
                    "four local commands are already running; wait for one to finish".into(),
                );
            }
            match ACTIVE.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self),
                Err(observed) => current = observed,
            }
        }
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::AcqRel);
    }
}

fn bounded(value: &str, limit: usize) -> &str {
    let mut end = value.len().min(limit);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn task_request(raw: &str) -> Result<(&'static str, Value), String> {
    let parts = raw.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        [] => Ok(("ssmcp.exec.list", json!({}))),
        ["cancel", id]
            if id.len() <= 128
                && !id.is_empty()
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)) =>
        {
            Ok(("ssmcp.exec.cancel", json!({"job_id":id})))
        }
        _ => Err("Use /tasks to list commands, or /tasks cancel JOB_ID to stop one".into()),
    }
}

fn task_message(value: &Value) -> String {
    let data = native_data(value);
    let Some(jobs) = data["jobs"].as_array() else {
        let id = data["job_id"].as_str().unwrap_or("unknown");
        let state = data["state"].as_str().unwrap_or("unknown");
        return format!("## Local task · {state}\n\nJob: {id}\n\nUse /tasks to refresh.");
    };
    let mut jobs = jobs.iter().collect::<Vec<_>>();
    jobs.sort_by_key(|job| job["running"] != true);
    let running = jobs.iter().filter(|job| job["running"] == true).count();
    let mut text = format!(
        "## Local tasks\n\n{running} running · {} retained\n",
        jobs.len()
    );
    if jobs.is_empty() {
        text.push_str("\nNo command jobs in this client.\n");
    }
    for job in jobs.iter().take(20) {
        let id = job["job_id"].as_str().unwrap_or("unknown");
        let state = job["state"].as_str().unwrap_or("unknown");
        let command = job["command"]
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        let command = command.split_whitespace().collect::<Vec<_>>().join(" ");
        let command = command
            .chars()
            .take(160)
            .collect::<String>()
            .replace('\u{0060}', "'");
        let exit = job["exit_code"]
            .as_i64()
            .map(|code| format!(" · exit {code}"))
            .unwrap_or_default();
        text.push_str(&format!("\n- **{state}{exit}** · {id}\n  {command}\n"));
    }
    if jobs.len() > 20 {
        text.push_str("\nShowing 20 jobs. ssmcp.exec.list returns the retained catalog.\n");
    }
    text.push_str("\nRefresh: /tasks\nStop a job: /tasks cancel JOB_ID\n");
    text
}

/// Task inspection/cancellation remains available even when all execution slots are busy.
pub(crate) fn submit_tasks(raw: &str) -> Result<(), String> {
    let (name, arguments) = task_request(raw)?;
    let binding = binding()?;
    let runtime = binding
        .runtime
        .upgrade()
        .ok_or("The client connection has ended")?;
    let handle = binding
        .handle
        .ok_or("The live client async runtime is not available")?;
    let source = crate::runtime::call_scope::Source::capture();
    handle.spawn(source.scope(async move {
        let message = match runtime
            .perform(Operation::CallTool {
                name: name.into(),
                arguments,
            })
            .await
        {
            Ok(value) => task_message(&value),
            Err(error) => format!("## Local tasks\n\n{}", bounded(&error, 4096)),
        };
        super::super::render::message(&message);
    }));
    Ok(())
}

fn shell_arguments(raw: &str) -> Result<Value, String> {
    let command = raw.trim();
    if command.is_empty() {
        return Err("Use ! followed by a shell command, for example !pwd".into());
    }
    if command.len() > MAX_INPUT
        || command
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(
            "Shell commands accept up to 16384 UTF-8 bytes without control characters".into(),
        );
    }
    let shell = crate::platform::default_shell();
    let shell = shell
        .to_str()
        .ok_or("The configured shell path is not valid UTF-8")?;
    #[cfg(unix)]
    let arguments = json!({"command":[shell,"-c",command],"stdin":"null"});
    #[cfg(windows)]
    let arguments = json!({"command":[shell,"/D","/S","/C",command],"stdin":"null"});
    Ok(arguments)
}

fn native_data(value: &Value) -> &Value {
    value.get("structuredContent").unwrap_or(value)
}

async fn execute_shell(runtime: Arc<Runtime>, raw: String, arguments: Value) -> Result<(), String> {
    let started = runtime
        .perform(Operation::CallTool {
            name: "ssmcp.exec.start".into(),
            arguments,
        })
        .await?;
    let data = native_data(&started);
    let job_id = data["job_id"]
        .as_str()
        .ok_or("The shell command did not return a job ID")?
        .to_owned();
    if let Err(error) = super::history::record("user", &format!("!{raw}")) {
        super::super::render::status("history", "Warning", &error);
    }
    super::super::render::message(&format!(
        "## ! command started\n\n\u{0060}\u{0060}\u{0060}sh\n{raw}\n\u{0060}\u{0060}\u{0060}\n\nJob: {job_id}"
    ));
    let mut stdout_cursor = 0u64;
    let mut stderr_cursor = 0u64;
    loop {
        let result = runtime
            .perform(Operation::CallTool {
                name: "ssmcp.exec.read".into(),
                arguments: json!({
                    "job_id":job_id,"stdout_cursor":stdout_cursor,
                    "stderr_cursor":stderr_cursor,"max_bytes":4096,"wait_ms":1000
                }),
            })
            .await?;
        let data = native_data(&result);
        stdout_cursor = data["stdout_cursor"].as_u64().unwrap_or(stdout_cursor);
        stderr_cursor = data["stderr_cursor"].as_u64().unwrap_or(stderr_cursor);
        for stream in ["stdout", "stderr"] {
            if let Some(text) = data[stream].as_str().filter(|text| !text.is_empty()) {
                super::super::render::message(&format!(
                    "### ! {stream}\n\n\u{0060}\u{0060}\u{0060}text\n{text}\n\u{0060}\u{0060}\u{0060}"
                ));
            }
        }
        if data["running"] == false && data["stdout_eof"] == true && data["stderr_eof"] == true {
            let status = if let Some(code) = data["exit_code"].as_i64() {
                format!("exit {code}")
            } else {
                data["snapshot"]["state"]
                    .as_str()
                    .unwrap_or("finished")
                    .to_owned()
            };
            let mut message = format!("## ! command · {status}\n\nJob: {job_id}");
            for stream in ["stdout", "stderr"] {
                if let Some(id) = data["snapshot"][stream]["artifact"]["id"].as_str() {
                    message.push_str(&format!(
                        "\n\nFull {stream}: {id} · read with ssmcp.output.read."
                    ));
                }
            }
            super::super::render::message(&message);
            return Ok(());
        }
    }
}

/// Run only a command the local user explicitly submitted with the ASCII ! prefix.
pub(crate) fn submit_shell(raw: &str) -> Result<(), String> {
    let arguments = shell_arguments(raw)?;
    let binding = binding()?;
    let runtime = binding
        .runtime
        .upgrade()
        .ok_or("The client connection has ended")?;
    runtime.local_registry().ensure_open()?;
    let handle = binding
        .handle
        .ok_or("The live client async runtime is not available")?;
    let slot = Slot::acquire()?;
    let raw = raw.trim().to_owned();
    let source = crate::runtime::call_scope::Source::capture();
    handle.spawn(source.scope(async move {
        let _slot = slot;
        if let Err(error) = execute_shell(runtime, raw, arguments).await {
            super::super::render::message(&format!(
                "## ! command failed\n\n{}",
                bounded(&error, 4096)
            ));
        }
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_commands_require_explicit_exact_cancellation() {
        assert_eq!(task_request("").unwrap().0, "ssmcp.exec.list");
        assert_eq!(task_request("cancel abc-12").unwrap().1["job_id"], "abc-12");
        for value in [
            "cancel",
            "cancel *",
            "cancel one two",
            "kill abc",
            "cancel ../x",
        ] {
            assert!(task_request(value).is_err(), "{value}");
        }
        let text = task_message(&json!({"jobs":[
            {"job_id":"done","state":"completed","exit_code":7,"running":false,"command":["echo","done"]},
            {"job_id":"active","state":"running","running":true,"command":["sleep","20"]}
        ]}));
        assert!(text.contains("1 running"));
        assert!(text.find("active").unwrap() < text.find("done").unwrap());
        assert!(text.contains("exit 7"));
    }

    #[test]
    fn shell_prefix_keeps_command_as_one_explicit_shell_argument() {
        let command = "printf '%s\\n' 'a b' | sort";
        let value = shell_arguments(command).unwrap();
        let args = value["command"].as_array().unwrap();
        assert_eq!(args.last().unwrap(), command);
        assert_eq!(value["stdin"], "null");
        assert!(shell_arguments(" \n ").is_err());
        assert!(shell_arguments("echo \u{1b}[2J").is_err());
        assert!(shell_arguments(&"x".repeat(MAX_INPUT + 1)).is_err());
    }
}
