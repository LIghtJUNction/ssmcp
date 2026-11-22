use super::{
    Config, now_ms,
    queue::{Actor, Availability, Queue, TaskStatus},
    remote::{self, Claimed, Completion, Remote},
};
#[cfg(target_os = "linux")]
use super::{executor, registry::Registry};
#[cfg(target_os = "linux")]
use crate::client::chat_io::process::Program;
use crate::{
    client::chat_io::Agent,
    runtime::{Runtime, call_scope::Source},
};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(any(target_os = "linux", test))]
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
pub(crate) struct TaskContext {
    #[cfg(target_os = "linux")]
    pub(crate) source: Source,
    #[cfg(target_os = "linux")]
    pub(crate) actor_key: String,
    pub(crate) prepared: String,
}

enum Execution {
    Remote(Remote<TaskContext>),
    #[cfg(target_os = "linux")]
    Local(Result<Program, String>),
}

pub(crate) struct Pool {
    pub(crate) generation: String,
    #[cfg(target_os = "linux")]
    pub(crate) config: Config,
    pub(crate) queue: Queue<TaskContext>,
    #[cfg(target_os = "linux")]
    pub(crate) resources: Mutex<Option<Arc<Registry>>>,
    pub(crate) cleanup_error: Mutex<Option<String>>,
    execution: Execution,
    #[cfg(target_os = "linux")]
    cwd: PathBuf,
    #[cfg(target_os = "linux")]
    supervisor: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Submit {
    request_id: String,
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    task_id: String,
    #[serde(default)]
    wait_ms: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct AvailabilityCursor {
    generation: String,
    revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitAvailable {
    #[serde(default)]
    after: Option<AvailabilityCursor>,
    #[serde(default = "default_availability_wait")]
    wait_ms: u64,
}

fn default_availability_wait() -> u64 {
    5000
}

impl Pool {
    pub(crate) fn new(config: Config, agent: &Agent, cwd: PathBuf) -> Result<Arc<Self>, String> {
        let generation = remote::nonce()?;
        let execution =
            match agent {
                Agent::Remote => Execution::Remote(Remote::new()),
                #[cfg(target_os = "linux")]
                _ => Execution::Local(Program::for_worker(agent)),
                #[cfg(not(target_os = "linux"))]
                _ => return Err(
                    "local worker executors require Linux cgroup v2 and the user's systemd manager"
                        .into(),
                ),
            };
        #[cfg(not(target_os = "linux"))]
        let _ = cwd;
        let pool = Arc::new(Self {
            queue: Queue::new(generation.clone(), config.count)?,
            generation,
            #[cfg(target_os = "linux")]
            config,
            #[cfg(target_os = "linux")]
            resources: Mutex::new(None),
            cleanup_error: Mutex::new(None),
            execution,
            #[cfg(target_os = "linux")]
            cwd,
            #[cfg(target_os = "linux")]
            supervisor: Mutex::new(None),
        });
        #[cfg(target_os = "linux")]
        if let Execution::Local(Err(error)) = &pool.execution {
            pool.unavailable(error);
        }
        super::view()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pool = Some(Arc::downgrade(&pool));
        Ok(pool)
    }

    pub(crate) fn start(self: &Arc<Self>) {
        match &self.execution {
            Execution::Remote(remote) => {
                if let Err(error) = remote.start(&self.queue) {
                    self.unavailable(&error);
                }
            }
            #[cfg(target_os = "linux")]
            Execution::Local(program) => {
                let mut supervisor = self
                    .supervisor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if supervisor.is_some() || !self.queue.accepting() {
                    return;
                }
                match program {
                    Ok(program) => {
                        *supervisor = Some(tokio::spawn(executor::supervise(
                            self.clone(),
                            program.clone(),
                            self.cwd.clone(),
                        )));
                    }
                    Err(error) => self.unavailable(error),
                }
            }
        }
    }

    fn execution_name(&self) -> &'static str {
        match &self.execution {
            Execution::Remote(_) => "remote",
            #[cfg(target_os = "linux")]
            Execution::Local(_) => "local_executor",
        }
    }

    fn remote(&self) -> Result<&Remote<TaskContext>, String> {
        match &self.execution {
            Execution::Remote(remote) => Ok(remote),
            #[cfg(target_os = "linux")]
            Execution::Local(_) => Err(
                "wait/claim/complete are for remote workers; this pool owns a local executor"
                    .into(),
            ),
        }
    }

    pub(crate) fn unavailable(&self, reason: &str) {
        for slot in self.queue.snapshot().slots {
            let _ = self.queue.offline(slot.slot, slot.epoch, reason);
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.queue.close(now_ms());
        match &self.execution {
            Execution::Remote(remote) => remote.close(&self.queue, now_ms())?,
            #[cfg(target_os = "linux")]
            Execution::Local(_) => {
                let handle = self
                    .supervisor
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(handle) = handle {
                    let _ = handle.await;
                }
                let registry = self
                    .resources
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                if let Some(registry) = registry {
                    registry.leave(&self.generation).await?;
                }
                self.queue.cleaned_closed_pool(now_ms())?;
            }
        }
        *self
            .cleanup_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let mut view = super::view()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if view
            .pool
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|pool| pool.generation == self.generation)
        {
            view.pending = Some(self.queue.snapshot());
            view.pool = None;
        }
        Ok(())
    }

    pub(crate) fn tools() -> Vec<Value> {
        let tool = |name: &str,
                    description: &str,
                    properties: Value,
                    required: Value,
                    read_only: bool| {
            json!({
            "name":format!("ssmcp.worker.{name}"),"description":description,
            "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
            "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only}})
        };
        let mut tools = vec![
            tool(
                "status",
                "Read actual idle/working/offline/total counts, the authenticated manager and bounded lifecycle logs. This read never acquires ownership. Remote slots are coordination capacity, not model processes or OS resource limits. Explicit local executors expose their verified resource limits.",
                json!({}),
                json!([]),
                true,
            ),
            tool(
                "attach",
                "Attach this exact verified participant thread as the pool manager. The first explicit attach or successful submit fixes ownership for this native connection. Identical owner retries are idempotent; other agents cannot take over. No model is started and caller-supplied owner IDs are not accepted.",
                json!({}),
                json!([]),
                false,
            ),
            tool(
                "tasks",
                "List only the manager's retained task metadata, ordered by task ID, up to 128 items per page. Prompts, expanded inputs and results are omitted; use result for one task. Terminal retention remains bounded and cursors are not durable across reconnects.",
                json!({"cursor":{"type":"string","maxLength":256},"limit":{"type":"integer","minimum":1,"maximum":128,"default":32}}),
                json!([]),
                true,
            ),
            tool(
                "wait_available",
                "The verified manager waits up to 5 seconds for pool availability to change. Omit after for an immediate initial snapshot; otherwise pass its generation and revision. Returns real idle coordination slot IDs and queued/working counts, not an online-agent roster or a slot reservation. Remote execution still requires an authenticated claim. Stop and connection closure return not_accepting; reconnect requires a new cursor. This wait does not consume human input or start a model.",
                json!({"after":{"type":"object","properties":{"generation":{"type":"string"},"revision":{"type":"integer","minimum":0}},"required":["generation","revision"],"additionalProperties":false},"wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":5000}}),
                json!([]),
                true,
            ),
            tool(
                "stop",
                "The authenticated manager stops this pool from accepting new tasks, cancels queued work and requests cancellation of running work. Running remote slots remain occupied until the exact claimant acknowledges; this does not kill an external agent. A later connection loss makes unconfirmed effects unknown, never replay-safe. This operation cannot restart or resize the pool.",
                json!({}),
                json!([]),
                false,
            ),
            tool(
                "submit",
                "Submit one task as this pool's verified manager; the first successful submission attaches its caller atomically. A bounded FIFO assigns exactly one idle slot; busy workers never fan out a task. Remote mode delegates text to a same-account authenticated agent that calls worker.wait; it never interprets text as a shell command. request_id deduplicates identical retries for the last 128 terminal tasks in this connection; reconnect is not replay permission.",
                json!({"request_id":{"type":"string","minLength":1,"maxLength":128},"text":{"type":"string","minLength":1,"maxLength":16384}}),
                json!(["request_id", "text"]),
                false,
            ),
            tool(
                "result",
                "Read only your own task's result, optionally waiting at most 5 seconds. unknown/cancelling is not permission to replay effects. Workers block locally between tasks without occupying human ui.wait calls.",
                json!({"task_id":{"type":"string"},"wait_ms":{"type":"integer","minimum":0,"maximum":5000}}),
                json!(["task_id"]),
                true,
            ),
            tool(
                "cancel",
                "Request cancellation of your own queued or running task. A running slot stays busy until its local cleanup is confirmed or its exact remote claimant reports completion; this cannot forcibly stop an external agent. Prior effects may remain; inspect the result before retrying.",
                json!({"task_id":{"type":"string"}}),
                json!(["task_id"]),
                false,
            ),
        ];
        tools.extend([
            tool("wait",
                "Block up to 5 seconds for one remote task from the same authenticated account and connection. A timeout means idle, not task completion. Retry the same request_id after a lost response to recover the exact claim; use a new ID after completing it. Completed wait/complete receipts are retained for the last 128 reports. Never execute a task twice after an uncertain result.",
                json!({"request_id":{"type":"string","minLength":1,"maxLength":128},"wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":5000}}), json!(["request_id"]), false),
            tool("claim", "Nonblocking worker.wait. Only a verified remote participant can claim one same-account task; assigning a slot does not execute the text.",
                json!({"request_id":{"type":"string","minLength":1,"maxLength":128}}), json!(["request_id"]), false),
            tool("complete", "Report a remote task's result using the exact claim and authenticated claimant. This records an external agent report, not independently observed OS completion. Identical retries are idempotent while retained; changed results and stale claims are rejected. After completion return to worker.wait.",
                json!({
                    "claim":{"type":"object","properties":{
                        "id":{"type":"string"},"generation":{"type":"string"},
                        "slot":{"type":"integer","minimum":0},"epoch":{"type":"integer","minimum":1},
                        "task_id":{"type":"string"},"attempt":{"type":"integer","minimum":1}},
                        "required":["id","generation","slot","epoch","task_id","attempt"],"additionalProperties":false},
                    "status":{"type":"string","enum":["completed","failed","cancelled"]},
                    "output":{"type":"string","maxLength":65536},"error":{"type":"string","maxLength":4096}
                }), json!(["claim","status"]), false),
        ]);
        tools
    }

    fn availability_value(&self, availability: Availability) -> Value {
        let mut value = json!(availability);
        value["execution"] = json!(self.execution_name());
        value["claim_required"] = json!(matches!(&self.execution, Execution::Remote(_)));
        value
    }

    async fn wait_available(
        &self,
        actor: &Actor,
        client_id: &str,
        request: WaitAvailable,
    ) -> Result<Value, String> {
        if request.wait_ms > 5000 {
            return Err("worker availability wait_ms must be at most 5000".into());
        }
        // Subscribe before the authenticated snapshot so a concurrent release
        // cannot fall between observing the revision and starting the wait.
        let mut wake = self.queue.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.wait_ms);
        loop {
            let availability = self.queue.availability(actor, client_id)?;
            let changed = if let Some(after) = &request.after {
                if after.generation != availability.generation {
                    return Err(
                        "worker availability cursor belongs to another pool generation".into(),
                    );
                }
                if after.revision > availability.revision {
                    return Err(
                        "worker availability cursor is ahead of the current pool revision".into(),
                    );
                }
                after.revision != availability.revision
            } else {
                false
            };
            let status = if !availability.accepting {
                Some("not_accepting")
            } else if request.after.is_none() {
                Some("current")
            } else if changed {
                Some("changed")
            } else if request.wait_ms == 0 || tokio::time::Instant::now() >= deadline {
                Some("unchanged")
            } else {
                None
            };
            if let Some(status) = status {
                return Ok(json!({"status":status,"changed":changed,
                    "availability":self.availability_value(availability)}));
            }
            if matches!(
                tokio::time::timeout_at(deadline, wake.changed()).await,
                Ok(Err(_))
            ) {
                return Err("worker availability observation ended".into());
            }
        }
    }

    async fn wait_remote(
        &self,
        actor: &Actor,
        request_id: &str,
        wait_ms: u64,
    ) -> Result<Value, String> {
        if wait_ms > 5000 {
            return Err("worker wait_ms must be at most 5000".into());
        }
        let remote = self.remote()?;
        let mut wake = self.queue.subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        loop {
            match remote.claim(&self.queue, actor, request_id, now_ms())? {
                Claimed::Assigned { claim, lease } => {
                    return Ok(json!({"status":"assigned","claim":claim,
                        "original_text":lease.text,
                        "text":lease.context.prepared,
                        "cancel_requested":self.queue.cancellation_requested(&lease),
                        "execution":"remote","worker":self.queue.snapshot(),
                        "wait_hint":"Use your own tools to perform this task, then call worker.complete with this exact claim. Reuse this wait request_id to observe cancellation; delivery alone never completes it."}));
                }
                Claimed::Completed { claim, task } => {
                    return Ok(
                        json!({"status":"already_completed","claim":claim,"task":task,
                        "completion_source":"authenticated_remote_agent","worker":self.queue.snapshot(),
                        "wait_hint":"This request_id already completed. Use a new request_id for worker.wait; do not repeat the task."}),
                    );
                }
                Claimed::Waiting if !self.queue.accepting() => {
                    return Ok(
                        json!({"status":"not_accepting","execution":"remote", "worker":self.queue.snapshot(),
                        "wait_hint":"The manager stopped this pool. Existing claimants must reuse their original request_id to acknowledge cancellation; no new tasks will be assigned."}),
                    );
                }
                Claimed::Waiting if wait_ms == 0 || tokio::time::Instant::now() >= deadline => {
                    return Ok(
                        json!({"status":"waiting","execution":"remote","worker":self.queue.snapshot(),"wait_hint":remote::WAIT_HINT}),
                    );
                }
                Claimed::Waiting => {
                    let _ = tokio::time::timeout_at(deadline, wake.changed()).await;
                }
            }
        }
    }

    pub(crate) async fn perform(
        &self,
        name: &str,
        arguments: Value,
        runtime: &Runtime,
    ) -> Result<Value, String> {
        let source = Source::capture();
        let actor = actor(&source)?;
        let client_id = source
            .call
            .as_ref()
            .and_then(|call| call.client_id())
            .ok_or("worker tools require the verified native client identity")?
            .to_owned();
        let mut result = match name {
            "ssmcp.worker.status" => {
                if arguments.as_object().is_none_or(|map| !map.is_empty()) {
                    return Err("worker status accepts an empty object".into());
                }
                let resources = match &self.execution {
                    Execution::Remote(_) => json!({
                        "scope":"external_agent","admitted":false,"limits_enforced":false,
                        "note":"Remote slots do not start local model processes and do not constrain an external agent's CPU or RAM. Explicit local executors retain OS-enforced limits."}),
                    #[cfg(target_os = "linux")]
                    Execution::Local(_) => {
                        let registry = self
                            .resources
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .clone();
                        match registry {
                            Some(registry) => registry
                                .snapshot(&self.generation)
                                .await
                                .unwrap_or_else(|error| json!({"error":error})),
                            None => json!({"scope":"current_os_user","admitted":false}),
                        }
                    }
                };
                Ok(
                    json!({"worker":self.queue.snapshot(),"execution":self.execution_name(),"resources":resources,
                    "caller_is_manager":self.queue.is_manager(&actor, &client_id),
                    "cleanup_error":self.cleanup_error.lock().unwrap_or_else(|error| error.into_inner()).clone()}),
                )
            }
            "ssmcp.worker.attach" => {
                empty_arguments(&arguments, "attach")?;
                source.require_owner()?;
                let manager = self.queue.attach(&actor, &client_id, now_ms())?;
                Ok(
                    json!({"manager":manager,"caller_is_manager":true,"worker":self.queue.snapshot()}),
                )
            }
            "ssmcp.worker.tasks" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct List {
                    #[serde(default)]
                    cursor: Option<String>,
                    #[serde(default = "default_task_limit")]
                    limit: usize,
                }
                fn default_task_limit() -> usize {
                    32
                }
                let request: List = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker task listing: {error}"))?;
                let page = self.queue.tasks(
                    &actor,
                    &client_id,
                    request.cursor.as_deref(),
                    request.limit,
                )?;
                Ok(json!({"page":page,"worker":self.queue.snapshot()}))
            }
            "ssmcp.worker.wait_available" => {
                let request: WaitAvailable = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker availability wait: {error}"))?;
                return self.wait_available(&actor, &client_id, request).await;
            }
            "ssmcp.worker.stop" => {
                empty_arguments(&arguments, "stop")?;
                let changed = self.queue.stop(&actor, &client_id, now_ms())?;
                Ok(
                    json!({"stop_requested":true,"changed":changed,"worker":self.queue.snapshot(),
                    "external_execution_stopped":false,
                    "wait_hint":"Inspect retained task results. A running remote task remains occupied until its original claimant reports completion; cancellation is not proof that external effects stopped."}),
                )
            }
            "ssmcp.worker.submit" => {
                let request: Submit = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid worker task: {e}"))?;
                source.require_owner()?;
                self.queue.check_submitter(&actor, &client_id)?;
                let prepared = runtime.prepare_references(&request.text).await?;
                if prepared.text.len() > 48 * 1024 {
                    return Err("expanded worker input exceeds 48 KiB; no task was admitted".into());
                }
                #[cfg(target_os = "linux")]
                let actor_key = actor_context_key(&actor)?;
                let task = self.queue.submit_managed(
                    actor.clone(),
                    &client_id,
                    &request.request_id,
                    &request.text,
                    TaskContext {
                        #[cfg(target_os = "linux")]
                        source,
                        #[cfg(target_os = "linux")]
                        actor_key,
                        prepared: prepared.text,
                    },
                    now_ms(),
                )?;
                Ok(
                    json!({"task":task,"warnings":prepared.warnings,"worker":self.queue.snapshot(),
                    "execution":self.execution_name(),"wait_hint":"Use worker.result for this task. Remote agents must claim and explicitly complete it; local executors return to their blocking wait after confirmed cleanup."}),
                )
            }
            "ssmcp.worker.result" => {
                let request: Task = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid worker result request: {e}"))?;
                if request.wait_ms > 5000 {
                    return Err("worker result wait_ms must be at most 5000".into());
                }
                let mut wake = self.queue.subscribe();
                let deadline = tokio::time::Instant::now() + Duration::from_millis(request.wait_ms);
                loop {
                    let task = self.queue.result(&actor, &request.task_id)?;
                    if !matches!(
                        task.status,
                        TaskStatus::Queued | TaskStatus::Working | TaskStatus::Cancelling
                    ) || request.wait_ms == 0
                        || tokio::time::Instant::now() >= deadline
                    {
                        break Ok(json!({"task":task,"worker":self.queue.snapshot()}));
                    }
                    let _ = tokio::time::timeout_at(deadline, wake.changed()).await;
                }
            }
            "ssmcp.worker.cancel" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Cancel {
                    task_id: String,
                }
                let request: Cancel = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid worker cancellation: {e}"))?;
                Ok(
                    json!({"task":self.queue.cancel(&actor, &request.task_id, now_ms())?,"worker":self.queue.snapshot()}),
                )
            }
            "ssmcp.worker.wait" => {
                fn default_wait() -> u64 {
                    5000
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Wait {
                    request_id: String,
                    #[serde(default = "default_wait")]
                    wait_ms: u64,
                }
                let request: Wait = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid remote worker wait: {error}"))?;
                self.wait_remote(&actor, &request.request_id, request.wait_ms)
                    .await
            }
            "ssmcp.worker.claim" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct ClaimRequest {
                    request_id: String,
                }
                let request: ClaimRequest = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid remote worker claim: {error}"))?;
                self.wait_remote(&actor, &request.request_id, 0).await
            }
            "ssmcp.worker.complete" => {
                let completion: Completion = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid remote completion: {error}"))?;
                let claim = completion.claim.clone();
                let (task, duplicate) =
                    self.remote()?
                        .complete(&self.queue, &actor, completion, now_ms())?;
                Ok(
                    json!({"status":"recorded","claim":claim,"task":task,"duplicate":duplicate,
                    "completion_source":"authenticated_remote_agent","worker":self.queue.snapshot(),
                    "wait_hint":remote::WAIT_HINT}),
                )
            }
            _ => Err("unknown worker tool".into()),
        }?;
        // Only the exact manager receives this convenience projection. The
        // underlying task/claim authorization above is never replaced by it.
        if self.queue.is_manager(&actor, &client_id) {
            result["availability"] =
                self.availability_value(self.queue.availability(&actor, &client_id)?);
        }
        Ok(result)
    }
}

fn empty_arguments(arguments: &Value, operation: &str) -> Result<(), String> {
    if arguments.as_object().is_none_or(|map| !map.is_empty()) {
        Err(format!("worker {operation} accepts an empty object"))
    } else {
        Ok(())
    }
}

#[cfg(any(target_os = "linux", test))]
fn actor_context_key(actor: &Actor) -> Result<String, String> {
    // Field boundaries and an absent participant are part of the identity.
    let encoded = serde_json::to_vec(&(
        actor.provider.as_str(),
        actor.subject.as_str(),
        actor.participant.as_deref(),
        actor.connection,
    ))
    .map_err(|error| format!("cannot encode worker actor identity: {error}"))?;
    Ok(crate::hex::encode(Sha256::digest(encoded)))
}

fn actor(source: &Source) -> Result<Actor, String> {
    let call = source
        .call
        .as_ref()
        .ok_or("worker tools require an authenticated relay call")?;
    let attribution = call
        .attribution()
        .ok_or("worker tools require verified account attribution")?;
    let participant = attribution
        .agent
        .as_ref()
        .ok_or("worker tools require a verified collab-bound participant")?;
    let thread = call
        .thread()
        .ok_or("bind the verified participant to this worker client before calling worker tools")?;
    if thread.thread_id() != participant.id || Some(thread.session_id()) != call.client_id() {
        return Err(
            "worker call thread does not match its authenticated participant and client".into(),
        );
    }
    Ok(Actor {
        provider: attribution.account.provider.clone(),
        subject: attribution.account.subject.clone(),
        participant: Some(participant.id.clone()),
        connection: call.connection(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const AVAILABILITY_CLIENT: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn availability_pool(count: usize) -> (Arc<Pool>, Actor) {
        let pool = Pool::new(
            Config { count, factor: 1.0 },
            &Agent::Remote,
            PathBuf::new(),
        )
        .unwrap();
        pool.start();
        let owner = Actor {
            provider: "fixture".into(),
            subject: "account".into(),
            participant: Some("manager".into()),
            connection: 1,
        };
        pool.queue.attach(&owner, AVAILABILITY_CLIENT, 1).unwrap();
        (pool, owner)
    }

    fn availability_cursor(value: &Value) -> AvailabilityCursor {
        AvailabilityCursor {
            generation: value["generation"].as_str().unwrap().into(),
            revision: value["revision"].as_u64().unwrap(),
        }
    }

    #[tokio::test]
    async fn manager_wait_observes_real_idle_release_without_repeating_completed_notifications() {
        let (pool, owner) = availability_pool(2);
        let initial = pool
            .wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: None,
                    wait_ms: 5000,
                },
            )
            .await
            .unwrap();
        assert_eq!(initial["status"], "current");
        assert_eq!(initial["availability"]["scope"], "coordination_slots");
        assert_eq!(initial["availability"]["execution"], "remote");
        assert_eq!(initial["availability"]["claim_required"], true);
        assert_eq!(
            initial["availability"]["idle_slots"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        let task = pool
            .queue
            .submit_managed(
                owner.clone(),
                AVAILABILITY_CLIENT,
                "availability-task",
                "literal task",
                TaskContext {
                    #[cfg(target_os = "linux")]
                    source: Source::default(),
                    #[cfg(target_os = "linux")]
                    actor_key: actor_context_key(&owner).unwrap(),
                    prepared: "prepared task".into(),
                },
                2,
            )
            .unwrap();
        let executor = Actor {
            participant: Some("executor".into()),
            ..owner.clone()
        };
        let assigned = pool
            .wait_remote(&executor, "availability-claim", 0)
            .await
            .unwrap();
        assert_eq!(assigned["claim"]["task_id"], task.task_id);
        let busy = pool.availability_value(
            pool.queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        assert_eq!(busy["working"], 1);
        assert_eq!(busy["idle"], 1);
        let pending = pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&busy)),
                wait_ms: 5000,
            },
        );
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("unchanged availability returned early: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let completion: Completion = serde_json::from_value(json!({
            "claim":assigned["claim"],"status":"completed","output":"done"
        }))
        .unwrap();
        assert!(
            !pool
                .remote()
                .unwrap()
                .complete(&pool.queue, &executor, completion.clone(), 3)
                .unwrap()
                .1
        );
        let released = pending.await.unwrap();
        assert_eq!(released["status"], "changed");
        assert_eq!(released["changed"], true);
        assert_eq!(released["availability"]["working"], 0);
        assert_eq!(released["availability"]["idle"], 2);
        assert!(
            released["availability"]["idle_slots"]
                .as_array()
                .unwrap()
                .iter()
                .any(|slot| slot["slot"] == assigned["claim"]["slot"]
                    && slot["epoch"] == assigned["claim"]["epoch"])
        );
        assert!(
            pool.remote()
                .unwrap()
                .complete(&pool.queue, &executor, completion, 4)
                .unwrap()
                .1
        );
        let unchanged = pool
            .wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: Some(availability_cursor(&released["availability"])),
                    wait_ms: 1,
                },
            )
            .await
            .unwrap();
        assert_eq!(unchanged["status"], "unchanged");
        assert_eq!(unchanged["changed"], false);
        assert_eq!(unchanged["availability"], released["availability"]);
        pool.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_availability_stop_and_disconnect_never_offer_idle_slots() {
        let (pool, owner) = availability_pool(1);
        let initial = pool.availability_value(
            pool.queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        let pending = pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&initial)),
                wait_ms: 5000,
            },
        );
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("unchanged availability returned early: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert!(pool.queue.stop(&owner, AVAILABILITY_CLIENT, 2).unwrap());
        let stopped = pending.await.unwrap();
        assert_eq!(stopped["status"], "not_accepting");
        assert_eq!(stopped["changed"], true);
        assert_eq!(stopped["availability"]["accepting"], false);
        assert_eq!(stopped["availability"]["idle_slots"], json!([]));
        assert!(!pool.queue.stop(&owner, AVAILABILITY_CLIENT, 3).unwrap());
        let repeated = pool
            .wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: Some(availability_cursor(&stopped["availability"])),
                    wait_ms: 5000,
                },
            )
            .await
            .unwrap();
        assert_eq!(repeated["status"], "not_accepting");
        assert_eq!(repeated["changed"], false);
        pool.shutdown().await.unwrap();

        let (fresh, owner) = availability_pool(1);
        let before = fresh.availability_value(
            fresh
                .queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        fresh.queue.close(4);
        let disconnected = fresh
            .wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: Some(availability_cursor(&before)),
                    wait_ms: 5000,
                },
            )
            .await
            .unwrap();
        assert_eq!(disconnected["status"], "not_accepting");
        assert_eq!(disconnected["availability"]["closed"], true);
        assert_eq!(disconnected["availability"]["accepting"], false);
        assert_eq!(disconnected["availability"]["idle_slots"], json!([]));
        fresh.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_availability_rejects_foreign_identity_stale_generation_and_invalid_waits() {
        let (pool, owner) = availability_pool(1);
        let current = pool.availability_value(
            pool.queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        let observer = Actor {
            participant: Some("observer".into()),
            ..owner.clone()
        };
        assert!(
            pool.wait_available(
                &observer,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: None,
                    wait_ms: 5000
                }
            )
            .await
            .is_err()
        );
        assert!(
            pool.wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: None,
                    wait_ms: 5001
                }
            )
            .await
            .is_err()
        );
        let mut future = availability_cursor(&current);
        future.revision += 1;
        assert!(
            pool.wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: Some(future),
                    wait_ms: 0
                }
            )
            .await
            .is_err()
        );
        for value in [
            json!({"wait_ms":-1}),
            json!({"after":{"generation":pool.generation,"revision":-1}}),
            json!({"after":{"generation":pool.generation,"revision":0,"owner":"forged"}}),
            json!({"owner":"forged"}),
        ] {
            assert!(serde_json::from_value::<WaitAvailable>(value).is_err());
        }
        let fresh = Pool::new(
            Config {
                count: 1,
                factor: 1.0,
            },
            &Agent::Remote,
            PathBuf::new(),
        )
        .unwrap();
        fresh.start();
        assert!(
            fresh
                .wait_available(
                    &owner,
                    AVAILABILITY_CLIENT,
                    WaitAvailable {
                        after: None,
                        wait_ms: 5000
                    }
                )
                .await
                .is_err()
        );
        assert!(fresh.queue.snapshot().manager.is_none());
        let next = Actor {
            connection: owner.connection + 1,
            ..owner.clone()
        };
        fresh.queue.attach(&next, AVAILABILITY_CLIENT, 4).unwrap();
        assert!(
            fresh
                .wait_available(
                    &next,
                    AVAILABILITY_CLIENT,
                    WaitAvailable {
                        after: Some(availability_cursor(&current)),
                        wait_ms: 5000
                    }
                )
                .await
                .is_err()
        );
        assert!(
            fresh
                .wait_available(
                    &owner,
                    AVAILABILITY_CLIENT,
                    WaitAvailable {
                        after: None,
                        wait_ms: 5000
                    }
                )
                .await
                .is_err()
        );
        pool.shutdown().await.unwrap();
        fresh.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn remote_wait_parks_until_a_real_task_without_starting_a_local_executor() {
        let pool = Pool::new(
            Config {
                count: 2,
                factor: 1.0,
            },
            &Agent::Remote,
            PathBuf::new(),
        )
        .unwrap();
        assert_eq!(pool.queue.snapshot().idle, 0);
        pool.start();
        assert_eq!(pool.execution_name(), "remote");
        assert_eq!(pool.queue.snapshot().idle, 2);
        #[cfg(target_os = "linux")]
        {
            assert!(pool.supervisor.lock().unwrap().is_none());
            assert!(pool.resources.lock().unwrap().is_none());
        }
        let executor = Actor {
            provider: "fixture".into(),
            subject: "owner".into(),
            participant: Some("executor".into()),
            connection: 1,
        };
        let pending = pool.wait_remote(&executor, "wait-one", 5000);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("idle wait returned before a task: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let mut owner = executor.clone();
        owner.participant = Some("submitter".into());
        let task = pool
            .queue
            .submit(
                owner.clone(),
                "task-one",
                "literal text",
                TaskContext {
                    #[cfg(target_os = "linux")]
                    source: Source::default(),
                    #[cfg(target_os = "linux")]
                    actor_key: actor_context_key(&owner).unwrap(),
                    prepared: "prepared task text".into(),
                },
                1,
            )
            .unwrap();
        let assigned = pending.await.unwrap();
        assert_eq!(assigned["status"], "assigned");
        assert_eq!(assigned["text"], "prepared task text");
        assert_eq!(assigned["claim"]["task_id"], task.task_id);
        assert_eq!(
            pool.queue.result(&owner, &task.task_id).unwrap().status,
            TaskStatus::Working
        );
        let completion: Completion = serde_json::from_value(json!({
            "claim":assigned["claim"],"status":"completed","output":"done"
        }))
        .unwrap();
        pool.remote()
            .unwrap()
            .complete(&pool.queue, &executor, completion, 2)
            .unwrap();
        assert_eq!(pool.queue.snapshot().idle, 2);
        assert_eq!(
            pool.wait_remote(&executor, "idle-now", 0).await.unwrap()["status"],
            "waiting"
        );
        pool.shutdown().await.unwrap();
        assert_eq!(pool.queue.snapshot().offline, 2);
    }

    #[test]
    fn worker_remote_catalog_keeps_management_and_execution_tools_separate() {
        let tools = Pool::tools();
        let names: std::collections::BTreeSet<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 11);
        for name in [
            "status",
            "attach",
            "tasks",
            "stop",
            "submit",
            "result",
            "cancel",
            "wait",
            "claim",
            "complete",
            "wait_available",
        ] {
            assert!(names.contains(format!("ssmcp.worker.{name}").as_str()));
        }
        assert!(
            tools
                .iter()
                .all(|tool| tool["inputSchema"]["additionalProperties"] == false)
        );
        assert!(actor(&Source::default()).is_err());
    }

    #[test]
    fn actor_context_keys_preserve_field_boundaries_and_optional_participants() {
        let actor = Actor {
            provider: "provider:subject".into(),
            subject: "value".into(),
            participant: None,
            connection: 7,
        };
        let key = actor_context_key(&actor).unwrap();
        assert_eq!(key.len(), 64);
        assert!(
            key.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!(key, actor_context_key(&actor.clone()).unwrap());

        // These two identities collided under colon-separated concatenation.
        let mut moved_boundary = actor.clone();
        moved_boundary.provider = "provider".into();
        moved_boundary.subject = "subject:value".into();
        assert_ne!(key, actor_context_key(&moved_boundary).unwrap());

        let mut empty_participant = actor.clone();
        empty_participant.participant = Some(String::new());
        assert_ne!(key, actor_context_key(&empty_participant).unwrap());

        let mut new_connection = actor;
        new_connection.connection += 1;
        assert_ne!(key, actor_context_key(&new_connection).unwrap());
    }

    #[test]
    fn task_arguments_cannot_create_authenticated_identity_or_select_a_slot() {
        for extra in [
            "actor",
            "manager",
            "client_id",
            "thread_id",
            "provider",
            "subject",
            "agent_id",
            "slot",
            "generation",
            "attempt",
        ] {
            let mut value = json!({"request_id":"one","text":"work"});
            value[extra] = json!("forged");
            assert!(serde_json::from_value::<Submit>(value).is_err());
        }
        assert!(actor(&Source::default()).is_err());
    }

    #[test]
    fn manager_identity_comes_only_from_the_verified_call_thread() {
        use crate::{
            runtime::call_scope::CallScope,
            wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
        };
        let client = "a".repeat(64);
        let participant = format!("agent_{}", "b".repeat(32));
        let attribution = ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "123".into(),
                login: "same-label".into(),
            },
            owner: None,
            agent: Some(ControlAgent {
                id: participant.clone(),
                name: "same display name".into(),
            }),
        };
        let mut source = Source::default();
        source.call = Some(
            CallScope::from_relay_for_client(7, Some(&client), 1, Some(&attribution), None)
                .unwrap(),
        );
        assert!(
            actor(&source).is_err(),
            "account/participant without a verified thread is not a manager"
        );
        let context = CodeContext {
            run_id: "c".repeat(32),
            code: "return 1".into(),
            timestamp_ms: 1,
            session_id: Some(client.clone()),
            thread_id: Some(participant.clone()),
        };
        source.call = Some(
            CallScope::from_relay_for_client(
                7,
                Some(&client),
                2,
                Some(&attribution),
                Some(&context),
            )
            .unwrap(),
        );
        let caller = actor(&source).unwrap();
        let queue = Queue::<()>::new("owned-pool".into(), 1).unwrap();
        let manager = queue.attach(&caller, &client, 1).unwrap();
        assert_eq!(manager.thread_id, participant);
        assert_eq!(manager.client_id, client);
        assert_eq!(manager.connection, 7);
        assert!(empty_arguments(&json!({"manager":"another participant"}), "attach").is_err());
        assert!(empty_arguments(&json!({"force":true}), "stop").is_err());
    }
}
