//! Candidate broker lifecycle for a single confirmed running native process.
//! Before wiring this in, the broker must supply a verified key, spawn receipt
//! and the same per-member authorization checks as ordinary reply delivery.

use super::CallResult;
use std::collections::HashMap;
use tokio::sync::oneshot;

struct Running<K> {
    key: K,
    members: HashMap<u64, oneshot::Sender<CallResult>>,
}

impl<K: Eq> Running<K> {
    fn started(key: K) -> Self {
        Self {
            key,
            members: HashMap::new(),
        }
    }

    fn join(&mut self, key: &K, id: u64) -> Option<oneshot::Receiver<CallResult>> {
        if key != &self.key
            || self.members.len() >= crate::wire::MAX_INFLIGHT
            || self.members.contains_key(&id)
        {
            return None;
        }
        let (send, recv) = oneshot::channel();
        self.members.insert(id, send);
        Some(recv)
    }

    fn has_waiters(&self) -> bool {
        self.members.values().any(|reply| !reply.is_closed())
    }

    // Consuming Running closes admission before any result can be delivered.
    fn finish(
        self,
        result: CallResult,
        confirmed_successful_spawn: bool,
        mut accept: impl FnMut(u64) -> Result<(), String>,
    ) -> usize {
        let success = confirmed_successful_spawn
            && result.as_ref().is_ok_and(|value| value["isError"] != true);
        let mut accepted = 0usize;
        for (id, reply) in self.members {
            if reply.is_closed() {
                continue;
            }
            match accept(id) {
                Err(error) => {
                    let _ = reply.send(Err(error));
                }
                Ok(()) => {
                    if reply.send(result.clone()).is_ok() && success {
                        accepted += 1;
                    }
                }
            }
        }
        accepted.saturating_sub(1)
    }
}

#[test]
#[cfg(target_os = "linux")]
fn ten_pending_members_share_one_fixed_process_and_credit_nine() {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    // cat stays in flight while stdin is open. No caller-supplied argv or env.
    let mut process = Command::new("/bin/cat")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut group = Running::started(("lease-1", "fixed-parameters"));
    let mut requests = (0..10)
        .map(|id| group.join(&("lease-1", "fixed-parameters"), id).unwrap())
        .collect::<Vec<_>>();
    assert!(group.join(&("lease-2", "fixed-parameters"), 10).is_none());
    assert!(group.join(&("lease-1", "other-parameters"), 10).is_none());
    assert!(group.join(&("lease-1", "fixed-parameters"), 0).is_none());
    assert!(
        requests
            .iter_mut()
            .all(|r| matches!(r.try_recv(), Err(oneshot::error::TryRecvError::Empty)))
    );
    process.stdin.take().unwrap().write_all(b"fixed").unwrap();
    let output = process.wait_with_output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fixed");
    let result = serde_json::json!({"status":"fixed"});
    assert_eq!(group.finish(Ok(result.clone()), true, |_| Ok(())), 9);
    for mut request in requests {
        assert_eq!(request.try_recv().unwrap().unwrap(), result);
    }
}

#[test]
fn cancellation_revocation_and_failures_do_not_earn_credit() {
    for (result, confirmed, expected) in [
        (Ok(serde_json::json!({})), true, 1),
        (Ok(serde_json::json!({})), false, 0),
        (Ok(serde_json::json!({"isError":true})), true, 0),
        (Err("failed".into()), false, 0),
    ] {
        let mut group = Running::started(());
        drop(group.join(&(), 0).unwrap());
        assert!(!group.has_waiters());
        let mut first = group.join(&(), 1).unwrap();
        let mut second = group.join(&(), 2).unwrap();
        let mut revoked = group.join(&(), 3).unwrap();
        assert!(group.has_waiters());
        assert_eq!(
            group.finish(result.clone(), confirmed, |id| {
                assert_ne!(id, 0);
                if id == 3 {
                    Err("revoked".into())
                } else {
                    Ok(())
                }
            }),
            expected
        );
        assert_eq!(first.try_recv().unwrap(), result);
        assert_eq!(second.try_recv().unwrap(), result);
        assert_eq!(revoked.try_recv().unwrap(), Err("revoked".into()));
    }
}
