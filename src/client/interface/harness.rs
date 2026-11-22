//! Explicit local slash commands over the live, registered harness primitives.
#[cfg(test)]
use crate::output::ArtifactMetadata;
use crate::{
    output::OutputStore,
    registry::{Provider, RegisteredTool, Registry},
    runtime::Runtime,
    wire::Operation,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex, OnceLock, Weak,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

const PAGE_SIZE: usize = 16;
const MAX_INPUT: usize = 16 * 1024;
const MAX_PREVIEW: usize = 8 * 1024;
const MAX_ACTIVE: usize = 4;

#[derive(Clone)]
struct Binding {
    runtime: Weak<Runtime>,
    handle: Option<tokio::runtime::Handle>,
}

static BINDING: OnceLock<Mutex<Option<Binding>>> = OnceLock::new();
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

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
        .ok_or_else(|| "harness shortcuts require a live client connection".into())
}

fn runtime() -> Result<Arc<Runtime>, String> {
    binding()?.runtime.upgrade().ok_or_else(|| {
        "the client connection has ended; reconnect before using a harness shortcut".into()
    })
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

fn provider(value: &str) -> Result<Provider, String> {
    match value {
        "cc" | "claude" => Ok(Provider::Claude),
        "codex" => Ok(Provider::Codex),
        "pi" => Ok(Provider::Pi),
        _ => Err("harness provider must be cc, claude, codex or pi".into()),
    }
}

fn prefix_matches(provider: Provider, id: &str) -> bool {
    let label = provider.label();
    id.starts_with(&format!("{label}.")) || id.starts_with(&format!("ssmcp.{label}."))
}

fn names(registry: &Registry, provider: Provider) -> Result<Vec<String>, String> {
    registry
        .snapshot()?
        .iter()
        .map(|descriptor| descriptor.header())
        .filter_map(|header| match header {
            Ok((name, _)) if prefix_matches(provider, &name) => Some(Ok(name)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn short_name(provider: Provider, id: &str) -> &str {
    id.strip_prefix(&format!("{}.", provider.label()))
        .or_else(|| id.strip_prefix(&format!("ssmcp.{}.", provider.label())))
        .unwrap_or(id)
}

fn resolve(registry: &Registry, provider: Provider, name: &str) -> Result<RegisteredTool, String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err("use an exact registered tool name from this provider's tools list".into());
    }
    let names = names(registry, provider)?;
    let matches = names
        .iter()
        .filter(|id| {
            id.as_str() == name
                || short_name(provider, id) == name
                || (provider == Provider::Claude
                    && name
                        .strip_prefix("cc.")
                        .is_some_and(|name| short_name(provider, id) == name))
        })
        .collect::<Vec<_>>();
    let id = matches
        .iter()
        .find(|id| id.as_str() == name)
        .copied()
        .or_else(|| (matches.len() == 1).then(|| matches[0]));
    let Some(id) = id else {
        if matches.len() > 1 {
            return Err("tool name is ambiguous; use its complete registered name".into());
        }
        return Err(if provider == Provider::Pi {
            format!(
                "Pi has no registered tool named {name}. Use /pi tools. pi.rpc is a control protocol; get_commands lists slash commands, skills and templates, not an internal LSP/model-tool catalog."
            )
        } else {
            format!(
                "{} has no registered tool named {name}; inspect /{} tools",
                provider.label(),
                if provider == Provider::Claude {
                    "cc"
                } else {
                    provider.label()
                }
            )
        });
    };
    let tool = registry.lookup(id)?;
    if tool.provider != provider {
        return Err("registered tool provider changed; refresh the tools list".into());
    }
    registry.validate(&tool)?;
    Ok(tool)
}

fn list(
    registry: &Registry,
    provider: Provider,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    if !(1..=32).contains(&limit) {
        return Err("harness tool page limit must be in 1..=32".into());
    }
    let names = names(registry, provider)?;
    if offset > names.len() {
        return Err("harness tool offset exceeds the current catalog length".into());
    }
    let mut tools = Vec::new();
    for id in names.iter().skip(offset).take(limit) {
        let tool = registry.lookup(id)?;
        if tool.provider != provider {
            return Err("harness catalog changed; refresh it".into());
        }
        let description = tool.description.as_deref().unwrap_or("");
        let preview = description.chars().take(180).collect::<String>();
        tools.push(json!({
            "name":tool.id,
            "native_name":tool.native_name,
            "description_preview":preview,
            "description_complete":description.chars().count() <= 180,
        }));
    }
    let next = offset + tools.len();
    Ok(json!({
        "provider":provider.label(),
        "generation":registry.generation(),
        "tools":tools,
        "total":names.len(),
        "offset":offset,
        "next_offset":(next < names.len()).then_some(next),
        "automatic_install":false,
        "automatic_model_turn":false,
    }))
}

pub(crate) fn provider_tools(
    provider_name: &str,
    offset: usize,
    limit: usize,
) -> Result<Value, String> {
    let runtime = runtime()?;
    list(
        &runtime.local_registry(),
        provider(provider_name)?,
        offset,
        limit,
    )
}

pub(crate) fn inspect(provider_name: &str, name: &str) -> Result<Value, String> {
    let runtime = runtime()?;
    Ok(resolve(&runtime.local_registry(), provider(provider_name)?, name)?.descriptor)
}

#[derive(Debug, PartialEq)]
enum Action {
    Tools(usize),
    Inspect(String),
    Call { name: String, arguments: Value },
}

fn parse(raw: &str) -> Result<Action, String> {
    if raw.len() > MAX_INPUT {
        return Err("harness command exceeds 16384 UTF-8 bytes".into());
    }
    if raw
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err("harness command contains unsupported control characters".into());
    }
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Action::Tools(0));
    }
    let (name, rest) = raw
        .split_once(char::is_whitespace)
        .map_or((raw, ""), |(name, rest)| (name, rest.trim()));
    if name == "tools" {
        return Ok(Action::Tools(if rest.is_empty() {
            0
        } else {
            rest.parse::<usize>()
                .map_err(|_| "tools accepts only an optional page offset")?
        }));
    }
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err("expected a registered tool name followed by a JSON argument object".into());
    }
    if rest.is_empty() {
        return Ok(Action::Inspect(name.to_owned()));
    }
    let arguments: Value = serde_json::from_str(rest).map_err(
        |_| "tool arguments must be one valid JSON object; omit them to inspect the native schema",
    )?;
    if !arguments.is_object() {
        return Err("tool arguments must be a JSON object".into());
    }
    Ok(Action::Call {
        name: name.to_owned(),
        arguments,
    })
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

fn omit_binary(value: &mut Value) -> bool {
    match value {
        Value::Object(object) => {
            let image = object.get("type").and_then(Value::as_str) == Some("image");
            let mut omitted = false;
            for (key, item) in object.iter_mut() {
                if (image && key == "data") || matches!(key.as_str(), "data_base64" | "blob") {
                    if let Some(data) = item.as_str() {
                        *item = json!(format!(
                            "[{} encoded bytes; use the complete native result]",
                            data.len()
                        ));
                        omitted = true;
                    }
                } else {
                    omitted |= omit_binary(item);
                }
            }
            omitted
        }
        Value::Array(values) => {
            let mut omitted = false;
            for item in values {
                omitted |= omit_binary(item);
            }
            omitted
        }
        _ => false,
    }
}

struct Prepared {
    text: String,
    #[cfg(test)]
    artifact: Option<ArtifactMetadata>,
}

fn prepare(output: &Arc<OutputStore>, value: Value) -> Result<Prepared, String> {
    let raw = serde_json::to_vec(&value).map_err(|_| "cannot encode native harness result")?;
    let mut visible = value;
    let binary = omit_binary(&mut visible);
    let pretty = serde_json::to_string_pretty(&visible)
        .map_err(|_| "cannot format native harness result")?;
    let artifact = if binary || pretty.len() > MAX_PREVIEW {
        let mut capture = output.capture("local_harness_result_json");
        capture.spill()?;
        capture.push(&raw)?;
        capture.finish()?;
        capture.metadata()
    } else {
        None
    };
    let prefix = bounded(&pretty, MAX_PREVIEW);
    let mut text = format!("\u{0060}\u{0060}\u{0060}json\n{prefix}\n\u{0060}\u{0060}\u{0060}");
    if prefix.len() != pretty.len() {
        text.push_str("\n\nPreview shortened at a UTF-8 boundary.");
    }
    if let Some(metadata) = &artifact {
        text.push_str(&format!(
            "\n\nComplete native JSON is retained in memory.\n\nArtifact ID: {}\nSHA-256: {}\nBytes: {}\nRead with ssmcp.output.read before it expires. Exit or reload clears it.",
            metadata.id, metadata.sha256, metadata.bytes
        ));
    }
    Ok(Prepared {
        text,
        #[cfg(test)]
        artifact,
    })
}

fn tool_list_message(provider: Provider, page: &Value) -> String {
    let shortcut = if provider == Provider::Claude {
        "cc"
    } else {
        provider.label()
    };
    let mut text = format!("## /{shortcut} · registered tools\n\n");
    let tools = page["tools"].as_array().cloned().unwrap_or_default();
    if tools.is_empty() {
        text.push_str("No tools from this harness are currently registered. Inspect ssmcp.harnesses.list for its cached status. This explicit command loaded only the selected optional harness; it did not install software or request a model turn.\n");
    }
    for tool in tools {
        let name = tool["name"].as_str().unwrap_or("");
        let description = tool["description_preview"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        text.push_str(&format!("- {name} — {description}\n"));
    }
    if let Some(next) = page["next_offset"].as_u64() {
        text.push_str(&format!("\nNext page: /{shortcut} tools {next}\n"));
    }
    text.push_str(&format!("\nInspect a schema: /{shortcut} TOOL\nCall only with explicit arguments: /{shortcut} TOOL {{...}}"));
    if provider == Provider::Pi {
        text.push_str("\n\nPi RPC exposes its actual control protocol. get_commands lists slash commands, skills and templates; it is not a catalog of internal LSP or model tools.");
    }
    text
}

async fn execute(
    runtime: Arc<Runtime>,
    provider: Provider,
    action: Action,
    call_id: u64,
) -> Result<(), String> {
    // A local slash command explicitly opts into one optional harness. Use the
    // same registered native entry point so hooks, auditing and failure caching apply.
    runtime
        .perform(Operation::CallTool {
            name: "ssmcp.harnesses.load".into(),
            arguments: json!({"provider":provider.label()}),
        })
        .await?;
    let registry = runtime.local_registry();
    match action {
        Action::Tools(offset) => {
            let page = list(&registry, provider, offset, PAGE_SIZE)?;
            super::super::render::message(&tool_list_message(provider, &page));
        }
        Action::Inspect(name) => {
            let tool = resolve(&registry, provider, &name)?;
            let prepared = prepare(&runtime.output(), tool.descriptor)?;
            super::super::render::message(&format!(
                "## {} · native schema\n\n{}",
                tool.id, prepared.text
            ));
        }
        Action::Call { name, arguments } => {
            let tool = resolve(&registry, provider, &name)?;
            registry.validate(&tool)?;
            super::super::render::message(&format!(
                "## {} · request {call_id}\n\nCalling the registered native primitive. You can continue typing while it runs.",
                tool.id
            ));
            let value = runtime
                .perform(Operation::CallTool {
                    name: tool.id.clone(),
                    arguments,
                })
                .await?;
            let status = if value["isError"] == true {
                "returned an error"
            } else {
                "returned"
            };
            let prepared = prepare(&runtime.output(), value)?;
            super::super::render::message(&format!(
                "## {} · request {call_id} {status}\n\n{}",
                tool.id, prepared.text
            ));
        }
    }
    Ok(())
}

/// Called only for a local user's explicit slash command. Never waits for a tool.
pub(crate) fn submit(provider_name: &str, raw: &str) -> Result<(), String> {
    let provider = provider(provider_name)?;
    let action = parse(raw)?;
    let binding = binding()?;
    let runtime = binding
        .runtime
        .upgrade()
        .ok_or("harness connection is no longer available")?;
    runtime.local_registry().ensure_open()?;
    let handle = binding
        .handle
        .ok_or("the live client async runtime is not available")?;
    let slot = Slot::acquire()?;
    let call_id = NEXT_CALL.fetch_add(1, Ordering::Relaxed);
    let source = crate::runtime::call_scope::Source::capture();
    handle.spawn(source.scope(async move {
        let _slot = slot;
        if let Err(error) = execute(runtime, provider, action, call_id).await {
            super::super::render::message(&format!(
                "## {} · request {call_id}\n\n{}",
                provider.label(),
                bounded(&error, 4096)
            ));
        }
    }));
    Ok(())
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
    text.push_str("\nRefresh: /tasks\nStop a job: /tasks cancel JOB_ID\nInstalled harness sessions retain their own task/approval interfaces.");
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

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Tools {
        provider: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Inspect {
        provider: String,
        tool: String,
    },
}

pub(super) fn call(arguments: Value) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "expected a harness tools or inspect action")?;
    match request {
        Request::Tools {
            provider,
            offset,
            limit,
        } => provider_tools(&provider, offset.unwrap_or(0), limit.unwrap_or(PAGE_SIZE)),
        Request::Inspect { provider, tool } => inspect(&provider, &tool),
    }
}

pub(super) fn tool() -> Value {
    let provider = json!({"type":"string","enum":["cc","claude","codex","pi"]});
    json!({
        "name":"ssmcp.ui.harness",
        "description":"Inspect only the current live registry for one harness. tools returns a bounded page of actual names and description previews; inspect returns one unchanged registered descriptor, including its original inputSchema. cc is the local alias for Claude's canonical registry namespace. Pi RPC is a control protocol and does not enumerate internal LSP/model/plugin tools. This observation never loads a harness, installs an extension, or starts a model turn. Optional harnesses begin not_loaded; explicitly call ssmcp.harnesses.load before inspecting their catalog. The local user's /cc, /codex and /pi commands load only the selected harness asynchronously. The local user may explicitly call a discovered primitive with /cc TOOL {...}, /codex TOOL {...} or /pi TOOL {...}; those calls run asynchronously and retain native approvals. Empty slash commands show a tools page.",
        "inputSchema":{"type":"object","oneOf":[
            {"type":"object","required":["action","provider"],"additionalProperties":false,"properties":{
                "action":{"const":"tools"},"provider":provider,
                "offset":{"type":"integer","minimum":0,"default":0},
                "limit":{"type":"integer","minimum":1,"maximum":32,"default":16}
            }},
            {"type":"object","required":["action","provider","tool"],"additionalProperties":false,"properties":{
                "action":{"const":"inspect"},"provider":provider,
                "tool":{"type":"string","minLength":1,"maxLength":128}
            }}
        ]},
        "annotations":{"readOnlyHint":true,"destructiveHint":false}
    })
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

    fn tool(name: &str) -> Value {
        json!({"name":name,"description":"Native description remains unchanged.",
            "inputSchema":{"type":"object","required":["input"],"properties":{"input":{"type":"string"}},"additionalProperties":false}})
    }

    #[test]
    fn only_explicit_json_arguments_invoke_tools() {
        assert_eq!(parse("").unwrap(), Action::Tools(0));
        assert_eq!(parse("tools 16").unwrap(), Action::Tools(16));
        assert_eq!(parse("LSP").unwrap(), Action::Inspect("LSP".into()));
        assert_eq!(
            parse("rpc {\"type\":\"get_commands\",\"args\":{}}").unwrap(),
            Action::Call {
                name: "rpc".into(),
                arguments: json!({"type":"get_commands","args":{}})
            }
        );
        for raw in [
            "tools -1",
            "tools 4 extra",
            "rpc []",
            "rpc true",
            "rpc not-json",
            "../run {}",
            "rpc {}\u{001b}",
        ] {
            assert!(parse(raw).is_err());
        }
        assert!(parse(&"x".repeat(MAX_INPUT + 1)).is_err());
    }

    #[test]
    fn provider_names_resolve_only_to_real_registered_routes() {
        let registry = Registry::new();
        registry
            .replace(Provider::Claude, vec![tool("cc.LSP")])
            .unwrap();
        registry
            .replace(Provider::Codex, vec![tool("codex.command_exec")])
            .unwrap();
        registry
            .replace(Provider::Pi, vec![tool("pi.rpc")])
            .unwrap();
        for alias in ["LSP", "cc.LSP", "claude.LSP"] {
            let selected = resolve(&registry, Provider::Claude, alias).unwrap();
            assert_eq!(selected.id, "claude.LSP");
            assert_eq!(selected.native_name, "cc.LSP");
        }
        assert!(resolve(&registry, Provider::Claude, "codex.command_exec").is_err());
        assert!(resolve(&registry, Provider::Pi, "lsp").is_err());
        assert!(resolve(&registry, Provider::Pi, "get_commands").is_err());
        assert_eq!(
            resolve(&registry, Provider::Pi, "rpc").unwrap().id,
            "pi.rpc"
        );
    }

    #[test]
    fn metadata_pages_preserve_native_schema_on_demand() {
        let registry = Registry::new();
        let original = tool("codex.command_exec");
        registry
            .replace(Provider::Codex, vec![original.clone(), tool("codex.rpc")])
            .unwrap();
        let page = list(&registry, Provider::Codex, 0, 1).unwrap();
        assert_eq!(page["total"], 2);
        assert_eq!(page["next_offset"], 1);
        assert!(page["tools"][0].get("inputSchema").is_none());
        let selected = resolve(&registry, Provider::Codex, "command_exec").unwrap();
        assert_eq!(selected.descriptor, original);
        assert_eq!(selected.input_schema, original["inputSchema"]);
        assert!(list(&registry, Provider::Codex, 3, 1).is_err());
        registry.invalidate(Provider::Codex);
        assert!(registry.validate(&selected).is_err());
        assert_eq!(list(&registry, Provider::Codex, 0, 16).unwrap()["total"], 0);
    }

    #[test]
    fn ambiguous_short_names_require_a_fully_qualified_route() {
        let registry = Registry::new();
        registry
            .replace(
                Provider::Pi,
                vec![tool("pi.events"), tool("ssmcp.pi.events")],
            )
            .unwrap();
        assert!(resolve(&registry, Provider::Pi, "events").is_err());
        assert_eq!(
            resolve(&registry, Provider::Pi, "ssmcp.pi.events")
                .unwrap()
                .id,
            "ssmcp.pi.events"
        );
    }

    #[test]
    fn binary_preview_omission_does_not_change_the_original_value() {
        let original = json!({"content":[{"type":"image","mimeType":"image/png","data":"private-base64"}],
            "structuredContent":{"data_base64":"more-private-bytes","result":"ordinary text"}});
        let mut visible = original.clone();
        assert!(omit_binary(&mut visible));
        assert!(!visible.to_string().contains("private-base64"));
        assert!(!visible.to_string().contains("more-private-bytes"));
        assert_eq!(original["content"][0]["data"], "private-base64");
        assert_eq!(visible["structuredContent"]["result"], "ordinary text");
        assert_eq!(bounded("你好world", 5), "你");
    }

    #[tokio::test]
    async fn preview_has_an_exact_complete_memory_output_when_native_output_is_large() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let output = OutputStore::new_connection().unwrap();
        let value = json!({"large":"界".repeat(MAX_PREVIEW)});
        let prepared = prepare(&output, value.clone()).unwrap();
        let metadata = prepared.artifact.unwrap();
        assert!(metadata.complete);
        assert!(metadata.path.is_none());
        assert_eq!(metadata.storage, "memory");
        let stat = output
            .perform("ssmcp.output.stat", json!({"id":metadata.id}))
            .await
            .unwrap();
        assert_eq!(stat["complete"], true);
        assert!(stat["path"].is_null());
        let mut raw = Vec::new();
        let mut offset = 0;
        loop {
            let chunk = output
                .perform(
                    "ssmcp.output.read",
                    json!({"id":metadata.id,"offset":offset,"max_bytes":257}),
                )
                .await
                .unwrap();
            raw.extend(
                STANDARD
                    .decode(chunk["data_base64"].as_str().unwrap())
                    .unwrap(),
            );
            let next = chunk["next_offset"].as_u64().unwrap();
            assert!(next > offset || chunk["eof"] == true);
            offset = next;
            if chunk["eof"] == true {
                break;
            }
        }
        assert_eq!(offset, metadata.bytes);
        assert_eq!(raw, serde_json::to_vec(&value).unwrap());
        assert!(prepared.text.contains("retained in memory"));
        assert!(prepared.text.contains("Preview shortened"));
        assert!(prepared.text.len() < 12 * 1024);
    }
}
