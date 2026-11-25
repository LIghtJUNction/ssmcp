//! Kernel-side resource calculations and checks for the session slices.
use crate::{Result, worker::limits};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const MOUNT: &str = "/sys/fs/cgroup";

pub(super) fn membership() -> Result<PathBuf> {
    let text = fs::read_to_string("/proc/self/cgroup")?;
    Ok(limits::group_path(&text)?)
}

pub(super) fn check_mount() -> Result<()> {
    let mut stat = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: statfs initializes the buffer on success, and the path is NUL terminated.
    if unsafe { libc::statfs(c"/sys/fs/cgroup".as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    if unsafe { stat.assume_init() }.f_type != libc::CGROUP2_SUPER_MAGIC {
        return Err("session resource limits require cgroup v2 at /sys/fs/cgroup".into());
    }
    Ok(())
}

fn optional(path: &Path, name: &str) -> Result<Option<String>> {
    match fs::read_to_string(path.join(name)) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn cpus(text: &str) -> Result<u64> {
    let mut total = 0u64;
    let mut previous = None;
    for item in text.trim().split(',') {
        let (low, high) = item.split_once('-').unwrap_or((item, item));
        let (low, high) = (low.parse::<u32>()?, high.parse::<u32>()?);
        if high < low || previous.is_some_and(|last| low <= last) {
            return Err("invalid kernel CPU list".into());
        }
        previous = Some(high);
        total += u64::from(high - low) + 1;
    }
    Ok(total)
}

fn effective(group: &Path, name: &str) -> Result<Option<String>> {
    for ancestor in group.ancestors().take_while(|p| p.starts_with(MOUNT)) {
        if let Some(value) = optional(ancestor, name)? {
            return Ok(Some(value.trim().to_owned()));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub cpu: u64, // quota in microseconds for one second of wall time
    pub memory: u64,
}

impl Budget {
    pub fn capacity(manager: &Path) -> Result<Self> {
        let cpu_list = match effective(manager, "cpuset.cpus.effective")? {
            Some(cpus) => cpus,
            None => fs::read_to_string("/sys/devices/system/cpu/online")?,
        };
        let mut cpu = cpus(&cpu_list)? * 1_000_000;
        for ancestor in manager.ancestors().take_while(|p| p.starts_with(MOUNT)) {
            if let Some(text) = optional(ancestor, "cpu.max")?
                && let Some(limit) = limits::cpu_limit(&text)?
            {
                cpu = cpu.min(limit);
            }
        }
        let text = fs::read_to_string("/proc/meminfo")?;
        let mut fields = text
            .lines()
            .find_map(|line| line.strip_prefix("MemTotal:"))
            .ok_or("MemTotal missing")?
            .split_whitespace();
        let kib = fields.next().ok_or("empty MemTotal")?.parse::<u64>()?;
        if fields.next() != Some("kB") {
            return Err("unrecognized MemTotal unit".into());
        }
        Ok(Self {
            cpu,
            memory: kib.checked_mul(1024).ok_or("MemTotal overflow")?,
        })
    }

    pub fn fraction(
        self,
        numerator: u64,
        denominator: u64,
        memory_divisor: u64,
        percent: Option<u32>,
        bytes: Option<u64>,
    ) -> Result<Self> {
        // Round down so kernel page and systemd quota quantization never relax a ceiling.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page <= 0 {
            return Err("unavailable kernel page size".into());
        }
        let page = page as u64;
        let cpu = (self.cpu * numerator / denominator)
            .min(percent.map_or(u64::MAX, |p| u64::from(p) * 10_000))
            / 1000
            * 1000;
        let memory = (self.memory / memory_divisor).min(bytes.unwrap_or(u64::MAX)) / page * page;
        if cpu < 1000 || memory == 0 {
            return Err("session resource budget below kernel minimum".into());
        }
        Ok(Self { cpu, memory })
    }

    pub fn read(group: &Path) -> Result<Self> {
        let cpu = limits::cpu_limit(&fs::read_to_string(group.join("cpu.max"))?)?
            .ok_or("unbounded session CPU quota")?;
        let memory = limits::memory_limit(&fs::read_to_string(group.join("memory.max"))?)?
            .ok_or("unbounded session memory quota")?;
        if fs::read_to_string(group.join("memory.swap.max"))?.trim() != "0" {
            return Err("session swap ceiling must be zero".into());
        }
        Ok(Self { cpu, memory })
    }

    pub fn verify(self, group: &Path) -> Result<Self> {
        let actual = Self::read(group)?;
        if actual.cpu > self.cpu || actual.memory > self.memory {
            return Err(format!(
                "resource ceilings at {} exceed the session budget",
                group.display()
            )
            .into());
        }
        Ok(actual)
    }
}

/// Moving to a sibling outside a limited subtree can split a shared quota even if
/// the same numeric limit is copied. Only ceilings already covered by an ancestor
/// common to both locations may be left behind.
pub(super) fn check_move(from: &Path, to: &Path) -> Result<()> {
    let common = from
        .ancestors()
        .find(|p| to.starts_with(p))
        .ok_or("unrelated cgroups")?;
    let cap = |name: &str, path: &Path| -> Result<Option<u64>> {
        let Some(text) = optional(path, name)? else {
            return Ok(None);
        };
        if name == "cpu.max" {
            return Ok(limits::cpu_limit(&text)?);
        }
        if text.trim() == "max" {
            return Ok(None);
        }
        Ok(Some(text.trim().parse()?))
    };
    for (name, hard) in [
        ("cpu.max", "cpu.max"),
        ("memory.max", "memory.max"),
        ("memory.high", "memory.max"),
        ("memory.swap.max", "memory.swap.max"),
    ] {
        let mut shared = u64::MAX;
        for node in common.ancestors().take_while(|p| p.starts_with(MOUNT)) {
            if let Some(limit) = cap(name, node)? {
                shared = shared.min(limit);
            }
            if hard != name
                && let Some(limit) = cap(hard, node)?
            {
                shared = shared.min(limit);
            }
        }
        for node in from.ancestors().take_while(|p| *p != common) {
            if cap(name, node)?.is_some_and(|limit| limit < shared) {
                return Err(
                    format!("cannot leave shared {name} limit at {}", node.display()).into(),
                );
            }
        }
    }
    for name in ["cpuset.cpus.effective", "cpuset.mems.effective"] {
        if effective(from, name)? != effective(common, name)? {
            return Err(format!("cannot leave inherited {name} placement").into());
        }
    }
    Ok(())
}
