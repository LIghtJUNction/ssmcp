//! Native-only execution timing. Never reconstructed from provider event JSON.
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub(crate) struct ExecutionTiming {
    pub started: Instant,
    pub deadline: Option<Instant>,
}

impl ExecutionTiming {
    #[cfg(test)]
    pub(crate) fn new(started: Instant, timeout_ms: u64) -> Self {
        Self {
            started,
            deadline: (timeout_ms != 0).then(|| started + Duration::from_millis(timeout_ms)),
        }
    }

    pub(crate) fn elapsed_ms(self, now: Instant) -> u64 {
        super::millis(now.saturating_duration_since(self.started))
    }

    pub(crate) fn remaining_ms(self, now: Instant) -> Option<u64> {
        self.deadline
            .map(|deadline| super::remaining_millis(deadline, now))
    }

    pub(crate) fn timeout_ms(self) -> u64 {
        self.deadline.map_or(0, |deadline| {
            super::millis(deadline.saturating_duration_since(self.started))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_execution_timing_uses_one_instant_and_does_not_infer_completion() {
        let start = Instant::now();
        let timed = ExecutionTiming::new(start, 2500);
        assert_eq!(timed.elapsed_ms(start + Duration::from_millis(999)), 999);
        assert_eq!(
            timed.remaining_ms(start + Duration::from_millis(999)),
            Some(1501)
        );
        assert_eq!(timed.remaining_ms(start + Duration::from_secs(4)), Some(0));
        // Cleanup may still be running after the deadline: elapsed does not
        // freeze and this display value cannot claim the process exited.
        assert_eq!(timed.elapsed_ms(start + Duration::from_secs(4)), 4000);
        assert_eq!(timed.timeout_ms(), 2500);
        let untimed = ExecutionTiming::new(start, 0);
        assert_eq!(untimed.remaining_ms(start + Duration::from_secs(20)), None);
        assert_eq!(untimed.elapsed_ms(start + Duration::from_secs(20)), 20_000);
    }
}
