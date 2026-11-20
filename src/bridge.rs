//! Authenticated, bounded background transfers and TCP mappings between clients.
use crate::{
    broker::{Authorization, Broker, Session},
    wire::Operation,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Read,
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Notify, Semaphore, watch},
    task::JoinSet,
    time::{Instant, timeout},
};

pub const MAX_ACTIVE_SYNCS: usize = 2;
pub const MAX_ACTIVE_PORTS: usize = 4;
pub const MAX_CONNECTIONS: usize = 2;
const MAX_HISTORY: usize = 32;
const CHUNK: usize = 32 * 1024;

pub struct Bridges {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    connections: Arc<Semaphore>,
}

impl Default for Bridges {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
        }
    }
}

impl Drop for Bridges {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap().values() {
            let _ = task.cancel.send(true);
        }
    }
}

struct Task {
    id: String,
    owner: String,
    kind: &'static str,
    value: Mutex<Value>,
    cancel: watch::Sender<bool>,
    changed: Notify,
    handles: Mutex<HashMap<String, Remote>>,
    completion: Mutex<Option<Completion>>,
    cleanup: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
struct Completion {
    state: &'static str,
    result: Option<Value>,
    error: Option<String>,
}

impl Task {
    fn snapshot(&self) -> Value {
        let mut value = self.value.lock().unwrap().clone();
        let handles = self.handles.lock().unwrap();
        value["owned_handles"] = json!(handles.len());
        if matches!(
            value["state"].as_str(),
            Some("cleanup_failed" | "finishing")
        ) && !handles.is_empty()
        {
            value["cleanup_pending"] = json!(
                handles
                    .values()
                    .map(|h| json!({
                        "client_id":h.client_id,"id":h.id,"namespace":h.namespace
                    }))
                    .collect::<Vec<_>>()
            );
        }
        value
    }
    fn active(&self) -> bool {
        matches!(
            self.value.lock().unwrap()["state"].as_str(),
            Some("starting" | "running" | "finishing")
        )
    }
    fn has_handles(&self) -> bool {
        !self.handles.lock().unwrap().is_empty()
    }
    fn track(&self, remote: &Remote) {
        self.handles
            .lock()
            .unwrap()
            .insert(remote.key(), remote.clone());
    }
    async fn close_remote(&self, remote: &Remote) -> Result<(), String> {
        match remote.close().await {
            Ok(()) => {
                self.handles.lock().unwrap().remove(&remote.key());
                Ok(())
            }
            Err(error) => {
                self.value.lock().unwrap()["cleanup_error"] = json!(error);
                let _ = self.cancel.send(true);
                Err(error)
            }
        }
    }
    async fn finish(&self, state: &'static str, result: Option<Value>, error: Option<String>) {
        *self.completion.lock().unwrap() = Some(Completion {
            state,
            result,
            error,
        });
        self.update("finishing", None, None);
        self.finalize().await;
    }
    async fn finalize(&self) {
        let _cleanup = self.cleanup.lock().await;
        let Some(completion) = self.completion.lock().unwrap().clone() else {
            return;
        };
        let handles: Vec<_> = self.handles.lock().unwrap().values().cloned().collect();
        let results =
            futures_util::future::join_all(handles.iter().map(|remote| self.close_remote(remote)))
                .await;
        let errors: Vec<_> = results.into_iter().filter_map(Result::err).collect();
        if errors.is_empty() {
            self.value
                .lock()
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("cleanup_error");
            self.update(completion.state, completion.result, completion.error);
        } else {
            self.update(
                "cleanup_failed",
                completion.result,
                Some(format!(
                    "Owned handles remain open; retry jobs.cancel(id) or ports.close(id): {}",
                    errors.join("; ")
                )),
            );
        }
    }
    fn retry_cleanup(self: &Arc<Self>) {
        if !self.active() && self.has_handles() && self.completion.lock().unwrap().is_some() {
            self.update("finishing", None, None);
            let task = self.clone();
            tokio::spawn(async move {
                task.finalize().await;
            });
        }
    }
    fn update(&self, state: &str, result: Option<Value>, error: Option<String>) {
        let mut value = self.value.lock().unwrap();
        value["state"] = json!(state);
        if let Some(result) = result {
            value["result"] = result;
        }
        if let Some(error) = error {
            value["error"] = json!(error.chars().take(8192).collect::<String>());
        } else {
            value.as_object_mut().unwrap().remove("error");
        }
        drop(value);
        self.changed.notify_waiters();
    }
    async fn wait(&self, duration: Duration) -> Value {
        let deadline = Instant::now() + duration;
        loop {
            let changed = self.changed.notified();
            if !self.active() {
                return self.snapshot();
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.snapshot();
            }
        }
    }
}

impl Bridges {
    fn insert(
        &self,
        owner: String,
        kind: &'static str,
        options: Value,
    ) -> Result<(Arc<Task>, watch::Receiver<bool>), String> {
        let mut tasks = self.tasks.lock().unwrap();
        let limit = if kind == "sync" {
            MAX_ACTIVE_SYNCS
        } else {
            MAX_ACTIVE_PORTS
        };
        if tasks
            .values()
            .filter(|t| t.kind == kind && (t.active() || t.has_handles()))
            .count()
            >= limit
        {
            return Err(format!(
                "at most {limit} active {kind} operations are supported; close or wait for one"
            ));
        }
        while tasks.len() >= MAX_HISTORY {
            let Some(id) = tasks
                .values()
                .filter(|t| !t.active() && !t.has_handles())
                .min_by_key(|t| t.snapshot()["created_at_ms"].as_u64().unwrap_or(0))
                .map(|t| t.id.clone())
            else {
                return Err("managed operation history is full".into());
            };
            tasks.remove(&id);
        }
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut bytes))
            .map_err(|e| e.to_string())?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let (cancel, receiver) = watch::channel(false);
        let task = Arc::new(Task {
            id: id.clone(),
            owner,
            kind,
            cancel,
            changed: Notify::new(),
            handles: Mutex::new(HashMap::new()),
            completion: Mutex::new(None),
            cleanup: tokio::sync::Mutex::new(()),
            value: Mutex::new(
                json!({"id":id,"kind":kind,"state":"starting","options":options,
                "created_at_ms":SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64}),
            ),
        });
        tasks.insert(id, task.clone());
        Ok((task, receiver))
    }

    fn get(&self, owner: &str, kind: &str, id: &str) -> Result<Arc<Task>, String> {
        self.tasks
            .lock()
            .unwrap()
            .get(id)
            .filter(|t| t.owner == owner && t.kind == kind)
            .cloned()
            .ok_or_else(|| "managed operation is unavailable for this authorization".into())
    }

    fn list(&self, owner: &str, kind: &str) -> Value {
        let mut values: Vec<_> = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|t| {
                t.owner == owner
                    && t.kind == kind
                    && (kind == "sync" || t.active() || t.has_handles())
            })
            .map(|t| {
                let mut summary = t.snapshot();
                if let Some(object) = summary.as_object_mut() {
                    object.remove("result");
                }
                summary
            })
            .collect();
        values.sort_by_key(|v| v["created_at_ms"].as_u64().unwrap_or(0));
        if kind == "sync" {
            json!({"jobs":values})
        } else {
            json!({"ports":values})
        }
    }

    pub async fn shutdown(&self) {
        let tasks: Vec<_> = self.tasks.lock().unwrap().values().cloned().collect();
        for task in &tasks {
            let _ = task.cancel.send(true);
            task.retry_cleanup();
        }
        let _ = timeout(Duration::from_secs(10), async {
            for task in tasks {
                task.wait(Duration::from_secs(10)).await;
            }
        })
        .await;
    }
}

#[derive(Clone)]
struct Access {
    broker: Weak<Broker>,
    user: String,
    token: String,
    audit_run: Option<crate::code_mode::AuditRun>,
}

impl Access {
    async fn authorized(&self) -> Result<(Arc<Broker>, Authorization), String> {
        let broker = self.broker.upgrade().ok_or("server stopped")?;
        let authorization = broker
            .authenticate(&self.user, &self.token)
            .await
            .ok_or("authorization expired or was revoked; refresh the MCP connection")?;
        Ok((broker, authorization))
    }

    async fn session(&self, id: &str) -> Result<Arc<Session>, String> {
        let (broker, authorization) = self.authorized().await?;
        broker.client(&self.user, &authorization, id).await
    }

    async fn native(
        &self,
        session: &Session,
        authorization: &Authorization,
        operation: Operation,
    ) -> Result<Value, String> {
        let call = session.call_with_authorization(operation, None, authorization);
        let result = if let Some(run) = &self.audit_run {
            crate::code_mode::AUDIT_RUN.scope(run.clone(), call).await
        } else {
            call.await
        };
        // A managed transfer or port can dispatch its first native call after
        // the JavaScript run has returned. Retain that run's source and outcome,
        // while its background operations keep their own lifecycle.
        if let Some(completion) = self.audit_run.as_ref().and_then(|run| run.completed())
            && let Ok(current) = self.session(session.client_id()).await
        {
            let _ = current.report_code_mode(completion);
        }
        result
    }

    async fn call(&self, id: &str, name: &str, arguments: Value) -> Result<Value, String> {
        let (broker, authorization) = self.authorized().await?;
        let session = broker.client(&self.user, &authorization, id).await?;
        let value = self
            .native(
                &session,
                &authorization,
                Operation::CallTool {
                    name: name.into(),
                    arguments,
                },
            )
            .await?;
        self.session(id).await?;
        unpack(value)
    }

    async fn accept(&self, listener: &Remote, task: &Task) -> Result<Option<Remote>, String> {
        let (broker, authorization) = self.authorized().await?;
        let session = broker
            .client(&self.user, &authorization, &listener.client_id)
            .await?;
        let value = unpack(
            self.native(
                &session,
                &authorization,
                Operation::CallTool {
                    name: "ssmcp.net.accept".into(),
                    arguments: json!({"id":listener.id,"wait_ms":500}),
                },
            )
            .await?,
        )?;
        let remote = match value.get("stream").filter(|v| v.is_object()) {
            None => None,
            Some(stream) => {
                let id = stream["id"]
                    .as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 128)
                    .ok_or("accept returned an invalid stream")?;
                Some(Remote {
                    client_id: listener.client_id.clone(),
                    id: id.into(),
                    namespace: "ssmcp.net",
                    session,
                    metadata: stream.clone(),
                })
            }
        };
        if let Some(remote) = &remote {
            task.track(remote);
        }
        if let Err(error) = self.session(&listener.client_id).await {
            if let Some(remote) = &remote {
                let _ = task.close_remote(remote).await;
            }
            return Err(error);
        }
        Ok(remote)
    }

    async fn open(
        &self,
        client_id: &str,
        namespace: &'static str,
        name: &str,
        args: Value,
        task: &Task,
    ) -> Result<Remote, String> {
        let (broker, authorization) = self.authorized().await?;
        let session = broker.client(&self.user, &authorization, client_id).await?;
        // Retain the authenticated session solely for cleanup even after authorization expires.
        let value = unpack(
            self.native(
                &session,
                &authorization,
                Operation::CallTool {
                    name: name.into(),
                    arguments: args,
                },
            )
            .await?,
        )?;
        let id = value["id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or("client returned an invalid handle")?
            .to_owned();
        let remote = Remote {
            client_id: client_id.into(),
            id,
            namespace,
            session,
            metadata: value,
        };
        task.track(&remote);
        if let Err(error) = self.session(client_id).await {
            let _ = task.close_remote(&remote).await;
            return Err(error);
        }
        Ok(remote)
    }
}

fn unpack(value: Value) -> Result<Value, String> {
    if value["isError"].as_bool() == Some(true) {
        let message = value["content"]
            .as_array()
            .and_then(|parts| parts.iter().find_map(|p| p["text"].as_str()))
            .unwrap_or("workspace operation failed");
        return Err(message.chars().take(4096).collect());
    }
    value
        .get("structuredContent")
        .filter(|v| v.is_object())
        .cloned()
        .ok_or_else(|| "workspace client returned no structured result; upgrade that client".into())
}

#[derive(Clone)]
struct Remote {
    client_id: String,
    id: String,
    namespace: &'static str,
    session: Arc<Session>,
    metadata: Value,
}

impl Remote {
    fn key(&self) -> String {
        format!("{}:{}:{}", self.client_id, self.namespace, self.id)
    }
    async fn close(&self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut last_error;
        loop {
            // A disconnected client owns teardown of all of its native handles.
            if !self.session.is_alive() {
                return Ok(());
            }
            let result = tokio::time::timeout_at(
                deadline,
                self.session.call_cleanup(Operation::CallTool {
                    name: format!("{}.close", self.namespace),
                    arguments: json!({"id":self.id}),
                }),
            )
            .await;
            match result {
                Ok(Ok(value)) => match unpack(value) {
                    Ok(value) if value["closed"].is_boolean() => return Ok(()),
                    Ok(_) => last_error = "client did not acknowledge handle closure".to_owned(),
                    Err(error) => last_error = error,
                },
                Ok(Err(error)) => last_error = error,
                Err(_) => last_error = "close acknowledgement timed out".into(),
            }
            if !self.session.is_alive() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep_until((Instant::now() + Duration::from_millis(50)).min(deadline))
                .await;
        }
        Err(format!(
            "{} handle {} on {}: {}",
            self.namespace,
            self.id,
            self.client_id,
            last_error.chars().take(512).collect::<String>()
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEnd {
    client_id: String,
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncOptions {
    source: FileEnd,
    destination: FileEnd,
    #[serde(default = "yes")]
    dry_run: bool,
    #[serde(default)]
    delete: bool,
    #[serde(default = "job_timeout")]
    timeout_ms: u64,
}
fn yes() -> bool {
    true
}
fn job_timeout() -> u64 {
    300_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortEnd {
    client_id: String,
    #[serde(default = "loopback")]
    host: String,
    port: u16,
}
fn loopback() -> String {
    "127.0.0.1".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PortOptions {
    listen: PortEnd,
    target: PortEnd,
    #[serde(default = "port_lifetime")]
    ttl_ms: u64,
}
fn port_lifetime() -> u64 {
    900_000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Id {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    id: String,
    #[serde(default)]
    wait_ms: u64,
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid builtin arguments: {e}"))
}

pub(crate) fn owner(user: &str, authorization: &Authorization) -> Result<String, String> {
    match authorization {
        Authorization::Identity(identity) => Ok(format!(
            "{user}\0{}\0{}",
            identity.provider, identity.subject
        )),
        Authorization::Legacy { anchor, .. } => anchor
            .upgrade()
            .filter(|s| s.is_alive())
            .map(|s| format!("{user}\0legacy\0{}", s.client_id()))
            .ok_or_else(|| "connection authorization ended".into()),
    }
}

fn valid_client(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 {
        Err("client_id must identify an online client returned by list".into())
    } else {
        Ok(())
    }
}

fn valid_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 4096
        || path.chars().any(char::is_control)
        || std::path::Path::new(path).is_absolute()
        || std::path::Path::new(path).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        Err("paths must be relative directories inside the client's startup workspace".into())
    } else {
        Ok(())
    }
}

pub async fn dispatch(
    broker: Arc<Broker>,
    user: String,
    token: String,
    name: String,
    arguments: Value,
) -> Result<Value, String> {
    let access = Access {
        broker: Arc::downgrade(&broker),
        user,
        token,
        audit_run: crate::code_mode::AUDIT_RUN.try_with(Clone::clone).ok(),
    };
    let (_, authorization) = access.authorized().await?;
    let owner = owner(&access.user, &authorization)?;
    match name.as_str() {
        name if name.starts_with("messages.") => {
            let mut fields = arguments
                .as_object()
                .cloned()
                .ok_or("messages arguments must be an object")?;
            if fields.contains_key("action") {
                return Err("action is selected by the messages helper".into());
            }
            fields.insert(
                "action".into(),
                serde_json::json!(&name["messages.".len()..]),
            );
            broker
                .message_request(&access.user, &access.token, Value::Object(fields))
                .await
        }
        name if name.starts_with("collab.") => {
            let mut args = arguments
                .as_object()
                .cloned()
                .ok_or("collab arguments must be an object")?;
            let client_id = args
                .remove("client_id")
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or("collab requires client_id from list")?;
            valid_client(&client_id)?;
            let session = access.session(&client_id).await?;
            if let Some(run) = &access.audit_run {
                run.record_connection(&session)?;
            }
            if name == "collab.bind" {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Binding {
                    agent_id: String,
                    token: String,
                }
                let binding: Binding = parse(Value::Object(args))?;
                let agent =
                    session
                        .collaboration
                        .bind(&owner, &binding.agent_id, &binding.token)?;
                let run = access.audit_run.as_ref().ok_or(
                    "collab.bind is scoped to code_mode; call it at the start of each run",
                )?;
                let current = access.session(&client_id).await?;
                if !Arc::ptr_eq(&current, &session) {
                    return Err("collaboration connection changed before binding".into());
                }
                run.bind(&client_id, &owner, &binding.agent_id, &binding.token)?;
                return Ok(json!({"client_id":client_id,"agent":agent,"bound":true}));
            }
            let result = session
                .collaboration
                .dispatch(&owner, &name["collab.".len()..], Value::Object(args))
                .await?;
            // Waiting never grants authority beyond the current token, connection and ACL.
            let current = access.session(&client_id).await?;
            if !Arc::ptr_eq(&current, &session) {
                return Err("collaboration connection changed before response".into());
            }
            if let Some(run) = &access.audit_run {
                run.question_delivery_server_action(&client_id);
                run.interacted(&client_id);
            }
            Ok(result)
        }
        "resources.list" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                cursor: Option<String>,
            }
            let args: Args = parse(arguments)?;
            let result = crate::resources::list(
                &broker,
                &access.user,
                &authorization,
                args.cursor.as_deref(),
            )
            .await
            .map_err(|e| e.message().to_owned())?;
            access.authorized().await?;
            Ok(result)
        }
        "resources.read" => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Args {
                uri: String,
            }
            let args: Args = parse(arguments)?;
            let result = crate::resources::read(&broker, &access.user, &authorization, &args.uri)
                .await
                .map_err(|e| e.message().to_owned())?;
            access.authorized().await?;
            Ok(result)
        }
        "resources.templates" => {
            empty(&arguments)?;
            Ok(crate::resources::templates())
        }
        "sync" => {
            let options: SyncOptions = parse(arguments.clone())?;
            for end in [&options.source, &options.destination] {
                valid_client(&end.client_id)?;
                valid_path(&end.path)?;
                access.session(&end.client_id).await?;
            }
            if options.source.client_id == options.destination.client_id {
                return Err(
                    "sync requires two distinct client IDs; select both workspaces explicitly"
                        .into(),
                );
            }
            if !(1_000..=900_000).contains(&options.timeout_ms) {
                return Err("sync timeout_ms must be 1000..=900000".into());
            }
            let (task, cancel) = broker.bridges.insert(owner, "sync", arguments)?;
            let response = task.snapshot();
            tokio::spawn(run_sync(access, task, cancel, options));
            Ok(response)
        }
        "jobs.list" => {
            empty(&arguments)?;
            Ok(broker.bridges.list(&owner, "sync"))
        }
        "ports.list" => {
            empty(&arguments)?;
            Ok(broker.bridges.list(&owner, "port"))
        }
        "jobs.status" => {
            let args: Id = parse(arguments)?;
            Ok(broker.bridges.get(&owner, "sync", &args.id)?.snapshot())
        }
        "jobs.wait" => {
            let args: Wait = parse(arguments)?;
            if args.wait_ms > 5000 {
                return Err("wait_ms must be 0..=5000".into());
            }
            let task = broker.bridges.get(&owner, "sync", &args.id)?;
            let result = task.wait(Duration::from_millis(args.wait_ms)).await;
            access.authorized().await?;
            Ok(result)
        }
        "jobs.cancel" | "ports.close" => {
            let args: Id = parse(arguments)?;
            let kind = if name == "jobs.cancel" {
                "sync"
            } else {
                "port"
            };
            let task = broker.bridges.get(&owner, kind, &args.id)?;
            let _ = task.cancel.send(true);
            task.retry_cleanup();
            let result = task.wait(Duration::from_secs(8)).await;
            access.authorized().await?;
            Ok(result)
        }
        "ports.open" => {
            let options: PortOptions = parse(arguments.clone())?;
            for end in [&options.listen, &options.target] {
                valid_client(&end.client_id)?;
                if !matches!(end.host.as_str(), "127.0.0.1" | "::1" | "localhost") {
                    return Err("port mappings only support loopback endpoints".into());
                }
                access.session(&end.client_id).await?;
            }
            if options.target.port == 0 || !(1000..=3_600_000).contains(&options.ttl_ms) {
                return Err("target port must be nonzero; ttl_ms must be 1000..=3600000".into());
            }
            let (task, cancel) = broker.bridges.insert(owner, "port", arguments)?;
            let worker = task.clone();
            let capacity = broker.bridges.connections.clone();
            tokio::spawn(run_port(access.clone(), worker, cancel, options, capacity));
            // Opening is tracked before awaiting the remote listener, so a cancelled
            // code_mode request can rediscover and close it through ports.list().
            let _ = timeout(Duration::from_secs(8), async {
                loop {
                    let changed = task.changed.notified();
                    if task.snapshot()["state"] != "starting" {
                        break;
                    }
                    changed.await;
                }
            })
            .await;
            access.authorized().await?;
            let result = task.snapshot();
            if result["state"] == "failed" {
                return Err(result["error"]
                    .as_str()
                    .unwrap_or("port mapping failed")
                    .into());
            }
            Ok(result)
        }
        _ => Err("unknown code_mode builtin".into()),
    }
}

fn empty(arguments: &Value) -> Result<(), String> {
    if arguments.as_object().is_some_and(|v| v.is_empty()) {
        Ok(())
    } else {
        Err("this builtin takes no arguments".into())
    }
}

async fn pump(access: &Access, source: &Remote, destination: &Remote) -> Result<u64, String> {
    let mut bytes = 0u64;
    loop {
        let data = access
            .call(
                &source.client_id,
                &format!("{}.read", source.namespace),
                json!({"id":source.id,"max_bytes":CHUNK,"wait_ms":500}),
            )
            .await?;
        let encoded = data["data_base64"]
            .as_str()
            .ok_or("stream read omitted data_base64")?;
        if !encoded.is_empty() {
            let length = STANDARD
                .decode(encoded)
                .map_err(|_| "stream returned invalid base64")?
                .len();
            if length > CHUNK {
                return Err("stream returned more bytes than requested".into());
            }
            let result = access
                .call(
                    &destination.client_id,
                    &format!("{}.write", destination.namespace),
                    json!({"id":destination.id,"data_base64":encoded}),
                )
                .await?;
            if result["error"].is_string()
                || result["timed_out"] == true
                || result["written"].as_u64() != Some(length as u64)
            {
                return Err("stream write failed or was partial; the connection was closed without replaying bytes".into());
            }
            bytes += length as u64;
        }
        if data["eof"] == true {
            let (name, args) = if destination.namespace == "ssmcp.rsync" {
                (
                    "ssmcp.rsync.write",
                    json!({"id":destination.id,"data_base64":"","close_stdin":true}),
                )
            } else {
                ("ssmcp.net.shutdown", json!({"id":destination.id}))
            };
            access.call(&destination.client_id, name, args).await?;
            return Ok(bytes);
        }
    }
}

async fn run_sync(
    access: Access,
    task: Arc<Task>,
    mut cancel: watch::Receiver<bool>,
    options: SyncOptions,
) {
    let mut handles = Vec::new();
    let setup: Result<(), String> = async {
        let receiver = access.open(&options.destination.client_id, "ssmcp.rsync", "ssmcp.rsync.start",
            json!({"role":"receiver","path":options.destination.path,"source":options.source.path,
                "dry_run":options.dry_run,"delete":options.delete}), &task).await?;
        let server_args = receiver.metadata["server_args"].clone();
        handles.push(receiver);
        if *cancel.borrow() { return Err("cancelled".into()); }
        let sender = access.open(&options.source.client_id, "ssmcp.rsync", "ssmcp.rsync.start",
            json!({"role":"sender","path":options.source.path,"server_args":server_args}), &task).await?;
        handles.push(sender);
        Ok(())
    }.await;
    let outcome = match setup {
        Err(error) => Err(error),
        Ok(()) if *cancel.borrow() => Err("cancelled".into()),
        Ok(()) => {
            task.update("running", None, None);
            tokio::select! {
                _ = cancel.changed() => Err("cancelled".into()),
                _ = tokio::time::sleep(Duration::from_millis(options.timeout_ms)) => Err("sync deadline exceeded".into()),
                result = async {
                    let (sent, received) = tokio::try_join!(pump(&access, &handles[1], &handles[0]), pump(&access, &handles[0], &handles[1]))?;
                    let (sender, receiver) = tokio::try_join!(exit_status(&access, &handles[1]), exit_status(&access, &handles[0]))?;
                    if sender["exit_code"] != 0 || receiver["exit_code"] != 0 {
                        return Err(format!("system rsync failed: sender={sender}; receiver={receiver}"));
                    }
                    Ok(json!({"sender":sender,"receiver":receiver,"source_bytes":sent,"destination_bytes":received,
                        "dry_run":options.dry_run,"delete":options.delete}))
                } => result,
            }
        }
    };
    match outcome {
        Ok(result) => task.finish("completed", Some(result), None).await,
        Err(error) if error == "cancelled" => task.finish("cancelled", None, None).await,
        Err(error) => task.finish("failed", None, Some(error)).await,
    }
}

async fn exit_status(access: &Access, remote: &Remote) -> Result<Value, String> {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = access
                .call(
                    &remote.client_id,
                    "ssmcp.rsync.status",
                    json!({"id":remote.id}),
                )
                .await?;
            if status["exit_code"].is_number() {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "rsync process did not exit after its protocol closed".to_owned())?
}

async fn run_port(
    access: Access,
    task: Arc<Task>,
    mut cancel: watch::Receiver<bool>,
    options: PortOptions,
    capacity: Arc<Semaphore>,
) {
    let listener = match access
        .open(
            &options.listen.client_id,
            "ssmcp.net",
            "ssmcp.net.listen",
            json!({"host":options.listen.host,"port":options.listen.port}),
            &task,
        )
        .await
    {
        Ok(listener) => listener,
        Err(error) => {
            task.finish("failed", None, Some(error)).await;
            return;
        }
    };
    task.value.lock().unwrap()["listener"] = listener.metadata.clone();
    task.update("running", None, None);
    let deadline = Instant::now() + Duration::from_millis(options.ttl_ms);
    let (stop, stopped) = watch::channel(false);
    let mut connections = JoinSet::new();
    let outcome: Result<(), String> = loop {
        while connections.try_join_next().is_some() {}
        if *cancel.borrow_and_update() || Instant::now() >= deadline {
            break Ok(());
        }
        if let Err(error) = access.session(&options.target.client_id).await {
            break Err(error);
        }
        // Do not cancel an in-flight accept: it may already have consumed a socket.
        // Short native waits keep shutdown responsive without losing that handle.
        let source = match access.accept(&listener, &task).await {
            Ok(Some(source)) => source,
            Ok(None) => continue,
            Err(error) => break Err(error),
        };
        if *cancel.borrow() || Instant::now() >= deadline {
            let _ = task.close_remote(&source).await;
            break Ok(());
        }
        let permit = match capacity.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Err(error) = task.close_remote(&source).await {
                    break Err(error);
                }
                continue;
            }
        };
        let access = access.clone();
        let client_id = options.target.client_id.clone();
        let host = options.target.host.clone();
        let port = options.target.port;
        let mut stopped = stopped.clone();
        let task = task.clone();
        connections.spawn(async move {
            let _permit = permit;
            let destination = match access.open(&client_id, "ssmcp.net", "ssmcp.net.connect", json!({"host":host,"port":port,"wait_ms":1000}), &task).await {
                Ok(destination) => destination,
                Err(error) => { let _ = task.close_remote(&source).await; task.value.lock().unwrap()["last_connection_error"] = json!(error.chars().take(4096).collect::<String>()); return; }
            };
            let result = if *stopped.borrow() { Ok((0, 0)) } else {
                tokio::select! {
                    _ = stopped.changed() => Ok((0, 0)),
                    result = async { tokio::try_join!(pump(&access, &source, &destination), pump(&access, &destination, &source)) } => result,
                }
            };
            let _ = tokio::join!(task.close_remote(&source), task.close_remote(&destination));
            if let Err(error) = result { task.value.lock().unwrap()["last_connection_error"] = json!(error.chars().take(4096).collect::<String>()); }
        });
    };
    let _ = stop.send(true);
    let _ = tokio::join!(task.close_remote(&listener), async {
        while connections.join_next().await.is_some() {}
    });
    match outcome {
        Ok(()) => task.finish("closed", None, None).await,
        Err(error) => task.finish("failed", None, Some(error)).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn options_and_scoped_task_ownership_are_strict() {
        for path in ["/etc", "../outside", "nested/../../outside", "a\0b", ""] {
            assert!(valid_path(path).is_err());
        }
        assert!(valid_path("project with spaces/").is_ok());
        let tasks = Bridges::default();
        let (task, _cancel) = tasks.insert("owner-a".into(), "sync", json!({})).unwrap();
        assert!(tasks.get("owner-b", "sync", &task.id).is_err());
        assert!(tasks.get("owner-a", "port", &task.id).is_err());
        assert_eq!(tasks.list("owner-b", "sync"), json!({"jobs":[]}));
        task.value.lock().unwrap()["result"] = json!({"stderr":"x".repeat(65536)});
        let summary = tasks.list("owner-a", "sync");
        assert!(summary["jobs"][0].get("result").is_none());
        assert!(task.snapshot()["result"]["stderr"].is_string());
        assert!(parse::<SyncOptions>(json!({"source":{"client_id":"a","path":"."},"destination":{"client_id":"b","path":"."},"extra":true})).is_err());
        let options: SyncOptions = parse(json!({"source":{"client_id":"a","path":"."},"destination":{"client_id":"b","path":"."}})).unwrap();
        assert!(options.dry_run);
        assert!(!options.delete);
    }

    #[tokio::test]
    async fn cancellation_and_history_are_bounded() {
        let tasks = Bridges::default();
        let mut running = Vec::new();
        for _ in 0..MAX_ACTIVE_SYNCS {
            running.push(tasks.insert("owner".into(), "sync", json!({})).unwrap());
        }
        assert!(tasks.insert("owner".into(), "sync", json!({})).is_err());
        let cancelled = running[0].0.clone();
        running[0].0.cancel.send(true).unwrap();
        assert!(*running[0].1.borrow());
        cancelled.update("cancelled", None, None);
        assert_eq!(
            cancelled.wait(Duration::from_secs(1)).await["state"],
            "cancelled"
        );
        for _ in 0..MAX_HISTORY * 2 {
            let (task, _receiver) = tasks.insert("owner".into(), "sync", json!({})).unwrap();
            task.update("completed", Some(json!({})), None);
        }
        assert_eq!(tasks.tasks.lock().unwrap().len(), MAX_HISTORY);
    }
    struct WirePeer {
        session: Arc<Session>,
        held: Arc<std::sync::atomic::AtomicUsize>,
        release: Arc<Semaphore>,
        reject_close: Arc<std::sync::atomic::AtomicBool>,
        closed: Arc<std::sync::atomic::AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
        attachment: tokio::task::JoinHandle<crate::Result<()>>,
        _broker: Arc<Broker>,
    }

    impl WirePeer {
        async fn start() -> Self {
            use crate::wire::{self, ClientMetadata, Frame, Mode};
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            let broker = Arc::new(Broker::new("https://ssmcp.example".into()));
            let (server, client) = tokio::net::UnixStream::pair().unwrap();
            let attachment = tokio::spawn(broker.clone().attach(server));
            let (reader, mut writer) = client.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            wire::write_frame(
                &mut writer,
                &Frame::Hello {
                    version: wire::VERSION,
                    mode: Mode::Shell,
                    metadata: ClientMetadata {
                        name: "cleanup regression".into(),
                        title: None,
                        cwd: "/workspace".into(),
                        hostname: "fixture".into(),
                        allow: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();
            let Some(Frame::Ready { url, .. }) = wire::read_frame(&mut reader).await.unwrap()
            else {
                panic!("fixture did not register");
            };
            let user = url.rsplit('/').next().unwrap();
            let session = broker.online_session(user).await.unwrap();
            let held = Arc::new(AtomicUsize::new(0));
            let release = Arc::new(Semaphore::new(0));
            let reject_close = Arc::new(AtomicBool::new(false));
            let closed = Arc::new(AtomicUsize::new(0));
            let (count, gate, reject, closes) = (
                held.clone(),
                release.clone(),
                reject_close.clone(),
                closed.clone(),
            );
            let task = tokio::spawn(async move {
                let writer = Arc::new(tokio::sync::Mutex::new(writer));
                let mut requests = JoinSet::new();
                loop {
                    tokio::select! {
                        frame = wire::read_frame(&mut reader) => {
                            let Ok(Some(Frame::Call {id,operation,..})) = frame else { break; };
                            let (writer,count,gate,reject,closes) = (writer.clone(),count.clone(),gate.clone(),reject.clone(),closes.clone());
                            requests.spawn(async move {
                                let result = match operation {
                                    Operation::CallTool {name,..} if name=="fixture.hold" => {
                                        count.fetch_add(1,Ordering::Release);
                                        let _permit = gate.acquire().await.unwrap();
                                        Ok(crate::backend::result(json!({})))
                                    }
                                    Operation::CallTool {name,..} if name=="ssmcp.net.close" => {
                                        if reject.load(Ordering::Acquire) { Err("fixture close unavailable".into()) }
                                        else {
                                            closes.fetch_add(1,Ordering::Release);
                                            Ok(crate::backend::result(json!({"closed":true})))
                                        }
                                    }
                                    _ => Err("unknown fixture operation".into()),
                                };
                                let _ = wire::write_frame(&mut *writer.lock().await,&Frame::Reply{id,result}).await;
                            });
                        }
                        _ = requests.join_next(), if !requests.is_empty() => {}
                    }
                }
            });
            Self {
                session,
                held,
                release,
                reject_close,
                closed,
                task,
                attachment,
                _broker: broker,
            }
        }
        fn remote(&self) -> Remote {
            Remote {
                client_id: self.session.client_id().into(),
                id: "owned-handle".into(),
                namespace: "ssmcp.net",
                session: self.session.clone(),
                metadata: json!({}),
            }
        }
    }
    impl Drop for WirePeer {
        fn drop(&mut self) {
            self.task.abort();
            self.attachment.abort();
        }
    }

    #[tokio::test]
    async fn cleanup_retries_saturated_session_until_native_close_is_acknowledged() {
        use std::sync::atomic::Ordering;
        let peer = WirePeer::start().await;
        let mut requests = Vec::new();
        for _ in 0..crate::wire::MAX_INFLIGHT {
            let session = peer.session.clone();
            requests.push(tokio::spawn(async move {
                session
                    .call(Operation::CallTool {
                        name: "fixture.hold".into(),
                        arguments: json!({}),
                    })
                    .await
            }));
        }
        timeout(Duration::from_secs(2), async {
            while peer.held.load(Ordering::Acquire) < crate::wire::MAX_INFLIGHT {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let remote = peer.remote();
        let closing = tokio::spawn(async move { remote.close().await });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !closing.is_finished(),
            "busy session was incorrectly treated as closed"
        );
        assert_eq!(peer.closed.load(Ordering::Acquire), 0);
        peer.release.add_permits(crate::wire::MAX_INFLIGHT);
        for request in requests {
            request.await.unwrap().unwrap();
        }
        timeout(Duration::from_secs(2), closing)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(peer.closed.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn failed_cleanup_stays_visible_survives_history_eviction_and_can_be_retried() {
        use std::sync::atomic::Ordering;
        let peer = WirePeer::start().await;
        peer.reject_close.store(true, Ordering::Release);
        let tasks = Bridges::default();
        let (task, _cancel) = tasks.insert("owner".into(), "port", json!({})).unwrap();
        task.track(&peer.remote());
        task.finish("closed", None, None).await;
        let failed = task.snapshot();
        assert_eq!(failed["state"], "cleanup_failed");
        assert_eq!(failed["cleanup_pending"].as_array().unwrap().len(), 1);
        assert_eq!(
            tasks.list("owner", "port")["ports"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for _ in 0..MAX_HISTORY + 4 {
            let (finished, _receiver) = tasks.insert("owner".into(), "sync", json!({})).unwrap();
            finished.update("completed", None, None);
        }
        assert!(
            tasks.get("owner", "port", &task.id).is_ok(),
            "pending cleanup was evicted"
        );
        peer.reject_close.store(false, Ordering::Release);
        task.retry_cleanup();
        let result = task.wait(Duration::from_secs(2)).await;
        assert_eq!(result["state"], "closed");
        assert_eq!(result["owned_handles"], 0);
        assert!(result.get("cleanup_pending").is_none());
        assert_eq!(tasks.list("owner", "port"), json!({"ports":[]}));
    }
}
