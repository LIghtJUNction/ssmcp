//! Linux owned-process execution. Only this module can turn a verified limited
//! child into an idle queue slot. A lost result never becomes an automatic retry.
use super::{
    Pool, TaskContext,
    child::{self, Command, Init, Reply},
    limits,
    queue::{Lease, Outcome},
    registry::{self, AdmissionError, Registry, Unit},
};
use crate::{
    backend::process::Managed,
    client::{chat_io::process::Program, interface::history},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{ChildStdin, ChildStdout},
    task::JoinSet,
};

struct Child {
    process: Managed,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    stderr: Option<tokio::task::JoinHandle<()>>,
    unit: Unit,
    stopped: bool,
}

impl Child {
    async fn start(
        pool: &Pool,
        registry: &Registry,
        unit: Unit,
        slot: usize,
        program: &Program,
        cwd: &Path,
    ) -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut command = tokio::process::Command::new(crate::program::executable("systemd-run")?);
        command
            .args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--unit={}", unit.name))
            .arg(format!("--description=SSMCP worker {}", pool.generation));
        for property in registry::properties(unit.allocation) {
            command.arg(format!("--property={property}"));
        }
        command
            .arg("--")
            .arg(executable)
            .arg("--ssmcp-worker-child")
            .current_dir(cwd)
            .env_remove("SSMCP_RELAY_TOKEN")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut process =
            Managed::spawn(&mut command).map_err(|e| format!("start limited worker: {e}"))?;
        let mut input = process.child.stdin.take().ok_or("worker input missing")?;
        let mut output =
            BufReader::new(process.child.stdout.take().ok_or("worker output missing")?);
        let mut stderr = process
            .child
            .stderr
            .take()
            .ok_or("worker error stream missing")?
            .take(8193);
        let errors = tokio::spawn(async move {
            let _ = stderr.read_to_end(&mut Vec::new()).await;
        });
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            child::write(
                &mut input,
                &Init {
                    generation: pool.generation.clone(),
                    slot,
                    parent: std::process::id(),
                    parent_start: limits::identity(std::process::id())?,
                    program: program.clone(),
                    allocation: unit.allocation,
                },
            )
            .await?;
            let reply: Reply = child::read(&mut output).await?;
            let Reply::Ready {
                generation,
                slot: actual,
                pid,
                start,
                group,
            } = reply
            else {
                return Err("worker did not return an initialization receipt".into());
            };
            if generation != pool.generation
                || actual != slot
                || limits::identity(pid).ok() != Some(start)
            {
                return Err("worker initialization identity mismatch".into());
            }
            let verified = registry.connected(&pool.generation, slot).await?;
            if verified.group.as_ref() != Some(&group) {
                return Err("worker initialized outside its admitted scope".into());
            }
            Ok(verified)
        })
        .await
        .map_err(|_| "worker initialization timed out".to_owned())
        .and_then(|value| value);
        match result {
            Ok(unit) => Ok(Self {
                process,
                input,
                output,
                stderr: Some(errors),
                unit,
                stopped: false,
            }),
            Err(error) => {
                let cleanup = registry::stop_owned(&pool.generation, &unit).await;
                process.shutdown().await;
                errors.abort();
                let _ = errors.await;
                Err(cleanup.err().map_or(error.clone(), |cleanup| {
                    format!("{error}; cleanup incomplete: {cleanup}")
                }))
            }
        }
    }

    async fn stop(&mut self, generation: &str) -> Result<(), String> {
        if self.stopped {
            return Ok(());
        }
        let _ = tokio::time::timeout(
            Duration::from_secs(1),
            child::write(&mut self.input, &Command::Shutdown {}),
        )
        .await;
        let stopped = registry::stop_owned(generation, &self.unit).await;
        self.process.shutdown().await;
        if let Some(stderr) = self.stderr.take() {
            stderr.abort();
            let _ = stderr.await;
        }
        self.stopped = stopped.is_ok();
        stopped
    }
}

pub(crate) async fn supervise(pool: Arc<Pool>, program: Program, cwd: PathBuf) {
    let registry = match Registry::shared() {
        Ok(registry) => Arc::new(registry),
        Err(error) => {
            pool.unavailable(&error);
            return;
        }
    };
    let capacity = match limits::detect() {
        Ok(capacity) => capacity,
        Err(error) => {
            pool.unavailable(&error);
            return;
        }
    };
    let mut wake = pool.queue.subscribe();
    *pool
        .resources
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(registry.clone());
    let units = loop {
        if !pool.queue.accepting() {
            if let Err(error) = registry.leave(&pool.generation).await {
                *pool
                    .cleanup_error
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(error);
            }
            return;
        }
        match registry
            .admit(&pool.generation, pool.config, capacity)
            .await
        {
            Ok(units) => break units,
            Err(AdmissionError::Deferred(error)) => {
                pool.unavailable(&error);
                wake.borrow_and_update();
                // No user executable has started. Deferred global admission
                // waits locally; it does not consume a native RPC/model slot.
                tokio::select! { _ = tokio::time::sleep(Duration::from_secs(2)) => {}, _ = wake.changed() => {} }
            }
            Err(AdmissionError::Failed(error)) => {
                if let Err(cleanup) = registry.leave(&pool.generation).await {
                    *pool
                        .cleanup_error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = Some(cleanup);
                }
                pool.unavailable(&error);
                return;
            }
        }
    };
    let mut children = Vec::new();
    let mut startup_error = None;
    for (slot, unit) in units.into_iter().enumerate() {
        if !pool.queue.accepting() {
            startup_error = Some("worker connection closed or stopped during startup".into());
            break;
        }
        match Child::start(&pool, &registry, unit, slot, &program, &cwd).await {
            Ok(child) => children.push(child),
            Err(error) => {
                startup_error = Some(error);
                break;
            }
        }
    }
    if let Some(error) = startup_error {
        for child in &mut children {
            let _ = child.stop(&pool.generation).await;
        }
        let cleanup = registry.leave(&pool.generation).await;
        if let Err(error) = &cleanup {
            *pool
                .cleanup_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error.clone());
        }
        pool.unavailable(&format!(
            "{error}{}",
            cleanup
                .err()
                .map_or(String::new(), |e| format!("; cleanup incomplete: {e}"))
        ));
        return;
    }
    let mut tasks = JoinSet::new();
    for (slot, child) in children.into_iter().enumerate() {
        let pool = pool.clone();
        let registry = registry.clone();
        tasks.spawn(async move {
            run_slot(pool, registry, slot, child).await;
        });
    }
    while tasks.join_next().await.is_some() {}
    pool.queue.close(super::now_ms());
    match registry.leave(&pool.generation).await {
        Err(error) => {
            *pool
                .cleanup_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error);
        }
        Ok(()) => {
            let _ = pool.queue.cleaned_closed_pool(super::now_ms());
        }
    }
}

async fn run_slot(pool: Arc<Pool>, registry: Arc<Registry>, slot: usize, mut child: Child) {
    let epoch = match pool.queue.online(slot) {
        Ok(epoch) => epoch,
        Err(_) => {
            let _ = child.stop(&pool.generation).await;
            return;
        }
    };
    let mut wake = pool.queue.subscribe();
    let mut unfinished = None;
    let reason = loop {
        if !pool.queue.accepting() {
            break "connection closed or stopped by manager".to_owned();
        }
        let lease = match pool.queue.claim(slot, epoch, super::now_ms()) {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                tokio::select! {
                    _ = wake.changed() => continue,
                    ended = child::read::<_, Reply>(&mut child.output) => break ended.err().unwrap_or_else(|| "unexpected idle worker reply".into()),
                }
            }
            Err(error) => break error,
        };
        let result = execute(&pool, &registry, &mut child, &lease, &mut wake).await;
        match result {
            Ok((outcome, reusable)) => {
                if let Err(error) =
                    pool.queue
                        .finish(&lease, outcome, true, reusable, super::now_ms())
                {
                    break error;
                }
                if !reusable {
                    break "worker stopped after task cleanup".into();
                }
            }
            Err(error) => {
                let cleanup = child.stop(&pool.generation).await;
                if cleanup.is_ok() {
                    let _ = pool.queue.finish(
                        &lease,
                        Outcome::Unknown(error.clone()),
                        true,
                        false,
                        super::now_ms(),
                    );
                } else {
                    *pool
                        .cleanup_error
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = cleanup.err();
                    unfinished = Some(lease);
                    // No false idle/offline transition: unknown live descendants
                    // keep this claimed slot busy until final pool cleanup.
                }
                break error;
            }
        }
    };
    if let Err(error) = child.stop(&pool.generation).await {
        *pool
            .cleanup_error
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(error);
    } else {
        if let Some(lease) = unfinished {
            let _ = pool.queue.finish(
                &lease,
                Outcome::Unknown(reason.clone()),
                true,
                false,
                super::now_ms(),
            );
        }
        let _ = pool.queue.offline(slot, epoch, &reason);
        if let Err(error) = registry.retire(&pool.generation, slot).await {
            *pool
                .cleanup_error
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(error);
        }
    }
}

async fn execute(
    pool: &Pool,
    registry: &Registry,
    child: &mut Child,
    lease: &Lease<TaskContext>,
    wake: &mut tokio::sync::watch::Receiver<u64>,
) -> Result<(Outcome, bool), String> {
    let unit = registry.begin(&pool.generation, lease.slot).await?;
    let owner = lease.context.source.require_owner()?.clone();
    let request = lease.task_id.clone();
    let text = lease.text.clone();
    let saved_owner = owner.clone();
    let saved_request = request.clone();
    let saved = tokio::task::spawn_blocking(move || {
        history::record_owned_request(&saved_owner, &saved_request, &text)
    })
    .await
    .map_err(|_| "worker request history writer stopped".to_owned())
    .and_then(|value| value);
    if let Err(error) = saved {
        registry.idle(&pool.generation, lease.slot).await?;
        return Ok((
            Outcome::Failed(format!("task was not dispatched: {error}")),
            pool.queue.accepting(),
        ));
    }
    if pool.queue.cancellation_requested(lease) {
        registry.idle(&pool.generation, lease.slot).await?;
        return Ok((Outcome::Cancelled, pool.queue.accepting()));
    }
    child::write(
        &mut child.input,
        &Command::Run {
            task_id: lease.task_id.clone(),
            attempt: lease.attempt,
            actor: lease.context.actor_key.clone(),
            text: lease.context.prepared.clone(),
            allocation: unit.allocation,
        },
    )
    .await?;
    let reply = child::read::<_, Reply>(&mut child.output);
    tokio::pin!(reply);
    let deadline = tokio::time::sleep(Duration::from_secs(135));
    tokio::pin!(deadline);
    let mut cancellation_sent = false;
    let result = loop {
        tokio::select! {
            biased;
            reply = &mut reply => break reply?,
            _ = &mut deadline => return Err("worker reply deadline elapsed after dispatch; effects may remain and must not be replayed automatically".into()),
            _ = wake.changed() => {
                if !cancellation_sent && pool.queue.cancellation_requested(lease) {
                    child::write(&mut child.input, &Command::Cancel { task_id: lease.task_id.clone(), attempt: lease.attempt }).await?;
                    cancellation_sent = true;
                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(10));
                }
            }
        }
    };
    let Reply::Done {
        task_id,
        attempt,
        status,
        output,
        error,
        cleanup_complete,
    } = result
    else {
        return Err("worker returned an initialization receipt during a task".into());
    };
    if task_id != lease.task_id || attempt != lease.attempt {
        return Err("worker completion lease mismatch".into());
    }
    if !cleanup_complete {
        return Err(error.unwrap_or_else(|| "worker could not confirm process cleanup".into()));
    }
    registry.idle(&pool.generation, lease.slot).await?;
    let outcome = match status.as_str() {
        "completed" => {
            let answer = output.ok_or("worker completion has no result")?;
            let retained = answer.clone();
            // A current UI selection never replaces the captured submit owner.
            let saved = tokio::task::spawn_blocking(move || {
                history::record_owned_program_reply(&owner, &request, &retained)
            })
            .await
            .map_err(|_| "original history writer stopped".to_owned())
            .and_then(|value| value);
            match saved {
                Ok(_) => Outcome::Completed(answer),
                Err(error) => Outcome::CompletedWithWarning(
                    answer,
                    format!(
                        "confirmed result retained; original history could not be saved: {error}"
                    ),
                ),
            }
        }
        "cancelled" => Outcome::Cancelled,
        "failed" => {
            Outcome::Failed(error.unwrap_or_else(|| "configured worker executor failed".into()))
        }
        _ => return Err("worker returned an unknown completion state".into()),
    };
    Ok((outcome, pool.queue.accepting()))
}
