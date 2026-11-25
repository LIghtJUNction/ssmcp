//! Current-user machine overview through the existing authenticated attach IPC.
//! Discovery never grants authority and observations never replay commands.
mod discovery;
mod metrics;
pub(crate) use discovery::Registration;
pub(super) use discovery::{foreground_rows, foreground_target};

use super::attach::{self, RemoteFrontend};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    time::Duration,
};
use zbridge::{Capabilities, Id, Operation, Request, Response, ResponseBody};

const HELP: &str = r#"Usage: ssmcp overview [--json] [--watch]
       ssmcp overview show TARGET
       ssmcp overview jobs TARGET
       ssmcp overview state TARGET RESOURCE [JSON_OBJECT]
       ssmcp overview exec TARGET -- PROGRAM [ARG ...]
       ssmcp overview cancel TARGET JOB_ID
       ssmcp overview command TARGET COMMAND JSON_OBJECT

Observe this OS user's registered foreground and saved local sessions. TARGET
is the exact saved:NAME or live:INSTANCE shown in the overview. Labels are not
selectors. Existing clients need a normal restart to publish foreground state.

--watch refreshes every two seconds. --json emits one bounded object per sample.
CPU is measured for the client process, where 100% means one busy CPU core;
the first sample is pending. Linux RSS is bytes. Unsupported metrics are null.
Agent/model usage and child-process usage are not inferred from client CPU.

Commands use the selected session's private input lease and current generation.
The native tool hooks and policy still apply. Exec returns the actual job receipt;
jobs reads the bounded registry without output or argv. A lost command receipt
is unknown: inspect the original job/session and never automatically repeat it.
No account login, cloud broadcast, service start or model start is performed.
"#;

fn request(operation: Operation) -> crate::Result<Request> {
    Ok(Request {
        version: zbridge::VERSION,
        id: Id::new(super::random_suffix()?)?,
        operation,
    })
}

async fn capabilities(frontend: &RemoteFrontend) -> crate::Result<(Id, Capabilities)> {
    let response = frontend
        .request(request(Operation::Capabilities {})?)
        .await?;
    match response.body {
        ResponseBody::Capabilities(value) => Ok((
            response.generation.ok_or("missing backend generation")?,
            value,
        )),
        ResponseBody::Error(error) => Err(error.into()),
        _ => Err("unexpected capability receipt".into()),
    }
}

async fn resource(
    frontend: &RemoteFrontend,
    generation: &Id,
    name: &str,
    query: Value,
) -> crate::Result<Value> {
    let response = frontend
        .request(request(Operation::State {
            generation: generation.clone(),
            resource: Id::new(name)?,
            query,
        })?)
        .await?;
    match response.body {
        ResponseBody::State(value) => Ok(value.value),
        ResponseBody::Error(error) => Err(error.into()),
        _ => Err("unexpected state receipt".into()),
    }
}

async fn probe(entry: &discovery::Entry) -> crate::Result<Value> {
    if !entry.process_matches() {
        return Ok(entry.empty("offline"));
    }
    let mut frontend = RemoteFrontend::connect(entry.directory.clone(), true).await?;
    let (generation, available) = capabilities(&frontend).await?;
    let mut result = entry.empty("connected");
    result["generation"] = json!(generation.as_str());
    let runtime = resource(&frontend, &generation, "runtime", json!({})).await?;
    let pid = runtime["process_id"]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok());
    if entry.pid.is_some() && pid != entry.pid {
        return Err("live endpoint process differs from its registered process".into());
    }
    result["pid"] = json!(pid);
    result["state"] = json!(if runtime["backend_active"] == true {
        "connected"
    } else {
        "connecting"
    });
    result["client_id"] = runtime["client_id"].clone();
    result["commands"] = json!(
        available
            .commands
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>()
    );
    let mut unavailable = Vec::new();
    for (name, key, query) in [
        ("threads", "agents", json!({"limit":32})),
        ("workers", "workers", json!({"limit":32})),
        ("executions", "executions", json!({})),
    ] {
        if available
            .resources
            .iter()
            .any(|value| value.id.as_str() == name)
        {
            match resource(&frontend, &generation, name, query).await {
                Ok(value) => result[key] = value,
                Err(_) => unavailable.push(name),
            }
        }
    }
    result["unavailable"] = json!(unavailable);
    result["detached"] = json!(frontend.close().await.is_ok());
    Ok(result)
}

async fn snapshot(sampler: &mut metrics::Sampler) -> crate::Result<Value> {
    let inventory = discovery::scan()?;
    let targets = inventory
        .entries
        .iter()
        .map(|entry| entry.target.clone())
        .collect::<Vec<_>>();
    sampler.retain(&targets);
    let mut pending = stream::iter(inventory.entries.iter().cloned().map(|entry| async move {
        let result = tokio::time::timeout(Duration::from_secs(2), probe(&entry)).await;
        let value = match result {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => entry.empty("unavailable"),
            Err(_) => entry.empty("unknown"),
        };
        (entry.target, value)
    }))
    .buffer_unordered(8);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut observed = std::collections::BTreeMap::new();
    while let Ok(Some((target, mut value))) =
        tokio::time::timeout_at(deadline, pending.next()).await
    {
        if (value["state"] == "connected" || value["state"] == "connecting")
            && let Some(pid) = value["pid"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
        {
            value["metrics"] = sampler.observe(&target, pid);
        }
        observed.insert(target, value);
    }
    drop(pending);
    let sessions = inventory
        .entries
        .iter()
        .map(|entry| {
            observed
                .remove(&entry.target)
                .unwrap_or_else(|| entry.empty("unknown"))
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"schema":"ssmcp.overview.v1","observed_at_unix":super::now(),"sessions":sessions,
        "scope":"current_os_user","max_sessions":discovery::MAX_ENTRIES,
        "truncated":inventory.truncated,"skipped_invalid_entries":inventory.skipped,
        "logs_omitted":true,"automatic_command_retry":false}),
    )
}

fn clean(value: &str, limit: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

fn print_table(snapshot: &Value) {
    println!(
        "{:<38} {:<24} {:<12} {:>8} {:>9} {:>7} {:>7}",
        "TARGET", "NAME", "STATE", "CPU/core", "RSS MiB", "AGENTS", "WORKERS"
    );
    if let Some(sessions) = snapshot["sessions"].as_array() {
        for session in sessions {
            let cpu = session["metrics"]["cpu_percent_one_core"]
                .as_f64()
                .map(|value| format!("{value:.1}%"))
                .unwrap_or_else(|| "—".into());
            let rss = session["metrics"]["rss_bytes"]
                .as_u64()
                .map(|value| format!("{:.1}", value as f64 / 1_048_576.0))
                .unwrap_or_else(|| "—".into());
            let agents = session["agents"]["agents"]
                .as_array()
                .map(|values| {
                    format!(
                        "{}{}",
                        values.len(),
                        if session["agents"]["has_more"] == true {
                            "+"
                        } else {
                            ""
                        }
                    )
                })
                .unwrap_or_else(|| "—".into());
            let workers = session["workers"]["registered_workers"]
                .as_u64()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "—".into());
            println!(
                "{:<38} {:<24} {:<12} {:>8} {:>9} {:>7} {:>7}",
                clean(session["target"].as_str().unwrap_or(""), 140),
                clean(session["name"].as_str().unwrap_or(""), 24),
                session["state"].as_str().unwrap_or("unknown"),
                cpu,
                rss,
                agents,
                workers
            );
        }
        if sessions.is_empty() {
            println!(
                "No registered local sessions. Existing foreground clients publish after a normal restart."
            );
        }
    }
    if snapshot["truncated"] == true {
        println!("Inventory limit reached; select an exact known target for details.");
    }
    println!(
        "Use overview show TARGET for details. — means unavailable or awaiting another CPU sample."
    );
}

async fn selected(args: &[String]) -> crate::Result<()> {
    if args.len() < 2 {
        return Err(HELP.into());
    }
    let action = &args[0];
    let entry = discovery::select(&args[1])?;
    if action == "show" {
        if args.len() != 2 {
            return Err(HELP.into());
        }
        let mut value = tokio::time::timeout(Duration::from_secs(3), probe(&entry)).await??;
        if (value["state"] == "connected" || value["state"] == "connecting")
            && let Some(pid) = value["pid"]
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
        {
            value["metrics"] = metrics::Sampler::default().observe(&entry.target, pid);
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    let (command, arguments) = match action.as_str() {
        "jobs" if args.len() == 2 => (None, json!({})),
        "state" if args.len() == 3 || args.len() == 4 => (
            None,
            if args.len() == 4 {
                serde_json::from_str::<Value>(&args[3])?
            } else {
                json!({})
            },
        ),
        "exec" if args.len() >= 4 && args[2] == "--" => {
            (Some("exec.start".to_owned()), json!({"command":&args[3..]}))
        }
        "cancel" if args.len() == 3 => (Some("exec.cancel".to_owned()), json!({"job_id":args[2]})),
        "command" if args.len() == 4 => (
            Some(args[2].clone()),
            serde_json::from_str::<Value>(&args[3])?,
        ),
        _ => return Err(HELP.into()),
    };
    let mut frontend = tokio::time::timeout(
        Duration::from_secs(3),
        RemoteFrontend::connect(entry.directory.clone(), command.is_none()),
    )
    .await??;
    let (generation, available) =
        tokio::time::timeout(Duration::from_secs(3), capabilities(&frontend)).await??;
    let result = if let Some(command) = command {
        if !available
            .commands
            .iter()
            .any(|value| value.id.as_str() == command)
        {
            return Err("the exact selected session does not grant this command".into());
        }
        let operation = Operation::Command {
            generation: generation.clone(),
            command: Id::new(command)?,
            arguments,
        };
        let request = request(operation)?;
        let request_id = request.id.clone();
        let outcome =
            tokio::time::timeout(Duration::from_secs(12), frontend.request(request)).await;
        let (receipt, unknown) = match outcome {
            Ok(Ok(Response {
                body: ResponseBody::Command(value),
                ..
            })) => (json!({"outcome":"received","receipt":value.value}), false),
            Ok(Ok(Response {
                body: ResponseBody::Error(error),
                ..
            }))
            | Ok(Err(error)) => {
                let unknown = error.effect == zbridge::Effect::MayHaveOccurred;
                (
                    json!({"outcome":if unknown{"unknown"}else{"rejected"},"error":error}),
                    unknown,
                )
            }
            _ => (
                json!({"outcome":"unknown","error":"command receipt unavailable"}),
                true,
            ),
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"target":entry.target,"generation":generation,
            "request_id":request_id,"result":receipt,"automatic_retry":false}))?
        );
        if unknown {
            Err("command outcome unknown; inspect original receipts, do not automatically repeat it".into())
        } else if receipt["outcome"] == "rejected" || receipt["receipt"]["isError"] == true {
            Err("command was rejected; inspect its native receipt".into())
        } else {
            Ok(())
        }
    } else {
        let name = if action == "state" {
            args[2].as_str()
        } else {
            "executions"
        };
        let value = tokio::time::timeout(
            Duration::from_secs(3),
            resource(&frontend, &generation, name, arguments),
        )
        .await??;
        println!("{}", serde_json::to_string_pretty(&value)?);
        Ok(())
    };
    let _ = tokio::time::timeout(Duration::from_secs(2), frontend.close()).await;
    result
}

pub async fn run(arguments: Vec<OsString>) -> crate::Result<()> {
    let args = arguments
        .iter()
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .ok_or("overview arguments must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if args.iter().any(|value| value == "--help" || value == "-h") {
        print!("{HELP}");
        return Ok(());
    }
    if args.first().is_some_and(|value| !value.starts_with('-')) {
        return selected(&args).await;
    }
    let mut json_output = false;
    let mut watch = false;
    for arg in &args {
        match arg.as_str() {
            "--json" => json_output = true,
            "--watch" => watch = true,
            _ => return Err(HELP.into()),
        }
    }
    let mut sampler = metrics::Sampler::default();
    loop {
        let snapshot = snapshot(&mut sampler).await?;
        if json_output {
            println!("{}", serde_json::to_string(&snapshot)?);
        } else {
            if watch && io::stdout().is_terminal() {
                print!("\x1b[2J\x1b[H");
            }
            print_table(&snapshot);
        }
        io::stdout().flush()?;
        if !watch {
            return Ok(());
        }
        tokio::select! { _=tokio::time::sleep(Duration::from_secs(2))=>{}, signal=tokio::signal::ctrl_c()=>{signal?;return Ok(());} }
    }
}
