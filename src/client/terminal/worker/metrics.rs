//! Read-only metrics for the local worker client, not its external agents.
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const INTERVAL: Duration = Duration::from_secs(1);
const MAX_SAMPLES: usize = 60;

#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    pub at: Instant,
    /// Fraction of aggregate machine CPU time, expressed as a percentage.
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub cpu_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    pid: u32,
    start_ticks: u64,
}

#[derive(Clone, Copy, Debug)]
struct Counters {
    identity: ProcessIdentity,
    process_ticks: u64,
    machine_ticks: u64,
}

#[derive(Default)]
struct Reading {
    counters: Option<Counters>,
    memory_bytes: Option<u64>,
}

#[derive(Default)]
pub(super) struct Sampler {
    pid: Option<u32>,
    identity: Option<ProcessIdentity>,
    previous: Option<Counters>,
    last_attempt: Option<Instant>,
    samples: VecDeque<Sample>,
}

impl Sampler {
    pub(super) fn sample(&mut self, pid: u32, now: Instant) {
        if !self.due(pid, now) {
            return;
        }
        self.record(now, read_process(pid));
    }

    fn due(&mut self, pid: u32, now: Instant) -> bool {
        if self.pid != Some(pid) {
            self.pid = Some(pid);
            self.identity = None;
            self.previous = None;
            self.last_attempt = None;
            self.samples.clear();
        }
        if self
            .last_attempt
            .is_some_and(|last| now.saturating_duration_since(last) < INTERVAL)
        {
            return false;
        }
        self.last_attempt = Some(now);
        true
    }

    fn record(&mut self, now: Instant, reading: Reading) {
        if let Some(counters) = reading.counters {
            if self
                .identity
                .is_some_and(|identity| identity != counters.identity)
            {
                self.previous = None;
                self.samples.clear();
            }
            self.identity = Some(counters.identity);
        }
        let cpu_percent = self.previous.zip(reading.counters).and_then(|(old, new)| {
            let process = new.process_ticks.checked_sub(old.process_ticks)?;
            let machine = new.machine_ticks.checked_sub(old.machine_ticks)?;
            (machine > 0).then(|| 100.0 * process as f64 / machine as f64)
        });
        self.previous = reading.counters;
        self.samples.push_back(Sample {
            at: now,
            cpu_percent,
            memory_bytes: reading.memory_bytes,
            cpu_pending: cpu_percent.is_none() && reading.counters.is_some(),
        });
        while self.samples.len() > MAX_SAMPLES {
            self.samples.pop_front();
        }
    }

    pub(super) fn latest(&self) -> Option<&Sample> {
        self.samples.back()
    }

    pub(super) fn samples(&self) -> &VecDeque<Sample> {
        &self.samples
    }
}

#[cfg(target_os = "linux")]
fn read_process(pid: u32) -> Reading {
    use std::{
        fs::File,
        io::{BufRead, BufReader, Read},
    };

    let counters = (|| {
        let mut stat = String::new();
        File::open(format!("/proc/{pid}/stat"))
            .ok()?
            .take(8192)
            .read_to_string(&mut stat)
            .ok()?;
        let (start_ticks, process_ticks) = process_counters(&stat)?;
        let mut machine = String::new();
        BufReader::new(File::open("/proc/stat").ok()?)
            .read_line(&mut machine)
            .ok()?;
        Some(Counters {
            identity: ProcessIdentity { pid, start_ticks },
            process_ticks,
            machine_ticks: machine_counters(&machine)?,
        })
    })();
    let memory_bytes = File::open(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|file| {
            BufReader::new(file)
                .lines()
                .take(128)
                .map_while(Result::ok)
                .find_map(|line| resident_memory(&line))
        });
    Reading {
        counters,
        memory_bytes,
    }
}

#[cfg(not(target_os = "linux"))]
fn read_process(_pid: u32) -> Reading {
    Reading::default()
}

#[cfg(any(target_os = "linux", test))]
fn process_counters(stat: &str) -> Option<(u64, u64)> {
    // comm may contain spaces and ')'; the remaining fields begin with state.
    let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let user: u64 = fields.get(11)?.parse().ok()?;
    let system: u64 = fields.get(12)?.parse().ok()?;
    let start: u64 = fields.get(19)?.parse().ok()?;
    Some((start, user.checked_add(system)?))
}

#[cfg(any(target_os = "linux", test))]
fn machine_counters(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    // guest and guest_nice are already included in user/nice, so exclude them.
    let values: Vec<_> = fields
        .take(8)
        .map(str::parse::<u64>)
        .collect::<Result<_, _>>()
        .ok()?;
    if values.len() < 4 {
        return None;
    }
    values.into_iter().try_fold(0_u64, u64::checked_add)
}

#[cfg(any(target_os = "linux", test))]
fn resident_memory(line: &str) -> Option<u64> {
    let mut fields = line.split_whitespace();
    if fields.next()? != "VmRSS:" {
        return None;
    }
    let kib: u64 = fields.next()?.parse().ok()?;
    (fields.next()? == "kB")
        .then(|| kib.checked_mul(1024))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(process_ticks: u64, machine_ticks: u64) -> Reading {
        Reading {
            counters: Some(Counters {
                identity: ProcessIdentity {
                    pid: 42,
                    start_ticks: 100,
                },
                process_ticks,
                machine_ticks,
            }),
            memory_bytes: Some(32 * 1024 * 1024),
        }
    }

    #[test]
    fn parses_process_names_with_spaces_and_parentheses_without_counting_guest_cpu_twice() {
        let stat = "42 (worker ) client) S 1 2 3 4 5 6 7 8 9 10 25 15 0 0 20 0 1 0 123456 8192 4";
        assert_eq!(process_counters(stat), Some((123456, 40)));
        assert_eq!(
            machine_counters("cpu 10 20 30 40 5 6 7 8 900 800"),
            Some(126)
        );
        assert_eq!(machine_counters("cpu0 10 20 30 40"), None);
        assert_eq!(resident_memory("VmRSS:\t1234 kB"), Some(1_263_616));
        assert_eq!(resident_memory("VmSize:\t9999 kB"), None);
        assert_eq!(resident_memory("VmRSS: 1234 bytes"), None);
    }

    #[test]
    fn first_cpu_sample_is_unknown_and_later_samples_measure_machine_share() {
        let mut sampler = Sampler::default();
        let now = Instant::now();
        sampler.record(now, reading(100, 10_000));
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
        assert!(sampler.latest().unwrap().cpu_pending);
        assert_eq!(
            sampler.latest().unwrap().memory_bytes,
            Some(32 * 1024 * 1024)
        );
        sampler.record(now + INTERVAL, reading(150, 15_000));
        assert_eq!(sampler.latest().unwrap().cpu_percent, Some(1.0));
        assert!(!sampler.latest().unwrap().cpu_pending);
        sampler.record(now + INTERVAL * 2, Reading::default());
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
        assert!(!sampler.latest().unwrap().cpu_pending);
        sampler.record(now + INTERVAL * 3, reading(170, 25_000));
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
    }

    #[test]
    fn polling_is_bounded_and_pid_reuse_discards_the_old_process_series() {
        let mut sampler = Sampler::default();
        let now = Instant::now();
        assert!(sampler.due(42, now));
        assert!(!sampler.due(42, now + Duration::from_millis(999)));
        assert!(sampler.due(42, now + INTERVAL));
        for step in 0..90 {
            sampler.record(
                now + INTERVAL * step,
                reading(u64::from(step), u64::from(step) * 100),
            );
        }
        assert_eq!(sampler.samples().len(), MAX_SAMPLES);
        sampler.record(now + INTERVAL * 90, Reading::default());
        let mut replacement = reading(500, 100_000);
        replacement.counters.as_mut().unwrap().identity.start_ticks += 1;
        sampler.record(now + INTERVAL * 91, replacement);
        assert_eq!(sampler.samples().len(), 1);
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
        assert!(sampler.due(43, now + INTERVAL * 92));
        assert!(sampler.samples().is_empty());
    }

    #[test]
    fn regressing_or_stalled_counters_are_unavailable_instead_of_zero_cpu() {
        let mut sampler = Sampler::default();
        let now = Instant::now();
        sampler.record(now, reading(100, 10_000));
        sampler.record(now + INTERVAL, reading(90, 11_000));
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
        sampler.record(now + INTERVAL * 2, reading(100, 11_000));
        assert!(sampler.latest().unwrap().cpu_percent.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_the_real_local_process_without_starting_another_program() {
        let reading = read_process(std::process::id());
        let counters = reading.counters.expect("local /proc process counters");
        assert_eq!(counters.identity.pid, std::process::id());
        assert!(counters.identity.start_ticks > 0);
        assert!(counters.machine_ticks > 0);
        assert!(reading.memory_bytes.is_some_and(|bytes| bytes > 0));
    }
}
