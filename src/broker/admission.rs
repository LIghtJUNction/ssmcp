//! Count live transports from admission through teardown, including handshakes.
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub(super) struct Admission {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    draining: bool,
    transports: usize,
}

pub(crate) struct Transport(Arc<Admission>);

impl Admission {
    pub(super) fn admit(self: &Arc<Self>) -> Option<Transport> {
        let mut state = self.state.lock().expect("admission state poisoned");
        if state.draining {
            return None;
        }
        state.transports += 1;
        Some(Transport(self.clone()))
    }

    pub(super) fn set_draining(&self, draining: bool) -> usize {
        let mut state = self.state.lock().expect("admission state poisoned");
        state.draining = draining;
        state.transports
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .expect("admission state poisoned")
            .transports -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_counts_preexisting_transports_and_rejects_new_ones_until_reopened() {
        let admission = Arc::new(Admission::default());
        let open = admission.admit().expect("first transport");
        let handshaking = admission.admit().expect("second transport");
        assert_eq!(admission.set_draining(true), 2);
        assert!(admission.admit().is_none());
        drop(handshaking);
        assert_eq!(admission.set_draining(true), 1);
        drop(open);
        assert_eq!(admission.set_draining(true), 0);
        assert!(admission.admit().is_none());
        assert_eq!(admission.set_draining(false), 0);
        assert!(admission.admit().is_some());
    }
}
