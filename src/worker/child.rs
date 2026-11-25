//! Private supervisor protocol. Each slot is a separate process/cgroup with its
//! own bounded context. The configured CLI runs only after the limits handshake;
//! between tasks this process blocks on its pipe, not a public human-input RPC.
use super::{config::Allocation, limits};
use crate::client::chat_io::process::Program;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const MAX_PACKET: usize = 256 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Init {
    pub(crate) generation: String,
    pub(crate) slot: usize,
    pub(crate) parent: u32,
    pub(crate) parent_start: u64,
    pub(crate) program: Program,
    pub(crate) allocation: Allocation,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Run {
        task_id: String,
        attempt: u64,
        actor: String,
        text: String,
        allocation: Allocation,
    },
    Cancel {
        task_id: String,
        attempt: u64,
    },
    Shutdown {},
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Reply {
    Ready {
        generation: String,
        slot: usize,
        pid: u32,
        start: u64,
        group: PathBuf,
    },
    Done {
        task_id: String,
        attempt: u64,
        status: String,
        output: Option<String>,
        error: Option<String>,
        cleanup_complete: bool,
    },
}

pub(crate) async fn read<R: AsyncBufRead + Unpin, T: serde::de::DeserializeOwned>(
    reader: &mut R,
) -> Result<T, String> {
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if available.is_empty() {
            return Err("worker pipe closed before a complete reply".into());
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if bytes.len() + count > MAX_PACKET {
            return Err("worker protocol packet is too large".into());
        }
        let complete = available[count - 1] == b'\n';
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if complete {
            return serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid worker protocol packet: {e}"));
        }
    }
}

pub(crate) async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() >= MAX_PACKET {
        return Err("worker protocol packet is too large".into());
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await.map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())
}

#[derive(Default)]
struct Context {
    actor: Option<String>,
    turns: VecDeque<(String, String)>,
}

impl Context {
    fn prompt(&mut self, actor: &str, text: &str, generation: &str, slot: usize) -> String {
        if self.actor.as_deref() != Some(actor) {
            // A reused slot must never reveal another authenticated sender's
            // conversation. Context is private to this slot and current actor.
            self.actor = Some(actor.into());
            self.turns.clear();
        }
        let previous = serde_json::to_string(&self.turns).unwrap_or_else(|_| "[]".into());
        format!(
            "You are the explicitly configured executor for SSMCP worker {generation}/{slot}. Complete the current task and return its result. SSMCP owns the idle blocking wait and automatically waits for the next task after your result; do not poll, invent a sleepp tool, or create other workers. This slot retains the following bounded conversation context as JSON data:\n{previous}\n\nCurrent task:\n{text}"
        )
    }

    fn remember(&mut self, text: String, output: String) {
        self.turns.push_back((text, output));
        while self.turns.len() > 8
            || serde_json::to_vec(&self.turns).map_or(usize::MAX, |bytes| bytes.len()) > 32 * 1024
        {
            self.turns.pop_front();
        }
    }
}

#[cfg(target_os = "linux")]
fn processes(root: &Path) -> Result<BTreeSet<(u32, u64)>, String> {
    let mut pending = vec![root.to_path_buf()];
    let mut seen = 0;
    let mut result = BTreeSet::new();
    while let Some(group) = pending.pop() {
        seen += 1;
        if seen > 256 {
            return Err("worker cgroup subtree exceeds its cleanup observation bound".into());
        }
        let members =
            std::fs::read_to_string(group.join("cgroup.procs")).map_err(|e| e.to_string())?;
        for pid in members.split_whitespace() {
            let pid = pid
                .parse::<u32>()
                .map_err(|_| "invalid worker cgroup process identity")?;
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(stat) => {
                    result.insert((pid, limits::process_start(&stat)?));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.to_string()),
            }
        }
        for entry in std::fs::read_dir(&group).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
async fn idle_checkpoint(group: &Path, baseline: &BTreeSet<(u32, u64)>) -> Result<(), String> {
    for _ in 0..100 {
        if processes(group)?.is_subset(baseline) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("worker task left processes in its scope; owned scope shutdown is required".into())
}

#[cfg(target_os = "linux")]
pub(crate) async fn run() -> Result<(), String> {
    let mut input = tokio::io::BufReader::new(tokio::io::stdin());
    let mut output = tokio::io::stdout();
    let init: Init = tokio::time::timeout(Duration::from_secs(15), read(&mut input))
        .await
        .map_err(|_| "worker initialization timed out")??;
    if init.generation.len() != 32
        || !init.generation.bytes().all(|b| b.is_ascii_hexdigit())
        || init.slot >= super::config::MAX_WORKERS
        || limits::identity(init.parent).ok() != Some(init.parent_start)
    {
        return Err("invalid worker supervisor identity".into());
    }
    let initial = limits::group_path(
        &std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?,
    )?;
    let group = if initial.file_name().is_some_and(|n| n == "ssmcp-supervisor") {
        initial
            .parent()
            .ok_or("missing delegated worker scope")?
            .to_owned()
    } else {
        initial
    };
    let expected = format!("ssmcp-worker-{}-{}.scope", init.generation, init.slot);
    if group.file_name().and_then(|name| name.to_str()) != Some(expected.as_str()) {
        return Err("private worker entry requires its own resource-limited scope".into());
    }
    // Check the inherited session budget before reporting a usable slot.
    crate::session_budget::enter().map_err(|e| e.to_string())?;
    let current = limits::group_path(
        &std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?,
    )?;
    if current != group && current != group.join("ssmcp-supervisor") {
        return Err("worker moved out of its admitted scope during budget verification".into());
    }
    limits::verify(&group, init.allocation)?;
    let baseline = processes(&group)?;
    let pid = std::process::id();
    write(
        &mut output,
        &Reply::Ready {
            generation: init.generation.clone(),
            slot: init.slot,
            pid,
            start: limits::identity(pid)?,
            group: group.clone(),
        },
    )
    .await?;
    let (sender, mut incoming) = tokio::sync::mpsc::channel(2);
    let (ended, mut end) = tokio::sync::watch::channel(false);
    let reader = tokio::spawn(async move {
        while let Ok(command) = read::<_, Command>(&mut input).await {
            let shutdown = matches!(command, Command::Shutdown {});
            if sender.send(command).await.is_err() || shutdown {
                break;
            }
        }
        ended.send_replace(true);
    });
    let mut context = Context::default();
    let outcome = async {
        while let Some(command) = incoming.recv().await {
            let Command::Run { task_id, attempt, actor, text, allocation } = command else {
                if matches!(command, Command::Shutdown {}) { break; }
                continue;
            };
            if task_id.len() > 128 || attempt == 0 || actor.len() != 64 || text.len() > 48 * 1024 || text.contains('\0') {
                return Err("invalid worker task packet".into());
            }
            limits::verify(&group, allocation)?;
            if *end.borrow() || limits::identity(init.parent).ok() != Some(init.parent_start) { break; }
            let prompt = context.prompt(&actor, &text, &init.generation, init.slot);
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            let running = init.program.run_cancellable(&prompt, Some(cancelled));
            tokio::pin!(running);
            let mut stopping = false;
            let result = loop {
                tokio::select! {
                    biased;
                    result = &mut running => break result,
                    changed = end.changed(), if !stopping => {
                        if changed.is_err() || *end.borrow() { stopping = true; cancel.send_replace(true); }
                    },
                    command = incoming.recv(), if !stopping => match command {
                        Some(Command::Cancel { task_id: id, attempt: number }) if id == task_id && number == attempt => { cancel.send_replace(true); },
                        Some(Command::Cancel { .. }) => {},
                        Some(Command::Shutdown {}) | None => { stopping = true; cancel.send_replace(true); },
                        Some(Command::Run { .. }) => { stopping = true; cancel.send_replace(true); },
                    },
                }
            };
            let cleanup = idle_checkpoint(&group, &baseline).await;
            let (status, answer, error) = match result {
                Ok(answer) => { context.remember(text, answer.clone()); ("completed", Some(answer), None) },
                Err(error) if *cancel.borrow() => ("cancelled", None, Some(error)),
                Err(error) => ("failed", None, Some(error)),
            };
            let clean = cleanup.is_ok();
            write(&mut output, &Reply::Done { task_id, attempt, status: status.into(), output: answer,
                error: cleanup.err().or(error), cleanup_complete: clean }).await?;
            if stopping || !clean { break; }
            // The next recv blocks locally until new input. The prompt's hook
            // is supplementary; executor idleness does not rely on model text.
        }
        Ok(())
    }.await;
    reader.abort();
    let _ = reader.await;
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_context_is_bounded_and_never_crosses_authenticated_actors() {
        let mut context = Context::default();
        context.prompt("a", "first", "generation", 0);
        context.remember("first".into(), "private result".into());
        assert!(
            context
                .prompt("a", "second", "generation", 0)
                .contains("private result")
        );
        assert!(
            !context
                .prompt("b", "other", "generation", 0)
                .contains("private result")
        );
        for _ in 0..20 {
            context.remember("x".repeat(4096), "y".repeat(4096));
        }
        assert!(context.turns.len() <= 6);
    }

    #[tokio::test]
    async fn private_protocol_rejects_extra_fields_and_oversize_without_dispatch() {
        let mut bytes =
            tokio::io::BufReader::new(&b"{\"op\":\"shutdown\",\"unlimited\":true}\n"[..]);
        assert!(read::<_, Command>(&mut bytes).await.is_err());

        let mut bytes = tokio::io::BufReader::new(
            &b"{\"op\":\"cancel\",\"task_id\":\"task\",\"attempt\":1,\"unlimited\":true}\n"[..],
        );
        assert!(read::<_, Command>(&mut bytes).await.is_err());

        let mut bytes = tokio::io::BufReader::new(&b"{\"op\":\"shutdown\"}\n"[..]);
        assert!(matches!(
            read::<_, Command>(&mut bytes).await.unwrap(),
            Command::Shutdown {}
        ));
        assert_eq!(
            serde_json::to_value(Command::Shutdown {}).unwrap(),
            serde_json::json!({"op": "shutdown"})
        );

        let data = vec![b'x'; MAX_PACKET + 1];
        let mut bytes = tokio::io::BufReader::new(data.as_slice());
        assert!(read::<_, Command>(&mut bytes).await.is_err());
    }
}
