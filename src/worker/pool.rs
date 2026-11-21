use super::{
    Config, executor, now_ms,
    queue::{Actor, Queue, TaskStatus},
    registry::Registry,
};
use crate::{
    client::chat_io::{Agent, process::Program},
    runtime::{Runtime, call_scope::Source},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
pub(crate) struct TaskContext {
    pub(crate) source: Source,
    pub(crate) actor_key: String,
    pub(crate) prepared: String,
}

pub(crate) struct Pool {
    pub(crate) generation: String,
    pub(crate) config: Config,
    pub(crate) queue: Queue<TaskContext>,
    pub(crate) resources: Mutex<Option<Arc<Registry>>>,
    pub(crate) cleanup_error: Mutex<Option<String>>,
    program: Result<Program, String>,
    cwd: PathBuf,
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

impl Pool {
    pub(crate) fn new(config: Config, agent: &Agent, cwd: PathBuf) -> Result<Arc<Self>, String> {
        let generation = super::registry::nonce()?;
        let pool = Arc::new(Self {
            queue: Queue::new(generation.clone(), config.count)?,
            generation,
            config,
            resources: Mutex::new(None),
            cleanup_error: Mutex::new(None),
            program: Program::for_worker(agent),
            cwd,
            supervisor: Mutex::new(None),
        });
        if let Err(error) = &pool.program {
            pool.unavailable(error);
        }
        super::view()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pool = Some(Arc::downgrade(&pool));
        Ok(pool)
    }

    pub(crate) fn start(self: &Arc<Self>) {
        let mut supervisor = self
            .supervisor
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if supervisor.is_some() || self.queue.snapshot().closed {
            return;
        }
        match &self.program {
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

    pub(crate) fn unavailable(&self, reason: &str) {
        for slot in self.queue.snapshot().slots {
            let _ = self.queue.offline(slot.slot, slot.epoch, reason);
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.queue.close(now_ms());
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
        vec![
            tool(
                "status",
                "Read the worker monitor's actual idle/working/offline/total counts and the current OS user's registered resource limits. Configured slots are offline until a limited executor completes its handshake.",
                json!({}),
                json!([]),
                true,
            ),
            tool(
                "submit",
                "Submit one task to this worker session. Requires a verified collab-bound caller. A bounded FIFO assigns exactly one idle slot; busy workers never fan out a task. request_id deduplicates identical retries for the last 128 terminal tasks in this connection; reconnect is not replay permission.",
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
                "Request cancellation of your own queued or running task. A running slot stays busy until its owned execution cleanup is confirmed. Prior effects may remain; inspect the result before retrying.",
                json!({"task_id":{"type":"string"}}),
                json!(["task_id"]),
                false,
            ),
        ]
    }

    pub(crate) async fn perform(
        &self,
        name: &str,
        arguments: Value,
        runtime: &Runtime,
    ) -> Result<Value, String> {
        let source = Source::capture();
        let actor = actor(&source)?;
        match name {
            "ssmcp.worker.status" => {
                if arguments.as_object().is_none_or(|map| !map.is_empty()) {
                    return Err("worker status accepts an empty object".into());
                }
                let registry = self
                    .resources
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .clone();
                let resources = match registry {
                    Some(registry) => registry
                        .snapshot(&self.generation)
                        .await
                        .map_or_else(|error| json!({"error":error}), |value| value),
                    None => json!({"scope":"current_os_user","admitted":false}),
                };
                Ok(json!({"worker":self.queue.snapshot(),"resources":resources,
                    "cleanup_error":self.cleanup_error.lock().unwrap_or_else(|error| error.into_inner()).clone()}))
            }
            "ssmcp.worker.submit" => {
                let request: Submit = serde_json::from_value(arguments)
                    .map_err(|e| format!("invalid worker task: {e}"))?;
                source.require_owner()?;
                let prepared = runtime.prepare_references(&request.text).await?;
                if prepared.text.len() > 48 * 1024 {
                    return Err("expanded worker input exceeds 48 KiB; no task was admitted".into());
                }
                let actor_key = actor_context_key(&actor)?;
                let task = self.queue.submit(
                    actor,
                    &request.request_id,
                    &request.text,
                    TaskContext {
                        source,
                        actor_key,
                        prepared: prepared.text,
                    },
                    now_ms(),
                )?;
                Ok(
                    json!({"task":task,"warnings":prepared.warnings,"worker":self.queue.snapshot(),
                    "wait_hint":"Use worker.result for this task. Worker supervisors automatically re-enter their local blocking wait after confirmed cleanup."}),
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
                        return Ok(json!({"task":task,"worker":self.queue.snapshot()}));
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
            _ => Err("unknown worker tool".into()),
        }
    }
}

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
}
