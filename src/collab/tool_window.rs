//! Per-membership observations admitted by the relay's public tool entries.
use std::{collections::VecDeque, time::Duration};
use tokio::time::Instant;

const WINDOW_MS: u64 = 600_000;
const OFFSET_MASK: u16 = 1023;
const UNIT: u16 = 1024;

enum Counts {
    // Ten low bits identify a millisecond, six high bits hold 1..=63 calls.
    Sparse(Vec<u16>),
    Dense(Box<[u64; 1000]>),
}

struct Second {
    index: u64,
    total: u64,
    counts: Counts,
}

impl Second {
    fn record(&mut self, offset: u16) {
        self.total += 1;
        if let Counts::Sparse(entries) = &mut self.counts {
            if let Some(entry) = entries.last_mut()
                && *entry & OFFSET_MASK == offset
            {
                if let Some(next) = entry.checked_add(UNIT) {
                    *entry = next;
                    return;
                }
                // More than 63 calls in this millisecond. Keep every count,
                // while retaining at most 1,000 counters for this second.
                let mut dense = Box::new([0; 1000]);
                for entry in entries.iter() {
                    dense[usize::from(*entry & OFFSET_MASK)] = u64::from(*entry / UNIT);
                }
                self.counts = Counts::Dense(dense);
            } else {
                entries.push(offset | UNIT);
                return;
            }
        }
        if let Counts::Dense(counts) = &mut self.counts {
            counts[usize::from(offset)] += 1;
        }
    }

    fn after(&self, offset: u16) -> u64 {
        match &self.counts {
            Counts::Sparse(entries) => {
                let first = entries.partition_point(|entry| *entry & OFFSET_MASK <= offset);
                entries[first..]
                    .iter()
                    .map(|entry| u64::from(*entry / UNIT))
                    .sum()
            }
            Counts::Dense(counts) => counts[usize::from(offset) + 1..].iter().sum(),
        }
    }
}

pub(super) struct ToolWindow {
    started: Instant,
    last_call: Option<Instant>,
    seconds: VecDeque<Second>,
}

impl ToolWindow {
    pub(super) fn new(started: Instant) -> Self {
        Self {
            started,
            last_call: None,
            seconds: VecDeque::new(),
        }
    }

    fn elapsed_ms(&self, now: Instant) -> u64 {
        u64::try_from(now.saturating_duration_since(self.started).as_millis()).unwrap_or(u64::MAX)
    }

    pub(super) fn record(&mut self, now: Instant) {
        let at = self.elapsed_ms(now);
        self.last_call = Some(now);
        let index = at / 1000;
        while self
            .seconds
            .front()
            .is_some_and(|s| index - s.index > WINDOW_MS / 1000)
        {
            self.seconds.pop_front();
        }
        if self.seconds.back().is_none_or(|s| s.index != index) {
            self.seconds.push_back(Second {
                index,
                total: 0,
                counts: Counts::Sparse(Vec::new()),
            });
        }
        self.seconds
            .back_mut()
            .expect("current second exists")
            .record((at % 1000) as u16);
    }

    pub(super) fn measure(&self, now: Instant) -> (u64, bool) {
        let lower = self.elapsed_ms(now).checked_sub(WINDOW_MS);
        let mut count = 0;
        for second in self.seconds.iter().rev() {
            if let Some(boundary) = lower
                && second.index <= boundary / 1000
            {
                if second.index == boundary / 1000 {
                    count += second.after((boundary % 1000) as u16);
                }
                break;
            }
            count += second.total;
        }
        let quiet = now.saturating_duration_since(self.last_call.unwrap_or(self.started));
        (
            count,
            count == 0 && quiet >= Duration::from_millis(WINDOW_MS),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn tool_window_requires_a_complete_quiet_interval_and_counts_each_dispatch() {
        let start = Instant::now();
        let mut window = ToolWindow::new(start);
        let ms = Duration::from_millis;
        assert_eq!(window.measure(start), (0, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS - 1)), (0, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS)), (0, true));
        for _ in 0..3 {
            window.record(start + ms(WINDOW_MS + 3));
        }
        window.record(start + ms(WINDOW_MS + 17));
        assert_eq!(window.measure(start + ms(WINDOW_MS + 18)), (4, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS * 2 + 2)), (4, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS * 2 + 3)), (1, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS * 2 + 17)), (0, true));
        window.record(start + ms(WINDOW_MS * 2 + 20));
        assert_eq!(window.measure(start + ms(WINDOW_MS * 2 + 20)), (1, false));
        assert!(window.seconds.len() <= 2);
    }

    #[test]
    fn dense_second_preserves_every_call_across_the_rolling_boundary() {
        let start = Instant::now();
        let ms = Duration::from_millis;
        let mut window = ToolWindow::new(start);
        window.record(start + ms(2));
        for _ in 0..700 {
            window.record(start + ms(40));
        }
        window.record(start + ms(999));
        window.record(start + ms(1000));
        assert_eq!(window.measure(start + ms(WINDOW_MS + 2)), (702, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS + 40)), (2, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS + 999)), (1, false));
        assert_eq!(window.measure(start + ms(WINDOW_MS + 1000)), (0, true));
    }

    #[test]
    fn tool_window_never_marks_offline_early_at_submillisecond_boundaries() {
        let start = Instant::now();
        let mut window = ToolWindow::new(start);
        let last = start + std::time::Duration::from_micros(900);
        window.record(last);
        let elapsed = std::time::Duration::from_millis(WINDOW_MS);
        assert!(!window.measure(start + elapsed).1);
        assert_eq!(window.measure(last + elapsed), (0, true));
    }
}
