//! Short, process-local host-input handoffs. Remote tool arguments never grant one.
use serde_json::{Value, json};
use std::{future::Future, sync::Mutex, time::Duration};
use tokio::{sync::Notify, time::Instant};

pub(crate) const DEFAULT_SECONDS: u64 = 15;
pub(crate) const MAX_SECONDS: u64 = 60;
pub(super) const LOCKED: &str = "host input is locked; use an isolated desktop or background browser control. The operator can locally run /computer host [seconds] in the ssmcp terminal for a short handoff";
pub(super) const ENDED: &str = "host input handoff ended; control returned to the operator";

#[derive(Clone, Copy)]
pub(super) struct Ticket {
    generation: u64,
    expires: Instant,
}

#[derive(Default)]
struct State {
    generation: u64,
    expires: Option<Instant>,
}

#[derive(Default)]
pub(super) struct HostControl {
    state: Mutex<State>,
    changed: Notify,
}

pub(super) fn input_tool(name: &str) -> bool {
    matches!(
        name,
        "ssmcp.computer.focus"
            | "ssmcp.computer.move"
            | "ssmcp.computer.click"
            | "ssmcp.computer.drag"
            | "ssmcp.computer.key"
            | "ssmcp.computer.type"
            | "ssmcp.computer.scroll"
    )
}

impl HostControl {
    /// Only the local interactive command path may call this method.
    pub(super) fn handoff_local(&self, seconds: u64) -> Result<(), String> {
        if !(1..=MAX_SECONDS).contains(&seconds) {
            return Err(format!(
                "host handoff must be between 1 and {MAX_SECONDS} seconds"
            ));
        }
        self.handoff_until(Instant::now() + Duration::from_secs(seconds));
        Ok(())
    }

    fn handoff_until(&self, expires: Instant) {
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.generation = state.generation.wrapping_add(1);
            state.expires = Some(expires);
        }
        // A fresh local handoff invalidates queued and running calls from the old one.
        self.changed.notify_waiters();
    }

    pub(super) fn revoke(&self) -> bool {
        let active = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let active = state
                .expires
                .is_some_and(|expires| expires > Instant::now());
            if state.expires.take().is_none() {
                return false;
            }
            state.generation = state.generation.wrapping_add(1);
            active
        };
        self.changed.notify_waiters();
        active
    }

    pub(super) fn ticket(&self) -> Result<Ticket, String> {
        self.ticket_at(Instant::now())
    }

    fn ticket_at(&self, now: Instant) -> Result<Ticket, String> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.expires {
            Some(expires) if expires > now => Ok(Ticket {
                generation: state.generation,
                expires,
            }),
            _ => Err(LOCKED.into()),
        }
    }

    fn valid(&self, ticket: Ticket) -> bool {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.generation == ticket.generation
            && state.expires == Some(ticket.expires)
            && Instant::now() < ticket.expires
    }

    pub(super) async fn cancelled(&self, ticket: Ticket) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            // Register before inspecting state so revoke cannot be lost between check and await.
            changed.as_mut().enable();
            if !self.valid(ticket) {
                return;
            }
            tokio::select! {
                _ = changed => {},
                _ = tokio::time::sleep_until(ticket.expires) => return,
            }
        }
    }

    pub(super) async fn run<T>(
        &self,
        ticket: Ticket,
        operation: impl Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        tokio::select! {
            biased;
            _ = self.cancelled(ticket) => Err(ENDED.into()),
            result = operation => result,
        }
    }

    pub(super) fn status(&self) -> Value {
        let now = Instant::now();
        let remaining = self
            .ticket_at(now)
            .ok()
            .map(|ticket| ticket.expires.saturating_duration_since(now));
        json!({
            "enabled": remaining.is_some(),
            "remaining_seconds": remaining.map(|duration|
                duration.as_secs() + u64::from(duration.subsec_nanos() != 0)
            ).unwrap_or(0),
            "default_seconds": DEFAULT_SECONDS,
            "max_seconds": MAX_SECONDS,
            "local_handoff": "/computer host [seconds]",
            "local_stop": "/computer stop",
            "revoked_by": "input in the ssmcp terminal, expiry, or client shutdown",
            "all_application_input_detection": false,
            "serialization_scope": "this client process",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn host_input_is_locked_by_default_and_new_connections_do_not_inherit_handoffs() {
        let control = HostControl::default();
        assert!(control.ticket().is_err());
        assert_eq!(control.status()["enabled"], false);
        control.handoff_local(DEFAULT_SECONDS).unwrap();
        assert!(control.ticket().is_ok());
        assert!(HostControl::default().ticket().is_err());
    }

    #[test]
    fn local_handoff_is_bounded_and_reading_does_not_renew_it() {
        let control = HostControl::default();
        assert!(control.handoff_local(0).is_err());
        assert!(control.handoff_local(MAX_SECONDS + 1).is_err());
        let expires = Instant::now() + Duration::from_secs(20);
        control.handoff_until(expires);
        let a = control.ticket().unwrap();
        let b = control.ticket().unwrap();
        assert_eq!(a.expires, expires);
        assert_eq!(b.expires, expires);
        assert!(control.ticket_at(expires).is_err());
        assert!(control.ticket_at(expires + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn local_retake_and_fresh_handoff_invalidate_prior_queued_calls() {
        let control = HostControl::default();
        control.handoff_local(15).unwrap();
        let old = control.ticket().unwrap();
        assert!(control.revoke());
        assert!(!control.valid(old));
        assert!(!control.revoke());
        control.handoff_local(15).unwrap();
        let next = control.ticket().unwrap();
        assert!(!control.valid(old));
        assert!(control.valid(next));
        control.handoff_local(15).unwrap();
        assert!(!control.valid(next));
    }

    #[test]
    fn every_pointer_keyboard_and_focus_operation_requires_handoff() {
        for name in ["focus", "move", "click", "drag", "key", "type", "scroll"] {
            assert!(input_tool(&format!("ssmcp.computer.{name}")));
        }
        for name in [
            "status",
            "monitors",
            "windows",
            "screenshot",
            "desktop_start",
            "desktop_stop",
            "app_launch",
        ] {
            assert!(!input_tool(&format!("ssmcp.computer.{name}")));
        }
    }

    #[tokio::test]
    async fn revoked_queued_operation_does_not_start() {
        let control = HostControl::default();
        control.handoff_local(15).unwrap();
        let ticket = control.ticket().unwrap();
        control.revoke();
        let started = AtomicBool::new(false);
        let result = control
            .run(ticket, async {
                started.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert_eq!(result.unwrap_err(), ENDED);
        assert!(!started.load(Ordering::SeqCst));
    }

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn terminal_retake_cancels_running_operation_and_drops_its_input_guard() {
        let control = Arc::new(HostControl::default());
        control.handoff_local(15).unwrap();
        let ticket = control.ticket().unwrap();
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = dropped.clone();
        let worker_control = control.clone();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let worker = tokio::spawn(async move {
            worker_control
                .run(ticket, async move {
                    let _guard = Dropped(flag);
                    let _ = started.send(());
                    std::future::pending::<Result<(), String>>().await
                })
                .await
        });
        started_rx.await.unwrap();
        control.revoke();
        let result = tokio::time::timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err(), ENDED);
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn expired_handoff_cancels_pending_operation_without_a_new_input_event() {
        let control = HostControl::default();
        control.handoff_until(Instant::now() + Duration::from_millis(20));
        let ticket = control.ticket().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            control.run(ticket, std::future::pending::<Result<(), String>>()),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err(), ENDED);
        assert_eq!(control.status()["enabled"], false);
    }
}
