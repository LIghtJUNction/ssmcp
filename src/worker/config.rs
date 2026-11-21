//! Worker options have no effects until an authenticated connection is ready.

pub(crate) const MAX_WORKERS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Config {
    pub(crate) count: usize,
    pub(crate) factor: f64,
}

impl Config {
    pub(crate) fn parse(count: &str, factor: Option<&str>) -> Result<Self, String> {
        let count = count
            .parse::<usize>()
            .ok()
            .filter(|count| (1..=MAX_WORKERS).contains(count))
            .ok_or("--worker requires an integer from 1 through 128")?;
        let factor = factor
            .unwrap_or("1")
            .parse::<f64>()
            .ok()
            .filter(|factor| factor.is_finite() && *factor > 0.0)
            .ok_or("--worker-k requires a finite, positive number")?;
        Ok(Self { count, factor })
    }
}

/// The effective total allocation budget, after parent cgroup/container caps.
/// This is not the fluctuating MemAvailable reading, nor an assertion that
/// memory outside the pool is unused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Capacity {
    pub(crate) cpu_micros_per_second: u64,
    pub(crate) memory_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Allocation {
    pub(crate) cpu_micros_per_second: u64,
    pub(crate) memory_bytes: u64,
    pub(crate) registered_workers: usize,
}

impl Capacity {
    pub(crate) fn allocate(self, total: usize, factor: f64) -> Result<Allocation, String> {
        if total == 0 || !factor.is_finite() || factor <= 0.0 {
            return Err("worker allocation needs a positive registered worker count and factor".into());
        }
        let share = |capacity: u64| -> Result<u64, String> {
            let value = capacity as f64 / total as f64 * factor;
            if !value.is_finite() || value < 1.0 || value >= u64::MAX as f64 {
                return Err("worker allocation is outside the enforceable resource range".into());
            }
            Ok(value.floor() as u64)
        };
        let cpu_micros_per_second = share(self.cpu_micros_per_second)?;
        let memory_bytes = share(self.memory_bytes)?;
        // Do not round a tiny budget up and silently exceed the user's formula.
        if cpu_micros_per_second < 10_000 || memory_bytes < 16 * 1024 * 1024 {
            return Err("the registered worker count leaves less than 1% CPU or 16 MiB per worker; reduce --worker or choose an explicit larger --worker-k".into());
        }
        Ok(Allocation {
            cpu_micros_per_second,
            memory_bytes,
            registered_workers: total,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_defaults_and_factor_reject_nonfinite_or_nonpositive_values() {
        assert_eq!(Config::parse("10", None).unwrap(), Config { count: 10, factor: 1.0 });
        for count in ["0", "129", "-1", "1.5", ""] {
            assert!(Config::parse(count, None).is_err(), "{count}");
        }
        for factor in ["0", "-1", "NaN", "inf", "-inf", "1e999", ""] {
            assert!(Config::parse("10", Some(factor)).is_err(), "{factor}");
        }
    }

    #[test]
    fn worker_share_uses_all_registered_sessions_and_effective_capacity() {
        let capacity = Capacity {
            cpu_micros_per_second: 2_000_000,
            memory_bytes: 1024 * 1024 * 1024,
        };
        let one = capacity.allocate(2, 1.0).unwrap();
        let two_sessions = capacity.allocate(10, 1.0).unwrap();
        assert_eq!(one.cpu_micros_per_second, 1_000_000);
        assert_eq!(two_sessions.cpu_micros_per_second, 200_000);
        assert_eq!(two_sessions.memory_bytes, capacity.memory_bytes / 10);
        assert_eq!(two_sessions.registered_workers, 10);
        assert_eq!(capacity.allocate(10, 0.5).unwrap().cpu_micros_per_second, 100_000);
    }

    #[test]
    fn unrepresentable_worker_budgets_fail_instead_of_rounding_up() {
        let capacity = Capacity { cpu_micros_per_second: 1, memory_bytes: 1 };
        assert!(capacity.allocate(10, 1.0).is_err());
        assert!(capacity.allocate(0, 1.0).is_err());
        assert!(capacity.allocate(1, f64::MAX).is_err());
    }
}
