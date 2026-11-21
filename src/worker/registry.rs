//! One private registry per OS user. A flock serializes admission, quota changes
//! and the idle-to-busy transition across SSMCP processes. It is not an OS-user
//! security sandbox: arbitrary programs run with the user's existing authority.
use super::{config::{Allocation, Capacity, Config}, limits};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::{File, OpenOptions}, io::{Read, Write}, os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}, time::Duration};

const MAX_REGISTRY: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Unit {
    pub(crate) name: String,
    pub(crate) allocation: Allocation,
    pub(crate) group: Option<PathBuf>,
    pub(crate) invocation: Option<String>,
    busy: bool,
    retired: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pool {
    generation: String,
    pid: u32,
    start: u64,
    boot: String,
    config: Config,
    capacity: Capacity,
    ready: bool,
    units: Vec<Unit>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State { schema: u32, pools: BTreeMap<String, Pool> }

pub(crate) struct Registry { root: PathBuf, boot: String }

pub(crate) enum AdmissionError { Deferred(String), Failed(String) }
impl From<String> for AdmissionError { fn from(value: String) -> Self { Self::Failed(value) } }
impl From<&str> for AdmissionError { fn from(value: &str) -> Self { Self::Failed(value.into()) } }

struct Locked { file: File, state: State }

impl Registry {
    pub(crate) fn shared() -> Result<Self, String> {
        #[cfg(test)]
        return Err("unit tests must supply a private worker registry fixture".into());
        #[cfg(not(test))]
        {
            // A single fixed namespace counts this UID's workers regardless of
            // cwd, HOME, session name or the caller's chosen TMPDIR.
            let root = PathBuf::from(format!("/tmp/ssmcp-workers-{}", unsafe { libc::geteuid() }));
            private_directory(&root)?;
            let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|e| e.to_string())?.trim().to_owned();
            Ok(Self { root, boot })
        }
    }

    async fn lock(&self) -> Result<Locked, String> {
        private_directory(&self.root)?;
        let file = private_file(&self.root.join("registry.lock"), true)?;
        let until = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match crate::platform::lock_file(&file, true) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && tokio::time::Instant::now() < until => {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(format!("worker registry lock unavailable: {error}")),
            }
        }
        let state = match std::fs::symlink_metadata(self.root.join("registry.json")) {
            Ok(_) => {
                let file = private_file(&self.root.join("registry.json"), false)?;
                let mut bytes = Vec::new();
                file.take(MAX_REGISTRY + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
                if bytes.len() as u64 > MAX_REGISTRY { return Err("worker registry is too large".into()); }
                let state: State = serde_json::from_slice(&bytes).map_err(|e| format!("invalid worker registry: {e}"))?;
                validate(&state)?;
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                State { schema: 1, pools: BTreeMap::new() }
            }
            Err(error) => return Err(error.to_string()),
        };
        Ok(Locked { file, state })
    }

    fn save(&self, locked: &Locked) -> Result<(), String> {
        let _lease = &locked.file;
        validate(&locked.state)?;
        let bytes = serde_json::to_vec(&locked.state).map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_REGISTRY { return Err("worker registry is full".into()); }
        let path = self.root.join(format!("registry-{}.new", nonce()?));
        let result = (|| {
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path).map_err(|e| e.to_string())?;
            file.write_all(&bytes).and_then(|_| file.sync_all()).map_err(|e| e.to_string())?;
            std::fs::rename(&path, self.root.join("registry.json")).map_err(|e| e.to_string())?;
            File::open(&self.root).and_then(|file| file.sync_all()).map_err(|e| e.to_string())
        })();
        if result.is_err() { let _ = std::fs::remove_file(path); }
        result
    }

    async fn prune(&self, locked: &mut Locked) -> Result<(), String> {
        let mut dead = Vec::new();
        for (key, pool) in &locked.state.pools {
            let gone = if pool.boot != self.boot { true } else {
                match std::fs::read_to_string(format!("/proc/{}/stat", pool.pid)) {
                    Ok(stat) => limits::process_start(&stat)? != pool.start,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                    Err(error) => return Err(format!("cannot verify existing worker owner: {error}")),
                }
            };
            if gone { dead.push(key.clone()); }
        }
        for key in dead {
            let pool = &locked.state.pools[&key];
            if pool.boot == self.boot {
                for unit in &pool.units { stop_owned(&pool.generation, unit).await?; }
            }
            locked.state.pools.remove(&key);
        }
        Ok(())
    }

    /// All existing slots must be at their owned idle checkpoint before their
    /// memory ceilings can decrease. No active model is OOM-killed to admit a
    /// new pool. The new pool remains offline while this returns a busy error.
    pub(crate) async fn admit(&self, generation: &str, config: Config, capacity: Capacity) -> Result<Vec<Unit>, AdmissionError> {
        valid_nonce(generation)?;
        let mut locked = self.lock().await?;
        self.prune(&mut locked).await?;
        if locked.state.pools.contains_key(generation) { return Err("worker generation was already admitted".into()); }
        if locked.state.pools.values().any(|pool| !pool.ready || pool.units.iter().any(|unit| !unit.retired && unit.busy)) {
            return Err(AdmissionError::Deferred("waiting for existing workers to reach their idle resource checkpoint".into()));
        }
        let total = locked.state.pools.values().map(|pool| pool.units.iter().filter(|unit| !unit.retired).count()).sum::<usize>()
            .checked_add(config.count).ok_or("registered worker count overflow")?;
        let budget = locked.state.pools.values().fold(capacity, |budget, pool| Capacity {
            cpu_micros_per_second: budget.cpu_micros_per_second.min(pool.capacity.cpu_micros_per_second),
            memory_bytes: budget.memory_bytes.min(pool.capacity.memory_bytes),
        });
        let allocation = budget.allocate(total, config.factor)?;
        let mut updates = Vec::new();
        for (key, pool) in &locked.state.pools {
            let share = budget.allocate(total, pool.config.factor)?;
            for (index, unit) in pool.units.iter().enumerate() {
                if unit.retired { continue; }
                let group = owned_group(&pool.generation, unit).await?.ok_or("an admitted worker is no longer connected")?;
                let current = std::fs::read_to_string(group.join("memory.current")).map_err(|e| e.to_string())?
                    .trim().parse::<u64>().map_err(|_| "invalid worker memory.current")?;
                // Admission is deferred rather than shrinking underneath the
                // idle process's current resident/cache charge.
                if current.saturating_add(1024 * 1024) >= share.memory_bytes {
                    return Err(AdmissionError::Deferred("waiting for idle worker memory to fit the smaller resource share".into()));
                }
                updates.push((key.clone(), index, share, group));
            }
        }
        for (key, index, share, group) in updates {
            let unit = &locked.state.pools[&key].units[index];
            let outcome = set_limits(&unit.name, share).await.and_then(|_| limits::verify(&group, share));
            if let Err(error) = outcome {
                // Any already-confirmed reductions stay recorded as stricter
                // ceilings. Never admit the new slots after a partial update.
                self.save(&locked)?;
                return Err(error.into());
            }
            locked.state.pools.get_mut(&key).expect("retained pool").units[index].allocation = share;
        }
        let units: Vec<_> = (0..config.count).map(|slot| Unit {
            name: format!("ssmcp-worker-{generation}-{slot}.scope"), allocation,
            group: None, invocation: None, busy: false, retired: false,
        }).collect();
        locked.state.pools.insert(generation.into(), Pool {
            generation: generation.into(), pid: std::process::id(), start: limits::identity(std::process::id())?,
            boot: self.boot.clone(), config, capacity, ready: false, units: units.clone(),
        });
        self.save(&locked)?;
        Ok(units)
    }

    pub(crate) async fn connected(&self, generation: &str, index: usize) -> Result<Unit, String> {
        let mut locked = self.lock().await?;
        let pool = own_pool(&mut locked.state, generation, &self.boot)?;
        let unit = pool.units.get_mut(index).ok_or("worker slot is missing")?;
        let shown = show(&unit.name).await?;
        let group = check_show(generation, unit, &shown)?.ok_or("worker scope did not start")?;
        limits::verify(&group, unit.allocation)?;
        unit.group = Some(group);
        unit.invocation = Some(shown.get("InvocationID").filter(|id| id.len() == 32).ok_or("worker scope has no invocation identity")?.clone());
        let result = unit.clone();
        pool.ready = pool.units.iter().all(|unit| unit.invocation.is_some());
        self.save(&locked)?;
        Ok(result)
    }

    /// Serialized with admission. The registry busy bit is set before any user
    /// code is sent to the child; failure never falls back to unbounded spawn.
    pub(crate) async fn begin(&self, generation: &str, index: usize) -> Result<Unit, String> {
        let mut locked = self.lock().await?;
        let pool = own_pool(&mut locked.state, generation, &self.boot)?;
        if !pool.ready { return Err("worker resource admission is incomplete".into()); }
        let unit = pool.units.get_mut(index).ok_or("worker slot is missing")?;
        if unit.retired { return Err("worker slot was retired after owned cleanup".into()); }
        if unit.busy { return Err("worker slot is already busy".into()); }
        let group = owned_group(generation, unit).await?.ok_or("worker scope disconnected")?;
        limits::verify(&group, unit.allocation)?;
        unit.busy = true;
        let result = unit.clone();
        self.save(&locked)?;
        Ok(result)
    }

    pub(crate) async fn idle(&self, generation: &str, index: usize) -> Result<(), String> {
        let mut locked = self.lock().await?;
        let pool = own_pool(&mut locked.state, generation, &self.boot)?;
        let unit = pool.units.get_mut(index).ok_or("worker slot is missing")?;
        let group = owned_group(generation, unit).await?.ok_or("worker scope disconnected before idle acknowledgement")?;
        limits::verify(&group, unit.allocation)?;
        unit.busy = false;
        self.save(&locked)
    }

    pub(crate) async fn snapshot(&self, generation: &str) -> Result<serde_json::Value, String> {
        let locked = self.lock().await?;
        let pool = locked.state.pools.get(generation).ok_or("worker resource admission is incomplete")?;
        Ok(serde_json::json!({"scope":"current_os_user", "uid":unsafe { libc::geteuid() },
            "registered_workers":locked.state.pools.values().map(|pool| pool.units.iter().filter(|unit| !unit.retired).count()).sum::<usize>(),
            "effective_capacity":pool.capacity,"factor":pool.config.factor,"units":pool.units,
            "note":"Ceilings are the last verified owned-scope limits; dispatch rechecks kernel values. Stricter ceilings may remain after another pool leaves or a partial admission fails."}))
    }

    pub(crate) async fn retire(&self, generation: &str, index: usize) -> Result<(), String> {
        let mut locked = self.lock().await?;
        let Some(pool) = locked.state.pools.get_mut(generation) else { return Ok(()); };
        let unit = pool.units.get_mut(index).ok_or("worker slot is missing")?;
        stop_owned(generation, unit).await?;
        unit.busy = false;
        unit.retired = true;
        if pool.units.iter().all(|unit| unit.retired) { locked.state.pools.remove(generation); }
        self.save(&locked)
    }

    /// Stop and observe every owned scope before removing its count. Raising
    /// surviving limits is optional: their already-verified stricter ceilings
    /// remain safe until the next admission recalculates the common budget.
    pub(crate) async fn leave(&self, generation: &str) -> Result<(), String> {
        let mut locked = self.lock().await?;
        let Some(pool) = locked.state.pools.get(generation) else { return Ok(()); };
        for unit in &pool.units { stop_owned(generation, unit).await?; }
        locked.state.pools.remove(generation);
        self.save(&locked)
    }
}

fn own_pool<'a>(state: &'a mut State, generation: &str, boot: &str) -> Result<&'a mut Pool, String> {
    let pool = state.pools.get_mut(generation).ok_or("worker generation is no longer registered")?;
    if pool.pid != std::process::id() || pool.boot != boot || limits::identity(pool.pid).ok() != Some(pool.start) {
        return Err("worker registry owner changed".into());
    }
    Ok(pool)
}

fn validate(state: &State) -> Result<(), String> {
    if state.schema != 1 || state.pools.len() > 256 { return Err("unsupported worker registry".into()); }
    for (key, pool) in &state.pools {
        valid_nonce(key)?;
        if key != &pool.generation || pool.pid == 0 || pool.start == 0 || pool.boot.is_empty()
            || pool.units.len() != pool.config.count || !(1..=super::config::MAX_WORKERS).contains(&pool.config.count)
            || !pool.config.factor.is_finite() || pool.config.factor <= 0.0 {
            return Err("invalid worker registry owner or slot count".into());
        }
        for (slot, unit) in pool.units.iter().enumerate() {
            if unit.name != format!("ssmcp-worker-{key}-{slot}.scope") { return Err("invalid owned worker unit".into()); }
            if let Some(group) = &unit.group {
                if !group.starts_with("/sys/fs/cgroup") || group.file_name().and_then(|n| n.to_str()) != Some(&unit.name)
                    || group.components().any(|part| !matches!(part, std::path::Component::RootDir | std::path::Component::Normal(_))) {
                    return Err("invalid owned worker cgroup".into());
                }
            }
        }
    }
    Ok(())
}

fn private_directory(path: &Path) -> Result<(), String> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
        Err(error) => return Err(error.to_string()),
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o700 {
        return Err("worker registry directory has unsafe ownership or permissions".into());
    }
    Ok(())
}

fn private_file(path: &Path, create: bool) -> Result<File, String> {
    let file = OpenOptions::new().read(true).write(create).create(create).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path).map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o777 != 0o600 || metadata.nlink() != 1 {
        return Err("worker registry file has unsafe ownership, permissions or links".into());
    }
    Ok(file)
}

pub(crate) fn nonce() -> Result<String, String> {
    let mut bytes = [0_u8; 16];
    crate::platform::random_bytes(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn valid_nonce(value: &str) -> Result<(), String> {
    if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err("invalid worker generation".into());
    }
    Ok(())
}

pub(crate) fn properties(allocation: Allocation) -> Vec<String> {
    // Round down the last sub-kernel quota digits, never above the formula.
    let quota = allocation.cpu_micros_per_second / 100;
    vec![format!("CPUQuota={}.{:02}%", quota / 100, quota % 100),
        "CPUQuotaPeriodSec=100ms".into(), format!("MemoryMax={}", allocation.memory_bytes / 4096 * 4096),
        "MemorySwapMax=0".into(), "TasksMax=256".into()]
}

async fn systemctl(args: &[String]) -> Result<Vec<u8>, String> {
    let mut all = vec!["--user".into(), "--no-pager".into()];
    all.extend_from_slice(args);
    crate::backend::run_helper(Path::new("/"), "systemctl", &all, None, 16 * 1024, Duration::from_secs(10)).await
}

async fn set_limits(name: &str, allocation: Allocation) -> Result<(), String> {
    let mut args = vec!["set-property".into(), "--runtime".into(), name.into()];
    args.extend(properties(allocation));
    systemctl(&args).await.map(|_| ())
}

async fn show(name: &str) -> Result<BTreeMap<String, String>, String> {
    use tokio::io::AsyncReadExt;
    let mut command = tokio::process::Command::new(crate::program::executable("systemctl")?);
    command.args(["--user", "--no-pager", "show", "--property=LoadState,ActiveState,ControlGroup,InvocationID,Description", name])
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut process = crate::backend::process::Managed::spawn(&mut command).map_err(|e| e.to_string())?;
    let mut out = process.child.stdout.take().ok_or("systemctl stdout missing")?.take(16_385);
    let mut err = process.child.stderr.take().ok_or("systemctl stderr missing")?.take(4_097);
    let mut bytes = Vec::new();
    let mut errors = Vec::new();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let (a, b, status) = tokio::join!(out.read_to_end(&mut bytes), err.read_to_end(&mut errors), process.wait());
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        status.map_err(|e| e.to_string())
    }).await.map_err(|_| "worker unit inspection timed out".to_owned()).and_then(|value| value);
    process.shutdown().await;
    let status = outcome?;
    if bytes.len() > 16_384 || errors.len() > 4_096 { return Err("worker unit inspection exceeded its output bound".into()); }
    let text = String::from_utf8(bytes).map_err(|_| "systemd returned non-UTF-8 unit properties")?;
    let fields: BTreeMap<String, String> = text.lines().filter_map(|line| line.split_once('=').map(|(key, value)| (key.into(), value.into()))).collect();
    // systemctl may return nonzero for a collected unit. Accept only its actual
    // explicit not-found/inactive pair, never a D-Bus or permission error.
    if !status.success() && !(fields.get("LoadState").is_some_and(|s| s == "not-found")
        && fields.get("ActiveState").is_some_and(|s| s == "inactive")) {
        return Err(format!("worker unit inspection failed ({status})"));
    }
    Ok(fields)
}

fn check_show(generation: &str, unit: &Unit, shown: &BTreeMap<String, String>) -> Result<Option<PathBuf>, String> {
    if shown.get("LoadState").is_some_and(|state| state == "not-found") { return Ok(None); }
    if shown.get("Description").map(String::as_str) != Some(format!("SSMCP worker {generation}").as_str()) {
        return Err("worker unit description does not match its owner".into());
    }
    if unit.invocation.as_ref().is_some_and(|id| shown.get("InvocationID") != Some(id)) {
        return Err("worker unit invocation changed; refusing to touch a replacement".into());
    }
    let path = match shown.get("ControlGroup").filter(|value| !value.is_empty()) {
        Some(path) => path,
        None if shown.get("ActiveState").is_some_and(|state| matches!(state.as_str(), "inactive" | "failed")) => return Ok(None),
        None => return Err("worker unit has no cgroup".into()),
    };
    let group = limits::group_path(&format!("0::{path}"))?;
    if group.file_name().and_then(|name| name.to_str()) != Some(&unit.name) {
        return Err("worker unit cgroup does not match its unique name".into());
    }
    if unit.group.as_ref().is_some_and(|old| old != &group) { return Err("worker cgroup identity changed".into()); }
    Ok(Some(group))
}

async fn owned_group(generation: &str, unit: &Unit) -> Result<Option<PathBuf>, String> {
    check_show(generation, unit, &show(&unit.name).await?)
}

pub(crate) async fn stop_owned(generation: &str, unit: &Unit) -> Result<(), String> {
    let group = owned_group(generation, unit).await?;
    if group.is_some() {
        systemctl(&["stop".into(), unit.name.clone()]).await?;
    }
    // Stopping the systemd-run wrapper is insufficient. The owned scope and all
    // descendants must actually be empty before a slot/count can be released.
    let group = group.as_ref().or(unit.group.as_ref());
    for _ in 0..40 {
        let empty = match group {
            None => true,
            Some(group) => match std::fs::read_to_string(group.join("cgroup.events")) {
                Ok(events) => events.lines().any(|line| line == "populated 0"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
                Err(error) => return Err(format!("worker cleanup readback failed: {error}")),
            },
        };
        if empty { return Ok(()); }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err("owned worker scope is still populated; slot remains unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn systemd_limits_round_down_and_never_use_unbounded_defaults() {
        let properties = properties(Allocation { cpu_micros_per_second: 1_333_333, memory_bytes: 107_374_182, registered_workers: 10 });
        assert_eq!(properties[0], "CPUQuota=133.33%");
        assert_eq!(properties[2], "MemoryMax=107372544");
        assert!(properties.contains(&"MemorySwapMax=0".into()));
        assert!(properties.contains(&"TasksMax=256".into()));
    }

    #[test]
    fn global_worker_registry_is_never_opened_by_library_tests() {
        assert!(Registry::shared().is_err());
    }
}
