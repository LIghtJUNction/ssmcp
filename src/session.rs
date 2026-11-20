//! Saved per-user sessions, supervised by one persistent service or a legacy
//! native session definition. No client process detaches itself.
//!
//! The native manager owns process lifetime; the client owns reconnect policy
//! and connection-scoped tools/tasks. A saved session is not a persistent task.
pub mod attach;
pub mod control;
pub mod install;
pub(crate) mod ipc;
pub mod service;
#[cfg(windows)]
pub mod windows_service;

use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::{Result, platform};

const FORMAT: u32 = 1;
// Configuration locations contain no credentials. Native service workers need
// the same account and trust store as the process that created their session.
const INHERITED_ENVIRONMENT: &[&str] = &[
    "PATH",
    "SSH_AUTH_SOCK",
    "XDG_CONFIG_HOME",
    "SSMCP_AUTH_HOME",
    "SSMCP_CLIENT_CONFIG",
];
const MAX_DOCUMENT: usize = 128 * 1024;
const LOG_BYTES: u64 = 256 * 1024;
const MANAGER_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSpec {
    pub name: String,
    pub cwd: PathBuf,
    pub args: Vec<String>,
    pub watchdog: bool,
    pub reconnect_delay_secs: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    #[serde(default)]
    supervised: bool,
    spec: SessionSpec,
    executable_sha256: String,
    version: String,
    manager_id: String,
    created_at: u64,
    environment: BTreeMap<String, String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Observation {
    phase: String,
    attempt: u64,
    retry_after_secs: Option<u64>,
    connection: Option<Value>,
    error: Option<Value>,
    pid: u32,
    observed_at: u64,
    #[serde(default)]
    generation: Option<String>,
}

#[derive(Default)]
struct NativeStatus {
    state: String,
    active: bool,
    pid: Option<u32>,
    exit_code: Option<i64>,
    started_at: Option<u64>,
    generation: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunGeneration {
    generation: String,
    started_at: u64,
    supervisor_pid: u32,
    #[serde(default)]
    service_epoch: String,
}

/// Held for the entire service process. The file lock prevents duplicate runners
/// even when a delayed manager start races an explicit restart.
pub struct Runner {
    manifest: Manifest,
    directory: PathBuf,
    _run_lock: File,
    generation: Option<String>,
    owner: Option<service::OwnerLease>,
}

impl Runner {
    pub fn open(name: &str) -> Result<Self> {
        let root = session_root(false)?;
        let (directory, manifest) = load(&root, name)?;
        let lock = lock(&directory.join("run.lock"), true)?;
        let (generation, owner) = if manifest.supervised {
            let active: RunGeneration = read_json(&directory.join("generation.json"))?;
            let inherited = std::env::var("SSMCP_SESSION_GENERATION")
                .map_err(|_| "supervised session must be started by its service")?;
            if inherited != active.generation
                || inherited.len() != 32
                || !inherited.bytes().all(|c| c.is_ascii_hexdigit())
            {
                return Err("session generation does not match its supervisor".into());
            }
            let owner = service::OwnerLease::start(root, active.service_epoch)?;
            (Some(inherited), Some(owner))
        } else {
            (None, None)
        };
        let image = executable(&directory);
        if checksum(&image)? != manifest.executable_sha256
            || checksum(&std::env::current_exe()?)? != manifest.executable_sha256
        {
            return Err(
                "session executable changed; remove and recreate the session to upgrade".into(),
            );
        }
        // The native definition restores only the approved environment references.
        // Opening the runner does not mutate process-global environment state.
        let runner = Self {
            manifest,
            directory,
            _run_lock: lock,
            generation,
            owner,
        };
        if !runner.stop_requested() {
            runner.record("starting", 0, None, None, None)?;
        }
        Ok(runner)
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn spec(&self) -> &SessionSpec {
        &self.manifest.spec
    }

    pub fn stop_requested(&self) -> bool {
        if self.owner.as_ref().is_some_and(service::OwnerLease::lost) {
            return true;
        }
        // Any unexpected filesystem error fails closed. A missing marker alone
        // means that an explicit start is still desired.
        match fs::symlink_metadata(self.directory.join("stop.requested")) {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        }
    }

    pub fn record(
        &self,
        phase: &str,
        attempt: u64,
        retry_after_secs: Option<u64>,
        connection: Option<&Value>,
        error: Option<&str>,
    ) -> Result<()> {
        if !matches!(
            phase,
            "starting" | "connected" | "reconnecting" | "disconnected" | "stopped"
        ) {
            return Err("invalid session lifecycle phase".into());
        }
        let _guard = lock(&self.directory.join("state.lock"), false)?;
        let phase = if self.stop_requested() {
            "stopped"
        } else {
            phase
        };
        let now = now();
        if phase == "connected" {
            if let Some(code) = connection
                .and_then(|v| v.get("login_code"))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= 4096)
            {
                write_json(
                    &self.directory.join("auth.json"),
                    &json!({
                        "login_code":code, "observed_at":now,
                    }),
                )?;
            } else {
                remove_file(&self.directory.join("auth.json"))?;
            }
        } else {
            remove_file(&self.directory.join("auth.json"))?;
        }
        let observation = Observation {
            phase: phase.into(),
            attempt,
            retry_after_secs,
            connection: if phase == "connected" {
                connection.map(public_connection)
            } else {
                None
            },
            error: error.map(|text| {
                crate::audit::event(json!({"message":text.chars().take(2048).collect::<String>()}))
                    ["message"]
                    .clone()
            }),
            pid: std::process::id(),
            observed_at: now,
            generation: self.generation.clone(),
        };
        write_json(&self.directory.join("state.json"), &observation)?;
        append_log(&self.directory, &observation)?;
        Ok(())
    }
}

/// Create a new immutable session definition and ask the native user manager to
/// start it. This does not claim that SSH or the broker handshake succeeded.
pub async fn start(spec: SessionSpec) -> Result<Value> {
    validate_spec(&spec)?;
    if service::available().await {
        return service::request(json!({"op":"create","spec":spec})).await;
    }
    let root = session_root(true)?;
    let _guard = lock(&root.join("manage.lock"), true)?;
    native_available().await?;
    let (directory, manifest) = prepare(&root, spec, false)?;
    if let Err(error) = native_start(&directory, &manifest).await {
        write_management_state(&directory, "disconnected", Some(&error.to_string()))?;
        return Err(error);
    }
    status_from(&directory, &manifest).await
}

// Callers hold manage.lock until the definition is committed and start requested.
fn prepare(root: &Path, mut spec: SessionSpec, supervised: bool) -> Result<(PathBuf, Manifest)> {
    validate_spec(&spec)?;
    crate::client::validate_session_spec(&spec)?;
    spec.cwd = spec.cwd.canonicalize()?;
    if !spec.cwd.is_dir() {
        return Err("session working directory is not a directory".into());
    }
    let directory = session_directory(root, &spec.name);
    if directory.try_exists()? {
        return Err(
            "session already exists; use start/restart or remove before changing its configuration"
                .into(),
        );
    }
    let temporary = root.join(format!(".prepare-{}", random_suffix()?));
    platform::private_directory(&temporary)?;
    let prepared = (|| -> Result<Manifest> {
        let executable_sha256 =
            snapshot_executable(&std::env::current_exe()?, &executable(&temporary))?;
        let mut environment = BTreeMap::new();
        for &key in INHERITED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(key) {
                let value = value
                    .into_string()
                    .map_err(|_| "background environment reference is not valid UTF-8")?;
                if value.contains(['\0', '\r', '\n']) {
                    return Err(
                        "background environment reference contains a control character".into(),
                    );
                }
                environment.insert(key.into(), value);
            }
        }
        environment.insert("SSMCP_SESSION_HOME".into(), path_text(root)?.into());
        let manifest = Manifest {
            format: FORMAT,
            supervised,
            manager_id: manager_id(root, &spec.name),
            spec,
            executable_sha256,
            version: env!("CARGO_PKG_VERSION").into(),
            created_at: now(),
            environment,
        };
        write_json(&temporary.join("manifest.json"), &manifest)?;
        Ok(manifest)
    })();
    let manifest = match prepared {
        Ok(value) => value,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    };
    fs::rename(&temporary, &directory)?;
    Ok((directory, manifest))
}

pub async fn manage(mut args: Vec<OsString>) -> Result<()> {
    let json_output = args.iter().any(|arg| arg == "--json");
    args.retain(|arg| arg != "--json");
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "session arguments must be valid UTF-8".into())
        })
        .collect::<Result<_>>()?;
    let operation = args.first().map(String::as_str).unwrap_or("list");
    if operation == "list" {
        if args.len() > 1 {
            return Err("usage: ssmcp sessions list [--json]".into());
        }
        let entries = list().await?;
        if json_output {
            println!("{}", serde_json::to_string_pretty(&entries)?);
        } else if entries.is_empty() {
            println!("No saved background sessions.");
        } else {
            for entry in entries {
                print_status(&entry);
            }
        }
        return Ok(());
    }
    let name = args.get(1).ok_or(
        "usage: ssmcp sessions status|start|close|stop|restart|logs|remove|auth NAME [--json]",
    )?;
    validate_name(name)?;
    if operation == "logs" {
        let lines = match &args[2..] {
            [] => 50,
            [flag, value] if flag == "--lines" => value
                .parse::<usize>()
                .ok()
                .filter(|v| (1..=1000).contains(v))
                .ok_or("--lines must be between 1 and 1000")?,
            _ => return Err("usage: ssmcp sessions logs NAME [--lines N] [--json]".into()),
        };
        let result = logs(name, lines).await?;
        if json_output {
            println!("{}", serde_json::to_string_pretty(&result)?);
        } else {
            if let Some(warning) = result["warning"].as_str() {
                eprintln!("ssmcp: {warning}");
            }
            if let Some(events) = result["events"].as_array() {
                for value in events {
                    if result["source"] == "journal" {
                        match &value["event"] {
                            Value::String(text) => println!("{text}"),
                            event => println!("{}", serde_json::to_string(event)?),
                        }
                    } else {
                        println!(
                            "{}  {}  attempt {}{}{}",
                            value["observed_at"],
                            value["phase"].as_str().unwrap_or("unknown"),
                            value["attempt"],
                            value["retry_after_secs"]
                                .as_u64()
                                .map(|v| format!(" · retry in {v}s"))
                                .unwrap_or_default(),
                            value["error"]
                                .as_str()
                                .map(|v| format!(" · {v}"))
                                .unwrap_or_default()
                        );
                    }
                }
            }
        }
        return Ok(());
    }
    if args.len() != 2 {
        return Err("unexpected session management arguments".into());
    }
    let value = match operation {
        "status" => status(name).await?,
        "start" => start_saved(name).await?,
        "stop" | "close" => stop(name).await?,
        "restart" => restart(name).await?,
        "remove" => remove(name).await?,
        "auth" => auth(name).await?,
        _ => return Err("unknown sessions command; expected list, status, start, close, stop, restart, logs, remove, or auth".into()),
    };
    if json_output || matches!(operation, "status" | "auth") {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print_status(&value);
    }
    Ok(())
}

pub async fn list() -> Result<Vec<Value>> {
    let root = match session_root(false) {
        Ok(root) => root,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|v| v.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let mut definitions = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("session-")
            || entry.file_type()?.is_symlink()
            || !entry.file_type()?.is_dir()
        {
            continue;
        }
        if definitions.len() >= 1024 {
            return Err("too many saved sessions to enumerate safely".into());
        }
        let manifest: Manifest = read_json(&entry.path().join("manifest.json"))?;
        let (directory, manifest) = load(&root, &manifest.spec.name)?;
        definitions.push((directory, manifest));
    }
    // One authenticated snapshot avoids one bounded IPC round trip per saved
    // session, including when the supervisor is unavailable.
    let supervised = if definitions.iter().any(|(_, manifest)| manifest.supervised) {
        Some(service::request(json!({"op":"list"})).await)
    } else {
        None
    };
    let mut values = Vec::new();
    for (directory, manifest) in definitions {
        if manifest.supervised {
            let managed = supervised
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(Value::as_array)
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| entry["name"] == manifest.spec.name)
                });
            if let Some(value) = managed {
                values.push(value.clone());
            } else {
                let error = supervised
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map_or_else(
                        || "supervisor did not return this saved session".into(),
                        ToString::to_string,
                    );
                values.push(status_observed(
                    &directory,
                    &manifest,
                    NativeStatus {
                        state: "unavailable".into(),
                        ..Default::default()
                    },
                    Some(error),
                )?);
            }
        } else {
            values.push(status_from(&directory, &manifest).await?);
        }
    }
    values.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(values)
}

pub async fn status(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    let (directory, manifest) = load(&root, name)?;
    status_from(&directory, &manifest).await
}

pub async fn start_saved(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    if load(&root, name)?.1.supervised {
        return service::request(json!({"op":"start","name":name})).await;
    }
    let _guard = lock(&root.join("manage.lock"), true)?;
    let (directory, manifest) = load(&root, name)?;
    // The definition may have been replaced while this caller acquired the
    // management lock. Route the definition loaded under that lock, releasing
    // it before IPC so the supervisor can perform its serialized operation.
    if manifest.supervised {
        drop(_guard);
        return service::request(json!({"op":"start","name":name})).await;
    }
    if native_status(&manifest).await?.active {
        return Err("session is already running".into());
    }
    let _run = lock(&directory.join("run.lock"), true)?;
    verify_snapshot(&directory, &manifest)?;
    remove_file(&directory.join("stop.requested"))?;
    clear_auth(&directory)?;
    write_management_state(&directory, "starting", None)?;
    drop(_run);
    native_start(&directory, &manifest).await?;
    status_from(&directory, &manifest).await
}

pub async fn stop(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    if load(&root, name)?.1.supervised {
        return service::request(json!({"op":"close","name":name})).await;
    }
    let _guard = lock(&root.join("manage.lock"), true)?;
    let (directory, manifest) = load(&root, name)?;
    // The definition may have been replaced while this caller acquired the
    // management lock. Route the definition loaded under that lock, releasing
    // it before IPC so the supervisor can perform its serialized operation.
    if manifest.supervised {
        drop(_guard);
        return service::request(json!({"op":"close","name":name})).await;
    }
    stop_loaded(&directory, &manifest).await?;
    status_from(&directory, &manifest).await
}

pub async fn restart(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    if load(&root, name)?.1.supervised {
        return service::request(json!({"op":"restart","name":name})).await;
    }
    let _guard = lock(&root.join("manage.lock"), true)?;
    let (directory, manifest) = load(&root, name)?;
    // The definition may have been replaced while this caller acquired the
    // management lock. Route the definition loaded under that lock, releasing
    // it before IPC so the supervisor can perform its serialized operation.
    if manifest.supervised {
        drop(_guard);
        return service::request(json!({"op":"restart","name":name})).await;
    }
    stop_loaded(&directory, &manifest).await?;
    verify_snapshot(&directory, &manifest)?;
    remove_file(&directory.join("stop.requested"))?;
    write_management_state(&directory, "starting", None)?;
    native_start(&directory, &manifest).await?;
    status_from(&directory, &manifest).await
}

pub async fn remove(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    if load(&root, name)?.1.supervised {
        return service::request(json!({"op":"remove","name":name})).await;
    }
    let _guard = lock(&root.join("manage.lock"), true)?;
    let (directory, manifest) = load(&root, name)?;
    // The definition may have been replaced while this caller acquired the
    // management lock. Route the definition loaded under that lock, releasing
    // it before IPC so the supervisor can perform its serialized operation.
    if manifest.supervised {
        drop(_guard);
        return service::request(json!({"op":"remove","name":name})).await;
    }
    stop_loaded(&directory, &manifest).await?;
    native_remove(&manifest).await?;
    // Opening the lock verifies no late runner remains. Close before removing it
    // because Windows does not unlink an open backing file.
    drop(lock(&directory.join("run.lock"), true)?);
    fs::remove_dir_all(&directory)?;
    Ok(
        json!({"name":name,"manager_state":"removed","connection_state":"disconnected","removed":true}),
    )
}

/// Return one explicitly identified source. Native journal records and
/// lifecycle observations are never concatenated and replayed as duplicates.
pub async fn logs(name: &str, count: usize) -> Result<Value> {
    let root = session_root(false)?;
    let (directory, manifest) = load(&root, name)?;
    let limit = count.clamp(1, 1000);
    if manifest.supervised {
        return Ok(json!({"source":"lifecycle","fallback":false,
            "coverage":"lifecycle_only","events":lifecycle_logs(&directory,limit)?,
            "limit":limit,"truncated":false,
            "warning":"Lifecycle observations only; attach to the running session for live tool and terminal output."}));
    }
    #[cfg(target_os = "linux")]
    {
        let unit = format!("{}.service", manifest.manager_id);
        let warning = match journal(&unit, limit).await {
            Ok((events, truncated)) if !events.is_empty() => {
                return Ok(json!({"source":"journal","fallback":false,"warning":null,
                    "coverage":"native_events","unit":unit,"events":events,
                    "limit":limit,"truncated":truncated}));
            }
            Ok(_) => "The user journal has no readable records for this unit; showing private lifecycle observations only.".to_owned(),
            Err(error) => format!("The user journal is unavailable ({error}); showing private lifecycle observations only."),
        };
        Ok(
            json!({"source":"lifecycle","fallback":true,"warning":warning,
            "coverage":"lifecycle_only","unit":unit,"events":lifecycle_logs(&directory,limit)?,
            "limit":limit,"truncated":false}),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        Ok(json!({"source":"lifecycle","fallback":false,
            "warning":"This platform records session lifecycle only; this view does not contain the complete native tool event stream.",
            "coverage":"lifecycle_only","unit":manifest.manager_id,
            "events":lifecycle_logs(&directory,limit)?,"limit":limit,"truncated":false}))
    }
}

fn lifecycle_logs(directory: &Path, count: usize) -> Result<Vec<Value>> {
    let mut lines = Vec::new();
    for filename in ["lifecycle.1.jsonl", "lifecycle.jsonl"] {
        let path = directory.join(filename);
        if !path.try_exists()? {
            continue;
        }
        let text = read_private(&path, LOG_BYTES as usize + MAX_DOCUMENT)?;
        for line in text.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            lines.push(serde_json::from_slice(line)?);
        }
    }
    let keep_from = lines.len().saturating_sub(count.clamp(1, 1000));
    Ok(lines.drain(keep_from..).collect())
}

#[cfg(target_os = "linux")]
async fn journal(unit: &str, count: usize) -> Result<(Vec<Value>, bool)> {
    use tokio::io::AsyncReadExt;
    const MAX_BYTES: usize = 1024 * 1024;
    let mut child = Command::new("journalctl")
        .args([
            "--user",
            "--no-pager",
            "--output=json",
            "--reverse",
            "--unit",
            unit,
            "-n",
            &count.to_string(),
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().ok_or("journal stdout is unavailable")?;
    let stderr = child.stderr.take().ok_or("journal stderr is unavailable")?;
    let operation = async {
        let read_output = async {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .await?;
            let truncated = bytes.len() > MAX_BYTES;
            if truncated {
                // Only the owned read-only journal query is stopped. Original
                // records remain in the native journal and can be queried again.
                let _ = child.start_kill();
            }
            let status = child.wait().await?;
            Ok::<_, io::Error>((status, bytes, truncated))
        };
        let read_error = async {
            let mut bytes = Vec::new();
            stderr.take(8192).read_to_end(&mut bytes).await
        };
        let (result, error) = tokio::join!(read_output, read_error);
        error?;
        result
    };
    let (status, mut bytes, truncated) =
        match tokio::time::timeout(MANAGER_TIMEOUT, operation).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = child.kill().await;
                return Err("bounded journal query timed out".into());
            }
        };
    if !status.success() && !truncated {
        return Err("journalctl could not read this user service".into());
    }
    if truncated {
        let complete = bytes[..MAX_BYTES]
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |position| position + 1);
        bytes.truncate(complete);
    }
    let mut events = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let record: Value = serde_json::from_slice(line)?;
        let message = record.get("MESSAGE").cloned().unwrap_or(Value::Null);
        let event = match message {
            Value::String(text) => serde_json::from_str(&text).unwrap_or(Value::String(text)),
            other => other,
        };
        events.push(json!({
            "timestamp_us":record["__REALTIME_TIMESTAMP"].as_str().and_then(|v| v.parse::<u64>().ok()),
            "pid":record["_PID"].as_str().and_then(|v| v.parse::<u32>().ok()),
            "event":crate::audit::event(event),
        }));
    }
    events.reverse();
    Ok((events, truncated))
}

pub async fn auth(name: &str) -> Result<Value> {
    let root = session_root(false)?;
    let (directory, manifest) = load(&root, name)?;
    let status = status_from(&directory, &manifest).await?;
    if status["connection_state"] != "connected" || status["stop_requested"] == true {
        return Err("session has no current connected login code".into());
    }
    let auth: Value = read_json(&directory.join("auth.json"))?;
    Ok(json!({"name":name,"login_code":auth["login_code"],"observed_at":auth["observed_at"]}))
}

async fn stop_loaded(directory: &Path, manifest: &Manifest) -> Result<()> {
    write_json(
        &directory.join("stop.requested"),
        &json!({"requested_at":now()}),
    )?;
    clear_auth(directory)?;
    native_stop(manifest).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if !native_status(manifest).await?.active
            && let Ok(guard) = lock(&directory.join("run.lock"), true)
        {
            drop(guard);
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("session stop was requested but native termination is not confirmed; inspect sessions status".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    write_management_state(directory, "stopped", None)?;
    Ok(())
}

async fn status_from(directory: &Path, manifest: &Manifest) -> Result<Value> {
    if manifest.supervised {
        return match service::request(json!({"op":"status","name":manifest.spec.name})).await {
            Ok(status) => Ok(status),
            Err(error) => status_observed(
                directory,
                manifest,
                NativeStatus {
                    state: "unavailable".into(),
                    ..Default::default()
                },
                Some(error.to_string()),
            ),
        };
    }
    let (native, manager_error) = match native_status(manifest).await {
        Ok(status) => (status, None),
        Err(error) => (
            NativeStatus {
                state: "unknown".into(),
                ..Default::default()
            },
            Some(error.to_string()),
        ),
    };
    status_observed(directory, manifest, native, manager_error)
}

fn status_observed(
    directory: &Path,
    manifest: &Manifest,
    native: NativeStatus,
    manager_error: Option<String>,
) -> Result<Value> {
    let state = match read_json::<Observation>(&directory.join("state.json")) {
        Ok(state) => state,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
        {
            Observation::default()
        }
        Err(error) => return Err(error),
    };
    let stopped = directory.join("stop.requested").try_exists()?;
    let observation_matches = (if manifest.supervised {
        native.generation.is_some() && native.generation == state.generation
    } else {
        native.pid.is_none_or(|pid| pid == state.pid)
    }) && native
        .started_at
        .is_none_or(|started| state.observed_at >= started);
    // Activating/queued/auto-restart is still owned by the manager, but is not
    // evidence that the previous connection survived a crash. Unix managers
    // also identify the current main process; it must match this observation.
    let actually_running = matches!(native.state.as_str(), "active" | "running")
        && (cfg!(windows) || native.pid.is_some());
    let connected = actually_running
        && observation_matches
        && state.pid != 0
        && state.phase == "connected"
        && !stopped;
    let connection_state = if connected {
        "connected"
    } else if stopped || !native.active && manager_error.is_none() {
        "disconnected"
    } else {
        "unknown"
    };
    Ok(json!({
        "name":manifest.spec.name, "cwd":manifest.spec.cwd,
        "manager":if manifest.supervised { "ssmcp-service" } else { manager_name() },
        "supervised":manifest.supervised, "generation":native.generation,
        "manager_id":manifest.manager_id, "manager_state":native.state, "manager_active":native.active,
        "manager_pid":native.pid, "manager_exit_code":native.exit_code, "manager_error":manager_error,
        "connection_state":connection_state, "connection":if connected { state.connection } else { None },
        "phase":if state.phase.is_empty() { "unknown" } else { &state.phase },
        "phase_observed_at":state.observed_at, "attempt":state.attempt,
        "retry_after_secs":state.retry_after_secs, "last_error":state.error, "stop_requested":stopped,
        "watchdog":manifest.spec.watchdog, "reconnect_delay_secs":manifest.spec.reconnect_delay_secs,
        "native_crash_restart_delay_secs":if manifest.spec.watchdog { Some(if manifest.supervised { manifest.spec.reconnect_delay_secs } else { native_restart_delay(manifest) }) } else { None },
        "native_crash_restart_limit":if !manifest.supervised && cfg!(windows) && manifest.spec.watchdog { Some(255) } else { None },
        "automatic_login_start":if manifest.supervised { None } else { Some(false) },
        "scope":if manifest.supervised { "current_user_service" } else if cfg!(windows) || cfg!(target_os="macos") { "current_logged_in_user" } else { "current_user_manager" },
        "version":manifest.version, "executable_sha256":manifest.executable_sha256,
        "created_at":manifest.created_at, "state_directory":directory,
        "logs_path":directory.join("lifecycle.jsonl"),
        "auth_available":connected && directory.join("auth.json").is_file(),
    }))
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.starts_with('-')
        || name.len() > 128
        || name.trim() != name
        || matches!(name, "." | "..")
        || name
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '/' | '\\'))
    {
        return Err("session name must not start with a hyphen and must be 1–128 bytes without control characters, path separators, or surrounding whitespace".into());
    }
    Ok(())
}

fn validate_spec(spec: &SessionSpec) -> Result<()> {
    validate_name(&spec.name)?;
    if !(1..=3600).contains(&spec.reconnect_delay_secs) {
        return Err("reconnect delay must be 1–3600 seconds".into());
    }
    if spec.args.is_empty()
        || spec.args.len() > 256
        || spec
            .args
            .iter()
            .any(|arg| arg.len() > 16_384 || arg.contains('\0'))
    {
        return Err("session requires bounded, validated foreground arguments".into());
    }
    Ok(())
}

fn public_connection(value: &Value) -> Value {
    let mut public = serde_json::Map::new();
    for key in [
        "id",
        "client_id",
        "name",
        "hostname",
        "title",
        "cwd",
        "connected_at",
    ] {
        if let Some(value) = value
            .get(key)
            .filter(|v| v.is_string() || v.is_number() || v.is_boolean())
        {
            public.insert(key.into(), value.clone());
        }
    }
    crate::audit::event(Value::Object(public))
}

fn session_root(create: bool) -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("SSMCP_SESSION_HOME") {
        PathBuf::from(value)
    } else {
        #[cfg(target_os = "linux")]
        {
            let state = match std::env::var_os("XDG_STATE_HOME") {
                Some(path) => PathBuf::from(path),
                None => home()?.join(".local/state"),
            };
            state.join("ssmcp/sessions")
        }
        #[cfg(target_os = "macos")]
        {
            home()?.join("Library/Application Support/ssmcp/sessions")
        }
        #[cfg(windows)]
        {
            PathBuf::from(std::env::var_os("LOCALAPPDATA").ok_or("LOCALAPPDATA is unavailable")?)
                .join("ssmcp/sessions")
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            return Err(
                "native background sessions are supported on Linux, macOS and Windows".into(),
            );
        }
    };
    if !path.is_absolute() {
        return Err("session state directory must be absolute".into());
    }
    if create {
        create_private_tree(&path)?;
    }
    verify_directory(&path)?;
    Ok(path.canonicalize()?)
}

#[cfg(unix)]
fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("HOME must be an absolute directory".into())
}

fn create_private_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                return Ok(());
            }
            return Err("session state path is not a regular directory".into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .ok_or("session state directory has no parent")?;
    if !parent.exists() {
        create_private_tree(parent)?;
    }
    platform::private_directory(path)?;
    Ok(())
}

fn verify_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("session directory must not be a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(
                "session directory must belong to the current user with permissions 0700".into(),
            );
        }
    }
    #[cfg(windows)]
    {
        // Reuse the output store's current-user, protected DACL and reparse
        // checks when reopening an existing session directory.
        drop(platform::windows::private_directory(path)?);
    }
    Ok(())
}

fn session_directory(root: &Path, name: &str) -> PathBuf {
    root.join(format!(
        "session-{:x}",
        crate::digest::Hex(Sha256::digest(name.as_bytes()))
    ))
}

fn manager_id(root: &Path, name: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(root.as_os_str().to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(name.as_bytes());
    format!("ssmcp-session-{:x}", crate::digest::Hex(digest.finalize()))
}

fn load(root: &Path, name: &str) -> Result<(PathBuf, Manifest)> {
    validate_name(name)?;
    let directory = session_directory(root, name);
    verify_directory(&directory)?;
    let manifest: Manifest = read_json(&directory.join("manifest.json"))?;
    validate_spec(&manifest.spec)?;
    if manifest.format != FORMAT
        || manifest.spec.name != name
        || manifest.manager_id != manager_id(root, name)
        || manifest
            .environment
            .get("SSMCP_SESSION_HOME")
            .map(String::as_str)
            != Some(path_text(root)?)
        || manifest.environment.keys().any(|key| {
            key != "SSMCP_SESSION_HOME" && !INHERITED_ENVIRONMENT.contains(&key.as_str())
        })
    {
        return Err("session manifest does not match this user state directory/name".into());
    }
    Ok((directory, manifest))
}

fn executable(directory: &Path) -> PathBuf {
    directory.join(if cfg!(windows) { "ssmcp.exe" } else { "ssmcp" })
}

fn path_text(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or("session path must be valid UTF-8".into())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0; 16];
    platform::random_bytes(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn private_open(path: &Path, write: bool, create: bool, append: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(!append)
        .write(write)
        .create(create)
        .append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err("session file is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(
                "session file must belong to the current user with private permissions".into(),
            );
        }
    }
    #[cfg(windows)]
    platform::windows::verify_file(&file)?;
    Ok(file)
}

pub(crate) fn lock(path: &Path, nonblocking: bool) -> Result<File> {
    let file = private_open(path, true, true, false)?;
    platform::lock_file(&file, nonblocking).map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            "another session operation or runner is already active".into()
        } else {
            crate::Error::from(error)
        }
    })?;
    Ok(file)
}

fn read_private(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = private_open(path, false, false, false)?;
    if file.metadata()?.len() > limit as u64 {
        return Err("session file exceeds its size limit".into());
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err("session file exceeds its size limit".into());
    }
    Ok(bytes)
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    Ok(serde_json::from_slice(&read_private(path, MAX_DOCUMENT)?)?)
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    if bytes.len() > MAX_DOCUMENT {
        return Err("session document exceeds its size limit".into());
    }
    write_private(path, &bytes)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", random_suffix()?));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn remove_file(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn clear_auth(directory: &Path) -> Result<()> {
    let _guard = lock(&directory.join("state.lock"), false)?;
    remove_file(&directory.join("auth.json"))
}

fn write_management_state(directory: &Path, phase: &str, error: Option<&str>) -> Result<()> {
    let _guard = lock(&directory.join("state.lock"), false)?;
    remove_file(&directory.join("auth.json"))?;
    let observation = Observation {
        phase: phase.into(),
        observed_at: now(),
        error: error.map(|e| crate::audit::event(json!({"message":e}))["message"].clone()),
        ..Default::default()
    };
    write_json(&directory.join("state.json"), &observation)?;
    append_log(directory, &observation)
}

fn append_log(directory: &Path, observation: &Observation) -> Result<()> {
    let path = directory.join("lifecycle.jsonl");
    if path.try_exists()? && fs::metadata(&path)?.len() >= LOG_BYTES {
        remove_file(&directory.join("lifecycle.1.jsonl"))?;
        fs::rename(&path, directory.join("lifecycle.1.jsonl"))?;
    }
    let mut file = private_open(&path, true, true, true)?;
    let mut bytes = serde_json::to_vec(observation)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    Ok(())
}

fn snapshot_executable(source: &Path, destination: &Path) -> Result<String> {
    let mut input = File::open(source)?;
    let mut output = private_open(destination, true, true, false)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65_536];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        digest.update(&buffer[..read]);
    }
    output.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        output.set_permissions(fs::Permissions::from_mode(0o500))?;
    }
    Ok(format!("{:x}", crate::digest::Hex(digest.finalize())))
}

fn checksum(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65_536];
    loop {
        let length = file.read(&mut bytes)?;
        if length == 0 {
            break;
        }
        hash.update(&bytes[..length]);
    }
    Ok(format!("{:x}", crate::digest::Hex(hash.finalize())))
}

fn verify_snapshot(directory: &Path, manifest: &Manifest) -> Result<()> {
    let path = executable(directory);
    if fs::symlink_metadata(&path)?.file_type().is_symlink()
        || checksum(&path)? != manifest.executable_sha256
    {
        return Err("saved session executable changed; remove and recreate the session".into());
    }
    if !manifest.spec.cwd.is_dir() {
        return Err("saved session workspace no longer exists".into());
    }
    Ok(())
}

fn print_status(value: &Value) {
    println!(
        "{} · {} · connection {}",
        value["name"].as_str().unwrap_or("session"),
        value["manager_state"].as_str().unwrap_or("unknown"),
        value["connection_state"].as_str().unwrap_or("unknown")
    );
}

fn manager_name() -> &'static str {
    if cfg!(target_os = "linux") {
        "systemd-user"
    } else if cfg!(target_os = "macos") {
        "launchd-user"
    } else if cfg!(windows) {
        "windows-task-scheduler"
    } else {
        "unsupported"
    }
}

fn native_restart_delay(manifest: &Manifest) -> u64 {
    if cfg!(windows) {
        manifest.spec.reconnect_delay_secs.max(60)
    } else {
        manifest.spec.reconnect_delay_secs
    }
}

async fn manager_command(
    program: &str,
    args: &[String],
    allow_failure: bool,
) -> Result<std::process::Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.as_std_mut().creation_flags(0x0800_0000);
    }
    let output = tokio::time::timeout(MANAGER_TIMEOUT, command.output()).await
        .map_err(|_| "native manager call timed out; inspect sessions status before repeating the operation")??;
    if output.stdout.len() + output.stderr.len() > 128 * 1024 {
        return Err("native manager response exceeded its bound".into());
    }
    if !output.status.success() && !allow_failure {
        return Err(format!("{} request failed (exit {:?}); inspect the named user service without changing authentication", Path::new(program).file_name().unwrap_or_default().to_string_lossy(), output.status.code()).into());
    }
    Ok(output)
}

#[cfg(target_os = "linux")]
fn systemctl_args(manifest: &Manifest, operation: &str) -> Vec<String> {
    vec![
        "--user".into(),
        "--no-pager".into(),
        operation.into(),
        format!("{}.service", manifest.manager_id),
    ]
}

async fn native_available() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        manager_command(
            "systemctl",
            &[
                "--user".into(),
                "--no-pager".into(),
                "show".into(),
                "--property=Version".into(),
            ],
            false,
        )
        .await?;
        platform::executable("systemd-run").map_err(crate::Error::from)?;
    }
    #[cfg(target_os = "macos")]
    {
        manager_command("/bin/launchctl", &["manageruid".into()], false).await?;
    }
    #[cfg(windows)]
    {
        powershell(
            "$s=New-Object -ComObject Schedule.Service; $s.Connect(); [Console]::Write('ready')",
        )
        .await?;
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        return Err("native background sessions are unsupported on this platform".into());
    }
    Ok(())
}

async fn native_start(directory: &Path, manifest: &Manifest) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut args = vec![
            "--user".into(),
            "--quiet".into(),
            "--collect".into(),
            "--service-type=exec".into(),
            format!("--unit={}", manifest.manager_id),
            format!("--working-directory={}", path_text(&manifest.spec.cwd)?),
            "--property=KillMode=control-group".into(),
            "--property=TimeoutStopSec=15s".into(),
            "--property=UMask=0077".into(),
            "--property=StandardInput=null".into(),
            "--property=StandardOutput=null".into(),
            "--property=StandardError=journal".into(),
            format!(
                "--property=Restart={}",
                if manifest.spec.watchdog {
                    "on-failure"
                } else {
                    "no"
                }
            ),
            format!("--property=RestartSec={}s", native_restart_delay(manifest)),
            "--property=StartLimitIntervalSec=0".into(),
        ];
        for (key, value) in &manifest.environment {
            args.push(format!("--setenv={key}={value}"));
        }
        args.extend([
            "--".into(),
            path_text(&executable(directory))?.into(),
            "__session-run".into(),
            manifest.spec.name.clone(),
        ]);
        manager_command("systemd-run", &args, false).await?;
    }
    #[cfg(target_os = "macos")]
    {
        let plist = directory.join("launchd.plist");
        write_private(&plist, launchd_plist(directory, manifest)?.as_bytes())?;
        let target = launchd_target(manifest);
        let loaded = manager_command("/bin/launchctl", &["print".into(), target.clone()], true)
            .await?
            .status
            .success();
        if loaded {
            manager_command("/bin/launchctl", &["kickstart".into(), target], false).await?;
        } else {
            manager_command(
                "/bin/launchctl",
                &[
                    "bootstrap".into(),
                    format!("gui/{}", unsafe { libc::geteuid() }),
                    path_text(&plist)?.into(),
                ],
                false,
            )
            .await?;
        }
    }
    #[cfg(windows)]
    {
        powershell(&windows_start_script(directory, manifest)?).await?;
    }
    Ok(())
}

async fn native_stop(manifest: &Manifest) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let status = native_status(manifest).await?;
        if status.state != "not-loaded" {
            manager_command("systemctl", &systemctl_args(manifest, "stop"), false).await?;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if native_status(manifest).await?.state != "not-loaded" {
            manager_command(
                "/bin/launchctl",
                &["bootout".into(), launchd_target(manifest)],
                false,
            )
            .await?;
        }
    }
    #[cfg(windows)]
    {
        powershell(&format!(
            "{} if ($null -ne $t) {{ $t.Enabled=$false; $t.Stop(0) }}",
            windows_task_prelude(manifest)
        ))
        .await?;
    }
    Ok(())
}

async fn native_remove(manifest: &Manifest) -> Result<()> {
    #[cfg(windows)]
    {
        powershell(&format!(
            "{} if ($null -ne $t) {{ $f.DeleteTask({},0) }}",
            windows_task_prelude(manifest),
            ps_quote(&manifest.manager_id)
        ))
        .await?;
    }
    #[cfg(not(windows))]
    {
        let _ = manifest;
    }
    Ok(())
}

async fn native_status(manifest: &Manifest) -> Result<NativeStatus> {
    #[cfg(target_os = "linux")]
    {
        let mut args = systemctl_args(manifest, "show");
        args.push("--property=LoadState,ActiveState,SubState,MainPID,ExecMainStatus".into());
        let output = manager_command("systemctl", &args, true).await?;
        let text = String::from_utf8(output.stdout)?;
        let properties: BTreeMap<&str, &str> = text
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        if properties.get("LoadState") == Some(&"not-found") {
            return Ok(NativeStatus {
                state: "not-loaded".into(),
                ..Default::default()
            });
        }
        if !output.status.success() || !properties.contains_key("ActiveState") {
            return Err("user systemd manager state is unavailable".into());
        }
        let state = properties["ActiveState"];
        Ok(NativeStatus {
            state: state.into(),
            active: matches!(
                state,
                "active" | "activating" | "deactivating" | "reloading"
            ),
            pid: properties
                .get("MainPID")
                .and_then(|v| v.parse().ok())
                .filter(|v| *v != 0),
            exit_code: properties
                .get("ExecMainStatus")
                .and_then(|v| v.parse().ok()),
            started_at: None,
            generation: None,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let output = manager_command(
            "/bin/launchctl",
            &["print".into(), launchd_target(manifest)],
            true,
        )
        .await?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            if message.contains("Could not find service")
                || message.contains("Could not find specified service")
            {
                return Ok(NativeStatus {
                    state: "not-loaded".into(),
                    ..Default::default()
                });
            }
            return Err("user launchd manager state is unavailable".into());
        }
        let text = String::from_utf8(output.stdout)?;
        let property = |key: &str| {
            text.lines()
                .map(str::trim)
                .find_map(|line| line.strip_prefix(key))
                .map(str::trim)
        };
        let state = property("state = ").unwrap_or("unknown");
        let pid = property("pid = ").and_then(|v| v.parse::<u32>().ok());
        Ok(NativeStatus {
            state: state.into(),
            active: pid.is_some(),
            pid,
            exit_code: property("last exit code = ").and_then(|v| v.parse().ok()),
            started_at: None,
            generation: None,
        })
    }
    #[cfg(windows)]
    {
        let script = format!(
            "{} if ($null -eq $t) {{ @{{state='not-loaded';active=$false}} | ConvertTo-Json -Compress }} else {{ @{{state=([string]$t.State);active=($t.State -eq 4 -or $t.State -eq 2);exit_code=([long]$t.LastTaskResult);started_at=([DateTimeOffset]$t.LastRunTime.ToUniversalTime()).ToUnixTimeSeconds()}} | ConvertTo-Json -Compress }}",
            windows_task_prelude(manifest)
        );
        let output: Value = serde_json::from_slice(&powershell(&script).await?.stdout)?;
        let state = match output["state"].as_str().unwrap_or("unknown") {
            "4" => "running",
            "3" => "ready",
            "2" => "queued",
            "1" => "disabled",
            "0" => "unknown",
            value => value,
        };
        Ok(NativeStatus {
            state: state.into(),
            active: output["active"].as_bool().unwrap_or(false),
            pid: None,
            exit_code: output["exit_code"].as_i64(),
            started_at: output["started_at"].as_u64(),
            generation: None,
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = manifest;
        Err("native sessions are unsupported".into())
    }
}

#[cfg(any(target_os = "macos", test))]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn launchd_target(manifest: &Manifest) -> String {
    format!("gui/{}/{}", unsafe { libc::geteuid() }, manifest.manager_id)
}

#[cfg(any(target_os = "macos", test))]
fn launchd_plist(directory: &Path, manifest: &Manifest) -> Result<String> {
    let mut text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string><string>__session-run</string><string>{}</string></array><key>WorkingDirectory</key><string>{}</string><key>RunAtLoad</key><true/><key>ProcessType</key><string>Background</string><key>ExitTimeOut</key><integer>15</integer><key>Umask</key><integer>63</integer><key>StandardOutPath</key><string>/dev/null</string><key>StandardErrorPath</key><string>/dev/null</string>",
        xml(&manifest.manager_id),
        xml(path_text(&executable(directory))?),
        xml(&manifest.spec.name),
        xml(path_text(&manifest.spec.cwd)?)
    );
    if manifest.spec.watchdog {
        text.push_str(&format!("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>{}</integer>", manifest.spec.reconnect_delay_secs));
    } else {
        text.push_str("<key>KeepAlive</key><false/>");
    }
    text.push_str("<key>EnvironmentVariables</key><dict>");
    for (key, value) in &manifest.environment {
        text.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            xml(key),
            xml(value)
        ));
    }
    text.push_str("</dict></dict></plist>\n");
    Ok(text)
}

#[cfg(any(windows, test))]
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(any(windows, test))]
fn powershell_encoded(script: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

#[cfg(windows)]
fn powershell_program() -> Result<String> {
    Ok(
        PathBuf::from(std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe")
            .to_str()
            .ok_or("PowerShell path is not UTF-8")?
            .into(),
    )
}

#[cfg(windows)]
async fn powershell(script: &str) -> Result<std::process::Output> {
    let script = format!(
        "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=New-Object System.Text.UTF8Encoding($false); {script}"
    );
    manager_command(
        &powershell_program()?,
        &[
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-WindowStyle".into(),
            "Hidden".into(),
            "-EncodedCommand".into(),
            powershell_encoded(&script),
        ],
        false,
    )
    .await
}

#[cfg(windows)]
fn windows_task_prelude(manifest: &Manifest) -> String {
    format!(
        "$s=New-Object -ComObject Schedule.Service; $s.Connect(); $f=$s.GetFolder('\\'); $t=$null; try {{ $t=$f.GetTask({}) }} catch {{ $e=$_.Exception; $missing=$false; while ($null -ne $e) {{ if ($e.HResult -eq -2147024894) {{ $missing=$true }}; $e=$e.InnerException }}; if (-not $missing) {{ throw }} }};",
        ps_quote(&manifest.manager_id)
    )
}

#[cfg(windows)]
fn windows_start_script(directory: &Path, manifest: &Manifest) -> Result<String> {
    let mut worker = String::from("$ErrorActionPreference='Stop'; ");
    for (key, value) in &manifest.environment {
        worker.push_str(&format!(
            "[Environment]::SetEnvironmentVariable({}, {}, 'Process'); ",
            ps_quote(key),
            ps_quote(value)
        ));
    }
    worker.push_str(&format!(
        "Set-Location -LiteralPath {}; & {} '__session-run' {}; exit $LASTEXITCODE",
        ps_quote(path_text(&manifest.spec.cwd)?),
        ps_quote(path_text(&executable(directory))?),
        ps_quote(&manifest.spec.name)
    ));
    let arguments = format!(
        "-NoLogo -NoProfile -NonInteractive -WindowStyle Hidden -EncodedCommand {}",
        powershell_encoded(&worker)
    );
    let restart = if manifest.spec.watchdog {
        format!(
            "$d.Settings.RestartCount=255; $d.Settings.RestartInterval='PT{}S';",
            native_restart_delay(manifest)
        )
    } else {
        String::new()
    };
    Ok(format!(
        "{} if ($null -eq $t) {{ $d=$s.NewTask(0); $sid=[Security.Principal.WindowsIdentity]::GetCurrent().User.Value; $d.RegistrationInfo.Description={}; $d.Principal.UserId=$sid; $d.Principal.LogonType=3; $d.Principal.RunLevel=0; $d.Settings.Enabled=$true; $d.Settings.Hidden=$true; $d.Settings.AllowDemandStart=$true; $d.Settings.DisallowStartIfOnBatteries=$false; $d.Settings.StopIfGoingOnBatteries=$false; $d.Settings.ExecutionTimeLimit='PT0S'; $d.Settings.MultipleInstances=2; {} $a=$d.Actions.Create(0); $a.Path={}; $a.Arguments={}; $a.WorkingDirectory={}; $t=$f.RegisterTaskDefinition({},$d,2,$sid,$null,3,$null) }}; if ($t.Definition.RegistrationInfo.Description -ne {}) {{ throw 'Task does not belong to this saved session' }}; $t.Enabled=$true; $null=$t.Run($null)",
        windows_task_prelude(manifest),
        ps_quote(&format!("ssmcp owned session {}", path_text(directory)?)),
        restart,
        ps_quote(&powershell_program()?),
        ps_quote(&arguments),
        ps_quote(path_text(&manifest.spec.cwd)?),
        ps_quote(&manifest.manager_id),
        ps_quote(&format!("ssmcp owned session {}", path_text(directory)?))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SessionSpec {
        SessionSpec {
            name: "workstation".into(),
            cwd: std::env::temp_dir(),
            args: vec!["ssmcp@example".into()],
            watchdog: false,
            reconnect_delay_secs: 5,
        }
    }

    fn manifest(root: &Path) -> Manifest {
        let spec = spec();
        Manifest {
            format: FORMAT,
            supervised: false,
            manager_id: manager_id(root, &spec.name),
            spec,
            executable_sha256: "fixture".into(),
            version: "fixture".into(),
            created_at: 0,
            environment: BTreeMap::from([(
                "SSMCP_SESSION_HOME".into(),
                root.to_string_lossy().into_owned(),
            )]),
        }
    }

    fn temporary() -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("ssmcp-session-test-{}", random_suffix().unwrap()));
        platform::private_directory(&path).unwrap();
        path
    }

    #[test]
    fn distinct_state_roots_and_names_never_target_the_same_native_service() {
        let first = std::env::temp_dir().join("one");
        let second = std::env::temp_dir().join("two");
        assert_ne!(manager_id(&first, "same"), manager_id(&second, "same"));
        assert_ne!(manager_id(&first, "same"), manager_id(&first, "other"));
        for invalid in [
            "",
            "--json",
            "-workstation",
            "../x",
            "a/b",
            "a\\b",
            "line\nbreak",
            " spaced ",
            "..",
        ] {
            assert!(validate_name(invalid).is_err());
        }
        assert!(validate_name("中文 work-station").is_ok());
        // A title that looks like an option is still a value. The CLI has
        // already parsed and removed the actual daemon option before this API.
        let mut options = spec();
        options.args = vec!["--title".into(), "--daemon".into(), "ssmcp@example".into()];
        assert!(validate_spec(&options).is_ok());
    }

    #[test]
    fn connection_secrets_are_absent_from_status_and_logs() {
        let public = public_connection(
            &json!({"client_id":"fixture", "name":"workstation", "login_code":"PRIVATE", "token":"PRIVATE", "nested":{"secret":"PRIVATE"}, "url":"https://user:PRIVATE@example"}),
        );
        assert_eq!(public["client_id"], "fixture");
        assert!(!public.to_string().contains("PRIVATE"));
    }

    #[test]
    fn locks_release_without_stale_pid_files_and_private_files_round_trip() {
        let directory = temporary();
        let path = directory.join("run.lock");
        let first = lock(&path, true).unwrap();
        assert!(lock(&path, true).is_err());
        drop(first);
        drop(lock(&path, true).unwrap());
        let path = directory.join("state.json");
        write_json(&path, &json!({"phase":"starting"})).unwrap();
        write_json(&path, &json!({"phase":"stopped"})).unwrap();
        assert_eq!(read_json::<Value>(&path).unwrap()["phase"], "stopped");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn stop_marker_wins_over_a_late_connected_notification() {
        let directory = temporary();
        let runner = Runner {
            manifest: manifest(&directory),
            _run_lock: lock(&directory.join("run.lock"), true).unwrap(),
            directory: directory.clone(),
            generation: None,
            owner: None,
        };
        runner
            .record(
                "connected",
                1,
                None,
                Some(&json!({"client_id":"one","login_code":"PRIVATE"})),
                None,
            )
            .unwrap();
        assert!(directory.join("auth.json").exists());
        write_json(
            &directory.join("stop.requested"),
            &json!({"requested_at":0}),
        )
        .unwrap();
        runner
            .record(
                "connected",
                2,
                None,
                Some(&json!({"client_id":"two","login_code":"PRIVATE"})),
                None,
            )
            .unwrap();
        assert!(!directory.join("auth.json").exists());
        let state: Value = read_json(&directory.join("state.json")).unwrap();
        assert_eq!(state["phase"], "stopped");
        assert!(
            !String::from_utf8(
                read_private(&directory.join("lifecycle.jsonl"), MAX_DOCUMENT).unwrap()
            )
            .unwrap()
            .contains("PRIVATE")
        );
        drop(runner);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn disconnect_invalidates_the_previous_login_code() {
        let directory = temporary();
        let runner = Runner {
            manifest: manifest(&directory),
            _run_lock: lock(&directory.join("run.lock"), true).unwrap(),
            directory: directory.clone(),
            generation: None,
            owner: None,
        };
        runner
            .record(
                "connected",
                1,
                None,
                Some(&json!({"login_code":"PRIVATE"})),
                None,
            )
            .unwrap();
        runner.record("disconnected", 1, None, None, None).unwrap();
        assert!(!directory.join("auth.json").exists());
        drop(runner);
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn state_file_symlink_cannot_read_an_outside_secret() {
        let directory = temporary();
        let outside = directory.join("outside");
        fs::write(&outside, "PRIVATE").unwrap();
        let link = directory.join("state.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(read_private(&link, 100).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_definition_escaping_keeps_values_as_data() {
        let root = std::env::temp_dir().join("path with spaces & <xml>");
        let mut value = manifest(&root);
        value.spec.name = "O'Brien & <work>".into();
        value.spec.watchdog = true;
        let xml = launchd_plist(&root, &value).unwrap();
        assert!(xml.contains("O&apos;Brien &amp; &lt;work&gt;"));
        assert!(xml.contains("<key>SuccessfulExit</key><false/>"));
        assert!(!xml.contains("<key>StartInterval</key>"));
        assert_eq!(ps_quote("a'; Remove-Item x; '"), "'a''; Remove-Item x; '''");
        use base64::Engine;
        let encoded = powershell_encoded("'中文'; $x=1");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let words: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|v| u16::from_le_bytes(*v))
            .collect();
        assert_eq!(String::from_utf16(&words).unwrap(), "'中文'; $x=1");
    }
}
