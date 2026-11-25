use super::{
    Config, MAX_OBSERVATION_WAIT_MS, ManagerTargetView, now_ms,
    queue::{
        Actor, Availability, EventRequest, Queue, Retirement, TaskStatus, WaitRequest, WaitSpec,
    },
    remote::{self, Claimed, CommandLog, Completion, Remote, Report},
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

    pub(crate) fn authenticated_contact(&self, source: &Source) {
        if self.remote().is_ok()
            && let Ok(actor) = actor(source)
        {
            self.queue
                .authenticated_contact(&actor, std::time::Instant::now());
        }
    }

    pub(crate) fn reconcile_roster(
        &self,
        connection: u64,
        sent: std::time::Instant,
        agents: &[crate::wire::SteeringAgent],
    ) {
        if let Ok(remote) = self.remote() {
            remote.reconcile_roster(&self.queue, connection, sent, agents, now_ms());
        }
    }

    fn remote(&self) -> Result<&Remote<TaskContext>, String> {
        match &self.execution {
            Execution::Remote(remote) => Ok(remote),
            #[cfg(target_os = "linux")]
            Execution::Local(_) => Err(
                "remote worker operations are unavailable for a pool that owns local executors"
                    .into(),
            ),
        }
    }

    /// Trusted local UI/IPC entry point. Remote callers use worker.resize,
    /// which separately verifies the already attached manager under the lock.
    pub(crate) fn resize(&self, total: usize) -> Result<Value, String> {
        self.remote()?;
        self.queue.resize(total)?;
        Ok(self.resize_result())
    }

    pub(crate) fn human_resize(&self, total: usize) -> Result<Value, String> {
        self.remote()?;
        let targets = self.queue.human_resize(total)?;
        let mut receipt = self.resize_result();
        receipt["retirement_targets"] = json!(targets);
        Ok(receipt)
    }

    pub(crate) fn human_select_pending_retirement(
        &self,
        target: usize,
        connection: u64,
    ) -> Result<Value, String> {
        self.remote()?;
        let retirement = self.queue.select_pending_retirement(target, connection)?;
        let mut receipt = self.resize_result();
        receipt["retirement_targets"] = json!([retirement]);
        Ok(receipt)
    }

    pub(crate) fn force_worker_retirements(&self, targets: &[Retirement]) -> Result<Value, String> {
        let count = self
            .remote()?
            .force_retirements(&self.queue, targets, now_ms())?;
        let mut receipt = self.resize_result();
        receipt["forced_stations"] = json!(count);
        Ok(receipt)
    }

    fn resize_result(&self) -> Value {
        let worker = self.queue.compact_snapshot();
        json!({"resize_pending":worker["total"] != worker["desired_total"],"worker":worker})
    }

    pub(crate) fn command_started(
        &self,
        source: &Source,
        command_id: &str,
        command: &str,
    ) -> Result<Option<CommandLog>, String> {
        #[cfg(target_os = "linux")]
        let remote = match &self.execution {
            Execution::Remote(remote) => remote,
            Execution::Local(_) => return Ok(None),
        };
        #[cfg(not(target_os = "linux"))]
        let Execution::Remote(remote) = &self.execution;
        let actor = actor(source)?;
        remote.command_started(&self.queue, &actor, command_id, command, now_ms())
    }

    pub(crate) fn command_finished(
        &self,
        command: &CommandLog,
        detail: &str,
    ) -> Result<(), String> {
        self.remote()?
            .command_finished(&self.queue, command, detail, now_ms())
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
        let claim_schema = json!({"type":"object","properties":{
            "id":{"type":"string"},"generation":{"type":"string"},
            "slot":{"type":"integer","minimum":0,"maximum":127},
            "epoch":{"type":"integer","minimum":1},"task_id":{"type":"string"},
            "attempt":{"type":"integer","minimum":1}},
            "required":["id","generation","slot","epoch","task_id","attempt"],"additionalProperties":false});
        let retirement_schema = json!({"type":"object","properties":{
            "id":{"type":"string"},"generation":{"type":"string"},
            "slot":{"type":"integer","minimum":0,"maximum":127},"epoch":{"type":"integer","minimum":1}},
            "required":["id","generation","slot","epoch"],"additionalProperties":false});
        let mut tools = vec![
            tool(
                "status",
                "Read actual idle/working/offline/retiring/total counts, registered workers, desired size, the authenticated manager, bounded task summaries and claim-scoped activity logs. Unconnected slots are offline; retiring workers still await an explicit release. This read never acquires ownership. Remote slots are coordination capacity, not model processes or OS resource limits.",
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
                "accept_handoff",
                "Consume a local operator's coordinator invitation using the named successor's bound pool identity. Retry the same handoff ID after a lost receipt. Read retained task IDs and original request IDs with worker.tasks, then reconcile results before dispatch.",
                json!({"handoff_id":{"type":"string","minLength":32,"maxLength":32}}),
                json!(["handoff_id"]),
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
                "events",
                "Read or wait up to 55 seconds for bounded worker lifecycle metadata recorded at state commit. Reading never consumes or acknowledges events, binds a manager, registers a station, or replays work. Omit after to read retained entries, then reuse next_cursor. A retention overflow is explicit in gap; stale generations and future cursors are rejected. Only a verified participant from this pool's account and connection can read. Bodies, commands, results and credentials are excluded; worker.completed records a confirmed task outcome, while worker.unknown remains indeterminate.",
                json!({"after":{"type":"object","properties":{"generation":{"type":"string","minLength":1,"maxLength":128},"seq":{"type":"integer","minimum":0}},"required":["generation","seq"],"additionalProperties":false},"limit":{"type":"integer","minimum":1,"maximum":128,"default":32},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_OBSERVATION_WAIT_MS,"default":0}}),
                json!([]),
                true,
            ),
            tool(
                "wait_available",
                "The verified manager observes capacity or waits up to 55 seconds for it to change. Omit after for an immediate snapshot; otherwise reuse its generation and revision. Remote idle slots require real worker registration; offline capacity is not a standby agent. Counts include retiring stations until release, and desired_total shows pending resize. accepting describes new valid task admission; zero capacity, all-retiring stations, a full queue, stop or closure disable it. Revisions ignore command logs and progress. Idle slot IDs and epochs are observations, never reservations. Reconnect requires a new cursor. This wait does not consume human input or start a model.",
                json!({"after":{"type":"object","properties":{"generation":{"type":"string"},"revision":{"type":"integer","minimum":0}},"required":["generation","revision"],"additionalProperties":false},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_OBSERVATION_WAIT_MS,"default":5000}}),
                json!([]),
                true,
            ),
            tool(
                "wait_for",
                "The verified manager waits up to 55 seconds for any or all explicit conditions in this pool generation. task_finished means a confirmed completed, failed or cancelled result; unknown is indeterminate. worker_idle identifies the exact slot and epoch and means currently allocatable, not merely offline. Conditions are evaluated together, never accumulated across time. This observes state, reserves no slot, consumes no human input and starts no model. Cancelling this call ends only the wait. Each call gets a separate operation_id and elapsed time.",
                json!({"generation":{"type":"string","minLength":1,"maxLength":128},"mode":{"type":"string","enum":["any","all"]},"conditions":{"type":"array","minItems":1,"maxItems":128,"uniqueItems":true,"items":{"oneOf":[{"type":"object","properties":{"type":{"const":"task_finished"},"task_id":{"type":"string","minLength":1,"maxLength":256}},"required":["type","task_id"],"additionalProperties":false},{"type":"object","properties":{"type":{"const":"worker_idle"},"slot":{"type":"integer","minimum":0,"maximum":127},"epoch":{"type":"integer","minimum":1}},"required":["type","slot","epoch"],"additionalProperties":false}]}},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_OBSERVATION_WAIT_MS,"default":5000}}),
                json!(["generation", "mode", "conditions"]),
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
                "resize",
                "The already attached authenticated manager sets remote worker capacity to 0..128. Growth adds offline slots without starting agents. Shrink removes empty offline slots first, then requests idle workers to release, then running workers if necessary. Running claims and results remain intact until exact completion and release acknowledgements. A pending resize accepts only the same target; external processes are never killed. Local executor pools cannot be resized this way.",
                json!({"total":{"type":"integer","minimum":0,"maximum":128}}),
                json!(["total"]),
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
                "Read only your own task's result, optionally waiting at most 55 seconds. unknown/cancelling is not permission to replay effects. Workers block locally between tasks without occupying human ui.wait calls.",
                json!({"task_id":{"type":"string"},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_OBSERVATION_WAIT_MS}}),
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
                "Register this authenticated external worker in one stable station and block up to 55 seconds for one same-account task. A timeout does not complete a task. Retry the same request_id after a lost response to recover its exact claim; use a new ID after completion. A retiring response requires stopping owned activity and acknowledging worker.release; a released response means stop waiting. No model is started. Receipts are retained for the last 128 reports.",
                json!({"request_id":{"type":"string","minLength":1,"maxLength":128},"wait_ms":{"type":"integer","minimum":0,"maximum":MAX_OBSERVATION_WAIT_MS,"default":5000}}), json!(["request_id"]), false),
            tool("claim", "Nonblocking worker.wait. Only a verified remote participant can claim one same-account task; assigning a slot does not execute the text.",
                json!({"request_id":{"type":"string","minLength":1,"maxLength":128}}), json!(["request_id"]), false),
            tool("complete", "Report a remote task's result using the exact claim and authenticated claimant. This records an external agent report, not independently observed OS completion. Identical retries are idempotent while retained; changed results and stale claims are rejected. After completion return to worker.wait; the idle deadline precedes Dreaming. Completion alone does not authorize offline.",
                json!({
                    "claim":claim_schema,
                    "status":{"type":"string","enum":["completed","failed","cancelled"]},
                    "output":{"type":"string","maxLength":65536},"error":{"type":"string","maxLength":4096}
                }), json!(["claim","status"]), false),
            tool("report", "Report real command start/end or progress for your exact active claim. The same request_id and content are idempotent while retained; changed content, another caller or stale claims are rejected. Text is bounded and sanitized for the shared worker monitor. Report progress only after it happened; this never completes a task. The response also exposes pending cancellation and retirement.",
                json!({"claim":claim_schema,"request_id":{"type":"string","minLength":1,"maxLength":128},
                    "event":{"type":"string","enum":["command_start","command_end","progress"]},
                    "command":{"type":"string","minLength":1,"maxLength":4096},
                    "progress":{"type":"string","minLength":1,"maxLength":1024},
                    "detail":{"type":"string","minLength":1,"maxLength":1024}}),
                json!(["claim","request_id","event"]), false),
            tool("release", "Acknowledge the exact pool-ordered retirement receipt returned by worker.wait, using the same authenticated worker. First finish the active claim and stop starting work; release is refused while a task remains active. This is the cooperative release path. The local human can force a pending retirement, and a fresh broker roster can establish that membership ended. Unconfirmed work then becomes unknown. Identical retained retries are idempotent. This records your shutdown acknowledgement; it does not kill an external model or process. After success, leave your collaboration session if needed and stop waiting.",
                json!({"retirement":retirement_schema}), json!(["retirement"]), false),
        ]);
        tools
    }

    fn availability_value(&self, availability: Availability) -> Value {
        let mut value = json!(availability);
        value["execution"] = json!(self.execution_name());
        value["claim_required"] = json!(matches!(&self.execution, Execution::Remote(_)));
        value
    }

    fn decorate_availability(&self, actor: &Actor, client_id: &str, mut result: Value) -> Value {
        // The local UI may show manager intent. An observing executor must not
        // gain the manager's private task listing through a status response.
        if !self.queue.is_manager(actor, client_id)
            && let Some(worker) = result.get_mut("worker").and_then(Value::as_object_mut)
        {
            worker.remove("manager_operations");
        }
        // This uses the exact existing manager binding and never acquires one.
        // A failed convenience observation must not erase a committed result.
        if let Ok(availability) = self.queue.availability(actor, client_id) {
            result["availability"] = self.availability_value(availability);
        }
        result
    }

    async fn wait_available(
        &self,
        actor: &Actor,
        client_id: &str,
        request: WaitAvailable,
    ) -> Result<Value, String> {
        if request.wait_ms > MAX_OBSERVATION_WAIT_MS {
            return Err(format!(
                "worker availability wait_ms must be at most {MAX_OBSERVATION_WAIT_MS}"
            ));
        }
        // Subscribe before the authenticated snapshot to avoid missing a change
        // between observing its revision and beginning the wait.
        let mut wake = self.queue.subscribe_capacity();
        let initial = self.queue.availability(actor, client_id)?;
        if let Some(after) = &request.after {
            if after.generation != initial.generation {
                return Err("worker availability cursor belongs to another pool generation".into());
            }
            if after.revision > initial.revision {
                return Err(
                    "worker availability cursor is ahead of the current pool revision".into(),
                );
            }
        }
        let guard = self.queue.begin_wait(
            actor,
            client_id,
            WaitSpec {
                kind: "wait_available",
                mode: "change",
                wait_ms: request.wait_ms,
                targets: vec![ManagerTargetView {
                    kind: "availability_change",
                    task_id: None,
                    slot: None,
                    epoch: None,
                    revision: request.after.as_ref().map(|after| after.revision),
                    summary: None,
                }],
            },
            now_ms(),
        )?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.wait_ms);
        loop {
            let availability = match self.queue.availability(actor, client_id) {
                Ok(value) => value,
                Err(error) => {
                    guard.finish("error");
                    return Err(error);
                }
            };
            // Generation is immutable and the checked capacity cursor never
            // moves backwards; exhaustion is rejected by availability().
            let changed = request
                .after
                .as_ref()
                .is_some_and(|after| after.revision != availability.revision);
            let status = if !availability.accepting() {
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
                let operation = guard.finish(status);
                return Ok(json!({"status":status,"changed":changed,
                    "operation_id":operation.operation_id,"elapsed_ms":operation.elapsed_ms,
                    "availability":self.availability_value(availability)}));
            }
            if matches!(
                tokio::time::timeout_at(deadline, wake.changed()).await,
                Ok(Err(_))
            ) {
                guard.finish("error");
                return Err("worker availability observation ended".into());
            }
        }
    }

    async fn wait_for(
        &self,
        actor: &Actor,
        client_id: &str,
        request: WaitRequest,
    ) -> Result<Value, String> {
        // Subscribe before registration and the first checked state. Display
        // registration itself does not publish a capacity or queue revision.
        let mut wake = self.queue.subscribe();
        let guard = self
            .queue
            .begin_conditions(actor, client_id, &request, now_ms())?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.wait_ms);
        loop {
            let conditions = match self.queue.conditions(actor, client_id, &request) {
                Ok(conditions) => conditions,
                Err(error) => {
                    guard.finish("error");
                    return Err(error);
                }
            };
            let status = request.outcome(&conditions).or_else(|| {
                (request.wait_ms == 0 || tokio::time::Instant::now() >= deadline)
                    .then_some("timed_out")
            });
            if let Some(status) = status {
                let operation = guard.finish(status);
                return Ok(
                    json!({"status":status,"operation_id":operation.operation_id,
                    "generation":self.generation,"mode":request.mode,
                    "elapsed_ms":operation.elapsed_ms,"conditions":conditions,
                    "worker":self.queue.compact_snapshot()}),
                );
            }
            if matches!(
                tokio::time::timeout_at(deadline, wake.changed()).await,
                Ok(Err(_))
            ) {
                guard.finish("error");
                return Err("worker condition observation ended".into());
            }
        }
    }

    async fn wait_result(
        &self,
        actor: &Actor,
        client_id: &str,
        request: Task,
    ) -> Result<Value, String> {
        if request.wait_ms > MAX_OBSERVATION_WAIT_MS {
            return Err(format!(
                "worker result wait_ms must be at most {MAX_OBSERVATION_WAIT_MS}"
            ));
        }
        let mut wake = self.queue.subscribe();
        self.queue.result(actor, &request.task_id)?;
        // Preserve the original task-owner permission even for internal tasks
        // that were not admitted through the manager API.
        let guard = if self.queue.is_manager(actor, client_id) {
            Some(self.queue.begin_wait(
                actor,
                client_id,
                WaitSpec {
                    kind: "result",
                    mode: "single",
                    wait_ms: request.wait_ms,
                    targets: vec![ManagerTargetView {
                        kind: "task_result",
                        task_id: Some(request.task_id.clone()),
                        slot: None,
                        epoch: None,
                        revision: None,
                        summary: None,
                    }],
                },
                now_ms(),
            )?)
        } else {
            None
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(request.wait_ms);
        loop {
            let task = match self.queue.result(actor, &request.task_id) {
                Ok(task) => task,
                Err(error) => {
                    if let Some(guard) = guard {
                        guard.finish("error");
                    }
                    return Err(error);
                }
            };
            let pending = matches!(
                task.status,
                TaskStatus::Queued | TaskStatus::Working | TaskStatus::Cancelling
            );
            if !pending || request.wait_ms == 0 || tokio::time::Instant::now() >= deadline {
                if let Some(guard) = guard {
                    guard.finish(if pending {
                        "timed_out"
                    } else if task.status == TaskStatus::Unknown {
                        "indeterminate"
                    } else {
                        "satisfied"
                    });
                }
                return Ok(json!({"task":task,"worker":self.queue.compact_snapshot()}));
            }
            let _ = tokio::time::timeout_at(deadline, wake.changed()).await;
        }
    }

    async fn wait_events(
        &self,
        actor: &Actor,
        client_id: &str,
        request: EventRequest,
    ) -> Result<Value, String> {
        request.validate()?;
        // Subscribe first: a commit between reading the page and waiting must
        // remain observable even if its original RPC reply is lost.
        let mut wake = self.queue.subscribe();
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(request.wait_ms);
        loop {
            let page = self.queue.events(actor, client_id, &request)?;
            let ready = !page.events.is_empty() || page.gap;
            if ready
                || page.closed
                || request.wait_ms == 0
                || tokio::time::Instant::now() >= deadline
            {
                let status = if ready {
                    "events"
                } else if page.closed {
                    "closed"
                } else if request.wait_ms == 0 {
                    "empty"
                } else {
                    "timed_out"
                };
                let mut result = serde_json::to_value(page)
                    .map_err(|error| format!("encode worker events: {error}"))?;
                result["status"] = json!(status);
                return Ok(result);
            }
            let _ = tokio::time::timeout_at(deadline, wake.changed()).await;
        }
    }

    async fn wait_remote(
        &self,
        actor: &Actor,
        request_id: &str,
        wait_ms: u64,
    ) -> Result<Value, String> {
        if wait_ms > MAX_OBSERVATION_WAIT_MS {
            return Err(format!(
                "worker wait_ms must be at most {MAX_OBSERVATION_WAIT_MS}"
            ));
        }
        let remote = self.remote()?;
        let mut wake = self.queue.subscribe_capacity();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait_ms);
        let request_started = std::time::Instant::now();
        loop {
            match remote.claim_since(&self.queue, actor, request_id, now_ms(), request_started)? {
                Claimed::Assigned { claim, lease } => {
                    return Ok(json!({"status":"assigned","claim":claim,
                        "original_text":lease.text,
                        "text":lease.context.prepared,
                        "cancel_requested":self.queue.cancellation_requested(&lease),
                        "retire_requested":self.queue.retire_requested(&lease),
                        "execution":"remote","worker":self.queue.compact_snapshot(),
                        "wait_hint":"Resume working immediately; this claim resets any Dreaming time and invalidates its exit request. Use your own tools to perform this task, then call worker.complete with this exact claim. Reuse this wait request_id to observe cancellation; delivery alone never completes it."}));
                }
                Claimed::Completed { claim, task } => {
                    return Ok(
                        json!({"status":"already_completed","claim":claim,"task":task,
                        "completion_source":"authenticated_remote_agent","worker":self.queue.compact_snapshot(),
                        "wait_hint":"This request_id already completed. Use a new request_id for worker.wait; do not repeat the task."}),
                    );
                }
                Claimed::Retiring { retirement } => {
                    return Ok(json!({"status":"retiring","retirement":retirement,
                        "execution":"remote","worker":self.queue.compact_snapshot(),
                        "wait_hint":"The pool ordered retirement of this station. Stop starting work, ensure your owned activity has stopped, then call worker.release with this exact retirement receipt. Do not leave before acknowledging release."}));
                }
                Claimed::Released { retirement } => {
                    return Ok(json!({"status":"released","retirement":retirement,
                        "execution":"remote","worker":self.queue.compact_snapshot(),
                        "wait_hint":"Your station has been released. Leave your collaboration session if needed and stop calling worker.wait. No external process was automatically terminated."}));
                }
                Claimed::Waiting if !self.queue.accepting() => {
                    return Ok(
                        json!({"status":"not_accepting","execution":"remote", "worker":self.queue.compact_snapshot(),
                        "wait_hint":"The manager stopped this pool. Existing claimants must reuse their original request_id to acknowledge cancellation; no new tasks will be assigned."}),
                    );
                }
                Claimed::Waiting if wait_ms == 0 || tokio::time::Instant::now() >= deadline => {
                    return Ok(
                        json!({"status":"waiting","execution":"remote","worker":self.queue.compact_snapshot(),"wait_hint":remote::WAIT_HINT}),
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
        let result = match name {
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
                    json!({"manager":manager,"caller_is_manager":true,"worker":self.queue.compact_snapshot()}),
                )
            }
            "ssmcp.worker.events" => {
                let request: EventRequest = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker events request: {error}"))?;
                self.wait_events(&actor, &client_id, request).await
            }
            "ssmcp.worker.accept_handoff" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Accept {
                    handoff_id: String,
                }
                let request: Accept = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid coordinator handoff: {error}"))?;
                source.require_owner()?;
                self.remote()?;
                self.queue
                    .accept_handoff(&actor, &client_id, &request.handoff_id, now_ms())
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
                Ok(json!({"page":page,"worker":self.queue.compact_snapshot()}))
            }
            "ssmcp.worker.wait_available" => {
                let request: WaitAvailable = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker availability wait: {error}"))?;
                return self.wait_available(&actor, &client_id, request).await;
            }
            "ssmcp.worker.wait_for" => {
                let request: WaitRequest = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker condition wait: {error}"))?;
                return self.wait_for(&actor, &client_id, request).await;
            }
            "ssmcp.worker.resize" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Resize {
                    total: usize,
                }
                let request: Resize = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker resize: {error}"))?;
                source.require_owner()?;
                self.remote()?;
                self.queue
                    .resize_managed(&actor, &client_id, request.total)?;
                Ok(self.resize_result())
            }
            "ssmcp.worker.stop" => {
                empty_arguments(&arguments, "stop")?;
                let changed = self.queue.stop(&actor, &client_id, now_ms())?;
                Ok(
                    json!({"stop_requested":true,"changed":changed,"worker":self.queue.compact_snapshot(),
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
                    json!({"task":task,"warnings":prepared.warnings,"worker":self.queue.compact_snapshot(),
                    "execution":self.execution_name(),"wait_hint":"Keep task_id. For one result, block on worker.result; for several, use one worker.wait_for(any task_finished), then read ready results. Use a bounded wait_ms within your transport deadline."}),
                )
            }
            "ssmcp.worker.result" => {
                let request: Task = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid worker result request: {e}"))?;
                self.wait_result(&actor, &client_id, request).await
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
                    json!({"task":self.queue.cancel(&actor, &request.task_id, now_ms())?,"worker":self.queue.compact_snapshot()}),
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
                    "completion_source":"authenticated_remote_agent","worker":self.queue.compact_snapshot(),
                    "wait_hint":remote::WAIT_HINT}),
                )
            }
            "ssmcp.worker.report" => {
                let report: Report = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker activity report: {error}"))?;
                let (duplicate, cancel_requested, retire_requested) =
                    self.remote()?
                        .report(&self.queue, &actor, report, now_ms())?;
                Ok(json!({"status":"recorded","duplicate":duplicate,
                    "cancel_requested":cancel_requested,"retire_requested":retire_requested,
                    "worker":self.queue.compact_snapshot()}))
            }
            "ssmcp.worker.release" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Release {
                    retirement: Retirement,
                }
                let request: Release = serde_json::from_value(arguments)
                    .map_err(|error| format!("invalid worker release: {error}"))?;
                let duplicate =
                    self.remote()?
                        .release(&self.queue, &actor, request.retirement, now_ms())?;
                Ok(
                    json!({"status":"released","duplicate":duplicate,"worker":self.queue.compact_snapshot(),
                    "wait_hint":"Your station is released. Leave your collaboration session if needed and stop waiting. No external process was automatically terminated."}),
                )
            }
            _ => Err("unknown worker tool".into()),
        }?;
        Ok(self.decorate_availability(&actor, &client_id, result))
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
mod command_logs_tests;
#[cfg(test)]
mod events_tests;
#[cfg(all(test, target_os = "linux"))]
mod handoff_tests;
#[cfg(test)]
mod noise_tests;
#[cfg(test)]
mod observation_tests;
#[cfg(test)]
mod saturated_tests;
#[cfg(test)]
mod scheduling_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const AVAILABILITY_CLIENT: &str =
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    pub(super) fn availability_pool(count: usize) -> (Arc<Pool>, Actor) {
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

    pub(super) fn availability_cursor(value: &Value) -> AvailabilityCursor {
        AvailabilityCursor {
            generation: value["generation"].as_str().unwrap().into(),
            revision: value["revision"].as_u64().unwrap(),
        }
    }

    pub(super) fn submit_observed(
        pool: &Pool,
        owner: &Actor,
        key: &str,
    ) -> super::super::queue::TaskView {
        pool.queue
            .submit_managed(
                owner.clone(),
                AVAILABILITY_CLIENT,
                key,
                "real queued task",
                TaskContext {
                    #[cfg(target_os = "linux")]
                    source: Source::default(),
                    #[cfg(target_os = "linux")]
                    actor_key: actor_context_key(owner).unwrap(),
                    prepared: "prepared task".into(),
                },
                2,
            )
            .unwrap()
    }

    pub(super) fn condition_request(
        pool: &Pool,
        mode: &str,
        task_ids: &[&str],
        wait_ms: u64,
    ) -> WaitRequest {
        serde_json::from_value(json!({
            "generation":pool.generation,"mode":mode,"wait_ms":wait_ms,
            "conditions":task_ids.iter().map(|task_id| json!({"type":"task_finished","task_id":task_id})).collect::<Vec<_>>()
        })).unwrap()
    }

    #[tokio::test]
    async fn manager_any_and_all_waits_are_visible_until_their_own_real_completions() {
        let (pool, owner) = availability_pool(2);
        let a = Actor {
            participant: Some("executor-a".into()),
            ..owner.clone()
        };
        let b = Actor {
            participant: Some("executor-b".into()),
            ..owner.clone()
        };
        pool.wait_remote(&a, "register-a", 0).await.unwrap();
        pool.wait_remote(&b, "register-b", 0).await.unwrap();
        let first = submit_observed(&pool, &owner, "first");
        let second = submit_observed(&pool, &owner, "second");
        let claim_a = pool.wait_remote(&a, "claim-a", 0).await.unwrap();
        let claim_b = pool.wait_remote(&b, "claim-b", 0).await.unwrap();
        let ids = [first.task_id.as_str(), second.task_id.as_str()];
        let mut any = Box::pin(pool.wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "any", &ids, 5000),
        ));
        let mut all = Box::pin(pool.wait_for(
            &owner,
            AVAILABILITY_CLIENT,
            condition_request(&pool, "all", &ids, 5000),
        ));
        tokio::select! {
            biased;
            result = &mut any => panic!("Any returned before completion: {result:?}"),
            result = &mut all => panic!("All returned before completion: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        assert_eq!(
            pool.queue
                .snapshot()
                .manager_operations
                .iter()
                .filter(|row| row.state == "waiting")
                .count(),
            2
        );
        let completion = serde_json::from_value(
            json!({"claim":claim_a["claim"],"status":"completed","output":"A done"}),
        )
        .unwrap();
        pool.remote()
            .unwrap()
            .complete(&pool.queue, &a, completion, 3)
            .unwrap();
        let any = any.await.unwrap();
        assert_eq!(any["status"], "satisfied");
        assert_eq!(any["mode"], "any");
        assert_eq!(any["conditions"][0]["state"], "satisfied");
        assert_eq!(any["conditions"][1]["state"], "pending");
        tokio::select! {
            biased;
            result = &mut all => panic!("All accepted only one completion: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let completion = serde_json::from_value(
            json!({"claim":claim_b["claim"],"status":"completed","output":"B done"}),
        )
        .unwrap();
        pool.remote()
            .unwrap()
            .complete(&pool.queue, &b, completion, 4)
            .unwrap();
        let all = all.await.unwrap();
        assert_eq!(all["status"], "satisfied");
        assert_ne!(all["operation_id"], any["operation_id"]);
        assert!(
            pool.queue
                .snapshot()
                .manager_operations
                .iter()
                .all(|row| row.state != "waiting")
        );
        pool.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_wait_drop_timeout_and_legacy_observers_do_not_release_work_or_publish_capacity()
     {
        let (pool, owner) = availability_pool(1);
        let executor = Actor {
            participant: Some("executor".into()),
            ..owner.clone()
        };
        pool.wait_remote(&executor, "register", 0).await.unwrap();
        let task = submit_observed(&pool, &owner, "pending");
        pool.wait_remote(&executor, "claim", 0).await.unwrap();
        let capacity = pool.availability_value(
            pool.queue
                .availability(&owner, AVAILABILITY_CLIENT)
                .unwrap(),
        );
        let mut result = Box::pin(pool.wait_result(
            &owner,
            AVAILABILITY_CLIENT,
            Task {
                task_id: task.task_id.clone(),
                wait_ms: 5000,
            },
        ));
        tokio::select! {
            biased;
            result = &mut result => panic!("pending task result returned early: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let result_id = pool
            .queue
            .snapshot()
            .manager_operations
            .iter()
            .find(|row| row.state == "waiting")
            .unwrap()
            .operation_id;
        drop(result);
        let mut change = Box::pin(pool.wait_available(
            &owner,
            AVAILABILITY_CLIENT,
            WaitAvailable {
                after: Some(availability_cursor(&capacity)),
                wait_ms: 5000,
            },
        ));
        tokio::select! {
            biased;
            result = &mut change => panic!("display registration changed capacity: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        drop(change);
        let timed_out = pool
            .wait_for(
                &owner,
                AVAILABILITY_CLIENT,
                condition_request(&pool, "all", &[&task.task_id], 0),
            )
            .await
            .unwrap();
        assert_eq!(timed_out["status"], "timed_out");
        let snapshot = pool.queue.snapshot();
        assert_eq!(snapshot.working, 1);
        assert!(
            snapshot
                .manager_operations
                .iter()
                .all(|row| row.state != "waiting")
        );
        assert_eq!(
            snapshot
                .manager_operations
                .iter()
                .find(|row| row.operation_id == result_id)
                .unwrap()
                .state,
            "cancelled"
        );
        assert_eq!(
            pool.availability_value(
                pool.queue
                    .availability(&owner, AVAILABILITY_CLIENT)
                    .unwrap()
            ),
            capacity
        );
        let response = json!({"worker":snapshot});
        assert!(
            pool.decorate_availability(&executor, AVAILABILITY_CLIENT, response.clone())["worker"]
                .get("manager_operations")
                .is_none()
        );
        assert!(pool.decorate_availability(&owner, AVAILABILITY_CLIENT, response)["worker"]["manager_operations"].as_array().unwrap().len() >= 5);
        pool.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn manager_wait_observes_registered_idle_release_without_log_or_duplicate_notifications()
    {
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
        assert_eq!(initial["availability"]["idle_slots"], json!([]));
        assert_eq!(initial["availability"]["offline"], 2);
        assert_eq!(initial["availability"]["registered_workers"], 0);
        let executor = Actor {
            participant: Some("executor".into()),
            ..owner.clone()
        };
        assert_eq!(
            pool.wait_remote(&executor, "standby", 0).await.unwrap()["status"],
            "waiting"
        );
        let registered = pool
            .wait_available(
                &owner,
                AVAILABILITY_CLIENT,
                WaitAvailable {
                    after: Some(availability_cursor(&initial["availability"])),
                    wait_ms: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(registered["status"], "changed");
        assert_eq!(registered["availability"]["idle"], 1);
        assert_eq!(registered["availability"]["offline"], 1);
        assert_eq!(registered["availability"]["registered_workers"], 1);
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
        assert_eq!(busy["idle"], 0);
        assert_eq!(busy["offline"], 1);
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
        let progress: Report = serde_json::from_value(json!({
            "claim":assigned["claim"], "request_id":"observed-progress",
            "event":"progress", "progress":"Checked the parser",
        }))
        .unwrap();
        pool.remote()
            .unwrap()
            .report(&pool.queue, &executor, progress, 3)
            .unwrap();
        tokio::select! {
            biased;
            result = &mut pending => panic!("progress was reported as capacity change: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let completion: Completion = serde_json::from_value(json!({
            "claim":assigned["claim"], "status":"completed", "output":"done",
        }))
        .unwrap();
        assert!(
            !pool
                .remote()
                .unwrap()
                .complete(&pool.queue, &executor, completion.clone(), 4)
                .unwrap()
                .1
        );
        let released = pending.await.unwrap();
        assert_eq!(released["status"], "changed");
        assert_eq!(released["changed"], true);
        assert_eq!(released["availability"]["working"], 0);
        assert_eq!(released["availability"]["idle"], 1);
        assert_eq!(released["availability"]["offline"], 1);
        assert_eq!(
            released["availability"]["idle_slots"],
            json!([{
                "slot":assigned["claim"]["slot"], "epoch":assigned["claim"]["epoch"],
            }])
        );
        assert!(
            pool.remote()
                .unwrap()
                .complete(&pool.queue, &executor, completion, 5)
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
    async fn manager_availability_stop_and_disconnect_withdraw_registered_idle_slots() {
        for close in [false, true] {
            let (pool, owner) = availability_pool(1);
            let executor = Actor {
                participant: Some("executor".into()),
                ..owner.clone()
            };
            pool.wait_remote(&executor, "standby", 0).await.unwrap();
            let initial = pool.availability_value(
                pool.queue
                    .availability(&owner, AVAILABILITY_CLIENT)
                    .unwrap(),
            );
            assert_eq!(initial["idle"], 1);
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
            if close {
                pool.queue.close(2);
            } else {
                assert!(pool.queue.stop(&owner, AVAILABILITY_CLIENT, 2).unwrap());
            }
            let stopped = pending.await.unwrap();
            assert_eq!(stopped["status"], "not_accepting");
            assert_eq!(stopped["changed"], true);
            assert_eq!(stopped["availability"]["accepting"], false);
            assert_eq!(stopped["availability"]["closed"], close);
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
        }
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
                    wait_ms: MAX_OBSERVATION_WAIT_MS + 1
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
                        wait_ms: 5000,
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
    async fn availability_decoration_preserves_the_result_and_requires_the_exact_manager() {
        let (pool, owner) = availability_pool(1);
        let result = json!({"task":{"task_id":"retained","output":"original result"},"warnings":["original warning"]});
        let observer = Actor {
            participant: Some("observer".into()),
            ..owner.clone()
        };
        assert_eq!(
            pool.decorate_availability(&observer, AVAILABILITY_CLIENT, result.clone()),
            result
        );
        assert_eq!(
            pool.decorate_availability(&owner, &"b".repeat(64), result.clone()),
            result
        );
        let mut decorated = pool.decorate_availability(&owner, AVAILABILITY_CLIENT, result.clone());
        assert_eq!(decorated["availability"]["offline"], 1);
        assert_eq!(decorated["availability"]["idle_slots"], json!([]));
        decorated.as_object_mut().unwrap().remove("availability");
        assert_eq!(decorated, result);
        pool.shutdown().await.unwrap();
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
        assert_eq!(pool.queue.snapshot().idle, 0);
        assert_eq!(pool.queue.snapshot().offline, 2);
        assert_eq!(pool.queue.snapshot().registered_workers, 0);
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
        assert_eq!(pool.queue.snapshot().idle, 1);
        assert_eq!(pool.queue.snapshot().offline, 1);
        assert_eq!(pool.queue.snapshot().registered_workers, 1);
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
        assert_eq!(pool.queue.snapshot().idle, 1);
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
        assert_eq!(names.len(), 17);
        for name in [
            "status",
            "attach",
            "accept_handoff",
            "events",
            "tasks",
            "stop",
            "submit",
            "result",
            "cancel",
            "wait",
            "claim",
            "complete",
            "resize",
            "release",
            "report",
            "wait_available",
            "wait_for",
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

    #[tokio::test]
    async fn resize_wakes_a_real_idle_wait_and_requires_its_release_receipt() {
        let pool = Pool::new(
            Config {
                count: 2,
                factor: 1.0,
            },
            &Agent::Remote,
            PathBuf::new(),
        )
        .unwrap();
        pool.start();
        let worker = Actor {
            provider: "fixture".into(),
            subject: "owner".into(),
            participant: Some("worker".into()),
            connection: 1,
        };
        let pending = pool.wait_remote(&worker, "waiting", 5000);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("idle wait returned before resize: {result:?}"),
            _ = tokio::task::yield_now() => {},
        }
        let empty_removed = pool.resize(1).unwrap();
        assert_eq!(empty_removed["resize_pending"], false);
        assert_eq!(empty_removed["worker"]["registered_workers"], 1);
        let requested = pool.resize(0).unwrap();
        assert_eq!(requested["resize_pending"], true);
        assert_eq!(requested["worker"]["retiring"], 1);
        let response = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response["status"], "retiring");
        let retirement: Retirement =
            serde_json::from_value(response["retirement"].clone()).unwrap();
        pool.remote()
            .unwrap()
            .release(&pool.queue, &worker, retirement, 10)
            .unwrap();
        assert_eq!(pool.queue.snapshot().total, 0);
        assert_eq!(pool.queue.snapshot().registered_workers, 0);
        assert_eq!(
            pool.wait_remote(&worker, "after-release", 0).await.unwrap()["status"],
            "released"
        );
        pool.shutdown().await.unwrap();
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
