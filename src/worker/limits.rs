//! Effective Linux capacity and exact cgroup readback. No limit is inferred from
//! a successful `systemd-run` exit alone, and MemAvailable is not the budget.
#[cfg(target_os = "linux")]
use super::config::{Allocation, Capacity};
use std::path::{Component, Path, PathBuf};

pub(crate) fn cpu_limit(text: &str) -> Result<Option<u64>, String> {
    let mut fields = text.split_whitespace();
    let quota = fields.next().ok_or("cpu.max is empty")?;
    let period = fields
        .next()
        .ok_or("cpu.max has no period")?
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("invalid cpu.max period")?;
    if fields.next().is_some() {
        return Err("invalid cpu.max fields".into());
    }
    if quota == "max" {
        return Ok(None);
    }
    let quota = quota
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("invalid cpu.max quota")?;
    u64::try_from((quota as u128 * 1_000_000) / period as u128)
        .map(Some)
        .map_err(|_| "cpu.max overflow".into())
}

pub(crate) fn memory_limit(text: &str) -> Result<Option<u64>, String> {
    if text.trim() == "max" {
        return Ok(None);
    }
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map(Some)
        .ok_or_else(|| "invalid memory.max".into())
}

pub(crate) fn group_path(text: &str) -> Result<PathBuf, String> {
    let relative = text
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("worker hard limits require a unified cgroup v2 hierarchy")?;
    let path = Path::new(relative);
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err("invalid unified cgroup path".into());
    }
    Ok(Path::new("/sys/fs/cgroup").join(path.strip_prefix("/").map_err(|e| e.to_string())?))
}

pub(crate) fn process_start(stat: &str) -> Result<u64, String> {
    // comm (field 2) may contain spaces and ')'; starttime is field 22.
    let rest = stat.rsplit_once(") ").ok_or("invalid process stat")?.1;
    rest.split_whitespace()
        .nth(19)
        .ok_or("process stat has no start time")?
        .parse()
        .map_err(|_| "invalid process start time".into())
}

#[cfg(target_os = "linux")]
pub(crate) fn identity(pid: u32) -> Result<u64, String> {
    process_start(&std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?)
}

#[cfg(target_os = "linux")]
pub(crate) fn verify(group: &Path, allocation: Allocation) -> Result<(), String> {
    let read = |name: &str| {
        std::fs::read_to_string(group.join(name))
            .map_err(|e| format!("worker {name} readback: {e}"))
    };
    let cpu = cpu_limit(&read("cpu.max")?)?.ok_or("worker CPU quota was not installed")?;
    let memory =
        memory_limit(&read("memory.max")?)?.ok_or("worker memory limit was not installed")?;
    // Kernel/page and systemd quota rounding must never silently raise a limit.
    if cpu > allocation.cpu_micros_per_second
        || cpu < 10_000
        || memory > allocation.memory_bytes
        || memory < 16 * 1024 * 1024
        || read("memory.swap.max")?.trim() != "0"
        || read("pids.max")?.trim() != "256"
    {
        return Err(
            "worker hard-limit readback did not match its admitted resource ceiling".into(),
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn detect() -> Result<Capacity, String> {
    let mut mask: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: the destination is a live cpu_set_t of the supplied size.
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&mask), &mut mask) } != 0 {
        return Err(format!(
            "read worker CPU affinity: {}",
            std::io::Error::last_os_error()
        ));
    }
    let cpus = unsafe { libc::CPU_COUNT(&mask) } as u64;
    if cpus == 0 {
        return Err("worker CPU affinity is empty".into());
    }
    let memory = std::fs::read_to_string("/proc/meminfo").map_err(|e| e.to_string())?;
    let mut capacity = Capacity {
        cpu_micros_per_second: cpus * 1_000_000,
        memory_bytes: memory
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .and_then(|line| line.split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
            .and_then(|n| n.checked_mul(1024))
            .ok_or("cannot read total physical memory")?,
    };
    let group =
        group_path(&std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?)?;
    for ancestor in group
        .ancestors()
        .take_while(|p| p.starts_with("/sys/fs/cgroup"))
    {
        for (name, cpu) in [("cpu.max", true), ("memory.max", false)] {
            let text = match std::fs::read_to_string(ancestor.join(name)) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(format!("read effective worker capacity {name}: {error}"));
                }
            };
            if cpu {
                if let Some(limit) = cpu_limit(&text)? {
                    capacity.cpu_micros_per_second = capacity.cpu_micros_per_second.min(limit);
                }
            } else if let Some(limit) = memory_limit(&text)? {
                capacity.memory_bytes = capacity.memory_bytes.min(limit);
            }
        }
    }
    Ok(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quota_parser_uses_period_and_rejects_unbounded_as_a_hard_limit() {
        assert_eq!(cpu_limit("25000 100000\n").unwrap(), Some(250_000));
        assert_eq!(cpu_limit("12500 50000").unwrap(), Some(250_000));
        assert_eq!(cpu_limit("max 100000").unwrap(), None);
        for text in ["", "25000", "1 0", "0 100000", "1 100 x"] {
            assert!(cpu_limit(text).is_err());
        }
        assert_eq!(memory_limit("max").unwrap(), None);
        assert_eq!(memory_limit("33554432").unwrap(), Some(33554432));
    }

    #[test]
    fn proc_identity_and_cgroup_paths_do_not_confuse_names_with_identity() {
        let tail = (3..=22)
            .map(|field| if field == 22 { "7654321" } else { "0" })
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            process_start(&format!("12 (a name ) with spaces) {tail}")).unwrap(),
            7654321
        );
        assert_eq!(
            group_path("0::/user.slice/worker.scope\n").unwrap(),
            Path::new("/sys/fs/cgroup/user.slice/worker.scope")
        );
        assert!(group_path("0::/../unowned").is_err());
        assert!(group_path("1:cpu:/old-v1").is_err());
    }
}
