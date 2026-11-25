//! Passive task-failure prompts. Each notice belongs to the launching native
//! connection and verified agent, even when a different agent reads the job.
use super::{Clock, Owner, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;

const CAPACITY: usize = 128;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum Notice {
    Memory {
        job_id: String,
        memory_limit_bytes: u64,
        memory_peak_bytes: Option<u64>,
    },
    Crash {
        job_id: String,
        exit_signal: i32,
        signal_name: String,
    },
}

impl Notice {
    pub(crate) fn from_snapshot(snapshot: &Value) -> Option<Self> {
        // Do not infer signal termination from an exit code. A shell can exit
        // normally with 128 + signal, or report an earlier child's crash.
        if snapshot["memory_limit_exceeded"] == true {
            return Some(Self::Memory {
                job_id: snapshot["job_id"].as_str()?.into(),
                memory_limit_bytes: snapshot["memory_limit_bytes"].as_u64()?,
                memory_peak_bytes: snapshot["memory_peak_bytes"].as_u64(),
            });
        }
        #[cfg(unix)]
        if snapshot["cancelled"] != true && snapshot["timed_out"] != true {
            let signal = i32::try_from(snapshot["exit_signal"].as_i64()?).ok()?;
            let name = match signal {
                libc::SIGSEGV => "SIGSEGV",
                libc::SIGILL => "SIGILL",
                libc::SIGBUS => "SIGBUS",
                libc::SIGABRT => "SIGABRT",
                libc::SIGFPE => "SIGFPE",
                _ => return None,
            };
            return Some(Self::Crash {
                job_id: snapshot["job_id"].as_str()?.into(),
                exit_signal: signal,
                signal_name: name.into(),
            });
        }
        None
    }

    pub(crate) fn job_id(&self) -> &str {
        match self {
            Self::Memory { job_id, .. } | Self::Crash { job_id, .. } => job_id,
        }
    }

    #[cfg(any(target_os = "linux", all(unix, test)))]
    pub(crate) fn event_type(&self) -> &'static str {
        match self {
            Self::Memory { .. } => "exec.memory_limit_exceeded",
            Self::Crash { .. } => "exec.crashed",
        }
    }

    pub(crate) fn page_key(&self) -> &'static str {
        match self {
            Self::Memory { .. } => "memory_limit_hooks",
            Self::Crash { .. } => "task_crash_hooks",
        }
    }

    pub(crate) fn valid(&self) -> bool {
        let job_id = self.job_id();
        !job_id.is_empty()
            && job_id.len() <= 96
            && job_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            && match self {
                Self::Memory {
                    memory_limit_bytes, ..
                } => *memory_limit_bytes > 0,
                Self::Crash {
                    exit_signal,
                    signal_name,
                    ..
                } => {
                    (1..=127).contains(exit_signal)
                        && matches!(
                            signal_name.as_str(),
                            "SIGSEGV" | "SIGILL" | "SIGBUS" | "SIGABRT" | "SIGFPE"
                        )
                }
            }
    }

    pub(crate) fn body(&self) -> String {
        match self {
            Self::Memory {
                job_id,
                memory_limit_bytes,
                ..
            } => format!(
                "SSMCP stopped task {job_id} after its process group hit its {memory_limit_bytes} byte memory limit. Before retrying, reduce the batch size or concurrency, or stream the input. Check the original job outcome before repeating work with side effects."
            ),
            Self::Crash {
                job_id,
                exit_signal,
                signal_name,
            } => format!(
                "Task {job_id} terminated with {signal_name} (signal {exit_signal}). This is the final signal in the task's wait status. Review the original stderr and any available crash diagnostics before assigning a cause or retrying work with side effects."
            ),
        }
    }
}

struct Entry {
    owner: Owner,
    notice: Notice,
    acknowledged: bool,
}

#[derive(Default)]
pub(super) struct Queue(VecDeque<Entry>);

impl Queue {
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    fn insert(&mut self, owner: Owner, notice: Notice) -> bool {
        if self
            .0
            .iter()
            .any(|entry| entry.owner == owner && entry.notice.job_id() == notice.job_id())
        {
            return false;
        }
        if self.0.len() == CAPACITY {
            let old = self
                .0
                .iter()
                .position(|entry| entry.acknowledged)
                .unwrap_or(0);
            self.0.remove(old);
        }
        self.0.push_back(Entry {
            owner,
            notice,
            acknowledged: false,
        });
        true
    }

    pub(super) fn read(&self, owner: &Owner, limit: usize) -> Vec<&Notice> {
        self.0
            .iter()
            .filter(|entry| !entry.acknowledged && entry.owner == *owner)
            .take(limit)
            .map(|entry| &entry.notice)
            .collect()
    }

    pub(super) fn ack(&mut self, owner: &Owner, job: &str) -> Result<Value, String> {
        let entry = self
            .0
            .iter_mut()
            .find(|entry| entry.owner == *owner && entry.notice.job_id() == job)
            .ok_or("unknown task-failure hook for this exact caller and connection")?;
        let duplicate = std::mem::replace(&mut entry.acknowledged, true);
        Ok(json!({"acknowledged":true, "duplicate":duplicate, "job_id":job}))
    }
}

impl Owner {
    pub(super) fn for_task_hook(source: &Source) -> Option<Self> {
        let call = source.call.as_ref()?;
        if let Some(participant) = call.local_task_participant() {
            return Some(Self {
                connection: Some(call.connection()),
                thread: Some(participant.clone()),
                attribution: None,
                history: None,
            });
        }
        call.thread().map(|_| Self::from_source(source))
    }
}

impl Clock {
    pub(crate) fn record_task_failure(&self, source: &Source, notice: Notice) {
        let Some(owner) = Owner::for_task_hook(source) else {
            return;
        };
        if !notice.valid() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || !state.memory_hooks.insert(owner, notice.clone()) {
            return;
        }
        drop(state);
        // The creating agent can still read and acknowledge its retained notice
        // when its session has no human-facing history (e.g. a worker pool).
        // Local OS participants share a human-facing history. Only authenticated
        // account threads have a per-participant history eligible for publication.
        if source.call.as_ref().is_some_and(|c| c.thread().is_some()) && source.owner().is_some() {
            let _ = crate::client::interface::publish_message(
                json!({"kind":"input","input_origin":"system_hook",
                    "idempotency_key": format!("task_failure_{}", notice.job_id()),
                    "text":notice.body()}),
                source.clone(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(connection: u64) -> Owner {
        Owner {
            connection: Some(connection),
            attribution: None,
            thread: None,
            history: None,
        }
    }

    #[tokio::test]
    async fn local_task_hooks_keep_participants_and_connections_separate_without_account_or_history()
     {
        use crate::runtime::call_scope::CallScope;
        use std::sync::Arc;

        let clock = Clock::new().unwrap();
        let scope = |generation, label| {
            let scope =
                CallScope::from_relay_for_client(generation, Some("local_fixture"), 1, None, None)
                    .unwrap();
            if let Some(ch) = label {
                scope
                    .bind_local_task_participant(format!(
                        "agent_{}",
                        char::to_string(&ch).repeat(32)
                    ))
                    .unwrap();
            }
            assert!(scope.thread().is_none() && scope.attribution().is_none());
            let _ = scope.clock.set(Arc::downgrade(&clock));
            scope
        };
        let creator = scope(1, Some('a'));
        let observer = scope(1, Some('b'));
        let reconnected = scope(2, Some('a'));
        let unbound = scope(1, None);
        let receipt =
            json!({"job_id":"local-oom","memory_limit_exceeded":true,"memory_limit_bytes":1048576});
        creator
            .scope(async {
                Source::capture().report_task_failure(&receipt);
            })
            .await;
        let read = || clock.prompt(json!({"action":"read"})).unwrap();
        let ack = || clock.prompt(json!({"action":"ack_task","job_id":"local-oom"}));
        for other in [&observer, &reconnected, &unbound] {
            assert!(
                other
                    .scope(async { read() })
                    .await
                    .get("memory_limit_hooks")
                    .is_none()
            );
            assert!(other.scope(async { ack() }).await.is_err());
        }
        let next_call = scope(1, Some('a'));
        assert_eq!(
            next_call.scope(async { read() }).await["memory_limit_hooks"][0]["job_id"],
            "local-oom"
        );
        assert_eq!(
            next_call.scope(async { ack() }).await.unwrap()["acknowledged"],
            true
        );
        assert!(
            creator
                .scope(async { read() })
                .await
                .get("memory_limit_hooks")
                .is_none()
        );
    }

    #[tokio::test]
    async fn only_the_bound_creator_receives_a_confirmed_kernel_kill() {
        use crate::runtime::call_scope::CallScope;
        use crate::wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution};
        use std::sync::Arc;

        let clock = Clock::new().unwrap();
        let scope = |letter: &str| {
            let agent = format!("agent_{}", letter.repeat(32));
            let attribution = ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: "123".into(),
                    login: "same".into(),
                },
                owner: None,
                agent: Some(ControlAgent {
                    id: agent.clone(),
                    name: "same".into(),
                }),
            };
            let context = CodeContext {
                run_id: "a".repeat(32),
                code: "return 1".into(),
                timestamp_ms: 1,
                session_id: Some("session_fixture".into()),
                thread_id: Some(agent),
            };
            let scope = CallScope::from_relay_for_client(
                7,
                Some("session_fixture"),
                1,
                Some(&attribution),
                Some(&context),
            )
            .unwrap();
            let _ = scope.clock.set(Arc::downgrade(&clock));
            scope
        };
        let first = scope("a");
        let other = scope("b");
        let event = json!({"job_id":"job-test", "exit_signal":9,"memory_limit_bytes":67108864});
        first
            .scope(async {
                Source::capture().report_task_failure(&event);
            })
            .await;
        let read = || clock.prompt(json!({"action":"read"}));
        assert!(
            first
                .scope(async { read() })
                .await
                .unwrap()
                .get("memory_limit_hooks")
                .is_none()
        );
        let mut killed = event;
        killed["memory_limit_exceeded"] = json!(true);
        first
            .scope(async {
                Source::capture().report_task_failure(&killed);
            })
            .await;
        assert_eq!(
            first.scope(async { read() }).await.unwrap()["memory_limit_hooks"][0]["job_id"],
            "job-test"
        );
        assert!(
            other
                .scope(async { read() })
                .await
                .unwrap()
                .get("memory_limit_hooks")
                .is_none()
        );
        let ack = || clock.prompt(json!({"action":"ack_memory","job_id":"job-test"}));
        assert!(other.scope(async { ack() }).await.is_err());
        assert_eq!(
            first.scope(async { ack() }).await.unwrap()["acknowledged"],
            true
        );
        assert!(
            first
                .scope(async { read() })
                .await
                .unwrap()
                .get("memory_limit_hooks")
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn crashes_require_a_fatal_wait_signal_and_preserve_the_final_signal() {
        for (signal, name) in [
            (libc::SIGSEGV, "SIGSEGV"),
            (libc::SIGILL, "SIGILL"),
            (libc::SIGBUS, "SIGBUS"),
            (libc::SIGABRT, "SIGABRT"),
            (libc::SIGFPE, "SIGFPE"),
        ] {
            let snapshot = json!({"job_id":"job","exit_signal":signal});
            let notice = Notice::from_snapshot(&snapshot).unwrap();
            assert!(notice.valid());
            assert_eq!(notice.event_type(), "exec.crashed");
            assert_eq!(json!(notice)["signal_name"], name);
            assert_eq!(json!(notice)["exit_signal"], signal);
            assert!(notice.body().contains(name));
        }
        for snapshot in [
            json!({"job_id":"job","exit_code":139}),
            json!({"job_id":"job","exit_code":132}),
            json!({"job_id":"job","exit_signal":libc::SIGKILL}),
            json!({"job_id":"job","exit_signal":libc::SIGILL,"cancelled":true}),
            json!({"job_id":"job","exit_signal":libc::SIGSEGV,"timed_out":true}),
        ] {
            assert!(Notice::from_snapshot(&snapshot).is_none());
        }
        let both = json!({"job_id":"job","memory_limit_exceeded":true,"memory_limit_bytes":4096,"exit_signal":libc::SIGABRT});
        assert_eq!(
            Notice::from_snapshot(&both).unwrap().event_type(),
            "exec.memory_limit_exceeded"
        );
    }

    #[test]
    fn memory_notices_are_owned_bounded_and_idempotent() {
        let mut queue = Queue::default();
        let notice = |i: usize| Notice::Memory {
            job_id: format!("job-{i}"),
            memory_limit_bytes: 65536,
            memory_peak_bytes: None,
        };
        assert!(queue.insert(owner(7), notice(0)));
        assert!(!queue.insert(owner(7), notice(0)));
        assert!(queue.read(&owner(8), 4).is_empty());
        assert!(queue.ack(&owner(8), "job-0").is_err());
        assert_eq!(queue.read(&owner(7), 4).len(), 1);
        assert_eq!(queue.ack(&owner(7), "job-0").unwrap()["duplicate"], false);
        assert_eq!(queue.ack(&owner(7), "job-0").unwrap()["duplicate"], true);
        assert!(queue.read(&owner(7), 4).is_empty());
        for i in 1..=CAPACITY + 1 {
            queue.insert(owner(7), notice(i));
        }
        assert_eq!(queue.0.len(), CAPACITY);
        assert_eq!(queue.read(&owner(7), 1)[0].job_id(), "job-2");
    }
}
